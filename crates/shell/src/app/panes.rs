//! panes.

use super::*;
use ui::scrollbar::{Scrollbar, ScrollbarMode};

/// WYSIWYG 宿主 `#live-body` 的顶部内衬（px）：缩放锚点在做「宿主 ScrollHandle 偏移 ↔
/// WyEditor 元素局部 y」换算时使用，必须与下方 `.py(px(LIVE_BODY_PAD_Y))` 保持一致。
pub(crate) const LIVE_BODY_PAD_Y: f32 = 8.0;

impl EditorApp {
    /// 当前选项卡的预览视图：图片选项卡用 `ImageViewer`（自处理滚轮），
    /// 视频选项卡用 `VideoViewer`（M1 首帧静态阅读），PDF 选项卡用 `PdfViewer`
    ///（整页原样），其余（MD / EPUB）用统一 WyEditor 只读宿主（M5-B5 P4-2：
    /// `MarkdownPreview` 已删）。预览未创建 → 空元素兜底。
    fn preview_element(tab: &EditorTab) -> gpui::AnyElement {
        match &tab.reader {
            MediaReader::Image(img) => img.clone().into_any_element(),
            #[cfg(feature = "video")]
            MediaReader::Video(vid) => vid.clone().into_any_element(),
            MediaReader::Pdf(pdf) => pdf.clone().into_any_element(),
            MediaReader::None => tab
                .md_wy
                .clone()
                .map(|w| w.into_any_element())
                .unwrap_or_else(|| gpui::div().into_any_element()),
        }
    }

    pub(super) fn render_editor_pane(
        &mut self,
        window: &mut Window,
        grow: f32,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        // PDF / EPUB 等只读预览文档不生成编辑区（即使处于分屏/编辑-only 布局也直接跳过）。
        let readonly = self
            .tabs
            .get(self.active_tab)
            .map(|t| t.readonly_preview)
            .unwrap_or(false);
        if readonly {
            return div().id("editor-area").size_full().into_any_element();
        }
        // 大文档异步打开：编辑器内容已就绪，但让预览先把内容画出来。
        // 这一帧渲染轻量占位（不含 16 万行编辑器 → 布局轻快），并复位标记请求
        // 下一帧挂载编辑器；编辑器的首帧 DisplayMap 布局（可能冻结一两秒）
        // 因此排在用户看到预览内容之后，而不是挡在最前面。
        if self.tabs[self.active_tab].editor_pending {
            self.tabs[self.active_tab].editor_pending = false;
            cx.notify();
            return self.render_editor_pending(grow);
        }
        // 所见即所得预览编辑（WYSIWYG）模式：左面板 = 本项目 `WyEditor`
        // （AUTO-GROW，内部无滚动条，宿主负责滚动 + 自绘纵向滚动条）。
        if let Some(ed) = self.tabs[self.active_tab].live2.clone() {
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

    /// 大文档异步打开中的编辑区轻量占位（与正式编辑区**同一套 flex 布局参数**——
    /// `flex_1/grow/min_w(0)/overflow_hidden`，否则 `.size_full()` 在分屏 flex 行里会占满
    /// 整行，把预览顶出窗口）。
    fn render_editor_pending(&self, grow: f32) -> gpui::AnyElement {
        let panel_bg = self.panel_bg();
        let muted = self.text_minor();
        div()
            .id("editor-area")
            .debug_selector(|| "editor-area".into())
            .flex_1()
            .flex_grow(grow)
            .min_w(px(0.))
            .min_h(px(0.))
            .overflow_hidden()
            .flex()
            .items_center()
            .justify_center()
            .bg(panel_bg)
            .child(
                div()
                    .text_color(muted)
                    .text_size(px(13.))
                    .child("正在载入编辑区…（预览已就绪）"),
            )
            .into_any_element()
    }

    /// 统一 WyEditor 宿主驱动（live2 左面板 / 右栏预览 / 全屏预览共用）：渲染期注入
    /// 宿主滚动句柄 + 内容顶部内衬，推入当前视口顶部（元素局部 y = 滚动偏移 − 顶部
    /// 内衬），再 `prepare_font_zoom`——字号变化时 WyEditor 在 render 期按新字号重建帧
    /// 并把锚点块的精确位置写回宿主滚动句柄（prepaint 读偏移之前完成），
    /// Ctrl+滚轮缩放**无缝**、不带动滚动位置（收敛自驱动在 WyEditor 元素 prepaint）。
    fn drive_wy_host(
        &self,
        ed: &gpui::Entity<markdown_wysiwyg::WyEditor>,
        vhand: &gpui::ScrollHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let fs = self.font_size;
        let local_top = (-vhand.offset().y).max(gpui::px(0.)).as_f32() - LIVE_BODY_PAD_Y;
        ed.update(cx, |ed, _| {
            ed.set_host_scroll(Some(vhand.clone()), LIVE_BODY_PAD_Y);
            ed.set_viewport_top(local_top); // 用缩放前旧顶捕捉锚点
            ed.prepare_font_zoom(gpui::px(fs), window);
        });
    }

    /// WYSIWYG（live2）左面板：宿主滚动容器 + 自绘纵向滚动条 + Ctrl+滚轮缩放。
    /// 渲染期把全局字号推进到 WYSIWYG，并做**缩放视口锚定**（对齐 view / editor）：
    /// 注入宿主滚动句柄 + 内容顶部内衬，推入当前视口顶部（元素局部 y = scroll 偏移 −
    /// 顶部内衬），再 `prepare_font_zoom`——字号变化时 WyEditor 在 render 期按新字号
    /// 重建帧并把锚点块的精确位置写回宿主滚动句柄（prepaint 读偏移之前完成），
    /// Ctrl+滚轮缩放**无缝**、不带动滚动位置。
    fn render_live_area(
        &mut self,
        ed: &gpui::Entity<markdown_wysiwyg::WyEditor>,
        window: &mut Window,
        grow: f32,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let vhand = self.tabs[self.active_tab].live_vscroll.clone();
        self.drive_wy_host(ed, &vhand, window, cx);
        let live_any = ed.clone().into_any_element();
        let text_color = self.text_primary();
        let fs = self.font_size;
        // 纵向滚动句柄（跨帧稳定）：`#live-scroll` 原生 `overflow_y_scroll` 在此
        // GPUI 不绘制可见滚动条，右侧 `Scrollbar::vertical` 自绘跟随它。
        let vhand = self.tabs[self.active_tab].live_vscroll.clone();
        div()
            .id("editor-area")
            .debug_selector(|| "editor-area".into())
            .relative()
            .flex_1()
            .flex_grow(grow)
            .min_w(px(0.))
            .min_h(px(0.))
            .bg(self.panel_bg())
            .overflow_hidden()
            .child(
                div()
                    .id("live-scroll")
                    .debug_selector(|| "live-scroll".into())
                    .relative()
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&vhand)
                    .child(
                        div()
                            .id("live-body")
                            .debug_selector(|| "live-body".into())
                            .w_full()
                            .px(px(12.))
                            .py(px(LIVE_BODY_PAD_Y))
                            .text_size(px(fs))
                            .text_color(text_color)
                            .child(live_any),
                    ),
            )
            .child(
                div()
                    .id("live-vscrollbar-overlay")
                    .debug_selector(|| "live-vscrollbar-overlay".into())
                    .absolute()
                    .inset_0()
                    .child(
                        Scrollbar::vertical(&vhand)
                            .mode(ScrollbarMode::Always)
                            .viewport_from_layout(),
                    ),
            )
            .child(Self::zoom_overlay(cx))
            .into_any_element()
    }

    /// 普通源码编辑区（非只读 / 非挂载占位 / 非 WYSIWYG）。
    /// `freeze_input=true`：live2 未就绪冻结展示——追加全区域透明输入拦截层。
    fn render_plain_editor(
        &mut self,
        grow: f32,
        freeze_input: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let editor = self.tabs[self.active_tab].editor.clone();
        // 字号覆盖（用户 Ctrl+=/- 调节；None=主题默认）。渲染时由 CodeEditor 自身读取。
        let font_size = self.font_size;
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
            .bg(self.panel_bg())
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

    /// live2 未就绪期间的全区域**透明输入拦截层**（形态对齐 `zoom_overlay`）：absolute
    /// inset_0 透明层 + `.occlude()`（GPUI 原生遮挡：阻断其下所有鼠标交互，含滚轮）
    /// ——不画任何视觉（样式零散落，全走 `ui::theme` 的父容器底色）。鼠标被整块吞掉，
    /// 键盘焦点在 toggle ON 时已移给 live2 空壳（不达源码编辑区）。
    fn live_pending_input_overlay() -> gpui::AnyElement {
        div()
            .id("live-pending-input-overlay")
            .debug_selector(|| "live-pending-input-overlay".into())
            .absolute()
            .inset_0()
            .occlude()
            .into_any_element()
    }

    /// 编辑区 / 预览区共用的 Ctrl+滚轮缩放覆盖层（Ctrl+滚轮改全局字号；普通滚轮透传）。
    /// 挂在整个内容区之上（最后绘制、命中测试最靠前），让编辑区与预览区都以同一个
    /// `change_font_size` 为唯一缩放入口，三处（编辑/预览/WYSIWYG）字号保持一致。
    fn zoom_overlay(cx: &mut Context<Self>) -> gpui::AnyElement {
        div()
            .id("zoom-overlay")
            .debug_selector(|| "zoom-overlay".into())
            .absolute()
            .left(px(0.))
            .right(px(0.))
            .top(px(0.))
            .bottom(px(0.))
            .on_scroll_wheel(cx.listener(
                |this: &mut EditorApp,
                 event: &gpui::ScrollWheelEvent,
                 _window,
                 cx| {
                    if !event.modifiers.control {
                        return;
                    }
                    let delta_y = match event.delta {
                        gpui::ScrollDelta::Lines(lines) => lines.y,
                        gpui::ScrollDelta::Pixels(pixels) => f32::from(pixels.y),
                    };
                    // 实测本平台：向上滚动 delta.y>0 → 放大；向下滚动 delta.y<0 → 缩小
                    let step = delta_y.clamp(-1.0, 1.0);
                    if step != 0.0 {
                        this.change_font_size(step, cx);
                    }
                    cx.stop_propagation();
                },
            ))
            .into_any_element()
    }

    /// 编辑区 / 预览区之间的分割线（可拖拽调整分屏比例）。
    ///
    /// 无缝直连：6px 命中带 + 居中 2px 细竖线——常态 `bar_border`，hover/拖拽时换
    /// `drag_hover_border` 强调色；命中区 / 拖拽逻辑完全不变（`w(6)` 命中带 +
    /// 容器 `on_mouse_move/up` 换算分屏比例）。
    pub(super) fn render_splitter(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let border = self.bar_border();
        let drag_line: gpui::Hsla =
            ui::theme::Theme::global(cx).colors.drag_hover_border.into();
        let line_color: gpui::Hsla = if self.splitter_hovered || self.split_drag.is_some() {
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
                    this.splitter_hovered = *hovered;
                    cx.notify();
                },
            ))
            .on_mouse_down(MouseButton::Left, cx.listener(
                |this: &mut EditorApp, ev: &gpui::MouseDownEvent, _window, cx| {
                    this.split_drag = Some((ev.position.x.as_f32(), this.split_ratio));
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
            return self.md_wy_host_children(window, cx, false, grow);
        }
        // PDF / 图片 / 视频（自包含阅读器）。
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
            .child(
                div()
                    .id("preview-pane")
                    .flex_1()
                    .min_w(px(0.))
                    .bg(self.panel_bg())
                    .relative()
                    // PDF → PdfViewer（整页原样）；其余 → Markdown 渲染器。
                    // Entity 直接作为子元素渲染（内部 size_full）。
                    .child(Self::preview_element(&self.tabs[self.active_tab]))
                    // Ctrl+滚轮缩放（同一入口 change_font_size）：预览区也参与全局字号调节。
                    .child(Self::zoom_overlay(cx)),
            )
            .into_any_element()
    }

    /// A2：MD 右栏/全屏 WyEditor 预览宿主（对齐左栏活区 `render_live_area` 结构）：
    /// 单一外层容器（`.overflow_hidden()`）+ 直接子件三件套（滚动容器 + 自绘纵向滚动条
    /// 叠层 + Ctrl+滚轮缩放叠层）。渲染期推进全局字号并做**缩放视口锚定**（同左面板
    /// `render_live_area`）：注入宿主滚动句柄 + 顶部内衬，推入当前视口顶部（元素局部
    /// y），再 `prepare_font_zoom`——字号变化在 render 期按新字号重建帧并把锚点块精确
    /// 位置写回 `ScrollHandle`。
    fn md_wy_host_children(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        fullscreen: bool,
        grow: f32,
    ) -> gpui::AnyElement {
        let ed = self.md_wy_preview().expect("调用方已判定走 WyEditor 预览");
        // Step2：大文档异步整篇解析在途（`md_wy` 空壳尚未 `install_open`）→ 渲染
        // 「正在排版 Markdown 预览…」占位——打开一瞬间右栏即有视觉内容，不再全白
        // 等整篇 Doc::open。解析一完成（`is_loaded()==true`）下一帧自动切回正式宿主。
        if !ed.read(cx).is_loaded() {
            return self.md_wy_pending_placeholder(fullscreen, grow);
        }
        let vhand = self.tabs[self.active_tab].md_wy_vscroll.clone();
        self.drive_wy_host(&ed, &vhand, window, cx);
        let fs = self.font_size;
        let any = ed.clone().into_any_element();
        let text_color = self.text_primary();
        let scroll = div()
            .id("md-wy-scroll")
            .debug_selector(|| "md-wy-scroll".into())
            .relative()
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&vhand)
            .child(
                div()
                    .id("md-wy-body")
                    .debug_selector(|| "md-wy-body".into())
                    .w_full()
                    // 宿主水平内衬 = 预览内容列留白**单一来源** `PREVIEW_SIDE_GAP_PX`
                    // （与 `markdown::view` 同源）：右侧留出肉眼可见间距，标题/正文最右字
                    // 不贴右缘、不被自绘滚动条带盖住；滚动条叠层贴外层右缘，gap 在其内侧。
                    .px(px(config::markdown::PREVIEW_SIDE_GAP_PX))
                    .py(px(LIVE_BODY_PAD_Y))
                    .text_size(px(fs))
                    .text_color(text_color)
                    .child(any),
            );
        let vscrollbar_overlay = div()
            .id("md-wy-vscrollbar-overlay")
            .debug_selector(|| "md-wy-vscrollbar-overlay".into())
            .absolute()
            .inset_0()
            .child(
                Scrollbar::vertical(&vhand)
                    .mode(ScrollbarMode::Always)
                    .viewport_from_layout(),
            );
        let zoom_overlay = Self::zoom_overlay(cx);
        if fullscreen {
            return div()
                .id("fullscreen-preview")
                .debug_selector(|| "fullscreen-preview".into())
                .size_full()
                .relative()
                .bg(self.panel_bg())
                .overflow_hidden()
                .child(scroll)
                .child(vscrollbar_overlay)
                .child(zoom_overlay)
                .into_any_element();
        }
        div()
            .id("preview-md-wy")
            .debug_selector(|| "preview-md-wy".into())
            .relative()
            // grow = 预览区占宽比例（与旧预览/左编辑区同为 flex 项：basis 0 + shrink 1）
            .flex_grow(grow)
            .flex_shrink_1()
            .flex_basis(px(0.))
            .min_w(px(0.))
            .bg(self.panel_bg())
            .overflow_hidden()
            // 预览区也可接收文件拖放
            .can_drop(|drag: &dyn Any, _w, _c| drag.downcast_ref::<ExternalPaths>().is_some())
            .on_drop(cx.listener(
                |this: &mut EditorApp, paths: &ExternalPaths, window, cx| {
                    this.open_dropped_paths(paths, window, cx);
                },
            ))
            .child(scroll)
            .child(vscrollbar_overlay)
            .child(zoom_overlay)
            .into_any_element()
    }

    /// Step2：右栏 `md_wy` 未就绪（大文档后台整篇解析在途）时的「正在排版」占位。
    /// 布局对齐 [`Self::render_editor_pending`]（flex 居中 + 主题 muted 文字）；**容器**形态
    /// 对齐 `md_wy_host_children` 外层（同一 `preview-md-wy` / `fullscreen-preview` 容器 id
    /// 与 `flex_grow/shrink/basis/min_w` 参数），占位→内容切换不改变窗口列宽布局。样式全走
    /// `ui::theme` token（`panel_bg` / `text_minor`），禁止散落 rgba。
    fn md_wy_pending_placeholder(&self, fullscreen: bool, grow: f32) -> gpui::AnyElement {
        let panel_bg = self.panel_bg();
        let muted = self.text_minor();
        let label = div()
            .id("md-wy-pending-label")
            .debug_selector(|| "md-wy-pending-label".into())
            .text_color(muted)
            .text_size(px(13.))
            .child("正在排版 Markdown 预览…");
        if fullscreen {
            return div()
                .id("fullscreen-preview")
                .debug_selector(|| "fullscreen-preview".into())
                .size_full()
                .relative()
                .bg(panel_bg)
                .overflow_hidden()
                .flex()
                .items_center()
                .justify_center()
                .child(label)
                .into_any_element();
        }
        div()
            .id("preview-md-wy")
            .debug_selector(|| "preview-md-wy".into())
            .relative()
            .flex_grow(grow)
            .flex_shrink_1()
            .flex_basis(px(0.))
            .min_w(px(0.))
            .bg(panel_bg)
            .overflow_hidden()
            .flex()
            .items_center()
            .justify_center()
            .child(label)
            .into_any_element()
    }

    /// 全屏预览（EPUB / PDF 阅读模式）：渲染占满整个内容区，不显示编辑区/分割线。
    /// A2：Markdown 全屏预览同样切统一组件 WyEditor（宿主滚动）。
    pub(super) fn render_fullscreen_preview(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        if self.md_wy_preview().is_some() {
            // A2：Markdown 全屏预览同样切统一组件 WyEditor（宿主滚动；结构与右栏同源）。
            return self.md_wy_host_children(window, cx, true, 1.0);
        }
        // 媒体查看器（图片/视频）：切换/解码完成后把标签标题与状态栏跟到当前媒体
        //（渲染期幂等同步，无媒体/路径未变则跳过）。
        if self.sync_active_media_meta(cx) {
            cx.notify();
        }
        // 全屏阅读（PDF / 图片 / 视频）：自包含阅读器自处理缩放，无需全局字号推进。
        let tab = &self.tabs[self.active_tab];
        let mut el = div()
            .id("fullscreen-preview")
            .debug_selector(|| "fullscreen-preview".into())
            .size_full()
            .relative()
            .bg(self.panel_bg())
            .child(Self::preview_element(tab));
        // 图片/视频查看器：自处理滚轮（图片：单滚切换 / Ctrl+滚轮缩放），不再挂全局
        // zoom_overlay（否则 Ctrl+滚轮会被全局字号缩放抢走，媒体缩放失效）；
        // PDF / 其余（无自处理滚轮）继续挂 zoom_overlay。
        #[cfg(feature = "video")]
        let media_self_wheel = !matches!(&tab.reader, MediaReader::Image(_) | MediaReader::Video(_));
        #[cfg(not(feature = "video"))]
        let media_self_wheel = !matches!(&tab.reader, MediaReader::Image(_));
        if media_self_wheel {
            el = el.child(Self::zoom_overlay(cx));
        }
        el.into_any_element()
    }
}
