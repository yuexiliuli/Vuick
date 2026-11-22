//! 单元测试（仅测试编译）。
// include! 拼接各主题测试文件；测试路径/计数与拆分前一致。
// 库内保留高外提域的宿主侧测试（tabs / panes / sidebar / preview / 文件 / 搜索族）；
// 其余低外提域已迁出为独立 integration target（crates/shell/tests/）——见 tests/ 对应文件。
// setup.rs 仍被本模块 include（余下域共用夹具）；tests/common/mod.rs 是与 setup.rs 同源的
// integration 版夹具。
include!("tests/setup.rs");
include!("tests/tabs_actions.rs");
include!("tests/tabs_context.rs");
include!("tests/sidebar.rs");
include!("tests/preview.rs");
include!("tests/scroll_sync.rs");
include!("tests/tab_bar.rs");
include!("tests/file_open.rs");
include!("tests/file_tree.rs");
include!("tests/panes.rs");
include!("tests/about.rs");
