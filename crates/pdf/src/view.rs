//! PDF 阅读器视图：瓦片级虚拟化核心（加载、滚动调度、按需瓦片渲染、滚动条）。
//! **零磁盘缓存**：瓦片与占位（缩略/中清）全部纯内存，关闭文档即释放。
//!
//! 子模块拆分（`src/pdf/` 下）：
//! - [`super::layout`]：页列纯布局 + 瓦片网格几何（worker 与 view 共用同一套数学）
//! - `view` 内部按职责拆成 5 个子模块（均在 `src/pdf/view/` 下，经下表 `super::*`
//!   共享本文件全部导入与私有成员）：
//!   - [`open`]：文档打开 / 状态访问器 / 资源释放 / 每帧装配（`ensure_window`）
//!   - [`input`]：交互 / 命中 / 选区（滚轮缩放、点击拖拽、Ctrl+C 复制）
//!   - [`tiles`]：worker 栅格化调度 / 瓦片与占位缓存 / 内存预算
//!   - [`search`]：页文本层懒加载缓存 + P3 全文检索
//!   - [`render`]：上屏渲染（页列 DOM 虚拟化 + 选区/检索高亮叠加）
//!   根 `view.rs` 保留骨架：`PdfViewer` 结构体、`WindowSig` 帧门控签名、
//!   通用 helper（`timing_view` / `collect_finished`）与测试块。
//!
//! 本文件顶部导入为 `view/` 各子模块的**共享外部依赖源**（各子模块 `use super::*;`），
//! 故保留完整导入不被裁剪，未在本文件直接消费的项经 `#[allow(unused_imports)]` 放行。
//!
//! 书签目录已统一移到侧边栏（[`crate::app::sidebar`] + 公共 [`crate::toc`]），
//! 本模块只保留书签数据（`outlines`）与 [`Self::scroll_to_page`] 跳页能力。
//!
//! 关键机制：
//! - **打开即显**：文件读取 + 结构解析 + 页尺寸测量都在后台线程做（便宜），
//!   完成后马上铺好全部页槽（滚动条正确）；**不**预先栅格化任何页。
//! - **瓦片按需栅格化（SumatraPDF 式分块）**：每帧根据滚动位置与可视区，把可见页
//!   切成 [TILE_SIZE × TILE_SIZE] 网格，只向后台派发「视口可见 + TILE_MARGIN 缓冲」
//!   的少数几片（每片 ~1MB）。内存 = 视口面积 × 4B，**不再随整页位图缩放爆炸**；
//!   瓦片按显示物理像素（[`super::layout::render_scale`]）1:1 渲染，高 DPI / Ctrl+滚轮
//!   放大始终锐利（根治旧版"一页 4× 位图 64MB + 密度封顶发糊"）。
//! - **占位层**：L0 缩略图 / L0.5 中清页在**快速拖动**期间铺为全页低清底图（瓦片
//!   逐片到达前"永远有内容"）；停止后瓦片立即按新视口补齐，替换占位。
//! - **回收**：滚离视口的瓦片按 [`TILE_CACHE_BYTES`] 字节预算 + 距离 LRU 淘汰，
//!   显式 `drop_image` 下线 GPU 纹理；缩放/换 DPI 后 `tile_scale` 比对惰性作废。
//! - **不闪烁**：页槽先是有尺寸占位（页号/加载中），瓦片到位后替换，布局稳定。
//!
//! ## 性能优化（相对旧整页版本）
//!
//! 1. **页列 DOM 虚拟化**：`render()` 每帧只构建「可见页 + MARGIN 缓冲」这几张页面卡片
//!    并以绝对定位铺在等高的“全文档高度”容器里（[`super::layout::compute_layout`]），
//!    不再把几百上千页全部 `div()` 出来布局——长文档滚动/换帧开销从 O(页数) 降到 O(可见页)。
//! 2. **瓦片级调度**：派发单位从"整页"降为"512px 瓦片"，视口填充粒度细、并发可更高
//!    （[`TILE_INFLIGHT`]），快速滚动时只会浪费少量小片，不再排队整页巨型任务。
//! 3. **在途请求节流**：瓦片/中清/缩略图各自额度封顶，单线程 pdfium 串行渲染吞吐不变，
//!    快速滚动时不会无限堆积过期请求。

// 根文件保持完整导入：`view/` 各子模块 `use super::*` 共享此处全部外部项，
// 本文件未直接消费的导入（已迁往子模块的职责）由 allow 放行，不做裁剪。
#![allow(unused_imports)]

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{mpsc, Arc};

use gpui::{
    div, img, px, rgba, rgb, App, ClipboardItem, Context, FocusHandle, Hsla, ImageSource,
    InteractiveElement, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ParentElement, Point, Pixels, Render, RenderImage, ScrollHandle, ScrollWheelEvent,
    SharedString, StatefulInteractiveElement, Styled, Window,
};
use ui::scrollbar::{Scrollbar, ScrollbarMode};

use crate::recolor::RecolorParams;
use crate::pdf::parser::{RawKind, RecolorKey};

/// 在途瓦片请求记录：`(派发时密度 scale, 导航世代 nav_gen, 结果通道)`。
/// `nav_gen` 用于 L2.4：大跳（`nav_gen++`）当帧清掉世代 < 新 gen 的在途条目，预算立即释放。
pub(crate) type PendingTileEntry = (f32, u64, mpsc::Receiver<Option<TileResult>>);

/// 静态帧门控签名（Sprint 5）：本帧到上帧「滚动 / 缩放 / DPI / 视口尺寸 / 页数」的
/// 变化判定信号。任一维不同 → 帧状态已变，必须恢复 keep_window/schedule/evict 全量
/// 调度；完全一致 → 才可进入「静置候选」（还需在途清空 + [`PdfViewer::window_settled`]
/// 确认才真正跳过——见 `ensure_window` 的门控分支）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct WindowSig {
    /// 未量程化纵向滚动像素（f32 位型）：任意微滚即变，杜绝量子化掩盖滚动（同 wy prewarm）。
    scroll_top: u32,
    /// 未量程化横向滚动像素（f32 位型）：窄窗下内容宽于视口可横向平移
    /// （见 render 的横向滚动条），`page_visible_rect` 用 `-offset().x` 推可见瓦片列——
    /// 漏掉该维会在横向平移停稳后假静止 → 新暴露列的瓦片永不调度（贴图迟滞）。
    scroll_x: u32,
    /// 页列显示宽 = `PAGE_WIDTH × zoom`（f32 位型）：Ctrl+滚轮缩放变化即变。
    page_width: u32,
    /// 窗口缩放密度（f32 位型）：换 DPI / 跨屏拖拽变化即变。
    scale_factor: u32,
    /// 滚动容器可视尺寸（px）：窗口/分屏/侧边栏宽度变化即变。
    viewport_w: u32,
    viewport_h: u32,
    /// 页数兜底：加载完成建槽 / 任何布局重算潜在的侧通道变化。
    pages: usize,
}

impl WindowSig {
    pub(crate) fn of(
        v: &PdfViewer,
        scroll_y: f32,
        page_width: f32,
        scale_factor: f32,
        viewport_h: f32,
    ) -> Self {
        Self {
            scroll_top: scroll_y.to_bits(),
            scroll_x: f32::from(-v.scroll.offset().x).to_bits(),
            page_width: page_width.to_bits(),
            scale_factor: scale_factor.to_bits(),
            viewport_w: f32::from(v.scroll.bounds().size.width).to_bits(),
            viewport_h: viewport_h.to_bits(),
            pages: v.dims.len(),
        }
    }
}

/// Phase B1：选中文字后自动弹出的操作菜单（复制/高亮/删除线/下划线，一行一项）状态。
/// 拖选结束（`mouse_up` 且曾 `selecting`）且选区非空 → 弹卡；点卡片外 / 重新拖选 /
/// 滚动 / Esc → 关闭。`scroll` 快照用于在 `ensure_window` 里侦测「滚动即关闭」。
#[derive(Clone, Copy, Debug)]
pub(crate) struct SelCard {
    /// 卡片锚点（**窗口坐标**，供 `deferred(anchored(Window))` 定位）。
    pub pos: gpui::Point<Pixels>,
    /// 打开瞬间的滚动容器偏移（随后滚动即关闭）。
    pub scroll: gpui::Point<Pixels>,
}

/// 一条**待保存**注释（待保存模型 P1）：只进内存叠加层渲染，`save()` 时才由 worker
/// 合并写盘。`quads_pt` 为页内 PDF 点矩形（y 向上），与写回几何共用同一坐标语义。
#[derive(Debug, Clone)]
pub(crate) struct PendingAnnot {
    /// 本阅读器内自增唯一 id（撤销/删除注释定位用；不改动写回内容）。
    pub id: u64,
    /// 所属页（0 基；页面操作插入/删除后随页号平移/丢弃）。
    pub page: usize,
    /// 注释类型（高亮/删除线/下划线）。
    pub kind: AnnotKind,
    /// 覆盖该条注释的字符区域（PDF 点矩形，可多行）。
    pub quads_pt: Vec<QuadPt>,
}

/// 待保存撤销栈条目（`VecDeque` 尾部 = 最近；Ctrl+Z 弹尾部逆操作）。
#[derive(Debug)]
pub(crate) enum UndoEntry {
    /// 撤销「添加注释」：按 id 从 pending 移除该条。
    RemoveAnnot { id: u64 },
    /// 撤销「删除注释」：把整条注释重新加回 pending。
    AddAnnotBack { annot: PendingAnnot },
    /// 撤销「插入/删除页面」：恢复页面操作前的内存工作副本字节 + 操作前待存注释快照，
    /// 并把页操作计数回退（渲染由 worker 内存级重载完成）。
    RestoreMemory { bytes: Vec<u8>, annots_before: Vec<PendingAnnot> },
}

use config::pdf::{
    COVER_INFLIGHT, IMAGE_CACHE_PAGES, IMAGE_INFLIGHT, PREFETCH_COVER_MARGIN,
    PREFETCH_COVER_MARGIN_MAX, PREFETCH_RETURN_SIDE_BONUS, JUMP_PAGES_THRESHOLD,
    MAX_MID_INFLIGHT, MAX_THUMB_INFLIGHT, MID_CACHE_PAGES, MOVE_THRESHOLD_LOWER_RATIO,
    MOVE_THRESHOLD_PX, MOVE_THRESHOLD_VERY_RATIO, PAGE_PAD_X, PAGE_WIDTH, STOP_DETECT_FRAMES,
    STOP_DETECT_PX, TEXT_CACHE_PAGES, TEXT_INFLIGHT, THUMB_CACHE_PAGES, TILE_CACHE_BYTES,
    TILE_INFLIGHT, TILE_MARGIN, TILE_SIZE, ZOOM_MAX, ZOOM_MIN, ZOOM_STEP,
};
use crate::pdf::layout::{
    advance_match_index, bitmap_display_scales, column_left, compute_layout, content_to_local,
    cover_affordable, cover_scale, display_height, first_visible_page, hit_char, keep_window,
    local_to_pt, page_bitmap_size, page_segment_text, pt_to_local, render_scale,
    selection_spans_pt, visible_tile_range_axis, zoom_rescroll_anchor, Slot, TileSlot,
};
use crate::pdf::{
    render_image_from_bgra, AnnotKind, AnnotateItem, AnnotationInfo, DocId, PageText,
    PdfImageBox, PdfImagePixels, PdfOutlineEntry, PdfSearchHit, PdfSearchResults, PdfWorker,
    QuadPt, RecolorEntry, RenderRequest, RenderResult, TileRequest, TileResult,
};
use viewer::pan_zoom;
/// 是否开启视图侧计时日志（`GPUI_EDITOR_PDF_TIMING=1`，进程内缓存一次）。
fn timing_view() -> bool {
    static EN: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *EN.get_or_init(|| {
        std::env::var_os("GPUI_EDITOR_PDF_TIMING").is_some_and(|v| v == "1" || v == "true")
    })
}

/// 是否开启**真机取证探针**（`GPUI_EDITOR_DIAG`，进程内缓存一次；未设零开销——
/// 仅有缓存到位的 O(1) 布尔查，无任何打印/行为影响）。供静态帧门控早退分支等
/// 只读观测点使用，只加日志不改任何调度语义。
fn diag_enabled() -> bool {
    static EN: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *EN.get_or_init(|| std::env::var_os("GPUI_EDITOR_DIAG").is_some())
}

/// 滚动速度分带（F3「滚动即清晰」三速状态机）：把原 `prev_fast: bool` 升级为四态枚举。
/// `prev_fast` 保持 = `is_fast()` 的派生布尔，既有消费点（自刷新泵 / 静态帧门控 / 文本预取 /
/// 回归测试）结构不变；本字段决定每帧**调度形态**。迟滞阈值：进 Fast = [`MOVE_THRESHOLD_PX`]，
/// 退 Fast = 其 × [`MOVE_THRESHOLD_LOWER_RATIO`]；进 VeryFast = × [`MOVE_THRESHOLD_VERY_RATIO`]，
/// 退 VeryFast = 再 × [`MOVE_THRESHOLD_LOWER_RATIO`]（防 480px/帧 附近横跳）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SpeedState {
    /// 近静止（速度 EMA ≤ 1px/帧）：与 [`SpeedState::Slow`] 同走常态瓦片调度（**真**静置帧
    /// 由静态帧门控 `window_sig`/`window_settled` 跳过，与速度状态正交）。
    Idle,
    /// 慢速常态：瓦片 + 封面常态调度（`schedule_tiles`，瓦片先派防封面堵小片）。
    Slow,
    /// 中速带 `(exit, very]`：**满密度封面优先**（`schedule_fast_covers`，封面不可负担的
    /// 病理性页回退中清/缩略占位），滚到哪清晰到哪——不再落 520/260 低清占位。
    Fast,
    /// 高速带 `(very, ∞)`：**纯平移**，不派任何新栅格（渲染队列不加活）；新进视口页交给
    /// F4 预取走廊兜底（本波未实现，可能短暂占位）。
    VeryFast,
}

impl SpeedState {
    /// 快速态（含中速带与高速带）：泵/门控/文本预取等「拖动期行为」的派生布尔（≡ 旧 `prev_fast`）。
    pub(crate) fn is_fast(self) -> bool {
        matches!(self, SpeedState::Fast | SpeedState::VeryFast)
    }
}

/// 通用「在途结果回收」：遍历 `pending` 各条目，经 `recv` 投影做非阻塞 `try_recv`，
/// 聚合已就绪的 `(key, res)`。抽自原 4 个 `collect_*_results` 的近同构 try_recv 循环；
/// 各密度的守卫 / 落槽语义由调用方拿到列表后自行处理（在本函数外，逐类保留）。
fn collect_finished<K, V, R>(
    pending: &mut HashMap<K, V>,
    recv: impl Fn(&mut V) -> Result<R, mpsc::TryRecvError>,
) -> Vec<(K, R)>
where
    K: Copy,
{
    pending
        .iter_mut()
        .filter_map(|(k, v)| match recv(v) {
            Ok(res) => Some((*k, res)),
            Err(_) => None,
        })
        .collect()
}

// 职责子模块（2026-10 由单文件 `view.rs` 拆出，纯 move；私有方法经 `pub(super)`
// 保持视图子树内可见，公共 API / 可见性语义不变，见各模块头部文档）。
mod open;
mod input;
mod tiles;
mod search;
mod render;

/// R1-R3 动态瓦片预算与钉住集的**本帧快照**（由 `update_tile_budget_and_degrade` 计算、
/// `evict_tiles` 消费——同一帧两处共用同一几何，杜绝「预算算一套、淘汰用另一套」漂移）：
/// - `budget`：本帧有效预算 = `max(固定, 钉住需求×1.25)`、夹在 `TILE_CACHE_BYTES` 与
///   `TILE_CACHE_BYTES_MAX` 之间；
/// - `pinned`：**不可淘汰**的当前钉住格集 = 各可见页 `visible_tile_range(可见 bbox +
///   TILE_MARGIN)` 覆盖的格子（R1 逐出跳过、R3 预算天然包住）；
/// - `center`：可见中心页（淘汰排名 anchor，与调度派发优先级同源）；
/// - `mid_rows`：各可见页可见行中心（R2 淘汰排名 = `|page−center|×1e6+|row−mid|`）。
#[derive(Default)]
pub(crate) struct TileBudgetState {
    pub(crate) budget: u64,
    pub(crate) center: usize,
    pub(crate) pinned: std::collections::HashSet<(usize, u32, u32)>,
    pub(crate) mid_rows: std::collections::HashMap<usize, u32>,
}

/// PDF 阅读器：自包含的瓦片级虚拟化视图。
pub struct PdfViewer {
    /// 文件路径（含文件名，标题用）。
    pub(crate) path: PathBuf,
    /// 进程级 pdfium 工作线程句柄（加载完成后为 `Some`）。
    pub(crate) worker: Option<PdfWorker>,
    /// 本阅读器在该工作线程中打开的文档编号。
    pub(crate) doc_id: DocId,
    /// 每页 (宽,高)（PDF 点）。
    pub(crate) dims: Vec<(f32, f32)>,
    /// 每页 top 偏移（`compute_layout` 结果，渲染定位 + 可见区推算共用）。
    pub(crate) page_tops: Vec<f32>,
    /// 整列总高（含上下内边距）。
    pub(crate) total_height: f32,
    /// 每页渲染槽（含瓦片缓存 + 缩略/中清占位）。
    pub(crate) slots: Vec<Slot>,
    /// 在途 **L0 缩略图**请求的结果接收通道（独立于瓦片额度）。
    /// L2.4：条目为 `(导航世代 nav_gen, 结果通道)`，大跳世代推进时按 `nav_gen` 清旧条目。
    pub(crate) pending_thumb: HashMap<usize, (u64, mpsc::Receiver<Option<RenderResult>>)>,
    /// 在途 **L0.5 中清页**请求的结果接收通道（快速拖动占位）。
    /// L2.4：条目为 `(导航世代 nav_gen, 结果通道)`，大跳世代推进时按 `nav_gen` 清旧条目。
    pub(crate) pending_mid: HashMap<usize, (u64, mpsc::Receiver<Option<RenderResult>>)>,
    /// 中清页内存 LRU 顺序（最近使用在队尾）。纹理本体就在 `slots[i].mid`
    /// （O(1) 主线程查找），本队列只用于淘汰顺序。
    pub(crate) mid_lru: VecDeque<usize>,
    /// 在途**瓦片**请求：`(页, row, col)` → (请求时密度 scale, 导航世代 nav_gen, 结果通道)。
    /// `scale` 用于结果到达时的密度守卫：缩放/换 DPI 后旧密度瓦片直接丢弃。
    /// `nav_gen` 用于 L2.4：大跳世代推进时按世代清旧条目（预算立即释放，不占死 COVER_INFLIGHT）。
    pub(crate) pending_tiles: HashMap<(usize, u32, u32), PendingTileEntry>,
    /// 在途**封面**（首屏整页锐化）请求：`页` → (密度 scale, **请求时 recolor 世代**,
    /// **导航世代 nav_gen**, 结果通道)。recolor 世代用于 `collect_cover_results` 的失败计数
    /// 语义（旧世代请求的失败不拉黑，评审 NIT）；`nav_gen` 用于 L2.4 大跳世代推进清旧条目。
    pub(crate) pending_cover: HashMap<
        usize,
        (f32, u64, u64, mpsc::Receiver<Option<RenderResult>>),
    >,
    /// F4 空闲预取走廊在途请求：`页` → (半密度 scale, 导航世代 nav_gen, 结果通道)。独立于
    /// [`Self::pending_cover`]（密度守卫不同：0.5× 封面、低优先级、仅空闲期消化），见
    /// `tiles::collect_prefetch_cover_results` 与 `tiles::prefetch_corridor`。非空期间自刷新泵
    /// 继续出帧以收集走廊结果。`nav_gen` 用于 L2.4 大跳世代推进清旧条目。
    pub(crate) pending_prefetch_cover: HashMap<
        usize,
        (f32, u64, mpsc::Receiver<Option<RenderResult>>),
    >,
    /// F4 观测计数：累计向 worker 派发的**走廊封面**栅格请求数（回归断言 idle 预取确实
    /// 发生、且窗口移动后不再增长用；纯观测，无副作用）。
    pub(crate) prefetch_reqs: u64,
    /// F4 观测计数：「第 N 页 加载中…」占位分支的实际命中次数（`render::page_base_image`
    /// 全空时才自增；`release` 时归零）。回归断言「常态主路径不再让可见页落加载态」用。
    /// `Cell<u64>`：渲染路径只持 `&Self`（`build_visible_page_cards`），内部可变性计数。
    pub(crate) loading_placeholder_hits: std::cell::Cell<u64>,
    /// S 系列「原子换」观测探针：**最近一帧**实际挂入页卡 DOM 的瓦片 img 元素总数
    /// （`render::PdfViewer::build_visible_page_cards` 每帧覆写：门控通过 = 本帧瓦片数、
    /// 未通过/无瓦片 = 0）。供回归断言「旧密度底图 + 部分新密度瓦片」同帧不混叠——
    /// 部分瓦片在场时本值必须为 0（整组一次换），补齐整组后本值 == 可见组大小。
    pub(crate) last_frame_tiles_drawn: std::cell::Cell<usize>,
    /// 在途**重着色刷新**结果接收通道（`set_recolor` 时向 worker 请求）：worker 把已缓存
    /// 原图现算新着色副本逐条回传，本通道 drain 完（断开）即清。驱动一次「切档不重栅」
    /// 的显示刷新；非 None 期间自刷新泵持续出帧直到收完（防晚到结果无帧可收集）。
    pub(crate) pending_recolor: Option<mpsc::Receiver<RecolorEntry>>,
    /// 本阅读器累计向 worker 派发的**栅格类请求数**（Render/RenderTile/Mid/Thumb）。
    /// 纯回归计数（无副作用）：`切档不重栅`测试断言「重着色切换前后该计数不变」。
    pub(crate) raster_reqs: u64,
    /// 最近一次重着色刷新请求的**可见集 scope**（view 侧定义，按近中心页排序；worker
    /// 按其 key 逐条现算回传）。回归断言「切档只刷可见集」：收到的 key 集合 == 本 scope。
    pub(crate) recolor_scope: Vec<RecolorKey>,
    /// 回归计数：本次重着色刷新**收到**的全部条目 key（`(kind, page, row, col)`，含被
    /// 密度/槽位守卫丢弃的——计数的是 worker 回传条目的集合）。每次新刷新开始时置空。
    pub(crate) recolor_received: Vec<(RawKind, usize, u32, u32)>,
    /// T1：仅**瓦片**栅格的累计派发计数（区分封面/占位）。回归断言「封面 affordable 的
    /// 首屏不派同页瓦片」用：封面先行时瓦片系计数应为 0，而 `raster_reqs` ≥ 封面数。
    pub(crate) tile_reqs: u64,
    /// T1：仅**封面**栅格的累计派发计数（与 `tile_reqs` 共同给出首屏「栅格命令构成」）。
    pub(crate) cover_reqs: u64,
    /// 当前已驻留瓦片的估算字节（近似 RGBA 内存；淘汰时重算自愈漂移）。
    pub(crate) tile_bytes: u64,
    /// R1-R3 **动态瓦片预算/钉住集状态**（本帧有效值，由 `open::ensure_window` 每帧经
    /// [`crate::pdf::view::tiles::PdfViewer::update_tile_budget_and_degrade`] 计算、`evict_tiles`
    /// 消费）：`None` 仅在未走 `ensure_window` 的直接调用路径（如单测）出现 → 回退固定
    /// 32MiB 预算 + 无钉住 + 绝对行号排名（旧语义），保证既有测试语义不变。
    pub(crate) tile_budget_state: Option<TileBudgetState>,
    /// R4 均匀降级**候选页计数器**（页 → 连续「静态 + 可见组不齐 + 预算触硬顶」帧数；
    /// 达 [`config::pdf::DEGRADE_STATIC_FRAMES`] 即 degrade 激活并移入 [`Self::degraded`]）。
    pub(crate) degrade_frames: HashMap<usize, u32>,
    /// R4 当前处于**均匀 0.5 降级**的页集合（`Slot.tiles` 已清、不再派瓦片/封面、底图由
    /// 0.5 密度 preview 提供 = 「宁均匀不格子」）。帧不再触顶/滑出可见窗即清除并自愈。
    pub(crate) degraded: HashSet<usize>,
    /// R4 动态预算**硬顶覆写**（仅测试钩子，`None` = 用 [`config::pdf::TILE_CACHE_BYTES_MAX`]）：
    /// 供 headless 回归构造「预算触顶 + 组不齐」场景（A3），无需真实超大视口/病理性大页。
    pub(crate) tile_budget_cap_override: Option<u64>,
    /// 瓦片自刷新泵是否在途：派发瓦片后若无用户输入就不会再有重绘，而瓦片结果要等
    /// 下一次 `ensure_window` 才收集拼图——此泵每 ~16ms 触发一次重绘直到在途瓦片清空。
    pub(crate) tile_pump_running: bool,
    /// 上一帧滚动偏移（预测滚动方向用：delta>0 正在往下滚 → 预取下方页）。
    pub(crate) last_scroll_y: f32,
    /// 滚动方向（F2 封面单向预取用）：1=向下（页号增大）、-1=向上、0=方向未知
    /// （初始未滚过 / 静止时保持上一方向，见 `ensure_window` 的递推）。
    pub(crate) scroll_dir: i8,
    /// 滚动速度估计（px/帧，EMA)：> [`MOVE_THRESHOLD_PX`] 视为快速拖动 →
    /// 改派缩略/中清占位、暂停瓦片派发（避免小片洪流），停止后瓦片立即补齐。
    pub(crate) scroll_velocity: f32,
    /// 迟滞判定后的"快速拖动"标志（进入 v>阈值、退出 v<阈值×比率，中间保持）。
    pub(crate) prev_fast: bool,
    /// 滚动速度分带（F3）：决定每帧调度形态（常态瓦片 / 中速封面 / 高速纯平移）。
    /// `prev_fast` = `speed_state.is_fast()` 的派生布尔（两者由 `update_fast_flag` 同步更新）。
    pub(crate) speed_state: SpeedState,
    /// L1/L2：**导航世代**（唯一真源在 view；worker 侧经 [`WorkerMsg::Navigate`] 消息镜像，
    /// 不自行递增）。大跳（见 `update_nav_jump`）每帧至多 +1，当帧清掉世代 < 新 gen 的全部
    /// 在途条目（预算立即释放）、并让 worker 出队时跳过旧世代请求——快速连续跳只渲染最新
    /// 落点，消灭「拖完等 EMA 衰减 150-250ms 才调度」的空窗。非大跳的常态滚动保持 0 不影响
    /// 任何既有路径。与 `recolor_epoch` 正交（世代推进不清黑名单，W2 语义不变）。
    pub(crate) nav_gen: u64,
    /// L1.1：大跳派发已武装（VeryFast 高速带跳转到落点页满密度派发封面的开关）。
    /// 置位后 `dispatch_jump_covers` 以 `jump_target` 为窗派封面；落点窗封面全部在槽/在途或
    /// 速度离开 VeryFast 后清零。快速连续跳/变向时每帧由 `update_nav_jump` 重算（后到覆盖
    /// 先到，无积压队列）。
    pub(crate) jump_armed: bool,
    /// L1.1：当前大跳落点中心页（`update_nav_jump` 置位时 = 当前可见中心；快速连续跳重算）。
    pub(crate) jump_target: Option<usize>,
    /// L1.1：上一帧可见中心页（大跳检测基线；`None` = 首帧，不触发大跳）。
    pub(crate) last_center_page: Option<usize>,
    /// L1.3：停稳检测的连续「小位移」帧计数（|Δy| < [`config::pdf::STOP_DETECT_PX`] 时递增、
    /// 否则清零；达 [`config::pdf::STOP_DETECT_FRAMES`] 即直落 Slow 绕过 EMA 衰减）。
    pub(crate) stop_small_frames: u32,
    /// S1/S3：缩放派发已武装（Ctrl+滚轮 [`super::view::input::PdfViewer::adjust_zoom`] 置位）。
    /// 为 true 期间（缩放防抖窗内）调度只按**当前 cscale** 派 ≤[`config::pdf::PRE_FETCH_COVER_INFLIGHT`]
    /// 枚 0.5 密度 preview（绝不满密 cover/tile 逐档抢渲被弃空转）；连续
    /// [`config::pdf::ZOOM_DEBOUNCE_FRAMES`] 帧无新缩放事件后清零，落常态满密收口。
    /// 与 `jump_armed` 各自独立、互不覆盖（缩放 + 大跳复合时大跳优先）。
    pub(crate) zoom_armed: bool,
    /// S3：缩放防抖计数（连续无新缩放事件的帧数，`ensure_window` 每帧自增；`adjust_zoom` 置零）。
    pub(crate) zoom_idle_frames: u32,
    /// Z2：**缩放世代号**（view 唯一真源）。每次缩放新档（`adjust_zoom` 生效）`+= 1` 并推送
    /// `worker.set_zoom_epoch`（worker 侧按文档采纳为镜像）；render 类请求入队时 stamp 本值，
    /// worker 出队时 `stamp < mirror[doc]` → 直接 drop 不渲——缩放**中间档** cover/瓦片/preview
    /// 不再排队空耗 pdfium 时隙（Z2 队列堆叠递归根治）。与 `nav_gen` **独立**（滚动/大跳时代
    /// 不变，防浊化导航世代语义）；嵌用户可见缩放的补帧/钳位不推进。
    pub(crate) zoom_epoch: u64,
    /// #5 高倍收口瓦片一次性出清（S5/E）：`zoom_armed` 清空的当帧置位，`schedule_tiles`
    /// 据此把单一 [`config::pdf::TILE_INFLIGHT`]=6 临时抬到 [`config::pdf::SETTLE_TILE_BURST_MAX`]
    /// 一次性入队可见瓦片（旧中间档已由 Z2 清）。只抬**在途**不增驻留；普通滚动恒 false。
    pub(crate) settle_tile_burst: bool,
    /// #5 观测计数：收口突发帧净超常规 [`config::pdf::TILE_INFLIGHT`] 的部分派发瓦片数
    /// （>0 ⇔ burst 生效；headless 断言「settle 出清预算到位」的确定性证据，不依赖 worker）。
    pub(crate) settle_tile_burst_dispatch: u64,
    /// 竖向滚动句柄（页列外层 scroll 容器）。
    pub(crate) scroll: ScrollHandle,
    /// 异步文档加载状态（`loading` + `load_error`）。数据本体是多字段的
    /// （worker / doc_id / dims / page_tops / …），故用 `AsyncState<()>` 只复用
    /// 转移 helper 的加载/错误两字段；`data` 槽不标记已加载——「已加载」由
    /// [`Self::is_loaded`] 按 `!loading && worker.is_some() && !dims.is_empty()` 自证，
    /// 与旧 `loading` / `load_error` 两字段语义逐字一致（Pattern 2.1 收敛）。
    pub(crate) state: ui::AsyncState<()>,
    /// 阅读画布背景（页面“纸面”之外底色；None 用主题推导）。
    pub(crate) bg: Option<Hsla>,
    /// 纸墨重着色（页面**位图显示**层；None = 直通无重着色）。与 worker 侧 `SetRecolor`
    /// 状态一致（view 在 set 时推送，worker 就位时再补推一次）。
    pub(crate) recolor: Option<RecolorParams>,
    /// W1：**重着色世代号**——`set_recolor` 每次**实际变更**参数时 +1（含 worker 未就绪
    /// 的早期 set，与 `apply_loaded` 的补推无关）。CPU 回退路径下 `collect_tile_results`/
    /// `collect_cover_results` 丢弃「回传世代 ≠ 当前」的在途结果（worker 在出队渲染时按
    /// 当时 recolor 着色、结果经独立 mpsc 回传，与 SetRecolor/RecolorRefresh 无总体次序）：
    /// 旧色重栅结果晚到即被丢弃，杜绝切档后旧色调覆盖新参数。GPU 门控下结果恒为 raw
    /// （着色在绘制时），guard 跳过无开销。构造 0；与 worker 侧同源（通道 FIFO：worker
    /// 世代 ≤ 本值、稳态相等）。
    pub(crate) recolor_epoch: u64,
    /// W2：**封面失败计数**（按页，CPU/GPU 通用路径）——`collect_cover_results` 在封面
    /// 失败（`res=None` / 建图失败）时 +1，连续达 [`crate::pdf::view::tiles`] 的
    /// `COVER_FAIL_BLACKLIST_THRESHOLD` 后把该页加入 [`Self::cover_blacklist`]；封面成功
    /// 或该页滚出封面窗（`evict_covers`）时复位。纯状态、不触 worker。
    pub(crate) cover_failures: HashMap<usize, u32>,
    /// W2：**封面禁用集合**——持续失败的页加入后，`plan_tiles_in_window` 的 skip_covered
    /// 对该页失效（回退瓦片供给，防「封面持续失败 → 永久占位/模糊」）、`schedule_covers`
    /// 停止盲目重试封面（直到封面成功或滚出封面窗复位）。
    pub(crate) cover_blacklist: HashSet<usize>,
    /// T4 GPU 重着色门控（构造时从进程级开关读入）：开启时编辑器把 raw 直传 + 图片元素
    /// 携带 `img().recolor(...)` 参数，着色在 **GPU 绘制时**由片元着色器完成（切档只改
    /// 参数、0 重算 0 重栅）；关闭时行为与 dbd3236 逐字节一致（worker CPU 着色路径照常）。
    pub(crate) gpu_recolor: bool,
    /// 待释放的旧位图/旧瓦片（替换或回收的，需在下一帧 `drop_image`）。
    pub(crate) pending_drops: Vec<Arc<RenderImage>>,
    /// 书签目录（标题 + 层级 + 目标页码）；无书签时为 `[]`。供侧边栏目录使用。
    pub(crate) outlines: Vec<PdfOutlineEntry>,
    /// 键盘焦点句柄（点选后聚焦，`Ctrl+C` 复制）。
    pub(crate) focus_handle: FocusHandle,
    /// 页文本层缓存（懒加载，LRU 上限 [`TEXT_CACHE_PAGES`]；选中/复制/命中测试用）。
    pub(crate) text_pages: HashMap<usize, PageText>,
    /// 文本层 LRU 顺序（最近使用在队尾）。
    pub(crate) text_lru: VecDeque<usize>,
    /// 在途文本请求（worker 回传通道）。
    pub(crate) pending_text: HashMap<usize, mpsc::Receiver<Option<PageText>>>,
    /// 页**图片盒**缓存（懒加载，LRU 上限 [`IMAGE_CACHE_PAGES`]；点击/右键命中图片 +
    /// 「复制图片」取数用）。`Vec<PdfImageBox>` 空 = 该页无 raster 图片。
    pub(crate) image_pages: HashMap<usize, Vec<PdfImageBox>>,
    /// 图片盒 LRU 顺序（最近使用在队尾）。
    pub(crate) image_lru: VecDeque<usize>,
    /// 在途图片盒请求（worker 回传通道）。
    pub(crate) pending_images: HashMap<usize, mpsc::Receiver<Option<Vec<PdfImageBox>>>>,
    /// 当前选中的图片：`(页号, 对象下标)`（`image_pages` 内条目）。左键单击命中图片
    /// 置位（青色描框高亮，与文本选区蓝/注释灰框区分）；点非图片清空。`None` = 未选中。
    pub(crate) selected_image: Option<(usize, usize)>,
    /// 选区锚点（鼠标按下点：(页号, 字符索引)）。松手后保留（作为选区一端）。
    pub(crate) sel_anchor: Option<(usize, usize)>,
    /// 选区活动端（拖拽中随鼠标更新）。
    pub(crate) sel_active: Option<(usize, usize)>,
    /// 是否正在拖拽选区（鼠标按下未松开）；松手置 false，但选区本身保留。
    pub(crate) selecting: bool,
    /// P3 检索：当前查询（由侧边栏搜索框驱动；trim 后用于检索）。
    pub(crate) search_query: String,
    /// 文档级检索命中（页升序）。
    pub(crate) search_matches: Vec<PdfSearchHit>,
    /// 检索是否因命中过多被截断。
    pub(crate) search_truncated: bool,
    /// 当前激活命中下标。
    pub(crate) search_active: usize,
    /// 在途检索结果接收通道（代次过期即弃）。
    pub(crate) search_pending: Option<mpsc::Receiver<Option<PdfSearchResults>>>,
    /// 在途检索对应的调度代次。
    pub(crate) search_pending_seq: u64,
    /// 检索调度代次（防抖：仅最新一次生效）。
    pub(crate) search_seq: u64,
    /// 页面缩放倍数（Ctrl+滚轮可调；页面显示宽度 = `base_w * zoom`）。
    pub(crate) zoom: f32,
    /// **自适应窗口宽**的页宽基线：= 滚动容器可视宽 − 两侧页边距（容器未布局时回退
    /// `PAGE_WIDTH`）。窗口 / 侧边栏拉宽拉窄 → 每帧 [`open`] 的 `ensure_window` 重算
    /// 基线并重排页列，页面随之缩放；Ctrl+滚轮在基线上再乘 `zoom` 倍率。
    pub(crate) base_w: f32,
    /// 当前窗口缩放密度（设备 px / CSS px，f32 位型）：`ensure_window` 每帧从窗口刷新。
    /// `page_width()` 用它把页宽量化到设备像素网格（`round(page_width·sf)/sf`），使
    /// `page_width·sf` 恒为整数 → 位图宽 `ceil(page_width·sf)` 与显示矩形（GPUI 绘制时
    /// `snap_bounds` 取整）逐设备像素 1:1，消灭「同一张正确密度位图被双线性重采样发糊」
    /// 的缩放相位（未布局/测试默认 1.0）。
    pub(crate) scale_factor: f32,
    /// 静态帧门控签名（Sprint 5）：上一非门帧的变化签名（[`WindowSig`]）；与当前帧
    /// 一致是「静置候选」的前提，不一致/首帧（`None`）绝不门。供 `ensure_window`
    /// 门控判定，另供 pdf::tests 回归测试断言「静置 → 滚动恢复」转换。
    pub(crate) window_sig: Option<WindowSig>,
    /// 上一帧是否已「静置确认」：非快速态做过一次常态调度、且调度后无在途瓦片/封面
    /// （即确认过「没有待派的新 tile 请求」）。门控必要条件——只有确认过无待派才允许
    /// 静态帧跳过调度（防「快速拖动刚停 / 首帧」等尚未补调的场景出现贴图迟滞）。
    pub(crate) window_settled: bool,
    /// 已被静态帧门控跳过的帧计数（Sprint 5）：门控分支每命中一次 +1。纯观测计数
    /// （静态跳过分支每次 ~1ns），供回归测试断言「静置帧确实跳过调度、变化帧不门」。
    pub(crate) idle_gate_skips: u64,
    /// 重载锚点：写回成功后重开文档应恢复到的目标页（0 基；None=维持当前第一可见页）。
    /// 由 [`super::open::PdfViewer::submit_modify`] 在提交时按操作语义设定，`start_load`
    /// 打开成功铺槽后消费（插入保持在原页之后 / 删除夹取到合法页）。
    pub(crate) reload_anchor: Option<usize>,
    /// 「阅读进度恢复」锚点：打开时由外壳写入 `(页下标 0 基, 页内比例, zoom)`，
    /// [`super::open::PdfViewer::apply_loaded`] 装配后消费（设 zoom → 按当次布局滚到
    /// 页内比例位置）。与 [`Self::reload_anchor`] 正交：前者用于「重开回到上次阅读
    /// 位置」，后者用于「写回后保持重载页」。`None` = 无恢复（回顶部）。
    pub(crate) restore_anchor: Option<(usize, f32, f32)>,
    /// 「阅读进度恢复」待落地滚动：`apply_loaded` 消费 `restore_anchor` 后把 `(页, 比例)`
    /// 暂存于此；**首个 `ensure_window` 在 base_w 按可视宽 refit 之后**施加（首帧真布局
    /// 落点——`apply_loaded` 时 base_w 还是 `PAGE_WIDTH` 回退值，非真实可视宽，若当时就该
    /// 落 scroll 会按错档 tops 算页）。施加后置 `None`。
    pub(crate) restore_pending: Option<(usize, f32)>,
    /// 最近一次写回/页操作失败的文案（成功或未操作时为 None）。Phase B 弹卡/提示用。
    pub(crate) last_modify_error: Option<String>,
    /// Phase B1：选中文字后自动弹出的操作卡片（`None` = 未弹出）。
    pub(crate) sel_card: Option<SelCard>,
    // ---- 待保存模型（P1）：内存待存注释 / 撤销栈 / 页面操作待存 / 灰框焦 ----
    /// 待保存注释列表（内存叠加层渲染；`save()` 时合并写盘后清空）。
    pub(crate) pending_annots: Vec<PendingAnnot>,
    /// 待保存撤销栈（尾部 = 最近；Ctrl+Z 弹尾部执行逆操作；save 后清空）。
    pub(crate) undo_stack: VecDeque<UndoEntry>,
    /// 页面结构操作计数（插入/删除在途即 +1；撤销复原 -1；save 后清零）。
    /// dirty = `注释非空 || 页操作计数 != 0` 的派生源，见 [`Self::is_dirty`]。
    pub(crate) pending_page_ops: usize,
    /// 注释 id 自增计数器（[`PendingAnnot::id`] 分配用）。
    pub(crate) next_annot_id: u64,
    /// 当前点击命中的「灰框」注释 id（点击注释文字显示其包围灰框；右键 → 菜单
    /// 含「删除注释」）。`None` = 无灰框。
    pub(crate) focused_annot: Option<u64>,
    // ---- 超宽平移：放大后内容宽于视口时左键拖拽 = 平移（见 `horizontal_overflow`）----
    /// 是否正在「平移页面」：横向溢出（放大后内容宽于视口）时左键拖拽 = 平移，
    /// 与「选中文本」互斥（行为表见 [`input::PdfViewer::on_mouse_down`]）。
    pub(crate) panning: bool,
    /// 平移起始的指针**窗口坐标**（按下点；拖拽期间 `offset = pan_offset0 + 指针位移`）。
    pub(crate) pan_origin: gpui::Point<Pixels>,
    /// 按下瞬间的滚动偏移快照（用于平移减法）。
    pub(crate) pan_offset0: gpui::Point<Pixels>,
}

impl PdfViewer {
    /// A1 收敛十字门辅助（仅测试）：逐可见页返回 `(page, missing, total)`……
    #[cfg(test)]
    pub(crate) fn visible_tile_presence(
        &self,
        scale_factor: f32,
        viewport_h: f32,
        window: &gpui::Window,
    ) -> Vec<(usize, u32, u32)> {
        use crate::pdf::layout as L;
        let pw = self.page_width();
        let sy = f32::from(-self.scroll.offset().y);
        let wrap_w = self.wrap_width(window, pw);
        let mut out = Vec::new();
        for (page, dim) in self.dims.iter().enumerate() {
            let scale = L::render_scale(pw, scale_factor, dim.0);
            let bmp = L::page_bitmap_size(*dim, scale);
            let disp_h = L::display_height(*dim, pw);
            let Some(rect) = self.page_visible_rect(page, sy, viewport_h, pw, wrap_w) else {
                continue;
            };
            let Some((r0, c0, r1, c1)) = L::visible_tile_range_axis(
                bmp,
                pw,
                disp_h,
                rect,
                TILE_SIZE,
                TILE_MARGIN,
                config::pdf::TILE_MARGIN_COL,
            ) else {
                continue;
            };
            let total = (r1 - r0 + 1) * (c1 - c0 + 1);
            let have = (r0..=r1)
                .flat_map(|row| (c0..=c1).map(move |col| (row, col)))
                .filter(|&(row, col)| self.slots[page].tiles.contains_key(&(row, col)))
                .count();
            out.push((page, total - have as u32, total));
        }
        out
    }
}

// 实现（impl）按职责拆分至 `view/<sub>.rs` 子模块：
// - `view/open.rs`：new / start_load / 状态访问器 / release / ensure_window
// - `view/input.rs`：滚轮缩放 / hit_test / 鼠标键盘 / 选区复制
// - `view/tiles.rs`：瓦片调度 / 占位 / 回收 / 内存预算
// - `view/search.rs`：文本缓存 / 全文检索
// - `view/render.rs`：Render impl + 页列虚拟化 + 高亮叠加

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::AppContext as _;
    use gpui::TestAppContext;

    /// 纯逻辑复现 minimal `PdfViewer`（**不触碰 pdfium**：不走 [`Self::new`]，无真实 PDF）。
    /// 只喂一页占位 dims + 初始滚动偏移，验证 `adjust_zoom` 的四件事：
    /// 1) 一档 = `1 + ZOOM_STEP`；2) 上下钳到 [`ZOOM_MIN`] / [`ZOOM_MAX`]（config::pdf 区间）；
    /// 3) 缩放后滚动偏移同比重锚（`offset *= new_w / old_w`）；4) delta=0（factor=1）不变换。
    #[gpui::test]
    async fn ctrl_zoom_clamps_and_reanchors_scroll(cx: &mut TestAppContext) {
        let viewer: gpui::Entity<PdfViewer> = cx.update(|app| {
            app.new(|cx| {
                let dims = vec![(612.0, 792.0)];
                let (tops, total) = compute_layout(&dims, PAGE_WIDTH);
                let v = PdfViewer {
                    path: "dummy.pdf".into(),
                    worker: None,
                    doc_id: 0,
                    dims: dims.clone(),
                    page_tops: tops,
                    total_height: total,
                    slots: Vec::new(),
                    pending_thumb: HashMap::new(),
                    pending_mid: HashMap::new(),
                    mid_lru: VecDeque::new(),
                    pending_tiles: HashMap::new(),
                    pending_cover: HashMap::new(),
                    pending_prefetch_cover: HashMap::new(),
                    prefetch_reqs: 0,
                    loading_placeholder_hits: std::cell::Cell::new(0),
                    last_frame_tiles_drawn: std::cell::Cell::new(0),
                    pending_recolor: None,
                    raster_reqs: 0,
                    recolor_scope: Vec::new(),
                    recolor_received: Vec::new(),
                    tile_reqs: 0,
                    cover_reqs: 0,
                    tile_bytes: 0,
                    tile_budget_state: None,
                    degrade_frames: HashMap::new(),
                    degraded: HashSet::new(),
                    tile_budget_cap_override: None,
                    tile_pump_running: false,
                    last_scroll_y: 0.0,
                    scroll_dir: 0,
                    scroll_velocity: 0.0,
                    prev_fast: false,
                    speed_state: SpeedState::Idle,
                    nav_gen: 0,
                    jump_armed: false,
                    jump_target: None,
                    last_center_page: None,
                    stop_small_frames: 0,
                    zoom_armed: false,
                    settle_tile_burst: false,
                    settle_tile_burst_dispatch: 0,
                    zoom_idle_frames: 0,
                    zoom_epoch: 0,
                    scroll: ScrollHandle::new(),
                    state: ui::AsyncState::idle(),
                    bg: None,
                    recolor: None,
                    recolor_epoch: 0,
                    cover_failures: HashMap::new(),
                    cover_blacklist: HashSet::new(),
                    gpu_recolor: crate::recolor::gpu_recolor_gate(),
                    pending_drops: Vec::new(),
                    outlines: Vec::new(),
                    focus_handle: cx.focus_handle(),
                    text_pages: HashMap::new(),
                    text_lru: VecDeque::new(),
                    pending_text: HashMap::new(),
                    image_pages: HashMap::new(),
                    image_lru: VecDeque::new(),
                    pending_images: HashMap::new(),
                    selected_image: None,
                    sel_anchor: None,
                    sel_active: None,
                    selecting: false,
                    search_query: String::new(),
                    search_matches: Vec::new(),
                    search_truncated: false,
                    search_active: 0,
                    search_pending: None,
                    search_pending_seq: 0,
                    search_seq: 0,
                    zoom: 1.0,
                    base_w: PAGE_WIDTH,
                    scale_factor: 1.0,
                    window_sig: None,
                    window_settled: false,
                    idle_gate_skips: 0,
                    reload_anchor: None,
                    restore_anchor: None,
                    restore_pending: None,
                    last_modify_error: None,
                    sel_card: None,
                    pending_annots: Vec::new(),
                    undo_stack: VecDeque::new(),
                    pending_page_ops: 0,
                    next_annot_id: 0,
                    focused_annot: None,
                    panning: false,
                    pan_origin: gpui::point(px(0.), px(0.)),
                    pan_offset0: gpui::point(px(0.), px(0.)),
                };
                // 先滚到文档中段（纵向负偏移），作为缩放重锚的基准。⚠️ 须落在新布局
                // max_offset 内（600 < 放大一档后的 `total_new−viewport` ≈ 1249）——越界
                // 起点的 B 场景由 #2 clamp 处理（见 lib.rs `zoom_bottom_clamp_*` 专测），
                // 这里锁的是「锚点页重锚分毫不差」的 Z1 公式（clamp 后公式不再可见）。
                v.scroll.set_offset(gpui::point(px(-40.0), px(-600.0)));
                assert!((v.total_height - total).abs() < 1e-3, "build 完 total_height");
                v
            })
        });

        // 一档放大：zoom / 整列高按 1+ZOOM_STEP 走，滚动偏移**锚点页重锚**（Z1：y 按
        // 锚点页 K 页顶线性缩放 + 页内相对偏移同比；常量区 PAGE_PAD/GAP 不随缩放乘因子——
        // 旧实现整段 ×ratio 对 K≥1 有按页累计漂移，深页跳 2-3 行高）。
        cx.update(|app| {
            viewer.update(app, |v, cx| {
                let w0 = v.page_width();
                let x0 = v.scroll.offset().x.as_f32();
                let y0 = v.scroll.offset().y.as_f32();
                let sy0 = -y0; // 滚动内容位置（≥0）
                let tops0 = v.page_tops.clone();
                let total0 = v.total_height;
                v.adjust_zoom(pan_zoom::zoom_factor(1.0, ZOOM_STEP), cx);
                let ratio = v.page_width() / w0;
                assert!((v.zoom - 1.12).abs() < 1e-6, "1 档放大 zoom={}", v.zoom);
                let k = first_visible_page(&v.dims, &tops0, w0, sy0);
                let want_sy = v.page_tops[k] + (sy0 - tops0[k]) * ratio;
                assert!(
                    (v.scroll.offset().x.as_f32() - x0 * ratio).abs() < 1e-3
                        && (v.scroll.offset().y.as_f32() + want_sy).abs() < 1e-2,
                    "放大应锚点页重锚（Z1）：x0={x0} x={} y0={y0} y={} want_y={}",
                    v.scroll.offset().x.as_f32(),
                    v.scroll.offset().y.as_f32(),
                    -want_sy
                );
                assert!(v.total_height > total0, "整列高随放大增大");
            })
        });

        // delta=0（factor=1）：缩放不变、不重锚
        cx.update(|app| {
            viewer.update(app, |v, cx| {
                let x_before = v.scroll.offset().x.as_f32();
                let y_before = v.scroll.offset().y.as_f32();
                v.adjust_zoom(1.0, cx);
                assert!((v.zoom - 1.12).abs() < 1e-6, "delta=0 缩放不变");
                assert_eq!(v.scroll.offset().x.as_f32(), x_before, "delta=0 不重锚 x");
                assert_eq!(v.scroll.offset().y.as_f32(), y_before, "delta=0 不重锚 y");
            })
        });

        // 反复放大 → 钳到上限
        for _ in 0..200 {
            cx.update(|app| {
                viewer.update(app, |v, cx| {
                    v.adjust_zoom(pan_zoom::zoom_factor(1.0, ZOOM_STEP), cx);
                })
            });
        }
        assert_eq!(
            cx.update(|app| viewer.read(app).zoom),
            ZOOM_MAX,
            "放大到上限"
        );
        // 反复缩小 → 钳到下限
        for _ in 0..200 {
            cx.update(|app| {
                viewer.update(app, |v, cx| {
                    v.adjust_zoom(pan_zoom::zoom_factor(-1.0, ZOOM_STEP), cx);
                })
            });
        }
        assert_eq!(
            cx.update(|app| viewer.read(app).zoom),
            ZOOM_MIN,
            "缩小到下限"
        );
    }

    /// 缩放期防闪底图选择（纯逻辑，封住 [`render::page_base_image`] 的优先级）：
    /// 缩放瞬间「当前密度封面未就绪（`cover_now=None`）× 旧密度封面仍在（`prev_cover`）」
    /// 时，底图必须是**旧封面**（CSS 拉伸兜底）而非 `None`——`None` 正是「第 N 页 加载中…」
    /// 闪白的根因。旧封面缺位才回落到中清/缩略；四者全空（首开/极速拖动）才允许落占位。
    #[test]
    fn zoom_transition_keeps_old_cover_as_base_not_placeholder() {
        let mk = |ch: u8| -> std::sync::Arc<gpui::RenderImage> {
            render_image_from_bgra(vec![ch, ch, ch, 255], 1, 1).expect("构造 1×1 测试位图")
        };
        let cur_cover = mk(0x20);
        let old_cover = mk(0x30); // 缩放前驻留的旧密度封面
        let mid = mk(0x40);
        let thumb = mk(0x50);

        // 缩放过渡中：新密度封面还没回来（cover_now=None）→ 底图 = 旧封面，而非 None。
        let base = super::render::page_base_image(
            None,
            Some(&old_cover),
            None,
            Some(&mid),
            Some(&thumb),
        );
        let base = base.expect("缩放过渡期只要有旧资产就不能落「加载中」");
        assert!(
            std::sync::Arc::ptr_eq(&base, &old_cover),
            "旧密度封面应优先于中清/缩略作底图"
        );

        // 当前密度封面就绪（锐利）→ 优先于旧封面。
        let base2 = super::render::page_base_image(
            Some(&cur_cover),
            Some(&old_cover),
            None,
            Some(&mid),
            Some(&thumb),
        );
        assert!(
            std::sync::Arc::ptr_eq(&base2.expect("有封面必有底图"), &cur_cover),
            "当前密度封面最优先"
        );

        // 无封面（含旧封面）时：中清 > 缩略 > 全空（F4 走廊封面夹在旧封面与中清之间）。
        assert!(std::sync::Arc::ptr_eq(
            &super::render::page_base_image(None, None, None, Some(&mid), Some(&thumb))
                .expect("中清存在"),
            &mid,
        ));
        assert!(std::sync::Arc::ptr_eq(
            &super::render::page_base_image(None, None, None, None, Some(&thumb))
                .expect("缩略存在"),
            &thumb,
        ));
        // 全空（首开/极速拖动且旧资产也被回收，连走廊封面也无）→ None，此时才显示「加载中」占位。
        assert!(
            super::render::page_base_image(None, None, None, None, None).is_none(),
            "全空才落「加载中」占位"
        );
    }

    /// 待保存模型（P1）纯逻辑状态机（最小 viewer，无 worker / 无 pdfium）：加注释 →
    /// dirty / pending / 撤销栈；Ctrl+Z 撤销 → 移除该条；删除注释（灰框右键）→ dirty 回落；
    /// 再撤销 → 恢复该条。全链路零磁盘（无 worker 即证不触碰磁盘路径）。
    #[gpui::test]
    async fn pending_annot_lifecycle_add_undo_delete(cx: &mut TestAppContext) {
        use gpui::AppContext as _;
        let viewer: gpui::Entity<PdfViewer> = cx.update(|app| {
            app.new(|cx| {
                let dims = vec![(612.0, 792.0)];
                let (tops, total) = compute_layout(&dims, PAGE_WIDTH);
                PdfViewer {
                    path: "dummy.pdf".into(),
                    worker: None,
                    doc_id: 0,
                    dims,
                    page_tops: tops,
                    total_height: total,
                    slots: Vec::new(),
                    pending_thumb: HashMap::new(),
                    pending_mid: HashMap::new(),
                    mid_lru: VecDeque::new(),
                    pending_tiles: HashMap::new(),
                    pending_cover: HashMap::new(),
                    pending_prefetch_cover: HashMap::new(),
                    prefetch_reqs: 0,
                    loading_placeholder_hits: std::cell::Cell::new(0),
                    last_frame_tiles_drawn: std::cell::Cell::new(0),
                    pending_recolor: None,
                    raster_reqs: 0,
                    recolor_scope: Vec::new(),
                    recolor_received: Vec::new(),
                    tile_reqs: 0,
                    cover_reqs: 0,
                    tile_bytes: 0,
                    tile_budget_state: None,
                    degrade_frames: HashMap::new(),
                    degraded: HashSet::new(),
                    tile_budget_cap_override: None,
                    tile_pump_running: false,
                    last_scroll_y: 0.0,
                    scroll_dir: 0,
                    scroll_velocity: 0.0,
                    prev_fast: false,
                    speed_state: SpeedState::Idle,
                    nav_gen: 0,
                    jump_armed: false,
                    jump_target: None,
                    last_center_page: None,
                    stop_small_frames: 0,
                    zoom_armed: false,
                    settle_tile_burst: false,
                    settle_tile_burst_dispatch: 0,
                    zoom_idle_frames: 0,
                    zoom_epoch: 0,
                    scroll: ScrollHandle::new(),
                    state: ui::AsyncState::idle(),
                    bg: None,
                    recolor: None,
                    recolor_epoch: 0,
                    cover_failures: HashMap::new(),
                    cover_blacklist: HashSet::new(),
                    gpu_recolor: crate::recolor::gpu_recolor_gate(),
                    pending_drops: Vec::new(),
                    outlines: Vec::new(),
                    focus_handle: cx.focus_handle(),
                    text_pages: HashMap::new(),
                    text_lru: VecDeque::new(),
                    pending_text: HashMap::new(),
                    image_pages: HashMap::new(),
                    image_lru: VecDeque::new(),
                    pending_images: HashMap::new(),
                    selected_image: None,
                    sel_anchor: None,
                    sel_active: None,
                    selecting: false,
                    search_query: String::new(),
                    search_matches: Vec::new(),
                    search_truncated: false,
                    search_active: 0,
                    search_pending: None,
                    search_pending_seq: 0,
                    search_seq: 0,
                    zoom: 1.0,
                    base_w: PAGE_WIDTH,
                    scale_factor: 1.0,
                    window_sig: None,
                    window_settled: false,
                    idle_gate_skips: 0,
                    reload_anchor: None,
                    restore_anchor: None,
                    restore_pending: None,
                    last_modify_error: None,
                    sel_card: None,
                    pending_annots: Vec::new(),
                    undo_stack: VecDeque::new(),
                    pending_page_ops: 0,
                    next_annot_id: 0,
                    focused_annot: None,
                    panning: false,
                    pan_origin: gpui::point(px(0.), px(0.)),
                    pan_offset0: gpui::point(px(0.), px(0.)),
                }
            })
        });

        let quad = || vec![[60.0_f32, 100.0, 200.0, 120.0]];
        cx.update(|app| {
            viewer.update(app, |v, cx| {
                assert!(!v.is_dirty(), "初始非脏");
                // 加一条高亮 → pending/dirty/撤销栈
                v.add_annotation(0, AnnotKind::Highlight, quad(), cx);
                assert_eq!(v.pending_annot_count(), 1, "加注释后 pending=1");
                assert!(v.is_dirty(), "加注释后置脏");
                assert_eq!(v.undo_stack.len(), 1, "加注释记一条撤销");

                // Ctrl+Z → 移除该条、dirty 回 0
                v.undo_last(cx);
                assert_eq!(v.pending_annot_count(), 0, "撤销后 pending=0");
                assert!(!v.is_dirty(), "撤销后非脏");

                // 再加一条 → 灰框右键删除（delete_pending_annot）→ pending=0、dirty 回 0
                v.add_annotation(0, AnnotKind::Underline, quad(), cx);
                let id = v.pending_annots[0].id;
                assert!(v.focused_annot_id().is_none(), "未点击前无灰框焦");
                v.delete_pending_annot(id, cx);
                assert_eq!(v.pending_annot_count(), 0, "删除注释后 pending=0");
                assert!(!v.is_dirty(), "删空后非脏");

                // Ctrl+Z 恢复删除 → pending=1/dirty；再 Ctrl+Z 撤销添加 → 归零且撤销栈空
                v.undo_last(cx);
                assert_eq!(v.pending_annot_count(), 1, "撤销删除后该注释恢复");
                assert!(v.is_dirty(), "恢复后置脏");
                v.undo_last(cx);
                assert_eq!(v.pending_annot_count(), 0, "撤销添加后 pending=0");
                assert!(!v.is_dirty(), "全撤销后非脏");
                assert!(v.undo_stack.is_empty(), "撤销栈耗尽");
            })
        });
    }

    /// R6：view 重着色状态与「切档不重栅」语义（worker 未就绪路径：`set_recolor` 更新
    /// 状态、发不出消息也不 panic；同参短值为空操作；变更仍复位门控触发一帧收集/重绘，
    /// 但**不失效缓存、不停泵**——重着色刷新由 worker 对缓存原图现算回传，不再清槽重栅）。
    /// 【决策留痕：切档不再重栅】旧语义断言"变更应重置门控（下一帧重排重渲）+ 停泵"；
    /// 新语义：清槽/停泵已由「worker 重着色刷新」取代，泵运行中的在途瓦片结果仍有效，
    /// 不应被停。
    #[gpui::test]
    async fn recolor_setter_updates_state_without_reraster(cx: &mut TestAppContext) {
        cx.update(|app| {
            let _viewer = app.new(|cx| {
                let dims = vec![(612.0, 792.0)];
                let (tops, total) = compute_layout(&dims, PAGE_WIDTH);
                let mut v = PdfViewer {
                    path: "dummy.pdf".into(),
                    worker: None,
                    doc_id: 0,
                    dims: dims.clone(),
                    page_tops: tops,
                    total_height: total,
                    slots: Vec::new(),
                    pending_thumb: HashMap::new(),
                    pending_mid: HashMap::new(),
                    mid_lru: VecDeque::new(),
                    pending_tiles: HashMap::new(),
                    pending_cover: HashMap::new(),
                    pending_prefetch_cover: HashMap::new(),
                    prefetch_reqs: 0,
                    loading_placeholder_hits: std::cell::Cell::new(0),
                    last_frame_tiles_drawn: std::cell::Cell::new(0),
                    pending_recolor: None,
                    raster_reqs: 0,
                    recolor_scope: Vec::new(),
                    recolor_received: Vec::new(),
                    tile_reqs: 0,
                    cover_reqs: 0,
                    tile_bytes: 0,
                    tile_budget_state: None,
                    degrade_frames: HashMap::new(),
                    degraded: HashSet::new(),
                    tile_budget_cap_override: None,
                    tile_pump_running: false,
                    last_scroll_y: 0.0,
                    scroll_dir: 0,
                    scroll_velocity: 0.0,
                    prev_fast: false,
                    speed_state: SpeedState::Idle,
                    nav_gen: 0,
                    jump_armed: false,
                    jump_target: None,
                    last_center_page: None,
                    stop_small_frames: 0,
                    zoom_armed: false,
                    settle_tile_burst: false,
                    settle_tile_burst_dispatch: 0,
                    zoom_idle_frames: 0,
                    zoom_epoch: 0,
                    scroll: ScrollHandle::new(),
                    state: ui::AsyncState::idle(),
                    bg: None,
                    recolor: None,
                    recolor_epoch: 0,
                    cover_failures: HashMap::new(),
                    cover_blacklist: HashSet::new(),
                    gpu_recolor: crate::recolor::gpu_recolor_gate(),
                    pending_drops: Vec::new(),
                    outlines: Vec::new(),
                    focus_handle: cx.focus_handle(),
                    text_pages: HashMap::new(),
                    text_lru: VecDeque::new(),
                    pending_text: HashMap::new(),
                    image_pages: HashMap::new(),
                    image_lru: VecDeque::new(),
                    pending_images: HashMap::new(),
                    selected_image: None,
                    sel_anchor: None,
                    sel_active: None,
                    selecting: false,
                    search_query: String::new(),
                    search_matches: Vec::new(),
                    search_truncated: false,
                    search_active: 0,
                    search_pending: None,
                    search_pending_seq: 0,
                    search_seq: 0,
                    zoom: 1.0,
                    base_w: PAGE_WIDTH,
                    scale_factor: 1.0,
                    window_sig: None,
                    window_settled: false,
                    idle_gate_skips: 0,
                    reload_anchor: None,
                    restore_anchor: None,
                    restore_pending: None,
                    last_modify_error: None,
                    sel_card: None,
                    pending_annots: Vec::new(),
                    undo_stack: VecDeque::new(),
                    pending_page_ops: 0,
                    next_annot_id: 0,
                    focused_annot: None,
                    panning: false,
                    pan_origin: gpui::point(px(0.), px(0.)),
                    pan_offset0: gpui::point(px(0.), px(0.)),
                };
                // 先给出「静置已确认」状态，验证变更会复位它（触发一帧收集重着色结果并重绘），
                // 而**不**清缓存/不停泵（切档不重栅：泵里在途瓦片结果仍有效、须继续收集）。
                v.window_sig = Some(WindowSig::default());
                v.window_settled = true;
                v.tile_pump_running = true;

                let p = RecolorParams { bg: [0x20, 0x20, 0x20], ink: None };
                v.set_recolor(Some(p), cx);
                assert_eq!(v.recolor, Some(p), "重着色状态应更新（存到 view 侧）");
                assert_eq!(v.window_sig, None, "变更应复位门控（触发一帧收集+重绘）");
                assert!(!v.window_settled, "变更应复位静置确认");
                assert!(v.tile_pump_running, "切档不重栅：不应停泵（在途瓦片结果仍有效）");
                assert!(v.pending_recolor.is_none(), "无 worker 时不发刷新请求、不 panic");

                // 同参短值：不动已确认门控（避免重复刷新）。
                v.window_sig = Some(WindowSig::default());
                v.window_settled = true;
                v.set_recolor(Some(p), cx);
                assert_eq!(v.window_sig, Some(WindowSig::default()), "同参应短路、不失效");
                assert!(v.window_settled, "同参应短路、不重置静置");

                // 回 None（默认直通）→ 清状态 + 复位门控（同样不重栅、不停泵）。
                v.set_recolor(None, cx);
                assert_eq!(v.recolor, None, "默认档应清除重着色");
                assert_eq!(v.window_sig, None, "清除同样复位门控");
                assert!(v.tile_pump_running, "回默认同样不停泵");
                v
            });
        });
    }

    /// 最小 pdf viewer（不触碰 pdfium / 无 worker）：1 页占位 dims + 一页槽，供
    /// `collect_*` 结果守卫与 `plan_tiles_in_window` 纯状态机断言。
    fn minimal_viewer(cx: &mut gpui::Context<PdfViewer>) -> PdfViewer {
        let dims = vec![(612.0, 792.0)];
        let (tops, total) = compute_layout(&dims, PAGE_WIDTH);
        PdfViewer {
            path: "dummy.pdf".into(),
            worker: None,
            doc_id: 0,
            dims: dims.clone(),
            page_tops: tops,
            total_height: total,
            slots: vec![Slot::default()],
            pending_thumb: HashMap::new(),
            pending_mid: HashMap::new(),
            mid_lru: VecDeque::new(),
            pending_tiles: HashMap::new(),
            pending_cover: HashMap::new(),
            pending_prefetch_cover: HashMap::new(),
            prefetch_reqs: 0,
            loading_placeholder_hits: std::cell::Cell::new(0),
            last_frame_tiles_drawn: std::cell::Cell::new(0),
            pending_recolor: None,
            raster_reqs: 0,
            recolor_scope: Vec::new(),
            recolor_received: Vec::new(),
            tile_reqs: 0,
            cover_reqs: 0,
            tile_bytes: 0,
            tile_budget_state: None,
            degrade_frames: HashMap::new(),
            degraded: HashSet::new(),
            tile_budget_cap_override: None,
            tile_pump_running: false,
            last_scroll_y: 0.0,
            scroll_dir: 0,
            scroll_velocity: 0.0,
            prev_fast: false,
            speed_state: SpeedState::Idle,
            nav_gen: 0,
            jump_armed: false,
            jump_target: None,
            last_center_page: None,
            stop_small_frames: 0,
            zoom_armed: false,
            settle_tile_burst: false,
            settle_tile_burst_dispatch: 0,
            zoom_idle_frames: 0,
            zoom_epoch: 0,
            scroll: ScrollHandle::new(),
            state: ui::AsyncState::idle(),
            bg: None,
            recolor: None,
            recolor_epoch: 0,
            cover_failures: HashMap::new(),
            cover_blacklist: HashSet::new(),
            gpu_recolor: false,
            pending_drops: Vec::new(),
            outlines: Vec::new(),
            focus_handle: cx.focus_handle(),
            text_pages: HashMap::new(),
            text_lru: VecDeque::new(),
            pending_text: HashMap::new(),
            image_pages: HashMap::new(),
            image_lru: VecDeque::new(),
            pending_images: HashMap::new(),
            selected_image: None,
            sel_anchor: None,
            sel_active: None,
            selecting: false,
            search_query: String::new(),
            search_matches: Vec::new(),
            search_truncated: false,
            search_active: 0,
            search_pending: None,
            search_pending_seq: 0,
            search_seq: 0,
            zoom: 1.0,
            base_w: PAGE_WIDTH,
            scale_factor: 1.0,
            window_sig: None,
            window_settled: false,
            idle_gate_skips: 0,
            reload_anchor: None,
            restore_anchor: None,
            restore_pending: None,
            last_modify_error: None,
            sel_card: None,
            pending_annots: Vec::new(),
            undo_stack: VecDeque::new(),
            pending_page_ops: 0,
            next_annot_id: 0,
            focused_annot: None,
            panning: false,
            pan_origin: gpui::point(px(0.), px(0.)),
            pan_offset0: gpui::point(px(0.), px(0.)),
        }
    }

    /// W1 回归：**CPU 回退路径切档后，旧世代在途渲染结果到达被丢弃、新世代结果被采纳**。
    /// 修评审 t1-t4 的 W1：worker 按「出队时」recolor 着色、结果经独立 mpsc 回传，与
    /// SetRecolor/RecolorRefresh 无总体次序 → 旧色瓦片/封面晚到会覆盖新参数槽位且无后续
    /// 重绘纠正。headless 插桩：先以世代 0 派请求、`set_recolor` 切档使世代 →1、延迟回传
    /// 世代 0（旧色）结果 → 断言槽位保持原样（被丢弃）；再回传世代 1（新色）结果 → 采纳。
    /// 缩放发糊根治的 view 侧契约：`page_width()` 把页宽量化到设备像素网格——
    /// 任意缩放档 × DPI 下 `page_width·sf` 恒为整数（位图/显示 1:1 的充要条件）、
    /// 与量化前差 ≤0.5 设备 px（fit-width/布局无感知）、同状态幂等（确定放置网格）。
    /// （1:1 全链见 `layout::quantized_page_width_is_device_pixel_1_to_1`。）
    #[gpui::test]
    async fn page_width_quantizes_to_device_pixel_grid(cx: &mut TestAppContext) {
        use gpui::AppContext as _;
        let zooms = [1.0f32, 1.12, 1.2544, 1.404928, 1.57351936, 0.75, 2.0];
        let sfs = [1.0f32, 1.5, 2.0];
        let viewer: gpui::Entity<PdfViewer> = cx.update(|app| {
            app.new(|cx| minimal_viewer(cx))
        });
        for &sf in &sfs {
            for &z in &zooms {
                let (raw, pw) = cx.update(|app| {
                    viewer.update(app, |v, _| {
                        v.zoom = z;
                        v.scale_factor = sf;
                    });
                    let v = viewer.read(app);
                    (v.base_w * v.zoom, v.page_width())
                });
                assert!(
                    (pw * sf - (pw * sf).round()).abs() < 1e-3,
                    "page_width·sf 应为设备整数：base_w*zoom={raw} sf={sf} pw={pw}"
                );
                assert_eq!(pw, crate::pdf::layout::snap_to_device(raw, sf), "与量化定义一致");
                assert!(
                    (pw - raw).abs() * sf <= 0.5 + 1e-3,
                    "量化偏移应 ≤0.5 设备 px：raw={raw} pw={pw} sf={sf}"
                );
                // 幂等：同状态两次取同值。
                let again = cx.update(|app| viewer.read(app).page_width());
                assert_eq!(pw, again, "page_width 应幂等（确定放置网格）");
            }
        }
    }

    #[gpui::test]
    async fn w1_stale_recolor_epoch_results_dropped_in_cpu_path(cx: &mut TestAppContext) {
        use gpui::AppContext as _;
        let rec = RecolorParams { bg: [0x20, 0x20, 0x20], ink: Some([0xD8, 0xD8, 0xD8]) };
        let viewer: gpui::Entity<PdfViewer> = cx.update(|app| {
            app.new(|cx| {
                let mut v = minimal_viewer(cx);
                // 切档（此处为演示切到新档）→ view 世代 0→1；旧世代（epoch 0）结果即旧色。
                v.set_recolor(Some(rec), cx);
                assert_eq!(v.recolor_epoch, 1, "切档后世代应 +1");
                v
            })
        });
        let (scale, bmp) = cx.read(|app| {
            let v = viewer.read(app);
            let s = render_scale(v.page_width(), 1.0, v.dims[0].0);
            (s, page_bitmap_size(v.dims[0], s))
        });

        // ① 旧世代（epoch 0）瓦片晚到 → 丢弃，槽位不被旧色覆盖。
        cx.update(|app| {
            viewer.update(app, |v, _cx| {
                let (tx, rx) = mpsc::channel();
                let _ = tx.send(Some(TileResult {
                    page: 0,
                    row: 0,
                    col: 0,
                    x0: 0,
                    y0: 0,
                    w: bmp.0,
                    h: bmp.1,
                    full_w: bmp.0,
                    full_h: bmp.1,
                    bgra: vec![0u8; (bmp.0 * bmp.1 * 4) as usize],
                    recolor_epoch: 0, // 旧世代（A 色）
                    luma: false,
                }));
                v.pending_tiles.insert((0, 0, 0), (scale, 0, rx));
                let any = v.collect_tile_results(1.0);
                assert!(!any, "旧世代瓦片应被丢弃（不返回新位图）");
                assert!(v.slots[0].tiles.is_empty(), "槽位不应被旧色瓦片覆盖");
            });
        });

        // ② 当前世代（epoch 1）瓦片到达 → 采纳。
        cx.update(|app| {
            viewer.update(app, |v, _cx| {
                let (tx, rx) = mpsc::channel();
                let _ = tx.send(Some(TileResult {
                    page: 0,
                    row: 0,
                    col: 0,
                    x0: 0,
                    y0: 0,
                    w: bmp.0,
                    h: bmp.1,
                    full_w: bmp.0,
                    full_h: bmp.1,
                    bgra: vec![0u8; (bmp.0 * bmp.1 * 4) as usize],
                    recolor_epoch: 1, // 当前世代（B 色）
                    luma: false,
                }));
                v.pending_tiles.insert((0, 0, 0), (scale, 0, rx));
                let any = v.collect_tile_results(1.0);
                assert!(any, "当前世代瓦片应被采纳");
                assert!(v.slots[0].tiles.contains_key(&(0, 0)), "槽位应装入新世代瓦片");
            });
        });

        // ③ 封面同守卫：旧世代封面晚到 → 丢弃；新世代 → 采纳。
        let cscale = cx.read(|app| {
            let v = viewer.read(app);
            cover_scale(v.dims[0], v.page_width(), 1.0)
        });
        cx.update(|app| {
            viewer.update(app, |v, _cx| {
                let (tx, rx) = mpsc::channel();
                let _ = tx.send(Some(RenderResult {
                    bgra: vec![0u8; (bmp.0 * bmp.1 * 4) as usize],
                    width: bmp.0,
                    height: bmp.1,
                    recolor_epoch: 0, // 旧世代
                    luma: false,
                }));
                v.pending_cover.insert(0, (cscale, 0, 0, rx)); // 请求世代 0（旧）+ 导航世代 0
                let any = v.collect_cover_results(1.0);
                assert!(!any, "旧世代封面应被丢弃");
                assert!(v.slots[0].cover.is_none(), "封面槽不应被旧色覆盖");
            });
        });
        cx.update(|app| {
            viewer.update(app, |v, _cx| {
                let (tx, rx) = mpsc::channel();
                let _ = tx.send(Some(RenderResult {
                    bgra: vec![0u8; (bmp.0 * bmp.1 * 4) as usize],
                    width: bmp.0,
                    height: bmp.1,
                    recolor_epoch: 1, // 当前世代
                    luma: false,
                }));
                v.pending_cover.insert(0, (cscale, 1, 0, rx)); // 请求世代 1（当前）+ 导航世代 0
                let any = v.collect_cover_results(1.0);
                assert!(any, "当前世代封面应被采纳");
                assert!(v.slots[0].cover.is_some(), "封面槽应装入新世代封面");
            });
        });

        // ④ NIT：封面失败计数只在**当前世代**请求时生效——旧世代（请求世代 0）失败不计数
        // （切档前入队的请求，失败不反映当前参数供给），当前世代失败才累计。
        cx.update(|app| {
            viewer.update(app, |v, _cx| {
                let (tx, rx) = mpsc::channel();
                let _ = tx.send(None::<RenderResult>);
                v.pending_cover.insert(0, (cscale, 0, 0, rx)); // 旧世代失败（recolor 世代 0 + nav_gen 0）
                let _any = v.collect_cover_results(1.0);
                assert!(
                    !v.cover_failures.contains_key(&0),
                    "旧世代封面失败不应计入黑名单计数"
                );
            });
        });
        cx.update(|app| {
            viewer.update(app, |v, _cx| {
                let (tx, rx) = mpsc::channel();
                let _ = tx.send(None::<RenderResult>);
                v.pending_cover.insert(0, (cscale, 1, 0, rx)); // 当前世代失败（recolor 世代 1 + nav_gen 0）
                let _any = v.collect_cover_results(1.0);
                assert_eq!(
                    v.cover_failures.get(&0).copied(),
                    Some(1),
                    "当前世代封面失败应计数"
                );
            });
        });
    }

    /// W2 回归：**封面持续失败 → 拉黑该页 → skip_covered 对该页失效、回退瓦片供给**。
    /// 修评审 t1-t4 的 W2：affordable+同密度页永远跳过瓦片，封面若持续失败（pdfium OOM 等）
    /// 每帧盲重试、瓦片永不再派 → 停在占位/模糊。断言状态机：连续 3 次 `None` 回传 → 黑名单
    /// 命中；`plan_tiles_in_window` 在拉黑前返回空（封面供给）→ 拉黑后返回非空（瓦片供给）；
    /// 失败计数达阈值后不再增长（不盲目重试）。
    #[gpui::test]
    async fn w2_cover_failure_blacklist_enables_tile_fallback(cx: &mut TestAppContext) {
        use gpui::AppContext as _;
        use std::cell::RefCell;
        use std::rc::Rc;

        struct RootProbe {
            pdf: gpui::Entity<PdfViewer>,
        }
        impl gpui::Render for RootProbe {
            fn render(
                &mut self,
                _window: &mut gpui::Window,
                _cx: &mut gpui::Context<Self>,
            ) -> impl gpui::IntoElement {
                gpui::div().size_full().child(self.pdf.clone())
            }
        }

        let holder: Rc<RefCell<Option<gpui::Entity<PdfViewer>>>> = Rc::default();
        let (_root, mut cx) = cx.add_window_view(|_window, cx| {
            let pdf = cx.new(|cx| minimal_viewer(cx));
            *holder.borrow_mut() = Some(pdf.clone());
            RootProbe { pdf }
        });
        cx.simulate_resize(gpui::size(gpui::px(1500.), gpui::px(900.)));
        cx.run_until_parked();

        // 封面失败计数注入：`collect_cover_results` 对 `res=None` 计失败。
        let cscale = cx.read(|app| {
            let v = holder.borrow().as_ref().unwrap().read(app);
            cover_scale(v.dims[0], v.page_width(), 1.0)
        });
        let drive_failure = |holder: &Rc<RefCell<Option<gpui::Entity<PdfViewer>>>>,
                             cx: &mut gpui::VisualTestContext|
         {
            cx.update(|_w, app| {
                holder.borrow().as_ref().unwrap().update(app, |v, _cx| {
                    let (tx, rx) = mpsc::channel();
                    let _ = tx.send(None::<RenderResult>);
                    v.pending_cover.insert(0, (cscale, 0, 0, rx)); // 请求世代 0 == 当前（未切档）
                    let _any = v.collect_cover_results(1.0);
                });
            });
        };
        drive_failure(&holder, &mut cx);
        let (f1, bl1) = cx.read(|app| {
            let v = holder.borrow().as_ref().unwrap().read(app);
            (v.cover_failures[&0], v.cover_blacklist.contains(&0))
        });
        assert_eq!(f1, 1, "第 1 次失败计数");
        assert!(!bl1, "未达阈值不拉黑");
        drive_failure(&holder, &mut cx);
        drive_failure(&holder, &mut cx);
        let (f3, bl3) = cx.read(|app| {
            let v = holder.borrow().as_ref().unwrap().read(app);
            (v.cover_failures[&0], v.cover_blacklist.contains(&0))
        });
        assert_eq!(f3, 3, "第 3 次失败计数");
        assert!(bl3, "达阈值后拉黑");

        // 拉黑后再失败 → 计数不再增长（不盲目重试封面）。
        drive_failure(&holder, &mut cx);
        let f4 = cx.read(|app| holder.borrow().as_ref().unwrap().read(app).cover_failures[&0]);
        assert_eq!(f4, 3, "拉黑后不再递增失败计数（停止盲目重试）");

        // skip_covered 翻转：拉黑前该页（封面 affordable 同密度）不派瓦片；拉黑后回退瓦片。
        // 「拉黑前」对照用**独立新鲜 viewer**（同一几何，未污染黑名单）。
        let fresh: gpui::Entity<PdfViewer> = cx.update(|_window, app| app.new(|cx| minimal_viewer(cx)));
        let (need_before, need_after) = cx.update(|window, cx| {
            let before = {
                let v = fresh.read(cx);
                v.plan_tiles_in_window(0..=0, 0, v.page_width(), 1.0, 0.0, 900.0, window)
            };
            let after = {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                v.plan_tiles_in_window(0..=0, 0, v.page_width(), 1.0, 0.0, 900.0, window)
            };
            (before, after)
        });
        assert!(need_before.is_empty(), "拉黑前封面供给：该页不应派瓦片");
        assert!(!need_after.is_empty(), "拉黑后回退瓦片供给：该页应派瓦片");
    }

    /// zoom 伪机制回归（2026-10-30，要求 2a/2b）：`plan_tiles_in_window` 的 skip_covered
    /// 必须「封面在槽（cover_scale≈当前 cscale）或在途（pending_cover 含该页且密度=当前）」
    /// 才成立——仅凭「封面 affordable && cover_serves_scale」就无条件跳瓦片，真机封面渲染
    /// 被丢/失败/逐出而未到位（headless 单 worker 封面瞬达掩盖）时，该页瓦片也被跳 →
    /// 停在低密底图、零在途、静止冻结（糊），只能靠再次缩放强制重渲。
    ///
    /// - 2a：封面缺失（cover 槽清空、保留 0.5 preview 低清底）+ affordable + cover_serves
    ///   → `plan_tiles_in_window` **不再跳过**（返回该页可见瓦片，作为最终锐利供给）；
    /// - 2b-在途：`pending_cover` 含该页（当前 cscale）→ 仍走 skip（不浪费瓦片）；
    /// - 2b-在槽：封面在槽且 cover_scale≈当前 cscale → 仍走 skip。
    #[gpui::test]
    async fn zoom_missing_cover_plan_skips_only_when_cover_in_slot_or_inflight(
        cx: &mut TestAppContext,
    ) {
        use gpui::AppContext as _;
        use std::sync::mpsc;

        struct RootProbe {
            pdf: gpui::Entity<PdfViewer>,
        }
        impl gpui::Render for RootProbe {
            fn render(
                &mut self,
                _window: &mut gpui::Window,
                _cx: &mut gpui::Context<Self>,
            ) -> impl gpui::IntoElement {
                gpui::div().size_full().child(self.pdf.clone())
            }
        }

        let holder: std::rc::Rc<std::cell::RefCell<Option<gpui::Entity<PdfViewer>>>> =
            std::rc::Rc::default();
        let (_root, cx) = cx.add_window_view(|_window, cx| {
            let pdf = cx.new(|cx| multi_page_viewer(cx, 1));
            *holder.borrow_mut() = Some(pdf.clone());
            RootProbe { pdf }
        });
        cx.simulate_resize(gpui::size(gpui::px(900.), gpui::px(900.)));
        cx.run_until_parked();

        // 单页 A4 612×792pt @ PAGE_WIDTH、sf=1.0、scroll_y≈0、viewport=900 —— 封面 affordable
        // 且与瓦片同密度（cover-served）的典型低倍 regime（同 W2 回归几何）。
        let plan = |v: &PdfViewer, window: &gpui::Window| -> Vec<(usize, u32, u32, u64)> {
            v.plan_tiles_in_window(0..=0, 0, v.page_width(), 1.0, 0.0, 900.0, window)
        };
        cx.update(|window, app| {
            // 构造前置确认：该页确为 cover-served（affordable && cscale≈scale）。
            let (cscale, dim, scale) = {
                let vv = holder.borrow().as_ref().unwrap().read(app);
                let dim = vv.dims[0];
                let pw = vv.page_width();
                (
                    cover_scale(dim, pw, 1.0),
                    dim,
                    render_scale(pw, 1.0, dim.0),
                )
            };
            assert!(
                cover_affordable(dim, cscale)
                    && crate::pdf::layout::cover_serves_scale(cscale, scale),
                "前提：封面应 affordable 且等效瓦片密度（cover-served）"
            );

            // 2a：封面缺失（默认槽位无封面、0.5 preview 低清底仍在，如清掉 cover 槽的页）
            // → 不得跳过瓦片 → plan 返回该页可见瓦片（最终锐利供给）。
            let missing = {
                let vv = holder.borrow().as_ref().unwrap().read(app);
                plan(vv, window)
            };
            assert!(
                !missing.is_empty(),
                "2a 封面缺失（无在槽/在途）不得跳过瓦片——应派当前密度 1:1 瓦片"
            );

            // 2b-在途：pending_cover 含该页（当前 cscale）→ 仍走 skip。
            holder.borrow().as_ref().unwrap().update(app, |vv, _| {
                let (_, rx) = mpsc::channel::<Option<RenderResult>>();
                vv.pending_cover.insert(0, (cscale, 0, 0, rx));
            });
            let inflight = {
                let vv = holder.borrow().as_ref().unwrap().read(app);
                plan(vv, window)
            };
            assert!(
                inflight.iter().all(|&(p, _, _, _)| p != 0),
                "2b-在途：封面在途应跳该页瓦片（不浪费）"
            );

            // 2b-在槽：封面在槽且 cover_scale≈cscale → 仍走 skip。
            holder.borrow().as_ref().unwrap().update(app, |vv, _| {
                vv.pending_cover.clear();
                vv.slots[0].cover =
                    crate::render_image_from_bgra(vec![0u8; 4 * 8 * 8], 8, 8).ok();
                vv.slots[0].cover_scale = cscale;
            });
            let in_slot = {
                let vv = holder.borrow().as_ref().unwrap().read(app);
                plan(vv, window)
            };
            assert!(
                in_slot.iter().all(|&(p, _, _, _)| p != 0),
                "2b-在槽：封面在槽（cover_scale≈cscale）应跳该页瓦片"
            );

            // 对照：封面缺失且（S5/S2/S4 同标记）cover_scale=0 → 一律回退瓦片（不冻结）。
            holder.borrow().as_ref().unwrap().update(app, |vv, _| {
                vv.slots[0].cover = None;
                vv.slots[0].cover_scale = 0.0;
            });
            let stale = {
                let vv = holder.borrow().as_ref().unwrap().read(app);
                plan(vv, window)
            };
            assert!(
                !stale.is_empty(),
                "封面缺失（stale 标记）应回退瓦片（不冻结）"
            );
        });
    }

    /// A1（**S 系列双密度混叠根治，真实渲染路径**）：settle 收口期一页绝不呈现
    /// 「旧密度底图 + 新密度瓦片」同帧混叠——新密度内容对「已有旧底」的页**整页一次换**，
    /// 逐块渐进只保留给「从未显示过」的新页。
    ///
    /// 复现（先红后绿）：`multi_page_viewer`（无 worker → 调度早退，状态确定性）在真实
    /// 窗口渲染框架下，直接制造真实缩放收口产物的**病理三元态**——高倍页封面不可负担 →
    /// `cover` 恒 `None`、旧档封面迁入 `prev_cover`（旧密度底）、瓦片按当前 `render_scale`
    /// 只**部分**到达（可见组缺一格）：
    /// 1) 渲染一帧 → 断言本帧**不含任何瓦片 img 元素**（`last_frame_tiles_drawn == 0`，
    ///    虽有部分瓦片在槽）——旧实现把部分瓦片直接挂树（红）；门控后整页只显示旧底（绿）。
    /// 2) 补齐可见组缺格 → 渲染一帧 → 断言本帧瓦片 img 数 == 可见组大小且一次全到位
    ///    （原子换）。
    /// 3) 全程 `loading_placeholder_hits` 零增长（底图链恒非空，R4）。
    ///
    /// 判定粒度 = 渲染路径每帧实际挂入卡片 DOM 的瓦片 img 元素计数探针
    /// （`last_frame_tiles_drawn`；旧底图 img 恒在 `prev_cover` 分支，两者可区分）。
    /// 「组」= 可见矩形 + [`TILE_MARGIN`] 的格集（与调度 `page_visible_rect` 同源；高倍/
    /// 大页整页 `rows×cols` 永不 到齐，不能作原子换门槛）。
    #[gpui::test]
    async fn settle_prevents_mixed_density_tiles_on_stale_cover(cx: &mut TestAppContext) {
        use gpui::AppContext as _;
        struct RootProbe {
            pdf: gpui::Entity<PdfViewer>,
        }
        impl gpui::Render for RootProbe {
            fn render(
                &mut self,
                _window: &mut gpui::Window,
                _cx: &mut gpui::Context<Self>,
            ) -> impl gpui::IntoElement {
                gpui::div().size_full().child(self.pdf.clone())
            }
        }

        let holder: std::rc::Rc<std::cell::RefCell<Option<gpui::Entity<PdfViewer>>>> =
            std::rc::Rc::default();
        let (_root, cx) = cx.add_window_view(|_window, cx| {
            let pdf = cx.new(|cx| multi_page_viewer(cx, 1));
            *holder.borrow_mut() = Some(pdf.clone());
            RootProbe { pdf }
        });
        cx.simulate_resize(gpui::size(gpui::px(1000.), gpui::px(900.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }

        // 读取渲染态几何（与 `build_visible_page_cards` / `schedule_tiles` 同一套数学）：
        // 当前页 + 当前 render_scale + 可见矩形（含 margin）的瓦片格集。
        let (page, group_cells, scale, total) = cx.update(|window, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            let page = 0;
            let dim = v.dims[page];
            let pw = v.page_width();
            let sf = window.scale_factor();
            let scale = render_scale(pw, sf, dim.0);
            let bmp = page_bitmap_size(dim, scale);
            let disp_h = display_height(dim, pw);
            let viewport_h = f32::from(v.scroll.bounds().size.height);
            assert!(viewport_h > 0.0, "滚动容器应在渲染后布局完成：viewport_h={viewport_h}");
            let scroll_y = f32::from(-v.scroll.offset().y);
            let wrap_w = v.wrap_width(window, pw);
            let rect = v
                .page_visible_rect(page, scroll_y, viewport_h, pw, wrap_w)
                .expect("第 0 页应可见");
            let (r0, c0, r1, c1) = visible_tile_range_axis(
                bmp,
                pw,
                disp_h,
                rect,
                TILE_SIZE,
                TILE_MARGIN,
                config::pdf::TILE_MARGIN_COL,
            )
            .expect("可见区应圈中瓦片格");
            let mut cells: Vec<(u32, u32)> = Vec::new();
            for row in r0..=r1 {
                for col in c0..=c1 {
                    cells.push((row, col));
                }
            }
            let total = (r1 - r0 + 1) * (c1 - c0 + 1);
            (page, cells, scale, total)
        });
        assert!(total > 1, "测试页可见瓦片组应 >1 格：total={total}");
        let make_img = || render_image_from_bgra(vec![0u8; 4 * 4 * 4], 4, 4).expect("测试位图");

        // 阶段 1：制造「封面不可负担」病理三元态——旧密度底（prev_cover）+ 部分新密度瓦片。
        let hits0 = cx.update(|_w, cx| {
            let ent = holder.borrow().as_ref().unwrap().clone();
            ent.update(cx, |v, _| {
                let slot = &mut v.slots[page];
                slot.cover = None;
                slot.cover_scale = 0.0;
                slot.prev_cover = Some(make_img());
                slot.tile_scale = scale;
                slot.tiles.clear();
                for &(row, col) in group_cells.iter().take(total as usize - 1) {
                    slot.tiles.insert(
                        (row, col),
                        TileSlot {
                            image: make_img(),
                            x0: 0,
                            y0: 0,
                            w: 4,
                            h: 4,
                        },
                    );
                }
            });
            ent.read(cx).loading_placeholder_hits.get()
        });
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |_v, cx| cx.notify());
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let (drawn1, partial, hits1) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (
                v.last_frame_tiles_drawn.get(),
                v.slots[page].tiles.len(),
                v.loading_placeholder_hits.get(),
            )
        });
        assert!(
            partial > 0 && (partial as usize) < total as usize,
            "应存在部分瓦片（缺一格）：partial={partial} total={total}"
        );
        assert_eq!(
            drawn1,
            0,
            "部分新密度瓦片在场时本帧不得绘制任何瓦片元素（同帧不混叠，宁糊不混）：partial={partial}"
        );
        assert_eq!(hits1, hits0, "底图链恒非空，过渡期不得落「加载中」（R4）");

        // 阶段 2：补齐整组瓦片 → 下帧一次全到位呈现（原子换）。
        cx.update(|_w, cx| {
            let ent = holder.borrow().as_ref().unwrap().clone();
            ent.update(cx, |v, cx| {
                let slot = &mut v.slots[page];
                for &(row, col) in &group_cells {
                    slot.tiles.entry((row, col)).or_insert(TileSlot {
                        image: make_img(),
                        x0: 0,
                        y0: 0,
                        w: 4,
                        h: 4,
                    });
                }
                cx.notify();
            });
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let (drawn2, full, hits2) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (
                v.last_frame_tiles_drawn.get(),
                v.slots[page].tiles.len(),
                v.loading_placeholder_hits.get(),
            )
        });
        assert_eq!(
            full as usize, total as usize,
            "整组瓦片应全部在槽：full={full} total={total}"
        );
        assert_eq!(
            drawn2, total as usize,
            "整组就位后本帧应一次全量呈现全部瓦片（原子换）：drawn={drawn2} total={total}"
        );
        assert_eq!(hits2, hits0, "补齐过渡同样不得落「加载中」（R4）");
    }

    /// 构造 N 页的最小 viewer（复用 `minimal_viewer` 后替换布局/槽位）：纯逻辑
    /// （L1/L3 走廊/大跳等）测试的几何基座，无 worker / 无 pdfium。
    fn multi_page_viewer(cx: &mut gpui::Context<PdfViewer>, n: usize) -> PdfViewer {
        let mut v = minimal_viewer(cx);
        let dims = vec![(612.0, 792.0); n.max(1)];
        let (tops, total) = compute_layout(&dims, PAGE_WIDTH);
        v.dims = dims;
        v.page_tops = tops;
        v.total_height = total;
        v.slots = vec![Slot::default(); n.max(1)];
        v.scroll_dir = 0;
        v
    }

    /// L1.3 **停稳快进（纯状态机）**：连续 K=3 帧 |Δy|<[`STOP_DETECT_PX`] → 速度直落 Slow
    /// （绕过 0.7ⁿ EMA 衰减）；单帧小位移 / 中途 ≥2px 位移帧清零计数不误触；非快速态（Slow/
    /// Idle）直落无副作用（保持原档）。
    #[gpui::test]
    async fn stop_detection_forces_slow_after_k_small_frames(cx: &mut TestAppContext) {
        use gpui::AppContext as _;
        let n = config::pdf::STOP_DETECT_FRAMES;
        let v: gpui::Entity<PdfViewer> = cx.update(|app| {
            app.new(|cx| {
                let mut v = multi_page_viewer(cx, 5);
                v.speed_state = SpeedState::VeryFast;
                v.prev_fast = true;
                v
            })
        });
        cx.update(|app| {
            v.update(app, |v, _cx| {
                for _ in 0..(n - 1) {
                    v.update_stop_detection(1.0);
                }
                assert_eq!(
                    v.speed_state,
                    SpeedState::VeryFast,
                    "不足 K 帧的微停不误触（保持快速态）"
                );
                assert!(v.prev_fast, "不足 K 帧 prev_fast 不变");
                // 中途一帧 ≥2px → 计数清零；即便后面再补小帧也不直落（须重新连续 K 帧）
                v.update_stop_detection(5.0);
                for _ in 0..(n - 1) {
                    v.update_stop_detection(0.0);
                }
                assert_eq!(
                    v.speed_state,
                    SpeedState::VeryFast,
                    "中途大位移帧清零、不直落"
                );
                // 连续 K 帧小位移 → 直落 Slow
                v.update_stop_detection(0.0);
                assert_eq!(v.speed_state, SpeedState::Slow, "连续 K 帧小位移应直落 Slow");
                assert!(!v.prev_fast, "直落 Slow 后 prev_fast 派生同步 false");
                // 已 Slow/Idle 再小帧 → 无副作用（保持原档，不误伤既有状态机）
                v.speed_state = SpeedState::Idle;
                v.prev_fast = false;
                v.update_stop_detection(0.0);
                assert_eq!(v.speed_state, SpeedState::Idle, "非快速态直落无副作用");
            });
        });
    }

    /// L3.2 **走廊窗口随速度扩大（纯逻辑）**：M(v)=clamp(round(v/平均页高)+1,
    /// [`PREFETCH_COVER_MARGIN`], [`PREFETCH_COVER_MARGIN_MAX`])——idle v≈0 → 回落基线 2
    /// （既有 idle 走廊语义不变）；中速按速度放大且单调；极高 `scroll_velocity` 封顶到 MAX。
    #[gpui::test]
    async fn corridor_margin_scales_with_velocity_and_clamps(cx: &mut TestAppContext) {
        use gpui::AppContext as _;
        let avg_h = {
            let dims = vec![(612.0, 792.0); 21];
            let (_tops, total) = compute_layout(&dims, PAGE_WIDTH);
            total / 21.0
        };
        let read_m = |cxt: &mut TestAppContext, vel: f32| -> usize {
            cxt.update(|app| {
                let v: gpui::Entity<PdfViewer> = app.new(|cx| multi_page_viewer(cx, 21));
                v.update(app, |vv, _| vv.scroll_velocity = vel);
                v.read(app).corridor_margin()
            })
        };
        assert_eq!(
            read_m(cx, 0.0),
            config::pdf::PREFETCH_COVER_MARGIN,
            "idle 速度回落基线"
        );
        assert_eq!(read_m(cx, avg_h * 3.0), 4, "v=3×平均页高 → round(3)+1=4");
        assert_eq!(
            read_m(cx, avg_h * 6.0),
            7,
            "v=6×平均页高 → round(6)+1=7（单调放大）"
        );
        assert_eq!(
            read_m(cx, avg_h * 100.0),
            config::pdf::PREFETCH_COVER_MARGIN_MAX,
            "极高速度封顶到 MAX"
        );
    }

    /// L3.3 **方向感知逐出（纯状态机）**：`evict_prefetch_covers` 排序键 = `dist − dir_bonus`，
    /// bonus 只给「当前滚动方向回退侧（向下滚时页号<中心）」近期过页——廊封面超 40MB 预算
    /// 从「最远/无回退加成」先逐，回退侧同距离页多驻留（滚回瞬间有底图）。只影响竞争顺序，
    /// 不改变任何预算；窗口外回收语义不变。
    #[gpui::test]
    async fn evict_prefetch_direction_bonus_keeps_return_side_longer(cx: &mut TestAppContext) {
        use gpui::AppContext as _;
        let v: gpui::Entity<PdfViewer> = cx.update(|app| {
            app.new(|cx| {
                let mut v = multi_page_viewer(cx, 21);
                v.scroll_dir = 1; // 向下滚：回退侧 = 页号 < 中心
                // 每页一枚 1×1 廊封面 + 大 `prefetch_cover_scale` 使估算字节显著 > 预算
                // （21 枚 × ~5MB = ~100MB > 40MB → 强制窗内逐出，且各页字节相等 → 排序决定谁留）。
                let img = render_image_from_bgra(vec![0x20, 0x20, 0x20, 255], 1, 1).expect("位图");
                for slot in v.slots.iter_mut() {
                    slot.prefetch_cover = Some(img.clone());
                    slot.prefetch_cover_scale = 1.55;
                }
                v
            })
        });
        cx.update(|app| {
            v.update(app, |v, _cx| {
                let center = 10usize;
                let before: Vec<usize> = v
                    .slots
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| s.prefetch_cover.is_some())
                    .map(|(p, _)| p)
                    .collect();
                assert_eq!(before.len(), 21, "预置 21 页廊封面");
                v.evict_prefetch_covers(center);
                let retained: Vec<usize> = v
                    .slots
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| s.prefetch_cover.is_some())
                    .map(|(p, _)| p)
                    .collect();
                // 预算约束：保留的字节 ≤ 40MB（构造时每页 ~5MB → ≤8 页）；但逐出的**顺序**
                // 必须回退侧（p<10）后逐：回退侧余页数 ≥ 前向侧余页数、且**最远保留的回退侧页
                // 距中心 ≥ 最远保留的前向侧页距中心**（bonus=2 让前向侧同距离先逐）。
                let return_side: Vec<usize> =
                    retained.iter().copied().filter(|&p| p < center).collect();
                let fwd_side: Vec<usize> =
                    retained.iter().copied().filter(|&p| p > center).collect();
                assert!(retained.contains(&center), "中心页保留");
                assert!(
                    return_side.len() >= fwd_side.len(),
                    "回退侧（近期滚过的）应比前向侧多驻留：ret={return_side:?} fwd={fwd_side:?}"
                );
                let farthest_return_dist =
                    center.saturating_sub(*return_side.first().unwrap_or(&center));
                let farthest_fwd_dist = fwd_side
                    .last()
                    .copied()
                    .map(|p| p.saturating_sub(center))
                    .unwrap_or(0);
                assert!(
                    farthest_return_dist >= farthest_fwd_dist,
                    "回退侧最近保留的余页应深于（距中心 ≥）前向侧最远保留页：ret_dist={farthest_return_dist} fwd_dist={farthest_fwd_dist}"
                );
                assert!(
                    retained.len() <= 9,
                    "超预算逐出后残余应显著小于 21 页：{}",
                    retained.len()
                );
            });
        });
    }

    /// L3.4 **cover→tile 换档不变式**：`collect_tile_results` / `evict_tiles` 在「同密度封面在
    /// 位」的页上**只弃瓦片、绝不清 cover**（PDF.js prevCanvas 同款；换档期间封面是唯一稳定
    /// 底图）。调度侧 `cover→tile` 的语义翻转（瓦片兜底）不得波及在位封面。
    #[gpui::test]
    async fn cover_to_tile_transition_never_clears_cover(cx: &mut TestAppContext) {
        use gpui::AppContext as _;
        let v: gpui::Entity<PdfViewer> = cx.update(|app| app.new(|cx| minimal_viewer(cx)));
        let (cscale, scale, bmp) = cx.update(|app| {
            let v = v.read(app);
            let s = render_scale(v.page_width(), 1.0, v.dims[0].0);
            (cover_scale(v.dims[0], v.page_width(), 1.0), s, page_bitmap_size(v.dims[0], s))
        });
        cx.update(|app| {
            v.update(app, |v, _cx| {
                // 同密度封面 + 一枚瓦片并存（模拟换档交界状态）
                let cover = render_image_from_bgra(vec![0x30; 4], 1, 1).expect("封面位图");
                v.slots[0].cover = Some(cover);
                v.slots[0].cover_scale = cscale;
                let (tx, rx) = mpsc::channel();
                let _ = tx.send(Some(TileResult {
                    page: 0,
                    row: 0,
                    col: 0,
                    x0: 0,
                    y0: 0,
                    w: bmp.0,
                    h: bmp.1,
                    full_w: bmp.0,
                    full_h: bmp.1,
                    bgra: vec![0u8; (bmp.0 * bmp.1 * 4) as usize],
                    recolor_epoch: 0,
                    luma: false,
                }));
                v.pending_tiles.insert((0, 0, 0), (scale, 0, rx));
                // ① collect_tile_results：同密度封面在位 → 新到瓦片被弃、cover 绝不清除。
                let any = v.collect_tile_results(1.0);
                assert!(!any, "同密度封面在位的页其晚到瓦片应被回收（不显示）");
                assert!(
                    v.slots[0].cover.is_some(),
                    "换档期间 cover 不得被 collect_tile_results 清除"
                );
                assert!(v.slots[0].tiles.is_empty(), "该页瓦片不应入槽（封面已供给）");
                // ② evict_tiles：同密度封面冗余清扫只清瓦片、绝不清 cover。
                v.slots[0].tiles.insert(
                    (0, 0),
                    TileSlot {
                        image: render_image_from_bgra(vec![0x40; 4], 1, 1).expect("瓦片"),
                        x0: 0,
                        y0: 0,
                        w: 1,
                        h: 1,
                    },
                );
                v.slots[0].tile_scale = scale;
                v.slots[0].tile_bytes = 4;
                v.evict_tiles(0, 0, 1.0);
                assert!(
                    v.slots[0].cover.is_some(),
                    "换档期间 cover 不得被 evict_tiles 清除"
                );
                assert!(v.slots[0].tiles.is_empty(), "同密度封面冗余瓦片应被清扫");
            });
        });
    }
}

