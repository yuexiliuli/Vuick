//! Markdown 渲染器的视图层：行级虚拟化 + gpui_component 滚动条（双向）。
//!
//! 核心思路（参考 vellum / zorite 的行级虚拟化）：文档拆成**一行一条**渲染行、用
//! gpui 的 `List` 做虚拟化；竖滚动条量程用「宽度感知的平均/精确 shard 高」铺设
//! （见 [`gateway`] 文件头与 `MarkdownPreview`）；页面内容恒换行，宽块块内横向滚动。
//!
//! 本目录按职责拆分（M5-B5 P3-1c-1，替代原先的单文件 `view/mod.rs` God 文件）：
//! - `gateway`：预览宿主 `MarkdownPreview`（源/滚动量程/渲染/双向滚动/导出等，见文件头）
//! - `epub`：EPUB 阅读增强（注记悬浮/引文导航/页码化，见文件头）
//! - `row`：单行渲染（[`row::render_row_shard`] 统一 shard 几何；表格/图片旧内容路径）
//! - `style`：排版参数快照（[`style::StyleParams`]）
//! - `dimensions`：行宽 / 表格列宽估算、图片头尺寸解析
//! - `table`：GFM 表格渲染（[`table::render_table`]）
//! - `tests`：视图层测试

pub(crate) mod dimensions;
mod epub;
mod gateway;
mod row;
pub(crate) mod style;
mod table;

use std::ops::Range;

use gpui::{App, Hsla};

/// 行内代码 pill 背景区间的统一扩展规则（预览与 WYSIWYG 共用单一行为源；
/// 定义在 `row`，作为 view 层 API 再导出给 `wysiwyg`）。
pub(crate) use row::expand_bg_over_whitespace;

/// 预览宿主与源类型（对外 API；`markdown/mod.rs` 再导出到 crate 顶层）。
pub use gateway::{MarkdownPreview, PreviewSrc};
/// 文本命中共享表（`row` 的 `ShardStrip` paint 写、宿主鼠标处理器读）。
pub(crate) use gateway::{TextCtx, TextHit};
/// EPUB 注记/引文类型（`row` 与宿主共用）。
pub(crate) use epub::{NoteCtx, NoteHit};

/// 文本选择的背景色（半透明蓝，浅/暗主题各一版；与搜索高亮同法，非主题强调色）。
/// WYSIWYG(`markdown/wysiwyg`) 也共用同一款，保证「选择区背景色」全局一致。
pub(crate) fn selection_color(cx: &App) -> Hsla {
    if crate::ui::theme::Theme::global(cx).is_dark() {
        gpui::rgba(0x4d6bfe61).into()
    } else {
        gpui::rgba(0x4d6bfe42).into()
    }
}

/// 在 `text` 内查找第一个忽略大小写命中小写 `needle` 的字节区间；未命中返回 `None`。
/// 用于搜索跳转为命中**文本**着色（非整行）。
/// `text` 内 `needle`（须已小写）**首次**出现的字节区间（逐字符取等长候选小写比较，
/// 正确处理 UTF-8 多字节）。WYSIWYG 搜索跳转高亮（`WyEditor`）与预览共用。
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

// ===== 视图层测试 prelude（P3-1c-1 拆分后保留） =====
// `tests.rs` 用 `include!` 把各测试文件拼进 `view::tests`，测试靠 `use super::*` 取得
// 下文这些名字（拆分前它们由 view 自身代码直接消费，现在宿主内部件已移入 gateway/epub，
// 此 prelude 仅供测试可达）。`#[cfg(test)]` 让生产构建不引入；`#[allow(unused_imports)]`
// 说明这些名字只被 `use super::*` 间接使用（rustc 不把子模块 glob 计入父模块的引用）。
#[cfg(test)]
#[allow(unused_imports)]
use gateway::{
    adaptive_overdraw, preview_backend, preview_text_avail, set_preview_backend, CodeHlCache,
    HEIGHT_EXACT_SHAPE_MAX, PreviewBackend, PreviewVerticalScrollHandle,
};
#[cfg(test)]
#[allow(unused_imports)]
use std::cell::{Cell, RefCell};
#[cfg(test)]
#[allow(unused_imports)]
use std::rc::Rc;
#[cfg(test)]
#[allow(unused_imports)]
use gpui::{
    anchored, deferred, div, list, point, px, rgba, Bounds, ClipboardItem, Context, Div,
    FocusHandle, InteractiveElement, IntoElement, KeyDownEvent, ListAlignment, ListOffset,
    ListState, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, Pixels,
    Point, Render, ScrollDelta, ScrollHandle, ScrollWheelEvent, SharedString, Stateful,
    StatefulInteractiveElement, Styled, Window, prelude::FluentBuilder as _,
};
#[cfg(test)]
#[allow(unused_imports)]
use ropey::Rope;
#[cfg(test)]
#[allow(unused_imports)]
use crate::ui::scrollbar::{Scrollbar, ScrollbarHandle, ScrollbarMode};
#[cfg(test)]
#[allow(unused_imports)]
use crate::config::limits::{
    CODE_HL_DEFER_BLOCKS, CODE_HL_DEFER_BYTES, CODE_HL_LRU_CAP, CODE_HL_PARSE_BLOCKS_PER_FRAME,
    CODE_HL_PARSE_BYTES_PER_FRAME, WINDOWED_MIN_BLOCKS, WINDOWED_OVERDRAW_BLOCKS,
    WINDOWED_OVERDRAW_FACTOR, WINDOWED_OVERDRAW_MAX,
};
#[cfg(test)]
#[allow(unused_imports)]
use crate::config::markdown::{CONTENT_EDGE_PAD_X_PX, PREVIEW_SIDE_GAP_PX, ROW_OVERDRAW_PX};
#[cfg(test)]
#[allow(unused_imports)]
use crate::config::timing::HIGHLIGHT_MS;
#[cfg(test)]
#[allow(unused_imports)]
use crate::markdown::doc::{assemble_docrow_window, DocLineView, DocPayload};
#[cfg(test)]
#[allow(unused_imports)]
use crate::markdown::ir::IrKind;
#[cfg(test)]
#[allow(unused_imports)]
use crate::markdown::parse::{
    build_line_starts_u32, parse_block_window, parse_block_window_rope, parse_to_rows,
    scan_blocks, scan_citations, BlockIndex, CitationIndex, Row, RowKind, RowPayload, RunKind, Toc,
};
#[cfg(test)]
#[allow(unused_imports)]
use crate::syntax::{parse_code_lines, CodeLineRuns};
#[cfg(test)]
#[allow(unused_imports)]
use crate::editor::rope_ext::RopeExt;
#[cfg(test)]
#[allow(unused_imports)]
use crate::markdown::layout::docrow_for_height;
#[cfg(test)]
#[allow(unused_imports)]
use crate::markdown::layout::shape_docrow_shard;
#[cfg(test)]
#[allow(unused_imports)]
use crate::markdown::wysiwyg::WyTheme;
#[cfg(test)]
#[allow(unused_imports)]
use dimensions::{image_dimensions, image_dimensions_bytes};
#[cfg(test)]
#[allow(unused_imports)]
use row::{render_row_shard, PreviewRow, RowRenderCtx};
#[cfg(test)]
#[allow(unused_imports)]
use style::StyleParams;

#[cfg(test)]
mod tests;
