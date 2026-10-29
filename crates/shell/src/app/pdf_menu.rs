//! PDF 右键上下文菜单（Phase B2）：添加书签 / 插入页面 / 删除页面 / 提取页面，
//! 浮层渲染与菜单生命周期装配收敛自 `ui::menu::spawn_context_menu`（同 media.rs /
//! tabs.rs / files_ops.rs 三处先例）。页面操作直接修改打开的 PDF 文件（Phase A
//! `PdfViewer::{insert_blank_page_after, delete_page, extract_page_to}` 写回→重载）。

use super::*;

/// PDF 右键 toast 的自托管 key 标记类型（同 goto_line / find 的 GotoLineToast 姿势）。
pub(crate) struct PdfToast;

/// 把 PDF 写回失败文案整理成 toast 消息：命中「目标被其他程序占用/只读」类错误时，
/// 在 pdf crate 的指引（已含路径 1：关闭占用程序后重试）上补第二条路——「另存为新副本」
/// （副本为文案指引；一次性另存 UI 未接线，headless 无法端到端验证，R3 低成本默认）。
pub(crate) fn pdf_modify_error_msg(err: &str) -> String {
    if pdf::is_writeback_busy_error(err) {
        format!("{err}；若暂时无法关闭占用程序，可「另存为新副本」以保留本次修改")
    } else {
        err.to_string()
    }
}

/// 「背景色」二级子菜单的**预设项**：当前生效档前导 ✓（media.rs 勾选行同款）；
/// 点击 → 应用 + 持久化，菜单不关闭（`keep_open`，用户可连续试色后「返回」/ Esc / 点外关闭）。
fn pdf_bg_preset_item(
    label: &'static str,
    pref: config::user::PdfBgPref,
    debug_id: &'static str,
    app: gpui::Entity<EditorApp>,
) -> PopupMenuItem {
    let app2 = app.clone();
    PopupMenuItem::element(move |_, cx| {
        let checked = app.read(cx).pdf_bg == pref;
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(div().w(px(18.)).child(if checked { "✓" } else { "" }))
            .child(label)
            .debug_selector(move || debug_id.into())
            .into_any_element()
    })
    .keep_open()
    .on_click(move |_, _window, cx| {
        let _ = app2.update(cx, |this, cx| this.set_pdf_bg(pref, None, cx));
    })
}

/// 「背景色」二级子菜单的**自定义项**：Windows 原生取色对话框（确定 → 应用 + 持久化；
/// 取消 → 不变）。当前档前导 ✓；`keep_open`（试色过程菜单保持展开）。
fn pdf_bg_custom_item(app: gpui::Entity<EditorApp>) -> PopupMenuItem {
    let app2 = app.clone();
    PopupMenuItem::element(move |_, cx| {
        let checked = app.read(cx).pdf_bg == config::user::PdfBgPref::Custom;
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(div().w(px(18.)).child(if checked { "✓" } else { "" }))
            .child("自定义")
            .debug_selector(|| "pdf-menu-bg-custom".into())
            .into_any_element()
    })
    .keep_open()
    .on_click(move |_click, window, cx| {
        let _ = app2.update(cx, |this, cx| this.pick_custom_pdf_bg(window, cx));
    })
}

/// 「背景色」右侧的 **返回项**：点击换回上一级菜单（`PopupMenuItem::back`，不关闭）。
/// 已随 flyout 改造休眠：真悬浮路径 hover 离开即收起、无需返回项（保留源码作备用）。
#[allow(dead_code)]
fn pdf_bg_back_item() -> PopupMenuItem {
    PopupMenuItem::element(|_, _| {
        div()
            .debug_selector(|| "pdf-menu-bg-back".into())
            .child("返回")
            .into_any_element()
    })
    .back()
}

impl EditorApp {
    /// 一条 PDF 操作结果提示（自托管 key：下次先移除上一条，不堆积）。
    pub(crate) fn pdf_toast(&mut self, title: &str, msg: String) {
        let key = gpui::ElementId::from("pdf-toast");
        self.remove_notification1::<PdfToast>(key.clone());
        self.push_notification(
            ui::notification::Notification::new()
                .id1::<PdfToast>(key)
                .title(title)
                .message(msg)
                .autohide(true),
        );
    }

    /// PDF 右键 → 弹「添加书签 / 插入页面 / 删除页面 / 提取页面」上下文菜单。
    /// 当前 shell 根元素 `on_media_right_click` 按 `is_lockable()` 过滤掉了 PDF ——
    /// 本方法在 media.rs 分发处为 PDF 分支调用；PDF 无「锁定」语义，四个项全部
    /// 是 Phase B 交互。
    #[allow(clippy::too_many_lines)]
    pub(super) fn on_pdf_right_click(
        &mut self,
        ev: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let pdf = match self.tabs.get(self.active_tab).map(|t| t.reader.clone()) {
            Some(MediaReader::Pdf(pv)) => pv,
            _ => return,
        };
        // 先收掉选项卡栏 / 媒体锁定 / 上一份 PDF 菜单（若有），再在光标处新建。
        self.tab_bar_menu_entity = None;
        self.tab_bar_menu_pos = None;
        self.tab_bar_menu_sub = None;
        self.media_lock_menu_entity = None;
        self.media_lock_menu_pos = None;
        self.media_lock_menu_sub = None;
        self.pdf_menu_entity = None;
        self.pdf_menu_pos = None;
        self.pdf_menu_sub = None;

        let app = cx.entity();
        let page_count = pdf.read(cx).page_count();
        // 待保存模型（P1）：右键命中某条待存注释 → 置灰框焦并记 id，菜单据此追加
        // 「删除注释」；未命中 → 保持原四项菜单不变（点非注释文字无灰框/无删除项）。
        let focused_id = pdf.update(cx, |v, cx| {
            v.focus_annot_at_window_pos(window, ev.position, cx)
        });
        // 图片命中（「复制图片」）：右键命中页内图片 → 未选中则先选中该图（青绿描框，
        // 与左键单击同一选中态）；菜单据此追加「复制图片」。未命中 → 无该项、不主动清除
        // 已选图片（右键只看命中，不改变图片选中态之外的状态）。
        let image_hit = pdf.read(cx).image_at_window_pos(window, ev.position);
        if let Some((page, obj)) = image_hit {
            pdf.update(cx, |v, cx| v.select_image(page, obj, cx));
        }
        // 背景色二级子列表（默认/夜间/护眼/羊皮纸/自定义）：悬停「背景色」→ 次级菜单在
        // 父菜单右侧自动展开（真悬浮 flyout，父项保持可见；无「返回」项，hover 离开即收起）。
        let fly_items: Vec<PopupMenuItem> = vec![
            pdf_bg_preset_item("默认", PdfBgPref::Default, "pdf-menu-bg-default", app.clone()),
            pdf_bg_preset_item("夜间", PdfBgPref::Night, "pdf-menu-bg-night", app.clone()),
            pdf_bg_preset_item("护眼", PdfBgPref::Eye, "pdf-menu-bg-eye", app.clone()),
            pdf_bg_preset_item("羊皮纸", PdfBgPref::Parchment, "pdf-menu-bg-parchment", app.clone()),
            pdf_bg_custom_item(app.clone()),
        ];
        let (menu, sub) = ui::menu::spawn_context_menu(
            window,
            cx,
            move |mut menu, _window, _cx| {
                // 落在待存注释灰框上：优先给「删除注释」（删除该条待存注释，不落盘）。
                if let Some(id) = focused_id {
                    let app_del = app.clone();
                    let pdf_del = pdf.clone();
                    menu = menu
                        .item(
                            PopupMenuItem::element(move |_w, _cx| {
                                div()
                                    .debug_selector(|| "pdf-menu-del-annot".into())
                                    .child("删除注释")
                                    .into_any_element()
                            })
                            .on_click(move |_, _w, cx| {
                                app_del.update(cx, |this, cx| {
                                    this.pdf_delete_annot_action(&pdf_del, id, cx);
                                });
                            }),
                        )
                        .separator();
                }
                // 落在页内图片上：追加「复制图片」（按自然分辨率写剪贴板 CF_DIB）。
                if let Some((page, obj)) = image_hit {
                    let app_img = app.clone();
                    let pdf_img = pdf.clone();
                    menu = menu
                        .item(
                            PopupMenuItem::element(move |_w, _cx| {
                                div()
                                    .debug_selector(|| "pdf-menu-copy-image".into())
                                    .child("复制图片")
                                    .into_any_element()
                            })
                            .on_click(move |_, window, cx| {
                                app_img.update(cx, |this, cx| {
                                    this.pdf_copy_image_action(window, &pdf_img, page, obj, cx);
                                });
                            }),
                        )
                        .separator();
                }
                // 添加书签：当前页存一条阅读位置书签（页码 + 标题，优先当前大纲标题）。
                let app_bm = app.clone();
                let pdf_bm = pdf.clone();
                let menu = menu
                    .item(
                        PopupMenuItem::element(move |_w, _cx| {
                            div()
                                .debug_selector(|| "pdf-menu-bookmark".into())
                                .child("添加书签")
                                .into_any_element()
                        })
                        .on_click(move |_, _w, cx| {
                            app_bm.update(cx, |this, cx| {
                                this.pdf_add_bookmark_action(&pdf_bm, cx);
                            });
                        }),
                    )
                    .separator();
                // 插入页面：当前页后插同尺寸空白页并写回（覆盖原文件前确认）。
                let app_ins = app.clone();
                let pdf_ins = pdf.clone();
                let dis_ins = page_count == 0;
                let menu = menu.item(
                    PopupMenuItem::element(move |_w, _cx| {
                        div()
                            .debug_selector(|| "pdf-menu-insert".into())
                            .child("插入页面")
                            .into_any_element()
                    })
                    .disabled(dis_ins)
                    .on_click(move |_, window, cx| {
                        app_ins.update(cx, |this, cx| {
                            this.pdf_insert_page_action(window, &pdf_ins, cx);
                        });
                    }),
                );
                // 删除页面：删除当前页并写回（破坏性，确认后执行；唯一页该项禁用）。
                let app_del = app.clone();
                let pdf_del = pdf.clone();
                let dis_del = page_count <= 1;
                let menu = menu.item(
                    PopupMenuItem::element(move |_w, _cx| {
                        div()
                            .debug_selector(|| "pdf-menu-delete".into())
                            .child("删除页面")
                            .into_any_element()
                    })
                    .disabled(dis_del)
                    .on_click(move |_, window, cx| {
                        app_del.update(cx, |this, cx| {
                            this.pdf_delete_page_action(window, &pdf_del, cx);
                        });
                    }),
                );
                // 提取页面：当前页存为独立 PDF（另存为对话框选目标路径）。
                let app_ext = app.clone();
                let pdf_ext = pdf.clone();
                let dis_ext = page_count == 0;
                menu.item(
                    PopupMenuItem::element(move |_w, _cx| {
                        div()
                            .debug_selector(|| "pdf-menu-extract".into())
                            .child("提取页面")
                            .into_any_element()
                    })
                    .disabled(dis_ext)
                    .on_click(move |_, window, cx| {
                        app_ext.update(cx, |this, cx| {
                            this.pdf_extract_page_action(window, &pdf_ext, cx);
                        });
                    }),
                )
                .separator()
                // 背景色：真悬浮二级菜单（悬停展开于父菜单右侧，父项保持可见）。
                .item(
                    PopupMenuItem::element(|_w, _cx| {
                        div()
                            .debug_selector(|| "pdf-menu-bg-color".into())
                            .child("背景色")
                            .into_any_element()
                    })
                    .flyout(fly_items),
                )
            },
            |this, emitter, cx| {
                // 关闭（选择/Esc/点击外）→ 按 emitter 身份校验清理本菜单槽位。
                if this.pdf_menu_entity.as_ref() == Some(&emitter) {
                    this.pdf_menu_entity = None;
                    this.pdf_menu_pos = None;
                }
                cx.notify();
            },
        );
        self.pdf_menu_pos = Some(ev.position);
        // 写入父菜单窗口锚点：flyout 据此做窗口边缘空间适配（翻左/翻上、自夹持、逐帧恒定）。
        menu.update(cx, |m, _| m.set_parent_pos(ev.position));
        self.pdf_menu_entity = Some(menu);
        self.pdf_menu_sub = Some(sub);
        cx.notify();
    }

    /// PDF 右键菜单 Overlay：按右键时记录的**窗口坐标**定位（同
    /// [`super::media::EditorApp::render_media_lock_menu`]：`AnchoredPositionMode::Window`
    /// + `position(pos)`）。PDF 选项卡切走/关闭 → 一并收起。
    pub(super) fn render_pdf_menu(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let still_pdf = self
            .tabs
            .get(self.active_tab)
            .map(|t| matches!(t.reader, MediaReader::Pdf(_)))
            .unwrap_or(false);
        if !still_pdf {
            self.pdf_menu_entity = None;
            self.pdf_menu_pos = None;
        }
        let Some(pos) = self.pdf_menu_pos else {
            return div().into_any_element();
        };
        let Some(menu) = self.pdf_menu_entity.clone() else {
            return div().into_any_element();
        };
        deferred(
            anchored()
                .position_mode(gpui::AnchoredPositionMode::Window)
                .anchor(Anchor::TopLeft)
                .position(pos)
                // 边缘留白与 PopupMenu 自夹持使用同一常量 → 父面板顶/左估算逐像素一致。
                .snap_to_window_with_margin(Edges::all(ui::menu::MENU_EDGE_MARGIN))
                .child(menu),
        )
        .with_priority(100)
        .into_any_element()
    }

    /// 「删除注释」动作：删除右键命中的那条**待存**注释（只用未保存模型：从 pending
    /// 移除 + 撤销栈记录可 Ctrl+Z 恢复；不落盘、不清 dirty 语义——save 后才真正消失）。
    pub(crate) fn pdf_delete_annot_action(
        &mut self,
        pdf: &gpui::Entity<pdf::PdfViewer>,
        id: u64,
        cx: &mut Context<Self>,
    ) {
        pdf.update(cx, |v, cx| v.delete_pending_annot(id, cx));
    }

    /// 「插入页面」动作：确认框（插入页为待存、消除「覆盖原文件」措辞）→
    /// `insert_blank_page_after`（内存级）。成功由页数变化/重载呈现；失败经
    /// `last_modify_error`（即时或 `_pdf_sub` 变化时）toast。
    pub(crate) fn pdf_insert_page_action(
        &mut self,
        window: &mut Window,
        pdf: &gpui::Entity<pdf::PdfViewer>,
        cx: &mut Context<Self>,
    ) {
        let Some((page1, total)) = pdf.read(cx).current_page() else {
            return;
        };
        if total == 0 {
            return;
        }
        let page_index = page1.saturating_sub(1);
        let entity = cx.entity();
        let pdf2 = pdf.clone();
        let answer = window.prompt(
            PromptLevel::Warning,
            "插入页面",
            Some(&format!("将在第 {page1} 页后插入一页空白页（尚未保存，保存时方写回原文件）。继续？")),
            &["插入", "取消"],
            cx,
        );
        window
            .spawn(cx, async move |cx| {
                if !matches!(answer.await, Ok(0)) {
                    return;
                }
                let _ = cx.update_window_entity(&entity, |this, _window, cx| {
                    let p = pdf2.clone();
                    p.update(cx, |v, cx| v.insert_blank_page_after(page_index, cx));
                    if let Some(err) = pdf2.read(cx).last_modify_error() {
                        this.pdf_toast("插入页面失败", pdf_modify_error_msg(err));
                    }
                });
            })
            .detach();
    }

    /// 「删除页面」动作：原生确认框（破坏性）→ `delete_page`。唯一页在菜单即禁用，
    /// 此处再兜底（Phase A `delete_page` 对唯一页同样拒绝）。失败经
    /// `last_modify_error` toast。
    pub(crate) fn pdf_delete_page_action(
        &mut self,
        window: &mut Window,
        pdf: &gpui::Entity<pdf::PdfViewer>,
        cx: &mut Context<Self>,
    ) {
        let Some((maybe_page1, total)) = pdf.read(cx).current_page() else {
            return;
        };
        if total == 0 {
            return;
        }
        let page1 = maybe_page1.min(total);
        let page_index = page1.saturating_sub(1);
        let entity = cx.entity();
        let pdf2 = pdf.clone();
        let answer = window.prompt(
            PromptLevel::Warning,
            "删除页面",
            Some(&format!("确定删除第 {page1} 页？（尚未保存，保存时方写回原文件）")),
            &["删除", "取消"],
            cx,
        );
        window
            .spawn(cx, async move |cx| {
                if !matches!(answer.await, Ok(0)) {
                    return;
                }
                let _ = cx.update_window_entity(&entity, |this, _window, cx| {
                    let p = pdf2.clone();
                    p.update(cx, |v, cx| v.delete_page(page_index, cx));
                    if let Some(err) = pdf2.read(cx).last_modify_error() {
                        this.pdf_toast("删除页面失败", pdf_modify_error_msg(err));
                    }
                });
            })
            .detach();
    }

    /// 「提取页面」动作：另存为对话框选目标路径 → `extract_page_to`（源文档不变）。
    /// 成功 toast 目标路径；失败经 `last_modify_error` toast。测试构建（`#[cfg(test)]`）
    /// 不弹系统对话框——端到端文件产出由测试直接驱动 `extract_page_to` 覆盖。
    pub(crate) fn pdf_extract_page_action(
        &mut self,
        window: &mut Window,
        pdf: &gpui::Entity<pdf::PdfViewer>,
        cx: &mut Context<Self>,
    ) {
        let Some((page1, _)) = pdf.read(cx).current_page() else {
            return;
        };
        let page_index = page1.saturating_sub(1);
        let base_name = pdf
            .read(cx)
            .path()
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "extract".to_string());
        let entity = cx.entity();
        let pdf2 = pdf.clone();
        #[cfg(not(test))]
        {
            window
                .spawn(cx, async move |cx| {
                    let Some(file) = rfd::AsyncFileDialog::new()
                        .set_title("提取页面为 PDF")
                        .set_file_name(format!("{base_name}_第{page1}页.pdf"))
                        .save_file()
                        .await
                    else {
                        return;
                    };
                    let out = file.path().to_path_buf();
                    let _ = cx.update_window_entity(&entity, |this, _window, cx| {
                        let p = pdf2.clone();
                        p.update(cx, |v, cx| v.extract_page_to(page_index, out.clone(), cx));
                        if let Some(err) = pdf2.read(cx).last_modify_error() {
                            this.pdf_toast("提取页面失败", pdf_modify_error_msg(err));
                        } else {
                            this.pdf_toast("已提取页面", format!("第 {page1} 页 → {}", out.display()));
                        }
                    });
                })
                .detach();
            let _ = base_name;
        }
        #[cfg(test)]
        {
            let _ = (window, cx, pdf);
            let _ = (base_name, entity, pdf2, page_index, page1);
        }
    }

    /// 「复制图片」动作：经 pdf view 按**自然分辨率**取回 BGRA 像素（异步 worker）→
    /// `tray::set_clipboard_image_bgra` 写系统剪贴板 CF_DIB。成功 toast 尺寸；取图失败
    /// （文档已关闭 / 对象失效 / worker 超时）或剪贴板被占用 → toast 失败文案。
    /// 取数是**按需一次性**、不缓存大图像素（只过一手剪贴板）。
    pub(crate) fn pdf_copy_image_action(
        &mut self,
        window: &mut Window,
        pdf: &gpui::Entity<pdf::PdfViewer>,
        page: usize,
        obj: usize,
        cx: &mut Context<Self>,
    ) {
        // 同步拿结果通道（无选中/对象失效即 None → 静默返回，菜单项只在命中时呈现）。
        let Some(rx) = pdf.read(cx).request_image_pixels(page, obj) else {
            return;
        };
        let entity = cx.entity();
        window
            .spawn(cx, async move |cx| {
                // 有界轮询 worker 结果（每次 50ms，不整段阻塞异步执行器；超时 10s → 失败）。
                let pix = cx
                    .background_executor()
                    .spawn(async move {
                        let deadline =
                            std::time::Instant::now() + std::time::Duration::from_secs(10);
                        loop {
                            match rx.recv_timeout(std::time::Duration::from_millis(50)) {
                                Ok(res) => break res,
                                Err(_) if std::time::Instant::now() < deadline => continue,
                                Err(_) => break None,
                            }
                        }
                    })
                    .await;
                // 剪贴板 Win32 调用回到 UI 线程（tray 契约：单 UI 线程 OpenClipboard/Write）。
                let _ = cx.update_window_entity(&entity, |this, _window, _cx| {
                    match pix {
                        Some(pix) => {
                            match tray::set_clipboard_image_bgra(&pix.bgra, pix.width, pix.height) {
                                Ok(()) => {
                                    this.pdf_toast(
                                        "已复制图片",
                                        format!("{} × {} 像素", pix.width, pix.height),
                                    );
                                }
                                Err(e) => {
                                    this.pdf_toast("复制图片失败", e.to_string());
                                }
                            }
                        }
                        None => {
                            this.pdf_toast("复制图片失败", "取图失败或文档已关闭".into());
                        }
                    }
                });
            })
            .detach();
    }

    /// PDF 页操作失败 toast（供 `_pdf_sub` observe 变化时调用；`last_modify_error`
    /// 由 None → Some 才算「变化」，同一错误不重复提示）。
    pub(crate) fn pdf_notify_last_error(&mut self, pdf: &gpui::Entity<pdf::PdfViewer>, cx: &mut Context<Self>) {
        let err = pdf.read(cx).last_modify_error().map(|s| s.to_string());
        match err {
            Some(e) if self.pdf_last_error_seen.as_deref() != Some(e.as_str()) => {
                self.pdf_last_error_seen = Some(e.clone());
                self.pdf_toast("PDF 操作失败", pdf_modify_error_msg(&e));
            }
            Some(_) => {}
            None => self.pdf_last_error_seen = None,
        }
    }
}
