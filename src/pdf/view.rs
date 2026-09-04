//! PDF 阅读器视图：把整份 PDF 逐页当作图片，从上到下按行级虚拟化显示。
//!
//! 页面即“原样”栅格化位图（PNG），宽度铺满预览可用宽度（上限 [`PAGE_MAX_W`]），
//! 高度按位图固有比例缩放，滚动条复用 gpui_component 组件。
//! 不做任何文本/布局改写，也不落盘。
//!
//! 性能：列表默认只测量/上传**可见行**（`ListMeasuringBehavior::Visible`），
//! 并用统一高度提示让滚动条从一开始就尺寸正确——避免 `measure_all()` 在首帧
//! 把全部页面一次性解码 + 上传 GPU 纹理导致窗口卡死。

use std::sync::Arc;

use gpui::{
    div, img, list, px, rgb, white, App, Context, Hsla, ImageSource, InteractiveElement,
    IntoElement, ListAlignment, ListState, ObjectFit, ParentElement, Render, Styled, StyledImage,
    Window,
};
use gpui_component::scroll::ScrollableElement as _;

use super::PdfPage;

/// 视口上下各多构建的像素量（保证快速滚动时内容连续）。
const ROW_OVERDRAW_PX: f32 = 600.0;
/// 页面左右/上下内边距（px）。
const PAGE_PAD_PX: f32 = 12.0;
/// 页面顶端留白（px）。
const PAGE_TOP_PX: f32 = 8.0;
/// 页面底端间距（px，页与页之间的视觉分隔）。
const PAGE_BOTTOM_PX: f32 = 20.0;
/// 页面显示宽度上限（px）：更宽视口下页面不再放大，水平居中。
const PAGE_MAX_W: f32 = 1000.0;
/// 列表行统一高度提示（px）：首帧未逐行测量前的滚动条尺寸估算，
/// 随实际渲染逐行收敛到真实高度。
const UNIFORM_ROW_H: f32 = 1300.0;

/// PDF 阅读器：行级虚拟化、整页原样显示。
pub struct PdfViewer {
    /// 逐页位图（内存，不落盘）。
    pages: Vec<Arc<PdfPage>>,
    list_state: ListState,
    /// 阅读画布背景（页面“纸面”之外的底色；None 用主题推导）。
    /// 页面本身会画成一张浅色“卡片”，与画布形成分隔。
    bg: Option<Hsla>,
}

impl PdfViewer {
    pub fn new(pages: Vec<PdfPage>, _cx: &mut App) -> Self {
        let list_state = ListState::new(pages.len(), ListAlignment::Top, px(ROW_OVERDRAW_PX))
            // 只测量可见行（避免首帧一次性解码全部页面位图导致卡顿），
            // 同时给出统一高度提示，让滚动条从一开始就尺寸正确。
            .with_uniform_item_height(px(UNIFORM_ROW_H));
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

    /// 设置阅读画布背景色（打开 / 主题切换时由应用传入）。
    pub fn set_background(&mut self, bg: Hsla) {
        self.bg = Some(bg);
    }

    /// 画布默认底色：浅色模式浅灰、深色模式深灰。
    fn default_canvas(cx: &App) -> Hsla {
        if gpui_component::Theme::global(cx).is_dark() {
            rgb(0x111827).into()
        } else {
            rgb(0xe5e7eb).into()
        }
    }

    /// 页面“卡片”配色：纸面白 / 深色纸面，配边框。
    fn page_card_colors(cx: &App) -> (Hsla, Hsla) {
        if gpui_component::Theme::global(cx).is_dark() {
            (rgb(0x1f2937).into(), rgb(0x374151).into())
        } else {
            (white().into(), rgb(0xd1d5db).into())
        }
    }
}

impl Render for PdfViewer {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let pages = self.pages.clone();
        let list_state = self.list_state.clone();
        let canvas = self.bg.unwrap_or_else(|| Self::default_canvas(cx));
        let (page_bg, page_border) = Self::page_card_colors(cx);

        let content = list(list_state.clone(), move |ix, _window, _cx| {
            render_page_row(&pages[ix], page_bg, page_border)
        })
        .size_full();

        div()
            .id("pdf-viewer")
            .debug_selector(|| "pdf-viewer".into())
            .size_full()
            .relative()
            .bg(canvas)
            .child(content)
            .vertical_scrollbar(&list_state)
    }
}

/// 单页：画成一张带边框圆角的“纸面卡片”，宽 = min(视口可用宽, PAGE_MAX_W)，
/// 高按位图固有比例缩放，水平居中；卡片底色/边框与画布形成清晰的页间分隔。
fn render_page_row(page: &PdfPage, page_bg: Hsla, page_border: Hsla) -> gpui::AnyElement {
    let aspect = page.width as f32 / page.height.max(1) as f32;
    let card = div()
        .w_full()
        .max_w(px(PAGE_MAX_W))
        .aspect_ratio(aspect)
        .rounded(px(4.))
        .bg(page_bg)
        .border_1()
        .border_color(page_border)
        .overflow_hidden()
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
        );
    div()
        .w_full()
        .flex_shrink_0()
        .px(px(PAGE_PAD_PX))
        .pt(px(PAGE_TOP_PX))
        .pb(px(PAGE_BOTTOM_PX))
        .flex()
        .justify_center()
        .child(card)
        .into_any_element()
}
