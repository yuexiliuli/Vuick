pub(crate) use std::collections::HashMap;

pub(crate) use config::pdf::{
    COVER_AREA_BUDGET, MARGIN, MAX_BITMAP_SCALE, MAX_RENDER_SCALE, PAGE_GAP, PAGE_PAD_Y, PAGE_WIDTH,
};

use super::paged::PagedLayout;

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
