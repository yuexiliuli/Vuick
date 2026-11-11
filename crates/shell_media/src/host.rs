//! 媒体域**宿主服务面**。
//!
//! 媒体窗口路由对宿主的交互收敛为最小接口：**纯决策**在 [`crate::media_window`]，路由的
//! **真正执行**（新开窗口 / 跨窗口打开）与 **「最近其它媒体窗口」查询**（`EditorAppRegistry`
//! 依赖是宿主内部实现）收敛到此 trait，使本 crate 不依赖 shell 的具体宿主类型。shell 侧
//! 以 `impl MediaHost for EditorApp` 提供实现（`app/host_impl.rs`，方法体薄转发宿主
//! 的 `open_media_in_new_window` / `route_media_to_window` /
//! `nearest_other_media_window`）。
//!
//! 本 trait 方法由宿主薄适配（`app::media_window::maybe_redirect_media_open`）**在自身
//! `&mut self` 上下文中调用**——不是 SidebarHost 式 `WeakEntity` 反调，而是「域定义服务
//! 契约、宿主编排期经 trait 方法分发执行」：`Context<H>` 拿不到 `&mut H`（`ui::menu`
//! §2.6 同款），路由执行又需同步改宿主窗口/状态，域内反调会双借 panic，故分发留在宿主。

use std::path::Path;

use gpui::{AnyWindowHandle, Context, Window};

/// 媒体域**宿主服务面**：把媒体路由对宿主的执行/查询收敛为最小接口，使本 crate 不依赖
/// shell 的具体宿主类型。shell 侧 `impl MediaHost for EditorApp` 在 `host_impl.rs`。
pub trait MediaHost: 'static {
    /// 「最近其它媒体窗口」查询：`EditorAppRegistry` 依赖收敛到宿主实现（域不直读注册表）；
    /// 返回**最近创建的其它媒体窗口**句柄（注册序最新 = 列表逆序首个，排除自身）。路由
    /// 决策「是否有其它媒体窗口可复用」据此判定（行为表 ReuseExisting 行）。
    fn media_host_nearest_other_media_window(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<AnyWindowHandle>
    where
        Self: Sized;

    /// 媒体路由「真正执行」：新开同进程独立窗口并打开该媒体（窗口构造 / 尺寸选择 / 路由
    /// 是宿主编排）。
    fn media_host_open_media_new_window(
        &mut self,
        path: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) where
        Self: Sized;

    /// 媒体路由「真正执行」：把媒体文件经目标窗口句柄转交该窗口宿主本体打开（跨窗口
    /// `open_file_here`）。
    fn media_host_route_media_to_window(
        &self,
        path: &Path,
        wh: AnyWindowHandle,
        cx: &mut Context<Self>,
    ) where
        Self: Sized;
}
