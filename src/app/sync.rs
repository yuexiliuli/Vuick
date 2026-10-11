//! 编辑器事件订阅与预览增量同步：编辑改动 → 脏追踪 / 状态栏缓存刷新 /
//! 统一 MD 预览逐键增量桥（M5-B5 P4-2：A 侧 `MarkdownPreview` 已删除，预览统一
//! 走 `WyEditor` 只读宿主，后台预览扫描循环随之移除）。

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

    /// 编辑事件：失效搜索缓存、推进版本，并把同一次编辑经增量桥重放到右栏统一预览
    ///（`WyEditor`，MD / EPUB 同一只读宿主）。用 **选项卡下标** 而非长借 `&mut tab`。
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
}
