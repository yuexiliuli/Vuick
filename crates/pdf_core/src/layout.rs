//! PDF 页面的**纯布局/调度数学**：页列尺寸、可见页区间、渲染倍率、瓦片网格、每帧工作规划。
//!
//! 全部为无副作用纯函数（便于单测），与 `view.rs` 的页列 DOM 绝对定位**使用同一套数学**，
//! 保证滚动位置推算与 DOM 位置严格一致（否则可见页会被误判）。瓦片几何（[`tile_rect`]）
//! 由 **worker 线程**（`parser.rs`）与 **view 线程**（`view.rs`）共用——两侧各自独立算出
//! 的瓦片矩形必须逐像素一致，拼片才不会错位。

use std::collections::HashMap;

use crate::pdf::PageText;
use config::pdf::{
    CACHE_MAX_PAGES, COVER_AREA_BUDGET, MARGIN, MAX_BITMAP_SCALE, MAX_RENDER_SCALE, PAGE_GAP,
    PAGE_PAD_Y, PAGE_WIDTH,
};

/// 页槽：某页的渲染状态（内存态）。
#[derive(Default, Clone)]
pub struct Slot {
    /// 已栅格化的**瓦片**缓存（键 = 页位图内的 (row, col) 网格格号）。瓦片按显示物理
    /// 像素密度渲染（见 [`render_scale`]），**内存 = 视口可见瓦片数 × TILE_SIZE² × 4**，
    /// 不随整页位图面积缩放爆炸（旧整页方案 4× 缩放一页 A4 = 64MB 的根因）。
    pub tiles: HashMap<(u32, u32), TileSlot>,
    /// 该页已驻留瓦片的估算字节和（每片增删同步记账，见 `view.rs` 各增删点）。
    /// 字节预算以各槽位账簿汇总为准（替代原「O(页×瓦片) 全扫兜底」的自愈重算）。
    pub tile_bytes: u64,
    /// 现有瓦片对应的渲染倍率（px/pt）。缩放/换 DPI 显示器后与新一轮 [`render_scale`]
    /// 不一致 → 该页瓦片被惰性作废，按新密度重栅格化（旧瓦片 `drop_image` 下线）。
    pub tile_scale: f32,
    /// **首屏整页锐化封面**（SumatraPDF 首遍同款）：可见页在「廉价密度」下先整页一渲、
    /// 立刻铺满首屏（瓦片几十片串行渲染前的瞬时清晰），瓦片补齐后丢弃回收内存。
    /// 与瓦片同密度，故与瓦片拼图完全对齐，只是作底层、先行到达。
    pub cover: Option<std::sync::Arc<gpui::RenderImage>>,
    /// 封面对应的渲染倍率（px/pt）；缩放变化后视为过期并丢弃。
    pub cover_scale: f32,
    /// **旧密度封面/退位过渡底图**（缩放过渡期 + F2 方向翻位兜底）：缩放/换 DPI 后
    /// `schedule_tiles` 把旧封面从 [`Slot::cover`] 移入此处暂留，在新密度封面到位前按新页宽
    /// **CSS 拉伸铺满**，防「Ctrl+滚轮缩放时每档闪一次『加载中』」；F2 评审 D1 兜底另把
    /// 「方向翻位时刚退位的反向邻居」封面转存此处（`evict_covers` 的 `retire` 分支，同一时刻
    /// 至多 1 张），反向微调滚动期间作该页底图桥。两者均在新封面到位（`collect_cover_results`）、
    /// 该页离开封面窗口且不再是退位页（`evict_covers`）或视图释放（`release`）时即下线回收——
    /// 只作瞬态过渡底图，**非新增常驻缓存**（同一时间每页至多一份，不影响预算账本）。
    pub prev_cover: Option<std::sync::Arc<gpui::RenderImage>>,
    /// L0 缩略图（低清层）：全尺寸瓦片缺席时先展示，拖动期间"永远有内容"；
    /// 快速拖动时全页只有缩略图占位，停止后按需补齐瓦片。
    pub thumb: Option<std::sync::Arc<gpui::RenderImage>>,
    /// 缩略图是否正在后台渲染（避免重复派发）。
    pub thumb_loading: bool,
    /// L0.5 中清页（~520px）：快速拖动时优先展示（比 260px 缩略图清晰、可辨识标题/
    /// 段落结构），作瓦片下方的底图占位。
    pub mid: Option<std::sync::Arc<gpui::RenderImage>>,
    /// 中清页是否正在后台渲染（避免重复派发）。
    pub mid_loading: bool,
    /// F4 空闲预取走廊的**半密度整页封面**（独立于 [`Slot::cover`] 的满密度窗口封面）：
    /// 空闲期把 [vis±2] 走廊页的低成本底图预取进槽，高速平移切入时新页已有可显示底图
    /// （WPS「好像全部已加载」的来源）。生命周期：`view/tiles::evict_prefetch_covers` 按
    /// 距离/字节预算 LRU 收回；该页进入封面窗、满密度封面到位（`view/tiles::
    /// collect_cover_results`）时即下线回收。不参与窗口封面驻留计数（`cover` 不变）。
    pub prefetch_cover: Option<std::sync::Arc<gpui::RenderImage>>,
    /// 走廊封面对应的渲染倍率（px/pt，= 0.5× `cover_scale`）；render 按新页宽 CSS 拉伸铺满。
    pub prefetch_cover_scale: f32,
}

/// 一块已完成栅格化的瓦片：图像本体 + 它在页位图中的几何。
#[derive(Clone)]
pub struct TileSlot {
    pub image: std::sync::Arc<gpui::RenderImage>,
    /// 瓦片在页位图中的左上角 x（含出血裁剪，已夹到页内）。
    pub x0: u32,
    /// 瓦片在页位图中的左上角 y。
    pub y0: u32,
    /// 瓦片位图实际宽（含出血，已夹到页内）。
    pub w: u32,
    /// 瓦片位图实际高。
    pub h: u32,
}

impl TileSlot {
    /// 瓦片占用的近似内存字节（RGBA 4BPP / **R8 luma 1BPP**——P2 门控激活时瓦片按单通道
    /// 亮度编码，-4×；其他路径维持 RGBA）。误按 4BPP 会高估 4×、破坏预算语义。
    pub fn bytes(&self) -> u64 {
        let bpp = if self.image.is_luma(0) { 1 } else { 4 };
        (self.w as u64) * (self.h as u64) * bpp
    }
}

/// 某页在页列里的显示高度（保持宽高比）。
pub fn display_height((w, h): (f32, f32), page_width: f32) -> f32 {
    if w > 0.0 {
        page_width * (h / w)
    } else {
        page_width
    }
}

/// 预先计算整列布局：每页的 top 偏移（[`PAGE_PAD_Y`] + 前缀和）与整列总高。
///
/// 与 [`keep_window`] / 页列 DOM 绝对定位使用同一套数学。
pub fn compute_layout(dims: &[(f32, f32)], page_width: f32) -> (Vec<f32>, f32) {
    let mut tops = Vec::with_capacity(dims.len());
    let mut y = PAGE_PAD_Y;
    for dim in dims {
        tops.push(y);
        y += display_height(*dim, page_width) + PAGE_GAP;
    }
    let last = dims.last().copied();
    let total = match last {
        Some(dim) => tops[dims.len() - 1] + display_height(dim, page_width) + PAGE_PAD_Y,
        None => PAGE_PAD_Y * 2.0,
    };
    (tops, total)
}

/// 全文档最小纵横比 `min_i(w_i/h_i)`（跳过 `w<=0 ∨ h<=0` 退化页；全退化/空 → 1.0）。
///
/// 连续（默认）模式「整页入屏」（contain-fit）的包含宽度口径：`base_w =
/// min(content_w, viewport_h·min_ar)`——与双页（Facing）模式行内同名 `min_ar`（见
/// [`paged_layout`]）同源姿势，只把「行内页区间」换成「全文档」：取最竖页的宽高比
/// 限高，保证整页（含最竖页）不超出一屏高。缓存于 view 层（`doc_min_ar`）避免每帧
/// O(n) 全扫 dims。
pub fn min_ar(dims: &[(f32, f32)]) -> f32 {
    let mut min_ar = f32::INFINITY;
    for &(w, h) in dims {
        if w > 0.0 && h > 0.0 {
            min_ar = min_ar.min(w / h);
        }
    }
    if min_ar.is_finite() {
        min_ar
    } else {
        1.0
    }
}

/// Z1 缩放锚点重锚（纯函数契约）：返回缩放后应保持「锚点页 K 的页内相对位置」的滚动 y。
///
/// 页列布局 `tops` 随 `page_width` 线性缩放（页高同比、`display_height` 同源），但
/// `PAGE_PAD_Y`/`PAGE_GAP` 常量**不随缩放变化**——因此正确重锚是「锚点页 K 的页顶按新布局
/// 换算 + 页内相对偏移 `(scroll_y − tops_old[K])` 同比缩放」，而不是把整个 scroll 按
/// `ratio` 乘（后者每档漂移 `-(ratio-1)×(PAD+K×GAP)`，深页可跳 2-3 行高、放大上跳/缩小下跳
/// 离散跳）。K = `first_visible_page` 语义（scroll 贴页顶时锚在该页；越界 clamp）。
/// 契约（Z1.1）：`result == tops_new[K] + (scroll_y − tops_old[K]) × ratio`，且缩放回原档
/// （ratio 互逆、布局对称）无漂移。
pub fn zoom_rescroll_anchor(
    dims: &[(f32, f32)],
    tops_old: &[f32],
    tops_new: &[f32],
    page_width_old: f32,
    ratio: f32,
    scroll_y: f32,
) -> f32 {
    if tops_old.is_empty() && tops_new.is_empty() {
        return scroll_y * ratio;
    }
    let k = first_visible_page(dims, tops_old, page_width_old, scroll_y);
    let top_old = tops_old.get(k).copied().unwrap_or(0.0);
    let top_new = tops_new.get(k).copied().unwrap_or(top_old * ratio);
    top_new + (scroll_y - top_old) * ratio
}

/// 分页模式缩放锚点重锚（纯函数契约，S6）：返回缩放后应保持「锚点行 `row` 的行内相对
/// 位置」的滚动 y（**显示口径** px，即 `−scroll.offset().y`）。
///
/// 分页几何 = `PagedLayout`（zoom=1 fit 口径）× zoom 均匀缩放：cell_w 等比 → 行内页相对
/// 位置按 `ratio` 线性缩放。行号 `row` 由「缩放前首可见行」捕获（rows 由页数/cols 决定、
/// 与 zoom 无关 → 缩放后行号不变），故只需把缩放前落在行内的显示偏移按 ratio 同比、
/// 再加上新 zoom 下的行顶。契约（S6.1）：
/// `result == pl_new.row_top[row] + (sy − pl_old.row_top[row]) × ratio`，
/// 且缩放回原档（ratio 互逆、`pl_old`/`pl_new` 对称）无漂移。
///
/// **入参口径**：`pl_old`/`pl_new` 必须是**显示口径**布局（`row_top`/`cell_w` 等已乘各自
/// zoom，即 [`paged_layout_scaled`] 的产物），`sy` 同为显示口径滚动偏移，`ratio` = 新页宽/
/// 旧页宽 = `zoom_new/zoom_old`（[`paged_layout_scaled`] 用同一 zoom 构造 pl_old/pl_new 时
/// 等价于 zoom 比）。`row` 越界（空布局 / 越界行）→ 回落 `sy × ratio`（整段同比兜底）。
pub fn zoom_rescroll_anchor_paged(
    pl_old: &PagedLayout,
    pl_new: &PagedLayout,
    row: usize,
    ratio: f32,
    sy: f32,
) -> f32 {
    // 行越界（空布局 / 缩放后行数收缩）→ 整段同比兜底（与连续 `zoom_rescroll_anchor` 同姿态）。
    let top_old = pl_old.row_top.get(row).copied();
    let top_new = pl_new.row_top.get(row).copied();
    match (top_old, top_new) {
        (Some(to), Some(tn)) => tn + (sy - to) * ratio,
        _ => sy * ratio,
    }
}

/// 把 zoom=1 fit 口径的 [`PagedLayout`] 均匀缩放到**显示口径**（所有坐标/尺寸 × `zoom`）：
/// `row_top`/`cell_w_per_row`/`cell_h`/`left_pad_per_row`/`spread_max`/`total_height`/`h_gap`/
/// `v_gap`/`padding` 全部乘 `zoom`，`row_page_start`/`cols`/`rows` 不变。产出与渲染口径一致
/// （`render_pdf_pages_paged` 读 zoom=1 布局的 `cell_rect` 各分量 × zoom），供
/// [`zoom_rescroll_anchor_paged`] 构造 `pl_old`/`pl_new`（分别乘旧/新 zoom）。
pub fn paged_layout_scaled(l: &PagedLayout, zoom: f32) -> PagedLayout {
    PagedLayout {
        cols: l.cols,
        rows: l.rows,
        h_gap: l.h_gap * zoom,
        v_gap: l.v_gap * zoom,
        padding: l.padding * zoom,
        row_top: l.row_top.iter().map(|&v| v * zoom).collect(),
        row_page_start: l.row_page_start.clone(),
        cell_w_per_row: l.cell_w_per_row.iter().map(|&v| v * zoom).collect(),
        cell_h: l.cell_h.iter().map(|&v| v * zoom).collect(),
        left_pad_per_row: l.left_pad_per_row.iter().map(|&v| v * zoom).collect(),
        spread_max: l.spread_max * zoom,
        total_height: l.total_height * zoom,
    }
}

/// 当前第一张**可见**页（0 基）：「页面底 > 视口顶」的首张页。
/// 与 [`keep_window`] 的窗口起点不同——**不含 MARGIN 缓冲**，用于状态栏页码等
/// 「如实反映视口」的读数；`scroll_y` 为内容已向下滚动的像素数（≥0）。
///
/// `tops` 为页列顶部偏移缓存（[`compute_layout`] 的第一个输出，调用方须与 `dims`
/// 同步维护、长度一致）。二分**直读缓存**，每帧 O(log P)，不再每帧内建 O(P) 表。
pub fn first_visible_page(
    dims: &[(f32, f32)],
    tops: &[f32],
    page_width: f32,
    scroll_y: f32,
) -> usize {
    if dims.is_empty() || tops.is_empty() {
        return 0;
    }
    let top = scroll_y.max(0.0);

    // 二分定位「页底 > 视口顶」的首张页（lower_bound）。
    // `tops` 严格递增（每页叠加正页高 + PAGE_GAP，见 `compute_layout_is_monotonic_and_bounded`），
    // 页底值 `tops[i] + display_height` 随之严格递增 → 谓词单调，可二分（千页 O(页数)→O(log 页数)）。
    // 与旧线性扫逐例等价：谓词用同一套浮点运算，且无任何页命中时（已滚过末页）仍返回 0。
    let n = dims.len();
    let mut lo = 0usize;
    let mut hi = n;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if tops[mid] + display_height(dims[mid], page_width) <= top {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    if lo == n { 0 } else { lo }
}

/// 可见页区间（含缓冲）：与页列布局镜像，纯函数便于测试。
///
/// `scroll_y` 为内容已向下滚动的像素数（≥0）；`viewport_h` 为可见区高度。
/// `tops` 为页列顶部偏移缓存（[`compute_layout`] 的第一个输出），直读不做 O(P) 重建。
pub fn keep_window(
    dims: &[(f32, f32)],
    tops: &[f32],
    page_width: f32,
    scroll_y: f32,
    viewport_h: f32,
) -> (usize, usize) {
    if dims.is_empty() || tops.is_empty() {
        return (0, 0);
    }
    let vh = if viewport_h > 1.0 { viewport_h } else { 720.0 };
    let top = scroll_y.max(0.0);
    let bottom = top + vh;

    // 二分首/尾两界（`tops` 严格递增 → 两个谓词都单调，见 `first_visible_page`）。与旧
    // 线性扫逐例等价：`first` 无命中（滚过末页）与 `last` 无命中（视口在首页顶之上）都回退 0。
    let n = dims.len();

    // 第一页「页底 > 视口顶」
    let mut lo = 0usize;
    let mut hi = n;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if tops[mid] + display_height(dims[mid], page_width) <= top {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    let first = if lo == n { 0 } else { lo };

    // 最后一页「页顶 < 视口底」
    let mut lo = 0usize;
    let mut hi = n;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if tops[mid] < bottom {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    let last = if lo == 0 { 0 } else { lo - 1 };

    let start = first.saturating_sub(MARGIN);
    let end = (last + MARGIN).min(n - 1);
    (start, end)
}

/// 渲染倍率：以在屏宽度、显示 DPI 推导，封顶避免超大位图。
/// 高倍缩放（page_width > [`PAGE_WIDTH`]）时按缩放比抬高封顶，保证放大后仍清晰。
/// **封顶随物理像素需求（`scale_factor`）同步抬升**：渲染目标必须先够到“显示物理像素”
/// （宽 = `page_width × DPI`），否则 GPU 对低分辨位图上采样拉伸 → 高 DPI 屏文字/图像发糊。
/// [`MAX_RENDER_SCALE`] 是相对 DPI 的倍率，[`MAX_BITMAP_SCALE`]（px/pt 绝对硬顶）兜底。
pub fn render_scale(page_width: f32, scale_factor: f32, page_pt_width: f32) -> f32 {
    if page_pt_width > 0.0 {
        let zoom = (page_width / PAGE_WIDTH).max(1.0);
        let cap = (MAX_RENDER_SCALE * zoom * scale_factor.max(1.0)).min(MAX_BITMAP_SCALE);
        (page_width * scale_factor / page_pt_width).clamp(0.5, cap)
    } else {
        MAX_RENDER_SCALE
    }
}

// ---- 瓦片（Tile）几何：worker 与 view 共用同一套数学，保证拼片逐像素对齐 ----

/// 位图尺寸的「先取整再 ceil」稳定器：`page_width()` 已把显示宽量化到设备像素整数后，
/// 理论值 `pt×scale` 应为整数，但 f32 的 scale 往返（除法再乘法）可能有数 ULP 抖动 →
/// 朴素 ceil 会让位图宽 = 整数+1，而显示矩形（GPUI 绘制时 `snap_bounds` 用 round 取整）
/// = 整数 → 仍差 1px 触发双线性重采样发糊。对落在整数 ε（0.01px，远小于 1px、远大于
/// ULP）内的值先取整再 ceil，使位图宽恒等于显示设备宽（逐纹素 1:1）；真实分数（高度轴
/// 常见）不在 ε 内 → 完全不受影响。
#[inline]
fn snapped_bitmap_len(v: f32) -> u32 {
    let r = v.round();
    let v = if (v - r).abs() < 0.01 { r } else { v.ceil() };
    v.max(1.0) as u32
}

/// 页（PDF 点）→ 页位图（物理像素）尺寸。与 pdfium 栅格化的取整一致：`ceil` 且最小 1
/// （含 [`snapped_bitmap_len`] 的 1:1 稳定化）。worker 与 view 共用同一套数学 → 位图宽
/// 与显示矩形（设备像素取整）逐纹素对齐，瓦片/封面/高亮几何零漂移。
pub fn page_bitmap_size((pw, ph): (f32, f32), scale: f32) -> (u32, u32) {
    (
        snapped_bitmap_len(pw * scale),
        snapped_bitmap_len(ph * scale),
    )
}

/// 位图像素面积是否在硬上限内（对齐 [`config::pdf::COVER_AREA_BUDGET`]，px）。
///
/// `render_page` 整页位图与 `extract_image_pixels` 复制位图**共用同一常量口径**：
/// A4@4.0 ≈ 8M px（`page_bitmap_size` 实测 2380×3368）恰在预算（12M px）内**不误杀**，
/// 只拦病理性天文尺寸（否则 `w×h×4` 字节分配直接撑爆内存）。saturating 防 32 位环绕。
#[inline]
pub fn pixel_area_ok(w: u32, h: u32) -> bool {
    (w as u64).saturating_mul(h as u64) <= COVER_AREA_BUDGET
}

/// 面积闸「+1 行/列松弛」变体（`extract_image_pixels` 第二闸对**实际渲染尺寸**专用）。
///
/// 预算精确边界上的合法大图（如 4000×3000 = 12M px 恰打满），pdfium 双渲 +1 漂移
/// （4000×3001 > 12M）不应被精确闸误伤为 None——松弛量 = `max(w,h)`（等效多一行面积），
/// 兜住单轴 +1；请求侧第一闸（`budget_clamped_dims`）保持精确预算不变。松弛只放大
/// 一行，病态天文尺寸（10^10 px）即使 +1 行也照拒，内存仍被预算量级有界（最坏
/// 超宽缩档态 ≈ 2× 预算 = 96MB，不构成内存爆炸）。saturating 防 32 位环绕。
#[inline]
pub fn pixel_area_ok_with_row_slack(w: u32, h: u32) -> bool {
    let area = (w as u64).saturating_mul(h as u64);
    area <= COVER_AREA_BUDGET + (w.max(h) as u64)
}

/// 把请求尺寸按面积预算**反算到闸内的成比例尺寸**（保持宽高比，两轴同比缩小）。
///
/// - 面积本就 ≤ 预算 → 原样返回（正常 A4/大图复制路径尺寸稳定，不扰 1:1 语义）；
/// - 病理性天文尺寸 → 按 `sqrt(预算/面积)` 等比缩到闸内，返回缩档尺寸（优雅降级）；
/// - 任一边为 0 → `None`（无法缩档成有意义位图，调用方直接拒绝）。
///
/// 与 [`pixel_area_ok`] 双闸一致：缩档后面积必然 ≤ 预算（短轴触底时按预算反算长轴兜底）。
pub fn budget_clamped_dims(w: u32, h: u32) -> Option<(u32, u32)> {
    let area = (w as u64).saturating_mul(h as u64);
    if area == 0 {
        return None;
    }
    if area <= COVER_AREA_BUDGET {
        return Some((w, h));
    }
    // 反算缩放因子：factor = sqrt(预算/area)，两轴同缩保持比例（f64 免 u64 无开方）。
    let factor = (COVER_AREA_BUDGET as f64 / area as f64).sqrt();
    let mut nw = ((w as f64) * factor).floor().max(1.0) as u64;
    let mut nh = ((h as f64) * factor).floor().max(1.0) as u64;
    // 短轴钉 1 时比例缩放无法到闸内（超宽/超扁病态，√ 缩放至短轴触底仍超预算）：
    // 直接按预算反算长轴（保持短轴不变，乘积必 ≤ 预算）；浮点取整残留的 ±1 超预算同样
    // 在此归零。O(1) 有界，无逐像素循环。
    if nw.saturating_mul(nh) > COVER_AREA_BUDGET {
        if nw >= nh {
            nw = (COVER_AREA_BUDGET / nh.max(1)).max(1);
        } else {
            nh = (COVER_AREA_BUDGET / nw.max(1)).max(1);
        }
    }
    Some((nw as u32, nh as u32))
}

/// 页位图 → 卡片显示像素的分轴缩放（宽/高各自独立，吸收两轴 ceil 取整差异）。
/// 与 [`pt_to_local`] 的映射同源（local = 位图像素 × 此缩放），瓦片与文本高亮严格对齐。
pub fn bitmap_display_scales(bmp: (u32, u32), display_w: f32, disp_h: f32) -> (f32, f32) {
    (
        display_w / bmp.0.max(1) as f32,
        disp_h / bmp.1.max(1) as f32,
    )
}

/// 把 CSS px 对齐到**设备像素网格**（0.5 设备 px 内取整）：`(v×sf).round()/sf`。
/// 缩放换档的「子像素基线 pop」根因是页卡 top 落在分数设备位、旧底图（旧密度）拉伸与新
/// 栅格（ceil 整数位图像素）共用同一 PDF→设备映射时差 ≤1 设备 px/轴——把**绘制用**放置
/// 矩形先做一次设备网格对齐（旧拉伸与新栅格共享同一放置网格），换档映射差归零（或
/// ≤0.5 设备 px）。纯函数、幂等（同输入恒同输出），只改放置不打滚动/布局账。
pub fn snap_to_device(v: f32, scale_factor: f32) -> f32 {
    let sf = scale_factor.max(0.01);
    (v * sf).round() / sf
}

/// 封面密度是否**等效**于瓦片密度（D cap 刀刃稳健化的唯一判定点）：
/// ① 精确到 1e-4 内相等 → 同密度（既有语义）；② `coverscale` 被 [`config::pdf::COVER_SCALE_CAP`]
/// 截断（≈4.0）、而 `scale ∈ (cap, COVER_DENSITY_EPS_ZOOM]` → 视作同密度 cover-served：
/// 密度差 ≤1.5%、视觉无差，把分派决策从「cover(锐) vs 瓦片雾窗(糊)」的 cap 刀刃两翼翻转
/// 中稳定下来（×1.12 f32 不幂等导致同显百分比可落两翼 → 时锐时糊）。超出区间仍按精确
/// 判定（4.1+ 必须 1:1 瓦片，清晰度达标优先）。
pub fn cover_serves_scale(coverscale: f32, scale: f32) -> bool {
    if (coverscale - scale).abs() <= 1e-4 {
        return true;
    }
    (config::pdf::COVER_SCALE_CAP - coverscale).abs() <= 1e-4
        && scale > config::pdf::COVER_SCALE_CAP
        && scale <= config::pdf::COVER_DENSITY_EPS_ZOOM
}

/// 瓦片网格维度：(行数, 列数)。每格名义 [TILE_SIZE × TILE_SIZE]（页尾/侧不足则为残片）。
pub fn tile_grid(bmp: (u32, u32), tile_size: u32) -> (u32, u32) {
    if tile_size == 0 {
        return (1, 1);
    }
    (
        bmp.1.div_ceil(tile_size).max(1),
        bmp.0.div_ceil(tile_size).max(1),
    )
}

/// 某格在页位图中的实际绘制矩形 `(x0, y0, w, h)`（含出血、夹到页内）。
/// 整格在页外（越界的排/列守卫）→ `None`；否则 worker 按此矩形渲染、view 按此定位。
/// ⚠️ 这是两侧共用的**唯一权威实现**：改动必须同步 worker 输出与 view 放置语义。
pub fn tile_rect(
    col: u32,
    row: u32,
    tile_size: u32,
    bleed: u32,
    bmp: (u32, u32),
) -> Option<(u32, u32, u32, u32)> {
    if tile_size == 0 {
        return None;
    }
    let (bw, bh) = bmp;
    let x0 = col.saturating_mul(tile_size).saturating_sub(bleed);
    let y0 = row.saturating_mul(tile_size).saturating_sub(bleed);
    if x0 >= bw || y0 >= bh {
        return None;
    }
    let x1 = col
        .saturating_add(1)
        .saturating_mul(tile_size)
        .saturating_add(bleed)
        .min(bw);
    let y1 = row
        .saturating_add(1)
        .saturating_mul(tile_size)
        .saturating_add(bleed)
        .min(bh);
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    Some((x0, y0, x1 - x0, y1 - y0))
}

/// 判断可见瓦片区间 `(r0, c0)–(r1, c1)`（含端点，即调度侧
/// [`visible_tile_range`] + `TILE_MARGIN` 的同一组格）的**每一格**是否都已收敛在槽位
/// （O(格数) 纯函数）。
///
/// 供两处共用同一套几何：
/// - 「S 系列双密度混叠根治」原子换判定（render 侧）：新密度瓦片首次呈现必须整组
///   **一次**就位，绝不部分浮现盖在旧密度底图上；
/// - [`super::view::tiles::PdfViewer::retire_high_scale_covers`] 的「可见区已铺满」对账
///   （未铺满不回收 cover，防闪底）。
///
/// `tiles` 必须已是**当前密度**的瓦片集（调用方须先确认 `tile_scale ≈ render_scale`，
/// 否则栅格密度不对齐，本判定无意义）。
pub fn tile_range_all_in_slot(
    tiles: &HashMap<(u32, u32), TileSlot>,
    r0: u32,
    c0: u32,
    r1: u32,
    c1: u32,
) -> bool {
    for row in r0..=r1 {
        for col in c0..=c1 {
            if !tiles.contains_key(&(row, col)) {
                return false;
            }
        }
    }
    true
}

/// 给定卡片局部可见矩形（CSS px，已夹在页内），推算需要覆盖的瓦片网格区间，
/// 返回 `(row0, col0, row1, col1)`（含端点，已夹进网格）。`rect = (x0, y0, x1, y1)`。
/// 纵轴（滚动方向）外扩 `row_margin` 格、横轴（跨列/平移方向）外扩 `col_margin` 格以缓冲
/// 滚动/P1 ①：横向焊死 0，宽轴不再整列进钉住集。无交点 → `None`。
pub fn visible_tile_range_axis(
    bmp: (u32, u32),
    display_w: f32,
    disp_h: f32,
    rect: (f32, f32, f32, f32),
    tile_size: u32,
    row_margin: u32,
    col_margin: u32,
) -> Option<(u32, u32, u32, u32)> {
    if tile_size == 0 {
        return None;
    }
    let (rx0, ry0, rx1, ry1) = rect;
    if rx1 <= rx0 || ry1 <= ry0 {
        return None;
    }
    let (sx, sy) = bitmap_display_scales(bmp, display_w, disp_h);
    if sx <= 0.0 || sy <= 0.0 {
        return None;
    }
    let (rows, cols) = tile_grid(bmp, tile_size);
    let clamp = |v: i64, max: i64| -> u32 { v.clamp(0, max.max(0)) as u32 };
    let cell = tile_size as i64;
    // 位图像素区间 → 网格格号区间（含端点）
    let c0 = ((rx0 / sx) / cell as f32).floor() as i64 - (col_margin as i64);
    let c1 = ((rx1 / sx - 1.0).max(0.0) / cell as f32).floor() as i64 + (col_margin as i64);
    let r0 = ((ry0 / sy) / cell as f32).floor() as i64 - (row_margin as i64);
    let r1 = ((ry1 / sy - 1.0).max(0.0) / cell as f32).floor() as i64 + (row_margin as i64);
    let c0 = clamp(c0, cols as i64 - 1);
    let c1 = clamp(c1, cols as i64 - 1);
    let r0 = clamp(r0, rows as i64 - 1);
    let r1 = clamp(r1, rows as i64 - 1);
    if r0 > r1 || c0 > c1 {
        return None;
    }
    Some((r0, c0, r1, c1))
}

/// 兼容入口：等 margin 双轴（旧约束语义不变，仅纯函数测试沿用；生产路径一律走
/// `visible_tile_range_axis`）。测试专用，避免非测试构建 dead_code 告警。
#[cfg(test)]
pub fn visible_tile_range(
    bmp: (u32, u32),
    display_w: f32,
    disp_h: f32,
    rect: (f32, f32, f32, f32),
    tile_size: u32,
    margin: u32,
) -> Option<(u32, u32, u32, u32)> {
    visible_tile_range_axis(bmp, display_w, disp_h, rect, tile_size, margin, margin)
}

/// 「整页锐化封面」是否廉价可整渲：该密度下整页位图字节 ≤ [`COVER_SIZE_BUDGET`] 时，
/// 一个 `render_page` 即铺满首屏（再把瓦片叠上来精修）；超预算（高倍缩放/超大页）→
/// 纯瓦片路径。
pub fn cover_affordable(page_pt: (f32, f32), scale: f32) -> bool {
    if scale < 0.2 || page_pt.0 <= 0.0 || page_pt.1 <= 0.0 {
        return false;
    }
    let pixels = (page_pt.0 as f64) * (page_pt.1 as f64) * (scale as f64) * (scale as f64);
    // P1 ⑤：位图像素面积硬上限（保底护栏，防病理性页把 cover 池顶爆）。
    if pixels > config::pdf::COVER_AREA_BUDGET as f64 {
        return false;
    }
    let bytes = pixels * 4.0;
    bytes <= config::pdf::COVER_SIZE_BUDGET as f64
}

/// 封面渲染密度：**满显示密度**（`render_scale`，物理像素 1:1）——实测整页渲染
/// ~10-50ms 且 Open 结构解析已近免费，首帧封面一渲即锐利、文字无需等瓦片精修；
/// 仅对病理性超高倍率用 [COVER_SCALE_CAP]（4.0）护栏（更高密度由预算/瓦片兜底）。
/// 1x / 2x-DPI 屏均得到与其屏幕密度完全一致的封面，缩放变化自会惰性重渲。
pub fn cover_scale(page_pt: (f32, f32), page_width: f32, scale_factor: f32) -> f32 {
    render_scale(page_width, scale_factor, page_pt.0).min(config::pdf::COVER_SCALE_CAP)
}

/// 页内 PDF 点坐标 → 页卡**局部**显示 px（原点 = 卡片左上角，Y 轴已翻转为"向下为正"）。
///
/// 关键点（文本层高亮与渲染字形严格对齐的基础）：
/// - **局部坐标**：只做"页内 → 卡片内"变换，**不放**滚动/分栏/居中偏移——`page_tops`、
///   列左偏移与 scroll 偏移由 P2 的 view 层叠加，否则滚动时高亮会漂移。
/// - **走位图真实尺寸**：`scale`（px/pt，见 [`render_scale`]）先把 pt 换算成位图像素，
///   再按"位图 → 卡片拉伸比"（`display_w / 位图宽`）映射到显示 px，`ceil` 亚像素偏差消掉。
/// - **Y 翻转**：PDF 原点在页左下、Y 向上；显示坐标原点在卡片左上、Y 向下
///   （`(page_pt_h - py)`）。
///
/// 边界（局部坐标）：pt 页左上 `(0, page_pt_h)` → `(0, 0)`；
/// pt 页右下 `(page_pt_w, 0)` → `(display_w, display_height(...))`。
pub fn pt_to_local(
    px: f32,
    py: f32,
    page_pt_w: f32,
    page_pt_h: f32,
    display_w: f32,
    scale: f32,
) -> (f32, f32) {
    let scale = scale.max(0.01);
    let (bmp_w, bmp_h) = page_bitmap_size((page_pt_w, page_pt_h), scale);
    let disp_h = display_height((page_pt_w, page_pt_h), display_w);
    let bx = px * scale;
    let by = (page_pt_h - py) * scale;
    (
        bx * (display_w / bmp_w as f32),
        by * (disp_h / bmp_h as f32),
    )
}

/// 页卡局部 px → 页内 PDF 点坐标（[`pt_to_local`] 的逆；命中测试：鼠标 → 字符索引）。
pub fn local_to_pt(
    lx: f32,
    ly: f32,
    page_pt_w: f32,
    page_pt_h: f32,
    display_w: f32,
    scale: f32,
) -> (f32, f32) {
    let scale = scale.max(0.01);
    let (bmp_w, bmp_h) = page_bitmap_size((page_pt_w, page_pt_h), scale);
    let disp_h = display_height((page_pt_w, page_pt_h), display_w);
    let bx = lx * (bmp_w as f32 / display_w);
    let by = ly * (bmp_h as f32 / disp_h);
    (bx / scale, page_pt_h - by / scale)
}

/// 页列左偏移：wrap 容器内居中对齐（列宽 = 页宽）。P2 命中测试与渲染同式。
pub fn column_left(wrap_w: f32, page_width: f32) -> f32 {
    ((wrap_w - page_width) / 2.0).max(0.0)
}

// ---- 网格视图（Grid）几何：页卡网格布局的纯函数（与页列视图同源数学，便于单测）----

/// 网格视图的页卡布局结果（不可变纯数据，与页列视图的 `tops` 缓存姿态一致）。
///
/// 坐标系与页列视图一致：内容原点 = 左上角、Y 轴向下为正；`padding` 为网格四周外留白。
/// `row_top`/`row_page_start`/`cell_h` 为一次性算好的直读缓存，命中/可见行/卡片矩形
/// 全靠本结构推导，不与 DOM/view 再各算一遍——保证滚动位置、命中判定与卡片实际放置
/// 严格一致（`row_index_at_y`/`col_slot_at_x` 原语与 `cell_rect` 用同一套加法，view 侧插入
/// 位 `compute_insert_index` 也基于这两个原语，不再重复手写二分/pitch 几何）。
#[derive(Clone, Debug)]
pub struct GridLayout {
    /// 列数（`cols = max(1, floor((viewport_width − 2·padding + gap) / (card_width + gap)))`）。
    pub cols: usize,
    /// 行数（`ceil(pages.len()/cols)`；0 页时 = 0）。
    pub rows: usize,
    /// 实际卡宽（**跟随请求卡宽**，缩放精调连续感知）：正常 = 请求卡宽 `card_width`
    /// （cols≥2 时 floor 已保证整行放得下 `cols·card_w + (cols−1)·g ≤ vw − 2p`，行尾
    /// 余量右侧留白）；唯一例外 = 单列且请求卡宽超内容区（`viewport_width − 2·padding`）
    /// 时夹到内容区宽，防横向溢出。
    pub cell_w: f32,
    /// 纵向行距（行间 gap，纵向语义不变；横向列距由 `col_gap` 负责）。
    pub gap: f32,
    /// 实际渲染列间距（≥ 入参 `gap`；「余量拉伸填满」优先，封顶后 = [`GRID_GAP_MAX`]；
    /// cols=1 时 = 入参 `gap` 无语义影响）。
    pub col_gap: f32,
    /// 左居留白（横向余量摊不下的部分左右对称居中；恰好填满 = 0；cols=1 单列居中用）。
    pub left_pad: f32,
    /// 网格四周外留白。
    pub padding: f32,
    /// 每行顶 y（含外 `padding`；`len = rows`，严格递增）。行高 = 该行内最大 `cell_h`。
    pub row_top: Vec<f32>,
    /// 每行起始页下标（`len = rows + 1`，末项 = 总页数）。
    pub row_page_start: Vec<usize>,
    /// 每页卡高（保持页纵横比，`cell_h[i] = cell_w·(页高/页宽)`；`len = pages`）。
    pub cell_h: Vec<f32>,
    /// 网格总高（上/下外 `padding` + 各行高与行间 `gap`）。
    pub total_height: f32,
}

/// 列间距封顶值 = 3 × GRID_GAP（36px；可调）。列数少、横向余量过大时列间距不无限
/// 拉大（避免离得过分稀疏）：封顶后剩余留白改左右对称居中（`left_pad`）。
const GRID_GAP_MAX: f32 = 36.0;

/// 计算网格视图布局（纯函数，与 [`compute_layout`] 同姿势）：`pages` 为每页 `(宽, 高)` pt
/// 尺寸，`card_width` 为请求卡宽。**新语义「卡宽跟随请求」**：实际 `cell_w` = 请求卡宽
/// （不再按填满视口反推）——Ctrl+滚轮缩放精调时，卡宽每步都连续变化、缩放逐档可感知，
/// 同列数区间内卡大小不再被冻结；行尾放不下时右侧留白（多余空间不再摊给卡）。
///
/// - 列数 = 能并排几列：`cols = max(1, floor((vw − 2·padding + gap) / (card_width + gap)))`；
/// - `cell_w` = `card_width`，唯一例外 = 单列且 `card_width` > 内容区（`vw − 2·padding`）
///   时夹到内容区宽（单卡填视口、防横向溢出）；窄视口（内容区 < 0）夹到 ≥0 防负宽退化；
/// - **列间距拉伸填满**（横向去右侧留白）：`col_gap = min(needed, GAP_MAX)`，
///   `needed = (C − cols·cell_w)/(cols−1)`（出发点是内容区宽、列数/卡宽固定后把余量全摊进
///   列间距，末列右缘贴齐内容区右缘）；余量过大（列数少、needed > GAP_MAX）时列间距封顶、
///   剩余留白左右对称居中（`left_pad = (C − cols·cell_w − (cols−1)·col_gap)/2`）；
///   cols=1 → `col_gap = gap`、`left_pad = (C − cell_w)/2`（单列居中、`needed` 无定义）；
/// - `cell_h` 保持各页纵横比（`宽 ≤ 0` 时按比例 1 处理，与 [`display_height`] 同源）；
/// - 行内页从左到右填满 `cols` 后换行，最后一行可不填满（部分行）；行高 = 该行最大卡高；
/// - 0 页 → `rows = 0`、`row_top`/`row_page_start` 空、`total_height = 0`（调用方按空布局处理）。
pub fn grid_layout(
    pages: &[(f32, f32)],
    card_width: f32,
    gap: f32,
    padding: f32,
    viewport_width: f32,
) -> GridLayout {
    // cols：能并排几列（分子 +gap 是「末列之后不留 gap」的放置会计，分母 = 卡宽 + 列间 gap）。
    let denom = card_width + gap;
    let cols = if denom > 0.0 {
        (((viewport_width - 2.0 * padding + gap) / denom).floor()).max(1.0) as usize
    } else {
        1
    };
    // cell_w：新语义 = 跟随请求卡宽 `card_width`（floor 已在 cols ≥ 1 时保证
    // `cols·card_w + (cols−1)·g ≤ vw − 2p`，整行放得下、行尾余量留白）；唯一例外 =
    // cols 被夹到 1 且请求卡宽超内容区（vw − 2p）→ 单卡填视口防横向溢出；窄视口
    // （vw − 2p < 0）夹到 ≥0 防负宽退化、无除零。
    let content_w = (viewport_width - 2.0 * padding).max(0.0);
    let cell_w = if cols <= 1 {
        card_width.min(content_w).max(0.0)
    } else {
        card_width.max(0.0)
    };
    // col_gap / left_pad：横向余量优先「全摊进列间距」填满内容区右缘；余量过大（needed 超封顶）
    // 时列间距封顶、剩余留白左右对称居中。cols=1 时 needed 无定义 → 单列居中、列距 = 入参 gap。
    // 演算（K143/K177）：needed = (C − cols·cell_w)/(cols−1)，由 cols 公式可证 always ≥ gap ≥ 0；
    //   cols ≥ 2：col_gap = min(needed, GRID_GAP_MAX)；needed ≤ GAP_MAX → left_pad = 0（填满）；
    //             needed > GAP_MAX → left_pad = (C − cols·cell_w − (cols−1)·col_gap)/2（封顶居中）；
    //   cols == 1：col_gap = gap、left_pad = (C − cell_w)/2（cell_w 已夹 ≤ C → left_pad ≥ 0）。
    let (col_gap, left_pad) = if cols >= 2 {
        let needed = (content_w - cols as f32 * cell_w) / (cols - 1) as f32;
        let col_gap = needed.min(GRID_GAP_MAX);
        let left_pad = if needed <= GRID_GAP_MAX {
            0.0
        } else {
            (content_w - cols as f32 * cell_w - (cols - 1) as f32 * col_gap) / 2.0
        };
        (col_gap, left_pad)
    } else {
        (gap, (content_w - cell_w) / 2.0)
    };
    let n = pages.len();
    let rows = n.div_ceil(cols);

    // 每页卡高：保持纵横比（`cell_h[i] = cell_w × (高/宽)`；宽 ≤ 0 按比例 1）。
    let cell_h: Vec<f32> = pages
        .iter()
        .map(|(w, h)| if *w > 0.0 { cell_w * (h / w) } else { cell_w })
        .collect();

    // row_page_start[r] = 第 r 行首张页下标（r = rows 时为总页数 n）。
    let row_page_start: Vec<usize> = (0..=rows).map(|r| (r * cols).min(n)).collect();

    // row_top：从 padding 起逐行累加「行高 + gap」（行高 = 该行内最大 cell_h）。
    let mut row_top = Vec::with_capacity(rows);
    let mut y = padding;
    for r in 0..rows {
        row_top.push(y);
        let s = row_page_start[r];
        let e = row_page_start[r + 1];
        let row_h = cell_h[s..e].iter().copied().fold(0.0f32, f32::max);
        y += row_h + gap;
    }
    // total_height = padding + Σ行高 + (rows−1)·gap + padding（去掉循环尾部多计的 gap）。
    let total_height = if rows == 0 { 0.0 } else { y - gap + padding };

    GridLayout {
        cols,
        rows,
        cell_w,
        gap,
        col_gap,
        left_pad,
        padding,
        row_top,
        row_page_start,
        cell_h,
        total_height,
    }
}

/// 网格缩放**列数钳制**：返回使列数恒在 `[least_cols, most_cols]` 内的缩放区间
/// `(z_min, z_max)`（`z_min` = 缩小端下限、`z_max` = 放大端上限；任意
/// `zoom ∈ [z_min, z_max]` ⇒ `least_cols ≤ cols(zoom) ≤ most_cols`）。
///
/// 语义对齐调用侧常量：本仓 `least_cols = GRID_MAX_COLS(2)`（放大极限正好 2 列，不可能到
/// 1 列）、`most_cols = GRID_MIN_COLS(9)`（缩小极限正好 9 列，不可能到 10+ 列）。网格缩放
/// 既有阅读共享的 `[ZOOM_MIN, ZOOM_MAX]` 不再钳网格（`set_grid_zoom` / input.rs 网格轮臂 /
/// render 每帧归一化改走本函数，随当前视口宽动态钳到 2/9 列）。
///
/// 演算（K143/K177）：`cols(zoom) = max(1, floor((C + g)/(base_card·zoom + g)))`，
/// `C = viewport_width − 2·padding`（内容区宽）、`g = gap`：
/// - 放大端 `cols ≥ k`（`k` = least_cols）：`floor((C+g)/(cell+g)) ≥ k ⇔ k·cell + (k−1)·g ≤ C
///   ⇔ cell ≤ (C − (k−1)·g)/k` ⇒ `z_max = (((C − (k−1)·g)/k) − 0.5)/base_card`（边界**再让
///   0.5px**，与缩小端 `+0.5px` 对称，防浮点把 `base_card × z_max` 抬过刀锋边界落到 k−1 列，
///   见下方实现注释；此值处 cols 恰 = k，以 k=2 验算 `(C+g)/((C−g)/2 + g) = 2`）；
/// - 缩小端 `cols ≤ m`（`m` = most_cols）：`floor((C+g)/(cell+g)) ≤ m ⇔ (m+1)·cell + m·g > C
///   ⇔ cell > (C − m·g)/(m+1)`；`+0.5px` 严格离开 open 边界，防浮点圈回 10+ 列；
///   `z_min = ((C − m·g)/(m+1) + 0.5)/base_card`。
///
/// 退化兜底（真实窗口不会触发，仅防病态输入）：负/极窄视口（`C < 0`）两端夹到极小正数
/// 防倒挂；参数倒挂（`z_min > z_max`）两端各取均值锁同一点，钳内 zoom 无定义但不产出
/// 越界病态值。
pub fn grid_zoom_limits_for_cols(
    base_card: f32,
    gap: f32,
    padding: f32,
    viewport_width: f32,
    least_cols: f32,
    most_cols: f32,
) -> (f32, f32) {
    let c = viewport_width - 2.0 * padding;
    // 放大端边界 = (C − (k−1)·g)/k 是**刀锋**：`fl(base_card × z_max)` 的 f32 舍入可能把
    // cell 抬过边界（评审位级实证 vw=402：`(179/280)×280` = 179.00002 > 179 → cols=1，违
    // 背「恒 2 列」）。从边界**再让 −0.5px**（与缩小端 +0.5px 对称）——`fl(base_card×z_max)
    // ≤ boundary` 恒成立，0.5px/280 ≈ 0.18% 不可见。
    let z_max = ((c - (least_cols - 1.0) * gap) / least_cols - 0.5) / base_card;
    let z_min = ((c - most_cols * gap) / (most_cols + 1.0) + 0.5) / base_card;
    const TINY: f32 = 1e-3;
    let (mut z_min, mut z_max) = (z_min.max(TINY), z_max.max(TINY));
    if z_min > z_max {
        let mid = (z_min + z_max) / 2.0;
        z_min = mid;
        z_max = mid;
    }
    (z_min, z_max)
}

impl GridLayout {
    /// 第 `r` 行的行高 = 该行内最大卡高（派生值，不入字段保持结构最小）。
    fn row_height(&self, r: usize) -> f32 {
        let s = self.row_page_start[r];
        let e = self.row_page_start[r + 1];
        self.cell_h[s..e].iter().copied().fold(0.0f32, f32::max)
    }

    /// 第 `i` 页卡的放置矩形 `(x, y, w, h)`（内容坐标系，见 [`GridLayout`]）；越界 → `None`。
    /// `x = padding + left_pad + (i % cols)·(cell_w + col_gap)`，`y = row_top[i / cols]`，
    /// 宽 `cell_w`、高 `cell_h[i]`（y/高不受横向拉伸影响）。
    pub fn cell_rect(&self, i: usize) -> Option<(f32, f32, f32, f32)> {
        if i >= self.cell_h.len() || self.cols == 0 {
            return None;
        }
        let r = i / self.cols;
        if r >= self.rows {
            return None;
        }
        let c = i % self.cols;
        let x = self.padding + self.left_pad + c as f32 * (self.cell_w + self.col_gap);
        let y = self.row_top[r];
        Some((x, y, self.cell_w, self.cell_h[i]))
    }

    /// 可见行区间（含端点，闭区间）：视口 `[offset, offset + viewport_h]` 命中的所有行，
    /// 二分行顶直读缓存 O(log rows)，与 [`keep_window`] 同姿势；0 行或视口完全落在内容
    /// 之外（上/下/悬在行间 gap 里）→ `None`。`viewport_h ≤ 1` 回退默认 720（首帧前口径）。
    pub fn first_visible_grid_row(&self, offset: f32, viewport_h: f32) -> Option<(usize, usize)> {
        if self.rows == 0 {
            return None;
        }
        let vh = if viewport_h > 1.0 { viewport_h } else { 720.0 };
        let top = offset.max(0.0);
        let bottom = top + vh;

        // 首行 = 第一个「行底 > 视口顶」的行（`row_top` 严格递增 → 行底单调，可二分）。
        let mut lo = 0usize;
        let mut hi = self.rows;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.row_top[mid] + self.row_height(mid) <= top {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        if lo == self.rows {
            return None; // 已滚过末行：视口完全在内容之下
        }
        let first = lo;

        // 末行 = 最后一个「行顶 < 视口底」的行。
        let mut lo = 0usize;
        let mut hi = self.rows;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.row_top[mid] < bottom {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        if lo == 0 || lo - 1 < first {
            return None; // 视口完全在内容之上，或仅悬在行间 gap 里（无任何行相交）
        }
        Some((first, lo - 1))
    }

    /// **行二分原语**：最后一个「行顶 ≤ y」的行下标（`row_top` 严格递增可二分；0 行 → 0）。
    /// 供 2D 命中 / 插入位实现共用同一套行几何（view 侧 `compute_insert_index` 与既有
    /// 命中语义都基于本原语，不再在 view 侧重复手写行二分）。调用方按需自行判定
    /// padding / 行间 gap 的语义（本原语只回答「y 属于哪一行」）。
    pub fn row_index_at_y(&self, y: f32) -> usize {
        let mut lo = 0usize;
        let mut hi = self.rows;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.row_top[mid] <= y {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo.saturating_sub(1)
    }

    /// **列槽原语**：由内容 x 算整数列槽 `round((x − padding − left_pad) / (cell_w + col_gap))`
    /// （≥0；退化视口 pitch ≤ 0 → 0）。`round` 语义 = 卡左半/右半、列间 gap、右留白都归到最近
    /// 整数槽（gap 中点即阈值）；`x < padding + left_pad`（左留白内）→ 负值 round 后取 0。
    /// 供 2D 命中 / 插入位实现共用同一套列 pitch 几何（view 侧 `compute_insert_index` 与既有
    /// 命中语义都基于本原语）。
    pub fn col_slot_at_x(&self, x: f32) -> usize {
        let pitch = self.cell_w + self.col_gap;
        if pitch <= 0.0 {
            return 0; // 退化视口（卡宽 0）无法按列定位
        }
        (((x - self.padding - self.left_pad) / pitch)
            .round()
            .max(0.0)) as usize
    }
}

// ---- 分页阅读视图（Paged）几何：单页/双页（非网格），纯 f32 数学 ----

/// 页平面上的轴对齐矩形（纯数据，f32）。本文件 Grid 系用 4 元组；Paged 的命中/吸附/
/// 测试需要命名字段与 Copy，故自建最小结构（不引入外部依赖，本仓无既有 Rect/Point）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// 单页/双页阅读分页布局（`cols ∈ {1,2}`；每行 contain-fit，页高 ≤ 视口高）。
///
/// 行内页宽 = min(列宽上限 `cap_w`，使「行内最竖页」恰一屏高的宽 `viewport_h·min_ar`)——
/// 页高永不超视口高；行按页高堆叠，行间 `v_gap`、同行左右页由 `h_gap` 分隔、上下/左右
/// `padding`。`row_top` /
/// `row_page_start` / `cell_w_per_row` / `cell_h` / `left_pad_per_row` 为一次性直读缓存，
/// `cell_rect` / 可见行 / 命中 / 吸附全由本结构推导（与 [`GridLayout`] 同一"结构即几何"姿态）。
///
/// 坐标系：内容原点 = 左上角、Y 轴向下为正。**`scroll_y` 为负偏移**：视口顶内容 y =
/// `−scroll_y`，`scroll_y = 0` = 停在内容顶，向下翻页使 `scroll_y` 变负——与网格/页列
/// 用的「正偏移」是同一视口的参数差一正负号（详见 [`PagedLayout::first_visible_row`]）。
#[derive(Clone, Debug)]
pub struct PagedLayout {
    /// 列数（1=单列（内部/测试用），2=双页（Facing）；构造时 0→1、>2→2 归一）。
    pub cols: usize,
    /// 行数（`ceil(pages.len()/cols)`；0 页时 = 0）。
    pub rows: usize,
    /// 水平页间距（同行左页↔右页之间，×zoom）。
    pub h_gap: f32,
    /// 垂直行间距（上页↔下页之间，×zoom）。
    pub v_gap: f32,
    /// 上下/左右外白。
    pub padding: f32,
    /// 每行顶 y（含外 `padding`；`len = rows`，严格递增）。行高 = 该行内最大 `cell_h`。
    pub row_top: Vec<f32>,
    /// 每行起始页下标（`len = rows + 1`，末项 = 总页数，方便取区间）。
    pub row_page_start: Vec<usize>,
    /// 每行统一页宽（`len = rows`；行内各页同宽）。
    pub cell_w_per_row: Vec<f32>,
    /// 每页高（保持页纵横比，`len = pages`）。
    pub cell_h: Vec<f32>,
    /// 每行水平左留白（行内居中用；`len = rows`，列 x 起点 = padding + 此项）。
    pub left_pad_per_row: Vec<f32>,
    /// 所有行内容宽的最大值（供容器宽）。
    pub spread_max: f32,
    /// 上/下外 `padding` + Σ各“行高 + 行间 gap”的总高。
    pub total_height: f32,
}

impl PagedLayout {
    /// 第 `r` 行的行高 = 该行内最大页高（派生值，不入字段保持结构最小）。
    /// `pub`：view 层 `reading_progress`/`restore` 分页分支取行高算行内比例（K177 手算）。
    pub fn row_height(&self, r: usize) -> f32 {
        let s = self.row_page_start[r];
        let e = self.row_page_start[r + 1];
        self.cell_h[s..e].iter().copied().fold(0.0f32, f32::max)
    }

    /// 第 `page` 页的放置矩形 `(x, y, w, h)`（内容坐标系，见 [`PagedLayout`]）；越界/空布局 → `None`。
    /// `x = padding + left_pad_per_row[r] + c·(cell_w_per_row[r] + h_gap)`，`y = row_top[r]`，
    /// `w = cell_w_per_row[r]`（行内统一页宽）、`h = cell_h[page]`（每页自身纵横比）。
    pub fn cell_rect(&self, page: usize) -> Option<Rect> {
        if self.rows == 0 || self.cols == 0 || page >= self.cell_h.len() {
            return None;
        }
        let r = page / self.cols;
        let c = page % self.cols;
        let w = self.cell_w_per_row[r];
        // 演算（K177）：列原点 = padding + left_pad[r]；列 c 再平移 c·pitch（pitch = 页宽 + h_gap）。
        let x = self.padding + self.left_pad_per_row[r] + c as f32 * (w + self.h_gap);
        Some(Rect {
            x,
            y: self.row_top[r],
            w,
            h: self.cell_h[page],
        })
    }

    /// 可见行区间（含端点，闭区间）：视口 `[−scroll_y, −scroll_y + viewport_h]`（**负偏移**，
    /// 视口顶内容 y = `−scroll_y`）命中的所有行，二分行顶直读缓存 O(log rows)，与
    /// [`GridLayout::first_visible_grid_row`] 同姿势（正偏移 `offset` 等价于 `−scroll_y`）。
    /// 0 行或视口完全落在内容之外（上/下/悬在行间 gap 里）→ `None`；`viewport_h ≤ 1` 回退
    /// 默认 720（首帧前口径，与既有网格一致）。
    ///
    /// 行 r 可见 ⇔ `row_bottom > −scroll_y ∧ row_top < −scroll_y + viewport_h`
    /// ⇔ `row_top[r] + scroll_y + row_h[r] > 0 ∧ row_top[r] + scroll_y < viewport_h`。
    pub fn first_visible_row(&self, scroll_y: f32, viewport_h: f32) -> Option<(usize, usize)> {
        if self.rows == 0 {
            return None;
        }
        let vh = if viewport_h > 1.0 { viewport_h } else { 720.0 };
        let top = (-scroll_y).max(0.0); // 视口顶内容 y（负偏移；钳掉"滚过头顶"）
        let bottom = top + vh;

        // 首行 = 第一个「行底 > 视口顶」的行（`row_top` 严格递增 → 行底单调，可二分）。
        let mut lo = 0usize;
        let mut hi = self.rows;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.row_top[mid] + self.row_height(mid) <= top {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        if lo == self.rows {
            return None; // 已滚过末行：视口完全在内容之下
        }
        let first = lo;

        // 末行 = 最后一个「行顶 < 视口底」的行。
        let mut lo = 0usize;
        let mut hi = self.rows;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.row_top[mid] < bottom {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        if lo == 0 || lo - 1 < first {
            return None; // 视口完全在内容之上，或仅悬在行间 gap 里（无任何行相交）
        }
        Some((first, lo - 1))
    }

    /// 行二分原语：最后一个「行顶 ≤ y」的行下标（`row_top` 严格递增可二分）；0 行 → `None`。
    pub fn row_index_at_y(&self, y: f32) -> Option<usize> {
        if self.rows == 0 {
            return None;
        }
        let mut lo = 0usize;
        let mut hi = self.rows;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.row_top[mid] <= y {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        Some(lo.saturating_sub(1))
    }

    /// 命中定位：内容坐标 (x, y) → `(页号, 页内局部 x, 页内局部 y)`；间隙/留白/越界/空布局 → `None`。
    ///
    /// 行由 [`row_index_at_y`] 定位；列用 **floor** 求候选槽
    /// `c = floor((x − (padding + left_pad_r)) / (cell_w_per_row[r] + h_gap))` 并在 `[0, cols)`
    /// 且落在本行页区间（`i = r·cols + c < n`）内，随后用 [`cell_rect`] 做**包含性复核**
    /// （点须落 `[0, w)×[0, h)` 内）——间隙/留白/越界点都过不了复核 → `None`。
    ///
    /// 注：**不用 round** —— round 的槽中点 `(c+0.5)·pitch` 会从卡片内部切过
    /// （如 w=479.5/h_gap=1（cap_w=(960−1)/2=479.5）⇒ pitch=480.5、round 槽界 = x0+240.25
    /// （=0.5×pitch），而真卡右缘 = x0+479.5），把卡右半的真命中误判到
    /// 邻列 / 间隙；floor 给出「含卡右缘前」的真列，再经包含性复核即得正确命中。
    /// （对比 [`GridLayout::col_slot_at_x`] 的 round 是"最近插槽"语义，用途不同。）
    pub fn page_at(&self, x: f32, y: f32) -> Option<(usize, f32, f32)> {
        if self.rows == 0 {
            return None;
        }
        let r = self.row_index_at_y(y)?;
        let pitch = self.cell_w_per_row[r] + self.h_gap;
        if pitch <= 0.0 {
            return None; // 退化视口（页宽 0）无法按列定位
        }
        let x0 = self.padding + self.left_pad_per_row[r];
        let cf = ((x - x0) / pitch).floor(); // 候选列（可为负 → 左留白）
        if cf < 0.0 || cf as usize >= self.cols {
            return None;
        }
        let i = r * self.cols + cf as usize;
        let rect = self.cell_rect(i)?; // i 已在行/列/页界内复核过，必有矩形
        if x >= rect.x && x < rect.x + rect.w && y >= rect.y && y < rect.y + rect.h {
            Some((i, x - rect.x, y - rect.y))
        } else {
            None
        }
    }

    /// 吸附目标：返回「行顶距视口顶最近」的行号 r 与其内容 y（= `row_top[r]`）。
    ///
    /// 语义：视口顶内容 y = `−scroll_y`；`r = argmin_r |(−scroll_y) − row_top[r]|`；
    /// 首行之上（或空布局）→ 回落 `(0, 0.0)`；平局取行号小者（确定性）。
    ///
    /// 已休眠（K24）：生产 0 消费——Facing 已改连续自由滚动（不吸附行顶），view 层
    /// `paged_snap_target`/步进在 2026-11-20 连滚战役已删；仅本模块单测保留（行为锚）。
    pub fn snap_target(&self, scroll_y: f32) -> (usize, f32) {
        if self.rows == 0 {
            return (0, 0.0);
        }
        let top = -scroll_y; // 视口顶内容 y
        let mut best = 0usize;
        let mut best_d = f32::INFINITY;
        for r in 0..self.rows {
            let d = (top - self.row_top[r]).abs();
            if d < best_d {
                best_d = d;
                best = r;
            }
        }
        (best, self.row_top[best])
    }
}

/// 计算阅读分页布局（纯函数，与 [`grid_layout`] 同姿势）：`pages` 为每页 `(宽, 高)` pt
/// （只用纵横比），`viewport_w/h` 视口、`cols ∈ {1,2}`（0→1、>2→2）、`h_gap` 水平页间距
/// （双页同行左页↔右页之间）、`v_gap` 垂直行间距（上页↔下页之间）、`padding` 四周外白。
///
/// 数学（每行 r，页区间 s..=e；K177 演算）：
/// 1. 列宽上限 `cap_w = max(0, (max(0, viewport_w − 2·padding) − (cols==2 ? h_gap : 0)) / cols)`
///    （防负宽：内容区先夹 ≥0，减列间 h_gap 后再夹 ≥0）；
/// 2. 行内最竖页 `min_ar = min_{i∈s..e}(w_i/h_i)`（跳过 w≤0 ∨ h≤0 退化页，全退化 → 1）；
///    `fit_w = viewport_h · min_ar`（恰一屏高）；页宽 `cell_w_r = min(cap_w, fit_w).max(0)`；
/// 3. 每页高 `cell_h[i] = cell_w_r·(h_i/w_i)`（w≤0 → cell_w_r，与 [`display_height`] 同源兜底）；
/// 4. 行宽统一按 cols 满行 `spread_r = cols·cell_w_r + (cols−1)·h_gap`（partial 行页靠列左、
///    宽度与满行一致，不重算居中基线），`left_pad_r = max(0, (content_w − spread_r)/2)`；
///    `spread_max = max_r(spread_r)`；
/// 5. `row_top[0] = padding`；`row_top[r+1] = row_top[r] + row_h[r] + v_gap`，
///    `row_h[r] = max_{i∈s..e} cell_h[i]`；
/// 6. `total_height = (rows==0) ? 0 : row_top[rows-1] + row_h[rows-1] + padding`
///    （= padding + Σ(行高+v_gap) + padding，删循环尾部多计的 v_gap）。
pub fn paged_layout(
    pages: &[(f32, f32)],
    viewport_w: f32,
    viewport_h: f32,
    cols: usize,
    h_gap: f32,
    v_gap: f32,
    padding: f32,
) -> PagedLayout {
    let cols = match cols {
        0 | 1 => 1,
        _ => 2,
    };
    let n = pages.len();
    let rows = n.div_ceil(cols);
    // 空布局：rows=0、total=0；row_page_start 留端点 [0]（len = rows+1），几何方法一律安全空。
    if rows == 0 {
        return PagedLayout {
            cols,
            rows: 0,
            h_gap,
            v_gap,
            padding,
            row_top: Vec::new(),
            row_page_start: vec![0],
            cell_w_per_row: Vec::new(),
            cell_h: Vec::new(),
            left_pad_per_row: Vec::new(),
            spread_max: 0.0,
            total_height: 0.0,
        };
    }
    let content_w = (viewport_w - 2.0 * padding).max(0.0);
    let cap_w = ((content_w - if cols == 2 { h_gap } else { 0.0 }) / cols as f32).max(0.0);
    let vh = viewport_h.max(0.0);

    let row_page_start: Vec<usize> = (0..=rows).map(|r| (r * cols).min(n)).collect();
    let mut cell_h = vec![0.0f32; n];
    let mut row_top = Vec::with_capacity(rows);
    let mut cell_w_per_row = Vec::with_capacity(rows);
    let mut left_pad_per_row = Vec::with_capacity(rows);
    let mut spread_max = 0.0f32;
    let mut y = padding;
    for r in 0..rows {
        let s = row_page_start[r];
        let e = row_page_start[r + 1];
        // 行内最竖页（w/h 最小 = 按宽度限高时最高者）；跳过退化页，全退化回退比例 1。
        let mut min_ar = f32::INFINITY;
        for &(w, h) in &pages[s..e] {
            if w > 0.0 && h > 0.0 {
                min_ar = min_ar.min(w / h);
            }
        }
        let min_ar = if min_ar.is_finite() { min_ar } else { 1.0 };
        let cell_w_r = cap_w.min(vh * min_ar).max(0.0);
        for (k, &(w, h)) in pages[s..e].iter().enumerate() {
            cell_h[s + k] = if w > 0.0 {
                cell_w_r * (h / w)
            } else {
                cell_w_r
            };
        }
        let row_h = cell_h[s..e].iter().copied().fold(0.0f32, f32::max);
        let spread_r = cols as f32 * cell_w_r + (cols - 1) as f32 * h_gap;
        spread_max = spread_max.max(spread_r);
        row_top.push(y);
        cell_w_per_row.push(cell_w_r);
        left_pad_per_row.push(((content_w - spread_r) / 2.0).max(0.0));
        y += row_h + v_gap;
    }
    // total = padding + Σ行高 + (rows−1)·v_gap + padding（去循环尾部多计的 v_gap）。
    let total_height = y - v_gap + padding;

    PagedLayout {
        cols,
        rows,
        h_gap,
        v_gap,
        padding,
        row_top,
        row_page_start,
        cell_w_per_row,
        cell_h,
        left_pad_per_row,
        spread_max,
        total_height,
    }
}

/// 分页**可见行页区间**（未扩）：由 [`PagedLayout::first_visible_row`] 的首行 r0 得
/// `[row_page_start[r0], row_page_start[r0+1]−1]`——分页模式每行 ≈ 一屏，只物化首行即可
/// 覆盖当前屏（render_pdf_pages_paged 与 A 线调度共用同一口径）。`scroll_y` 为负偏移
/// （视口顶内容 y = −scroll_y，见 [`PagedLayout`]）；仍由首行推出闭区间页下标。
/// 无可见行 / 空布局 → `None`。
pub fn paged_visible_pages(
    layout: &PagedLayout,
    scroll_y: f32,
    viewport_h: f32,
    n: usize,
) -> Option<(usize, usize)> {
    let (r0, _) = layout.first_visible_row(scroll_y, viewport_h)?;
    let start = layout.row_page_start[r0];
    let end = layout.row_page_start[r0 + 1]
        .saturating_sub(1)
        .min(n.saturating_sub(1));
    if start > end || n == 0 {
        return None;
    }
    Some((start, end))
}

/// 分页**调度窗**（闭区间页下标）：可见行页区间再向两侧扩 `k_rows` 行（× `cols` 页）。
/// 语义对齐连续 [`keep_window`] 的 ±[`MARGIN`] 页缓冲：分页每行 ≈ 一屏，±`k_rows` 行 =
/// 保住「显示页必有底图 + 下一行提前暖」；再大由封面驻留窗/走廊预算独立限流。无可见行 /
/// 空布局 → `None`。
pub fn paged_schedule_window(
    layout: &PagedLayout,
    scroll_y: f32,
    viewport_h: f32,
    n: usize,
    k_rows: usize,
) -> Option<(usize, usize)> {
    let (vis_start, vis_end) = paged_visible_pages(layout, scroll_y, viewport_h, n)?;
    let expand = k_rows.saturating_mul(layout.cols.max(1));
    let start = vis_start.saturating_sub(expand);
    let end = (vis_end + expand).min(n.saturating_sub(1));
    Some((start, end))
}

/// 分页**渲染窗**（闭区间页下标）：视口相交的**全可见行** `[r0, r1]`（可含行中位自由滚动时
/// 视口底部压到的下一行 r1）再向两侧各扩 `buf_rows` 行，`clamp [0, rows−1]`，映射回行页区间
/// `[row_page_start[r_lo], row_page_start[r_hi+1]−1]` clamp `[0, n−1]`。语义对齐连续
/// `render_pdf_pages` 的 ±[`MARGIN`] 页缓冲：Facing=连续自由滚动，行中停位要同时看到上下
/// 两行——`paged_visible_pages` 只按首行 r0 会漏掉视口底部的下一行（只物化 r0 = 露底）。
/// 画面机械化缓冲由 A 线调度窗（±[`PAGED_WINDOW_ROWS`] 行）兜底，本窗只保「本帧显示行
/// 都有卡」。`scroll_y` 为负偏移（视口顶内容 y = −scroll_y，见 [`PagedLayout`]）；无可见行 /
/// 空布局 → `None`。
pub fn paged_render_window(
    layout: &PagedLayout,
    scroll_y: f32,
    viewport_h: f32,
    n: usize,
    buf_rows: usize,
) -> Option<(usize, usize)> {
    let (r0, r1) = layout.first_visible_row(scroll_y, viewport_h)?;
    let r_lo = r0.saturating_sub(buf_rows);
    let r_hi = (r1 + buf_rows).min(layout.rows.saturating_sub(1));
    let start = layout.row_page_start[r_lo];
    let end = layout.row_page_start[r_hi + 1]
        .saturating_sub(1)
        .min(n.saturating_sub(1));
    if start > end || n == 0 {
        return None;
    }
    Some((start, end))
}

/// 内容列坐标 (cx, cy) → (页号, 页内局部 x, 页内局部 y)。
///
/// - `cx/cy` 为**页列空间**坐标（wrap 内减去 [`column_left`] 后的列偏移；无需关心滚动
///   偏移——那是 view 层把窗口坐标换算到 wrap 空间时消掉的）。
/// - 列宽 = `page_width`；点到列的空白（左/右页边、页间纵向间隙）→ `None`（清空选区）。
pub fn content_to_local(
    dims: &[(f32, f32)],
    tops: &[f32],
    page_width: f32,
    cx: f32,
    cy: f32,
) -> Option<(usize, f32, f32)> {
    if dims.is_empty() || cx < 0.0 || cx > page_width {
        return None;
    }
    // 二分定位「页带（`页顶-1 ..= 页底+1`）含 `cy`」的页：页带互不交叠（PAGE_GAP > 2）
    // 且随 `tops` 严格递增 → 谓词「页带起点 ≤ cy」单调，可二分（对齐 `first_visible_page`
    // 既有姿势，千页命中测试 O(页数)→O(log 页数)），与旧线性扫逐例等价：同套浮点运算，
    // 页间沟槽 / 首页之上仍按「任何页带都不含 `cy`」返回 `None`（清空选区）。
    let n = dims.len();
    let mut lo = 0usize;
    let mut hi = n;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if tops[mid] - 1.0 <= cy {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    // `lo-1` = 最后一个「页带起点 ≤ cy」的页；其页带底（页底 + 1）未包住 `cy`（页间沟槽）
    // 或 `lo == 0`（首页带起点之上）→ `None`，与旧线性扫的沟槽判据一致。
    if lo == 0 {
        return None;
    }
    let i = lo - 1;
    let h = display_height(dims[i], page_width);
    if cy <= tops[i] + h + 1.0 {
        Some((i, cx, cy - tops[i]))
    } else {
        None
    }
}

/// 命中测试（纯函数）：页内 PDF 点 → 最近字符索引。
///
/// 优先“包含该点的字符”（若多个取中心最近）；否则取加权最近中心（纵向权 3，
/// 落在行间空隙时贴向最近的一行）。synthetic（零宽盒）不参与。
pub fn hit_char(text: &PageText, px: f32, py: f32) -> Option<usize> {
    let mut contain: Option<(usize, f32)> = None;
    let mut near: Option<(usize, f32)> = None;
    for (i, c) in text.chars.iter().enumerate() {
        if c.synthetic {
            continue;
        }
        let (cxm, cym) = ((c.left + c.right) / 2.0, (c.bottom + c.top) / 2.0);
        let inside = px >= c.left && px <= c.right && py >= c.bottom && py <= c.top;
        let d2 = (cxm - px).powi(2) + (cym - py).powi(2);
        if inside {
            match contain {
                Some((_, bd)) if bd <= d2 => {}
                _ => contain = Some((i, d2)),
            }
        }
        let wd2 = (cxm - px).powi(2) + 9.0 * (cym - py).powi(2);
        match near {
            Some((_, bd)) if bd <= wd2 => {}
            _ => near = Some((i, wd2)),
        }
    }
    contain.or(near).map(|(i, _)| i)
}

/// 单个字符选中区间的高亮矩形（PDF 点），按“行”合并成最小条数。
///
/// 将 `chars[lo..=hi]` 中非 synthetic 字符按基线（`origin_y`，容差 2pt）分组成行，
/// 每行一个矩形，覆盖该行选中字符的 min-left..max-right、min-bottom..max-top——
/// 行为与 Edge/pdf.js 的“整行高亮”一致；synthetic 的零宽盒不产生子矩形。
pub fn selection_spans_pt(text: &PageText, lo: usize, hi: usize) -> Vec<(f32, f32, f32, f32)> {
    const ROW_BUCKET_PT: f32 = 2.0;
    // P1-15：空字符集下无选中区间——提前返回空（`chars.len()-1` 饱和为 0 会把 hi 钳成 0，
    // `lo=hi=0` 仍会进 `chars[0]` 越界 panic）。全越界区间本就走空循环，语义不变。
    if text.chars.is_empty() {
        return Vec::new();
    }
    let hi = hi.min(text.chars.len().saturating_sub(1));
    let mut rows: Vec<(f32, (f32, f32, f32, f32))> = Vec::new();
    for i in lo..=hi {
        let c = &text.chars[i];
        // 零宽/退化盒（synthetic 或引擎漏标）不参与几何，仅正文
        if c.synthetic || c.right <= c.left || c.top <= c.bottom {
            continue;
        }
        let bucket = (c.origin_y / ROW_BUCKET_PT).round() * ROW_BUCKET_PT;
        match rows.iter_mut().find(|(b, _)| (*b - bucket).abs() < 1e-3) {
            Some((_, r)) => {
                r.0 = r.0.min(c.left);
                r.1 = r.1.min(c.bottom);
                r.2 = r.2.max(c.right);
                r.3 = r.3.max(c.top);
            }
            None => rows.push((bucket, (c.left, c.bottom, c.right, c.top))),
        }
    }
    rows.into_iter().map(|(_, r)| r).collect()
}

/// 单页选中文本（纯函数启发式，PDF 无“逻辑换行”信息）：
///
/// - 换行：下一字符基线比当前低超过 [`LINE_EPS_PT`] → 插入 `\n`；
/// - 空格：同行且字符间隙 > 平均字宽 × [`SPACE_GAP_RATIO`] → 插入 `' '`；
/// - 跳过 `\0`（pdfium 生成的填充）。
pub fn page_segment_text(text: &PageText, lo: usize, hi: usize) -> String {
    const LINE_EPS_PT: f32 = 3.0;
    const SPACE_GAP_RATIO: f32 = 1.5;
    // P1-15：空字符集下无可提取文本——提前返回空（防 `chars[0]` 越界 panic）。
    if text.chars.is_empty() {
        return String::new();
    }
    let hi = hi.min(text.chars.len().saturating_sub(1));
    if lo > hi {
        return String::new();
    }
    let count = (hi - lo + 1) as f32;
    let avg_w = (lo..=hi)
        .filter_map(|i| text.chars.get(i))
        .map(|c| (c.right - c.left).max(0.0))
        .sum::<f32>()
        / count.max(1.0);
    let mut out = String::new();
    let mut prev: Option<(f32, f32)> = None; // (右缘, origin_y)
    for i in lo..=hi {
        let c = &text.chars[i];
        if c.ch == '\0' {
            continue;
        }
        if let Some((pr, py)) = prev {
            if c.origin_y < py - LINE_EPS_PT {
                out.push('\n');
            } else if avg_w > 0.0
                && avg_w.is_finite()
                && c.origin_y >= py - LINE_EPS_PT - 0.5
                && c.left - pr > SPACE_GAP_RATIO * avg_w
            {
                out.push(' ');
            }
        }
        out.push(c.ch);
        prev = Some((c.right.max(c.left), c.origin_y));
    }
    out
}

#[allow(dead_code)] // 测试与后续命中预览使用（生产 bin 尚未物化命中文本）
/// 原始字符下标区间 → 拼接字符串（跳过 NUL；与 [`search_page_text`] 的"可见序列"一致）。
/// P3 用于校验检索命中文本 == 关键字，及展示命中预览。
pub fn page_char_substring(text: &PageText, lo: usize, hi: usize) -> String {
    // P1-15：空字符集下无可拼接原文——提前返回空（防 `chars[0]` 越界 panic）。
    if text.chars.is_empty() {
        return String::new();
    }
    let hi = hi.min(text.chars.len().saturating_sub(1));
    let mut s = String::new();
    for i in lo..=hi {
        if text.chars[i].ch != '\0' {
            s.push(text.chars[i].ch);
        }
    }
    s
}

/// 检索命中上下文片段：`前 ctx 字 + 命中目标 + 后 ctx 字`（基于“可见序列”，跳过 NUL；
/// 接近页首/页尾时该侧自然取不足 ctx 字，不再补省略符）。P3 侧边栏命中行展示用。
pub fn search_snippet(text: &PageText, start: usize, end: usize, ctx: usize) -> String {
    let visible: Vec<(usize, char)> = text
        .chars
        .iter()
        .enumerate()
        .filter(|(_, c)| c.ch != '\0' && c.ch != '\r' && c.ch != '\n')
        .map(|(orig, c)| (orig, c.ch))
        .collect();
    if visible.is_empty() {
        return String::new();
    }
    // 命中在可见序列里的半开区间 [m0, m1)
    let m0 = visible
        .iter()
        .position(|(orig, _)| *orig >= start)
        .unwrap_or(visible.len());
    let m1 = visible
        .iter()
        .position(|(orig, _)| *orig >= end)
        .unwrap_or(visible.len());
    if m0 >= m1 {
        return String::new();
    }
    let a = m0.saturating_sub(ctx);
    let b = (m1 + ctx).min(visible.len());
    let mut s = String::new();
    s.extend(visible[a..m0].iter().map(|(_, ch)| *ch));
    s.extend(visible[m0..m1].iter().map(|(_, ch)| *ch));
    s.extend(visible[m1..b].iter().map(|(_, ch)| *ch));
    s
}

/// 检索激活下标前进/后退（循环）；空命中 → None。
pub fn advance_match_index(count: usize, active: usize, delta: i64) -> Option<usize> {
    if count == 0 {
        return None;
    }
    let next = (active as i64 + delta).rem_euclid(count as i64) as usize;
    Some(next)
}

/// 每帧的页工作规划（纯函数，便于测试）：通过投影闭包按页号读取「是否已渲染 /
/// 是否正在加载 / 渲染代次」，决定 (需要渲染的页, 需要淘汰位图的页)。
///
/// ⚠️ 整页位图渲染已由瓦片渲染取代（参见 `view.rs`），本函数不再被生产代码调用；
/// 保留仅作为「整页蓝图调度」的回归测试锚点（`scrolling_down_and_back_*` 等测试），
/// 并可供未来按页聚合瓦片的调度复用。
#[allow(dead_code)] // 仅回归测试使用（生产走瓦片调度）
pub fn plan_page_work(
    page_count: usize,
    start: usize,
    end: usize,
    generation: u64,
    is_rendered: impl Fn(usize) -> bool,
    is_loading: impl Fn(usize) -> bool,
    rendered_generation: impl Fn(usize) -> u64,
) -> (Vec<usize>, Vec<usize>) {
    let last = page_count.saturating_sub(1);
    let mut to_render: Vec<usize> = Vec::new();
    for i in start..=end.min(last) {
        let needs_render = !is_rendered(i) || rendered_generation(i) != generation;
        if needs_render && !is_loading(i) {
            to_render.push(i);
        }
    }

    let mut retained = 0usize;
    for i in 0..page_count {
        if is_rendered(i) {
            retained += 1;
        }
    }
    if retained <= CACHE_MAX_PAGES {
        return (to_render, Vec::new());
    }
    // 预算已满：离中心最远优先淘汰（等距取索引小者，保证确定性）
    let center = (start + end) / 2;
    let mut candidates: Vec<usize> = (0..page_count).filter(|&i| is_rendered(i)).collect();
    candidates.sort_by_key(|&i| (std::cmp::Reverse((i as i64 - center as i64).abs()), i));
    let mut to_evict: Vec<usize> = Vec::new();
    for i in candidates {
        if to_evict.len() >= retained - CACHE_MAX_PAGES {
            break;
        }
        to_evict.push(i);
    }
    to_evict.sort_unstable();
    (to_render, to_evict)
}

#[cfg(test)]
mod tests;
