use crate::pdf::PageText;
pub(crate) use config::pdf::CACHE_MAX_PAGES;

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
