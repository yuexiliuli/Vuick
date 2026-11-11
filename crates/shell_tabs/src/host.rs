//! 选项卡栏**宿主服务面**（2026-11-11，对照 `shell_sidebar::SidebarHost` /
//! `shell_preview::PreviewHost` 先例）。
//!
//! 选项卡栏域对宿主的交互收敛为最小接口：域逻辑（[`crate::bar::TabBar`] 实体渲染 +
//! 共享 `AppModel` 模型态）不依赖 shell 的具体宿主类型；凡**必须落宿主的副作用**
//! （激活/关闭/批关闭/跨窗动作、预览切换、横向滚动条点亮、右键菜单槽位落地）经本
//! trait 反调宿主 `EditorApp`；拖拽开始预建窗外幽灵窗入口
//! `tabs_host_prime_drag_ghost` 已随 H2 迁至 [`crate::drag::TabDragHost`]。shell 侧以
//! `impl shell_tabs::TabsHost for EditorApp` 提供实现（`app/host_impl.rs`，方法体自原
//! `EditorApp` 既有方法薄转发，行为零变化）。
//!
//! ## 借用纪律
//!
//! 域侧调用点都在「宿主实体未被租用」的**事件回调**（选项卡元素 `mouse_down` /
//! `on_drag` / 滚轮 / 菜单项 `on_click`）内经 `WeakEntity<H>::update` 反调宿主——渲染期
//! （宿主正持 `&mut EditorApp`）**禁入宿主**（双借 panic，同 `SidebarHost` 硬约束）；
//! 渲染期所需的宿主只读数据（`show_preview` / 活动 tab 是否 Markdown）由宿主在调用域
//! 渲染前**以参数预取传入**（`TabBar::render_tab_bar` 的 `preview_on` / `active_is_md`）。

use gpui::{Context, Entity, Pixels, Point, Subscription, Window};
use ui::menu::PopupMenu;

/// 选项卡栏**宿主服务面**：把选项卡栏对宿主的副作用收敛为最小接口，使本 crate 不依赖
/// shell 的具体宿主类型。shell 侧 `impl TabsHost for EditorApp` 在 `app/host_impl.rs`。
pub trait TabsHost: 'static {
    /// 选项卡单击激活 → 宿主 `activate_tab`（模型置活动 + 持久化 + 聚焦编辑区/只读根）。
    fn tabs_host_activate_tab(
        &mut self,
        idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) where
        Self: Sized;

    /// × 关闭钮 / 菜单「关闭」→ 宿主 `close_tab`（干净命名折叠直通 remove；脏/未命名弹
    /// 原生确认框异步等结果 —— 宿主文件保存副作用）。
    fn tabs_host_close_tab(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>)
    where
        Self: Sized;

    /// 菜单「关闭其他」→ 宿主 `tab_close_others`（任一受影响脏/未命名 → 中止一个都不关）。
    fn tabs_host_close_others(
        &mut self,
        idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) where
        Self: Sized;

    /// 菜单「关闭右侧」→ 宿主 `tab_close_right`（idx 右侧全部；任一脏/未命名 → 中止）。
    fn tabs_host_close_right(
        &mut self,
        idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) where
        Self: Sized;

    /// 菜单「关闭已保存」→ 宿主 `tab_close_saved`（只关有路径且 !dirty）。
    fn tabs_host_close_saved(&mut self, window: &mut Window, cx: &mut Context<Self>)
    where
        Self: Sized;

    /// 菜单「关闭全部」→ 宿主 `tab_close_all`（任一脏/未命名 → 中止）。
    fn tabs_host_close_all(&mut self, window: &mut Window, cx: &mut Context<Self>)
    where
        Self: Sized;

    /// 菜单「移入新窗口」→ 宿主 `tab_move_to_new_window`（commit_live + 拆新窗 + 会话补刷）。
    fn tabs_host_move_to_new_window(
        &mut self,
        idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) where
        Self: Sized;

    /// 菜单「复制到新窗口」→ 宿主 `tab_copy_to_new_window`（文本快照 / 媒体按原路径重开）。
    fn tabs_host_copy_to_new_window(
        &mut self,
        idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) where
        Self: Sized;

    /// 「新建文件」菜单项 → 宿主 `new_tab`（建空 tab + 聚焦 + 持久化）。
    fn tabs_host_new_tab(&mut self, window: &mut Window, cx: &mut Context<Self>)
    where
        Self: Sized;

    /// 「打开文件」菜单项 → 宿主 `open_dialog`（系统原生打开对话框）。
    fn tabs_host_open_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>)
    where
        Self: Sized;

    /// 眼睛预览钮点击 → 宿主 `toggle_preview_for_tab`（激活 + 翻转 preview_on + 喂源/释放）。
    fn tabs_host_toggle_preview_for_tab(
        &mut self,
        idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) where
        Self: Sized;

    /// 选项卡区滚轮 → 点亮横向滚动条（宿主 `poke_tab_hscroll`：宿主字段 `tab_bar_hscroll`
    /// + 自动隐藏计时 `tab_hscroll_delay`）。
    fn tabs_host_poke_tab_hscroll(&mut self, cx: &mut Context<Self>)
    where
        Self: Sized;

    /// 右键菜单落地（宿主 `tab_bar_menu_*` 槽位）——测试直读宿主字段，media/pdf 前置
    /// 弹窗互关亦直清宿主槽位，故菜单槽位留宿主（域不持有菜单状态）。
    fn tabs_host_open_tab_bar_menu(
        &mut self,
        pos: Point<Pixels>,
        menu: Entity<PopupMenu>,
        sub: Subscription,
        cx: &mut Context<Self>,
    ) where
        Self: Sized;

    /// 右键菜单关闭清理（emitter 身份校验后清宿主槽位，防旧菜单的关闭误清新菜单）。
    fn tabs_host_dismiss_tab_bar_menu(&mut self, menu: &Entity<PopupMenu>, cx: &mut Context<Self>)
    where
        Self: Sized;
}
