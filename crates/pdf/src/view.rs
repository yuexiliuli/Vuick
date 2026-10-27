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

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{mpsc, Arc};

use gpui::{
    div, img, px, rgba, rgb, App, ClipboardItem, Context, FocusHandle, Hsla, ImageSource,
    InteractiveElement, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ParentElement, Point, Pixels, Render, RenderImage, ScrollHandle, ScrollWheelEvent,
    SharedString, StatefulInteractiveElement, Styled, Window,
};
use ui::scrollbar::{Scrollbar, ScrollbarMode};

/// 在途瓦片请求记录：`(派发时密度 scale, 结果通道)`。
pub(crate) type PendingTileEntry = (f32, mpsc::Receiver<Option<TileResult>>);

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
    COVER_INFLIGHT, MAX_MID_INFLIGHT, MAX_THUMB_INFLIGHT, MID_CACHE_PAGES, MOVE_THRESHOLD_LOWER_RATIO, MOVE_THRESHOLD_PX, PAGE_PAD_X, PAGE_WIDTH, TEXT_CACHE_PAGES,
    TEXT_INFLIGHT, THUMB_CACHE_PAGES, TILE_CACHE_BYTES, TILE_INFLIGHT, TILE_MARGIN, TILE_SIZE,
    ZOOM_MAX, ZOOM_MIN, ZOOM_STEP,
};
use crate::pdf::layout::{
    advance_match_index, bitmap_display_scales, column_left, compute_layout, content_to_local,
    cover_affordable, cover_scale, display_height, first_visible_page, hit_char, keep_window,
    local_to_pt, page_bitmap_size, page_segment_text, pt_to_local, render_scale,
    selection_spans_pt, visible_tile_range, Slot, TileSlot,
};
use crate::pdf::{
    render_image_from_bgra, AnnotKind, AnnotateItem, AnnotationInfo, DocId, PageText,
    PdfOutlineEntry, PdfSearchHit, PdfSearchResults, PdfWorker, QuadPt, RenderRequest,
    RenderResult, TileRequest, TileResult,
};
use viewer::pan_zoom;
/// 是否开启视图侧计时日志（`GPUI_EDITOR_PDF_TIMING=1`，进程内缓存一次）。
fn timing_view() -> bool {
    static EN: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *EN.get_or_init(|| {
        std::env::var_os("GPUI_EDITOR_PDF_TIMING").is_some_and(|v| v == "1" || v == "true")
    })
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
    pub(crate) pending_thumb: HashMap<usize, mpsc::Receiver<Option<RenderResult>>>,
    /// 在途 **L0.5 中清页**请求的结果接收通道（快速拖动占位）。
    pub(crate) pending_mid: HashMap<usize, mpsc::Receiver<Option<RenderResult>>>,
    /// 中清页内存 LRU 顺序（最近使用在队尾）。纹理本体就在 `slots[i].mid`
    /// （O(1) 主线程查找），本队列只用于淘汰顺序。
    pub(crate) mid_lru: VecDeque<usize>,
    /// 在途**瓦片**请求：`(页, row, col)` → (请求时密度 scale, 结果通道)。
    /// `scale` 用于结果到达时的密度守卫：缩放/换 DPI 后旧密度瓦片直接丢弃。
    pub(crate) pending_tiles: HashMap<(usize, u32, u32), PendingTileEntry>,
    /// 在途**封面**（首屏整页锐化）请求：`页` → (密度 scale, 结果通道)。
    pub(crate) pending_cover: HashMap<usize, (f32, mpsc::Receiver<Option<RenderResult>>)>,
    /// 当前已驻留瓦片的估算字节（近似 RGBA 内存；淘汰时重算自愈漂移）。
    pub(crate) tile_bytes: u64,
    /// 瓦片自刷新泵是否在途：派发瓦片后若无用户输入就不会再有重绘，而瓦片结果要等
    /// 下一次 `ensure_window` 才收集拼图——此泵每 ~16ms 触发一次重绘直到在途瓦片清空。
    pub(crate) tile_pump_running: bool,
    /// 上一帧滚动偏移（预测滚动方向用：delta>0 正在往下滚 → 预取下方页）。
    pub(crate) last_scroll_y: f32,
    /// 滚动速度估计（px/帧，EMA)：> [`MOVE_THRESHOLD_PX`] 视为快速拖动 →
    /// 改派缩略/中清占位、暂停瓦片派发（避免小片洪流），停止后瓦片立即补齐。
    pub(crate) scroll_velocity: f32,
    /// 迟滞判定后的"快速拖动"标志（进入 v>阈值、退出 v<阈值×比率，中间保持）。
    pub(crate) prev_fast: bool,
    /// 竖向滚动句柄（页列外层 scroll 容器）。
    pub(crate) scroll: ScrollHandle,
    /// 是否仍在后台解析。
    pub(crate) loading: bool,
    /// 加载失败信息。
    pub(crate) load_error: Option<String>,
    /// 阅读画布背景（页面“纸面”之外底色；None 用主题推导）。
    pub(crate) bg: Option<Hsla>,
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
                    tile_bytes: 0,
                    tile_pump_running: false,
                    last_scroll_y: 0.0,
                    scroll_velocity: 0.0,
                    prev_fast: false,
                    scroll: ScrollHandle::new(),
                    loading: false,
                    load_error: None,
                    bg: None,
                    pending_drops: Vec::new(),
                    outlines: Vec::new(),
                    focus_handle: cx.focus_handle(),
                    text_pages: HashMap::new(),
                    text_lru: VecDeque::new(),
                    pending_text: HashMap::new(),
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
                    window_sig: None,
                    window_settled: false,
                    idle_gate_skips: 0,
                    reload_anchor: None,
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
                // 先滚到文档中段（纵向负偏移），作为缩放重锚的基准
                v.scroll.set_offset(gpui::point(px(-40.0), px(-1200.0)));
                assert!((v.total_height - total).abs() < 1e-3, "build 完 total_height");
                v
            })
        });

        // 一档放大：zoom / 整列高按 1+ZOOM_STEP 走，滚动偏移同比重锚
        cx.update(|app| {
            viewer.update(app, |v, cx| {
                let w0 = v.page_width();
                let x0 = v.scroll.offset().x.as_f32();
                let y0 = v.scroll.offset().y.as_f32();
                let total0 = v.total_height;
                v.adjust_zoom(pan_zoom::zoom_factor(1.0, ZOOM_STEP), cx);
                let ratio = v.page_width() / w0;
                assert!((v.zoom - 1.12).abs() < 1e-6, "1 档放大 zoom={}", v.zoom);
                assert!(
                    (v.scroll.offset().x.as_f32() - x0 * ratio).abs() < 1e-3
                        && (v.scroll.offset().y.as_f32() - y0 * ratio).abs() < 1e-3,
                    "放大应同比重锚：x0={x0} x={} y0={y0} y={}",
                    v.scroll.offset().x.as_f32(),
                    v.scroll.offset().y.as_f32()
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
            Some(&mid),
            Some(&thumb),
        );
        assert!(
            std::sync::Arc::ptr_eq(&base2.expect("有封面必有底图"), &cur_cover),
            "当前密度封面最优先"
        );

        // 无封面（含旧封面）时：中清 > 缩略 > 全空。
        assert!(std::sync::Arc::ptr_eq(
            &super::render::page_base_image(None, None, Some(&mid), Some(&thumb))
                .expect("中清存在"),
            &mid,
        ));
        assert!(std::sync::Arc::ptr_eq(
            &super::render::page_base_image(None, None, None, Some(&thumb))
                .expect("缩略存在"),
            &thumb,
        ));
        // 四者全空（首开/极速拖动且旧资产也被回收）→ None，此时才显示「加载中」占位。
        assert!(
            super::render::page_base_image(None, None, None, None).is_none(),
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
                    tile_bytes: 0,
                    tile_pump_running: false,
                    last_scroll_y: 0.0,
                    scroll_velocity: 0.0,
                    prev_fast: false,
                    scroll: ScrollHandle::new(),
                    loading: false,
                    load_error: None,
                    bg: None,
                    pending_drops: Vec::new(),
                    outlines: Vec::new(),
                    focus_handle: cx.focus_handle(),
                    text_pages: HashMap::new(),
                    text_lru: VecDeque::new(),
                    pending_text: HashMap::new(),
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
                    window_sig: None,
                    window_settled: false,
                    idle_gate_skips: 0,
                    reload_anchor: None,
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
}

