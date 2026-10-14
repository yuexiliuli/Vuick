// ═══ M4 跳过归档：本仓 P1 冷启动 emoji 缺陷的**永久驻留防御**（勿删）═══
// P1 冷启动 emoji：Windows COLR 光栅失败被字模缓存钉住，上游 zed（含 93d7456）未修；
// M4 vendor gpui_windows+[patch] 已决定跳过 → 本组件为本仓永久驻留防御，勿删；若升级
// gpui 或引入 vendor 修复可整体移除并删除本注释。
// 拆 workspace 时把实现整体迁入 `base::warm`（进程级 COLR 光栅预热，
// 一次/进程），本模块保留为薄转发（`markdown::wysiwyg::warm_emoji_*` 路径不变）：
// - `base::warm` 持有真实实现 + 完整技术注释（含「勿删」与实测结论），改前先读；
// - 与 cache.rs 的 `NotdefRetry` 状态机、frame/mod.rs Pass2 的 notdef/回退校验桥接
//   同属一套防御体系（改前先读 `WY_NOTDEF_DEBUG` / `WY_WARM_DEBUG` 实测取证，
//   `docs/debug-envs.md`）。
use gpui::Window;

/// 进程级 warm 是否已完成（转发 [`base::warm::warm_emoji_is_done`]）。
pub(crate) fn warm_emoji_is_done() -> bool {
    base::warm::warm_emoji_is_done()
}

/// Windows 冷启动 color-emoji 光栅预热（一次/进程，**共享给所有文本窗格**）。
///
/// 薄转发：真实实现见 [`base::warm::warm_emoji_rasterizer`]，逻辑逐行
/// 未动。`tag` 标识触发窗格（editor / wysiwyg，仅调试日志用）。
pub(crate) fn warm_emoji_rasterizer(window: &mut Window, tag: &'static str) {
    base::warm::warm_emoji_rasterizer(window, tag);
}
