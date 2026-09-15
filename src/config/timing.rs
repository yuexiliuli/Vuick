//! 与时间相关的配置常量（去抖、自动隐藏、提示时长）。

/// Markdown 搜索跳转高亮停留时长（ms），随后自动消失。
pub const HIGHLIGHT_MS: u64 = 2200;
/// 打开文件失败通知自动消失秒数。
pub const OPEN_FAILURE_AUTOHIDE_SECS: u64 = 5;
/// 选项卡栏横向滚动条自动隐藏（ms，停止滚动后该时长隐藏）。
pub const TAB_HSCROLL_HIDE_MS: u64 = 900;
/// 统一行高提示的 O(块数) SumTree 重建**最小间隔**（ms）：换宽/拖分屏时宽度每帧变化，
/// 若每帧都重建 160k 节点列表会持续卡顿；节流后至多每 120ms 重建一次，拖动期间滚动条
/// 量程短暂滞后，松手即收敛。首次建立不节流（打开需立即铺满量程）。
pub const HINT_REAPPLY_INTERVAL_MS: u64 = 120;