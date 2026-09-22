//! 自研 Markdown 渲染器（行级虚拟化版）+ 所见即所得编辑（WYSIWYG）。
//!
//! - `parser`：手写行扫描器，把源码逐行转成渲染行
//! - `view`：基于 gpui `List` 的行级虚拟化 + 置顶滚动条
//! - `wysiwyg`：本项目自研 WYSIWYG 编辑器（[`WyEditor`]，auto-grow + 宿主滚动；
//!   元素级所见即所得，直接编辑 `Doc` 数据源）。已取代 zorite 移植的 `live`。
//! - `recognition` / `inline_style`：markdown 构造识别 / 行内样式扫描（自 `live`
//!   迁出，`wysiwyg` 与预览 `view` 共用同一套识别）。
//!
//! 相比直接使用 gpui-component 的 `TextView`：
//! - 代码块 / 长文档天然按行虚拟化，只构建 / 绘制可见行；
//! - 滚动条由我们自己作为最上层子元素叠加，不再被不透明背景盖住。

mod parser;
pub(crate) mod view;

mod cells;
mod doc;
mod ir;
pub(crate) mod recognition;
pub(crate) mod inline_style;
pub(crate) mod wysiwyg;

#[cfg(test)]
mod parity;

pub use parser::{scan_blocks_incremental_rope, scan_blocks_rope, BlockIndex, RowAlign};
#[cfg(test)]
pub use parser::scan_blocks;
pub use view::{MarkdownPreview, PreviewSrc};
pub use wysiwyg::{WyEditor, WyEvent};
