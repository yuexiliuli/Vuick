//! 预览 / WYSIWYG 渲染元素（2026-11-11 自 `crates/shell` 的 `app/panes.rs` 预览渲染半 +
//! `app/preview.rs` 迁出，纯结构移动）。
//!
//! 统一 `md_wy` WyEditor 右栏/全屏宿主（`md_wy_host_children` + 占位）、WYSIWYG 活区
//! 左面板宿主（`render_live_area` / `drive_wy_host`）、编辑区/预览区共用 Ctrl+滚轮缩放
//! 叠层（`zoom_overlay`，宿主缩放入口的唯一转发点）与自绘滚动条 / 输入拦截层。媒体/
//! PDF 查看器元素（`preview_element`）供 shell 的媒体渲染分支复用。
//!
//! 主题色（`panel_bg` / `text_primary` / `text_minor`）由宿主 chrome 提供、经参数传入
//! （域不直读宿主 `EditorApp`）；`zoom_overlay` / `on_drop` 的宿主副作用经
//! [`crate::host::PreviewHost`] 反调。

use std::any::Any;
use std::cell::RefCell;
use std::rc::Rc;

use gpui::{
    div, px, Context, ExternalPaths, Hsla, InteractiveElement, IntoElement, ParentElement,
    ScrollDelta, ScrollHandle, ScrollWheelEvent, StatefulInteractiveElement, Styled, Window,
};
use markdown_wysiwyg::WyEditor;
use shell_core::{AppModel, EditorTab, MediaReader, LIVE_BODY_PAD_Y};
use ui::scrollbar::{Scrollbar, ScrollbarMode};

use crate::PreviewHost;

/// 当前选项卡的预览视图：图片选项卡用 `ImageViewer`（自处理滚轮），
/// 视频选项卡用 `VideoViewer`（M1 首帧静态阅读），PDF 选项卡用 `PdfViewer`
///（整页原样），其余（MD / EPUB）用统一 WyEditor 只读宿主（M5-B5 P4-2：
/// `MarkdownPreview` 已删）。预览未创建 → 空元素兜底。
pub fn preview_element(tab: &EditorTab) -> gpui::AnyElement {
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

/// 是否开启本覆盖层滚轮的诊断探针（`GPUI_EDITOR_WHEEL=1`，进程内缓存一次）：与 PDF
/// 侧 `[VIEW-WHEEL]` 探针（`GPUI_EDITOR_PDF_TIMING=1`）对位，两处日志确认 Ctrl+滚轮
/// 的修饰键命中（ev_ctrl / win_ctrl）与事件是否到达查看器。
fn wheel_probe() -> bool {
    static EN: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *EN.get_or_init(|| {
        std::env::var_os("GPUI_EDITOR_WHEEL").is_some_and(|v| v == "1" || v == "true")
    })
}

/// 编辑区 / 预览区共用的 Ctrl+滚轮缩放覆盖层（Ctrl+滚轮改全局字号；普通滚轮透传）。
/// 挂在整个内容区之上（最后绘制、命中测试最靠前），让编辑区与预览区都以同一个
/// `preview_change_font_size` 为唯一缩放入口，三处（编辑/预览/WYSIWYG）字号保持一致。
/// ⚠️ 只挂在「不自处理滚轮的文本宿主」之上：PDF / 图片 / 视频 等自带阅读器一律不挂
/// （见 shell `render_preview_pane` / `render_fullscreen_preview`）——否则本层作为最靠前
/// 命中在 Ctrl+滚轮时先消费并 stop_propagation，媒体自身缩放失效。
pub fn zoom_overlay<H: PreviewHost>(cx: &mut Context<H>) -> gpui::AnyElement {
    div()
        .id("zoom-overlay")
        .debug_selector(|| "zoom-overlay".into())
        .absolute()
        .left(px(0.))
        .right(px(0.))
        .top(px(0.))
        .bottom(px(0.))
        .on_scroll_wheel(cx.listener(
            |this: &mut H,
             event: &ScrollWheelEvent,
             window: &mut Window,
             cx| {
                if wheel_probe() {
                    eprintln!(
                        "[SHELL-WHEEL] zoom_overlay ev_ctrl={} win_ctrl={}",
                        event.modifiers.control,
                        window.modifiers().control,
                    );
                }
                if !event.modifiers.control {
                    return;
                }
                let delta_y = match event.delta {
                    ScrollDelta::Lines(lines) => lines.y,
                    ScrollDelta::Pixels(pixels) => f32::from(pixels.y),
                };
                // 实测本平台：向上滚动 delta.y>0 → 放大；向下滚动 delta.y<0 → 缩小
                let step = delta_y.clamp(-1.0, 1.0);
                if step != 0.0 {
                    this.preview_change_font_size(step, cx);
                }
                cx.stop_propagation();
            },
        ))
        .into_any_element()
}

/// live2 未就绪期间的全区域**透明输入拦截层**（形态对齐 [`zoom_overlay`]）：absolute
/// inset_0 透明层 + `.occlude()`（GPUI 原生遮挡：阻断其下所有鼠标交互，含滚轮）
/// ——不画任何视觉（样式零散落，全走宿主父容器底色）。鼠标被整块吞掉，
/// 键盘焦点在 toggle ON 时已移给 live2 空壳（不达源码编辑区）。
pub fn live_pending_input_overlay() -> gpui::AnyElement {
    div()
        .id("live-pending-input-overlay")
        .debug_selector(|| "live-pending-input-overlay".into())
        .absolute()
        .inset_0()
        .occlude()
        .into_any_element()
}

/// 统一 WyEditor 宿主驱动（live2 左面板 / 右栏预览 / 全屏预览共用）：渲染期注入
/// 宿主滚动句柄 + 内容顶部内衬，推入当前视口顶部（元素局部 y = 滚动偏移 − 顶部
/// 内衬），再 `prepare_font_zoom`——字号变化时 WyEditor 在 render 期按新字号重建帧
/// 并把锚点块的精确位置写回宿主滚动句柄（prepaint 读偏移之前完成），
/// Ctrl+滚轮缩放**无缝**、不带动滚动位置（收敛自驱动在 WyEditor 元素 prepaint）。
pub fn drive_wy_host<H: 'static>(
    model: &Rc<RefCell<AppModel>>,
    ed: &gpui::Entity<WyEditor>,
    vhand: &ScrollHandle,
    window: &mut Window,
    cx: &mut Context<H>,
) {
    let fs = model.borrow().font_size;
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
pub fn render_live_area<H: PreviewHost>(
    model: &Rc<RefCell<AppModel>>,
    ed: &gpui::Entity<WyEditor>,
    window: &mut Window,
    grow: f32,
    cx: &mut Context<H>,
    panel_bg: Hsla,
    text_primary: Hsla,
) -> gpui::AnyElement {
    let active = model.borrow().active_tab;
    let vhand = model.borrow().tabs[active].live_vscroll.clone();
    drive_wy_host(model, ed, &vhand, window, cx);
    let live_any = ed.clone().into_any_element();
    let text_color = text_primary;
    let fs = model.borrow().font_size;
    // 纵向滚动句柄（跨帧稳定）：`#live-scroll` 原生 `overflow_y_scroll` 在此
    // GPUI 不绘制可见滚动条，右侧 `Scrollbar::vertical` 自绘跟随它。
    let vhand = model.borrow().tabs[active].live_vscroll.clone();
    div()
        .id("editor-area")
        .debug_selector(|| "editor-area".into())
        .relative()
        .flex_1()
        .flex_grow(grow)
        .min_w(px(0.))
        .min_h(px(0.))
        .bg(panel_bg)
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
        .child(zoom_overlay::<H>(cx))
        .into_any_element()
}

/// A2：MD 右栏/全屏 WyEditor 预览宿主（对齐左栏活区 `render_live_area` 结构）：
/// 单一外层容器（`.overflow_hidden()`）+ 直接子件三件套（滚动容器 + 自绘纵向滚动条
/// 叠层 + Ctrl+滚轮缩放叠层）。渲染期推进全局字号并做**缩放视口锚定**（同左面板
/// `render_live_area`）：注入宿主滚动句柄 + 顶部内衬，推入当前视口顶部（元素局部
/// y），再 `prepare_font_zoom`——字号变化在 render 期按新字号重建帧并把锚点块精确
/// 位置写回 `ScrollHandle`。
pub fn md_wy_host_children<H: PreviewHost>(
    model: &Rc<RefCell<AppModel>>,
    window: &mut Window,
    cx: &mut Context<H>,
    fullscreen: bool,
    grow: f32,
    panel_bg: Hsla,
    text_primary: Hsla,
    text_minor: Hsla,
) -> gpui::AnyElement {
    let ed = crate::mdwy::md_wy_preview(&model.borrow()).expect("调用方已判定走 WyEditor 预览");
    // Step2：大文档异步整篇解析在途（`md_wy` 空壳尚未 `install_open`）→ 渲染
    // 「正在排版 Markdown 预览…」占位——打开一瞬间右栏即有视觉内容，不再全白
    // 等整篇 Doc::open。解析一完成（`is_loaded()==true`）下一帧自动切回正式宿主。
    if !ed.read(cx).is_loaded() {
        return md_wy_pending_placeholder(fullscreen, grow, panel_bg, text_minor);
    }
    let active = model.borrow().active_tab;
    let vhand = model.borrow().tabs[active].md_wy_vscroll.clone();
    drive_wy_host(model, &ed, &vhand, window, cx);
    let fs = model.borrow().font_size;
    let any = ed.clone().into_any_element();
    let text_color = text_primary;
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
    let zoom_overlay = zoom_overlay::<H>(cx);
    if fullscreen {
        return div()
            .id("fullscreen-preview")
            .debug_selector(|| "fullscreen-preview".into())
            .size_full()
            .relative()
            .bg(panel_bg)
            .overflow_hidden()
            .child(scroll)
            .child(vscrollbar_overlay)
            .child(zoom_overlay)
            .into_any_element();
    }
    let drop_paths = cx.listener(
        |this: &mut H, paths: &ExternalPaths, window: &mut Window, cx| {
            this.preview_open_dropped_paths(paths, window, cx);
        },
    );
    div()
        .id("preview-md-wy")
        .debug_selector(|| "preview-md-wy".into())
        .relative()
        // grow = 预览区占宽比例（与旧预览/左编辑区同为 flex 项：basis 0 + shrink 1）
        .flex_grow(grow)
        .flex_shrink_1()
        .flex_basis(px(0.))
        .min_w(px(0.))
        .bg(panel_bg)
        .overflow_hidden()
        // 预览区也可接收文件拖放
        .can_drop(|drag: &dyn Any, _w, _c| drag.downcast_ref::<ExternalPaths>().is_some())
        .on_drop(drop_paths)
        .child(scroll)
        .child(vscrollbar_overlay)
        .child(zoom_overlay)
        .into_any_element()
}/// Step2：右栏 `md_wy` 未就绪（大文档后台整篇解析在途）时的「正在排版」占位。
/// 布局对齐 shell `render_editor_pending`（flex 居中 + 主题 muted 文字）；**容器**形态
/// 对齐 [`md_wy_host_children`] 外层（同一 `preview-md-wy` / `fullscreen-preview` 容器 id
/// 与 `flex_grow/shrink/basis/min_w` 参数），占位→内容切换不改变窗口列宽布局。样式全走
/// 主题 token（`panel_bg` / `text_minor`），禁止散落 rgba。
pub fn md_wy_pending_placeholder(
    fullscreen: bool,
    grow: f32,
    panel_bg: Hsla,
    text_minor: Hsla,
) -> gpui::AnyElement {
    let label = div()
        .id("md-wy-pending-label")
        .debug_selector(|| "md-wy-pending-label".into())
        .text_color(text_minor)
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
