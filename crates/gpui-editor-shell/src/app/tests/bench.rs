    /// 基准：打开 16 万行大文档的逐阶段耗时（读文件 / 编辑器装配 / 预览扫描 / 首帧渲染）。
    /// 用与拖入完全相同的 `open_file` 同步路径，定位"拖入大文档卡顿几秒"到底卡在哪。
    /// 运行：`cargo test --release --bench --ignored bench_open_large_md --nocapture`（见下）
    #[gpui::test]
    #[ignore]
    async fn bench_open_large_md(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        unsafe { std::env::set_var("MD_OPEN_DEBUG", "1") };
        // md：大文档异步打开。open_file 应立即返回（拖入不卡 UI），
        // 内容稍后在后台装配出来。
        let t0 = std::time::Instant::now();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.open_file(r"../../fixtures/大文档测试.md", window, cx);
            });
        });
        let sync_ms = t0.elapsed().as_millis();
        println!("【md 异步打开：open_file 同步返回】(拖入不卡 UI) {} ms", sync_ms);
        // 两段式打开：D=编辑区**内容**即时就绪（快段，≈几十 ms），A=预览就绪（慢段
        // 后台并行统计+scan_blocks 完成后）。B 语义沿用「编辑区挂载完成」。
        // 用「行数 + !editor_pending」作廉价就绪判据（大文档必然远超；不用每帧 value()
        // 物化 6MB，那会污染墙钟）。
        let mut d_ready = false;
        let mut a_ready = false;
        let mut d_ready_ms = 0usize;
        let mut a_ready_ms = 0usize;
        let mut b_ready_ms = 0usize;
        let mut frames = 0;
        loop {
            frames += 1;
            cx.run_until_parked();
            let (wy_loaded, editor_pending, editor_lines) = cx.update(|_, cx| {
                let a = app.read(cx);
                let i = a.tabs.len().saturating_sub(1);
                (
                    a.tabs[i]
                        .md_wy
                        .as_ref()
                        .map(|w| w.read(cx).is_loaded())
                        .unwrap_or(false),
                    a.tabs[i].editor_pending,
                    a.tabs[i].editor.read(cx).line_count(),
                )
            });
            if !d_ready && !editor_pending && editor_lines > 1000 {
                d_ready = true;
                d_ready_ms = t0.elapsed().as_millis() as usize;
            }
            if !a_ready && wy_loaded {
                a_ready = true;
                a_ready_ms = t0.elapsed().as_millis() as usize;
            }
            if d_ready && a_ready {
                b_ready_ms = t0.elapsed().as_millis() as usize;
                break;
            }
            if frames > 200 {
                break;
            }
        }
        println!("【D. 编辑区内容即时就绪】(两段式快段) {d_ready_ms} ms");
        println!("【A. 预览内容就绪】(慢段后台完成) {a_ready_ms} ms");
        println!("【B. 两者就绪】(总墙钟) {}", if b_ready_ms > 0 { format!("{b_ready_ms} ms") } else { "超时".to_string() });
        // 首帧真实渲染/布局（编辑器 DisplayMap + 预览 ListState + 高亮窗口）
        let tb = std::time::Instant::now();
        for i in 0..3 {
            let tf = std::time::Instant::now();
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            println!("【C. 挂载后第 {} 次 refresh】(真实布局) {} ms", i + 1, tf.elapsed().as_millis());
        }
        println!("【C. 前 3 次 refresh 合计】 {} ms", tb.elapsed().as_millis());

        // 对照：同内容的 .txt（非 md → 无预览/无 scan_blocks；大文件同样走后台流水线）
        let t_txt = std::time::Instant::now();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.open_file(r"../../fixtures/大文档测试.txt", window, cx);
            });
        });
        println!("【txt 打开：open_file 返回】(对照) {} ms", t_txt.elapsed().as_millis());
    }

    /// A2/A4 基准（ignored，release）：大文档异步打开 + 右栏 WyEditor 预览装配的墙钟拆解。
    /// 真实 `fixtures/大文档测试2.md`（6.4MB / ~14 万行）。A4 后 MD 走统一组件：后台只做
    /// 一份 `Doc::open`（不再 scan_blocks 喂 MarkdownPreview）。测同步返回（UI 不卡）→
    /// 编辑区内容可见（D）→ 右栏 WyEditor 预览装配（WY）。运行：
    /// `cargo test --release -- --ignored bench_a2_md_wy_open_large --nocapture`
    #[gpui::test]
    #[ignore]
    async fn bench_a2_md_wy_open_large(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1280.), px(800.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        unsafe { std::env::set_var("MD_OPEN_DEBUG", "1") };
        let p = r"../../fixtures/大文档测试2.md";
        assert!(std::path::Path::new(p).is_file(), "{p} 缺失");
        let t0 = std::time::Instant::now();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(p, window, cx));
        });
        println!("【同步返回】(UI 不卡) {} ms", t0.elapsed().as_millis());
        let mut d_ready = false;
        let mut wy_ready = false;
        let mut d_ms = 0usize;
        let mut wy_ms = 0usize;
        let mut frames = 0;
        loop {
            frames += 1;
            cx.run_until_parked();
            let (editor_lines, pending, wy_loaded) = cx.update(|_, cx| {
                let a = app.read(cx);
                let i = a.tabs.len().saturating_sub(1);
                (
                    a.tabs[i].editor.read(cx).line_count(),
                    a.tabs[i].editor_pending,
                    a.tabs[i]
                        .md_wy
                        .as_ref()
                        .map(|w| w.read(cx).is_loaded())
                        .unwrap_or(false),
                )
            });
            if !d_ready && !pending && editor_lines > 1000 {
                d_ready = true;
                d_ms = t0.elapsed().as_millis() as usize;
            }
            if !wy_ready && wy_loaded {
                wy_ready = true;
                wy_ms = t0.elapsed().as_millis() as usize;
            }
            if d_ready && (wy_ready || frames > 90) {
                break;
            }
            if frames > 400 {
                break;
            }
        }
        println!("【D. 编辑区内容可见】 {d_ms} ms");
        println!(
            "【WY. 右栏 WyEditor 预览装配完成】 {} ms",
            if wy_ms > 0 {
                format!("{wy_ms} ms")
            } else {
                "未装配".to_string()
            }
        );
        // P4-2：MD 预览 = 唯一统一 WyEditor（`MarkdownPreview` 已删），无第二份扫描可量。
        let wy_blocks = cx.update(|_, cx| {
            let a = app.read(cx);
            let i = a.tabs.len().saturating_sub(1);
            a.tabs[i]
                .md_wy
                .as_ref()
                .map(|w| w.read(cx).frame_total().map_or(0., |t| t) as usize)
                .unwrap_or(0)
        });
        println!("【WyEditor 预览帧总高】 {wy_blocks} px（可对照 □ 量程）");
        // 首帧真实布局
        let tb = std::time::Instant::now();
        for i in 0..3 {
            let tf = std::time::Instant::now();
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            println!(
                "【C. 挂载后第 {} 次 refresh】(真实布局) {} ms",
                i + 1,
                tf.elapsed().as_millis()
            );
        }
        println!("【C. 前 3 次 refresh 合计】 {} ms", tb.elapsed().as_millis());
    }

    /// A3 基准（ignored，release）：大文档「增量桥」的真实成本拆解（真实 6.4MB / 139k 行）：
    /// 1. 桥模型成本 = 直接对装配好的 WyEditor 逐键 `preview_apply_edit`（= undo/redo 的
    ///    `apply_raw_edit` 增量管线：Doc::edit + Pass1 + derived + 单块作废），应亚毫秒；
    /// 2. 真实逐键路径（Paste → insert_edit → Edited → 桥）200 键后预览必须与编辑区
    ///    逐字节一致（正确性；墙钟含旧的 md_preview 后台重索引 headless 等待，不作断言）。
    /// 运行：`cargo test --release -- --ignored bench_a3_incremental_bridge_per_key --nocapture`
    #[gpui::test]
    #[ignore]
    async fn bench_a3_incremental_bridge_per_key(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1280.), px(800.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let p = r"../../fixtures/大文档测试2.md";
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(p, window, cx));
        });
        let mut wy_ready = false;
        for _ in 0..400 {
            cx.run_until_parked();
            wy_ready = cx.update(|_, cx| {
                let a = app.read(cx);
                let i = a.tabs.len().saturating_sub(1);
                // Step2：提前整篇解析可先于编辑区收尾装好预览 → 逐键一致性（m==e）必须
                // 等**打开流水线整体完成**（open_finish_count>=1 ⇒ 编辑区已全文落定）。
                a.open_finish_count >= 1
                    && a.tabs[i]
                        .md_wy
                        .as_ref()
                        .map(|w| w.read(cx).is_loaded())
                        .unwrap_or(false)
            });
            if wy_ready {
                break;
            }
        }
        assert!(wy_ready, "md_wy 应装配完成（大文档异步 install）");
        const N: usize = 200;
        cx.update(|window, cx| {
            let focus = app.read(cx).editor_focus_handle(cx);
            window.focus(&focus, cx);
        });
        cx.write_to_clipboard(gpui::ClipboardItem::new_string("啊".to_string()));
        let t_real = std::time::Instant::now();
        for _ in 0..N {
            cx.dispatch_action(Paste);
            // 只推到桥应用前后（不等后台 md_preview 重索引——那会混入旧管线头等待）。
            for _ in 0..3 {
                cx.run_until_parked();
            }
        }
        let real_total_ms = t_real.elapsed().as_millis();
        // 真实逐键路径一致性：预览必须恒等于编辑区（增量桥正确性）。
        let (m, e) = cx.update(|_, cx| {
            let a = app.read(cx);
            let t = &a.tabs[a.active_tab];
            (
                t.md_wy
                    .as_ref()
                    .map(|w| w.read(cx).text().to_string())
                    .unwrap_or_default(),
                t.editor.read(cx).value().to_string(),
            )
        });
        assert_eq!(m, e, "{N} 键后预览应仍与编辑区逐字节一致");
        // 隔离测量桥模型成本（独立 WyEditor 副本，绝不污染已对齐的 md_wy）：
        // ⚠️ 循环内不读 `text()`（O(n) 物化）；用 O(1) `len()` 取初始长后自行推进。
        let scratch = cx.update(|window, cx| {
            let rope = app.read(cx).tabs[app.read(cx).active_tab]
                .editor
                .read(cx)
                .rope()
                .clone();
            let s = cx.new(|cx| WyEditor::new(window, cx));
            s.update(cx, |w, _cx| {
                w.install_open(crate::markdown::wysiwyg::WyEditor::open_from_rope(rope))
            });
            s
        });
        let start_len = cx.update(|_w, cx| scratch.read(cx).len());
        let model_ms = cx
            .update(|_w, cx| {
                let t = std::time::Instant::now();
                let mut at = start_len;
                for _ in 0..200 {
                    scratch.update(cx, |w, cx| w.preview_apply_edit(at..at, "啊", cx));
                    at += 3; // "啊" 3 字节
                }
                t.elapsed()
            })
            .as_millis();
        println!(
            "【桥模型成本】 200 次 preview_apply_edit 合计 {model_ms} ms ≈ {:.3} ms/键",
            model_ms as f64 / 200.0
        );
        println!(
            "【真实逐键路径】 {N} 键合计 {real_total_ms} ms（含编辑器/帧/headless 泵；
            A4 已移除每键 md_preview 后台重索引，不作断言）"
        );
        assert!(
            model_ms as f64 / 200.0 < 2.0,
            "桥模型成本应亚毫秒/键（6.4MB/139k 行，实际 {model_ms} ms/200 键）"
        );
    }

    /// 基准：用编辑器本体追加同一段文本 1000 次所需耗时。
    /// 在编辑器里打开目标文件（与真实使用完全相同的 `open_file` 路径），
    /// 再通过 gpui-component 编辑器的 `insert` 接口连续插入同一段样本文本 1000 次，
    /// 统计总耗时。运行：
    /// `cargo test --release -- --ignored --nocapture bench_insert_1000_times`
    #[gpui::test]
    #[ignore]
    async fn bench_insert_1000_times(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 每次插入的“同样文本”（多行样本，贴近真实编辑内容）
        const SAMPLE: &str = "### 性能基准测试\n性能测试文本 0123456789 abcdefghijklmnopqrstuvwxyz 中文内容测试。\n```text\ncode block line\n```\n";
        const N: usize = 1000;

        // 目标文件可由环境变量 BENCH_INSERT_FILES 覆盖（分号分隔），默认三份样本
        let files: Vec<String> = std::env::var("BENCH_INSERT_FILES")
            .map(|v| {
                v.split(';')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_else(|_| {
                vec![
                    r"../../fixtures/大文档测试.md".to_string(),
                    r"../../fixtures/SKILL.md".to_string(),
                    r"../../fixtures/SKILL.txt".to_string(),
                ]
            });

        for path in &files {
            let file_name = std::path::Path::new(path)
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.to_string());
            if !std::path::Path::new(path).is_file() {
                println!("跳过（文件不存在）：{file_name}");
                continue;
            }

            // 1) 打开文件（md 大文档走异步打开，小文档同步打开）
            cx.update(|window, cx| {
                app.update(cx, |app, cx| app.open_file(path, window, cx));
            });
            // 2) 等待编辑器挂载完成（editor_pending 复位 = 编辑区已升起）
            let t_open = std::time::Instant::now();
            let mut frames = 0;
            loop {
                cx.run_until_parked();
                let pending = cx.update(|_, cx| {
                    let a = app.read(cx);
                    a.tabs.last().map(|t| t.editor_pending).unwrap_or(true)
                });
                if !pending {
                    break;
                }
                frames += 1;
                if frames > 400 {
                    panic!("打开超时：{file_name}");
                }
            }
            let open_ms = t_open.elapsed();

            // 3) 聚焦当前编辑区
            let (editor, focus) = cx.update(|_, cx| {
                let a = app.read(cx);
                let idx = a.active_tab;
                (
                    a.tabs[idx].editor.clone(),
                    a.tabs[idx].editor.read(cx).focus_handle(),
                )
            });
            cx.update(|window, cx| window.focus(&focus, cx));

            // 4) 连续插入同样文本 N 次并计时（编辑器操作本体，不含渲染帧）
            let before = cx.update(|_, cx| editor.read(cx).value().len());
            let t_insert = std::time::Instant::now();
            for _ in 0..N {
                cx.update(|_, cx| {
                    editor.update(cx, |ed, cx| ed.insert_text(SAMPLE, cx));
                });
            }
            let dt = t_insert.elapsed();
            let after = cx.update(|_, cx| editor.read(cx).value().len());
            let grew = after.saturating_sub(before);
            let expect = N * SAMPLE.len();
            assert_eq!(
                grew, expect,
                "插入 {N} 次后应恰好新增 {expect} B，实际 {grew} B"
            );
            println!(
                "【{file_name}】打开(异步装配) {} ms | {} 次 insert 耗时 {:.1} ms（单次平均 {:.2} ms），长度 {} -> {}（+{} B）",
                open_ms.as_millis(),
                N,
                dt.as_secs_f64() * 1000.0,
                dt.as_secs_f64() * 1000.0 / N as f64,
                before,
                after,
                grew,
            );
        }
    }

    /// 进程内存探针（Windows）：`PagefileUsage`（私有提交字节）= 本进程真实分配量，
    /// `WorkingSetSize`（工作集）= 物理驻留。二者均可反映内存峰值。
    #[cfg(windows)]
    mod mem_probe {
        #[repr(C)]
        pub struct ProcessMemoryCounters {
            pub cb: u32,
            pub page_fault_count: u32,
            pub peak_working_set_size: usize,
            pub working_set_size: usize,
            pub quota_peak_paged_pool_usage: usize,
            pub quota_paged_pool_usage: usize,
            pub quota_peak_nonpaged_pool_usage: usize,
            pub quota_nonpaged_pool_usage: usize,
            pub pagefile_usage: usize,
            pub peak_pagefile_usage: usize,
        }
        unsafe extern "system" {
            fn GetCurrentProcess() -> *mut std::ffi::c_void;
            #[allow(clashing_extern_declarations)]
            fn K32GetProcessMemoryInfo(
                process: *mut std::ffi::c_void,
                counter: *mut ProcessMemoryCounters,
                cb: u32,
            ) -> i32;
        }
        pub fn counters() -> (u64, u64) {
            unsafe {
                let mut c: ProcessMemoryCounters = std::mem::zeroed();
                c.cb = std::mem::size_of::<ProcessMemoryCounters>() as u32;
                if K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) != 0 {
                    (c.pagefile_usage as u64, c.working_set_size as u64)
                } else {
                    (0, 0)
                }
            }
        }
    }

    /// 流式数 `\n` 字节 + 尾换行幻影，得 ropey `lines_len()` 口径的预期行数（不经
    /// 常驻整篇拷贝；memchr SIMD 扫 128MB ≈ 百毫秒级）。
    fn expected_rope_lines(path: &str) -> usize {
        use std::io::Read as _;
        let mut f = std::fs::File::open(path).expect("基准文件应存在");
        let mut buf = vec![0u8; 1 << 20];
        let mut nl = 0usize;
        let mut last = 0u8;
        loop {
            let n = f.read(&mut buf).expect("读基准文件");
            if n == 0 {
                break;
            }
            nl += memchr::memchr_iter(b'\n', &buf[..n]).count();
            last = buf[n - 1];
        }
        nl + usize::from(last == b'\n')
    }

    /// 基准：打开大文档（默认 `fixtures/大文档测试126MB.txt`，可用 `BENCH_OPEN_FILE`
    /// 覆盖）的分阶段墙钟 + 进程私有内存峰值。
    /// - 「首屏可见」= 编辑器首次出现内容（旧：整篇读完一次性挂载才非空；
    ///   新：分块读盘首块 256KB 即挂）。
    /// - 「全文就绪」= 行数达到全文行数（旧：快段整篇装配；新：末块追加完成）。
    /// - 「流水线结束」= `open_finish_count>=1`（旧：还要全量行宽统计 ≈7s；
    ///   新：EOF 后即刻）。
    /// - 内存峰值 = 打开窗口内进程私有提交字节增量（1ms 采样；旧路径读盘期有
    ///   「读盘 String + 归一化 String + Rope」≈2× 文件，新路径峰值≈1× 文件）。
    /// 运行：`cargo test --release --bin gpui-editor bench_open_large_txt_progressive -- \
    ///        --ignored --nocapture`（debug 亦可，比值一致、绝对值放大）。
    #[gpui::test]
    #[ignore]
    async fn bench_open_large_txt_progressive(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1200.), px(800.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let path = std::env::var("BENCH_OPEN_FILE").unwrap_or_else(|_| {
            r"../../fixtures/大文档测试126MB.txt".to_string()
        });
        if !std::path::Path::new(&path).is_file() {
            // 资产缺失：现场生成一份 ~120MB LF 文本（保证可复现基准）。
            let line = "x".repeat(80);
            let mut out = std::fs::File::create(&path).expect("创建基准文件");
            use std::io::Write as _;
            for i in 0..1_400_000usize {
                let _ = writeln!(out, "{line} {i:010}");
            }
            drop(out);
        }
        let expected_lines = expected_rope_lines(&path);
        let (base_pf, base_ws) = {
            #[cfg(windows)]
            {
                mem_probe::counters()
            }
            #[cfg(not(windows))]
            {
                (0, 0)
            }
        };

        // 看门狗：打开期间 1ms 采样私有内存峰值（读盘瞬间的瞬态高峰也能抓到）。
        use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
        let peak_pf = std::sync::Arc::new(AtomicU64::new(base_pf));
        let peak_ws = std::sync::Arc::new(AtomicU64::new(base_ws));
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        {
            let (pp, pw, st) = (peak_pf.clone(), peak_ws.clone(), stop.clone());
            std::thread::spawn(move || {
                while !st.load(Ordering::Relaxed) {
                    #[cfg(windows)]
                    {
                        let (pf, ws) = mem_probe::counters();
                        pp.fetch_max(pf, Ordering::Relaxed);
                        pw.fetch_max(ws, Ordering::Relaxed);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            });
        }

        let t0 = std::time::Instant::now();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(&path, window, cx));
        });
        let sync_ms = t0.elapsed().as_secs_f64() * 1000.0;

        let mut t_first_ms = f64::NAN;
        let mut t_full_ms = f64::NAN;
        let mut t_done_ms = f64::NAN;
        let mut ticks = 0usize;
        loop {
            if !cx.cx.executor().tick() {
                break;
            }
            ticks += 1;
            let (lc, fin) = cx.update(|_, cx| {
                let a = app.read(cx);
                let i = a.tabs.len().saturating_sub(1);
                (
                    a.tabs[i].editor.read(cx).line_count(),
                    a.open_finish_count,
                )
            });
            let now = t0.elapsed().as_secs_f64() * 1000.0;
            if t_first_ms.is_nan() && lc > 1 {
                t_first_ms = now;
            }
            if t_full_ms.is_nan() && lc >= expected_lines {
                t_full_ms = now;
            }
            if t_done_ms.is_nan() && fin >= 1 {
                t_done_ms = now;
            }
            if !t_first_ms.is_nan() && !t_done_ms.is_nan() {
                break;
            }
            if ticks > 400_000 {
                break;
            }
        }
        stop.store(true, Ordering::Relaxed);

        // 最终字节级一致性（一次性物化比较；编辑器统一 LF，与读取后归一化对比）。
        let ok = cx.update(|_, cx| {
            let a = app.read(cx);
            let i = a.tabs.len().saturating_sub(1);
            let expected = std::fs::read_to_string(&path)
                .unwrap()
                .replace("\r\n", "\n")
                .replace('\r', "\n");
            a.tabs[i].editor.read(cx).value() == expected
        });
        let last_lc = cx.update(|_, cx| {
            let a = app.read(cx);
            let i = a.tabs.len().saturating_sub(1);
            a.tabs[i].editor.read(cx).line_count()
        });
        let d_pf = peak_pf.load(Ordering::Relaxed).saturating_sub(base_pf);
        let d_ws = peak_ws.load(Ordering::Relaxed).saturating_sub(base_ws);

        println!("==== 打开大文档基准：{} ====", path);
        println!(
            "  文件 {} MB / {} 行；base 私有提交 {:.1} MB / 工作集 {:.1} MB",
            std::fs::metadata(&path).map(|m| m.len() / 1024 / 1024).unwrap_or(0),
            expected_lines,
            base_pf as f64 / 1048576.0,
            base_ws as f64 / 1048576.0,
        );
        println!(
            "  open_file 同步返回: {:.1} ms（不卡 UI）",
            sync_ms
        );
        println!(
            "  首屏可见(T_first):        {:.1} ms",
            t_first_ms
        );
        println!(
            "  全文装配(T_full_content): {:.1} ms",
            t_full_ms
        );
        println!(
            "  流水线结束(T_done):       {:.1} ms",
            t_done_ms
        );
        println!(
            "  峰值内存增量: 私有提交 {:.1} MB / 工作集 {:.1} MB",
            d_pf as f64 / 1048576.0,
            d_ws as f64 / 1048576.0
        );
        println!(
            "  最终: {} 行 / 字节一致={ok} / ticks={ticks}",
            last_lc
        );
    }

    /// 基准：ropey 全文构建「一次性 `Rope::from`」 vs「1MB 分块末尾 `insert`」——
    /// 量化新管线把构建搬到逐块追加（<主线程每块 O(块)>）相对旧一次性构建的开销。
    /// 运行：`cargo test --release --bin gpui-editor bench_rope_grow_strategies -- \
    ///        --ignored --nocapture`
    #[gpui::test]
    #[ignore]
    async fn bench_rope_grow_strategies(cx: &mut TestAppContext) {
        init_test(cx);
        // ~33MB 全文（3300 行 × 10KB），可比又不会跑太久。
        let line = "x".repeat(10_000);
        let full = {
            let mut s = String::with_capacity(line.len() * 3_300 + 1);
            for _ in 0..3_300 {
                s.push_str(&line);
                s.push('\n');
            }
            s
        };

        let t0 = std::time::Instant::now();
        let rope_oneshot = ropey::Rope::from(full.as_str());
        let ms_oneshot = t0.elapsed().as_secs_f64() * 1000.0;

        let t0 = std::time::Instant::now();
        let mut rope_incr = ropey::Rope::new();
        let chunk = 1 << 20;
        let mut i = 0usize;
        while i < full.len() {
            let e = full.len().min(i + chunk);
            rope_incr.insert(rope_incr.len(), &full[i..e]);
            i = e;
        }
        let ms_incr = t0.elapsed().as_secs_f64() * 1000.0;

        let n_chunks = full.len().div_ceil(chunk);
        assert_eq!(rope_oneshot.len(), rope_incr.len());
        println!(
            "==== ropey 构建基准（{} MB / {} 块）====",
            full.len() / 1024 / 1024,
            n_chunks
        );
        println!("  Rope::from 一次性:   {ms_oneshot:.2} ms");
        println!("  1MB×{n_chunks} 分块追加: {ms_incr:.2} ms（增量开销 = {:.2} ms，{:.1}%）",
            ms_incr - ms_oneshot,
            if ms_oneshot > 0. { (ms_incr - ms_oneshot) / ms_oneshot * 100. } else { 0. });
    }

    /// 基准（debug 体感）：大文档开着 Markdown 预览时，一次按键的 UI 线程成本拆解
    /// （insert 编辑 vs 帧渲染/布局），并对比「预览开/关」以定位预览在每帧里的占比。
    #[gpui::test]
    #[ignore]
    async fn bench_keystroke_frame_cost(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1200.), px(800.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(r"../../fixtures/大文档测试.md", window, cx));
        });
        let mut frames = 0;
        loop {
            cx.run_until_parked();
            let pending =
                cx.update(|_, cx| app.read(cx).tabs.last().map(|t| t.editor_pending).unwrap_or(true));
            if !pending {
                break;
            }
            frames += 1;
            if frames > 400 {
                panic!("打开超时");
            }
        }
        let editor = cx.update(|_, cx| app.read(cx).tabs.last().unwrap().editor.clone());
        let sample = "x";
        // 预热：预览窗口物化 + 行高提示收敛
        for _ in 0..3 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }

        // 预览开：insert / 帧渲染 各自耗时
        {
            let mut insert_tot = 0.0f64;
            let mut paint_tot = 0.0f64;
            let rep = 5;
            for i in 0..rep {
                let t0 = std::time::Instant::now();
                cx.update(|_w, cx| editor.update(cx, |e, cx| e.insert_text(sample, cx)));
                let t1 = std::time::Instant::now();
                cx.run_until_parked();
                let _ = cx.cx.refresh();
                cx.run_until_parked();
                let t2 = std::time::Instant::now();
                let ins = t1.duration_since(t0).as_secs_f64() * 1000.0;
                let paint = t2.duration_since(t1).as_secs_f64() * 1000.0;
                insert_tot += ins;
                paint_tot += paint;
                println!("  [预览开] iter {i}: insert={ins:.2} ms, frame={paint:.2} ms");
            }
            println!(
                "【预览开】insert={:.2} ms + 帧渲染={:.2} ms = {:.2} ms/键",
                insert_tot / rep as f64,
                paint_tot / rep as f64,
                (insert_tot + paint_tot) / rep as f64,
            );
        }
        // 关预览后再测一次
        cx.update(|window, cx| {
            app.update(cx, |a, cx| a.toggle_preview_for_tab(a.active_tab, window, cx));
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        {
            let mut insert_tot = 0.0f64;
            let mut paint_tot = 0.0f64;
            let rep = 5;
            for _ in 0..rep {
                let t0 = std::time::Instant::now();
                cx.update(|_w, cx| editor.update(cx, |e, cx| e.insert_text(sample, cx)));
                let t1 = std::time::Instant::now();
                cx.run_until_parked();
                let _ = cx.cx.refresh();
                cx.run_until_parked();
                let t2 = std::time::Instant::now();
                insert_tot += t1.duration_since(t0).as_secs_f64() * 1000.0;
                paint_tot += t2.duration_since(t1).as_secs_f64() * 1000.0;
            }
            println!(
                "【预览关】insert={:.2} ms + 帧渲染={:.2} ms = {:.2} ms/键",
                insert_tot / rep as f64,
                paint_tot / rep as f64,
                (insert_tot + paint_tot) / rep as f64,
            );
        }
    }

    /// 基准（debug，KILO_PROF=1 时为默认关闭内部拆解）：模拟真实编辑大文件的
    /// 「连续输入 → 停顿（触发预览增量同步）」节奏，逐键测插入与渲染停驻耗时，
    /// 并用 [`crate::prof`] 输出各热点（tab_change / hl_update / hl_styles / md_render /
    /// md_ensure_window）的累计拆解。
    #[gpui::test]
    #[ignore]
    async fn bench_realistic_large_file_editing(cx: &mut TestAppContext) {
        use std::time::{Duration, Instant};
        crate::prof::enable();
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1400.), px(900.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(r"../../fixtures/大文档测试.md", window, cx));
        });
        let mut frames = 0;
        loop {
            cx.run_until_parked();
            let pending = cx
                .update(|_, cx| app.read(cx).tabs.last().map(|t| t.editor_pending).unwrap_or(true));
            if !pending {
                break;
            }
            frames += 1;
            if frames > 600 {
                panic!("打开超时");
            }
        }
        let editor = cx.update(|_, cx| app.read(cx).tabs.last().unwrap().editor.clone());
        // 预热：预览窗口物化 + 行高提示收敛
        for _ in 0..5 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        crate::prof::dump("开篇预热后");
        let sample = "性能耗时代码行 performance line 1234567890 更多中文输入继续。\n";
        // —— A. 光标在文档开头连续输入 ~8 键（每键都渲染一帧）——
        let mut ins: Vec<f64> = Vec::new();
        let mut park: Vec<f64> = Vec::new();
        for _ in 0..8 {
            let t = Instant::now();
            cx.update(|_w, cx| editor.update(cx, |e, cx| e.insert_text(sample, cx)));
            let t1 = Instant::now();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
            let t2 = Instant::now();
            ins.push(t1.duration_since(t).as_secs_f64() * 1000.0);
            park.push(t2.duration_since(t1).as_secs_f64() * 1000.0);
        }
        crate::prof::dump("A. 开头连续输入8键");
        println!(
            "A. 开头连续输入：insert 均值 {:.2} ms，渲染停驻均值 {:.2} ms",
            ins.iter().sum::<f64>() / ins.len() as f64,
            park.iter().sum::<f64>() / park.len() as f64,
        );
        // 停顿 → 触发预览增量同步（真实应用里用户打字停顿处）
        cx.executor().advance_clock(Duration::from_millis(300));
        let t = Instant::now();
        for _ in 0..80 {
            cx.run_until_parked();
        }
        println!("A. 停顿+预览同步 停驻 {:.2} ms", t.elapsed().as_secs_f64() * 1000.0);
        crate::prof::dump("A. 停顿同步后");

        // —— B. 光标移到文档中部继续输入 ——
        let total = cx.update(|_, cx| editor.read(cx).value().len());
        let mid = (total / 2).min(total);
        cx.update(|_, cx| {
            editor.update(cx, |e, cx| e.set_selection_range(mid..mid, cx));
        });
        for _ in 0..8 {
            let t = Instant::now();
            cx.update(|_w, cx| editor.update(cx, |e, cx| e.insert_text(sample, cx)));
            let t1 = Instant::now();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
            let t2 = Instant::now();
            ins.push(t1.duration_since(t).as_secs_f64() * 1000.0);
            park.push(t2.duration_since(t1).as_secs_f64() * 1000.0);
        }
        crate::prof::dump("B. 中部连续输入8键");
        println!(
            "B. 中部连续输入：insert 均值 {:.2} ms，渲染停驻均值 {:.2} ms",
            ins.iter().rev().take(8).sum::<f64>() / 8.0,
            park.iter().rev().take(8).sum::<f64>() / 8.0,
        );
        cx.executor().advance_clock(Duration::from_millis(300));
        let t = Instant::now();
        for _ in 0..80 {
            cx.run_until_parked();
        }
        println!("B. 停顿+预览同步 停驻 {:.2} ms", t.elapsed().as_secs_f64() * 1000.0);
        crate::prof::dump("B. 停顿同步后（完成）");

        // —— C. 探针：裸 ropey Rope.insert vs 编辑器 insert，分离引擎/回调 ——
        {
            use ropey::Rope;
            let text = cx.update(|_, cx| editor.read(cx).value().to_string());
            let mid = text.len() / 2;
            let mut rope = Rope::from(text.as_str());
            let t0 = Instant::now();
            for _ in 0..50 {
                rope.insert(mid.min(rope.len()), "x");
            }
            let rope_ms = t0.elapsed().as_secs_f64() * 1000.0 / 50.0;
            let t1 = Instant::now();
            for _ in 0..50 {
                cx.update(|_w, cx| editor.update(cx, |e, cx| e.insert_text("x", cx)));
            }
            let ins_ms = t1.elapsed().as_secs_f64() * 1000.0 / 50.0;
            println!(
                "C. 探针：裸 ropey Rope(6MB).insert 中段 = {rope_ms:.2} ms/次；编辑器 insert（含事件/订阅） = {ins_ms:.2} ms/次"
            );
            crate::prof::dump("C. 探针后");
        }
    }

    /// 基准：编辑器本体（左侧纯文本 CodeEditor）连续删除 1000 次字符的耗时，与
    /// `bench_insert_1000_times` 对称。删除漏斗说明：`CodeEditor::backspace` /
    /// `delete_forward` 是私有方法（src/editor/view/mod.rs:2236，内部都走
    /// `insert_edit(…, EditIntent::Backspace/Delete)`），测试拿不到；这里用等价公开
    /// 漏斗复现同一 delete 成本——`set_selection_range(at..at+1)` + `insert_text("")`
    /// （insert_text 内部即 `insert_edit(self.selection.range(), text, …)`：逐键单字符
    /// 删除、撤销合并走同一主链）。打开真实文件后从正文中部向前逐字符删 N 次，输出
    /// 总耗时 + ms/次（单位：ms/次），`KILO_PROF=1` 时 `crate::prof::dump` 拆账。
    /// 运行：`cargo test --release -- --ignored --nocapture bench_editor_delete_1000_times`
    #[gpui::test]
    #[ignore]
    async fn bench_editor_delete_1000_times(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 目标文件继承 bench_insert_1000_times 的环境变量覆盖约定；默认三份样本。
        let files: Vec<String> = std::env::var("BENCH_INSERT_FILES")
            .map(|v| {
                v.split(';')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_else(|_| {
                vec![
                    r"../../fixtures/大文档测试.md".to_string(),
                    r"../../fixtures/SKILL.md".to_string(),
                    r"../../fixtures/SKILL.txt".to_string(),
                ]
            });
        const N: usize = 1000;
        for path in &files {
            let file_name = std::path::Path::new(path)
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.to_string());
            if !std::path::Path::new(path).is_file() {
                println!("跳过（文件不存在）：{file_name}");
                continue;
            }
            // 1) 打开文件（md 大文档走异步打开，小文档同步打开）
            cx.update(|window, cx| {
                app.update(cx, |app, cx| app.open_file(path, window, cx));
            });
            // 2) 等待编辑器挂载完成（editor_pending 复位 = 编辑区已升起）
            let t_open = std::time::Instant::now();
            let mut frames = 0;
            loop {
                cx.run_until_parked();
                let pending = cx.update(|_, cx| {
                    let a = app.read(cx);
                    a.tabs.last().map(|t| t.editor_pending).unwrap_or(true)
                });
                if !pending {
                    break;
                }
                frames += 1;
                if frames > 400 {
                    panic!("打开超时：{file_name}");
                }
            }
            let open_ms = t_open.elapsed();
            // 3) 取编辑器实体；光标放正文中部，向前逐字符删 N 次（每次删 1 字符）。
            let editor = cx.update(|_, cx| {
                let a = app.read(cx);
                a.tabs[a.active_tab].editor.clone()
            });
            let before = cx.update(|_, cx| editor.read(cx).value().len());
            if before < 2 {
                println!("【{file_name}】过短跳过（len={before}）");
                continue;
            }
            let n = N.min(before - 1).max(1);
            let before_chars = cx.update(|_, cx| editor.read(cx).char_count());
            crate::prof::enable();
            crate::prof::dump("editor-delete.warmup");
            // 逐字符前删 = 编辑器 `delete_forward` 同款语义：光标 `at` 固定（边界，先经
            // char_left 钳正，避免 before/2 劈开多字节字符），每键删 `[at, char_right(at))`
            // 一个字符并重新取边界；删除后文档左移，固定 `at` 即连续删后继字符——
            // 等价真 Delete 键的 `insert_edit(head..char_right, "", Delete)`。
            let at = cx.update(|_, cx| {
                let e = editor.read(cx);
                crate::editor::movement::char_left(e.text(), (before / 2).min(e.text().len()))
            });
            let mut deleted = 0usize;
            let t_del = std::time::Instant::now();
            for _ in 0..n {
                let done = cx.update(|_w, cx| {
                    let e = editor.read(cx);
                    if at >= e.text().len() {
                        return true;
                    }
                    let r = crate::editor::movement::char_right(e.text(), at);
                    editor.update(cx, |e, cx| {
                        e.set_selection_range(at..r, cx);
                        e.insert_text("", cx);
                    });
                    false
                });
                if done {
                    break;
                }
                deleted += 1;
            }
            let dt = t_del.elapsed();
            crate::prof::dump("editor-delete.measured");
            let after = cx.update(|_, cx| editor.read(cx).value().len());
            let after_chars = cx.update(|_, cx| editor.read(cx).char_count());
            let shrank = before.saturating_sub(after);
            assert_eq!(
                before_chars - after_chars,
                deleted,
                "删除 {deleted} 次后应恰好少 {deleted} 个字符，实际少 {} 个字符（字节 {} B）",
                before_chars - after_chars,
                shrank,
            );
            println!(
                "【{file_name}】打开(异步装配) {} ms | {deleted} 次删除(逐字符) 耗时 {:.1} ms（单次平均 {:.3} ms），长度 {} -> {}（-{} B）",
                open_ms.as_millis(),
                dt.as_secs_f64() * 1000.0,
                dt.as_secs_f64() * 1000.0 / deleted.max(1) as f64,
                before,
                after,
                shrank,
            );
        }
    }


