/// 手动性能基准：定位 B1 rope 单真源后 build_frame 的成本构成——同一大文档在
/// **编辑前（Str 源）** vs **编辑后（Rope 源）** 于不同**带高**下的重建成本：
/// 若随带线性 → 每块物化/装配；若恒定 → 帧内固定代价（整篇扫描/复制）。
#[gpui::test]
#[ignore]
fn bench_frame_str_vs_rope(cx: &mut TestAppContext) {
    use crate::markdown::wysiwyg::WyEditor;
    use gpui::App;
    cx.update(crate::actions::bind_local_keymaps);
    let para = "alpha beta gamma delta epsilon zeta eta theta one two three four five six seven eight nine ten\n";
    let mut text = String::new();
    while text.len() < 6_000_000 {
        text.push_str(para);
    }
    let ref_text = text.clone();
    for band_h in [200f32, 900., 4000., 40_000.] {
        // ── Str 对照：全新（未编辑）编辑器。
        let (host_s, vcx_s) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&ref_text));
            WyHost { ed }
        });
        let vcx_s: &mut VisualTestContext = vcx_s;
        let mut ts = 0u128;
        vcx_s.update(|window, cx: &mut App| {
            for k in 0..6 {
                let band = (k as f32 * 20., k as f32 * 20. + band_h);
                let t0 = std::time::Instant::now();
                let f = build_frame(host_s.read(cx).ed.read(cx), window, px(1200.), band);
                let _ = f.total;
                ts += t0.elapsed().as_millis();
            }
        });
        // ── Rope：同一文本，编辑一次 → 转 Rope 源。
        let (host_r, vcx_r) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&ref_text));
            WyHost { ed }
        });
        let vcx_r: &mut VisualTestContext = vcx_r;
        let mut tr = 0u128;
        vcx_r.update(|window, cx: &mut App| {
            let ed = host_r.read(cx).ed.clone();
            let at = ed.read(cx).text().as_ref().find("one two three").unwrap() + 4;
            ed.update(cx, |e, ecx| e.replace_range(at..at + 1, "x", ecx));
            for k in 0..6 {
                let band = (k as f32 * 20., k as f32 * 20. + band_h);
                let t0 = std::time::Instant::now();
                let f = build_frame(ed.read(cx), window, px(1200.), band);
                let _ = f.total;
                tr += t0.elapsed().as_millis();
            }
        });
        eprintln!(
            "[bench] plain-6MB band_h={band_h}: Str {:.2}ms vs Rope {:.2}ms",
            ts as f64 / 6.,
            tr as f64 / 6.
        );
    }
}

/// 手动性能基准：**结构编辑（插入换行 / Enter）**的完整周期成本（edit + build_frame）
/// ——B3 增量 Pass1 前后对比用：Enter 贴文尾时高度重估只扫「编辑线上」前缀复用；
/// 并行测「文首 Enter」（前缀少、重估近似全篇）作上界。
#[gpui::test]
#[ignore]
fn bench_structural_enter(cx: &mut TestAppContext) {
    use crate::markdown::wysiwyg::WyEditor;
    use gpui::App;
    cx.update(crate::actions::bind_local_keymaps);
    let mut text = String::new();
    for i in 0..3_000 {
        text.push_str(&format!("$$\n\\int_{i} x dx\n$$\n"));
        text.push_str(&format!(
            "text line {i} with $e^{{i\\pi}}+1={i}$ inline math here\n"
        ));
        text.push_str(&format!(
            "> [!NOTE]- alert body line {i}\n>   continuation {i}\n"
        ));
        text.push_str(&format!("```rust\nfn f{i}() {{}}\n```\n"));
        text.push_str(&format!("| h{i} | h2 |\n| --- | --- |\n| c{i} | d |\n"));
    }
    text.push_str("\n\n");
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    // ── 文尾 Enter（前缀复用最多）。
    vcx.update(|window, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        let mut at = ed.read(cx).text().as_ref().len();
        // 预热一帧 + 2 次 Enter（把首次 Pass1/折叠计划/derived 全量重建跑掉），
        // 之后计时的是**稳态 Enter**（增量路径）。
        let _warm = build_frame(ed.read(cx), window, px(1200.), (0., 900.));
        for _ in 0..2 {
            ed.update(cx, |e, ecx| {
                e.set_caret(at);
                e.replace_range(at..at, "\n", ecx);
                at = e.caret();
            });
            build_frame(ed.read(cx), window, px(1200.), (0., 900.));
        }
        let mut t = 0u128;
        for _ in 0..12 {
            let t0 = std::time::Instant::now();
            ed.update(cx, |e, ecx| {
                e.set_caret(at);
                e.replace_range(at..at, "\n", ecx);
                at = e.caret();
            });
            build_frame(ed.read(cx), window, px(1200.), (0., 900.));
            t += t0.elapsed().as_millis();
        }
        eprintln!("[bench] Enter at TAIL+cycle: {:.2}ms/次", t as f64 / 12.);
    });
}

/// 手动性能基准：量化 A1「命中先行」的收益——缓存预热后**同带缓存重建**（只物化
/// miss / caret 块）对比旧行为的「整带 `rows_in_block_window` 物化」单次成本
/// （旧实现每帧/每带重建都先付整带物化再查缓存）。
#[gpui::test]
#[ignore]
fn bench_cached_band_rebuild_vs_materialize(cx: &mut TestAppContext) {
    use crate::markdown::wysiwyg::WyEditor;
    use gpui::App;
    cx.update(crate::actions::bind_local_keymaps);
    let mut text = String::new();
    for i in 0..3_000 {
        text.push_str(&format!("$$\n\\int_{i} x dx\n$$\n"));
        text.push_str(&format!(
            "text line {i} with $e^{{i\\pi}}+1={i}$ inline math here\n"
        ));
        text.push_str(&format!(
            "> [!NOTE]- alert body line {i}\n>   continuation {i}\n"
        ));
        text.push_str(&format!("```rust\nfn f{i}() {{}}\n```\n"));
        text.push_str(&format!("| h{i} | h2 |\n| --- | --- |\n| c{i} | d |\n"));
    }
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|window, cx: &mut App| {
            let ed = host.read(cx).ed.clone();
            let band = (0., 800.);
            let warm = build_frame(ed.read(cx), window, px(900.), band);
            let nband_blocks = warm.rows.len().max(1);
            // (a) 同带缓存重建（A1 后只物化 caret 行 + 未命中块）。
            let mut t_a = 0u128;
            for _ in 0..20 {
                let t0 = std::time::Instant::now();
                let f = build_frame(ed.read(cx), window, px(900.), band);
                let _ = f.total;
                t_a += t0.elapsed().as_millis();
            }
            // (b) 旧行为每重建必付的「整带物化」成本（同量级块数）。
            let mut t_b = 0u128;
            for _ in 0..20 {
                let t0 = std::time::Instant::now();
                let rows = ed
                    .read(cx)
                    .doc()
                    .rows_in_block_window(0, nband_blocks.saturating_sub(1));
                let _ = rows.len();
                t_b += t0.elapsed().as_millis();
            }
            eprintln!(
                "[bench] cached same-band rebuild: {:.2}ms; full-band materialize(旧每帧必付): {:.2}ms (band blocks={})",
                t_a as f64 / 20.,
                t_b as f64 / 20.,
                nband_blocks
            );
            // 全文档带（`(0., 999999.)`，屏外/整表重建路径）：缓存预热后重建对比整带物化。
            let band_all = (0., 999_999.);
            let _warm_all = build_frame(ed.read(cx), window, px(900.), band_all);
            let n_all = ed.read(cx).doc().block_count();
            let mut t_a2 = 0u128;
            for _ in 0..10 {
                let t0 = std::time::Instant::now();
                let f = build_frame(ed.read(cx), window, px(900.), band_all);
                let _ = f.total;
                t_a2 += t0.elapsed().as_millis();
            }
            let mut t_b2 = 0u128;
            for _ in 0..10 {
                let t0 = std::time::Instant::now();
                let rows = ed
                    .read(cx)
                    .doc()
                    .rows_in_block_window(0, n_all.saturating_sub(1));
                let _ = rows.len();
                t_b2 += t0.elapsed().as_millis();
            }
            eprintln!(
                "[bench] full-doc cached rebuild: {:.2}ms; full-doc materialize(旧每帧必付): {:.2}ms (blocks={})",
                t_a2 as f64 / 10.,
                t_b2 as f64 / 10.,
                n_all
            );
        });
}

/// M5-B3-1 手动基准（`--ignored` + `--nocapture` 才跑）：量化两个逐键 O(n) 残余的收口
/// ① 折叠了一个标题后、在别处（折叠体之外）按 Enter——旧实现每键 `text_shared()` 全篇
/// 物化 + `heading_fold_regions` 全扫 + 禁用 deltas 平移；新实现标题折叠体移入 derived
/// 行号平移、sets 重建不再物化全文、deltas 平移。
/// ② 在普通行打 `$`（行内公式不构成区域）——旧实现每键 `regions_ver++` → derived 全篇
/// 数学/警报重扫；新实现精确判定后跳过。
/// 输出 ms/key 与 text_shared 计数差（后者是收口对象）供 before/after 对比。
/// 注意：Pass1（结构编辑后缀估计）与带内 Pass2 整形是此规模文档的固有成本，两种模式
/// 都在；对比看的是**差量**（旧实现的 text_shared + 标题全扫 + deltas 整建被消掉）。
#[gpui::test]
#[ignore]
fn bench_m5b31_per_key_enter_and_dollar(cx: &mut TestAppContext) {
    use crate::markdown::wysiwyg::WyEditor;
    use gpui::App;
    cx.update(crate::actions::bind_local_keymaps);
    // 大文档：头部一个折叠标题（body 被 `# Tail` 截断）+ 顶部一簇数学块 + 普通段落；
    // 编辑点放在顶部的 `# Tail` 之后（折叠体之外、又不至于让 Pass1 后缀太长）。
    let mut text = String::from("## App\napp body\n");
    for i in 0..12 {
        text.push_str(&format!(
            "zone paragraph {i} with $e^{{i\\pi}}$ inline math here\n$$\n\\int_{i} x dx\n$$\n"
        ));
    }
    text.push_str("# Tail\n");
    let mut k = 0;
    while text.len() < 1_000_000 {
        text.push_str(&format!(
            "ordinary para line {k} some more words to fill the doc here\n"
        ));
        k += 1;
    }
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|window, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |e, cx| e.toggle_heading_fold_at_line(0, cx));
        let _f0 = build_frame(ed.read(cx), window, px(900.), (0., 999_999.));
        let n = ed.read(cx).content().len();
        let tail_at = ed.read(cx).content().find("ordinary para line 3").unwrap();
        // 40 个编辑点（都在 `# Tail` 之下、折叠体之外）；每点隔 80 字节（落不同行）。
        let ats: Vec<usize> = (0..40).map(|j| tail_at + j * 80).collect();
        // ① Enter。
        let t0 = std::time::Instant::now();
        for (i, &at) in ats.iter().enumerate() {
            let c_a = ed.read(cx).doc().text_shared_calls();
            ed.update(cx, |e, ecx| e.replace_range(at..at, "\n", ecx));
            let _f = build_frame(ed.read(cx), window, px(900.), (0., 1400.));
            let delta_c = ed.read(cx).doc().text_shared_calls() - c_a;
            eprintln!("[bench] Enter iter={i} text_shared_delta={delta_c}");
        }
        let dt1 = t0.elapsed().as_millis() as f64 / ats.len() as f64;
        // ② 打 `$`：同样 40 个编辑点各打一个 `$`（仍在普通行/折叠体之外；① 的换行
        // 使偏移略漂移，但仍在普通段落区，计时对比成立）。
        let t2 = std::time::Instant::now();
        for (i, &at) in ats.iter().enumerate() {
            let c_a = ed.read(cx).doc().text_shared_calls();
            ed.update(cx, |e, ecx| e.replace_range(at..at, "$", ecx));
            let _f = build_frame(ed.read(cx), window, px(900.), (0., 1400.));
            let delta_c = ed.read(cx).doc().text_shared_calls() - c_a;
            eprintln!("[bench] dollar iter={i} text_shared_delta={delta_c}");
        }
        let dt2 = t2.elapsed().as_millis() as f64 / ats.len() as f64;
        eprintln!(
            "[bench] M5-B3-1 doc={n}B 折叠标题後别处 40×Enter: {dt1:.3}ms/key; 普通行 40×打`$`: {dt2:.3}ms/key"
        );
    });
}

/// 手动性能基准（`--ignored` + `--nocapture` 才跑）：在「结构密集」大文档上量化
/// 编辑/滚动的单次成本，供人工复测比对（debug 模式即与 `cargo run` 同构）。
#[gpui::test]
#[ignore]
fn bench_dense_doc_edit_and_scroll(cx: &mut TestAppContext) {
    use crate::markdown::wysiwyg::WyEditor;
    use gpui::App;
    cx.update(crate::actions::bind_local_keymaps);
    // 仿 `大文档测试2.md`：每 6 行一组「数学块 + 行内公式 + callout + 代码 + 表格」，
    // 末尾附一个纯文本段（普通键入的落点）。
    let mut text = String::new();
    for i in 0..3_000 {
        text.push_str(&format!("$$\n\\int_{i} x dx\n$$\n"));
        text.push_str(&format!(
            "text line {i} with $e^{{i\\pi}}+1={i}$ inline math here\n"
        ));
        text.push_str(&format!(
            "> [!NOTE]- alert body line {i}\n>   continuation {i}\n"
        ));
        text.push_str(&format!("```rust\nfn f{i}() {{}}\n```\n"));
        text.push_str(&format!("| h{i} | h2 |\n| --- | --- |\n| c{i} | d |\n"));
    }
    text.push_str("plain paragraph zone one\nplain paragraph zone two\n");
    let nbytes = text.len();
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;

    vcx.update(|window, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        let t0 = std::time::Instant::now();
        let f = build_frame(ed.read(cx), window, px(900.), (0., 800.));
        let dt = t0.elapsed().as_millis();
        let nreg = ed.read(cx).derived().borrow().math_regions.len();
        eprintln!(
            "[bench] open+first band frame: {dt}ms ({nbytes}B, blocks={}, math_regions={nreg})",
            f.block_count()
        );
    });

    vcx.update(|window, cx: &mut App| {
            let ed = host.read(cx).ed.clone();
            let mut rebuilt = 0u32;
            // 锚点只取一次（此后 1:1 字符替换，长度不变 → 锚点恒有效）。
            let at = ed.read(cx).content().find("zone one").unwrap() + 5;
            let t0 = std::time::Instant::now();
            // **真实逐键循环**：replace_range + 下一帧 build_frame（caret 每键前进，
            // 与真实应用一致——`set_caret`/`replace_range` 都会清帧触发重建）。
            for k in 0..30 {
                let v0 = ed.read(cx).regions_ver();
                ed.update(cx, |e, ecx| e.replace_range(at..at + 1, "x", ecx));
                if ed.read(cx).regions_ver() != v0 {
                    rebuilt += 1;
                }
                let band = (k as f32 * 20., k as f32 * 20. + 800.);
                build_frame(ed.read(cx), window, px(900.), band);
            }
            let dt = t0.elapsed().as_millis();
            eprintln!("[bench] 30 typing full-cycles (edit+frame): {dt}ms ({:.1}ms/key, rebuilt={rebuilt}/30)", dt as f64 / 30.);
        });

    vcx.update(|_w, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        let mut rebuilt = 0u32;
        let at = ed.read(cx).content().find("\\int_1 x dx").unwrap() + 5;
        let t0 = std::time::Instant::now();
        for _ in 0..10 {
            let v0 = ed.read(cx).regions_ver();
            ed.update(cx, |e, ecx| e.replace_range(at..at + 1, "2", ecx));
            if ed.read(cx).regions_ver() != v0 {
                rebuilt += 1;
            }
        }
        let dt = t0.elapsed().as_millis();
        eprintln!(
            "[bench] 10 math-interior edits: {dt}ms ({:.1}ms/key, rebuilt={rebuilt}/10)",
            dt as f64 / 10.
        );
    });

    vcx.update(|window, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        let mut t = 0u128;
        let mut t2 = 0u128;
        for k in 0..20 {
            let t0 = std::time::Instant::now();
            let band = (k as f32 * 4000., k as f32 * 4000. + 800.);
            let f = build_frame(ed.read(cx), window, px(900.), band);
            let _ = f.total;
            t += t0.elapsed().as_millis();
            // 模拟「公式光栅逐个完成」：mgen 变化 → fold_plan 重算（基础布局不再整篇重建）。
            let t1 = std::time::Instant::now();
            let band2 = (k as f32 * 4000. + 2000., k as f32 * 4000. + 2800.);
            let f2 = build_frame(ed.read(cx), window, px(900.), band2);
            let _ = f2.total;
            t2 += t1.elapsed().as_millis();
        }
        eprintln!(
            "[bench] 20 band rebuilds: {t}ms ({:.1}ms/band)",
            t as f64 / 20.
        );
        eprintln!(
            "[bench] 20 near-band rebuilds (caret-in-band): {t2}ms ({:.1}ms/band)",
            t2 as f64 / 20.
        );
    });

    // 光标移动（不动内容）→ `set_caret` 清帧 → build_frame 每步成本。
    vcx.update(|window, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        let n = ed.read(cx).content().len();
        let mut t = 0u128;
        for k in 0..20 {
            let off = (n * k / 20).min(n.saturating_sub(1));
            ed.update(cx, |e, _| e.set_caret(off));
            let t0 = std::time::Instant::now();
            let band = (k as f32 * 4000., k as f32 * 4000. + 800.);
            let f = build_frame(ed.read(cx), window, px(900.), band);
            let _ = f.total;
            t += t0.elapsed().as_millis();
        }
        eprintln!(
            "[bench] 20 caret-move frame rebuilds: {t}ms ({:.1}ms/step)",
            t as f64 / 20.
        );
    });
}

/// 编辑路径分项剖析：整串复制 / SharedString 转 / Doc::reopen / 通知 各占多少。
#[gpui::test]
#[ignore]
fn bench_edit_path_split(cx: &mut TestAppContext) {
    use crate::markdown::wysiwyg::WyEditor;
    use gpui::App;
    cx.update(crate::actions::bind_local_keymaps);
    let mut text = String::new();
    let para = "alpha beta gamma delta epsilon zeta eta theta one two three four five six seven eight nine ten\n";
    while text.len() < 6_000_000 {
        text.push_str(para);
    }
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|window, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        let at = ed.read(cx).content().find("one two three").unwrap() + 4;
        let s = ed.read(cx).content();
        let r = at..at + 1;
        // (a) 整串复制。
        let mut t = 0u128;
        let mut next = String::new();
        for _ in 0..20 {
            let t0 = std::time::Instant::now();
            let mut n = String::with_capacity(s.len() - r.len() + 1);
            n.push_str(&s[..r.start]);
            n.push('x');
            n.push_str(&s[r.end..]);
            next = n;
            t += t0.elapsed().as_millis();
        }
        eprintln!("[bench] (a) full-string copy: {:.2}ms", t as f64 / 20.);
        // (b) SharedString::from。
        let mut t = 0u128;
        for _ in 0..20 {
            let t0 = std::time::Instant::now();
            let _ss = gpui::SharedString::from(next.clone());
            t += t0.elapsed().as_millis();
        }
        eprintln!(
            "[bench] (b) SharedString::from(String): {:.2}ms",
            t as f64 / 20.
        );
        // (c) Doc::edit（原地 rope 编辑，替代旧 `reopen` 的整串拼接+重建）。
        let mut t = 0u128;
        let s_ss = ed.read(cx).text();
        let mut doc = crate::markdown::doc::Doc::open(s_ss.clone());
        for _ in 0..20 {
            let t0 = std::time::Instant::now();
            doc.edit(r.clone(), "x");
            t += t0.elapsed().as_millis();
        }
        eprintln!("[bench] (c) Doc::edit incremental: {:.2}ms", t as f64 / 20.);
        // (e) 新零拷贝路径：String → Box<str> → Arc<str> → SharedString。
        let mut t = 0u128;
        for _ in 0..20 {
            let mut x = String::with_capacity(s.len());
            x.push_str(&s[..r.start]);
            x.push('x');
            x.push_str(&s[r.end..]);
            let t0 = std::time::Instant::now();
            let _ss = gpui::SharedString::from(std::sync::Arc::<str>::from(x.into_boxed_str()));
            t += t0.elapsed().as_millis();
        }
        eprintln!(
            "[bench] (e) new zero-copy concat+SharedString: {:.2}ms",
            t as f64 / 20.
        );
        // (f) rebuild_block_regions（replace_range 每键 O(块数) 扫描）。
        let mut t = 0u128;
        let mut doc2 = crate::markdown::doc::Doc::open(s_ss.clone());
        for _ in 0..20 {
            doc2.edit(r.clone(), "x");
            let n = doc2.block_count();
            let t0 = std::time::Instant::now();
            let mut regions: usize = 0;
            for i in 0..n {
                if let Some(m) = doc2.block_meta(i)
                    && matches!(
                        m.kind,
                        crate::markdown::parse::RowKind::Code
                            | crate::markdown::parse::RowKind::Table
                    )
                {
                    regions += 1;
                }
            }
            let _ = regions;
            t += t0.elapsed().as_millis();
        }
        eprintln!(
            "[bench] (f) rebuild_block_regions scan (n={}): {:.2}ms",
            doc2.block_count(),
            t as f64 / 20.
        );
        // (d) board emits 未计入（UI 层）。总 edit-only 对照。
        let _ = window;
        let _ = ed;
    });
}

#[gpui::test]
#[ignore]
fn bench_scale_with_doc_size(cx: &mut TestAppContext) {
    use crate::markdown::wysiwyg::WyEditor;
    use gpui::App;
    cx.update(crate::actions::bind_local_keymaps);
    // —— 1) 纯文本大文档：键入成本随字节数线性放大？——
    let mut text = String::new();
    let para = "alpha beta gamma delta epsilon zeta eta theta one two three four five six seven eight nine ten\n";
    let mut mb = 0usize;
    while mb < 6_000_000 {
        text.push_str(para);
        mb = text.len();
    }
    let (host, _vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = _vcx;
    vcx.update(|window, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        let at = ed.read(cx).content().find("one two three").unwrap() + 4;
        let mut t = 0u128;
        // replace_range 单独（不 rebuild frame）→ 量化整串复制 + reopen。
        for _ in 0..30 {
            let t0 = std::time::Instant::now();
            ed.update(cx, |e, ecx| e.replace_range(at..at + 1, "x", ecx));
            t += t0.elapsed().as_millis();
        }
        eprintln!(
            "[bench] plain-6MB edit-only: {t}ms ({:.1}ms/key, text={}B)",
            t as f64 / 30.,
            ed.read(cx).content().len()
        );
        // 完整键入周期（edit + band rebuild）。
        let mut t2 = 0u128;
        for k in 0..30 {
            let t0 = std::time::Instant::now();
            ed.update(cx, |e, ecx| e.replace_range(at..at + 1, "y", ecx));
            let band = (k as f32 * 20., k as f32 * 20. + 900.);
            build_frame(ed.read(cx), window, px(1200.), band);
            t2 += t0.elapsed().as_millis();
        }
        eprintln!(
            "[bench] plain-6MB typing-cycle: {t2}ms ({:.1}ms/key)",
            t2 as f64 / 30.
        );
    });

    // —— 2) 代码密集文档：滚动重建里代码块占多少（语法高亮重解析）？——
    let mut code = String::new();
    for i in 0..2_000 {
        code.push_str(&format!("```rust\nfn compute_{i}(a: u32, b: u32) -> u32 {{\n    let sum = a + b * {i};\n    let diff = a.saturating_sub(b);\n    if sum > diff {{ return sum; }}\n    diff\n}}\n```\n"));
        code.push_str(&format!("some paragraph text {i} after code block\n"));
    }
    let (host2, _vcx2) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&code));
        WyHost { ed }
    });
    let vcx2: &mut VisualTestContext = _vcx2;
    let mut t3 = 0u128;
    let mut t4 = 0u128;
    vcx2.update(|window, cx: &mut App| {
            let ed = host2.read(cx).ed.clone();
            let n = ed.read(cx).doc().block_count();
            let total = n;
            let _ = ed.read(cx).derived();
            // 预热基础布局 + 派生扫描。
            build_frame(ed.read(cx), window, px(1200.), (0., 2400.));
            for k in 0..12 {
                let top = (total * k / 12).min(total.saturating_sub(1)) as f32;
                let band = (top * 22., top * 22. + 2400.); // 每块 ~22px 的近似推进
                let t0 = std::time::Instant::now();
                let f = build_frame(ed.read(cx), window, px(1200.), band);
                t3 += t0.elapsed().as_millis();
                let _ = f.rows.len();
                // 前进半个屏幕（512px 量程内不同带 → 命中 1 次重建）。
                let t1 = std::time::Instant::now();
                let band2 = (top * 22. + 300., top * 22. + 2700.);
                let _ = build_frame(ed.read(cx), window, px(1200.), band2);
                t4 += t1.elapsed().as_millis();
            }
            eprintln!(
                "[bench] code-heavy band rebuild: fresh {:.1}ms, +300px-shift {:.1}ms (blocks={}, code blocks≈{})",
                t3 as f64 / 12.,
                t4 as f64 / 12.,
                total,
                total / 2
            );
        });
}

#[gpui::test]
fn handler_large_doc_cjk_roundtrip_no_full_materialize(cx: &mut TestAppContext) {
    use gpui::EntityInputHandler;
    cx.update(crate::actions::bind_local_keymaps);
    // 大文档：8000 行混合 ASCII/中文/emoji——行索引目标落在文档中后部（半篇~整篇）。
    // 同时含尾随换行（幻影尾行）与多字节字符，覆盖 UTF-16 换算的边界。
    let mut text = String::new();
    for i in 0..8000 {
        if i % 17 == 0 {
            text.push('中');
        }
        if i % 31 == 0 {
            text.push('\u{1f680}');
        }
        text.push_str(&format!("line {i}\n"));
    }
    text.push_str("你好 target 尾\n");
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        // `reference` 逐编辑镜像 doc，作为全文比对与 UTF-16 换算的参照。
        let mut reference = text.clone();
        let tail = "你好 target 尾\n";
        ed.update(cx, |e, cx| {
            let calls0 = e.text_shared_calls();
            // ---- 阶段 A：纯协议查询/编辑，全部不物化全文（期间不调 e.text()）----
            // ① 总长 = 全文 UTF-16 单元数（O(1) 计数器）。
            assert_eq!(
                e.text_length_utf16(window, cx),
                Some(reference.encode_utf16().count()),
                "total_utf16 应等于全文 UTF-16 长"
            );
            // ② text_for_range：文档末尾一段（跨中文/emoji/换行），切片精确。
            let total_u16 = reference.encode_utf16().count();
            let start_u16 = total_u16 - tail.encode_utf16().count();
            let mut actual = None;
            let got = e
                .text_for_range(start_u16..total_u16, &mut actual, window, cx)
                .expect("text_for_range 应命中");
            assert_eq!(got, tail, "text_for_range 切片应精确（含中文/emoji/换行）");
            assert_eq!(actual, Some(start_u16..total_u16), "actual_range 应回填");
            // ③ 选区：光标到文末 → UTF-16 末端零宽选区。
            e.set_caret(reference.len());
            let sel = e.selected_text_range(true, window, cx).unwrap();
            assert_eq!(sel.range, total_u16..total_u16, "caret 在文末");
            assert!(!sel.reversed);
            // ④ set_selected_text_range 设置中后部，UTF-16 往返无损。
            let mid_u16 = total_u16 / 2;
            e.set_selected_text_range(mid_u16..mid_u16 + 1, window, cx);
            assert!(e.caret() <= reference.len(), "caret 应钳在字节界内");
            let sel2 = e.selected_text_range(true, window, cx).unwrap();
            assert_eq!(sel2.range, mid_u16..mid_u16 + 1, "UTF-16 往返无损");
            // ⑤ marked 组合：字节存储 → UTF-16 换算精确；取消（ESC）清占位。
            let caret_byte = reference.len() - tail.len();
            e.set_caret(caret_byte);
            let mark_u16 = reference[..caret_byte].encode_utf16().count();
            e.replace_and_mark_utf16(None, "组合", None, cx);
            reference.insert_str(caret_byte, "组合");
            let marked = e.marked_text_range(window, cx).expect("有 marked");
            assert_eq!(marked.start, mark_u16, "marked 起点 UTF-16");
            assert_eq!(marked.end - marked.start, "组合".encode_utf16().count());
            e.replace_text_utf16(None, "", cx);
            reference.replace_range(caret_byte..caret_byte + "组合".len(), "");
            // ⑥ Windows 组合序列（range=None，替换旧 marked 不堆叠）：n→nih→提交"你好"。
            e.set_caret(reference.len());
            e.replace_and_mark_utf16(None, "n", None, cx);
            e.replace_and_mark_utf16(None, "nih", None, cx);
            e.replace_text_utf16(None, "你好", cx);
            reference.push_str("你好");
            // ⑦ 显式 UTF-16 区间（macOS 风格）走 utf16_to_byte 换算：文末插 X。
            let pos_u16 = reference.encode_utf16().count();
            e.replace_text_utf16(Some(pos_u16..pos_u16), "X", cx);
            reference.push('X');
            // ---- 断言：阶段 A 的查询/编辑全程零次 text_shared（O(n) 全拷）----
            assert_eq!(
                e.text_shared_calls(),
                calls0,
                "EntityInputHandler 的查询/编辑不得调用 text_shared——必须走行索引/切片/增量计数"
            );
            // ---- 收尾：全文精确比对（e.text() 触发 text_shared 属正常宿主写回路径）----
            assert_eq!(
                &e.text(),
                &reference,
                "大文档协议层编辑后全文与参照逐字节一致"
            );
        });
    });
}
#[gpui::test]
#[ignore]
fn bench_rw_dense(cx: &mut TestAppContext) {
    use crate::markdown::wysiwyg::WyEditor;
    cx.update(crate::actions::bind_local_keymaps);

    fn doc_dense() -> String {
        let mut t = String::new();
        let mut i = 0usize;
        while t.len() < 6_000_000 {
            t.push_str(&format!(
                "# Heading {i}\n\n- list item a\n- list item b\n\n> quote {i}\n\n"
            ));
            i += 1;
        }
        t
    }
    fn single_case(cx: &mut TestAppContext, name: &str, text: String) {
        let (host, _vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&text));
            WyHost { ed }
        });
        let _ = cx.refresh();
        let at = text.len() / 2;
        let mut t = 0u128;
        let n = 12;
        for k in 0..n {
            let t0 = std::time::Instant::now();
            let ins = if k % 2 == 0 { "x" } else { "y" };
            cx.update(|cx| {
                host.update(cx, |w, cx| {
                    w.ed.update(cx, |e, ecx| e.replace_range(at..at + 1, ins, ecx))
                });
            });
            let _ = cx.refresh();
            t += t0.elapsed().as_millis();
        }
        eprintln!(
            "[bench] {name} type-char: {}ms ({:.1}ms/key)",
            t,
            t as f64 / n as f64
        );
        crate::prof::dump(&format!("wyreal.{name}.type"));
        let mut t2 = 0u128;
        let n2 = 8;
        for _k in 0..n2 {
            let t0 = std::time::Instant::now();
            cx.update(|cx| {
                host.update(cx, |w, cx| {
                    w.ed.update(cx, |e, ecx| e.replace_range(at..at + 1, "\n", ecx))
                });
            });
            let _ = cx.refresh();
            t2 += t0.elapsed().as_millis();
        }
        eprintln!(
            "[bench] {name} enter: {}ms ({:.1}ms/key)",
            t2,
            t2 as f64 / n2 as f64
        );
        crate::prof::dump(&format!("wyreal.{name}.enter"));
    }
    single_case(cx, "dense", doc_dense());
}
#[gpui::test]
#[ignore]
fn bench_rw_plain(cx: &mut TestAppContext) {
    cx.update(crate::actions::bind_local_keymaps);
    fn doc_plain() -> String {
        let para = "alpha beta gamma delta epsilon zeta eta theta one two three four five six seven eight nine ten\n";
        let mut t = String::new();
        while t.len() < 6_000_000 {
            t.push_str(para);
        }
        t
    }
    let (host, _vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&doc_plain()));
        WyHost { ed }
    });
    let _ = cx.refresh();
    let at = 3_000_000usize;
    let mut t = 0u128;
    let n = 12;
    for k in 0..n {
        let t0 = std::time::Instant::now();
        let ins = if k % 2 == 0 { "x" } else { "y" };
        cx.update(|cx| {
            host.update(cx, |w, cx| {
                w.ed.update(cx, |e, ecx| e.replace_range(at..at + 1, ins, ecx))
            })
        });
        let _ = cx.refresh();
        t += t0.elapsed().as_millis();
    }
    eprintln!(
        "[bench] plain type-char: {}ms ({:.1}ms/key)",
        t,
        t as f64 / n as f64
    );
    crate::prof::dump("wyreal.plain.type");
}
#[gpui::test]
#[ignore]
fn bench_rw_code(cx: &mut TestAppContext) {
    cx.update(crate::actions::bind_local_keymaps);
    fn doc_code() -> String {
        let mut t = String::from("```\n");
        let line = "let alpha beta gamma delta epsilon zeta = compute_one_two_three_four_five_six_seven_eight_nine_ten();\n";
        while t.len() < 6_000_000 {
            t.push_str(line);
        }
        t.push_str("```\n");
        t
    }
    let (host, _vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&doc_code()));
        WyHost { ed }
    });
    let _ = cx.refresh();
    let at = 3_000_000usize;
    let mut t = 0u128;
    let n = 12;
    for k in 0..n {
        let t0 = std::time::Instant::now();
        let ins = if k % 2 == 0 { "x" } else { "y" };
        cx.update(|cx| {
            host.update(cx, |w, cx| {
                w.ed.update(cx, |e, ecx| e.replace_range(at..at + 1, ins, ecx))
            })
        });
        let _ = cx.refresh();
        t += t0.elapsed().as_millis();
    }
    eprintln!(
        "[bench] code type-char: {}ms ({:.1}ms/key)",
        t,
        t as f64 / n as f64
    );
    crate::prof::dump("wyreal.code.type");
}
#[gpui::test]
#[ignore]
fn bench_per_key_code_highlight_completion(cx: &mut TestAppContext) {
    // 量化「代码块内每键打字」的稳态成本（真实窗口顶部密集带 24-34ms/键的表现路径）：
    // 每键 = replace_range（非结构，块内单字符）→ build_frame（新内容纯文本行）→
    // `on_code_parse_ready`（模拟后台高亮就绪）→ 紧接 build_frame。
    // 修复前：on_code_parse_ready 全局 code_parse_gen++ → 整带缓存全 miss → 最后一帧
    // 重排整个带（含无关段落）；修复后：只定向重排该代码块。这里测的就是「就绪帧」。
    use crate::markdown::wysiwyg::WyEditor;
    use gpui::App;
    cx.update(crate::actions::bind_local_keymaps);
    let mut text = String::new();
    for i in 0..6 {
        text.push_str(&format!("para line {i} some more words to wrap here\n"));
    }
    text.push_str("```rust\n");
    for i in 0..12 {
        text.push_str(&format!(
            "fn compute_{i}(a: u32, b: u32) -> u32 {{\n    a + b + {i}\n}}\n"
        ));
    }
    text.push_str("```\n");
    for i in 6..90 {
        text.push_str(&format!("para line {i} some more words to wrap here\n"));
    }
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&text));
        WyHost { ed }
    });
    let code_src_line = 6; // ```rust 围栏源行
    // 代码块内一个 ASCII 字节位置（首行 fn 后或函数体内）；反复用同一位置替换。
    let at = text.find("fn compute_0").unwrap();
    vcx.update(|window, cx: &mut App| {
            let ed = host.read(cx).ed.clone();
            ed.update(cx, |e, _| e.set_caret(at));
            // 首帧：带覆盖代码块（全物化 + 请求后台解析）；随后归零整形计数。
            let _f0 = build_frame(ed.read(cx), window, px(900.), (0., 1600.));
            ed.read(cx).take_reshapes();
            // 2 次暖机（首帧 Pass1/脏状态）。
            for ins in ["q", "w"] {
                ed.update(cx, |e, ecx| e.replace_range(at..at + 1, ins, ecx));
                build_frame(ed.read(cx), window, px(900.), (0., 1600.));
                ed.update(cx, |e, ecx| e.on_code_parse_ready(&[code_src_line], ecx));
                build_frame(ed.read(cx), window, px(900.), (0., 1600.));
            }
            let mut t_edit_bf = 0u128;
            let mut t_ready_bf = 0u128;
            let mut resh = 0u64;
            let n = 12;
            for k in 0..n {
                let ins = if k % 2 == 0 { "x" } else { "y" };
                let t0 = std::time::Instant::now();
                ed.update(cx, |e, ecx| e.replace_range(at..at + 1, ins, ecx));
                build_frame(ed.read(cx), window, px(900.), (0., 1600.));
                t_edit_bf += t0.elapsed().as_millis();
                let t1 = std::time::Instant::now();
                ed.update(cx, |e, ecx| e.on_code_parse_ready(&[code_src_line], ecx));
                build_frame(ed.read(cx), window, px(900.), (0., 1600.));
                t_ready_bf += t1.elapsed().as_millis();
                resh += ed.read(cx).take_reshapes();
            }
            eprintln!(
                "[bench] code-per-key: edit-frame {:.2}ms/键, ready-frame {:.2}ms/键, {} 次/键重排 (band blocks, 修复前≈整带)",
                t_edit_bf as f64 / n as f64,
                t_ready_bf as f64 / n as f64,
                resh as f64 / n as f64,
            );
        });
}
#[gpui::test]
#[ignore]
fn bench_per_key_math_completion(cx: &mut TestAppContext) {
    // 量化「公式排版就绪（on_math_ready）→ 下一帧」的稳态重排次数（对齐 code 修复
    // 前的 `bench_per_key_code_highlight_completion`：43.2 次/键整带重排）。修复前
    // `mgen` 进全局 shape_key——每个公式完成都让整带所有块（含无关段落）全 miss 重排
    // 一次（肉眼 = 公式就绪时整片闪烁）；修复后 `on_math_ready` 只定向作废该公式
    // 所在行 → 就绪帧只见该行/带边界重排。显示块（`$$…$$`，Pass2 blocked 不入缓存）
    // 与行内公式（行正常入 `shape_rows`）两例各测 12 轮。
    use crate::markdown::wysiwyg::MathSlot;
    use gpui::App;
    cx.update(crate::actions::bind_local_keymaps);
    let mut text = String::new();
    for i in 0..6 {
        text.push_str(&format!("para line {i} some more words to wrap here\n"));
    }
    text.push_str("$$\nx + y = 2^5\n$$\n");
    text.push_str("inline math $E=mc^2$ here ends\n");
    for i in 6..90 {
        text.push_str(&format!("para line {i} some more words to wrap here\n"));
    }
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let img = std::sync::Arc::new(gpui::RenderImage::new(vec![image::Frame::new(
        image::RgbaImage::from_raw(200, 40, vec![0u8; 200 * 40 * 4]).unwrap(),
    )]));
    vcx.update(|window, cx: &mut App| {
            let ed = host.read(cx).ed.clone();
            // 光标停在正文（数学块被折叠成卡、行内行以常规行入 `shape_rows`；光标外行可缓存）。
            ed.update(cx, |e, _| e.set_caret(0));
            let f = build_frame(ed.read(cx), window, px(900.), (0., 1600.));
            // 从首帧摘键：块卡 latex（trimmed）与行内公式 key（"inline:…"）。
            let block_key = f
                .rows
                .iter()
                .find_map(|r| match &r.extra.card {
                    Some(super::BlockCard::Math { latex, .. }) => Some(latex.trim().to_string()),
                    _ => None,
                })
                .expect("应有 $$ 显示块卡片");
            let inline_key = f
                .rows
                .iter()
                .find_map(|r| r.extra.inline_maths.first())
                .map(|m| m.key.to_string())
                .expect("应有行内公式");
            let _ = ed.read(cx).take_reshapes();
            let band_blocks = f.rows.len();
            let mut completion_case = |label: &str, key: &str, h_img: f32| {
                let mut resh = 0u64;
                let mut t_ms = 0u128;
                let n = 12;
                for _ in 0..n {
                    ed.update(cx, |e, ecx| {
                        e.math_slots()
                            .borrow_mut()
                            .insert(key.into(), MathSlot::Ready(img.clone(), 200., h_img));
                        e.on_math_ready(key, ecx);
                    });
                    let t0 = std::time::Instant::now();
                    build_frame(ed.read(cx), window, px(900.), (0., 1600.));
                    t_ms += t0.elapsed().as_millis();
                    resh += ed.read(cx).take_reshapes();
                }
                eprintln!(
                    "[bench] math-completion {label}: {:.2} 次/键重排, {:.3}ms/键（band blocks={band_blocks}，修复前≈整带 67 行 + 带重建）",
                    resh as f64 / n as f64,
                    t_ms as f64 / n as f64,
                );
            };
            completion_case("block", &block_key, 40.);
            completion_case("inline", &inline_key, 24.);
        });
}
#[gpui::test]
#[ignore]
fn bench_undo_redo_cycle(cx: &mut TestAppContext) {
    // 真实文件 undo/redo 循环稳态成本：恢复快照（Doc::open_rope 全量重建 +
    // rebuild_block_regions + text_shared 整串）+ restore 后 build_frame 的
    // pass1(pass1_base_layout 全量重估) / setup(derived 重建) / pass2(带内重排)。
    // restore（=ed.undo/redo 内部）与 build_frame 分拆计时；KILO_PROF=1 时 dump 拆账。
    use crate::markdown::wysiwyg::WyEditor;
    use gpui::App;
    cx.update(crate::actions::bind_local_keymaps);
    let path = "fixtures/大文档测试2.md";
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("[bench] skip realfile: {e}");
            return;
        }
    };
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|window, cx: &mut App| {
            let ed = host.read(cx).ed.clone();
            // 顶部一个 ASCII 字节位置（行 1 的 `#` 后首个字母——`## C1 ·` 之前），
            // 替换成单字符制造一个撤销单元；基准改造前后同口径（旧整篇快照 vs 逆操作）。
            let mut at = 0usize;
            for _ in 0..4096 {
                if at >= text.len() {
                    break;
                }
                let c = text[at..].chars().next().unwrap();
                if c.is_ascii() && c.is_alphabetic() {
                    break;
                }
                at += c.len_utf8();
            }
            ed.update(cx, |e, _| e.set_caret(at));
            // 暖机：首帧 + 3 次 build_frame（首帧 Pass1/冷缓存不计）。
            let _w0 = build_frame(ed.read(cx), window, px(910.), (0., 1536.));
            for _ in 0..3 {
                build_frame(ed.read(cx), window, px(910.), (0., 1536.));
            }
            // 制造一个撤销单元：base 快照入栈，doc 变为 base+insert。
            ed.update(cx, |e, ecx| e.replace_range(at..at + 1, "z", ecx));
            build_frame(ed.read(cx), window, px(910.), (0., 1536.));
            crate::prof::dump("wy.undo.prewarm");
            let mut restore_undo = 0u128;
            let mut bf_undo = 0u128;
            let mut restore_redo = 0u128;
            let mut bf_redo = 0u128;
            let n = 10;
            for _ in 0..n {
                let t0 = std::time::Instant::now();
                ed.update(cx, |e, ecx| e.undo(ecx)); // 恢复 base
                restore_undo += t0.elapsed().as_micros();
                let t1 = std::time::Instant::now();
                build_frame(ed.read(cx), window, px(910.), (0., 1536.));
                bf_undo += t1.elapsed().as_micros();
                let t2 = std::time::Instant::now();
                ed.update(cx, |e, ecx| e.redo(ecx)); // 前向恢复 base+insert
                restore_redo += t2.elapsed().as_micros();
                let t3 = std::time::Instant::now();
                build_frame(ed.read(cx), window, px(910.), (0., 1536.));
                bf_redo += t3.elapsed().as_micros();
            }
            eprintln!(
                "[bench] undo: restore {:.3}ms + build_frame {:.3}ms = {:.3}ms/次; redo: restore {:.3}ms + build_frame {:.3}ms = {:.3}ms/次",
                restore_undo as f64 / n as f64 / 1000.0,
                bf_undo as f64 / n as f64 / 1000.0,
                (restore_undo + bf_undo) as f64 / n as f64 / 1000.0,
                restore_redo as f64 / n as f64 / 1000.0,
                bf_redo as f64 / n as f64 / 1000.0,
                (restore_redo + bf_redo) as f64 / n as f64 / 1000.0,
            );
            crate::prof::dump("wy.undo.current");
        });
}
#[gpui::test]
#[ignore]
fn bench_rw_realfile(cx: &mut TestAppContext) {
    use crate::markdown::wysiwyg::WyEditor;
    cx.update(crate::actions::bind_local_keymaps);
    let path = "fixtures/大文档测试2.md";
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("[bench] skip realfile: {e}");
            return;
        }
    };
    // 定位各类位置：围栏内部、表格行、数学行、普通文本行、文档中部。
    let find_after = |needle: &str, from: usize| -> Option<usize> {
        let rel = text[from..].find(needle)?;
        Some(from + rel)
    };
    // 第一个 ``` 之后的内容位置（代码块内）。
    let fence_open = find_after("```\n", 0)
        .map(|i| i + 4)
        .unwrap_or(text.len() / 2);
    let fence_mid = {
        let next = find_after("```\n", fence_open + 1).unwrap_or(text.len());
        let mid = fence_open + 10 * ((next - fence_open) / 21).max(1);
        if mid < next { mid } else { fence_open }
    };
    let table_row = find_after("\n|", 0)
        .map(|i| i + 2)
        .unwrap_or(text.len() / 2);
    let math_line = find_after("\n$$", 0)
        .map(|i| i + 3)
        .unwrap_or(text.len() / 2);
    let cb = |s: &str, i: usize| -> usize {
        let i = i.min(s.len());
        if i == 0 || s.is_char_boundary(i) {
            return i;
        }
        let mut j = i;
        let b = s.as_bytes();
        while j > 0 && (b[j] & 0b1100_0000) == 0b1000_0000 {
            j -= 1;
        }
        j
    };
    // 一个普通文本位置（避开 $ | # ` 前缀）。
    let plain_at = (0..100)
        .find_map(|k| {
            let raw = text.len() / 2 + k * 97;
            let i = cb(&text, raw.min(text.len()));
            let line = text[i..].lines().next().unwrap_or("");
            let at_line_start = i == 0 || text[..i].ends_with('\n');
            if at_line_start
                && !line.starts_with('#')
                && !line.starts_with('|')
                && !line.starts_with("```")
                && !line.starts_with(">")
                && !line.starts_with("- ")
                && !line.starts_with('$')
                && !line.contains('$')
                && !line.is_empty()
            {
                Some(i)
            } else {
                None
            }
        })
        .unwrap_or(cb(&text, text.len() / 2));

    eprintln!(
        "[bench] realfile len={} fence_mid={fence_mid} table_row={table_row} math_line={math_line} plain_at={plain_at}",
        text.len()
    );
    let case = |cx: &mut TestAppContext, name: &str, at: usize, ins_seq: &[&str]| {
        let (host, _vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&text));
            WyHost { ed }
        });
        let _ = cx.refresh();
        // 定位到 ASCII 字节：替换后仍是单字节 ASCII → 区间跨多次迭代恒在字符边界。
        let mut at2 = cb(&text, at.min(text.len().saturating_sub(1)));
        for _ in 0..4096 {
            if at2 >= text.len() {
                break;
            }
            let c = text[at2..].chars().next().unwrap();
            if c.is_ascii() {
                break;
            }
            at2 += c.len_utf8();
        }
        let ok = at2 < text.len()
            && text.is_char_boundary(at2)
            && text.is_char_boundary(at2 + 1)
            && text[at2..]
                .chars()
                .next()
                .map(|c| c.is_ascii())
                .unwrap_or(false);
        if !ok {
            eprintln!("[bench] realfile {name}: 跳过（4096 内无 ASCII）");
            return;
        }
        eprintln!("[bench] realfile {name}: ascii_at={at2}");
        // 预热：先打 2 键进稳态（首次 bf 含 cold Pass1/冷缓存、不代表逐键成本），
        // 计时与 prof 都只覆盖预热后的稳态段。
        let mut warm = |ins: &str| {
            cx.update(|cx| {
                host.update(cx, |w, cx| {
                    w.ed.update(cx, |ed2, ecx| ed2.replace_range(at2..at2 + 1, ins, ecx))
                });
            });
            let _ = cx.refresh();
        };
        warm("q");
        warm("w");
        // 预热后把 prof 已累计的暖机开销清掉，只统计稳态。
        crate::prof::dump(&format!("wy.realfile.{name}.warmup"));
        let mut t = 0u128;
        let n = ins_seq.len().max(1);
        for ins in ins_seq {
            let t0 = std::time::Instant::now();
            cx.update(|cx| {
                host.update(cx, |w, cx| {
                    w.ed.update(cx, |ed2, ecx| ed2.replace_range(at2..at2 + 1, ins, ecx))
                });
            });
            let _ = cx.refresh();
            t += t0.elapsed().as_millis();
        }
        eprintln!(
            "[bench] realfile {name}: {}ms ({:.1}ms/键 稳态) at={at2}",
            t,
            t as f64 / n as f64
        );
        crate::prof::dump(&format!("wy.realfile.{name}"));
    };
    case(
        cx,
        "plain-x",
        plain_at,
        &["x", "y", "x", "y", "x", "y", "x", "y"],
    );
    case(
        cx,
        "fence-x",
        fence_mid,
        &["x", "y", "x", "y", "x", "y", "x", "y"],
    );
    case(
        cx,
        "table-x",
        table_row,
        &["x", "y", "x", "y", "x", "y", "x", "y"],
    );
    case(
        cx,
        "math-$$-x",
        math_line,
        &["x", "y", "x", "y", "x", "y", "x", "y"],
    );
    case(cx, "enter-mid", text.len() / 2, &["\n", "\n", "\n", "\n"]);
}
#[gpui::test]
#[ignore]
fn bench_scroll_bands(cx: &mut TestAppContext) {
    // 真实文档滚动带重建基准：模拟 prepaint 的 512px 带量化，从顶部连续滚 24 档、
    // 每档 400px（≈每 1-2 档触发一次带内重建），统计每档 build_frame 耗时 + Pass2
    // 实际整形（缓存未命中）块数。KILO_PROF=1 时同时 dump 各阶段开销。
    use crate::markdown::wysiwyg::WyEditor;
    cx.update(crate::actions::bind_local_keymaps);
    let path = "fixtures/大文档测试2.md";
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("[bench] skip realfile: {e}");
            return;
        }
    };
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let width = px(980.);
    let vh = 800.0f32;
    let q = 512.0f32;
    let n = 24;
    let mut bands: Vec<(f32, f32)> = Vec::new();
    // 前段：文档上部（普通文本/标题/列表）；后段：~30k px 起的数学/表格密集区。
    for k in 0..n {
        let top = 400.0 * (k + 1) as f32;
        bands.push((
            (((top - vh) / q).floor() * q).max(0.),
            ((top + 2. * vh) / q).ceil() * q,
        ));
    }
    for k in 0..n {
        let top = 30_000.0 + 400.0 * (k + 1) as f32;
        bands.push((
            (((top - vh) / q).floor() * q).max(0.),
            ((top + 2. * vh) / q).ceil() * q,
        ));
    }
    let n = bands.len();
    // 阶段 A：首次滚过全部新带（每带含新进块整形）——真实滚动前沿成本。
    let mut t = 0.0f64;
    let mut reshapes = 0u64;
    for &band in &bands {
        let t0 = std::time::Instant::now();
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            let _ = build_frame(ed.read(cx), window, width, band);
        });
        t += t0.elapsed().as_secs_f64() * 1000.0;
        vcx.update(|_window, cx| {
            reshapes += host.read(cx).ed.read(cx).take_reshapes();
        });
    }
    eprintln!(
        "[bench] scroll-新带 {n} band-builds: {total:.2}ms ({avg:.3}ms/band, {resh} 次真实整形, {per:.1} 次/band)",
        total = t,
        avg = t / n as f64,
        resh = reshapes,
        per = reshapes as f64 / n as f64,
    );
    crate::prof::dump("wy.scroll.fresh");
    // 阶段 B：原样滚回（全部缓存命中、0 整形）——纯组装/折叠开销。
    let mut t2 = 0.0f64;
    let mut r2 = 0u64;
    for &band in bands.iter().rev() {
        let t0 = std::time::Instant::now();
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            let _ = build_frame(ed.read(cx), window, width, band);
        });
        t2 += t0.elapsed().as_secs_f64() * 1000.0;
        vcx.update(|_window, cx| {
            r2 += host.read(cx).ed.read(cx).take_reshapes();
        });
    }
    eprintln!(
        "[bench] scroll-全热 {n} band-builds: {total:.2}ms ({avg:.3}ms/band, {resh} 次真实整形, {per:.1} 次/band)",
        total = t2,
        avg = t2 / n as f64,
        resh = r2,
        per = r2 as f64 / n as f64,
    );
    crate::prof::dump("wy.scroll.warm");
}

/// 手动性能基准（`--ignored` + `--nocapture` 才跑）：**删除/Backspace** 逐键稳态成本。
/// 全走 `edit.rs::replace_range`（src/markdown/wysiwyg/edit.rs:342）唯一漏斗，拆两段：
/// ① 普通逐键 Backspace（非结构：单字符、行内、块几何不变）——6MB 纯文本、光标放正文
///    中部，预热 2 键后连续回退 N 键；输出 `ms/key`（含每键一帧 `refresh`），
///    `KILO_PROF=1` 时 prof::dump 拆 `wy.replace_range`/Pass1/Pass2 账。
/// ② 跨行删除（结构路径）：光标在行首 Backspace 删除前一个 `\n` → `cross_newline` →
///    structural（Pass1 三段式 + derived 整篇回退/增量判定），重复 M 次段落合并；
///    单独标注输出 `ms/次`，与 perf 对比的基线相同。单位：ms/key、ms/次。
#[gpui::test]
#[ignore]
fn bench_rw_delete_plain(cx: &mut TestAppContext) {
    use crate::markdown::wysiwyg::WyEditor;
    crate::prof::enable();
    cx.update(crate::actions::bind_local_keymaps);
    fn doc_plain() -> (String, usize) {
        let para = "alpha beta gamma delta epsilon zeta eta theta one two three four five six seven eight nine ten\n";
        let mut t = String::new();
        while t.len() < 6_000_000 {
            t.push_str(para);
        }
        (t, para.len())
    }
    let (text, line_len) = doc_plain();
    let (host, _vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&text));
        WyHost { ed }
    });
    let _ = cx.refresh();
    // ① 逐键 Backspace（正文中部；ASCII 恒在字符边界）。每键 = replace_range(at-1..at,"")
    // + 一帧 refresh；替换后 caret 自动回退 1 → at 跟着递减。
    let mut at = 3_000_000usize;
    let warm = |cx: &mut TestAppContext, at: &mut usize| {
        cx.update(|cx| {
            host.update(cx, |w, cx| {
                w.ed.update(cx, |e, ecx| {
                    e.set_caret(*at);
                    e.replace_range(*at - 1..*at, "", ecx);
                })
            })
        });
        let _ = cx.refresh();
        *at -= 1;
    };
    warm(cx, &mut at);
    warm(cx, &mut at);
    crate::prof::dump("wy.delete.plain.warmup");
    let mut t = 0u128;
    let n = 48usize;
    for _ in 0..n {
        let t0 = std::time::Instant::now();
        warm(cx, &mut at);
        t += t0.elapsed().as_millis();
    }
    eprintln!(
        "[bench] rw-delete plain 逐键Backspace: {t}ms ({:.1}ms/key 稳态, {n} 键)",
        t as f64 / n as f64
    );
    crate::prof::dump("wy.delete.plain.key");
    // ② 跨行删除（结构路径）：从 `at` 所在行起，逐次把光标放行首、Backspace 掉前一个
    // `\n` 合并段落（`cross_newline` → structural）。合并后行号 -1，用 line_start(line)
    // 定位下次行首（line>=1 保证前面必有 `\n`）。
    let mut line = (at / line_len).max(1);
    let mut t2 = 0u128;
    let m = 32usize;
    for _ in 0..m {
        let ls = cx.update(|cx| host.read(cx).ed.read(cx).line_start(line));
        let t0 = std::time::Instant::now();
        cx.update(|cx| {
            host.update(cx, |w, cx| {
                w.ed.update(cx, |e, ecx| {
                    e.set_caret(ls);
                    e.replace_range(ls - 1..ls, "", ecx);
                })
            })
        });
        let _ = cx.refresh();
        t2 += t0.elapsed().as_millis();
        line = line.saturating_sub(1);
    }
    eprintln!(
        "[bench] rw-delete plain 跨行合并(结构路径): {t2}ms ({:.1}ms/次 稳态, {m} 次)",
        t2 as f64 / m as f64
    );
    crate::prof::dump("wy.delete.plain.structural");
}

/// 手动性能基准（`--ignored` + `--nocapture` 才跑）：**删除/Backspace** 在代码块
/// （围栏内）的逐键稳态成本，与 `bench_rw_code` 对称；围栏内删字符是非结构路径
/// （块几何不变），输出 `ms/key` + `KILO_PROF=1` 时 prof::dump 拆账。单位：ms/key。
#[gpui::test]
#[ignore]
fn bench_rw_delete_code(cx: &mut TestAppContext) {
    use crate::markdown::wysiwyg::WyEditor;
    crate::prof::enable();
    cx.update(crate::actions::bind_local_keymaps);
    fn doc_code() -> String {
        let mut t = String::from("```\n");
        let line = "let alpha beta gamma delta epsilon zeta = compute_one_two_three_four_five_six_seven_eight_nine_ten();\n";
        while t.len() < 6_000_000 {
            t.push_str(line);
        }
        t.push_str("```\n");
        t
    }
    let (host, _vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&doc_code()));
        WyHost { ed }
    });
    let _ = cx.refresh();
    let mut at = 3_000_000usize;
    let warm = |cx: &mut TestAppContext, at: &mut usize| {
        cx.update(|cx| {
            host.update(cx, |w, cx| {
                w.ed.update(cx, |e, ecx| {
                    e.set_caret(*at);
                    e.replace_range(*at - 1..*at, "", ecx);
                })
            })
        });
        let _ = cx.refresh();
        *at -= 1;
    };
    warm(cx, &mut at);
    warm(cx, &mut at);
    crate::prof::dump("wy.delete.code.warmup");
    let mut t = 0u128;
    let n = 48usize;
    for _ in 0..n {
        let t0 = std::time::Instant::now();
        warm(cx, &mut at);
        t += t0.elapsed().as_millis();
    }
    eprintln!(
        "[bench] rw-delete code 逐键Backspace: {t}ms ({:.1}ms/键 稳态, {n} 键)",
        t as f64 / n as f64
    );
    crate::prof::dump("wy.delete.code.key");
}

/// 手动性能基准（`--ignored` + `--nocapture` 才跑）：**删除/Backspace** 在真实文档
/// `fixtures/大文档测试2.md` 上挑 5 个代表点、各删若干键的稳态成本，与
/// `bench_rw_realfile` 同一文档/同一命中点选择逻辑（输出格式一致，脚本可抓取）：
/// - plain-del / fence-del / table-del / math-$$-del：非结构逐键删除（固定 `at2`
///   向前删，留在同一 markup 构造内），预热 2 键后稳态 `ms/键` + prof::dump；
/// - merge-lines：结构路径（光标在行首 Backspace 合并上一行段落，重复 M 次），
///   单独标注输出 `ms/次`。
/// 单位：ms/键、ms/次。
#[gpui::test]
#[ignore]
fn bench_rw_delete_realfile(cx: &mut TestAppContext) {
    use crate::markdown::wysiwyg::WyEditor;
    crate::prof::enable();
    cx.update(crate::actions::bind_local_keymaps);
    let path = "fixtures/大文档测试2.md";
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("[bench] skip realfile: {e}");
            return;
        }
    };
    // 定位各类位置：围栏内部、表格行、数学行、普通文本行（与 bench_rw_realfile 相同）。
    let find_after = |needle: &str, from: usize| -> Option<usize> {
        let rel = text[from..].find(needle)?;
        Some(from + rel)
    };
    let fence_open = find_after("```\n", 0)
        .map(|i| i + 4)
        .unwrap_or(text.len() / 2);
    let fence_mid = {
        let next = find_after("```\n", fence_open + 1).unwrap_or(text.len());
        let mid = fence_open + 10 * ((next - fence_open) / 21).max(1);
        if mid < next { mid } else { fence_open }
    };
    let table_row = find_after("\n|", 0)
        .map(|i| i + 2)
        .unwrap_or(text.len() / 2);
    let math_line = find_after("\n$$", 0)
        .map(|i| i + 3)
        .unwrap_or(text.len() / 2);
    let cb = |s: &str, i: usize| -> usize {
        let i = i.min(s.len());
        if i == 0 || s.is_char_boundary(i) {
            return i;
        }
        let mut j = i;
        let b = s.as_bytes();
        while j > 0 && (b[j] & 0b1100_0000) == 0b1000_0000 {
            j -= 1;
        }
        j
    };
    // 一个普通文本行行首（避开 $ | # > - ` 前缀），也用作 merge-lines 的起始行。
    let plain_at = (0..100)
        .find_map(|k| {
            let raw = text.len() / 2 + k * 97;
            let i = cb(&text, raw.min(text.len()));
            let line = text[i..].lines().next().unwrap_or("");
            let at_line_start = i == 0 || text[..i].ends_with('\n');
            if at_line_start
                && !line.starts_with('#')
                && !line.starts_with('|')
                && !line.starts_with("```")
                && !line.starts_with(">")
                && !line.starts_with("- ")
                && !line.starts_with('$')
                && !line.contains('$')
                && !line.is_empty()
            {
                Some(i)
            } else {
                None
            }
        })
        .unwrap_or(cb(&text, text.len() / 2));

    eprintln!(
        "[bench] rw-delete realfile len={} fence_mid={fence_mid} table_row={table_row} math_line={math_line} plain_at={plain_at}",
        text.len()
    );
    // 非结构逐键删除 case：Backspace 语义（删光标前一字符）——含中文/多字节的真实文档
    // 上最稳妥：每键在**实时 doc** 的当前行串上定位前一字符起点（行串短，O(行宽)），恒在
    // 字符边界；到行首即停（再删就是跨行结构路径，交给下方 merge-lines 单独测）。起点 =
    // 模板 at2 往行内推 push 个字符（给足可回字符数）。预热 2 键后测稳态，prof dump 只
    // 覆盖稳态段。单位：ms/键。
    let advance = |s: &str, at: usize, k: usize| -> usize {
        let mut j = at.min(s.len());
        for _ in 0..k {
            match s[j..].chars().next() {
                Some(c) => j += c.len_utf8(),
                None => break,
            }
        }
        j
    };
    let case = |cx: &mut TestAppContext, name: &str, at: usize, n: usize, push: usize| {
        let (host, _vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&text));
            WyHost { ed }
        });
        let _ = cx.refresh();
        let at2 = cb(&text, at.min(text.len().saturating_sub(1)));
        let start0 = advance(&text, at2, push).min(text.len());
        if start0 == text.len() {
            eprintln!("[bench] rw-delete realfile {name}: 跳过（起点在文件尾）");
            return;
        }
        eprintln!("[bench] rw-delete realfile {name}: start0={start0}");
        // Backspace：删光标前一字符；到行首/文档头返回 false（本 case 结束）。
        let del = |cx: &mut TestAppContext, at: &mut usize| -> bool {
            let (line, col) = cx.update(|cx| host.read(cx).ed.read(cx).row_col(*at));
            if *at == 0 || col == 0 {
                return false;
            }
            let (ls, le) = cx.update(|cx| {
                let ed = host.read(cx).ed.read(cx);
                (ed.line_start(line), ed.line_end(line))
            });
            if le <= ls {
                return false;
            }
            let line_s =
                cx.update(|cx| host.read(cx).ed.read(cx).doc().slice_str(ls..le.max(ls)));
            let col = col.min(line_s.len());
            if col == 0 {
                return false;
            }
            let prev_col = line_s[..col]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
            let prev = ls + prev_col;
            if prev >= *at {
                return false;
            }
            cx.update(|cx| {
                host.update(cx, |w, cx| {
                    w.ed.update(cx, |ed2, ecx| {
                        ed2.set_caret(*at);
                        ed2.replace_range(prev..*at, "", ecx);
                    })
                });
            });
            let _ = cx.refresh();
            *at = prev;
            true
        };
        // 预热 2 键进稳态（首次 bf 冷缓存/Pass1 不代表逐键成本），dump 清掉暖机开销。
        let mut a = start0;
        for _ in 0..2 {
            if !del(cx, &mut a) {
                break;
            }
        }
        crate::prof::dump(&format!("wy.delete.realfile.{name}.warmup"));
        let mut t = 0u128;
        let mut k = 0usize;
        for _ in 0..n {
            let t0 = std::time::Instant::now();
            if !del(cx, &mut a) {
                break;
            }
            t += t0.elapsed().as_millis();
            k += 1;
        }
        eprintln!(
            "[bench] rw-delete realfile {name}: {t}ms ({:.1}ms/键 稳态, {k} 键) start0={start0}",
            t as f64 / (k.max(1)) as f64
        );
        crate::prof::dump(&format!("wy.delete.realfile.{name}"));
    };
    let n = 4usize; // 每点删 4 键，留在同一构造内（非结构）。
    let push = 6usize; // 起点往行内推 6 字符，给足 Backspace 可回字符数。
    case(cx, "plain-del", plain_at, n, push);
    case(cx, "fence-del", fence_mid, n, push);
    case(cx, "table-del", table_row, n, push);
    case(cx, "math-$$-del", math_line, n, push);
    // 结构删除（跨行合并）：在普通文本行首 Backspace 合并上一行（cross_newline →
    // structural 路径），重复 M 次段落合并；每次用 line_start(line) 定位当前行首。
    let (host_m, _vcx_m) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&text));
        WyHost { ed }
    });
    let _ = cx.refresh();
    let mut line = cx.update(|cx| host_m.read(cx).ed.read(cx).row_col(plain_at).0);
    let merge_once = |cx: &mut TestAppContext, line: &mut usize| {
        let ls = cx.update(|cx| host_m.read(cx).ed.read(cx).line_start(*line));
        cx.update(|cx| {
            host_m.update(cx, |w, cx| {
                w.ed.update(cx, |ed2, ecx| {
                    ed2.set_caret(ls);
                    ed2.replace_range(ls - 1..ls, "", ecx);
                })
            });
        });
        let _ = cx.refresh();
        *line = line.saturating_sub(1);
    };
    merge_once(cx, &mut line);
    crate::prof::dump("wy.delete.realfile.merge.warmup");
    let mut t2 = 0u128;
    let m = 6usize;
    for _ in 0..m {
        let t0 = std::time::Instant::now();
        merge_once(cx, &mut line);
        t2 += t0.elapsed().as_millis();
    }
    eprintln!(
        "[bench] rw-delete realfile merge-lines(结构路径): {t2}ms ({:.1}ms/次 稳态, {m} 次)",
        t2 as f64 / m as f64
    );
    crate::prof::dump("wy.delete.realfile.merge");
}

