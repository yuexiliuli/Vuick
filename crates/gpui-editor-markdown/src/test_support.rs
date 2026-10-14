//! `gpui-editor-markdown` 自身的测试键位助手（仅 `cfg(test)` 可见）。
//!
//! markdown crate 独立后其测试不能再调根包的 `crate::actions::bind_local_keymaps`
//! （组合函数含 `ui::root::bind_keys` 且定义在根包，会形成依赖反转）。
//! 本助手 = `gpui_editor_base::actions::bind_input_keys`（Input 键上下文）+
//! `markdown::wysiwyg::bind_keys`（wy-editor 键上下文），等价于原组合去掉
//! Root 部分——markdown 测试只用这两套上下文（WYSIWYG 内嵌 CodeEditor 的控制键
//! 在 `on_key_down` 直接自理，不依赖 keymap/action 注册）。

use gpui::App;

/// 安装 markdown 测试所需的本地快捷键（Input + wy-editor 两套键上下文）。
pub fn bind_local_keymaps(cx: &mut App) {
    gpui_editor_base::actions::bind_input_keys(cx);
    crate::wysiwyg::bind_keys(cx);
}
