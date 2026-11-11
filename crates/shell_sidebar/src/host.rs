//! 侧边栏**宿主服务面**（2026-11-11，对照 `session_store::RecentPanelHost` 先例）。
//!
//! 侧栏实体 [`crate::chrome::Sidebar`] 与 [`crate::toc`] 在**事件回调**（活动栏点击 /
//! 调宽手柄 / 目录折叠 / PDF 书签删除）里改的侧栏状态不在域之内（`SidebarChromeState`
//! 是 `EditorApp::sidebar` 普通字段），统一经本 trait 以弱引用反调宿主落地；宿主侧实现
//! 只调 [`crate::state`] 的**状态纯变换** + `notify`，零业务逻辑。
//!
//! ## 渲染期禁入宿主（硬约束）
//!
//! 侧栏外壳 / 目录内容在宿主 `EditorApp::render` 内经
//! `self.sidebar_view.update(cx, …)` 构建——此时宿主实体正被 render 可变借用，**不得**
//! 再经本 trait 反调宿主（实体.map 读/写会二次借用 panic）。渲染期宿主把「域外数据」
//! 以参数预取传入（如 TOC 的 PDF 书签），域侧只在**事件回调**（宿主 render 已结束）里
//! 反调。作业顺序：宿主编排先预取 → 调域渲染；域事件回调 → 反调宿主办状态。
//!
//! 方法均带 `where Self: Sized` 者与 `RecentPanelHost` 同约束（实现 `Context<Self>`），
//! 纯 `&self` 取值方法（[`Self::notify_host`]）无此约束。

use std::path::Path;

use gpui::Context;

use crate::state::SidebarView;

/// 侧边栏宿主抽象：把侧栏对宿主的交互收敛为最小接口，使域 crate（shell_sidebar）不
/// 依赖 shell 的具体宿主类型。shell 侧以 `impl SidebarHost for EditorApp` 提供实现
/// （方法转发到 [`crate::state`] 状态纯变换 / 既有文件操作 + `notify`，行为不变）。
///
/// ## 再入纪律（硬约束）
///
/// 本 trait 方法只从**域实体的事件回调**（`cx.listener` 在元素事件派发中被调用，此刻
/// 宿主实体未被借用）经 `WeakEntity<H>::update` 反调宿主；宿主实现 `&mut Context<Self>`
/// 直接操作 `self.sidebar` + `cx.notify()`（`notify` 自带，不另设通知方法）。宿主侧**不得**
/// 在自身已持有 `&mut EditorApp` 的上下文（如 `app.update` 测试闭包 / 外层事件 listener）
/// 调用本 trait —— 那会二次 `Entity<H>::update` 触发 RefCell 双借 panic；这些路径改走
/// [`crate::state`] 纯变换 + 宿主自身 `cx.notify()`。
pub trait SidebarHost: 'static {
    /// 活动栏按钮点击：切换视图 / 面板开合（state 纯变换 `toggle_view`，宿主落在
    /// `self.sidebar` 上再 notify）。
    fn sidebar_toggle_view(&mut self, view: SidebarView, cx: &mut Context<Self>)
    where
        Self: Sized;

    /// 调宽手柄 hover：置 `resize_hovered` 高亮态（state 纯变换 + notify）。
    fn sidebar_set_resize_hovered(&mut self, hovered: bool, cx: &mut Context<Self>)
    where
        Self: Sized;

    /// 调宽手柄**按下**：开始拖拽（state 纯变换 `resize_start` + notify）。
    /// 拖拽中 / 结束（`resize_update` / `resize_end`）由宿主外层 `on_mouse_move/up(_out)`
    /// 持 `&mut self.sidebar` 直接走 `state` 纯变换 + 宿主 `persist_prefs`，不经本 trait
    /// （那些路径宿主已持 `&mut EditorApp`，再入 trait 会双借 panic）。
    fn sidebar_resize_start(&mut self, start_x: f32, cx: &mut Context<Self>)
    where
        Self: Sized;

    /// 目录折叠集切换（state 纯变换 `toc_collapse` + notify）。`has_child` 由域侧经模型
    /// 预计算传入（域不读宿主字段）。
    fn sidebar_toc_collapse(&mut self, idx: usize, has_child: bool, cx: &mut Context<Self>)
    where
        Self: Sized;

    /// PDF「书签」区段删除一条（宿主既有 `remove_pdf_bookmark`：改模型 + 落盘 + notify）。
    fn sidebar_remove_pdf_bookmark(&mut self, path: &Path, idx: usize, cx: &mut Context<Self>)
    where
        Self: Sized;
}
