//! `markdown::view`：Markdown 预览视图层（M5-B5 P4-2 收口后仅剩**共享助手**）。
//!
//! M5-B5「统一文档层」终态（P4-2）：A 侧 Markdown 预览引擎 `MarkdownPreview`
//!（gateway/epub/row/table 宿主与 `view/tests*`）已整体删除——MD 与 EPUB 全部走
//! **WYSIWYG（`wysiwyg` 的 Doc+shard 管线 + 只读宿主）** 单一引擎。本模块只保留
//! 被 `wysiwyg` / `layout` 仍引用的最小共享面：
//! - `style`：排版参数快照 [`style::StyleParams`]（`WyTheme` 由它一次性派生）；
//! - `dimensions`：图片头尺寸嗅探（`image_dimensions*`，EPUB 内存图与 WYSIWYG 复用）；
//! - 本文件：`selection_color`（选区背景，与 WYSIWYG 共用一款）与 `find_text_range`
//!   （搜索命中文本定位，`WyEditor::highlight_search` 复用）。
//!
//! `expand_bg_over_whitespace`（行内代码 pill 空白扩展）已迁至 `layout`。
//! 其余预览引擎代码全部删除（含 `MarkdownPreview` / `PreviewSrc` / `TextCtx` /
//! `NoteCtx` 等公共与测试面）。

pub(crate) mod dimensions;
pub mod style;

use std::ops::Range;

use gpui::{App, Hsla};

/// 文本选择的背景色（半透明蓝，浅/暗主题各一版；与搜索高亮同法，非主题强调色）。
/// WYSIWYG(`markdown/wysiwyg`) 共用同一款，保证「选区背景色」全局一致。
pub(crate) fn selection_color(cx: &App) -> Hsla {
    if ui::theme::Theme::global(cx).is_dark() {
        gpui::rgba(0x4d6bfe61).into()
    } else {
        gpui::rgba(0x4d6bfe42).into()
    }
}

/// 在 `text` 内查找第一个忽略大小写命中小写 `needle` 的字节区间；未命中返回 `None`。
/// 用于搜索跳转为命中**文本**着色（非整行）。
/// `text` 内 `needle`（须已小写）**首次**出现的字节区间（逐字符取等长候选小写比较，
/// 正确处理 UTF-8 多字节）。WYSIWYG 搜索跳转高亮（`WyEditor`）复用。
pub(crate) fn find_text_range(text: &str, needle: &str) -> Option<Range<usize>> {
    let needle_chars = needle.chars().count();
    if needle_chars == 0 {
        return None;
    }
    let mut i = 0usize;
    while i < text.len() {
        let candidate: String = text[i..].chars().take(needle_chars).collect();
        if !candidate.is_empty() && candidate.to_lowercase() == needle {
            return Some(i..(i + candidate.len()));
        }
        let ch = text[i..].chars().next()?;
        i += ch.len_utf8();
    }
    None
}
