//! `gpui-editor-ui` 自身的测试键位助手（仅 `cfg(test)` 可见）。
//!
//! ui crate 独立后其测试不能再调根包的 `crate::actions::bind_local_keymaps`
//! （组合函数含 `markdown::wysiwyg::bind_keys` 且定义在根包，会形成依赖反转）。
//! 本助手 = `gpui_editor_base::actions::bind_input_keys`（Input 键上下文）+
//! `crate::root::bind_keys`（Root 键上下文），等价于原组合去掉 markdown 部分，
//! 覆盖 ui 测试真正用到的两套键上下文。

use gpui::App;

/// 安装 ui 测试所需的本地快捷键（Input + Root 两套键上下文）。
pub fn bind_local_keymaps(cx: &mut App) {
    gpui_editor_base::actions::bind_input_keys(cx);
    crate::root::bind_keys(cx);
}
