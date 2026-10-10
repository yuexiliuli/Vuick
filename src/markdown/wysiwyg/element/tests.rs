//! `element` 的单元测试。原内联于 `element.rs`，抽取后经 `#[path]` 挂在
//! [`element`] 下语义不变：`super::` 仍指 `element`。
// Split by theme (M2.4): include! splices text within the tests module, so test paths/count stay identical to before.
include!("tests/byte_map.rs");
include!("tests/wrapping.rs");
include!("tests/shaping.rs");
include!("tests/theme_reading.rs");
include!("tests/code_edit.rs");
include!("tests/hscroll_caret.rs");
include!("tests/table_grid.rs");
include!("tests/search_features.rs");
include!("tests/callout_fold.rs");
include!("tests/text_link.rs");
include!("tests/math.rs");
include!("tests/edit_cache.rs");
include!("tests/prewarm_invalidate.rs");
include!("tests/undo.rs");
include!("tests/bench.rs");
include!("tests/notdef.rs");
include!("tests/incremental.rs");
include!("tests/image.rs");
include!("tests/block_math.rs");
