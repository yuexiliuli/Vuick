    /// 回归：MarkdownPreview「最右侧文字被卡片/窗口右缘遮挡、右侧无间距」。
    ///
    /// 根因（真实渲染路径取证）：窗口模式（块数 > [`WINDOWED_MIN_BLOCKS`]）打开大文档后，
    /// gpui 的 [`ListState`] 缓存可见条目的量测宽/位置；面板/窗口**变窄**时这些条目仍按
    /// 旧（更宽）宽度排版——引述/callout 行右缘超出阅读栏与卡片右缘、被 `overflow` 裁掉，
    /// 左有间距、右没有。修复 = 宽度变化时 `list_state.remeasure()` 强制按新宽重测，并给
    /// 内容列单入口补左右对称留白 [`PREVIEW_SIDE_GAP_PX`]。
    #[gpui::test]
    fn preview_right_gap_survives_pane_shrink(cx: &mut TestAppContext) {
        cx.update(crate::actions::bind_local_keymaps);
        let (host, cx) = cx.add_window_view(|_window, cx| {
            let md = cx.new(|cx| MarkdownPreview::new(cx));
            PreviewHost { md }
        });
        let cx: &mut VisualTestContext = cx;
        // 生成 > WINDOWED_MIN_BLOCKS 块的大文档（进入窗口模式）：顶部窗口含引述/callout
        // + 正文，复现「宽开窄收后引述行右缘按旧宽残留在窄窗、被右缘裁掉」。
        let mut src = String::from("# 标题\n\n");
        for i in 0..4600 {
            src.push_str(&format!(
                "> [!NOTE] 提示 {i}：点击箭头折叠，内容足够长以复现最右文字被遮挡的问题。\n> 提示主体 {i}：窗口变窄后最右侧文字不应被卡片边缘裁掉。\n\n普通段落 {i} 内容：这一行也应保持左右对称留白。\n\n"
            ));
        }
        cx.update(|_, cx| {
            let md = host.read(cx).md.clone();
            md.update(cx, |md, cx| {
                md.set_source(&src, cx);
                md.set_wrap(true, cx);
            });
        });
        assert!(
            cx.update(|_, cx| host.read(cx).md.read(cx).index.is_some()),
            "生成文档应进入窗口模式（块数须 > WINDOWED_MIN_BLOCKS）"
        );
        // 宽窗打开（居中阅读栏）→ 缩窄到「列 = 可用宽」退化档：修复前此处右缘越界被裁。
        cx.simulate_resize(size(px(1400.), px(800.)));
        settle(cx);
        cx.simulate_resize(size(px(500.), px(700.)));
        settle(cx);
        // 清空命中表再收尾一帧：只统计「这一轮真正渲染」的行（TextHit 只覆盖同行重画）。
        cx.update(|_, cx| {
            let md = host.read(cx).md.clone();
            let m = md.read(cx);
            m.text_ctx.hits.borrow_mut().clear();
        });
        settle(cx);
        let pane_right = cx
            .debug_bounds("md-preview")
            .expect("md-preview 应渲染")
            .right()
            .as_f32();
        let hits: Vec<(usize, f32, f32, String)> = cx.update(|_, cx| {
            let md = host.read(cx).md.read(cx);
            md.text_ctx
                .hits
                .borrow()
                .iter()
                .map(|h| {
                    let w = h.layout.bounds().size.width.as_f32();
                    let left = h.origin.x.as_f32();
                    (
                        h.row_ix,
                        left,
                        left + w,
                        h.text.chars().take(16).collect(),
                    )
                })
                .collect()
        });
        assert!(
            hits.len() >= 3,
            "缩窄后应仍有可见文本行，实际命中 {} 行",
            hits.len()
        );
        let side_gap = crate::config::markdown::PREVIEW_SIDE_GAP_PX;
        // ① 最右字右缘 ≤ 面板右缘 − 对称间距（不再贴边/越界被裁）。
        for (ix, _left, right, snip) in &hits {
            assert!(
                *right <= pane_right - side_gap + 1.0,
                "行 {ix}（{snip}…）右缘越界：right={right:.1} > pane_right({pane_right:.1})−间隔({side_gap:.0})"
            );
        }
        // ② 正文（Paragraph）行左右留白对称，且右留白 ≥ 内容列留白 + 行内边距（结构化保证）。
        let paras: Vec<(usize, f32, f32)> = hits
            .iter()
            .filter(|(_, _, _, s)| s.contains("段落"))
            .map(|(ix, l, r, _)| (*ix, *l, *r))
            .collect();
        let (ix, left, right) = *paras.first().expect("应有正文行命中");
        let left_gap = left;
        let right_gap = pane_right - right;
        assert!(
            (left_gap - right_gap).abs() <= 1.0,
            "正文行 {ix} 左右留白应对称：left_gap={left_gap:.1} right_gap={right_gap:.1}"
        );
        let inner = crate::config::markdown::CONTENT_EDGE_PAD_X_PX;
        assert!(
            right_gap >= side_gap + inner - 1.5,
            "正文行 {ix} 右留白应 ≥ 内容列留白 + 行内内衬（{}），实际 {right_gap:.1}",
            side_gap + inner
        );

        // —— 真实大文档（存在才跑）：锁定「面板/窗口变窄后最右字不被裁」的根因 ——
        // 生成的窗口模式文档无法稳定复现 gpui `ListState` 的旧宽缓存，6.5MB 真实文档
        // 可（引述/callout 宽行按旧宽残留在窄窗）。修复前此处会断言失败。
        let big = std::path::Path::new(r"D:\WorkSpace\1\rust\gpui\gpui-editor\target\大文档测试2.md");
        if big.exists() {
            let src = std::fs::read_to_string(big).unwrap_or_default();
            // 同窗重建：先把旧源换成空白，再从文件喂大文档（窗口模式）。
            let md = cx.update(|_, cx| host.read(cx).md.clone());
            md.update(cx, |md, cx| md.set_source("", cx));
            md.update(cx, |md, cx| {
                md.set_source(&src, cx);
                md.set_wrap(true, cx);
            });
            assert!(
                cx.update(|_, cx| host.read(cx).md.read(cx).index.is_some()),
                "真实大文档应进入窗口模式"
            );
            cx.simulate_resize(size(px(1400.), px(800.)));
            settle(cx);
            for (wi, wh) in [(900., 700.), (500., 700.)] {
                cx.simulate_resize(size(px(wi), px(wh)));
                settle(cx);
                cx.update(|_, cx| {
                    let m = host.read(cx).md.read(cx);
                    m.text_ctx.hits.borrow_mut().clear();
                });
                settle(cx);
                let pane_right = cx
                    .debug_bounds("md-preview")
                    .expect("md-preview 应渲染")
                    .right()
                    .as_f32();
                let over: Vec<(usize, f32, String)> = cx.update(|_, cx| {
                    let m = host.read(cx).md.read(cx);
                    m.text_ctx
                        .hits
                        .borrow()
                        .iter()
                        .map(|h| {
                            let w = h.layout.bounds().size.width.as_f32();
                            let right = h.origin.x.as_f32() + w;
                            (
                                h.row_ix,
                                right,
                                h.text.chars().take(20).collect(),
                            )
                        })
                        .filter(|(_, r, _)| *r > pane_right - side_gap + 1.0)
                        .collect()
                });
                assert!(
                    over.is_empty(),
                    "真实大文档缩窄到 {wi}px 后最右字右缘不得越界（被卡片右缘裁），越界={over:?}"
                );
            }
        }
    }

    /// 回归：MarkdownPreview「标题行」最右字符不得被边缘截断——与正文行共享同一
    /// 左右对称留白语义（`PREVIEW_SIDE_GAP_PX`）。页面恒换行（列宽 = min(72ch, 视口)）
    /// 下，标题/正文按列宽折行，任何窗口宽度最右字右缘都留在 `md-list-wrap` 的
    /// `.px(PREVIEW_SIDE_GAP_PX)` 内衬内——窄窗退化（列宽 = 可用宽）同样不贴边。
    ///
    /// 历史根因（已随「非换行自然宽 + 页面级横向滚动」模式移除）：`remeasure_dimensions`
    /// 把 `2 × PREVIEW_SIDE_GAP_PX` 计入非换行「最宽自然宽模型」，否则大字号标题
    /// （估算误差随字号放大）会顶到内容盒右缘、被行级 `overflow_hidden` 贴边裁掉。
    #[gpui::test]
    fn heading_right_edge_stays_inside_side_gap(cx: &mut TestAppContext) {
        cx.update(crate::actions::bind_local_keymaps);
        let (host, cx) = cx.add_window_view(|_window, cx| {
            let md = cx.new(|cx| MarkdownPreview::new(cx));
            PreviewHost { md }
        });
        let cx: &mut VisualTestContext = cx;
        let side_gap = crate::config::markdown::PREVIEW_SIDE_GAP_PX;
        // 无空格长串 + 带空格长串：考验折行与自然宽（大字号标题最宽行场景）。
        let long = "这是一个非常非常长的标题用来复现最右字符是否被边缘截断ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
        let long_sp = "这是一个非常非常长的标题，用来复现最右侧字符边距的问题。A B C D E F G H I J K L M N O P Q R S T U V W X Y Z 0 1 2 3 4 5 6 7 8 9";
        let src = format!(
            "# {long}\n\n# {long_sp}\n\n## {long}\n\n#### 短标题四\n\n正文段落一：{long}\n\n普通正文：{long_sp}\n"
        );
        cx.update(|_, cx| {
            let md = host.read(cx).md.clone();
            md.update(cx, |md, cx| {
                md.set_source(&src, cx);
                md.set_wrap(true, cx);
            });
        });
        // 行下标 → 是否标题（小文档 rows 全量物化）。
        let heading_ids: std::collections::HashMap<usize, bool> = cx.update(|_, cx| {
            let m = host.read(cx).md.read(cx);
            m.rows
                .iter()
                .enumerate()
                .map(|(ix, r)| (ix, matches!(r.docrow.kind, IrKind::Heading(_))))
                .collect()
        });
        // —— ① wrap 阅读栏（app / EPUB 路径）：宽开→窄收（列=可用宽退化档），
        // 每个标题行的最右字符右缘必须留在 `md-list-wrap` 右 padding 内。 ——
        for (wi, wh) in [(1400., 800.), (500., 700.)] {
            cx.simulate_resize(size(px(wi), px(wh)));
            settle(cx);
            cx.update(|_, cx| {
                let m = host.read(cx).md.read(cx);
                m.text_ctx.hits.borrow_mut().clear();
            });
            settle(cx);
            let pane_right = cx
                .debug_bounds("md-preview")
                .expect("md-preview 应渲染")
                .right()
                .as_f32();
            // 逐行取「最右字符右缘」= 末字符 caret 的绝对 x（折行取末行末字）。
            let rows: Vec<(usize, f32, f32, String)> = cx.update(|_, cx| {
                let m = host.read(cx).md.read(cx);
                m.text_ctx
                    .hits
                    .borrow()
                    .iter()
                    .map(|h| {
                        let caret = h
                            .layout
                            .position_for_index(h.text.len())
                            .map(|p| p.x.as_f32())
                            .unwrap_or(0.0);
                        (h.row_ix, h.origin.x.as_f32(), caret, h.text.chars().take(8).collect())
                    })
                    .collect()
            });
            let mut head_seen = 0usize;
            for (ix, _left, right, snip) in &rows {
                if heading_ids.get(ix).copied().unwrap_or(false) {
                    head_seen += 1;
                    assert!(
                        *right <= pane_right - side_gap + 1.5,
                        "wrap {wi}px 标题行 {ix}（{snip}…）右缘越界：right={right:.1} > pane_right({pane_right:.1}) − 间距({side_gap:.0})"
                    );
                }
            }
            assert!(
                head_seen >= 3,
                "wrap {wi}px 应命中至少 3 个标题行，实际 {head_seen}"
            );
        }

        // —— ② 对称性：标题与正文**左对齐基线一致**（同一起始 x），且正文右缘同样
        //    满足右留白（标题不因字号而偏离正文的列内排版）。測量在 ① 末尾的窄窗态。 ——
        let pane_right = cx
            .debug_bounds("md-preview")
            .expect("md-preview 应渲染")
            .right()
            .as_f32();
        let (h_left, p_left) = cx.update(|_, cx| {
            let m = host.read(cx).md.read(cx);
            let mut hl = None;
            let mut pl = None;
            for h in m.text_ctx.hits.borrow().iter() {
                let is_heading = heading_ids.get(&h.row_ix).copied().unwrap_or(false);
                let caret = h
                    .layout
                    .position_for_index(h.text.len())
                    .map(|p| p.x.as_f32())
                    .unwrap_or(0.0);
                if is_heading {
                    hl = Some((h.origin.x.as_f32(), caret));
                } else if h.text.starts_with("正文段落一") {
                    pl = Some((h.origin.x.as_f32(), caret));
                }
            }
            (hl, pl)
        });
        let (h_left, _h_right) = h_left.expect("应有标题行命中");
        let (p_left, p_right) = p_left.expect("应有正文行命中");
        assert!(
            (h_left - p_left).abs() <= 1.0,
            "标题与正文左对齐基线应一致：标题 left={h_left:.1} vs 正文 left={p_left:.1}"
        );
        assert!(
            p_right <= pane_right - side_gap + 1.5,
            "正文行右缘不应越界（p_right={p_right:.1} > pane_right({pane_right:.1}) − 间距({side_gap:.0})）"
        );

        // —— ③ 恒换行窄窗退化档（800px < 72ch 阅读栏 → 列 = 可用宽）：标题/正文最右
        //    字右缘仍留在 `PREVIEW_SIDE_GAP_PX` 内衬内、且无页面级横向滚动。 ——
        cx.simulate_resize(size(px(800.), px(700.)));
        settle(cx);
        cx.update(|_, cx| {
            let m = host.read(cx).md.read(cx);
            m.text_ctx.hits.borrow_mut().clear();
        });
        settle(cx);
        let pane_right3 = cx
            .debug_bounds("md-preview")
            .expect("md-preview 应渲染")
            .right()
            .as_f32();
        let over: Vec<(usize, f32, String)> = cx.update(|_, cx| {
            let m = host.read(cx).md.read(cx);
            m.text_ctx
                .hits
                .borrow()
                .iter()
                .map(|h| {
                    let caret = h
                        .layout
                        .position_for_index(h.text.len())
                        .map(|p| p.x.as_f32())
                        .unwrap_or(0.0);
                    (
                        h.row_ix,
                        h.origin.x.as_f32() + caret,
                        h.text.chars().take(12).collect(),
                    )
                })
                .filter(|(_, r, _)| *r > pane_right3 - side_gap + 1.5)
                .collect()
        });
        let (max_x, _page_w) = cx.update(|_, cx| {
            let m = host.read(cx).md.read(cx);
            (m.h_handle.max_offset().x, m.h_handle.bounds().size.width.as_f32())
        });
        assert!(
            over.is_empty(),
            "恒换行窄窗（800px）下标题/正文最右字不得越界，越界={over:?}"
        );
        assert_eq!(
            max_x, px(0.),
            "页面级横向滚动应始终为 0（恒换行），实际={max_x:?}"
        );

        // —— ④ 真实大文档（存在才跑）：窗口模式下锁定「标题行最右字不被卡片/窗口
        //    右缘裁、且右留白 ≥ PREVIEW_SIDE_GAP_PX」。 ——
        let big = std::path::Path::new(r"D:\WorkSpace\1\rust\gpui\gpui-editor\target\大文档测试2.md");
        if big.exists() {
            let src = std::fs::read_to_string(big).unwrap_or_default();
            cx.update(|_, cx| {
                let md = host.read(cx).md.clone();
                md.update(cx, |md, cx| md.set_source("", cx));
                md.update(cx, |md, cx| {
                    md.set_source(&src, cx);
                    md.set_wrap(true, cx);
                });
            });
            assert!(
                cx.update(|_, cx| host.read(cx).md.read(cx).index.is_some()),
                "真实大文档应进入窗口模式"
            );
            for (wi, wh) in [(1400., 800.), (500., 700.)] {
                cx.simulate_resize(size(px(wi), px(wh)));
                settle(cx);
                cx.update(|_, cx| {
                    let m = host.read(cx).md.read(cx);
                    m.text_ctx.hits.borrow_mut().clear();
                });
                settle(cx);
                let pane_right = cx
                    .debug_bounds("md-preview")
                    .expect("md-preview 应渲染")
                    .right()
                    .as_f32();
                let over: Vec<(usize, f32, String)> = cx.update(|_, cx| {
                    let m = host.read(cx).md.read(cx);
                    let mut bad = Vec::new();
                    for h in m.text_ctx.hits.borrow().iter() {
                        // 窗口模式行下标 = 绝对块；win_rows 携带 kind → 只查标题行。
                        let is_head = h.row_ix >= m.win_lo
                            && m.win_rows
                                .get(h.row_ix - m.win_lo)
                                .map(|r| matches!(r.docrow.kind, IrKind::Heading(_)))
                                .unwrap_or(false);
                        if !is_head {
                            continue;
                        }
                        let caret = h
                            .layout
                            .position_for_index(h.text.len())
                            .map(|p| p.x.as_f32())
                            .unwrap_or(0.0);
                        if caret > pane_right - side_gap + 1.5 {
                            bad.push((h.row_ix, caret, h.text.chars().take(20).collect()));
                        }
                    }
                    bad
                });
                assert!(
                    over.is_empty(),
                    "真实大文档缩窄到 {wi}px 后标题行最右字不得越界（被卡片右缘裁），越界={over:?}"
                );
            }
        }
    }

    /// 回归（用户拍板方案「右侧明显留白」）：把 `PREVIEW_SIDE_GAP_PX` 放大后，在真实大
    /// 文档（窗口模式 wrap）上**逐行实测**标题行与正文行的最右字右缘——断言右留白同时
    /// ≥ `PREVIEW_SIDE_GAP_PX` 且 ≥ 明显地板 24px（> 自绘滚动条带 ~10px + 两个字符
    /// 量级），标题不再贴右缘被裁；打印实测值供肉眼核对。
    #[gpui::test]
    fn preview_visible_right_gap_on_big_doc(cx: &mut TestAppContext) {
        cx.update(crate::actions::bind_local_keymaps);
        let (host, cx) = cx.add_window_view(|_window, cx| {
            let md = cx.new(|cx| MarkdownPreview::new(cx));
            PreviewHost { md }
        });
        let cx: &mut VisualTestContext = cx;
        // 真实大文档优先；缺省时生成窗口模式大文档兜底（等价几何）。
        let big = std::path::Path::new(
            r"D:\WorkSpace\1\rust\gpui\gpui-editor\target\大文档测试2.md",
        );
        let src = if big.exists() {
            std::fs::read_to_string(big).unwrap_or_default()
        } else {
            let mut s = String::from("# 标题\n\n");
            for i in 0..4600 {
                s.push_str(&format!(
                    "> [!NOTE] 提示 {i}：内容足够长以复现最右文字被遮挡。\n> 提示主体 {i}：窗口变窄后最右侧文字不应被卡片边缘裁掉。\n\n普通段落 {i} 内容：这一行也应保持右侧明显留白。\n\n"
                ));
            }
            s
        };
        cx.update(|_, cx| {
            let md = host.read(cx).md.clone();
            md.update(cx, |md, cx| {
                md.set_source("", cx);
                md.set_source(&src, cx);
                md.set_wrap(true, cx);
            });
        });
        assert!(
            cx.update(|_, cx| host.read(cx).md.read(cx).index.is_some()),
            "应进入窗口模式（块数须 > WINDOWED_MIN_BLOCKS）"
        );
        cx.simulate_resize(size(px(900.), px(700.)));
        settle(cx);
        cx.update(|_, cx| {
            let m = host.read(cx).md.read(cx);
            m.text_ctx.hits.borrow_mut().clear();
        });
        settle(cx);
        let pane_right = cx
            .debug_bounds("md-preview")
            .expect("md-preview 应渲染")
            .right()
            .as_f32();
        // 行下标 → 行类：窗口模式经 `win_rows`（全局块下标映射），其余走全量 `rows`。
        let (head_min, para_min, head_n, para_n) = cx.update(|_, cx| {
            let m = host.read(cx).md.read(cx);
            let kind_of = |ix: usize| -> Option<IrKind> {
                if ix >= m.win_lo {
                    m.win_rows.get(ix - m.win_lo).map(|r| r.docrow.kind)
                } else {
                    m.rows.get(ix).map(|r| r.docrow.kind)
                }
            };
            let mut head_min = f32::INFINITY;
            let mut para_min = f32::INFINITY;
            let mut head_n = 0usize;
            let mut para_n = 0usize;
            for h in m.text_ctx.hits.borrow().iter() {
                if h.text.trim().is_empty() {
                    continue;
                }
                let right = h.origin.x.as_f32() + h.layout.bounds().size.width.as_f32();
                let ws = pane_right - right;
                match kind_of(h.row_ix) {
                    Some(IrKind::Heading(_)) => {
                        head_min = head_min.min(ws);
                        head_n += 1;
                    }
                    Some(IrKind::Paragraph) => {
                        para_min = para_min.min(ws);
                        para_n += 1;
                    }
                    _ => {}
                }
            }
            (head_min, para_min, head_n, para_n)
        });
        let side_gap = crate::config::markdown::PREVIEW_SIDE_GAP_PX;
        eprintln!(
            "[gap] 窗口模式大文档右留白：标题 {head_min:.1}px（{head_n} 行）、正文 {para_min:.1}px（{para_n} 行）、side_gap={side_gap}px"
        );
        for (name, min_ws, n) in [("标题", head_min, head_n), ("正文", para_min, para_n)] {
            assert!(n > 0, "{name}行应有命中用于测量右留白");
            assert!(
                min_ws >= side_gap - 1.0,
                "{name}行最右字右缘应 ≤ 面板右缘 − PREVIEW_SIDE_GAP_PX({side_gap})，实测最小右留白 {min_ws:.1}px"
            );
            assert!(
                min_ws >= 24.0,
                "{name}行右侧留白应肉眼明显（≥24px），实测最小右留白 {min_ws:.1}px"
            );
        }
    }

    /// 单一来源回归：预览内容列的**左右两侧**留白必须由同一对常量同时驱动——
    /// `PREVIEW_SIDE_GAP_PX`（宿主外侧 gap）+ `CONTENT_EDGE_PAD_X_PX`（内容盒内衬）。
    /// 断言左右对称 + 两侧都精确等于「常量之和」的同一表达式：改任何一侧/任一常量，
    /// 左右会**同步同量**地平移（不再各管各的散落数值）。窗口 ≤ 阅读列宽 → 无居中移位，
    /// 左缘/右缘留白直接等于 `side + inner`。
    #[gpui::test]
    fn preview_side_gaps_symmetric_and_single_source(cx: &mut TestAppContext) {
        cx.update(crate::actions::bind_local_keymaps);
        let (host, cx) = cx.add_window_view(|_window, cx| {
            let md = cx.new(|cx| MarkdownPreview::new(cx));
            PreviewHost { md }
        });
        let cx: &mut VisualTestContext = cx;
        let md_src = "# 标题行不可贴边\n\n普通段落内容：abcdefghijklmnopqrstuvwxyz 0123456789\n\n```\nlet x = 1;\n```\n";
        cx.update(|_, cx| {
            let md = host.read(cx).md.clone();
            md.update(cx, |md, cx| {
                md.set_source(md_src, cx);
                md.set_wrap(true, cx);
            });
        });
        // 760px < 阅读列 72ch@14px(806.4) → 列 = 可用宽、无居中移位，左右留白即常量之和。
        cx.simulate_resize(size(px(760.), px(600.)));
        settle(cx);
        let pane = cx
            .debug_bounds("md-preview")
            .expect("md-preview 应渲染");
        let pane_left = pane.left().as_f32();
        let pane_right = pane.right().as_f32();
        let hits: Vec<(usize, f32, f32, String)> = cx.update(|_, cx| {
            let md = host.read(cx).md.read(cx);
            md.text_ctx
                .hits
                .borrow()
                .iter()
                .map(|h| {
                    let w = h.layout.bounds().size.width.as_f32();
                    let left = h.origin.x.as_f32();
                    (h.row_ix, left, left + w, h.text.chars().take(12).collect())
                })
                .collect()
        });
        assert!(
            hits.len() >= 2,
            "应有可见文本行用于测量，实际 {} 行",
            hits.len()
        );
        let side = crate::config::markdown::PREVIEW_SIDE_GAP_PX;
        let inner = crate::config::markdown::CONTENT_EDGE_PAD_X_PX;
        let expect_gap = side + inner;
        // 对每一行算「左留白」与「右留白」，取最差（最小）值——两者必须都 ≥ 常量之和。
        let (worst_left, worst_right_gap) = hits.iter().fold(
            (f32::INFINITY, f32::INFINITY),
            |(wl, wr), (.., left, right, _)| {
                (
                    wl.min(left - pane_left),
                    wr.min(pane_right - right),
                )
            },
        );
        eprintln!(
            "[gap] 左右对称单源：expect=side+inner={expect_gap:.1}px，实测左={worst_left:.1}px 右={worst_right_gap:.1}px"
        );
        assert!(
            (worst_left - expect_gap).abs() <= 1.0,
            "左留白应 = PREVIEW_SIDE_GAP_PX + CONTENT_EDGE_PAD_X_PX（{expect_gap:.1}px），实测 {worst_left:.1}px"
        );
        assert!(
            (worst_right_gap - expect_gap).abs() <= 1.0,
            "右留白应 = 同一常量之和（{expect_gap:.1}px，左右对称、单一来源），实测 {worst_right_gap:.1}px"
        );
        assert!(
            (worst_left - worst_right_gap).abs() <= 1.0,
            "左右留白应对称：左 {worst_left:.1}px vs 右 {worst_right_gap:.1}px"
        );
    }



/// 页面恒换行自适应窗口宽度：窗口 1400→700→500 缩窄时，同一正文段折行数增多
/// （随窗口即时重排）、每个宽度下最右行右缘 ≤ 可用宽 − `PREVIEW_SIDE_GAP_PX`、
/// 页面级横向滚动恒为 0——覆盖「任意窗口尺寸内容完整、不被窗口边缘裁切」。
#[gpui::test]
fn narrow_window_reflows_and_keeps_right_gap(cx: &mut TestAppContext) {
    cx.update(crate::actions::bind_local_keymaps);
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let md = cx.new(|cx| MarkdownPreview::new(cx));
        PreviewHost { md }
    });
    let cx: &mut VisualTestContext = cx;
    let side_gap = crate::config::markdown::PREVIEW_SIDE_GAP_PX;
    // 一段足够长、足以在窄窗折成多行的正文（超长无空格串靠 CJK 断行）。
    let long = "正文内容用于验证窗口自适应重排".repeat(12);
    let src = format!("# 标题{long}\n\n普通段落{long}。\n");
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| {
            md.set_source(&src, cx);
            md.set_wrap(true, cx);
        });
    });
    // 正文段行下标（小文档 rows 全量物化）。
    let par_ix = cx.update(|_, cx| {
        let m = host.read(cx).md.read(cx);
        m.rows
            .iter()
            .position(|r| r.docrow.text.starts_with("普通段落"))
            .expect("应有普通段落行")
    });
    let mut heights: Vec<(f32, f32)> = Vec::new();
    for (wi, wh) in [(1400., 800.), (700., 800.), (500., 800.)] {
        cx.simulate_resize(size(px(wi), px(wh)));
        settle(cx);
        cx.update(|_, cx| {
            let m = host.read(cx).md.read(cx);
            m.text_ctx.hits.borrow_mut().clear();
        });
        settle(cx);
        let pane_right = cx
            .debug_bounds("md-preview")
            .expect("md-preview 应渲染")
            .right()
            .as_f32();
        let (max_x, item_h) = cx.update(|_, cx| {
            let m = host.read(cx).md.read(cx);
            let item_h = m
                .list_state
                .bounds_for_item(par_ix)
                .map(|b| b.size.height.as_f32())
                .unwrap_or(0.0);
            (m.h_handle.max_offset().x, item_h)
        });
        assert_eq!(
            max_x, px(0.),
            "{wi}px 页面级横向滚动应恒为 0（恒换行），实际={max_x:?}"
        );
        assert!(item_h > 0.0, "{wi}px 正文段应有实测高度");
        heights.push((wi, item_h));
        // 每行最右右缘 ≤ 可用宽 − gap（`origin + 布局宽` 与既有右留白断言同口径）。
        let over: Vec<(usize, f32, String)> = cx.update(|_, cx| {
            let m = host.read(cx).md.read(cx);
            m.text_ctx
                .hits
                .borrow()
                .iter()
                .map(|h| {
                    let right = h.origin.x.as_f32() + h.layout.bounds().size.width.as_f32();
                    (h.row_ix, right, h.text.chars().take(10).collect())
                })
                .filter(|(_, r, _)| *r > pane_right - side_gap + 1.5)
                .collect()
        });
        assert!(
            over.is_empty(),
            "{wi}px 缩窄后正文/标题最右字右缘不得越界（被面板右缘裁），越界={over:?}"
        );
        eprintln!("[reflow] {wi}px → 普通段落 item 高 {item_h:.1}px");
    }
    let (w_wide, h_wide) = heights[0];
    let (_, h_narrow) = heights[heights.len() - 1];
    assert!(
        h_narrow > h_wide,
        "窗口缩窄应即时重排（折行更多、段更高）：{w_wide}px={h_wide:.1}px → 最窄={h_narrow:.1}px"
    );
}

/// EPUB 阅读（恒换行）随窗口窄化：列宽 = min(72ch, 视口) 且不横向超界——窄窗下列 =
/// 可用宽、正文折行、最右字右缘留在右留白内、页面级横向滚动恒为 0。
#[gpui::test]
fn epub_narrow_window_wraps(cx: &mut TestAppContext) {
    cx.update(crate::actions::bind_local_keymaps);
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let md = cx.new(|cx| MarkdownPreview::new(cx));
        PreviewHost { md }
    });
    let cx: &mut VisualTestContext = cx;
    let side_gap = crate::config::markdown::PREVIEW_SIDE_GAP_PX;
    let src = format!("# 第一章\n\n正文{}，结尾。\n\n第二段。\n", "内容".repeat(240));
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| {
            md.set_epub(true, Default::default(), cx);
            md.set_wrap(true, cx);
            md.set_source(&src, cx);
        });
    });
    for (wi, wh) in [(1400., 800.), (600., 800.)] {
        cx.simulate_resize(size(px(wi), px(wh)));
        settle(cx);
        cx.update(|_, cx| {
            let m = host.read(cx).md.read(cx);
            m.text_ctx.hits.borrow_mut().clear();
        });
        settle(cx);
        let wrap = cx
            .debug_bounds("md-list-wrap")
            .expect("md-list-wrap 应渲染");
        let pane = cx
            .debug_bounds("md-preview")
            .expect("md-preview 应渲染");
        let pane_right = pane.right().as_f32();
        let body_fs = cx.update(|_, cx| {
            StyleParams::from_theme_with_font(cx, None).para_size.as_f32()
        });
        let col = crate::config::markdown::reading_col_px(body_fs);
        let expect_w = col.min(pane.size.width.as_f32());
        assert!(
            (wrap.size.width.as_f32() - expect_w).abs() < 3.0,
            "EPUB 列宽应 = min(72ch={col:.0}, 视口={:.0})，实际 {:.0}",
            pane.size.width.as_f32(),
            wrap.size.width.as_f32()
        );
        let (max_x, over) = cx.update(|_, cx| {
            let m = host.read(cx).md.read(cx);
            let over = m
                .text_ctx
                .hits
                .borrow()
                .iter()
                .map(|h| {
                    let right = h.origin.x.as_f32() + h.layout.bounds().size.width.as_f32();
                    (h.row_ix, right)
                })
                .filter(|(_, r)| *r > pane_right - side_gap + 1.5)
                .collect::<Vec<_>>();
            (m.h_handle.max_offset().x, over)
        });
        assert_eq!(max_x, px(0.), "EPUB {wi}px 页面级横向滚动应恒为 0，实际={max_x:?}");
        assert!(over.is_empty(), "EPUB {wi}px 最右字右缘不得越界，越界={over:?}");
    }
}
