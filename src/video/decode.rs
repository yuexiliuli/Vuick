//! 视频解码核心：打开媒体 → 定位视频流 → 解码 → 转换 BGRA。
//!
//! 只依赖 `ffmpeg_next`（高层安全封装），不碰 GUI / 线程调度（线程归属调用方：
//! 查看器里用 `std::thread` 跑播放线程，本模块只提供同步的「读下一帧」）。
//!
//! # 核心类型
//! - [`VideoReader`]：可 seek 的持久解码器 —— `open` 打开并取参数，`seek` 跳转，
//!   `next_frame_bgra` 逐帧推进并转 BGRA。首帧静态显示与播放循环共用它
//!   （取代早前一次性 [`decode_first_frame_bgra`] 的临时入口）。
//!
//! # 环境依赖（构建期）
//! `ffmpeg-sys-next` 需要 `FFMPEG_DIR`（指向含 include/ lib/ 的 FFmpeg 构建）与
//! `LIBCLANG_PATH`（bindgen 用）。开发机一键装填见 `scripts/setup-ffmpeg.ps1`。

use std::path::Path;

use anyhow::{Context as _, Result, anyhow};

use ffmpeg_next as ffmpeg;
use ffmpeg::media::Type;

/// 视频顶层参数（打开即得，无需逐帧解码）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VideoParams {
    /// 解码后原始宽。
    pub width: u32,
    /// 解码后原始高。
    pub height: u32,
    /// 总时长（秒）；未知/流式时可能为 0。
    pub duration: f64,
    /// 标称帧率 `(num, den)`。
    pub frame_rate: Option<(u32, u32)>,
    /// 是否含音轨（查看器据此决定是否起音频播放）。
    pub has_audio: bool,
}

/// 一帧解码结果：`(帧 pts 秒, 宽, 高, BGRA 像素)`（BGRA 字节序同 GPUI `RenderImage`）。
pub type DecodedFrame = (f64, u32, u32, Vec<u8>);

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
pub struct AudioReader {
    ictx: ffmpeg::format::context::Input,
    stream_index: usize,
    decoder: ffmpeg::codec::decoder::Audio,
    resampler: ffmpeg::software::resampling::Context,
    /// 已确定的输出规格（构造后仅供 [`Self::params`] 返回）。
    #[allow(dead_code)] // 与 params() 配套，M4 输出规格入参时启用
    params: AudioParams,
    time_base: ffmpeg::Rational,
    drained: bool,
    last_pts: f64,
}

impl AudioReader {
    /// 打开媒体音轨并重采样到 `sample_rate`/立体声/F32 交错。
    pub fn open_pcm_f32(path: &Path, sample_rate: u32) -> Result<Self> {
        use ffmpeg::channel_layout::ChannelLayout;
        use ffmpeg::format::Sample;

        ffmpeg::init().context("ffmpeg init 失败")?;
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
            ictx,
            stream_index,
            decoder,
            resampler,
            params: AudioParams {
                sample_rate,
                channels: 2, // STEREO
                duration: duration.max(0.0),
            },
            time_base,
            drained: false,
            last_pts: 0.0,
        })
    }

    /// 已确定的输出参数（速率/声道/时长）。
    #[allow(dead_code)] // 测试校验用；播放线程经构造参数可知规格
    pub fn params(&self) -> AudioParams {
        self.params
    }

    /// 跳转到 `seconds`：decode flush + drain 复位（与视频 reader 同款）。
    pub fn seek(&mut self, seconds: f64) -> Result<()> {
        let ts = (seconds.max(0.0) * 1e6) as i64;
        self.ictx
            .seek(ts, i64::MIN..i64::MAX)
            .context("音频 seek 失败")?;
        self.decoder.flush();
        self.drained = false;
        Ok(())
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
    pub fn next_pcm_f32(&mut self) -> Result<Option<(f64, Vec<f32>)>> {
        loop {
            let packet = self.next_audio_packet();
            let packet = match packet {
                Some(p) => p,
                None => {
                    if !self.drained {
                        self.decoder.send_eof().ok();
                        self.drained = true;
                    }
                    // 冲掉重采样器内部剩余样本
                    loop {
                        let mut out = ffmpeg::frame::Audio::empty();
                        let no_more = match self.resampler.flush(&mut out) {
                            Ok(Some(_)) => false, // 还有内部帧
                            _ => true,
                        };
                        let samples = Self::collect_f32(&out);
                        if !samples.is_empty() {
                            return Ok(Some((self.last_pts, samples)));
                        }
                        if no_more {
                            break;
                        }
                    }
                    return Ok(None);
                }
            };
            self.decoder
                .send_packet(&packet)
                .context("音频 decoder send_packet 失败")?;
            let mut decoded = ffmpeg::frame::Audio::empty();
            while self.decoder.receive_frame(&mut decoded).is_ok() {
                let pts = decoded.pts().unwrap_or(0) as f64
                    * f64::from(self.time_base.numerator())
                    / f64::from(self.time_base.denominator());
                self.last_pts = pts;
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

/// 可 seek 的持久视频解码器。
///
/// 构造即打开文件并定位视频流、建好 sws（BGRA 输出）。调用方负责线程：把
/// 该类型搬进后台线程逐帧拉取，主线程只按渲染节奏消费（配合时钟丢弃迟到帧）。
pub struct VideoReader {
    ictx: ffmpeg::format::context::Input,
    stream_index: usize,
    decoder: ffmpeg::codec::decoder::Video,
    scaler: ffmpeg::software::scaling::Context,
    params: VideoParams,
    /// 帧 pts 使用的 time_base（秒 = pts × num/den）。
    time_base: ffmpeg::Rational,
    /// 是否已进入 EOF drain 模式（send_eof 已发）。
    drained: bool,
}

impl VideoReader {
    /// 打开媒体并定位视频流（失败返回带原因的 [Error] 链）。
    pub fn open(path: &Path) -> Result<Self> {
        ffmpeg::init().context("ffmpeg init 失败")?;

        let ictx = ffmpeg::format::input(path)
            .with_context(|| format!("打开媒体失败: {}", path.display()))?;
        let stream = ictx
            .streams()
            .best(Type::Video)
            .ok_or_else(|| anyhow!("媒体中没有视频流: {}", path.display()))?;
        let stream_index = stream.index();
        let time_base = stream.time_base();
        let params_for_decoder = stream.parameters();

        // 时长：优先容器总长（µs）；为 0/未知（流式/异常）时退回视频流 stats 估算
        let container_us = ictx.duration();
        let stream_dur = stream.duration();
        let duration = if container_us > 0 {
            container_us as f64 / 1e6
        } else if stream_dur > 0 {
            stream_dur as f64 * f64::from(time_base.numerator()) / f64::from(time_base.denominator())
        } else {
            0.0
        };

        let decoder = ffmpeg::codec::context::Context::from_parameters(params_for_decoder)
            .context("解析视频流参数失败")?
            .decoder()
            .video()
            .context("该流不是视频")?;

        let (width, height) = (decoder.width(), decoder.height());
        let frame_rate = decoder.frame_rate();
        let has_audio = ictx.streams().best(Type::Audio).is_some();
        let scaler = ffmpeg::software::scaling::Context::get(
            decoder.format(),
            width,
            height,
            ffmpeg::format::Pixel::BGRA,
            width,
            height,
            ffmpeg::software::scaling::Flags::BILINEAR,
        )
        .context("初始化 sws 缩放失败")?;

        Ok(Self {
            ictx,
            stream_index,
            decoder,
            scaler,
            params: VideoParams {
                width,
                height,
                duration: duration.max(0.0),
                frame_rate: frame_rate.map(|r| (r.numerator() as u32, r.denominator() as u32)),
                has_audio,
            },
            time_base,
            drained: false,
        })
    }

    /// 已解析的顶层参数。
    pub fn params(&self) -> VideoParams {
        self.params
    }

    /// 跳转到 `seconds`（µs 级、回拉关键帧）：解码器 flush 后从 keyframe 前继续解，
    /// 调用方应丢弃 `pts < 目标` 的帧直至越过目标（或直接取首个可用帧，容忍轻微回拉）。
    pub fn seek(&mut self, seconds: f64) -> Result<()> {
        let ts = (seconds.max(0.0) * 1e6) as i64;
        self.ictx
            .seek(ts, i64::MIN..i64::MAX)
            .context("seek 失败")?;
        self.decoder.flush();
        self.drained = false;
        Ok(())
    }

    /// 推进到**下一帧**并转成 BGRA。返回 [`DecodedFrame`]；
    /// `None` = 已到流末尾（之后可再 `seek` 复用）。
    pub fn next_frame_bgra(&mut self) -> Result<Option<DecodedFrame>> {
        loop {
            let packet = self.next_video_packet();
            let packet = match packet {
                Some(p) => p,
                None => {
                    // 流结束：drain 掉解码器缓冲的剩余帧
                    if !self.drained {
                        self.decoder.send_eof().ok();
                        self.drained = true;
                    }
                    let mut f = ffmpeg::frame::Video::empty();
                    while self.decoder.receive_frame(&mut f).is_ok() {
                        if let Some(buf) = self.convert_bgra(&f)? {
                            return Ok(Some(buf));
                        }
                    }
                    return Ok(None);
                }
            };
            self.decoder
                .send_packet(&packet)
                .context("decoder send_packet 失败")?;
            // 一个包可能产出 0~多帧（B 帧）；全部消耗到 EAGAIN 再喂下个包
            let mut f = ffmpeg::frame::Video::empty();
            while self.decoder.receive_frame(&mut f).is_ok() {
                if let Some(buf) = self.convert_bgra(&f)? {
                    return Ok(Some(buf));
                }
            }
        }
    }

    /// 取属于本视频流的下一个 demux 包（跨流包一律跳过、不影响光标）。
    fn next_video_packet(&mut self) -> Option<ffmpeg::packet::Packet> {
        for (s, p) in self.ictx.packets() {
            if s.index() == self.stream_index {
                return Some(p);
            }
        }
        None
    }

    /// 把解码帧转 BGRA；无 pts 的帧跳过（返回 `None`）。
    fn convert_bgra(&mut self, f: &ffmpeg::frame::Video) -> Result<Option<DecodedFrame>> {
        let Some(pts) = f.pts() else {
            return Ok(None);
        };
        let mut bgra = ffmpeg::frame::Video::empty();
        self.scaler.run(f, &mut bgra).context("sws 转换失败")?;
        let secs = pts as f64 * f64::from(self.time_base.numerator())
            / f64::from(self.time_base.denominator());
        Ok(Some((
            secs,
            self.params.width,
            self.params.height,
            bgra.data(0).to_vec(),
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures")
            .join("video测试.mp4")
    }

    #[test]
    fn open_reads_params() {
        let r = VideoReader::open(&fixture()).unwrap();
        let p = r.params();
        assert_eq!((p.width, p.height), (320, 240));
        assert!((1.9..=2.5).contains(&p.duration), "duration={}", p.duration);
        assert!(p.frame_rate.is_some(), "应识别出帧率");
    }

    #[test]
    fn next_frame_pts_are_monotonic() {
        let mut r = VideoReader::open(&fixture()).unwrap();
        let p = r.params();
        let mut last = -1.0f64;
        for i in 0..24 {
            let (pts, w, h, bgra) = r.next_frame_bgra().unwrap().expect("帧应可解码");
            assert!(pts >= last - 1e-9, "pts 单调（i={i}）");
            assert!((0.0..=p.duration + 0.1).contains(&pts), "pts 在时长内 i={i}");
            last = pts;
            assert_eq!((w, h), (320, 240));
            assert_eq!(bgra.len(), (320 * 240 * 4) as usize);
        }
    }

    #[test]
    fn seek_lands_near_target() {
        let mut r = VideoReader::open(&fixture()).unwrap();
        r.seek(1.0).unwrap();
        let (pts, ..) = r.next_frame_bgra().unwrap().expect("seek 后应出帧");
        // 回拉关键帧最多落后一点；testsrc 全 intraframe 应贴近
        assert!((pts - 1.0).abs() < 0.15, "seek 落点偏差过大 pts={pts}");
    }

    #[test]
    fn reaches_eof_then_seek_reuses() {
        let mut r = VideoReader::open(&fixture()).unwrap();
        let mut n = 0;
        while r.next_frame_bgra().unwrap().is_some() {
            n += 1;
            assert!(n < 200, "不该无限出帧");
        }
        assert!(n >= 50, "2s@30fps 应解出约 60 帧, 实得 {n}");
        // EOF 后 seek 仍可复用
        r.seek(0.0).unwrap();
        assert!(r.next_frame_bgra().unwrap().is_some(), "seek 回 0 应再出帧");
    }

    #[test]
    fn rejects_missing_file() {
        let bad = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("no-such-file.mp4");
        assert!(VideoReader::open(&bad).is_err());
    }

    fn audio_fixture() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures")
            .join("video测试有声.mp4")
    }

    #[test]
    fn audio_open_and_decode() {
        let mut r = AudioReader::open_pcm_f32(&audio_fixture(), 48000).unwrap();
        let p = r.params();
        assert_eq!(p.channels, 2);
        assert_eq!(p.sample_rate, 48000);
        assert!((1.5..=3.0).contains(&p.duration), "duration={}", p.duration);
        let mut n = 0;
        let mut last = -1.0f64;
        while let Some((pts, samples)) = r.next_pcm_f32().unwrap() {
            assert!(pts >= last - 1e-9, "音频 pts 单调");
            last = pts;
            assert_eq!(samples.len() % 2, 0, "立体声交错样本");
            if n == 0 {
                assert!(
                    samples.iter().any(|&s| s != 0.0),
                    "anoisesrc 应产出非静音样本"
                );
            }
            n += 1;
            assert!(n < 1000, "不该无限出音频块");
        }
        assert!(n > 5, "2s 音频应产出多块, 实得 {n}");
    }

    #[test]
    fn audio_seek_lands_near_target() {
        let mut r = AudioReader::open_pcm_f32(&audio_fixture(), 48000).unwrap();
        r.seek(1.0).unwrap();
        let (pts, _) = r.next_pcm_f32().unwrap().expect("seek 后应出音频");
        assert!((pts - 1.0).abs() < 0.25, "音频 seek 落点偏差过大 pts={pts}");
    }

    #[test]
    fn audio_rejects_missing() {
        let bad = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("no-such.mp4");
        assert!(AudioReader::open_pcm_f32(&bad, 48000).is_err());
    }
}
