//! 代码块高亮解析的缓存键 / 状态 / 查询结果类型已随引擎迁移到
//! `markdown_wysiwyg_engine::code_parse`（类型不依赖宿主，纯结构移动）：本模块作
//! re-export 保持 `wysiwyg::code_parse::{CodeParseKey, CodeParseLookup, CodeParseState}`
//! 既有路径（`mod.rs` 第 94 行的 `pub(crate) use self::code_parse::{…}` 原样成立）。
pub use markdown_wysiwyg_engine::code_parse::{CodeParseKey, CodeParseLookup, CodeParseState};
