//! file_ops.

use super::*;
use gpui_component::notification::{Notification, NotificationType};
use gpui_component::WindowExt as _;

/// 打开失败通知的 id 类型（配合唯一 key，便于 5 秒后精确移除）。
struct OpenFailureNotification;

/// 打开失败提示自动消失时长（秒）。
const OPEN_FAILURE_AUTOHIDE_SECS: u64 = 5;

/// Markdown 预览重渲染的去抖时长（毫秒）。
///
/// 打字时不再逐键整篇重解析+重建虚拟化列表（大文件每键都可能触发整份
/// set_text + 列表 reset 重测量），而是停止输入片刻后再同步一次。
pub(crate) const PREVIEW_DEBOUNCE_MS: u64 = 200;

impl EditorApp {
    /// 打开失败：状态栏置为失败，并弹出可点击关闭、5 秒后自动消失的通知。
    fn report_open_failure(
        &mut self,
        p: &std::path::Path,
        e: &std::io::Error,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.status = SharedString::from(format!("打开失败：{e}"));
        self.status_ok = false;
        // 自托管计时移除（而非依赖通知层按窗口激活状态倒计时），保证必然消失。
        let key = gpui::ElementId::from(SharedString::from(format!(
            "open-fail-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        )));
        window.push_notification(
            Notification::new()
                .id1::<OpenFailureNotification>(key.clone())
                .title("打开失败")
                .message(format!("无法读取文件内容：{}\n{e}", p.display()))
                .with_type(NotificationType::Error)
                .autohide(false),
            cx,
        );
        let entity = cx.entity();
        window
            .spawn(cx, async move |cx| {
                use gpui::VisualContext as _;
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(OPEN_FAILURE_AUTOHIDE_SECS))
                    .await;
                let _ = cx.update_window_entity(&entity, |_, window, cx| {
                    window.remove_notification1::<OpenFailureNotification>(key, cx)
                });
            })
            .detach();
        cx.notify();
    }

    /// 把已读取的 UTF-8 文本载入指定选项卡，并刷新文件信息与状态。
    fn load_file_text(
        &mut self,
        idx: usize,
        p: &std::path::Path,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let panel_bg = self.panel_bg();
        let Some(tab) = self.tabs.get_mut(idx) else {
            return;
        };
        let lang = Self::language_id(p);
        let lines = text.lines().count().max(1);
        let chars = text.chars().count();
        tab.editor.update(cx, |ed, cx| {
            ed.set_value(text.clone(), window, cx);
            ed.set_highlighter(lang, cx);
        });
        // 自研渲染器按行解析，原文直接喂入即可（行级虚拟化内部做块识别）
        tab.md_preview.update(cx, |view, cx| {
            view.set_background(panel_bg);
            view.set_source(&text, cx);
        });
        tab.file_path = Some(p.to_path_buf());
        tab.content = SharedString::from(text);
        tab.line_count = lines;
        tab.char_count = chars;
        tab.cursor_row = 0;
        tab.cursor_col = 0;
        tab.dirty = false;
        if Self::is_markdown_path(p) {
            tab.preview_on = true;
        }
        self.status = SharedString::from(format!("已打开：{}", p.display()));
        self.status_ok = true;
        cx.notify();
    }

    /// 把文件内容载入已存在的选项卡（读取失败仅提示，保留原选项卡）。
    pub(super) fn open_file_in_tab(
        &mut self,
        idx: usize,
        path: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let p = PathBuf::from(path.trim());
        match fs::read_to_string(&p) {
            Ok(text) => {
                let text = Self::normalize_newlines(&text);
                self.load_file_text(idx, &p, text, window, cx);
            }
            Err(e) => self.report_open_failure(&p, &e, window, cx),
        }
    }

    /// 打开文件：新建一个文件选项卡并在其中载入文件内容，随后把焦点移到新选项卡的编辑区。
    ///
    /// 先读取文件内容再创建选项卡：读取失败时只给出错误提示，
    /// 不再生成一个内容空白的标签页。
    pub(super) fn open_file(&mut self, path: &str, window: &mut Window, cx: &mut Context<Self>) {
        let p = PathBuf::from(path.trim());
        let text = match fs::read_to_string(&p) {
            Ok(text) => Self::normalize_newlines(&text),
            Err(e) => {
                self.report_open_failure(&p, &e, window, cx);
                return;
            }
        };
        let idx = self.make_tab(window, cx);
        self.load_file_text(idx, &p, text, window, cx);
        if let Some(tab) = self.tabs.get(idx) {
            let focus = tab.editor.read(cx).focus_handle(cx);
            window.focus(&focus, cx);
        }
    }

    /// 外部拖入文件（整窗 / 编辑区 / 预览区共用）：逐个打开，目录跳过。
    pub(super) fn open_dropped_paths(
        &mut self,
        paths: &ExternalPaths,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for path in paths.paths() {
            if path.is_file() {
                self.open_file(&path.to_string_lossy(), window, cx);
            }
        }
    }

    pub(super) fn open_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let entity = cx.entity();
        window
            .spawn(cx, async move |cx| {
                let Some(file) = rfd::AsyncFileDialog::new()
                    .set_title("打开文件")
                    .pick_file()
                    .await
                else {
                    return;
                };
                let path = file.path().to_string_lossy().into_owned();
                let _ = cx.update_window_entity(&entity, |this, window, cx| {
                    this.open_file(&path, window, cx);
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
                let Some(file) = rfd::AsyncFileDialog::new()
                    .set_title("保存文件")
                    .set_file_name(&file_name)
                    .save_file()
                    .await
                else {
                    return;
                };
                let file = file.path().to_path_buf();
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
