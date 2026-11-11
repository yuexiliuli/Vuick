//! 预览域**宿主服务面**。
//!
//! 预览域对宿主的交互收敛为最小接口：域逻辑（聚合 `AppModel` 模型态 + `WyEditor` 实体 +
//! gpui 上下文）不依赖 shell 的具体宿主类型；凡**必须落宿主的副作用**（会话持久化 /
//! 字号缩放入口 / 拖放打开文件）经本 trait 反调宿主 `EditorApp`。shell 侧以
//! `impl shell_preview::PreviewHost for EditorApp` 提供实现（`app/host_impl.rs`，方法体薄转发）。
//!
//! 三个方法都是 `cx.listener` 回调 `&mut H`（订阅 / 事件回调）内调用，不涉及域函数自身的
//! `Context<H>` 回借，无双借风险。
//!
//! ## 借用约束
//!
//! 域侧调用点都在「宿主实体未被租用」的回调体（WyEditor 变更订阅 / Ctrl+滚轮缩放 /
//! 预览拖放）内经 `cx.listener` 或 `cx.subscribe` 的 `&mut H` 直调——不持有 `AppModel`
//! 长借用可复用，故允许实现读/写模型（与 media `MediaHost` 同款）。

use gpui::{Context, ExternalPaths, Window};

/// 预览域**宿主服务面**：把预览域对宿主的副作用收敛为最小接口，使本 crate 不依赖 shell
/// 的具体宿主类型。shell 侧 `impl PreviewHost for EditorApp` 在 `app/host_impl.rs`。
pub trait PreviewHost: 'static {
    /// WYSIWYG 活区（`live2`）内容变更 → 防抖会话持久化（debounce 300ms 落盘一次；
    /// 结构变化即时的判定在宿主 session 模块）。域 `live::enter_live` 的 `WyEvent::Changed`
    /// 订阅回调里经 `&mut H` 直调——宿主转发既有 `schedule_session_flush(cx, false)`。
    fn preview_schedule_session_flush(&mut self, cx: &mut Context<Self>)
    where
        Self: Sized;

    /// Ctrl+滚轮缩放字号（编辑区 / 预览区 / WYSIWYG 共用 `zoom_overlay` 的唯一缩放入口）：
    /// 宿主转发既有 `change_font_size(delta, cx)`（含字号持久化 + notify），域不重复实现。
    fn preview_change_font_size(&mut self, delta: f32, cx: &mut Context<Self>)
    where
        Self: Sized;

    /// 预览区拖放文件 → 打开（`md_wy_host_children` 的 `can_drop`/`on_drop` 落地）：
    /// 宿主转发既有 `open_dropped_paths(paths, window, cx)`。仅当宿主对 `ExternalPaths`
    /// 的打开语义与本窗口一致时实现；测试宿主可置空实现。
    fn preview_open_dropped_paths(
        &mut self,
        paths: &ExternalPaths,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) where
        Self: Sized;
}
