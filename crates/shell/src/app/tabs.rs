//! tabs：选项卡栏的**宿主薄适配**（渲染/交互/右键菜单项构建在 shell_tabs 的 `TabBar` 实体）。
//!
//! 本模块只保留 `EditorApp` 的宿主侧内容：
//!
//! - **生命周期动作 glue**：`make_tab` / `new_tab` / `activate_tab` / `remove_tab` /
//!   `reorder_tab` / `tab_close_*` / `tab_move/copy_*` —— 动作体在 `shell_core::AppModel`
//!   （经 `AppHost` 反调宿主），此处保留薄转发（`app/tests` 与 shell 其它模块经此面调用）。
//! - **关闭保存流程**：`close_tab`（脏/未命名弹原生确认框异步等待）+ `save_unnamed_and_close`
//!   （另存为对话框）——宿主文件保存副作用，留在宿主。
//! - **tab_drag 相关入口**：`reorder_tab`（排序落地
//!   动作体在 `AppModel`，glue 转发）；`tab_insert_index` / `tab_drag_over_row`（栏内
//!   排序命中）在 `shell_tabs::drag` 纯函数（宿主 `tab_bounds` 快照以参数传入），
//!   layout.rs `on_drag_move` 直调域函数。
//! - **横向滚动条宿主面**：`poke_tab_hscroll`（宿主字段 `tab_bar_hscroll` + 计时
//!   `tab_hscroll_delay`）。
//! - **渲染 facade**：`render_tab_bar` 经 `tab_bar_view` 实体渲染（宿主预取派生数据传入）；
//!   `render_tab_bar_menu` 读宿主 `tab_bar_menu` 槽位（槽位仍属宿主：测试直读 +
//!   media/pdf 前置弹窗互关直清，实体经 `TabsHost` 落地）。

use super::*;
use config::timing::TAB_HSCROLL_HIDE_MS;

impl EditorApp {
    /// 动作体在 `AppModel::make_tab`（shell_core::actions）。此处 glue 转发。
    #[doc(hidden)]
    pub fn make_tab(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> usize {
        // 新建选项卡成为活动（含打开视频文件）→ 重新武装视频自动隐藏：手动打开覆盖复位，
        // 保证「每次视频激活都从隐藏开始」。
        self.sidebar.video_hide_override = false;
        let host: &dyn AppHost = &*self;
        self.model.borrow_mut().make_tab(host, &mut **cx)
    }

    /// 动作体在 `AppModel::new_tab`。glue 转发。
    pub(super) fn new_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let host: &dyn AppHost = &*self;
        self.model.borrow_mut().new_tab(host, window, &mut **cx);
    }

    /// 栏内拖拽排序落地：把当前下标 `from` 的选项卡移动到「插入下标」`target`
    /// （`shell_tabs::drag::tab_insert_index` 输出，相对当前 `tabs`；`tabs.len()` = 末尾）。
    /// 插回原位 / 原相邻位（顺序不变）→ 不动作；单选项卡天然不动作。活动下标随移动
    /// 校正；各选项卡内容/脏点/路径不变；结构变化即时持久化。
    /// 动作体在 `AppModel::reorder_tab`。glue 转发。
    pub(super) fn reorder_tab(&mut self, from: usize, target: usize, cx: &mut Context<Self>) {
        let host: &dyn AppHost = &*self;
        self.model.borrow_mut().reorder_tab(host, from, target, &mut **cx);
    }

    /// 动作体在 `AppModel::activate_tab`。glue 转发。
    #[doc(hidden)]
    pub fn activate_tab(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        // 活动选项卡**真正切换**（idx != 现活动）→ 重新武装视频自动隐藏：手动打开覆盖
        // 复位（每次切到视频都从隐藏开始）。相同下标重复点击不重置（避免误关已手动打开的
        // 侧边栏）。
        let switching = self.model.borrow().active_tab != idx;
        if switching {
            self.sidebar.video_hide_override = false;
        }
        let host: &dyn AppHost = &*self;
        self.model.borrow_mut().activate_tab(host, idx, window, &mut **cx);
    }

    /// 对话框 await 返回后的下标复验：按「发起时的选项卡身份（CodeEditor 实体 id）」
    /// 在当前 `tabs` 里找其当前下标。await 期间用户可能拖拽重排 / 经其它路径关闭
    /// 选项卡，`idx` 已不可靠——按下标操作会误关/误存错选项卡（P1-33）。
    /// 返回 `None` = 该身份已不在（已被其它路径关闭），调用方应放弃。
    fn tab_index_for_editor_id(&self, editor_id: gpui::EntityId) -> Option<usize> {
        self.model
            .borrow()
            .tabs
            .iter()
            .position(|t| t.payload.editor.entity_id() == editor_id)
    }

    /// 关闭选项卡：干净命名文件夹闭直通 `remove_tab`；脏 / 未命名（空内容同样在盘上有
    /// 会话槽位）→ 弹原生任务对话框异步等待用户选择（是=另存为转正 / 否=删临时槽位 /
    /// 取消=留在应用）。宿主文件保存副作用收在此（PDF 走待保存模型 save 流程）。
    #[doc(hidden)]
    pub fn close_tab(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        if idx >= self.model.borrow().tabs.len() {
            return;
        }
        // 发起时记录选项卡身份：确认框异步等待期间用户可能拖拽重排 / 另路关闭选项卡，
        // 下标不可靠，await 返回后须按身份复验再操作（P1-33）。
        let tab_id = self.model.borrow().tabs[idx].payload.editor.entity_id();
        // 未命名选项卡（无论脏与否：空内容同样在盘上有会话槽位）→ 一律弹确认
        // （是=另存为转正、否=删除临时槽位、取消=留在应用）；命名文件仅当脏（有
        // 未落盘修改）才询问，干净直通关闭。
        if self.model.borrow().tabs[idx].payload.file_path.is_some()
            && !self.model.borrow().tabs[idx].payload.dirty
        {
            self.remove_tab(idx, window, cx);
            return;
        }
        // 未保存 / 未命名 → 弹原生任务对话框，异步等待用户选择
        let entity = cx.entity();
        let answer = window.prompt(
            PromptLevel::Warning,
            "未保存的更改",
            Some("此文件尚未保存，关闭前要保存吗？"),
            &["保存", "不保存", "cancel"],
            cx,
        );
        window
            .spawn(cx, async move |cx| {
                // oneshot 通道被取消视为"取消"
                let idx_ans = answer.await.unwrap_or(2);
                // P1-33：await 期间用户可能拖拽重排 / 经其它路径关闭选项卡，原 `idx`
                // 已不可靠。按「发起时的选项卡身份（CodeEditor 实体 id）」复验当前下标；
                // 已不在（被其它路径关闭）→ 放弃本次关闭（不误删/误存错选项卡）。
                let Some(idx) = cx
                    .update_window_entity(&entity, |this, _w, _cx| {
                        this.tab_index_for_editor_id(tab_id)
                    })
                    .unwrap_or(None)
                else {
                    return;
                };
                let should_close = match idx_ans {
                    0 => {
                        // PDF 选项卡：走待保存模型——触发 PdfViewer::save，等完成信号；
                        // 保存成功（或本就无待存）才关，失败保留标签（observe 已 toast）。
                        let is_pdf = cx
                            .update_window_entity(&entity, |this, _w, _cx| {
                                this.model.borrow().tabs
                                    .get(idx)
                                    .is_some_and(|t| matches!(t.payload.reader, MediaReader::Pdf(_)))
                            })
                            .unwrap_or(false);
                        if is_pdf {
                            // 触发 PdfViewer::save（fire-and-forget，成功清 dirty + notify）；
                            // 随后在**前台**用 timer 轮询 dirty 是否清（save 成功即清）。不在
                            // 后台做阻塞 recv / 长自旋——与 save 自身的 worker 任务零竞争
                            // （后台线程池被阻塞 recv 占住会让 save 的 worker 任务饿死）。
                            let started = cx
                                .update_window_entity(&entity, |this, _w, cx| {
                                    let p = this
                                        .model.borrow().tabs
                                        .get(idx)
                                        .and_then(|t| match &t.payload.reader {
                                            MediaReader::Pdf(p) => Some(p.clone()),
                                            _ => None,
                                        });
                                    p.map(|p| p.update(cx, |v, cx| v.save(cx)))
                                        .unwrap_or(false)
                                })
                                .unwrap_or(false);
                            if !started {
                                true // 无待存 → 直接关
                            } else {
                                // 等 save 完成（dirty 清）：200 × 25ms = 5s 上限；
                                // save 失败 → dirty 保持 → 超时返回 false → 保留标签。
                                let mut saved = false;
                                for _ in 0..200 {
                                    let clean = cx
                                        .update_window_entity(&entity, |this, _w, cx| {
                                            this.model.borrow().tabs
                                                .get(idx)
                                                .map(|t| match &t.payload.reader {
                                                    MediaReader::Pdf(p) => {
                                                        !p.read(cx).is_dirty()
                                                    }
                                                    _ => true,
                                                })
                                                .unwrap_or(true)
                                        })
                                        .unwrap_or(true);
                                    if clean {
                                        saved = true;
                                        break;
                                    }
                                    cx.background_executor()
                                        .timer(std::time::Duration::from_millis(25))
                                        .await;
                                }
                                saved
                            }
                        } else {
                            let (saved, need_dialog) = cx
                                .update_window_entity(&entity, |this, _window, cx| {
                                    // 先取出 path（owned）再 match，避免 `match` 掮获 `borrow()`
                                    // 临时 Ref 跨过块内 `&mut self` 调用（E0502 / 借用约束收窄）。
                                    let path = this
                                        .model.borrow().tabs
                                        .get(idx)
                                        .and_then(|t| t.payload.file_path.clone());
                                    match path {
                                        Some(p) => {
                                            // 走统一口径：live2 开启时读 WyEditor 实时内容
                                            // （否则关闭保存的是陈旧的源码编辑区，丢整段编辑）。
                                            let text =
                                                this.model.borrow().tab_effective_text(idx, cx);
                                            (Some(this.write_to_path(idx, &p, text, cx)), false)
                                        }
                                        None => (None, true),
                                    }
                                })
                                .unwrap_or((None, false));
                            if need_dialog {
                                let _ =
                                    cx.update_window_entity(&entity, |this, window, cx| {
                                        this.save_unnamed_and_close(idx, window, cx);
                                    });
                                false
                            } else {
                                saved.unwrap_or(false)
                            }
                        }
                    }
                    1 => true,
                    _ => false,
                };
                if should_close {
                    let _ = cx.update_window_entity(&entity, |this, window, cx| {
                        this.remove_tab(idx, window, cx);
                    });
                }
            })
            .detach();
    }

    /// 未命名选项卡关闭「保存（另存为）」→ 系统另存为对话框，成功转正后关闭。
    fn save_unnamed_and_close(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        if idx >= self.model.borrow().tabs.len() {
            return;
        }
        // 记录发起时选项卡身份：另存为对话框异步等待期间同样可能拖拽重排/另路关闭，
        // await 返回后须按身份复验再操作（P1-33）。
        let tab_id = self.model.borrow().tabs[idx].payload.editor.entity_id();
        let file_name = self.tab_file_name(idx);
        // live2 开启时读 WyEditor 实时内容（防关闭保存丢编辑）。
        let text = self.model.borrow().tab_effective_text(idx, cx);
        let entity = cx.entity();
        window
            .spawn(cx, async move |cx| {
                let Some(file) = rfd::AsyncFileDialog::new()
                    .set_title("保存文件")
                    .set_file_name(&file_name)
                    .save_file()
                    .await
                else {
                    return;
                };
                let file = file.path().to_path_buf();
                // P1-33：对话框 await 返回后按身份复验当前下标；已不在（被其它路径
                // 关闭）→ 放弃（不把写到/关闭错选项卡）。仍在 → 用复验后的下标。
                let Some(idx) = cx
                    .update_window_entity(&entity, |this, _w, _cx| {
                        this.tab_index_for_editor_id(tab_id)
                    })
                    .unwrap_or(None)
                else {
                    return;
                };
                let saved = cx
                    .update_window_entity(&entity, |this, _window, cx| {
                        this.write_to_path(idx, &file, text.clone(), cx)
                    })
                    .unwrap_or(false);
                if saved {
                    let _ = cx.update_window_entity(&entity, |this, window, cx| {
                        this.remove_tab(idx, window, cx);
                    });
                }
            })
            .detach();
    }

    /// 动作体在 `AppModel::remove_tab`。glue 转发。
    #[doc(hidden)]
    pub fn remove_tab(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        // 关闭的正是活动选项卡 → 活动下标变化（域内选中新活动，不经本 activate_tab glue）
        // → 重新武装视频自动隐藏：手动打开覆盖复位（落回文本/另一视频都从隐藏开始）。
        // 移除非活动选项卡不扰动（仍在同一视频 tab 上，保留用户手动打开）。
        let removing_active = self.model.borrow().active_tab == idx;
        if removing_active {
            self.sidebar.video_hide_override = false;
        }
        let host: &dyn AppHost = &*self;
        self.model.borrow_mut().remove_tab(host, idx, window, &mut **cx);
    }

    /// 标记“选项卡栏最近在滚动”：横向滚动条立即可见，并重启自动隐藏计时
    /// （[`TAB_HSCROLL_HIDE_MS`] 后无新滚动则隐藏）。`SeqAfter` 代序作废旧定时器，
    /// 旧定时器到点不误清新滚动。`TabBar` 实体滚轮回调经 `TabsHost` 反调本方法。
    pub(super) fn poke_tab_hscroll(&mut self, cx: &mut Context<Self>) {
        *self.tab_bar_hscroll.visible.borrow_mut() = true;
        cx.notify();
        let weak = cx.weak_entity();
        self.tab_hscroll_delay.after(
            cx,
            std::time::Duration::from_millis(TAB_HSCROLL_HIDE_MS),
            move |app| {
                let _ = weak.update(app, |app, cx| {
                    *app.tab_bar_hscroll.visible.borrow_mut() = false;
                    cx.notify();
                });
            },
        );
    }

    /// 选项卡栏渲染**宿主 facade**（逻辑迁 shell_tabs `TabBar` 实体）：宿主渲染期把
    /// `&mut self.tab_bounds`（拖拽命中槽，实体重建 + 记录器写回）/ `&self.tab_bar_hscroll`
    /// 与按模型预取的派生布尔（眼睛钮可见性 = 活动 tab 是否 Markdown、当前是否开预览）
    /// 传入域实体渲染。——渲染期禁入宿主（宿主正被 render 借用），派生数据以参数预取。
    pub(super) fn render_tab_bar(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let active_is_md = self
            .model
            .borrow()
            .tabs
            .get(self.model.borrow().active_tab)
            .and_then(|t| t.payload.file_path.as_ref())
            .map(|p| Self::is_markdown_path(p))
            .unwrap_or(false);
        let preview_on = self.show_preview();
        self.tab_bar_view.update(cx, |tb, cx| {
            tb.render_tab_bar(
                &mut self.tab_bounds,
                &self.tab_bar_hscroll,
                active_is_md,
                preview_on,
                window,
                cx,
            )
        })
    }

    /// 选项卡栏右键菜单 Overlay：按右键时记录的**窗口坐标**定位
    /// （`AnchoredPositionMode::Window` + `position(pos)`——`position()` 才是 gpui 语义里
    /// 「窗口坐标」入参；旧的 `offset(pos)` 缺 `position()` 时会被当作「相对锚定元素自身
    /// 静态布局位置的偏移」，而本菜单包在 `#tab-bar-menu` 外壳 flex 流内、静态位在窗口
    /// 底部 → 菜单被推出窗口再被 `snap_to_window_with_margin` 钳回底部（沉底 bug）。
    /// 媒体右键菜单因直接作为外壳 flex 直子、静态位恰为 (0,0) 才表现出正常而已，同隐患。
    /// 菜单槽位（pos/实体/订阅）留宿主（测试直读 + 前置弹窗互关直清），`TabBar` 实体
    /// 经 `TabsHost` 落地。
    pub(super) fn render_tab_bar_menu(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let Some(pos) = self.tab_bar_menu.pos else {
            return div().into_any_element();
        };
        let Some(menu) = self.tab_bar_menu.entity.clone() else {
            return div().into_any_element();
        };
        div()
            .id("tab-bar-menu")
            .debug_selector(|| "tab-bar-menu".into())
            .child(ui::menu::popup_overlay(pos, menu, ui::menu::MENU_EDGE_MARGIN))
            .into_any_element()
    }

    // -----------------------------------------------------------------
        // 批量关闭 + 移入/复制新窗口（动作体在 AppModel，glue 转发）
    // -----------------------------------------------------------------

    /// 批量关闭动作体在 `AppModel`（tab_close_guard/others/right/all/saved）。
    /// 「关闭其他」：关闭除 `idx` 外全部；任一受影响脏/未命名 → 中止一个都不关。
    pub(super) fn tab_close_others(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        let host: &dyn AppHost = &*self;
        self.model.borrow_mut().tab_close_others(host, idx, window, &mut **cx);
    }

    /// 「关闭右侧」：关闭 `idx` 右侧全部；任一受影响脏/未命名 → 中止一个都不关。
    pub(super) fn tab_close_right(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        let host: &dyn AppHost = &*self;
        self.model.borrow_mut().tab_close_right(host, idx, window, &mut **cx);
    }

    /// 「关闭全部」：关闭全部；任一脏/未命名 → 中止一个都不关。
    pub(super) fn tab_close_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let host: &dyn AppHost = &*self;
        self.model.borrow_mut().tab_close_all(host, window, &mut **cx);
    }

    /// 「关闭已保存」：只关 `有路径 && !dirty` 的选项卡（定义如此，无中止语义）；
    /// 脏 / 未命名 / 无路径空选项卡保留。
    pub(super) fn tab_close_saved(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let host: &dyn AppHost = &*self;
        self.model.borrow_mut().tab_close_saved(host, window, &mut **cx);
    }

    /// 「移入新窗口」：取载荷 → 拆进新独立窗口（级联偏移 = 源窗口左上 + (24,24)）。
    /// 源窗口剩零选项卡 → 现行为回欢迎页；detached 窗口同样可再拆。
    pub(super) fn tab_move_to_new_window(
        &mut self,
        idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if idx >= self.model.borrow().tabs.len() {
            return;
        }
        let Some(payload) = self.commit_live_and_take_tab_payload(idx, window, cx) else {
            return;
        };
        self.split_tab_to_new_window(payload, Point::new(px(24.), px(24.)), window, cx);
    }

    /// 「复制到新窗口」：源选项卡**保留原状**；文本类 = 内容快照副本（独立编辑器缓冲，
    /// 可独立编辑）；媒体类 = 新窗口按原路径重新打开（文件不可变，路径即快照）。
    pub(super) fn tab_copy_to_new_window(
        &mut self,
        idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if idx >= self.model.borrow().tabs.len() {
            return;
        }
        // 先提纯 reader_none / media_path（owned）再分支，避免 `if` 条件里的 `borrow()`
        // 临时 Ref 跨过块内 `&mut self` 调用（E0502 / 借用约束收窄）。
        let reader_none = self.model.borrow().tabs[idx].payload.reader.is_none();
        let media_path = self.model.borrow().tabs[idx].payload.file_path.clone();
        if reader_none {
            self.tab_copy_text_to_new_window(idx, window, cx);
        } else if let Some(p) = media_path {
            // 媒体（PDF/图片/视频）：文件型不可变 → 新窗口按路径重开（路径即快照）。
            self.tab_copy_media_to_new_window(&p, window, cx);
        }
    }

    /// 文本类「复制到新窗口」：先 `make_tab` 得到空 EditorTab（免手抄 25 字段）→
    /// 用新的独立 CodeEditor 种子（有效文本/语言/光标）→ 回填元数据（路径/dirty/
    /// 预览等）→ `take_tab_payload` 转 DetachedTab → `split_tab_to_new_window`。
    /// 副本不共享源编辑器实体（独立缓冲）；源选项卡不动。复制期间源的编辑事件订阅会
    /// 置脏/刷新缓存，故**先喂编辑器、后回填元数据**（覆盖事件副作用，保 dirty 语义）。
    fn tab_copy_text_to_new_window(
        &mut self,
        idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (text, path, dirty, preview_on, readonly_preview, cursor_row, cursor_col, line_count, char_count, src_enc) = {
            let model = self.model.borrow();
            let t = &model.tabs[idx];
            (
                model.tab_effective_text(idx, &**cx),
                t.payload.file_path.clone(),
                t.payload.dirty,
                t.payload.preview_on,
                t.payload.readonly_preview,
                t.payload.cursor_row,
                t.payload.cursor_col,
                t.payload.line_count,
                t.payload.char_count,
                t.payload.source_encoding, // 复制 → 编码随文档走（保存回写不丢）
            )
        };
        let lang = path.as_ref().map(|p| Self::language_id(p));
        // 复制前记下源窗口活动选项卡：`make_tab` 把 `active_tab` 指到瞬态副本、
        // `take_tab_payload` 拔出副本后又把它改到末位选项卡——复制「非末位」选项卡时
        // 源窗口活动会跳到最后一个选项卡（可见闪烁）。取下后还原到复制前活动。
        let kept_active = self.model.borrow().active_tab;
        let new_idx = self.make_tab(window, cx);
        // ① 先喂独立编辑器（新实体，不共享源缓冲）：内容 / 语言沿用 / 光标保留。
        self.model.borrow().tabs[new_idx].payload.editor.update(cx, |ed, _cx| {
            ed.set_text(&text);
            if let Some(l) = lang {
                ed.set_language(Some(l));
            }
            ed.set_caret_to(ed.offset_for_line_col(cursor_row, cursor_col));
        });
        // ② 后回填元数据（覆盖 set_text 触发的编辑事件副作用，保 dirty/preview 语义）。
        {
            let mut model = self.model.borrow_mut();
            let t = &mut model.tabs[new_idx];
            t.payload.file_path = path.clone();
            t.payload.source_encoding = src_enc;
            t.payload.dirty = dirty;
            t.payload.content = SharedString::default();
            t.payload.line_count = line_count;
            t.payload.char_count = char_count;
            t.payload.cursor_row = cursor_row;
            t.payload.cursor_col = cursor_col;
            t.payload.preview_on = preview_on;
            t.payload.readonly_preview = readonly_preview;
        }
        let Some(payload) = self.take_tab_payload(new_idx) else {
            return;
        };
        // 还原源窗口活动选项卡（瞬态副本已拔出，原下标全部未变）。
        self.model.borrow_mut().active_tab = kept_active;
        // make_tab 期间的会话快照带瞬态副本 tab → 取下后立即补刷（detached 内部早退）。
        self.schedule_session_flush(cx, true);
        self.split_tab_to_new_window(payload, Point::new(px(24.), px(24.)), window, cx);
    }

    /// 媒体类「复制到新窗口」：新开一个 detached 窗口（占位空选项卡）→ 找到该窗口 →
    /// 占位让位于「按原路径重开」媒体（路径即不可变文件的快照，源选项卡保留）。
    fn tab_copy_media_to_new_window(
        &mut self,
        path: &std::path::Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // ① 记当前已注册窗口句柄 → 开新 detached 窗口（占位空载荷）。
        let before: Vec<AnyWindowHandle> = if cx.has_global::<EditorAppRegistry>() {
            cx.global::<EditorAppRegistry>()
                .windows
                .iter()
                .map(|(_, wh)| *wh)
                .collect()
        } else {
            Vec::new()
        };
        let editor = cx.new(|cx| editor::CodeEditor::new(cx));
        let placeholder = DetachedTab {
            editor,
            reader: MediaReader::None,
            md_wy: None,
            md_wy_vscroll: gpui::ScrollHandle::new(),
            md_wy_install: MdWyInstall::default(),
            epub_images: Default::default(),
            epub_notes: Default::default(),
            file_path: None,
            dirty: false,
            source_encoding: None, // 媒体副本：按原路径重开，编码由新窗口打开时重新探测
            content: SharedString::default(),
            line_count: 1,
            char_count: 0,
            cursor_row: 0,
            cursor_col: 0,
            preview_on: false,
            preview_fullscreen: false,
            readonly_preview: false,
            loading_lock: false,
            edit_version: 0,
            reading_restore: None,
            md_restore_pending: None,
        };
        self.split_tab_to_new_window(placeholder, Point::new(px(24.), px(24.)), window, cx);
        // ② 找到刚注册的 detached 新窗口实体（不在 before）→ 占位让位、按原路径重开。
        let Some((target, wh)) = cx
            .global::<EditorAppRegistry>()
            .windows
            .iter()
            .find(|(weak, wh)| {
                !before.contains(wh)
                    && weak
                        .upgrade()
                        .is_some_and(|e| e.read_with(cx, |a, _| a.is_detached))
            })
            .map(|(weak, wh)| (weak.clone(), *wh))
        else {
            return;
        };
        let reopen = path.to_path_buf();
        let _ = wh.update(cx, |_root, w, cx| {
            let _ = target.update(cx, |app, cx| {
                if !app.model.borrow().tabs.is_empty() {
                    app.remove_tab(0, w, cx); // 占位空选项卡让位
                }
                // 直调 `open_file_here`（而非 `open_file`）：本动作的目标窗口就是刚建的新
                // detached 窗口，若走 `open_file` 的媒体重定向，视频会被再投到媒体窗口、
                // 留下空 detached 窗口（media_window.rs 分流只认 `open_file` 一个入口）。
                // 新窗口不含该路径 → 无需「已开则激活」分支。
                app.open_file_here(&reopen.to_string_lossy(), w, cx);
            });
        });
    }
}
