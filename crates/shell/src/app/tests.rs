//! 单元测试（自 mod.rs 拆出，仅测试编译）。
// Split by theme (M2.4): include! splices text within the tests module, so test paths/count stay identical to before.
include!("tests/setup.rs");
include!("tests/tabs_actions.rs");
include!("tests/tabs_context.rs");
include!("tests/sidebar.rs");
include!("tests/search_scan.rs");
include!("tests/preview.rs");
include!("tests/scroll_sync.rs");
include!("tests/tab_bar.rs");
include!("tests/file_open.rs");
include!("tests/media_window.rs");
include!("tests/music.rs");
include!("tests/file_tree.rs");
include!("tests/pdf_epub.rs");
include!("tests/pdf_phaseb.rs");
include!("tests/epub_wy.rs");
include!("tests/bench.rs");
include!("tests/panes.rs");
include!("tests/palette.rs");
include!("tests/splitter.rs");
include!("tests/session.rs");
include!("tests/find.rs");
include!("tests/goto.rs");
include!("tests/close_tray.rs");
include!("tests/recent.rs");
include!("tests/config_isolation.rs");
include!("tests/settings_xfer.rs");
