//! 性能 / 解析 / 缓存相关的阈值上限常量。

/// 超过该体积的文档不做语法高亮（工作集过大，无法支撑换代场景；正常文档远小于此）。
pub const MAX_TEXT_BYTES: usize = 24 * 1024 * 1024;
/// 可见窗口**向前**多解析的行数（语法状态前缀），使跨越行边界的注释/字符串/
/// 代码块在大多数情况下也能正确配色。
pub const LOOKBACK_LINES: usize = 48;
/// 计算时向可见窗口上下各扩展的行数：滚动在小余量内命中缓存不重算。
/// 调小 → 每次重算更快、颜色更早落屏。
pub const WINDOW_PAD: usize = 16;
/// 单次**同步**解析的行数上限：超过则本次不着色（避免超大视口卡帧）。
pub const SYNC_MAX_PARSE_LINES: usize = 2000;
/// 编辑器自动软换行阈值：超过该行数的文档不换行（保持滚动/打开流畅）。
pub const EDITOR_WRAP_MAX_LINES: usize = 30_000;
/// 渲染行数超过该阈值启用「窗口级懒解析」（大字文档不整篇物化）。
pub const WINDOWED_MIN_BLOCKS: usize = 4_000;
/// 窗口懒解析时在可见范围上下各多物化的块数（过扫余量，减少反复重解析）。
pub const WINDOWED_OVERDRAW_BLOCKS: usize = 24;
/// 侧边栏搜索结果条数上限。
pub const SEARCH_RESULT_CAP: usize = 200;
