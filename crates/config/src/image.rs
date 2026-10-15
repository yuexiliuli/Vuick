//! 图片查看器相关配置常量（解码并发、缓存预算、滚轮手感）。
//!
//! 与 [`crate::config::pdf`] 同款「编译期常量集中管理」：调参数不用进实现里翻。
//!
//! # 调参基准（V2 实测，school 20MP 照片 + 展示档解码）
//! - 解码结果：展示档（20MP→1/2=2560×1920 BGRA）≈ **20MB/张**；压缩字节 ≈ 5.9MB/张。
//! - 后台线程池 = 逻辑核数（gpui `ThreadedDispatcher`），`MAX_DECODE` 即实际解码核上限。
//! - release 冷解码 ~0.19s/张、debug ~1.7s/张；瓶颈是标量熵解码。
//!
//! # 平衡策略（优先速度）
//! - **解码缓存给到「窗口全入住」**：前向 8 + 回看 4 + 当前 = 13 张 × 20MB ≈ 256MB——
//!   滚出窗口才冷，最快滚动 13 张内全命中。
//! - **字节缓存 ≥ 2 个解码窗口**：LRU 淘汰后的重解码/回看命中内存字节，不再等磁盘。
//! - **MAX_DECODE 用满逻辑核**：8 路并发把预热窗口一次铺完；持续 CPU 已由「缓存满即停
//!   预取」封顶（`pump_prefetch` 见 `cache_bytes >= CACHE_MAX_BYTES`），不会整文件夹白算。

/// 解码结果缓存上限（张数）：超出按 LRU 淘汰（配合 [`CACHE_MAX_BYTES`]）。给足窗口
/// 上限（12 前向 + 4 回看 + 余量），实际由字节预算 [`CACHE_MAX_BYTES`] 绑。
pub const CACHE_MAX_ITEMS: usize = 20;
/// 级联预取**前向窗口**（滚动前进方向，当前图之后最多预解几张）：8 张 + 回看 4 + 当前
/// = 13 ≈ 缓存 13 张全入住 → 普通滚动全程命中（速度优先，窗口内 CPU 有界）。
pub const PREFETCH_FORWARD: usize = 8;
/// 级联预取**反向窗口**（刚看过的往回侧，最多保留几张，避免回看时被 LRU 挤掉变冷）。
pub const PREFETCH_BACKWARD: usize = 4;
/// 检测到「快速连续滚动」时**临时放宽**的前向窗口（猛甩滚轮；快速模式不保留反向窗口，
/// 把全部容量让给前方深预取）。12 张 ≈ 缓存上限，不白解窗口外图。
pub const PREFETCH_FORWARD_FAST: usize = 12;
/// 连续快速滚动的判定（[`FAST_SCROLL_THRESHOLD`] 采用迟滞避免抖）：
/// 相邻两步滚动间隔（毫秒）≤ 本值且同方向 → 计一次「快」。
pub const FAST_SCROLL_STEP_MS: u64 = 120;
/// 快滚计数达到本值进入快速模式（放宽前向窗口）——2 步即进，猛甩滚轮尽早深预取；
/// 衰减到 `FAST_SCROLL_THRESHOLD`/3 以下退出（迟滞避免抖进抖出）。
pub const FAST_SCROLL_THRESHOLD: u32 = 2;
/// 前向/反向窗口、快速前向均 < 缓存上限，保证两侧共存不被互挤（编译期校验）。
pub const _CHECK: () = assert!(
    PREFETCH_FORWARD + PREFETCH_BACKWARD < CACHE_MAX_ITEMS
        && PREFETCH_FORWARD_FAST < CACHE_MAX_ITEMS
);
/// 解码结果缓存字节预算（RGBA 近似内存）：256MB ≈ 13 张展示档（20MB/张）= 前8+回4+当前
/// 全窗口常驻 → 滚动全程冷命中；内存峰值 ~384MB（解码缓存 + 字节缓存，不算在飞解码）。
pub const CACHE_MAX_BYTES: usize = 256 * 1024 * 1024;
/// 原始文件字节预取缓存预算（磁盘→RAM 预读；解码命中内存字节即不再等磁盘）。
/// 128MB ≈ 22 张压缩字节（5.9MB/张）→ 重解码/回看不重读盘。
pub const BYTE_CACHE_BYTES: usize = 128 * 1024 * 1024;
/// 同时进行的后台解码上限（含当前目标 + 预取；= 后台线程池逻辑核数上限，8 路并发把
/// 预热窗口一次铺完；持续 CPU 由「缓存满即停预取」封顶，非无界）。
pub const MAX_DECODE: usize = 8;
/// 触控板「像素滚轮」的一格门限（px）：累计像素滚动超过该值才切一张——把高分辨率
/// 手势的洪水事件合并成离散步进；普通鼠标的「行滚轮」仍是 1 事件 = 1 张，不受影响。
pub const WHEEL_PIXEL_STEP: f32 = 24.0;
/// 缩放倍数范围（Ctrl+滚轮可调；语义同 PDF 阅读器 [`crate::config::pdf::ZOOM_MIN`] 等，
/// 区间更宽以支持图片 1:1 微调）：0.2×～8×。
pub const ZOOM_MIN: f32 = 0.2;
/// 缩放倍数上限（放大看细节 1:1 用）。
pub const ZOOM_MAX: f32 = 8.0;
/// 每格滚轮（≈1 行）的缩放倍率增幅（12%）。
pub const ZOOM_STEP: f32 = 0.12;
