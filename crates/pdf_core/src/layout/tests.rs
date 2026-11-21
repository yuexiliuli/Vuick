    use super::*;
    use config::pdf::{PAGED_GAP_X, PAGED_GAP_Y};
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
    fn min_ar_uniform_multi_page_takes_min_w_over_h() {
        // 均匀多页：min_ar = 任意页的 w/h（A4 595×842 ≈ 0.70665）。
        let dims: Vec<(f32, f32)> = vec![a4(); 5];
        let v = min_ar(&dims);
        assert!(
            (v - 595.0 / 842.0).abs() < 1e-5,
            "均匀 A4 多页 min_ar 应为单页纵横比：{v}"
        );
    }

    #[test]
    fn min_ar_skips_degenerate_and_all_degenerate_falls_back() {
        // 含退化页（w≤0 ∨ h≤0）：跳过，取其余竖页的最小 w/h。
        let v = min_ar(&[(595.0, 842.0), (0.0, 100.0), (1.0, 0.0)]);
        assert!(
            (v - 595.0 / 842.0).abs() < 1e-5,
            "退化页应被跳过、取竖页纵横比：{v}"
        );
        // 混合尺寸：横页(841.89,595.28) 纵横比 > 竖页 → 竖页受限。
        let v2 = min_ar(&[(595.28, 841.89), (841.89, 595.28)]);
        assert!((v2 - 595.28 / 841.89).abs() < 1e-5, "应取最竖页 w/h：{v2}");
        // 全退化 / 空 → 1.0（与 paged_layout 行内全退化回退同源）。
        assert_eq!(min_ar(&[(0.0, 1.0), (1.0, 0.0)]), 1.0);
        assert_eq!(min_ar(&[]), 1.0);
    }

    #[test]
    fn min_ar_contain_fit_continuous_matches_facing_fit() {
        // 连续「整页入屏」基线与 Facing 同 fit 口径（K177 演算，锚 view.rs open_paged_host
        // 1000×700、A4ish (793,1123)，对照既存 `paged_mid_row_render_window_covers_visible_rows`
        // Facing 演算 494.3）：
        //   doc_min_ar = min_ar([(793,1123)]) = 793/1123 ≈ 0.706144；
        //   continuous: content_w = 1000 − 2×PAGE_PAD_X = 960、
        //      base = min(960, 700×0.706144 ≈ 494.30) = 494.30（高度轴限幅主导）；
        //   Facing:     fit_w = 700×0.706144 ≈ 494.30（行内 min_ar 同源数学）→ 两模式同一值。
        // 2-up 半宽封顶例外（Facing cols=2 cap_w=(960−1)/2=479.5 < 494.3）→ cell_w=479.5，
        // 两模式差 ≈3% 属预期（brief 明确「也算通过」）。
        let dims = [(793.0, 1123.0)];
        let doc_min_ar = min_ar(&dims);
        assert!(
            (doc_min_ar - 793.0 / 1123.0).abs() < 1e-5,
            "doc_min_ar 应为单页纵横比：{doc_min_ar}"
        );
        let content_w = (1000.0 - 2.0 * config::pdf::PAGE_PAD_X).max(1.0);
        let cont_base = content_w.min(700.0 * doc_min_ar);
        assert!(
            (cont_base - 494.3).abs() < 0.5,
            "连续 contain-fit 基线应≈ 494.3（= viewport_h×min_ar）：{cont_base}"
        );
        // 与 Facing 同视口高 2 列（无 2-up 封顶时）的 fit_w 逐值一致。
        let l = paged_layout(&dims, 1500.0, 700.0, 2, PAGED_GAP_X, PAGED_GAP_Y, 20.0);
        assert!(
            (l.cell_w_per_row[0] - cont_base).abs() < 0.5,
            "连续 contain-fit 应与 Facing fit_w 同值（同 min_ar 数学）：cont={cont_base:.2} facing={:.2}",
            l.cell_w_per_row[0]
        );
    }

    #[test]
    fn keep_window_covers_only_visible_plus_margin() {
        // 30 页 A4，页宽 840 → 每页高约 1189；视口 600px 高，滚到第 10 页附近
        let dims: Vec<(f32, f32)> = vec![a4(); 30];
        let (tops, _) = compute_layout(&dims, 840.0);
        let scroll_y = 10.0 * page_step(840.0);
        let (s, e) = keep_window(&dims, &tops, 840.0, scroll_y, 600.0);
        // 第 10 页应可见，区间含 MARGIN 缓冲且不越界
        assert!(s <= 10 && 10 <= e, "第 10 页应在可见区间 [{s},{e}]");
        assert!(e - s + 1 > 2 * MARGIN, "区间宽度应至少为可见页 + 2*MARGIN");
        assert!(e <= 29, "末页不越界");
    }

    #[test]
    fn keep_window_empty_dims_returns_zero() {
        assert_eq!(keep_window(&[], &[], 840.0, 0.0, 600.0), (0, 0));
    }

    #[test]
    fn first_visible_page_tracks_viewport_not_margin_buffer() {
        // 30 页 A4，页宽 840 → 每页高约 1189。滚到第 10 页（1 基）顶附近：首张**可见**
        // 页正是页底越进视口顶的 0 基 9；keep_window 的窗口起点会再前移 MARGIN 张——
        // 页码读数必须用前者（否则状态栏显示「第 1 / N 页」时实际已看到更后面的页）。
        let dims: Vec<(f32, f32)> = vec![a4(); 30];
        let (tops, _) = compute_layout(&dims, 840.0);
        let step = page_step(840.0);
        let scroll_y = 10.0 * step;
        let visible = first_visible_page(&dims, &tops, 840.0, scroll_y);
        assert_eq!(
            visible, 9,
            "第二十滚位=10 步：页底越过视口顶的首张页应 0 基 9"
        );
        let (s, _) = keep_window(&dims, &tops, 840.0, scroll_y, 600.0);
        assert_eq!(
            s,
            visible - MARGIN,
            "keep_window 起点含 MARGIN 缓冲（≠ visible）"
        );

        // 顶部：第 0 页；空 dims / 负滚动安全。
        assert_eq!(first_visible_page(&dims, &tops, 840.0, 0.0), 0);
        assert_eq!(first_visible_page(&[], &[], 840.0, 0.0), 0);
        assert_eq!(first_visible_page(&dims, &tops, 840.0, -5.0), 0);
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
        let mut probes: Vec<f32> = vec![
            f32::MIN,
            -1000.0,
            -1.0,
            0.0,
            0.5,
            total,
            total + 100.0,
            1e9,
            f32::MAX,
        ];
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
            let got_first = first_visible_page(&dims, &tops, pw, scroll);
            assert_eq!(
                got_first, expect_first,
                "first_visible_page 在 scroll={scroll} 处偏离线性版"
            );
            for &vh in &[600.0_f32, 720.0, 1.0, 0.5, 0.0, 2000.0] {
                let expect_win = ref_keep_window(&dims, pw, scroll, vh);
                let got_win = keep_window(&dims, &tops, pw, scroll, vh);
                assert_eq!(
                    got_win, expect_win,
                    "keep_window 在 scroll={scroll}, viewport={vh} 处偏离线性版"
                );
            }
        }

        // 显式断言二分关键的「滚过末页回退 0」与「视口底压过末页」边界不退化。
        assert_eq!(
            first_visible_page(&dims, &tops, pw, total + 1.0),
            0,
            "滚过末页应回退 0"
        );
        assert_eq!(
            keep_window(&dims, &tops, pw, total + 1.0, 600.0).1,
            dims.len() - 1
        );
        assert_eq!(
            keep_window(&dims, &tops, pw, 0.0, 0.5).0,
            0,
            "视口高 ≤1 回退默认 720 时窗口起点仍为首页"
        );
    }

    /// 超长 top 表直读二分：5000 页缓存 tops 构造一次后直接传入，抽查首/中/末/越界滚动位，
    /// 与引用线性实现逐例对拍——锁「直读缓存 tops」路径（不再每帧内建 O(P) 表）行为不退化。
    #[test]
    fn binary_reads_long_cached_tops() {
        let dims: Vec<(f32, f32)> = vec![a4(); 5000];
        let pw = 840.0_f32;
        let (tops, total) = compute_layout(&dims, pw);
        assert_eq!(tops.len(), dims.len(), "tops 缓存与 dims 同长");
        for &scroll in &[
            0.0_f32,
            total * 0.25,
            total * 0.5,
            total - 0.5,
            total,
            total + 1.0,
            1e9,
        ] {
            assert_eq!(
                first_visible_page(&dims, &tops, pw, scroll),
                ref_first_visible_page(&dims, pw, scroll),
                "first_visible_page 长表 scroll={scroll} 偏离线性版"
            );
            for &vh in &[0.0_f32, 600.0, 2000.0] {
                assert_eq!(
                    keep_window(&dims, &tops, pw, scroll, vh),
                    ref_keep_window(&dims, pw, scroll, vh),
                    "keep_window 长表 scroll={scroll} vh={vh} 偏离线性版"
                );
            }
        }
        // 滚过末页回退 0 / 视口底压过末页夹到末页。
        assert_eq!(
            first_visible_page(&dims, &tops, pw, total + 1.0),
            0,
            "滚过末页应回退 0"
        );
        assert_eq!(
            keep_window(&dims, &tops, pw, total + 1.0, 600.0).1,
            dims.len() - 1
        );
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
    fn zoom_rescroll_anchor_contract_locks_anchor_page_linear_scaling() {
        // Z1 契约：缩放重锚 = 锚点页 K 页顶按新布局换算 + 页内相对偏移同比缩放；
        // 旧实现「整个 offset × ratio」对 K≥1 的常量区（PAGE_PAD_Y/GAP）也乘因子 → 必漂移。
        let dims: Vec<(f32, f32)> = vec![a4(); 5]; // 多页同 pt
        let old_w = 1200.0;
        let new_w = 1344.0; // ×1.12
        let ratio = new_w / old_w;
        let (tops_old, _) = compute_layout(&dims, old_w);
        let (tops_new, _) = compute_layout(&dims, new_w);
        // 逐页在中部采样：K=0..=4 都要满足契约（K=0 常量区小、K=4 最大）。
        for k in 0..dims.len() {
            let h_k = display_height(dims[k], old_w);
            let scroll_y = tops_old[k] + h_k * 0.35; // 落在页 k 中部
            // 锚点语义：scroll 贴页 k 顶→锚 k；此处 mid→first_visible_page 应返回 k。
            assert_eq!(
                first_visible_page(&dims, &tops_old, old_w, scroll_y),
                k,
                "采样点应锚页 {k}"
            );
            let got = zoom_rescroll_anchor(&dims, &tops_old, &tops_new, old_w, ratio, scroll_y);
            let want = tops_new[k] + (scroll_y - tops_old[k]) * ratio;
            assert!(
                (got - want).abs() < 1e-2,
                "页 {k} 契约：got={got} want={want}"
            );
            // 旧公式（整段 ×ratio）对 K≥1 必 FAIL（常量区被错乘）。
            if k >= 1 {
                let legacy = scroll_y * ratio;
                let drift = (legacy - want).abs();
                assert!(
                    drift > 2.0,
                    "旧公式对 K={k} 应产生明显漂移（>2px=半行+），实际 {drift:.2}px —— 契约无鉴别力"
                );
            }
        }
        // 对称性：2 档进 + 2 档退 → 回原档无漂移（来回各步锚点页内位置守恒）。
        let scroll_0 = tops_old[1] + display_height(dims[1], old_w) * 0.5;
        let (tops1, _) = compute_layout(&dims, old_w * 1.12);
        let y1 = zoom_rescroll_anchor(&dims, &tops_old, &tops1, old_w, 1.12, scroll_0);
        let (tops0b, _) = compute_layout(&dims, old_w);
        let y_back = zoom_rescroll_anchor(&dims, &tops1, &tops0b, old_w * 1.12, 1.0 / 1.12, y1);
        assert!(
            (y_back - scroll_0).abs() < 0.5,
            "缩放回原档应无漂移：{scroll_0} →退→ {y_back}"
        );
    }

    /// #1 往返（Z1.1 扩展）：**3 档进 + 3 档退**（各档用精确逆比 1.12 / 1/1.12、深页 K=4）
    /// → 同字符 window-y 与初始差 <0.5px（纯公式层：布局/放置/栅格尺寸对同一 page_width
    /// 两次取一致——`page_bitmap_size`/`bitmap_display_scales`/`snap_to_device` 均幂等）。
    #[test]
    fn zoom_three_notch_roundtrip_within_half_px() {
        let dims: Vec<(f32, f32)> = vec![a4(); 8]; // 深页 K=4（常量区累计最大）
        let w0 = 1460.0f32; // 实测初始 fit 宽
        let (tops0, _) = compute_layout(&dims, w0);
        // 同一 page_width 两次取栅格尺寸/放置一致（幂等）。
        let r0 = render_scale(w0, 2.0, a4().0);
        let b0a = page_bitmap_size(a4(), r0);
        let b0b = page_bitmap_size(a4(), render_scale(w0, 2.0, a4().0));
        assert_eq!(b0a, b0b, "同一 page_width 两次取栅格尺寸应一致（幂等）");
        assert_eq!(
            bitmap_display_scales(b0a, w0, display_height(a4(), w0)),
            bitmap_display_scales(b0b, w0, display_height(a4(), w0)),
            "同一 page_width 两次放置缩放应一致"
        );
        // 3 档进 + 3 档退（精确逆比）→ 同字符 window-y 与初始差 <0.5px。
        let k = 4usize;
        let scroll0 = tops0[k] + display_height(dims[k], w0) * 0.35;
        let mut w = w0;
        let mut tops = tops0;
        let mut y = scroll0;
        for _ in 0..3 {
            let w1 = w * 1.12;
            let (tops1, _) = compute_layout(&dims, w1);
            y = zoom_rescroll_anchor(&dims, &tops, &tops1, w, 1.12, y);
            w = w1;
            tops = tops1;
        }
        for _ in 0..3 {
            let ratio = 1.0 / 1.12;
            let w1 = w * ratio;
            let (tops1, _) = compute_layout(&dims, w1);
            y = zoom_rescroll_anchor(&dims, &tops, &tops1, w, ratio, y);
            w = w1;
            tops = tops1;
        }
        assert!(
            (y - scroll0).abs() < 0.5,
            "3 进 3 退（精确逆比）同字符 window-y 差应 <0.5px：scroll0={scroll0} 回={y} Δ={:.3}",
            y - scroll0
        );
    }

    #[test]
    fn plan_renders_only_unrendered_visible_pages() {
        // 10 页，第 0 页已渲染且代次一致；视口 [0,2] → 只应渲染 1、2
        let rendered = [
            true, false, false, false, false, false, false, false, false, false,
        ];
        let loading = [false; 10];
        let gens = [0u64; 10];
        let n = rendered.len();
        let (render, evict) =
            plan_page_work(n, 0, 2, 0, |i| rendered[i], |i| loading[i], |i| gens[i]);
        assert_eq!(render, vec![1, 2], "仅未渲染的可见页应列入渲染");
        assert!(evict.is_empty(), "无越界已渲染页不应淘汰");
    }

    #[test]
    fn plan_skips_pages_already_loading() {
        // 可见页 1 正在加载 → 不应重复派发渲染任务（CPU 占用暴增的元凶）
        let rendered = [
            true, false, false, false, false, false, false, false, false, false,
        ];
        let mut loading = [false; 10];
        loading[1] = true;
        let gens = [0u64; 10];
        let n = rendered.len();
        let (render, _) = plan_page_work(n, 0, 2, 0, |i| rendered[i], |i| loading[i], |i| gens[i]);
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
        assert!(
            evict.is_empty(),
            "预算未满不应淘汰任何页（即使远在 40 页之外）"
        );
    }

    #[test]
    fn plan_evicts_farthest_first_when_over_budget() {
        // 200 页都渲染（超预算 CACHE_MAX_PAGES），视口在 [100,105]。
        // 应淘汰离中心最远的页（最前/最后），而可见页绝不能被淘汰。
        let n = 200usize;
        let loading = vec![false; n];
        let gens = vec![0u64; n];
        let (r, evict) = plan_page_work(n, 100, 105, 0, |_| true, |i| loading[i], |i| gens[i]);
        assert!(r.is_empty(), "全部已渲染，无需再渲染");
        // 回收到预算内
        assert_eq!(n - evict.len(), CACHE_MAX_PAGES, "保留数应等于预算");
        assert!(
            evict.contains(&0) && evict.contains(&199),
            "最远两端应先被淘汰"
        );
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
        let (tops, _) = compute_layout(&dims, 840.0);
        let (s, e) = keep_window(&dims, &tops, 840.0, 0.0, 0.0);
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

    /// #1 页卡设备像素网格对齐：`snap_to_device` 的结果必落在设备像素网格上（幂等、往返闭合），
    /// 取整方向用 round（沪深各半设备 px），sf=1 时退化为整数取整。
    #[test]
    fn snap_to_device_snaps_to_device_grid() {
        for &sf in &[1.0f32, 1.25, 1.5, 2.0, 3.0] {
            for &v in &[0.0f32, 13.7, 516.31, 840.0, 0.5, 123.499, 123.501] {
                let s = snap_to_device(v, sf);
                // 结果 ×sf 为整数（设备像素网格）……
                assert!(
                    (s * sf - (s * sf).round()).abs() < 1e-3,
                    "snap({v}, sf={sf}) = {s} 应落在设备网格（s×sf 整数）"
                );
                // ……且与输入差 ≤ 0.5 设备 px（round 语义）……
                assert!(
                    (s - v).abs() * sf <= 0.5 + 1e-3,
                    "snap({v}, sf={sf}) = {s} 与输入差应 ≤0.5 设备 px"
                );
                // ……幂等：同输入两次取同一结果（确定性放置网格）。
                assert_eq!(s, snap_to_device(s, sf), "snap 应幂等：{s} @sf={sf}");
            }
        }
        // 往返闭合：snap 后再 snap（不同 sf 无意义，同 sf 恒等）。
        assert_eq!(
            snap_to_device(snap_to_device(123.456, 2.0), 2.0),
            snap_to_device(123.456, 2.0)
        );
        // 极端 sf 含 0 不 panic（内部夹到最小 0.01）。
        assert!(snap_to_device(1.5, 0.0).is_finite());
    }

    /// 缩放发糊根治不变式：`page_width()` 经 `snap_to_device` 量化后，任意缩放档下
    /// `page_width·sf` 恒为整数 → 位图宽 `ceil(page_width·sf)` 与显示矩形（GPUI 绘制时
    /// `snap_bounds` 对设备像素取整）逐纹素 1:1，绘制 scale≈1（无双线性重采样）。
    /// 覆盖 fit 宽 × zoom 档 × 多 DPI；仅取 `render_scale` 未被封顶（`MAX_RENDER_SCALE`/
    /// `MAX_BITMAP_SCALE`，那是独立的「超大位图护栏」）的区间——封顶路径本就确认位图
    /// 按上限采样、非 1:1 属设计语义。
    #[test]
    fn quantized_page_width_is_device_pixel_1_to_1() {
        let dim = (595.0f32, 842.0f32); // A4
        let base_ws = [PAGE_WIDTH, 1012.5f32, 1460.0f32];
        // 缩放档：1.0(适配宽) + 1.12^k 档位链条（含中间分数值，如 985.6/1103.87）+ 缩档 0.75
        let zooms = [0.5f32, 0.75, 1.0, 1.12, 1.2544, 1.404928, 1.57351936, 2.0];
        let sfs = [1.0f32, 1.25, 1.5, 2.0, 2.5];
        for &bw in &base_ws {
            for &z in &zooms {
                for &sf in &sfs {
                    let raw = bw * z;
                    let pw = snap_to_device(raw, sf); // 与 page_width() 同款量化
                    let dev = pw * sf;
                    assert!(
                        (dev - dev.round()).abs() < 1e-3,
                        "量化后 page_width·sf 应为设备整数：raw={raw} pw={pw} sf={sf} dev={dev}"
                    );
                    // 封顶护栏判定：scale 被 cap 夹住（位图按上限采样）→ 跳过 1:1 断言。
                    let scale = render_scale(pw, sf, dim.0);
                    let zoom_ratio = (pw / PAGE_WIDTH).max(1.0);
                    let cap = (MAX_RENDER_SCALE * zoom_ratio * sf.max(1.0)).min(MAX_BITMAP_SCALE);
                    let clamped = scale >= cap - 1e-4;
                    if clamped {
                        continue;
                    }
                    let bmp = page_bitmap_size(dim, scale);
                    assert_eq!(
                        bmp.0 as f32,
                        dev.round(),
                        "未封顶下位图宽应等于显示设备宽（1:1）：raw={raw} pw={pw} sf={sf} dev={dev}"
                    );
                    assert!(
                        (bmp.0 as f32 / dev - 1.0).abs() < 1e-3,
                        "绘制 scale≈1（≤1e-3）：raw={raw} dev={dev} bmp_w={}",
                        bmp.0
                    );
                }
            }
        }
    }

    /// #4 cap 刀刃稳健化：`cover_serves_scale` 在 `scale ∈ [cap, COVER_DENSITY_EPS_ZOOM]`
    /// 恒判 cover-served（跨 cap 的 LOD 决策无 run-to-run 翻转），区间外仍精确判定。
    #[test]
    fn cover_serves_scale_covers_cap_blade() {
        let cap = config::pdf::COVER_SCALE_CAP; // 4.0
        // ① 未截断区间：cover_scale == render_scale（精确同密度）→ cover-served。
        assert!(
            cover_serves_scale(3.99, 3.99),
            "未截断 3.99 同密度应 cover-served"
        );
        assert!(
            cover_serves_scale(cap, cap),
            "恰 cap 应 cover-served（同密度）"
        );
        // ② 截断但贴近 cap：coverscale=cap（4.0）、scale ∈ (cap, 4.06] → cover-served。
        //    4.0→4.02 跨 cap 的 LOD 决策两侧均为 cover-served（无「一个 cover(锐)/一个瓦片(糊)」翻转）。
        assert!(
            cover_serves_scale(cap, 4.02),
            "4.0->4.02 跨 cap 应保持 cover-served（无翻转）"
        );
        assert!(
            cover_serves_scale(cap, config::pdf::COVER_DENSITY_EPS_ZOOM),
            "上界 4.06 应 cover-served"
        );
        // ③ 区间外（>4.06）或非 cap 截断 → 精确判定（4.1+ 必须回 1:1 瓦片，清晰度优先）。
        assert!(cover_serves_scale(1.4, 1.4), "精确同密度应 cover-served");
        assert!(
            !cover_serves_scale(1.4, 1.5),
            "非 cap 截断且不等密 → 不 serve"
        );
        assert!(
            !cover_serves_scale(cap, 4.2),
            "超出等效上界（4.2>4.06）→ 回 1:1 瓦片"
        );
        // 亚 cap 截断值（<cap 的 cover，如 3.9）不得因 scale 靠近 cap 而误判 serve。
        assert!(
            !cover_serves_scale(3.9, 3.99),
            "cover 未到 cap 截断 → 按精确密度判定"
        );
    }

    /// #4 端到端（纯函数层）：A4 整页在 3.99 / 4.02 跨 cap 两档的「封面是否供给瓦片」决策
    /// 逐档一致（cover-served），4.06 上界内仍为 cover-served、4.2 必须 1:1 瓦片——与
    /// `plan_tiles_in_window` 的 `cover_affordable && cover_serves_scale` 判定同构。
    #[test]
    fn cap_blade_lod_decision_invariant_across_flip() {
        let a4 = (595.0f32, 842.0f32);
        // 判定 = `plan_tiles_in_window` 的 cover_serves_tile_density（去掉黑名单等状态）。
        let serves = |s: f32| {
            let c = s.min(config::pdf::COVER_SCALE_CAP);
            cover_affordable(a4, c) && cover_serves_scale(c, s)
        };
        // 3.99 与 4.02 跨 cap 两翼：决策同为 cover-served（无 run-to-run 翻转）。
        let s399 = serves(3.99);
        let s402 = serves(4.02);
        assert!(s399, "3.99（cap 下邻域）应 cover-served");
        assert!(s402, "4.02（cap 上邻域）应 cover-served");
        assert_eq!(s399, s402, "跨 cap 的 LOD 决策必须一致（时锐时糊根治）");
        assert!(
            serves(config::pdf::COVER_DENSITY_EPS_ZOOM),
            "4.06 上界内 cover-served"
        );
        assert!(!serves(4.2), "4.2 超等效上界 → 1:1 瓦片");
    }

    /// 页内 pt → 卡片局部 px：Y 翻转、边界与单调性（纯函数，不触 pdfium）。
    #[test]
    fn pt_to_local_flips_y_and_maps_corners() {
        let (pw, ph) = a4(); // 595 x 842
        let (dw, scale) = (840.0f32, 1.4f32);
        let disp_h = display_height(a4(), dw);
        // 页左上 (0, ph) → 卡片左上 (0, 0)
        let (x, y) = pt_to_local(0.0, ph, pw, ph, dw, scale);
        assert!(
            x.abs() < 1e-2 && y.abs() < 1e-2,
            "页左上应映射到卡片左上: ({x},{y})"
        );
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
        assert!(
            (lx - 10.0).abs() < 0.01 && (ly - 100.0).abs() < 0.01,
            "页内局部坐标应正确"
        );
        let (p1, _, _) = content_to_local(&dims, &tops, page_width, 10.0, tops[1] + 50.0)
            .expect("第 2 页应命中");
        assert_eq!(p1, 1);
        // 页间沟槽中点
        let gutter = tops[0] + display_height(a4(), page_width) + PAGE_GAP / 2.0;
        assert!(
            content_to_local(&dims, &tops, page_width, 10.0, gutter).is_none(),
            "沟槽应 None"
        );
        // 列外 x
        assert!(
            content_to_local(&dims, &tops, page_width, -5.0, tops[0] + 100.0).is_none(),
            "左缘外应 None"
        );
        assert!(
            content_to_local(&dims, &tops, page_width, page_width + 5.0, tops[0] + 100.0).is_none(),
            "右缘外应 None"
        );
    }

    /// 命中测试：盒内含点优先（中心最近），空隙贴最近行。
    #[test]
    fn hit_char_prefers_contained_then_nearest() {
        let text = page_text_of(
            vec![
                tchar('A', 10.0, 20.0, 12.0, 22.0, 20.0),
                tchar('B', 14.0, 20.0, 16.0, 22.0, 20.0),
                tchar('S', 0.0, 0.0, 0.0, 0.0, 0.0), // synthetic：应被跳过
            ],
            1,
        );
        assert_eq!(hit_char(&text, 11.0, 21.0), Some(0), "点在 A 盒内");
        assert_eq!(hit_char(&text, 15.0, 21.0), Some(1), "点在 B 盒内");
        assert_eq!(hit_char(&text, 14.5, 21.0), Some(1), "格间空隙应贴最近");
        assert_eq!(hit_char(&text, 11.0, 25.0), Some(0), "行下空隙应贴最近行");
    }

    /// 选区合并成行矩形：同行顺延合并，synthetic 零宽盒不产生子矩形。
    #[test]
    fn selection_spans_merge_by_line_and_skip_synthetic() {
        let text = page_text_of(
            vec![
                tchar('a', 10.0, 100.0, 12.0, 112.0, 100.0),
                tchar('b', 13.0, 100.0, 15.0, 112.0, 100.0),
                tchar('c', 10.0, 80.0, 14.0, 92.0, 80.0),
                tchar('d', 16.0, 80.0, 20.0, 92.0, 80.0),
                PdfTextChar {
                    synthetic: true,
                    ..tchar('s', 0.0, 0.0, 0.0, 0.0, 0.0)
                },
            ],
            1,
        );
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
        let text = page_text_of(
            vec![
                tchar('a', 10.0, 100.0, 12.0, 112.0, 100.0),
                tchar('b', 13.0, 100.0, 15.0, 112.0, 100.0),
                tchar('c', 30.0, 100.0, 32.0, 112.0, 100.0), // 与 b 间隙 15 > 1.5*avg(2)
                tchar('\0', 0.0, 0.0, 0.0, 0.0, 100.0),      // 填充 NUL
                tchar('d', 10.0, 80.0, 12.0, 92.0, 80.0),    // 基线降 20 → 换行
                tchar('e', 13.0, 80.0, 15.0, 92.0, 80.0),
            ],
            1,
        );
        assert_eq!(page_segment_text(&text, 0, 5), "ab c\nde");
    }

    /// 原文子串拼接：跳过 NUL、越界下标钳制。
    #[test]
    fn page_char_substring_skips_nul_and_clamps() {
        let text = page_text_of(
            vec![
                tchar('a', 0.0, 0.0, 1.0, 1.0, 0.0),
                tchar('\0', 0.0, 0.0, 0.0, 0.0, 0.0),
                tchar('c', 2.0, 0.0, 3.0, 1.0, 0.0),
            ],
            0,
        );
        assert_eq!(page_char_substring(&text, 0, 2), "ac", "NUL 应被跳过");
        assert_eq!(page_char_substring(&text, 0, 99), "ac", "越界 hi 应钳制");
        assert_eq!(page_char_substring(&text, 5, 9), "", "全越界应为空");
    }

    /// P1-15：空字符集下文本提取三函数不得越界 panic——空 chars 时 `chars[0]` 直接越界
    /// （`chars.len().saturating_sub(1)` 把 hi 钳成 0，`lo=hi=0` 仍进 `chars[0]`）。回归锚：
    /// 空输入一律返回安全空结果（选区空矩形 / 空串），与「全越界区间为空」既有语义一致。
    #[test]
    fn empty_chars_text_extractors_return_empty_without_panic() {
        let empty = page_text_of(vec![], 0);
        assert_eq!(
            selection_spans_pt(&empty, 0, 0),
            Vec::<(f32, f32, f32, f32)>::new(),
            "空 chars 选区矩形应为空（不得越界 panic）"
        );
        assert_eq!(
            page_segment_text(&empty, 0, 0),
            "",
            "空 chars 段文本应为空串（不得越界 panic）"
        );
        assert_eq!(
            page_char_substring(&empty, 0, 0),
            "",
            "空 chars 原文子串应为空串（不得越界 panic）"
        );
        // 全越界区间同样安全（既有「全越界为空」语义保持）
        assert_eq!(
            page_segment_text(&empty, 3, 9),
            "",
            "空 chars 越界区间段文本应为空串"
        );
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
        assert_eq!(
            search_snippet(&with_nul, 3, 5, 0),
            "cd",
            "NUL 不进入可见序列"
        );
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
            let (tr, _ev) = plan_page_work(n, start, end, generation, |i| r[i], |i| l[i], |i| g[i]);
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
        assert!(
            tile_rect(2, 0, 512, 2, bmp).is_none(),
            "第 3 列整格在页外应 None"
        );
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
        assert!(
            (sx - 1.0).abs() < 1e-3 && (sy - 1.0).abs() < 1e-3,
            "1x 应 1:1"
        );
        // 2x DPI：位图 1680×2377 显示在 840×1189 CSS px → 每个 CSS px = 2 位图像素
        let (sx2, sy2) = bitmap_display_scales((1680, 2377), 840.0, 1189.0);
        assert!(
            (sx2 - 0.5).abs() < 1e-3 && (sy2 - 0.5).abs() < 1e-3,
            "2x 应半缩"
        );
    }

    /// 可见矩形 → 瓦片区间：只圈中相交的格、外扩 margin、且夹进网格。
    #[test]
    fn visible_tile_range_covers_only_intersecting_tiles() {
        let bmp = page_bitmap_size(a4(), 1.0); // 595×842，512 片 → 2×2
        // 只看左上角 200×100 CSS px（@1x 即位图像素 200×100）→ 只应圈第 0 行 0 列
        let mut r =
            visible_tile_range(bmp, 595.0, 842.0, (0.0, 0.0, 200.0, 100.0), 512, 0).unwrap();
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

    /// `tile_range_all_in_slot`：仅在区间每一格都在槽位时返回 true（S 系列原子换 /
    /// S5 回收共用的可见区铺满判定）。
    #[test]
    fn tile_range_all_in_slot_requires_every_cell_present() {
        let mut tiles: HashMap<(u32, u32), TileSlot> = HashMap::new();
        let mk = || TileSlot {
            image: crate::pdf::render_image_from_bgra(vec![0u8; 4 * 4 * 4], 4, 4).unwrap(),
            x0: 0,
            y0: 0,
            w: 4,
            h: 4,
        };
        // 空槽：任何区间都不完整。
        assert!(
            !tile_range_all_in_slot(&tiles, 0, 0, 1, 1),
            "空槽应判不完整"
        );
        // 只放 (0,0) → 1×1 区间完整、2×2 区间缺 (0,1)/(1,0)/(1,1)。
        tiles.insert((0, 0), mk());
        assert!(tile_range_all_in_slot(&tiles, 0, 0, 0, 0), "单格在场应完整");
        assert!(
            !tile_range_all_in_slot(&tiles, 0, 0, 1, 1),
            "缺格应判不完整"
        );
        // 补齐 2×2 → 完整。
        tiles.insert((0, 1), mk());
        tiles.insert((1, 0), mk());
        assert!(
            !tile_range_all_in_slot(&tiles, 0, 0, 1, 1),
            "仍缺一格应不完整"
        );
        tiles.insert((1, 1), mk());
        assert!(tile_range_all_in_slot(&tiles, 0, 0, 1, 1), "2×2 齐备应完整");
    }

    /// 面积闸（render_page 口径）：A4 各档（1.0/2.0/3.0/4.0）均在预算内不误杀；
    /// 超 cap（5.0）才越闸。A4@4.0 = 2380×3368 ≈ 8M px < 12M px 预算。
    #[test]
    fn area_gate_keeps_a4_at_normal_scales() {
        for s in [1.0f32, 2.0, 3.0, 4.0] {
            let (w, h) = page_bitmap_size(a4(), s);
            assert!(pixel_area_ok(w, h), "A4@{s} {w}×{h} 应恰在闸内");
        }
        let (w, h) = page_bitmap_size(a4(), 5.0);
        assert!(!pixel_area_ok(w, h), "A4@5.0 {w}×{h} 应越闸");
    }

    /// 面积闸边界与病态：预算 12M px 的边界（3464×3464 内、3465×3465 外）；
    /// u32 天文尺寸走 saturating 不 panic；0 面积在预算内（0 尺寸位图由调用方各自拒绝——
    /// render 恒 ≥1px、extract 先 `bw<=0||bh<=0` 返回 None，闸只管面积上限）。
    #[test]
    fn area_gate_rejects_pathological_and_zero_dims() {
        assert!(pixel_area_ok(3464, 3464), "3464² = 11,999,296 ≤ 预算");
        assert!(!pixel_area_ok(3465, 3465), "3465² = 12,006,225 > 预算");
        assert!(!pixel_area_ok(100_000, 100_000), "10^10 px 病态尺寸应拒");
        assert!(
            !pixel_area_ok(u32::MAX, u32::MAX),
            "u32 天文尺寸应拒且不 panic"
        );
        assert!(pixel_area_ok(0, 100), "0 面积在预算内");
        assert!(pixel_area_ok(100, 0), "0 面积在预算内");
    }

    /// 面积闸「+1 行/列松弛」（extract 第二闸对实际渲染尺寸，评审 NIT#2）：
    /// 恰打满预算的合法大图（4000×3000 = 12M）pdfium 双渲 +1 行（4000×3001）时精确闸
    /// 会误伤为 None，松弛变体放行；单轴 +1 均覆盖；两轴同 +1 或病态天文仍拒。
    #[test]
    fn area_gate_with_row_slack_tolerates_boundary_plus_one() {
        assert!(
            pixel_area_ok(4000, 3000),
            "4000×3000 = 12,000,000 恰打满预算"
        );
        assert!(
            !pixel_area_ok(4000, 3001),
            "精确闸应拒 4000×3001（12,004,000 > 12M）"
        );
        assert!(
            pixel_area_ok_with_row_slack(4000, 3001),
            "+1 行松弛应放行 4000×3001（≤ 预算 + max 轴）"
        );
        // 单轴 +1（行或列）都在松弛内；两轴同 +1 超出松弛（回归边界，注明）。
        assert!(
            pixel_area_ok_with_row_slack(3465, 3464),
            "3465×3464 单轴 +1 应放行"
        );
        assert!(
            pixel_area_ok_with_row_slack(3464, 3465),
            "3464×3465 单轴 +1 应放行"
        );
        assert!(
            !pixel_area_ok_with_row_slack(3465, 3465),
            "两轴同 +1 超出松弛"
        );
        assert!(
            pixel_area_ok_with_row_slack(2380, 3369),
            "A4@4.0 +1 行应在松弛内"
        );
        // 病态天文尺寸即使 +1 松弛也照拒（松弛只有一行，防内存爆炸目的不变）。
        assert!(
            !pixel_area_ok_with_row_slack(100_000, 100_000),
            "10^10 px 仍应拒"
        );
        assert!(
            !pixel_area_ok_with_row_slack(3465, 10_000_000),
            "极端宽仍应拒"
        );
        assert!(
            !pixel_area_ok_with_row_slack(u32::MAX, u32::MAX),
            "u32 天文仍应拒且不 panic"
        );
    }

    /// 缩档口径（extract 请求侧）：正常尺寸原样返回（尺寸稳定，不扰 1:1 复制语义）。
    #[test]
    fn budget_clamped_keeps_normal_sizes_unchanged() {
        assert_eq!(budget_clamped_dims(595, 842), Some((595, 842)));
        let (w, h) = page_bitmap_size(a4(), 4.0);
        assert_eq!(budget_clamped_dims(w, h), Some((w, h)), "A4@4.0 应原样");
        // 边界内大图（3464²）也原样。
        assert_eq!(budget_clamped_dims(3464, 3464), Some((3464, 3464)));
        // 0 尺寸 → None（调用方拒绝）。
        assert_eq!(budget_clamped_dims(0, 0), None);
        assert_eq!(budget_clamped_dims(100, 0), None);
    }

    /// 缩档口径：病态天文尺寸被缩到闸内，且与 `pixel_area_ok` 双点口径一致。
    #[test]
    fn budget_clamped_downscales_pathological_into_budget() {
        // 方图病态：等比缩后面积 ≤ 预算、比例保持、不 panic。
        let (w, h) = budget_clamped_dims(100_000, 100_000).expect("方图病态应缩档而非拒");
        assert!(pixel_area_ok(w, h), "缩档后 {w}×{h} 应 ≤ 预算");
        assert!(w >= 1 && h >= 1);
        let ratio = w as f64 / h as f64;
        assert!((ratio - 1.0).abs() < 0.02, "方图缩档应保持比例: {w}×{h}");
        // 超扁病态（宽 10^8 × 1）：短轴触底时按预算反算长轴，乘积 ≤ 预算、有界。
        let (w2, h2) = budget_clamped_dims(100_000_000, 1).expect("超扁病态应缩档");
        assert!(pixel_area_ok(w2, h2), "缩档后 {w2}×{h2} 应 ≤ 预算");
        assert!(w2 <= COVER_AREA_BUDGET as u32 && h2 >= 1);
        // u32 天文尺寸走 saturating，不 panic 且结果在闸内。
        let (w3, h3) = budget_clamped_dims(u32::MAX, u32::MAX).expect("u32 天文应缩档");
        assert!(pixel_area_ok(w3, h3), "缩档后 {w3}×{h3} 应 ≤ 预算");
    }

    // ---- 网格视图（GridLayout）测试 ----

    const GRID_SQRT2: f32 = 1.414_213_56; // √2 ≈ 1.41421356（测试用竖版页纵横比）

    /// 测试用固定参数：卡宽 40、gap 8、padding 8、viewport 140（见 grid_layout 手算注释）。
    fn grid_3pages() -> GridLayout {
        grid_layout(&[(1.0, GRID_SQRT2); 3], 40.0, 8.0, 8.0, 140.0)
    }

    /// 用例 A：3 页全竖版（1×√2）、卡宽 40、gap 8、padding 8、viewport 140。
    ///
    /// 手算（新语义「卡宽跟随请求」）：
    /// - cols = floor((140 − 2×8 + 8) / (40 + 8)) = floor(132/48) = 2；
    /// - rows = ceil(3/2) = 2；
    /// - cell_w = 请求卡宽 40（不再按填满反算 58）；
    /// - 横向：C = 140 − 16 = 124、needed = (124 − 2×40)/1 = 44 > GAP_MAX(36) → 列间距封顶
    ///   col_gap = 36、left_pad = (124 − 80 − 36)/2 = 4（封顶后剩余居中）；
    /// - cell_h[i] = 40 × √2 ≈ 56.569；行 0/1 各含页，行高 = max = 56.569；
    /// - row_top = [8, 8 + 56.569 + 8 = 72.569]；
    /// - total = 8 + 56.569 + 56.569 + (2−1)×8 + 8 = 137.137。
    #[test]
    fn grid_layout_multiple_rows_hand_computed() {
        let g = grid_3pages();
        assert_eq!(g.cols, 2, "cols 手算应 2");
        assert_eq!(g.rows, 2, "rows 手算应 2");
        assert!(
            (g.cell_w - 40.0).abs() < 0.01,
            "cell_w 手算应 40（跟随请求）: {}",
            g.cell_w
        );
        assert_eq!(g.row_page_start, vec![0, 2, 3], "行起始页表 = [0,2,3]");
        // 横向新语义：needed = 44 > 36 → col_gap 封顶 36、left_pad = (124−80−36)/2 = 4。
        assert!(
            (g.col_gap - 36.0).abs() < 0.01,
            "col_gap 封顶应 36: {}",
            g.col_gap
        );
        assert!(
            (g.left_pad - 4.0).abs() < 0.01,
            "left_pad 封顶居中应 4: {}",
            g.left_pad
        );
        assert_eq!(g.row_top.len(), 2);
        assert!((g.row_top[0] - 8.0).abs() < 0.01, "行 0 顶 = padding 8");
        // 行 1 顶 = 8 + 56.569 + 8 = 72.569
        assert!(
            (g.row_top[1] - 72.569).abs() < 0.01,
            "行 1 顶手算 ≈72.569: {}",
            g.row_top[1]
        );
        // total = 8 + 56.569×2 + (2−1)×8 + 8 = 137.137
        assert!(
            (g.total_height - 137.137).abs() < 0.01,
            "总高手算 ≈137.137: {}",
            g.total_height
        );
        // cell_h 保持纵横比：= 40 × √2 ≈ 56.569
        for (i, h) in g.cell_h.iter().enumerate() {
            assert!((h - 56.569).abs() < 0.01, "页 {} 卡高 ≈56.569: {}", i, h);
        }
    }

    /// 用例 B：`cell_rect` 对每页手算 x/y/w/h 精确断言。
    ///
    /// 手算（同用例 A 布局，横向偏移 = padding + left_pad = 12，pitch = cell_w + col_gap = 40 + 36 = 76）：
    /// - 页 0：x = 12 + 0×76 = 12，y = row_top[0] = 8，w = 40，h ≈ 56.569；
    /// - 页 1：x = 12 + 1×76 = 88，y = 8，w = 40，h ≈ 56.569；
    /// - 页 2：x = 12（第 2 行第 0 列），y = 8 + 56.569 + 8 = 72.569，w = 40，h ≈ 56.569。
    #[test]
    fn grid_cell_rect_per_page_hand_computed() {
        let g = grid_3pages();
        let (x0, y0, w0, h0) = g.cell_rect(0).expect("页 0 应有卡");
        assert!(
            (x0 - 12.0).abs() < 0.01 && (y0 - 8.0).abs() < 0.01,
            "页 0 左上 = (padding+left_pad, padding) = (12,8)"
        );
        assert!(
            (w0 - 40.0).abs() < 0.01 && (h0 - 56.569).abs() < 0.01,
            "页 0 宽高 = (40, ≈56.569)"
        );
        let (x1, y1, w1, h1) = g.cell_rect(1).expect("页 1 应有卡");
        // 卡 i=1：x = 12 + 1×(40+36) = 88
        assert!(
            (x1 - 88.0).abs() < 0.01 && (y1 - 8.0).abs() < 0.01,
            "页 1 左上 = (88, 8)"
        );
        assert!(
            (w1 - 40.0).abs() < 0.01 && (h1 - 56.569).abs() < 0.01,
            "页 1 宽高 = (40, ≈56.569)"
        );
        let (x2, y2, _w2, h2) = g.cell_rect(2).expect("页 2 应有卡");
        // 卡 i=2：x = 12 + 0×76 = 12，y = row_top[1] = 72.569
        assert!(
            (x2 - 12.0).abs() < 0.01 && (y2 - 72.569).abs() < 0.01,
            "页 2 左上 = (12, ≈72.569)"
        );
        assert!((h2 - 56.569).abs() < 0.01, "页 2 卡高 ≈56.569");
        // 越界：i ≥ pages → None
        assert!(g.cell_rect(3).is_none(), "页 3 越界应 None");
    }

    /// 用例 C：`first_visible_grid_row` 具体 offset/viewport 手算断言（行 0 中途 / 整行 /
    /// 完全在总高外 / 完全在内容之上 / 悬在行间 gap）。
    ///
    /// 手算（同用例 A 布局）：行 0 纵跨 [8, 64.569]、行 1 纵跨 [72.569, 129.137]、总高 137.137。
    #[test]
    fn grid_first_visible_row_cases() {
        let g = grid_3pages();
        // 从行 0 中途：offset = 8 + 40 = 48（行 0 中部）、viewport 100 → 视口 [48, 148] 同时
        // 压住行 0（底 64.569 > 48）与行 1（顶 72.569 < 148）→ (0, 1)。
        assert_eq!(
            g.first_visible_grid_row(8.0 + 40.0, 100.0),
            Some((0, 1)),
            "行 0 中途 + 100 高视口应命中两行"
        );
        // 只命中行 1：offset = 72.569（行 1 顶）、viewport 50 → 视口 [72.569, 122.569]
        // 首行为行 1（行 0 底 64.569 ≤ 72.569）、末行为行 1 → (1, 1)。
        assert_eq!(
            g.first_visible_grid_row(g.row_top[1], 50.0),
            Some((1, 1)),
            "贴行 1 顶应只命中行 1"
        );
        // 完全在总高外（下方）：offset = 137.137 + 10 = 147.137 → 无行底 > 视口顶 → None。
        assert_eq!(
            g.first_visible_grid_row(g.total_height + 10.0, 100.0),
            None,
            "视口完全在内容之下应 None"
        );
        // 完全在内容之上：offset = −500、viewport 4（>1 不走默认回退）→ 视口 [0, 4] < 行 0 顶 8 → None。
        assert_eq!(
            g.first_visible_grid_row(-500.0, 4.0),
            None,
            "视口完全在内容之上应 None"
        );
        // 悬在行间 gap：offset = 68（行 0 底 64.569 与行 1 顶 72.569 之间）、viewport 2 →
        // 视口 [68, 70] 不与任何行相交 → None。
        assert_eq!(
            g.first_visible_grid_row(68.0, 2.0),
            None,
            "视口悬在行间 gap 应 None"
        );
    }

    /// 用例 D（重写：`page_index_at` 已删 —— 生产零调用，几何统一走
    /// `row_index_at_y`/`col_slot_at_x` 原语，view 侧 `compute_insert_index` 与命中语义基于
    /// 同一套原语）：行二分 / 列槽原语的 gap / padding / 末行 / 退化视口语义。
    ///
    /// 手算（同用例 A 布局，横向偏移 = padding + left_pad = 12、pitch = cell_w + col_gap = 76；
    /// 卡 0 = [12, 52]、卡 1 = [88, 128]）：
    /// - 行 0 顶 = 8、行 1 顶 = 72.569 → `row_index_at_y(49) = 0`、`row_index_at_y(72.569) = 1`；
    /// - `col_slot_at_x(28) = round((28−12)/76) = round(0.2105) = 0`（卡 0 内左半）；
    ///   `col_slot_at_x(76) = round(0.8421) = 1`（列间缝隙 [52,88] 内，归最近槽）；
    ///   `col_slot_at_x(52) = round(40/76) = round(0.5263) = 1`（卡 0 右缘 = 缝隙起点，>0.5 阈值 → 槽 1）；
    /// - 顶部 padding（y=2 < 行 0 顶 8）→ 原语退化为行 0（上层调用方按其 padding 语义处理）；
    /// - 左侧 padding（x=4 < padding+left_pad=12）→ 负槽归 0（上层调用方按其 padding 语义处理）；
    /// - 末行底外（y 远超 total）→ 原语仍返回末行下标（末行）；
    /// - 右 padding（x=130 > 末列右缘 128、视口 140 内）→ 最近槽 2（gap/padding 都归最近槽）。
    #[test]
    fn grid_row_col_primitives_edge_cases() {
        let g = grid_3pages();
        assert_eq!(g.row_index_at_y(49.0), 0, "行 0 中部 → 行 0");
        // 贴行 1 顶：用 `row_top[1]` 真实缓存值（浮点 `72.569` 字面量 < 实际 72.568542…，
        // 会落在行 0 —— K177：期望值须从真实几何取值，不拍脑袋）。
        assert_eq!(g.row_index_at_y(g.row_top[1]), 1, "贴行 1 顶 → 行 1");
        assert_eq!(
            g.row_index_at_y(2.0),
            0,
            "顶部 padding（行 0 顶之上）退化 → 行 0"
        );
        assert_eq!(g.row_index_at_y(9999.0), 1, "末行底外 → 末行");
        assert_eq!(
            g.col_slot_at_x(28.0),
            0,
            "卡 0 内左半（卡 0 = [12,52]）→ 槽 0"
        );
        assert_eq!(
            g.col_slot_at_x(76.0),
            1,
            "列间缝隙 [52,88] 内 → 最近槽 1（gap 归最近槽）"
        );
        assert_eq!(
            g.col_slot_at_x(52.0),
            1,
            "卡 0 右缘（缝隙起点，round 0.5263 → 1）"
        );
        assert_eq!(
            g.col_slot_at_x(4.0),
            0,
            "左侧 padding（< padding+left_pad=12）→ 负槽归 0"
        );
        assert_eq!(
            g.col_slot_at_x(130.0),
            2,
            "右 padding（x=130 > 末列右缘 128）→ 最近槽 2"
        );
        // 退化视口：极窄 viewport 把 cell_w 夹到 0 → pitch ≤ 0 → 列槽 0（无法按列定位）。
        let degenerate = grid_layout(&[(1.0, GRID_SQRT2); 1], 40.0, 0.0, 40.0, 20.0);
        assert_eq!(degenerate.cell_w, 0.0, "极窄视口 cell_w 夹到 0");
        assert_eq!(degenerate.col_slot_at_x(3.0), 0, "退化视口列槽 → 0");
        // 0 行布局：两个原语安全返回 0（不 panic；新 pitch=48 下 x=20 落在首槽）。
        let empty = grid_layout(&[], 40.0, 8.0, 8.0, 140.0);
        assert_eq!(empty.row_index_at_y(49.0), 0, "空布局行原语安全返回 0");
        assert_eq!(empty.col_slot_at_x(20.0), 0, "空布局列槽返回 0");
    }

    /// 用例 E 边界：0 页 / view 宽 < 卡宽（cols 必须 = 1）/ 1 页 / 平页（宽>高）与竖页（高>宽）
    /// 混排时行高 = 该行最高卡。
    #[test]
    fn grid_layout_edge_cases() {
        // 0 页：rows=0、total_height=0；row_top 空、row_page_start 仅留端点 [0]（len=rows+1）；
        // 几何方法一律 None。
        let empty = grid_layout(&[], 40.0, 8.0, 8.0, 140.0);
        assert_eq!(empty.rows, 0);
        assert!((empty.total_height - 0.0).abs() < 0.01);
        assert!(empty.row_top.is_empty());
        assert_eq!(
            empty.row_page_start,
            vec![0],
            "空布局预留端点 [0]（len = rows+1）"
        );
        assert_eq!(empty.cell_rect(0), None);
        assert_eq!(empty.first_visible_grid_row(0.0, 500.0), None);

        // view 宽 < 卡宽：卡宽 100、gap 8、padding 8、viewport 60 →
        // cols = max(1, floor((60−16+8)/108)) = max(1, floor(52/108)) = 1；
        // cols 夹到 1 且卡宽 100 > 内容区 44 → cell_w 夹到内容区 44（唯一夹取例外）；
        // 竖页卡高 = 44×√2 ≈ 62.2。
        let narrow = grid_layout(&[(1.0, GRID_SQRT2)], 100.0, 8.0, 8.0, 60.0);
        assert_eq!(narrow.cols, 1, "view 宽 < 卡宽时 cols 必须 = 1");
        assert!(
            (narrow.cell_w - 44.0).abs() < 0.01,
            "cell_w 夹到内容区 44（单卡填视口）"
        );
        assert_eq!(narrow.rows, 1);
        let (x, y, w, h) = narrow.cell_rect(0).expect("单卡应有矩形");
        // 卡 0：x = 8 + 0×52 = 8，y = padding = 8，w = 44，h = 44×√2 ≈ 62.225
        assert!((x - 8.0).abs() < 0.01 && (y - 8.0).abs() < 0.01);
        assert!(
            (w - 44.0).abs() < 0.01 && (h - 62.225).abs() < 0.01,
            "h 手算 ≈62.225: {h}"
        );

        // 1 页：正常视口下 cols=2、rows=1、row_page_start=[0,1]、total = 8 + 56.569 + 8。
        let one = grid_layout(&[(1.0, GRID_SQRT2)], 40.0, 8.0, 8.0, 140.0);
        assert_eq!(one.cols, 2);
        assert_eq!(one.rows, 1);
        assert_eq!(one.row_page_start, vec![0, 1]);
        assert!(
            (one.total_height - (8.0 + 56.569 + 8.0)).abs() < 0.01,
            "单行总高 ≈72.569"
        );

        // 平页（宽>高）与竖页（高>宽）混排：pages = [(2,1),(1,3),(1,2)]、卡宽 60、gap 8、
        // padding 8、viewport 160 → cols = floor((160−16+8)/68) = floor(2.235) = 2；
        // cell_w = 请求卡宽 60（新语义，不反推填充）；cell_h = [60×½, 60×3, 60×2] = [30, 180, 120]；
        // 行 0 含页 0/1 → 行高 = max(30, 180) = 180；行 1 含页 2 → 行高 = 120；
        // row_top = [8, 8+180+8 = 196]；total = 8+180+120+8+8 = 324。
        let mixed = grid_layout(&[(2.0, 1.0), (1.0, 3.0), (1.0, 2.0)], 60.0, 8.0, 8.0, 160.0);
        assert_eq!(mixed.cols, 2, "混排 cols 手算应 2");
        assert_eq!(mixed.rows, 2);
        let (_, _, _, h0) = mixed.cell_rect(0).expect("平页卡应有矩形");
        assert!((h0 - 30.0).abs() < 0.01, "平页 0 卡高应保持纵横比 30: {h0}");
        // 卡 1：横向 needed = (144−2×60)/1 = 24 ≤ 36 → col_gap=24、left_pad=0，x = 8 + 1×(60+24) = 92；
        //   h = 60×3 = 180（行高被竖页顶起）
        let (x1, y1, w1, h1) = mixed.cell_rect(1).expect("竖页卡应有矩形");
        assert!(
            (x1 - 92.0).abs() < 0.01 && (y1 - 8.0).abs() < 0.01,
            "竖页卡 1 左上 = (92, 8)"
        );
        assert!((w1 - 60.0).abs() < 0.01);
        assert!(
            (h1 - 180.0).abs() < 0.01,
            "竖页卡高 180（行高 = 该行最高卡）"
        );
        // 卡 2：x = 8（第 2 行第 0 列），y = 8+180+8 = 196，h = 120
        let (x2, y2, _, h2) = mixed.cell_rect(2).expect("页 2 卡应有矩形");
        assert!((x2 - 8.0).abs() < 0.01);
        assert!((y2 - 196.0).abs() < 0.01, "行 1 顶 = 196: {y2}");
        assert!((h2 - 120.0).abs() < 0.01, "页 2 卡高 120");
        assert!((mixed.row_top[1] - 196.0).abs() < 0.01, "行 1 顶 = 196");
        assert!(
            (mixed.total_height - 324.0).abs() < 0.01,
            "混排总高 = 324: {}",
            mixed.total_height
        );
    }

    /// 用例 G（横向拉伸填满三态）：列间距由固定 `gap` 改为「余量摊进列间距 / 封顶居中 /
    /// 单列居中」——末列右缘贴齐内容区右缘，消除右侧空白；列数少时封顶对称居中。
    ///
    /// **T-A 恰摊满**（needed ≤ GAP_MAX，无 left_pad）：vw=1000、padding=16、card_w=180、gap=12
    /// → C = 1000−32 = 968、cols = floor((968+12)/192) = floor(980/192) = 5、cell_w = 180、
    /// needed = (968−5·180)/4 = 68/4 = 17 ≤ 36 → col_gap = 17、left_pad = 0。
    /// 断言：cell_rect(4).right == 16 + C == 984（== viewport − padding，填满到内容区右缘）；
    ///   col_slot_at_x(卡 4 中心 894) == 4；cell_rect(0).left == 16。
    #[test]
    fn grid_fill_gap_ta_exact_fill() {
        let g = grid_layout(&[(1.0, 1.0); 8], 180.0, 12.0, 16.0, 1000.0);
        assert_eq!(g.cols, 5, "T-A cols 手算应 5");
        assert!(
            (g.col_gap - 17.0).abs() < 0.01,
            "T-A col_gap 应=needed 17: {}",
            g.col_gap
        );
        assert!(
            (g.left_pad - 0.0).abs() < 0.01,
            "T-A 恰填满 → left_pad = 0: {}",
            g.left_pad
        );
        let (x4, _, w4, _) = g.cell_rect(4).expect("卡 4 应有矩形");
        assert!(
            (x4 - 804.0).abs() < 0.01,
            "T-A 卡 4 左缘 = 16 + 4×197 = 804: {x4}"
        );
        assert!(
            (x4 + w4 - 984.0).abs() < 0.01,
            "T-A 卡 4 右缘应贴齐内容区右缘 16+C=984：{}",
            x4 + w4
        );
        let (x0, _, _, _) = g.cell_rect(0).expect("卡 0 应有矩形");
        assert!((x0 - 16.0).abs() < 0.01, "T-A 卡 0 左缘 = padding 16: {x0}");
        // 卡 4 中心 = (804+984)/2 = 894 → round((894−16)/197) = round(4.4568) = 4
        assert_eq!(g.col_slot_at_x(894.0), 4, "T-A 卡 4 中心 → 槽 4");
        // 一致性封口（防 K153 两路径分叉）：每行每个卡的 cell_rect 中心都被 col_slot_at_x
        // 映回自身列槽（col_slot 与 cell_rect 用同一 pitch/偏移，不得各算一套）。
        for i in 0..8 {
            let (cx, _, w, _) = g.cell_rect(i).expect("T-A 卡应有矩形");
            assert_eq!(
                g.col_slot_at_x(cx + w / 2.0),
                i % g.cols,
                "T-A 卡 {i} 中心应映回槽 {}(=i%cols=5)",
                i % g.cols
            );
        }
    }

    /// **T-B 封顶居中**（needed > GAP_MAX）：vw=1000、padding=16、card_w=280、gap=12 →
    /// C = 968、cols = floor((968+12)/292) = floor(980/292) = 3、cell_w = 280、
    /// needed = (968−3·280)/2 = 128/2 = 64 > 36 → col_gap 封顶 36、
    /// left_pad = (968−840−2·36)/2 = 56/2 = 28。
    /// 断言：cell_rect(0).left == 16+28 == 44；cell_rect(2).right == 44+2·316+280 == 956；
    ///   左空白 44（卡 0 左缘距视口左）== 右空白 44（1000−956）→ 对称居中。
    #[test]
    fn grid_fill_gap_tb_capped_centered() {
        let g = grid_layout(&[(1.0, GRID_SQRT2); 3], 280.0, 12.0, 16.0, 1000.0);
        assert_eq!(g.cols, 3, "T-B cols 手算应 3");
        assert!(
            (g.col_gap - 36.0).abs() < 0.01,
            "T-B col_gap 应封顶 36: {}",
            g.col_gap
        );
        assert!(
            (g.left_pad - 28.0).abs() < 0.01,
            "T-B left_pad 应 28: {}",
            g.left_pad
        );
        let (x0, _, _, _) = g.cell_rect(0).expect("卡 0 应有矩形");
        assert!(
            (x0 - 44.0).abs() < 0.01,
            "T-B 卡 0 左缘 = padding+left_pad = 44: {x0}"
        );
        let (x2, _, w2, _) = g.cell_rect(2).expect("卡 2 应有矩形");
        assert!(
            (x2 + w2 - 956.0).abs() < 0.01,
            "T-B 卡 2 右缘 = 44 + 2×316 + 280 = 956：{}",
            x2 + w2
        );
        // 对称：左空白（卡 0 左缘距视口左 0）= 44，右空白（1000 − 卡 2 右缘 956）= 44 → 相等。
        let right_blank = 1000.0 - (x2 + w2);
        assert!(
            (x0 - right_blank).abs() < 0.01,
            "T-B 封顶后留白应左右对称居中：左 {x0} / 右 {right_blank}"
        );
    }

    /// **T-C 单列居中**（cols == 1）：vw=300、padding=16、card_w=200、gap=12 →
    /// C = 268、cols = floor((268+12)/212) = floor(280/212) = 1、cell_w = min(200, 268) = 200、
    /// col_gap = gap = 12、left_pad = (268−200)/2 = 34。
    /// 断言：cell_rect(0).left == 16+34 == 50；col_slot_at_x(卡中心 150) == 0。
    #[test]
    fn grid_fill_gap_tc_single_col_centered() {
        let g = grid_layout(&[(1.0, GRID_SQRT2)], 200.0, 12.0, 16.0, 300.0);
        assert_eq!(g.cols, 1, "T-C cols 手算应 1");
        assert!(
            (g.cell_w - 200.0).abs() < 0.01,
            "T-C cell_w 应 = 卡宽 200（不超内容区 268）"
        );
        assert!(
            (g.col_gap - 12.0).abs() < 0.01,
            "T-C col_gap 应 = 入参 gap 12: {}",
            g.col_gap
        );
        assert!(
            (g.left_pad - 34.0).abs() < 0.01,
            "T-C left_pad 应 34（单列居中）: {}",
            g.left_pad
        );
        let (x0, _, w, _) = g.cell_rect(0).expect("单卡应有矩形");
        assert!(
            (x0 - 50.0).abs() < 0.01,
            "T-C 卡 0 左缘 = padding+left_pad = 50: {x0}"
        );
        // 卡中心 = 50 + 200/2 = 150 → round((150−16−34)/212) = round(100/212) = round(0.4717) = 0
        assert_eq!(g.col_slot_at_x(x0 + w / 2.0), 0, "T-C 单卡中心 → 槽 0");
    }

    /// 用例 F（新语义核心，Ctrl+滚轮缩放跳变根治）：**cell_w 跟随请求卡宽**——同 viewport
    /// 下请求卡宽连续变化时，卡大小每步都变（不再被「填满反算」冻结在列数不变区间）。
    /// 逐档断言：
    /// - 非夹取区（cols>1，或 cols=1 且卡宽 ≤ 内容区）`cell_w == card_w`；
    /// - 夹取区（cols=1 且卡宽 > 内容区 `vw − 2p`）`cell_w == 内容区宽`（单卡填视口）；
    /// - `cols` 随卡宽单调不增（更大卡 → 更少列），且只在阈值处变化（每次至多 -1）；
    /// - `cell_w` 相邻两步差值恒 = 卡宽步长（缩放连续可感知；夹取区内按设计钉在内容区宽）。
    #[test]
    fn grid_layout_cell_w_follows_card_width_continuously() {
        let pages = vec![(1.0, GRID_SQRT2); 3];
        let (gap, padding, viewport) = (8.0f32, 8.0f32, 140.0f32);
        let content_w = viewport - 2.0 * padding; // 124
        let step = 4.0f32;
        let mut prev_cell_w: Option<f32> = None;
        let mut prev_cols: Option<usize> = None;
        let mut card = 40.0f32;
        while card <= 200.0 {
            let g = grid_layout(&pages, card, gap, padding, viewport);
            if g.cols == 1 {
                // cols=1（夹取或恰一列）：cell_w = 请求卡宽 ∩ 内容区（防横向溢出）
                assert!(
                    (g.cell_w - card.min(content_w)).abs() < 1e-3,
                    "cols=1 时 cell_w 应 = min(卡宽, 内容区)={}: 卡宽 {card} → {}",
                    card.min(content_w),
                    g.cell_w
                );
            } else {
                assert!(
                    (g.cell_w - card).abs() < 1e-3,
                    "cols>1 时 cell_w 应跟随请求卡宽 {card}: {}",
                    g.cell_w
                );
            }
            // cols 单调不增、不跳档（更大卡 → 列数只减不增，且在阈值处才变）。
            if let Some(pc) = prev_cols {
                assert!(
                    g.cols <= pc,
                    "cols 随卡宽增大不应增加: 卡 {card}(cols {}) 之前 {pc}",
                    g.cols
                );
                assert!(
                    g.cols + 1 >= pc,
                    "cols 应只在阈值处变化（每次至多 -1）：{pc} → {}",
                    g.cols
                );
            }
            // cell_w 连续：夹取区外相邻步差恒 = 步长（缩放每步都变大小）。
            if let (Some(pw), true) = (prev_cell_w, card <= content_w) {
                let diff = g.cell_w - pw;
                assert!(
                    (diff - step).abs() < 1e-2,
                    "cell_w 应每步精确变 step={step}（缩放连续可感知）：卡 {card} 前 {pw} → {} (差 {diff})",
                    g.cell_w,
                );
            }
            prev_cell_w = Some(g.cell_w);
            prev_cols = Some(g.cols);
            card += step;
        }
    }

    // ---- 网格缩放列数钳制（K167 本线）：grid_zoom_limits_for_cols ----

    /// 列数钳在多个真实视口宽（800/1000/1500）下恰落 [2, 9]。期望值由「渲染同口径」
    /// `grid_layout` 公式推出（K177：先注释演算再落值）：`cols(zoom) = max(1, floor((C+g)/
    /// (base_card·zoom + g)))`，`C = vw − 2·padding`。手算：z_max = ((C−g)/2 − 0.5)/280
    /// （让 −0.5px 对称余量，见实现注释；该处 cols 恰 2）、z_min = ((C−9·g)/10 + 0.5)/280
    /// （该处 cols 恰 9）。
    #[test]
    fn grid_zoom_limits_clamp_columns_to_two_and_nine() {
        const BASE: f32 = 280.0; // GRID_CARD_WIDTH
        const GAP: f32 = 12.0; // GRID_GAP
        const PAD: f32 = 16.0; // GRID_PADDING
        // 渲染同口径 cols 解码器：与 grid_layout 同一公式（空 pages 只取 cols，不依赖页数）。
        let cols_at = |zoom: f32, vw: f32| grid_layout(&[], BASE * zoom, GAP, PAD, vw).cols;
        for vw in [800.0f32, 1000.0, 1500.0] {
            let c = vw - 2.0 * PAD;
            let expect_z_max = (((c - GAP) / 2.0) - 0.5) / BASE;
            let expect_z_min = ((c - 9.0 * GAP) / 10.0 + 0.5) / BASE;
            let (z_min, z_max) = grid_zoom_limits_for_cols(BASE, GAP, PAD, vw, 2.0, 9.0);
            assert!(
                (z_min - expect_z_min).abs() < 1e-4,
                "vw={vw} z_min 应 = {expect_z_min}，实际 {z_min}"
            );
            assert!(
                (z_max - expect_z_max).abs() < 1e-4,
                "vw={vw} z_max 应 = {expect_z_max}，实际 {z_max}"
            );
            assert!(z_min < z_max, "vw={vw} 区间应有效：[{z_min}, {z_max}]");
            // 边界准确：钳界两端恰给 2 / 9 列（放大不可能到 1、缩小不可能到 10）。
            assert_eq!(cols_at(z_max, vw), 2, "vw={vw} z_max 处应恰 2 列");
            assert_eq!(cols_at(z_min, vw), 9, "vw={vw} z_min 处应恰 9 列");
            // 越界 ε=0.05（可复现小量）即破界：z_max+ε → 1 列、z_min−ε → ≥10 列，
            // 证明钳界是「列数变化」的真实边界而非安全冗余。
            assert_eq!(cols_at(z_max + 0.05, vw), 1, "vw={vw} z_max+ε 应落 1 列");
            assert!(cols_at(z_min - 0.05, vw) >= 10, "vw={vw} z_min−ε 应 ≥10 列");
            // 区间内抽样若干 zoom 恒在 [2, 9]（夹逼，防中间凹陷/越界）。
            let mid = (z_min + z_max) / 2.0;
            for z in [z_min, (z_min + mid) / 2.0, mid, (mid + z_max) / 2.0, z_max] {
                let cols = cols_at(z, vw);
                assert!(
                    (2..=9).contains(&cols),
                    "vw={vw} zoom={z} 列数应 ∈[2,9]，实际 {cols}"
                );
            }
        }
    }

    /// K173 脏宽度剪刀边界（评审 P1 实证）：`vw=402` 时旧 z_max 的 `base_card × z_max` 被
    /// f32 舍入抬过边界（`(179/280)×280 = 179.00002` → cols=1，违背「恒 2 列」）。修复后
    /// z_max 让 −0.5px 对称余量 → 恒 2 列且 ε 越界仍落 1 列。
    /// 手算（K177）：C = 402 − 32 = 370、g=12 ⇒ 边界 = (370−12)/2 = 179、
    /// z_max = (179 − 0.5)/280 = 178.5/280 = 0.6375 ⇒ card = 178.5、
    /// cols = floor((370+12)/(178.5+12)) = floor(382/190.5) = 2；z_max+0.05 → card = 192.5、
    /// cols = floor(382/204.5) = 1。
    #[test]
    fn grid_zoom_limits_dirty_width_scissors_stays_two_cols() {
        const BASE: f32 = 280.0;
        const GAP: f32 = 12.0;
        const PAD: f32 = 16.0;
        let vw = 402.0f32;
        let c = vw - 2.0 * PAD;
        let cols_at = |zoom: f32| grid_layout(&[], BASE * zoom, GAP, PAD, vw).cols;
        let (z_min, z_max) = grid_zoom_limits_for_cols(BASE, GAP, PAD, vw, 2.0, 9.0);
        let expect_z_max = (((c - GAP) / 2.0) - 0.5) / BASE;
        assert!(
            (z_max - expect_z_max).abs() < 1e-5,
            "vw=402 z_max 应 = {expect_z_max}，实际 {z_max}"
        );
        assert!(z_min < z_max, "vw=402 区间应有效：[{z_min}, {z_max}]");
        // 直接断言不变式（防 f32 抬过）：z_max 处 card ≤ 边界 179。
        assert!(
            z_max * BASE <= (c - GAP) / 2.0,
            "vw=402 z_max 处 card 应 ≤ 边界 (C−g)/2=179"
        );
        assert_eq!(
            cols_at(z_max),
            2,
            "vw=402 z_max 处应恰 2 列（旧实现此处 1 列，评审 P1）"
        );
        assert_eq!(
            cols_at(z_max + 0.05),
            1,
            "vw=402 z_max+ε 应落 1 列（剪刀边界仍准）"
        );
    }

    /// 退化兜底（K167 契约）：零宽负视口（内容区 < 0）不 panic、两端夹到极小正数且不倒挂；
    /// 非法参数倒挂（least > most，`z_min > z_max` 正值）各取均值锁同一点。
    #[test]
    fn grid_zoom_limits_degenerate_viewports_are_bounded() {
        for vw in [0.0f32, -100.0] {
            let (z_min, z_max) = grid_zoom_limits_for_cols(280.0, 12.0, 16.0, vw, 2.0, 9.0);
            assert!(z_min.is_finite() && z_max.is_finite(), "vw={vw} 应有限");
            assert!(
                z_min > 0.0 && z_max > 0.0,
                "vw={vw} 应夹到极小正数：[{z_min}, {z_max}]"
            );
            assert!(z_max >= z_min, "vw={vw} 不应倒挂：[{z_min}, {z_max}]");
        }
        let (a, b) = grid_zoom_limits_for_cols(280.0, 12.0, 16.0, 800.0, 9.0, 2.0);
        assert!((a - b).abs() < 1e-6, "参数倒挂应各取均值锁同点：[{a}, {b}]");
    }

    // ---- 分页阅读布局（PagedLayout）测试 ----

    /// 测试用 A4 竖版页（793,1123 pt），视口 1000×700、h_gap=PAGED_GAP_X=1、v_gap=PAGED_GAP_Y=6、
    /// padding 20。
    /// 手算前提（K177）：ar = w/h = 793/1123 ≈ 0.70614、h/w = 1123/793 ≈ 1.41614。
    fn paged_a4_3() -> PagedLayout {
        paged_layout(
            &[(793.0, 1123.0); 3],
            1000.0,
            700.0,
            2,
            PAGED_GAP_X,
            PAGED_GAP_Y,
            20.0,
        )
    }

    /// 双页模式（cols=2，3 页 A4）：行 0 满两页（宽受限 cap），行 1 单页靠列左（宽度同满行）。
    ///
    /// 手算（K177）：content_w=960、cap_w=(960−1)/2=479.5、fit_w=700×0.70614≈494.30 →
    /// cell_w = 479.5；cell_h = 479.5×(1123/793) ≈ 679.04；spread = 2×479.5+1 = 960 =
    /// content_w → left_pad = 0；页 0 = (20, 20, 479.5, 679.04)、页 1 = (20+479.5+1 = 500.5,
    /// 20, 479.5, 679.04)；row_top = [20, 20+679.04+6 = 705.04]；页 2（行 1 第 0 列，靠列左非
    /// 居中）：(20, 705.04, 479.5, 679.04)；total = 705.04 + 679.04 + 20 = 1404.08。
    #[test]
    fn paged_double_col_a4_hand_computed() {
        let l = paged_a4_3();
        assert_eq!(l.cols, 2, "双页模式 cols=2");
        assert_eq!(l.rows, 2);
        assert_eq!(l.row_page_start, vec![0, 2, 3], "行起始页表 = [0,2,3]");
        assert!(
            (l.cell_w_per_row[0] - 479.5).abs() < 0.05,
            "页宽应受列宽限制 ≈479.5: {}",
            l.cell_w_per_row[0]
        );
        assert!(
            (l.cell_h[0] - 679.04).abs() < 0.05,
            "页高 ≈679.04: {}",
            l.cell_h[0]
        );
        // 行 1 单页宽度与满行一致（统一 cols），left_pad 同 0 → 页 2 靠列左顶格。
        assert!(
            (l.cell_w_per_row[1] - 479.5).abs() < 0.05,
            "行 1 单页宽度同满行 479.5"
        );
        assert!(
            (l.left_pad_per_row[0] - 0.0).abs() < 1e-3
                && (l.left_pad_per_row[1] - 0.0).abs() < 1e-3
        );
        let p0 = l.cell_rect(0).expect("页 0");
        let p1 = l.cell_rect(1).expect("页 1");
        let p2 = l.cell_rect(2).expect("页 2");
        assert!(
            (p0.x - 20.0).abs() < 1e-3 && (p0.y - 20.0).abs() < 1e-3,
            "页 0 左上 = (20,20)"
        );
        assert!((p0.w - 479.5).abs() < 0.05 && (p0.h - 679.04).abs() < 0.05);
        assert!(
            (p1.x - 500.5).abs() < 0.05 && (p1.y - 20.0).abs() < 1e-3,
            "页 1 左上 ≈(500.5,20)"
        );
        assert!((p1.w - 479.5).abs() < 0.05);
        assert!(
            (p2.x - 20.0).abs() < 1e-3,
            "行 1 单页靠列左：x = padding = 20（非居中）: {}",
            p2.x
        );
        assert!((p2.y - 705.04).abs() < 0.05, "行 1 顶 ≈705.04: {}", p2.y);
        assert!((p2.w - 479.5).abs() < 0.05 && (p2.h - 679.04).abs() < 0.05);
        assert!((l.row_top[1] - 705.04).abs() < 0.05);
        assert!(
            (l.total_height - 1404.08).abs() < 0.05,
            "总高 ≈1404.08: {}",
            l.total_height
        );
        assert!(
            (l.spread_max - 960.0).abs() < 0.05,
            "spread_max = 满行宽 960: {}",
            l.spread_max
        );
        // 越界页 → None。
        assert!(l.cell_rect(3).is_none(), "页 3 越界应 None");
        // 可见行：视口 [0, 700)（scroll_y=0）→ 行 0 [20, 699.04]；行 1 顶 705.04 > 700 不见。
        assert_eq!(
            l.first_visible_row(0.0, 700.0),
            Some((0, 0)),
            "scroll 0 只见行 0"
        );
        // 视口顶 704（scroll_y=−704）→ 视口 [704, 1404] 压住行 1 [705.04, 1384.08] → (1,1)。
        assert_eq!(
            l.first_visible_row(-704.0, 700.0),
            Some((1, 1)),
            "视口顶 704 应命中行 1"
        );
        // 悬行间 gap：行 0 底 699.04、行 1 顶 705.04；视口 [700, 702] 无行 → None。
        assert_eq!(l.first_visible_row(-700.0, 2.0), None, "悬行间 gap 应 None");
        // 完全在总高之下：视口顶 total+10 → 无行底 > 视口顶 → None。
        assert_eq!(
            l.first_visible_row(-(l.total_height + 10.0), 100.0),
            None,
            "内容之下应 None"
        );
    }

    /// 单列（cols=1，内部/测试用）手算几何：A4(793,1123)、视口 1000×700、pad 20 →
    /// fit 主导、单列居中、行数=页数、row_top 按 v_gap 累加。`paged_schedule_window_deep_page`
    /// 只断窗口不断几何，本条补 cols=1 的纯函数几何面。
    ///
    /// 手算（K177，h_gap=PAGED_GAP_X=1、v_gap=PAGED_GAP_Y=6）：
    /// - content_w = 960、cap_w = (960−0)/1 = 960（cols=1 不减 h_gap，见 `paged_layout` 公式；
    ///   fit 主导，cap_w 是否含 h_gap 不影响 min）、fit_w = 700×0.70614 ≈ 494.30 →
    ///   cell_w = min(960, 494.30) = 494.30；
    /// - cell_h = 494.30×(1123/793) ≈ 700.00；spread = 1×494.30 + 0×h_gap = 494.30；
    /// - left_pad = (960−494.30)/2 ≈ 232.85 → 页 0 x = 20 + 232.85 = 252.85（单列居中）；
    /// - 1 页 → rows=1、row_top=[20]、total = 20 + 700 + 20 = 740；
    /// - 2 页 → rows=2、row_top=[20, 20+700+6=726]、total = 20+700+6+700+20 = 1446（v_gap 行距）。
    #[test]
    fn paged_single_col_a4_hand_computed() {
        let l = paged_layout(
            &[(793.0, 1123.0)],
            1000.0,
            700.0,
            1,
            PAGED_GAP_X,
            PAGED_GAP_Y,
            20.0,
        );
        assert_eq!(l.cols, 1, "cols=1");
        assert_eq!(l.rows, 1, "1 页单列 → 1 行");
        assert!(
            (l.cell_w_per_row[0] - 494.30).abs() < 0.05,
            "页宽 fit 主导 ≈494.30: {}",
            l.cell_w_per_row[0]
        );
        assert!(
            (l.cell_h[0] - 700.0).abs() < 0.05,
            "页高 ≈700.00: {}",
            l.cell_h[0]
        );
        assert!(
            (l.left_pad_per_row[0] - 232.85).abs() < 0.05,
            "单列居中 left_pad ≈232.85: {}",
            l.left_pad_per_row[0]
        );
        assert!(
            (l.spread_max - 494.30).abs() < 0.05,
            "spread_max = 单列宽 494.30: {}",
            l.spread_max
        );
        let p0 = l.cell_rect(0).expect("页 0");
        assert!(
            (p0.x - 252.85).abs() < 0.05 && (p0.y - 20.0).abs() < 1e-3,
            "页 0 左上 = (252.85, 20)"
        );
        assert!((p0.w - 494.30).abs() < 0.05 && (p0.h - 700.0).abs() < 0.05);
        assert!((l.row_top[0] - 20.0).abs() < 1e-3);
        assert!(
            (l.total_height - 740.0).abs() < 0.05,
            "单页总高 = 20 + 700 + 20 = 740: {}",
            l.total_height
        );
        assert!(l.cell_rect(1).is_none(), "页 1 越界应 None");

        // 2 页单列：rows=2、row_top 按 v_gap 累加、total 含行距。
        let l2 = paged_layout(
            &[(793.0, 1123.0); 2],
            1000.0,
            700.0,
            1,
            PAGED_GAP_X,
            PAGED_GAP_Y,
            20.0,
        );
        assert_eq!(l2.rows, 2);
        assert!(
            (l2.row_top[1] - 726.0).abs() < 0.05,
            "行 1 顶 = 20 + 700 + 6 = 726: {}",
            l2.row_top[1]
        );
        assert!(
            (l2.total_height - 1446.0).abs() < 0.05,
            "两页总高 = 20+700+6+700+20 = 1446: {}",
            l2.total_height
        );
    }

    /// 命中定位（双页）：页内点命中、列间 gap/留白/越界/空 → None。柱槽用 floor（非 round，
    /// 见 [`PagedLayout::page_at`] 注释——round 的槽中点会切进卡片内部造成漏命中）。
    ///
    /// 手算（页 0 = (20,20,479.5,679.04)、页 1 = (500.5,20,479.5,679.04)、pitch = 480.5）：
    /// - (405, 380.1)：r = row_index_at_y(380.1) = 0；c = floor((405−20)/480.5) = floor(0.801) = 0；
    ///   页 0 内含点 → (0, 405−20 = 385, 380.1−20 = 360.1)；
    /// - (520, 100)：c = floor(500/480.5) = 1 → 页 1 内 → (1, 520−500.5 = 19.5, 100−20 = 80)；
    /// - (500, 100)：c = floor(480/480.5) = 0，页 0 右缘 499.5 之外（列间 gap [499.5,500.5]）→ None；
    /// - (10, 100)：x < padding+left_pad = 20 → c = floor(−0.02) = −1 → None；
    /// - (405, 700)：y 在行 0 底 699.04 外、行 1 顶 705.04 前（行间 gap）→ None；
    /// - (300, 5)：y < 行 0 顶 20（顶部 padding）→ 页 0 含点复核不过 → None。
    #[test]
    fn paged_page_at_hits_cells_rejects_gaps() {
        let l = paged_a4_3();
        let (i, lx, ly) = l.page_at(405.0, 380.1).expect("页内应命中");
        assert_eq!(i, 0);
        assert!(
            (lx - 385.0).abs() < 1e-2 && (ly - 360.1).abs() < 1e-2,
            "页内局部 = (385, 360.1): ({lx},{ly})"
        );
        let (i, lx, ly) = l.page_at(520.0, 100.0).expect("页 1 应命中");
        assert_eq!(i, 1);
        assert!(
            (lx - 19.5).abs() < 1e-2 && (ly - 80.0).abs() < 1e-2,
            "页 1 局部 = (19.5, 80): ({lx},{ly})"
        );
        assert!(l.page_at(500.0, 100.0).is_none(), "列间 gap 应 None");
        assert!(l.page_at(10.0, 100.0).is_none(), "左留白应 None");
        assert!(l.page_at(405.0, 700.0).is_none(), "行间 gap 应 None");
        assert!(l.page_at(300.0, 5.0).is_none(), "顶部 padding 应 None");
        assert!(l.page_at(1000.0, 100.0).is_none(), "右留白应 None");
        // 末行右侧虚拟列（行 1 只有页 2，第 1 列不存在）→ None。
        assert!(l.page_at(700.0, 800.0).is_none(), "末行右虚拟列应 None");
    }

    /// 边界与退化：空页全安全、cols 归一（0→1、5→2）、窄视口防负宽、极窄视口 0 宽不 panic。
    #[test]
    fn paged_layout_edge_cases_are_bounded() {
        // 空页：rows/total=0、row_top 空、row_page_start 仅端点 [0]、几何方法一律安全空。
        let empty = paged_layout(&[], 1000.0, 700.0, 2, PAGED_GAP_X, PAGED_GAP_Y, 20.0);
        assert_eq!(empty.cols, 2, "cols 保留归一值");
        assert_eq!(empty.rows, 0);
        assert!(empty.row_top.is_empty(), "无行");
        assert_eq!(empty.row_page_start, vec![0], "空布局预留端点 [0]");
        assert!((empty.total_height - 0.0).abs() < 1e-3);
        assert_eq!(empty.cell_rect(0), None);
        assert_eq!(empty.first_visible_row(0.0, 700.0), None);
        assert_eq!(empty.row_index_at_y(5.0), None);
        assert_eq!(empty.page_at(10.0, 10.0), None);
        assert_eq!(empty.snap_target(0.0), (0, 0.0), "空布局吸附回落 (0, 0)");

        // cols 归一：0 → 1、5 → 2（>2 按 2）。
        assert_eq!(
            paged_layout(&[(793.0, 1123.0)], 1000.0, 700.0, 0, PAGED_GAP_X, PAGED_GAP_Y, 20.0)
                .cols,
            1
        );
        assert_eq!(
            paged_layout(&[(793.0, 1123.0)], 1000.0, 700.0, 5, PAGED_GAP_X, PAGED_GAP_Y, 20.0)
                .cols,
            2
        );
        assert_eq!(
            paged_layout(&[(793.0, 1123.0)], 1000.0, 700.0, 1, PAGED_GAP_X, PAGED_GAP_Y, 20.0)
                .cols,
            1
        );

        // 窄视口 vw=200（双页）：content_w = 160、cap_w = (160−1)/2 = 79.5 → cell_w = 79.5（正宽）。
        let narrow = paged_layout(
            &[(793.0, 1123.0); 3],
            200.0,
            700.0,
            2,
            PAGED_GAP_X,
            PAGED_GAP_Y,
            20.0,
        );
        assert!(
            (narrow.cell_w_per_row[0] - 79.5).abs() < 0.05,
            "窄视口页宽 ≈79.5: {}",
            narrow.cell_w_per_row[0]
        );
        let r0 = narrow.cell_rect(0).expect("页 0 应有矩形");
        assert!(r0.w > 0.0, "窄视口不应得负宽: w={}", r0.w);
        // 任意命中安全（不 panic）。
        let _ = narrow.page_at(100.0, 100.0);
        let _ = narrow.first_visible_row(0.0, 700.0);

        // 极窄视口堆叠 padding：content_w 夹 0、cap_w=(0−1)/2 夹 0 → 页宽/高退化为 0，不 panic、
        // 无负宽、列 pitch 仍 >0（=h_gap）故 page_at 走包含性复核稳返 None。
        let degenerate = paged_layout(
            &[(793.0, 1123.0); 2],
            30.0,
            700.0,
            2,
            PAGED_GAP_X,
            PAGED_GAP_Y,
            20.0,
        );
        assert_eq!(degenerate.rows, 1);
        assert!(
            (degenerate.cell_w_per_row[0] - 0.0).abs() < 1e-3,
            "退化视口页宽夹到 0"
        );
        assert_eq!(
            degenerate.cell_rect(0),
            Some(Rect {
                x: 20.0,
                y: 20.0,
                w: 0.0,
                h: 0.0
            })
        );
        assert!(
            degenerate.page_at(100.0, 100.0).is_none(),
            "退化视口命中安全 None"
        );
        assert_eq!(degenerate.snap_target(0.0), (0, degenerate.row_top[0]));
        // 退化总高（rows=1）：padding + 行高 0 + padding = 20 + 20 = 40（无行间 gap）。
        assert!(
            (degenerate.total_height - 40.0).abs() < 1e-3,
            "退化总高应 = 20 + 0 + 20 = 40: {}",
            degenerate.total_height
        );
    }

    /// R1 分页缩放锚点纯函数：放大 1.12× 后行内相对位置按 ratio 保持 + 往返零漂移。
    ///
    /// 演算（K177）：用 `paged_a4_3()`（cols=2, 3 页 A4, 视口 1000×700, h_gap=1, v_gap=6,
    /// padding 20）：
    /// - zoom=1 布局 `l`：row_top=[20, 705.04]、cell_h=679.04、total=1404.08；
    /// - pl_old = `paged_layout_scaled(l, 1.0)` = l；pl_new = `paged_layout_scaled(l, 1.12)`：
    ///   row_top=[22.4, 789.64]、row_h[0]=679.04×1.12=760.52；
    /// - 取行 0、sy_old（显示口径 = 布局口径 @ zoom=1）= row_top[0]+row_h[0]×0.35
    ///   = 20+237.664 = 257.664（行内比例 0.35）；ratio = 1.12；
    /// - 期望 sy_new = pl_new.row_top[0] + (sy_old − pl_old.row_top[0])×ratio
    ///   = 22.4 + 237.664×1.12 = 22.4+266.184 = 288.584；
    ///   行内比例验证：(288.584−22.4)/760.52 = 266.184/760.52 = 0.35 ✓；
    /// - 回程 ratio'=1/1.12、pl_old'=pl_new、pl_new'=pl_old：
    ///   sy_back = 20 + (288.584−22.4)/1.12 = 20+237.66 = 257.66 ≈ sy_old ✓。
    #[test]
    fn zoom_rescroll_anchor_paged_preserves_row_relative_position() {
        let l = paged_a4_3();
        let pl_old = paged_layout_scaled(&l, 1.0);
        let pl_new = paged_layout_scaled(&l, 1.12);
        let row = 0usize;
        let row_h_old = l.row_height(row); // 679.04
        let row_h_new = row_h_old * 1.12; // 760.52
        let sy_old = l.row_top[row] + row_h_old * 0.35; // 257.664（行内 35%）
        let ratio = 1.12f32;
        let sy_new = zoom_rescroll_anchor_paged(&pl_old, &pl_new, row, ratio, sy_old);
        // 期望值（演算）：22.4 + 237.664×1.12 = 288.584。
        let want = pl_new.row_top[row] + (sy_old - pl_old.row_top[row]) * ratio;
        assert!((sy_new - want).abs() < 1e-2, "sy_new={sy_new} want={want}");
        // 行内相对位置保持（0.35）。
        let frac_new = (sy_new - pl_new.row_top[row]) / row_h_new;
        assert!(
            (frac_new - 0.35).abs() < 1e-3,
            "行内比例应保持 0.35，实际 {frac_new}"
        );
        // 往返：回程 ratio'=1/1.12、pl_old/pl_new 互换 → 无漂移。
        let sy_back = zoom_rescroll_anchor_paged(&pl_new, &pl_old, row, 1.0 / 1.12, sy_new);
        assert!(
            (sy_back - sy_old).abs() < 0.5,
            "往返应无漂移：{sy_old} → {sy_new} → {sy_back}"
        );
        // 行 1 同样验证（深页，行顶更大、常量区不影响分页锚）。
        let sy1_old = l.row_top[1] + row_h_old * 0.5; // 705.04 + 339.52 = 1044.56
        let sy1_new = zoom_rescroll_anchor_paged(&pl_old, &pl_new, 1, ratio, sy1_old);
        let frac1_new = (sy1_new - pl_new.row_top[1]) / row_h_new;
        assert!(
            (frac1_new - 0.5).abs() < 1e-3,
            "行 1 行内比例应保持 0.5，实际 {frac1_new}"
        );
    }

    // ---- A 线：分页调度窗（paged_visible_pages / paged_schedule_window）纯函数测试 ----
    // 夹具 `paged_a4_3()`（见上）：cols=2、rows=2、row_top=[20, 705.04]、cell_h=679.04、
    // total=1404.08、row_page_start=[0,2,3]。行 0 页区间 [0,1]、行 1 页区间 [2,2]。

    /// 可见行页区间（未扩）：首行 r0 推出的闭区间页下标。
    #[test]
    fn paged_visible_pages_from_first_row() {
        let l = paged_a4_3();
        // 视口 [0,700]：首行 0 → 页区间 [row_page_start[0]=0, row_page_start[1]−1=1] = (0,1)。
        assert_eq!(paged_visible_pages(&l, 0.0, 700.0, 3), Some((0, 1)));
        // 视口 [740,1440]：首行 1 → 页区间 [row_page_start[1]=2, row_page_start[2]−1=2] = (2,2)。
        assert_eq!(paged_visible_pages(&l, -740.0, 700.0, 3), Some((2, 2)));
        // 听 0 页文档（rows=0）：first_visible_row → None → 函数 None。
        let empty = paged_layout(&[], 1000.0, 700.0, 1, PAGED_GAP_X, PAGED_GAP_Y, 20.0);
        assert_eq!(paged_visible_pages(&empty, 0.0, 700.0, 0), None);
    }

    /// 调度窗 = 可见行页区间再扩 `k_rows × cols` 页（闭区间、夹到文档内）。
    #[test]
    fn paged_schedule_window_expands_and_clamps() {
        let l = paged_a4_3();
        // 首行可见：k_rows=3、cols=2 → 扩 6 页；start=0−6→0、end=(1+6)=7→min(n−1=2)=2。
        assert_eq!(
            paged_schedule_window(&l, 0.0, 700.0, 3, 3),
            Some((0, 2)),
            "3 页文档整窗即全部页（扩后夹回 0..=2）"
        );
        // 深行可见（页 2）：k_rows=1 → 扩 2 页；start=2−2=0、end=2+2=4→2 → (0,2)。
        assert_eq!(
            paged_schedule_window(&l, -740.0, 700.0, 3, 1),
            Some((0, 2)),
            "深页扩窗应覆盖整窗往返"
        );
        // 空布局 → None（无可见行）。
        let empty = paged_layout(&[], 1000.0, 700.0, 1, PAGED_GAP_X, PAGED_GAP_Y, 20.0);
        assert_eq!(paged_schedule_window(&empty, 0.0, 700.0, 0, 3), None);
    }

    /// 调度窗在**大文档**深页上保「显示页 + 邻行」：用 200 页 A4 单列布局，视口顶贴页 150
    /// 所在行 → 可见页 = 单页区间、扩 `k_rows=3` 行（=±3 页，行=页）→ 窗口 [147..153]。
    #[test]
    fn paged_schedule_window_deep_page_keeps_target() {
        let l = paged_layout(
            &[(793.0, 1123.0); 200],
            1000.0,
            700.0,
            1,
            PAGED_GAP_X,
            PAGED_GAP_Y,
            20.0,
        );
        // 单列：rows=200（行 0..199）、row_height=700、row_top[r]=20+r×(700+6)=20+r×706。贴行 150：行顶 = 20+150×706。
        let p = 150usize;
        let scroll_y = -(20.0 + p as f32 * 706.0);
        let expect = paged_schedule_window(&l, scroll_y, 700.0, 200, 3);
        let Some((s, e)) = expect else {
            panic!("深页应给出调度窗");
        };
        assert!(s <= p && p <= e, "目标页 {p} 应在窗内 [{s},{e}]");
        assert_eq!((s, e), (147, 153), "扩 PAGED_WINDOW_ROWS=3 行 = ±3 页");
    }

    // ---- 渲染窗（paged_render_window）：全可见行 ± 缓冲，Facing=连续自由滚动 ----
    // 同夹具 `paged_a4_3()`：cols=2、rows=2、row_top=[20, 705.04]、row_h=679.04、
    // row_page_start=[0,2,3]、total=1404.08。行 0 页区间 [0,1]、行 1 页区间 [2,2]。

    /// 行中位自由滚动：视口 [65,765] 同时压行 0（[20,699.04]）与行 1（[705.04,1384.08]）→
    /// 全可见行 (0,1) ± 1 行缓冲仍夹到 (0,1) → 页 [row_page_start[0]=0, row_page_start[2]−1=2]
    /// = (0,2)：行中停位上下两行都物化（旧 `paged_visible_pages` 只按 r0 → (0,1) 漏页 2）。
    #[test]
    fn paged_render_window_mid_row_covers_all_visible_rows() {
        let l = paged_a4_3();
        let scroll_y = -(20.0 + 45.0); // 视口顶 = 行 0 顶下 45
        assert_eq!(l.first_visible_row(scroll_y, 700.0), Some((0, 1)));
        assert_eq!(
            paged_render_window(&l, scroll_y, 700.0, 3, 1),
            Some((0, 2)),
            "行中位全可见行(0,1) ± 1 缓冲 → 页 0..=2"
        );
        // 同一视口旧 helper 只按首行 r0 → (0,1)：证明渲染窗与调度窗口径之别。
        assert_eq!(paged_visible_pages(&l, scroll_y, 700.0, 3), Some((0, 1)));
    }

    /// 视口顶贴行 0 顶：可见行 (0,0)，±1 行缓冲把窗扩到行 1 → 页 (0,2)（首行之上不外扩，
    /// 向下缓冲保「下一行提前有卡」——与连续 ±MARGIN 同构）。buf=0 时只物化首行页区间。
    #[test]
    fn paged_render_window_top_row_expands_and_clamps() {
        let l = paged_a4_3();
        // 视口 [0,700]：行 0 可见、行 1 顶（705.04）恰在视口底下方 → 仅 (0,0)。
        assert_eq!(l.first_visible_row(0.0, 700.0), Some((0, 0)));
        // buf=0 → 仅可见行页区间 (0,1)。
        assert_eq!(paged_render_window(&l, 0.0, 700.0, 3, 0), Some((0, 1)));
        // buf=1 → r_hi = (0+1).min(1) = 1 → 页 [0, row_page_start[2]−1=2] = (0,2)。
        assert_eq!(paged_render_window(&l, 0.0, 700.0, 3, 1), Some((0, 2)));
        // 末行/末尾：视口 [740,1440] 压行 1（末行）→ r_hi 夹到 rows−1=1；r_lo = 1−1 = 0
        // → 页 (0,2)（全部夹回文档内，不越界）。
        let scroll_y = -740.0;
        assert_eq!(l.first_visible_row(scroll_y, 700.0), Some((1, 1)));
        assert_eq!(
            paged_render_window(&l, scroll_y, 700.0, 3, 1),
            Some((0, 2)),
            "末行可见时缓冲向上扩 + 下夹不越界"
        );
    }

    /// 空布局 / 已滚过末行 → None（无可见行）。
    #[test]
    fn paged_render_window_empty_returns_none() {
        let empty = paged_layout(&[], 1000.0, 700.0, 1, PAGED_GAP_X, PAGED_GAP_Y, 20.0);
        assert_eq!(paged_render_window(&empty, 0.0, 700.0, 0, 1), None);
        let l = paged_a4_3();
        // 滚过内容底：视口完全在文档之下 → first_visible_row None → 窗 None。
        assert_eq!(
            paged_render_window(&l, -(l.total_height + 10.0), 100.0, 3, 1),
            None
        );
    }
