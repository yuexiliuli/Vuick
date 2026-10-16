//! `markdown-wysiwyg` 自身的测试键位助手（仅 `cfg(test)` 可见）。
//!
//! 自 `markdown` crate 的 `test_support` 迁入：原组合 =
//! `base::actions::bind_input_keys`（Input 键上下文）+ `markdown::wysiwyg::bind_keys`
//! （wy-editor 键上下文），等价于根包 `bind_local_keymaps` 去掉 Root 部分。
//! 拆分后 `bind_keys` 即本 crate 的 `crate::wysiwyg::bind_keys`，直接调用即可；
//! `markdown` core 已无 wy 键位需求，其 `test_support` 随本迁移删除。

use gpui::App;

/// 安装本 crate 测试所需的本地快捷键（Input + wy-editor 两套键上下文）。
pub fn bind_local_keymaps(cx: &mut App) {
    base::actions::bind_input_keys(cx);
    crate::wysiwyg::bind_keys(cx);
}
