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
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};

use gpui::{
    App, AppContext, ClipboardItem, Context, DragMoveEvent, FocusHandle, Hsla, ImageSource,
    InteractiveElement, IntoElement, KeyDownEvent, Modifiers, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, ParentElement, Pixels, Point, PromptLevel, Render, RenderImage,
    ScrollHandle, ScrollWheelEvent, SharedString, StatefulInteractiveElement, Styled, Window, div,
    img, px, rgb, rgba,
};
use ui::scrollbar::{Scrollbar, ScrollbarMode};

use crate::pdf::parser::{RawKind, RecolorKey};
use crate::recolor::RecolorParams;

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
    /// 分页模式几何特征（R6）：分页时 = 首行页显示宽 `page_cell_width(0)`（f32 位型，
    /// 即 `cell_w_per_row[0] × zoom`）——缩放变化必进 sig，防静态帧门误判分页模式稳定
    /// （分页缩放时 `page_width` 维虽也变，但显式纳入 cell_w 更贴近分页几何真源，
    /// 且模式切换 cell_w 从 0↔非 0 跳变亦被捕获）；非分页 = 0。
    paged_cell_w: u32,
    /// 页排模式位（R6）：0=连续/网格、2=Facing（= `1u8 + (page_mode == Facing) as u8`；
    /// 分页态只剩 Facing，非分页 = 0）——分页态必进 sig（cell_w 0↔非 0 已捕获，此位
    /// 作分页态兜底维保留）。
    page_mode: u8,
}

impl WindowSig {
    pub(crate) fn of(
        v: &PdfViewer,
        scroll_y: f32,
        page_width: f32,
        scale_factor: f32,
        viewport_h: f32,
    ) -> Self {
        // R6：分页模式几何特征——cell_w（首行页显示宽）+ 模式位。非分页 = 0/0。
        let (paged_cell_w, page_mode) = if v.paged_mode_active() {
            (
                v.page_cell_width(0).to_bits(),
                1u8 + (v.page_mode == PageMode::Facing) as u8,
            )
        } else {
            (0u32, 0u8)
        };
        Self {
            scroll_top: scroll_y.to_bits(),
            scroll_x: f32::from(-v.scroll.offset().x).to_bits(),
            page_width: page_width.to_bits(),
            scale_factor: scale_factor.to_bits(),
            viewport_w: f32::from(v.scroll.bounds().size.width).to_bits(),
            viewport_h: viewport_h.to_bits(),
            pages: v.dims.len(),
            paged_cell_w,
            page_mode,
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
    RestoreMemory {
        bytes: Vec<u8>,
        annots_before: Vec<PendingAnnot>,
    },
}

use crate::pdf::layout::{
    GridLayout, PagedLayout, Slot, TileSlot, advance_match_index, bitmap_display_scales,
    column_left, compute_layout, content_to_local, cover_affordable, cover_scale, display_height,
    first_visible_page, grid_layout, grid_zoom_limits_for_cols, hit_char, keep_window, local_to_pt,
    min_ar, page_bitmap_size, page_segment_text, paged_layout, paged_layout_scaled, paged_render_window,
    pt_to_local, render_scale, selection_spans_pt, visible_tile_range_axis, zoom_rescroll_anchor,
    zoom_rescroll_anchor_paged,
};
use crate::pdf::parser::PDF_MERGE_MAX_SRC_BYTES;
use crate::pdf::{
    AnnotKind, AnnotateItem, AnnotationInfo, DocId, PageText, PdfImageBox, PdfImagePixels,
    PdfOutlineEntry, PdfSearchHit, PdfSearchResults, PdfWorker, QuadPt, RecolorEntry,
    RenderRequest, RenderResult, TileRequest, TileResult, render_image_from_bgra,
};
use config::pdf::{
    COVER_INFLIGHT, IMAGE_CACHE_PAGES, IMAGE_INFLIGHT, JUMP_PAGES_THRESHOLD, MAX_MID_INFLIGHT,
    MAX_THUMB_INFLIGHT, MID_CACHE_PAGES, MOVE_THRESHOLD_LOWER_RATIO, MOVE_THRESHOLD_PX,
    MOVE_THRESHOLD_VERY_RATIO, PAGED_GAP_X, PAGED_GAP_Y, PAGE_PAD_X, PAGE_WIDTH,
    PREFETCH_COVER_MARGIN, PREFETCH_COVER_MARGIN_MAX, PREFETCH_RETURN_SIDE_BONUS,
    STOP_DETECT_FRAMES, STOP_DETECT_PX, TEXT_CACHE_PAGES, TEXT_INFLIGHT, THUMB_CACHE_PAGES,
    TILE_CACHE_BYTES, TILE_INFLIGHT, TILE_MARGIN, TILE_SIZE, ZOOM_MAX, ZOOM_MIN, ZOOM_STEP,
};
use viewer::pan_zoom;
/// 是否开启视图侧计时日志（`VUICK_PDF_TIMING=1`，进程内缓存一次）。
fn timing_view() -> bool {
    static EN: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *EN.get_or_init(|| {
        std::env::var_os("VUICK_PDF_TIMING").is_some_and(|v| v == "1" || v == "true")
    })
}

/// 是否开启**真机取证探针**（`VUICK_DIAG`，进程内缓存一次；未设零开销——
/// 仅有缓存到位的 O(1) 布尔查，无任何打印/行为影响）。供静态帧门控早退分支等
/// 只读观测点使用，只加日志不改任何调度语义。
fn diag_enabled() -> bool {
    static EN: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *EN.get_or_init(|| std::env::var_os("VUICK_DIAG").is_some())
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
mod input;
mod open;
mod render;
mod search;
mod tiles;

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

// ---- 网格视图（Grid）常量（wave3；推导见各条注释）----
/// 网格缩略图**请求卡宽**（逻辑 px）：A4（595×842pt，纵横比 ≈1.41）在 280px 宽下卡高
/// ≈396px，一屏 ~5 列可辨标题/段落结构；与阅读器 L0 缩略图同量级栅格带宽。请求目标宽经
/// worker 按 [`config::pdf::GRID_BUCKET_PX`] 分桶缓存（相邻宽落同桶复用，不重栅）。
pub(crate) const GRID_CARD_WIDTH: f32 = 280.0;
/// 网格卡片行内/行间间距（px）。取 12：与页列视图 [`PAGE_GAP`]（卡片间距）同量级；过大
/// 切碎整屏卡数、过小卡卡粘连难区分。
pub(crate) const GRID_GAP: f32 = 12.0;
/// 网格四周外留白（px）。取 16：常见内容区 8-16px 边距，网格四边不贴死、留统一画布衬底。
pub(crate) const GRID_PADDING: f32 = 16.0;
/// 网格缩放**列数钳**（K167 本线）：网格 Ctrl+滚轮 / `set_grid_zoom` 不再用阅读共享的
/// [ZOOM_MIN, ZOOM_MAX] 钳网格，改按当前视口宽经 [`crate::pdf::layout::grid_zoom_limits_for_cols`]
/// 动态钳到 `[GRID_MAX_COLS, GRID_MIN_COLS]` 列：
/// - `GRID_MAX_COLS` = 放大上限的**最少**列数（2：最大只能放大到 2 列，不可能到 1 列）；
/// - `GRID_MIN_COLS` = 缩小下限的**最多**列数（9：最大只能缩小到 9 列，不可能到 10+ 列）。
/// 任意钳内 `grid_zoom` ⇒ 恒有 `GRID_MAX_COLS ≤ cols ≤ GRID_MIN_COLS`（推导见该函数注释）。
pub(crate) const GRID_MAX_COLS: f32 = 2.0;
pub(crate) const GRID_MIN_COLS: f32 = 9.0;
/// 网格行带虚拟化的上下外扩缓冲行数：可视行之外各多物化 [`Self::grid_visible_range`]
/// 一行的页卡，滚动/惯性时新行提前就位——对齐页列视图 `keep_window` 的 MARGIN 页缓冲语义。
pub(crate) const GRID_ROW_BUFFER: usize = 1;
/// 网格顶栏（wave5 工具栏）高度（px）：贴网格滚动容器顶部，滚动条随之下移对齐（见
/// `render.rs` 的滚动条 overlay 顶偏）。选 40：一行胶囊按钮 + 上下留白够点，不侵占卡区。
pub(crate) const GRID_TOOLBAR_H: f32 = 40.0;

/// 网格可视页区间（纯函数，含行带上下缓冲）：`layout` 为当帧 [`grid_layout`] 结果，
/// `offset` = 内容已滚动 px（≥0），`viewport_h` = 滚动容器可视高。取首/末可视行外侧各扩
/// [`GRID_ROW_BUFFER`] 行后，返回该行带内页下标的闭区间 `[start, end]`（物化集合）；0 行
/// 布局 / 视口完全落在内容之外（[`GridLayout::first_visible_grid_row`] = None）→ `None`
/// （本帧不物化任何卡）。渲染与调度都走本函数，保证「物化的 = 派发的 = 保留的」同一几何。
fn grid_visible_range(layout: &GridLayout, offset: f32, viewport_h: f32) -> Option<(usize, usize)> {
    let (r0, r1) = layout.first_visible_grid_row(offset, viewport_h)?;
    let r0 = r0.saturating_sub(GRID_ROW_BUFFER);
    let r1 = (r1 + GRID_ROW_BUFFER).min(layout.rows.saturating_sub(1));
    let start = layout.row_page_start[r0];
    let end = layout.row_page_start[r1 + 1].saturating_sub(1);
    if end < start {
        return None;
    }
    Some((start, end))
}

// ---- 网格拖拽重组（wave6）：纯命中 / 纯重排 / 拖拽态载荷类型 ----

/// 网格卡拖拽载荷（`on_drag` 的 value 类型）：只记被按下的页下标；选中组在拖拽开始
/// （[`PdfViewer::grid_drag_begin`]）自选中集快照读取。
#[derive(Clone, Copy, Debug)]
pub(crate) struct GridDragData {
    /// 按下的页（原页序下标）。
    pub page: usize,
}

/// 网格「按下」待决单击（wave6）：卡 `on_mouse_down` 记录，`on_mouse_up` 时若**未**进入
/// 拖拽（位移未超阈值）则按 wave5 语义消费；一旦进入拖拽即作废——拖拽与单击用位移阈值
/// 互斥（按下即触发会误触勾选/跳页）。
#[derive(Clone, Copy, Debug)]
pub(crate) struct GridPress {
    /// 按下的页（原页序下标）。
    pub page: usize,
    /// 按下修饰键（决定单击是 Ctrl 切换 / Shift 范围 / 普通选中）。
    pub modifiers: Modifiers,
    /// 按下时的点击计数（1 = 单击、≥2 = 双击/多击）。gpui 双击序列：第一击 count=1 先执行
    /// 「选中」（留在网格），随后第二击 count=2 才「退出网格 + 跳页」——与文件管理器
    /// 「先选定再打开」一致。
    pub click_count: usize,
}

/// 网格拖拽重组状态（wave6）：`Some` = 正在拖拽页组。
#[derive(Clone, Debug, Default)]
pub(crate) struct GridDrag {
    /// 被拖页组：原页序下标，升序去重（拖拽开始时的选中集快照）。
    pub dragging: Vec<usize>,
    /// 当前插入目标线性位（0..=页数；`None` = 指针不在网格内容内 → 松手放弃不提交）。
    pub insert: Option<usize>,
    /// 指针当前**网格内容**坐标（ghost 随指针定位）。
    pub pointer: gpui::Point<f32>,
}

/// 计算指针（网格内容坐标）对应的**线性插入位**（0..=页数；网格外 → `None`）。
///
/// 返回「插入到哪个原序下标之前」的线性位（0 = 最前、n = 末尾），插入指示线即画在该位
/// 目标卡左缘 / 末卡右缘。**网格内容矩形** `(0, 0, vw, total_height)` 之外 → `None`
/// （松手在网格外 = 放弃，不提交）。内部映射（不依赖逐卡命中，与 GridLayout 同源几何）：
/// - 行：y ∈ `[padding, total_height − padding)` 内按 `row_index_at_y`（行二分原语）定位行
///   `r`；顶部/底部外留白 → 0 / n（末行底 = `total_height − padding`，grid_layout 推导）；
/// - 列：`col_slot_at_x`（列槽原语）= 卡左半/右半、列间 gap 与右留白都归到最近整数槽
///   （对齐 tab 拖拽「卡左半前、右半后」语义，且 gap 中点即阈值）；
/// - 线性位 = `row_page_start[r] + clamp(col_slot, 0, row_len)`；**垂直行间 gap**（y 在行底
///   `row_top[r+1] − gap` 之下、非末行）→ 直接 = `row_page_start[r+1]`（本行末 = 下一行首，
///   同一线性位）。列槽越行尾 → 行内最后（下一行首 / 整档末尾）。
/// 手写算式样例见测试 [`tests::compute_insert_index_hand_cases`]（K177：期望值先演算后落）。
pub(crate) fn compute_insert_index(layout: &GridLayout, vw: f32, x: f32, y: f32) -> Option<usize> {
    let n = layout.row_page_start.last().copied().unwrap_or(0);
    if n == 0 || layout.rows == 0 {
        return None;
    }
    if x < 0.0 || x > vw || y < 0.0 || y > layout.total_height {
        return None; // 网格外
    }
    if y < layout.padding {
        return Some(0); // 顶部 padding → 插入最前
    }
    if y >= layout.total_height - layout.padding {
        return Some(n); // 底部 padding / 末行之下 → 插入末尾
    }
    // 行：grid_layout 行二分原语「最后一个行顶 ≤ y 的行」（顶部 padding 已在上层排除）。
    let r = layout.row_index_at_y(y);
    // 垂直行间 gap（非末行）：y 已在行 `r` 底缘之下、下一行顶之上 → 该位 = 本行末 =
    // 下一行首（`row_page_start[r+1]`，同一线性位）；行底 = `row_top[r+1] − gap`。
    if r + 1 < layout.rows && y >= layout.row_top[r + 1] - layout.gap {
        return Some(layout.row_page_start[r + 1]);
    }
    let base = layout.row_page_start[r];
    let row_len = layout.row_page_start[r + 1] - base;
    // 列：grid_layout 列槽原语（最近整数槽，gap 中点即阈值）。
    let col_slot = layout.col_slot_at_x(x);
    Some((base + col_slot.min(row_len)).min(n))
}

/// 由「原长度 + 被拖页组（升序去重原下标）+ 插入线性位」计算新的**全排列**（页序）。
///
/// 语义：被拖组从原序移除后，以整块插到「原序线性位 `insert` 所指槽」（0 = 最前、
/// n = 末尾）——拖拽期间网格按**原序**展示、指示线即原序 gap，故 `insert` 在原序空间。
/// 插入位（rest 内下标）算式：`pos = insert − count(dragging < insert)`——原序前 `insert`
/// 个元素去掉被拖组后恰为 rest 的前 `insert −（被拖组中下标 < insert 的个数）` 个。随后
/// 把被拖块（升序 = 原相对顺序）整体 splice 进 `pos`。
/// 边界：连续被拖组在自身跨度内松手（insert ∈ (lo, hi]）→ 该删除位即块边缘 → 序不变；
/// insert 越界夹到 `[0, n]`；空组 → 恒等；全选 → 整体平移（序不变）。
/// 手写算式样例见测试 [`tests::apply_reorder_hand_cases`]。
pub(crate) fn apply_reorder(orig_len: usize, dragging: &[usize], insert: usize) -> Vec<usize> {
    let insert = insert.min(orig_len);
    let mut in_drag = vec![false; orig_len];
    for &d in dragging {
        if d < orig_len {
            in_drag[d] = true;
        }
    }
    let mut rest: Vec<usize> = (0..orig_len).filter(|&i| !in_drag[i]).collect();
    let before = dragging.iter().filter(|&&d| d < insert).count();
    let pos = insert.saturating_sub(before).min(rest.len());
    rest.splice(pos..pos, dragging.iter().copied());
    rest
}

/// 网格缩略图缓存条目：页卡位图 + 其**所属分桶**（`grid_bucket_width` GRID_BUCKET_PX=32
/// 量化，见 `pdf_core/parser.rs`）。跨桶缩放时旧桶条目**保留作桥**（渲染时 CSS 拉伸铺满
/// 新卡盒，杜绝整屏占位白闪），仅新桶结果回填时替换；`bucket == 当前桶` 才是「就绪」
/// 图，否则视为 stale（调度重派、渲染回退桥）。`bucket` 为 0 不可能出现（分桶 ≥32）。
#[derive(Clone)]
pub(crate) struct GridThumb {
    /// 该位图栅格化时的分桶宽（px）。
    pub(crate) bucket: u32,
    /// 已就绪的页卡位图。
    pub(crate) image: Arc<RenderImage>,
}

/// 阅读页排模式（状态层打桩：本切片**只存状态**，渲染三路分派/滚动锚点恢复由后续切片接）：
/// - [`Continuous`]：连续滚动（单列全文，默认）
/// - [`Facing`]：双页对开（一次两页）
/// 与网格总览（[`PdfViewer::grid_mode`]）**正交**：网格进出保留本值，`set_grid_mode`
/// 不改 page_mode（网格只是一时总览，读写它不清阅读页排模式）。分页态 = Facing
/// （历史 Single 单页模式已删除，`page_mode != Continuous` 即分页的判断式不变）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PageMode {
    /// 连续滚动（默认）。
    Continuous,
    /// 双页对开模式。
    Facing,
}

/// 鼠标焦点缩放锚（Ctrl+滚轮「以鼠标所指页面点为焦点」）：真实滚轮命中页时捕获
/// 「鼠标窗口坐标 + 命中页内 PDF 点」——缩放前后让**同一 PDF 内容点**钉在同一屏幕位置。
/// 命中空白（沟槽/留白/状态栏）→ [`super::view::input::PdfViewer::window_to_pt`] 返 `None`
/// → 无焦点 → 视口顶锚（既有 Z1/Z1-S6 语义不变）；滑条/程序化缩放不传焦点同样落顶锚。
#[derive(Clone, Copy, Debug)]
pub(crate) struct ZoomMouseFocus {
    /// 鼠标窗口坐标（缩放锚点要钉住的屏幕位置）。
    pub(crate) pos: Point<Pixels>,
    /// 命中页下标。
    pub(crate) page: usize,
    /// 命中页内 PDF 点 x（页内原点左下、y 向上坐标系）。
    pub(crate) px: f32,
    /// 命中页内 PDF 点 y。
    pub(crate) py: f32,
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
    pub(crate) pending_cover: HashMap<usize, (f32, u64, u64, mpsc::Receiver<Option<RenderResult>>)>,
    /// F4 空闲预取走廊在途请求：`页` → (半密度 scale, 导航世代 nav_gen, 结果通道)。独立于
    /// [`Self::pending_cover`]（密度守卫不同：0.5× 封面、低优先级、仅空闲期消化），见
    /// `tiles::collect_prefetch_cover_results` 与 `tiles::prefetch_corridor`。非空期间自刷新泵
    /// 继续出帧以收集走廊结果。`nav_gen` 用于 L2.4 大跳世代推进清旧条目。
    pub(crate) pending_prefetch_cover:
        HashMap<usize, (f32, u64, mpsc::Receiver<Option<RenderResult>>)>,
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
    /// S1/S3：缩放派发已武装（Ctrl+滚轮 [`super::view::input::PdfViewer::adjust_zoom_mouse`] 置位）。
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
    /// **非溢出档水平内容偏移**（Ctrl+滚轮鼠标焦点 x 锚专用，2026-11-20）：页列在视口内
    /// 居中（居中 = 0，正值 = 页列整体右移、负值 = 左移；单位 CSS px）。仅非横向溢出
    /// （`zoom_target_impl` 的 `max_sx == 0`，页宽 + 2·PAD ≤ 视口）时由本字段承担横向锚；
    /// 横向溢出时由 `scroll.offset().x` 承担、本字段强制归 0（见 open.rs）。
    /// 鼠标缩放（[`super::view::open::PdfViewer::zoom_target_impl`]）把「缩放后同内容点窗口位
    /// − 鼠标屏位」增量落进本字段，保持鼠标所指内容点同屏；渲染（render.rs wrap 外边距）/
    /// 命中（`window_to_pt`/`pt_to_window`）/横向量程（tiles page_visible_rect）三处同口径
    /// 消费（K153）。**复位规则**（归 0 回中）：普通滚动、`scroll_to_page`/跳页、`set_page_mode`、
    /// 进出网格、`set_zoom_abs`/`set_zoom` 等任何非「鼠标缩放」的水平位移（各守卫处清 0），
    /// 连续 Ctrl+滚轮鼠标在页上期间保持。
    pub(crate) x_zoom_offset: f32,
    /// **上一档 Ctrl+滚轮鼠标命中的页内内容点**（沟槽/留白兜底锚，2026-11-20）：本档
    /// `window_to_pt` 未命中（鼠标落页外/沟槽）但上一档命中页内时，按本值锚 **y**（该 PDF
    /// 内容点钉在上一档屏位）防「x 重居中把鼠标推出页缘 → 瞬时回落视口顶锚 → y 整段跳」；
    /// **x** 交由量程分派维持既有/居中。连续 Ctrl+滚轮期间由 `zoom_target_impl` 写入；普通
    /// 滚动/跳页/模式切换/平移等非缩放交互时清空（与 `x_zoom_offset` 同批复位）。
    pub(crate) last_mouse_focus: Option<ZoomMouseFocus>,
    /// **非缩放滚动检测快照**（评审 nit，2026-11-20）：`ensure_window` 每帧比对
    /// `scroll.offset()` 与本值，变化且不在缩放防抖（`!zoom_armed`）→ 判定为滚动条拖拽 /
    /// 键盘滚动等**非「鼠标缩放」滚动** → `reset_x_zoom()`（清水平偏移 + 沟槽兜底锚，回中）。
    /// 连续 Ctrl+滚轮缩放序列内（`zoom_armed`，offset 每档变化）不触发，保兜底锚。
    pub(crate) last_ui_scroll: (f32, f32),
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
    /// **全文档最小纵横比** `min(w_i/h_i)`（跳过退化页，全退化 → 1.0，见 [`layout::min_ar`]）：
    /// 连续（默认）模式「整页入屏」（contain-fit）用——`base_w = min(content_w,
    /// viewport_h·min_ar)`，与双页（Facing）模式同 fit 口径 → 同一缩放值（状态栏 zoom%）
    /// 下两模式页视觉大小一致。`apply_loaded` 由 `self.dims` 一次算出并缓存（避免每帧
    /// O(n) 全扫 dims）；测试夹具（`paged_viewer`）同步设值防 stale。
    pub(crate) doc_min_ar: f32,
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
    /// 「提取页面」**成功**的提示文案（「第 N 页 → 路径」）：异步成功路径置位 + `notify`，
    /// 壳层 `_pdf_sub` observe 消费一次（`take` 清空）弹成功 toast——成败提示都收敛到
    /// 异步完成的确定性结果，菜单不再在同步调用后臆断成败（防成功 toast 恒假 / 双 toast）。
    pub(crate) last_extract_success: Option<String>,
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
    // ---- 网格视图（Grid）状态（wave3）：页卡网格模式的独立滚动 + 缩略图槽 ----
    /// 网格模式是否激活（false = 单列阅读；true = 页卡网格视图）。
    /// 独立于单列 `scroll`/`page_tops` 全套状态——两模式各自持有滚动偏移，切模式互不干扰。
    pub(crate) grid_mode: bool,
    /// 阅读页排模式：连续/双页；与网格总览**正交**——网格进出保留本值（读写网格
    /// 不清 page_mode）。本切片只存状态 + 通知，渲染三路分派/滚动锚点恢复由后续切片接
    /// （见 `view/open.rs` 的 `set_page_mode`）。
    pub(crate) page_mode: PageMode,
    /// Facing 分页「sticky 列记忆」（0 = 左/偶页、1 = 右/奇页）：同行两列顶对齐 → y 轴无法
    /// 区分「读者实际在读哪列」，模式切换/进度捕获必须靠**确定性锚事件**钉列——Facing
    /// restore（`ensure_window` 按恢复页 `page % cols` 写入）与 Facing `scroll_to_page`
    /// （按目标页列写入）两处是唯一锚源。
    /// [`Self::reading_progress`] 分页分支据此解析「真正当前页」（`row_page_start[r0] +
    /// facing_anchor_col`，末行 ragged 仅 1 页时夹回 0），使连续 ↔ Facing 的 `(page, frac)`
    /// 互为精确逆（修复「切模式读 spread 右/奇页丢页回退」红测）。默认 0 = 既有行左页语义
    /// （未锚过列时保持旧行为，不引入默认翻转）。
    pub(crate) facing_anchor_col: usize,
    /// 网格卡**独立缩放**倍数（网格 Ctrl+滚轮可调；[`Self::grid_card_width`] =
    /// `GRID_CARD_WIDTH × grid_zoom`）。**独立于单列阅读缩放 [`Self::zoom`]**：网格里
    /// Ctrl+滚轮只改卡大小，退出网格回到阅读保持用户阅读缩放；阅读 Ctrl+滚轮/`set_zoom`
    /// 也不影响网格卡宽。**列数钳制**：缩放区间按当前视口宽经
    /// [`crate::pdf::layout::grid_zoom_limits_for_cols`] 动态钳到 2/9 列（见 [`GRID_MAX_COLS`]/
    /// [`GRID_MIN_COLS`]）——不再是阅读共享的 [ZOOM_MIN, ZOOM_MAX]。网格会话语义：文档
    /// `release`/切换复位 1.0，同文档内存重载（页操作 reload）期间保持。
    pub(crate) grid_zoom: f32,
    /// 最近一次网格 render 已知的**视口宽**（CSS px，来自 [`Self::grid_viewport`] 宽轴）：
    /// 供无 window 句柄的 [`PdfViewer::set_grid_zoom`] 算动态列数钳用（`grid_scroll`
    /// bounds 未布局时回退本值；非网格/未渲染过 = 0）。仅网格 render 帧更新。
    pub(crate) last_grid_viewport_w: f32,
    /// 网格模式独立滚动句柄（`pdf-grid-scroll` 容器；与单列 `scroll` 完全隔离）。
    pub(crate) grid_scroll: ScrollHandle,
    /// 网格缩略图：页 → 已就绪卡位图 + 所属分桶（经 [`super::PdfWorker::request_grid`] 拉取；
    /// 独立于 `slots` 的 LOD 槽——网格卡图只存这里，不碰 thumb/mid/cover 预算）。跨桶缩放
    /// **保留**旧桶条目作桥（`reset_grid_dispatch_for_zoom_bucket` 只清在途不清图），bucket
    /// 判断 stale：调度对有 stale 图照常重派、渲染回退画桥（见 `GridThumb`）。
    pub(crate) grid_thumbs: HashMap<usize, GridThumb>,
    /// 在途网格缩略图请求：页 → (导航世代 nav_gen, 结果通道)。世代守卫对齐
    /// [`Self::pending_thumb`]（大跳/切档时按 nav_gen/zoom_epoch 由 worker drop 旧请求）。
    pub(crate) pending_grid: HashMap<usize, (u64, mpsc::Receiver<Option<RenderResult>>)>,
    /// 网格 render 帧观测：上帧实际物化/绘制的页区间（含行带 ±1 缓冲）——headless
    /// 虚拟化断言用（对齐 `last_frame_tiles_drawn` 既有观测姿势；非网格帧 = (0,0)）。
    pub(crate) grid_last_range: std::cell::Cell<(usize, usize)>,
    /// 网格 render 帧观测：上帧实际物化的页卡总数（= `grid_last_range` 覆盖页数）。
    pub(crate) grid_last_drawn: std::cell::Cell<usize>,
    /// 网格占位命中计数：网格卡**完全无缩略图**（无当前桶、也无旧桶桥图）才画占位时分支
    /// 自增（`render::build_grid_cards` 的 None 分支；与单列 `loading_placeholder_hits` 对称，
    /// 二者不共享——单列计数不覆盖网格卡路径）。回归断言网格 Ctrl+滚轮跨桶缩放期间桥图
    /// 生效、占位不增长而新增；`release` 时归零。
    pub(crate) grid_placeholder_hits: std::cell::Cell<u64>,
    // ---- 网格多选（wave5）：选中集模型 + Shift 范围锚点 ----
    /// 当前网格**选中页下标集**（`HashSet` 提供 O(1) 存在查询；页数为千级时全选集内存
    /// ~几十 KB，量级可接受）。**一次网格会话语义**：进入网格、退出网格（含点卡跳页、
    /// toggle 退出）与文档页数变化（重载/页操作后）都清空——选中的是「本次浏览快照」，
    /// 不跨会话/跨页码集合串场（详见 `view/open.rs` 的 `set_grid_mode`/`apply_loaded`）。
    pub(crate) grid_selected: HashSet<usize>,
    /// Shift 范围选**锚点页**：上次「单选 / Ctrl 切换 / checkbox 勾选」更新的页下标；
    /// Shift+单击以其为闭区间起点选中（其它已选页保持）。无锚点时 Shift 退化单选。
    pub(crate) grid_anchor: Option<usize>,
    // ---- 网格拖拽重组（wave6）：按下待决单击 + 拖拽态 + 每帧布局快照 ----
    /// 网格「按下」待决单击（wave6）：卡按下记录，松手时若未进入拖拽按 wave5 语义消费；
    /// 进入拖拽即作废（拖拽与单击用位移阈值互斥见 [`GridPress`]）。
    pub(crate) grid_press: Option<GridPress>,
    /// 网格拖拽重组状态（wave6）：`Some` = 正在拖拽页组（见 [`GridDrag`]）。
    pub(crate) grid_drag: Option<GridDrag>,
    /// 网格布局每帧快照（wave6）：「渲染期写、事件期读」对齐 TabBoundsRecorder 姿势——
    /// render 每帧把 [`GridLayout`] 抄入，`on_drag_move` 事件期读上一帧布局算插入命中。
    pub(crate) grid_layout_cell: std::cell::RefCell<Option<GridLayout>>,
    /// 分页页排（Facing）布局每帧快照：`ensure_window` 每帧以 zoom=1（fit）口径、
    /// 真实阅读视口重算 [`PagedLayout`] 并抄入，render 期读上一帧布局做分页定位（同
    /// `grid_layout_cell`「渲染期写、render 期读」姿势，下一页排命中/滚动沿用本快照）。
    /// 连续模式 / 网格 / 空文档 → `None`（渲染走连续或网格分支，不读它）。
    pub(crate) paged_layout_cell: std::cell::RefCell<Option<PagedLayout>>,
    /// 拖拽重排后的 reload 保持网格模式（wave6）：`grid_drag_release` 提交前置位，
    /// [`PdfViewer::apply_loaded`] 消费后清——拖拽重组是网格内操作，reload 不应把用户踢
    /// 回单列（默认 reload 复位到单列语义仅适用非拖拽的普通重载）。
    pub(crate) grid_hold_on_reload: bool,
}

impl PdfViewer {
    /// 当前阅读页排模式（连续/双页；与网格总览正交，进出网格保留）。
    /// shell 状态栏页排钮选中态读取；渲染三路分派由后续切片接。
    pub fn page_mode(&self) -> PageMode {
        self.page_mode
    }

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
    use crate::pdf::parser::grid_bucket_width;
    use crate::pdf::test_util;
    use gpui::AppContext as _;
    use gpui::Entity;
    use gpui::TestAppContext;
    use gpui::VisualTestContext;
    use std::cell::RefCell;
    use std::rc::Rc;

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
                    x_zoom_offset: 0.0,
                    last_mouse_focus: None,
                    last_ui_scroll: (0.0, 0.0),
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
                    doc_min_ar: 1.0,
                    scale_factor: 1.0,
                    window_sig: None,
                    window_settled: false,
                    idle_gate_skips: 0,
                    reload_anchor: None,
                    restore_anchor: None,
                    restore_pending: None,
                    last_modify_error: None,
                    last_extract_success: None,
                    sel_card: None,
                    pending_annots: Vec::new(),
                    undo_stack: VecDeque::new(),
                    pending_page_ops: 0,
                    next_annot_id: 0,
                    focused_annot: None,
                    panning: false,
                    pan_origin: gpui::point(px(0.), px(0.)),
                    pan_offset0: gpui::point(px(0.), px(0.)),
                    grid_mode: false,
                    page_mode: PageMode::Continuous,
                    facing_anchor_col: 0,
                    grid_zoom: 1.0,
                    last_grid_viewport_w: 0.0,
                    grid_scroll: ScrollHandle::new(),
                    grid_thumbs: HashMap::new(),
                    pending_grid: HashMap::new(),
                    grid_last_range: std::cell::Cell::new((0, 0)),
                    grid_last_drawn: std::cell::Cell::new(0),
                    grid_placeholder_hits: std::cell::Cell::new(0),
                    grid_selected: HashSet::new(),
                    grid_anchor: None,
                    grid_press: None,
                    grid_drag: None,
                    grid_layout_cell: std::cell::RefCell::new(None),
                    paged_layout_cell: std::cell::RefCell::new(None),
                    grid_hold_on_reload: false,
                };
                // 先滚到文档中段（纵向负偏移），作为缩放重锚的基准。⚠️ 须落在新布局
                // max_offset 内（600 < 放大一档后的 `total_new−viewport` ≈ 1249）——越界
                // 起点的 B 场景由 #2 clamp 处理（见 lib.rs `zoom_bottom_clamp_*` 专测），
                // 这里锁的是「锚点页重锚分毫不差」的 Z1 公式（clamp 后公式不再可见）。
                v.scroll.set_offset(gpui::point(px(-40.0), px(-600.0)));
                assert!(
                    (v.total_height - total).abs() < 1e-3,
                    "build 完 total_height"
                );
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
        let base =
            super::render::page_base_image(None, Some(&old_cover), None, Some(&mid), Some(&thumb));
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
                    x_zoom_offset: 0.0,
                    last_mouse_focus: None,
                    last_ui_scroll: (0.0, 0.0),
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
                    doc_min_ar: 1.0,
                    scale_factor: 1.0,
                    window_sig: None,
                    window_settled: false,
                    idle_gate_skips: 0,
                    reload_anchor: None,
                    restore_anchor: None,
                    restore_pending: None,
                    last_modify_error: None,
                    last_extract_success: None,
                    sel_card: None,
                    pending_annots: Vec::new(),
                    undo_stack: VecDeque::new(),
                    pending_page_ops: 0,
                    next_annot_id: 0,
                    focused_annot: None,
                    panning: false,
                    pan_origin: gpui::point(px(0.), px(0.)),
                    pan_offset0: gpui::point(px(0.), px(0.)),
                    grid_mode: false,
                    page_mode: PageMode::Continuous,
                    facing_anchor_col: 0,
                    grid_zoom: 1.0,
                    last_grid_viewport_w: 0.0,
                    grid_scroll: ScrollHandle::new(),
                    grid_thumbs: HashMap::new(),
                    pending_grid: HashMap::new(),
                    grid_last_range: std::cell::Cell::new((0, 0)),
                    grid_last_drawn: std::cell::Cell::new(0),
                    grid_placeholder_hits: std::cell::Cell::new(0),
                    grid_selected: HashSet::new(),
                    grid_anchor: None,
                    grid_press: None,
                    grid_drag: None,
                    grid_layout_cell: std::cell::RefCell::new(None),
                    paged_layout_cell: std::cell::RefCell::new(None),
                    grid_hold_on_reload: false,
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
                    x_zoom_offset: 0.0,
                    last_mouse_focus: None,
                    last_ui_scroll: (0.0, 0.0),
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
                    doc_min_ar: 1.0,
                    scale_factor: 1.0,
                    window_sig: None,
                    window_settled: false,
                    idle_gate_skips: 0,
                    reload_anchor: None,
                    restore_anchor: None,
                    restore_pending: None,
                    last_modify_error: None,
                    last_extract_success: None,
                    sel_card: None,
                    pending_annots: Vec::new(),
                    undo_stack: VecDeque::new(),
                    pending_page_ops: 0,
                    next_annot_id: 0,
                    focused_annot: None,
                    panning: false,
                    pan_origin: gpui::point(px(0.), px(0.)),
                    pan_offset0: gpui::point(px(0.), px(0.)),
                    grid_mode: false,
                    page_mode: PageMode::Continuous,
                    facing_anchor_col: 0,
                    grid_zoom: 1.0,
                    last_grid_viewport_w: 0.0,
                    grid_scroll: ScrollHandle::new(),
                    grid_thumbs: HashMap::new(),
                    pending_grid: HashMap::new(),
                    grid_last_range: std::cell::Cell::new((0, 0)),
                    grid_last_drawn: std::cell::Cell::new(0),
                    grid_placeholder_hits: std::cell::Cell::new(0),
                    grid_selected: HashSet::new(),
                    grid_anchor: None,
                    grid_press: None,
                    grid_drag: None,
                    grid_layout_cell: std::cell::RefCell::new(None),
                    paged_layout_cell: std::cell::RefCell::new(None),
                    grid_hold_on_reload: false,
                };
                // 先给出「静置已确认」状态，验证变更会复位它（触发一帧收集重着色结果并重绘），
                // 而**不**清缓存/不停泵（切档不重栅：泵里在途瓦片结果仍有效、须继续收集）。
                v.window_sig = Some(WindowSig::default());
                v.window_settled = true;
                v.tile_pump_running = true;

                let p = RecolorParams {
                    bg: [0x20, 0x20, 0x20],
                    ink: None,
                };
                v.set_recolor(Some(p), cx);
                assert_eq!(v.recolor, Some(p), "重着色状态应更新（存到 view 侧）");
                assert_eq!(v.window_sig, None, "变更应复位门控（触发一帧收集+重绘）");
                assert!(!v.window_settled, "变更应复位静置确认");
                assert!(
                    v.tile_pump_running,
                    "切档不重栅：不应停泵（在途瓦片结果仍有效）"
                );
                assert!(
                    v.pending_recolor.is_none(),
                    "无 worker 时不发刷新请求、不 panic"
                );

                // 同参短值：不动已确认门控（避免重复刷新）。
                v.window_sig = Some(WindowSig::default());
                v.window_settled = true;
                v.set_recolor(Some(p), cx);
                assert_eq!(
                    v.window_sig,
                    Some(WindowSig::default()),
                    "同参应短路、不失效"
                );
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
            x_zoom_offset: 0.0,
            last_mouse_focus: None,
                    last_ui_scroll: (0.0, 0.0),
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
            doc_min_ar: 1.0,
            scale_factor: 1.0,
            window_sig: None,
            window_settled: false,
            idle_gate_skips: 0,
            reload_anchor: None,
            restore_anchor: None,
            restore_pending: None,
            last_modify_error: None,
            last_extract_success: None,
            sel_card: None,
            pending_annots: Vec::new(),
            undo_stack: VecDeque::new(),
            pending_page_ops: 0,
            next_annot_id: 0,
            focused_annot: None,
            panning: false,
            pan_origin: gpui::point(px(0.), px(0.)),
            pan_offset0: gpui::point(px(0.), px(0.)),
            grid_mode: false,
            page_mode: PageMode::Continuous,
            facing_anchor_col: 0,
            grid_zoom: 1.0,
            last_grid_viewport_w: 0.0,
            grid_scroll: ScrollHandle::new(),
            grid_thumbs: HashMap::new(),
            pending_grid: HashMap::new(),
            grid_last_range: std::cell::Cell::new((0, 0)),
            grid_last_drawn: std::cell::Cell::new(0),
            grid_placeholder_hits: std::cell::Cell::new(0),
            grid_selected: HashSet::new(),
            grid_anchor: None,
            grid_press: None,
            grid_drag: None,
            grid_layout_cell: std::cell::RefCell::new(None),
            paged_layout_cell: std::cell::RefCell::new(None),
            grid_hold_on_reload: false,
        }
    }

    /// 状态栏滑条（绝对缩放目标 [`PdfViewer::set_zoom_abs`]）：设值 + 上下钳位。
    /// 纯状态夹具 `minimal_viewer`（不触碰 pdfium）验证 `zoom_target` 的 clamp 兜底。
    #[gpui::test]
    async fn set_zoom_abs_sets_and_clamps(cx: &mut TestAppContext) {
        let viewer: gpui::Entity<PdfViewer> = cx.update(|app| app.new(|cx| minimal_viewer(cx)));
        // 2.0 直接生效
        cx.update(|app| {
            viewer.update(app, |v, cx| v.set_zoom_abs(2.0, cx));
            assert!(
                (viewer.read(app).zoom - 2.0).abs() < 1e-4,
                "set_zoom_abs(2.0) 直设"
            );
        });
        // 10.0 → 钳到上限 ZOOM_MAX（4.0）
        cx.update(|app| {
            viewer.update(app, |v, cx| v.set_zoom_abs(10.0, cx));
            assert!(
                (viewer.read(app).zoom - ZOOM_MAX).abs() < 1e-4,
                "clamp 上限"
            );
        });
        // 0.1 → 钳到下限 ZOOM_MIN（0.5）
        cx.update(|app| {
            viewer.update(app, |v, cx| v.set_zoom_abs(0.1, cx));
            assert!(
                (viewer.read(app).zoom - ZOOM_MIN).abs() < 1e-4,
                "clamp 下限"
            );
        });
    }

    /// 状态栏滑条（绝对缩放目标 [`PdfViewer::set_zoom_abs`]）：同目标早退——zoom 不变、
    /// worker 缩放世代 `zoom_epoch` 不增（zoom_target 内 1e-4 无变化短路生效）。
    #[gpui::test]
    async fn set_zoom_abs_same_target_noop(cx: &mut TestAppContext) {
        let viewer: gpui::Entity<PdfViewer> = cx.update(|app| app.new(|cx| minimal_viewer(cx)));
        let (zoom, epoch_before, epoch_after) = cx.update(|app| {
            viewer.update(app, |v, cx| v.set_zoom_abs(2.0, cx));
            let before = viewer.read(app).zoom_epoch;
            viewer.update(app, |v, cx| v.set_zoom_abs(2.0, cx));
            (viewer.read(app).zoom, before, viewer.read(app).zoom_epoch)
        });
        assert!((zoom - 2.0).abs() < 1e-4, "同目标 zoom 保持 2.0");
        assert_eq!(epoch_after, epoch_before, "同目标早退，zoom_epoch 不增");
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
        let viewer: gpui::Entity<PdfViewer> = cx.update(|app| app.new(|cx| minimal_viewer(cx)));
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
                assert_eq!(
                    pw,
                    crate::pdf::layout::snap_to_device(raw, sf),
                    "与量化定义一致"
                );
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
        let rec = RecolorParams {
            bg: [0x20, 0x20, 0x20],
            ink: Some([0xD8, 0xD8, 0xD8]),
        };
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
                assert!(
                    v.slots[0].tiles.contains_key(&(0, 0)),
                    "槽位应装入新世代瓦片"
                );
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
                             cx: &mut gpui::VisualTestContext| {
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
        let fresh: gpui::Entity<PdfViewer> =
            cx.update(|_window, app| app.new(|cx| minimal_viewer(cx)));
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
                (cover_scale(dim, pw, 1.0), dim, render_scale(pw, 1.0, dim.0))
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
                vv.slots[0].cover = crate::render_image_from_bgra(vec![0u8; 4 * 8 * 8], 8, 8).ok();
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
            assert!(
                viewport_h > 0.0,
                "滚动容器应在渲染后布局完成：viewport_h={viewport_h}"
            );
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
            holder
                .borrow()
                .as_ref()
                .unwrap()
                .update(cx, |_v, cx| cx.notify());
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
            drawn1, 0,
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

    /// P1-16：`clear_stale_pending` 清掉「世代 < 当前 nav_gen」的在途条目时，必须一并复位
    /// 对应页槽的 `mid_loading`/`thumb_loading` 标志（对齐 `invalidate_page_bitmaps_for_r8_flip`
    /// 的复位姿势）——否则大跳后该页占位链永久失能（结果永不回、标志永不复位，页卡永远停
    /// 「加载中」；对照清世代的 `pending_mid`/`pending_thumb` 只保证预算释放，不修槽标志）。
    #[gpui::test]
    async fn clear_stale_pending_resets_mid_and_thumb_loading_flags(cx: &mut TestAppContext) {
        use gpui::AppContext as _;
        let v: gpui::Entity<PdfViewer> = cx.update(|app| {
            app.new(|cx| {
                let mut v = multi_page_viewer(cx, 4);
                // 模拟大跳后：nav_gen 已推进到 5，页 1/2 持有世代 3（旧）的在途中清/缩略请求
                v.nav_gen = 5;
                let (_tx_mid, rx_mid) = mpsc::channel::<Option<RenderResult>>();
                v.slots[1].mid_loading = true;
                v.pending_mid.insert(1, (3, rx_mid));
                let (_tx_thumb, rx_thumb) = mpsc::channel::<Option<RenderResult>>();
                v.slots[2].thumb_loading = true;
                v.pending_thumb.insert(2, (3, rx_thumb));
                v
            })
        });
        cx.update(|app| {
            v.update(app, |v, _cx| {
                v.clear_stale_pending();
                // 旧世代在途条目按世代被清（既有语义）
                assert!(
                    !v.pending_mid.contains_key(&1) && !v.pending_thumb.contains_key(&2),
                    "旧世代在途条目应按世代被清"
                );
                // P1-16：被清条目对应的槽 loading 标志必须复位（大跳后占位链不永久失能）
                assert!(
                    !v.slots[1].mid_loading && !v.slots[2].thumb_loading,
                    "被清条目对应槽的 loading 标志应复位"
                );
            });
        });
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
                assert_eq!(
                    v.speed_state,
                    SpeedState::Slow,
                    "连续 K 帧小位移应直落 Slow"
                );
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
            (
                cover_scale(v.dims[0], v.page_width(), 1.0),
                s,
                page_bitmap_size(v.dims[0], s),
            )
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
                assert!(
                    v.slots[0].tiles.is_empty(),
                    "该页瓦片不应入槽（封面已供给）"
                );
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

    /// H2 红测（P1 worker 泄漏 / 僵尸泵，无 pdfium 的纯调度/状态断言）：
    /// **快速滚动中关闭标签页（release）后，自刷新泵必须停止**。
    ///
    /// 根因链：`ensure_tile_pump`（tiles.rs）spawn 的循环只按 `pump_still_needed()` 判活，而
    /// `pump_still_needed()` 对「已 release 视图」无防——`release()`（open.rs:551）清掉了
    /// pending/zoom_armed 却**不复位** `prev_fast`/`speed_state`；滚动置 fast 态后
    /// `prev_fast==true` 使 `pump_still_needed()` 恒真 → 泵以 16ms 周期继续 `refresh_windows` +
    /// 强 `this: Entity` 永不 drop（僵尸泵）。
    ///
    /// 断言：已打开视图置于 fast 态后 `pump_still_needed()` 为 true（泵应出帧）；调用
    /// `release()`（shell 关标签页的等价关闭路径）后 `pump_still_needed()` 必须回落 false。
    /// 修复前 release 未复位 fast 态 → 仍 true → 红。
    #[gpui::test]
    async fn release_stops_fast_state_pump(cx: &mut TestAppContext) {
        use std::cell::RefCell;
        use std::rc::Rc;

        // 窗口宿主：把（不触碰 pdfium 的）minimal viewer 渲染进真实布局窗口，令
        // `scroll.bounds()` 拿到真实视口（>0）——否则 `pump_still_needed` 的「已加载且
        // 视口 0」分支恒真，会污染本用例隔离出的 `prev_fast` 因子。
        struct PumpProbe {
            pdf: gpui::Entity<PdfViewer>,
        }
        impl Render for PumpProbe {
            fn render(
                &mut self,
                _window: &mut gpui::Window,
                _cx: &mut gpui::Context<Self>,
            ) -> impl IntoElement {
                gpui::div().size_full().child(self.pdf.clone())
            }
        }

        let holder: Rc<RefCell<Option<gpui::Entity<PdfViewer>>>> = Rc::default();
        let (_root, cx) = cx.add_window_view(|_window, cx| {
            let pdf = cx.new(|cx| minimal_viewer(cx));
            *holder.borrow_mut() = Some(pdf.clone());
            PumpProbe { pdf }
        });
        cx.simulate_resize(gpui::size(gpui::px(1400.), gpui::px(900.)));
        // 让页列（含 track_scroll 容器）完成一次真实布局，scroll.bounds 落到视口尺寸。
        for _ in 0..3 {
            let _ = cx.update(|w, _| w.refresh());
            cx.run_until_parked();
        }
        // 前提 1：视口已布局（>0），加载分支不把 pump_still_needed 永久拉真。
        let vh = cx.update(|_, cx| {
            holder
                .borrow()
                .as_ref()
                .unwrap()
                .read(cx)
                .scroll
                .bounds()
                .size
                .height
                .as_f32()
        });
        assert!(vh > 0.0, "视口应已布局：vh={vh}");

        // 模拟快速滚动：置 fast 态（`update_fast_flag` 的等价终态：speed_state=Fast +
        // prev_fast=true 两者恒同步）。
        cx.update(|_, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, _c| {
                v.speed_state = SpeedState::Fast;
                v.prev_fast = true;
            });
        });
        // 前提 2：release 前泵应仍需运行（快速滚动中持续出帧清尾）。
        let before = cx.update(|_, cx| {
            holder
                .borrow()
                .as_ref()
                .unwrap()
                .read(cx)
                .pump_still_needed()
        });
        assert!(before, "快速滚动中 pump_still_needed 应为 true");

        // 关闭标签页等价路径：release()
        cx.update(|window, cx| {
            holder
                .borrow()
                .as_ref()
                .unwrap()
                .update(cx, |v, c| v.release(window, c));
        });
        // 断言：release 后 pump_still_needed() 必须为 false（泵应停）。
        // 修复前 release 未复位 prev_fast、pump_still_needed 又未防「已 release 视图」→ 仍 true → 红。
        let after = cx.update(|_, cx| {
            holder
                .borrow()
                .as_ref()
                .unwrap()
                .read(cx)
                .pump_still_needed()
        });
        assert!(
            !after,
            "release 后 pump_still_needed 应为 false（僵尸泵：release 未复位 prev_fast 且 \
             pump_still_needed 未防已释放视图）"
        );
    }

    // ---- 网格视图（Grid，wave3）集成测试：真实 PDF 夹具（多页）+ pdfium worker ----

    /// 网格测试的窗口宿主：整窗渲染一个 PdfViewer（同「瓦片管线端到端」测试的 PageProbe 姿势）。
    struct GridHost {
        pdf: Entity<PdfViewer>,
    }
    impl Render for GridHost {
        fn render(
            &mut self,
            _w: &mut gpui::Window,
            _cx: &mut gpui::Context<Self>,
        ) -> impl IntoElement {
            gpui::div().size_full().child(self.pdf.clone())
        }
    }

    /// 网格测试公共开窗 + 等待加载（含 pdfium 串行锁守卫约定由调用方先取）。
    /// 随后进入网格并渲染足够帧使 `grid_last_range`/`grid_last_drawn` 探针落地。
    fn enter_grid(holder: &Rc<RefCell<Option<Entity<PdfViewer>>>>, cx: &mut VisualTestContext) {
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, cx| {
                v.set_grid_mode(true, cx);
            });
        });
        // 网格 mode 切换后需真实渲染帧（render 内布 `grid_last_*` 探针）；refresh 无条件
        // 要求重绘 + run_until_parked 推进虚拟帧（对齐既有静态帧驱动姿势）。
        for _ in 0..5 {
            let _ = cx.refresh();
            cx.run_until_parked();
        }
    }

    /// 进入网格后，按「渲染同口径」（网格滚动容器真实 bounds + 同套常量/纯函数）手算
    /// 期望物化区间与卡数，供断言对表（期望源自真实几何推导，K177）。
    fn expected_grid_band(
        holder: &Rc<RefCell<Option<Entity<PdfViewer>>>>,
        cx: &mut VisualTestContext,
        offset: f32,
    ) -> ((usize, usize), usize, usize) {
        let (vw, vh, dims) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            let b = v.grid_scroll.bounds();
            (
                b.size.width.as_f32(),
                b.size.height.as_f32(),
                v.dims.clone(),
            )
        });
        let layout = grid_layout(&dims, GRID_CARD_WIDTH, GRID_GAP, GRID_PADDING, vw);
        let band = grid_visible_range(&layout, offset, vh).expect("网格首屏应存在可见带");
        let count = band.1 - band.0 + 1;
        (band, count, dims.len())
    }

    /// 向网格卡派发一次**真实双击**（第一击 `click_count=1` + 第二击 `click_count=2`）：
    /// 还原 gpui 双击序列——第一击先执行「单击选中」、第二击才「退出网格 + 跳页」（见
    /// `grid_click_commit` 的点击计数分发；与文件管理器「先选定再打开」一致）。
    /// `simulate_click` 恒为 click_count=1，无法覆盖双击。
    fn simulate_grid_double_click(cx: &mut VisualTestContext, at: gpui::Point<gpui::Pixels>) {
        for count in [1usize, 2] {
            cx.simulate_event(gpui::MouseDownEvent {
                button: gpui::MouseButton::Left,
                position: at,
                modifiers: gpui::Modifiers::default(),
                click_count: count,
                first_mouse: false,
            });
            cx.simulate_event(gpui::MouseUpEvent {
                button: gpui::MouseButton::Left,
                position: at,
                modifiers: gpui::Modifiers::default(),
                click_count: count,
            });
        }
    }

    /// Ctrl+滚轮缩放事件构造（同 lib.rs `zoom_wheel`，供网格交互分支测试派发真实事件）：
    /// vertical pixel delta，向上 >0 放大、向下 <0 缩小；`control` 控制修饰键。gpui 测试
    /// 平台经 `simulate_event` 可派发带修饰键的**真实** `ScrollWheelEvent`（同单列
    /// `pdf_ctrl_wheel_zooms_in_out` 与 markdown_wysiwyg 姿势）。
    fn grid_zoom_wheel(control: bool, delta_y: f32) -> gpui::ScrollWheelEvent {
        gpui::ScrollWheelEvent {
            position: gpui::point(gpui::px(400.), gpui::px(450.)),
            delta: gpui::ScrollDelta::Pixels(gpui::point(gpui::px(0.), gpui::px(delta_y))),
            modifiers: gpui::Modifiers {
                control,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    // ---- 阅读页排模式（page_mode）状态层：只存状态 + 通知，渲染分派/锚点恢复未接 -----
    // （状态断言复用既有 `minimal_viewer`：不触碰 pdfium 的 1 页最小 PdfViewer）

    /// 默认 Continuous；`set_page_mode` 双态可换；重设同值幂等（不抖动）。
    #[gpui::test]
    async fn page_mode_defaults_continuous_and_setter_changes(cx: &mut TestAppContext) {
        let viewer = cx.update(|app| app.new(|cx| minimal_viewer(cx)));
        let mode = cx.update(|app| viewer.read(app).page_mode());
        assert_eq!(mode, PageMode::Continuous, "默认页排模式应为连续滚动");
        cx.update(|app| {
            viewer.update(app, |v, cx| v.set_page_mode(PageMode::Facing, cx));
        });
        let mode = cx.update(|app| viewer.read(app).page_mode());
        assert_eq!(mode, PageMode::Facing, "set_page_mode(Facing) 后应为双页对开");
        cx.update(|app| {
            viewer.update(app, |v, cx| v.set_page_mode(PageMode::Facing, cx));
        });
        let mode = cx.update(|app| viewer.read(app).page_mode());
        assert_eq!(
            mode,
            PageMode::Facing,
            "重复 set_page_mode(Facing) 幂等不变"
        );
        cx.update(|app| {
            viewer.update(app, |v, cx| v.set_page_mode(PageMode::Continuous, cx));
        });
        let mode = cx.update(|app| viewer.read(app).page_mode());
        assert_eq!(
            mode,
            PageMode::Continuous,
            "set_page_mode(Continuous) 后应回连续"
        );
    }

    /// page_mode 与网格总览正交：进入网格（grid_mode=true）与退出网格（grid_mode=false）
    /// 都保留 page_mode——读写网格不清阅读页排模式。
    #[gpui::test]
    async fn page_mode_survives_grid_toggle(cx: &mut TestAppContext) {
        let viewer = cx.update(|app| app.new(|cx| minimal_viewer(cx)));
        cx.update(|app| {
            viewer.update(app, |v, cx| v.set_page_mode(PageMode::Facing, cx));
        });
        cx.update(|app| {
            viewer.update(app, |v, cx| v.set_grid_mode(true, cx));
        });
        let (gm, pm) = cx.update(|app| {
            let v = viewer.read(app);
            (v.grid_mode(), v.page_mode())
        });
        assert!(gm, "进入网格后 grid_mode 应为 true");
        assert_eq!(pm, PageMode::Facing, "进入网格不应清 page_mode");
        cx.update(|app| {
            viewer.update(app, |v, cx| v.set_grid_mode(false, cx));
        });
        let (gm, pm) = cx.update(|app| {
            let v = viewer.read(app);
            (v.grid_mode(), v.page_mode())
        });
        assert!(!gm, "退出网格后 grid_mode 应为 false");
        assert_eq!(pm, PageMode::Facing, "退出网格仍保留 page_mode");
    }

    /// 网格总览中直接 set_page_mode → 先退出网格再落新页排模式（网格只是一时总览）。
    #[gpui::test]
    async fn set_page_mode_from_grid_exits_grid(cx: &mut TestAppContext) {
        let viewer = cx.update(|app| app.new(|cx| minimal_viewer(cx)));
        cx.update(|app| {
            viewer.update(app, |v, cx| v.set_grid_mode(true, cx));
        });
        cx.update(|app| {
            viewer.update(app, |v, cx| v.set_page_mode(PageMode::Facing, cx));
        });
        let (gm, pm) = cx.update(|app| {
            let v = viewer.read(app);
            (v.grid_mode(), v.page_mode())
        });
        assert!(!gm, "网格中 set_page_mode 应先退出网格");
        assert_eq!(pm, PageMode::Facing, "退出网格后 page_mode 应落为新模式");
    }

    // ---- 分页页排（page_mode = Facing，非网格）渲染集成测试 ----

    /// 分页测试夹具：构造不触碰 pdfium 的**已加载**多页 PdfViewer（worker=None），
    /// dims/slots 按传入页数铺齐（`minimal_viewer` 同姿态，仅补多页槽）。
    fn paged_viewer(cx: &mut gpui::Context<PdfViewer>, dims: Vec<(f32, f32)>) -> PdfViewer {
        let (tops, total) = compute_layout(&dims, PAGE_WIDTH);
        let mut v = minimal_viewer(cx);
        v.dims = dims.clone();
        // contain-fit 缓存字段同步设（测试建 viewer 不走 apply_loaded，漏设会 stale）。
        v.doc_min_ar = min_ar(&dims);
        v.page_tops = tops;
        v.total_height = total;
        v.slots = (0..dims.len()).map(|_| Slot::default()).collect();
        v
    }

    /// 分页测试的窗口宿主：整窗渲染一个 PdfViewer（无 pdfium，打开即已加载）。
    struct PagedHost {
        pdf: Entity<PdfViewer>,
    }
    impl Render for PagedHost {
        fn render(
            &mut self,
            _w: &mut gpui::Window,
            _cx: &mut gpui::Context<Self>,
        ) -> impl IntoElement {
            gpui::div().size_full().child(self.pdf.clone())
        }
    }

    /// 分页测试公共开窗（页数参数化）：`dims` 传入页尺寸集，整窗 `PagedHost` 宿主 +
    /// 真实容器 ≈1000×700（K185：注入真实宿主结构，scroll.bounds() 为真实视口）。
    fn open_paged_host_pages<'a>(
        cx: &'a mut TestAppContext,
        dims: Vec<(f32, f32)>,
    ) -> (
        Rc<RefCell<Option<Entity<PdfViewer>>>>,
        &'a mut VisualTestContext,
    ) {
        let holder: Rc<RefCell<Option<Entity<PdfViewer>>>> = Rc::default();
        let (_root, vcx) = cx.add_window_view(|_window, cx| {
            let pdf = cx.new(|cx| paged_viewer(cx, dims));
            *holder.borrow_mut() = Some(pdf.clone());
            PagedHost { pdf }
        });
        vcx.simulate_resize(gpui::size(gpui::px(1000.), gpui::px(700.)));
        for _ in 0..6 {
            let _ = vcx.refresh();
            vcx.run_until_parked();
        }
        (holder, vcx)
    }

    /// 分页测试公共开窗：3 页 A4（793×1123 pt，与 layout.rs paged 手算同口径）。
    fn open_paged_host<'a>(
        cx: &'a mut TestAppContext,
    ) -> (
        Rc<RefCell<Option<Entity<PdfViewer>>>>,
        &'a mut VisualTestContext,
    ) {
        open_paged_host_pages(cx, vec![(793.0, 1123.0); 3])
    }

    /// 分页期望（K177）：真实阅读视口 + 同套常量/纯函数手算显示矩形。
    /// 口径统一到 [`expected_display_rect`]（水平按视口重算居中，任意 zoom 正确）；
    /// zoom=1 时与旧 `cell_rect(i) × zoom` 代数恒等，既有 zoom=1 断言值不变。
    fn expected_paged_cell(
        holder: &Rc<RefCell<Option<Entity<PdfViewer>>>>,
        cx: &mut VisualTestContext,
        cols: usize,
        page: usize,
    ) -> Option<(f32, f32, f32, f32)> {
        expected_display_rect(holder, cx, cols, page)
    }

    /// 双页模式（Facing）：首行并排 2 页（y 相同、x 相邻 = 卡 0 左 + 卡 0 宽 + gap）；
    /// 第二行只 1 卡（或视口未及则不物化，`.is_some() == 行 1 顶 < 视口底`）。
    #[gpui::test]
    async fn paged_facing_renders_two_per_row(cx: &mut TestAppContext) {
        let (holder, cx) = open_paged_host(cx);
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, cx| {
                v.set_page_mode(PageMode::Facing, cx);
            });
        });
        for _ in 0..6 {
            let _ = cx.refresh();
            cx.run_until_parked();
        }
        let r0 = expected_paged_cell(&holder, cx, 2, 0).expect("页 0");
        let r1 = expected_paged_cell(&holder, cx, 2, 1).expect("页 1");
        let sc = cx.debug_bounds("pdf-scroll").expect("滚动容器应在场");
        let b0 = cx.debug_bounds("pdf-page-0").expect("首行卡 0 应在场");
        let b1 = cx.debug_bounds("pdf-page-1").expect("首行卡 1 应在场");
        let (x0, y0, w0) = (
            b0.left().as_f32() - sc.left().as_f32(),
            b0.top().as_f32() - sc.top().as_f32(),
            b0.size.width.as_f32(),
        );
        let (x1, y1) = (
            b1.left().as_f32() - sc.left().as_f32(),
            b1.top().as_f32() - sc.top().as_f32(),
        );
        assert!(
            (x0 - r0.0).abs() < 0.5 && (y0 - r0.1).abs() < 0.5,
            "卡 0 应按 cell_rect(0)"
        );
        assert!(
            (x1 - r1.0).abs() < 0.5 && (y1 - r1.1).abs() < 0.5,
            "卡 1 应按 cell_rect(1)"
        );
        assert!((y1 - y0).abs() < 0.5, "首行两卡 y 应相同（同行并排）");
        // 卡 1 左 = 卡 0 左 + 卡 0 宽 + gap（PAGED_GAP_X=1 × zoom=1）。
        assert!(
            (x1 - (x0 + w0) - PAGED_GAP_X).abs() < 0.5,
            "首行两卡相邻：卡1.x − (卡0.x+卡0.w) 应≈PAGED_GAP_X({PAGED_GAP_X})，实际 {}",
            x1 - (x0 + w0)
        );
        // 第二行只 1 卡：页 2 是否物化由**渲染窗**（全可见行 ±1 行缓冲）决定，不再按
        // 「行 1 顶 < 视口底」的旧 r0-only 口径。期望值从 `paged_render_window` 独立重算
        // （K177）——3 页文档共 2 行、顶端可见行 (0,0) + 缓冲到行 1 → 窗含页 2。
        let render_includes_p2 = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            let zoom = v.zoom.max(0.01);
            let offset = f32::from(-v.scroll.offset().y).max(0.0);
            let vh = f32::from(v.scroll.bounds().size.height);
            let l = v.paged_layout_cell.borrow().clone().expect("分页快照");
            paged_render_window(&l, -(offset / zoom), vh / zoom, v.dims.len(), 1)
                .is_some_and(|(s, e)| s <= 2 && 2 <= e)
        });
        let row1_visible = cx.debug_bounds("pdf-page-2").is_some();
        assert_eq!(
            row1_visible, render_includes_p2,
            "页 2 物化应与渲染窗含页 2 一致"
        );
        if row1_visible {
            let r2 = expected_paged_cell(&holder, cx, 2, 2).expect("页 2");
            let b2 = cx.debug_bounds("pdf-page-2").expect("页 2 应在场");
            assert!(
                (b2.left().as_f32() - sc.left().as_f32() - r2.0).abs() < 0.5
                    && (b2.top().as_f32() - sc.top().as_f32() - r2.1).abs() < 0.5,
                "第二行单页应在 cell_rect(2)"
            );
        }
    }

    /// 连续模式回归：page_mode=Continuous → 走既有 `render_pdf_pages`（每页左 0、宽
    /// page_width、top = compute_layout）不回归，卡 debug_bounds 与连续布局一致。
    #[gpui::test]
    async fn paged_mode_off_uses_continuous_layout(cx: &mut TestAppContext) {
        let (holder, cx) = open_paged_host(cx);
        // page_mode 默认 Continuous：保持连续渲染（不改任何模式）。
        for _ in 0..3 {
            let _ = cx.refresh();
            cx.run_until_parked();
        }
        let (page_width, tops, disp_h0, vw) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            let pw = v.page_width();
            (
                pw,
                v.page_tops.clone(),
                display_height((793.0, 1123.0), pw),
                f32::from(v.scroll.bounds().size.width),
            )
        });
        let sc = cx.debug_bounds("pdf-scroll").expect("滚动容器应在场");
        let b0 = cx.debug_bounds("pdf-page-0").expect("连续首卡应在场");
        // 连续页列：卡 0 左 = 居中偏移（容器宽 − 页宽)/2、顶 = PAGE_PAD_Y、宽 = page_width、
        // 高 = display_height（page_tops[0] 即 compute_layout 首行顶 = PAGE_PAD_Y）。
        let cx0 = (vw - page_width) / 2.0;
        let left0 = b0.left().as_f32() - sc.left().as_f32();
        let top0 = b0.top().as_f32() - sc.top().as_f32();
        assert!(
            (left0 - cx0).abs() < 1.0,
            "连续卡 0 应居中（左≈{cx0}），实际 {left0:.2}"
        );
        assert!(
            (top0 - tops[0]).abs() < 0.5,
            "连续卡 0 顶应≈PAGE_PAD_Y={}",
            tops[0]
        );
        assert!(
            (b0.size.width.as_f32() - page_width).abs() < 0.5,
            "连续卡 0 宽应=page_width"
        );
        assert!(
            (b0.size.height.as_f32() - disp_h0).abs() < 0.5,
            "连续卡 0 高应=display_height"
        );
        // 第 2 张连续卡：顶 = tops[1]（前缀页高 + PAGE_GAP），还在 DOM（MARGIN 缓冲）。
        let b1 = cx.debug_bounds("pdf-page-1").expect("连续第 2 卡应在场");
        let top1 = b1.top().as_f32() - sc.top().as_f32();
        assert!(
            (top1 - tops[1]).abs() < 0.5,
            "连续卡 1 顶应≈tops[1]={}",
            tops[1]
        );
    }

    // ---- 分页页拍交互片（R3-R6）：连续自由滚动 / scroll_to_page / 命中 / 几何 ----

    /// 当前分页滚动位：内容顶 = `−scroll.offset().y`（显示 px，≥0）。
    fn paged_content_top(
        holder: &Rc<RefCell<Option<Entity<PdfViewer>>>>,
        cx: &mut VisualTestContext,
    ) -> f32 {
        cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            f32::from(-v.scroll.offset().y)
        })
    }

    /// 分页窗口坐标：内容 `(cx, cy)`（显示 px）→ 窗口坐标（滚动容器原点 + 内容位 + 偏移 0）。
    fn paged_window_pos(
        holder: &Rc<RefCell<Option<Entity<PdfViewer>>>>,
        cx: &mut VisualTestContext,
        cx_: f32,
        cy: f32,
    ) -> gpui::Point<gpui::Pixels> {
        let (bl, bt) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            let b = v.scroll.bounds();
            (b.left().as_f32(), b.top().as_f32())
        });
        gpui::point(px(bl + cx_), px(bt + cy))
    }

    /// R4': 双页模式普通滚轮 = **连续自由滚动**（不再是逐行步进翻页）：非 Ctrl 滚轮不拦截、
    /// 交给滚动容器平滑滚动，可停在行中间看到上下两行。断言用**不可能触发的整行跳**不变式
    /// ——`delta=-120` 后内容顶必须远小于行 1 顶（旧步进会直接落到 `row_top[1]`，必破）。
    /// headless 下滚轮**确实推动 scroll**（gpui 源实证：`dispatch_event` 设 `mouse_position`
    /// → `hit_test` 命中滚动容器 → 容器 handler 经共享 offset `RefCell` 做
    /// `scroll_offset.y += delta_y`，`delta=-120` → 内容顶 +≈120px）；本断言锁「不跳整行」，
    /// 对回归步进是真保护（旧步进落 703.25 会破 <367.625 上界）。
    #[gpui::test]
    async fn paged_facing_wheel_free_scroll_no_page_jump(cx: &mut TestAppContext) {
        let (holder, cx) = open_paged_host(cx);
        cx.update(|_w, cx| {
            holder
                .borrow()
                .as_ref()
                .unwrap()
                .update(cx, |v, cx| v.set_page_mode(PageMode::Facing, cx));
        });
        for _ in 0..6 {
            let _ = cx.refresh();
            cx.run_until_parked();
        }
        let row0_top = expected_paged_cell(&holder, cx, 2, 0).expect("页 0").1;
        let row1_top = expected_paged_cell(&holder, cx, 2, 2)
            .expect("页 2 应在行 1")
            .1;
        let row_h = paged_fit_layout(&holder, cx).row_height(0);
        assert!(
            (row1_top - row0_top).abs() > 200.0,
            "前置：双页行 1 顶应在行 0 顶之下"
        );
        // 演算（K177）：行 1 顶 705.04 − 半行 339.52 = 365.52。从未有一档 wheel（120px）能
        // 滚过半行以上 → 内容顶恒 < 365.52 即证明「未跳整行」（旧步进到 705.04 必破）。
        let jump_bound = row1_top - row_h / 2.0;

        // 向下滚：自由滚动（或 headless 不移动 = t0）都远在半行界内，绝无整行步进。
        cx.simulate_event(grid_zoom_wheel(false, -120.0));
        let t_down = paged_content_top(&holder, cx);
        assert!(
            t_down < jump_bound,
            "双页向下滚 = 自由滚动，未跳整行：content_top={t_down:.1} 应 < {jump_bound:.1}（旧步进到 {row1_top:.1}）"
        );
        // 上滚同理：只回滚小位移 / 停顶，不整行跳回、也不把内容推更向下。
        cx.simulate_event(grid_zoom_wheel(false, 120.0));
        let t_up = paged_content_top(&holder, cx);
        assert!(
            t_up < jump_bound,
            "双页向上滚 = 自由滚动，未跳整行：content_top={t_up:.1} 应 < {jump_bound:.1}"
        );
        assert!(
            t_up <= t_down + 1.0,
            "上滚不得把内容再向下推：t_up={t_up:.1} ≤ t_down+1={:.1}",
            t_down + 1.0
        );
    }

    /// R5': 双页模式停在行中 = **自由保持**（吸附机制已删）：滚动位设到行 0 中部
    /// （row0_top + 行高/2）→ 泵帧停稳后内容顶原样保持，不吸顶/不吸底/不吸附。
    /// Facing 几何（K177）：cols=2、3 页 A4 → 2 行；row_top=[20,705.04]、行高 679.04 →
    /// 行中 = 20 + 339.52 = 359.52（该位在分页量程 704.08 内，无任何既有 clamp 会改写）。
    #[gpui::test]
    async fn paged_facing_mid_row_persists_no_snap(cx: &mut TestAppContext) {
        let (holder, cx) = open_paged_host(cx);
        cx.update(|_w, cx| {
            holder
                .borrow()
                .as_ref()
                .unwrap()
                .update(cx, |v, cx| v.set_page_mode(PageMode::Facing, cx));
        });
        for _ in 0..6 {
            let _ = cx.refresh();
            cx.run_until_parked();
        }
        let row0_top = expected_paged_cell(&holder, cx, 2, 0).expect("页 0").1;
        let l = paged_fit_layout(&holder, cx);
        // 行中位（K177）：row0_top=20 + 行高 679.04/2 = 359.52。
        let mid = row0_top + l.row_height(0) / 2.0;
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, _| {
                v.scroll.set_offset(gpui::point(px(0.), px(-mid)));
            });
        });
        // 泵帧停稳（吸附已删：无任何机制改写滚动位，位置应原样保持）。
        for _ in 0..12 {
            let _ = cx.refresh();
            cx.run_until_parked();
        }
        let t = paged_content_top(&holder, cx);
        assert!(
            (t - mid).abs() < 0.5,
            "行中停位应保持（不吸顶/不吸底/不吸附）：content_top={t:.1} 应 ≈ {mid:.1}"
        );
    }

    /// R6：`scroll_to_page(2)`（Facing）→ `r = 2/2 = 1` → 偏移 = `−row_top[1]×zoom`（行 1 顶
    /// 对齐视口顶 = 页 2 所在行）；期望 = `cell_rect(2).y`。
    #[gpui::test]
    async fn paged_scroll_to_page_jumps_to_row(cx: &mut TestAppContext) {
        let (holder, cx) = open_paged_host(cx);
        cx.update(|_w, cx| {
            holder
                .borrow()
                .as_ref()
                .unwrap()
                .update(cx, |v, cx| v.set_page_mode(PageMode::Facing, cx));
        });
        for _ in 0..6 {
            let _ = cx.refresh();
            cx.run_until_parked();
        }
        let want_unclamped = expected_paged_cell(&holder, cx, 2, 2).expect("页 2").1;
        // 末行（行 1）顶 + 视口 700 超出文档底 → scroll_to_page 按分页量程夹（末行底对底）。
        let max_scroll = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            let b = v.scroll.bounds();
            let l = paged_layout(
                &v.dims,
                b.size.width.as_f32(),
                b.size.height.as_f32(),
                2,
                PAGED_GAP_X,
                PAGED_GAP_Y,
                PAGE_PAD_X,
            );
            (l.total_height - b.size.height.as_f32()).max(0.0)
        });
        let want = want_unclamped.min(max_scroll);
        cx.update(|_w, cx| {
            holder
                .borrow()
                .as_ref()
                .unwrap()
                .update(cx, |v, _| v.scroll_to_page(2));
        });
        let t = paged_content_top(&holder, cx);
        assert!(
            (t - want).abs() < 0.5,
            "scroll_to_page(2)（Facing）应把行 1 顶对齐视口顶，got {t:.1} want {want:.1}"
        );
    }

    /// R3：Facing 命中——点左页（页 0）中心 → 页 0；点右页（页 1）中心 → 页 1；点行间沟槽
    /// （纵向 gap 中点）→ None（无命中/无选区）。`window_to_pt` 直接取命中的页号（probe 取页，
    /// 无需文本层）。
    #[gpui::test]
    async fn paged_hit_clicks_facing_right_page(cx: &mut TestAppContext) {
        let (holder, cx) = open_paged_host(cx);
        cx.update(|_w, cx| {
            holder
                .borrow()
                .as_ref()
                .unwrap()
                .update(cx, |v, cx| v.set_page_mode(PageMode::Facing, cx));
        });
        for _ in 0..6 {
            let _ = cx.refresh();
            cx.run_until_parked();
        }
        let r0 = expected_paged_cell(&holder, cx, 2, 0).expect("页 0（左页）");
        let r1 = expected_paged_cell(&holder, cx, 2, 1).expect("页 1（右页）");
        let c0 = paged_window_pos(&holder, cx, r0.0 + r0.2 / 2.0, r0.1 + r0.3 / 2.0);
        let c1 = paged_window_pos(&holder, cx, r1.0 + r1.2 / 2.0, r1.1 + r1.3 / 2.0);
        // 行间沟槽中点（纵向 gap）：列 0 中心 x，y = 行 0 底 + v_gap/2。
        let gap_y = r0.1 + r0.3 + PAGED_GAP_Y / 2.0;
        let cgap = paged_window_pos(&holder, cx, r0.0 + r0.2 / 2.0, gap_y);

        let hit0 = cx.update(|w, cx| {
            holder
                .borrow()
                .as_ref()
                .unwrap()
                .read(cx)
                .window_to_pt(w, c0)
        });
        let hit1 = cx.update(|w, cx| {
            holder
                .borrow()
                .as_ref()
                .unwrap()
                .read(cx)
                .window_to_pt(w, c1)
        });
        let hitgap = cx.update(|w, cx| {
            holder
                .borrow()
                .as_ref()
                .unwrap()
                .read(cx)
                .window_to_pt(w, cgap)
        });
        assert_eq!(hit0.map(|(p, _, _)| p), Some(0), "点左页中心应命中页 0");
        assert_eq!(hit1.map(|(p, _, _)| p), Some(1), "点右页中心应命中页 1");
        assert!(hitgap.is_none(), "点行间沟槽应 None（无命中/无选区）");
    }

    // ---- R7：分页精确锚 / 阅读进度往返 / WindowSig 分页维度（S6 收口片） ----

    /// 取分页 viewer 当前 zoom=1 布局快照（zoom=1 口径，与 render 同源）。
    fn paged_fit_layout(
        holder: &Rc<RefCell<Option<Entity<PdfViewer>>>>,
        cx: &mut VisualTestContext,
    ) -> PagedLayout {
        cx.update(|_w, cx| {
            holder
                .borrow()
                .as_ref()
                .unwrap()
                .read(cx)
                .paged_layout_cell
                .borrow()
                .clone()
                .expect("分页快照应存在")
        })
    }

    /// R5：双页模式滚到行 1 → zoom 2 档 → 首可见行仍 r0==1（不因缩放跳行）。
    /// 期望（K177 演算）：5 页 A4 Facing 布局 rows=3（行高 679.04 + v_gap 6）→
    /// row_top=[20,705.04,1390.08]、row_top[1]=705.04 非末行（末行顶 1390.08，量程
    /// 1389.16 内不被夹）；滚到行 1 顶 → first_visible_row 首行 = 1；zoom→2.0 后精确锚
    /// `zoom_rescroll_anchor_paged` 保持行内相对位置（行顶 0%）→ 新内容顶 =
    /// row_top[1]×2 = 1410.08 → 首可见行仍 1（行号不随 zoom 变）。
    #[gpui::test]
    async fn paged_zoom_keeps_anchored_row_on_screen(cx: &mut TestAppContext) {
        let (holder, cx) = open_paged_host_pages(cx, vec![(793.0, 1123.0); 5]);
        cx.update(|_w, cx| {
            holder
                .borrow()
                .as_ref()
                .unwrap()
                .update(cx, |v, cx| v.set_page_mode(PageMode::Facing, cx));
        });
        for _ in 0..6 {
            let _ = cx.refresh();
            cx.run_until_parked();
        }
        let l1 = paged_fit_layout(&holder, cx);
        assert_eq!(l1.rows, 3, "Facing 5 页 → 3 行");
        // 滚到行 1 顶（内容顶 = row_top[1]，非末行不超量程）。
        let row1_top = l1.row_top[1];
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, _cx| {
                v.scroll.set_offset(gpui::point(px(0.), px(-row1_top)));
            });
        });
        for _ in 0..3 {
            let _ = cx.refresh();
            cx.run_until_parked();
        }
        // 缩放前首可见行 = 1。
        let (r0_before, _) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            let l = v.paged_layout_cell.borrow();
            let l = l.as_ref().unwrap();
            let zoom = v.zoom.max(0.01);
            let top = f32::from(-v.scroll.offset().y).max(0.0);
            let vh = f32::from(v.scroll.bounds().size.height) / zoom;
            l.first_visible_row(-(top / zoom), vh).unwrap()
        });
        assert_eq!(r0_before, 1, "缩放前首可见行应为 1");
        // zoom → 2.0（精确锚重锚）。
        cx.update(|_w, cx| {
            holder
                .borrow()
                .as_ref()
                .unwrap()
                .update(cx, |v, cx| v.set_zoom_abs(2.0, cx));
        });
        for _ in 0..6 {
            let _ = cx.refresh();
            cx.run_until_parked();
        }
        // 缩放后首可见行仍 = 1。
        let (r0_after, content_top_after) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            let l = v.paged_layout_cell.borrow();
            let l = l.as_ref().unwrap();
            let zoom = v.zoom.max(0.01);
            let top = f32::from(-v.scroll.offset().y).max(0.0);
            let vh = f32::from(v.scroll.bounds().size.height) / zoom;
            (l.first_visible_row(-(top / zoom), vh).unwrap().0, top)
        });
        assert_eq!(
            r0_after, 1,
            "缩放后首可见行仍应为 1（精确锚不跳行），实际 {r0_after}"
        );
        // 内容顶应 ≈ row_top[1]×2 = 1464（行内 0% 锚）。
        let l2 = paged_fit_layout(&holder, cx);
        let want_top = l2.row_top[1] * 2.0;
        assert!(
            (content_top_after - want_top).abs() < 2.0,
            "缩放后内容顶应≈{want_top}（行 1 顶×2），实际 {content_top_after}"
        );
    }

    /// R2/R3：分页阅读进度往返（Facing）——滚到行 0 中部 (page=0, frac=0.5) →
    /// reading_progress 读 → set_restore_anchor + restore_pending 应用 → 重新
    /// reading_progress ≈ 原值（往返误差小）。
    #[gpui::test]
    async fn paged_reading_progress_restore_roundtrip(cx: &mut TestAppContext) {
        const COLS: usize = 2;
        let (holder, cx) = open_paged_host(cx);
        cx.update(|_w, cx| {
            holder
                .borrow()
                .as_ref()
                .unwrap()
                .update(cx, |v, cx| v.set_page_mode(PageMode::Facing, cx));
        });
        for _ in 0..6 {
            let _ = cx.refresh();
            cx.run_until_parked();
        }
        let l = paged_fit_layout(&holder, cx);
        // 滚到行 0 中部（frac=0.5）：行 0 满两页、非末行（行 1 是末行，中部会超
        // paged_max_scroll → 夹失真），故用行 0。
        let test_row = 0usize;
        let frac_target = 0.5f32;
        let sy = l.row_top[test_row] + l.row_height(test_row) * frac_target;
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, _cx| {
                v.scroll.set_offset(gpui::point(px(0.), px(-sy)));
            });
        });
        for _ in 0..3 {
            let _ = cx.refresh();
            cx.run_until_parked();
        }
        // 读阅读进度（分页分支）。
        let (page, frac_read, zoom) = cx.update(|_w, cx| {
            holder
                .borrow()
                .as_ref()
                .unwrap()
                .read(cx)
                .reading_progress()
                .expect("应读到进度")
        });
        // 演算（K177）：行 0 首页 = row_page_start[0] = 0 × cols = 0；frac ≈ 0.5。
        assert_eq!(page, test_row * COLS, "行 0 首页应 = 0, 实际 {page}");
        assert!(
            (frac_read - frac_target).abs() < 0.01,
            "frac 应≈0.5，实际 {frac_read}"
        );
        // 恢复：set_restore_anchor + 手动塞 restore_pending（测试夹具不走 apply_loaded）。
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, _cx| {
                v.set_restore_anchor(page, frac_read, zoom);
                v.restore_pending = Some((page, frac_read));
                v.zoom = zoom;
            });
        });
        for _ in 0..6 {
            let _ = cx.refresh();
            cx.run_until_parked();
        }
        // 重新读 → 应 ≈ 原值（往返误差小）。
        let (page2, frac2, _) = cx.update(|_w, cx| {
            holder
                .borrow()
                .as_ref()
                .unwrap()
                .read(cx)
                .reading_progress()
                .expect("恢复后应读到进度")
        });
        assert_eq!(page2, page, "恢复后 page 应不变：{page} vs {page2}");
        assert!(
            (frac2 - frac_read).abs() < 0.01,
            "恢复后 frac 应≈原值：{frac_read} vs {frac2}"
        );
    }

    /// R6：分页缩放使 cell_w 变 → WindowSig 变化（静态帧门不早退）。
    #[gpui::test]
    async fn paged_window_sig_changes_on_cell_width(cx: &mut TestAppContext) {
        let (holder, cx) = open_paged_host(cx);
        cx.update(|_w, cx| {
            holder
                .borrow()
                .as_ref()
                .unwrap()
                .update(cx, |v, cx| v.set_page_mode(PageMode::Facing, cx));
        });
        for _ in 0..6 {
            let _ = cx.refresh();
            cx.run_until_parked();
        }
        let sig_before = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            let pw = v.page_width();
            let vh = f32::from(v.scroll.bounds().size.height);
            WindowSig::of(v, f32::from(-v.scroll.offset().y), pw, 1.0, vh)
        });
        // 缩放 → cell_w 变 → sig 变。
        cx.update(|_w, cx| {
            holder
                .borrow()
                .as_ref()
                .unwrap()
                .update(cx, |v, cx| v.set_zoom_abs(2.0, cx));
        });
        for _ in 0..3 {
            let _ = cx.refresh();
            cx.run_until_parked();
        }
        let sig_after = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            let pw = v.page_width();
            let vh = f32::from(v.scroll.bounds().size.height);
            WindowSig::of(v, f32::from(-v.scroll.offset().y), pw, 1.0, vh)
        });
        assert_ne!(
            sig_before, sig_after,
            "缩放使 cell_w 变 → WindowSig 必变（静态帧门不早退）"
        );
        // 额外：paged_cell_w 维非 0（分页激活）。
        assert!(
            sig_after.paged_cell_w != 0,
            "分页激活时 paged_cell_w 应非 0"
        );
    }

    // ---- 显示矩形单一真源（R1 paged_display_rect）+ 缩小居 / 不露邻页（bug1/bug2） ----

    /// 分页显示矩形期望（K177 手算，**独立重算**布局 → 按视口水平居中、垂直 ×zoom）：
    /// 与生产 `paged_display_rect` 同公式，但从 `paged_layout(dims, viewport)` 独立推导，
    /// 用于封口「渲染卡/命中确用居中几何」（zoom≠1 时旧的 `cell_rect×zoom` 已不成立）。
    fn expected_display_rect(
        holder: &Rc<RefCell<Option<Entity<PdfViewer>>>>,
        cx: &mut VisualTestContext,
        cols: usize,
        page: usize,
    ) -> Option<(f32, f32, f32, f32)> {
        let (vw, vh, dims, zoom) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            let b = v.scroll.bounds();
            (
                f32::from(b.size.width),
                f32::from(b.size.height),
                v.dims.clone(),
                v.zoom.max(0.01),
            )
        });
        let l = paged_layout(&dims, vw, vh, cols, PAGED_GAP_X, PAGED_GAP_Y, PAGE_PAD_X);
        let r = l.cell_rect(page)?;
        let cw = r.w * zoom;
        let ch = r.h * zoom;
        let cols_f = cols as f32;
        let gap = PAGED_GAP_X * zoom;
        let spread = cols_f * cw + (cols_f - 1.0) * gap;
        let left_pad = ((vw - spread) / 2.0).max(0.0);
        let c = (page % cols) as f32;
        Some((left_pad + c * (cw + gap), r.y * zoom, cw, ch))
    }

    /// 卡 `page` 的**内容坐标**矩形 `(x, y, w, h)`（= 窗口 debug_bounds 减滚动容器原点再减
    /// 滚动偏移），与 `expected_display_rect`（内容坐标）同口径可直接对表。
    fn paged_card_content_rect(
        holder: &Rc<RefCell<Option<Entity<PdfViewer>>>>,
        cx: &mut VisualTestContext,
        page: usize,
    ) -> Option<(f32, f32, f32, f32)> {
        let id: &'static str = match page {
            0 => "pdf-page-0",
            1 => "pdf-page-1",
            2 => "pdf-page-2",
            _ => return None,
        };
        let b = cx.debug_bounds(id)?;
        let (sl, st, offx, offy) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            let sb = v.scroll.bounds();
            (
                sb.left().as_f32(),
                sb.top().as_f32(),
                f32::from(v.scroll.offset().x),
                f32::from(v.scroll.offset().y),
            )
        });
        Some((
            b.left().as_f32() - sl - offx,
            b.top().as_f32() - st - offy,
            b.size.width.as_f32(),
            b.size.height.as_f32(),
        ))
    }

    /// 双页模式（Facing）= 连续自由滚动：zoom 0.5 视口纵向同压行 0/行 1 → **页 0/1/2 全部
    /// 物化**且位置 = 期望矩形（K177：矩形 = cell_rect×zoom 经 [`expected_display_rect`] 独立
    /// 重算）。旧行为只渲首行两页、不露页 2（「绝不露下一行」），本测试锁新语义的反面。
    /// 演算（K177）：zoom0.5 时 vh_fit = 700/0.5 = 1400 > 行 1 顶距（705.04−20=685.04）→
    /// 行 1 真可见 → 渲染窗 = 全可见行 (0,1) ± 缓冲 → 页 2 物化。
    #[gpui::test]
    async fn paged_facing_free_scroll_materializes_both_rows(cx: &mut TestAppContext) {
        let (holder, cx) = open_paged_host(cx);
        cx.update(|_w, cx| {
            holder
                .borrow()
                .as_ref()
                .unwrap()
                .update(cx, |v, cx| v.set_page_mode(PageMode::Facing, cx));
        });
        for _ in 0..6 {
            let _ = cx.refresh();
            cx.run_until_parked();
        }
        cx.update(|_w, cx| {
            holder
                .borrow()
                .as_ref()
                .unwrap()
                .update(cx, |v, cx| v.set_zoom_abs(0.5, cx));
        });
        for _ in 0..6 {
            let _ = cx.refresh();
            cx.run_until_parked();
        }
        // 三页全部物化（行 0 双页 + 行 1 单页），且各卡内容矩形 = 期望矩形。
        for page in 0usize..3 {
            let got = paged_card_content_rect(&holder, cx, page)
                .expect("双页 zoom0.5 连续自由滚动应物化页 {page}");
            let exp = expected_display_rect(&holder, cx, 2, page).expect("期望矩形");
            assert!(
                (got.0 - exp.0).abs() < 0.6
                    && (got.1 - exp.1).abs() < 0.6
                    && (got.2 - exp.2).abs() < 0.6
                    && (got.3 - exp.3).abs() < 0.6,
                "页 {page} 渲染卡应 = paged_display_rect 期望 {exp:?}，实际 {got:?}"
            );
        }
    }

    /// R7-bug2：双页模式缩小到 zoom 0.5，整行两页应在视口内水平居中（spread 中心 ≈ 视口中心）。
    #[gpui::test]
    async fn paged_facing_zoom_out_centered(cx: &mut TestAppContext) {
        let (holder, cx) = open_paged_host(cx);
        cx.update(|_w, cx| {
            holder
                .borrow()
                .as_ref()
                .unwrap()
                .update(cx, |v, cx| v.set_page_mode(PageMode::Facing, cx));
        });
        for _ in 0..6 {
            let _ = cx.refresh();
            cx.run_until_parked();
        }
        cx.update(|_w, cx| {
            holder
                .borrow()
                .as_ref()
                .unwrap()
                .update(cx, |v, cx| v.set_zoom_abs(0.5, cx));
        });
        for _ in 0..6 {
            let _ = cx.refresh();
            cx.run_until_parked();
        }
        let vw = cx.update(|_w, cx| {
            f32::from(
                holder
                    .borrow()
                    .as_ref()
                    .unwrap()
                    .read(cx)
                    .scroll
                    .bounds()
                    .size
                    .width,
            )
        });
        let e0 = expected_display_rect(&holder, cx, 2, 0).expect("页 0");
        let e1 = expected_display_rect(&holder, cx, 2, 1).expect("页 1");
        let g0 = paged_card_content_rect(&holder, cx, 0).expect("卡 0");
        let g1 = paged_card_content_rect(&holder, cx, 1).expect("卡 1");
        assert!(
            (g0.0 - e0.0).abs() < 0.6 && (g1.0 - e1.0).abs() < 0.6,
            "双页 zoom0.5 卡位应≈期望"
        );
        // 整行 spread 中心 = (左卡左 + 右卡右)/2 ≈ 视口中心。
        let spread_center = (g0.0 + (g1.0 + g1.2)) / 2.0;
        assert!(
            (spread_center - vw / 2.0).abs() < 1.0,
            "双页 zoom0.5 整行应水平居中 {:.1}，实际 {:.2}",
            vw / 2.0,
            spread_center
        );
    }

    /// R7-K153 封口：分页下渲染卡 debug_bounds 与 `paged_display_rect`（经 `expected_display_rect`
    /// 独立重算）一致；且对该卡中心 simulate 命中 → 命中同页（render/hit 同源，防两路径分叉）。
    #[gpui::test]
    async fn paged_display_rect_matches_render_and_hit(cx: &mut TestAppContext) {
        let (holder, cx) = open_paged_host(cx);
        cx.update(|_w, cx| {
            holder
                .borrow()
                .as_ref()
                .unwrap()
                .update(cx, |v, cx| v.set_page_mode(PageMode::Facing, cx));
        });
        for _ in 0..6 {
            let _ = cx.refresh();
            cx.run_until_parked();
        }
        // 缩小到 0.5 使几何与 zoom=1 明显不同（封口非平凡）。
        cx.update(|_w, cx| {
            holder
                .borrow()
                .as_ref()
                .unwrap()
                .update(cx, |v, cx| v.set_zoom_abs(0.5, cx));
        });
        for _ in 0..6 {
            let _ = cx.refresh();
            cx.run_until_parked();
        }
        for page in 0usize..2 {
            let exp = expected_display_rect(&holder, cx, 2, page).expect("期望矩形");
            let got = paged_card_content_rect(&holder, cx, page).expect("卡应物化");
            assert!(
                (got.0 - exp.0).abs() < 0.6
                    && (got.1 - exp.1).abs() < 0.6
                    && (got.2 - exp.2).abs() < 0.6
                    && (got.3 - exp.3).abs() < 0.6,
                "页 {page} 渲染卡应 = paged_display_rect 期望 {exp:?}，实际 {got:?}"
            );
            // 卡中心（内容坐标）→ 窗口坐标（逆 window_to_pt：加滚动容器原点与偏移）→ 命中。
            let center_cx = exp.0 + exp.2 / 2.0;
            let center_cy = exp.1 + exp.3 / 2.0;
            let (bl, bt, offx, offy) = cx.update(|_w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                let b = v.scroll.bounds();
                (
                    b.left().as_f32(),
                    b.top().as_f32(),
                    f32::from(v.scroll.offset().x),
                    f32::from(v.scroll.offset().y),
                )
            });
            let click = gpui::point(px(bl + center_cx + offx), px(bt + center_cy + offy));
            let hit = cx.update(|w, cx| {
                holder
                    .borrow()
                    .as_ref()
                    .unwrap()
                    .read(cx)
                    .window_to_pt(w, click)
            });
            assert_eq!(
                hit.map(|(p, _, _)| p),
                Some(page),
                "页 {page} 卡中心命中应为该页（render/hit 同源）"
            );
        }
    }

    #[gpui::test]
    async fn grid_enter_renders_container_with_derived_virtualized_cards(cx: &mut TestAppContext) {
        if test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _pdf_guard = test_util::lock().lock().unwrap();
        let pdf_path = test_util::test_pdf_paths()[1].clone(); // pdf测试2.pdf：长文多页
        let holder: Rc<RefCell<Option<Entity<PdfViewer>>>> = Rc::default();
        let (_root, cx) = cx.add_window_view(|_window, cx| {
            let pdf = cx.new(|cx| PdfViewer::new(pdf_path.clone(), cx));
            *holder.borrow_mut() = Some(pdf.clone());
            GridHost { pdf }
        });
        cx.simulate_resize(gpui::size(px(1500.), px(900.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let mut loaded = false;
        for _ in 0..600 {
            cx.run_until_parked();
            loaded = cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).is_loaded());
            if loaded {
                break;
            }
        }
        assert!(loaded, "PDF 应在 600 帧内加载完成");
        enter_grid(&holder, cx);

        // 手算期望（渲染同口径）：网格滚动容器布局 bounds + 同常量 + 同纯函数
        let ((start, end), expect_count, n) = { expected_grid_band(&holder, cx, 0.0) };
        assert!(n > 120, "夹具应足量页（pdf测试2.pdf），实际 {n}");
        assert!(
            expect_count < n,
            "行带虚拟化：物化卡数应远小于总页数（{expect_count} < {n}）"
        );

        // 容器在 DOM，物化区间/卡数 == GridLayout 派生值
        assert!(cx.debug_bounds("pdf-grid").is_some(), "网格容器应渲染");
        assert!(
            cx.debug_bounds("pdf-grid-scroll").is_some(),
            "网格滚动容器应渲染"
        );
        let (range, drawn) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (v.grid_last_range.get(), v.grid_last_drawn.get())
        });
        assert_eq!(
            range,
            (start, end),
            "可视带应等于 GridLayout 派生区间 {start}..={end}"
        );
        assert_eq!(
            drawn, expect_count,
            "物化卡数应等于 GridLayout 派生数（首屏可视行 +±1 缓冲）"
        );
        // 带内卡在 DOM、可视带外页不物化（静态选择器：页 100 恒在带外）
        assert!(
            cx.debug_bounds("pdf-grid-item-0").is_some(),
            "带内首卡应在场"
        );
        assert!(
            cx.debug_bounds("pdf-grid-item-100").is_none(),
            "可视带外页（100）不应物化任何 DOM（虚拟化）"
        );
    }

    #[gpui::test]
    async fn grid_double_click_card_exits_and_jumps_to_page(cx: &mut TestAppContext) {
        if test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _pdf_guard = test_util::lock().lock().unwrap();
        let pdf_path = test_util::test_pdf_paths()[1].clone();
        let holder: Rc<RefCell<Option<Entity<PdfViewer>>>> = Rc::default();
        let (_root, cx) = cx.add_window_view(|_window, cx| {
            let pdf = cx.new(|cx| PdfViewer::new(pdf_path.clone(), cx));
            *holder.borrow_mut() = Some(pdf.clone());
            GridHost { pdf }
        });
        cx.simulate_resize(gpui::size(px(1500.), px(900.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let mut loaded = false;
        for _ in 0..600 {
            cx.run_until_parked();
            loaded = cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).is_loaded());
            if loaded {
                break;
            }
        }
        assert!(loaded, "PDF 应加载完成");
        enter_grid(&holder, cx);
        let n = cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).dims.len());

        // 双击「第 2 张卡」（页下标 1）中心 → 退出网格 + 单列跳转到该页（wave3 跳页语义迁到双击）
        let c1 = cx
            .debug_bounds("pdf-grid-item-1")
            .expect("带内第 2 卡应在场")
            .center();
        simulate_grid_double_click(cx, c1);
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let (mode, cur) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (v.grid_mode(), v.current_page())
        });
        assert!(!mode, "双击卡应退出网格模式");
        assert_eq!(
            cur,
            Some((2, n)),
            "双击第 2 张卡（0 基 1）后单列首见页应为 1 基 2，实际 {cur:?}"
        );
        // 退出网格后网格缩略图槽应已清空（切模式不残留引用）
        let (thumbs, pending) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (v.grid_thumbs.len(), v.pending_grid.len())
        });
        assert_eq!(thumbs, 0, "退出网格应清空网格缩略图槽");
        assert_eq!(pending, 0, "退出网格应清空在途网格请求");
    }

    /// 普通**单击**卡（无修饰、click_count==1）→ 切换该页选中态且**留在网格**（不退出、
    /// 不跳页）；再单击同卡 → 取消选中（toggle）。对应「单击选中」交互契约。
    #[gpui::test]
    async fn grid_single_click_selects_card_stays_in_grid(cx: &mut TestAppContext) {
        if test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _pdf_guard = test_util::lock().lock().unwrap();
        let pdf_path = test_util::test_pdf_paths()[1].clone();
        let holder: Rc<RefCell<Option<Entity<PdfViewer>>>> = Rc::default();
        let (_root, cx) = cx.add_window_view(|_window, cx| {
            let pdf = cx.new(|cx| PdfViewer::new(pdf_path.clone(), cx));
            *holder.borrow_mut() = Some(pdf.clone());
            GridHost { pdf }
        });
        cx.simulate_resize(gpui::size(px(1500.), px(900.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let mut loaded = false;
        for _ in 0..600 {
            cx.run_until_parked();
            loaded = cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).is_loaded());
            if loaded {
                break;
            }
        }
        assert!(loaded, "PDF 应加载完成");
        enter_grid(&holder, cx);
        // 普通单击卡 1（0 基）中心 → 选中该页 + 仍网格模式（不退出、不跳页）
        let c1 = cx
            .debug_bounds("pdf-grid-item-1")
            .expect("带内卡应在场")
            .center();
        cx.simulate_click(c1, gpui::Modifiers::default());
        for _ in 0..5 {
            let _ = cx.refresh();
            cx.run_until_parked();
        }
        let (selected, count, mode) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (v.grid_selected(1), v.grid_selected_count(), v.grid_mode())
        });
        assert!(selected, "普通单击应选中页 1");
        assert_eq!(count, 1, "选中计数应为 1");
        assert!(mode, "普通单击不应退出网格（留在网格）");
        // 再普通单击同卡 → 取消选中（toggle），仍网格模式
        cx.simulate_click(c1, gpui::Modifiers::default());
        for _ in 0..5 {
            let _ = cx.refresh();
            cx.run_until_parked();
        }
        let (selected, count, mode) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (v.grid_selected(1), v.grid_selected_count(), v.grid_mode())
        });
        assert!(!selected, "再次普通单击应取消选中（toggle）");
        assert_eq!(count, 0, "取消后计数为 0");
        assert!(mode, "取消选中后仍应在网格模式");
    }

    /// item 3：网格缩放——卡宽由**独立网格缩放 `grid_zoom`** 派生（`GRID_CARD_WIDTH ×
    /// grid_zoom`），**与阅读 `self.zoom` 互不影响**。程序化 `set_grid_zoom(×2)` → 卡宽/列数
    /// 按新 grid_zoom 变化、`self.zoom` **不变**、仍停留网格模式、可视页集合随新布局更新、
    /// 旧分桶缩略图保留作桥、按新分桶重取替换（R-C）。反向：网格内 `set_zoom`（阅读缩放）
    /// 不联动卡宽（卡宽仍按 grid_zoom）。期望值用「渲染同口径」手算（K177）：目标布局 =
    /// `grid_layout(dims, GRID_CARD_WIDTH×grid_zoom, …)` 同公式推导（放大 ⇒ 列数减少），
    /// 不自揣测。
    #[gpui::test]
    async fn grid_zoom_resizes_cards_and_refetches_thumbnails(cx: &mut TestAppContext) {
        if test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _pdf_guard = test_util::lock().lock().unwrap();
        let holder: Rc<RefCell<Option<Entity<PdfViewer>>>> = Rc::default();
        let (_root, cx) = open_grid_host_fixture(&holder, cx, 1);
        let (vw, vh, n, dims) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            let b = v.grid_scroll.bounds();
            (
                b.size.width.as_f32(),
                b.size.height.as_f32(),
                v.dims.len(),
                v.dims.clone(),
            )
        });
        assert!(n > 120, "夹具应足量页（pdf测试2.pdf），实际 {n}");
        // 前置：初始卡宽 = 280×grid_zoom（grid_zoom 默认 1.0），阅读 self.zoom 独立为 1.0。
        let (gz0, rz0) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (v.grid_zoom, v.zoom)
        });
        assert!(
            (gz0 - 1.0).abs() < 1e-6 && (rz0 - 1.0).abs() < 1e-6,
            "前置：grid_zoom/阅读 zoom 初始均 1.0：grid={gz0} read={rz0}"
        );
        // 手算目标几何（K177）：grid_zoom=2.0 ⇒ 卡宽 560，列数 = floor((vw-2·pad+gap)/(560+gap)) < 基线列数。
        let grid_zoom = 2.0f32;
        let target_w = GRID_CARD_WIDTH * grid_zoom;
        let layout_before = grid_layout(&dims, GRID_CARD_WIDTH, GRID_GAP, GRID_PADDING, vw);
        let layout_after = grid_layout(&dims, target_w, GRID_GAP, GRID_PADDING, vw);
        assert_ne!(layout_after.cols, layout_before.cols, "×2 缩放后列数应变化");
        assert!(
            layout_after.cols < layout_before.cols,
            "放大后列数应变少（卡变宽）"
        );
        let band_after = grid_visible_range(&layout_after, 0.0, vh).expect("缩放后首屏应有可视带");

        // 网格模式程序化缩放：set_grid_zoom(×2)（doc(hidden) 测试面）→ 卡宽派生 + 跨桶
        // 清旧缩略图（下一帧 ensure_window_grid 按新卡宽重取）；阅读 self.zoom 不变。
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, cx| {
                v.set_grid_zoom(grid_zoom, cx);
            });
        });
        // 等渲染带收敛：headless 重绘经 park 异步落到 render 帧（`grid_last_range` 由
        // render_pdf_grid 每帧按当前卡宽写），轮询至数值稳定 = 新布局真正接管。
        let mut got_band = false;
        for _ in 0..60 {
            let _ = cx.refresh();
            cx.run_until_parked();
            let band = cx.update(|_w, cx| {
                holder
                    .borrow()
                    .as_ref()
                    .unwrap()
                    .read(cx)
                    .grid_last_range
                    .get()
            });
            if band == band_after {
                got_band = true;
                break;
            }
        }
        assert!(
            got_band,
            "缩放后网格渲染带应收敛到新布局派生区间 {band_after:?}"
        );
        let (card_w, mode, vis_range, gz1, rz1) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (
                v.grid_card_width(),
                v.grid_mode(),
                v.grid_last_range.get(),
                v.grid_zoom,
                v.zoom,
            )
        });
        assert!(
            (card_w - target_w).abs() < 1e-3,
            "缩放后卡宽应 = 280×2=560，实际 {card_w}"
        );
        assert!(mode, "网格缩放应停留网格模式（不退出、不跳页）");
        assert_eq!(vis_range, band_after, "缩放后可视页集合 = 新布局派生区间");
        assert!(
            (gz1 - grid_zoom).abs() < 1e-4,
            "set_grid_zoom 应改 grid_zoom：{gz1}"
        );
        assert!(
            (rz1 - 1.0).abs() < 1e-6,
            "网格 set_grid_zoom 不得改阅读 zoom（独立缩放）：{rz1}"
        );
        // 效果断言：跨桶缩放**保留旧桶缩略图作桥**（不清空），下一帧按新分桶重取
        // **替换**桥——轮询直到出现当前桶图（bucket == grid_bucket_width(560) = 576）。
        // 不确定 worker 时序 → 推进虚拟时钟 + 少量真实等待轮询，与既有缩略图测试同姿势。
        let bucket_new = grid_bucket_width(target_w);
        let mut got = false;
        for _ in 0..600 {
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(std::time::Duration::from_millis(2));
            got = cx.update(|_w, cx| {
                holder
                    .borrow()
                    .as_ref()
                    .unwrap()
                    .read(cx)
                    .grid_thumbs
                    .values()
                    .any(|t| t.bucket == bucket_new)
            });
            if got {
                break;
            }
        }
        assert!(
            got,
            "缩放后缩略图应按新分桶重取替换桥（bucket == {bucket_new}）"
        );

        // 反向独立性：网格内程序化改**阅读** zoom（set_zoom）×1.5 → 卡宽仍按 grid_zoom
        // （不变）、仅 self.zoom 变化、仍停留网格（卡宽不被阅读缩放联动）。
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, _| {
                v.set_zoom(1.5);
            });
        });
        let (card_after_read, read_after, mode2) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (v.grid_card_width(), v.zoom, v.grid_mode())
        });
        assert!(
            (read_after - 1.5).abs() < 1e-3,
            "set_zoom 应改阅读 zoom：{read_after}"
        );
        assert!(
            (card_after_read - target_w).abs() < 1e-3,
            "阅读 set_zoom 不应联动网格卡宽（仍按 grid_zoom）：{card_after_read}"
        );
        assert!(mode2, "阅读 set_zoom 不应退出网格");
    }

    /// item 3b：网格 **Ctrl+滚轮** 交互分支（input.rs `on_scroll_wheel` 网格臂）——真实
    /// 输入路径（非程序化 `set_grid_zoom`）。gpui 测试平台可派发真实 `ScrollWheelEvent`（带 ctrl
    /// 修饰键，同单列 `pdf_ctrl_wheel_zooms_in_out`）。断五件事：
    /// 1) 卡宽按 **grid_zoom** 步进派生（`GRID_CARD_WIDTH × grid_zoom`，280×1.12 手算）；
    /// 2) **独立性**：`self.zoom`（阅读缩放）**不变**——网格缩放与阅读缩放解耦；
    /// 3) `grid_scroll` 偏移按 `ratio = new_card/old_card` **同比重锚**（offset×ratio）；
    /// 4) 网格模式保持（不退出、不跳页）；
    /// 5) 普通（无 ctrl）滚轮不缩放（grid_zoom/阅读 zoom/卡宽都不变）且照常垂直滚动（未被拦截）。
    #[gpui::test]
    async fn grid_ctrl_wheel_zooms_cards_reanchors_scroll_and_keeps_mode(cx: &mut TestAppContext) {
        if test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _pdf_guard = test_util::lock().lock().unwrap();
        let holder: Rc<RefCell<Option<Entity<PdfViewer>>>> = Rc::default();
        let (_root, cx) = open_grid_host_fixture(&holder, cx, 1);
        // 前置：给网格一个非零滚动偏移（滚到第 3 行顶），供缩放重锚断言（offset × ratio 同比）。
        let off_target = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            let b = v.grid_scroll.bounds();
            let layout = grid_layout(
                &v.dims,
                v.grid_card_width(),
                GRID_GAP,
                GRID_PADDING,
                b.size.width.as_f32(),
            );
            layout.row_top[2]
        });
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, _| {
                v.grid_scroll
                    .set_offset(gpui::point(px(0.), px(-off_target)));
            });
        });
        for _ in 0..5 {
            let _ = cx.refresh();
            cx.run_until_parked();
        }
        let (gz0, read0, card0, off0) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (
                v.grid_zoom,
                v.zoom,
                v.grid_card_width(),
                v.grid_scroll.offset(),
            )
        });
        let sy0 = f32::from(-off0.y);
        assert!(
            sy0 > 0.0,
            "前置：网格应处于非零滚动（滚到第 3 行顶），实际 {sy0}"
        );
        assert!(
            (gz0 - 1.0).abs() < 1e-6,
            "前置：grid_zoom 初始应为 1.0，实际 {gz0}"
        );
        assert!(
            (read0 - 1.0).abs() < 1e-6,
            "前置：阅读 zoom 初始应为 1.0，实际 {read0}"
        );

        // Ctrl+向上滚一档（delta_y=240 → factor=1+ZOOM_STEP=1.12）→ grid_zoom 步进衍生卡宽、
        // 滚动同比重锚；阅读 zoom **保持不变**（独立性）。
        cx.simulate_event(grid_zoom_wheel(true, 240.0));
        for _ in 0..5 {
            let _ = cx.refresh();
            cx.run_until_parked();
        }
        let (gz1, read1, card1, off1, mode) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (
                v.grid_zoom,
                v.zoom,
                v.grid_card_width(),
                v.grid_scroll.offset(),
                v.grid_mode(),
            )
        });
        let ratio = card1 / card0;
        assert!(gz1 > gz0, "Ctrl+向上滚应放大 grid_zoom：{gz0} → {gz1}");
        assert!(
            (card1 - GRID_CARD_WIDTH * gz1).abs() < 1e-3,
            "缩放后卡宽应 = 280×grid_zoom，实际 {card1}"
        );
        assert!(
            (read1 - read0).abs() < 1e-6,
            "网格 Ctrl+滚轮不得改阅读 zoom（独立缩放）：{read0} → {read1}"
        );
        let exp_sy = sy0 * ratio;
        let got_sy = f32::from(-off1.y);
        assert!(
            (got_sy - exp_sy).abs() < 2.0,
            "Ctrl 缩放后网格滚动应同比重锚（offset×ratio）：{sy0}×{ratio:.3}={exp_sy:.1}，实际 {got_sy:.1}"
        );
        assert!(mode, "Ctrl 缩放应停留网格模式（不退出、不跳页）");

        // 普通滚轮（无 Ctrl）→ 不缩放（grid_zoom/阅读 zoom/卡宽都不变），且照常垂直滚动（不拦截）。
        let (gz_before, read_before, card_before) = (gz1, read1, card1);
        let off_before = f32::from(-off1.y);
        cx.simulate_event(grid_zoom_wheel(false, -120.0));
        for _ in 0..5 {
            let _ = cx.refresh();
            cx.run_until_parked();
        }
        let (gz2, read2, card2, off2) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (
                v.grid_zoom,
                v.zoom,
                v.grid_card_width(),
                v.grid_scroll.offset(),
            )
        });
        assert!(
            (gz2 - gz_before).abs() < 1e-6,
            "普通滚轮不应缩放 grid_zoom：{gz_before} → {gz2}"
        );
        assert!(
            (read2 - read_before).abs() < 1e-6,
            "普通滚轮不应改阅读 zoom：{read_before} → {read2}"
        );
        assert!(
            (card2 - card_before).abs() < 1e-3,
            "普通滚轮不应改卡宽：{card_before} → {card2}"
        );
        assert!(
            f32::from(-off2.y) > off_before,
            "普通滚轮应垂直下滚（未被缩放拦截）：{off_before} → {}",
            f32::from(-off2.y)
        );
    }

    /// item 3b'：**网格缩放列数钳制**（K167 本线）——Ctrl+滚轮把 `grid_zoom` 推到当前视口
    /// 宽的钳界（[z_min, z_max]，见 `grid_zoom_limits_for_cols`）即**停止**：放大极限恒 2 列
    /// （不可能到 1）、缩小极限恒 9 列（不可能到 10+）。期望列数用「渲染同口径」公式自洽
    /// 推导（K177：钳界来自同口径纯函数；列数 = `grid_layout` 同公式）。
    #[gpui::test]
    async fn grid_ctrl_wheel_clamps_columns_to_two_and_nine(cx: &mut TestAppContext) {
        if test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _pdf_guard = test_util::lock().lock().unwrap();
        let holder: Rc<RefCell<Option<Entity<PdfViewer>>>> = Rc::default();
        let (_root, cx) = open_grid_host_fixture(&holder, cx, 1);
        // 前置：渲染同口径 vw（与 render/tiles/set_grid_zoom 同一来源 = grid_scroll bounds 宽）。
        let (vw, gz0) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (v.grid_scroll.bounds().size.width.as_f32(), v.grid_zoom)
        });
        assert!(vw > 100.0, "前置：网格视口宽应有效：{vw}");
        let (z_min, z_max) = grid_zoom_limits_for_cols(
            GRID_CARD_WIDTH,
            GRID_GAP,
            GRID_PADDING,
            vw,
            GRID_MAX_COLS,
            GRID_MIN_COLS,
        );
        assert!(z_min < z_max, "前置：钳区间应有效：[{z_min}, {z_max}]");
        assert!(
            (gz0 - 1.0).abs() < 1e-6,
            "前置：grid_zoom 初始应为 1.0：{gz0}"
        );

        // ① 放大：连滚大档（factor=1+ZOOM_STEP=1.12）直到 grid_zoom 不再增长（钳到 z_max）。
        let wheel_until_stable = |cx: &mut VisualTestContext, delta: f32| {
            let mut prev = cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).grid_zoom);
            let mut gz = prev;
            for _ in 0..200 {
                cx.simulate_event(grid_zoom_wheel(true, delta));
                for _ in 0..3 {
                    let _ = cx.refresh();
                    cx.run_until_parked();
                }
                let next = cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).grid_zoom);
                gz = next;
                if (next - prev).abs() < 1e-6 {
                    break;
                }
                prev = next;
            }
            gz
        };
        let gz_up = wheel_until_stable(cx, 240.0);
        assert!(
            (gz_up - z_max).abs() < 1e-3,
            "放大应钳到 z_max={z_max}，实际 {gz_up}"
        );
        // 列数按「渲染同口径」= 2（不可能到 1 列）。
        let cols_up = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            grid_layout(&v.dims, v.grid_card_width(), GRID_GAP, GRID_PADDING, vw).cols
        });
        assert_eq!(cols_up, 2, "放大极限应恒 2 列（不可能到 1 列）：{cols_up}");
        // 再滚一档放大也不应变化（已钉在上限）。
        let before = gz_up;
        cx.simulate_event(grid_zoom_wheel(true, 240.0));
        for _ in 0..3 {
            let _ = cx.refresh();
            cx.run_until_parked();
        }
        let after = cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).grid_zoom);
        assert!(
            (after - before).abs() < 1e-6,
            "钉上限后放大不应变化：{before} → {after}"
        );

        // ② 缩小：连滚小档（factor=1/(1+ZOOM_STEP)）直到 grid_zoom 不再减少（钳到 z_min）。
        let gz_down = wheel_until_stable(cx, -240.0);
        assert!(
            (gz_down - z_min).abs() < 1e-3,
            "缩小应钳到 z_min={z_min}，实际 {gz_down}"
        );
        let cols_down = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            grid_layout(&v.dims, v.grid_card_width(), GRID_GAP, GRID_PADDING, vw).cols
        });
        assert_eq!(
            cols_down, 9,
            "缩小极限应恒 9 列（不可能到 10 列）：{cols_down}"
        );
        // 再滚一档缩小也不应变化（已钉在下限）。
        let before = gz_down;
        cx.simulate_event(grid_zoom_wheel(true, -240.0));
        for _ in 0..3 {
            let _ = cx.refresh();
            cx.run_until_parked();
        }
        let after = cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).grid_zoom);
        assert!(
            (after - before).abs() < 1e-6,
            "钉下限后缩小不应变化：{before} → {after}"
        );

        // 全程未退出网格 / 未跳页。
        let mode = cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).grid_mode());
        assert!(mode, "列数钳缩放应停留网格模式");
    }

    /// item 3b''：**视口宽变化归一化**（R4）——宽视口把网格缩放到放大上限后拉窄窗口
    /// （新列数钳界收窄）→ `grid_zoom` 超标，下一帧经 `set_grid_zoom` 归一化回新界内
    /// （同比重锚/跨桶桥随路生效），列数随新视口重收为 2。走真实 resize → render 路径。
    #[gpui::test]
    async fn grid_viewport_shrink_normalizes_zoom_to_new_column_bounds(cx: &mut TestAppContext) {
        if test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _pdf_guard = test_util::lock().lock().unwrap();
        let holder: Rc<RefCell<Option<Entity<PdfViewer>>>> = Rc::default();
        let (_root, cx) = open_grid_host_fixture(&holder, cx, 1);
        // 前置：宽视口（≈1500）下把 grid_zoom 推到列数钳上限 z_max(wide)=2.6。
        let vw_wide = cx.update(|_w, cx| {
            holder
                .borrow()
                .as_ref()
                .unwrap()
                .read(cx)
                .grid_scroll
                .bounds()
                .size
                .width
                .as_f32()
        });
        let (_, z_max_wide) = grid_zoom_limits_for_cols(
            GRID_CARD_WIDTH,
            GRID_GAP,
            GRID_PADDING,
            vw_wide,
            GRID_MAX_COLS,
            GRID_MIN_COLS,
        );
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, cx| {
                v.set_grid_zoom(100.0, cx); // 100 超上限 → 钳到 z_max(wide)
            });
        });
        let gz_before = cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).grid_zoom);
        assert!(
            (gz_before - z_max_wide).abs() < 1e-3,
            "前置：宽视口 set_grid_zoom(100) 应钳到 z_max(wide)={z_max_wide}，实际 {gz_before}"
        );
        // 拉窄窗口（800×900）：新列数钳界收窄（z_max(narrow) < z_max(wide)）。
        cx.simulate_resize(gpui::size(px(800.), px(900.)));
        // 轮询直到归一化 settle：resize → render(narrow) → set_grid_zoom → notify →
        // 下一帧 render 的链在 headless 需多拍收敛（每次轮询多刷几帧）。
        let mut vw_narrow = 1500.0;
        let mut gz_after = gz_before;
        let mut z_max_narrow = z_max_wide;
        for _ in 0..30 {
            for _ in 0..3 {
                let _ = cx.refresh();
                cx.run_until_parked();
            }
            (vw_narrow, gz_after) = cx.update(|_w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                (v.grid_scroll.bounds().size.width.as_f32(), v.grid_zoom)
            });
            let (_, z_max) = grid_zoom_limits_for_cols(
                GRID_CARD_WIDTH,
                GRID_GAP,
                GRID_PADDING,
                vw_narrow,
                GRID_MAX_COLS,
                GRID_MIN_COLS,
            );
            z_max_narrow = z_max;
            if (gz_after - z_max).abs() < 1e-3 {
                break;
            }
        }
        assert!(
            z_max_narrow < gz_before - 1e-3,
            "前置：拉窄后钳上限应收窄：z_max(narrow)={z_max_narrow} < 原 grid_zoom={gz_before}"
        );
        assert!(
            (gz_after - z_max_narrow).abs() < 1e-3,
            "视口拉窄后 grid_zoom 应归一化到新 z_max(narrow)={z_max_narrow}，实际 {gz_after}"
        );
        // 列数随新视口重收为 2（不可能到 1）。
        let cols = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            grid_layout(
                &v.dims,
                v.grid_card_width(),
                GRID_GAP,
                GRID_PADDING,
                vw_narrow,
            )
            .cols
        });
        assert_eq!(cols, 2, "视口拉窄归一化后列数应 2：{cols}");
        // 网格模式保持（归一化不退出网格）。
        let mode = cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).grid_mode());
        assert!(mode, "视口拉窄归一化应停留网格模式");
    }

    /// item 3c：**阅读缩放与网格卡宽双向独立**——起始在单列阅读模式 `set_zoom(1.5)`
    /// （程序化阅读缩放）→ `self.zoom=1.5`；随后进入网格 → `grid_card_width()` 仍按
    /// **独立 grid_zoom**（默认 1.0）= 280（**不是** 280×1.5），`self.zoom` 保持 1.5；
    /// 退出网格回单列阅读 → `self.zoom` 仍 1.5（网格缩放不串读阅读）。
    #[gpui::test]
    async fn reading_zoom_independent_of_grid_card_width(cx: &mut TestAppContext) {
        if test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _pdf_guard = test_util::lock().lock().unwrap();
        let holder: Rc<RefCell<Option<Entity<PdfViewer>>>> = Rc::default();
        // 不用 open_grid_host_fixture（它会自动进网格）；先留在单列阅读模式改阅读 zoom。
        let pdf_path = test_util::test_pdf_paths()[1].clone();
        let (_root, cx) = cx.add_window_view(|_window, cx| {
            let pdf = cx.new(|cx| PdfViewer::new(pdf_path.clone(), cx));
            *holder.borrow_mut() = Some(pdf.clone());
            GridHost { pdf }
        });
        cx.simulate_resize(gpui::size(px(1500.), px(900.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let mut loaded = false;
        for _ in 0..600 {
            cx.run_until_parked();
            loaded = cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).is_loaded());
            if loaded {
                break;
            }
        }
        assert!(loaded, "PDF 应加载完成");
        assert!(
            !cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).grid_mode()),
            "前置：应在单列阅读模式"
        );
        // 单列阅读缩放到 1.5（程序化 set_zoom，等同阅读 Ctrl+滚轮改 self.zoom）。
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, _| {
                v.set_zoom(1.5);
            });
        });
        let (read_z0, card_reading) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (v.zoom, v.grid_card_width())
        });
        assert!(
            (read_z0 - 1.5).abs() < 1e-3,
            "set_zoom 应改阅读 zoom：{read_z0}"
        );
        assert!(
            (card_reading - GRID_CARD_WIDTH).abs() < 1e-3,
            "阅读 set_zoom 不应改网格卡宽（grid_zoom 默认 1.0）：{card_reading}"
        );
        // 进入网格：卡宽应仍按独立 grid_zoom（默认 1.0）= 280，而非 280×1.5=420。
        enter_grid(&holder, cx);
        let (card, grid_z, read_z1, mode) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (v.grid_card_width(), v.grid_zoom, v.zoom, v.grid_mode())
        });
        assert!(
            (card - GRID_CARD_WIDTH).abs() < 1e-3,
            "阅读缩放后进网格，卡宽应仍按 grid_zoom（默认 1.0）=280，实际 {card}（不是 280×阅读zoom）"
        );
        assert!(
            (grid_z - 1.0).abs() < 1e-6,
            "grid_zoom 初始应为 1.0：{grid_z}"
        );
        assert!(
            (read_z1 - 1.5).abs() < 1e-3,
            "进网格不改阅读 zoom：{read_z1}"
        );
        assert!(mode, "应在网格模式");
        // 退出网格回阅读：self.zoom 仍 1.5（网格缩放不串读阅读）。
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, cx| {
                v.set_grid_mode(false, cx);
            });
        });
        let read_z2 = cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).zoom);
        assert!(
            (read_z2 - 1.5).abs() < 1e-3,
            "退出网格后阅读 zoom 应保持 1.5：{read_z2}"
        );
    }

    /// item 3（同桶复用裁决 + 跨桶保桥）：网格缩放按 `grid_bucket_width` GRID_BUCKET_PX=32
    /// 量化分桶缓存。断言两翼：
    /// ① **同桶内**缩放（factor=1.012 → 卡宽 280→283.36，手算 283.36/32=8.855→round 9→288，
    ///    与 280 同 288 桶）→ 已入槽缩略图**保留**（不清、不闪占位）；
    /// ② **跨桶**缩放（factor=1.12 → 卡宽 313.6 → 320 桶 ≠ 288）→ 旧分桶缩略图**保留作桥**
    ///   （清在途不清图，整屏不落占位白闪；R-A），随后按新分桶重取**替换**桥（R-C/R-D）。
    #[gpui::test]
    async fn grid_ctrl_wheel_same_bucket_reuses_thumbs_cross_bucket_keeps_bridge(
        cx: &mut TestAppContext,
    ) {
        if test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _pdf_guard = test_util::lock().lock().unwrap();
        let holder: Rc<RefCell<Option<Entity<PdfViewer>>>> = Rc::default();
        let (_root, cx) = open_grid_host_fixture(&holder, cx, 1);
        // 前置：等待至少一张缩略图入槽（worker 真实栅格返回，K37 效果断言；同桶复用断言
        // 需有「已入槽缓存」可观察）。
        let mut primed = false;
        for _ in 0..600 {
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(std::time::Duration::from_millis(2));
            primed = cx.update(|_w, cx| {
                !holder
                    .borrow()
                    .as_ref()
                    .unwrap()
                    .read(cx)
                    .grid_thumbs
                    .is_empty()
            });
            if primed {
                break;
            }
        }
        assert!(
            primed,
            "前置：网格缩略图应已入槽（同桶复用断言需有缓存可观察）"
        );
        let bucket0 = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            grid_bucket_width(v.grid_card_width())
        });
        assert_eq!(
            bucket0, 288,
            "前置：卡宽 280 应落 288 桶（280/32=8.75→round 9）"
        );

        // ① 同桶内缩放：delta_y=0.10 → factor=1+0.10×0.12=1.012 → 卡宽 283.36，仍在 288 桶
        //    → 缩略图缓存与在途请求保留（不清）。
        cx.simulate_event(grid_zoom_wheel(true, 0.10));
        for _ in 0..5 {
            let _ = cx.refresh();
            cx.run_until_parked();
        }
        let (card_mid, bucket_mid, thumbs_mid) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (
                v.grid_card_width(),
                grid_bucket_width(v.grid_card_width()),
                !v.grid_thumbs.is_empty(),
            )
        });
        assert_eq!(bucket_mid, bucket0, "小步缩放应仍落在同一分桶（{bucket0}）");
        assert!(
            (card_mid - GRID_CARD_WIDTH * (1.0 + 0.10 * ZOOM_STEP)).abs() < 1e-3,
            "同桶缩放卡宽应 = 280×(1+0.10×ZOOM_STEP)，实际 {card_mid}"
        );
        assert!(thumbs_mid, "同桶缩放不应清空已入槽缩略图（同桶复用）");

        // ② 跨桶缩放：delta_y=240 → factor=1.12 → 卡宽 313.6 → 320 桶 ≠ 288。
        cx.simulate_event(grid_zoom_wheel(true, 240.0));
        // 紧接事件后的同步窗口：旧分桶缩略图**保留作桥**（R-A；不再即时清空——旧图拉伸
        // 顶住新卡盒防整屏占位白闪）；此刻调度重派尚未发生，已入槽图应仍属旧桶 288（全
        // 部 stale）。
        let (thumbs_kept, all_stale, bucket_after) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            let bucket = grid_bucket_width(v.grid_card_width());
            let buckets: Vec<u32> = v.grid_thumbs.values().map(|t| t.bucket).collect();
            let all_stale = !buckets.is_empty() && buckets.iter().all(|&b| b != bucket);
            (!v.grid_thumbs.is_empty(), all_stale, bucket)
        });
        assert_ne!(
            bucket_after, bucket_mid,
            "跨桶缩放应换桶：{bucket_mid} → {bucket_after}"
        );
        assert!(thumbs_kept, "跨桶缩放应保留旧分桶缩略图作桥（不清空，R-A）");
        assert!(
            all_stale,
            "跨桶缩放后旧桶缩略图应全部 stale（bucket != 当前桶）"
        );
        // 随后按新分桶重取**替换桥**（R-D：调度对 stale 页照常重派；R-C：新桶图回填后
        // bucket == 当前桶）。worker 真实栅格，同既有缩略图测试轮询姿势。
        let mut replaced = false;
        for _ in 0..600 {
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(std::time::Duration::from_millis(2));
            replaced = cx.update(|_w, cx| {
                holder
                    .borrow()
                    .as_ref()
                    .unwrap()
                    .read(cx)
                    .grid_thumbs
                    .values()
                    .any(|t| t.bucket == bucket_after)
            });
            if replaced {
                break;
            }
        }
        assert!(
            replaced,
            "跨桶缩放后应重取当前桶缩略图替换桥（bucket == {bucket_after}）"
        );
    }

    /// R-B「跨桶缩放无占位增长」：跨桶后旧分桶缩略图保留作桥 → 可见带卡**不再落占位**。
    /// 前置把**当前可视带**全部页缩略图打满（调度每帧 ≤[`config::pdf::GRID_PREFETCH_INFLIGHT`]
    /// 枚，600 帧内可达）；记录 `grid_placeholder_hits` 基线 → 跨桶缩放 → 直到某张新桶图
    /// 替换桥为止，**逐帧渲染断言占位计数不增长**——桥图顶上；缩放后卡变大带收敛为旧带
    /// 子集，无「新入带无图页」混入。K173：断言的是「画的是桥/图而非占位」的真实机制
    /// （渲染路径计数），非宽松 is_empty 断言。
    #[gpui::test]
    async fn grid_ctrl_wheel_cross_bucket_no_placeholder_growth(cx: &mut TestAppContext) {
        if test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _pdf_guard = test_util::lock().lock().unwrap();
        let holder: Rc<RefCell<Option<Entity<PdfViewer>>>> = Rc::default();
        let (_root, cx) = open_grid_host_fixture(&holder, cx, 1);
        // 前置：把当前可视带逐页缩略图全部打满（调度每帧 ≤2 枚）。带内页全部就位后跨桶
        // 缩放才不会因「带内新增无图页」误计入占位。
        let (band, _, _) = expected_grid_band(&holder, cx, 0.0);
        let (band_start, band_end) = band;
        let mut band_primed = false;
        for _ in 0..600 {
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(std::time::Duration::from_millis(2));
            band_primed = cx.update(|_w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                (band_start..=band_end).all(|p| v.grid_thumbs.contains_key(&p))
            });
            if band_primed {
                break;
            }
        }
        assert!(
            band_primed,
            "前置：可视带全部页缩略图应打满（{band_start}..={band_end}）"
        );
        // 跨桶缩放基线：跨桶事件前最后一次渲染已把带内页就位图渲染为图/桥，计数已稳定。
        let baseline = cx.update(|_w, cx| {
            holder
                .borrow()
                .as_ref()
                .unwrap()
                .read(cx)
                .grid_placeholder_hits
                .get()
        });
        cx.simulate_event(grid_zoom_wheel(true, 240.0));
        let bucket_after = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            grid_bucket_width(v.grid_card_width())
        });
        // 跨桶事件到新桶图替换桥为止：逐帧渲染，占位计数必须不增长（桥图顶上，R-B）。
        let mut replaced = false;
        for _ in 0..600 {
            let _ = cx.refresh();
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(std::time::Duration::from_millis(2));
            let (hits, now_replaced) = cx.update(|_w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                (
                    v.grid_placeholder_hits.get(),
                    v.grid_thumbs.values().any(|t| t.bucket == bucket_after),
                )
            });
            assert!(
                hits <= baseline,
                "跨桶缩放后占位计数不应增长（桥图顶上）：baseline={baseline} got={hits}"
            );
            if now_replaced {
                replaced = true;
                break;
            }
        }
        assert!(
            replaced,
            "跨桶缩放后新桶缩略图应替换桥（bucket == {bucket_after}）"
        );
    }

    #[gpui::test]
    async fn grid_scroll_moves_band_and_releases_old_cards(cx: &mut TestAppContext) {
        if test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _pdf_guard = test_util::lock().lock().unwrap();
        let pdf_path = test_util::test_pdf_paths()[1].clone();
        let holder: Rc<RefCell<Option<Entity<PdfViewer>>>> = Rc::default();
        let (_root, cx) = cx.add_window_view(|_window, cx| {
            let pdf = cx.new(|cx| PdfViewer::new(pdf_path.clone(), cx));
            *holder.borrow_mut() = Some(pdf.clone());
            GridHost { pdf }
        });
        cx.simulate_resize(gpui::size(px(1500.), px(900.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let mut loaded = false;
        for _ in 0..600 {
            cx.run_until_parked();
            loaded = cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).is_loaded());
            if loaded {
                break;
            }
        }
        assert!(loaded, "PDF 应加载完成");
        enter_grid(&holder, cx);
        let ((old_start, _old_end), _, _) = expected_grid_band(&holder, cx, 0.0);
        // 滚到第 6 行顶（内容坐标系 y = row_top[5]）并推进渲染帧
        let offset = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            let b = v.grid_scroll.bounds();
            let layout = grid_layout(
                &v.dims,
                GRID_CARD_WIDTH,
                GRID_GAP,
                GRID_PADDING,
                b.size.width.as_f32(),
            );
            layout.row_top[5]
        });
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, _| {
                v.grid_scroll.set_offset(gpui::point(px(0.), px(-offset)));
            });
        });
        for _ in 0..5 {
            let _ = cx.refresh();
            cx.run_until_parked();
        }

        // 新可视带 = 同口径派生的第 6 行带；断言明显下移
        let ((new_start, new_end), new_count, _) = expected_grid_band(&holder, cx, offset);
        assert!(
            new_start >= old_start + 4,
            "滚到第 6 行后可视带应明显下移：{old_start} → {new_start}"
        );
        let (range, drawn) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (v.grid_last_range.get(), v.grid_last_drawn.get())
        });
        assert_eq!(
            range,
            (new_start, new_end),
            "滚动后可视带应跟随网格滚动偏移"
        );
        assert_eq!(drawn, new_count, "滚动后物化卡数仍应等于派生带宽");

        // 旧卡释放：grid_thumbs / pending_grid 不得残留新带之外的页（确定性，不依赖 worker 时序）
        let (thumbs_keys, pending_keys) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (
                v.grid_thumbs.keys().copied().collect::<Vec<usize>>(),
                v.pending_grid.keys().copied().collect::<Vec<usize>>(),
            )
        });
        for p in thumbs_keys.iter().chain(pending_keys.iter()) {
            assert!(
                *p >= new_start && *p <= new_end,
                "滚动后旧带页 {p} 应被释放（新带 [{new_start},{new_end}]）"
            );
        }
    }

    #[gpui::test]
    async fn grid_thumbnails_fetched_into_slots(cx: &mut TestAppContext) {
        if test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _pdf_guard = test_util::lock().lock().unwrap();
        let pdf_path = test_util::test_pdf_paths()[1].clone();
        let holder: Rc<RefCell<Option<Entity<PdfViewer>>>> = Rc::default();
        let (_root, cx) = cx.add_window_view(|_window, cx| {
            let pdf = cx.new(|cx| PdfViewer::new(pdf_path.clone(), cx));
            *holder.borrow_mut() = Some(pdf.clone());
            GridHost { pdf }
        });
        cx.simulate_resize(gpui::size(px(1500.), px(900.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let mut loaded = false;
        for _ in 0..600 {
            cx.run_until_parked();
            loaded = cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).is_loaded());
            if loaded {
                break;
            }
        }
        assert!(loaded, "PDF 应加载完成");
        enter_grid(&holder, cx);

        // 效果断言：网格缩略图经 request_grid（背景信道，每帧 ≤GRID_PREFETCH_INFLIGHT）
        // 真实拉取入槽——推进虚拟时钟驱动 16ms 泵 + 少量真实等待给 pdfium worker。
        let mut got = false;
        for _ in 0..600 {
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(std::time::Duration::from_millis(2));
            got = cx.update(|_w, cx| {
                !holder
                    .borrow()
                    .as_ref()
                    .unwrap()
                    .read(cx)
                    .grid_thumbs
                    .is_empty()
            });
            if got {
                break;
            }
        }
        assert!(
            got,
            "网格缩略图应经 request_grid 拉取入 grid_thumbs（带内首屏）"
        );
    }

    // ---- 网格多选（wave5）：选中集 / checkbox / Ctrl+Shift / 工具栏 ----

    /// wave5：勾选卡右上角 checkbox → 该页进入选中集 + 选中视觉（角标）出现；**不退出网格**
    /// （checkbox 子元素 `stop_propagation` 独占，卡的本底单击「退出网格」不应触发）。
    /// 期望由交互路径推导：checkbox 是卡内子元素，命中栈先于卡触发且拦截冒泡（K15）。
    #[gpui::test]
    async fn grid_checkbox_toggles_selection_with_visual(cx: &mut TestAppContext) {
        if test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _pdf_guard = test_util::lock().lock().unwrap();
        let pdf_path = test_util::test_pdf_paths()[1].clone();
        let holder: Rc<RefCell<Option<Entity<PdfViewer>>>> = Rc::default();
        let (_root, cx) = cx.add_window_view(|_window, cx| {
            let pdf = cx.new(|cx| PdfViewer::new(pdf_path.clone(), cx));
            *holder.borrow_mut() = Some(pdf.clone());
            GridHost { pdf }
        });
        cx.simulate_resize(gpui::size(px(1500.), px(900.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let mut loaded = false;
        for _ in 0..600 {
            cx.run_until_parked();
            loaded = cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).is_loaded());
            if loaded {
                break;
            }
        }
        assert!(loaded, "PDF 应加载完成");
        enter_grid(&holder, cx);
        // 进入网格应清空选中集（一次会话语义）
        assert_eq!(
            cx.update(|_w, cx| holder
                .borrow()
                .as_ref()
                .unwrap()
                .read(cx)
                .grid_selected_count()),
            0,
            "进入网格应清空选中集"
        );
        assert!(
            cx.debug_bounds("pdf-grid-badge-0").is_none(),
            "未选中时不应有选中角标"
        );
        // 勾选第 1 张卡（页 0）checkbox
        let c0 = cx
            .debug_bounds("pdf-grid-check-0")
            .expect("带内首卡 checkbox 应在场")
            .center();
        cx.simulate_click(c0, gpui::Modifiers::default());
        for _ in 0..5 {
            let _ = cx.refresh();
            cx.run_until_parked();
        }
        let (selected, count, mode) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (v.grid_selected(0), v.grid_selected_count(), v.grid_mode())
        });
        assert!(selected, "勾选该卡后页 0 应进入选中集");
        assert_eq!(count, 1, "选中计数应为 1");
        assert!(mode, "勾选 checkbox 不应退出网格");
        assert!(
            cx.debug_bounds("pdf-grid-badge-0").is_some(),
            "选中后应出现选中角标（选中视觉）"
        );
        // 再勾选一次 → 取消选中
        let c0 = cx.debug_bounds("pdf-grid-check-0").unwrap().center();
        cx.simulate_click(c0, gpui::Modifiers::default());
        for _ in 0..5 {
            let _ = cx.refresh();
            cx.run_until_parked();
        }
        let (selected, count) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (v.grid_selected(0), v.grid_selected_count())
        });
        assert!(!selected, "再次勾选应取消选中");
        assert_eq!(count, 0, "取消后计数为 0");
    }

    /// wave5：Ctrl+单击卡 → 切换选中态（select→unselect）且**不退出网格**（Ctrl 分支不调
    /// `set_grid_mode(false)`，行为表明文）。期望 = Ctrl 修饰键到达卡的 `on_mouse_down`。
    #[gpui::test]
    async fn grid_ctrl_click_toggles_selection_without_exit(cx: &mut TestAppContext) {
        if test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _pdf_guard = test_util::lock().lock().unwrap();
        let pdf_path = test_util::test_pdf_paths()[1].clone();
        let holder: Rc<RefCell<Option<Entity<PdfViewer>>>> = Rc::default();
        let (_root, cx) = cx.add_window_view(|_window, cx| {
            let pdf = cx.new(|cx| PdfViewer::new(pdf_path.clone(), cx));
            *holder.borrow_mut() = Some(pdf.clone());
            GridHost { pdf }
        });
        cx.simulate_resize(gpui::size(px(1500.), px(900.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let mut loaded = false;
        for _ in 0..600 {
            cx.run_until_parked();
            loaded = cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).is_loaded());
            if loaded {
                break;
            }
        }
        assert!(loaded, "PDF 应加载完成");
        enter_grid(&holder, cx);
        let ctrl = gpui::Modifiers {
            control: true,
            ..Default::default()
        };
        // 第 1 张卡（页 0）Ctrl+单击 → 选中且不退出网格
        let c0 = cx.debug_bounds("pdf-grid-item-0").unwrap().center();
        cx.simulate_click(c0, ctrl);
        for _ in 0..5 {
            let _ = cx.refresh();
            cx.run_until_parked();
        }
        let (selected, mode) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (v.grid_selected(0), v.grid_mode())
        });
        assert!(selected, "Ctrl+单击应选中页 0");
        assert!(mode, "Ctrl+单击不应退出网格");
        // 同一位置再次 Ctrl+单击 → 取消选中且仍不退出网格
        cx.simulate_click(c0, ctrl);
        for _ in 0..5 {
            let _ = cx.refresh();
            cx.run_until_parked();
        }
        let (selected, mode) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (v.grid_selected(0), v.grid_mode())
        });
        assert!(!selected, "再次 Ctrl+单击应取消选中");
        assert!(mode, "取消选中后仍应在网格模式");
    }

    /// wave5：Shift+单击 → 从「锚点页」（上次 Ctrl 单选）到当前页**闭区间**选中（3 页断言
    /// 具体集合），其它已选保持、锚点更新为该页，且不退出网格。锚点由 Ctrl+单击先建立。
    #[gpui::test]
    async fn grid_shift_click_selects_closed_range(cx: &mut TestAppContext) {
        if test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _pdf_guard = test_util::lock().lock().unwrap();
        let pdf_path = test_util::test_pdf_paths()[1].clone();
        let holder: Rc<RefCell<Option<Entity<PdfViewer>>>> = Rc::default();
        let (_root, cx) = cx.add_window_view(|_window, cx| {
            let pdf = cx.new(|cx| PdfViewer::new(pdf_path.clone(), cx));
            *holder.borrow_mut() = Some(pdf.clone());
            GridHost { pdf }
        });
        cx.simulate_resize(gpui::size(px(1500.), px(900.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let mut loaded = false;
        for _ in 0..600 {
            cx.run_until_parked();
            loaded = cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).is_loaded());
            if loaded {
                break;
            }
        }
        assert!(loaded, "PDF 应加载完成");
        enter_grid(&holder, cx);
        // 锚点源：Ctrl+单击页 0（单选 → 锚点=0）
        let anchor_click = cx.debug_bounds("pdf-grid-item-0").unwrap().center();
        cx.simulate_click(
            anchor_click,
            gpui::Modifiers {
                control: true,
                ..Default::default()
            },
        );
        for _ in 0..5 {
            let _ = cx.refresh();
            cx.run_until_parked();
        }
        assert_eq!(
            cx.update(|_w, cx| holder
                .borrow()
                .as_ref()
                .unwrap()
                .read(cx)
                .grid_selection_anchor()),
            Some(0),
            "Ctrl+单击后锚点应为页 0"
        );
        // Shift+单击页 2 → 闭区间 [0,2] 全部选中（含锚点与当前页）
        let range_click = cx.debug_bounds("pdf-grid-item-2").unwrap().center();
        cx.simulate_click(
            range_click,
            gpui::Modifiers {
                shift: true,
                ..Default::default()
            },
        );
        for _ in 0..5 {
            let _ = cx.refresh();
            cx.run_until_parked();
        }
        let (actual, anchor, mode) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (
                v.grid_selected.clone(),
                v.grid_selection_anchor(),
                v.grid_mode(),
            )
        });
        let expected: HashSet<usize> = [0usize, 1, 2].into_iter().collect();
        assert_eq!(actual, expected, "Shift 范围应闭区间选中 {{0,1,2}}");
        assert_eq!(anchor, Some(2), "Shift+单击后锚点应更新为当前页 2");
        assert!(mode, "Shift+单击不应退出网格");
    }

    /// wave5：工具栏「全选」→ 选中计数 == 总页数；「清空选择」→ 计数 0（真实点击驱动，
    /// 非直接调方法——验证按钮接线）。点击发生在可用态（-on 标记在场）。
    #[gpui::test]
    async fn grid_toolbar_select_all_and_clear(cx: &mut TestAppContext) {
        if test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _pdf_guard = test_util::lock().lock().unwrap();
        let pdf_path = test_util::test_pdf_paths()[1].clone();
        let holder: Rc<RefCell<Option<Entity<PdfViewer>>>> = Rc::default();
        let (_root, cx) = cx.add_window_view(|_window, cx| {
            let pdf = cx.new(|cx| PdfViewer::new(pdf_path.clone(), cx));
            *holder.borrow_mut() = Some(pdf.clone());
            GridHost { pdf }
        });
        cx.simulate_resize(gpui::size(px(1500.), px(900.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let mut loaded = false;
        for _ in 0..600 {
            cx.run_until_parked();
            loaded = cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).is_loaded());
            if loaded {
                break;
            }
        }
        assert!(loaded, "PDF 应加载完成");
        enter_grid(&holder, cx);
        let n = cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).dims.len());
        // 点「全选」→ 全部选中
        let sa = cx
            .debug_bounds("pdf-grid-tool-select-all-on")
            .expect("全选按钮应在场")
            .center();
        cx.simulate_click(sa, gpui::Modifiers::default());
        for _ in 0..5 {
            let _ = cx.refresh();
            cx.run_until_parked();
        }
        let count = cx.update(|_w, cx| {
            holder
                .borrow()
                .as_ref()
                .unwrap()
                .read(cx)
                .grid_selected_count()
        });
        assert_eq!(count, n, "全选应选中全部页 N={n}");
        // 点「清空选择」→ 计数 0
        let clr = cx
            .debug_bounds("pdf-grid-tool-clear-on")
            .expect("清空选择按钮应在场")
            .center();
        cx.simulate_click(clr, gpui::Modifiers::default());
        for _ in 0..5 {
            let _ = cx.refresh();
            cx.run_until_parked();
        }
        let count = cx.update(|_w, cx| {
            holder
                .borrow()
                .as_ref()
                .unwrap()
                .read(cx)
                .grid_selected_count()
        });
        assert_eq!(count, 0, "清空选择后应为 0");
    }

    /// wave5：工具栏「退出网格」→ `grid_mode=false` 且选中集/锚点清空（一次会话语义）。
    #[gpui::test]
    async fn grid_toolbar_exit_clears_selection(cx: &mut TestAppContext) {
        if test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _pdf_guard = test_util::lock().lock().unwrap();
        let pdf_path = test_util::test_pdf_paths()[1].clone();
        let holder: Rc<RefCell<Option<Entity<PdfViewer>>>> = Rc::default();
        let (_root, cx) = cx.add_window_view(|_window, cx| {
            let pdf = cx.new(|cx| PdfViewer::new(pdf_path.clone(), cx));
            *holder.borrow_mut() = Some(pdf.clone());
            GridHost { pdf }
        });
        cx.simulate_resize(gpui::size(px(1500.), px(900.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let mut loaded = false;
        for _ in 0..600 {
            cx.run_until_parked();
            loaded = cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).is_loaded());
            if loaded {
                break;
            }
        }
        assert!(loaded, "PDF 应加载完成");
        enter_grid(&holder, cx);
        // 先选中页 1（Ctrl）
        let sel1 = cx.debug_bounds("pdf-grid-item-1").unwrap().center();
        cx.simulate_click(
            sel1,
            gpui::Modifiers {
                control: true,
                ..Default::default()
            },
        );
        for _ in 0..5 {
            let _ = cx.refresh();
            cx.run_until_parked();
        }
        let (sel, count, mode) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (v.grid_selected(1), v.grid_selected_count(), v.grid_mode())
        });
        assert!(sel && count == 1 && mode, "前置：应处于选中 1 页的网格态");
        // 工具栏「退出网格」→ 退出且清选
        let exit = cx
            .debug_bounds("pdf-grid-tool-exit-on")
            .expect("退出网格按钮应在场")
            .center();
        cx.simulate_click(exit, gpui::Modifiers::default());
        for _ in 0..5 {
            let _ = cx.refresh();
            cx.run_until_parked();
        }
        let (mode, count, anchor) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (
                v.grid_mode(),
                v.grid_selected_count(),
                v.grid_selection_anchor(),
            )
        });
        assert!(!mode, "工具栏退出网格应生效");
        assert_eq!(count, 0, "退出网格应清空选中集");
        assert_eq!(anchor, None, "退出网格应清空 Shift 锚点");
    }

    /// wave5：**双击**卡退出（跳页语义迁到双击后）+ 退出后选中集清空；再进入网格选中集仍空。
    /// 与 `grid_double_click_card_exits_and_jumps_to_page` 互补：后者断「双击退出 + 跳页」，
    /// 本条断「退出路径同样清选」（双击卡与 toggle 都经 `set_grid_mode(false)`）。
    #[gpui::test]
    async fn grid_click_card_exit_clears_selection(cx: &mut TestAppContext) {
        if test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _pdf_guard = test_util::lock().lock().unwrap();
        let pdf_path = test_util::test_pdf_paths()[1].clone();
        let holder: Rc<RefCell<Option<Entity<PdfViewer>>>> = Rc::default();
        let (_root, cx) = cx.add_window_view(|_window, cx| {
            let pdf = cx.new(|cx| PdfViewer::new(pdf_path.clone(), cx));
            *holder.borrow_mut() = Some(pdf.clone());
            GridHost { pdf }
        });
        cx.simulate_resize(gpui::size(px(1500.), px(900.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let mut loaded = false;
        for _ in 0..600 {
            cx.run_until_parked();
            loaded = cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).is_loaded());
            if loaded {
                break;
            }
        }
        assert!(loaded, "PDF 应加载完成");
        enter_grid(&holder, cx);
        // 先选中页 0（Ctrl，带内可点）；随后**双击**卡 1 退出
        let sel0 = cx.debug_bounds("pdf-grid-item-0").unwrap().center();
        cx.simulate_click(
            sel0,
            gpui::Modifiers {
                control: true,
                ..Default::default()
            },
        );
        for _ in 0..5 {
            let _ = cx.refresh();
            cx.run_until_parked();
        }
        // 双击卡 1 中心（checkbox 在右上角不干扰）→ 退出网格
        let plain1 = cx.debug_bounds("pdf-grid-item-1").unwrap().center();
        simulate_grid_double_click(cx, plain1);
        for _ in 0..5 {
            let _ = cx.refresh();
            cx.run_until_parked();
        }
        let (mode, count) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (v.grid_mode(), v.grid_selected_count())
        });
        assert!(!mode, "双击卡应退出网格");
        assert_eq!(count, 0, "双击卡退出路径同样清空选中集");
        // 再进入网格：仍为空（一次会话语义）
        enter_grid(&holder, cx);
        let count = cx.update(|_w, cx| {
            holder
                .borrow()
                .as_ref()
                .unwrap()
                .read(cx)
                .grid_selected_count()
        });
        assert_eq!(count, 0, "再进入网格应从头开始（选中集空）");
    }

    /// wave5：工具栏页操作按钮 UI 呈现——无选中时 删除/拆分/提取/合并 **禁用**（`-off`
    /// 标记在场、`-on` 不在），有选中时含计数变为**可用**（`-on` 在场）。只断 UI 呈现，
    /// 不断 handler 行为（wave8 才接线）；计数 badge 文案由 `grid_selected_count` 派生。
    #[gpui::test]
    async fn grid_toolbar_action_buttons_state(cx: &mut TestAppContext) {
        if test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _pdf_guard = test_util::lock().lock().unwrap();
        let pdf_path = test_util::test_pdf_paths()[1].clone();
        let holder: Rc<RefCell<Option<Entity<PdfViewer>>>> = Rc::default();
        let (_root, cx) = cx.add_window_view(|_window, cx| {
            let pdf = cx.new(|cx| PdfViewer::new(pdf_path.clone(), cx));
            *holder.borrow_mut() = Some(pdf.clone());
            GridHost { pdf }
        });
        cx.simulate_resize(gpui::size(px(1500.), px(900.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let mut loaded = false;
        for _ in 0..600 {
            cx.run_until_parked();
            loaded = cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).is_loaded());
            if loaded {
                break;
            }
        }
        assert!(loaded, "PDF 应加载完成");
        enter_grid(&holder, cx);
        assert!(
            cx.debug_bounds("pdf-grid-toolbar").is_some(),
            "网格顶栏应在场"
        );
        // 无选中：全部禁用（off 在场、on 不在）
        for (off, on) in [
            ("pdf-grid-tool-delete-off", "pdf-grid-tool-delete-on"),
            ("pdf-grid-tool-split-off", "pdf-grid-tool-split-on"),
            ("pdf-grid-tool-extract-off", "pdf-grid-tool-extract-on"),
            ("pdf-grid-tool-merge-off", "pdf-grid-tool-merge-on"),
        ] {
            assert!(cx.debug_bounds(off).is_some(), "{off} 应在场（无选中禁用）");
            assert!(cx.debug_bounds(on).is_none(), "{on} 不应对场（无选中禁用）");
        }
        // Ctrl+单击页 0 → 有选中
        let sel0 = cx.debug_bounds("pdf-grid-item-0").unwrap().center();
        cx.simulate_click(
            sel0,
            gpui::Modifiers {
                control: true,
                ..Default::default()
            },
        );
        for _ in 0..5 {
            let _ = cx.refresh();
            cx.run_until_parked();
        }
        // 有选中：全部可用（on 在场、off 不在）；计数 badge 由计数派生
        for (off, on) in [
            ("pdf-grid-tool-delete-off", "pdf-grid-tool-delete-on"),
            ("pdf-grid-tool-split-off", "pdf-grid-tool-split-on"),
            ("pdf-grid-tool-extract-off", "pdf-grid-tool-extract-on"),
            ("pdf-grid-tool-merge-off", "pdf-grid-tool-merge-on"),
        ] {
            assert!(
                cx.debug_bounds(off).is_none(),
                "{off} 不应在场（有选中可用）"
            );
            assert!(cx.debug_bounds(on).is_some(), "{on} 应在场（有选中可用）");
        }
    }

    /// wave8：四操作按钮的 **handler 存在性静态核验**——有选中时逐个 `simulate_click`，
    /// 断言 listener 已挂载、点击不 panic、模型不变（headless 无法真机弹对话框/文件框，
    /// 测试构建下手 handler 是 no-op 桩；真实行为由 `grid_apply_*` seam 测试覆盖——把
    /// 「接线存在」与「动作正确」分层验证，wave5 的 `grid_toolbar_action_buttons_state`
    /// 只断 UI 呈现、本测试补「点击确实命中 handler」）。
    #[gpui::test]
    async fn grid_toolbar_action_buttons_handler_wired(cx: &mut TestAppContext) {
        if test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _pdf_guard = test_util::lock().lock().unwrap();
        let holder: Rc<RefCell<Option<Entity<PdfViewer>>>> = Rc::default();
        let (_root, cx) = open_grid_host_fixture(&holder, cx, 0);
        let n = cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).dims.len());
        assert!(n >= 2, "夹具页数需 ≥2，实际 {n}");
        // 勾选两页 → 四按钮全部可用（on）。
        let sel0 = cx.debug_bounds("pdf-grid-item-0").unwrap().center();
        cx.simulate_click(
            sel0,
            gpui::Modifiers {
                control: true,
                ..Default::default()
            },
        );
        let sel1 = cx.debug_bounds("pdf-grid-item-1").unwrap().center();
        cx.simulate_click(
            sel1,
            gpui::Modifiers {
                control: true,
                ..Default::default()
            },
        );
        for _ in 0..5 {
            let _ = cx.refresh();
            cx.run_until_parked();
        }
        // 逐个点击可用操作按钮：handler 命中（挂载 + 不 panic）；测试桩不改模型。
        for selector in [
            "pdf-grid-tool-delete-on",
            "pdf-grid-tool-split-on",
            "pdf-grid-tool-extract-on",
            "pdf-grid-tool-merge-on",
        ] {
            let c = cx.debug_bounds(selector).unwrap().center();
            cx.simulate_click(c, gpui::Modifiers::default());
            for _ in 0..3 {
                let _ = cx.refresh();
                cx.run_until_parked();
            }
            let (count, mode) = cx.update(|_w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                (v.dims.len(), v.grid_mode())
            });
            assert_eq!(count, n, "点击 {selector}（测试桩）不应改变页数");
            assert!(mode, "点击 {selector}（测试桩）不应退出网格");
        }
    }

    // ---- wave6：网格拖拽重组——纯函数手算用例（K177：期望值先演算后落）----

    /// `compute_insert_index` 手算用例：6 页 A4、vw=1500 → cols=5、cell_w=280（新语义跟随
    /// 请求卡宽，不再按填满反算 284）、cell_h=280×792/612≈362.353、gap=12（纵向行距）、
    /// padding=16、col_gap=17（C=1468、needed=(1468−5·280)/4=17 ≤ 36 不封顶 → 填满、left_pad=0）→
    /// pitch=280+17=297、cell_rect x = 16 + c·297（卡 0=[16,296]、卡 1=313、卡 4=1204）、
    /// row_top=[16, 16+362.353+12≈390.353]、total≈16+362.353+12+362.353+16≈768.706。
    /// 期望插入位全部手写算式推导。
    #[gpui::test]
    fn compute_insert_index_hand_cases(_cx: &mut TestAppContext) {
        let dims: Vec<(f32, f32)> = vec![(612.0, 792.0); 6];
        let layout = grid_layout(&dims, 280.0, 12.0, 16.0, 1500.0);
        assert_eq!(layout.cols, 5, "6 页 1500 宽应 5 列");
        let (x0, y0, w0, h0) = layout.cell_rect(0).unwrap();
        let (x1, _, _, _) = layout.cell_rect(1).unwrap();
        let (x4, _, _, _) = layout.cell_rect(4).unwrap();
        let (_, y5, _, _) = layout.cell_rect(5).unwrap();
        // 卡 0 左上角（x 取左缘+4、y 取卡 0 内）：插到最前 → 线性位 0
        assert_eq!(
            compute_insert_index(&layout, 1500.0, x0 + 4.0, y0 + 4.0),
            Some(0),
            "卡 0 左上角 → 线性位 0（最前）"
        );
        // 卡 1 右半：x = 313 + 0.8·280 = 537 → round((537−16)/297) = round(521/297) ≈ round(1.7542)
        //   = 2 → 线性位 2（卡间）
        assert_eq!(
            compute_insert_index(&layout, 1500.0, x1 + w0 * 0.8, y0 + 2.0),
            Some(2),
            "卡 1 右半 → 线性位 2（卡 1 之后 / 卡 2 之前）"
        );
        // 卡 4（行 0 末卡）右半：x = 1204 + 224 = 1428 → round((1428−16)/297) = round(1412/297) ≈
        //   round(4.7542) = 5 → min(row_len=5) → 线性位 5
        assert_eq!(
            compute_insert_index(&layout, 1500.0, x4 + w0 * 0.8, y0 + 2.0),
            Some(5),
            "行 0 末卡右半 → 线性位 5（行 1 首）"
        );
        // 行 1（卡 5）左半：行 r=1、col_slot=round((x−16)/297)=0 → base=row_page_start[1]=5 → 5
        assert_eq!(
            compute_insert_index(&layout, 1500.0, x0 + 4.0, y5 + 4.0),
            Some(5),
            "卡 5 左半 → 线性位 5（行 1 首前）"
        );
        // 行间垂直 gap（y = 行 0 底与行 1 顶之中点）：r=0、y ≥ row_top[1]−gap →
        //   直接 = row_page_start[1] = 5（本行末 = 下一行首）
        let gap_y = (y0 + h0 + layout.row_top[1]) / 2.0;
        assert_eq!(
            compute_insert_index(&layout, 1500.0, x0 + 4.0, gap_y),
            Some(5),
            "行间 gap → 线性位 5（行 1 首）"
        );
        // 顶 padding → 0；底 padding（y ≥ total − padding = 末行底）→ n=6
        assert_eq!(
            compute_insert_index(&layout, 1500.0, 500.0, 5.0),
            Some(0),
            "顶部 padding → 线性位 0"
        );
        assert_eq!(
            compute_insert_index(&layout, 1500.0, 500.0, layout.total_height - 5.0),
            Some(6),
            "底部 padding → 线性位 n=6（末尾）"
        );
        // 网格外：x 超出 vw / y 超出 total_height → None（松手即放弃）
        assert_eq!(
            compute_insert_index(&layout, 1500.0, 1505.0, y0 + 4.0),
            None,
            "x 超出内容宽 → None"
        );
        assert_eq!(
            compute_insert_index(&layout, 1500.0, x0 + 4.0, layout.total_height + 5.0),
            None,
            "y 超出总高 → None"
        );
    }

    /// `apply_reorder` 手算用例：语义 = 移除被拖组（升序原下标）整体插到「原序线性位
    /// insert 所指槽」；插入位（rest 内下标）`pos = insert − count(dragging < insert)`。
    #[gpui::test]
    fn apply_reorder_hand_cases(_cx: &mut TestAppContext) {
        // 单卡前移：D=[0]、insert=3 → rest=[1,2,3,4,5]、pos=3−1=2 → [1,2,0,3,4,5]
        assert_eq!(apply_reorder(6, &[0], 3), vec![1, 2, 0, 3, 4, 5]);
        // 单卡后移到末尾：D=[1]、insert=6 → rest=[0,2,3,4,5]、pos=6−1=5 → [0,2,3,4,5,1]
        assert_eq!(apply_reorder(6, &[1], 6), vec![0, 2, 3, 4, 5, 1]);
        // 连续块后移：D=[1,2]、insert=5 → rest=[0,3,4,5]、pos=5−2=3 → [0,3,4,1,2,5]
        //   （块「1,2」移到原序位 5 所指槽 = 页 5 之前）
        assert_eq!(apply_reorder(6, &[1, 2], 5), vec![0, 3, 4, 1, 2, 5]);
        // 拖回原位（insert ∈ 块自身跨度）→ 序不变：D=[1,2]、insert=2 → rest=[0,3,4,5]、
        //   pos=2−2=0 → [1,2,0,3,4,5] = 原序（块又插回自身原位置，连续块 = 不变）
        assert_eq!(apply_reorder(6, &[1, 2], 2), vec![0, 1, 2, 3, 4, 5]);
        // 非连续组：D=[1,3]、insert=0（最前）→ rest=[0,2,4,5]、pos=0 → [1,3,0,2,4,5]
        assert_eq!(apply_reorder(6, &[1, 3], 0), vec![1, 3, 0, 2, 4, 5]);
        // 越界 insert 夹到 n：D=[0]、insert=99 → 等价 insert=6 → [1,2,3,4,5,0]
        assert_eq!(apply_reorder(6, &[0], 99), vec![1, 2, 3, 4, 5, 0]);
        // 空组 → 恒等；全选 → 恒等（整体平移）
        assert_eq!(apply_reorder(4, &[], 2), vec![0, 1, 2, 3]);
        assert_eq!(
            apply_reorder(4, &[0, 1, 2, 3], 2),
            vec![0, 1, 2, 3],
            "全选拖拽 = 整体平移 = 序不变"
        );
    }

    // ---- wave6：网格拖拽重组——交互集成（真实鼠标事件驱动）----

    /// 等待模型条件成立（最多 600 帧；加载/重排后的 reload 均以 `is_loaded` + dims 收敛）。
    fn wait_grid_until(
        holder: &Rc<RefCell<Option<Entity<PdfViewer>>>>,
        cx: &mut VisualTestContext,
        mut cond: impl FnMut(&PdfViewer) -> bool,
    ) {
        for _ in 0..600 {
            if cx.update(|_w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                cond(v)
            }) {
                return;
            }
            let _ = cx.refresh();
            cx.run_until_parked();
        }
        panic!("等待 600 帧内模型条件未满足（重排未落地？）");
    }

    /// 网格拖拽测试公共开窗：`GridHost` 整窗渲染 + 等待加载 + 进网格。
    fn open_grid_host<'a>(
        holder: &Rc<RefCell<Option<Entity<PdfViewer>>>>,
        cx: &'a mut TestAppContext,
    ) -> (gpui::Entity<GridHost>, &'a mut VisualTestContext) {
        open_grid_host_fixture(holder, cx, 1)
    }

    /// 按夹具索引开网格宿主（wave8 seam 测试用 `fixture_idx=0` 的文本型 pdf测试.pdf 控探针
    /// 渲染成本；拖拽测试仍默认索引 1 的长文多页）。其余与 `open_grid_host` 完全同构。
    fn open_grid_host_fixture<'a>(
        holder: &Rc<RefCell<Option<Entity<PdfViewer>>>>,
        cx: &'a mut TestAppContext,
        fixture_idx: usize,
    ) -> (gpui::Entity<GridHost>, &'a mut VisualTestContext) {
        let pdf_path = test_util::test_pdf_paths()[fixture_idx].clone();
        let (root, vcx) = cx.add_window_view(|_window, cx| {
            let pdf = cx.new(|cx| PdfViewer::new(pdf_path.clone(), cx));
            holder.borrow_mut().replace(pdf.clone());
            GridHost { pdf }
        });
        vcx.simulate_resize(gpui::size(px(1500.), px(900.)));
        for _ in 0..3 {
            vcx.run_until_parked();
        }
        let mut loaded = false;
        for _ in 0..600 {
            vcx.run_until_parked();
            loaded = vcx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).is_loaded());
            if loaded {
                break;
            }
        }
        assert!(loaded, "PDF 应加载完成");
        enter_grid(holder, vcx);
        (root, vcx)
    }

    /// wave6：拖已选卡（Ctrl 预选）→ 位移超阈值进入拖拽 → drop 到行 1 卡 5 前 →
    /// 调 `reorder_pages`（模型尺寸序列 = `apply_reorder` 预期、页数不变）；整链**不退出**
    /// 网格（拖拽与点卡跳页互斥）；重排成功后 ghost/指示线撤下、选中清空。
    #[gpui::test]
    async fn grid_drag_drop_reorders_page_order(cx: &mut TestAppContext) {
        if test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _pdf_guard = test_util::lock().lock().unwrap();
        let holder: Rc<RefCell<Option<Entity<PdfViewer>>>> = Rc::default();
        let (_root, mut cx) = open_grid_host(&holder, cx);
        let n = cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).dims.len());
        assert!(n > 8, "夹具页数需 >8，实际 {n}");
        // Ctrl+单击卡 0 预选（wave5 路径，此刻仍是网格内）
        let p0 = cx.debug_bounds("pdf-grid-item-0").unwrap().center();
        cx.simulate_click(
            p0,
            gpui::Modifiers {
                control: true,
                ..Default::default()
            },
        );
        for _ in 0..5 {
            let _ = cx.refresh();
            cx.run_until_parked();
        }
        assert_eq!(
            cx.update(|_w, cx| holder
                .borrow()
                .as_ref()
                .unwrap()
                .read(cx)
                .grid_selected_count()),
            1,
            "前置：应选择页 0"
        );
        // 拖拽：按下卡 0 → 位移 6px（>2px 阈值）建立拖拽 → 移动指针到卡 5 中心
        // （行 1 左半 → 线性位 5）→ 松手提交。
        let target = cx.debug_bounds("pdf-grid-item-5").unwrap().center();
        cx.simulate_mouse_down(p0, MouseButton::Left, gpui::Modifiers::default());
        cx.simulate_mouse_move(
            gpui::point(p0.x + px(0.), p0.y + px(6.)),
            Some(MouseButton::Left),
            gpui::Modifiers::default(),
        );
        for _ in 0..3 {
            let _ = cx.refresh();
            cx.run_until_parked();
        }
        // 拖拽建立：ghost 在场；模型已进入拖拽态（且未触发跳页/仍网格）。插入指示线须
        // 待首个 on_drag_move（指针进网格算出 insert）后才在场——此处移动尚为阈值越界，
        // insert 仍 None，故不 assert 指示线（见下方目标移动后断言）。
        assert!(
            cx.debug_bounds("pdf-grid-ghost").is_some(),
            "拖拽中 ghost 应在场"
        );
        let dragging = cx.update(|_w, cx| {
            holder
                .borrow()
                .as_ref()
                .unwrap()
                .read(cx)
                .grid_drag
                .as_ref()
                .map(|d| d.dragging.clone())
        });
        assert_eq!(dragging, Some(vec![0]), "拖已选卡 0 被拖组应为 [0]");
        assert!(
            cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).grid_mode()),
            "拖拽中不应退出网格（拖拽/点卡互斥）"
        );
        // 移动到目标（卡 5 中心）→ 插入位 5；插入指示线随之在场；再松手提交
        cx.simulate_mouse_move(target, Some(MouseButton::Left), gpui::Modifiers::default());
        for _ in 0..3 {
            let _ = cx.refresh();
            cx.run_until_parked();
        }
        assert!(
            cx.debug_bounds("pdf-grid-reorder-line").is_some(),
            "指针进入网格（insert 已算）后插入指示线应在场"
        );
        let insert = cx.update(|_w, cx| {
            holder
                .borrow()
                .as_ref()
                .unwrap()
                .read(cx)
                .grid_drag
                .as_ref()
                .and_then(|d| d.insert)
        });
        assert_eq!(insert, Some(5), "指针在行 1 卡 5 左半 → 插入线性位 5");
        cx.simulate_mouse_up(target, MouseButton::Left, gpui::Modifiers::default());
        for _ in 0..3 {
            let _ = cx.refresh();
            cx.run_until_parked();
        }
        // 期望：原尺寸序列做 apply_reorder(n, [0], 5) → 页 0 移到页 5 前。
        let orig_dims = cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).dims.clone());
        let expect: Vec<(f32, f32)> = {
            let order = apply_reorder(n, &[0], 5);
            assert_eq!(order.len(), n, "重排应返回全排列");
            order.iter().map(|&i| orig_dims[i]).collect()
        };
        wait_grid_until(&holder, &mut cx, move |v| {
            v.is_loaded() && v.dims.len() == n && v.dims == expect
        });
        // 重排后：页数不变、仍网格（拖拽不跳页）、选中已清（reload 语义）、拖拽态已清。
        let (count, mode, drag_state) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (v.dims.len(), v.grid_mode(), v.grid_drag.is_some())
        });
        assert_eq!(count, n, "重排后页数不变");
        assert!(mode, "拖拽重排后应保持网格模式（不跳页）");
        assert!(!drag_state, "drop 后拖拽态应清");
        let sel = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            v.grid_selected_count()
        });
        assert_eq!(sel, 0, "重排成功经 reload 清选中（wave5 契约）");
    }

    /// wave6：拖**未选**卡 → 先清空既有选择改为单卡选中（整组拖 = 单卡）；drop 提交
    /// 该单卡的重排。顺带断言拖拽中不触发勾选（原选中集被替换为单卡，非叠加）。
    #[gpui::test]
    async fn grid_drag_unselected_card_replaces_selection_then_drags(cx: &mut TestAppContext) {
        if test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _pdf_guard = test_util::lock().lock().unwrap();
        let holder: Rc<RefCell<Option<Entity<PdfViewer>>>> = Rc::default();
        let (_root, mut cx) = open_grid_host(&holder, cx);
        let n = cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).dims.len());
        // 先选中页 0（Ctrl），再拖**未选**的页 3 → 应替换为 {3}
        let s0 = cx.debug_bounds("pdf-grid-item-0").unwrap().center();
        cx.simulate_click(
            s0,
            gpui::Modifiers {
                control: true,
                ..Default::default()
            },
        );
        for _ in 0..5 {
            let _ = cx.refresh();
            cx.run_until_parked();
        }
        let p3 = cx.debug_bounds("pdf-grid-item-3").unwrap().center();
        cx.simulate_mouse_down(p3, MouseButton::Left, gpui::Modifiers::default());
        cx.simulate_mouse_move(
            gpui::point(p3.x + px(0.), p3.y + px(6.)),
            Some(MouseButton::Left),
            gpui::Modifiers::default(),
        );
        for _ in 0..3 {
            let _ = cx.refresh();
            cx.run_until_parked();
        }
        let (dragging, sel) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (
                v.grid_drag.as_ref().map(|d| d.dragging.clone()),
                v.grid_selected.clone(),
            )
        });
        assert_eq!(dragging, Some(vec![3]), "拖未选卡被拖组应为 [3]");
        let expect_sel: HashSet<usize> = HashSet::from([3usize]);
        assert_eq!(sel, expect_sel, "未选卡开拖 → 选中集替换为单卡 [3]");
        // 拉到卡 5 前提交 → 期望 = apply_reorder(n, [3], 5)。
        let target = cx.debug_bounds("pdf-grid-item-5").unwrap().center();
        cx.simulate_mouse_move(target, Some(MouseButton::Left), gpui::Modifiers::default());
        for _ in 0..3 {
            let _ = cx.refresh();
            cx.run_until_parked();
        }
        cx.simulate_mouse_up(target, MouseButton::Left, gpui::Modifiers::default());
        let orig_dims = cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).dims.clone());
        let expect: Vec<(f32, f32)> = {
            let order = apply_reorder(n, &[3], 5);
            order.iter().map(|&i| orig_dims[i]).collect()
        };
        wait_grid_until(&holder, &mut cx, move |v| {
            v.is_loaded() && v.dims.len() == n && v.dims == expect
        });
        let mode = cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).grid_mode());
        assert!(mode, "拖未选卡重排后仍应网格模式");
    }

    /// wave6：drop 在网格外（指针移出内容容器，如工具条区域）→ 不提交（模型维度不变）、
    /// 选中保持原样、拖拽态清。
    #[gpui::test]
    async fn grid_drag_drop_outside_abandons_keeping_selection(cx: &mut TestAppContext) {
        if test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _pdf_guard = test_util::lock().lock().unwrap();
        let holder: Rc<RefCell<Option<Entity<PdfViewer>>>> = Rc::default();
        let (_root, cx) = open_grid_host(&holder, cx);
        let n = cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).dims.len());
        // 预选 {0}
        let s0 = cx.debug_bounds("pdf-grid-item-0").unwrap().center();
        cx.simulate_click(
            s0,
            gpui::Modifiers {
                control: true,
                ..Default::default()
            },
        );
        for _ in 0..5 {
            let _ = cx.refresh();
            cx.run_until_parked();
        }
        // 拖起卡 0，移到网格内容之外（工具条区内，内容坐标 y<0 → insert None），松手。
        cx.simulate_mouse_down(s0, MouseButton::Left, gpui::Modifiers::default());
        cx.simulate_mouse_move(
            gpui::point(s0.x + px(0.), s0.y + px(6.)),
            Some(MouseButton::Left),
            gpui::Modifiers::default(),
        );
        for _ in 0..3 {
            let _ = cx.refresh();
            cx.run_until_parked();
        }
        let outside = gpui::point(px(400.), px(10.)); // 工具条区：在网格根内、内容容器外
        cx.simulate_mouse_move(outside, Some(MouseButton::Left), gpui::Modifiers::default());
        for _ in 0..3 {
            let _ = cx.refresh();
            cx.run_until_parked();
        }
        let insert = cx.update(|_w, cx| {
            holder
                .borrow()
                .as_ref()
                .unwrap()
                .read(cx)
                .grid_drag
                .as_ref()
                .and_then(|d| d.insert)
        });
        assert_eq!(insert, None, "指针移出内容容器 → 插入位 None");
        cx.simulate_mouse_up(outside, MouseButton::Left, gpui::Modifiers::default());
        for _ in 0..5 {
            let _ = cx.refresh();
            cx.run_until_parked();
        }
        let (count, sel, mode, drag_some) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (
                v.dims.len(),
                v.grid_selected_count(),
                v.grid_mode(),
                v.grid_drag.is_some(),
            )
        });
        assert_eq!(count, n, "drop 网格外不应改变模型（页数不变）");
        assert_eq!(sel, 1, "drop 网格外应保持选中（不提交不清选）");
        assert!(mode, "drop 网格外应在网格模式（不跳页）");
        assert!(!drag_some, "drop 后拖拽态应清");
    }

    // ---- wave8：网格工具栏页操作 seam——「确认框 / 文件选择之后」的可测锚点 ----
    // 按钮 handler 只做「确认/弹框 → 调 seam」的接线（headless 无法真机弹对话框/文件框），
    // 故行为断言全部落在 `grid_apply_*` seam 上（此层已含真实 view 状态副作用：保持网格、
    // 清选中、页操作异步链路），与 wave7 的 fixture/oracle 姿势一致（K177：期望值先演算）。

    /// 页操作 seam 异步链路等待：带时钟前进 + 真实睡眠（页操作后台 round-trip 与新镜象
    /// `open_from_bytes` 解析在后台执行器，快循环连抽空帧会错过完成窗口——对齐既有
    /// thumb-wait 的 advance_clock + sleep 姿势）。
    fn wait_seam_until(
        holder: &Rc<RefCell<Option<Entity<PdfViewer>>>>,
        cx: &mut VisualTestContext,
        mut cond: impl FnMut(&PdfViewer) -> bool,
    ) {
        for _ in 0..800 {
            if cx.update(|_w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                cond(v)
            }) {
                return;
            }
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        panic!("等待 800 轮内模型条件未满足（页操作/重载未落地？）");
    }

    /// 渲染第 `page` 页（scale=1.0 全密度）并返回 (宽, 高, FNV-1a 内容哈希)——内容 oracle，
    /// 与 pdf_core `render_page_probe` 同口径。页操作成功重载后 viewer 的 worker/doc 已被
    /// `open_from_bytes` **替换**（新 doc 新句柄）→ 调用方须在 op 完成后重读 `v.worker`/
    /// `v.doc_id` 再探（K37：效果断言须落在重载后的真实文档上）。
    fn page_probe(worker: &PdfWorker, doc_id: DocId, page: usize) -> (u32, u32, u64) {
        let rx = worker.request_render(doc_id, RenderRequest { page, scale: 1.0 }, 0, 0);
        let r = rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("应收到渲染结果")
            .expect("渲染应成功");
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for &b in &r.bgra {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        (r.width, r.height, h)
    }

    /// 对当前 viewer 的 worker/doc 探一页渲染哈希。
    fn live_page_probe(
        holder: &Rc<RefCell<Option<Entity<PdfViewer>>>>,
        cx: &mut VisualTestContext,
        page: usize,
    ) -> (u32, u32, u64) {
        let (worker, doc_id) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (v.worker.clone(), v.doc_id)
        });
        page_probe(&worker.expect("文档已加载应有 worker"), doc_id, page)
    }

    /// 网格工具栏 seam 测试的临时输出路径（pid + 序号唯一，落系统 temp，勿污染仓库）。
    fn grid_tool_output(name: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("vpic-grid-{name}-{}-{seq}.pdf", std::process::id()))
    }

    /// wave8：删除 seam `grid_apply_delete` —— 整批删 {1,3}：页数 n-2、幸存尺寸序列 =
    /// 原剔除 {1,3}、残留渲染哈希（首/次/末）对齐原页、成功重载后仍网格模式且选中清空。
    #[gpui::test]
    async fn grid_apply_delete_removes_pages_and_keeps_grid(cx: &mut TestAppContext) {
        if test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _pdf_guard = test_util::lock().lock().unwrap();
        let holder: Rc<RefCell<Option<Entity<PdfViewer>>>> = Rc::default();
        let (_root, mut cx) = open_grid_host_fixture(&holder, cx, 0);
        let (n, orig_dims) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (v.dims.len(), v.dims.clone())
        });
        assert!(n >= 4, "夹具页数需 ≥4，实际 {n}");
        // 算式（K177）：删除 {1,3}（k=2）→ 剩 n-2 页；幸存原序 = 0,2,4..n-1。
        // 原图 oracle：幸存页 0 / 2 / 末页 n-1 在**原 worker/doc** 上探渲染哈希。
        let orig0 = live_page_probe(&holder, &mut cx, 0);
        let orig2 = live_page_probe(&holder, &mut cx, 2);
        let orig_last = live_page_probe(&holder, &mut cx, n - 1);
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, cx| {
                v.grid_apply_delete(vec![1, 3], cx);
            });
        });
        wait_seam_until(&holder, &mut cx, move |v| {
            v.is_loaded() && v.dims.len() == n - 2
        });
        let expect_dims: Vec<(f32, f32)> = orig_dims
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != 1 && *i != 3)
            .map(|(_, d)| *d)
            .collect();
        let (count, mode, dims_after, sel) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (
                v.dims.len(),
                v.grid_mode(),
                v.dims.clone(),
                v.grid_selected_count(),
            )
        });
        assert_eq!(count, n - 2, "删除后页数 = n-2");
        assert_eq!(dims_after, expect_dims, "幸存尺寸序列 = 原剔除 {{1,3}}");
        assert!(mode, "删除重载后应保持网格模式（grid_hold_on_reload 姿势）");
        assert_eq!(sel, 0, "删除成功经重载清选中");
        // 残留渲染哈希：新文档 0/1/末页 = 原页 0/2/n-1（删页重排后内容不搬动）。
        let new0 = live_page_probe(&holder, &mut cx, 0);
        let new1 = live_page_probe(&holder, &mut cx, 1);
        let new_last = live_page_probe(&holder, &mut cx, n - 3);
        assert_eq!(new0, orig0, "残留首页渲染 = 原页 0");
        assert_eq!(new1, orig2, "残留次要页渲染 = 原页 2");
        assert_eq!(new_last, orig_last, "残留末页渲染 = 原末页 n-1");
    }

    /// wave8：拆分 seam `grid_apply_split` —— 拆 [1,3]：新文件 3 页（尺寸/渲染对齐源区间
    /// 原序）、当前 n-3 页（幸存序）、保持网格模式；`undo_last` 可撤销回 n 页（新文件仍在）。
    #[gpui::test]
    async fn grid_apply_split_writes_range_trims_and_undo(cx: &mut TestAppContext) {
        if test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _pdf_guard = test_util::lock().lock().unwrap();
        let holder: Rc<RefCell<Option<Entity<PdfViewer>>>> = Rc::default();
        let (_root, mut cx) = open_grid_host_fixture(&holder, cx, 0);
        let (n, orig_dims) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (v.dims.len(), v.dims.clone())
        });
        assert!(n >= 4, "夹具页数需 ≥4，实际 {n}");
        // 原图 oracle：区间页 1..3 + 幸存首页 0。
        let orig0 = live_page_probe(&holder, &mut cx, 0);
        let orig1 = live_page_probe(&holder, &mut cx, 1);
        let orig2 = live_page_probe(&holder, &mut cx, 2);
        let orig3 = live_page_probe(&holder, &mut cx, 3);
        let out = grid_tool_output("split-range");
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, cx| {
                v.grid_apply_split((1, 3), out.clone(), cx);
            });
        });
        // 算式（K177）：区间 [1,3] 长 = 3-1+1 = 3 → 新文件 3 页、当前剩余 n-3 页。
        wait_seam_until(&holder, &mut cx, move |v| {
            v.is_loaded() && v.dims.len() == n - 3
        });
        assert!(
            out.exists(),
            "拆分应先写新文件再移除区间: {}",
            out.display()
        );
        // ① 新文件：页数 3、尺寸 = 原 dims[1..=3]、渲染逐页 = 原页 1..3。
        let new_bytes = std::fs::read(&out).expect("新文件应可读");
        let (w_new, d_new, dims_new) = PdfWorker::open_and_dims(new_bytes).expect("新文件应可打开");
        assert_eq!(
            dims_new,
            orig_dims[1..4].to_vec(),
            "新文件尺寸 = 源区间原序"
        );
        assert_eq!(
            (0..3)
                .map(|i| page_probe(&w_new, d_new, i))
                .collect::<Vec<_>>(),
            vec![orig1, orig2, orig3],
            "新文件逐页渲染 = 源区间页"
        );
        w_new.close(d_new);
        // ② 当前文档：n-3 页、幸存尺寸 = 原剔除 {1,2,3}、首页渲染仍 = 原页 0、保持网格。
        let expect_dims: Vec<(f32, f32)> = orig_dims
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != 1 && *i != 2 && *i != 3)
            .map(|(_, d)| *d)
            .collect();
        let (count, mode, dims_after, sel) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (
                v.dims.len(),
                v.grid_mode(),
                v.dims.clone(),
                v.grid_selected_count(),
            )
        });
        assert_eq!(count, n - 3, "拆分后当前页数 = n-3");
        assert_eq!(dims_after, expect_dims, "当前幸存尺寸 = 原剔除 {{1,2,3}}");
        assert!(mode, "拆分重载后应保持网格模式");
        assert_eq!(sel, 0, "拆分成功经重载清选中");
        // 当前首页渲染 = 原页 0（在重载后的新 worker/doc 上探）。
        let cur0_probe = live_page_probe(&holder, &mut cx, 0);
        assert_eq!(cur0_probe, orig0, "当前首页渲染 = 原页 0");
        // ③ 可撤销：undo_last 回 n 页（新文件仍在）。
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, cx| {
                v.undo_last(cx);
            });
        });
        wait_seam_until(&holder, &mut cx, move |v| {
            v.is_loaded() && v.dims.len() == n
        });
        let restored_dims =
            cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).dims.clone());
        assert_eq!(restored_dims, orig_dims, "撤销后页尺寸序列还原");
        assert!(out.exists(), "撤销后新文件仍存在: {}", out.display());
        let _ = std::fs::remove_file(&out);
    }

    /// wave8：提取 seam `grid_apply_extract` —— 勾选 {0,2}（升序）：新文件 2 页（尺寸/渲染
    /// 对齐源页）、当前页数/尺寸/网格不变、选中同步清空（副本语义）。
    #[gpui::test]
    async fn grid_apply_extract_writes_checked_pages_current_unchanged(cx: &mut TestAppContext) {
        if test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _pdf_guard = test_util::lock().lock().unwrap();
        let holder: Rc<RefCell<Option<Entity<PdfViewer>>>> = Rc::default();
        let (_root, mut cx) = open_grid_host_fixture(&holder, cx, 0);
        let (n, orig_dims) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (v.dims.len(), v.dims.clone())
        });
        assert!(n >= 3, "夹具页数需 ≥3，实际 {n}");
        let orig0 = live_page_probe(&holder, &mut cx, 0);
        let orig2 = live_page_probe(&holder, &mut cx, 2);
        let out = grid_tool_output("extract");
        // 用**非排序**勾选快照 {2,0} 直接调 seam（HashSet 快照插入序本就不保证；按钮 handler
        // 经 `grid_extract_clicked` 归序，这里以非排序入参断言 seam `grid_apply_extract` **自身**
        // 归序后导出产物页序 = 勾选页升序 [0,2]）。
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, cx| {
                v.grid_selected.extend([2usize, 0]);
                v.grid_apply_extract(vec![2, 0], out.clone(), cx);
            });
        });
        // 副本语义：导出**异步成功**后选中集才消费清空（失败不清选可重试）→ 先等文件落盘
        // 成功，再断言选中清空。
        let out_clone = out.clone();
        wait_seam_until(&holder, &mut cx, move |_v| out_clone.exists());
        let (sel, mode) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (v.grid_selected_count(), v.grid_mode())
        });
        assert_eq!(sel, 0, "提取异步成功后选中清空（副本语义）");
        assert!(mode, "提取后仍在网格模式（不重载不退出）");
        // 新文件：2 页、尺寸 = [dims[0], dims[2]]（**升序**——非排序入参也须归序）、
        // 渲染 = [orig0, orig2]。
        let new_bytes = std::fs::read(&out).expect("新文件应可读");
        let (w_new, d_new, dims_new) = PdfWorker::open_and_dims(new_bytes).expect("新文件应可打开");
        assert_eq!(
            dims_new,
            vec![orig_dims[0], orig_dims[2]],
            "提取产物尺寸 = 勾选升序"
        );
        assert_eq!(
            (0..2)
                .map(|i| page_probe(&w_new, d_new, i))
                .collect::<Vec<_>>(),
            vec![orig0, orig2],
            "提取产物逐页渲染 = 源勾选页"
        );
        w_new.close(d_new);
        // 当前不变：页数/尺寸原样。
        let (count, dims_after) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (v.dims.len(), v.dims.clone())
        });
        assert_eq!(count, n, "提取后当前页数不变");
        assert_eq!(dims_after, orig_dims, "提取后当前尺寸不变");
        let _ = std::fs::remove_file(&out);
    }

    /// wave8：提取 seam `grid_apply_extract` **失败不清选** —— 同步预检失败（目标 == 当前
    /// 文档路径 P1 防护）→ `last_modify_error` 置位、**选中集保留**（可重试）；成功分支
    /// 的「异步成功后清选」见 `grid_apply_extract_writes_checked_pages_current_unchanged`。
    #[gpui::test]
    async fn grid_apply_extract_failure_keeps_selection(cx: &mut TestAppContext) {
        if test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _pdf_guard = test_util::lock().lock().unwrap();
        let holder: Rc<RefCell<Option<Entity<PdfViewer>>>> = Rc::default();
        let (_root, cx) = open_grid_host_fixture(&holder, cx, 0);
        let (n, cur_path) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (v.dims.len(), v.path.clone())
        });
        assert!(n >= 2, "夹具页数需 ≥2，实际 {n}");
        // 勾选 {0,1}，然后提取到**当前文档路径** → `extract_target_precheck` 同步拒 → 不
        // 发起异步、`last_modify_error` 置位、选中保留（可重试）。
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, cx| {
                v.grid_selected.extend([0usize, 1]);
                v.grid_apply_extract(vec![0, 1], cur_path, cx);
            });
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        let (sel, err) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (
                v.grid_selected_count(),
                v.last_modify_error().map(String::from),
            )
        });
        assert!(
            err.as_deref()
                .is_some_and(|m| m.contains("不能覆盖当前打开的文档")),
            "应报 P1 防护文案：{err:?}"
        );
        assert_eq!(sel, 2, "提取失败不清选（保持可重试）");
        assert_eq!(
            cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).dims.len()),
            n,
            "预检失败不改变模型页数"
        );
    }

    /// wave8：合并 seam `grid_apply_merge` —— 当前 pdf测试.pdf (n) + 源 pdf测试2.pdf (m)：
    /// 总页 n+m、前 n 页尺寸 = 原、尾部 m 页尺寸 = 源、尾部头 2 页渲染 = 源 0/1、
    /// 保持网格模式且选中清空。
    #[gpui::test]
    async fn grid_apply_merge_appends_source_and_keeps_grid(cx: &mut TestAppContext) {
        if test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _pdf_guard = test_util::lock().lock().unwrap();
        let holder: Rc<RefCell<Option<Entity<PdfViewer>>>> = Rc::default();
        let (_root, mut cx) = open_grid_host_fixture(&holder, cx, 0);
        let (n, orig_dims) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (v.dims.len(), v.dims.clone())
        });
        assert!(n >= 1, "夹具页数需 ≥1，实际 {n}");
        // 源度量：临时 worker 开 pdf测试2.pdf 拿 m 与源头 2 页渲染基准。
        let src_p = test_util::test_pdf_paths()[1].clone();
        let (w_src, d_src, src_dims) =
            PdfWorker::open_and_dims(std::fs::read(&src_p).expect("读取合并源应成功"))
                .expect("合并源应可打开");
        let m = src_dims.len();
        assert!(m >= 2, "合并源页数需 ≥2（尾部 2 页断言），实际 {m}");
        let src0 = page_probe(&w_src, d_src, 0);
        let src1 = page_probe(&w_src, d_src, 1);
        w_src.close(d_src);
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, cx| {
                v.grid_apply_merge(src_p, cx);
            });
        });
        // 算式（K177）：合并后总页数 = n（当前）+ m（源全页）。
        wait_seam_until(&holder, &mut cx, move |v| {
            v.is_loaded() && v.dims.len() == n + m
        });
        let (count, mode, sel, dims_all) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (
                v.dims.len(),
                v.grid_mode(),
                v.grid_selected_count(),
                v.dims.clone(),
            )
        });
        assert_eq!(count, n + m, "合并后总页数 = n+m");
        assert!(mode, "合并重载后应保持网格模式");
        assert_eq!(sel, 0, "合并成功经重载清选中");
        assert_eq!(dims_all[..n].to_vec(), orig_dims, "合并后前 n 页尺寸 = 原");
        assert_eq!(dims_all[n..].to_vec(), src_dims, "合并后尾部 m 页 = 源各页");
        // 尾部 2 页渲染 = 源 0/1（在重载后的新 worker/doc 上探）。
        let tail0 = live_page_probe(&holder, &mut cx, n);
        let tail1 = live_page_probe(&holder, &mut cx, n + 1);
        assert_eq!(tail0, src0, "合并后尾部第 1 页渲染 = 源首页");
        assert_eq!(tail1, src1, "合并后尾部第 2 页渲染 = 源第 2 页");
    }

    /// item 5：网格模式下插入空白页 → 插入后**保持网格模式**（`grid_hold_on_reload` 姿势）
    /// 且**不滚动到新页**（anchor 抑制——网格不消费阅读滚动，退出网格仍回原阅读位置）；
    /// 页数 +1、`pdf-grid` 容器在场（已回网格渲染）、新页出现在网格可视数据（页数已 +1）。
    /// 单列模式插入保持 index 定位由既有 lib.rs mem op 测试覆盖，此处不重复。
    #[gpui::test]
    async fn grid_mode_insert_blank_page_stays_in_grid(cx: &mut TestAppContext) {
        if test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _pdf_guard = test_util::lock().lock().unwrap();
        let holder: Rc<RefCell<Option<Entity<PdfViewer>>>> = Rc::default();
        let (_root, mut cx) = open_grid_host_fixture(&holder, cx, 0);
        let (n, scroll_y0) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (v.dims.len(), f32::from(-v.scroll.offset().y))
        });
        assert!(n >= 2, "夹具页数需 ≥2，实际 {n}");
        assert!(
            cx.debug_bounds("pdf-grid").is_some(),
            "插入前应处于网格视图"
        );
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, cx| {
                v.insert_blank_page_after(0, cx);
            });
        });
        wait_seam_until(&holder, &mut cx, move |v| {
            v.is_loaded() && v.dims.len() == n + 1
        });
        let (count, mode, scroll_y) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (v.dims.len(), v.grid_mode(), f32::from(-v.scroll.offset().y))
        });
        assert_eq!(count, n + 1, "插入后页数 +1");
        assert!(
            mode,
            "网格模式插入空白页应保持网格模式（grid_hold_on_reload）"
        );
        assert!(
            cx.debug_bounds("pdf-grid").is_some(),
            "插入重载后 `pdf-grid` 容器应在场（仍在网格视图）"
        );
        assert!(
            (scroll_y - scroll_y0).abs() < 1.0,
            "插入后单列滚动应保持原位（grid 下 anchor 被抑制、不跳新页），{scroll_y0} → {scroll_y}"
        );
    }
}
