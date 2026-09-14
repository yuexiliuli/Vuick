//! file_ops.

use super::*;
use gpui_component::notification::{Notification, NotificationType};
use gpui_component::WindowExt as _;

use crate::config::limits::EDITOR_WRAP_MAX_LINES;
use crate::config::timing::OPEN_FAILURE_AUTOHIDE_SECS;

/// 打开失败通知的 id 类型（配合唯一 key，便于 5 秒后精确移除）。
struct OpenFailureNotification;

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
    ///
    /// `prebuilt_index`：非 None 时预览直接复用外部（后台）预建的块索引，
    /// 跳过 UI 线程上的 `scan_blocks`（大文档异步打开用）。
    fn load_file_text(
        &mut self,
        idx: usize,
        p: &std::path::Path,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
        prebuilt_index: Option<std::rc::Rc<crate::markdown::BlockIndex>>,
    ) {
        let panel_bg = self.panel_bg();
        let Some(tab) = self.tabs.get_mut(idx) else {
            return;
        };
        let lang = Self::language_id(p);
        let lines = text.lines().count().max(1);
        let chars = text.chars().count();
        let src_len = text.len();
        // Markdown 文件编辑区默认软换行（其他文件默认不换行，见 make_tab 的 wrap_enabled）
        let is_md = Self::is_markdown_path(p);
        let t_editor = std::time::Instant::now();
        tab.editor.update(cx, |ed, cx| {
            ed.set_value(text.clone(), window, cx);
            if is_md && lines <= EDITOR_WRAP_MAX_LINES {
                // 大文档不软换行：避免 DisplayMap 一次性布局整篇（见常量注释）
                ed.set_soft_wrap(true, window, cx);
            }
            ed.set_highlighter(lang, cx);
        });
        let elapsed_editor = t_editor.elapsed();
        // 渲染器按行解析，原文直接喂入即可（行级虚拟化内部做块识别）。
        // 只对 Markdown 类文件解析预览：非 md 文件预览区不展示，喂入整篇 6MB
        // 文本纯属浪费（parse_to_rows 会全量解析），拖慢打开。
        // 大文档（> PREVIEW_MAX_DOC_LINES 行）由预览内部自动切窗口级懒解析。
        // 大文档异步打开（带预建索引）：编辑器已填入内容，但挂载延后一拍——
        // 让预览先把内容画出来，编辑器 16 万行的首帧布局再放到随后帧（见 panes）。
        tab.editor_pending = prebuilt_index.is_some();
        let t_preview = std::time::Instant::now();
        tab.md_preview.update(cx, |view, cx| {
            view.set_background(panel_bg);
            view.set_base_dir(p.parent().map(|d| d.to_path_buf()));
            if is_md {
                // Markdown 默认/始终自动换行
                view.set_wrap(true, cx);
                if let Some(index) = prebuilt_index {
                    // 复用后台预建的块索引（省 UI 线程几百 ms）
                    view.set_source_with_index(&text, index, cx);
                } else {
                    view.set_source(&text, cx);
                }
            }
        });
        let elapsed_preview = t_preview.elapsed();
        if std::env::var_os("MD_OPEN_DEBUG").is_some() {
            eprintln!(
                "[MD_OPEN_DEBUG] load_file_text: 编辑器(set_value+wrap+highlight)={} ms, 预览(set_wrap+scan_blocks)={} ms",
                elapsed_editor.as_millis(),
                elapsed_preview.as_millis(),
            );
        }
        tab.file_path = Some(p.to_path_buf());
        // 普通文本/Markdown 选项卡：正文以编辑器 Rope 为准，不再整份镜像到
        // `content`（省一份全文副本，6MB 文件即省 6MB/卡）。EPUB/PDF 这类
        // `readonly_preview`（编辑区为空）才把正文存进 content。
        tab.content = SharedString::default();
        tab.line_count = lines;
        tab.char_count = chars;
        tab.cursor_row = 0;
        tab.cursor_col = 0;
        tab.dirty = false;
        // 运行时内存诊断：`MD_MEM_DEBUG=1 cargo run` 打开文件时输出各份文本副本与
        // 预览解析驻留（定位“同样内容，.md 比 .txt 内存高”到底高在哪）。
        if std::env::var_os("MD_MEM_DEBUG").is_some() {
            let content_len = tab.content.len();
            let editor_len = tab.editor.read(cx).value().len();
            let (rows, preview_bytes) = tab.md_preview.read(cx).memory_estimate();
            eprintln!(
                "[MD_MEM_DEBUG] 打开 {:?}: 源文件 {} B / {} 行 / {} 字符 → 副本 content={} B + editor={} B + 预览 {rows} 行≈{} KB",
                p.file_name(),
                src_len,
                lines,
                chars,
                content_len,
                editor_len,
                preview_bytes / 1024,
            );
        }
        if is_md {
            tab.preview_on = true;
        }
        self.status = SharedString::from(if is_md && lines > EDITOR_WRAP_MAX_LINES {
            format!("已打开：{} · 大文档编辑区已关闭软换行（Ctrl+W 可开，预览保持换行）", p.display())
        } else {
            format!("已打开：{}", p.display())
        });
        self.status_ok = true;
        cx.notify();
    }

    /// 把 EPUB 解析结果载入指定选项卡：纯展示（只读预览），编辑区置空。
    fn load_epub(
        &mut self,
        idx: usize,
        p: &std::path::Path,
        doc: crate::epub::EpubDocument,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let panel_bg = self.panel_bg();
        let Some(tab) = self.tabs.get_mut(idx) else {
            return;
        };
        let text = doc.markdown;
        let lines = text.lines().count().max(1);
        let chars = text.chars().count();
        // 编辑区置为提示文本（EPUB 只读，不参与文本编辑）
        tab.editor.update(cx, |ed, cx| {
            ed.set_value("", window, cx);
            ed.set_highlighter("markdown", cx);
        });
        tab.md_preview.update(cx, |view, cx| {
            view.set_background(panel_bg);
            view.set_base_dir(p.parent().map(|d| d.to_path_buf()));
            // EPUB 图片保持内存，渲染时由预览查表直接解码（不写硬盘）
            view.set_memory_images(doc.images.clone());
            // EPUB 阅读默认开换行（正文自然折行），fullscreen 时占满为阅读模式
            view.set_wrap(true, cx);
            view.set_source(&text, cx);
        });
        tab.file_path = Some(p.to_path_buf());
        tab.content = SharedString::from(text);
        tab.line_count = lines;
        tab.char_count = chars;
        tab.cursor_row = 0;
        tab.cursor_col = 0;
        tab.dirty = false;
        tab.preview_on = true;
        tab.preview_fullscreen = true;
        // EPUB 正文只存在于预览，编辑区为空 → 预览锁定，toggle 不翻转也不显示空编辑区
        tab.readonly_preview = true;
        let author = if doc.author.is_empty() {
            String::new()
        } else {
            format!("（{}）", doc.author)
        };
        self.status = SharedString::from(format!(
            "已打开 EPUB：{}{} · {} 章",
            doc.title, author, doc.chapter_count
        ));
        self.status_ok = true;
        cx.notify();
    }

    /// 把一个新 PDF 阅读器载入指定选项卡：编辑区置空、全屏只读预览。
    /// 解析由 [`crate::pdf::PdfViewer`] 自己在后台完成，窗口不阻塞。
    fn load_pdf(
        &mut self,
        idx: usize,
        p: &std::path::Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 阅读画布用灰色系（非白面板），让页面“纸面”卡片与画布形成分隔
        let canvas_bg = self.window_bg();
        let Some(tab) = self.tabs.get_mut(idx) else {
            return;
        };
        // 编辑区置为提示文本（PDF 只读，不参与文本编辑）
        tab.editor.update(cx, |ed, cx| {
            ed.set_value("", window, cx);
            ed.set_highlighter("markdown", cx);
        });
        tab.md_preview.update(cx, |view, cx| {
            view.set_background(canvas_bg);
            view.set_source("", cx);
        });
        // PdfViewer：自包含阅读器（异步读文件+解析+页级按需栅格化，不解析为 Markdown）
        let viewer = cx.new(|cx| crate::pdf::PdfViewer::new(p.to_path_buf(), cx));
        viewer.update(cx, |view, _| view.set_background(canvas_bg));
        tab.pdf_preview = Some(viewer);
        tab.file_path = Some(p.to_path_buf());
        tab.content = SharedString::default();
        tab.line_count = 0;
        tab.char_count = 0;
        tab.cursor_row = 0;
        tab.cursor_col = 0;
        tab.dirty = false;
        tab.preview_on = true;
        tab.preview_fullscreen = true;
        // PDF 正文只存在于阅读器，编辑区为空 → 预览锁定，toggle 不翻转也不显示空编辑区
        tab.readonly_preview = true;
        self.status = SharedString::from(format!("已打开 PDF：{}（解析中…）", p.display()));
        self.status_ok = true;
        cx.notify();
    }

    /// 打开 PDF：立即建标签页并创建自包含的 PdfViewer（在后台加载解析），
    /// 窗口保持响应；解析完成/失败由阅读器内部呈现。
    fn open_pdf_async(
        &mut self,
        idx: usize,
        p: std::path::PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.status = SharedString::from(format!("正在打开 PDF：{}…", p.display()));
        self.status_ok = true;
        self.load_pdf(idx, &p, window, cx);
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
        if Self::is_epub_path(&p) {
            match crate::epub::parse_epub_file(&p) {
                Ok(doc) => self.load_epub(idx, &p, doc, window, cx),
                Err(e) => {
                    self.status = SharedString::from(format!("打开失败：{e}"));
                    self.status_ok = false;
                    cx.notify();
                }
            }
            return;
        }
        if Self::is_pdf_path(&p) {
            self.open_pdf_async(idx, p.clone(), window, cx);
            return;
        }
        match fs::read_to_string(&p) {
            Ok(text) => {
                let text = Self::normalize_newlines(&text);
                self.load_file_text(idx, &p, text, window, cx, None);
            }
            Err(e) => self.report_open_failure(&p, &e, window, cx),
        }
    }

    /// 打开文件：新建一个文件选项卡并在其中载入文件内容，随后把焦点移到新选项卡的编辑区。
    ///
    /// 先确认内容可读/可解析再创建选项卡：读取失败时只给出错误提示，
    /// 不再生成一个内容空白的标签页。
    pub(super) fn open_file(&mut self, path: &str, window: &mut Window, cx: &mut Context<Self>) {
        let p = PathBuf::from(path.trim());
        // EPUB / PDF：先生成解析结果，失败则仅提示、不建空标签页
        if Self::is_epub_path(&p) {
            match crate::epub::parse_epub_file(&p) {
                Ok(doc) => {
                    let idx = self.make_tab(window, cx);
                    self.load_epub(idx, &p, doc, window, cx);
                    if let Some(tab) = self.tabs.get(idx) {
                        let focus = tab.editor.read(cx).focus_handle(cx);
                        window.focus(&focus, cx);
                    }
                }
                Err(e) => {
                    self.status = SharedString::from(format!("打开失败：{e}"));
                    self.status_ok = false;
                    cx.notify();
                }
            }
            return;
        }
        if Self::is_pdf_path(&p) {
            // 先建标签页、立即返回：解析放到后台，避免拖窗卡顿
            let idx = self.make_tab(window, cx);
            self.open_pdf_async(idx, p.clone(), window, cx);
            if let Some(tab) = self.tabs.get(idx) {
                let focus = tab.editor.read(cx).focus_handle(cx);
                window.focus(&focus, cx);
            }
            return;
        }
        // 文本：先读成功再建 tab
        let t_read = std::time::Instant::now();
        let text = match fs::read_to_string(&p) {
            Ok(text) => Self::normalize_newlines(&text),
            Err(e) => {
                self.report_open_failure(&p, &e, window, cx);
                return;
            }
        };
        if std::env::var_os("MD_OPEN_DEBUG").is_some() {
            eprintln!(
                "[MD_OPEN_DEBUG] 读文件+归一化: {} ms ({} B)",
                t_read.elapsed().as_millis(),
                text.len()
            );
        }
        let lines = text.lines().count().max(1);
        // 大文档（md 且超编辑区软换行阈值）：**异步打开**——先建标签页 + “正在打开…”
        // 状态立即返回，read/scan_blocks 已在上面完成读、预览索引放后台线程算，
        // 完成后在主线程一次性装配编辑器与预览。拖入瞬间不再卡死 UI。
        if Self::is_markdown_path(&p) && lines > EDITOR_WRAP_MAX_LINES {
            let idx = self.make_tab(window, cx);
            self.status =
                SharedString::from(format!("正在打开大文档：{}…", p.display()));
            self.status_ok = true;
            let entity = cx.entity();
            let p2 = p.clone();
            // 后台线程预建预览块索引（纯函数，几百 ms，不阻塞 UI）
            let src_for_scan = text.clone();
            window
                .spawn(cx, async move |cx| {
                    let index = cx
                        .background_executor()
                        .spawn(async move { crate::markdown::scan_blocks(&src_for_scan) })
                        .await;
                    eprintln!(
                        "[MD_OPEN_DEBUG] 后台 scan_blocks 完成: {} 块",
                        index.blocks.len()
                    );
                    let _ = cx.update_window_entity(&entity, move |this, window, cx| {
                        this.load_file_text(
                            idx,
                            &p2,
                            text,
                            window,
                            cx,
                            Some(std::rc::Rc::new(index)),
                        );
                        if let Some(tab) = this.tabs.get(idx) {
                            let focus = tab.editor.read(cx).focus_handle(cx);
                            window.focus(&focus, cx);
                        }
                    });
                })
                .detach();
            return;
        }
        // 小文档：同步打开（原路径）
        let t_mk = std::time::Instant::now();
        let idx = self.make_tab(window, cx);
        if std::env::var_os("MD_OPEN_DEBUG").is_some() {
            eprintln!("[MD_OPEN_DEBUG] make_tab: {} ms", t_mk.elapsed().as_millis());
        }
        let t_load = std::time::Instant::now();
        self.load_file_text(idx, &p, text, window, cx, None);
        if std::env::var_os("MD_OPEN_DEBUG").is_some() {
            eprintln!(
                "[MD_OPEN_DEBUG] load_file_text: {} ms",
                t_load.elapsed().as_millis()
            );
        }
        if let Some(tab) = self.tabs.get(idx) {
            let t_focus = std::time::Instant::now();
            let focus = tab.editor.read(cx).focus_handle(cx);
            window.focus(&focus, cx);
            if std::env::var_os("MD_OPEN_DEBUG").is_some() {
                eprintln!(
                    "[MD_OPEN_DEBUG] focus 编辑器: {} ms",
                    t_focus.elapsed().as_millis()
                );
            }
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
        // 若侧边栏搜索框持有焦点 → 粘贴到搜索框（其不处理应用级 PasteNormalized）
        let search_focused = window
            .focused(cx)
            .is_some_and(|fh| fh == self.search_state.read(cx).focus_handle(cx));
        if search_focused {
            let search = self.search_state.clone();
            search.update(cx, |s, cx| {
                s.replace(text, window, cx);
            });
            return;
        }
        let Some(tab) = self.tabs.get(self.active_tab) else {
            return;
        };
        tab.editor.update(cx, |editor, cx| {
            editor.replace(text, window, cx);
        });
    }
}
