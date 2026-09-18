//! PDF 页面的**纯布局/调度数学**：页列尺寸、可见页区间、渲染倍率、每帧页工作规划。
//!
//! 全部为无副作用纯函数（便于单测），与 `view.rs` 的页列 DOM 绝对定位**使用同一套数学**，
//! 保证滚动位置推算与 DOM 位置严格一致（否则可见页会被误判）。

use crate::config::pdf::{
    CACHE_MAX_PAGES, MARGIN, MAX_BITMAP_SCALE, MAX_RENDER_SCALE, PAGE_GAP, PAGE_PAD_Y, PAGE_WIDTH,
};
use crate::pdf::PageText;

/// 页槽：某页的渲染状态（内存态）。
#[derive(Default, Clone)]
pub(crate) struct Slot {
    /// 已栅格化位图（`None` = 从未渲染或已回收）。
    pub(crate) image: Option<std::sync::Arc<gpui::RenderImage>>,
    /// 是否正在后台栅格化（避免重复派发）。
    pub(crate) loading: bool,
    /// 该位图对应的渲染代次（缩放/质量变化时使旧位图失效）。
    pub(crate) generation: u64,
    /// L0 缩略图（低清层）：全尺寸位图缺席时先展示，拖动期间"永远有内容"；
    /// 全尺寸位图回收后仍保留，回翻时即时占位再补全尺寸。
    pub(crate) thumb: Option<std::sync::Arc<gpui::RenderImage>>,
    /// 缩略图是否正在后台渲染（避免重复派发）。
    pub(crate) thumb_loading: bool,
    /// L0.5 中清页（~520px）：快速拖动时优先展示（比 260px 缩略图清晰、可辨识标题/
    /// 段落结构），停止后作为淡入底图过渡到全尺寸。
    pub(crate) mid: Option<std::sync::Arc<gpui::RenderImage>>,
    /// 中清页是否正在后台渲染（避免重复派发）。
    pub(crate) mid_loading: bool,
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

    // 第一页「页面底 > 视口顶」
    let mut first = 0usize;
    for (i, dim) in dims.iter().enumerate() {
        if tops[i] + display_height(*dim, page_width) > top {
            first = i;
            break;
        }
    }
    // 最后一页「页面顶 < 视口底」
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

/// 渲染倍率：以在屏宽度、显示 DPI 推导，封顶避免超大位图。
/// 高倍缩放（page_width > [`PAGE_WIDTH`]）时按缩放比抬高封顶，保证放大后仍清晰；
/// zoom=1 时封顶仍为 [`MAX_RENDER_SCALE`]，行为不变。
pub(crate) fn render_scale(page_width: f32, scale_factor: f32, page_pt_width: f32) -> f32 {
    if page_pt_width > 0.0 {
        let zoom = (page_width / PAGE_WIDTH).max(1.0);
        let cap = (MAX_RENDER_SCALE * zoom).min(MAX_BITMAP_SCALE);
        (page_width * scale_factor / page_pt_width).clamp(0.5, cap)
    } else {
        MAX_RENDER_SCALE
    }
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
            } else if avg_w > 0.0 && avg_w.is_finite() && c.origin_y >= py - LINE_EPS_PT - 0.5 {
                if c.left - pr > SPACE_GAP_RATIO * avg_w {
                    out.push(' ');
                }
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
    for k in a..m0 {
        s.push(visible[k].1);
    }
    for k in m0..m1 {
        s.push(visible[k].1);
    }
    for k in m1..b {
        s.push(visible[k].1);
    }
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

/// 快速拖动时的"瞬态写丢弃"决定（纯函数）：写队列超上限时返回应丢弃的**最旧**条目数。
/// `moving=false`（静止/慢速）时从不丢弃——保证停止后所有热页都会归档到磁盘。
pub(crate) fn trim_transient_writes(len: usize, queue_max: usize, moving: bool) -> usize {
    if moving && len > queue_max {
        len - queue_max
    } else {
        0
    }
}

/// 每帧的页工作规划（纯函数，便于测试）：通过投影闭包按页号读取「是否已渲染 /
/// 是否正在加载 / 渲染代次」，决定 (需要渲染的页, 需要淘汰位图的页)。
///
/// **不物化任何按总页数的临时数组**：旧版在 `view.rs` 每帧先按全文页数构造
/// `Vec<bool>`×2 + `Vec<u64>`（几百上千页时每帧三次堆分配），再传给本函数；
/// 这里改为闭包逐页投影，渲染帧零额外分配。
///
/// - **渲染**：可见区内既**未渲染**又**未在加载中**（或位图代次过期）的页。
///   正在加载的页绝不再派发任务，避免每个可见帧都重复栅格化同一页。
/// - **淘汰（延后回收 / 距离 LRU）**：仅当保留位图数超过 [`CACHE_MAX_PAGES`] 时，
///   才把**离当前视口中心最远**的页依次回收直到回到预算内。预算内任一页——无论多靠前
///   多靠后——都保留，因此「往下翻很多页再倒回去」命中缓存，不重新加载。
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
    fn trim_transient_writes_only_when_moving_over_cap() {
        // 拖动中超上限 → 丢弃最旧的（返回应丢弃条数）；未超上限/静止 → 全部保留
        assert_eq!(trim_transient_writes(20, 16, true), 4, "拖动超上限应丢弃 4 条最旧");
        assert_eq!(trim_transient_writes(16, 16, true), 0, "恰在上限不丢弃");
        assert_eq!(trim_transient_writes(10, 16, true), 0, "未超上限不丢弃");
        assert_eq!(
            trim_transient_writes(20, 16, false),
            0,
            "静止/慢速从不丢弃（保证热页归档）"
        );
        assert_eq!(
            trim_transient_writes(0, 16, true),
            0,
            "空队列丢弃 0 条"
        );
    }

    #[test]
    fn keep_window_covers_only_visible_plus_margin() {
        // 30 页 A4，页宽 840 → 每页高约 1189；视口 600px 高，滚到第 10 页附近
        let dims: Vec<(f32, f32)> = vec![a4(); 30];
        let scroll_y = 10.0 * page_step(840.0);
        let (s, e) = keep_window(&dims, 840.0, scroll_y, 600.0);
        // 第 10 页应可见，区间含 MARGIN 缓冲且不越界
        assert!(s <= 10 && 10 <= e, "第 10 页应在可见区间 [{s},{e}]");
        assert!(e - s + 1 >= 2 * MARGIN + 1, "区间宽度应至少为可见页 + 2*MARGIN");
        assert!(e <= 29, "末页不越界");
    }

    #[test]
    fn keep_window_empty_dims_returns_zero() {
        assert_eq!(keep_window(&[], 840.0, 0.0, 600.0), (0, 0));
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
        // 页宽 595pt，在屏 840px @ 1x DPI → ~1.41x，夹在 [0.5, MAX_RENDER_SCALE]
        let s = render_scale(840.0, 1.0, 595.0);
        assert!(s > 0.5 && s <= MAX_RENDER_SCALE);
        // 超大 DPI（Retina 2x）也要封顶
        let s2 = render_scale(840.0, 2.0, 100.0);
        assert!(s2 <= MAX_RENDER_SCALE);
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
            let r: Vec<bool> = rendered.iter().copied().collect();
            let l: Vec<bool> = loading.iter().copied().collect();
            let g: Vec<u64> = gens.iter().copied().collect();
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
}
