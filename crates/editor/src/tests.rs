//! 编辑器组件测试： 纯逻辑（buffer/selection/movement/undo/highlight）+ 视图
//! （渲染、输入、撤销、选区、鼠标）。
// Split by theme (M2.4): include! splices text within the tests module, so test paths/count stay identical to before.
include!("tests/logic.rs");
include!("tests/view.rs");
include!("tests/find.rs");
include!("tests/goto.rs");
include!("tests/tab.rs");
