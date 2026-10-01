//! 图片查看器相关配置常量（解码并发、缓存预算、滚轮手感）。
//!
//! 与 [`crate::config::pdf`] 同款「编译期常量集中管理」：调参数不用进实现里翻。
//! 总量原则：缓存预算以「最常遇见的几 MB 图 × 大步进浏览」为锚——解码结果缓存
//! 一张 ~500KB JPEG 解出的 RGBA 约 2MB（500k 像素×4B），预留给高频浏览回看；字节
//! 缓存只存压缩字节，比解码结存便宜得多，预算可给更大以覆盖「跳过一张再回看」。

/// 解码结果缓存上限（张数）：超出按 LRU 淘汰（配合 [`CACHE_MAX_BYTES`]）。
/// ≥ 前向窗口 + 反向窗口 + 当前图（+余量），保证「往前滚（滚动方向）」与「回看上一张」
/// 两侧都常驻缓存、命中即切。
pub const CACHE_MAX_ITEMS: usize = 16;
/// 级联预取**前向窗口**（滚动前进方向，当前图之后最多预解几张）。
pub const PREFETCH_FORWARD: usize = 6;
/// 级联预取**反向窗口**（刚看过的往回侧，最多保留几张，避免回看时被 LRU 挤掉变冷）。
pub const PREFETCH_BACKWARD: usize = 4;
/// 检测到「快速连续滚动」时**临时放宽**的前向窗口（追上猛甩滚轮；快速模式不保留反向窗口，
/// 把全部容量让给前方深预取）。
pub const PREFETCH_FORWARD_FAST: usize = 10;
/// 连续快速滚动的判定（[`FAST_SCROLL_THRESHOLD`] 采用迟滞避免抖）：
/// 相邻两步滚动间隔（毫秒）≤ 本值且同方向 → 计一次「快」。
pub const FAST_SCROLL_STEP_MS: u64 = 120;
/// 快滚计数达到本值进入快速模式；衰减到 [`FAST_SCROLL_THRESHOLD`]/3 以下退出（迟滞）。
pub const FAST_SCROLL_THRESHOLD: u32 = 3;
/// 前向/反向窗口、快速前向均 < 缓存上限，保证两侧共存不被互挤（编译期校验）。
pub const _CHECK: () = assert!(
    PREFETCH_FORWARD + PREFETCH_BACKWARD < CACHE_MAX_ITEMS
        && PREFETCH_FORWARD_FAST < CACHE_MAX_ITEMS
);
/// 解码结果缓存字节预算（RGBA 近似内存）。
pub const CACHE_MAX_BYTES: usize = 192 * 1024 * 1024;
/// 原始文件字节预取缓存预算（磁盘→RAM 预读；解码命中内存字节即不再等磁盘）。
pub const BYTE_CACHE_BYTES: usize = 96 * 1024 * 1024;
/// 同时进行的后台解码上限（含当前目标 + 预取；多核并行解码，超出时只追「最新目标」）。
pub const MAX_DECODE: usize = 8;
/// 触控板「像素滚轮」的一格门限（px）：累计像素滚动超过该值才切一张——把高分辨率
/// 手势的洪水事件合并成离散步进；普通鼠标的「行滚轮」仍是 1 事件 = 1 张，不受影响。
pub const WHEEL_PIXEL_STEP: f32 = 24.0;
