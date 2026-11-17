//! vuick 的**应用外壳 crate**。
//!
//! 承载 `EditorApp` 主视图
//! （自绘标题栏 + 菜单栏 + 选项卡栏 + 编辑区 + 可选 Markdown 预览 + 状态栏）。
//! 自绘横向滚动条 `TableScrollState` / `horizontal_scrollbar` 与选项卡边界记录器
//! `TabBoundsRecorder` 在 L0 纯 UI 组件 crate `shell_widgets`（本 shell 仅消费）。
//!
//! ## 结构
//!
//! - [`app`]：应用主视图 `EditorApp`（类型 / 状态 / 共享 prelude 在 `app::mod`，
//!   方法按职责拆分到 `init`/`sync`/`preview`/`media`/`chrome`/`layout`/`file_ops`/
//!   `open_progress`/`menu`/`palette`/`panes`/`sidebar`/`status_bar`/`tabs` 等子模块）
//! - [`bind_local_keymaps`]：完整本地快捷键组合（编辑区 Input + 窗口根 Root +
//!   WYSIWYG wy-editor 三套键上下文）
//!
//! 每个窗口根视图为 `ui::root::Root`（自研，替代 gpui-component `Root`），由
//! 二进制壳 `vuick` 的 `main` 装配；本 crate 只提供视图与键位。
//!
//! 依赖关系：本 crate 依赖全部功能 crate（config/base/syntax/ui/editor/markdown/
//! toc/pdf/image/video(feature)/epub），是**唯一「依赖所有人」的顶层**；二进制只挂在本 crate 之上。

pub mod app;

/// 公共命令表在独立 crate `shell_commands`（菜单 / 命令面板 / 根级快捷键共享
/// label 与快捷键常量，[`commands::root_keybindings`] 供二进制壳 `src/main.rs` 绑定）。
/// 此处经 `commands` 别名重导出，维持既有 `shell::commands::*` 路径。
pub use shell_commands as commands;

/// 应用主视图 `EditorApp`（文本 / 代码 / Markdown 编辑器主窗口）。
pub use app::EditorApp;
/// 别名：`crate::prof::` → `base::prof`。
use base::prof;
/// 别名：`crate::actions::` → `base::actions`（动作类型在 base）。
pub use base::actions;

/// 安装全部本地快捷键（编辑区 Input + 窗口根 Root + WYSIWYG wy-editor 三种键上下文）。
///
/// `bind_local_keymaps` 是**组合根**：`bind_input_keys`（base，Input 键上下文）+
/// `ui::root::bind_keys`（Root 键上下文）+ `markdown_wysiwyg::bind_keys`（wy-editor 键
/// 上下文）。供二进制壳 `main.rs` 与测试初始化共用。调用方须在随后注册比 `Input` 上下文
/// 更高优先级的覆盖绑定（如 `ctrl-v` → `PasteNormalized`），顺序＝优先级。
pub fn bind_local_keymaps(cx: &mut gpui::App) {
    base::actions::bind_input_keys(cx);
    ui::root::bind_keys(cx);
    markdown_wysiwyg::bind_keys(cx);
}
