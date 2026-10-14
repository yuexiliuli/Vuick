//! gpui-editor 的**应用外壳 crate**（P5 顶层，最大迁移块）。
//!
//! 自根包 `src/app/`（31 文件）整体迁出的 crate：承载 `EditorApp` 主视图
//! （自绘标题栏 + 菜单栏 + 选项卡栏 + 编辑区 + 可选 Markdown 预览 + 状态栏）、
//! 以及原根级 `src/scrollbar.rs`（`TableScrollState` / `horizontal_scrollbar`，
//! 仅 app 消费的自绘美化横向滚动条）。
//!
//! ## 结构
//!
//! - [`app`]：应用主视图 `EditorApp`（类型 / 状态 / 共享 prelude 在 `app::mod`，
//!   方法按职责拆分到 `init`/`sync`/`preview`/`media`/`chrome`/`layout`/`file_ops`/
//!   `open_progress`/`menu`/`palette`/`panes`/`sidebar`/`status_bar`/`tabs` 等子模块）
//! - [`scrollbar`]：自绘横向滚动条（选项卡栏用）
//! - [`bind_local_keymaps`]：完整本地快捷键组合（编辑区 Input + 窗口根 Root +
//!   WYSIWYG wy-editor 三套键上下文），自原根包 `src/actions.rs` 上移而来
//!
//! 每个窗口根视图为 `ui::root::Root`（自研，替代 gpui-component `Root`），由
//! 二进制壳 `gpui-editor` 的 `main` 装配；本 crate 只提供视图与键位。
//!
//! 依赖关系：本 crate 依赖全部功能 crate（config/base/syntax/ui/editor/markdown/
//! toc/pdf/image/video(feature)/epub），是**唯一「依赖所有人」的顶层**；二进制只挂在本 crate 之上。

pub mod app;

/// 自绘横向滚动条（原根级 `src/scrollbar.rs`，仅 app 消费，随 shell 吸收）。
mod scrollbar;

/// 应用主视图 `EditorApp`（文本 / 代码 / Markdown 编辑器主窗口）。
pub use app::EditorApp;

// ---------------------------------------------------------------------------
// 迁移期 crate-root 短别名（与根包 P1–P4 同一策略）：
// 让本 crate 内既有的 `crate::config::` / `crate::ui::` / `crate::syntax::` /
// `crate::editor::` / `crate::markdown::` / `crate::epub::` / `crate::pdf::` /
// `crate::image::` / `crate::video::` / `crate::toc::` / `crate::prof::` /
// `crate::actions::` 旧路径在拆 crate 后零改动即可解析。
// ---------------------------------------------------------------------------
/// 迁移期别名：`crate::config::` → `gpui_editor_config`。
use gpui_editor_config as config;
/// 迁移期别名：`crate::ui::` → `gpui_editor_ui`。
use gpui_editor_ui as ui;
/// 迁移期别名：`crate::syntax::` → `gpui_editor_syntax`（仅本 crate 测试与非测试共用；
/// 非测试构建只在 doc 注释 [`crate::syntax`] 链接引用，故 `allow(unused_imports)`）。
#[allow(unused_imports)]
use gpui_editor_syntax as syntax;
/// 迁移期别名：`crate::editor::` → `gpui_editor_editor`。
use gpui_editor_editor as editor;
/// 迁移期别名：`crate::markdown::` → `gpui_editor_markdown`。
use gpui_editor_markdown as markdown;
/// 迁移期别名：`crate::epub::` → `gpui_editor_epub`。
use gpui_editor_epub as epub;
/// 迁移期别名：`crate::pdf::` → `gpui_editor_pdf`。
use gpui_editor_pdf as pdf;
/// 迁移期别名：`crate::image::` → `gpui_editor_image`。
use gpui_editor_image as image;
/// 迁移期别名：`crate::video::` → `gpui_editor_video`（随本 crate `video` feature）。
#[cfg(feature = "video")]
use gpui_editor_video as video;
/// 迁移期别名：`crate::toc::` → `gpui_editor_toc`。
use gpui_editor_toc as toc;
/// 迁移期别名：`crate::prof::` → `gpui_editor_base::prof`。
use gpui_editor_base::prof;
/// 迁移期别名：`crate::actions::` → `gpui_editor_base::actions`（动作类型已下沉 base）。
pub use gpui_editor_base::actions;

/// 安装全部本地快捷键（编辑区 Input + 窗口根 Root + WYSIWYG wy-editor 三种键上下文）。
///
/// 原根包 `src/actions.rs::bind_local_keymaps` 是**组合根**，P5 随 shell（app）整体
/// 上移至此：`bind_input_keys`（base，Input 键上下文）+ `ui::root::bind_keys`（Root 键
/// 上下文）+ `markdown::wysiwyg::bind_keys`（wy-editor 键上下文）。供二进制壳
/// `main.rs` 与测试初始化共用。调用方须在随后注册比 `Input` 上下文更高优先级的覆盖
/// 绑定（如 `ctrl-v` → `PasteNormalized`），顺序＝优先级。
pub fn bind_local_keymaps(cx: &mut gpui::App) {
    gpui_editor_base::actions::bind_input_keys(cx);
    gpui_editor_ui::root::bind_keys(cx);
    gpui_editor_markdown::wysiwyg::bind_keys(cx);
}

/// 测试用键位助手：与生产版 [`bind_local_keymaps`] 完全一致（完整组合），供
/// 本 crate 的 `app::tests` 初始化使用（原 `crate::actions::bind_local_keymaps`）。
#[cfg(test)]
pub mod test_support {
    /// 安装全部本地快捷键（= [`super::bind_local_keymaps`]），app 测试初始化用。
    pub fn bind_local_keymaps(cx: &mut gpui::App) {
        gpui_editor_base::actions::bind_input_keys(cx);
        gpui_editor_ui::root::bind_keys(cx);
        gpui_editor_markdown::wysiwyg::bind_keys(cx);
    }
}
