//! 与时间相关的配置常量（去抖、自动隐藏、提示时长）。

/// Markdown 搜索跳转高亮停留时长（ms），随后自动消失。
pub const HIGHLIGHT_MS: u64 = 2200;
/// 打开文件失败通知自动消失秒数。
pub const OPEN_FAILURE_AUTOHIDE_SECS: u64 = 5;
/// Markdown 预览重渲染去抖（ms，避免打字时逐键整篇重解析）。
pub const PREVIEW_DEBOUNCE_MS: u64 = 30;
/// 选项卡栏横向滚动条自动隐藏（ms，停止滚动后该时长隐藏）。
pub const TAB_HSCROLL_HIDE_MS: u64 = 900;