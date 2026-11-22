//! panes（预览渲染半在 `shell_preview`）。
//!
//! 本模块只保留**编辑区 / 分割条**宿主半：`render_editor_pane`（编辑区 + WYSIWYG 活区
//! 左面板接入）、`render_plain_editor`（大文档占位与冻结）、
//! `render_splitter`（分屏分割线拖拽）。预览渲染半（`render_preview_pane` /
//! `render_fullscreen_preview` 的 md_wy 分支、`md_wy_host_children` / `preview_element` /
//! `render_live_area` / `zoom_overlay` 元素构建）在 `shell_preview`，宿主经薄适配调用；
//! 媒体/PDF 查看器渲染分支（`md_wy_preview()==None`，依赖 `sync_active_media_meta` 等宿主
//! 媒体域）留本模块。

use super::*;

/// WYSIWYG 宿主 `#live-body` 的顶部内衬（px）：缩放锚点在做「宿主 ScrollHandle 偏移 ↔
/// WyEditor 元素局部 y」换算时使用，必须与各宿主 `.py(px(LIVE_BODY_PAD_Y))` 保持一致。
/// 定义在 `shell_core::LIVE_BODY_PAD_Y`，此处重导出维持 `app::panes::…` 路径
///（`app/tests/scroll_sync.rs` 断言读；生产构建不再直接消费 → 允许未使用）。
#[allow(unused_imports)]
pub(crate) use shell_core::LIVE_BODY_PAD_Y;

impl EditorApp {
    pub(super) fn render_editor_pane(
        &mut self,
        window: &mut Window,
        grow: f32,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        // PDF / EPUB 等只读预览文档不生成编辑区（即使处于分屏/编辑-only 布局也直接跳过）。
        let readonly = self
            .model.borrow().tabs
            .get(self.model.borrow().active_tab)
            .map(|t| t.payload.readonly_preview)
            .unwrap_or(false);
        if readonly {
            return div().id("editor-area").size_full().into_any_element();
        }
        // P1 大文档整篇仍在后台装配：编辑区已有首块内容但**锁输入**（全区域透明拦截，
        // 防「打开期间编辑」与 EOF 整篇换装竞态）；`finish_file_text` 换装后同帧解锁。
        let active = self.model.borrow().active_tab;
        if self.model.borrow().tabs[active].payload.loading_lock {
            return self.render_plain_editor(grow, true, cx);
        }
        // 所见即所得预览编辑（WYSIWYG）模式：左面板 = 本项目 `WyEditor`
        // （AUTO-GROW，内部无滚动条，宿主负责滚动 + 自绘纵向滚动条）。
        // 先取出 live2 实体（owned）再 `if let`，避免 `if let` 掮获 `borrow()` 临时
        // Ref 的生命周期跨过块内 `&mut self` 调用（E0502 / 借用约束收窄）。
        let live2 = self.model.borrow().tabs[active].live2.clone();
        if let Some(ed) = live2 {
            // live2 未就绪（大文档后台装配在途）：**冻结源码编辑区** + 全区域透明输入
            // 拦截层——窗口期内不整白屏等后台（原白屏根因），也不在空壳上画空白帧。
            // 就绪后下一帧自动切入 `render_live_area`（首帧即真内容，靠既有预建槽）。
            if !ed.read(cx).is_loaded() {
                return self.render_plain_editor(grow, true, cx);
            }
            return self.render_live_area(&ed, window, grow, cx);
        }
        self.render_plain_editor(grow, false, cx)
    }

    /// WYSIWYG（live2）左面板宿主接入（元素构建在 `shell_preview::render::render_live_area`；
    /// 宿主只取主题色 + 状态，行为与迁移前逐字一致）。
    fn render_live_area(
        &mut self,
        ed: &gpui::Entity<WyEditor>,
        window: &mut Window,
        grow: f32,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let model = self.model.clone();
        let panel_bg = self.panel_bg(cx);
        let text_primary = self.text_primary(cx);
        shell_preview::render::render_live_area(
            &model, ed, window, grow, cx, panel_bg, text_primary,
        )
    }

    /// 普通源码编辑区（非只读 / 非挂载占位 / 非 WYSIWYG）。
    /// `freeze_input=true`：live2 未就绪冻结展示——追加全区域透明输入拦截层。
    fn render_plain_editor(
        &mut self,
        grow: f32,
        freeze_input: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let editor = self.model.borrow().tabs[self.model.borrow().active_tab].payload.editor.clone();
        // 字号覆盖（用户 Ctrl+=/- 调节；None=主题默认）。渲染时由 CodeEditor 自身读取。
        let font_size = self.model.borrow().font_size;
        editor.update(cx, |ed, _| ed.set_font_override(Some(font_size)));
        let mut el = div()
            .id("editor-area")
            .debug_selector(|| "editor-area".into())
            .relative()
            // grow = 编辑区占宽比例（无预览时为 1）
            .flex_1()
            .flex_grow(grow)
            .min_w(px(0.))
            .min_h(px(0.))
            .bg(self.panel_bg(cx))
            // 无边框、无圆角：编辑区占满整块区域，不留白
            .overflow_hidden()
            // 从资源管理器拖入文件 → 打开（编辑器自身不接收拖放）
            .can_drop(|drag: &dyn Any, _w, _c| drag.downcast_ref::<ExternalPaths>().is_some())
            .on_drop(cx.listener(
                |this: &mut EditorApp, paths: &ExternalPaths, window, cx| {
                    this.open_dropped_paths(paths, window, cx);
                },
            ))
            .child(editor)
            // 透明覆盖层（画在编辑器之上，命中测试最靠前)：拦截 Ctrl+滚轮缩放。
            .child(Self::zoom_overlay(cx));
        // live2 未就绪冻结期追加**全区域输入拦截层**（画在最上、吞鼠标/滚轮）：防
        // 「窗口期内输入进源码」与后台解析快照分歧——源码此刻是权威源（live2 尚无内容），
        // 只读展示；就绪后本分支不再渲染，自然消失。
        if freeze_input {
            el = el.child(Self::live_pending_input_overlay());
        }
        el.into_any_element()
    }

    /// live2 未就绪期间的全区域**透明输入拦截层**（元素构建在
    /// [`shell_preview::render::live_pending_input_overlay`]）。
    fn live_pending_input_overlay() -> gpui::AnyElement {
        shell_preview::render::live_pending_input_overlay()
    }

    /// 编辑区 / 预览区共用的 Ctrl+滚轮缩放覆盖层（元素构建在
    /// [`shell_preview::render::zoom_overlay`]；缩放经 `PreviewHost::preview_change_font_size`
    /// 落宿主 `change_font_size`）。
    fn zoom_overlay(cx: &mut Context<Self>) -> gpui::AnyElement {
        shell_preview::render::zoom_overlay(cx)
    }

    /// 编辑区 / 预览区之间的分割线（可拖拽调整分屏比例）。
    ///
    /// 无缝直连：6px 命中带 + 居中 2px 细竖线——常态 `bar_border`，hover/拖拽时换
    /// `drag_hover_border` 强调色；命中区 / 拖拽逻辑完全不变（`w(6)` 命中带 +
    /// 容器 `on_mouse_move/up` 换算分屏比例）。
    pub(super) fn render_splitter(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let border = self.bar_border(cx);
        let drag_line: gpui::Hsla =
            ui::theme::Theme::global(cx).colors.drag_hover_border.into();
        let line_color: gpui::Hsla = if self.panes.splitter_hovered || self.panes.split_drag.is_some()
        {
            drag_line
        } else {
            border
        };
        div()
            .id("splitter")
            .debug_selector(|| "splitter".into())
            .w(px(6.))
            .h_full()
            .flex_none()
            .cursor(CursorStyle::ResizeColumn)
            .bg(gpui::transparent_black())
            .flex()
            .items_stretch()
            .justify_center()
            // 命中测试加宽（左右各 3px），视觉只有中间 2px 细线
            .on_hover(cx.listener(
                |this: &mut EditorApp, hovered: &bool, _window, cx| {
                    this.panes.splitter_hovered = *hovered;
                    cx.notify();
                },
            ))
            .on_mouse_down(MouseButton::Left, cx.listener(
                |this: &mut EditorApp, ev: &gpui::MouseDownEvent, _window, cx| {
                    this.panes.split_drag = Some((ev.position.x.as_f32(), this.panes.split_ratio));
                    cx.notify();
                },
            ))
            .child(
                div()
                    .h_full()
                    .w(px(2.))
                    .flex_none()
                    .rounded(px(1.))
                    .bg(line_color),
            )
    }

    /// 当前选项卡的预览视图（右栏分屏），**md_wy 分支**转域
    /// [`shell_preview::render::md_wy_host_children`]；媒体/PDF 自包含阅读器分支（依赖
    /// 宿主媒体域 `preview_element` / `zoom_overlay`）留本模块。
    pub(super) fn render_preview_pane(
        &mut self,
        window: &mut Window,
        grow: f32,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        // A2：Markdown 右栏预览从 `MarkdownPreview`（元素树）切到统一组件 WyEditor
        //（只读）——宿主滚动（复用 `set_host_scroll` / `prepare_font_zoom`）。宿主结构
        // 对齐左栏活区 `render_live_area`（单一块容器 + `.overflow_hidden()` + 滚动容器/
        // 叠层为直接子件），避免旧包裹（flex-row + 内层相对块）在 gpui 测量时为
        // WyEditor 交替提供 definite 宽度（800↔388/440），导致帧缓存每键 ×7 重建。
        if self.md_wy_preview().is_some() {
            let panel_bg = self.panel_bg(cx);
            let text_primary = self.text_primary(cx);
            let text_minor = self.text_minor(cx);
            return shell_preview::render::md_wy_host_children(
                &self.model, window, cx, false, grow, panel_bg, text_primary, text_minor,
            );
        }
        // PDF / 图片 / 视频（自包含阅读器）。
        let mut preview_pane = div()
            .id("preview-pane")
            .flex_1()
            .min_w(px(0.))
            .bg(self.panel_bg(cx))
            .relative()
            // PDF → PdfViewer（整页原样）；其余 → Markdown 渲染器。
            // Entity 直接作为子元素渲染（内部 size_full）。
            .child(shell_preview::render::preview_element(
                &self.model.borrow().tabs[self.model.borrow().active_tab],
            ));
        // 自包含阅读器（PDF/图片/视频）各自处理滚轮：不再挂全局 zoom_overlay（对齐
        // `render_fullscreen_preview` 同一口径）。否则绝对定位的覆盖层（命中测试最靠前）
        // 会在 Ctrl+滚轮时**先于阅读器**消费并 `stop_propagation`，媒体自身 Ctrl+滚轮
        // 缩放失效——普通滚轮则被不拦截地透传到阅读器。仅非阅读器占位（无 md_wy 的
        // 文本/纯文本等）保留全局字号入口。
        if !self.model.borrow().tabs[self.model.borrow().active_tab].payload.reader.is_reader() {
            preview_pane = preview_pane.child(Self::zoom_overlay(cx));
        }
        div()
            .flex()
            .flex_row()
            // grow = 预览区占宽比例
            .flex_grow(grow)
            .flex_shrink_1()
            .flex_basis(px(0.))
            .min_w(px(0.))
            // 预览区也可接收文件拖放
            .can_drop(|drag: &dyn Any, _w, _c| drag.downcast_ref::<ExternalPaths>().is_some())
            .on_drop(cx.listener(
                |this: &mut EditorApp, paths: &ExternalPaths, window, cx| {
                    this.open_dropped_paths(paths, window, cx);
                },
            ))
            .child(preview_pane)
            .into_any_element()
    }

    /// 全屏预览（EPUB / PDF 阅读模式）：渲染占满整个内容区，不显示编辑区/分割线。
    /// **md_wy 分支**转域 [`shell_preview::render::md_wy_host_children`]；媒体/PDF 自包含
    /// 阅读器（`sync_active_media_meta` 标题/状态栏同步 + `preview_element`）留本模块。
    pub(super) fn render_fullscreen_preview(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        if self.md_wy_preview().is_some() {
            // A2：Markdown 全屏预览同样切统一组件 WyEditor（宿主滚动；结构与右栏同源）。
            let panel_bg = self.panel_bg(cx);
            let text_primary = self.text_primary(cx);
            let text_minor = self.text_minor(cx);
            return shell_preview::render::md_wy_host_children(
                &self.model, window, cx, true, 1.0, panel_bg, text_primary, text_minor,
            );
        }
        // 媒体查看器（图片/视频）：切换/解码完成后把标签标题与状态栏跟到当前媒体
        //（渲染期幂等同步，无媒体/路径未变则跳过）。
        if self.sync_active_media_meta(cx) {
            cx.notify();
        }
        // 全屏阅读（PDF / 图片 / 视频）：自包含阅读器自处理缩放，无需全局字号推进。
        let panel_bg = self.panel_bg(cx);
        let m = self.model.borrow();
        let tab = &m.tabs[m.active_tab];
        let mut el = div()
            .id("fullscreen-preview")
            .debug_selector(|| "fullscreen-preview".into())
            .size_full()
            .relative()
            .bg(panel_bg)
            .child(shell_preview::render::preview_element(tab));
        // 图片/视频查看器：自处理滚轮（图片：单滚切换 / Ctrl+滚轮缩放），不再挂全局
        // zoom_overlay（否则 Ctrl+滚轮会被全局字号缩放抢走，媒体缩放失效）。PDF 自身
        // 已有 Ctrl+滚轮缩放 → 与图片/视频同归「媒体自处理」，同样不挂；仅非阅读器占位
        // （无 md_wy 的文本/纯文本等）保留全局字号入口。
        if !tab.payload.reader.is_reader() {
            el = el.child(Self::zoom_overlay(cx));
        }
        el.into_any_element()
    }
}
