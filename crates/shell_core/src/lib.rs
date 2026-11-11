//! 外壳宿主模型类型 crate（2026-11 自 `crates/shell` 的 `app/mod.rs` 拆出）。
//!
//! 纯结构移动（无函数体 / 算法 / 行为 / 接口语义改动）：把编辑器主视图 `EditorApp`
//! 的**纯模型类型**独立成可单独编译 / 单独测试的 crate，宿主的**编排（`impl EditorApp`）
//! 留在 shell**。本 crate 只交付五块模型：
//!
//! - [`model`]：一个文件选项卡的完整状态模型——[`EditorTab`] / [`DetachedTab`]
//!   （跨窗口整包搬迁）、媒体阅读器收敛枚举 [`MediaReader`]、WYSIWYG 预览「在途装配」
//!   状态包 [`MdWyInstall`] / [`MdWyEarly`]、选项卡拖拽数据 [`TabDragData`] /
//!   [`TabDragGhost`]、标题栏窗口控制钮 [`WinCtrlKind`] / [`TitleCtrlIconFg`]。
//!
//! 依赖只重分配 workspace 既有包：`gpui` / `ui` / `editor` / `markdown_wysiwyg` /
//! `pdf` / `image_viewer` / `video`（feature 转发）/ `encoding_rs`，不新增外部包。
//!
//! 宿主模型类型 `EditorAppRegistry`（持 `WeakEntity<EditorApp>`，随宿主 `EditorApp`
//! 一起，待 AppHost 抽象落地）与 `EditorApp` 本体留在 shell，不属本 crate；滚动同步 /
//! 面板分割 / 活区内衬 / 滚屏基准等纯值类型（[`MdScrollSync`] / [`MdTocScroll`] /
//! [`PanesState`] / [`LIVE_BODY_PAD_Y`]）已随 SC 下沉至 [`model`]，宿主以重导出维持路径。

pub mod actions;
pub mod file_ops;
pub mod host;
pub mod model;

pub use host::AppHost;
pub use model::{
    AppModel, CommitLiveAlign, DetachedTab, EditorTab, LIVE_BODY_PAD_Y, LIVE_COMMIT_OPS_COUNT,
    LIVE_COMMIT_OPS_REPLAYED, LIVE_COMMIT_SKIP_UNCHANGED, LIVE_COMMIT_WRITEBACK, MdScrollSync,
    MdTocScroll, MdWyEarly, MdWyInstall, MediaReader, PanesState, PdfBookmark, SearchScopeTab,
    TabDragData, TabDragGhost, TitleCtrlIconFg, WinCtrlKind,
};
