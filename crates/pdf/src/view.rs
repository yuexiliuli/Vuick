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
//!
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
// PDF 视图状态树类型较长 → 放行
#![allow(clippy::type_complexity)]
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

use crate::pdf::parser::{GridPrewarmGeom, RawKind, RecolorKey};
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
    GridLayout, PagedFingerprint, PagedLayout, Slot, TileSlot, advance_match_index,
    bitmap_display_scales, column_left, compute_layout, content_to_local, cover_affordable,
    cover_scale, display_height, first_visible_page, grid_layout, grid_zoom_limits_for_cols,
    hit_char, keep_window, local_to_pt, min_ar, page_bitmap_size, page_segment_text, paged_layout,
    paged_layout_scaled, paged_render_window, pt_to_local, render_scale, selection_spans_pt,
    visible_tile_range_axis, zoom_rescroll_anchor, zoom_rescroll_anchor_paged,
};
use crate::pdf::parser::PDF_MERGE_MAX_SRC_BYTES;
use crate::pdf::{
    AnnotKind, AnnotateItem, AnnotationInfo, DocId, PageText, PdfImageBox, PdfImagePixels,
    PdfOutlineEntry, PdfSearchHit, PdfSearchResults, PdfWorker, QuadPt, RecolorEntry,
    RenderRequest, RenderResult, TileRequest, TileResult, render_image_from_bgra,
};
use config::pdf::{
    COVER_INFLIGHT, GRID_MBAND_ROWS, GRID_NBAND_ROWS, IMAGE_CACHE_PAGES, IMAGE_INFLIGHT,
    JUMP_PAGES_THRESHOLD, MAX_MID_INFLIGHT, MAX_THUMB_INFLIGHT, MID_CACHE_PAGES,
    MOVE_THRESHOLD_LOWER_RATIO, MOVE_THRESHOLD_PX, MOVE_THRESHOLD_VERY_RATIO, PAGE_PAD_X,
    PAGE_WIDTH, PAGED_GAP_X, PAGED_GAP_Y, PREFETCH_COVER_MARGIN, PREFETCH_COVER_MARGIN_MAX,
    PREFETCH_RETURN_SIDE_BONUS, STOP_DETECT_FRAMES, STOP_DETECT_PX, TEXT_CACHE_PAGES,
    TEXT_INFLIGHT, THUMB_CACHE_PAGES, TILE_CACHE_BYTES, TILE_INFLIGHT, TILE_MARGIN, TILE_SIZE,
    ZOOM_MAX, ZOOM_MIN, ZOOM_STEP,
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

/// T2：分页态深跳落点封面窗（**行界收窄**，`buf_rows=0`）。以**当前滚动位置**的可见行页
/// 区间（[`paged_render_window`] `buf_rows=0`）为窗——落点行中间的常规深跳恰为「目标行 2 列
/// = 2 页」；行中位半行压两行时自动含相邻行（≤4 页，**不缩水下可见页**）。滚动尚在途/算不
/// 出可见行、或可见窗不含目标页时，退化为「目标页所在行」的行页区间，保证落点页必在窗内。
/// 禁 `buf_rows=1`（双向外扩最宽 6 页反而变宽，评审已证）。`scroll_y` 为视口顶内容偏移
/// （正为向下滚；负偏移换算同 `ensure_window` 的 `scroll_y_fit = −scroll_y/zoom`）。
fn paged_jump_cover_window(
    layout: &PagedLayout,
    zoom: f32,
    target: usize,
    scroll_y: f32,
    viewport_h: f32,
    n: usize,
) -> Option<(usize, usize)> {
    if layout.rows == 0 || n == 0 {
        return None;
    }
    let zoom = zoom.max(0.01);
    let scroll_y_fit = -(scroll_y.max(0.0) / zoom);
    let vh_fit = viewport_h / zoom;
    if let Some((s, e)) = paged_render_window(layout, scroll_y_fit, vh_fit, n, 0)
        && s <= target
        && target <= e
    {
        return Some((s, e));
    }
    // 退化为目标页所在行的行页区间（首页/末页由 `row_page_start` 区间 + `min(n−1)` 兜底夹回，
    // 尾部半行自动只剩该行实际页数）。
    let cols = layout.cols.max(1);
    let row = (target / cols).min(layout.rows.saturating_sub(1));
    let s = layout.row_page_start[row];
    let e = layout
        .row_page_start[row + 1]
        .saturating_sub(1)
        .min(n.saturating_sub(1));
    if s <= e {
        Some((s, e))
    } else {
        None
    }
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
mod ensure;
mod input;
mod open;
mod ops;
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
///
/// 任意钳内 `grid_zoom` ⇒ 恒有 `GRID_MAX_COLS ≤ cols ≤ GRID_MIN_COLS`（推导见该函数注释）。
pub(crate) const GRID_MAX_COLS: f32 = 2.0;
pub(crate) const GRID_MIN_COLS: f32 = 9.0;
/// 网格行带虚拟化的上下外扩缓冲行数：可视行之外各多物化 [`Self::grid_visible_range`]
/// 一行的页卡，滚动/惯性时新行提前就位——对齐页列视图 `keep_window` 的 MARGIN 页缓冲语义。
/// 取 2（从 1 加大，本线滚动闪动修复）：可视带上下各提前 2 行，单行滚动/小幅回看时
/// 旧带卡仍在带内、不触发淘汰重载。渲染与调度共用本值（`grid_visible_range` 同一几何），
/// 该档卡小（缩略图级），多物化 1-2 行 DOM 成本可接受。
pub(crate) const GRID_ROW_BUFFER: usize = 2;
/// 网格缩略图**淘汰保活窗**行数：`evict_grid_thumbs` 在渲染/调度带（`grid_visible_range`，
/// 可见行 ±[`GRID_ROW_BUFFER`]）之外再按本值外扩行数保留已就绪图——滚动把卡带出可视带
/// 后**不立即逐出**，回看时直接复用（滚回即显、零重载，C 保活替代硬驱逐）。超保留窗的
/// 陈旧卡按「距可视中心行距」由远及近逐出（结构性有界；字节账 + 预算常量统一归提交前
/// 跟进项）。取 2：可视带外再容 2 行（≈ 覆盖单帧滚出二行 + 上一帧带位），回看零重载与
/// 驻留有界取得平衡。
pub(crate) const GRID_KEEP_ROWS: usize = 2;
/// 网格顶栏（wave5 工具栏）高度（px）：贴网格滚动容器顶部，滚动条随之下移对齐（见
/// `render.rs` 的滚动条 overlay 顶偏）。选 40：一行胶囊按钮 + 上下留白够点，不侵占卡区。
pub(crate) const GRID_TOOLBAR_H: f32 = 40.0;
// ---- 网格低清快速档（本线）：快滚期显式钳低栅格宽（面积 ↓4× ⇒ pdfium ~4× 快）----
/// 低清快速档渲染宽相对全清档的比例：`grid_quick_target_px(t) = t × 本值`（面积 ↓4×，
/// 快滚期每卡先落一张低清图替代「第 N 页」占位；停滚后用 idle 预算升级全清替换，低清
/// 图在升级期间作桥保留）。取 0.5：面积 1/4、pdfium 栅格耗时约 1/4，观感仍可辨字形。
pub(crate) const GRID_QUICK_SCALE: f32 = 0.5;
/// 低清快速档渲染宽下限（px）：快档宽度不无限缩小（过小糊到不可辨 + 分桶退化），
/// 取 64：卡宽 200-600 的 25-50%，A4 竖页 64px 宽仍可读页码/段落块。
pub(crate) const GRID_QUICK_MIN_PX: f32 = 64.0;
/// 网格**滚动风暴**判定位移阈值（px/帧）：相邻帧滚动偏移差 ≥ 本值即入风暴态（快滚期
/// 低清派发）。取 1.0：单帧亚像素抖动（<1px）不误触发；滚轮一档/拖拽即超。
pub(crate) const GRID_STORM_CHANGE_PX: f32 = 1.0;
/// 网格**滚动风暴**离场判定空闲帧数：连续本帧数帧内偏移差 < [`GRID_STORM_CHANGE_PX`] →
/// 判定停滚（退出风暴、idle 升级接管）。取 10：10 帧 ≈ 160ms @60fps（8ms 帧周期下 ≈ 80ms）
/// ——滚轮骤停的 1-2 帧惯性微移不误触退出，又不拖长低清期。
pub(crate) const GRID_STORM_IDLE_FRAMES: u32 = 10;
/// 网格**滚动风暴方向感知预取**的超前行数（本批「方向感知派发」杠杆）：风暴期把带内
/// 派发序的「派发中心」沿滚动方向外移本值行数——下滚先补下方前向页、上滚先补上方前向
/// 页（同方向内仍距超前中心近优先），落后方向页被推到最后（预算 8/帧先被前向吃光）。
/// 取 2：与渲染/调度行带缓冲 [`GRID_ROW_BUFFER`] 同量级——风暴滚动一秒能滚 12-20 行，
/// 前向多预备 2 行即将入屏的页，新入带卡在滚动到达前已落图，直击「新入带卡暴露」。
pub(crate) const GRID_STORM_AHEAD_ROWS: usize = 2;

/// 网格低清快速档目标渲染宽（纯函数）：`target_px × [`GRID_QUICK_SCALE`]`、下限
/// [`GRID_QUICK_MIN_PX`]。`target_px` = 全清档目标宽（`grid_card_width() × scale_factor`，
/// 与 worker `card_width × scale_factor` 同口径）。调用方把 `quick_px` 换算成 `card_width`
/// 传 worker（`quick_px / scale_factor`），worker 按传入宽分桶 + clamp scale，零改动。
/// 边界：仅当 `target_px` 病态过小（<~1 桶，正常网格卡宽 ≥150px 不触发）使
/// `grid_bucket_width(quick_px) == grid_bucket_width(target_px)` 时，低清与全清同桶无收益
/// → 调用方按该同桶判别（见 `schedule_grid_thumbs` 风暴分支）静默退化直派全清。
pub(crate) fn grid_quick_target_px(target_px: f32) -> f32 {
    (target_px * GRID_QUICK_SCALE).max(GRID_QUICK_MIN_PX)
}

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

/// 网格**淘汰保活窗**页区间（纯函数，供 `evict_grid_thumbs` 用）：与 [`grid_visible_range`]
/// 完全同构的行带 → 页区间映射，但外扩行数取保留窗阈值 `GRID_ROW_BUFFER + GRID_KEEP_ROWS`
/// （渲染/调度带外再各留 [`GRID_KEEP_ROWS`] 行）——带外已就绪卡在保留窗内**不逐出**（滚动
/// 回看即显、零重载，C 保活替代硬驱逐）。语义边界：带内（[`grid_visible_range`] 所得）恒
/// 钉住；保留窗 = 带 + 2 行，超窗按距可视中心由远及近逐「最远者」。无可见带（视口在内容
/// 外）→ `None`（调用方按全清处理）。
fn grid_keep_range(layout: &GridLayout, offset: f32, viewport_h: f32) -> Option<(usize, usize)> {
    let (r0, r1) = layout.first_visible_grid_row(offset, viewport_h)?;
    let keep = GRID_ROW_BUFFER + GRID_KEEP_ROWS;
    let r0 = r0.saturating_sub(keep);
    let r1 = (r1 + keep).min(layout.rows.saturating_sub(1));
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
///
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
    /// 位图占用的**字节账**（view 侧 `grid_bytes` 总额计账用；写入时按
    /// `w×h×bpp` 派生——本批「字节账+预算」杠杆，见 [`config::pdf::GRID_VIEW_BUDGET_BYTES`]）。
    pub(crate) bytes: u64,
}

/// 阅读页排模式（状态层打桩：本切片**只存状态**，渲染三路分派/滚动锚点恢复由后续切片接）：
/// - [`Continuous`]：连续滚动（单列全文，默认）
/// - [`Facing`]：双页对开（一次两页）
///
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
    /// T1：封面**重派（同页双渲）**累计计数——既有 stale→prev_cover→按新密度重派通道
    /// 每触发一次 +1。Facing 冷开回归断言「分页封面窗全 None 帧跳过派发后该计数降至
    /// ≤1」消除「邻居降档 2.0 → 满密度 2.138」的同页双渲（配合 `schedule_covers` 的全
    /// None 守卫，见 tiles.rs；残余 1 属模式翻转密度沉降）。
    pub(crate) cover_re_renders: u64,
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
    /// 在途网格缩略图请求：页 → (导航世代 nav_gen, **请求时传的卡宽**, 结果通道)。世代守卫
    /// 对齐 [`Self::pending_thumb`]（大跳/切档时按 nav_gen/zoom_epoch 由 worker drop 旧请求）；
    /// **请求时卡宽**供 `collect_grid_results` 按请求分桶打戳（既有 view 侧口径
    /// `grid_bucket_width(卡宽)`）——低清/全清两档在途并存，不能像旧单档那样按「当前全清
    /// 桶」统一打戳（低清在途被错标成全清桶、破坏「桥 + idle 升级」语义）。
    pub(crate) pending_grid:
        HashMap<usize, (u64, f32, mpsc::Receiver<Option<RenderResult>>)>,
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
    /// 网格**滚动风暴**标志：相邻帧滚动偏移差 ≥ [`GRID_STORM_CHANGE_PX`] 进入（快滚期低清
    /// 派发 + 占位弱化）；连续 [`GRID_STORM_IDLE_FRAMES`] 帧 < 阈值退出（idle 升级接管）。
    /// 由 `ensure_window_grid` 每帧首行更新（open.rs），`schedule_grid_thumbs`/`build_grid_cards`
    /// 同帧读取。首帧默认 false（回退旧「第 N 页」占位与全清派发）。
    pub(crate) grid_storm: bool,
    /// 网格滚动风暴检测：上一帧的内容滚动偏移 y（px，`-grid_scroll.offset().y`）。
    pub(crate) grid_last_offset: f32,
    /// 网格滚动风暴检测：已连续「几乎静止」（偏移差 < [`GRID_STORM_CHANGE_PX`]）的帧数，
    /// ≥ [`GRID_STORM_IDLE_FRAMES`] 时宣告停滚（退出风暴）。
    pub(crate) grid_storm_idle_frames: u32,
    /// 网格**滚动方向**（本批「方向感知预取」）：紧随 `ensure_window_grid` 的滚动偏移差
    /// 更新——`+1` = 向下滚（内容上移、应超前补下方大行号页）、`-1` = 向上滚、`0` = 静止
    /// （保持上一方向，避免抖动翻转）。`schedule_grid_thumbs` 风暴分支据此沿方向超前派发。
    pub(crate) grid_scroll_dir: i8,
    /// 网格**本帧实际派发页序**（本批方向预取的可观测性，供测试断言派发序；仅风暴分支按
    /// 方向超前序填充、idle 分支填中心优先序，每帧 `schedule_grid_thumbs` 覆盖写、非累积）。
    pub(crate) grid_dispatch_order: Vec<usize>,
    /// 网格缩略图**字节账**（`grid_thumbs` 全部卡位图字节和，本批「字节账+预算」杠杆）：
    /// 入槽计加、替换/逐出/清空计减；`evict_grid_thumbs` 超
    /// [`config::pdf::GRID_VIEW_BUDGET_BYTES`] 时按「距可视中心最远」逐出（可视带永不逐）。
    pub(crate) grid_bytes: u64,
    /// 网格**带宽预熟最近推送**（本线）：上次经 `set_grid_prewarm` 下发给 worker 的
    /// `(中心页, 中带外沿区间, 近带区间, 全清桶)` 打包——`push_grid_prewarm` 以此为节流基准：
    /// 几何或跨桶变化才重推（滚动在**同行内**中心页不变 → 不每帧刷 worker；跨行/跨桶才推），
    /// 幂等跳过同一窗口的重复推送。`None` = 尚未推。
    pub(crate) grid_prewarm_pushed: Option<(usize, (usize, usize), (usize, usize), u32)>,
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
    /// 分页页排（Facing）布局每帧快照：`ensure_window` 以 zoom=1（fit）口径、
    /// 真实阅读视口重算 [`PagedLayout`] 并抄入，render 期读上一帧布局做分页定位（同
    /// `grid_layout_cell`「渲染期写、render 期读」姿势，下一页排命中/滚动沿用本快照）。
    /// 连续模式 / 网格 / 空文档 → `None`（渲染走连续或网格分支，不读它）。
    /// **问题 B 指纹门**：`ensure_window` 只在 [`Self::paged_fingerprint`] 变化时重建本
    /// 快照；纯滚动/静止帧输入不变 → 复用上帧（零 5×O(rows) 向量分配）。
    pub(crate) paged_layout_cell: std::cell::RefCell<Option<PagedLayout>>,
    /// Facing 分页快照的**指纹**（问题 B）：`Some` = `paged_layout_cell` 按此输入压实而成，
    /// `None` = 连续/网格/空文档或未算过。`ensure_window` 先比对指纹、只在变化时重建
    /// 分页快照并更新；连续/网格分支清为 `None`（模式切换后不误复用旧几何）。
    pub(crate) paged_fingerprint: Option<PagedFingerprint>,
    /// 问题 B 观测计数：分页快照**实际重建**次数（`ensure_window` 指纹变化分支 +1）。
    /// 纯回归观测（无副作用）：守门测试断言「同视口静止帧零重建、resize 后必重建」。
    pub(crate) paged_layout_rebuilds: u64,
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
mod tests;
