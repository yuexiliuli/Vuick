//! 选项卡栏域 crate（2026-11-11 自 `crates/shell` 的 `app/tabs.rs` 拆出）。
//!
//! 纯结构移动 + 域内解耦：把选项卡栏的**渲染与交互**（`render_tab_bar` / 每个选项卡
//! 元素 / 眼睛预览钮 / 横向滚动条接线 / 栏级与选项卡级右键菜单项构建）从 `EditorApp`
//! 的 `impl` 块整迁到 [`bar::TabBar`] 实体（泛型宿主 `H`），使这些逻辑可独立编译 /
//! 独立测试。宿主 `EditorApp` 只保留：
//!
//! - **状态字段**：`tab_bounds`（拖拽排序命中边界槽）、`tab_bar_hscroll`（横向滚动条
//!   状态，`app/tests/tab_bar.rs` 直读）、`tab_hscroll_delay`（自动隐藏计时）、
//!   `tab_bar_menu_*` 三元组（右键菜单槽位，`app/tests/{tabs_actions,tabs_context}.rs`
//!   直读；media/pdf 前置弹窗互关亦直清宿主槽位）仍留宿主管辖（被测试直读 +
//!   cx-less 前置清理多处，无法整体入实体）。
//! - **宿主服务面**：选项卡生命周期动作（激活 / 关闭 / 批关闭 / 跨窗 move·copy /
//!   新建 / 打开）、预览切换、横向滚动条点亮、窗外幽灵窗预建 与右键菜单槽位落地
//!   统一经泛型宿主 [`host::TabsHost`] 反调宿主（对照 `shell_sidebar::SidebarHost` /
//!   `shell_preview::PreviewHost` 先例）；跨窗口拖拽的「真正执行」经 [`drag::TabDragHost`]
//!   反调宿主；域不直读 `EditorApp`。
//! - **留宿主的薄适配**：选项卡生命周期动作的**模型操作**已在 `shell_core::AppModel`
//!   （make/activate/close/remove/reorder/批关闭/tab_drag payload 半），宿主 `tabs.rs`
//!   保留 glue 转发；tab_drag 的**纯逻辑**（`tab_insert_index` / `tab_drag_over_row` /
//!   幽灵窗布局参数 / 拖拽状态机 / 拆分决策）已迁 [`drag`]，宿主编排（拖出拆分窗口 /
//!   HWND / 注册表 / 幽灵窗实体）留 shell `app/tab_drag.rs` 经 [`drag::TabDragHost`] 分发。
//!
//! ## 模块
//!
//! - [`bar`]：[`TabBar<H>`] 实体——选项卡栏渲染 / 选项卡元素 / 眼睛预览钮 / 横向滚动条
//!   接线 / 栏级·选项卡级右键菜单项构建（菜单槽位经 [`host::TabsHost`] 落宿主）。
//! - [`host`]：[`TabsHost`] 宿主服务面。
//! - [`drag`]：跨窗口选项卡搬迁与拖拽的**域内纯逻辑**（拖拽状态机单步迁移 / 命中计算 /
//!   幽灵窗布局参数 / 拆分决策）+ [`drag::TabDragHost`] 宿主服务面（建窗 / 注册表 /
//!   HWND / 幽灵窗实体创建销毁 / adopt·restore 目标窗操作，宿主侧在 shell
//!   `app/host_impl.rs` 实现）。
//!
//! 依赖只重分配 workspace 既有包（gpui / ui / shell_core / shell_widgets），不新增外部包，
//! 不依赖 shell_preview/shell_sidebar 等其它域（跨域调用经宿主服务分发）。

pub mod bar;
pub mod drag;
pub mod host;

pub use bar::TabBar;
pub use drag::TabDragHost;
pub use host::TabsHost;
