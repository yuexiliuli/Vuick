//! 应用级动作类型与本地快捷键**组合根**。
//!
//! 动作类型与编辑区输入键位绑定已下沉到 `gpui_editor_base::actions`（叶子），
//! 本模块 re-export 保持 `crate::actions::NewFile` 等旧路径；`bind_local_keymaps`
//! 作为「编辑区 Input + 窗口根 Root + WYSIWYG wy-editor」的组合仍定义在根包
//! （后续 shell crate 吸收时整体上移）。

/// 动作类型（editor_app / editor_input 两组）+ [`bind_input_keys`] 的 re-export，
/// 保持 `crate::actions::X` 与 `crate::actions::bind_input_keys` 路径不变。
pub use gpui_editor_base::actions::*;

/// 安装全部本地快捷键（编辑区 Input + 窗口根 Root + WYSIWYG wy-editor 三种键上下文）。
///
/// 替代已移除的 `gpui_component::init` 的按键映射职责，供 `main.rs` 与测试初始化共用。
/// 调用方须在随后注册比 `Input` 上下文更高优先级的覆盖绑定（如 `ctrl-v` →
/// `PasteNormalized`），顺序＝优先级。
pub fn bind_local_keymaps(cx: &mut gpui::App) {
    gpui_editor_base::actions::bind_input_keys(cx);
    crate::ui::root::bind_keys(cx);
    crate::markdown::wysiwyg::bind_keys(cx);
}
