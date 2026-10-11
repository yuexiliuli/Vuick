//! 与时间相关的配置常量（去抖、自动隐藏、提示时长）。

/// Markdown 搜索跳转高亮停留时长（ms），随后自动消失。
pub const HIGHLIGHT_MS: u64 = 2200;
/// 注记标记 `[n]` 单击跳转的「单击确认」窗口（ms）：首击后等待该时长，期间未出现
/// 第二击（双击）才执行跳转；双击会取消首击的待跳转——否则双击注记段落（尤其 EPUB
/// 尾部 90 条注记）首击即整页位移、加亮闪烁（P4-2 回归，用户报告双击闪烁/掉行）。
pub const NOTE_NAV_CONFIRM_MS: u64 = 300;
/// 打开文件失败通知自动消失秒数。
pub const OPEN_FAILURE_AUTOHIDE_SECS: u64 = 5;
/// 选项卡栏横向滚动条自动隐藏（ms，停止滚动后该时长隐藏）。
pub const TAB_HSCROLL_HIDE_MS: u64 = 900;