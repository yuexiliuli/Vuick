//! 文本输入控件模块。
//!
//! 解耦为两个组件 + 一层共享逻辑：
//! - [`text_box::TextBox`]：**单行文本框**（无换行/无滚动条，Enter 可提交），
//!   适合搜索框、标签输入等。
//! - [`text_area::TextArea`]：**多行文本域**（自动换行、自带滚动条、可视行导航），
//!   聊天/笔记等富文本框用。
//! - [`shared`]：两者复用的纯逻辑（动作类型、撤销栈、字素/词边界、UTF-16 换算、
//!   换行选区矩形），避免两处各自维护一份。

pub mod shared;
pub mod text_area;
pub mod text_box;

pub use shared::*;
pub use text_area::TextArea;
// TextBox 为二进制 crate 中"预留"的单行组件（暂无调用方），保持导出供将来接入。
#[allow(unused_imports)]
pub use text_box::TextBox;
