//! 自研 Markdown 渲染器（行级虚拟化版）+ 实时预览编辑（WYSIWYG）。
//!
//! - `parser`：手写行扫描器，把源码逐行转成渲染行
//! - `view`：基于 gpui `List` 的行级虚拟化 + 置顶滚动条
//! - `live`：从 zorite `gpui-editor` 移植的 WYSIWYG Markdown 编辑器
//!   （[`MarkdownLivePreview`]，auto-grow + 宿主滚动；启用即元素级所见即所得）
//!
//! 相比直接使用 gpui-component 的 `TextView`：
//! - 代码块 / 长文档天然按行虚拟化，只构建 / 绘制可见行；
//! - 滚动条由我们自己作为最上层子元素叠加，不再被不透明背景盖住。

mod live;
mod parser;
pub(crate) mod view;

mod cells;

#[cfg(test)]
mod parity;

pub use live::{bind_keys, EditorEvent, MarkdownLivePreview, SyntaxStyle};
pub use parser::{scan_blocks, scan_blocks_incremental_rope, scan_blocks_rope, BlockIndex, RowAlign};
pub use view::{MarkdownPreview, PreviewSrc};
