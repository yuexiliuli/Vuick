//! 跨窗口选项卡搬迁与拖拽：从本窗口「拔出」/收纳选项卡、拖出窗口拆新窗口 /
//! 还原回主窗口（多窗口 split/tear-off），以及全局 `EditorAppRegistry` 的读写。

use super::*;

impl EditorApp {
    /// 从本窗口「拔出」一个选项卡：移除选项卡（**不**释放 PDF 纹理、不重置编辑器——
    /// 实体与元数据整体移到目标窗口），下标与激活态自动校正。
    /// 整结构 move：一次解构 `EditorTab` 搬到 `DetachedTab`（`..` 丢弃源窗口专属的
    /// live 态 / 订阅），不再逐字段手抄。
    pub(super) fn take_tab_payload(&mut self, idx: usize) -> Option<DetachedTab> {
        if idx >= self.tabs.len() {
            return None;
        }
        let tab = self.tabs.remove(idx);
        if idx < self.active_tab {
            self.active_tab -= 1;
        } else if self.active_tab >= self.tabs.len() {
            self.active_tab = self.tabs.len().saturating_sub(1);
        }
        self.search_scan = None; // 选项卡集合变化 → 搜索缓存失效
        let EditorTab {
            editor,
            reader,
            md_wy,
            md_wy_vscroll,
            md_wy_install,
            epub_images,
            epub_notes,
            file_path,
            dirty,
            source_encoding,
            content,
            line_count,
            char_count,
            cursor_row,
            cursor_col,
            preview_on,
            preview_fullscreen,
            readonly_preview,
            editor_pending,
            loading_lock,
            edit_version,
            ..
        } = tab;
        Some(DetachedTab {
            editor,
            reader,
            md_wy,
            md_wy_vscroll,
            md_wy_install,
            epub_images,
            epub_notes,
            file_path,
            dirty,
            source_encoding,
            content,
            line_count,
            char_count,
            cursor_row,
            cursor_col,
            preview_on,
            preview_fullscreen,
            readonly_preview,
            editor_pending,
            loading_lock,
            edit_version,
        })
    }

    /// 移出选项卡前的统一收尾（live2 守卫下沉）：实时预览编辑实体不随选项卡搬迁——
    /// 先 `commit_live` 把 live2 写回编辑区再取载荷。两处拖拽移出路径（`release_tab_drag_outside`
    /// 窗口外松开 / `tear_off_drag` 越过窗口缘）此前各自手抄「`live_edit` → `commit_live`」守卫，
    /// 收敛为此 helper；`take_tab_payload` 本身保持无副作用（测试直调、不隐含 commit）。
    pub(super) fn commit_live_and_take_tab_payload(
        &mut self,
        idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<DetachedTab> {
        if self.tabs[idx].live_edit {
            self.commit_live(idx, window, cx);
        }
        self.take_tab_payload(idx)
    }

    /// 把一个「拔出」的选项卡收纳进本窗口：共享编辑器/预览实体、重建编辑订阅、
    /// 置为激活并同步预览/PDF 主题与软换行偏好。
    /// `window=None` 用于跨窗口还原（此时不抢焦点，由调用方在目标窗口聚焦）；
    /// `Some(window)` 用于拆出新窗口（直接聚焦新窗口的编辑区）。
    /// 整结构 move：一次解构 `DetachedTab` 搬回 `EditorTab`（live 态/订阅置目标窗口
    /// 缺省值），不再逐字段手抄。
    pub(crate) fn adopt_tab(
        &mut self,
        payload: DetachedTab,
        mut window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) {
        let DetachedTab {
            editor,
            reader,
            md_wy,
            md_wy_vscroll,
            md_wy_install,
            epub_images,
            epub_notes,
            file_path,
            dirty,
            source_encoding,
            content,
            line_count,
            char_count,
            cursor_row,
            cursor_col,
            preview_on,
            preview_fullscreen,
            readonly_preview,
            editor_pending,
            loading_lock,
            edit_version,
        } = payload;
        let edit_sub = self.subscribe_editor(&editor, cx);
        let is_md = file_path
            .as_ref()
            .map(|p| Self::is_markdown_path(p))
            .unwrap_or(false);
        let idx = self.tabs.len();
        self.tabs.push(EditorTab {
            editor,
            file_path,
            dirty,
            source_encoding,
            content,
            line_count,
            char_count,
            cursor_row,
            cursor_col,
            reader,
            md_wy,
            md_wy_vscroll,
            md_wy_install,
            epub_images,
            epub_notes,
            preview_on,
            preview_fullscreen,
            readonly_preview,
            editor_pending,
            loading_lock,
            edit_version,
            // 预览编辑（WYSIWYG）不随选项卡搬迁（搬迁前已提交写回，见 take_tab_payload 调用点）
            live_edit: false,
            live2: None,
            live2_dirty: false,
            _live2_sub: None,
            live_vscroll: gpui::ScrollHandle::new(),
            _edit_sub: edit_sub,
            // PDF 阅读器订阅随选项卡重建（源窗口的 `_pdf_sub` 已随 take_tab_payload 丢弃）
            _pdf_sub: None,
        });
        self.active_tab = idx;
        self.search_scan = None;
        // 编辑区软换行与目标窗口偏好对齐（md 默认换行、预览开着强制换行）
        let wrap = is_md || self.wrap_enabled || self.tabs[idx].preview_on;
        self.tabs[idx].editor.update(cx, |ed, _cx| {
            ed.set_soft_wrap(wrap);
        });
        // 预览/PDF 主题背景刷新 + PDF 订阅重建
        let canvas_bg = self.window_bg();
        // P4-2：MD / EPUB 预览都走统一 WyEditor 只读宿主——实体随选项卡搬迁已带内容，
        // 无需再喂 MarkdownPreview（该后端已删除）。
        match self.tabs[idx].reader.clone() {
            MediaReader::Pdf(pdf) => {
                pdf.update(cx, |view, _| view.set_background(canvas_bg));
                // PDF 订阅随选项卡重建（换位后订阅跟 tab 走；多 PDF 并存各自独立）
                self.subscribe_pdf(idx, &pdf, cx);
            }
            MediaReader::Image(img) => {
                img.update(cx, |view, _| view.set_background(canvas_bg));
            }
            #[cfg(feature = "video")]
            MediaReader::Video(vid) => {
                vid.update(cx, |view, _| view.set_background(canvas_bg));
            }
            MediaReader::None => {}
        }
        self.pdf_search = None; // 重新推送检索查询
        if let Some(window) = window.as_mut() {
            self.activate_tab(idx, window, cx);
            self.status = SharedString::from("已拆分为独立窗口");
            // 窗口标题跟随当前文件（拆出的新窗口直接以此命名）
            if let Some(p) = self.tabs[idx].file_path.as_ref() {
                window.set_window_title(&p.to_string_lossy());
            } else {
                window.set_window_title("未命名");
            }
        } else {
            self.status = SharedString::from("已还原到主窗口");
        }
        self.status_ok = true;
        // 收纳后选项卡集合变化 → 立即持久化（detached 窗口由 persist 内部早退）。
        self.schedule_session_flush(cx, true);
        cx.notify();
    }

    /// 拖拽中的选项卡在**窗口外**松开：主窗口 → 拆成独立窗口；独立窗口 → 还原回主窗口。
    /// `window.remove_window()`（独立窗口还原后），标志窗口本帧结束后销毁。
    pub(crate) fn release_tab_drag_outside(
        &mut self,
        window: &mut Window,
        release_position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let Some(idx) = self.tab_dragging.take() else {
            return;
        };
        if idx >= self.tabs.len() {
            return;
        }
        // 实时预览编辑模式下鼠标按住拖出：先把内容写回编辑区（实时实体不随选项卡搬）。
        let Some(payload) = self.commit_live_and_take_tab_payload(idx, window, cx) else {
            return;
        };
        if self.is_detached {
            // 独立文件窗口：把选项卡还给主窗口（找不到主窗口则自我转正为主窗口）
            self.restore_tab_to_primary(payload, cx);
            window.remove_window();
        } else {
            // 主窗口：把选项卡拆进新开的独立窗口（窗口出现在鼠标释放处）
            self.split_tab_to_new_window(payload, release_position, window, cx);
        }
    }

    /// 拖拽中光标**第一次越过窗口边缘** → 立即把选项卡拆进光标处的新窗口（浏览器式
    /// tear-off），让「拖出窗口」有连续过渡，而不是松开后才突然弹出独立窗口。
    /// 独立文件窗口不在此拆分：它保持「拖到选项卡栏松开 = 还原回主窗口」的语义。
    pub(super) fn tear_off_drag(
        &mut self,
        idx: usize,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.is_detached {
            return;
        }
        if idx >= self.tabs.len() {
            return;
        }
        // 实时预览编辑模式下 tear-off：先写回（实时实体不随选项卡搬）。
        let Some(payload) = self.commit_live_and_take_tab_payload(idx, window, cx) else {
            return;
        };
        self.tab_dragging = None;
        // 结束旧窗口的拖拽：幽灵不再画在旧窗口，也不会被新窗口的首帧绘制出来。
        let _ = cx.stop_active_drag(window);
        self.split_tab_to_new_window(payload, position, window, cx);
    }

    /// 把选项卡拆进一个新开的独立窗口：窗口左上角跟随鼠标释放位置（屏幕坐标 =
    /// 源窗口屏幕左上角 + 拖放点的窗口内坐标）；本窗口落下最后一个选项卡（`tabs`
    /// 变空）即回到零选项卡「欢迎页」——不再补空选项卡（与关闭最后一项语义一致）。
    pub(super) fn split_tab_to_new_window(
        &mut self,
        payload: DetachedTab,
        release_position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 主窗口剩下的选项卡接管焦点；零选项卡时 `activate_tab` 空返回（欢迎页）。
        self.activate_tab(self.active_tab, window, cx);
        let src_bounds = window.bounds();
        let bounds = Bounds {
            origin: src_bounds.origin + release_position,
            size: src_bounds.size,
        };
        let titlebar_options = gpui::TitlebarOptions {
            title: Some("GPUI 编辑器".into()),
            appears_transparent: true,
            ..Default::default()
        };
        let _ = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(titlebar_options),
                ..Default::default()
            },
            |new_window, cx| {
                let view = cx.new(|cx| EditorApp::new_detached(new_window, cx, payload));
                cx.new(|cx| ui::root::Root::new(view, new_window, cx))
            },
        );
        cx.notify();
    }

    /// 把选项卡还原到主（非 detached）窗口；找不到主窗口时把本窗口转正为主窗口并原地收纳。
    pub(super) fn restore_tab_to_primary(&mut self, payload: DetachedTab, cx: &mut Context<Self>) {
        let registry = cx.global::<EditorAppRegistry>();
        let primary: Option<(gpui::Entity<EditorApp>, AnyWindowHandle)> = registry
            .windows
            .iter()
            .filter_map(|(weak, wh)| {
                let entity = weak.upgrade()?;
                let detached = entity.read_with(cx, |app, _| app.is_detached);
                (!detached).then_some((entity, *wh))
            })
            .next();
        let focus_handle = payload.editor.read(cx).focus_handle();
        match primary {
            Some((entity, wh)) => {
                // 把选项卡交给主窗口（数据收纳 + 不抢焦点），随后在主窗口里聚焦编辑区
                entity.update(cx, |app, cx| app.adopt_tab(payload, None, cx));
                let _ = wh.update(cx, |_root, window, cx| {
                    window.focus(&focus_handle, cx);
                });
            }
            None => {
                // 主窗口已不存在（例如被关闭）→ 本窗口转正为主窗口，选项卡原地保留
                self.is_detached = false;
                self.adopt_tab(payload, None, cx);
            }
        }
    }
}
