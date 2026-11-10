//! 「关闭到系统托盘」开关：菜单点击切换 + 立即持久化 + 同步 `tray` crate 状态。
//!
//! 非 `windows` 目标由 `tray` crate 编译为 no-op → 本模块逻辑不变，行为自动降级为
//! 「点 × 直接退出」（不建托盘图标、`should_close_to_tray()` 恒 false）。

use super::*;

impl EditorApp {
    /// 「关闭到系统托盘」开关：翻转 - 同步 `tray` crate 的 flag（决定点 × 是否隐藏到
    /// 托盘；托盘图标本身由 `tray::ensure_tray_icon` 启动时常驻，不随开关增删）- 立即
    /// 持久化。`persist_prefs` 与 `tray::set_close_to_tray` 在测试构建都不写盘 / 不动
    /// Win32（见 `chrome.rs` 与 `tray` crate 文档），故本函数在测试里只验证字段翻转（R3）。
    #[doc(hidden)]
    pub fn toggle_close_to_tray(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let next = !self.model.borrow().close_to_tray;
        self.model.borrow_mut().close_to_tray = next;
        // 同步托盘 crate 状态（只翻 flag，失败不 panic，静默跳过）。
        tray::set_close_to_tray(next);
        // 与其它偏好同款落盘（persist_prefs 已纳入 close_to_tray 字段）。
        self.persist_prefs();
        cx.notify();
    }
}
