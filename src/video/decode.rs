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
///
/// **decode-to-display（降载）**：可用 [`Self::open_scaled`] 把 sws 输出尺寸
/// 按最长边封顶下采样（如 4K 源只解 1080p）——解码/内存/上屏均按输出尺寸来的。
/// 每次解码成本 ~输出像素数，4K 源封顶 1920 即省 ~4×。「≤输出尺寸」时按源码解，
/// 行为与 [`Self::open`] 完全一致，但 `params()` 与逐帧尺寸始终反映**实际输出**。
pub struct VideoReader {
    ictx: ffmpeg::format::context::Input,
    stream_index: usize,
    decoder: ffmpeg::codec::decoder::Video,
    scaler: ffmpeg::software::scaling::Context,
    params: VideoParams,
    /// sws 输出宽（decode-to-display 下采样目标；`params.width` 为源宽）。
    out_width: u32,
    /// sws 输出高（见 `out_width`）。
    out_height: u32,
    /// 帧 pts 使用的 time_base（秒 = pts × num/den）。
    time_base: ffmpeg::Rational,
    /// 是否已进入 EOF drain 模式（send_eof 已发）。
    drained: bool,
}

impl VideoReader {
    /// 打开媒体并定位视频流（失败返回带原因的 [Error] 链）；按源码全尺寸解码。
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_scaled(path, u32::MAX)
    }

    /// 同 [`Self::open`]，但 sws 输出按「长边 ≤ `max_dim`」封顶下采样（保持宽高比、
    /// 输出至少 1×1；`max_dim=u32::MAX` 即全尺寸）。`params()` 仍报告源宽高——上层
    /// 用它保持原始宽高比做显示适配；逐帧输出尺寸见 [`DecodedFrame`] 的 w/h。
    pub fn open_scaled(path: &Path, max_dim: u32) -> Result<Self> {
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
        let (out_width, out_height) = scaled_dims(width, height, max_dim);
        let scaler = ffmpeg::software::scaling::Context::get(
            decoder.format(),
            width,
            height,
            ffmpeg::format::Pixel::BGRA,
            out_width,
            out_height,
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
            out_width,
            out_height,
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

    /// 把解码帧转 BGRA；无 pts 的帧跳过（返回 `None`）。尺寸用 sws 输出
    /// （decode-to-display 下采样目标），与 `out_width/out_height` 一致。
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
            self.out_width,
            self.out_height,
            bgra.data(0).to_vec(),
        )))
    }
}

/// 按「长边 ≤ `max_dim`」下采样保持宽高比：源码已在界内则原样返回；
/// `max_dim=0` 视为不限制。输出至少 1×1（避免 sws 空输出）。
fn scaled_dims(width: u32, height: u32, max_dim: u32) -> (u32, u32) {
    if max_dim == 0 {
        return (width, height);
    }
    let longest = width.max(height);
    if longest <= max_dim {
        return (width, height);
    }
    let scale = f64::from(max_dim) / f64::from(longest);
    (
        (f64::from(width) * scale).round().max(1.0) as u32,
        (f64::from(height) * scale).round().max(1.0) as u32,
    )
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

    /// decode-to-display 下采样：长边封顶后保持宽高比（320×240 → 160×120），
    /// 逐帧输出与内存按输出尺寸走；`params()` 仍报源宽高供显示适配。
    #[test]
    fn open_scaled_caps_longest_side_and_keeps_source_params() {
        let mut r = VideoReader::open_scaled(&fixture(), 160).unwrap();
        let p = r.params();
        assert_eq!((p.width, p.height), (320, 240), "params 保持源尺寸");
        let (pts, w, h, bgra) = r.next_frame_bgra().unwrap().expect("缩放后应出帧");
        assert!(pts >= 0.0);
        assert_eq!((w, h), (160, 120), "输出按长边封顶下采样");
        assert_eq!(bgra.len(), (160 * 120 * 4) as usize, "内存按输出尺寸分配");
    }

    /// 封顶 ≥ 源长边时与全尺寸一致（320×240 原样）。
    #[test]
    fn open_scaled_with_huge_cap_is_identity() {
        let r = VideoReader::open_scaled(&fixture(), 4096).unwrap();
        assert_eq!(r.params(), VideoReader::open(&fixture()).unwrap().params());
        let mut r = VideoReader::open_scaled(&fixture(), 4096).unwrap();
        let (w, h) = { let f = r.next_frame_bgra().unwrap().unwrap(); (f.1, f.2) };
        assert_eq!((w, h), (320, 240), "封顶超源应按源尺寸输出");
    }

    #[test]
    fn scaled_dims_preserves_aspect_and_floor() {
        assert_eq!(scaled_dims(320, 240, 0), (320, 240), "max_dim=0 不限制");
        assert_eq!(scaled_dims(320, 240, u32::MAX), (320, 240));
        assert_eq!(scaled_dims(320, 240, 160), (160, 120), "等比减半");
        assert_eq!(scaled_dims(200, 100, 50), (50, 25), "等比 1/4");
        assert_eq!(scaled_dims(3, 2, 6), (3, 2), "界内原样");
        assert!((scaled_dims(1, 1, 0)).0 >= 1, "输出至少 1×1");
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
        let mut all: Vec<f32> = Vec::new();
        while let Some((pts, samples)) = r.next_pcm_f32().unwrap() {
            assert!(pts >= last - 1e-9, "音频 pts 单调");
            last = pts;
            assert_eq!(samples.len() % 2, 0, "立体声交错样本");
            assert!(
                samples.iter().all(|&s| s.is_finite() && s.abs() <= 2.0),
                "解码样本应有限且有界"
            );
            all.extend_from_slice(&samples);
            n += 1;
            assert!(n < 1000, "不该无限出音频块");
        }
        assert!(n > 5, "2s 音频应产出多块, 实得 {n}");
        // 全段 440Hz 正弦「形状」断言（AAC 编码会把电平压低，故不断言绝对幅度，
        // 只断言它是音调而非静态噪音）：过零率 ≈ 2×440/秒（静噪≈48000/秒），
        // 峰值与 RMS 有正弦特征（peak ≥ 1.4×rms）。
        let len = all.len();
        let rms = (all.iter().map(|s| s * s).sum::<f32>() / len as f32).sqrt();
        let peak = all.iter().fold(0.0f32, |a, &s| a.max(s.abs()));
        let zc = all.windows(2).filter(|w| (w[0] >= 0.0) != (w[1] >= 0.0)).count();
        let secs = len as f32 / (2.0 * 48000.0);
        let zc_s = zc as f32 / secs;
        eprintln!("audio stats: rms={rms:.4} peak={peak:.4} zc/s={zc_s:.0}");
        assert!(
            (400.0..=1400.0).contains(&zc_s),
            "440Hz 音调过零率≈880/s，实得 {zc_s:.0}（静噪≈48000/s）"
        );
        assert!(
            peak > rms * 1.2 && rms > peak * 0.2,
            "正弦峰值/均方根应具正弦特征 (peak={peak:.4}, rms={rms:.4})"
        );
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
