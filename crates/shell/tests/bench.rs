//! 性能基准域（全部 `#[ignore]`）：大文档打开/编辑/删除、预览桥、侧栏扫描、启动→首帧、
//! 新开媒体窗口、拖拽拆窗断点等真夹具墙钟基准。断言内部与 lib 单测 `src/app/tests/bench.rs`
//! 逐字一致（P1 测试域化），仅调整测试不可见的 crate 内路径：
//! - `base::prof` → `base::prof`（lib 内私有别名 `use base::prof`，integration 直连 base）
//! - `crate::app::sidebar::scan_line_hits` → `shell_scan::scan_line_hits`（leaf 直挂）
//! - `crate::app::sidebar::SidebarView` → `shell::app::SidebarView`（§8.6 外提重导出）
//! 另将 `tests/media_window.rs` 的 4 个媒体夹具 helper（`registry_window_count` /
//! `media_win_snap` / `other_app_entity` / `placeholder_audio`）随迁本目标
//! （`bench_new_media_window_first_frame` 依赖；该测试文件未在本批拆出）。
//! 所有基准均 `#[ignore]`：常规 `cargo test -p shell` 不计入通过数；跑 `--ignored` 时才执行。

mod common;

use common::*;

use shell::app::EditorAppRegistry;

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
        // 用「行数」作廉价就绪判据（大文档必然远超；不用每帧 value()
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
            let (wy_loaded, editor_lines) = cx.update(|_, cx| {
                let a = app.read(cx);
                let i = a.model.borrow().tabs.len().saturating_sub(1);
                (
                    a.model.borrow().tabs[i].payload.md_wy
                        .as_ref()
                        .map(|w| w.read(cx).is_loaded())
                        .unwrap_or(false),
                    a.model.borrow().tabs[i].payload.editor.read(cx).line_count(),
                )
            });
            if !d_ready && editor_lines > 1000 {
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
            let (editor_lines, wy_loaded) = cx.update(|_, cx| {
                let a = app.read(cx);
                let i = a.model.borrow().tabs.len().saturating_sub(1);
                (
                    a.model.borrow().tabs[i].payload.editor.read(cx).line_count(),
                    a.model.borrow().tabs[i].payload.md_wy
                        .as_ref()
                        .map(|w| w.read(cx).is_loaded())
                        .unwrap_or(false),
                )
            });
            if !d_ready && editor_lines > 1000 {
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
            let i = a.model.borrow().tabs.len().saturating_sub(1);
            a.model.borrow().tabs[i].payload.md_wy
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

    /// A4 基准（ignored）：大文档「目录」视图的卡顿拆解。
    /// 真实 `fixtures/大文档测试2.md`（6.4MB / ~10 万行 / ~1.6 万标题）。等右栏 WY 就绪后：
    /// 1. 点活动栏「目录」打开面板并重绘首帧耗时（用户报告「点目录卡死」的那一刻）；
    /// 2. 目录常开时的逐帧 refresh 耗时（观察是否每帧全量物化上万条目）。
    /// 运行：`cargo test -p shell -- --ignored bench_toc_open_large_md --nocapture`（debug）
    #[gpui::test]
    #[ignore]
    async fn bench_toc_open_large_md(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1280.), px(800.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let p = r"../../fixtures/大文档测试2.md";
        assert!(std::path::Path::new(p).is_file(), "{p} 缺失");
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(p, window, cx));
        });
        let mut wy_ready = false;
        for _ in 0..400 {
            cx.run_until_parked();
            wy_ready = cx.update(|_, cx| {
                let a = app.read(cx);
                let i = a.model.borrow().tabs.len().saturating_sub(1);
                a.model.borrow().tabs[i].payload.md_wy
                    .as_ref()
                    .map(|w| w.read(cx).is_loaded())
                    .unwrap_or(false)
            });
            if wy_ready {
                break;
            }
        }
        assert!(wy_ready, "右栏 WY 预览未就绪");
        // 预热（编辑器/预览本体首帧布局，排除在 TOC 计时外）
        for _ in 0..3 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
        }
        // 1) 点「目录」打开面板 + 首帧重绘
        let t0 = std::time::Instant::now();
        cx.update(|_w, cx| {
            app.update(
                cx,
                |app, cx| app.toggle_sidebar_view(shell::app::SidebarView::Toc, cx),
            );
        });
        cx.run_until_parked();
        let _ = cx.cx.refresh();
        let open_ms = t0.elapsed().as_millis();
        println!("【TOC 首帧开启】(debug) {open_ms} ms");
        // 已物化/可命中的末尾条目抽样（第 15000 个标题：全量物化才存在）
        let far = cx.debug_bounds("sidebar-toc-md-15000");
        println!(
            "【末尾条目 sidebar-toc-md-15000 已物化】 {}",
            far.is_some()
        );
        // 2) 常开时逐帧成本
        let mut per_frame: Vec<u128> = Vec::new();
        for _ in 0..5 {
            let tf = std::time::Instant::now();
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            per_frame.push(tf.elapsed().as_millis());
        }
        println!("【TOC 常开逐帧 refresh】(debug) {per_frame:?} ms");
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
                let i = a.model.borrow().tabs.len().saturating_sub(1);
                // Step2：提前整篇解析可先于编辑区收尾装好预览 → 逐键一致性（m==e）必须
                // 等**打开流水线整体完成**（open_finish_count>=1 ⇒ 编辑区已全文落定）。
                a.open_finish_count >= 1
                    && a.model.borrow().tabs[i].payload.md_wy
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
            let t = &a.model.borrow().tabs[a.model.borrow().active_tab];
            (
                t.payload.md_wy
                    .as_ref()
                    .map(|w| w.read(cx).text().to_string())
                    .unwrap_or_default(),
                t.payload.editor.read(cx).value().to_string(),
            )
        });
        assert_eq!(m, e, "{N} 键后预览应仍与编辑区逐字节一致");
        // 隔离测量桥模型成本（独立 WyEditor 副本，绝不污染已对齐的 md_wy）：
        // ⚠️ 循环内不读 `text()`（O(n) 物化）；用 O(1) `len()` 取初始长后自行推进。
        let scratch = cx.update(|window, cx| {
            let rope = app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab].payload.editor
                .read(cx)
                .rope()
                .clone();
            let s = cx.new(|cx| WyEditor::new(window, cx));
            s.update(cx, |w, _cx| {
                w.install_open(markdown_wysiwyg::WyEditor::open_from_rope(rope))
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
            // 2) 等待选项卡就绪（`open_file` 已同步建 tab；编辑器内容随首块/整篇即时可见）。
            let t_open = std::time::Instant::now();
            let mut frames = 0;
            loop {
                cx.run_until_parked();
                if cx.update(|_, cx| app.read(cx).model.borrow().tabs.last().is_some()) {
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
                let idx = a.model.borrow().active_tab;
                (
                    a.model.borrow().tabs[idx].payload.editor.clone(),
                    a.model.borrow().tabs[idx].payload.editor.read(cx).focus_handle(),
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
    /// 运行：`cargo test --release --bin vuick bench_open_large_txt_progressive -- \
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
                let i = a.model.borrow().tabs.len().saturating_sub(1);
                (
                    a.model.borrow().tabs[i].payload.editor.read(cx).line_count(),
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
            let i = a.model.borrow().tabs.len().saturating_sub(1);
            let expected = std::fs::read_to_string(&path)
                .unwrap()
                .replace("\r\n", "\n")
                .replace('\r', "\n");
            a.model.borrow().tabs[i].payload.editor.read(cx).value() == expected
        });
        let last_lc = cx.update(|_, cx| {
            let a = app.read(cx);
            let i = a.model.borrow().tabs.len().saturating_sub(1);
            a.model.borrow().tabs[i].payload.editor.read(cx).line_count()
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
    /// 运行：`cargo test --release --bin vuick bench_rope_grow_strategies -- \
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
            if cx.update(|_, cx| app.read(cx).model.borrow().tabs.last().is_some()) {
                break;
            }
            frames += 1;
            if frames > 400 {
                panic!("打开超时");
            }
        }
        let editor = cx.update(|_, cx| app.read(cx).model.borrow().tabs.last().unwrap().payload.editor.clone());
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
            app.update(cx, |a, cx| {
                let idx = a.model.borrow().active_tab;
                a.toggle_preview_for_tab(idx, window, cx);
            });
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
    /// 并用 [`base::prof`] 输出各热点（tab_change / hl_update / hl_styles / md_render /
    /// md_ensure_window）的累计拆解。
    #[gpui::test]
    #[ignore]
    async fn bench_realistic_large_file_editing(cx: &mut TestAppContext) {
        use std::time::{Duration, Instant};
        base::prof::enable();
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
            if cx
                .update(|_, cx| app.read(cx).model.borrow().tabs.last().is_some())
            {
                break;
            }
            frames += 1;
            if frames > 600 {
                panic!("打开超时");
            }
        }
        let editor = cx.update(|_, cx| app.read(cx).model.borrow().tabs.last().unwrap().payload.editor.clone());
        // 预热：预览窗口物化 + 行高提示收敛
        for _ in 0..5 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        base::prof::dump("开篇预热后");
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
        base::prof::dump("A. 开头连续输入8键");
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
        base::prof::dump("A. 停顿同步后");

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
        base::prof::dump("B. 中部连续输入8键");
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
        base::prof::dump("B. 停顿同步后（完成）");

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
            base::prof::dump("C. 探针后");
        }
    }

    /// 基准：编辑器本体（左侧纯文本 CodeEditor）连续删除 1000 次字符的耗时，与
    /// `bench_insert_1000_times` 对称。删除漏斗说明：`CodeEditor::backspace` /
    /// `delete_forward` 是私有方法（src/editor/view/mod.rs:2236，内部都走
    /// `insert_edit(…, EditIntent::Backspace/Delete)`），测试拿不到；这里用等价公开
    /// 漏斗复现同一 delete 成本——`set_selection_range(at..at+1)` + `insert_text("")`
    /// （insert_text 内部即 `insert_edit(self.selection.range(), text, …)`：逐键单字符
    /// 删除、撤销合并走同一主链）。打开真实文件后从正文中部向前逐字符删 N 次，输出
    /// 总耗时 + ms/次（单位：ms/次），`KILO_PROF=1` 时 `base::prof::dump` 拆账。
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
            // 2) 等待选项卡就绪（`open_file` 已同步建 tab；编辑器内容随首块/整篇即时可见）。
            let t_open = std::time::Instant::now();
            let mut frames = 0;
            loop {
                cx.run_until_parked();
                if cx.update(|_, cx| app.read(cx).model.borrow().tabs.last().is_some()) {
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
                a.model.borrow().tabs[a.model.borrow().active_tab].payload.editor.clone()
            });
            let before = cx.update(|_, cx| editor.read(cx).value().len());
            if before < 2 {
                println!("【{file_name}】过短跳过（len={before}）");
                continue;
            }
            let n = N.min(before - 1).max(1);
            let before_chars = cx.update(|_, cx| editor.read(cx).char_count());
            base::prof::enable();
            base::prof::dump("editor-delete.warmup");
            // 逐字符前删 = 编辑器 `delete_forward` 同款语义：光标 `at` 固定（边界，先经
            // char_left 钳正，避免 before/2 劈开多字节字符），每键删 `[at, char_right(at))`
            // 一个字符并重新取边界；删除后文档左移，固定 `at` 即连续删后继字符——
            // 等价真 Delete 键的 `insert_edit(head..char_right, "", Delete)`。
            let at = cx.update(|_, cx| {
                let e = editor.read(cx);
                editor::movement::char_left(e.text(), (before / 2).min(e.text().len()))
            });
            let mut deleted = 0usize;
            let t_del = std::time::Instant::now();
            for _ in 0..n {
                let done = cx.update(|_w, cx| {
                    let e = editor.read(cx);
                    if at >= e.text().len() {
                        return true;
                    }
                    let r = editor::movement::char_right(e.text(), at);
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
            base::prof::dump("editor-delete.measured");
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

    /// 当前扫描语义的匹配阶段（直接调生产 `scan_line_hits`——`sidebar_search_scan` 的
    /// 可编辑分支正是 `value()` 物化后逐行复用它）。
fn new_style_scan(content: &str, q: &str) -> usize {
    let mut out = Vec::new();
    shell_scan::scan_line_hits(content, q, usize::MAX, &mut out);
    out.len()
}

    /// 旧式基准：每行 `to_lowercase()` 新分配小写串 + 朴素查找（物化/防抖优化前的每行分配路径）。
    fn old_style_scan(content: &str, q: &str) -> usize {
        let mut hits = 0;
        for line in content.split('\n') {
            if line.to_lowercase().find(q).is_some() {
                hits += 1;
            }
        }
        hits
    }

    /// 基准（ignored）：侧边栏搜索「一次扫描」端到端性能拆解——10k 行 × ~80 字符文本
    /// （≈0.8MB，超 128KB 阈值 → 与真实使用一致走后台上/分块装配）。在真实 app 状态上
    /// 测生产可编辑路径的两段：`value()` 一次物化 + `scan_line_hits` 零分配匹配，并对照
    /// 旧式（每行 `to_lowercase()` 分配 + 朴素查找）；命中集须一致。
    /// 运行：`cargo test -p shell -- --ignored bench_sidebar_search_scan --nocapture`
    #[gpui::test]
    #[ignore]
    async fn bench_sidebar_search_scan(cx: &mut TestAppContext) {
        use config::limits::SEARCH_RESULT_CAP;
        use std::fmt::Write as _;
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1280.), px(800.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        const LINES: usize = 10_000;
        let dir = std::env::temp_dir().join("gpui-sz-bench-corpus.txt");
        let mut sb = String::with_capacity(LINES * 84);
        let body = "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
        for i in 0..LINES {
            // 每 100 行埋一个大小写混合的目标词（命中 ~100 行，低于 CAP 走完整扫描）
            let marker = if i % 100 == 5 { "NeedleMark target " } else { "" };
            let _ = writeln!(sb, "{i:05} {marker}{body}");
        }
        std::fs::write(&dir, &sb).unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(dir.to_str().unwrap(), window, cx));
        });
        // 后台分块装配完成判据：编辑区行数到位（与 bench_open_large_md 同型轮询）
        let mut ready = false;
        for _ in 0..400 {
            cx.run_until_parked();
            let n = cx.update(|_w, cx| {
                app.read(cx)
                    .model.borrow().tabs
                    .last()
                    .map(|t| t.payload.editor.read(cx).line_count())
                    .unwrap_or(0)
            });
            if n >= LINES {
                ready = true;
                break;
            }
        }
        assert!(ready, "corpus 后台装配超时");
        let ti = cx.update(|_w, cx| app.read(cx).model.borrow().active_tab);
        let q = "needlemark"; // 查询词（归一化后小写）

        // 取正文（可编辑路径的一次 `value()` 整篇物化，内含物化耗时）
        let value_t = std::time::Instant::now();
        let content = cx.update(|_w, cx| app.read(cx).model.borrow().tabs[ti].payload.editor.read(cx).value());
        let value_ms = value_t.elapsed().as_millis();
        // 多次取均（debug 单次抖动大；两者同轮同 jit/allocator 状态，比值才可信）
        const RUNS: usize = 20;
        let t = std::time::Instant::now();
        let mut new_hits = 0usize;
        for _ in 0..RUNS {
            new_hits = new_style_scan(&content, q);
        }
        let new_match_ms = t.elapsed().as_secs_f64() * 1000.0 / RUNS as f64;
        let t = std::time::Instant::now();
        let mut old_hits = 0usize;
        for _ in 0..RUNS {
            old_hits = old_style_scan(&content, q);
        }
        let old_ms = t.elapsed().as_secs_f64() * 1000.0 / RUNS as f64;
        // 命中集一致（防基准自身漂移）；命中数低于 CAP 保持语义等价（不触发截断）
        assert_eq!(new_hits, old_hits, "新旧扫描命中集应一致");
        assert!(new_hits < SEARCH_RESULT_CAP, "本基准应低于 CAP（否则触发截断）");
        let new_total = value_ms as f64 + new_match_ms;
        let old_total = value_ms as f64 + old_ms;
        println!("【搜索扫描基准】正文 {} B / {} 行，查询词 {:?}，命中 {new_hits} 行（{} 次取均）", content.len(), LINES, q, RUNS);
        println!("  新路径 · 单次搜索 ≈ value() 物化 {value_ms} ms + scan_line_hits 匹配（小写副本+str::find）{new_match_ms:.1} ms = {new_total:.1} ms");
        println!("  旧式 · 每行 to_lowercase+朴素查找 = {old_ms:.1} ms（全流程 {old_total:.1} ms）");
        println!("  （debug 数值约 release 的 ~10×，看相对量级）");
        let _ = std::fs::remove_file(&dir);
    }

    /// 轨 A 基准（ignored）：启动 → 首帧墙钟（真夹具真墙钟，headless 可复跑，纯 ADD 不改
    /// 行为）。分段：① 窗口构造（EditorApp::new 真实启动路径 + Root）+ 首帧 render（refresh
    /// 真实布局/paint）② 会话恢复段（`restore_session_from` 显式临时目录，含逐选项卡同步
    /// open_file / 建未命名选项卡灌槽位）＝轨 A 改造前首帧成本基线。
    /// 生产完整 `[STARTUP]` 直读：`VUICK_STARTUP_DIAG=1 target\debug\vuick.exe`
    /// 的 stderr 首行。运行：
    /// `cargo test -p shell bench_startup_to_first_frame -- --ignored --nocapture`
    #[gpui::test]
    #[ignore]
    async fn bench_startup_to_first_frame(cx: &mut TestAppContext) {
        use session_store::SessionTab;
        use std::time::Instant;
        init_test(cx);
        // ① 构造 + 首帧（真实启动路径：EditorApp::new = startup=true，restore/tray 在
        // 测试构建 cfg 掉，量「构造 + 首次 render」的剩余同步成本）。
        let holder: std::rc::Rc<std::cell::RefCell<Option<gpui::Entity<EditorApp>>>> =
            std::rc::Rc::default();
        let t_start = Instant::now();
        let (_root, cx) = cx.add_window_view(|window, cx| {
            let app = cx.new(|cx| EditorApp::new(window, cx));
            *holder.borrow_mut() = Some(app.clone());
            ui::root::Root::new(app, window, cx)
        });
        let app = holder.borrow().clone().unwrap();
        cx.simulate_resize(gpui::size(px(1200.), px(800.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let _ = cx.cx.refresh();
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let construct_frame_ms = t_start.elapsed().as_secs_f64() * 1000.0;
        let _ = app;
        // ② 会话恢复段：显式临时目录播种 3 文件 + 2 未命名，干净零 tab 实例恢复。
        let dir = std::env::temp_dir().join(format!(
            "gpui-session-bench-a-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut files = Vec::new();
        for i in 0..3 {
            let p = dir.join(format!("f{i}.txt"));
            let body: String = (0..2000)
                .map(|k| format!("line{k} 一些内容 {i}\n"))
                .collect();
            std::fs::write(&p, &body).unwrap();
            files.push(p);
        }
        let m = session_store::SessionManifest {
            version: 1,
            active_tab: 0,
            tabs: vec![
                SessionTab::File { path: files[0].clone() },
                SessionTab::New { slot: "t0".into(), name: "未命名".into() },
                SessionTab::File { path: files[1].clone() },
                SessionTab::File { path: files[2].clone() },
                SessionTab::New { slot: "t1".into(), name: "未命名".into() },
            ],
        };
        assert!(session_store::write_manifest(&dir, &m), "清单应写入");
        assert!(session_store::write_slot(&dir, "t0", "未命名内容甲"));
        assert!(session_store::write_slot(&dir, "t1", "未命名内容乙"));
        let (app2, cx) = add_app_zero_tabs(cx);
        let t_restore = Instant::now();
        cx.update(|window, cx| {
            app2.update(cx, |app, cx| {
                assert!(session::restore_session_from(app, &dir, window, cx));
            });
        });
        let restore_ms = t_restore.elapsed().as_secs_f64() * 1000.0;
        let restored = cx.update(|_w, cx| {
            let a = app2.read(cx);
            (
                a.model.borrow().tabs.len(),
                a.model.borrow().tabs.iter().filter(|t| t.payload.file_path.is_none()).count(),
            )
        });
        assert_eq!(restored, (5, 2), "恢复应得 5 选项卡（3 文件 + 2 未命名）");
        let _ = std::fs::remove_dir_all(&dir);
        println!("==== 启动→首帧墙钟基准（debug，headless；真机以 VUICK_STARTUP_DIAG=1 的 [STARTUP] 行为准） ====");
        println!("  窗口构造 + 首帧 render(refresh 真实布局): {construct_frame_ms:.1} ms");
        println!("  会话恢复段(3 文件 + 2 未命名):            {restore_ms:.1} ms");
        println!(
            "  ≈ 启动→首帧合计: {:.1} ms",
            construct_frame_ms + restore_ms
        );
    }

    /// 轨 D 基准（ignored）：新开媒体窗口「触发 → 首次 render 结束」墙钟（headless）。
    /// 复用 media_window 测试姿势：主窗口开音频（media_new_window/reuse 默认开、无既有
    /// 媒体窗口）→ `maybe_redirect_media_open` 决策 NewWindow → `open_media_in_new_window`
    /// 新开窗口 → 媒体路由打开。首帧判据 = 新窗口实体出现且完整进入音乐模式（music_mode
    /// 且 MusicView 已创建），源窗口 tabs 不变、窗口数 +1（保基准形态正确）。
    /// 运行：`cargo test -p shell bench_new_media_window_first_frame -- --ignored --nocapture`
    #[gpui::test]
    #[ignore]
    async fn bench_new_media_window_first_frame(cx: &mut TestAppContext) {
        use std::time::Instant;
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let s = placeholder_audio("bench-wall");
        let tabs_before = cx.update(|_w, cx| app.read(cx).model.borrow().tabs.len());
        // 预热首窗口首帧（排除在计时外）
        for _ in 0..3 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
        }
        let t0 = Instant::now();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(s.to_str().unwrap(), window, cx));
        });
        let dispatch_ms = t0.elapsed().as_secs_f64() * 1000.0;
        // 轮询到新窗口完整进入音乐模式（首屏 render 就绪）
        let mut ready = false;
        let mut frames = 0usize;
        let mut media_ms = 0.0f64;
        while frames < 200 && !ready {
            frames += 1;
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            ready = other_app_entity(cx, &app)
                .into_iter()
                .any(|e| media_win_snap(cx, &e).0);
            if ready {
                media_ms = t0.elapsed().as_secs_f64() * 1000.0;
            }
        }
        // 再等几帧让首屏完整 render 结束
        for _ in 0..3 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
        }
        let total_ms = t0.elapsed().as_secs_f64() * 1000.0;
        assert!(ready, "媒体窗口应进入音乐模式（首帧就绪）");
        assert_eq!(
            cx.update(|_w, cx| app.read(cx).model.borrow().tabs.len()),
            tabs_before,
            "源窗口 tabs 应不变"
        );
        assert_eq!(registry_window_count(cx), 2, "应恰好多 1 个媒体窗口");
        println!("==== 新开媒体窗口墙钟基准（debug，headless） ====");
        println!("  open_file 同步返回(分流决策,不卡 UI): {dispatch_ms:.1} ms");
        println!("  → 新窗口音乐模式就绪(约首帧): {media_ms:.1} ms（frames={frames}）");
        println!("  → 额外 settle 后合计: {total_ms:.1} ms");
        let _ = std::fs::remove_file(&s);
    }

    /// 探针（BIGDOC_BENCH=1）：大文档选项卡「拖出拆新窗」的松手→新窗可见墙钟断点：
    /// 按下 / 首个 on_drag（建拖拽实体+预建幽灵窗）/ 首个 on_drag_move（幽灵窗定位）/
    /// 松手同步（含 open_window 同步首帧）。作为拖拽拆窗流畅度的回归对比基准（#ignore）。
    #[gpui::test]
    #[ignore]
    async fn bench_probe_drag_split_wall(cx: &mut TestAppContext) {
        if std::env::var_os("BIGDOC_BENCH").is_none() {
            return;
        }
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let p = r"../../fixtures/大文档测试.rs";
        assert!(std::path::Path::new(p).is_file(), "{p} 缺失");
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(p, window, cx));
        });
        let mut frames = 0;
        loop {
            frames += 1;
            cx.run_until_parked();
            let lines = cx.update(|_w, cx| {
                let a = app.read(cx);
                let i = a.model.borrow().tabs.len().saturating_sub(1);
                a.model.borrow().tabs[i].payload.editor.read(cx).line_count()
            });
            if lines > 1000 {
                break;
            }
            if frames > 400 {
                break;
            }
        }
        println!(
            "【大档就绪】tab 数={}",
            cx.update(|_w, cx| app.read(cx).model.borrow().tabs.len())
        );
        // 拖**文件**选项卡（tab-1）出窗拆新窗 —— 大文档 open_window 首帧主路径
        let tab = cx
            .debug_bounds("tab-1")
            .expect("文件选项卡应渲染");
        let start = gpui::Point::new(
            tab.origin.x + px(10.),
            tab.origin.y + px(tab.size.height.as_f32() / 2.0),
        );
        let t_down = std::time::Instant::now();
        cx.simulate_mouse_down(start, MouseButton::Left, Default::default());
        println!("【按下】dispatch {} ms", t_down.elapsed().as_millis());
        // 首次移动：越 DRAG_THRESHOLD → on_drag 构造器（记录抓取偏移、建拖拽实体，
        // 并**预建幽灵窗**——创建前移到拖拽起点，首个 on_drag_move 只 SetWindowPos 定位）
        let t_thresh = std::time::Instant::now();
        cx.simulate_mouse_move(
            gpui::Point::new(start.x + px(40.), start.y),
            MouseButton::Left,
            Default::default(),
        );
        println!(
            "【首次移动(越阈值→on_drag+预建幽灵窗)】dispatch {} ms",
            t_thresh.elapsed().as_millis()
        );
        for _ in 0..2 {
            cx.run_until_parked();
        }
        // 第二次移动：首个 on_drag_move → update_drag_ghost（幽灵窗已就绪，只定位/显隐）
        let t_firstmove = std::time::Instant::now();
        cx.simulate_mouse_move(
            gpui::Point::new(start.x + px(90.), start.y),
            MouseButton::Left,
            Default::default(),
        );
        println!(
            "【第二移(首 on_drag_move→幽灵窗定位)】dispatch {} ms",
            t_firstmove.elapsed().as_millis()
        );
        for _ in 0..2 {
            cx.run_until_parked();
        }
        let outside = gpui::Point::new(px(-80.), px(120.));
        cx.simulate_mouse_move(outside, MouseButton::Left, Default::default());
        for _ in 0..8 {
            cx.run_until_parked();
        }
        let t_rel = std::time::Instant::now();
        cx.simulate_mouse_up(outside, MouseButton::Left, Default::default());
        println!(
            "【松手同步·大文档】release（含 open_window 同步首帧）{} ms",
            t_rel.elapsed().as_millis()
        );
        for _ in 0..4 {
            cx.run_until_parked();
        }
        // 对照 2：拖**未命名空**选项卡（拆分后主窗口仅剩 tab-0）—— open_window 固定开销
        let start = cx
            .debug_bounds("tab-0")
            .map(|t| {
                gpui::Point::new(
                    t.origin.x + px(10.),
                    t.origin.y + px(t.size.height.as_f32() / 2.0),
                )
            })
            .expect("空选项卡应渲染");
        cx.simulate_mouse_down(start, MouseButton::Left, Default::default());
        cx.simulate_mouse_move(
            gpui::Point::new(start.x + px(40.), start.y),
            MouseButton::Left,
            Default::default(),
        );
        for _ in 0..2 {
            cx.run_until_parked();
        }
        let outside = gpui::Point::new(px(-80.), px(120.));
        cx.simulate_mouse_move(outside, MouseButton::Left, Default::default());
        for _ in 0..8 {
            cx.run_until_parked();
        }
        let t_rel = std::time::Instant::now();
        cx.simulate_mouse_up(outside, MouseButton::Left, Default::default());
        println!(
            "【松手同步】release（含 open_window 同步首帧）{} ms",
            t_rel.elapsed().as_millis()
        );
        let t_settle = std::time::Instant::now();
        for _ in 0..6 {
            cx.run_until_parked();
        }
        println!(
            "【新窗稳定】松手后 +6 帧 {} ms",
            t_settle.elapsed().as_millis()
        );
    }

// ===== 媒体窗口夹具 helper（随迁自 tests/media_window.rs，`bench_new_media_window_first_frame` 依赖）=====

/// 只读全局注册表的窗口总数（EditorAppRegistry 在每次构造时登记）。
fn registry_window_count(cx: &mut TestAppContext) -> usize {
    cx.update(|cx| cx.global::<EditorAppRegistry>().windows.len())
}

/// 快速读取某窗口的可观测状态：(music 模式, 活动选项卡文件路径, tab 数)。
fn media_win_snap(
    cx: &mut TestAppContext,
    e: &gpui::Entity<EditorApp>,
) -> (bool, Option<std::path::PathBuf>, usize) {
    cx.update(|cx| {
        let a = e.read(cx);
        let model = a.model.borrow();
        let tab = model.tabs.get(model.active_tab);
        (
            a.model.borrow().music_mode,
            tab.and_then(|t| t.payload.file_path.clone()),
            a.model.borrow().tabs.len(),
        )
    })
}

/// 按实体 id 过滤出「非给定窗口」的注册表强实体（找另一窗口用）。
fn other_app_entity(
    cx: &mut TestAppContext,
    exclude: &gpui::Entity<EditorApp>,
) -> Vec<gpui::Entity<EditorApp>> {
    cx.update(|cx| {
        let reg = cx.global::<EditorAppRegistry>();
        reg.windows
            .iter()
            .filter_map(|(weak, _)| {
                let e = weak.upgrade()?;
                (e.entity_id() != exclude.entity_id()).then_some(e)
            })
            .collect()
    })
}

/// 占位音频文件（.ogg，路由只看扩展名）。
fn placeholder_audio(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "gpui-mediawnd-a-{}-{tag}",
        std::process::id()
    ));
    let _ = std::fs::create_dir_all(&d);
    let p = d.join(format!("{tag}.ogg"));
    std::fs::write(&p, b"OggS placeholder").unwrap();
    p
}


