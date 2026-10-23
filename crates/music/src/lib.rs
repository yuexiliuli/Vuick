//! 音乐播放器核心引擎 crate（Phase A：核心，无 UI）。
//!
//! 架构（对齐视频链路的「运行时动态加载」决策）：
//! - [`decode`]：libloading 桥，运行时加载 `video_ffmpeg.dll` 复用其导出的音频 FFI
//!   （vff_open_audio / vff_audio_params / vff_audio_seek / vff_audio_pcm / vff_close_audio），
//!   **不静态链 FFmpeg**——缺库不碍 exe 启动，播放时窗口内报错提示。
//! - [`sink`]：cpal 输出（同 video::audio::AudioSink 设计，本 crate 独立复用）。
//! - [`player`]：`MusicPlayer`（gpui Entity）：主线程权威状态 + 后台解码线程 + 事件回送。
//! - [`library`] / [`model`]：音乐库扫描 / 曲目与混排分组数据模型。
//!
//! UI（MusicView，Phase B）调用 [`player::MusicPlayer`] 播放与取状态；本 crate 只交付
//! 可编译、可单测的核心引擎。

pub mod library;
pub mod model;
pub mod player;
mod decode; // 私有：libloading 桥
mod sink;   // 私有：cpal AudioSink

pub use library::{AUDIO_EXTENSIONS, is_audio_extension};
pub use model::{MixGroup, RepeatMode, Track};
pub use player::MusicPlayer;
