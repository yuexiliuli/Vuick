//! gpui-editor 的 **L0 纯 UI 组件叶子 crate**（2026-11-11 拆分）。
//!
//! 只放「不依赖 EditorApp / 外壳编排」的纯 UI 组件，供 shell 各域（及后续拆分）复用：
//!
//! - [`scrollbar`]：自绘横向滚动条（`TableScrollState` / `horizontal_scrollbar`，
//!   原 shell/src/scrollbar.rs 整体迁入；零 EditorApp 依赖，仅 app 消费）。
//! - [`TabBoundsRecorder`]：选项卡边界记录器（原 shell/src/app/tabs.rs 迁入，
//!   包在选项卡元素外，每帧把真实 laid-out 尺寸 / 边界写入 `Rc<Cell<..>>`）。
//!
//! 纯结构移动：只重分配既有 workspace 依赖，不新增外部包；行为零变。

pub mod scrollbar;

mod tab_bounds_recorder;
pub use tab_bounds_recorder::TabBoundsRecorder;
