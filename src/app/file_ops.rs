//! file_ops.

use super::*;

impl EditorApp {
    pub(super) fn open_file_in_tab(
        &mut self,
        idx: usize,
        path: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let p = PathBuf::from(path.trim());
        let Some(tab) = self.tabs.get_mut(idx) else {
            return;
        };
        match fs::read_to_string(&p) {
            Ok(text) => {
                let text = Self::normalize_newlines(&text);
                let lang = Self::language_id(&p);
                let lines = text.lines().count().max(1);
                let chars = text.chars().count();
                tab.editor.update(cx, |ed, cx| {
                    ed.set_value(text.clone(), window, cx);
                    ed.set_highlighter(lang, cx);
                });
                tab.md_view.update(cx, |view, cx| view.set_text(&text, cx));
                tab.file_path = Some(p.clone());
                tab.content = SharedString::from(text);
                tab.line_count = lines;
                tab.char_count = chars;
                tab.cursor_row = 0;
                tab.cursor_col = 0;
                tab.dirty = false;
                if Self::is_markdown_path(&p) {
                    tab.preview_on = true;
                }
                self.preview_scroll = gpui::ScrollHandle::new();
                self.status = SharedString::from(format!("已打开：{}", p.display()));
                self.status_ok = true;
                cx.notify();
            }
            Err(e) => {
                self.status = SharedString::from(format!("打开失败：{e}"));
                self.status_ok = false;
                cx.notify();
            }
        }
    }

    pub(super) fn open_file(&mut self, path: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.open_file_in_tab(self.active_tab, path, window, cx);
    }

    pub(super) fn open_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let entity = cx.entity();
        let idx = self.active_tab;
        window
            .spawn(cx, async move |cx| {
                let Some(path) = rfd::FileDialog::new().set_title("打开文件").pick_file() else {
                    return;
                };
                let path = path.to_string_lossy().into_owned();
                let _ = cx.update_window_entity(&entity, |this, window, cx| {
                    this.open_file_in_tab(idx, &path, window, cx);
                });
            })
            .detach();
    }

    pub(super) fn write_to_path(
        &mut self,
        idx: usize,
        path: &std::path::Path,
        text: String,
        cx: &mut Context<Self>,
    ) -> bool {
        match fs::write(path, text) {
            Ok(()) => {
                if let Some(tab) = self.tabs.get_mut(idx) {
                    tab.file_path = Some(path.to_path_buf());
                    tab.dirty = false;
                }
                self.status = SharedString::from(format!("已保存：{}", path.display()));
                self.status_ok = true;
                cx.notify();
                true
            }
            Err(e) => {
                self.status = SharedString::from(format!("保存失败：{e}"));
                self.status_ok = false;
                cx.notify();
                false
            }
        }
    }

    pub(super) fn save_tab(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(tab) = self.tabs.get(idx) else {
            return false;
        };
        let Some(p) = tab.file_path.clone() else {
            return self.save_tab_as(idx, window, cx);
        };
        let text = tab.editor.read(cx).value().to_string();
        self.write_to_path(idx, &p, text, cx)
    }

    pub(super) fn save_tab_as(
        &mut self,
        idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(tab) = self.tabs.get(idx) else {
            return false;
        };
        let file_name = self.tab_file_name(idx);
        let text = tab.editor.read(cx).value().to_string();
        let entity = cx.entity();
        window
            .spawn(cx, async move |cx| {
                let Some(file) = rfd::FileDialog::new()
                    .set_title("保存文件")
                    .set_file_name(&file_name)
                    .save_file()
                else {
                    return;
                };
                let _ = cx.update_window_entity(&entity, |this, _window, cx| {
                    this.write_to_path(idx, &file, text, cx);
                });
            })
            .detach();
        true
    }

    pub(super) fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.save_tab(self.active_tab, window, cx);
    }

    pub(super) fn save_as(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.save_tab_as(self.active_tab, window, cx);
    }

    pub(super) fn paste_normalized(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(clipboard) = cx.read_from_clipboard() else {
            return;
        };
        let Some(text) = clipboard.text() else {
            return;
        };
        let text = Self::normalize_newlines(&text);
        let Some(tab) = self.tabs.get(self.active_tab) else {
            return;
        };
        tab.editor.update(cx, |editor, cx| {
            editor.replace(text, window, cx);
        });
    }
}
