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
}

/// 一帧解码结果：`(帧 pts 秒, 宽, 高, BGRA 像素)`（BGRA 字节序同 GPUI `RenderImage`）。
pub type DecodedFrame = (f64, u32, u32, Vec<u8>);

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
}
