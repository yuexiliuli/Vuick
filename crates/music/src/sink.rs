//! 音乐音频输出：复用共享 `media_io::audio_sink::AudioSink`（err 标签 "[music]"）。
//!
//! 本地 cpal sink 已并入 media_io（统一 5 元 stats 含 `dropped_samples`）；本模块为 music
//! 保留 `crate::sink::AudioSink` 路径与构造语义——`[music]` 标签经 `open_with_tag` 在
//! player 侧传入，消费方 import 不变。
pub(crate) use media_io::audio_sink::AudioSink;
