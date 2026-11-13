//! 媒体共享 IO crate（供 video / music 消费方迁移复用）：
//!
//! - [`bridge`]：`media_ffmpeg.dll` FFI 桥——运行时 libloading 加载，**全符号表 = 视频全集**
//!   （视频消费全集、音乐只取音频子集，桥层不特判消费方）；共享类型
//!   [`bridge::AudioParams`] / [`bridge::ParamFFI`] 亦放此处。
//! - [`audio_sink`]：CPAL 音频输出 sink——环形缓冲 + 播放位置 + 音量 + 健康计数
//!   （含 `dropped_samples` 的统一 5 元 stats）；`err` 标签可作构造参数（"[music]" / "[video]"）。
//!
//! 本批只沉淀共享组件，**不迁任何消费方**（video / music 迁移为下一批）。
//! 纪律对齐工作区：注释中文；unsafe 只存在于桥层（符号加载/transmute，逐字照抄原实现）。

pub mod audio_sink;
pub mod bridge;
