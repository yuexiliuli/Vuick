//! 视频查看器相关配置常量（缩放区间、播放/解码参数）。

/// Ctrl+滚轮缩放区间 / 步进（语义同图片 [`crate::config::image::ZOOM_*`] / PDF
/// [`crate::config::pdf::ZOOM_*`] 阅读器）。视频画面相对「适应视口」放大看细节用，
/// 缩放下限略高于图片（0.25×）：视频按 contain 适配后通常已够小，不需再缩到 0.2×。
pub const ZOOM_MIN: f32 = 0.25;
/// 缩放倍数上限。
pub const ZOOM_MAX: f32 = 8.0;
/// 每格滚轮（≈1 行）的缩放倍率增幅（12%）。
pub const ZOOM_STEP: f32 = 0.12;
