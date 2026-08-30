//! Markdown data model and theme definitions.

use gpui::{px, rgb, rgba, Hsla, Pixels, SharedString};

/// 表格超宽时的处理方式。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TableMode {
    /// 列按可用宽度等分，单元格内文本自动换行（不出现横向滚动）。
    Wrap,
    /// 列取内容自然宽度（各列等宽对齐），超出可用宽度时出现**可拖拽的横向滚动条**。
    Scroll,
}

/// 单元格左右内边距（px_2 = 8px*2）——列宽测量时补上，避免文字顶到边框。
pub(super) const CELL_H_PAD: Pixels = px(16.);
/// 单元格左边框宽度（列宽测量时补上）。
pub(super) const CELL_BORDER: Pixels = px(1.);
/// 表头加粗比普通字稍宽的测量缓冲（Scroll 模式列宽按普通字重测量）。
pub(super) const CELL_HEADER_BOLD_BUFFER: Pixels = px(2.);

/// 渲染配色（可整体定制明/暗主题）。
pub struct MarkdownTheme {
    pub text: Hsla,
    pub code_bg: Hsla,
    pub code_text: Hsla,
    pub mono_family: SharedString,
    pub link: Hsla,
    pub heading: Hsla,
    pub blockquote_bg: Hsla,
    pub blockquote_border: Hsla,
    pub table_bg: Hsla,
    pub table_text: Hsla,
}

impl MarkdownTheme {
    /// 亮色主题（对应原前端的浅色风格）。
    pub fn light() -> Self {
        Self {
            text: rgb(0x1f2328).into(),
            code_bg: rgba(0x00000014).into(),
            code_text: rgb(0x1f6feb).into(),
            mono_family: "Consolas".into(),
            link: rgb(0x2563eb).into(),
            heading: rgb(0x111827).into(),
            blockquote_bg: rgb(0xf9fafb).into(),
            blockquote_border: rgb(0xd1d5db).into(),
            table_bg: rgb(0xf3f4f6).into(),
            table_text: rgb(0x1f2328).into(),
        }
    }
}

/// 行内样式片段。
#[derive(Clone, Debug)]
pub(crate) struct Span {
    pub(super) text: String,
    pub(super) bold: bool,
    pub(super) italic: bool,
    pub(super) strike: bool,
    pub(super) code: bool,
    pub(super) link: bool,
}

/// 块级元素。
#[derive(Clone, Debug)]
pub(crate) enum Block {
    Paragraph(Vec<Vec<Span>>),
    Heading {
        level: usize,
        spans: Vec<Span>,
    },
    Code(String),
    List {
        ordered: bool,
        items: Vec<Vec<Span>>,
    },
    Table {
        headers: Vec<Vec<Span>>,
        rows: Vec<Vec<Vec<Span>>>,
    },
    Quote(Vec<Span>),
    Hr,
}
