    #[gpui::test]
    async fn md_wy_preview_host_renders_for_md(cx: &mut TestAppContext) {
        // A2：Markdown 右栏预览切统一组件 WyEditor（只读）——live-area 宿主（滚动容器 +
        // 自绘纵向滚动条）应渲染，且 `md_wy` 实体被创建、只读门控生效、源文本就位。
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(500.), px(500.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }

        let long_line = "f".repeat(500);
        let dir = std::env::temp_dir().join("gpui-editor-hscroll-test.md");
        std::fs::write(&dir, format!("```rs\n{long_line}\n```\n")).unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(dir.to_str().unwrap(), window, cx));
        });
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }

        // 宿主三件套：分屏外框 + 滚动容器 + 自绘竖滚动条叠层。
        assert!(
            cx.debug_bounds("preview-md-wy").is_some(),
            "MD WyEditor 预览外框 preview-md-wy 应渲染"
        );
        let scroll = cx
            .debug_bounds("md-wy-scroll")
            .expect("宿主滚动容器 md-wy-scroll 应渲染");
        assert!(
            scroll.size.width > px(0.) && scroll.size.height > px(0.),
            "宿主滚动容器应有非零尺寸，实际={scroll:?}"
        );
        assert!(
            cx.debug_bounds("md-wy-vscrollbar-overlay").is_some(),
            "自绘纵向滚动条叠层 md-wy-vscrollbar-overlay 应渲染"
        );
        // 实体级：只读门控 + 源文本就位（含超宽代码行）。
        let (editable, text) = cx.update(|_w, cx| {
            let a = app.read(cx);
            let t = &a.model.borrow().tabs[a.model.borrow().active_tab];
            let ed = t.payload.md_wy.clone().expect("md 打开应创建右栏 WyEditor 预览");
            (ed.read(cx).is_editable(), ed.read(cx).text().to_string())
        });
        assert!(!editable, "右栏 WyEditor 预览应只读");
        assert!(!text.is_empty(), "右栏 WyEditor 预览应收满源文本");
        let _ = std::fs::remove_file(&dir);
    }

    /// Step2：大文档 md 冷开「打开即骨架占位」——右栏 `md_wy` 未就绪（后台整篇解析在途）
    /// 时渲染「正在排版 Markdown 预览…」占位（打开一瞬间即有视觉内容，不再右栏全白等
    /// 几秒）；`install_open`（`is_loaded()==true`）后同一帧起切回正式宿主——占位外框与
    /// 正式宿主同名（`#preview-md-wy`），切替不改变窗口列宽布局。
    ///
    /// 测试说明：本测试钩子的 `run_until_parked` 会一次排空全部后台任务，真实 open_file
    /// 的「首块已挂、解析在途」窗口无法在帧间观察到 → 用**等价状态布置**（`ensure_md_wy
    /// (feed=false)` 建空壳 = 大文档首块挂载后的待装配态）走真实渲染路径断言占位 →
    /// 就绪切换；真实 open 的「提前解析装好 + EOF 不重复 install」另由
    /// `large_md_open_early_install_skips_eof_double` 覆盖。
    #[gpui::test]
    async fn md_wy_pending_placeholder_renders_until_loaded(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 建「空壳未装」的右栏 WyEditor：等价大文档打开首块挂载后、整篇解析完成前。
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                let idx = app.model.borrow().active_tab;
                app.ensure_md_wy(idx, window, cx, false);
                // 等价大文档 md 打开：预览打开（空壳已建、正文待后台装）。
                app.model.borrow_mut().tabs[idx].payload.preview_on = true;
            });
        });
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let (loaded,) = cx.update(|_, cx| {
            let a = app.read(cx);
            let wy = a.model.borrow().tabs[a.model.borrow().active_tab].payload.md_wy.clone().expect("空壳应已创建");
            (wy.read(cx).is_loaded(),)
        });
        let (label_shown, scroll_shown) = (
            cx.debug_bounds("md-wy-pending-label").is_some(),
            cx.debug_bounds("md-wy-scroll").is_some(),
        );
        assert!(!loaded, "空壳应处于未就绪");
        assert!(
            label_shown && !scroll_shown,
            "md_wy 未就绪时应渲染「正在排版」占位（#md-wy-pending-label），且不得出现正式宿主 #md-wy-scroll"
        );
        // 就绪：install 全量 → 下一帧起正式宿主出现、占位消失。
        cx.update(|_window, cx| {
            app.update(cx, |app, cx| {
                let wy = app.model.borrow().tabs[app.model.borrow().active_tab].payload.md_wy.clone().unwrap();
                wy.update(cx, |ed, _cx| {
                    ed.install_open(markdown_wysiwyg::WyEditor::open_from_str(
                        "# 就绪正文\n\n解析完成后的预览内容。\n".into(),
                    ));
                });
            });
        });
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let (loaded,) = cx.update(|_, cx| {
            let a = app.read(cx);
            let wy = a.model.borrow().tabs[a.model.borrow().active_tab].payload.md_wy.clone().expect("md_wy 应仍在");
            (wy.read(cx).is_loaded(),)
        });
        let (label_shown, scroll_shown) = (
            cx.debug_bounds("md-wy-pending-label").is_some(),
            cx.debug_bounds("md-wy-scroll").is_some(),
        );
        assert!(loaded, "install_open 后应就绪");
        assert!(
            scroll_shown && !label_shown,
            "就绪后应渲染正式宿主（#md-wy-scroll），占位消失"
        );
    }

    /// 回归（用户拍板方案「右侧明显留白」）：右栏 WyEditor 预览宿主（`#md-wy-body`）
    /// 给内容**右侧**留出明显可感的间距——`.md` 预览标题/正文最右字不贴右缘、不被自绘
    /// 滚动条带盖住。宿主水平内衬走**单一来源** `PREVIEW_SIDE_GAP_PX`（与 `markdown::view`
    /// 同一常量）；gap 在滚动条叠层**内侧**（叠层贴外层右缘、不被推出）。真实渲染路径
    /// 取证：宿主盒右缘 − 元素右缘 = gap、左右对称，而 WyEditor 帧内容（含标题最右字形）
    /// 恒在元素盒内 ⇒ 最右字右缘 ≤ 宿主右缘 − gap（窄分屏下列占满可用宽、列=可用宽）。
    #[gpui::test]
    async fn md_wy_preview_host_right_gap_is_visible(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        // 窄分屏（预览列 < 72ch 阅读栏，列=可用宽）——标题贴右缘截断的必现档。
        cx.simulate_resize(gpui::size(px(900.), px(700.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let gap = config::markdown::PREVIEW_SIDE_GAP_PX;

        // 打开（真实 open_file 路径，等待后台装配）+ 宿主右缘间隙几何断言。
        let open_and_check =
            |vcx: &mut VisualTestContext, p: &std::path::Path, target_finish: usize| {
                vcx.update(|window, cx| {
                    app.update(cx, |app, cx| {
                        app.open_file(p.to_str().unwrap(), window, cx)
                    });
                });
                let mut frames = 0;
                loop {
                    vcx.cx
                        .executor()
                        .advance_clock(std::time::Duration::from_millis(20));
                    vcx.run_until_parked();
                    let _ = vcx.cx.refresh();
                    let done = vcx
                        .update(|_, cx| app.read(cx).open_finish_count >= target_finish);
                    if done && frames > 6 {
                        break;
                    }
                    frames += 1;
                    if frames > 160 {
                        break;
                    }
                }
                let body = vcx
                    .debug_bounds("md-wy-body")
                    .expect("预览宿主 md-wy-body 应渲染");
                let overlay = vcx
                    .debug_bounds("md-wy-vscrollbar-overlay")
                    .expect("自绘滚动条叠层应渲染");
                // 滚动条叠层贴宿主右缘：gap 在滚动条**内侧**，未把滚动条推出。
                assert!(
                    (overlay.right() - body.right()).abs() <= px(1.5),
                    "滚动条叠层应贴住宿主右缘：overlay.right={:?} body.right={:?}",
                    overlay.right(),
                    body.right()
                );
                // 元素盒 = 宿主内容盒（`.w_full().px(gap)`）：左/右边距都 = gap。
                let eb = vcx
                    .update(|_w, cx| {
                        let a = app.read(cx);
                        let ed = a.model.borrow().tabs[a.model.borrow().active_tab]
                            .payload.md_wy
                            .clone()
                            .expect("md 打开应创建右栏 WyEditor 预览");
                        ed.read(cx)
                            .last_bounds()
                            .expect("渲染后应有元素 bounds（窗口坐标）")
                    });
                let left_gap = (eb.origin.x - body.origin.x).as_f32();
                let right_gap = (body.right() - eb.right()).as_f32();
                assert!(
                    (left_gap - gap).abs() <= 1.0,
                    "宿主左侧内衬应 = PREVIEW_SIDE_GAP_PX({gap}px)，实际 {left_gap:.1}px"
                );
                assert!(
                    (right_gap - gap).abs() <= 1.0,
                    "宿主右侧内衬应 = PREVIEW_SIDE_GAP_PX({gap}px)（最右字右缘 ≤ 宿主右缘 − gap），实际 {right_gap:.1}px"
                );
                assert!(
                    (left_gap - right_gap).abs() <= 1.0,
                    "宿主内衬应左右对称：left={left_gap:.1} right={right_gap:.1}"
                );
                eprintln!(
                    "[gap] md-wy 宿主右缘间隙：left={left_gap:.1}px right={right_gap:.1}px（gap={gap}px）{}",
                    p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
                );
            };

        let dir = std::env::temp_dir().join(format!("gpui-mdwy-gap-{}.md", std::process::id()));
        std::fs::write(
            &dir,
            "# 最右侧不得被截断的超长标题一行\n\n正文段落，右侧也应留出明显间距，不贴右缘。\n",
        )
        .unwrap();
        open_and_check(&mut *cx, &dir, 1);
        let _ = std::fs::remove_file(&dir);

        // 真实大文档（存在才跑，锁定用户实测路径）：同几何补跑，标题行最右字右缘
        // 不贴宿主右缘。
        let big = std::path::Path::new(
            r"D:\WorkSpace\1\rust\gpui\gpui-editor\target\大文档测试2.md",
        );
        if big.exists() {
            let dst = std::env::temp_dir().join(format!(
                "gpui-mdwy-gap-big-{}.md",
                std::process::id()
            ));
            std::fs::copy(big, &dst).unwrap();
            open_and_check(&mut *cx, &dst, 2);
            let _ = std::fs::remove_file(&dst);
        }
    }

    /// 回归：切换黑白主题后，右栏统一 WyEditor 预览（`md_wy`）必须整份重染色。
    /// `WyTheme` 在创建时从 [`markdown::view::style::StyleParams`] 一次性派生并
    /// 缓存（`WyEditor::new`）；若 `refresh_theme_ui` 不重建，亮→暗后预览文字/代码块/
    /// 链接仍保持旧亮色版（黑字压 #121314 编辑区底上几乎不可见）——即
    /// 用户报的「切换黑白后预览不完全重染色 / 用旧色」。用真切换路径断言预览主题
    /// 逐项切换到暗色 `StyleParams`、再切回亮色板。
    #[gpui::test]
    async fn md_wy_preview_rethemes_on_theme_toggle(cx: &mut TestAppContext) {
        init_test(cx);
        // 固定亮色起（全局共享，避免污染其它测试）。
        cx.update(|cx| ui::theme::Theme::set(ui::theme::ThemeMode::Light, cx));
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let md = std::env::temp_dir().join(format!("gpui-mdwy-theme-{}.md", std::process::id()));
        std::fs::write(&md, "# 标题\n\n正文内容，以及 ```rs\nlet x = 1;\n``` 代码块\n").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(md.to_str().unwrap(), window, cx));
        });
        for _ in 0..10 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let read_theme = |cx: &mut gpui::App| {
            let a = app.read(cx);
            let ed = a.model.borrow().tabs[a.model.borrow().active_tab].payload.md_wy.clone()?;
            let e = ed.read(cx);
            Some((
                e.test_theme_fg(),
                e.test_theme_code_bg(),
                e.test_theme_link(),
                e.test_theme_is_dark(),
            ))
        };
        let light = cx
            .update(|_w, cx| read_theme(cx).expect("右栏预览应已创建（亮色）"));
        assert!(!light.3, "初始应为亮主题");
        // 切暗色（真切换路径 refresh_theme_ui）。
        cx.update(|_w, cx| {
            app.update(cx, |a, cx| {
                a.model.borrow_mut().dark_mode = true;
                a.refresh_theme_ui(cx);
                cx.notify();
            });
        });
        for _ in 0..6 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let dark = cx
            .update(|_w, cx| read_theme(cx).expect("右栏预览应仍存在（暗色）"));
        assert!(dark.3, "切换暗色后预览主题应标记 is_dark");
        assert_ne!(
            (dark.0, dark.1, dark.2),
            (light.0, light.1, light.2),
            "预览正文/代码/链接色必须随主题切换（不再是旧亮色板）"
        );
        // 新色板与暗色 StyleParams 单一来源逐项一致。
        let sp_dark = cx.update(|_w, cx| {
            markdown::view::style::StyleParams::from_theme_with_font(cx, None)
        });
        assert_eq!(dark.0, sp_dark.fg, "预览正文色应=暗色 StyleParams.fg");
        assert_eq!(dark.1, sp_dark.code_bg, "预览代码底色应=暗色 StyleParams.code_bg");
        assert_eq!(dark.2, sp_dark.link, "预览链接色应=暗色 StyleParams.link");
        // 切回亮色。
        cx.update(|_w, cx| {
            app.update(cx, |a, cx| {
                a.model.borrow_mut().dark_mode = false;
                a.refresh_theme_ui(cx);
                cx.notify();
            });
        });
        for _ in 0..6 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let light2 = cx
            .update(|_w, cx| read_theme(cx).expect("右栏预览应仍存在（切回亮色）"));
        assert!(!light2.3, "切回亮色后预览应标记非暗色");
        assert_eq!(
            (light2.0, light2.1, light2.2),
            (light.0, light.1, light.2),
            "切回亮色应回到亮色板"
        );
        let _ = std::fs::remove_file(&md);
    }

    /// 回归守卫（issue-2）：预览宿主容器应恰好铺满正文区视口高
    /// （= 窗高 − 标题栏 − 选项卡栏 − 状态栏），滚动容器与预览外框等高、底不越到
    /// 状态栏——不塌缩（内容不可见）也不超高（底部被状态栏/窗口裁剪）。
    #[gpui::test]
    async fn preview_pane_content_box_fills_viewport(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1400.), px(900.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let md = std::env::temp_dir().join(format!("gpui-pane-h-{}.md", std::process::id()));
        std::fs::write(&md, "正文行\n".repeat(300)).unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(md.to_str().unwrap(), window, cx));
        });
        for _ in 0..10 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let (pv, sc, tb, st) = {
            let pv = cx
                .debug_bounds("preview-md-wy")
                .expect("预览外框 preview-md-wy 应渲染");
            let sc = cx
                .debug_bounds("md-wy-scroll")
                .expect("预览滚动容器 md-wy-scroll 应渲染");
            let tb = cx
                .debug_bounds("tab-bar")
                .expect("选项卡栏应渲染");
            let st = cx
                .debug_bounds("status-bar")
                .expect("状态栏应渲染");
            (pv, sc, tb, st)
        };
        let win_h = 900.0f32;
        let title_h = 32.0f32;
        let expect_h = win_h - title_h - tb.size.height.as_f32() - st.size.height.as_f32();
        assert!(
            (pv.size.height.as_f32() - expect_h).abs() < 2.0,
            "预览外框高应=正文区视口高({expect_h:.0})，实际 {pv:?}"
        );
        assert_eq!(
            sc.size.height, pv.size.height,
            "滚动容器应与预览外框等高（不塌缩/不超高），sc={sc:?} pv={pv:?}"
        );
        assert!(
            pv.bottom().as_f32() <= win_h - st.size.height.as_f32() + 1.0,
            "预览底不得越到状态栏（无裁剪），pv={pv:?}"
        );
        let _ = std::fs::remove_file(&md);
    }

    /// A2：右栏 WyEditor 预览的只读门控——`replace_range` 等编辑入口在预览实体上
    /// 一律短路（第 #1 条：右栏只能看/选/复制，绝不能改到预览内容）。
    #[gpui::test]
    async fn md_wy_preview_readonly_blocks_editing(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let md = std::env::temp_dir().join(format!("gpui-mdwy-ro-{}.md", std::process::id()));
        std::fs::write(&md, "初始内容\n").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(md.to_str().unwrap(), window, cx));
        });
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let wy = cx.update(|_, cx| {
            let a = app.read(cx);
            a.model.borrow().tabs[a.model.borrow().active_tab].payload.md_wy.clone().expect("应有右栏 WyEditor 预览")
        });
        let (editable, before) = cx.update(|_, cx| {
            (wy.read(cx).is_editable(), wy.read(cx).text().to_string())
        });
        assert!(!editable, "右栏预览应只读");
        // 各种编辑入口在只读预览上全部短路（内容不变）。
        cx.update(|_, cx| {
            wy.update(cx, |ed, cx| ed.replace_range(0..2, "X", cx));
        });
        let after = cx.update(|_, cx| wy.read(cx).text().to_string());
        assert_eq!(before, after, "只读预览不应被 replace_range 改动");
        let _ = std::fs::remove_file(&md);
    }

    /// A2：按眼睛/菜单翻转预览——开启时创建并喂源右栏 WyEditor 预览（源 = 编辑区文本），
    /// 关闭时释放（`md_wy = None`，内存回基线），再开重建并重新喂源。
    #[gpui::test]
    async fn md_wy_preview_fed_on_open_and_released_on_toggle(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let md = std::env::temp_dir().join(format!("gpui-mdwy-tog-{}.md", std::process::id()));
        std::fs::write(&md, "第一段\n第二段\n").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(md.to_str().unwrap(), window, cx));
        });
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        // 打开即创建 + 喂源（源 = 编辑区文本）。
        let (fed, src) = cx.update(|_, cx| {
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
        assert!(!fed.is_empty(), "打开 md 应创建右栏 WyEditor 预览");
        assert_eq!(fed, src, "右栏 WyEditor 预览源应与编辑区一致");

        // 关预览 → 释放（md_wy=None）。
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                let active_idx = app.model.borrow().active_tab;
                app.toggle_preview_for_tab(active_idx, window, cx)
            });
        });
        for _ in 0..4 {
            cx.run_until_parked();
        }
        let off = cx.update(|_, cx| app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab].payload.md_wy.is_none());
        assert!(off, "关闭预览应释放右栏 WyEditor 预览实体");

        // 再开 → 重建 + 重新喂源。
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                let active_idx = app.model.borrow().active_tab;
                app.toggle_preview_for_tab(active_idx, window, cx)
            });
        });
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let (on, fed2, src2) = cx.update(|_, cx| {
            let a = app.read(cx);
            let t = &a.model.borrow().tabs[a.model.borrow().active_tab];
            (
                t.payload.md_wy.is_some(),
                t.payload.md_wy
                    .as_ref()
                    .map(|w| w.read(cx).text().to_string())
                    .unwrap_or_default(),
                t.payload.editor.read(cx).value().to_string(),
            )
        });
        assert!(on, "再开预览应重建右栏 WyEditor 预览");
        assert_eq!(fed2, src2, "重建预览应重新喂源（与编辑区一致）");
        let _ = std::fs::remove_file(&md);
    }

    /// A3：逐键增量桥——编辑区每次编辑（Paste 触发 Change）都重放到右栏 WyEditor 预览，
    /// 预览文本恒等于编辑区文本（实时跟随，非重装）。
    #[gpui::test]
    async fn md_wy_preview_follows_editor_edits_incrementally(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let p = std::env::temp_dir().join(format!("gpui-mdwy-bridge-{}.md", std::process::id()));
        std::fs::write(&p, "# 初始标题\n正文\n").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(p.to_str().unwrap(), window, cx));
        });
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let par = |cx: &mut VisualTestContext| {
            cx.update(|_w, cx| {
                let a = app.read(cx);
                let t = &a.model.borrow().tabs[a.model.borrow().active_tab];
                (
                    t.payload.md_wy
                        .as_ref()
                        .map(|w| w.read(cx).text().to_string())
                        .unwrap_or_default(),
                    t.payload.editor.read(cx).value().to_string(),
                )
            })
        };
        let (m0, e0) = par(cx);
        assert_eq!(m0, e0, "初始预览应与编辑区一致");

        // 聚焦编辑区 + 粘贴 → Change → 增量桥重放。
        cx.update(|window, cx| {
            let focus = app.read(cx).editor_focus_handle(cx);
            window.focus(&focus, cx);
        });
        cx.write_to_clipboard(gpui::ClipboardItem::new_string("新增段落".to_string()));
        cx.dispatch_action(Paste);
        for _ in 0..20 {
            cx.run_until_parked();
        }
        let (m1, e1) = par(cx);
        assert_eq!(m1, e1, "粘贴后预览应实时跟随编辑区，md_wy={m1:?} editor={e1:?}");
        assert!(m1.contains("新增段落"), "预览应含新贴入文本，实际 {m1:?}");

        // 再贴一次 → 仍保持一致。
        cx.write_to_clipboard(gpui::ClipboardItem::new_string("第二处".to_string()));
        cx.dispatch_action(Paste);
        for _ in 0..20 {
            cx.run_until_parked();
        }
        let (m2, e2) = par(cx);
        assert_eq!(m2, e2, "连续多次编辑后预览仍应与编辑区一致");
        let _ = std::fs::remove_file(&p);
    }

    /// A3：增量桥随撤销/重做的逆编辑同步回退/重放——预览在每一步都与编辑区一致。
    #[gpui::test]
    async fn md_wy_preview_follows_editor_undo_and_redo(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let p = std::env::temp_dir().join(format!("gpui-mdwy-undoredo-{}.md", std::process::id()));
        std::fs::write(&p, "一行\n").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(p.to_str().unwrap(), window, cx));
        });
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        cx.update(|window, cx| {
            let focus = app.read(cx).editor_focus_handle(cx);
            window.focus(&focus, cx);
        });
        cx.write_to_clipboard(gpui::ClipboardItem::new_string("X".to_string()));
        cx.dispatch_action(Paste);
        for _ in 0..20 {
            cx.run_until_parked();
        }
        let md_text = |cx: &mut VisualTestContext| {
            cx.update(|_w, cx| {
                let a = app.read(cx);
                a.model.borrow().tabs[a.model.borrow().active_tab]
                    .payload.md_wy
                    .clone()
                    .expect("应有 md_wy")
                    .read(cx)
                    .text()
                    .to_string()
            })
        };
        let ed_text = |cx: &mut VisualTestContext| {
            cx.update(|_w, cx| app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab].payload.editor.read(cx).value().to_string())
        };
        assert!(ed_text(cx).contains('X'), "基线：粘贴生效");
        assert_eq!(md_text(cx), ed_text(cx), "粘贴后预览应含 X");

        // 撤销：editor 与 md_wy 同步回退到无 X。
        cx.dispatch_action(Undo);
        for _ in 0..20 {
            cx.run_until_parked();
        }
        assert!(!ed_text(cx).contains('X'), "撤销后编辑区不含 X");
        assert_eq!(md_text(cx), ed_text(cx), "撤销后预览应与编辑区一致（同步回退）");

        // 重做：同步恢复 X。
        cx.dispatch_action(Redo);
        for _ in 0..20 {
            cx.run_until_parked();
        }
        assert!(ed_text(cx).contains('X'), "重做后编辑区含 X");
        assert_eq!(md_text(cx), ed_text(cx), "重做后预览应与编辑区一致（同步恢复）");
        let _ = std::fs::remove_file(&p);
    }

    /// A3：混合编辑（粘贴 + 换行结构变更 + 再次粘贴）后预览仍与编辑区逐字节一致——
    /// 增量桥走 `Doc::edit` 结构/非结构两路都必须对齐。
    #[gpui::test]
    async fn md_wy_preview_bridge_parity_after_mixed_edits(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let p = std::env::temp_dir().join(format!("gpui-mdwy-mix-{}.md", std::process::id()));
        std::fs::write(&p, "- 项目 A\n- 项目 B\n").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(p.to_str().unwrap(), window, cx));
        });
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        cx.update(|window, cx| {
            let focus = app.read(cx).editor_focus_handle(cx);
            window.focus(&focus, cx);
        });
        let par = |cx: &mut VisualTestContext| {
            cx.update(|_w, cx| {
                let a = app.read(cx);
                let t = &a.model.borrow().tabs[a.model.borrow().active_tab];
                (
                    t.payload.md_wy
                        .as_ref()
                        .map(|w| w.read(cx).text().to_string())
                        .unwrap_or_default(),
                    t.payload.editor.read(cx).value().to_string(),
                )
            })
        };
        // 粘贴（普通插入）→ 换行（结构变更）→ 粘贴（再次普通插入）。
        for clip in ["追加甲", "\n", "追加乙"] {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(clip.to_string()));
            cx.dispatch_action(Paste);
            for _ in 0..20 {
                cx.run_until_parked();
            }
            let (m, e) = par(cx);
            assert_eq!(m, e, "混合编辑后预览仍应与编辑区逐字节一致，editor={e:?} md_wy={m:?}");
        }
        let _ = std::fs::remove_file(&p);
    }

    /// Markdown 编辑区与预览区默认/始终自动换行，不提供切换 → 状态栏隐藏「换行/不换行」按钮；
    /// 普通文本文件仍保留该切换按钮。
    #[gpui::test]
    async fn wrap_button_hidden_for_markdown(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(500.), px(500.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }

        // md 文件：应隐藏换行按钮
        let md = std::env::temp_dir().join("gpui-editor-wrap-test.md");
        std::fs::write(&md, "# 标题\n正文\n").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(md.to_str().unwrap(), window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        assert!(
            cx.debug_bounds("status-wrap-btn").is_none(),
            "Markdown 文件预览时状态栏不应出现换行开关"
        );
        // A4：MD 预览走统一组件 WyEditor（自动换行由组件默认保障，不再有 md_preview 可查）。
        let wy_active = cx.update(|_w, cx| {
            let a = app.read(cx);
            a.model.borrow().tabs[a.model.borrow().active_tab].payload.md_wy.is_some()
        });
        assert!(wy_active, "刚打开 Markdown 预览应走统一组件（WyEditor 自动换行）");
        let _ = std::fs::remove_file(&md);

        // 普通文本文件：应保留换行按钮
        let txt = std::env::temp_dir().join("gpui-editor-wrap-test.txt");
        std::fs::write(&txt, "hello world\n").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(txt.to_str().unwrap(), window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        assert!(
            cx.debug_bounds("status-wrap-btn").is_some(),
            "普通文本文件状态栏应保留换行开关"
        );
        let _ = std::fs::remove_file(&txt);
    }

    /// 回归：「查看」→「自动换行」只作用编辑区，不改变 Markdown 预览的换行状态。
    /// 旧逻辑 toggle_wrap 以 `preview_on || wrap_enabled` 算 editor wrap——Markdown
    /// 打开即 preview_on=true → 编辑区恒为换行、"开关只动预览"；现在去掉预览联动。
    #[gpui::test]
    async fn wrap_toggle_affects_md_editor_only(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(500.), px(500.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let md = std::env::temp_dir().join("gpui-editor-wrap-toggle.md");
        std::fs::write(&md, "# 标题\n正文\n").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(md.to_str().unwrap(), window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        // 初始：全局编辑区换行偏好默认关闭（仅 md 编辑区默认换行）；MD 预览走统一组件。
        let (wy0, pref0) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (a.model.borrow().tabs[a.model.borrow().active_tab].payload.md_wy.is_some(), a.model.borrow().wrap_enabled)
        });
        assert!(wy0, "initial: MD 预览应走统一组件 WyEditor（自动换行由组件保障）");
        assert!(!pref0, "全局编辑区换行偏好应默认关闭（仅 md 编辑区换行），pref0={pref0}");

        // 切换换行：只作用编辑区 → 预览组件不变、全局偏好开启
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.toggle_wrap(window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let pref1 = cx.update(|_w, cx| app.read(cx).model.borrow().wrap_enabled);
        let host1 = cx.debug_bounds("preview-md-wy").is_some();
        assert!(
            host1,
            "切换「自动换行」不应动 Markdown 预览（统一组件宿主仍渲染）"
        );
        assert!(pref1, "切换一次后全局编辑区换行偏好应开启");

        // 再切回：预览仍渲染、偏好恢复关闭
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.toggle_wrap(window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let pref2 = cx.update(|_w, cx| app.read(cx).model.borrow().wrap_enabled);
        let host2 = cx.debug_bounds("preview-md-wy").is_some();
        assert!(host2, "再切换后预览应仍渲染");
        assert!(!pref2, "再切换后全局编辑区换行偏好应恢复关闭");
        let _ = std::fs::remove_file(&md);
    }

    /// WYSIWYG（预览编辑测试 / WyEditor）模式：宿主 `#live-scroll` 提供纵向滚动，
    /// 右侧 `Scrollbar::vertical` 自绘**可见纵向滚动条**（此 GPUI 的 `overflow_y_scroll`
    /// 不绘制原生滚动条）。回归：切到 live 模式后滚动条叠层应存在。
    #[gpui::test]
    async fn wysiwyg_live_mode_has_vertical_scrollbar(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(600.), px(600.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let md = std::env::temp_dir().join(format!(
            "gpui-wy-vscroll-{}.md",
            std::process::id()
        ));
        // 内容足够高 → 真实需要纵向滚动。
        std::fs::write(&md, format!("{}\n", "# 标题\n正文\n".repeat(300))).unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(md.to_str().unwrap(), window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        // 打开前：非 live 模式无该滚动条。
        assert!(
            cx.debug_bounds("live-vscrollbar-overlay").is_none(),
            "源码编辑模式不应有 WYSIWYG 滚动条"
        );
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.toggle_live_test(window, cx));
        });
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        assert!(
            cx.debug_bounds("live-scroll").is_some(),
            "WYSIWYG 宿主纵向滚动容器应存在"
        );
        assert!(
            cx.debug_bounds("live-vscrollbar-overlay").is_some(),
            "WYSIWYG 模式应渲染自绘纵向滚动条叠层"
        );
        let _ = std::fs::remove_file(&md);
    }

    /// 回归：用户报告的「编辑→切换」panic 路径（`toggle_live_test` ON → WYSIWYG 里
    /// 编辑含 CJK/emoji 的行内样式文本 → 切 OFF（`commit_live` 写回源码编辑区与预览）
    /// → 再切 ON）。历史上该往返在 run 边界非 UTF-8 字符边界时于 GPUI
    /// `StyledText::with_highlights` 崩（`assertion failed: is_char_boundary(run.start)`，
    /// 预览 `char_align_range` 已防御，WYSIWYG `runs_to_text_runs` 缺防御）。本测试用
    /// 真实 `target/测试.md`（CJK/链接/粗体/emoji 满篇）走完整开关往返，确保不 panic
    /// 且内容往返一致。
    #[gpui::test]
    async fn wysiwyg_toggle_edit_cjk_roundtrip_does_not_panic(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1000.), px(800.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let md = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/测试.md");
        // 缺真实夹具（手动装配的 target/测试.md）时 SKIP：SKIP 是常态不是回归（AGENTS §5）。
        let Ok(src_expected) = std::fs::read_to_string(&md) else {
            eprintln!("SKIP: missing {}", md.display());
            return;
        };
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(md.to_str().unwrap(), window, cx));
        });
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        // ON：进入 WYSIWYG（live2）实时编辑模式。
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.toggle_live_test(window, cx));
        });
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        // 在 WYSIWYG 文档开头插入一行含 CJK/emoji/行内样式的文本（编辑后的多字节内容）。
        let inserted = "# 切换后新增❗🍎 **测试**\n";
        cx.update(|_w, cx| {
            let live = app
                .read(cx)
                .model.borrow().tabs[app.read(cx).model.borrow().active_tab]
                .live2
                .clone()
                .expect("live2 应存在");
            live.update(cx, |e, cx| {
                e.replace_range(0..0, inserted, cx);
            });
        });
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let live_text = cx.update(|_w, cx| {
            app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab]
                .live2
                .clone()
                .expect("live2 应存在")
                .read(cx)
                .text()
                .to_string()
        });
        assert!(live_text.starts_with("# 切换后新增❗🍎 **测试**\n"), "WYSIWYG 编辑应生效：{live_text:?}");
        // OFF：commit_live 写回源码编辑区 + Markdown 预览（此步历史上触发 run 边界 panic）。
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.toggle_live_test(window, cx));
        });
        for _ in 0..10 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let committed = cx.update(|_w, cx| {
            let a = app.read(cx);
            a.model.borrow().tabs[a.model.borrow().active_tab].payload.editor.read(cx).value().to_string()
        });
        assert_eq!(committed, format!("{inserted}{src_expected}"), "切换 OFF 应把 WYSIWYG 全文写回编辑区");
        // ON 再切回：WYSIWYG 以已提交文本重建，往复一致、不 panic。
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.toggle_live_test(window, cx));
        });
        for _ in 0..10 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let re_on = cx.update(|_w, cx| {
            let a = app.read(cx);
            a.model.borrow().tabs[a.model.borrow().active_tab]
                .live2
                .as_ref()
                .map(|w| w.read(cx).text().to_string())
                .unwrap_or_default()
        });
        assert_eq!(re_on, committed, "再次切换 ON 后 WYSIWYG 内容应与写回内容一致");
    }

    /// 回归（用户报告）：**仅移动鼠标**自动选中。复现：编辑/预览界面点「预览编辑测试」
    /// 进 WYSIWYG，再点切回编辑/预览，随后不按任何键只移动鼠标 → 文本从 ln1 col1
    /// （offset 0）被自动选中到指针处。
    ///
    /// 根因链路（GPUI 派发模型）：`dispatch_mouse_event`（gpui window.rs）对本帧所有已
    /// 注册鼠标 listener 按 `hitbox.is_hovered` 逐个派发、**不按 z-order 裁掉被菜单遮挡
    /// 的区间** → 点菜单项的 MouseDown 会漏进编辑区 `on_mouse_down` 置 `dragging=true`；
    /// 而菜单项 `PopupMenu::activate`（src/ui/menu.rs）`stop_propagation()` 截断后续派发
    /// → 编辑区 `on_mouse_up` 收不到，`dragging` 残留。`commit_live` 的 `set_text` 只把
    /// 选区收拢到 0、不清 `dragging` → 切回后一次 MouseMove 就把选区拉长。
    ///
    /// 本测试走真实渲染/事件路径：在编辑区按下（模拟菜单项漏进来的 MouseDown，**不**
    /// 派发 MouseUp）→ 切换 ON → 切换 OFF（`commit_live` 写回）→ 仅 MouseMove →
    /// 断言选区仍为 0..0（拖选状态已被复位）。
    #[gpui::test]
    async fn wysiwyg_toggle_off_leaves_no_stale_mouse_drag(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1000.), px(800.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let md = std::env::temp_dir().join(format!(
            "gpui-wy-stale-drag-{}.md",
            std::process::id()
        ));
        std::fs::write(&md, "# 标题\n正文段落。\n\n第二段内容。\n".repeat(3)).unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(md.to_str().unwrap(), window, cx));
        });
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let editor = cx.update(|_w, cx| app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab].payload.editor.clone());
        // 编辑区首格内按下：模拟菜单项 MouseDown 漏进编辑区。不派发 MouseUp，对应真实
        // 场景里 MouseUp 被菜单项 `activate` 的 stop_propagation 截断、到不了编辑器。
        let top_left = cx.debug_bounds("code-editor").expect("编辑区应渲染").origin;
        let at = gpui::point(top_left.x + px(4.), top_left.y + px(4.));
        let mods = gpui::Modifiers::default();
        cx.simulate_mouse_move(at, MouseButton::Left, mods);
        cx.simulate_mouse_down(at, MouseButton::Left, mods);
        for _ in 0..2 {
            cx.run_until_parked();
        }
        let sel_before = cx.update(|_w, cx| editor.read(cx).selection());
        assert_eq!(
            (sel_before.anchor, sel_before.head),
            (0, 0),
            "按下后选区应落在 offset 0：{sel_before:?}"
        );
        // 切 ON → 切 OFF（真实 toggle 路径）。
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.toggle_live_test(window, cx));
        });
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
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
        // 切回后编辑区重新在树上；仅移动鼠标（不按任何键）→ 选区不得被拉长。
        let ed_b = cx.debug_bounds("code-editor").expect("切回后编辑区应渲染");
        let target = gpui::point(ed_b.origin.x + px(80.), ed_b.origin.y + px(40.));
        cx.simulate_mouse_move(target, MouseButton::Left, mods);
        for _ in 0..2 {
            cx.run_until_parked();
        }
        let sel_after = cx.update(|_w, cx| editor.read(cx).selection());
        assert_eq!(
            (sel_after.anchor, sel_after.head),
            (0, 0),
            "切回后仅移动鼠标不得改变选区（拖选状态残留）：{sel_after:?}"
        );
        let _ = std::fs::remove_file(&md);
    }

    /// 回归（用户确认）：WYSIWYG 区**删除标题记号 `#`**（标题行 → 普通段落，CJK 文本
    /// 行变短/重排），再切「预览编辑测试」OFF（`commit_live` 整篇写回源码编辑区）与 ON。
    /// 历史上源码编辑区 `styled_row_text` 把陈旧的 `rd.highlights`（按字节搬运的区间，
    /// `end` 越过变短后的新行长 / 切进 CJK 字符内部）直接喂 `StyledText::with_highlights`
    /// → gpui `elements/text.rs:446` 的 `is_char_boundary(run.end)` debug_assert 崩溃。
    /// 本测试走真实 `target/测试.md`（找不到则造等价临时文件）删首个 `#` 记号后完整
    /// 开关往返，确保不 panic 且内容往返一致。
    #[gpui::test]
    async fn wysiwyg_toggle_heading_delete_does_not_panic(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1000.), px(800.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 候选源：真实 `target/测试.md`（不清写）；缺或无标题则造等价 CJK 临时文件。
        let md = std::env::current_dir().unwrap().join("../../target/测试.md");
        let (md, src_text) = match std::fs::read_to_string(&md) {
            Ok(s) if s.contains("# ") => (md, s),
            _ => {
                let tmp = std::env::temp_dir().join(format!(
                    "gpui-wy-head-del-{}.md",
                    std::process::id()
                ));
                let fallback = "# 标题\n正文段落。\n\n## 二级\n\n- [ ] 待办\n".to_string();
                std::fs::write(&tmp, &fallback).unwrap();
                (tmp, fallback)
            }
        };
        // 在原始源文本里定位首个行首 `# ` 记号，算「删掉该 `#` 后」的期望全文。
        let del_at_full = {
            let mut byte = 0usize;
            let mut found = None;
            for line in src_text.split('\n') {
                if line.starts_with("# ") {
                    found = Some(byte);
                    break;
                }
                byte += line.len() + 1;
            }
            found.expect("src 应有行首 `# ` 标题记号")
        };
        let expected_after = {
            let mut s = src_text.clone();
            s.replace_range(del_at_full..del_at_full + 1, "");
            s
        };
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(md.to_str().unwrap(), window, cx));
        });
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        // ON：进入 WYSIWYG（live2）实时编辑模式。
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.toggle_live_test(window, cx));
        });
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        // 在 WYSIWYG 文档里按同一偏移删掉该 `#`（标题 → 普通段落）。
        let live = cx.update(|_w, cx| {
            app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab]
                .live2
                .clone()
                .expect("live2 应存在")
        });
        let live_len = cx.update(|_w, cx| live.read(cx).text().len());
        assert!(
            del_at_full < live_len,
            "WYSIWYG 文本字节应与源一致（标题记号偏移 {del_at_full} < {live_len}）"
        );
        live.update(cx, |e, cx| {
            e.replace_range(del_at_full..del_at_full + 1, "", cx);
        });
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        // OFF：commit_live 写回源码编辑区（此步历史上触发 run.end 非字符边界 panic）。
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.toggle_live_test(window, cx));
        });
        for _ in 0..10 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let committed = cx.update(|_w, cx| {
            let a = app.read(cx);
            a.model.borrow().tabs[a.model.borrow().active_tab].payload.editor.read(cx).value().to_string()
        });
        assert_eq!(
            committed, expected_after,
            "切换 OFF 应把「删掉首个 `#` 记号」后的全文写回编辑区"
        );
        // ON 再切回：WYSIWYG 以已提交文本重建，往复一致、不 panic。
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.toggle_live_test(window, cx));
        });
        for _ in 0..10 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let re_on = cx.update(|_w, cx| {
            let a = app.read(cx);
            a.model.borrow().tabs[a.model.borrow().active_tab]
                .live2
                .as_ref()
                .map(|w| w.read(cx).text().to_string())
                .unwrap_or_default()
        });
        assert_eq!(re_on, committed, "再次切换 ON 后 WYSIWYG 内容应与写回内容一致");
    }

    /// 回归（用户实测卡顿）：切「预览编辑测试」ON →（live 期间**不做任何编辑**）→ OFF，
    /// `commit_live` 必须跳过整篇写回。旧实现无条件物化全文 + `set_text` 写回源码编辑区
    /// + 喂右栏（大文档 6.4MB 级联重排卡 7-8s），哪怕内容从未被 WYSIWYG 改过。live2 未
    /// 编辑 ⇔ 内容恒等于 toggle ON 时源码 COW 快照（无其它写入路径，`install_open` 不发
    /// `WyEvent::Changed`）→ 直接销毁实体切回（O(1)）。断言：源码编辑区文本实例未被替换
    /// （`version()` 不变）、live2 销毁、原内容保留且仍可继续编辑。
    #[gpui::test]
    async fn wysiwyg_toggle_off_unchanged_skips_write_back(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1000.), px(800.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let md = std::env::temp_dir().join(format!(
            "gpui-wy-skip-wb-{}.md",
            std::process::id()
        ));
        let src = "# 标题\n正文段落。\n\n- [ ] 待办\n\n```rs\nlet x = 1;\n```\n".repeat(6);
        std::fs::write(&md, &src).unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(md.to_str().unwrap(), window, cx));
        });
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let editor = cx.update(|_w, cx| app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab].payload.editor.clone());
        let (src_before, ver_before, skip_before) = cx.update(|_w, cx| {
            let ed = editor.read(cx);
            (
                ed.value(),
                ed.version(),
                shell_core::LIVE_COMMIT_SKIP_UNCHANGED
                    .load(std::sync::atomic::Ordering::Relaxed),
            )
        });
        assert_eq!(src_before, src, "源码编辑区应已装载文件原文");
        // ON：进入 WYSIWYG（live2），期间不做任何编辑。
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.toggle_live_test(window, cx));
        });
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        // OFF：未编辑 → 应走 O(1) 跳过写回路径。
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.toggle_live_test(window, cx));
        });
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let (src_after, ver_after, skip_after) = cx.update(|_w, cx| {
            let a = app.read(cx);
            let t = &a.model.borrow().tabs[a.model.borrow().active_tab];
            assert!(t.live2.is_none(), "切回后 live2 实体应已销毁");
            assert!(!t.live_edit, "切回后应退出 live_edit 模式");
            assert!(
                a.model.borrow().status.to_string().contains("未改动，无需写回"),
                "未编辑切回应提示「无需写回」，实际状态：{}",
                a.model.borrow().status
            );
            let ed = editor.read(cx);
            (
                ed.value(),
                ed.version(),
                shell_core::LIVE_COMMIT_SKIP_UNCHANGED
                    .load(std::sync::atomic::Ordering::Relaxed),
            )
        });
        // 全局跳过探针：并行的其它用例也走同一跳过路径、可能并发推进计数 → 用 ≥（本用例
        // 至少一次）。「未走 set_text 写回」的确定性判据是下方的 version()+内容不变。
        assert!(
            skip_after >= skip_before + 1,
            "未编辑切回应使跳过写回探针前进（至少本用例一次）"
        );
        assert_eq!(
            ver_after, ver_before,
            "未编辑切回不得对源码编辑区做 set_text 写回（编辑版本号应不变）"
        );
        assert_eq!(src_after, src_before, "切回后源码编辑区仍保持原内容");
        // 仍可继续编辑（真实编辑路径未被误伤）。
        let ver_edit_pre = cx.update(|_w, cx| editor.read(cx).version());
        cx.update(|_w, cx| editor.update(cx, |ed, cx| ed.insert_text("追加。", cx)));
        for _ in 0..2 {
            cx.run_until_parked();
        }
        let (ver_final, src_final) = cx.update(|_w, cx| {
            let ed = editor.read(cx);
            (ed.version(), ed.value())
        });
        assert!(ver_final > ver_edit_pre, "切回后编辑应推进编辑版本号");
        assert!(
            src_final.starts_with("追加。"),
            "切回后编辑应落到源码编辑区开头：{src_final:?}"
        );
        let _ = std::fs::remove_file(&md);
    }

    /// 回归：切「预览编辑测试」ON → 在 WYSIWYG 里**编辑一字** → OFF → 必须仍走整篇写回
    /// 路径（`live2_dirty == true`），源码编辑区得到新内容。修复只跳过「未改动」情形，
    /// 不能误砍有改动的写回逻辑。
    #[gpui::test]
    async fn wysiwyg_toggle_off_edited_still_writes_back(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1000.), px(800.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let md = std::env::temp_dir().join(format!(
            "gpui-wy-wb-{}.md",
            std::process::id()
        ));
        let src = "# 标题\n正文段落。\n\n- [ ] 待办\n".repeat(4);
        std::fs::write(&md, &src).unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(md.to_str().unwrap(), window, cx));
        });
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let editor = cx.update(|_w, cx| app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab].payload.editor.clone());
        let wb_before = shell_core::LIVE_COMMIT_WRITEBACK
            .load(std::sync::atomic::Ordering::Relaxed);
        // ON：进入 WYSIWYG（live2）。
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.toggle_live_test(window, cx));
        });
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        // 在 WYSIWYG 文档开头插入一行（编辑路径 → live2_dirty 置位）。
        let inserted = "# 切换后新增\n";
        cx.update(|_w, cx| {
            let live = app
                .read(cx)
                .model.borrow().tabs[app.read(cx).model.borrow().active_tab]
                .live2
                .clone()
                .expect("live2 应存在");
            live.update(cx, |e, cx| e.replace_range(0..0, inserted, cx));
        });
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        // OFF：有改动 → 应走整篇写回（源码编辑区得到新内容）。
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.toggle_live_test(window, cx));
        });
        for _ in 0..10 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        cx.update(|_w, cx| {
            let a = app.read(cx);
            let t = &a.model.borrow().tabs[a.model.borrow().active_tab];
            assert!(t.live2.is_none(), "切回后 live2 实体应已销毁");
            assert!(
                a.model.borrow().status.to_string().contains("内容已写回编辑区"),
                "有改动切回应提示「已写回」，实际状态：{}",
                a.model.borrow().status
            );
        });
        let committed = cx.update(|_w, cx| editor.read(cx).value());
        assert_eq!(
            committed,
            format!("{inserted}{src}"),
            "有改动切回应整篇写回源码编辑区"
        );
        // 全局写回探针：并行的其它用例（编辑过再切回）也走同一写回路径 → 用 ≥（本用例
        // 至少一次）。「写回真的发生」的确定性判据是上方的内容相等。
        let wb_after = shell_core::LIVE_COMMIT_WRITEBACK
            .load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            wb_after >= wb_before + 1,
            "有改动切回应使写回探针前进（至少本用例一次）"
        );
        let _ = std::fs::remove_file(&md);
    }

    /// M：WYSIWYG 编辑过 → 切回不再整篇 `set_text`，而是按**编辑日志按序重放**
    /// （方案2 op 主路径；护栏/无日志时才回退行级 diff `src/app/diff.rs`）。
    /// 在 WYSIWYG 里做多行编辑（开头插入一行 + 中部改一行）→ OFF → 断言：
    /// 1) 源码编辑区内容 == live2 最终文本（全量等价）；
    /// 2) 走 op 重放路径：`LIVE_COMMIT_OPS_REPLAYED` 前进、`LIVE_COMMIT_OPS_COUNT` 累计
    ///    ≥ 编辑条数，**且整个切回期间 `CodeEditor::set_text`（reset_document 全量级联）
    ///    未被调用**；
    /// 3) 右栏只读预览 `md_wy` 也随重放对齐到最终文本；
    /// 4) 切回后继续编辑/保存语义正常。
    #[gpui::test]
    async fn wysiwyg_toggle_off_edited_uses_incremental_diff(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1000.), px(800.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let md = std::env::temp_dir().join(format!(
            "gpui-wy-diff-{}.md",
            std::process::id()
        ));
        let src = "line0\nline1\nBB\nline3\nline4\nline5\n".repeat(40);
        std::fs::write(&md, &src).unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(md.to_str().unwrap(), window, cx));
        });
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let editor = cx.update(|_w, cx| app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab].payload.editor.clone());
        // ON：进入 WYSIWYG（live2），等加载完成。
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.toggle_live_test(window, cx));
        });
        for _ in 0..12 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        // 在 WYSIWYG 里做多行编辑：文档开头插入一行 + 中部替换一行。
        cx.update(|_w, cx| {
            let live = app
                .read(cx)
                .model.borrow().tabs[app.read(cx).model.borrow().active_tab]
                .live2
                .clone()
                .expect("live2 应存在");
            live.update(cx, |e, cx| e.replace_range(0..0, "# 开头新增\n", cx));
        });
        cx.update(|_w, cx| {
            let live = app
                .read(cx)
                .model.borrow().tabs[app.read(cx).model.borrow().active_tab]
                .live2
                .clone()
                .expect("live2 应存在");
            let at = src.find("BB").expect("应有 BB 行") + "# 开头新增\n".len();
            live.update(cx, |e, cx| e.replace_range(at..at + 2, "XX", cx));
        });
        for _ in 0..10 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        // 采样切回前的探针与本 live2 最终文本。
        let (expected, ops_replayed_pre, ops_count_pre, wy_text_pre) = cx.update(|_w, cx| {
            let a = app.read(cx);
            let t = &a.model.borrow().tabs[a.model.borrow().active_tab];
            let l2 = t.live2.clone().expect("live2 应存在");
            let expected = l2.read(cx).text().to_string();
            let wy_text_pre = t.payload.md_wy.clone().map(|w| w.read(cx).text().to_string());
            (
                expected,
                shell_core::LIVE_COMMIT_OPS_REPLAYED
                    .load(std::sync::atomic::Ordering::Relaxed),
                shell_core::LIVE_COMMIT_OPS_COUNT
                    .load(std::sync::atomic::Ordering::Relaxed),
                wy_text_pre,
            )
        });
        assert!(
            expected.len() > src.len(),
            "多行编辑后 live2 文本应比源长（插入了一行）"
        );
        // OFF：有改动 → 应走编辑日志按序重放写回（op 主路径）。
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.toggle_live_test(window, cx));
        });
        for _ in 0..10 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let (committed, ops_replayed_after, ops_count_after, wy_text_after) =
            cx.update(|_w, cx| {
                let a = app.read(cx);
                let t = &a.model.borrow().tabs[a.model.borrow().active_tab];
                assert!(t.live2.is_none(), "切回后 live2 实体应已销毁");
                assert!(
                    a.model.borrow().status.to_string().contains("内容已写回编辑区"),
                    "有改动切回应提示「已写回」，实际状态：{}",
                    a.model.borrow().status
                );
                let ed = editor.read(cx);
                (
                    ed.value(),
                    shell_core::LIVE_COMMIT_OPS_REPLAYED
                        .load(std::sync::atomic::Ordering::Relaxed),
                    shell_core::LIVE_COMMIT_OPS_COUNT
                        .load(std::sync::atomic::Ordering::Relaxed),
                    t.payload.md_wy.clone().map(|w| w.read(cx).text().to_string()),
                )
            });
        assert_eq!(committed, expected, "重放写回后源码编辑区应与 live2 全文等价");
        // 走 op 重放路径（`apply_live_replay` → `apply_edit_synced`，不调用整篇 `set_text`
        // 的 reset_document 级联）——此探针只在 `commit_live` 的 op 重放分支前进，故其
        // 推进即证明本次切回未走整篇 set_text 写回。
        assert!(
            ops_replayed_after >= ops_replayed_pre + 1,
            "有改动切回应走 op 重放写回路径（至少本用例一次）"
        );
        assert!(
            ops_count_after >= ops_count_pre + 2,
            "两次编辑应至少缴存 2 条 op（实际增量 {}）",
            ops_count_after.saturating_sub(ops_count_pre)
        );
        // 右栏只读预览随重放对齐到最终文本。live 期间右栏不镜像（保持源内容）——
        // 切回后应被逐 op 增量追平到最终文本。
        if let (Some(pre), Some(after)) = (wy_text_pre, wy_text_after) {
            assert_eq!(pre, src, "切回前右栏预览应仍是源内容（live 期间不镜像）");
            assert_eq!(after, expected, "切回后右栏预览应与最终文本一致");
        }
        // 切回后继续编辑/保存语义正常（真实编辑路径未被误伤）：再编辑一次并核对内容。
        let inserted2 = "追加。";
        cx.update(|_w, cx| editor.update(cx, |ed, cx| ed.insert_text(inserted2, cx)));
        for _ in 0..2 {
            cx.run_until_parked();
        }
        let src_final = cx.update(|_w, cx| editor.read(cx).value());
        assert!(
            src_final.starts_with(inserted2),
            "切回后编辑应落到源码编辑区开头：{src_final:?}"
        );
        let _ = std::fs::remove_file(&md);
    }

    /// M：单字符这类**行内小改动**切回走**编辑日志重放**（方案2）且**恰缴存 1 条 op**
    /// （`LIVE_COMMIT_OPS_COUNT` 增量 == 1），不再做行级 LCS / 整篇物化。源码编辑区
    /// 内容与 live2 全文等价。
    #[gpui::test]
    async fn wysiwyg_toggle_off_one_char_edit_incremental(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1000.), px(800.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let md = std::env::temp_dir().join(format!(
            "gpui-wy-1char-{}.md",
            std::process::id()
        ));
        let src = "# 标题\n正文段落。\n\n- [ ] 待办\n".repeat(20);
        std::fs::write(&md, &src).unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(md.to_str().unwrap(), window, cx));
        });
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let editor = cx.update(|_w, cx| app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab].payload.editor.clone());
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.toggle_live_test(window, cx));
        });
        for _ in 0..12 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        // 单字符编辑：把文档中间的「段」改成「更」。
        cx.update(|_w, cx| {
            let live = app
                .read(cx)
                .model.borrow().tabs[app.read(cx).model.borrow().active_tab]
                .live2
                .clone()
                .expect("live2 应存在");
            let e = live.read(cx).text().to_string();
            let at = e.find('段').expect("应有中文");
            live.update(cx, |ed, cx| ed.replace_range(at..at + 3, "更", cx));
        });
        for _ in 0..10 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let expected = cx.update(|_w, cx| {
            app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab]
                .live2
                .clone()
                .expect("live2 应存在")
                .read(cx)
                .text()
                .to_string()
        });
        // 采样切回前（实例级，确定性）：编辑日志应恰 1 条 op。
        let ops_len = cx.update(|_w, cx| {
            app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab]
                .live2
                .clone()
                .expect("live2 应存在")
                .read(cx)
                .live_ops_len()
        });
        assert_eq!(ops_len, 1, "单字符编辑应恰缴存 1 条 op，实际 {ops_len}");
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.toggle_live_test(window, cx));
        });
        for _ in 0..10 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let (committed, replays, diffs) = cx.update(|_w, cx| {
            let ed = editor.read(cx);
            (
                ed.value(),
                ed.live_replay_calls(),
                ed.apply_replaces_calls(),
            )
        });
        assert_eq!(committed, expected, "单字符编辑切回应与 live2 全文等价");
        // 走 op 重放主路径（`apply_live_replay`），不经行级 diff 兜底（实例级探针）。
        assert_eq!(replays, 1, "单字符改动切回应走 op 重放路径");
        assert_eq!(diffs, 0, "单字符改动不应走 diff 兜底");
        let _ = std::fs::remove_file(&md);
    }

    /// 方案2：**多次编辑 + 撤销/重做混用** → 编辑日志（含撤/重做的净逆操作缴存）按序
    /// 重放后源码编辑区内容与 live2 全文等价（宏语义：撤/重做也是 op，按序重放即得最终态）。
    /// 右栏只读预览也逐 op 对齐；`live_ops_len`（实例级）精确断言本次缴存 5 条。
    #[gpui::test]
    async fn wysiwyg_toggle_off_edits_undo_redo_replay_equivalent(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1000.), px(800.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let md = std::env::temp_dir().join(format!(
            "gpui-wy-undo-{}.md",
            std::process::id()
        ));
        let src = "甲\n乙\n丙\n丁\n戊\n".repeat(8);
        std::fs::write(&md, &src).unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(md.to_str().unwrap(), window, cx));
        });
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let editor = cx.update(|_w, cx| app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab].payload.editor.clone());
        // ON：进入 WYSIWYG（live2）。
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.toggle_live_test(window, cx));
        });
        for _ in 0..12 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let live = cx.update(|_w, cx| {
            app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab]
                .live2
                .clone()
                .expect("live2 应存在")
        });
        // 1) 开头插入一行；2) 中部改一处；3) 撤销第 2 笔；4) 重做第 2 笔；5) 再开头插入。
        cx.update(|_w, cx| {
            live.update(cx, |e, cx| e.replace_range(0..0, "前言\n", cx));
        });
        cx.update(|_w, cx| {
            let t = live.read(cx).text().to_string();
            let at = t.find('丙').expect("应有丙行");
            live.update(cx, |e, cx| e.replace_range(at..at + 3, "丙X", cx));
        });
        cx.update(|_w, cx| live.update(cx, |e, cx| e.undo(cx)));
        cx.update(|_w, cx| live.update(cx, |e, cx| e.redo(cx)));
        cx.update(|_w, cx| {
            live.update(cx, |e, cx| e.replace_range(0..0, "另一行\n", cx));
        });
        for _ in 0..10 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let (expected, wy_text_pre, ops_len) = cx.update(|_w, cx| {
            let a = app.read(cx);
            let l2 = a.model.borrow().tabs[a.model.borrow().active_tab].live2.clone().expect("live2 应存在");
            (
                l2.read(cx).text().to_string(),
                a.model.borrow().tabs[a.model.borrow().active_tab]
                    .payload.md_wy
                    .clone()
                    .map(|w| w.read(cx).text().to_string()),
                l2.read(cx).live_ops_len(),
            )
        });
        assert!(
            expected.starts_with("另一行\n前言\n"),
            "撤销/重做混用后的 live2 最终文本应如预期：{expected:?}"
        );
        // 本次缴存 5 条 op：2 次 insert + 1 次 replace + 1 次 undo + 1 次 redo（实例级）。
        assert_eq!(
            ops_len, 5,
            "插入×2 + 改×1 + 撤×1 + 重×1 应恰缴存 5 条 op，实际 {ops_len}"
        );
        // OFF：有改动 → 编辑日志（含撤/重做 op）按序重放写回。
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.toggle_live_test(window, cx));
        });
        for _ in 0..10 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let (committed, replays, wy_text_after) = cx.update(|_w, cx| {
            let a = app.read(cx);
            let t = &a.model.borrow().tabs[a.model.borrow().active_tab];
            let ed = editor.read(cx);
            (ed.value(), ed.live_replay_calls(), t.payload.md_wy.clone().map(|w| w.read(cx).text().to_string()))
        });
        assert_eq!(committed, expected, "编辑日志（含撤/重做）重放后须与 live2 全文等价");
        assert_eq!(replays, 1, "含撤/重做混用的切回应走 op 重放路径（实例级探针）");
        if let (Some(pre), Some(after)) = (wy_text_pre, wy_text_after) {
            assert_eq!(pre, src, "切回前右栏预览应仍是源内容（live 期间不镜像）");
            assert_eq!(after, expected, "切回后右栏预览应逐 op 对齐到最终文本");
        }
        let _ = std::fs::remove_file(&md);
    }

    /// 方案2：**大段粘贴单大 op** → 编辑日志恰 1 条且内容等价；(探针)走 op 重放而非
    /// 行级 diff 兜底（实例级 `live_replay_calls == 1` / `apply_replaces_calls == 0`）。
    /// 右栏预览随后逐 op 增量对齐（< 64KB 阈值）。
    #[gpui::test]
    async fn wysiwyg_toggle_off_large_paste_single_op_replay(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1000.), px(800.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let md = std::env::temp_dir().join(format!(
            "gpui-wy-big-{}.md",
            std::process::id()
        ));
        let src = "# 原文\n首段。\n\n第二段。\n".repeat(5);
        std::fs::write(&md, &src).unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(md.to_str().unwrap(), window, cx));
        });
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let editor = cx.update(|_w, cx| app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab].payload.editor.clone());
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.toggle_live_test(window, cx));
        });
        for _ in 0..12 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        // 大段粘贴：单条 replace_range 插入约 30KB（远大于单键 op，仍远低于 md_wy 整篇
        // 兜底阈值 64KB → 右栏应走逐 op 增量对齐路径）。
        let big = format!("## 大段\n{}\n", "粘贴内容行示例，中文与 ASCII 混排。\n".repeat(1000));
        assert!(big.len() > 30_000, "粘贴内容应足够大：{}", big.len());
        cx.update(|_w, cx| {
            let live = app
                .read(cx)
                .model.borrow().tabs[app.read(cx).model.borrow().active_tab]
                .live2
                .clone()
                .expect("live2 应存在");
            live.update(cx, |e, cx| e.replace_range(0..0, &big, cx));
        });
        for _ in 0..10 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let expected = cx.update(|_w, cx| {
            app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab]
                .live2
                .clone()
                .expect("live2 应存在")
                .read(cx)
                .text()
                .to_string()
        });
        // 采样切回前（实例级）：大段粘贴应恰缴存 1 条 op（单大 op）。
        let ops_len = cx.update(|_w, cx| {
            app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab]
                .live2
                .clone()
                .expect("live2 应存在")
                .read(cx)
                .live_ops_len()
        });
        assert!(expected.starts_with(&big), "大段粘贴应落到文档开头");
        assert_eq!(ops_len, 1, "大段粘贴应恰缴存 1 条 op（单大 op），实际 {ops_len}");
        // OFF：有改动 → 应走 op 重放（单大 op），而非行级 diff 兜底。
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.toggle_live_test(window, cx));
        });
        for _ in 0..10 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let (committed, replays, diffs) = cx.update(|_w, cx| {
            let ed = editor.read(cx);
            (ed.value(), ed.live_replay_calls(), ed.apply_replaces_calls())
        });
        assert_eq!(committed, expected, "大段粘贴切回应与 live2 全文等价");
        // （探针）走 op 重放主路径：`apply_live_replay` 成功 1 次、`apply_replaces` 兜底 0 次
        // （实例级确定性计数，契合「护栏成功 → op 重放而非 diff」验收点）。
        assert_eq!(replays, 1, "大段粘贴切回应走 op 重放路径（实例级探针）");
        assert_eq!(diffs, 0, "大段粘贴切回不得走行级 diff 兜底");
        let _ = std::fs::remove_file(&md);
    }

    /// 方案2 护栏：**构造坐标系错位**（live 期间外部改写源码编辑区，模拟 CRLF/BOM/
    /// 漏 op 等偏移漂移）→ `apply_live_replay` 干跑/长度栏失败、**不改动源码编辑区**，回退
    /// 既有行级 diff 管道（实例级探针：`live_replay_calls == 0` / `apply_replaces_calls == 1`）；
    /// 最终源码编辑区内容仍与 live2 全文等价。
    #[gpui::test]
    async fn wysiwyg_toggle_off_guard_falls_back_to_diff_on_drift(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1000.), px(800.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let md = std::env::temp_dir().join(format!(
            "gpui-wy-guard-{}.md",
            std::process::id()
        ));
        let src = "line0\nline1\nline2\nline3\nline4\n".repeat(20);
        std::fs::write(&md, &src).unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(md.to_str().unwrap(), window, cx));
        });
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let editor = cx.update(|_w, cx| app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab].payload.editor.clone());
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.toggle_live_test(window, cx));
        });
        for _ in 0..12 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        // 在 WYSIWYG 里做一笔开头插入（缴存 1 条 op）。
        cx.update(|_w, cx| {
            let live = app
                .read(cx)
                .model.borrow().tabs[app.read(cx).model.borrow().active_tab]
                .live2
                .clone()
                .expect("live2 应存在");
            live.update(cx, |e, cx| e.replace_range(0..0, "插入行\n", cx));
        });
        for _ in 0..10 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        // 构造错位：外部直接把源码编辑区整体换成「漂移」基线（≠ live2 的 toggle ON 快照）。
        cx.update(|_w, cx| {
            editor.update(cx, |ed, _| ed.set_text(&format!("漂移基线\n{src}")));
        });
        let expected = cx.update(|_w, cx| {
            app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab]
                .live2
                .clone()
                .expect("live2 应存在")
                .read(cx)
                .text()
                .to_string()
        });
        // 采样切回前（实例级）：up 日志确有 1 条 op（护栏将被触发，重放不应成功）。
        let ops_len = cx.update(|_w, cx| {
            app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab]
                .live2
                .clone()
                .expect("live2 应存在")
                .read(cx)
                .live_ops_len()
        });
        assert_eq!(ops_len, 1, "错位用例也应先缴存编辑 op（实际 {ops_len}）");
        // OFF：op 干跑自检失败 → 回退行级 diff（实例级探针：`apply_live_replay` 0 次 /
        // `apply_replaces` 兜底 1 次），仍须追平到 live2 最终文本。
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.toggle_live_test(window, cx));
        });
        for _ in 0..10 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let (committed, replays, diffs) = cx.update(|_w, cx| {
            let ed = editor.read(cx);
            (ed.value(), ed.live_replay_calls(), ed.apply_replaces_calls())
        });
        assert_eq!(committed, expected, "护栏回退 diff 后仍须与 live2 全文等价");
        assert_eq!(replays, 0, "错位时不得走 op 重放路径（护栏应拦截）");
        assert_eq!(diffs, 1, "错位时应回退走既有行级 diff 兜底（本用例一次）");
        // diff 兜底路径必须落下与拆分前一致的收尾 status/status_ok（拆分后
        // Diff 分支曾漏设 status，状态栏残留旧文案）。防再漂移。
        let (status, status_ok) = cx.update(|_w, cx| {
            let m = app.read(cx).model.borrow();
            (m.status.to_string(), m.status_ok)
        });
        assert_eq!(
            status,
            "预览编辑（WYSIWYG）：关（内容已写回编辑区）",
            "diff 兜底写回应把状态栏文案归位为已写回"
        );
        assert!(status_ok, "diff 兜底写回应置 status_ok = true");
        let _ = std::fs::remove_file(&md);
    }

    /// Markdown 预览（分屏，统一组件 WyEditor 宿主）纵向滚动条拖拽：向下拖 -> 内容实时
    /// 向下滚，向上拖 -> 实时反向。回归：预览窗口模式拖拽只写共享 `desired`，render 逐帧
    /// `apply_desired` 落窗口；若该 Apply 在往下方向被 clamp/跳过，会出现「只有松开才动」。
    #[gpui::test]
    async fn md_wy_preview_scrollbar_drag_follows_both_directions(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1200.), px(800.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let md = std::env::temp_dir().join(format!(
            "gpui-preview-drag-{}.md",
            std::process::id()
        ));
        std::fs::write(&md, format!("{}\n", "# 标题\n正文段落。\n\n".repeat(600))).unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(md.to_str().unwrap(), window, cx));
        });
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let overlay = cx
            .debug_bounds("md-wy-vscrollbar-overlay")
            .expect("MD WyEditor 预览纵向滚动条叠层应渲染");
        let vhand = cx.update(|_, cx| {
            let a = app.read(cx);
            a.model.borrow().tabs[a.model.borrow().active_tab].payload.md_wy_vscroll.clone()
        });
        let abs_top = |cx: &mut VisualTestContext| {
            cx.update(|_w, _cx| (-vhand.offset().y).as_f32())
        };

        let before = abs_top(cx);
        assert_eq!(before, 0.0, "预览初始应在顶部");

        let x = overlay.right() - px(8.);
        let grab_y = overlay.top() + px(14.);
        cx.simulate_mouse_down(
            gpui::point(x, grab_y),
            MouseButton::Left,
            Default::default(),
        );
        for _ in 0..2 {
            cx.run_until_parked();
        }

        // 未松开向下拖两步 → 预览必须边拖边向下滚。只走 run_until_parked（真实 App 路径）。
        let mid1_y = overlay.top() + px(120.);
        cx.simulate_mouse_move(
            gpui::point(x, mid1_y),
            MouseButton::Left,
            Default::default(),
        );
        for _ in 0..4 {
            cx.run_until_parked();
        }
        let s1 = abs_top(cx);
        assert!(
            s1 > before + 1.0,
            "向下拖(1)预览应先实时滚动（abs={s1:.0}）——不能等松开才动"
        );

        let mid2_y = overlay.top() + px(240.);
        cx.simulate_mouse_move(
            gpui::point(x, mid2_y),
            MouseButton::Left,
            Default::default(),
        );
        for _ in 0..4 {
            cx.run_until_parked();
        }
        let s2 = abs_top(cx);
        assert!(
            s2 > s1 + 1.0,
            "继续向下拖(2)预览应继续实时滚动（abs={s2:.0} vs {s1:.0}）"
        );

        // 未松开向上拖回 → 实时反向。
        cx.simulate_mouse_move(
            gpui::point(x, grab_y),
            MouseButton::Left,
            Default::default(),
        );
        for _ in 0..4 {
            cx.run_until_parked();
        }
        let s3 = abs_top(cx);
        assert!(
            s3 < s2 - 1.0,
            "向上拖预览应实时反向滚动（abs={s3:.0} vs {s2:.0}）"
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

    /// A1：`commit_live` 把 `tab.content` 设为 live2 文本的**零拷贝共享**（`SharedString`
    /// 句柄 clone，同一分配），不整篇 `.to_string()` 物化。探针：切回前先取 live2
    /// `text()` 快照（先物化一次热身、令 `Doc::text_shared` 缓存就绪），切回后断言
    /// `tab.content` 与快照**同一分配**（`as_ptr()` 相等）+ 内容等价——若 `commit_live`
    /// 退化成 `.to_string()` + `SharedString::from`，必然新分配，指针即不等。
    #[gpui::test]
    async fn wysiwyg_toggle_off_content_shares_live2_text_zero_copy(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1000.), px(800.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let md = std::env::temp_dir().join(format!(
            "gpui-wy-zerocopy-{}.md",
            std::process::id()
        ));
        let src = "# 标题\n正文段落。\n\n- [ ] 待办\n".repeat(40);
        std::fs::write(&md, &src).unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(md.to_str().unwrap(), window, cx));
        });
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.toggle_live_test(window, cx));
        });
        for _ in 0..12 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        // 编辑一字（切回走 op 重放主路径）。
        cx.update(|_w, cx| {
            let live = app
                .read(cx)
                .model.borrow().tabs[app.read(cx).model.borrow().active_tab]
                .live2
                .clone()
                .expect("live2 应存在");
            let e = live.read(cx).text().to_string();
            let at = e.find('段').expect("应有中文");
            live.update(cx, |ed, cx| ed.replace_range(at..at + 3, "更", cx));
        });
        for _ in 0..10 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        // 切回前快照：`text()` 返回与 `commit_live` 内部**同源同分配**的 SharedString。
        let snap = cx.update(|_w, cx| {
            app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab]
                .live2
                .clone()
                .expect("live2 应存在")
                .read(cx)
                .text()
        });
        let snap_ptr = snap.as_ptr() as usize;
        let snap_len = snap.len();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.toggle_live_test(window, cx));
        });
        // 探针：`tab.content` 与快照同一分配（零拷贝共享，非 to_string 物化）＋内容等价。
        let (mirror_ptr, mirror) = cx.update(|_w, cx| {
            let c = app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab].payload.content.clone();
            (c.as_ptr() as usize, c)
        });
        assert_eq!(mirror.len(), snap_len, "tab.content 应等于 live2 最终文本长度");
        assert_eq!(
            mirror, snap,
            "tab.content 应与 live2 最终文本内容等价"
        );
        assert_eq!(
            mirror_ptr, snap_ptr,
            "commit_live 的 tab.content 必须零拷贝共享 live2 text（同分配）；\
            若退化成 .to_string()/SharedString::from 会另开新分配"
        );
        let _ = std::fs::remove_file(&md);
    }

    /// A2：切回（`commit_live`）时 live2 实体**不立即销毁**，而是暂存进 app 侧
    /// `retired_live2`，由 `cx.on_next_frame` 在「下一帧渲染完之后」清空队列、放实体
    /// 出作用域 Drop（WyEditor 级联 Drop 因此移出点击同步路径）。
    /// 断言：切回瞬间队列长度 == 1（app 侧不立即放行实体）；渲染一帧 + 投递
    /// on_next_frame 后队列 == 0（app 侧不再持有、无累积）；再开再切一轮仍回到 0，
    /// 不跨会话泄漏到下一次 open。
    #[gpui::test]
    async fn wysiwyg_toggle_off_retired_live2_drained_next_frame(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1000.), px(800.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let md = std::env::temp_dir().join(format!(
            "gpui-wy-retire-{}.md",
            std::process::id()
        ));
        let src = "# 标题\n正文段落。\n\n- [ ] 待办\n".repeat(30);
        std::fs::write(&md, &src).unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(md.to_str().unwrap(), window, cx));
        });
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.toggle_live_test(window, cx));
        });
        for _ in 0..12 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        // ON → OFF：实体进 `retired_live2`，此时尚未到下一帧 on_next_frame，队列应暂存 1
        //（app 侧不立即放行、不同步 Drop——销毁被延迟出点击路径）。
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.toggle_live_test(window, cx));
        });
        let n0 = cx.update(|_w, cx| app.read(cx).retired_live2_len());
        assert_eq!(n0, 1, "切回瞬间旧 live2 应暂存进 retired_live2，实际 {n0}");
        // 渲染一帧（不带 live2 的源码编辑区 UI），再投递 `cx.on_next_frame` 登记的下一帧
        // 回调 → 队列清空（app 侧放行实体，不再持有/累积）。
        let mut drained = 0;
        for _ in 0..6 {
            cx.run_until_parked();
            drained = cx.update(|window, cx| window.simulate_next_frame(cx));
            cx.run_until_parked();
            if drained > 0 {
                break;
            }
        }
        assert!(drained > 0, "on_next_frame 应有已登记的下一帧回调");
        let n1 = cx.update(|_w, cx| app.read(cx).retired_live2_len());
        assert_eq!(n1, 0, "下一帧后 retired_live2 应被清空（app 侧无累积），实际 {n1}");
        // 再来一轮 ON→OFF：队列重回 1 再清 0，无跨会话累积（无泄漏到下一次 open）。
        assert_eq!(
            cx.update(|_w, cx| app.read(cx).retired_live2_len()),
            0,
            "重新开 live 前回收队列应为空"
        );
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.toggle_live_test(window, cx));
        });
        for _ in 0..12 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        assert_eq!(cx.update(|_w, cx| app.read(cx).retired_live2_len()), 0);
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.toggle_live_test(window, cx));
        });
        assert_eq!(
            cx.update(|_w, cx| app.read(cx).retired_live2_len()),
            1,
            "再次切回仍应暂存 1 个旧实体"
        );
        let mut drained2 = 0;
        for _ in 0..6 {
            cx.run_until_parked();
            drained2 = cx.update(|window, cx| window.simulate_next_frame(cx));
            cx.run_until_parked();
            if drained2 > 0 {
                break;
            }
        }
        assert!(drained2 > 0, "第二轮 on_next_frame 回调应被投递");
        assert_eq!(cx.update(|_w, cx| app.read(cx).retired_live2_len()), 0);
        let _ = std::fs::remove_file(&md);
    }

    /// B3：**大跨度切回 → 右栏 md_wy 后台异步整篇装配**。大内容（≥512KB）整篇喂源不再
    /// 在点击路径同步 `Doc::open`：OFF 瞬间断言「异步已登记（探针 +1）、右栏仍显示旧内容、
    /// 未同步解析」；泵后台到完成后断言「右栏最终内容 == live2 全文 == 源码编辑区」，且
    /// pending 清空、异步计数不重复。
    #[gpui::test]
    async fn wysiwyg_toggle_off_big_span_async_installs_right_pane(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1000.), px(800.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let md = std::env::temp_dir().join(format!(
            "gpui-wy-b3-async-{}.md",
            std::process::id()
        ));
        // 大文档：内容 ≥ 512KB（B3 后台装配门槛）。
        let block = "# 大标题\n这是正文段落，用于 WYSIWYG 大段切回的后台装配回归。\n\n- [ ] 待办\n";
        let src = block.repeat((600 * 1024) / block.len());
        assert!(src.len() >= 512 * 1024, "大文档应 ≥512KB，实际 {}", src.len());
        std::fs::write(&md, &src).unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(md.to_str().unwrap(), window, cx));
        });
        // 等到右栏 md_wy 与编辑区都就绪（大文档走两段式打开，md_wy 后台 install_open）。
        let mut ready = false;
        for _ in 0..200 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
            ready = cx.update(|_w, cx| {
                let a = app.read(cx);
                let t = &a.model.borrow().tabs[a.model.borrow().active_tab];
                t.payload.editor.read(cx).value().len() >= src.len()
                    && t.payload.md_wy.as_ref().map(|w| w.read(cx).is_loaded()).unwrap_or(false)
            });
            if ready {
                break;
            }
        }
        assert!(ready, "大文档右栏 md_wy 应在 200 帧内装好");
        let wy_old = cx.update(|_w, cx| {
            app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab]
                .payload.md_wy
                .clone()
                .expect("md_wy 应存在")
                .read(cx)
                .text()
                .to_string()
        });
        assert_eq!(wy_old, src, "切回前右栏应显示原文档全文");
        // ON：进入 WYSIWYG（live2），等加载完成。
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.toggle_live_test(window, cx));
        });
        let mut l2_loaded = false;
        for _ in 0..200 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
            l2_loaded = cx.update(|_w, cx| {
                app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab]
                    .live2
                    .as_ref()
                    .map(|l| l.read(cx).is_loaded())
                    .unwrap_or(false)
            });
            if l2_loaded {
                break;
            }
        }
        assert!(l2_loaded, "live2 应在 200 帧内加载完大文档");
        // 大段插入：单 op，跨度 > 64KB 阈值 → commit 走整篇喂源。
        let big = format!("## 大段\n{}\n", "插入的正文内容，确保大跨度走整篇喂源。\n".repeat(3000));
        assert!(big.len() > 64 * 1024, "大段应 >64KB 跨度，实际 {}", big.len());
        cx.update(|_w, cx| {
            let live = app
                .read(cx)
                .model.borrow().tabs[app.read(cx).model.borrow().active_tab]
                .live2
                .clone()
                .expect("live2 应存在");
            live.update(cx, |e, cx| e.replace_range(0..0, &big, cx));
        });
        for _ in 0..10 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let expected = cx.update(|_w, cx| {
            app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab]
                .live2
                .clone()
                .expect("live2 应存在")
                .read(cx)
                .text()
                .to_string()
        });
        let async_before = crate::app::preview::LIVE_COMMIT_MD_WY_ASYNC
            .load(std::sync::atomic::Ordering::Relaxed);
        // OFF：大内容整篇喂源 → 后台异步装配（同步路径零 O(n) 解析）。
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.toggle_live_test(window, cx));
        });
        // 不泵后台，立即断言：异步已登记、右栏仍是旧内容（未同步解析）、探针 +1。
        let (pending, wy_now) = cx.update(|_w, cx| {
            let a = app.read(cx);
            let t = &a.model.borrow().tabs[a.model.borrow().active_tab];
            (
                t.payload.md_wy_install.pending.clone(),
                t.payload.md_wy.clone().map(|w| w.read(cx).text().to_string()),
            )
        });
        let async_after = crate::app::preview::LIVE_COMMIT_MD_WY_ASYNC
            .load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            async_after,
            async_before + 1,
            "大段切回应恰发起 1 次后台异步装配（探针）"
        );
        assert!(
            pending.as_ref().is_some_and(|(_, c)| c.as_ref() == expected),
            "OFF 后右栏应登记待装配目标 = live2 全文"
        );
        assert_eq!(wy_now.as_deref(), Some(src.as_str()), "OFF 瞬间右栏仍是旧内容（未同步解析）");
        // 泵后台到完成：右栏最终内容 == live2 全文，pending 清空。
        let mut done = false;
        for _ in 0..200 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
            done = cx.update(|_w, cx| {
                app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab]
                    .payload.md_wy_install.pending
                    .is_none()
            });
            if done {
                break;
            }
        }
        assert!(done, "后台装配应在 200 帧内完成（pending 清空）");
        let (wy_final, committed) = cx.update(|_w, cx| {
            let a = app.read(cx);
            let w = a.model.borrow().tabs[a.model.borrow().active_tab]
                .payload.md_wy
                .clone()
                .expect("md_wy 应在")
                .read(cx)
                .text()
                .to_string();
            (w, a.model.borrow().tabs[a.model.borrow().active_tab].payload.editor.read(cx).value().to_string())
        });
        assert_eq!(wy_final, expected, "右栏 md_wy 最终应等价于 live2 全文（后到一致）");
        assert_eq!(committed, expected, "源码编辑区切回内容应等价（未回归写回）");
        let _ = std::fs::remove_file(&md);
    }

    /// B3：**小跨度维持增量、不发起后台装配**。小文档小改动切回：`LIVE_COMMIT_MD_WY_ASYNC`
    /// 不动（探针 0），右栏 md_wy 在 OFF 点击路径内**同步**逐 op 增量对齐（无「后到」），
    /// 最终内容等价——既保全量路径不回归，也不自找异步（右栏无旧内容滞留）。
    #[gpui::test]
    async fn wysiwyg_toggle_off_small_span_stays_incremental_no_async(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1000.), px(800.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let md = std::env::temp_dir().join(format!(
            "gpui-wy-b3-small-{}.md",
            std::process::id()
        ));
        let src = "# 小标题\n甲行。\n\n乙行。\n".repeat(30);
        std::fs::write(&md, &src).unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(md.to_str().unwrap(), window, cx));
        });
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let editor = cx.update(|_w, cx| app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab].payload.editor.clone());
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.toggle_live_test(window, cx));
        });
        for _ in 0..12 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        // 单字符编辑（小跨度 → 右栏逐 op 增量对齐）。
        cx.update(|_w, cx| {
            let live = app
                .read(cx)
                .model.borrow().tabs[app.read(cx).model.borrow().active_tab]
                .live2
                .clone()
                .expect("live2 应存在");
            let e = live.read(cx).text().to_string();
            let at = e.find('甲').expect("应有中文");
            live.update(cx, |ed, cx| ed.replace_range(at..at + 3, "更", cx));
        });
        for _ in 0..10 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let expected = cx.update(|_w, cx| {
            app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab]
                .live2
                .clone()
                .expect("live2 应存在")
                .read(cx)
                .text()
                .to_string()
        });
        let async_before = crate::app::preview::LIVE_COMMIT_MD_WY_ASYNC
            .load(std::sync::atomic::Ordering::Relaxed);
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.toggle_live_test(window, cx));
        });
        // OFF 同步完成（无后台等待）：右栏应立即等于最终文本。
        let (committed, wy_now) = cx.update(|_w, cx| {
            let a = app.read(cx);
            let ed = editor.read(cx);
            (
                ed.value().to_string(),
                a.model.borrow().tabs[a.model.borrow().active_tab]
                    .payload.md_wy
                    .clone()
                    .map(|w| w.read(cx).text().to_string()),
            )
        });
        assert_eq!(committed, expected, "小跨度切回源码编辑区应等价");
        assert_eq!(wy_now.as_deref(), Some(expected.as_str()), "小跨度右栏应在 OFF 内同步增量对齐");
        assert_eq!(
            crate::app::preview::LIVE_COMMIT_MD_WY_ASYNC
                .load(std::sync::atomic::Ordering::Relaxed),
            async_before,
            "小跨度不得发起后台整篇装配（探针不动）"
        );
        let _ = std::fs::remove_file(&md);
    }

    /// B3：**异步在途被同一实体上的更新同步喂源作废（装配代号守卫）**。大段切回登记后台
    /// 装配（目标 CX）后，尚未落地前：源码编辑区改写成 CY 并 `ensure_md_wy(feed)` 同步喂源
    /// → 装配代号换代；泵后台完成 → 旧装配必须丢弃，右栏保持 CY（不被陈旧的 CX 覆盖）。
    #[gpui::test]
    async fn wysiwyg_toggle_off_async_inflight_invalidated_by_sync_feed(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1000.), px(800.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let md = std::env::temp_dir().join(format!(
            "gpui-wy-b3-seq-{}.md",
            std::process::id()
        ));
        let block = "# 大标题\n正文段落，用于装配代号守卫回归。\n\n- [ ] 待办\n";
        let src = block.repeat((600 * 1024) / block.len());
        std::fs::write(&md, &src).unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(md.to_str().unwrap(), window, cx));
        });
        let mut ready = false;
        for _ in 0..200 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
            ready = cx.update(|_w, cx| {
                let a = app.read(cx);
                let t = &a.model.borrow().tabs[a.model.borrow().active_tab];
                t.payload.editor.read(cx).value().len() >= src.len()
                    && t.payload.md_wy.as_ref().map(|w| w.read(cx).is_loaded()).unwrap_or(false)
            });
            if ready {
                break;
            }
        }
        assert!(ready, "大文档右栏 md_wy 应在 200 帧内装好");
        let editor = cx.update(|_w, cx| app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab].payload.editor.clone());
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.toggle_live_test(window, cx));
        });
        let mut l2_loaded = false;
        for _ in 0..200 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
            l2_loaded = cx.update(|_w, cx| {
                app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab]
                    .live2
                    .as_ref()
                    .map(|l| l.read(cx).is_loaded())
                    .unwrap_or(false)
            });
            if l2_loaded {
                break;
            }
        }
        assert!(l2_loaded, "live2 应在 200 帧内加载完大文档");
        // 大段插入 X（切回登记后台装配 CX）。
        let big_x = format!("## 大段 X\n{}\n", "X 内容，构成大跨度。\n".repeat(3000));
        cx.update(|_w, cx| {
            let live2 = app
                .read(cx)
                .model
                .borrow()
                .tabs[app.read(cx).model.borrow().active_tab]
                .live2
                .clone()
                .expect("live2");
            live2.update(cx, |e, cx| e.replace_range(0..0, &big_x, cx));
        });
        for _ in 0..10 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.toggle_live_test(window, cx));
        });
        // 尚未泵后台：异步装配在途（pending 挂着 CX）。
        let (pending, wy) = cx.update(|_w, cx| {
            let a = app.read(cx);
            let t = &a.model.borrow().tabs[a.model.borrow().active_tab];
            (
                t.payload.md_wy_install.pending.clone(),
                t.payload.md_wy.clone().expect("md_wy 应在"),
            )
        });
        let cx_snapshot = pending.clone().expect("在途装配应有 pending").1;
        // 在途窗口内：源码编辑区改成 CY（≠ CX），再同步喂源右栏 → 装配代号换代。
        let cy = format!("CY 全文基线\n{src}");
        cx.update(|_w, cx| {
            editor.update(cx, |ed, _| ed.set_text(&cy));
        });
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                let idx = app.model.borrow().active_tab;
                app.ensure_md_wy(idx, window, cx, true);
            });
        });
        let wy_now = cx.update(|_w, cx| wy.read(cx).text().to_string());
        assert_eq!(wy_now, cy, "同步喂源后右栏应立即显示 CY");
        assert!(cx_snapshot != cy, "测试前提：CX ≠ CY（否则无法检测陈旧覆盖）");
        // 泵后台完成：旧装配（CX）必须被代号守卫丢弃，右栏保持 CY。
        for _ in 0..200 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let wy_final = cx.update(|_w, cx| wy.read(cx).text().to_string());
        assert_eq!(wy_final, cy, "异步在途被同步喂源作废后，右栏必须保持 CY（不得回滚到 CX）");
        let _ = std::fs::remove_file(&md);
    }

    /// B3：**异步在途关闭预览（重建实体）不装错 tab（tab 归属守卫）**。大段切回登记后台
    /// 装配后，尚未落地前关闭预览（md_wy 实体被丢弃）再重开（新实体，同步喂 CY）→ 泵后台
    /// 完成：旧装配目标已不在原 tab，不得装进新实体，右栏保持 CY。
    #[gpui::test]
    async fn wysiwyg_toggle_off_async_inflight_dropped_when_preview_recreated(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1000.), px(800.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let md = std::env::temp_dir().join(format!(
            "gpui-wy-b3-entity-{}.md",
            std::process::id()
        ));
        let block = "# 大标题\n正文段落，用于 tab 归属守卫回归。\n\n- [ ] 待办\n";
        let src = block.repeat((600 * 1024) / block.len());
        std::fs::write(&md, &src).unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(md.to_str().unwrap(), window, cx));
        });
        let mut ready = false;
        for _ in 0..200 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
            ready = cx.update(|_w, cx| {
                let a = app.read(cx);
                let t = &a.model.borrow().tabs[a.model.borrow().active_tab];
                t.payload.editor.read(cx).value().len() >= src.len()
                    && t.payload.md_wy.as_ref().map(|w| w.read(cx).is_loaded()).unwrap_or(false)
            });
            if ready {
                break;
            }
        }
        assert!(ready, "大文档右栏 md_wy 应在 200 帧内装好");
        let editor = cx.update(|_w, cx| app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab].payload.editor.clone());
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.toggle_live_test(window, cx));
        });
        let mut l2_loaded = false;
        for _ in 0..200 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
            l2_loaded = cx.update(|_w, cx| {
                app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab]
                    .live2
                    .as_ref()
                    .map(|l| l.read(cx).is_loaded())
                    .unwrap_or(false)
            });
            if l2_loaded {
                break;
            }
        }
        assert!(l2_loaded, "live2 应在 200 帧内加载完大文档");
        let big_x = format!("## 大段 X\n{}\n", "X 内容，构成大跨度。\n".repeat(3000));
        cx.update(|_w, cx| {
            let live2 = app
                .read(cx)
                .model
                .borrow()
                .tabs[app.read(cx).model.borrow().active_tab]
                .live2
                .clone()
                .expect("live2");
            live2.update(cx, |e, cx| e.replace_range(0..0, &big_x, cx));
        });
        for _ in 0..10 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.toggle_live_test(window, cx));
        });
        // 尚未泵后台：异步装配在途；关闭预览（实体丢弃）再重开（新实体，同步喂编辑器文本）。
        let cy = format!("CY 全文基线\n{src}");
        cx.update(|_w, cx| {
            editor.update(cx, |ed, _| ed.set_text(&cy));
        });
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                let idx = app.model.borrow().active_tab;
                app.toggle_preview_for_tab(idx, window, cx);
            });
        });
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                let idx = app.model.borrow().active_tab;
                app.toggle_preview_for_tab(idx, window, cx);
            });
        });
        let new_wy_text = cx.update(|_w, cx| {
            app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab]
                .payload.md_wy
                .clone()
                .expect("重开后 md_wy 应在")
                .read(cx)
                .text()
                .to_string()
        });
        assert_eq!(new_wy_text, cy, "重开预览应立即显示 CY");
        // 泵后台完成：旧装配不得装进新实体，右栏保持 CY。
        for _ in 0..200 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let wy_final = cx.update(|_w, cx| {
            app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab]
                .payload.md_wy
                .clone()
                .expect("md_wy 应在")
                .read(cx)
                .text()
                .to_string()
        });
        assert_eq!(wy_final, cy, "异步在途预览重开后，右栏必须保持 CY（旧装配不得装新实体）");
        let _ = std::fs::remove_file(&md);
    }

    /// B3 基准（`--ignored`，诊断不断言）：**大跨度切回同步点击路径耗时**（debug）。
    /// 大段插入 + OFF，逐段打点：① OFF 同步路径（op 重放 + 最终态物化 + 登记后台装配，
    /// 不再含整篇同步 `Doc::open`）；② 后台 `open_from_str` 完成（右栏 md_wy 追平）墙钟。
    /// 旧实现该大段切回同步 ≈2.3s（其中整篇 `set_source_text` 同步 Doc::open ~1.5s）。
    /// 运行：`cargo test --bin gpui-editor app::tests::bench_b3_big_span_switch_async -- --ignored --nocapture`
    #[gpui::test]
    #[ignore]
    async fn bench_b3_big_span_switch_async(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1000.), px(800.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let md = std::env::temp_dir().join(format!(
            "gpui-wy-b3-bench-{}.md",
            std::process::id()
        ));
        // ≈6.4MB 大文档（与 `bench_wysiwyg_switch_fixed_overhead` 同口径，优先仓内真实
        // 富结构素材，缺失回退合成块）——「before」记录即此文档大段的 ~2.3s。
        let src = match std::fs::read(r"../../fixtures/大文档测试2.md") {
            Ok(b) => String::from_utf8_lossy(&b).into_owned(),
            Err(_) => {
                let block = "# 标题\n正文段落，凑大文档基准文本。\n\n- [ ] 待办\n";
                block.repeat((64 * 1024 * 100) / block.len())
            }
        };
        assert!(src.len() > 5 * 1024 * 1024, "基准文档应 ≥5MB，实际 {}", src.len());
        std::fs::write(&md, &src).unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(md.to_str().unwrap(), window, cx));
        });
        let mut ready = false;
        for _ in 0..200 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
            // 与 `bench_wysiwyg_switch_fixed_overhead` 同口径（真实素材可能是 CRLF，不能
            // 按字节长比对；行数 >1000 即视为内容已就位）。
            ready = cx.update(|_w, cx| {
                let a = app.read(cx);
                let t = &a.model.borrow().tabs[a.model.borrow().active_tab];
                !t.payload.editor_pending && t.payload.editor.read(cx).line_count() > 1000
            });
            if ready {
                break;
            }
        }
        assert!(ready, "大文档编辑区应在 200 帧内就绪");
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.toggle_live_test(window, cx));
        });
        let mut l2_loaded = false;
        for _ in 0..200 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
            l2_loaded = cx.update(|_w, cx| {
                app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab]
                    .live2
                    .as_ref()
                    .map(|l| l.read(cx).is_loaded())
                    .unwrap_or(false)
            });
            if l2_loaded {
                break;
            }
        }
        assert!(l2_loaded, "live2 应在 200 帧内加载完 6.4MB");
        // 大段插入（单 op，跨度 >64KB → 整篇喂源）。
        let big = format!("## 大段\n{}\n", "插入正文，构成大跨度。\n".repeat(5000));
        cx.update(|_w, cx| {
            let live2 = app
                .read(cx)
                .model
                .borrow()
                .tabs[app.read(cx).model.borrow().active_tab]
                .live2
                .clone()
                .expect("live2");
            live2.update(cx, |e, cx| e.replace_range(0..0, &big, cx));
        });
        for _ in 0..10 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        // ① OFF 同步路径耗时（写回 + 物化 + 登记异步；不含实体 Drop=已延迟、不含 Doc::open）。
        let off_ms = cx.update(|window, cx| {
            let t = std::time::Instant::now();
            app.update(cx, |app, cx| app.toggle_live_test(window, cx));
            t.elapsed().as_millis()
        });
        // ② 泵后台到右栏 md_wy 追平（异步装配完成）。
        let bg_t0 = std::time::Instant::now();
        let mut done = false;
        for _ in 0..400 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
            done = cx.update(|_w, cx| {
                app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab]
                    .payload.md_wy_install.pending
                    .is_none()
            });
            if done {
                break;
            }
        }
        assert!(done, "后台装配应在 400 帧内完成");
        let bg_ms = bg_t0.elapsed().as_millis();
        let (wy_len, committed_len) = cx.update(|_w, cx| {
            let a = app.read(cx);
            let t = &a.model.borrow().tabs[a.model.borrow().active_tab];
            (
                t.payload.md_wy.clone().map(|w| w.read(cx).len()).unwrap_or(0),
                t.payload.editor.read(cx).value().len(),
            )
        });
        println!("【B3·after】大段切回 OFF 同步路径 ≈ {off_ms} ms（旧实现 ≈2300 ms，含同步 Doc::open ~1500 ms）");
        println!("【B3·after】右栏后台追平（异步装配完成）墙钟 ≈ {bg_ms} ms");
        println!("【B3·after】右栏 md_wy {wy_len} B == 源码编辑区 {committed_len} B（最终一致）");
        let _ = std::fs::remove_file(&md);
    }

    /// A 基准（`--ignored`，诊断不断言）：6.4MB 大文档「单字符」切回**同步点击路径**
    /// 的分项实测——① `commit_live` 最终态物化（`text_shared` 冷一次，= A1 剩余下限
    ///  ~20ms）；② WyEditor 实体**级联 Drop**（= A2 已移出点击路径的 ~36ms），用它
    /// 还原「改动前」预估值 = 本测试的 OFF 耗时 + ②。运行：
    /// `cargo test --bin gpui-editor app::tests::bench_wysiwyg_switch_fixed_overhead -- --ignored --nocapture`
    #[gpui::test]
    #[ignore]
    async fn bench_wysiwyg_switch_fixed_overhead(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1000.), px(800.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 6.4MB 文档：优先取仓内真实富结构 `fixtures/大文档测试2.md`（≈6.27MB，含标题/
        // 表格/代码/待办等，与 63ms 基线的同一批素材），缺失则回退合成重复块。
        let src = match std::fs::read(r"../../fixtures/大文档测试2.md") {
            Ok(b) => String::from_utf8_lossy(&b).into_owned(),
            Err(_) => {
                let block = "# 标题\n这是正文段落，凑 6.4MB 大文档的切回基准文本。\n\n- [ ] 待办\n";
                block.repeat((64 * 1024 * 100) / block.len())
            }
        };
        assert!(src.len() > 5 * 1024 * 1024, "基准文档应 ≥5MB，实际 {}", src.len());
        let md = std::env::temp_dir().join(format!(
            "gpui-wy-bench-{}.md",
            std::process::id()
        ));
        std::fs::write(&md, &src).unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(md.to_str().unwrap(), window, cx));
        });
        // 等到编辑区挂载完成（大文档两段式打开）。
        let mut ready = false;
        for _ in 0..200 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
            ready = cx.update(|_w, cx| {
                let a = app.read(cx);
                let t = &a.model.borrow().tabs[a.model.borrow().active_tab];
                !t.payload.editor_pending && t.payload.editor.read(cx).line_count() > 1000
            });
            if ready {
                break;
            }
        }
        assert!(ready, "大文档编辑区应在 200 帧内就绪");
        // ON，等 live2 后台解析就绪。
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.toggle_live_test(window, cx));
        });
        let mut l2_loaded = false;
        for _ in 0..200 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
            l2_loaded = cx.update(|_w, cx| {
                app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab]
                    .live2
                    .as_ref()
                    .map(|l| l.read(cx).is_loaded())
                    .unwrap_or(false)
            });
            if l2_loaded {
                break;
            }
        }
        assert!(l2_loaded, "live2 应在 200 帧内加载完 6.4MB");
        // 单字符编辑。
        cx.update(|_w, cx| {
            let live = app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab]
                .live2.clone().expect("live2");
            let e = live.read(cx).text().to_string();
            let at = e.find('段').expect("应有中文");
            live.update(cx, |ed, cx| ed.replace_range(at..at + 3, "更", cx));
        });
        for _ in 0..10 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        // ② 实体级联 Drop 成本（A2 已移出点击路径的分项）：OFF 前 clone 现实中的
        // live2 句柄（live 会话已被 app 真实渲染过多帧、布局/整形缓存全就位）。OFF 后
        // 让延迟回收队列清空、再把最后的句柄 drop 掉——complete 级联释放就是旧代码在
        // 点击路径内同步做掉的成本。
        let live_handle = cx.update(|_w, cx| {
            app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab]
                .live2.clone().expect("live2")
        });
        // ① OFF 切回（改动后）同步路径耗时 = 写回 + A1 物化下限；不含 Drop（已延迟到
        // 下一帧，`ref retired_live2` 探针见 `wysiwyg_toggle_off_retired_live2_drained_next_frame`）。
        let off_ms = cx.update(|window, cx| {
            let t = std::time::Instant::now();
            app.update(cx, |app, cx| app.toggle_live_test(window, cx));
            t.elapsed().as_millis()
        });
        // ② OFF 后 app 侧不再持有旧实体（render + on_next_frame 清空退休队列）；实体的
        // Rc 级联释放此前在点击路径内同步做（旧实现），现移到这之后。留一个句柄测其
        // 释放被移出后点击路径的后果：此处 drop 已与点击路径无涉（仅供参考量级）。
        for _ in 0..4 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        for _ in 0..6 {
            cx.run_until_parked();
            let _ = cx.update(|window, cx| window.simulate_next_frame(cx));
            cx.run_until_parked();
        }
        let drop_ms = cx.update(|_w, _cx| {
            let t = std::time::Instant::now();
            drop(live_handle);
            t.elapsed().as_millis()
        });
        println!("【A2 分项】页点击路径内已不再持有/释放 live2；残留句柄 Drop ≈ {drop_ms} ms（量级参考）");
        println!("【A·after】切回同步路径（写回+物化，不含 Drop）≈ {off_ms} ms");
        println!("【A·before 参考】旧 GUI 实测 ≈63ms（1 字符，HEAD~0 提交文档）＝本 after + 被移出的实体 Drop ~36ms");
        let _ = std::fs::remove_file(&md);
    }

    /// Step3：大文档 md 冷开「首块 prefix 首装」——`md_wy_spawn_prefix_install`
    /// 后台 `open_from_rope(首块 rope 快照)` + `install_open` → **首屏即真内容**
    ///（`is_loaded()==true`、内容 == 首块 prefix），并置「整篇在途」标记
    /// `md_wy_pending_full=(代号, prefix_len)`。测试钩子的 `run_until_parked` 一次排空
    /// 全部后台任务、看不到真实 open 的「prefix 已装、整篇未到」中间窗口（Step2 已遇），
    /// 用**等价状态布置**（编辑器 rope = 首块内容 + 空壳 md_wy + `md_wy_early=Pending`）
    /// 走真实渲染路径（`open_from_rope` → `install_open`）断言 prefix 首装。
    #[gpui::test]
    async fn md_wy_prefix_first_install_shows_content(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 首块内容（≈几十 KB）：线 4i 为 `## 标题`（块起点），与真实 ChunkReader 首块同为
        // 「行完整、LF 归一」文本。
        let prefix = (0..1600)
            .map(|i| format!("## 标题 {i}\n\n正文段 {i}。\n\n"))
            .collect::<String>();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                let idx = app.model.borrow().active_tab;
                let rope = ropey::Rope::from(prefix.as_str());
                app.model.borrow().tabs[idx].payload.editor.update(cx, |ed, cx| {
                    ed.set_rope_with_stats(rope.clone(), 0, Default::default());
                    cx.notify();
                });
                app.ensure_md_wy(idx, window, cx, false);
                app.model.borrow_mut().tabs[idx].payload.preview_on = true;
                // 等价 `md_wy_spawn_early_install` 启动后（代号 = ensure_md_wy 已 invalidate →
                // seq=1；整篇在途、未编辑）。
                app.model.borrow_mut().tabs[idx].payload.md_wy_install.early = Some(crate::app::MdWyEarly::Pending { seq: 1, edit_version: 0 });
                app.md_wy_spawn_prefix_install(idx, rope.clone(), 1, cx);
            });
        });
        for _ in 0..20 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let (loaded, text, pending, early) = cx.update(|_, cx| {
            let a = app.read(cx);
            let i = a.model.borrow().active_tab;
            let wy = a.model.borrow().tabs[i].payload.md_wy.clone().expect("prefix 首装前应已建空壳");
            (
                wy.read(cx).is_loaded(),
                wy.read(cx).text().to_string(),
                a.model.borrow().tabs[i].payload.md_wy_install.pending_full,
                a.model.borrow().tabs[i].payload.md_wy_install.early,
            )
        });
        assert!(loaded, "prefix 首装应 `install_open`（loaded=true），首屏立刻真内容");
        assert_eq!(text, prefix, "首屏预览内容应 = 首块 prefix（编辑器 rope 快照）");
        assert_eq!(
            pending,
            Some((1, prefix.len())),
            "prefix 首装后应置「整篇在途」标记 (代号, prefix_len)"
        );
        assert_eq!(
            early,
            Some(crate::app::MdWyEarly::Pending { seq: 1, edit_version: 0 }),
            "整篇仍应在途（prefix 首装不消费 early 状态）"
        );
    }

    /// Step3：整篇就绪后 `md_wy_install_full_swap` **带守卫换入全文 + 滚动锚**——换入前
    /// 顶行 `src_line` 在换入后保持不变（视觉不跳），内容 == 全文，`md_wy_early=Done`、
    /// `md_wy_pending_full=None`（整篇已装，增量桥恢复）。真实渲染路径：prefix 首装 →
    /// 编辑器与预览同滚到标题线 300（块起点）→ 换入全文 → 逐帧刷新断言锚定。
    #[gpui::test]
    async fn md_wy_full_swap_anchors_top_src_line(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let full = (0..2200)
            .map(|i| format!("## 标题 {i}\n\n正文段 {i}。\n\n"))
            .collect::<String>();
        let prefix = (0..1600)
            .map(|i| format!("## 标题 {i}\n\n正文段 {i}。\n\n"))
            .collect::<String>();
        assert!(prefix.len() < full.len(), "prefix 应为全文真前缀");
        let target_line = 300usize; // 4*75 → `## 标题 75`（块起点，prefix / full 同名同列）
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                let idx = app.model.borrow().active_tab;
                let rope = ropey::Rope::from(prefix.as_str());
                app.model.borrow().tabs[idx].payload.editor.update(cx, |ed, cx| {
                    ed.set_rope_with_stats(rope.clone(), 0, Default::default());
                    cx.notify();
                });
                app.ensure_md_wy(idx, window, cx, false);
                app.model.borrow_mut().tabs[idx].payload.preview_on = true;
                app.model.borrow_mut().tabs[idx].payload.md_wy_install.early =
                    Some(crate::app::MdWyEarly::Pending { seq: 1, edit_version: 0 });
                app.md_wy_spawn_prefix_install(idx, rope.clone(), 1, cx);
            });
        });
        // prefix 就绪并渲染（宿主帧建立 → viewport_top 推入）。
        for _ in 0..20 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        // 预览与编辑器同滚到目标标题线（对齐 app 双向滚动同步，避免它在测试内互拉）。
        cx.update(|_, cx| {
            let (mdw, ed) = {
                let a = app.read(cx);
                let idx = a.model.borrow().active_tab;
                (
                    a.model.borrow().tabs[idx].payload.md_wy.clone().unwrap(),
                    a.model.borrow().tabs[idx].payload.editor.clone(),
                )
            };
            mdw.update(cx, |w, _| w.set_scroll_target(target_line));
            ed.update(cx, |ed, _| ed.scroll_to_line(target_line));
        });
        let mut before: Option<usize> = None;
        for _ in 0..30 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
            before = cx.update(|_, cx| {
                let a = app.read(cx);
                let i = a.model.borrow().active_tab;
                a.model.borrow().tabs[i].payload.md_wy.as_ref().unwrap().read(cx).top_source_line()
            });
            // 首帧 viewport_top 还是旧值（0），advance_pending_scroll 逐帧重钉后收敛。
            if before.is_some_and(|b| b != 0) {
                break;
            }
        }
        let before = before.expect("预览滚动后应有顶行 src_line");
        assert!(
            before != 0,
            "滚动到标题 300 后顶行不应为 0（实际 {before}）"
        );
        // 整篇就绪 → 带守卫换入全文（同 `md_wy_spawn_early_install` 完成回调的落地）。
        let wy_open = markdown_wysiwyg::WyEditor::open_from_str(full.clone().into());
        cx.update(|_, cx| {
            app.update(cx, |app, cx| {
                let idx = app.model.borrow().active_tab;
                let wy = app.model.borrow().tabs[idx].payload.md_wy.clone().unwrap();
                app.md_wy_install_full_swap(idx, &wy, wy_open, cx);
            });
        });
        let mut after: Option<usize> = None;
        for _ in 0..30 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
            after = cx.update(|_, cx| {
                let a = app.read(cx);
                let i = a.model.borrow().active_tab;
                a.model.borrow().tabs[i].payload.md_wy.as_ref().unwrap().read(cx).top_source_line()
            });
            // 换入后由 advance_pending_scroll 重钉回锚点；收敛到 before 即稳定。
            if after == Some(before) {
                break;
            }
        }
        let (after, text, early, pending) = cx.update(|_, cx| {
            let a = app.read(cx);
            let i = a.model.borrow().active_tab;
            let model = a.model.borrow();
            let wy = model.tabs[i].payload.md_wy.as_ref().unwrap();
            (
                after,
                wy.read(cx).text().to_string(),
                a.model.borrow().tabs[i].payload.md_wy_install.early,
                a.model.borrow().tabs[i].payload.md_wy_install.pending_full,
            )
        });
        assert_eq!(after, Some(before), "整篇换入后顶行 src_line 应锚定不变（视觉不跳）");
        assert_eq!(text, full, "整篇换入后预览内容应 == 全文");
        assert_eq!(
            early,
            Some(crate::app::MdWyEarly::Done),
            "整篇换入应置 md_wy_early = Done（EOF 收尾据此跳过重复 install）"
        );
        assert_eq!(pending, None, "整篇已装 → 解除「整篇在途」标记（增量桥恢复逐键）");
    }

    /// Step3：prefix 首装后用户**编辑** → `sync::on_editor_edited` 不得把编辑区坐标增量
    /// 打到 prefix（预览 ≠ 编辑区，会错位）；应作废在途整篇 + 以当前编辑区 Rope 异步整篇
    /// 重喂（`md_wy_feed_full_rope`），装好后预览 == 编辑区，此后退化逐键增量——**不装旧**。
    /// 等价状态布置：编辑器 rope = 全文、预览 = 仅 prefix（`md_wy_pending_full` 在途），
    /// 真实 `Paste` 落进编辑区触发增量桥，断言最终预览恒 == 编辑区。
    #[gpui::test]
    async fn md_wy_prefix_edit_revokes_full_and_sync_refeeds(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let full = (0..2200)
            .map(|i| format!("## 标题 {i}\n\n正文段 {i}。\n\n"))
            .collect::<String>();
        let prefix = (0..1600)
            .map(|i| format!("## 标题 {i}\n\n正文段 {i}。\n\n"))
            .collect::<String>();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                let idx = app.model.borrow().active_tab;
                // 编辑器 rope = 全文（等价「编辑区已追加超出 prefix」）；预览只装 prefix。
                let rope_full = ropey::Rope::from(full.as_str());
                app.model.borrow().tabs[idx].payload.editor.update(cx, |ed, cx| {
                    ed.set_rope_with_stats(rope_full.clone(), 0, Default::default());
                    cx.notify();
                });
                app.ensure_md_wy(idx, window, cx, false);
                app.model.borrow_mut().tabs[idx].payload.preview_on = true;
                app.model.borrow_mut().tabs[idx].payload.md_wy_install.early =
                    Some(crate::app::MdWyEarly::Pending { seq: 1, edit_version: 0 });
                let rope_prefix = ropey::Rope::from(prefix.as_str());
                app.md_wy_spawn_prefix_install(idx, rope_prefix.clone(), 1, cx);
            });
        });
        for _ in 0..20 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        // 前置断言：预览 = prefix（已装、整篇在途），与编辑区长度不等。
        let (pre_loaded, pre_pending, pre_len) = cx.update(|_, cx| {
            let a = app.read(cx);
            let i = a.model.borrow().active_tab;
            let model = a.model.borrow();
            let wy = model.tabs[i].payload.md_wy.as_ref().unwrap();
            (wy.read(cx).is_loaded(), a.model.borrow().tabs[i].payload.md_wy_install.pending_full, wy.read(cx).len())
        });
        assert!(pre_loaded, "prefix 应已 install（loaded）");
        assert_eq!(pre_pending, Some((1, prefix.len())));
        assert!(pre_len < full.len(), "预览=prefix 应短于编辑区全文");
        // 真实编辑：聚焦编辑区 + 粘贴（走 EditorEvent::Edited → 增量桥）。
        cx.update(|window, cx| {
            let focus = app.read(cx).editor_focus_handle(cx);
            window.focus(&focus, cx);
        });
        cx.write_to_clipboard(gpui::ClipboardItem::new_string("早编插入".to_string()));
        cx.dispatch_action(Paste);
        for _ in 0..20 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let (wy_text, ed_text, pending, loaded) = cx.update(|_, cx| {
            let a = app.read(cx);
            let i = a.model.borrow().active_tab;
            let model = a.model.borrow();
            let wy = model.tabs[i].payload.md_wy.as_ref().unwrap();
            (
                wy.read(cx).text().to_string(),
                a.model.borrow().tabs[i].payload.editor.read(cx).value(),
                a.model.borrow().tabs[i].payload.md_wy_install.pending_full,
                wy.read(cx).is_loaded(),
            )
        });
        assert!(loaded);
        assert!(
            ed_text.starts_with("早编插入") || ed_text.contains("早编插入"),
            "编辑区应含粘贴内容（实际前 20 字符 {:?}）",
            &ed_text[..ed_text.len().min(20)]
        );
        assert_eq!(
            wy_text, ed_text,
            "pending_full 期间编辑 → 应作废在途整篇 + 以当前编辑区全文重喂（预览恒 == 编辑区，\
             不装旧、不把增量错位打到 prefix）"
        );
        assert_eq!(
            pending, None,
            "编辑触发的整篇重喂落地后（小 rope 同步装）应解除在途标记"
        );
    }

    /// 左面板 live2 **未就绪冻结源码**回归（修复「预览编辑测试切左面板 WYSIWYG 白屏」①）：
    /// 空壳 live2 未 `is_loaded()`（大文档后台装配在途）时，左面板不渲染空白 WYSIWYG 帧
    /// （原白屏根因），而是**冻结渲染切换前的源码编辑区** + 全区域透明输入拦截层
    /// （`#live-pending-input-overlay`，`occlude()` 吞鼠标/滚轮，防窗口期内输入进源码与
    /// 后台解析快照分歧）；`install_open`（`is_loaded()==true`）后同一帧起切正式 live 宿主
    /// （`#live-scroll`）、拦截层消失。
    ///
    /// 测试说明：本测试钩子 `run_until_parked` 会一次排空后台任务，真实 toggle ON 的
    /// 「空壳已建、装配在途」窗口无法在帧间观察到 → 用**等价状态布置**（建空壳 live2 =
    /// 大文档装配完成前态，同 `md_wy_pending_placeholder_renders_until_loaded` 手法）走
    /// 真实渲染路径断言冻结 → 就绪切换。
    #[gpui::test]
    async fn wysiwyg_live2_pending_freeze_renders_source_then_live_area(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 打开一个小 md（让冻结展示的源码编辑区有内容可看）。
        let md = std::env::temp_dir().join(format!(
            "gpui-wy-live-pending-{}.md",
            std::process::id()
        ));
        std::fs::write(&md, "# 标题\n正文。\n\n```rust\nlet a = 1;\n```\n").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(md.to_str().unwrap(), window, cx));
        });
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        // 等价大文档装配在途：建「空壳未装」的左面板 live2（不 install_open → is_loaded()==false）。
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                let idx = app.model.borrow().active_tab;
                let ed = cx.new(|cx| markdown_wysiwyg::WyEditor::new(window, cx));
                ed.update(cx, |ed, _| ed.capture_live_ops());
                app.model.borrow_mut().tabs[idx].live2 = Some(ed.clone());
                app.model.borrow_mut().tabs[idx]._live2_sub = None;
                app.model.borrow_mut().tabs[idx].live_edit = true;
                app.model.borrow_mut().tabs[idx].live2_dirty = false;
            });
        });
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let (loaded,) = cx.update(|_, cx| {
            let a = app.read(cx);
            let live = a.model.borrow().tabs[a.model.borrow().active_tab].live2.clone().expect("live2 应已创建");
            (live.read(cx).is_loaded(),)
        });
        let (freeze_shown, live_shown) = (
            cx.debug_bounds("live-pending-input-overlay").is_some(),
            cx.debug_bounds("live-scroll").is_some(),
        );
        assert!(!loaded, "空壳应处于未就绪");
        assert!(
            freeze_shown && !live_shown,
            "未就绪应渲染冻结源码 + 输入拦截层（#live-pending-input-overlay），且不得出现正式宿主 #live-scroll"
        );
        // 就绪：install 全量 → 下一帧起正式 live 宿主出现、拦截层消失。
        cx.update(|_window, cx| {
            app.update(cx, |app, cx| {
                let live = app.model.borrow().tabs[app.model.borrow().active_tab].live2.clone().unwrap();
                live.update(cx, |ed, _cx| {
                    ed.install_open(markdown_wysiwyg::WyEditor::open_from_str(
                        "# 就绪正文\n\n装配完成后的 WYSIWYG 内容。\n".into(),
                    ));
                });
            });
        });
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let (loaded,) = cx.update(|_, cx| {
            let a = app.read(cx);
            let live = a.model.borrow().tabs[a.model.borrow().active_tab].live2.clone().expect("live2 应仍在");
            (live.read(cx).is_loaded(),)
        });
        let (freeze_shown, live_shown) = (
            cx.debug_bounds("live-pending-input-overlay").is_some(),
            cx.debug_bounds("live-scroll").is_some(),
        );
        assert!(loaded, "install_open 后应就绪");
        assert!(
            live_shown && !freeze_shown,
            "就绪后应渲染正式 live 宿主（#live-scroll），拦截层消失"
        );
        let _ = std::fs::remove_file(&md);
    }

    /// 共享复用右栏已解析 Doc（修复「预览编辑测试」白屏 ②）回归：大文档 md 右栏 `md_wy`
    /// 已 loaded（== 编辑区）时 toggle ON → 守卫命中**复用路径**（探针 +1）——live2 装配
    /// 自 `md_wy.doc().clone()`（O(1) 共享、消双解析），`is_loaded()` 后 len == 编辑区（
    /// 全文即见）；md_wy 不受影响（仍为源全文）；live2 首次编辑后 md_wy Doc 逐字节不变
    /// （`Doc::clone` 的「克隆共享、编辑即 COW 分离」，右栏只读预览不被左面板编辑污染）。
    #[gpui::test]
    async fn wysiwyg_toggle_on_reuses_md_wy_doc_cow(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1000.), px(800.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let md = std::env::temp_dir().join(format!(
            "gpui-wy-b3-reuse-{}.md",
            std::process::id()
        ));
        // 大文档（≥512KB）走既有大文档打开流：右栏 md_wy 由「打开即提前整篇解析」装好。
        let block = "# 大标题\n这是正文段落，用于 WYSIWYG 共享复用右栏已解析 Doc 的回归。\n\n- [ ] 待办\n";
        let src = block.repeat((600 * 1024) / block.len());
        assert!(src.len() >= 512 * 1024, "大文档应 ≥512KB，实际 {}", src.len());
        std::fs::write(&md, &src).unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(md.to_str().unwrap(), window, cx));
        });
        let mut ready = false;
        for _ in 0..200 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
            ready = cx.update(|_w, cx| {
                let a = app.read(cx);
                let t = &a.model.borrow().tabs[a.model.borrow().active_tab];
                t.payload.editor.read(cx).value().len() >= src.len()
                    && t.payload.md_wy.as_ref().map(|w| w.read(cx).is_loaded()).unwrap_or(false)
            });
            if ready {
                break;
            }
        }
        assert!(ready, "大文档右栏 md_wy 应在 200 帧内装好");
        let md_old = cx.update(|_w, cx| {
            app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab]
                .payload.md_wy
                .clone()
                .expect("md_wy 应存在")
                .read(cx)
                .text()
                .to_string()
        });
        assert_eq!(md_old, src, "toggle 前右栏应为原文档全文");
        // ON：守卫该命中共享复用路径（探针 +1）。
        let reuse_before = crate::app::preview::LIVE_ON_MD_WY_REUSE
            .load(std::sync::atomic::Ordering::Relaxed);
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.toggle_live_test(window, cx));
        });
        let reuse_after = crate::app::preview::LIVE_ON_MD_WY_REUSE
            .load(std::sync::atomic::Ordering::Relaxed);
        // 全局复用探针：并行的其它大文档 toggle ON 用例（同一 ≥512KB + md_wy 已 loaded
        // 同源布置）也走同一复用路径、可能并发推进计数 → 用 ≥（本用例至少一次），与
        // 既有 `LIVE_COMMIT_*` 全局探针判据同款（见 1207/1314 注释）。「本次 toggle 真的
        // 复用」的确定性判据是下方 live2 len/text == 编辑区全文、右栏 md_wy 不被改动。
        assert!(
            reuse_after > reuse_before,
            "大文档 toggle ON 且右栏 md_wy 已 loaded 应命中共享复用路径"
        );
        let mut l2_loaded = false;
        for _ in 0..200 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
            l2_loaded = cx.update(|_w, cx| {
                app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab]
                    .live2
                    .as_ref()
                    .map(|l| l.read(cx).is_loaded())
                    .unwrap_or(false)
            });
            if l2_loaded {
                break;
            }
        }
        assert!(l2_loaded, "live2 应在 200 帧内装配完（复用路径 = 克隆 + 预建）");
        let (l2_len, l2_text, md_now) = cx.update(|_w, cx| {
            let a = app.read(cx);
            let t = &a.model.borrow().tabs[a.model.borrow().active_tab];
            let l2 = t.live2.clone().expect("live2 应存在");
            (
                l2.read(cx).len(),
                l2.read(cx).text().to_string(),
                t.payload.md_wy.clone().map(|w| w.read(cx).text().to_string()),
            )
        });
        assert_eq!(l2_len, src.len(), "live2 应全文即见（复用已解析 Doc，len == 编辑区）");
        assert_eq!(l2_text, src, "live2 内容应 == 编辑区全文（复用同源）");
        assert_eq!(md_now.as_deref(), Some(src.as_str()), "live2 装配不得改动右栏 md_wy");
        // live2 首次编辑 → 自身 Doc::edit COW 分离，右栏 md_wy Doc 逐字节不变。
        cx.update(|_w, cx| {
            let live = app
                .read(cx)
                .model.borrow().tabs[app.read(cx).model.borrow().active_tab]
                .live2
                .clone()
                .expect("live2 应存在");
            live.update(cx, |e, cx| e.replace_range(0..0, "X", cx));
        });
        for _ in 0..10 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let (l2_after, md_after) = cx.update(|_w, cx| {
            let a = app.read(cx);
            let t = &a.model.borrow().tabs[a.model.borrow().active_tab];
            (
                t.live2.clone().expect("live2 应存在").read(cx).text().to_string(),
                t.payload.md_wy.clone().map(|w| w.read(cx).text().to_string()),
            )
        });
        assert!(l2_after.starts_with('X'), "live2 编辑应生效：{l2_after:?}");
        assert_eq!(
            md_after.as_deref(),
            Some(src.as_str()),
            "live2 编辑一次后右栏 md_wy Doc 必须保持原状（克隆共享、编辑即 COW 分离）"
        );
        let _ = std::fs::remove_file(&md);
    }

    /// P10：数学编辑卡在 live2（WYSIWYG 实时编辑）内聚焦时，Ctrl+V（真实 app 走
    /// `PasteNormalized` 覆盖绑定 → app 根 `paste_normalized`）必须把剪贴板**路由进
    /// 卡片输入区**，而不是误粘进源码编辑区；写回经卡片 `Edited` 订阅落到 live2 文档。
    /// 键盘解析/派发走真实链路（聚焦卡片 + ctrl-v 键 + 与 main.rs 同序的
    /// PasteNormalized 覆盖绑定）。
    #[gpui::test]
    async fn wysiwyg_live_math_edit_ctrl_v_pastes_into_card_not_source_editor(
        cx: &mut TestAppContext,
    ) {
        use crate::actions::PasteNormalized;
        use gpui::{App, KeyBinding, Keystroke};
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(800.), px(800.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let md = std::env::temp_dir().join(format!(
            "gpui-wy-math-paste-{}.md",
            std::process::id()
        ));
        let src = "$$\na + b\n$$\n尾行\n";
        std::fs::write(&md, src).unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(md.to_str().unwrap(), window, cx));
        });
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        // ON：进入 WYSIWYG（live2）实时编辑模式。
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.toggle_live_test(window, cx));
        });
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let live = cx.update(|_w, cx| {
            app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab]
                .live2
                .clone()
                .expect("live2 应存在")
        });
        // 程序化打开数学编辑卡（卡片锚定第 0 块）+ 聚焦卡片输入区。
        cx.update(|window, cx| {
            live.update(cx, |e, cx| e.open_math_edit(0, window, cx));
        });
        cx.update(|window, cx| {
            live.update(cx, |e, cx| {
                let fh = e
                    .math_edit_input()
                    .expect("应有卡片输入区")
                    .read(cx)
                    .focus_handle();
                window.focus(&fh, cx);
            });
            let _ = window.draw(cx);
        });
        let card = cx.update(|_w, cx| {
            live.read(cx).math_edit_input().expect("应有卡片输入区")
        });
        // 与 main.rs 同序：在 `bind_local_keymaps` **之后**补 `PasteNormalized` 覆盖
        // 绑定（更高优先级 → 真实 app 链路：Ctrl+V → PasteNormalized → app 根路由）。
        cx.update(|_w, cx: &mut App| {
            cx.bind_keys([KeyBinding::new("ctrl-v", PasteNormalized, Some("Input"))]);
        });
        // Ctrl+V 粘贴 → 应进卡片输入区（而非源码编辑区）。
        let wh = cx.windows().first().copied().expect("窗口句柄");
        cx.write_to_clipboard(gpui::ClipboardItem::new_string("XY".to_string()));
        cx.dispatch_keystroke(wh, Keystroke::parse("ctrl-v").unwrap());
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let (card_text, src_value, live_text) = cx.update(|_w, cx: &mut App| {
            let a = app.read(cx);
            let t = &a.model.borrow().tabs[a.model.borrow().active_tab];
            (
                card.read(cx).text_string(),
                t.payload.editor.read(cx).value().to_string(),
                t.live2.clone().expect("live2 应存在").read(cx).text().to_string(),
            )
        });
        assert_eq!(
            card_text.as_str(),
            "XYa + b",
            "Ctrl+V 应把剪贴板粘贴进数学编辑卡输入区：card={card_text:?}"
        );
        assert_eq!(
            src_value.as_str(),
            src,
            "源码编辑区不得被误粘（P10：修复前 Ctrl+V 落在源码编辑区）：src={src_value:?}"
        );
        assert_eq!(
            live_text.as_str(),
            "$$\nXYa + b\n$$\n尾行\n",
            "卡片粘贴写回 live2 文档（Edited 订阅，围栏形态保留）：live={live_text:?}"
        );
        let _ = std::fs::remove_file(&md);
    }

    /// P10 同族（第 2 块/最后一块）：图片编辑卡的**链接输入框**在 live2（WYSIWYG 实时
    /// 编辑）内聚焦时，Ctrl+V（真实 app 走 `PasteNormalized` 覆盖绑定 → app 根
    /// `paste_normalized`）必须把剪贴板**路由进**卡内链接框（光标处插入），而不是误粘进
    /// 源码编辑区；图片卡保存前不改源码 → live2 文档保持原状（对比数学卡：图片卡是
    /// 输入框暂存 + 点「保存」才写回）。
    #[gpui::test]
    async fn wysiwyg_live_image_edit_ctrl_v_pastes_into_card_not_source_editor(
        cx: &mut TestAppContext,
    ) {
        use crate::actions::PasteNormalized;
        use gpui::{App, KeyBinding, Keystroke};
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(800.), px(800.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let md = std::env::temp_dir().join(format!(
            "gpui-wy-image-paste-{}.md",
            std::process::id()
        ));
        let src = "![独立图](img测试.png)\n";
        std::fs::write(&md, src).unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(md.to_str().unwrap(), window, cx));
        });
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        // ON：进入 WYSIWYG（live2）实时编辑模式。
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.toggle_live_test(window, cx));
        });
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let live = cx.update(|_w, cx| {
            app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab]
                .live2
                .clone()
                .expect("live2 应存在")
        });
        // 程序化打开图片编辑卡（锚定第 0 行图片）+ 显式聚焦链接输入框。
        cx.update(|window, cx| {
            live.update(cx, |e, cx| {
                e.open_image_edit(
                    markdown_wysiwyg::ImageEditTarget::Block { src_line: 0 },
                    window,
                    cx,
                );
                let fh = e
                    .image_edit_link_input()
                    .expect("应有链接输入框")
                    .read(cx)
                    .focus_handle(cx);
                window.focus(&fh, cx);
            });
            let _ = window.draw(cx);
        });
        let link = cx.update(|_w, cx| {
            live.read(cx).image_edit_link_input().expect("应有链接输入框")
        });
        // 与 main.rs 同序：在 `bind_local_keymaps` **之后**补 `PasteNormalized` 覆盖
        // 绑定（更高优先级 → 真实 app 链路：Ctrl+V → PasteNormalized → app 根路由）。
        cx.update(|_w, cx: &mut App| {
            cx.bind_keys([KeyBinding::new("ctrl-v", PasteNormalized, Some("Input"))]);
        });
        // Ctrl+V 粘贴 → 应进图片卡链接框（而非源码编辑区）。
        let wh = cx.windows().first().copied().expect("窗口句柄");
        cx.write_to_clipboard(gpui::ClipboardItem::new_string("XY".to_string()));
        cx.dispatch_keystroke(wh, Keystroke::parse("ctrl-v").unwrap());
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let (link_val, src_value, live_text) = cx.update(|_w, cx: &mut App| {
            let a = app.read(cx);
            let t = &a.model.borrow().tabs[a.model.borrow().active_tab];
            (
                link.read(cx).value().to_string(),
                t.payload.editor.read(cx).value().to_string(),
                t.live2.clone().expect("live2 应存在").read(cx).text().to_string(),
            )
        });
        assert_eq!(
            link_val.as_str(),
            "img测试.pngXY",
            "Ctrl+V 应把剪贴板粘贴进图片卡链接框（光标在末尾插入）：link={link_val:?}"
        );
        assert_eq!(
            src_value.as_str(),
            src,
            "源码编辑区不得被误粘（修复前 Ctrl+V 落在源码编辑区）：src={src_value:?}"
        );
        assert_eq!(
            live_text.as_str(),
            src,
            "live2 文档不得被误粘（图片卡点保存才写回源码）：live={live_text:?}"
        );
        let _ = std::fs::remove_file(&md);
    }

    /// 场景 B（用户真机确认）：WYSIWYG live2 编辑区点击**内容元素下方大片空白** →
    /// 光标应跳到文档内容末尾并聚焦（等效于点末尾后）。live2 是 auto-grow（元素高=内容
    /// 高），这片空白在元素之外、`#live-scroll` 容器之内——普通鼠标事件到不了 WyEditor，
    /// 由 `render_live_area` 在容器挂的转发处理（严格低于 `last_bounds` 底部才转发，
    /// 元素内点击放行给 WyEditor 自身命中路径，不重复触发）。
    #[gpui::test]
    async fn wysiwyg_live_click_below_content_jumps_caret_to_end(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1000.), px(800.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let md = std::env::temp_dir().join(format!(
            "gpui-wy-below-click-{}.md",
            std::process::id()
        ));
        std::fs::write(&md, "# 标题\n一段正文内容。\n").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(md.to_str().unwrap(), window, cx));
        });
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
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
        // 取 live2 + 其元素底部（paint 期 last_bounds），点击点须在元素底之下、编辑区内。
        let live2_ent = cx.update(|_w, cx| {
            app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab]
                .live2
                .clone()
                .expect("live2 应存在")
        });
        let (len, bottom) = cx.update(|_w, cx| {
            let e = live2_ent.read(cx);
            let b = e.last_bounds().expect("live2 已渲染应有 last_bounds");
            (e.len(), b.bottom().as_f32())
        });
        let area = cx.debug_bounds("editor-area").expect("编辑区应渲染");
        let click_y = bottom + 40.;
        assert!(
            click_y < area.bottom().as_f32() - 2.,
            "点击点应在编辑区内（area origin={:?} bottom={:.1}，click_y={:.1}）否则内容超高本用例无效",
            area.origin,
            area.bottom().as_f32(),
            click_y
        );
        let click_x = (area.left() + px(40.)).as_f32();
        // 先落一个确定的非末尾光标（行 0），确认下方空白点击真实「移动」到末尾并聚焦。
        cx.update(|_w, cx| {
            live2_ent.update(cx, |e, _| e.set_caret(0));
        });
        for _ in 0..3 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        assert_eq!(
            cx.update(|_w, cx| live2_ent.read(cx).caret()),
            0,
            "前置：光标应先落在非末尾"
        );
        cx.simulate_click(
            gpui::point(px(click_x), px(click_y)),
            gpui::Modifiers::default(),
        );
        for _ in 0..6 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let (caret_after, focused) = cx.update(|_w, cx| {
            let e = live2_ent.read(cx);
            (e.caret(), e.is_focused())
        });
        assert_eq!(
            caret_after, len,
            "点击 WYSIWYG 元素下方空白 → 光标应钳到内容末尾 len={len}，实际 caret={caret_after}"
        );
        assert!(focused, "点击 WYSIWYG 元素下方空白 → 编辑器应已聚焦");
        let _ = std::fs::remove_file(&md);
    }

