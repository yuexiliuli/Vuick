//! PDF 页面的**纯布局/调度数学**：页列尺寸、可见页区间、渲染倍率、每帧页工作规划。
//!
//! 全部为无副作用纯函数（便于单测），与 `view.rs` 的页列 DOM 绝对定位**使用同一套数学**，
//! 保证滚动位置推算与 DOM 位置严格一致（否则可见页会被误判）。

use crate::config::pdf::{
    CACHE_MAX_PAGES, MARGIN, MAX_RENDER_SCALE, PAGE_GAP, PAGE_PAD_Y,
};

/// 页槽：某页的渲染状态（内存态）。
#[derive(Default, Clone)]
pub(crate) struct Slot {
    /// 已栅格化位图（`None` = 从未渲染或已回收）。
    pub(crate) image: Option<std::sync::Arc<gpui::RenderImage>>,
    /// 是否正在后台栅格化（避免重复派发）。
    pub(crate) loading: bool,
    /// 该位图对应的渲染代次（缩放/质量变化时使旧位图失效）。
    pub(crate) generation: u64,
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
pub(crate) fn render_scale(page_width: f32, scale_factor: f32, page_pt_width: f32) -> f32 {
    if page_pt_width > 0.0 {
        (page_width * scale_factor / page_pt_width).clamp(0.5, MAX_RENDER_SCALE)
    } else {
        1.5
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

    /// 基准：`plan_page_work` 每帧「先按总页数物化 3 个 Vec 再规划」 vs
    /// 「闭包投影直接规划」（[`Self`] 里被 `view.rs` 每帧调用）。
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
