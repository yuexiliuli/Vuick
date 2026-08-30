//! Markdown parsing, selection, and GPUI rendering.
//!
//! This module remains the public facade so existing `crate::markdown::*`
//! imports are unaffected by the internal file layout.

mod model;
mod parser;
mod render;
mod selection;
mod text;

#[allow(unused_imports)]
pub(crate) use model::Block;
#[allow(unused_imports)]
pub use model::{MarkdownTheme, TableMode};
#[allow(unused_imports)]
pub(crate) use parser::parse_markdown;
#[allow(unused_imports)]
pub(crate) use render::render_markdown_blocks;
#[allow(unused_imports)]
pub use render::{render_markdown, render_markdown_themed, render_markdown_with_options};
#[allow(unused_imports)]
pub use selection::{selected_markdown, LeafCache, LeafGeom, SelectCtx};
#[allow(unused_imports)]
pub use text::break_long_runs;
