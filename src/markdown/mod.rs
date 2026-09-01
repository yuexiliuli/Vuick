//! 自研 Markdown 渲染器（行级虚拟化版）。
//!
//! - `parser`：手写行扫描器，把源码逐行转成渲染行
//! - `view`：基于 gpui `List` 的行级虚拟化 + 置顶滚动条
//!
//! 相比直接使用 gpui-component 的 `TextView`：
//! - 代码块 / 长文档天然按行虚拟化，只构建 / 绘制可见行；
//! - 滚动条由我们自己作为最上层子元素叠加，不再被不透明背景盖住。

mod parser;
mod view;

pub use view::MarkdownPreview;
