//! 侧边栏「文件树操作」子模块：右键菜单（新建文件/文件夹、重命名、删除、资源管理器
//! 定位、复制路径、剪切/复制）与文件系统变更（新建 / 重命名 / 删除及其派生状态失效）。

use super::*;

impl EditorApp {
    /// 文件树项右键 → 弹上下文菜单（位置存窗口坐标）：目录 = 新建文件 / 新建文件夹 /
    /// 重命名 / 删除；文件 = 重命名 / 删除。新建类仅目录适用。关闭经 `DismissEvent`
    /// 订阅清理（同选项卡栏右键，见 [`Self::on_tab_bar_right_click`]）。
    pub(crate) fn on_file_tree_right_click(
        &mut self,
        path: &std::path::Path,
        is_dir: bool,
        ev: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 事件继续冒泡到树根会触发图片/视频右键（on_media_right_click）等其它右键菜单，
        // 此处拦截保证右键文件树只弹本菜单。
        cx.stop_propagation();
        // 生命周期：菜单构建闭包是 'static，borrow 的 `path` 参数需先物化为 owned 再移动。
        let path = path.to_path_buf();
        self.file_tree_menu_entity = None;
        self.file_tree_menu_pos = None;
        self.file_tree_menu_sub = None;
        let weak = cx.weak_entity();
        let (menu, sub) = ui::menu::spawn_context_menu(
            window,
            cx,
            move |menu, _window, _cx| {
                let mut m = menu;
                if is_dir {
                    m = m.item({
                        let weak = weak.clone();
                        let parent = path.to_path_buf();
                        PopupMenuItem::element(move |_, _| {
                            div()
                                .w_full()
                                .flex()
                                .items_center()
                                .child("新建文件…")
                                .debug_selector(|| "file-tree-menu-new-file".into())
                        })
                        .on_click(move |_, window, cx| {
                            let _ = weak.update(cx, |this, cx| {
                                this.open_file_tree_prompt(
                                    FileTreePromptKind::NewFile { parent: parent.clone() },
                                    window,
                                    cx,
                                );
                            });
                        })
                    });
                    m = m.item({
                        let weak = weak.clone();
                        let parent = path.to_path_buf();
                        PopupMenuItem::element(move |_, _| {
                            div()
                                .w_full()
                                .flex()
                                .items_center()
                                .child("新建文件夹…")
                                .debug_selector(|| "file-tree-menu-new-folder".into())
                        })
                        .on_click(move |_, window, cx| {
                            let _ = weak.update(cx, |this, cx| {
                                this.open_file_tree_prompt(
                                    FileTreePromptKind::NewFolder { parent: parent.clone() },
                                    window,
                                    cx,
                                );
                            });
                        })
                    });
                }
                m = m.item({
                    let weak = weak.clone();
                    let target = path.to_path_buf();
                    PopupMenuItem::element(move |_, _| {
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .child("重命名…")
                            .debug_selector(|| "file-tree-menu-rename".into())
                    })
                    .on_click(move |_, window, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            this.open_file_tree_prompt(
                                FileTreePromptKind::Rename {
                                    path: target.clone(),
                                    is_dir: is_dir,
                                },
                                window,
                                cx,
                            );
                        });
                    })
                });
                m = m.item({
                    let weak = weak.clone();
                    let target = path.clone();
                    PopupMenuItem::element(move |_, _| {
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .child("删除…")
                            .debug_selector(|| "file-tree-menu-delete".into())
                    })
                    .on_click(move |_, window, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            this.file_tree_delete_request(&target, is_dir, window, cx);
                        });
                    })
                });
                // ── 第二组：在资源管理器中显示（Explorer /select,<绝对路径>）──
                m = m.separator();
                m = m.item({
                    let weak = weak.clone();
                    let target = path.clone();
                    PopupMenuItem::element(move |_, _| {
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .child("在资源管理器中显示")
                            .debug_selector(|| "file-tree-menu-reveal".into())
                    })
                    .on_click(move |_, _window, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            this.file_tree_reveal_in_explorer(&target, cx);
                        });
                    })
                });
                // ── 第三组：复制路径 / 复制相对路径（无引号文本）──
                m = m.separator();
                m = m.item({
                    let weak = weak.clone();
                    let target = path.clone();
                    PopupMenuItem::element(move |_, _| {
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .child("复制路径")
                            .debug_selector(|| "file-tree-menu-copy-path".into())
                    })
                    .on_click(move |_, _window, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            this.file_tree_copy_path(&target, cx);
                        });
                    })
                });
                m = m.item({
                    let weak = weak.clone();
                    let target = path.clone();
                    PopupMenuItem::element(move |_, _| {
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .child("复制相对路径")
                            .debug_selector(|| "file-tree-menu-copy-rel-path".into())
                    })
                    .on_click(move |_, _window, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            this.file_tree_copy_relative_path(&target, cx);
                        });
                    })
                });
                // ── 第四组：剪切 / 复制（文件操作，CF_HDROP + PreferredDropEffect）──
                m.separator()
                    .item({
                        let weak = weak.clone();
                        let target = path.clone();
                        PopupMenuItem::element(move |_, _| {
                            div()
                                .w_full()
                                .flex()
                                .items_center()
                                .child("剪切")
                                .debug_selector(|| "file-tree-menu-cut".into())
                        })
                        .on_click(move |_, _window, cx| {
                            let _ = weak.update(cx, |this, cx| {
                                this.file_tree_cut(&target, cx);
                            });
                        })
                    })
                    .item({
                        let weak = weak.clone();
                        let target = path.clone();
                        PopupMenuItem::element(move |_, _| {
                            div()
                                .w_full()
                                .flex()
                                .items_center()
                                .child("复制")
                                .debug_selector(|| "file-tree-menu-copy-file".into())
                        })
                        .on_click(move |_, _window, cx| {
                            let _ = weak.update(cx, |this, cx| {
                                this.file_tree_copy_file(&target, cx);
                            });
                        })
                    })
            },
            |this, emitter, cx| {
                if this.file_tree_menu_entity.as_ref() == Some(&emitter) {
                    this.file_tree_menu_entity = None;
                    this.file_tree_menu_pos = None;
                }
                cx.notify();
            },
        );
        self.file_tree_menu_pos = Some(ev.position);
        self.file_tree_menu_entity = Some(menu);
        self.file_tree_menu_sub = Some(sub);
        cx.notify();
    }

    /// 文件树右键菜单 Overlay（光标处弹出，同选项卡栏右键菜单定位姿势）。
    pub(crate) fn render_file_tree_menu(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let Some(pos) = self.file_tree_menu_pos else {
            return div().into_any_element();
        };
        let Some(menu) = self.file_tree_menu_entity.clone() else {
            return div().into_any_element();
        };
        div()
            .id("file-tree-menu")
            .debug_selector(|| "file-tree-menu".into())
            .child(
                deferred(
                    anchored()
                        .position_mode(gpui::AnchoredPositionMode::Window)
                        .anchor(Anchor::TopLeft)
                        .position(pos)
                        .snap_to_window_with_margin(Edges::all(px(8.)))
                        .child(menu),
                )
                .with_priority(100),
            )
            .into_any_element()
    }

    /// 打开文件树输入弹层（新建/重命名共用）：建实体 + 订阅（Dismiss → 清引用）+
    /// 聚焦输入框。
    pub(crate) fn open_file_tree_prompt(
        &mut self,
        kind: FileTreePromptKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let host = cx.weak_entity();
        let previous_focus = window.focused(cx);
        let prompt = cx.new(|cx| {
            file_tree_prompt::FileTreePrompt::new(host, kind, previous_focus, window, cx)
        });
        let sub = cx.subscribe(&prompt, |this, _e, _: &DismissEvent, cx| {
            this.file_tree_prompt = None;
            this._file_tree_prompt_sub = None;
            cx.notify();
        });
        let handle = prompt.read(cx).input_focus(cx);
        window.focus(&handle, cx);
        self.file_tree_prompt = Some(prompt);
        self._file_tree_prompt_sub = Some(sub);
        cx.notify();
    }

    /// 文件树输入弹层的渲染挂点（根级 overlay；无弹层 → 空元素）。
    pub(crate) fn render_file_tree_prompt(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        match self.file_tree_prompt.clone() {
            Some(p) => p.into_any_element(),
            None => div().into_any_element(),
        }
    }

    /// 弹层提交入口：按类型分发到新建文件/新建文件夹/重命名。`Err(msg)` 回给弹层在
    /// 卡片内联提示（不静默关闭）；`Ok(())` 弹层自关。
    pub(crate) fn file_tree_submit_prompt(
        &mut self,
        kind: &FileTreePromptKind,
        value: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let name = value.trim();
        if name.is_empty() {
            return Err("名称不能为空".to_string());
        }
        match kind {
            FileTreePromptKind::NewFile { parent } => {
                self.file_tree_create_file(parent, name, window, cx)
            }
            FileTreePromptKind::NewFolder { parent } => {
                self.file_tree_create_dir(parent, name, window, cx)
            }
            FileTreePromptKind::Rename { path, is_dir } => {
                self.file_tree_rename(path, *is_dir, name, window, cx)
            }
        }
    }

    /// 名称合法性校验（新建/重命名共用）：非空、非 `.`/`..`、无路径分隔/非法字符。
    fn validate_name(name: &str) -> Result<(), String> {
        if name.is_empty() {
            return Err("名称不能为空".to_string());
        }
        if name == "." || name == ".." {
            return Err("非法名称".to_string());
        }
        if name.contains(&['/', '\\', ':', '*', '?', '"', '<', '>', '|'][..]) {
            return Err("名称包含非法字符：/ \\ : * ? \" < > |".to_string());
        }
        Ok(())
    }

    /// 新建文件：建空文件 → 作废父目录缓存重扫 → **自动打开成选项卡** → 会话落盘。
    fn file_tree_create_file(
        &mut self,
        parent: &std::path::Path,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        Self::validate_name(name)?;
        let target = parent.join(name);
        if target.exists() {
            return Err("已存在同名文件或文件夹".to_string());
        }
        std::fs::File::create(&target).map_err(|e| format!("新建失败：{e}"))?;
        self.file_tree_invalidate(&target, window, cx);
        self.open_path_or_activate(&target, window, cx); // 新建文件 → 自动打开
        self.schedule_session_flush(cx, true);
        Ok(())
    }

    /// 新建文件夹：建空目录 → 作废父目录缓存重扫。
    fn file_tree_create_dir(
        &mut self,
        parent: &std::path::Path,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        Self::validate_name(name)?;
        let target = parent.join(name);
        if target.exists() {
            return Err("已存在同名文件或文件夹".to_string());
        }
        std::fs::create_dir(&target).map_err(|e| format!("新建文件夹失败：{e}"))?;
        self.file_tree_invalidate(&target, window, cx);
        Ok(())
    }

    /// 重命名：磁盘改名 → 树刷新 → **已打开选项卡路径同步**（含目录下子树）+ 会话落盘。
    /// 重命名根目录：换根（重建树）并按新路径持久化 `opened_folder`。
    fn file_tree_rename(
        &mut self,
        path: &std::path::Path,
        is_dir: bool,
        new_name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        Self::validate_name(new_name)?;
        let target = path
            .parent()
            .map(|p| p.join(new_name))
            .unwrap_or_else(|| PathBuf::from(new_name));
        if target == path {
            return Ok(()); // 名未变 → 无操作
        }
        if target.exists() {
            return Err("已存在同名文件或文件夹".to_string());
        }
        std::fs::rename(path, &target).map_err(|e| format!("重命名失败：{e}"))?;
        let was_root = self.sidebar.files.root.as_deref() == Some(path);
        if was_root {
            // 重命名根：换根重建树（展开/缓存全清，不可复用旧路径）。
            self.sidebar.files.set_root(Some(target.clone()));
            self.sidebar.files.expanded.insert(target.clone());
            self.spawn_file_tree_scan(target.clone(), window, cx);
            self.persist_prefs(); // opened_folder 跟随新根路径
        } else {
            self.file_tree_invalidate(path, window, cx);
        }
        // 已打开选项卡路径同步（文件精确 / 目录下子树整体前缀改写）。
        for tab in self.model.borrow_mut().tabs.iter_mut() {
            let Some(fp) = tab.file_path.clone() else { continue };
            if fp == path {
                tab.file_path = Some(target.clone());
            } else if is_dir
                && fp.starts_with(path)
                && let Ok(rel) = fp.strip_prefix(path)
            {
                tab.file_path = Some(target.join(rel));
            }
        }
        self.schedule_session_flush(cx, true); // 会话清单记录新路径
        Ok(())
    }

    /// 删除请求：原生确认框（复用「未保存关闭确认」同款 `window.prompt`）→ 确认后执行。
    pub(crate) fn file_tree_delete_request(
        &mut self,
        path: &std::path::Path,
        is_dir: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string_lossy().into_owned());
        let kind_label = if is_dir { "文件夹" } else { "文件" };
        let entity = cx.entity();
        let answer = window.prompt(
            PromptLevel::Warning,
            "删除",
            Some(&format!("确定要删除{kind_label}「{name}」？此操作不可恢复。")),
            &["删除", "取消"],
            cx,
        );
        let target = path.to_path_buf();
        window
            .spawn(cx, async move |cx| {
                let idx = answer.await.unwrap_or(1);
                if idx == 0 {
                    let _ = cx.update_window_entity(&entity, move |this, window, cx| {
                        this.file_tree_delete(&target, window, cx);
                    });
                }
                let _ = ();
            })
            .detach();
    }

    /// 执行删除：**任一受影响打开选项卡有未保存修改 → 不删磁盘**（不可静默丢内容，
    /// 提示后取消整个删除，请用户先保存/关闭相关选项卡）；否则干净关闭受影响选项卡
    /// → 删磁盘 → 树刷新。删根 → 清空文件夹根（等价关文件夹）。
    pub(crate) fn file_tree_delete(
        &mut self,
        path: &std::path::Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let is_dir = path.is_dir();
        // 受影响的打开选项卡：文件精确匹配 / 目录按前缀覆盖子树。
        let affected: Vec<usize> = self
            .model.borrow().tabs
            .iter()
            .enumerate()
            .filter(|(_, t)| {
                t.file_path.as_ref().is_some_and(|p| {
                    if is_dir {
                        p.starts_with(path)
                    } else {
                        p == path
                    }
                })
            })
            .map(|(i, _)| i)
            .collect();
        // 决策：任一受影响选项卡有未保存修改 → 不删磁盘（不可静默丢内容），提示后取消。
        if affected.iter().any(|&i| self.model.borrow().tabs[i].dirty) {
            self.model.borrow_mut().status = SharedString::from(
                "有未保存修改，未删除：请先保存或关闭相关选项卡，再重试删除",
            );
            self.model.borrow_mut().status_ok = false;
            cx.notify();
            return;
        }
        // 干净受影响选项卡：同步关闭（下标降序防位移）。
        for &i in affected.iter().rev() {
            self.remove_tab(i, window, cx);
        }
        // 删磁盘。
        let res = if is_dir {
            std::fs::remove_dir_all(path)
        } else {
            std::fs::remove_file(path)
        };
        match res {
            Ok(()) => {
                let was_root = self.sidebar.files.root.as_deref() == Some(path);
                if was_root {
                    self.sidebar.files.set_root(None);
                    self.persist_prefs(); // opened_folder 写回 null
                } else {
                    self.file_tree_invalidate(path, window, cx);
                }
                self.schedule_session_flush(cx, true);
                self.model.borrow_mut().status = SharedString::from(format!(
                    "已删除{}{}",
                    if is_dir { "文件夹" } else { "文件" },
                    path.display()
                ));
                self.model.borrow_mut().status_ok = true;
            }
            Err(e) => {
                self.model.borrow_mut().status = SharedString::from(format!("删除失败：{e}"));
                self.model.borrow_mut().status_ok = false;
            }
        }
        cx.notify();
    }

    /// 树模型失效（新建/重命名/删除后的派生状态失效源）：父目录直接子项缓存作废并
    /// 重扫；`changed`（目录时含其后代）的展开集/子项缓存/在途扫描一并作废；布局代 +1。
    fn file_tree_invalidate(
        &mut self,
        changed: &std::path::Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(parent) = changed.parent()
            && self
                .sidebar
                .files
                .root
                .as_deref()
                .is_some_and(|r| parent.starts_with(r) || parent == r)
        {
            self.sidebar.files.children.remove(parent);
            self.sidebar.files.pending.remove(parent);
            self.spawn_file_tree_scan(parent.to_path_buf(), window, cx);
        }
        self.sidebar
            .files
            .children
            .retain(|p, _| p != changed && !p.starts_with(changed));
        self.sidebar.files.pending.retain(|p| p != changed && !p.starts_with(changed));
        self.sidebar.files.expanded.retain(|p| p != changed && !p.starts_with(changed));
        self.sidebar.files.layout_gen = self.sidebar.files.layout_gen.wrapping_add(1);
    }

    // -----------------------------------------------------------------
    // 文件树右键：资源管理器定位 / 复制路径 / 复制相对路径 / 剪切 / 复制（文件）
    // -----------------------------------------------------------------

    /// 「在资源管理器中显示」：构造 `explorer /select,<绝对路径>` 并 spawn（不阻塞 UI）；
    /// 启动失败 → 状态栏提示（不 panic、不弹错误框，静默可接受）。
    pub(crate) fn file_tree_reveal_in_explorer(
        &mut self,
        path: &std::path::Path,
        cx: &mut Context<Self>,
    ) {
        let mut cmd = tray::reveal_in_explorer_command(path);
        match cmd.spawn() {
            Ok(_) => {}
            Err(e) => {
                self.model.borrow_mut().status = SharedString::from(format!("无法在资源管理器中显示：{e}"));
                self.model.borrow_mut().status_ok = false;
            }
        }
        cx.notify();
    }

    /// 「复制路径」：把**绝对路径**写入文本剪贴板（不带引号）。
    pub(crate) fn file_tree_copy_path(&mut self, path: &std::path::Path, cx: &mut Context<Self>) {
        let p = path.to_string_lossy().into_owned();
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(p));
        cx.notify();
    }

    /// 「复制相对路径」：相对已打开文件夹根（`files.root`）的相对路径，原生分隔符、
    /// 不带引号；根缺失 / `strip_prefix` 失败 / **路径即根自身（相对为空）** →
    /// 防御性退化复制绝对路径。
    pub(crate) fn file_tree_copy_relative_path(
        &mut self,
        path: &std::path::Path,
        cx: &mut Context<Self>,
    ) {
        let rel = self
            .sidebar
            .files
            .root
            .as_deref()
            .and_then(|r| path.strip_prefix(r).ok())
            // 根自身 strip_prefix 得空串：复制空前缀无意义 → 退化绝对路径。
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string_lossy().into_owned());
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(rel));
        cx.notify();
    }

    /// 「剪切」：把路径写入系统剪贴板文件操作（`CF_HDROP` + effect=Move），语义 =
    /// 资源管理器「剪切」；写入失败（剪贴板占用等）→ 状态栏提示不崩溃。
    pub(crate) fn file_tree_cut(&mut self, path: &std::path::Path, cx: &mut Context<Self>) {
        self.file_clipboard_drop(path, false, cx);
    }

    /// 「复制」（文件操作）：同「剪切」但 effect=Copy，语义 = 资源管理器「复制」。
    pub(crate) fn file_tree_copy_file(&mut self, path: &std::path::Path, cx: &mut Context<Self>) {
        self.file_clipboard_drop(path, true, cx);
    }

    /// 剪贴板文件操作共用执行：`tray::set_clipboard_file_drop`（unsafe Win32 收敛在 tray）；
    /// 成功 → 状态栏「已复制/已剪切」；失败 → 状态栏提示（截断长路径）。
    fn file_clipboard_drop(&mut self, path: &std::path::Path, copy: bool, cx: &mut Context<Self>) {
        let label = if copy { "已复制：" } else { "已剪切：" };
        match tray::set_clipboard_file_drop(path, copy) {
            Ok(()) => {
                self.model.borrow_mut().status = SharedString::from(format!(
                    "{label}{}",
                    path.display()
                ));
                self.model.borrow_mut().status_ok = true;
            }
            Err(e) => {
                self.model.borrow_mut().status = SharedString::from(e);
                self.model.borrow_mut().status_ok = false;
            }
        }
        cx.notify();
    }
}
