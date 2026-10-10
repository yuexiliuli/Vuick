//! 编辑器事件订阅与预览增量同步：编辑改动 → 脏追踪 / 状态栏缓存刷新 /
//! 统一 MD 预览逐键增量桥，以及脱 UI 线程的后台预览扫描循环（去抖 + 代际闸门）。

use super::*;

impl EditorApp {
    /// 订阅编辑器变更（置脏 + 刷新状态栏 + 预览增量入口）。
    /// `make_tab` 新建与 `adopt_tab` 跨窗口迁移共用：订阅绑定的是 *本窗口* 的 `EditorApp`，
    /// 因此选项卡迁到哪个窗口，编辑事件就只刷新哪个窗口。
    pub(super) fn subscribe_editor(
        &self,
        editor: &gpui::Entity<crate::editor::CodeEditor>,
        cx: &mut Context<Self>,
    ) -> Subscription {
        let ed = editor.clone();
        cx.subscribe(&ed, |this, emitter, ev: &crate::editor::EditorEvent, cx| {
            if let Some(t) = this.tabs.iter().position(|t| t.editor == emitter) {
                match ev {
                    crate::editor::EditorEvent::Edited => this.on_editor_edited(t, cx),
                    // 纯光标/选区移动（鼠标点击/方向键/搜索跳转）不走重活，
                    // 只刷新选项卡缓存的行/列 → 状态栏 `Ln x, Col y` 即时跟随。
                    crate::editor::EditorEvent::SelectionChanged => {
                        this.on_editor_selection_changed(t, cx)
                    }
                    _ => {}
                }
            }
        })
    }

    /// 编辑器「光标 / 选区变化」（不修改文本）：刷新选项卡缓存的行/列。轻量路径——
    /// 只做 O(1) 光标读数并写回缓存，不碰搜索缓存 / 脏区间 / 预览同步（与
    /// `on_editor_edited` 的重活分开，避免鼠标选中拖动时触发逐次预览扫描）。
    /// 不 `cx.notify()`：编辑侧 `notify` 已令窗口把本视图及其祖先（含 app 根视图）
    /// 标脏（gpui `mark_view_dirty`，见 on_editor_edited 注解），状态栏同帧读到新值。
    pub(super) fn on_editor_selection_changed(&mut self, tab_ix: usize, cx: &mut Context<Self>) {
        let (row, col) = {
            let st = self.tabs[tab_ix].editor.read(cx);
            st.cursor_position()
        };
        let tab = &mut self.tabs[tab_ix];
        tab.cursor_row = row;
        tab.cursor_col = col;
    }

    /// 编辑事件：失效搜索缓存、推进版本、更新脏区间/计数，并按需触发一轮预览同步。
    /// 用 **选项卡下标** 而非长借 `&mut tab`——中途要调 `start_preview_sync`（整 `self` 可变）。
    pub(super) fn on_editor_edited(&mut self, tab_ix: usize, cx: &mut Context<Self>) {
        let _t_change = crate::prof::begin();
        // 内容被编辑 → 侧边栏搜索缓存失效 + 版本号推进
        self.search_scan = None;
        // 编辑侧通道 → dirty 区间（bounding box，当前文本字节坐标）：
        // CodeEditor 记录最近一次编辑区间（字节），每轮 Change 1:1 取走。
        let dirty_range = {
            let tab = &mut self.tabs[tab_ix];
            tab.edit_version = tab.edit_version.wrapping_add(1);
            tab.editor.update(cx, |ed, _| ed.take_last_edit())
        };
        if let Some((s, oe, ne)) = dirty_range {
            let tab = &mut self.tabs[tab_ix];
            tab.preview_dirty = merge_preview_dirty(tab.preview_dirty, s, oe, ne);
            // A3 逐键增量桥：右栏 WyEditor 预览（若该选项卡有）重放**同一次**编辑
            // （src 字节区间 `[s, oe)` → 编辑后新文本 `[s, ne)`），走 WyEditor 增量管线
            //（Doc::edit + Pass1 + derived），大文档每键 ≈0.4ms——预览恒等于编辑区。
            if let Some(wy) = tab.md_wy.clone() {
                let ed = tab.editor.clone();
                let new_text = ed.read(cx).rope().slice(s..ne).to_string();
                if wy.read(cx).is_loaded() {
                    wy.update(cx, |w, cx| w.preview_apply_edit(s..oe, &new_text, cx));
                } else {
                    // 自愈：异步装配尚未完成用户已编辑 → 一次性从当前编辑区全文补齐
                    //（O(n) 仅此一次），此后退化到逐键增量（防空壳上重放错位）。
                    let full = ed.read(cx).value().to_string();
                    wy.update(cx, |w, _cx| w.set_source_text(&full));
                }
            }
        }
        // 行/字/光标走 CodeEditor 的 O(1) 计数，不再每键物化全文。
        // 行数用显示语义（`str::lines()` 口径，与文件打开/状态栏一致），
        // 不用含幻影尾行的编辑器模型 `line_count()`。
        let (line_count, char_count, cursor_row, cursor_col) = {
            let st = self.tabs[tab_ix].editor.read(cx);
            let cursor = st.cursor_position();
            (st.display_line_count(), st.char_count(), cursor.0, cursor.1)
        };
        // A4 脱离：仅在非 Markdown（EPUB / 手动预览）时才同步 MarkdownPreview——
        // MD 右栏预览已由逐键增量桥（`on_editor_edited` 上面的 bridge）实时跟随统一组件，
        // 不再为 MD 调后台 scan_blocks/喂 MarkdownPreview（省每键一次扫块 + 一份源副本）。
        if self.tabs[tab_ix].preview_on && !self.is_markdown_tab(tab_ix) {
            self.start_preview_sync(cx);
        }
        let tab = &mut self.tabs[tab_ix];
        // 只对 readonly_preview（EPUB/PDF，编辑区为空）镜像全文到 content；
        // 普通文本/Markdown 以编辑器 Rope 为准，不做整份副本（省内存）。
        if tab.readonly_preview {
            let content = tab.editor.read(cx).text().to_string();
            tab.content = SharedString::from(content);
        }
        tab.line_count = line_count;
        tab.char_count = char_count;
        tab.cursor_row = cursor_row;
        tab.cursor_col = cursor_col;
        if !tab.dirty {
            tab.dirty = true;
        }
        // 不再 `cx.notify()`：编辑侧的 `notify` 已令窗口把**本视图及其祖先**
        // （含 app 根视图）标脏（gpui `mark_view_dirty`），下一帧必重渲染，
        // 状态栏/选项卡脏点/预览都在同一帧读到最新值——重复通知只是
        // 多走一遍 invalidation 簿记（大文档逐键该项 ~10ms+，见 prof）。
        crate::prof::end("tab_change", _t_change);
    }

    /// 开启 **一轮** Markdown 预览同步：循环「快照 → 后台扫描 → 装配 → 检查版本是否有
    /// 新编辑」，有新编辑就续跑，直到追上最新内容才停——打字期间**不会叠起多轮后台扫描**
    /// （`preview_syncing` 闸门），从「最后一次按键」起该轮扫描结束即收敛。
    pub(super) fn start_preview_sync(&mut self, cx: &mut Context<Self>) {
        if self.preview_pending || self.preview_syncing {
            return;
        }
        self.preview_pending = true;
        self.preview_syncing = true;
        self.preview_seq = self.preview_seq.wrapping_add(1);
        let seq = self.preview_seq;
        cx.spawn(async move |this, cx| {
            loop {
                let Some((snapshot, panel_bg, base_dir, prev, version, dirty)) =
                    this
                        .update(cx, |this, cx| {
                            let panel_bg = this.panel_bg();
                            let base_dir = this
                                .tabs
                                .get(this.active_tab)
                                .and_then(|t| t.file_path.as_ref())
                                .and_then(|p| p.parent())
                                .map(|d| d.to_path_buf());
                            let snapshot = this
                                .tabs
                                .get(this.active_tab)
                                .filter(|t| t.preview_on)
                                .map(|t| t.editor.read(cx).text().clone());
                            let prev = this
                                .tabs
                                .get(this.active_tab)
                                .and_then(|t| t.preview_prev.clone());
                            let version = this
                                .tabs
                                .get(this.active_tab)
                                .map(|t| t.edit_version)
                                .unwrap_or(0);
                            let dirty = this
                                .tabs
                                .get_mut(this.active_tab)
                                .and_then(|t| t.preview_dirty.take());
                            snapshot.map(|snap| {
                                (snap, panel_bg, base_dir, prev, version, dirty)
                            })
                        })
                        .ok()
                        .flatten()
                else {
                    break;
                };
                let index = match &prev {
                    Some((p_src, p_idx)) => {
                        let p_src = p_src.clone();
                        let p_idx = (**p_idx).clone();
                        let snap = snapshot.clone();
                        let dirty = dirty.map(|(a, b)| a..b);
                        cx.background_executor()
                            .spawn(async move {
                                crate::markdown::scan_blocks_incremental_rope(
                                    &p_src,
                                    &p_idx,
                                    &snap,
                                    dirty,
                                )
                            })
                            .await
                    }
                    None => {
                        let snap = snapshot.clone();
                        cx.background_executor()
                            .spawn(async move {
                                crate::markdown::scan_blocks_rope(&snap)
                            })
                            .await
                    }
                };
                let index = std::rc::Rc::new(index);
                let keep = this
                    .update(cx, |this, cx| {
                        if this.preview_seq != seq {
                            return false;
                        }
                        let mut keep = false;
                        if let Some(tab) = this.tabs.get_mut(this.active_tab) {
                            if tab.preview_on {
                                tab.md_preview.update(cx, |view, cx| {
                                    view.set_background(panel_bg);
                                    view.set_base_dir(base_dir);
                                    view.set_source_with_index(
                                        crate::markdown::PreviewSrc::Rope(snapshot.clone()),
                                        index.clone(),
                                        cx,
                                    );
                                });
                                cx.notify();
                            }
                            tab.preview_prev = Some((snapshot.clone(), index.clone()));
                            keep = tab.preview_on && tab.edit_version != version;
                        }
                        keep
                    })
                    .unwrap_or(false);
                if !keep {
                    break;
                }
            }
            let _ = this.update(cx, |this, _| {
                this.preview_pending = false;
                this.preview_syncing = false;
            });
        })
        .detach();
    }
}
