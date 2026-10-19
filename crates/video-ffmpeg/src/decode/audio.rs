//! 音频解码（F32 交错立体声重采样）子模块：从 `decode.rs` 按职责拆出。
//!
//! 组合关系：独立于视频独立使用（`AudioReader`），FFMPEG_LOCK 串行纪律由模块根的
//! [`super::FFMPEG_LOCK`]、时间换算 [`super::pts_to_seconds`]、日志收敛
//! [`super::quiet_ffmpeg_log_once`] 在此复用。

use super::{FFMPEG_LOCK, pts_to_seconds, quiet_ffmpeg_log_once};
use std::path::Path;

use anyhow::{Context as _, Result, anyhow};

use ffmpeg_next as ffmpeg;
use ffmpeg::media::Type;

/// 音频顶层参数。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AudioParams {
    /// 输出采样率（统一重采样到该率）。
    pub sample_rate: u32,
    /// 输出声道数（统一 2，STERO 交错）。
    pub channels: u32,
    /// 音轨总时长（秒）。
    pub duration: f64,
}

/// 可 seek 的音频解码器：把源音轨统一重采样为 **F32 交错立体声**（`out_rate` Hz）。
///
/// 与 [`VideoReader`] 同款姿势：调用方搬进后台线程逐块拉取。输出字节可直接喂
/// 音频输出设备（CPAL 等）。
///
/// **释放纪律**：`Drop` 把 FFmpeg 成员（`Input`/decoder/resampler）手动 drop 并持有
/// [`FFMPEG_LOCK`] —— 实测一侧线程正 `avformat_close_input` 释放某文件、另一线程同时
/// `avformat_open_input` 同文件会 deadlock（72.MP4 切换复现）。open/seek/decode/free 全部
/// 过同一互斥量，进程内 FFmpeg FFI 严格串行。
pub struct AudioReader {
    ictx: std::mem::ManuallyDrop<ffmpeg::format::context::Input>,
    stream_index: usize,
    decoder: std::mem::ManuallyDrop<ffmpeg::codec::decoder::Audio>,
    /// 重采样器（tests 直接读 `delay`/`flush` 做残留排空断言，故 `pub(crate)`）。
    pub(crate) resampler: std::mem::ManuallyDrop<ffmpeg::software::resampling::Context>,
    /// 已确定的输出规格（构造后仅供 [`Self::params`] 返回）。
    #[allow(dead_code)] // 与 params() 配套，M4 输出规格入参时启用
    params: AudioParams,
    time_base: ffmpeg::Rational,
    drained: bool,
    last_pts: f64,
    /// seek 后的首个 clean 输出锚点（秒）：`seek(target)` 置 `target`；`next_pcm_f32`
    /// 在输出第一个 pts ≥ 落点的 clean 帧前，跳过落点前的包/坏帧，随后复位 `-1`
    ///（不再跳过，正常顺序读）。`-1` = 非 seek 窗口（初始打开/首次读不用跳）。
    ///
    /// 必要性：`avformat_seek_file` 对 MKV 的落点可能大幅回退（本仓 `视频测试字幕.mkv`
    /// 实测首块可落后目标 ≈-12s），不跳过会整段播放落点前音频（越位/重起时错段）；
    /// 且落点边界常落在 AAC 帧中间（MKV 1ms lacing vs AAC 21.33ms 帧长）→ HE-AAC SBR
    /// 断链坏帧，解码报 `env_facs_q ... is invalid`。两者都靠本锚点截断。
    skip_until: f64,
}

impl Drop for AudioReader {
    fn drop(&mut self) {
        let _ffmpeg_guard = FFMPEG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            std::mem::ManuallyDrop::drop(&mut self.resampler);
            std::mem::ManuallyDrop::drop(&mut self.decoder);
            std::mem::ManuallyDrop::drop(&mut self.ictx);
        }
    }
}

impl AudioReader {
    /// 打开媒体音轨并重采样到 `sample_rate`/立体声/F32 交错。
    pub fn open_pcm_f32(path: &Path, sample_rate: u32) -> Result<Self> {
        use ffmpeg::channel_layout::ChannelLayout;
        use ffmpeg::format::Sample;

        let _ffmpeg_guard = FFMPEG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        ffmpeg::init().context("ffmpeg init 失败")?;
        quiet_ffmpeg_log_once();
        let ictx = ffmpeg::format::input(path)
            .with_context(|| format!("打开媒体失败: {}", path.display()))?;
        let stream = ictx
            .streams()
            .best(Type::Audio)
            .ok_or_else(|| anyhow!("媒体中没有音轨: {}", path.display()))?;
        let stream_index = stream.index();
        let time_base = stream.time_base();
        let params_for_decoder = stream.parameters();

        let container_us = ictx.duration();
        let duration = if container_us > 0 {
            container_us as f64 / 1e6
        } else {
            0.0
        };

        let decoder = ffmpeg::codec::context::Context::from_parameters(params_for_decoder)
            .context("解析音轨参数失败")?
            .decoder()
            .audio()
            .context("该流不是音频")?;

        let src_format = decoder.format();
        let src_layout = decoder.channel_layout();
        let src_rate = decoder.rate();
        let resampler = ffmpeg::software::resampling::Context::get(
            src_format,
            src_layout,
            src_rate,
            Sample::F32(ffmpeg::format::sample::Type::Packed),
            ChannelLayout::STEREO,
            sample_rate,
        )
        .context("初始化音频重采样失败")?;

        Ok(Self {
            ictx: std::mem::ManuallyDrop::new(ictx),
            stream_index,
            decoder: std::mem::ManuallyDrop::new(decoder),
            resampler: std::mem::ManuallyDrop::new(resampler),
            params: AudioParams {
                sample_rate,
                channels: 2, // STEREO
                duration: duration.max(0.0),
            },
            time_base,
            drained: false,
            last_pts: 0.0,
            // 打开即按「从起点播放」的 skip 口径（=seek 到 0）：普通打开/顺序播放也套用
            // seek 的「落点前包/坏帧丢弃到首个 clean 帧」——流头若带 `pts<0` 的包（负
            // 时间戳/夹头包）会先丢那个并且首个坏 AAC 帧在输出前被跳过，保证正常打开
            // 从头播放 stderr 不刷 `env_facs_q ... invalid`；对 clean 头零影响（首帧
            // pts≥0 直接返回）。真 seek 时 [`Self::seek`] 用精确 target 覆盖本值。
            skip_until: 0.0,
        })
    }

    /// 已确定的输出参数（速率/声道/时长）。
    #[allow(dead_code)] // 测试校验用；播放线程经构造参数可知规格
    pub fn params(&self) -> AudioParams {
        self.params
    }

    /// 跳转到 `seconds`：decode flush + drain 复位，并**冲掉重采样器内部残留**。复用
    /// 同一 reader 连续 seek（查看器音频 worker 常驻复用）时，swr 会把 seek 前未消耗的
    /// 输入延迟带进新位置 → 新起播首音夹旧段；flush 把 delay/中间/输出缓冲全清
    ///（对应 `next_pcm_f32` EOF 处 [`Self::flush_resampler`] 的收尾姿势）。
    /// 同时置 [`Self::skip_until`]：seek 落点可能非 AAC 帧对齐（MKV lacing 1ms vs
    /// AAC 21.33ms）+ 落点大幅回退 → 首包可能半帧/坏帧（SBR 断链刷 `env_facs_q`）、
    /// 起播音整段落在落点前；`next_pcm_f32` 据此丢弃落点前包/坏帧、从首 clean 帧起播。
    pub fn seek(&mut self, seconds: f64) -> Result<()> {
        let _ffmpeg_guard = FFMPEG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let ts = (seconds.max(0.0) * 1e6) as i64;
        self.ictx
            .seek(ts, i64::MIN..i64::MAX)
            .context("音频 seek 失败")?;
        self.decoder.flush();
        self.drained = false;
        let _ = self.flush_resampler();
        self.skip_until = seconds.max(0.0);
        Ok(())
    }

    /// 把重采样器内部残留全部排空并返回样本（调用方丢弃即「清残留」，如 seek；EOF 处
    /// 则作为收尾块直接返回）。
    ///
    /// ffmpeg-next 的 `Context::flush` 对**全空** out 帧会因 `swr_convert_frame` 的
    /// `config_changed` 检查（空帧 format=NONE/layout=0 ≠ 输出 F32/STEREO）直接报
    /// `Output changed`、**一次都排不出**（探针实证：空帧 flush 恒 Err、`delay()` 原地
    /// 不动）——须按输出规格预分配 out 帧，`av_frame_get_buffer` 才把 ch_layout 补齐、
    /// 正常走下排。终止判据用「产出样本数」而非 `delay()`：排空后 `swr_get_delay`
    /// 仍返回非零（引擎内插延迟），按它判会死循环（实证排空后 delay≈17 仍 `Some`）。
    /// `plane()` 长度 = `linesize` 全容量并非实际样本数，须按 `samples()×channels`
    /// 截到真实产出，否则返回 1 秒容量整段的未定尾。
    pub(crate) fn flush_resampler(&mut self) -> Vec<f32> {
        let d = self.resampler.output();
        let mut out = ffmpeg::frame::Audio::new(d.format, d.rate as usize, d.channel_layout);
        let mut all = Vec::new();
        loop {
            match self.resampler.flush(&mut out) {
                Ok(Some(_)) if out.samples() > 0 => {
                    let n = out.samples() * out.channels() as usize;
                    let plane = Self::collect_f32(&out);
                    let take = n.min(plane.len());
                    all.extend_from_slice(&plane[..take]);
                }
                _ => break,
            }
        }
        all
    }

    /// 把重采样输出帧读成 F32 样本（空/未分配帧返回空，避免读 plane 越界）。
    fn collect_f32(out: &ffmpeg::frame::Audio) -> Vec<f32> {
        if out.planes() == 0 {
            return Vec::new();
        }
        out.plane::<f32>(0).to_vec()
    }

    /// 推进到**下一块** F32 交错样本。`(pts 秒, 样本)`（样本数为偶数=立体声）；
    /// `None` = EOF（之后可再 `seek` 复用）。
    ///
    /// seek 窗口内（[`Self::skip_until`] ≥ 0）：跳过落点前的包（不喂解码器）与坏/残缺帧
    ///（SBR 断链等解码错误不中止读取，让下个包重同步），直到第一个 pts ≥ 落点的 clean
    /// 帧输出后复位——保证起播音精确从落点开始、不夹 SBR 断链坏帧的杂音/不出错断流。
    pub fn next_pcm_f32(&mut self) -> Result<Option<(f64, Vec<f32>)>> {
        let _ffmpeg_guard = FFMPEG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            let packet = self.next_audio_packet();
            // 包级跳过（省解码 + 不产坏帧）：seek 落点前的 demux 包直接丢弃——MKV 落点
            // 可回退长达 -12s，不丢则整段预滚音频被解码/进重采样/被设备播放（越位/错段）。
            if self.skip_until >= 0.0
                && let Some(p) = packet.as_ref().and_then(|p| p.pts())
            {
                let p_sec = pts_to_seconds(p, self.time_base);
                if p_sec < self.skip_until {
                    continue;
                }
            }
            let packet = match packet {
                Some(p) => p,
                None => {
                    if !self.drained {
                        self.decoder.send_eof().ok();
                        self.drained = true;
                    }
                    // 冲掉重采样器内部剩余样本（同 seek 排残留的收尾姿势，见 flush_resampler）
                    let samples = self.flush_resampler();
                    if !samples.is_empty() {
                        return Ok(Some((self.last_pts, samples)));
                    }
                    return Ok(None);
                }
            };
            if let Err(e) = self.decoder.send_packet(&packet) {
                if self.skip_until >= 0.0 {
                    // seek 窗口内的坏/残缺包（SBR 断链提交失败）：跳过让下个包重同步，
                    // 否则操作面一遇坏帧就断流 → 音频停滞/卡顿。
                    continue;
                }
                return Err(anyhow!("音频 decoder send_packet 失败: {e}"));
            }
            let mut decoded = ffmpeg::frame::Audio::empty();
            while self.decoder.receive_frame(&mut decoded).is_ok() {
                let pts = pts_to_seconds(decoded.pts().unwrap_or(0), self.time_base);
                self.last_pts = pts;
                // 帧级 gate（包级跳过的兜底：时间戳缺失/落点偏差时仍保证不从落点前起播）
                if self.skip_until >= 0.0 && pts < self.skip_until {
                    continue;
                }
                self.skip_until = -1.0; // 首个 ≥ 落点帧已就绪：后续正常顺序读
                let mut out = ffmpeg::frame::Audio::empty();
                self.resampler.run(&decoded, &mut out).context("音频重采样失败")?;
                let samples = Self::collect_f32(&out);
                if !samples.is_empty() {
                    return Ok(Some((pts, samples)));
                }
            }
        }
    }

    /// 取属于本音轨的下一个 demux 包。
    fn next_audio_packet(&mut self) -> Option<ffmpeg::packet::Packet> {
        for (s, p) in self.ictx.packets() {
            if s.index() == self.stream_index {
                return Some(p);
            }
        }
        None
    }
}
