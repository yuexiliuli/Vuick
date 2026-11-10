//! 单元测试（自 mod.rs 拆出，仅测试编译）。
// Split by theme (M2.4): include! splices text within the tests module, so test paths/count stay identical to before.
// P1（2026-11-10 测试域化）已把首轮低外提域迁出为独立 integration target（crates/shell/tests/）：
// config_isolation / recent / session(+settings_xfer) / tray_menu / bench —— 见 tests/ 对应文件；
// P1 中批（2026-11-10 §8.3）继续迁出 find_goto / pdf / epub / palette / media —— 见 tests/ 对应文件；
// P1 收尾（2026-11-10 §8.4 步 3）再迁出外提面低于阈值者 search_scan（0 符号）/ splitter（5 只读字段）
// —— 见 tests/ 对应文件。
// 此处只保留仍在 lib 单测的高外提域（tabs/panes/sidebar/preview 族），setup.rs 仍被本模块
// include（余下域共用夹具），tests/common/mod.rs 是与 setup.rs 同源的 integration 版夹具。
include!("tests/setup.rs");
// P1 冻结：外提面过高（tabs_actions，约 8 符号——5 状态方法 active_media_locked/show_preview/
// change_font_size/new_tab/paste_normalized + 3 只读字段 open_dialog_calls/media_lock_menu_pos/
// tab_bar_menu_pos），见 specs/2026-11-10-shell-split-design.md §8.4
include!("tests/tabs_actions.rs");
// P1 冻结：外提面过高（tabs_context，约 7+ 符号——6 状态方法 tab_close_*/tab_move_*/tab_copy_*
// + is_detached 字段 + 与 panes 共享 registry_snapshot），见 specs/2026-11-10-shell-split-design.md §8.4
include!("tests/tabs_context.rs");
// P1 冻结：外提面过高（sidebar，约 14 符号——8 状态方法等 + FolderSearchScan/FileTreePromptKind
// 私有类型 + 深层树/折叠状态），见 specs/2026-11-10-shell-split-design.md §8.4
include!("tests/sidebar.rs");
// P1 冻结：外提面过高（preview，约 10 符号——4 状态方法 ensure_md_wy/md_wy_install_full_swap/
// md_wy_spawn_prefix_install/toggle_live_test + 6 私有 LIVE_COMMIT_* 原子计数），
// 见 specs/2026-11-10-shell-split-design.md §8.4
include!("tests/preview.rs");
// P1 冻结：外提面过高（scroll_sync，约 3 符号但为状态方法 change_font_size/toggle_live_test
// + 常量，非只读无状态；归 preview 族留 lib），见 specs/2026-11-10-shell-split-design.md §8.4
include!("tests/scroll_sync.rs");
// P1 冻结：外提面过高（tab_bar，约 3 符号但含 tab_bar_hscroll 深层 TableScrollState
// handle/visible 私有滚动条状态），见 specs/2026-11-10-shell-split-design.md §8.4
include!("tests/tab_bar.rs");
// P1 冻结：外提面过高（file_open，约 5 符号——1 状态方法 save_tab + 3 私有 PROGRESSIVE_OPEN_*
// 常量 + pub(crate) open_fast_count 字段），见 specs/2026-11-10-shell-split-design.md §8.4
include!("tests/file_open.rs");
// P1 冻结：外提面过高（file_tree，约 2 方法 + 与 sidebar 共享 ft_wait_tree_loaded 夹具耦合，
// sidebar 冻结则 file_tree 无法独立），见 specs/2026-11-10-shell-split-design.md §8.4
include!("tests/file_tree.rs");
// P1 冻结：外提面过高（panes，约 5 符号——状态方法 restore_tab_to_primary + is_detached/
// drag_ghost 深层私有状态 + tab_drag 私有几何助手 + 与 tabs_context 共享 registry_snapshot），
// 见 specs/2026-11-10-shell-split-design.md §8.4
include!("tests/panes.rs");
