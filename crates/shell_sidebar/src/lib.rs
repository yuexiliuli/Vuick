//! 侧边栏域 crate（2026-11 自 `crates/shell` 的 `app/sidebar` 拆出）。
//!
//! 纯结构移动 + 域内解耦：把侧边栏的**状态 / 外壳（chrome）渲染 / 目录（toc）视图 /
//! 文件树输入弹层（FileTreePrompt）**从 `EditorApp` 的 `impl` 块中整迁出来，使这些
//! 逻辑可独立编译 / 独立测试，宿主 `EditorApp` 只保留：
//!
//! - **状态**：`sidebar: SidebarChromeState` 仍作 `EditorApp` 的普通字段（Pub 重导出
//!   保持 `app::sidebar::…` 与测试既有路径；调用面零变化）。
//! - **实体接线**：`sidebar_view: Entity<Sidebar<EditorApp>>` 承接外壳 / 目录的渲染与
//!   事件（`Context<Sidebar>` 内 `cx.listener` 绑定域实体），宿主渲染期以
//!   `&mut SidebarChromeState` 传入（状态在宿主、渲染在域内，行为与既有一致）。
//! - **宿主服务面**：事件回调改状态统一经 [`host::SidebarHost`]（`WeakEntity<H>` 反调，
//!   对照 `session_store::RecentPanelHost` 先例）；状态**纯变换**收在
//!   [`state`]（`toggle_view` / `resize_*` / `toc_collapse`），宿主侧零逻辑。
//!
//! ## 模块
//!
//! - [`state`]：纯状态与类型——[`SidebarChromeState`] / [`SidebarView`] / [`FileTreeState`]
//!   / [`TreeEntry`] / [`FileRow`] / 常量 / `read_dir_entries` / 状态纯变换。
//! - [`host`]：[`SidebarHost`] 宿主服务面（渲染期数据预取 + 事件回调的状态迁移）。
//! - [`chrome`]：[`Sidebar`] 实体——侧栏外壳（活动栏 / 右缘拖拽调宽手柄）渲染与调宽状态机。
//! - [`toc`]：[`Sidebar`] 实体的目录视图渲染与折叠逻辑（Markdown 标题 / PDF 书签）。
//! - [`prompt`]：[`FileTreePrompt`] 输入弹层实体（泛型宿主 [`FileTreePromptHost`]）。
//! - [`files`]：[`Sidebar`] 实体的文件树视图（懒加载扫描 / 展开折叠 / 过滤 typeahead / 打开）。
//! - [`files_ops`]：[`Sidebar`] 实体的文件树操作（右键菜单 / 输入弹层生命周期）；文件系统
//!   副作用经 [`host::SidebarHost`] / [`prompt::FileTreePromptHost`] 分发宿主。
//!
//! `search.rs` 仍在 shell（下一阶段随迁）；本 crate 只承载上述「地基 + 视图主体」。依赖
//! 只重分配 workspace 既有包（gpui / ui / config / shell_core / toc / pdf / markdown_wysiwyg），
//! 不新增外部包。

pub mod chrome;
pub mod files;
pub mod files_ops;
pub mod host;
pub mod prompt;
pub mod state;
pub mod toc;

pub use chrome::Sidebar;
pub use host::SidebarHost;
pub use prompt::{FileTreePrompt, FileTreePromptHost, FileTreePromptKind};
pub use state::{
    file_tree_invalidate, file_tree_scan_complete, file_tree_toggle, read_dir_entries,
    normalize_search_query, FileRow, FileTreeState, SidebarChromeState, SidebarView, TreeEntry,
    FILE_TREE_INDENT, FILE_TREE_ROW_H, FILES_FILTER_DEBOUNCE_MS, SEARCH_DEBOUNCE_MS,
};
// 目录虚拟化常量（文件树视图共用，`pub` 供宿主按需重导出）。
pub use toc::{TOC_BAND_BUFFER, TOC_VIRTUALIZE_MIN};
