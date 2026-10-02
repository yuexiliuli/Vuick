//! 视频查看器模块（src/video/ 独立文件夹）。
//!
//! 打开 / 拖入视频（mp4/mkv/webm/mov…）时以全屏阅读器展示 —— 与 [`crate::image`]
//! 同款「自包含阅读器」姿势：实体自持状态、后台解码线程、主线程包 `RenderImage` 上屏。
//!
//! 分步路线（守每步 clippy 零新增纪律）：
//! - **M0/M1**（已落地）：`decode` 核心（`VideoReader`：open/seek/逐帧 BGRA）+
//!   `VideoViewer` 首帧静态阅读 + 扩展名路由；
//! - **M2**（当前）：播放循环（帧钟 + 上屏 + play/pause/seek）+ 同文件夹切换；
//! - **M3**：缩放/适配 + 音频 + 性能定标（decode-to-display、缓存预算），对齐 image V2。
//!
//! 上屏方式与 [`crate::pdf::render_image_from_bgra`] 同构：解码帧转 BGRA → `RenderImage`。
//!
//! 模块结构：
//! - [`decode`]：`VideoReader` —— 可 seek 的持久解码器（open/params/seek/next_frame_bgra）
//! - [`viewer`]：`VideoViewer` 视图（后台解码线程 + 帧钟播放 + 控制 + 同文件夹切换）

mod audio;
mod decode;
mod viewer;

pub use viewer::VideoViewer;

/// 可识别的视频扩展名（小写；按文件头之外的扩展名路由，最终能否解出由 ffmpeg 决定）。
/// 供 `EditorApp::is_video_path`（打开路由）与后续「同文件夹扫描」共用。
pub const VIDEO_EXTENSIONS: &[&str] = &[
    "mp4", "mkv", "webm", "mov", "avi", "ts", "m4v", "wmv", "flv", "3gp",
];

/// 某路径是否可识别的视频文件（按扩展名判断）。
pub fn is_video_extension(p: &std::path::Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| VIDEO_EXTENSIONS.iter().any(|x| e.eq_ignore_ascii_case(x)))
        .unwrap_or(false)
}
