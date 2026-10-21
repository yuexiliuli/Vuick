//! 「关闭到系统托盘」开关：菜单点击切换 + 立即持久化 + 同步 `tray` crate 状态。
//!
//! 非 `windows` 目标由 `tray` crate 编译为 no-op → 本模块逻辑不变，行为自动降级为
//! 「点 × 直接退出」（不建托盘图标、`should_close_to_tray()` 恒 false）。

use super::*;

impl EditorApp {
    /// 「关闭到系统托盘」开关：翻转 - 同步托盘图标（`tray::set_close_to_tray`：
    /// 开启建图标 / 关闭撤图标，幂等；测试构建只翻 flag 不真建图标）- 立即持久化。
    /// `persist_prefs` 与 `tray::set_close_to_tray` 在测试构建都不写盘 / 不动 Win32
    /// （见 `chrome.rs` 与 `tray` crate 文档），故本函数在测试里只验证字段翻转（R3）。
    pub(super) fn toggle_close_to_tray(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.close_to_tray = !self.close_to_tray;
        // 同步托盘 crate 状态（失败不 panic，静默跳过）。
        tray::set_close_to_tray(self.close_to_tray);
        // 与其它偏好同款落盘（persist_prefs 已纳入 close_to_tray 字段）。
        self.persist_prefs();
        cx.notify();
    }
}
