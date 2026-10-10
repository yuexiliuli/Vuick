//! 视图层测试：行级虚拟化 / 横向滚动 / 表格 / 目录 / 图片 / 字号缩放 的回归与冒烟测试。
// Split by theme (M2.4): include! splices text within the tests module, so test paths/count stay identical to before.
include!("tests/viewport.rs");
include!("tests/render.rs");
include!("tests/toc_image.rs");
include!("tests/bench.rs");
include!("tests/scroll_wrap.rs");
include!("tests/code_block_palette.rs");
include!("tests/windowed.rs");
include!("tests/epub_select.rs");
include!("tests/bench_preview.rs");
include!("tests/gap.rs");
