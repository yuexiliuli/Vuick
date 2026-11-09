//! 文本打开 / 编码 / 差异 独立叶子 crate（自 shell 的 `app/encoding` / `app/open_progress` /
//! `app/diff` 纯结构拆出，2026-11）。
//!
//! - [`encoding`]：文本编码检测与「按原编码读写」（BOM 优先 / 合法 UTF-8 / GB18030 兜底）。
//! - [`open_progress`]：大文档「分块读盘 · 渐进装配」的**纯函数前端**（`ChunkReader` /
//!   `TextChunk`），与 UI 线程异步装配解耦。
//! - [`diff`]：WYSIWYG 写回的**行级差异**（有界 LCS → 少量替换 hunk）。
//!
//! 本 crate 不依赖视图层（gpui/editor 门面均不引用），消费方（shell）经 crate-root
//! 重导出短别名使用。
//!
//! # 边界
//!
//! 纯结构移动：函数体 / 算法 / 行为零改动。原 `pub(crate)` 接口随拆 crate 升为 `pub`
//! （本 crate 是它们的新归属），跨模块互引用由 `crate::app::encoding` 改为 `crate::encoding`。

pub mod diff;
pub mod encoding;
pub mod open_progress;

// 重导出接口（shell 等消费方经 crate-root 短别名访问，与拆 crate 前 `crate::app::*` 等价）。
pub use diff::line_diff;
pub use encoding::{decode_bytes, detect_source_encoding, encode_for_save, is_truncated_utf8_tail};
pub use open_progress::{ChunkBuilder, ChunkReader, TextChunk};
// 测试用同步收尾助手：随迁自 open_progress 的 `#[cfg(test)]`，保持 test-only 语义。
#[cfg(test)]
pub use open_progress::produce_open_chunks;
