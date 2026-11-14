//! 音乐播放器核心引擎 crate（Phase A：核心，无 UI）。
//!
//! 架构（对齐视频链路的「运行时动态加载」决策）：
//! - [`decode`]：共享 `media_io::bridge` 取音频子集 FFI（vff_open_audio / vff_audio_params /
//!   vff_audio_seek / vff_audio_pcm / vff_close_audio），运行时加载 `media_ffmpeg.dll`，
//!   **不静态链 FFmpeg**——缺库不碍 exe 启动，播放时窗口内报错提示。
//! - [`sink`]：cpal 输出——复用共享 `media_io::audio_sink::AudioSink`（err 标签 "[music]"）。
//! - [`player`]：`MusicPlayer`（gpui Entity）：主线程权威状态 + 后台解码线程 + 事件回送。
//! - [`library`] / [`model`]：音乐库扫描 / 曲目与混排分组数据模型。
//!
//! UI（MusicView，Phase B）调用 [`player::MusicPlayer`] 播放与取状态；本 crate 只交付
//! 可编译、可单测的核心引擎。

pub mod library;
pub mod model;
pub mod player;
pub mod view;
mod cover;  // 私有：封面图像解码（同名 .jpg/.png/.webp → RenderImage）
mod decode; // 私有：media_io 桥音频子集包装（START_CAP/reserve/-1 扩容保留在 music）
mod sink;   // 私有：media_io AudioSink 复用（err "[music]"）

pub use library::{AUDIO_EXTENSIONS, is_audio_extension};
pub use model::{MixGroup, RepeatMode, Track};
pub use player::MusicPlayer;
pub use view::{MusicEvent, MusicView};
