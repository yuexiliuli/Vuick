    /// 编辑区⇄预览滚动同步（editor→preview）：把编辑器滚到第 20 行 → 右栏 WyEditor
    /// 预览逐帧跟随到「该行所在块的起点」（同步单位 = 块）。驱动来自 render 开头
    /// `sync_editor_preview_scroll`，两侧收敛到同一块后静止。
    #[gpui::test]
    async fn editor_scroll_drives_md_preview(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1200.), px(800.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let md = std::env::temp_dir().join(format!(
            "gpui-sync-ed2pv-{}.md",
            std::process::id()
        ));
        let mut doc = String::new();
        for i in 0..60 {
            doc.push_str(&format!("# 大标题 {i}\n\n正文章节 {i}。\n\n"));
        }
        std::fs::write(&md, &doc).unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(md.to_str().unwrap(), window, cx));
        });
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let (editor, mdw) = cx.update(|_, cx| {
            let a = app.read(cx);
            (
                a.model.borrow().tabs[a.model.borrow().active_tab].payload.editor.clone(),
                a.model.borrow().tabs[a.model.borrow().active_tab]
                    .payload.md_wy
                    .clone()
                    .expect("MD 打开后 WyEditor 预览应在场"),
            )
        });
        let vhand = cx.update(|_, cx| {
            let a = app.read(cx);
            a.model.borrow().tabs[a.model.borrow().active_tab].payload.md_wy_vscroll.clone()
        });
        let before = cx.update(|_w, _cx| (-vhand.offset().y).as_f32());
        assert_eq!(before, 0.0, "预览初始在顶部");

        // 「用户」滚动编辑区到第 20 行（= 某个标题块所在行）。
        let target = 20usize;
        cx.update(|_, cx| {
            editor.update(cx, |ed, _| ed.scroll_to_line(target));
        });
        // 预览滚动由 WY 元素 prepaint 逐帧重钉宿主偏移直到稳住（需几帧收敛）。
        let mut after = before;
        let mut settled_after = None;
        for _frame in 0..12 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
            after = cx.update(|_w, _cx| (-vhand.offset().y).as_f32());
            if after > before + 1.0 {
                settled_after = Some(after);
                break;
            }
        }
        assert!(
            settled_after.is_some(),
            "编辑器滚动应驱动预览滚动（{before:.0} → {after:.0}px）"
        );
        let (editor_top, preview_src) = cx.update(|_, cx| {
            let et = editor.read(cx).viewport_top_line();
            let ps = mdw.read(cx).top_source_line();
            (et, ps)
        });
        let expect_block = cx.update(|_, cx| mdw.read(cx).block_start_of_line(target));
        assert_eq!(
            preview_src,
            Some(expect_block),
            "预览顶块应 = 编辑器目标行所在块（同步键=块起点）"
        );
        assert_eq!(editor_top, target, "编辑器顶行应为目标行");
        // 收敛后稳定性：再跑 2 帧，预览宿主偏移必须保持不变——防「左右互驱振荡」
        // 每次重钉把预览拉走再被趋势拽回（若存在振荡，这里偏移会继续变）。
        let stable = cx.update(|_w, _cx| (-vhand.offset().y).as_f32());
        for _ in 0..2 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let stable2 = cx.update(|_w, _cx| (-vhand.offset().y).as_f32());
        assert!(
            (stable2 - stable).abs() < 1.5,
            "收敛后预览偏移应稳定不再振荡（{stable:.0} → {stable2:.0}px）"
        );
        let _ = std::fs::remove_file(&md);
    }

    /// 编辑区⇄预览滚动同步（preview→editor）：拖右栏 WyEditor 预览纵向滚动条向下（真实
    /// 用户滚动路径）→ 编辑器逐帧跟随到预览顶块起点，且两侧收敛到同一块后不再互拉。
    #[gpui::test]
    async fn preview_scroll_drives_editor(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1200.), px(800.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let md = std::env::temp_dir().join(format!(
            "gpui-sync-pv2ed-{}.md",
            std::process::id()
        ));
        let mut doc = String::new();
        for i in 0..60 {
            doc.push_str(&format!("# 大标题 {i}\n\n正文章节 {i}。\n\n"));
        }
        std::fs::write(&md, &doc).unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(md.to_str().unwrap(), window, cx));
        });
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let (editor, mdw) = cx.update(|_, cx| {
            let a = app.read(cx);
            (
                a.model.borrow().tabs[a.model.borrow().active_tab].payload.editor.clone(),
                a.model.borrow().tabs[a.model.borrow().active_tab]
                    .payload.md_wy
                    .clone()
                    .expect("MD 打开后 WyEditor 预览应在场"),
            )
        });
        let overlay = cx
            .debug_bounds("md-wy-vscrollbar-overlay")
            .expect("MD WyEditor 预览纵向滚动条叠层应渲染");

        // 拖预览滚动条向下大幅滚动（真实路径：写共享 desired → render 每帧落窗口）。
        let x = overlay.right() - px(8.);
        cx.simulate_mouse_down(gpui::point(x, overlay.top() + px(12.)), MouseButton::Left, Default::default());
        for _ in 0..2 {
            cx.run_until_parked();
        }
        let mid_y = overlay.top() + px(480.);
        cx.simulate_mouse_move(gpui::point(x, mid_y), MouseButton::Left, Default::default());
        for _ in 0..6 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        cx.simulate_mouse_up(gpui::point(x, mid_y), MouseButton::Left, Default::default());
        for _ in 0..6 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let vhand = cx.update(|_, cx| {
            let a = app.read(cx);
            a.model.borrow().tabs[a.model.borrow().active_tab].payload.md_wy_vscroll.clone()
        });
        let (editor_top, preview_src, s1) = cx.update(|_, cx| {
            let et = editor.read(cx).viewport_top_line();
            let ps = mdw.read(cx).top_source_line();
            let off = (-vhand.offset().y).as_f32();
            (et, ps, off)
        });
        assert!(
            editor_top > 0,
            "预览滚动应驱动编辑器跟随（editor top={editor_top}）"
        );
        // 编辑器跟随 ⇐ 驱动 requires 预览顶块就绪（sync 的 preview→editor 分支只在
        // preview_block 为 Some 时才驱动）→ 此处预览帧必然就绪，做强断言。
        let ps = preview_src.expect("预览帧应就绪（编辑器已跟随）");
        // 同步键是「块起点」粒度（双侧 `block_start_of_line` 归一）：重排（折行宽改变）
        // 后编辑器视口顶行可能是块内的视觉行（如 157∈块156），原始行号可比块密钥多 1——
        // 按块归一比较才是同步的不变式（两侧同块即视为对齐，与 `sync_editor_preview_scroll`
        // 的收敛判据一致）。
        let (editor_top, editor_block) = cx.update(|_, cx| {
            let et = editor.read(cx).viewport_top_line();
            (et, mdw.read(cx).block_start_of_line(et))
        });
        // 折行宽随窗口重排后，视口顶部边界存在 1 行内的像素取整（预览顶块起点 vs
        // 编辑器顶行所在块可能相邻而非相等）——同步键是块粒度，允许 ±1 相邻块对齐，
        // 核心回归仍是「无来回跳、单调跟随、编辑器确实被驱动到预览顶块附近」。
        assert!(
            editor_block.abs_diff(ps) <= 1,
            "编辑器顶块应与预览顶块对齐（editor={editor_top}, block={editor_block}, preview={ps}）"
        );

        // 第二次继续向下滚（用户后续滚动）：直接推宿主偏移模拟（同步只读 vhand.offset
        // 推算预览顶块，与真实滚动等价）。回归「预览始终来回跳动、滚动不下去」——预览
        // 驱动编辑器后，编辑器落位不得被误判成用户滚动而反向把预览拽回。此段预览偏移
        // 必须单调前进（s2 > s1），且编辑器跟随到更深顶块并与之对齐（有界等待收敛）。
        let deeper = s1 + 4000.0;
        cx.update(|_, _cx| {
            vhand.set_offset(gpui::point(gpui::px(0.), gpui::px(-deeper)));
        });
        // 有界等待**精确对齐**：块粒度同步键两侧相等才停（瞬态中编辑器可能先跑到更
        // 深而预览顶块仍读旧值，须等最终稳态）。
        let mut advanced: Option<(usize, usize, f32)> = None;
        for _frame in 0..30 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
            let (et2, eb2, ps2, off2) = cx.update(|_, cx| {
                let et = editor.read(cx).viewport_top_line();
                let eb = mdw.read(cx).block_start_of_line(et);
                let ps = mdw.read(cx).top_source_line();
                let off = (-vhand.offset().y).as_f32();
                (et, eb, ps, off)
            });
            if let Some(ps) = ps2
                && eb2.abs_diff(ps) <= 1
                && et2 > editor_top
                && off2 > s1 + 50.0
            {
                advanced = Some((et2, ps, off2));
                break;
            }
        }
        let (_et2, preview_src2, _s2) =
            advanced.expect("预览继续向下滚动后编辑器应跟随并对齐（有界 30 帧收敛）");
        let (editor_top2, editor_block2) = cx.update(|_, cx| {
            let et = editor.read(cx).viewport_top_line();
            (et, mdw.read(cx).block_start_of_line(et))
        });
        assert!(
            editor_block2.abs_diff(preview_src2) <= 1,
            "第二次后编辑器顶块应与预览顶块对齐（editor={editor_top2}, block={editor_block2}, preview={preview_src2}）"
        );
        let _ = std::fs::remove_file(&md);
    }

    /// I2：右栏预览**拖动中**编辑区跟随被延后——高跨块系列 tick（真实 MouseDown +
    /// 连续 Move，每帧跨大量块 = 拖动每步连跨）下编辑器 scroll 只更新首次、随后进入
    /// 延后**不逐帧**跟随（消除「每帧嵌编辑器全窗重建」的 28ms/帧卡顿）；停止滚动
    /// 但**不松手** → 按静止帧阈值（/墙钟兜底，取先到）**一次收敛**到最终预览顶块
    /// （覆盖「静止 150-250ms 取先到」的静止路径，不依赖 mouse-up）。
    #[gpui::test]
    async fn preview_fast_scrub_defers_editor_follow_then_converges(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1200.), px(800.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let md = std::env::temp_dir().join(format!(
            "gpui-sync-scrub-{}.md",
            std::process::id()
        ));
        let mut doc = String::new();
        for i in 0..60 {
            doc.push_str(&format!("# 大标题 {i}\n\n正文章节 {i}。\n\n"));
        }
        std::fs::write(&md, &doc).unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(md.to_str().unwrap(), window, cx));
        });
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let (editor, mdw) = cx.update(|_, cx| {
            let a = app.read(cx);
            (
                a.model.borrow().tabs[a.model.borrow().active_tab].payload.editor.clone(),
                a.model.borrow().tabs[a.model.borrow().active_tab].payload.md_wy.clone().expect("预览应在场"),
            )
        });
        let overlay = cx
            .debug_bounds("md-wy-vscrollbar-overlay")
            .expect("预览滚动条叠层应渲染");
        let x = overlay.right() - px(8.);
        let grab_y = overlay.top() + px(14.);
        cx.simulate_mouse_down(gpui::point(x, grab_y), MouseButton::Left, Default::default());
        for _ in 0..2 {
            cx.run_until_parked();
        }
        // 连续快速下拖：逐帧一笔（只 refresh 单帧推进——完整 park 会排空落位/跟随、
        // 退化成慢速步进而测不出延后；自绘滚动条 prepaint 帧级落地 pending_target，
        // 与 `scrollbar_drag_defers_then_converges_on_release` 同路径）。
        for step in 1..=8 {
            let y = overlay.top() + px(70.0 * step as f32);
            cx.simulate_mouse_move(gpui::point(x, y), MouseButton::Left, Default::default());
            let _ = cx.cx.refresh();
        }

        // 拖动中（尚无静止帧）：编辑器顶块应明显落后于预览最终顶块（延后生效）。
        let (editor_top_drag, preview_pb_drag) = cx.update(|_, cx| {
            let et = editor.read(cx).viewport_top_line();
            let pb = mdw.read(cx).top_source_line();
            (et, pb)
        });
        let final_pb = preview_pb_drag.expect("预览帧应就绪");
        let editor_block_drag = cx.update(|_, cx| mdw.read(cx).block_start_of_line(editor_top_drag));
        assert!(
            final_pb >= 20,
            "预览应已被拖到深处（final_pb={final_pb}）——夹具无效则断言无从谈起"
        );
        assert!(
            final_pb.saturating_sub(editor_block_drag) >= 12,
            "拖动中编辑器不应逐帧跟随（preview 顶块 {final_pb} vs editor 顶块 {editor_block_drag}）：延后失效时二者接近",
        );

        // 停止移动但**不松手** → 静止帧阈值内一次收敛到最终预览顶块（±1 块对齐）。
        let mut converged: Option<(usize, usize)> = None;
        for _frame in 0..40 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
            let (et, ps) = cx.update(|_, cx| {
                let et = editor.read(cx).viewport_top_line();
                (et, mdw.read(cx).top_source_line())
            });
            if let Some(ps) = ps {
                let eb = cx.update(|_, cx| mdw.read(cx).block_start_of_line(et));
                if eb.abs_diff(ps) <= 1 {
                    converged = Some((eb, ps));
                    break;
                }
            }
        }
        let (eb_done, ps_done) =
            converged.expect("静止后编辑器应一次收敛到预览最终顶块（有界 40 帧）");
        assert!(
            ps_done >= final_pb.saturating_sub(2),
            "收敛目标应为延后期最终预览块（final={final_pb}, got={ps_done}）"
        );
        assert!(
            eb_done.abs_diff(ps_done) <= 1,
            "收敛后两侧同块（editor={eb_done} vs preview={ps_done}）"
        );
        // 清理：补发 mouse_up 结束拖拽态（避免拖条状态残留影响后续测试）。
        cx.simulate_mouse_up(
            gpui::point(x, overlay.top() + px(560.)),
            MouseButton::Left,
            Default::default(),
        );
        for _ in 0..2 {
            cx.run_until_parked();
        }
        let _ = std::fs::remove_file(&md);
    }

    /// I2：右栏**拖滚动条**真实事件路径（MouseDown / 连续 Move / Up）：拖动中编辑区
    /// 跟随延后（连续多笔快速跨块），松手（mouse-up）预览静止后一次收敛到最终预览
    /// 顶块。复用自绘滚动条（`#md-wy-vscrollbar-overlay`）与真实事件分发，与
    /// `preview_scroll_drives_editor` 同路径但模拟**连续快速**拖动。
    #[gpui::test]
    async fn preview_scrollbar_drag_defers_then_converges_on_release(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1200.), px(800.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let md = std::env::temp_dir().join(format!(
            "gpui-sync-scrub-release-{}.md",
            std::process::id()
        ));
        let mut doc = String::new();
        for i in 0..60 {
            doc.push_str(&format!("# 大标题 {i}\n\n正文章节 {i}。\n\n"));
        }
        std::fs::write(&md, &doc).unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(md.to_str().unwrap(), window, cx));
        });
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let (editor, mdw) = cx.update(|_, cx| {
            let a = app.read(cx);
            (
                a.model.borrow().tabs[a.model.borrow().active_tab].payload.editor.clone(),
                a.model.borrow().tabs[a.model.borrow().active_tab].payload.md_wy.clone().expect("预览应在场"),
            )
        });
        let overlay = cx
            .debug_bounds("md-wy-vscrollbar-overlay")
            .expect("预览滚动条叠层应渲染");
        let x = overlay.right() - px(8.);
        let grab_y = overlay.top() + px(14.);
        cx.simulate_mouse_down(gpui::point(x, grab_y), MouseButton::Left, Default::default());
        for _ in 0..2 {
            cx.run_until_parked();
        }
        // 连续快速下拖：逐帧一笔（只 refresh 单帧推进；完整 park 会排空落位/跟随、
        // 退化成慢速步进而测不出延后）。
        for step in 1..=8 {
            let y = overlay.top() + px(70.0 * step as f32);
            cx.simulate_mouse_move(gpui::point(x, y), MouseButton::Left, Default::default());
            let _ = cx.cx.refresh();
        }
        // 拖动中（未松手）：编辑器不得逐帧跟到预览深处。
        let (editor_top_drag, preview_pb_drag) = cx.update(|_, cx| {
            let et = editor.read(cx).viewport_top_line();
            let pb = mdw.read(cx).top_source_line();
            (et, pb)
        });
        let final_pb = preview_pb_drag.expect("预览帧应就绪");
        let editor_block_drag = cx.update(|_, cx| mdw.read(cx).block_start_of_line(editor_top_drag));
        assert!(
            final_pb >= 20,
            "预览应已被拖到深处（final_pb={final_pb}）——夹具无效则断言无从谈起"
        );
        assert!(
            final_pb.saturating_sub(editor_block_drag) >= 12,
            "拖动中（未松手）编辑器不应逐帧跟随（preview 顶块 {final_pb} vs editor 顶块 {editor_block_drag}）",
        );

        // 松手 + 静止：一次收敛到最终预览顶块（±1 块对齐）。
        cx.simulate_mouse_up(
            gpui::point(x, overlay.top() + px(560.)),
            MouseButton::Left,
            Default::default(),
        );
        let mut converged: Option<(usize, usize)> = None;
        for _frame in 0..40 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
            let (et, ps) = cx.update(|_, cx| {
                let et = editor.read(cx).viewport_top_line();
                (et, mdw.read(cx).top_source_line())
            });
            if let Some(ps) = ps {
                let eb = cx.update(|_, cx| mdw.read(cx).block_start_of_line(et));
                if eb.abs_diff(ps) <= 1 {
                    converged = Some((eb, ps));
                    break;
                }
            }
        }
        let (eb_done, ps_done) =
            converged.expect("松手后编辑器应一次收敛到预览最终顶块（有界 40 帧）");
        assert!(
            ps_done >= final_pb.saturating_sub(2),
            "收敛目标应为延后期最终预览块（final={final_pb}, got={ps_done}）"
        );
        assert!(
            eb_done.abs_diff(ps_done) <= 1,
            "松开后两侧同块（editor={eb_done} vs preview={ps_done}）"
        );
        let _ = std::fs::remove_file(&md);
    }

    /// I2：速度边界——低频**单次**跨块（慢速单步滚 / 同块内微滚）**不**被误判为拖动：
    /// 单笔跨块照常立即跟随（与旧行为一致，编辑区逐帧对齐），不进入延后。
    #[gpui::test]
    async fn slow_single_preview_cross_follows_immediately(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1200.), px(800.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let md = std::env::temp_dir().join(format!(
            "gpui-sync-scrub-slow-{}.md",
            std::process::id()
        ));
        let mut doc = String::new();
        for i in 0..60 {
            doc.push_str(&format!("# 大标题 {i}\n\n正文章节 {i}。\n\n"));
        }
        std::fs::write(&md, &doc).unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(md.to_str().unwrap(), window, cx));
        });
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let (editor, mdw) = cx.update(|_, cx| {
            let a = app.read(cx);
            (
                a.model.borrow().tabs[a.model.borrow().active_tab].payload.editor.clone(),
                a.model.borrow().tabs[a.model.borrow().active_tab].payload.md_wy.clone().expect("预览应在场"),
            )
        });
        let vhand = cx.update(|_, cx| {
            let a = app.read(cx);
            a.model.borrow().tabs[a.model.borrow().active_tab].payload.md_wy_vscroll.clone()
        });
        let max = cx.update(|_w, _cx| vhand.max_offset().y.as_f32().max(100.0));

        // 单笔跨块 + 完整排空（慢速单步滚语义）：不进入延后，编辑器逐帧对齐。
        vhand.set_offset(gpui::point(gpui::px(0.), gpui::px(-max * 0.3)));
        let mut converged: Option<(usize, usize)> = None;
        for _frame in 0..30 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
            let (et, ps) = cx.update(|_, cx| {
                let et = editor.read(cx).viewport_top_line();
                (et, mdw.read(cx).top_source_line())
            });
            if let Some(ps) = ps {
                let eb = cx.update(|_, cx| mdw.read(cx).block_start_of_line(et));
                if eb.abs_diff(ps) <= 1 {
                    converged = Some((eb, ps));
                    break;
                }
            }
        }
        assert!(
            converged.is_some(),
            "单次跨块应照常立即跟随并对齐（不进入延后；速度相关边界）"
        );
        let _ = std::fs::remove_file(&md);
    }

    /// WYSIWYG（原生 `ScrollHandle` 宿主）滚动条拖拽：向下拖 -> 内容应**实时**向下滚，
    /// 向上拖 -> 实时反向滚。回归：自绘滚动条拖动只写 `live_vscroll` + notify，若宿主
    /// 原生滚动容器没有重渲染，内容会只在松开后才跳（用户报告「从上到下要松开才动」）。
    #[gpui::test]
    async fn wysiwyg_live_scrollbar_drag_follows_both_directions(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(700.), px(700.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let md = std::env::temp_dir().join(format!(
            "gpui-live-drag-{}.md",
            std::process::id()
        ));
        std::fs::write(&md, format!("{}\n", "# 标题\n正文。\n\n".repeat(400))).unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(md.to_str().unwrap(), window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.toggle_live_test(window, cx));
        });
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let overlay = cx
            .debug_bounds("live-vscrollbar-overlay")
            .expect("live 滚动条叠层应渲染");
        let vhand = cx.update(|_, cx| {
            let a = app.read(cx);
            a.model.borrow().tabs[a.model.borrow().active_tab].live_vscroll.clone()
        });
        let scroll_px = |cx: &mut VisualTestContext| {
            cx.update(|_w, _cx| (-vhand.offset().y).as_f32())
        };

        let before = scroll_px(cx);
        assert_eq!(before, 0.0, "初始应在顶部");

        let x = overlay.right() - px(8.);
        let grab_y = overlay.top() + px(14.);
        // 压下 thumb，**未松开**向下拖（分两步），内容必须边拖边滚。
        cx.simulate_mouse_down(
            gpui::point(x, grab_y),
            MouseButton::Left,
            Default::default(),
        );
        for _ in 0..2 {
            cx.run_until_parked();
        }
        let mid1_y = overlay.top() + px(120.);
        cx.simulate_mouse_move(
            gpui::point(x, mid1_y),
            MouseButton::Left,
            Default::default(),
        );
        // 只走 run_until_parked，不手动 refresh —— 真实 App 里同样只靠 notify 触发重绘。
        for _ in 0..4 {
            cx.run_until_parked();
        }
        let s1 = scroll_px(cx);
        assert!(
            s1 > before + 1.0,
            "向下拖(1)应先实时滚动（s1={s1:.0}）——不能等松开才动"
        );

        let mid2_y = overlay.top() + px(220.);
        cx.simulate_mouse_move(
            gpui::point(x, mid2_y),
            MouseButton::Left,
            Default::default(),
        );
        for _ in 0..4 {
            cx.run_until_parked();
        }
        let s2 = scroll_px(cx);
        assert!(
            s2 > s1 + 1.0,
            "继续向下拖(2)应继续实时滚动（s2={s2:.0} vs s1={s1:.0}）"
        );

        // 未松开向上拖回 → 实时反向滚动。
        cx.simulate_mouse_move(
            gpui::point(x, grab_y),
            MouseButton::Left,
            Default::default(),
        );
        for _ in 0..4 {
            cx.run_until_parked();
        }
        let s3 = scroll_px(cx);
        assert!(
            s3 < s2 - 1.0,
            "向上拖应实时反向滚动（s3={s3:.0} vs s2={s2:.0}）"
        );

        cx.simulate_mouse_up(
            gpui::point(x, grab_y),
            MouseButton::Left,
            Default::default(),
        );
        for _ in 0..4 {
            cx.run_until_parked();
        }
        let _ = std::fs::remove_file(&md);
    }

    /// 拖动编辑器纵向滚动条滑块：内容必须**跟随拖动**实时滚动，禁止「只有松开才跳」。
    /// 回归：自绘滚动条拖动只写 desired_scroll，若 `notify` 没让编辑器重渲染，
    /// 内容会停在原位直到 mouseup 后别的路径触发重绘。
    #[gpui::test]
    async fn scrollbar_drag_follows_content_live(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(900.), px(700.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let f = std::env::temp_dir().join(format!(
            "gpui-scrollbar-drag-{}.txt",
            std::process::id()
        ));
        std::fs::write(
            &f,
            (0..800)
                .map(|i| format!("line {i}"))
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(f.to_str().unwrap(), window, cx));
        });
        for _ in 0..8 {
            cx.run_until_parked();
        }

        let overlay = cx
            .debug_bounds("ce-vscrollbar-overlay")
            .expect("编辑器纵向滚动条叠层应渲染");
        let (before_top, before_desired) = cx.update(|_w, cx| {
            let a = app.read(cx);
            let ed = a.model.borrow().tabs[a.model.borrow().active_tab].payload.editor.read(cx);
            (ed.test_text_scroll_top(), ed.test_desired_scroll_px())
        });
        assert_eq!(before_top, (0, 0.0), "初始应在顶部");
        assert_eq!(before_desired, 0.0);

        // 点住滑块（右缘 16px 轨道，滑块顶部 = overlay.top + inset 4）…
        let thumb_x = overlay.right() - px(8.);
        let grab_y = overlay.top() + px(14.);
        cx.simulate_mouse_down(
            gpui::point(thumb_x, grab_y),
            MouseButton::Left,
            Default::default(),
        );
        for _ in 0..3 {
            cx.run_until_parked();
        }

        // …未松开就向下拖 100px → 内容必须已经跟随滚动。
        let drag_y = overlay.top() + px(114.);
        cx.simulate_mouse_move(
            gpui::point(thumb_x, drag_y),
            MouseButton::Left,
            Default::default(),
        );
        // 只走 run_until_parked，**不手动 refresh**——真实 App 里同样只靠 notify 触发重绘；
        // 若 notify 没把视图标脏，这里就会复现「拖动中内容不动，只有松开才跳」。
        for _ in 0..5 {
            cx.run_until_parked();
        }
        let mid = cx.update(|_w, cx| {
            let a = app.read(cx);
            let ed = a.model.borrow().tabs[a.model.borrow().active_tab].payload.editor.read(cx);
            (ed.test_text_scroll_top(), ed.test_desired_scroll_px())
        });
        assert!(
            mid.1 > before_desired + 1.0 || mid.0 != before_top,
            "拖动中内容应实时跟随（scroll_top={:?}, desired={:.1}）——不能等松开才动",
            mid.0,
            mid.1
        );

        cx.simulate_mouse_up(
            gpui::point(thumb_x, drag_y),
            MouseButton::Left,
            Default::default(),
        );
        for _ in 0..5 {
            cx.run_until_parked();
        }
        let _ = std::fs::remove_file(&f);
    }

    /// WYSIWYG Ctrl+滚轮缩放：字号放大/缩小后视图顶部**内容点**保持不变（视口锚定，
    /// 对齐 src/markdown/view 与 src/editor），而不是整体滚动偏离。
    #[gpui::test]
    async fn wysiwyg_zoom_keeps_viewport_anchored(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(600.), px(600.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 内容足够高 → 真实需要纵向滚动，且缩放后还有双向余量。
        let md = std::env::temp_dir().join(format!(
            "gpui-wy-zoom-{}.md",
            std::process::id()
        ));
        std::fs::write(&md, format!("{}\n", "# 标题\n正文段落内容用于缩放锚定验证。\n\n".repeat(200))).unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(md.to_str().unwrap(), window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.toggle_live_test(window, cx));
        });
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let vhand = cx.update(|_, cx| {
            let a = app.read(cx);
            a.model.borrow().tabs[a.model.borrow().active_tab].live_vscroll.clone()
        });
        // 滚到中深部（留出双向余量且让「无锚定漂移」足够大到可分辨）。
        vhand.set_offset(gpui::point(gpui::px(0.), gpui::px(-1500.)));
        for _ in 0..5 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let pad_y = crate::app::panes::LIVE_BODY_PAD_Y;
        let local_top = |y: f32| y.max(0.0) - pad_y;
        let read_anchor = |cx: &mut VisualTestContext| -> Option<(usize, f32)> {
            cx.update(|_, cx| {
                let a = app.read(cx);
                a.model.borrow().tabs[a.model.borrow().active_tab]
                    .live2
                    .as_ref()
                    .expect("live2 应就绪")
                    .read(cx)
                    .block_of_px(local_top(vhand.offset().y.as_f32().abs()))
            })
        };
        // 读**缩放前**视口顶部的 WYSIWYG 内容点（旧字号旧 frame）。
        let before = read_anchor(&mut *cx).expect("缩放前应有可识别锚点块");
        // 放大 3 档（等价 Ctrl+向上滚 3 次），多泵几帧让「估计高→整形真值」收敛完成。
        cx.update(|_, cx| {
            app.update(cx, |app, cx| app.change_font_size(3.0, cx));
        });
        for _ in 0..20 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        // 读缩放后视口顶部内容点；线性锚点近似允许 ±1 块（对齐 markdown/view ±2 容差）。
        let after = read_anchor(&mut *cx).expect("缩放后也应能识别锚点块");
        assert!(
            (before.0 as i64 - after.0 as i64).abs() <= 1,
            "缩放不应明显改变视口顶部块：{before:?} → {after:?}"
        );
        assert!(
            (before.1 - after.1).abs() < 0.3,
            "缩放后块内比例应基本不变：{:.3} → {:.3}",
            before.1,
            after.1
        );
        let _ = std::fs::remove_file(&md);
    }

