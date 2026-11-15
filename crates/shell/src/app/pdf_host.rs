//! PDF 域**宿主薄适配**：域逻辑（书签持久化 / 背景重着色 / 页操作动作 /
//! 错误映射）在独立 crate `shell_pdf`；本模块只保留宿主 `EditorApp` 的薄适配与右键
//! 菜单的**视图/挂载**：
//!
//! - **书签 / 背景 / 失败 toast**：模型读数/写数转发 `shell_pdf` 域函数，宿主落地
//!   `notify` / `persist_prefs` / 通知容器（toast）。
//! - **右键菜单**：`ui::menu` 弹出结构、`pdf_menu` 宿主槽位与浮层渲染依赖 EditorApp
//!   菜单槽位/弹窗结构，留宿主（挂载适配）；菜单项 on_click 的动作逻辑经薄适配转发
//!   `shell_pdf::menu`（插入/删除/提取/删除注释/书签）。
//! - **弹窗互关**：右键新弹前清 `tab_bar_menu` / `media_lock_menu` / `pdf_menu`
//!   槽位收敛到 [`EditorApp::close_pdf_popups_predecessors`]，供
//!   `shell_pdf::PdfHost::pdf_host_close_all_popups`（宿主实现，`host_impl.rs`）与
//!   [`EditorApp::on_pdf_right_click`] 共用。
//! - **取色**：win32 `CHOOSECOLORW` FFI 在 `shell_pdf::bg::pick_color`；本模块只留
//!   当前窗口 HWND 提取 + `set_pdf_bg` 落地的宿主 window 部分。

use super::*;

impl EditorApp {
    // -----------------------------------------------------------------
    // 书签：模型读数/写数经 shell_pdf::bookmarks，此处薄适配 + notify
    // -----------------------------------------------------------------

    /// 某路径当前内存态的书签列表（无 → `None`）。侧边栏 TOC「书签」区段与测试读。
    /// 域逻辑 [`shell_pdf::bookmarks::bookmarks_for`]（owned 克隆，语义与迁移前一致）。
    /// `#[doc(hidden)] pub`：`tests/pdf.rs` 书签持久化断言（§8.6 外提清单）。
    #[doc(hidden)]
    pub fn pdf_bookmarks_for(&self, path: &std::path::Path) -> Option<Vec<PdfBookmark>> {
        shell_pdf::bookmarks::bookmarks_for(&self.model.borrow(), path)
    }

    /// 添加/覆盖一条书签（同页去重）：更新内存 + 原子落盘 + notify。
    /// 域逻辑 [`shell_pdf::bookmarks::add_bookmark`]（模型 + save），notify 宿主落地。
    /// `#[doc(hidden)] pub`：`tests/pdf.rs` 书签持久化断言（§8.6 外提清单）。
    #[doc(hidden)]
    pub fn add_pdf_bookmark(
        &mut self,
        path: &std::path::Path,
        page_index: usize,
        title: String,
        cx: &mut Context<Self>,
    ) {
        shell_pdf::bookmarks::add_bookmark(&mut self.model.borrow_mut(), path, page_index, title);
        cx.notify();
    }

    /// 移除某路径书签列表中第 `idx` 条（列表空 → 整键删除）+ 落盘 + notify；返回是否移除。
    /// 域逻辑 [`shell_pdf::bookmarks::remove_bookmark`]，notify 宿主落地。
    pub(crate) fn remove_pdf_bookmark(
        &mut self,
        path: &std::path::Path,
        idx: usize,
        cx: &mut Context<Self>,
    ) -> bool {
        let removed = shell_pdf::bookmarks::remove_bookmark(&mut self.model.borrow_mut(), path, idx);
        if removed {
            cx.notify();
        }
        removed
    }

    /// PDF 右键「添加书签」动作：当前页 + 标题（优先当前大纲标题；无则「第 N 页」）。
    /// 域逻辑 [`shell_pdf::menu::add_bookmark_action`]（模型 + save + 文案），toast 宿主落地。
    pub(crate) fn pdf_add_bookmark_action(
        &mut self,
        pdf: &gpui::Entity<pdf::PdfViewer>,
        cx: &mut Context<Self>,
    ) {
        let toast = {
            let mut m = self.model.borrow_mut();
            shell_pdf::menu::add_bookmark_action(&mut m, pdf, cx)
        };
        if let Some((title, msg)) = toast {
            self.pdf_toast(&title, msg);
        }
    }

    // -----------------------------------------------------------------
    // 背景：解析 / 重着色 / 应用经 shell_pdf::bg，此处薄适配 + persist/notify
    // -----------------------------------------------------------------

    /// 把当前 `pdf_bg` 偏好应用到单个 PDF 阅读器。
    /// 域逻辑 [`shell_pdf::bg::apply_background`]。
    pub(super) fn apply_pdf_background(
        &self,
        pdf: &gpui::Entity<pdf::PdfViewer>,
        cx: &mut Context<Self>,
    ) {
        shell_pdf::bg::apply_background(&self.model.borrow(), pdf, cx);
    }

    /// 把当前 `pdf_bg` 偏好应用到本窗口全部 PDF 阅读器（打开 / 换主题 / 改偏好共用）。
    /// 域逻辑 [`shell_pdf::bg::apply_backgrounds`]。
    pub(super) fn apply_pdf_backgrounds(&self, cx: &mut Context<Self>) {
        shell_pdf::bg::apply_backgrounds(&self.model.borrow(), cx);
    }

    /// 设 PDF 画布背景偏好并立即应用 + 持久化（右键「背景色」预设/自定义点击共用）。
    /// `custom` 仅在 `Some` 时更新（重选「自定义」保留上次颜色；预设不覆盖存储值）。
    /// 域逻辑 [`shell_pdf::bg::set_pdf_bg_model`]（模型）+
    /// [`shell_pdf::bg::apply_backgrounds`]（应用），persist/notify 宿主落地。
    /// `#[doc(hidden)] pub`：`tests/pdf.rs` 背景预设/主题切换断言（§8.6 外提清单）。
    #[doc(hidden)]
    pub fn set_pdf_bg(
        &mut self,
        pref: PdfBgPref,
        custom: Option<String>,
        cx: &mut Context<Self>,
    ) {
        shell_pdf::bg::set_pdf_bg_model(&mut self.model.borrow_mut(), pref, custom);
        shell_pdf::bg::apply_backgrounds(&self.model.borrow(), cx);
        self.persist_prefs();
        cx.notify();
    }

    /// 「自定义」取色入口：Windows 原生取色对话框（CHOOSECOLORW，FFI 在
    /// [`shell_pdf::bg::pick_color`]）→ 选中确定 → 应用 + 持久化；取消 → 不变。
    /// 测试构建 / 非 windows 不弹框（no-op）。
    pub(super) fn pick_custom_pdf_bg(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        #[cfg(all(windows, not(test)))]
        {
            use raw_window_handle::HasWindowHandle as _;
            // 所有者句柄：当前窗口 HWND（init.rs 同款提取姿势）——让取色对话框模态挂在本
            // 窗口上；取不到（理论上不该发生）→ 零所有者仍可弹（顶层独立对话框）。
            let mut owner: isize = 0;
            if let Ok(handle) = window.window_handle()
                && let Ok(raw_window_handle::RawWindowHandle::Win32(win32)) =
                    raw_window_handle::RawWindowHandle::try_from(handle)
            {
                owner = win32.hwnd.get();
            }
            let initial = shell_pdf::bg::canvas_bg(&self.model.borrow());
            // 自定义色对话框移后台线程 + async 回传（镜像 file_tree_ops window.prompt
            // 姿势）；修复主线程 borrow 帧内阻塞模态重入 panic（用户真机复报）：
            // 事件帧持有 gpui App RefCell，`pick_color` 的模态消息泵同线程重入窗程 →
            // 异步/输入续体二次 `app.borrow_mut()` panic。后台线程弹框不再持主线程
            // App borrow，取色结果经 oneshot + window.spawn 回传主线程再落地。
            let (tx, rx) = futures::channel::oneshot::channel();
            let entity = cx.entity();
            let _ = std::thread::Builder::new()
                .name("pdf-color-dialog".into())
                .spawn(move || {
                    let r = shell_pdf::bg::pick_color(initial, owner);
                    let _ = tx.send(r);
                });
            window
                .spawn(cx, async move |cx| {
                    if let Ok(Some(hex)) = rx.await {
                        let _ = cx.update_window_entity(&entity, move |this, _window, cx| {
                            this.set_pdf_bg(PdfBgPref::Custom, Some(hex), cx);
                        });
                    }
                })
                .detach();
        }
        #[cfg(any(not(windows), test))]
        {
            let _ = (window, cx);
        }
    }

    // -----------------------------------------------------------------
    // 菜单：toast 宿主通知容器 + 右键菜单视图/挂载 + 动作薄适配
    // -----------------------------------------------------------------

    /// 一条 PDF 操作结果提示（自托管 key：下次先移除上一条，不堆积）。
    /// 通知标识类型 [`shell_pdf::PdfToast`]（域契约），容器/去重在宿主通知容器上落地。
    pub(crate) fn pdf_toast(&mut self, title: &str, msg: String) {
        let key = gpui::ElementId::from("pdf-toast");
        self.remove_notification1::<shell_pdf::PdfToast>(key.clone());
        self.push_notification(
            ui::notification::Notification::new()
                .id1::<shell_pdf::PdfToast>(key)
                .title(title)
                .message(msg)
                .autohide(true),
        );
    }

    /// 右键新弹前互关宿主全部自绘弹出菜单槽位（选项卡栏 / 媒体锁定 / 上份 PDF）。
    /// `pub(super)`：`host_impl.rs` 的 `PdfHost::pdf_host_close_all_popups` 与
    /// [`EditorApp::on_pdf_right_click`] 共用同一清理语义。
    pub(super) fn close_pdf_popups_predecessors(&mut self) {
        self.tab_bar_menu.clear();
        self.media_lock_menu.clear();
        self.pdf_menu.clear();
    }

    /// PDF 右键 → 弹「添加书签 / 插入页面 / 删除页面 / 提取页面」上下文菜单。
    /// 当前 shell 根元素 `on_media_right_click` 按 `is_lockable()` 过滤掉了 PDF ——
    /// 本方法在 media.rs 分发处为 PDF 分支调用；PDF 无「锁定」语义，四个项全部
    /// 是 Phase B 交互。菜单**视图/挂载**留宿主，动作逻辑在 `shell_pdf::menu`。
    #[allow(clippy::too_many_lines)]
    pub(super) fn on_pdf_right_click(
        &mut self,
        ev: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let pdf = match self.model.borrow().tabs.get(self.model.borrow().active_tab).map(|t| t.payload.reader.clone()) {
            Some(MediaReader::Pdf(pv)) => pv,
            _ => return,
        };
        // 先收掉选项卡栏 / 媒体锁定 / 上一份 PDF 菜单（若有），再在光标处新建
        // （统一经宿主 popup 互关服务，同 PdfHost 契约）。
        self.close_pdf_popups_predecessors();

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
        // 父菜单右侧自动展开（真悬浮 flyout，父项保持可见；无「返回」项，次级一旦展开即持久，
        // 移入次级面板/间隙或途中驻留不收起，直到移到其它选项 / Esc / 点外才变化）。
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
                if this.pdf_menu.entity.as_ref() == Some(&emitter) {
                    this.pdf_menu.clear();
                }
                cx.notify();
            },
        );
        self.pdf_menu.pos = Some(ev.position);
        // 写入父菜单窗口锚点：flyout 据此做窗口边缘空间适配（翻左/翻上、自夹持、逐帧恒定）。
        menu.update(cx, |m, _| m.set_parent_pos(ev.position));
        self.pdf_menu.entity = Some(menu);
        self.pdf_menu.sub = Some(sub);
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
            .model.borrow().tabs
            .get(self.model.borrow().active_tab)
            .map(|t| matches!(t.payload.reader, MediaReader::Pdf(_)))
            .unwrap_or(false);
        if !still_pdf {
            self.pdf_menu.clear();
        }
        let Some(pos) = self.pdf_menu.pos else {
            return div().into_any_element();
        };
        let Some(menu) = self.pdf_menu.entity.clone() else {
            return div().into_any_element();
        };
        ui::menu::popup_overlay(pos, menu, ui::menu::MENU_EDGE_MARGIN).into_any_element()
    }

    /// 「删除注释」动作：删除右键命中的那条**待存**注释。域逻辑
    /// [`shell_pdf::menu::delete_annot_action`]（纯 PdfViewer 实体操作）。
    pub(crate) fn pdf_delete_annot_action(
        &mut self,
        pdf: &gpui::Entity<pdf::PdfViewer>,
        id: u64,
        cx: &mut Context<Self>,
    ) {
        shell_pdf::menu::delete_annot_action(pdf, id, cx);
    }

    /// 「插入页面」动作：确认框（插入页为待存）→ `insert_blank_page_after`（内存级）
    /// + 失败 toast。域逻辑 [`shell_pdf::menu::insert_page_action`]（toast 经宿主
    /// `PdfHost` 再入）。
    pub(crate) fn pdf_insert_page_action(
        &mut self,
        window: &mut Window,
        pdf: &gpui::Entity<pdf::PdfViewer>,
        cx: &mut Context<Self>,
    ) {
        shell_pdf::menu::insert_page_action(window, pdf, cx);
    }

    /// 「删除页面」动作：原生确认框（破坏性）→ `delete_page` + 失败 toast。
    /// 域逻辑 [`shell_pdf::menu::delete_page_action`]。
    pub(crate) fn pdf_delete_page_action(
        &mut self,
        window: &mut Window,
        pdf: &gpui::Entity<pdf::PdfViewer>,
        cx: &mut Context<Self>,
    ) {
        shell_pdf::menu::delete_page_action(window, pdf, cx);
    }

    /// 「提取页面」动作：另存为对话框选目标路径 → `extract_page_to`（源文档不变）
    /// + 成功/失败 toast。域逻辑 [`shell_pdf::menu::extract_page_action`]。
    pub(crate) fn pdf_extract_page_action(
        &mut self,
        window: &mut Window,
        pdf: &gpui::Entity<pdf::PdfViewer>,
        cx: &mut Context<Self>,
    ) {
        shell_pdf::menu::extract_page_action(window, pdf, cx);
    }

    /// PDF 页操作失败 toast（供 `_pdf_sub` observe 变化时调用；`last_modify_error`
    /// 由 None → Some 才算「变化」，同一错误不重复提示）。域逻辑
    /// [`shell_pdf::menu::notify_last_error`]（模型去重 + 文案），toast 宿主落地。
    pub(crate) fn pdf_notify_last_error(&mut self, pdf: &gpui::Entity<pdf::PdfViewer>, cx: &mut Context<Self>) {
        let toast = {
            let mut m = self.model.borrow_mut();
            shell_pdf::menu::notify_last_error(&mut m, pdf, cx)
        };
        if let Some(msg) = toast {
            self.pdf_toast("PDF 操作失败", msg);
        }
    }

    /// 「复制图片」动作：经 pdf view 按**自然分辨率**取回 BGRA 像素（异步 worker）→
    /// `tray::set_clipboard_image_bgra` 写系统剪贴板 CF_DIB。成功 toast 尺寸；取图失败
    /// （文档已关闭 / 对象失效 / worker 超时）或剪贴板被占用 → toast 失败文案。
    /// 取数是**按需一次性**、不缓存大图像素（只过一手剪贴板）。
    /// （走 `tray` 剪贴板宿主服务，随 tray 留宿主。）
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
        let checked = app.read(cx).model.borrow().pdf_bg == pref;
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
        let checked = app.read(cx).model.borrow().pdf_bg == config::user::PdfBgPref::Custom;
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
/// 已随 flyout 改造休眠：真悬浮路径下附属于次级菜单、无需返回项（保留源码作备用）。
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
