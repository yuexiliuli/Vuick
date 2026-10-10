//! PDF 页面的**纯布局/调度数学**：页列尺寸、可见页区间、渲染倍率、瓦片网格、每帧工作规划。
//!
//! 全部为无副作用纯函数（便于单测），与 `view.rs` 的页列 DOM 绝对定位**使用同一套数学**，
//! 保证滚动位置推算与 DOM 位置严格一致（否则可见页会被误判）。瓦片几何（[`tile_rect`]）
//! 由 **worker 线程**（`parser.rs`）与 **view 线程**（`view.rs`）共用——两侧各自独立算出
//! 的瓦片矩形必须逐像素一致，拼片才不会错位。

use std::collections::HashMap;

use crate::config::pdf::{CACHE_MAX_PAGES, MARGIN, MAX_BITMAP_SCALE, MAX_RENDER_SCALE, PAGE_GAP, PAGE_PAD_Y, PAGE_WIDTH};
use crate::pdf::PageText;

/// 页槽：某页的渲染状态（内存态）。
#[derive(Default, Clone)]
pub(crate) struct Slot {
    /// 已栅格化的**瓦片**缓存（键 = 页位图内的 (row, col) 网格格号）。瓦片按显示物理
    /// 像素密度渲染（见 [`render_scale`]），**内存 = 视口可见瓦片数 × TILE_SIZE² × 4**，
    /// 不随整页位图面积缩放爆炸（旧整页方案 4× 缩放一页 A4 = 64MB 的根因）。
    pub(crate) tiles: HashMap<(u32, u32), TileSlot>,
    /// 现有瓦片对应的渲染倍率（px/pt）。缩放/换 DPI 显示器后与新一轮 [`render_scale`]
    /// 不一致 → 该页瓦片被惰性作废，按新密度重栅格化（旧瓦片 `drop_image` 下线）。
    pub(crate) tile_scale: f32,
    /// **首屏整页锐化封面**（SumatraPDF 首遍同款）：可见页在「廉价密度」下先整页一渲、
    /// 立刻铺满首屏（瓦片几十片串行渲染前的瞬时清晰），瓦片补齐后丢弃回收内存。
    /// 与瓦片同密度，故与瓦片拼图完全对齐，只是作底层、先行到达。
    pub(crate) cover: Option<std::sync::Arc<gpui::RenderImage>>,
    /// 封面对应的渲染倍率（px/pt）；缩放变化后视为过期并丢弃。
    pub(crate) cover_scale: f32,
    /// L0 缩略图（低清层）：全尺寸瓦片缺席时先展示，拖动期间"永远有内容"；
    /// 快速拖动时全页只有缩略图占位，停止后按需补齐瓦片。
    pub(crate) thumb: Option<std::sync::Arc<gpui::RenderImage>>,
    /// 缩略图是否正在后台渲染（避免重复派发）。
    pub(crate) thumb_loading: bool,
    /// L0.5 中清页（~520px）：快速拖动时优先展示（比 260px 缩略图清晰、可辨识标题/
    /// 段落结构），作瓦片下方的底图占位。
    pub(crate) mid: Option<std::sync::Arc<gpui::RenderImage>>,
    /// 中清页是否正在后台渲染（避免重复派发）。
    pub(crate) mid_loading: bool,
}

/// 一块已完成栅格化的瓦片：图像本体 + 它在页位图中的几何。
#[derive(Clone)]
pub(crate) struct TileSlot {
    pub(crate) image: std::sync::Arc<gpui::RenderImage>,
    /// 瓦片在页位图中的左上角 x（含出血裁剪，已夹到页内）。
    pub(crate) x0: u32,
    /// 瓦片在页位图中的左上角 y。
    pub(crate) y0: u32,
    /// 瓦片位图实际宽（含出血，已夹到页内）。
    pub(crate) w: u32,
    /// 瓦片位图实际高。
    pub(crate) h: u32,
}

impl TileSlot {
    /// 瓦片占用的近似内存字节（RGBA）。
    pub(crate) fn bytes(&self) -> u64 {
        (self.w as u64) * (self.h as u64) * 4
    }
}

/// 某页在页列里的显示高度（保持宽高比）。
pub(crate) fn display_height((w, h): (f32, f32), page_width: f32) -> f32 {
    if w > 0.0 {
        page_width * (h / w)
    } else {
        page_width
    }
}

/// 预先计算整列布局：每页的 top 偏移（[`PAGE_PAD_Y`] + 前缀和）与整列总高。
///
/// 与 [`keep_window`] / 页列 DOM 绝对定位使用同一套数学。
pub(crate) fn compute_layout(dims: &[(f32, f32)], page_width: f32) -> (Vec<f32>, f32) {
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

/// 当前第一张**可见**页（0 基）：「页面底 > 视口顶」的首张页。
/// 与 [`keep_window`] 的窗口起点不同——**不含 MARGIN 缓冲**，用于状态栏页码等
/// 「如实反映视口」的读数；`scroll_y` 为内容已向下滚动的像素数（≥0）。
pub(crate) fn first_visible_page(dims: &[(f32, f32)], page_width: f32, scroll_y: f32) -> usize {
    if dims.is_empty() {
        return 0;
    }
    let top = scroll_y.max(0.0);
    let (tops, _) = compute_layout(dims, page_width);

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
pub(crate) fn keep_window(
    dims: &[(f32, f32)],
    page_width: f32,
    scroll_y: f32,
    viewport_h: f32,
) -> (usize, usize) {
    if dims.is_empty() {
        return (0, 0);
    }
    let vh = if viewport_h > 1.0 { viewport_h } else { 720.0 };
    let top = scroll_y.max(0.0);
    let bottom = top + vh;
    let (tops, _) = compute_layout(dims, page_width);

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
pub(crate) fn render_scale(page_width: f32, scale_factor: f32, page_pt_width: f32) -> f32 {
    if page_pt_width > 0.0 {
        let zoom = (page_width / PAGE_WIDTH).max(1.0);
        let cap = (MAX_RENDER_SCALE * zoom * scale_factor.max(1.0)).min(MAX_BITMAP_SCALE);
        (page_width * scale_factor / page_pt_width).clamp(0.5, cap)
    } else {
        MAX_RENDER_SCALE
    }
}

// ---- 瓦片（Tile）几何：worker 与 view 共用同一套数学，保证拼片逐像素对齐 ----

/// 页（PDF 点）→ 页位图（物理像素）尺寸。与 pdfium 栅格化的取整一致：`ceil` 且最小 1。
pub(crate) fn page_bitmap_size((pw, ph): (f32, f32), scale: f32) -> (u32, u32) {
    (
        (pw * scale).ceil().max(1.0) as u32,
        (ph * scale).ceil().max(1.0) as u32,
    )
}

/// 页位图 → 卡片显示像素的分轴缩放（宽/高各自独立，吸收两轴 ceil 取整差异）。
/// 与 [`pt_to_local`] 的映射同源（local = 位图像素 × 此缩放），瓦片与文本高亮严格对齐。
pub(crate) fn bitmap_display_scales(
    bmp: (u32, u32),
    display_w: f32,
    disp_h: f32,
) -> (f32, f32) {
    (
        display_w / bmp.0.max(1) as f32,
        disp_h / bmp.1.max(1) as f32,
    )
}

/// 瓦片网格维度：(行数, 列数)。每格名义 [TILE_SIZE × TILE_SIZE]（页尾/侧不足则为残片）。
pub(crate) fn tile_grid(bmp: (u32, u32), tile_size: u32) -> (u32, u32) {
    if tile_size == 0 {
        return (1, 1);
    }
    (bmp.1.div_ceil(tile_size).max(1), bmp.0.div_ceil(tile_size).max(1))
}

/// 某格在页位图中的实际绘制矩形 `(x0, y0, w, h)`（含出血、夹到页内）。
/// 整格在页外（越界的排/列守卫）→ `None`；否则 worker 按此矩形渲染、view 按此定位。
/// ⚠️ 这是两侧共用的**唯一权威实现**：改动必须同步 worker 输出与 view 放置语义。
pub(crate) fn tile_rect(
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

/// 给定卡片局部可见矩形（CSS px，已夹在页内），推算需要覆盖的瓦片网格区间，
/// 返回 `(row0, col0, row1, col1)`（含端点，已夹进网格）。`rect = (x0, y0, x1, y1)`。
/// 视口外缘再外扩 `margin` 格以缓冲滚动。无交点 → `None`。
pub(crate) fn visible_tile_range(
    bmp: (u32, u32),
    display_w: f32,
    disp_h: f32,
    rect: (f32, f32, f32, f32),
    tile_size: u32,
    margin: u32,
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
    let c0 = ((rx0 / sx) / cell as f32).floor() as i64 - (margin as i64);
    let c1 = ((rx1 / sx - 1.0).max(0.0) / cell as f32).floor() as i64 + (margin as i64);
    let r0 = ((ry0 / sy) / cell as f32).floor() as i64 - (margin as i64);
    let r1 = ((ry1 / sy - 1.0).max(0.0) / cell as f32).floor() as i64 + (margin as i64);
    let c0 = clamp(c0, cols as i64 - 1);
    let c1 = clamp(c1, cols as i64 - 1);
    let r0 = clamp(r0, rows as i64 - 1);
    let r1 = clamp(r1, rows as i64 - 1);
    if r0 > r1 || c0 > c1 {
        return None;
    }
    Some((r0, c0, r1, c1))
}

/// 「整页锐化封面」是否廉价可整渲：该密度下整页位图字节 ≤ [`COVER_SIZE_BUDGET`] 时，
/// 一个 `render_page` 即铺满首屏（再把瓦片叠上来精修）；超预算（高倍缩放/超大页）→
/// 纯瓦片路径。
pub(crate) fn cover_affordable(page_pt: (f32, f32), scale: f32) -> bool {
    if scale < 0.2 || page_pt.0 <= 0.0 || page_pt.1 <= 0.0 {
        return false;
    }
    let bytes = (page_pt.0 as f64) * (page_pt.1 as f64) * (scale as f64) * (scale as f64) * 4.0;
    bytes <= crate::config::pdf::COVER_SIZE_BUDGET as f64
}

/// 封面渲染密度：**满显示密度**（`render_scale`，物理像素 1:1）——实测整页渲染
/// ~10-50ms 且 Open 结构解析已近免费，首帧封面一渲即锐利、文字无需等瓦片精修；
/// 仅对病理性超高倍率用 [COVER_SCALE_CAP]（4.0）护栏（更高密度由预算/瓦片兜底）。
/// 1x / 2x-DPI 屏均得到与其屏幕密度完全一致的封面，缩放变化自会惰性重渲。
pub(crate) fn cover_scale(page_pt: (f32, f32), page_width: f32, scale_factor: f32) -> f32 {
    render_scale(page_width, scale_factor, page_pt.0).min(crate::config::pdf::COVER_SCALE_CAP)
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
pub(crate) fn pt_to_local(
    px: f32,
    py: f32,
    page_pt_w: f32,
    page_pt_h: f32,
    display_w: f32,
    scale: f32,
) -> (f32, f32) {
    let scale = scale.max(0.01);
    let bmp_w = (page_pt_w * scale).ceil().max(1.0);
    let bmp_h = (page_pt_h * scale).ceil().max(1.0);
    let disp_h = display_height((page_pt_w, page_pt_h), display_w);
    let bx = px * scale;
    let by = (page_pt_h - py) * scale;
    (
        bx * (display_w / bmp_w),
        by * (disp_h / bmp_h),
    )
}

/// 页卡局部 px → 页内 PDF 点坐标（[`pt_to_local`] 的逆；命中测试：鼠标 → 字符索引）。
pub(crate) fn local_to_pt(
    lx: f32,
    ly: f32,
    page_pt_w: f32,
    page_pt_h: f32,
    display_w: f32,
    scale: f32,
) -> (f32, f32) {
    let scale = scale.max(0.01);
    let bmp_w = (page_pt_w * scale).ceil().max(1.0);
    let bmp_h = (page_pt_h * scale).ceil().max(1.0);
    let disp_h = display_height((page_pt_w, page_pt_h), display_w);
    let bx = lx * (bmp_w / display_w);
    let by = ly * (bmp_h / disp_h);
    (bx / scale, page_pt_h - by / scale)
}

/// 页列左偏移：wrap 容器内居中对齐（列宽 = 页宽）。P2 命中测试与渲染同式。
pub(crate) fn column_left(wrap_w: f32, page_width: f32) -> f32 {
    ((wrap_w - page_width) / 2.0).max(0.0)
}

/// 内容列坐标 (cx, cy) → (页号, 页内局部 x, 页内局部 y)。
///
/// - `cx/cy` 为**页列空间**坐标（wrap 内减去 [`column_left`] 后的列偏移；无需关心滚动
///   偏移——那是 view 层把窗口坐标换算到 wrap 空间时消掉的）。
/// - 列宽 = `page_width`；点到列的空白（左/右页边、页间纵向间隙）→ `None`（清空选区）。
pub(crate) fn content_to_local(
    dims: &[(f32, f32)],
    tops: &[f32],
    page_width: f32,
    cx: f32,
    cy: f32,
) -> Option<(usize, f32, f32)> {
    if dims.is_empty() || cx < 0.0 || cx > page_width {
        return None;
    }
    for i in 0..dims.len() {
        let h = display_height(dims[i], page_width);
        let (top, bottom) = (tops[i], tops[i] + h);
        if cy >= top - 1.0 && cy <= bottom + 1.0 {
            return Some((i, cx, cy - tops[i]));
        }
    }
    None
}

/// 命中测试（纯函数）：页内 PDF 点 → 最近字符索引。
///
/// 优先“包含该点的字符”（若多个取中心最近）；否则取加权最近中心（纵向权 3，
/// 落在行间空隙时贴向最近的一行）。synthetic（零宽盒）不参与。
pub(crate) fn hit_char(text: &PageText, px: f32, py: f32) -> Option<usize> {
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
pub(crate) fn selection_spans_pt(text: &PageText, lo: usize, hi: usize) -> Vec<(f32, f32, f32, f32)> {
    const ROW_BUCKET_PT: f32 = 2.0;
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
pub(crate) fn page_segment_text(text: &PageText, lo: usize, hi: usize) -> String {
    const LINE_EPS_PT: f32 = 3.0;
    const SPACE_GAP_RATIO: f32 = 1.5;
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
            } else if avg_w > 0.0 && avg_w.is_finite() && c.origin_y >= py - LINE_EPS_PT - 0.5
                && c.left - pr > SPACE_GAP_RATIO * avg_w {
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
pub(crate) fn page_char_substring(text: &PageText, lo: usize, hi: usize) -> String {
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
pub(crate) fn search_snippet(text: &PageText, start: usize, end: usize, ctx: usize) -> String {
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
pub(crate) fn advance_match_index(count: usize, active: usize, delta: i64) -> Option<usize> {
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
pub(crate) fn plan_page_work(
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
mod tests {
    use super::*;
    use crate::pdf::{PageText, PdfTextChar};

    /// 测试用：构造一个（非合成）字符盒（PDF 点，左下原点，Y 向上）。
    fn tchar(ch: char, l: f32, b: f32, r: f32, t: f32, oy: f32) -> PdfTextChar {
        PdfTextChar {
            ch,
            left: l,
            bottom: b,
            right: r,
            top: t,
            synthetic: false,
            origin_x: l,
            origin_y: oy,
        }
    }

    /// 测试用：把字符列表包成 PageText（`synthetic` 为其中合成计数，仅诊断字段）。
    fn page_text_of(chars: Vec<PdfTextChar>, synthetic: usize) -> PageText {
        PageText {
            page_index: 0,
            width: 100.0,
            height: 200.0,
            chars,
            synthetic_count: synthetic,
        }
    }

    fn a4() -> (f32, f32) {
        (595.0, 842.0) // A4 竖版（pt）
    }

    fn page_step(page_width: f32) -> f32 {
        display_height(a4(), page_width) + PAGE_GAP
    }

    #[test]
    fn keep_window_covers_only_visible_plus_margin() {
        // 30 页 A4，页宽 840 → 每页高约 1189；视口 600px 高，滚到第 10 页附近
        let dims: Vec<(f32, f32)> = vec![a4(); 30];
        let scroll_y = 10.0 * page_step(840.0);
        let (s, e) = keep_window(&dims, 840.0, scroll_y, 600.0);
        // 第 10 页应可见，区间含 MARGIN 缓冲且不越界
        assert!(s <= 10 && 10 <= e, "第 10 页应在可见区间 [{s},{e}]");
        assert!(e - s + 1 > 2 * MARGIN, "区间宽度应至少为可见页 + 2*MARGIN");
        assert!(e <= 29, "末页不越界");
    }

    #[test]
    fn keep_window_empty_dims_returns_zero() {
        assert_eq!(keep_window(&[], 840.0, 0.0, 600.0), (0, 0));
    }

    #[test]
    fn first_visible_page_tracks_viewport_not_margin_buffer() {
        // 30 页 A4，页宽 840 → 每页高约 1189。滚到第 10 页（1 基）顶附近：首张**可见**
        // 页正是页底越进视口顶的 0 基 9；keep_window 的窗口起点会再前移 MARGIN 张——
        // 页码读数必须用前者（否则状态栏显示「第 1 / N 页」时实际已看到更后面的页）。
        let dims: Vec<(f32, f32)> = vec![a4(); 30];
        let step = page_step(840.0);
        let scroll_y = 10.0 * step;
        let visible = first_visible_page(&dims, 840.0, scroll_y);
        assert_eq!(visible, 9, "第二十滚位=10 步：页底越过视口顶的首张页应 0 基 9");
        let (s, _) = keep_window(&dims, 840.0, scroll_y, 600.0);
        assert_eq!(s, visible - MARGIN, "keep_window 起点含 MARGIN 缓冲（≠ visible）");

        // 顶部：第 0 页；空 dims / 负滚动安全。
        assert_eq!(first_visible_page(&dims, 840.0, 0.0), 0);
        assert_eq!(first_visible_page(&[], 840.0, 0.0), 0);
        assert_eq!(first_visible_page(&dims, 840.0, -5.0), 0);
    }

    /// 引用实现：旧线性扫 `first_visible_page`（用于与二分版逐例对拍）。
    fn ref_first_visible_page(dims: &[(f32, f32)], page_width: f32, scroll_y: f32) -> usize {
        if dims.is_empty() {
            return 0;
        }
        let top = scroll_y.max(0.0);
        let (tops, _) = compute_layout(dims, page_width);
        let mut first = 0usize;
        for (i, dim) in dims.iter().enumerate() {
            if tops[i] + display_height(*dim, page_width) > top {
                first = i;
                break;
            }
        }
        first
    }

    /// 引用实现：旧线性扫 `keep_window`（用于与二分版逐例对拍）。
    fn ref_keep_window(
        dims: &[(f32, f32)],
        page_width: f32,
        scroll_y: f32,
        viewport_h: f32,
    ) -> (usize, usize) {
        if dims.is_empty() {
            return (0, 0);
        }
        let vh = if viewport_h > 1.0 { viewport_h } else { 720.0 };
        let top = scroll_y.max(0.0);
        let bottom = top + vh;
        let (tops, _) = compute_layout(dims, page_width);
        let mut first = 0usize;
        for (i, dim) in dims.iter().enumerate() {
            if tops[i] + display_height(*dim, page_width) > top {
                first = i;
                break;
            }
        }
        let mut last = 0usize;
        for i in (0..dims.len()).rev() {
            if tops[i] < bottom {
                last = i;
                break;
            }
        }
        let start = first.saturating_sub(MARGIN);
        let end = (last + MARGIN).min(dims.len() - 1);
        (start, end)
    }

    #[test]
    fn binary_search_matches_linear_reference_across_scroll_span() {
        // 大页数二分正确性：构造混合尺寸页表（A4 + 信纸 + 宽 169 × 高 100 横页等，验证
        // 页底/页顶序列仍严格递增），在稠密滚动位（页顶/页底/页间空白/越界）上与引用线性
        // 实现逐例对拍——二分版与线性版用同一套浮点运算，边界语义（回退 0、MARGIN、夹取）必须分毫不差。
        let mut dims: Vec<(f32, f32)> = Vec::new();
        for i in 0..300 {
            dims.push(match i % 3 {
                0 => a4(),
                1 => (612.0, 792.0), // 信纸（letter）
                _ => (169.0, 100.0), // 横页小页（矮页：密集页顶/底）
            });
        }
        let pw = 840.0_f32;
        let (tops, total) = compute_layout(&dims, pw);

        // 覆盖全部边界：每页顶/底 ± 0.5、页间间隙中间、起点与越界（含滚过末页 → 回退 0）。
        let mut probes: Vec<f32> = vec![f32::MIN, -1000.0, -1.0, 0.0, 0.5, total, total + 100.0, 1e9, f32::MAX];
        for i in 0..dims.len() {
            let top = tops[i];
            let bottom = top + display_height(dims[i], pw);
            probes.push(top - 0.5);
            probes.push(top);
            probes.push(top + 0.5);
            probes.push(bottom - 0.5);
            probes.push(bottom);
            probes.push(bottom + 0.5);
            if i + 1 < dims.len() {
                probes.push((bottom + tops[i + 1]) / 2.0); // 页间 gap 中间
            }
        }

        for &scroll in &probes {
            let expect_first = ref_first_visible_page(&dims, pw, scroll);
            let got_first = first_visible_page(&dims, pw, scroll);
            assert_eq!(
                got_first, expect_first,
                "first_visible_page 在 scroll={scroll} 处偏离线性版"
            );
            for &vh in &[600.0_f32, 720.0, 1.0, 0.5, 0.0, 2000.0] {
                let expect_win = ref_keep_window(&dims, pw, scroll, vh);
                let got_win = keep_window(&dims, pw, scroll, vh);
                assert_eq!(
                    got_win, expect_win,
                    "keep_window 在 scroll={scroll}, viewport={vh} 处偏离线性版"
                );
            }
        }

        // 显式断言二分关键的「滚过末页回退 0」与「视口底压过末页」边界不退化。
        assert_eq!(first_visible_page(&dims, pw, total + 1.0), 0, "滚过末页应回退 0");
        assert_eq!(keep_window(&dims, pw, total + 1.0, 600.0).1, dims.len() - 1);
        assert_eq!(keep_window(&dims, pw, 0.0, 0.5).0, 0, "视口高 ≤1 回退默认 720 时窗口起点仍为首页");
    }

    #[test]
    fn compute_layout_is_monotonic_and_bounded() {
        let dims: Vec<(f32, f32)> = vec![a4(); 100];
        let (tops, total) = compute_layout(&dims, 840.0);
        assert_eq!(tops.len(), 100);
        assert_eq!(tops[0], PAGE_PAD_Y, "首页 top = 顶部内边距");
        for i in 1..100 {
            assert!(
                tops[i] > tops[i - 1],
                "top 应严格递增：第 {i} 页 top={} < 前一页 top={}",
                tops[i],
                tops[i - 1]
            );
        }
        let last = tops[99] + display_height(dims[99], 840.0);
        assert!(
            (total - (last + PAGE_PAD_Y)).abs() < 0.01,
            "总高 = 末页底 + 底部内边距：total={total}, last={last}"
        );
    }

    #[test]
    fn plan_renders_only_unrendered_visible_pages() {
        // 10 页，第 0 页已渲染且代次一致；视口 [0,2] → 只应渲染 1、2
        let rendered = [true, false, false, false, false, false, false, false, false, false];
        let loading = [false; 10];
        let gens = [0u64; 10];
        let n = rendered.len();
        let (render, evict) = plan_page_work(
            n,
            0,
            2,
            0,
            |i| rendered[i],
            |i| loading[i],
            |i| gens[i],
        );
        assert_eq!(render, vec![1, 2], "仅未渲染的可见页应列入渲染");
        assert!(evict.is_empty(), "无越界已渲染页不应淘汰");
    }

    #[test]
    fn plan_skips_pages_already_loading() {
        // 可见页 1 正在加载 → 不应重复派发渲染任务（CPU 占用暴增的元凶）
        let rendered = [true, false, false, false, false, false, false, false, false, false];
        let mut loading = [false; 10];
        loading[1] = true;
        let gens = [0u64; 10];
        let n = rendered.len();
        let (render, _) = plan_page_work(
            n,
            0,
            2,
            0,
            |i| rendered[i],
            |i| loading[i],
            |i| gens[i],
        );
        assert!(
            !render.contains(&1),
            "正在加载的页 1 不应重复派发（杜绝每帧重栅格化）"
        );
        assert!(render.contains(&2), "其余未渲染可见页仍应渲染");
    }

    #[test]
    fn plan_keeps_rendered_pages_within_budget() {
        // 200 页：仅第 10 页渲染，其余未渲染。预算 CACHE_MAX_PAGES，远未满 → 第 10 页必须保留。
        let n = 200usize;
        let rendered = vec![false; n];
        let gens = vec![0u64; n];
        let loading = vec![false; n];
        let (_r, evict) = plan_page_work(
            n,
            50,
            55,
            0,
            |i| i == 10 || rendered[i],
            |i| loading[i],
            |i| gens[i],
        );
        assert!(evict.is_empty(), "预算未满不应淘汰任何页（即使远在 40 页之外）");
    }

    #[test]
    fn plan_evicts_farthest_first_when_over_budget() {
        // 200 页都渲染（超预算 CACHE_MAX_PAGES），视口在 [100,105]。
        // 应淘汰离中心最远的页（最前/最后），而可见页绝不能被淘汰。
        let n = 200usize;
        let loading = vec![false; n];
        let gens = vec![0u64; n];
        let (r, evict) = plan_page_work(
            n,
            100,
            105,
            0,
            |_| true,
            |i| loading[i],
            |i| gens[i],
        );
        assert!(r.is_empty(), "全部已渲染，无需再渲染");
        // 回收到预算内
        assert_eq!(n - evict.len(), CACHE_MAX_PAGES, "保留数应等于预算");
        assert!(evict.contains(&0) && evict.contains(&199), "最远两端应先被淘汰");
        for i in 100..=105 {
            assert!(!evict.contains(&i), "可见页 {i} 绝不能被淘汰");
        }
    }

    #[test]
    fn scrolling_down_and_back_does_not_reload_rendered_pages() {
        // 模拟用户「往下翻很多页再倒回去」：全程用 plan_page_work 推进状态机，
        // 已渲染页只要未超预算、未被 LRU 淘汰，就绝不会进入 to_render（即不再“加载”）。
        let n = 400usize;
        let mut rendered = vec![false; n];
        let mut gens = vec![0u64; n];
        let loading = vec![false; n];

        // 逐窗口滚动：从第 0 页往下翻 100 次（每次窗口推进 2 页），再倒回翻 100 次
        for step in -100i64..=100 {
            let base = (50 + step * 2).clamp(0, (n as i64) - 6) as usize;
            let (start, end) = (base, (base + 5).min(n - 1));
            let (to_render, to_evict) = plan_page_work(
                n,
                start,
                end,
                0,
                |i| rendered[i],
                |i| loading[i],
                |i| gens[i],
            );
            // 记录本轮“需要重新加载”的页（应只含“从未渲染过”的页）
            for i in &to_render {
                assert!(
                    !rendered[*i],
                    "第 {i} 页已渲染却仍被要求渲染（往回翻又加载了！）step={step}"
                );
                rendered[*i] = true;
                gens[*i] = 0;
            }
            for i in &to_evict {
                rendered[*i] = false;
            }
        }
    }

    #[test]
    fn keep_window_before_first_paint_assumes_viewport() {
        // viewport_h ≤ 1 时回退默认高度，首帧也应覆盖从首页开始的若干页
        let dims: Vec<(f32, f32)> = vec![a4(); 5];
        let (s, e) = keep_window(&dims, 840.0, 0.0, 0.0);
        assert!(s == 0, "首帧应从首页开始，实际 s={s}");
        assert!(e <= 4, "不越界，实际 e={e}");
    }

    #[test]
    fn display_height_preserves_aspect() {
        let h = display_height(a4(), 840.0);
        assert!((h - 840.0 * 842.0 / 595.0).abs() < 0.01);
    }

    #[test]
    fn render_scale_capped_and_sane() {
        // 1x DPI：页宽 595pt、在屏 840px → ~1.41x，夹在 [0.5, MAX_RENDER_SCALE]，行为不变
        let s = render_scale(840.0, 1.0, 595.0);
        assert!(s > 0.5 && s <= MAX_RENDER_SCALE);
        // 高 DPI（150%）：渲染必须够到“显示物理像素”，不能被 MAX_RENDER_SCALE 压回 1.5
        // （否则 GPU 上采样拉伸 → 文字/图像发糊）。A4 595pt → 自然 840*1.5/595 ≈ 2.117
        let s_dpi = render_scale(840.0, 1.5, 595.0);
        assert!(
            s_dpi > MAX_RENDER_SCALE,
            "150% DPI 下缩放应超过 1.5（物理像素对齐），实际 {s_dpi}"
        );
        // 换算成位图宽应约等于显示物理宽度 840*1.5 = 1260px
        let px = (595.0 * s_dpi).round();
        assert!(
            (px - 840.0 * 1.5).abs() <= 1.0,
            "位图宽应≈显示物理宽 1260px，实际 {px}"
        );
        // 超高 DPI / 超大页仍受 MAX_BITMAP_SCALE 绝对硬顶
        let s2 = render_scale(840.0, 2.0, 100.0);
        assert!(s2 <= MAX_BITMAP_SCALE, "不得突破绝对硬顶: {s2}");
        let s3 = render_scale(840.0, 4.0, 100.0);
        assert!(s3 <= MAX_BITMAP_SCALE, "不得突破绝对硬顶: {s3}");
    }

    /// 页内 pt → 卡片局部 px：Y 翻转、边界与单调性（纯函数，不触 pdfium）。
    #[test]
    fn pt_to_local_flips_y_and_maps_corners() {
        let (pw, ph) = a4(); // 595 x 842
        let (dw, scale) = (840.0f32, 1.4f32);
        let disp_h = display_height(a4(), dw);
        // 页左上 (0, ph) → 卡片左上 (0, 0)
        let (x, y) = pt_to_local(0.0, ph, pw, ph, dw, scale);
        assert!(x.abs() < 1e-2 && y.abs() < 1e-2, "页左上应映射到卡片左上: ({x},{y})");
        // 页右下 (pw, 0) → 卡片右下 (dw, disp_h)；y 允许 ±ceil 亚像素
        let (x, y) = pt_to_local(pw, 0.0, pw, ph, dw, scale);
        assert!((x - dw).abs() < 1e-2, "x 应到卡片右缘: {x}");
        assert!(
            (y - disp_h).abs() < 0.25,
            "y 应到卡片底缘（±ceil 亚像素）: {y} vs {disp_h}"
        );
        // 单调：pt x 增大 → 局部 x 增大；pt y 减小（更靠页底）→ 局部 y 增大
        let (x1, y1) = pt_to_local(10.0, 100.0, pw, ph, dw, scale);
        let (x2, _) = pt_to_local(20.0, 100.0, pw, ph, dw, scale);
        let (x3, y3) = pt_to_local(10.0, 90.0, pw, ph, dw, scale);
        assert!(x2 > x1, "右移应使局部 x 增大: {x1} → {x2}");
        assert!((x3 - x1).abs() < 1e-3, "x 不变则局部 x 不变");
        assert!(y3 > y1, "更靠页底（pt y 更小）应使局部 y 增大: {y1} → {y3}");
    }

    /// pt → local → pt 往返闭合（P2 命中测试依赖逆向映射精度）。
    #[test]
    fn pt_to_local_roundtrips_via_local_to_pt() {
        let (pw, ph, dw, scale) = (595.0f32, 842.0f32, 900.0f32, 1.5f32);
        for (px, py) in [
            (0.0, 0.0),
            (pw, ph),
            (123.0, 456.0),
            (pw / 2.0, ph * 0.3),
            (1.0, 1.0),
            (pw - 1.0, ph - 1.0),
        ] {
            let (lx, ly) = pt_to_local(px, py, pw, ph, dw, scale);
            let (bx, by) = local_to_pt(lx, ly, pw, ph, dw, scale);
            assert!(
                (bx - px).abs() < 0.05 && (by - py).abs() < 0.05,
                "往返应闭合: ({px},{py}) → local({lx},{ly}) → ({bx},{by})"
            );
        }
    }

    /// 列坐标 → (页, 页内局部)；空白/页间沟槽/列外应返回 None（清空选区）。
    #[test]
    fn content_to_local_hits_pages_and_rejects_gutters() {
        let dims = vec![a4(), a4()];
        let page_width = 840.0f32;
        let (tops, _total) = compute_layout(&dims, page_width);
        let (p0, lx, ly) =
            content_to_local(&dims, &tops, page_width, 10.0, tops[0] + 100.0).expect("页内应命中");
        assert_eq!(p0, 0);
        assert!((lx - 10.0).abs() < 0.01 && (ly - 100.0).abs() < 0.01, "页内局部坐标应正确");
        let (p1, _, _) =
            content_to_local(&dims, &tops, page_width, 10.0, tops[1] + 50.0).expect("第 2 页应命中");
        assert_eq!(p1, 1);
        // 页间沟槽中点
        let gutter = tops[0] + display_height(a4(), page_width) + PAGE_GAP / 2.0;
        assert!(content_to_local(&dims, &tops, page_width, 10.0, gutter).is_none(), "沟槽应 None");
        // 列外 x
        assert!(content_to_local(&dims, &tops, page_width, -5.0, tops[0] + 100.0).is_none(), "左缘外应 None");
        assert!(content_to_local(&dims, &tops, page_width, page_width + 5.0, tops[0] + 100.0).is_none(), "右缘外应 None");
    }

    /// 命中测试：盒内含点优先（中心最近），空隙贴最近行。
    #[test]
    fn hit_char_prefers_contained_then_nearest() {
        let text = page_text_of(vec![
            tchar('A', 10.0, 20.0, 12.0, 22.0, 20.0),
            tchar('B', 14.0, 20.0, 16.0, 22.0, 20.0),
            tchar('S', 0.0, 0.0, 0.0, 0.0, 0.0), // synthetic：应被跳过
        ], 1);
        assert_eq!(hit_char(&text, 11.0, 21.0), Some(0), "点在 A 盒内");
        assert_eq!(hit_char(&text, 15.0, 21.0), Some(1), "点在 B 盒内");
        assert_eq!(hit_char(&text, 14.5, 21.0), Some(1), "格间空隙应贴最近");
        assert_eq!(hit_char(&text, 11.0, 25.0), Some(0), "行下空隙应贴最近行");
    }

    /// 选区合并成行矩形：同行顺延合并，synthetic 零宽盒不产生子矩形。
    #[test]
    fn selection_spans_merge_by_line_and_skip_synthetic() {
        let text = page_text_of(vec![
            tchar('a', 10.0, 100.0, 12.0, 112.0, 100.0),
            tchar('b', 13.0, 100.0, 15.0, 112.0, 100.0),
            tchar('c', 10.0, 80.0, 14.0, 92.0, 80.0),
            tchar('d', 16.0, 80.0, 20.0, 92.0, 80.0),
            PdfTextChar {
                synthetic: true,
                ..tchar('s', 0.0, 0.0, 0.0, 0.0, 0.0)
            },
        ], 1);
        let mut spans = selection_spans_pt(&text, 0, 4);
        spans.sort_by(|a, b| a.1.total_cmp(&b.1)); // 按 bottom 排序，去顺序依赖
        assert_eq!(
            spans,
            vec![(10.0, 80.0, 20.0, 92.0), (10.0, 100.0, 15.0, 112.0)],
            "每行应合并为一个矩形，synthetic 不参与"
        );
    }

    /// 文本拼接：行内宽间隙插空格、跨行插 \n、跳过 \0。
    #[test]
    fn page_segment_text_handles_space_newline_and_nul() {
        let text = page_text_of(vec![
            tchar('a', 10.0, 100.0, 12.0, 112.0, 100.0),
            tchar('b', 13.0, 100.0, 15.0, 112.0, 100.0),
            tchar('c', 30.0, 100.0, 32.0, 112.0, 100.0), // 与 b 间隙 15 > 1.5*avg(2)
            tchar('\0', 0.0, 0.0, 0.0, 0.0, 100.0),       // 填充 NUL
            tchar('d', 10.0, 80.0, 12.0, 92.0, 80.0),     // 基线降 20 → 换行
            tchar('e', 13.0, 80.0, 15.0, 92.0, 80.0),
        ], 1);
        assert_eq!(page_segment_text(&text, 0, 5), "ab c\nde");
    }

    /// 原文子串拼接：跳过 NUL、越界下标钳制。
    #[test]
    fn page_char_substring_skips_nul_and_clamps() {
        let text = page_text_of(vec![
            tchar('a', 0.0, 0.0, 1.0, 1.0, 0.0),
            tchar('\0', 0.0, 0.0, 0.0, 0.0, 0.0),
            tchar('c', 2.0, 0.0, 3.0, 1.0, 0.0),
        ], 0);
        assert_eq!(page_char_substring(&text, 0, 2), "ac", "NUL 应被跳过");
        assert_eq!(page_char_substring(&text, 0, 99), "ac", "越界 hi 应钳制");
        assert_eq!(page_char_substring(&text, 5, 9), "", "全越界应为空");
    }

    /// 检索命中上下文片段：目标前/后各含至多 ctx 字（页首/页尾一侧不足即取不足）。
    #[test]
    fn search_snippet_shows_context_around_hit() {
        let text = page_text_of(
            "abcdefghij"
                .chars()
                .enumerate()
                .map(|(i, ch)| tchar(ch, i as f32, 0.0, i as f32 + 1.0, 1.0, 0.0))
                .collect(),
            0,
        );
        // 命中 'def'(3..6)，ctx=2 → b,c + def + g,h
        assert_eq!(search_snippet(&text, 3, 6, 2), "bcdefgh");
        // 页首命中 'a'(0..1)，ctx=2 → 前不足则只取可用：a + b,c
        assert_eq!(search_snippet(&text, 0, 1, 2), "abc");
        // 页尾命中 'ij'(8..10)，ctx=2 → g,h + i,j
        assert_eq!(search_snippet(&text, 8, 10, 2), "ghij");
        // ctx=0 → 只目标本身
        assert_eq!(search_snippet(&text, 3, 6, 0), "def");
        // 跳过 NUL：可见序列里连续，命中片段不含 NUL
        let with_nul = page_text_of(
            ['a', 'b', '\0', 'c', 'd']
                .into_iter()
                .enumerate()
                .map(|(i, ch)| tchar(ch, i as f32, 0.0, i as f32 + 1.0, 1.0, 0.0))
                .collect(),
            1,
        );
        assert_eq!(search_snippet(&with_nul, 3, 5, 0), "cd", "NUL 不进入可见序列");
        // 换行控制符（pdfium 文本流里的 CR/LF）也不进入可见序列，保持单行干净
        let with_nl = page_text_of(
            ['a', 'b', '\r', '\n', 'c']
                .into_iter()
                .enumerate()
                .map(|(i, ch)| tchar(ch, i as f32, 0.0, i as f32 + 1.0, 1.0, 0.0))
                .collect(),
            0,
        );
        assert_eq!(search_snippet(&with_nl, 0, 5, 2), "abc", "CR/LF 被过滤");
    }

    /// 检索激活下标循环前进/后退。
    #[test]
    fn advance_match_wraps_cyclically() {
        assert_eq!(advance_match_index(4, 0, 1), Some(1));
        assert_eq!(advance_match_index(4, 3, 1), Some(0), "末尾 +1 应回绕到 0");
        assert_eq!(advance_match_index(4, 0, -1), Some(3), "0 -1 应到末位");
        assert_eq!(advance_match_index(0, 0, 1), None, "无命中返回 None");
    }

    /// 基准：`plan_page_work` 每帧「先按总页数物化 3 个 Vec 再规划」 vs
    ///「闭包投影直接规划」（[`Self`] 里被 `view.rs` 每帧调用）。
    /// 打印、不构成断言；`--ignored` 运行（匹配项目既有 bench 惯例）。
    #[test]
    #[ignore]
    fn bench_plan_page_work_vec_materialize_vs_closure_projection() {
        use std::time::Instant;
        let n = 2000usize;
        let mut rendered = vec![false; n];
        let loading = vec![false; n];
        let mut gens = vec![0u64; n];
        for (i, r) in rendered.iter_mut().enumerate() {
            if i % 3 == 0 {
                *r = true;
                gens[i] = 1;
            }
        }
        let (start, end) = (950usize, 1050usize);
        let generation = 1u64;
        let runs = 1000usize;

        // 旧路径：每帧先按总页数物化 3 个 Vec（要消除的成本）
        let t0 = Instant::now();
        let mut sink = 0usize;
        for _ in 0..runs {
            let r: Vec<bool> = rendered.to_vec();
            let l: Vec<bool> = loading.to_vec();
            let g: Vec<u64> = gens.to_vec();
            let (tr, _ev) = plan_page_work(
                n,
                start,
                end,
                generation,
                |i| r[i],
                |i| l[i],
                |i| g[i],
            );
            sink = sink.wrapping_add(tr.len());
        }
        let old = t0.elapsed();

        // 新路径：闭包直接投影 slots 状态，零按总页数分配
        let t1 = Instant::now();
        let mut sink2 = 0usize;
        for _ in 0..runs {
            let (tr, _ev) = plan_page_work(
                n,
                start,
                end,
                generation,
                |i| rendered[i],
                |i| loading[i],
                |i| gens[i],
            );
            sink2 = sink2.wrapping_add(tr.len());
        }
        let new_ = t1.elapsed();
        assert_eq!(sink, sink2, "两种路径规划结果必须一致");
        println!(
            "PDF plan_page_work（{n} 页 × {runs} 帧）：旧(每帧物化 3 Vec)={}ms, 新(闭包投影)={}ms（提升 {:.1}x）",
            old.as_millis(),
            new_.as_millis(),
            old.as_secs_f64() / new_.as_secs_f64().max(1e-9),
        );
    }

    /// 瓦片基础几何：位图尺寸 ceil 取整、网格行列计数、残片夹边。
    #[test]
    fn tile_geometry_grid_and_rect_with_bleed() {
        // A4 @1.0 (595pt→595px)：bmp=(595,842)；512 片 → 2×2 网格，第二行列是残片
        let bmp = page_bitmap_size(a4(), 1.0);
        assert_eq!(bmp, (595, 842), "位图尺寸应 ceil(页pt×scale)");
        let (rows, cols) = tile_grid(bmp, 512);
        assert_eq!((rows, cols), (2, 2), "595/842 应各切成 2 格");
        // 左上角瓦片：出血把 x0/y0 顶在页缘，右/下缘 512+2 出血（页面够大不夹边）
        let r = tile_rect(0, 0, 512, 2, bmp).expect("应有矩形");
        assert_eq!(r, (0, 0, 514, 514), "首片含出血：x0/y0 顶页缘，右/下扩 2px");
        // 第二行第二列（残片）：x0=512-2=510 起，end=512+2 夹到 595
        let r2 = tile_rect(1, 1, 512, 2, bmp).expect("残片应有矩形");
        assert_eq!(r2, (510, 510, 85, 332), "x0/y0 含出血，宽高夹到页内");
        // 越界格（整格在页外）→ None
        assert!(tile_rect(2, 0, 512, 2, bmp).is_none(), "第 3 列整格在页外应 None");
        assert!(
            tile_rect(0, 2, 512, 2, bmp).is_none(),
            "第 3 行整格在页外应 None"
        );
        // 空位图守卫：tile_size=0 不除零
        assert!(tile_rect(0, 0, 0, 2, (1, 1)).is_none());
    }

    /// 页位图像素尺寸与显示像素尺寸的换算（高 DPI 物理像素 1:1 的前提）。
    #[test]
    fn bitmap_display_scales_are_physical_pixel_faithful() {
        // 1x DPI：位图 840×1189 显示在 840×1189 CSS px → 缩放 1:1
        let bmp = (840u32, 1189u32);
        let (sx, sy) = bitmap_display_scales(bmp, 840.0, 1189.0);
        assert!((sx - 1.0).abs() < 1e-3 && (sy - 1.0).abs() < 1e-3, "1x 应 1:1");
        // 2x DPI：位图 1680×2377 显示在 840×1189 CSS px → 每个 CSS px = 2 位图像素
        let (sx2, sy2) = bitmap_display_scales((1680, 2377), 840.0, 1189.0);
        assert!((sx2 - 0.5).abs() < 1e-3 && (sy2 - 0.5).abs() < 1e-3, "2x 应半缩");
    }

    /// 可见矩形 → 瓦片区间：只圈中相交的格、外扩 margin、且夹进网格。
    #[test]
    fn visible_tile_range_covers_only_intersecting_tiles() {
        let bmp = page_bitmap_size(a4(), 1.0); // 595×842，512 片 → 2×2
        // 只看左上角 200×100 CSS px（@1x 即位图像素 200×100）→ 只应圈第 0 行 0 列
        let mut r = visible_tile_range(bmp, 595.0, 842.0, (0.0, 0.0, 200.0, 100.0), 512, 0).unwrap();
        assert_eq!(r, (0, 0, 0, 0), "仅可见左上格不含 margin");
        // margin=2 → 扩展到整页网格
        r = visible_tile_range(bmp, 595.0, 842.0, (0.0, 0.0, 200.0, 100.0), 512, 2).unwrap();
        assert_eq!(r, (0, 0, 1, 1), "margin 应外扩到 2×2 全覆盖");
        // 可见区恰跨两个格（x ∈ [200, 595]）→ 只覆盖 0、1 两列
        r = visible_tile_range(bmp, 595.0, 842.0, (200.0, 0.0, 595.0, 100.0), 512, 0).unwrap();
        assert_eq!(r, (0, 0, 0, 1), "横向跨格两列都应覆盖");
        // 空白/空矩形 → None
        assert!(
            visible_tile_range(bmp, 595.0, 842.0, (0.0, 0.0, 0.0, 100.0), 512, 0).is_none(),
            "零宽矩形应 None"
        );
    }

    /// 「整页锐化封面」廉价判定：普通 A4 @1.0 应可整渲（≈2MB），超高密度则应转纯瓦片。
    #[test]
    fn cover_affordable_gates_by_size_budget() {
        assert!(cover_affordable(a4(), 1.0), "A4 @1.0（~2MB）应可整渲封面");
        assert!(
            cover_affordable(a4(), 2.8),
            "A4 @2.8（~16MB）仍在 32MB 预算内"
        );
        assert!(!cover_affordable(a4(), 8.0), "A4 @8.0（~128MB）应转纯瓦片");
        assert!(
            !cover_affordable((2000.0, 1000.0), 4.0),
            "超大页 @4.0（~128MB）应转纯瓦片"
        );
        assert!(!cover_affordable((0.0, 100.0), 1.0), "零宽页不整渲");
        assert!(!cover_affordable(a4(), 0.0), "零密度不整渲");
    }

    /// 瓦片几何往返：visible_tile_range 覆盖的每格，tile_rect 都能给出正尺寸矩形，
    /// 且网格与矩形互洽（view 派发 view 收工，同一套数学）。
    #[test]
    fn tile_range_rects_are_well_formed_and_in_grid() {
        let bmp = page_bitmap_size(a4(), 1.0);
        let (rows, cols) = tile_grid(bmp, 512);
        let rng = visible_tile_range(bmp, 595.0, 842.0, (0.0, 0.0, 595.0, 842.0), 512, 0).unwrap();
        let mut count = 0usize;
        for row in rng.0..=rng.2 {
            for col in rng.1..=rng.3 {
                assert!(row < rows && col < cols, "格号应在网格内");
                let (x0, y0, w, h) = tile_rect(col, row, 512, 2, bmp).expect("格应有矩形");
                assert!(w >= 1 && h >= 1, "瓦片应正尺寸");
                assert!(x0 + w <= bmp.0 && y0 + h <= bmp.1, "瓦片不越页位图边界");
                count += 1;
            }
        }
        assert_eq!(count, (rows * cols) as usize, "整页应圈出全部 2×2 格");
    }
}
