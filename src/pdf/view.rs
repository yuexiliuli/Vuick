//! PDF 阅读器视图：把整份 PDF 逐页当作图片，从上到下按行级虚拟化显示。
//!
//! 页面即“原样”栅格化位图（PNG），宽度铺满预览可用宽度（上限 [`PAGE_MAX_W`]），
//! 高度按位图固有比例缩放，滚动条复用 gpui_component 组件。
//! 不做任何文本/布局改写，也不落盘。

use std::sync::Arc;

use gpui::{
    div, img, list, px, App, Context, Hsla, ImageSource, InteractiveElement, IntoElement,
    ListAlignment, ListState, ObjectFit, ParentElement, Render, Styled, StyledImage, Window,
};
use gpui_component::scroll::ScrollableElement as _;

use super::PdfPage;

/// 视口上下各多构建的像素量（保证快速滚动时内容连续）。
const ROW_OVERDRAW_PX: f32 = 600.0;
/// 页面左右/上下内边距（px）。
const PAGE_PAD_PX: f32 = 12.0;
/// 页面显示宽度上限（px）：更宽视口下页面不再放大，水平居中。
const PAGE_MAX_W: f32 = 1000.0;

/// PDF 阅读器：行级虚拟化、整页原样显示。
pub struct PdfViewer {
    /// 逐页位图（内存，不落盘）。
    pages: Vec<Arc<PdfPage>>,
    list_state: ListState,
    /// 背景色覆盖（由应用传入，与编辑区保持一致；None 用主题背景）。
    bg: Option<Hsla>,
}

impl PdfViewer {
    pub fn new(pages: Vec<PdfPage>, _cx: &mut App) -> Self {
        let list_state =
            ListState::new(pages.len(), ListAlignment::Top, px(ROW_OVERDRAW_PX)).measure_all();
        Self {
            pages: pages.into_iter().map(Arc::new).collect(),
            list_state,
            bg: None,
        }
    }

    /// 页数（测试 / 状态栏等场景使用）。
    #[allow(dead_code)]
    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    /// 设置预览背景色（打开 / 主题切换时由应用传入）。
    pub fn set_background(&mut self, bg: Hsla) {
        self.bg = Some(bg);
    }
}

impl Render for PdfViewer {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let pages = self.pages.clone();
        let list_state = self.list_state.clone();
        let bg = self.bg.unwrap_or_else(|| {
            gpui_component::Theme::global(cx).semantic_tokens().colors.background
        });

        let content = list(list_state.clone(), move |ix, _window, _cx| {
            render_page_row(&pages[ix])
        })
        .size_full();

        div()
            .id("pdf-viewer")
            .debug_selector(|| "pdf-viewer".into())
            .size_full()
            .relative()
            .bg(bg)
            .child(content)
            .vertical_scrollbar(&list_state)
    }
}

/// 单页：整页图片，宽 = min(视口可用宽, PAGE_MAX_W)，高按位图固有比例缩放，水平居中。
fn render_page_row(page: &PdfPage) -> gpui::AnyElement {
    let aspect = page.width as f32 / page.height.max(1) as f32;
    div()
        .w_full()
        .flex_shrink_0()
        .px(px(PAGE_PAD_PX))
        .py(px(PAGE_PAD_PX))
        .flex()
        .justify_center()
        .child(
            div()
                .w_full()
                .max_w(px(PAGE_MAX_W))
                .aspect_ratio(aspect)
                .child(
                    img(ImageSource::Image(page.image.clone()))
                        .object_fit(ObjectFit::Contain)
                        .w_full()
                        .h_full()
                        .with_fallback(move || {
                            div()
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_size(px(13.))
                                .child("页面渲染失败")
                                .into_any_element()
                        }),
                ),
        )
        .into_any_element()
}
