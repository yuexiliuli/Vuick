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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, Once};

use anyhow::{Context as _, Result, anyhow};

use ffmpeg_next as ffmpeg;
use ffmpeg::media::Type;

// 子模块：按职责拆分的解码器（硬解 / 音频 / 字幕 / 测试），本根只保留视频正文并做再导出。
pub(crate) mod audio;
pub(crate) mod hw;
pub(crate) mod subtitle;
#[cfg(test)]
mod tests;

// 供视频正文（VideoReader）引用的硬解内部名：裸名直达，正文复用拆分前原样。
use self::hw::{
    HardwareDecode, HwFramePin, build_external_d3d11_device_ctx, is_hw_frame, try_open_hw_decoder,
};

// 再导出：让 `decode::{AudioReader, SubtitleReader, VideoReader, HwFrame, FFMPEG_LOCK,
// d3d11_supports_shader_bound_textures, SubtitleFormat}` 等 lib.rs / crate 内原路径继续可解析
//（拆分不改任何公共 API / FFI 导出）。
pub(crate) use audio::AudioReader;
pub(crate) use hw::{d3d11_supports_shader_bound_textures, HwFrame};
pub(crate) use subtitle::{SubtitleFormat, SubtitleReader};
// 仅测试消费的再导出（tests.rs 经 `use super::*` 取用；非测试构建无引用，故 cfg(test) 门控）。
#[cfg(test)]
pub(crate) use subtitle::{SubtitleEntry, active_subtitle_index};

// ---------------- 全局 FFmpeg 串行闸 + 日志收敛 ----------------

/// 进程级 FFmpeg 闸：本构建（ffmpeg 9.0 gyan）里**并发的 FFmpeg 操作**会互相占死——
/// 实测切 72.MP4：新视频线程 `avformat_open_input` 与音频线程解码/再打开并行时直接卡死
/// 在 `format::input`（开放闸只串 open 无效，open 与解码也会互相卡）。因此**所有 FFI
/// 调用**（open / seek / next_frame / next_pcm / subtitle）都过此互斥量串行；单次操作毫秒级，
/// 视频 2 帧通道 + 音频 1s 缓冲足以吸收串行抖动。打开后、解码途中均释放，不长期持锁。
pub(crate) static FFMPEG_LOCK: Mutex<()> = Mutex::new(());

/// FFmpeg 日志级别一次性收敛到 `Error`（进程内仅执行一次）：ffmpeg 默认 `Warning` 会把
/// 大量良性抖动打到 stderr（如 mp4/mov 的 `udta` 盒布局异常 → `UDTA parsing failed
/// retrying raw` 后仍能正常播放）。保留 `Error` 级让真正解码错误仍可见。用独立
/// `Once` 保证幂等（早于首次 open 由播放线程触发）。
static FFMPEG_LOG_QUIET: Once = Once::new();

/// 被 [`ffmpeg_log_filter_cb`] 滤掉的良性坏帧报错条数（诊断/测试用，非热路径）。
static FFMPEG_LOG_FILTERED: AtomicUsize = AtomicUsize::new(0);

/// HE-AAC SBR 断链坏帧的良性报错特征：seek 落点非 AAC 帧对齐（MKV lacing 1ms vs AAC
/// 21.33ms 帧长）时，解码器内部 `av_log` **ERROR** 级打印 `env_facs_q ... is invalid`——
/// `skip_until` 已把这些坏帧丢弃（不改解码正确性），这行日志纯噪音。quiet 的
/// `Level::Error` 挡不住（它就是 Error 级），SKIP_REPEATED 只收敛高频重复，故在此
/// **按消息文本过滤**，其余日志照常透传。
fn ffmpeg_log_line_is_benign(line: &str) -> bool {
    line.contains("env_facs_q")
}

/// FFmpeg 日志回调（挂进 `av_log_set_callback`）：把每条消息格式化后按
/// [`ffmpeg_log_line_is_benign`] 过滤，命中即丢弃（`env_facs_q ... is invalid`）；
/// 未命中则透传给默认回调（格式化/前缀/重复收敛/落盘行为与默认一致）。
/// 回调内禁止 panic（extern "C" unwind 会 abort）：一律 `unwrap_or` 兜底。
unsafe extern "C" fn ffmpeg_log_filter_cb(
    ptr: *mut std::ffi::c_void,
    level: i32,
    fmt: *const std::ffi::c_char,
    vl: *mut std::ffi::c_char,
) {
    let mut buf = [0i8; 512];
    let mut prefix: i32 = 1;
    // `av_log_format_line` 在 FFmpeg 9.x 是 `void`（不回长度）——按 snprintf 约定写入
    // buffer 并 0 结尾，长度由 `CStr` 扫描得出
    unsafe {
        ffmpeg::ffi::av_log_format_line(
            ptr,
            level,
            fmt,
            vl, // bindgen 把 va_list 摊成 `*mut c_char`
            buf.as_mut_ptr(),
            buf.len() as i32,
            &mut prefix,
        );
    }
    let line = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) };
    if ffmpeg_log_line_is_benign(
        line.to_string_lossy().trim(),
    ) {
        FFMPEG_LOG_FILTERED.fetch_add(1, Ordering::Relaxed);
        return;
    }
    unsafe { ffmpeg::ffi::av_log_default_callback(ptr, level, fmt, vl) };
}

fn quiet_ffmpeg_log_once() {
    FFMPEG_LOG_QUIET.call_once(|| {
        ffmpeg::log::set_level(ffmpeg::log::Level::Error);
        // 连续重复的同一错误只打一次（ffmpeg 自带「last message repeated N times」收敛）——
        // HE-AAC SBR 断链坏帧在反复 seek 时会重复触发同一 `env_facs_q ... is invalid`，
        // 不开这会逐帧刷屏把 stderr 灌爆（也是 UI 卡顿源之一）。
        ffmpeg::log::set_flags(ffmpeg::log::Flags::SKIP_REPEATED);
        // 挂过滤回调：把 Error 级下仍会打印的良性坏帧消息按文本滤掉（见上）
        unsafe {
            ffmpeg::ffi::av_log_set_callback(Some(ffmpeg_log_filter_cb));
        }
    });
}

// ---------------- 视频参数与解码帧 ----------------

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
#[allow(dead_code)] // 便捷壳 [`VideoReader::next_frame_bgra`] 用；FFI 走 `next_frame_bgra_into`
pub type DecodedFrame = (f64, u32, u32, Vec<u8>);

// ---------------- 时间换算（视频 / 音频共用）----------------

/// 帧 pts（第纳秒）转秒（time_base 换算）——视频/音频共用的公共换算。
#[inline]
fn pts_to_seconds(pts: i64, time_base: ffmpeg::Rational) -> f64 {
    pts as f64 * f64::from(time_base.numerator()) / f64::from(time_base.denominator())
}

// ---------------- 视频解码器（VideoReader）----------------

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
    ictx: std::mem::ManuallyDrop<ffmpeg::format::context::Input>,
    stream_index: usize,
    decoder: std::mem::ManuallyDrop<ffmpeg::codec::decoder::Video>,
    /// sws 缩放器（sws 输入 → BGRA 输出）；首帧后按**实际帧格式**惰性创建：
    /// 硬件路径回读帧的软件格式（如 NV12/P010）到首个解码帧才确定，软件路径用解码器输出格式。
    scaler: Option<std::mem::ManuallyDrop<ffmpeg::software::scaling::Context>>,
    params: VideoParams,
    /// sws 输出宽（decode-to-display 下采样目标；`params.width` 为源宽）。
    out_width: u32,
    /// sws 输出高（见 `out_width`）。
    out_height: u32,
    /// 帧 pts 使用的 time_base（秒 = pts × num/den）。
    time_base: ffmpeg::Rational,
    /// 是否已进入 EOF drain 模式（send_eof 已发）。
    drained: bool,
    /// 硬件解码状态（`None` = 软件路径）。
    hw: Option<HardwareDecode>,
}

/// 释放持 [`FFMPEG_LOCK`]（见 [`AudioReader`] 同款说明）：`avformat_close_input` 与
/// 另一线程同时 `avformat_open_input` 同文件会 deadlock，故 free 也过同一闸。
/// 手动 drop `scaler/decoder/ictx`；`hw` 字段随后自动 drop（设备引用在解码器释放后才 `unref`）。
impl Drop for VideoReader {
    fn drop(&mut self) {
        let _ffmpeg_guard = FFMPEG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            if let Some(scaler) = &mut self.scaler {
                std::mem::ManuallyDrop::drop(scaler);
            }
            std::mem::ManuallyDrop::drop(&mut self.decoder);
            std::mem::ManuallyDrop::drop(&mut self.ictx);
        }
    }
}

impl VideoReader {
    /// 打开媒体并定位视频流（失败返回带原因的 [Error] 链）；按源码全尺寸解码。
    #[allow(dead_code)] // 测试与外部语义入口用；查看器走 `open_scaled`（不额外预开 reader）
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_scaled(path, u32::MAX)
    }

    /// 同 [`Self::open`]，但 sws 输出按「长边 ≤ `max_dim`」封顶下采样（保持宽高比、
    /// 输出至少 1×1；`max_dim=u32::MAX` 即全尺寸）。`params()` 仍报告源宽高——上层
    /// 用它保持原始宽高比做显示适配；逐帧输出尺寸见 [`DecodedFrame`] 的 w/h。
    ///
    /// 默认软件解码（2026-10 探针结论与「硬解默认开」翻转开关见 [`HW_DEFAULT`] / [`hw_enabled`]）；
    /// `VFF_VIDEO_HW=1|on|true` 走硬件（D3D11VA → DXVA2，见 [`try_open_hw_decoder`]），硬件
    /// 不可用自动回退软件；`VFF_VIDEO_SW=1|on|true` 强制软解（测试/诊断确定性回退）。
    pub fn open_scaled(path: &Path, max_dim: u32) -> Result<Self> {
        Self::open_scaled_impl(path, max_dim, false)
    }

    /// 仅硬件解码打开（无软件回退）——测试/诊断用；硬件不可用即 `Err`。
    #[cfg(test)]
    fn open_hw_only(path: &Path, max_dim: u32) -> Result<Self> {
        Self::open_scaled_impl(path, max_dim, true)
    }

    pub fn open_scaled_hw(path: &Path, max_dim: u32) -> Result<Self> {
        if matches!(
            std::env::var("VFF_VIDEO_SW"),
            Ok(v)
                if v.eq_ignore_ascii_case("1")
                    || v.eq_ignore_ascii_case("on")
                    || v.eq_ignore_ascii_case("true")
        ) {
            return Self::open_scaled_impl(path, max_dim, false);
        }
        // 硬件优先（FFmpeg 自有 D3D11VA/DXVA2 设备，worker 线程独享上下文——无调用方设备
        // 注入、无共享 immediate context，线程安全）；设备/解码器不支持 → 自动回软解。
        match Self::open_scaled_impl(path, max_dim, true) {
            Ok(r) => Ok(r),
            Err(_) => Self::open_scaled_impl(path, max_dim, false),
        }
    }

    fn open_scaled_impl(path: &Path, max_dim: u32, hw_required: bool) -> Result<Self> {
        let _ffmpeg_guard = FFMPEG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        ffmpeg::init().context("ffmpeg init 失败")?;
        quiet_ffmpeg_log_once();

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
            stream_dur as f64
                * f64::from(time_base.numerator())
                / f64::from(time_base.denominator())
        } else {
            0.0
        };

        // 解码器：默认按 [`HW_DEFAULT`]（本机保持软解；改 `true` 即「硬解默认开 + 失败回退」），
        // `hw_required` 时强制硬件（测试路径，失败直接 Err）。
        let (decoder, hw) = match try_open_hw_decoder(&params_for_decoder, hw_required) {
            Some((video, hw)) => (video, Some(hw)),
            None if hw_required => return Err(anyhow!("硬件解码不可用")),
            None => {
                let mut dec = ffmpeg::codec::context::Context::from_parameters(params_for_decoder)
                    .context("解析视频流参数失败")?
                    .decoder();
                // 软件解码默认单线程 = 每帧最少 CPU（headless release 实测：1080p30 单线程
                // ≈11.3ms/帧 ≈0.33 核@30fps，余量 3×；帧线程 4 核虽然墙钟 8.1ms 但忙核 1.67，
                // 每帧 CPU 反而 +20%——对本机「省 CPU」目标有害）。
                // `VFF_DECODE_THREADS=N` 可开到帧线程：仅当单线程解码跟不上实时（如 4K/
                // 高帧率源）才需要，代价是 CPU 略升。
                let threads: usize = std::env::var("VFF_DECODE_THREADS")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(1);
                if threads > 1 {
                    dec.set_threading(ffmpeg::codec::threading::Config {
                        kind: ffmpeg::codec::threading::Type::Frame,
                        count: threads,
                    });
                }
                (dec.video().context("该流不是视频")?, None)
            }
        };

        let (width, height) = (decoder.width(), decoder.height());
        let frame_rate = decoder.frame_rate();
        let has_audio = ictx.streams().best(Type::Audio).is_some();
        let (out_width, out_height) = scaled_dims(width, height, max_dim);

        Ok(Self {
            ictx: std::mem::ManuallyDrop::new(ictx),
            stream_index,
            decoder: std::mem::ManuallyDrop::new(decoder),
            scaler: None,
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
            hw,
        })
    }

    /// 已解析的顶层参数。
    pub fn params(&self) -> VideoParams {
        self.params
    }

    /// 外部设备硬件路径（零回读 GPU 直通）：与 [`Self::open_scaled`] 等价但解码器使用
    /// **调用方提供的 D3D11 设备**（经 FFI 传入 `ID3D11Device*`），并用 [`Self::next_frame_hw`]
    /// 逐帧吐出 GPU 纹理句柄——**不做** `av_hwframe_transfer_data` 回读、**不做** sws，是
    /// 「零回读 GPU 视频」的地基：YUV/NV12 纹理直接绑着色器采样。无软件回退（显式走硬解）。
    ///
    /// 参数语义：`device` 必填（`*mut c_void` = `ID3D11Device*`）、`device_context` 可空
    /// （`ID3D11DeviceContext*`，空则由 FFmpeg 自行 `GetImmediateContext`）。本函数对两个
    /// 接口各 AddRef 一次，解码器关闭（`Drop`）时由 FFmpeg uninit 统一 Release；调用方
    /// 只需自持它的那份引用即可（不夺引用、也不代还引用）。
    ///
    /// `BindFlags` 处理：默认按规范给设备挂 `D3D11_BIND_SHADER_RESOURCE`（帧纹理可被调用方
    /// 直接 SRV 采样）——但**部分驱动（实测 Intel Arc 驱动）拒绝 `DECODER|SHADER_RESOURCE`
    /// 的 NV12 解码数组纹理**，退化时自动「无 SHADER_RESOURCE」重开（解码照常，仅纹理不再
    /// 可被调用方直接采样；返回的 reader 照常工作，见 [`build_external_d3d11_device_ctx`]）。
    ///
    /// **不缩放**：`max_dim` 仅作参数兼容保留——HW 帧不经 sws，输出即源尺寸（`HwFrame`
    /// 自带实际宽高，不受本参数影响）。
    pub fn open_external_hw(
        path: &Path,
        max_dim: u32,
        device: *mut std::ffi::c_void,
        device_context: *mut std::ffi::c_void,
    ) -> Result<Self> {
        let _ffmpeg_guard = FFMPEG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _ = max_dim; // HW 路径不缩放（无 sws），输出即源尺寸
        if device.is_null() {
            return Err(anyhow!("外部 D3D11 设备指针为空"));
        }
        ffmpeg::init().context("ffmpeg init 失败")?;
        quiet_ffmpeg_log_once();

        let ictx = ffmpeg::format::input(path)
            .with_context(|| format!("打开媒体失败: {}", path.display()))?;
        let stream = ictx
            .streams()
            .best(Type::Video)
            .ok_or_else(|| anyhow!("媒体中没有视频流: {}", path.display()))?;
        let stream_index = stream.index();
        let time_base = stream.time_base();
        let params_for_decoder = stream.parameters();

        // 时长：优先容器总长（µs）；为 0/未知时退回视频流 stats 估算（与 open_scaled_impl 同款）
        let container_us = ictx.duration();
        let stream_dur = stream.duration();
        let duration = if container_us > 0 {
            container_us as f64 / 1e6
        } else if stream_dur > 0 {
            stream_dur as f64
                * f64::from(time_base.numerator())
                / f64::from(time_base.denominator())
        } else {
            0.0
        };

        // 注入外部 D3D11 设备：先测驱动是否支持「解码数组纹理带 SHADER_RESOURCE」——
        // 支持 → 带 SHR（调用方可直接采样解码纹理）；不支持（Intel Arc 等驱动实测
        // E_INVALIDARG）→ 无 SHR 设备上下文，解码照常（仅纹理不可再被调用方直接采样）。
        // 探测用最小帧池（不开解码器），无副作用。
        let use_shr = d3d11_supports_shader_bound_textures(device, device_context);
        if !use_shr {
            eprintln!(
                "[video-ffmpeg] 驱动不支持 D3D11VA 解码纹理带 SHADER_RESOURCE（Intel Arc 等驱动实测），\
                 已回退 DECODER-only——解码正常，但纹理不可直接做 SRV 采样"
            );
        }
        let device_buf = build_external_d3d11_device_ctx(device, device_context, use_shr)?;

        let context = ffmpeg::codec::context::Context::from_parameters(params_for_decoder)
            .context("解析视频流参数失败")?;
        let mut decoder = context.decoder();
        unsafe {
            (*decoder.as_mut_ptr()).hw_device_ctx = ffmpeg::ffi::av_buffer_ref(device_buf.0);
        }
        if unsafe { (*decoder.as_ptr()).hw_device_ctx }.is_null() {
            return Err(anyhow!("外部 D3D11VA 设备挂接解码器失败"));
        }
        // D3D11VA 硬解要求单线程（帧池/DecoderOutputView 按单线程假设）；显式钉死 1。
        // 注：h264 D3D11VA 的 hwaccel 在**首帧**才协商启用（open 后 pix_fmt 仍为软件
        // 格式、hw_frames_ctx 为空），故此处不校验、首帧由 hw_frame_from 判定。
        decoder.set_threading(ffmpeg::codec::threading::Config {
            kind: ffmpeg::codec::threading::Type::Frame,
            count: 1,
        });
        let decoder = decoder.video().context("该流不是视频")?;

        let (width, height) = (decoder.width(), decoder.height());
        let frame_rate = decoder.frame_rate();
        let has_audio = ictx.streams().best(Type::Audio).is_some();

        Ok(Self {
            ictx: std::mem::ManuallyDrop::new(ictx),
            stream_index,
            decoder: std::mem::ManuallyDrop::new(decoder),
            scaler: None,
            params: VideoParams {
                width,
                height,
                duration: duration.max(0.0),
                frame_rate: frame_rate.map(|r| (r.numerator() as u32, r.denominator() as u32)),
                has_audio,
            },
            out_width: width,
            out_height: height,
            time_base,
            drained: false,
            hw: Some(HardwareDecode { device: device_buf }),
        })
    }

    /// 跳转到 `seconds`（µs 级、回拉关键帧）：解码器 flush 后从 keyframe 前继续解，
    /// 调用方应丢弃 `pts < 目标` 的帧直至越过目标（或直接取首个可用帧，容忍轻微回拉）。
    pub fn seek(&mut self, seconds: f64) -> Result<()> {
        let _ffmpeg_guard = FFMPEG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        self.seek_inner(seconds)
    }

    /// 无锁版 `seek`（调用方须已持有 [`FFMPEG_LOCK`]）：`avformat_seek_file` + 解码器 flush。
    /// 供 [`Self::skip_to_bgra_into`] 在自身持锁期间复用，避免 `std::Mutex` 不可重入自锁。
    fn seek_inner(&mut self, seconds: f64) -> Result<()> {
        let ts = (seconds.max(0.0) * 1e6) as i64;
        self.ictx
            .seek(ts, i64::MIN..i64::MAX)
            .context("seek 失败")?;
        self.decoder.flush();
        self.drained = false;
        Ok(())
    }

    /// 是否硬件解码（D3D11VA/DXVA2，FFmpeg 自有设备）：供上层决定预滚是否走
    /// [`Self::skip_to_bgra_into`]（硬件跳帧免转换、免回读）。
    pub fn is_hw(&self) -> bool {
        self.hw.is_some()
    }

    /// **seek 预滚专用**：`seek(target)` 后逐帧解码，**落点前帧只解码不转换**（不算 pts 的
    /// sws/回读；硬件帧 ≈0.5ms/帧），直到第一个 `pts ≥ target` 的帧，再把该帧一次性转
    /// BGRA 写进 `out`（须 ≥ 输出 w×h×4，一般用源尺寸预分配即可）。
    ///
    /// 返回 `Some((pts 秒, 宽, 高, 已跳帧数))`；`None` = 到达流末尾仍未到 target（EOF，
    /// 之后可再 seek 复用）。无 pts 的帧一律跳过（不计 skipped）。相比逐帧 `next_frame_bgra_into`
    /// + 上层过滤：省掉每帧 sws/回读（软解 4.6→2.9ms/帧；硬解 4.9→0.5ms/帧），是 seek 墙钟
    /// 的主要加速来源（交接文档「seek2first 占 90%+」直指本路径）。
    ///
    /// 线程归属调用方（worker 拉帧），恒持 [`FFMPEG_LOCK`]；`seek_inner` 不再重复加锁。
    /// 三段逐帧解码循环（seek 预滚 / 逐帧 BGRA / 逐帧 HW）共享的私有**逐帧驱动**：
    /// 骨架 = `next_video_packet → send_packet → 把解码器产出的全部帧给 `accept` 取舍`，
    /// 包耗尽后进入 EOF drain（`send_eof` + 继续 `receive_frame` 取完缓冲）。三条路径
    /// 只供一个 `accept`（每收到一帧调一次）：返回 `Some(_)` 立即交货提前返回，`None`
    /// 继续解下一帧；EOF drain 耗尽仍无帧 → `Ok(None)`（之后可再 seek 复用）。
    ///
    /// **不改任何一条路径的处理细节语义**（跳过/计数/EOF 行为与三条旧循环逐码一致）；
    /// 加锁由公共入口负责，本驱动器不重复上锁。
    fn drain_until<T>(
        &mut self,
        mut accept: impl FnMut(&mut Self, &ffmpeg::frame::Video) -> Result<Option<T>>,
    ) -> Result<Option<T>> {
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
                        if let Some(v) = accept(self, &f)? {
                            return Ok(Some(v));
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
                if let Some(v) = accept(self, &f)? {
                    return Ok(Some(v));
                }
            }
        }
    }

    pub fn skip_to_bgra_into(
        &mut self,
        target: f64,
        out: &mut [u8],
    ) -> Result<Option<(f64, u32, u32, u64)>> {
        let _ffmpeg_guard = FFMPEG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        self.seek_inner(target)?;
        // 落点前帧只解码不转换（不算 skipped 的 sws/回读）；第一个 pts ≥ target 的帧一次性
        // 转 BGRA 交货（`convert_bgra_into` 对已确认有 pts 的帧恒定产出，`None` 仅防御）。
        let mut skipped: u64 = 0;
        self.drain_until(|this, f| {
            let Some(pts) = f.pts() else {
                return Ok(None);
            };
            if this.pts_seconds(pts) >= target {
                Ok(this.convert_bgra_into(f, out)?.map(|(p, w, h)| (p, w, h, skipped)))
            } else {
                skipped += 1;
                Ok(None)
            }
        })
    }

    /// 帧 pts（第纳秒）转秒（time_base 换算）。
    #[inline]
    fn pts_seconds(&self, pts: i64) -> f64 {
        pts_to_seconds(pts, self.time_base)
    }

    /// 推进到**下一帧**并转成 BGRA。返回 [`DecodedFrame`]；
    /// `None` = 已到流末尾（之后可再 `seek` 复用）。
    ///
    /// 便捷壳：内部临时缓冲 + 委托 [`Self::next_frame_bgra_into`]（测试/旧调用点用）。
    #[allow(dead_code)] // 测试与语义入口；播放 FFI 走 `next_frame_bgra_into`（省一次 to_vec）
    pub fn next_frame_bgra(&mut self) -> Result<Option<DecodedFrame>> {
        let cap = (self.params.width as usize)
            .saturating_mul(self.params.height as usize)
            .saturating_mul(4)
            .max(1);
        let mut out = vec![0u8; cap];
        match self.next_frame_bgra_into(&mut out)? {
            Some((p, w, h)) => {
                out.truncate((w as usize) * (h as usize) * 4);
                Ok(Some((p, w, h, out)))
            }
            None => Ok(None),
        }
    }

    /// 推进到**下一帧**并把 BGRA 直接写进调用方提供的 `out`（须 ≥ 输出 w×h×4，一般用
    /// 源尺寸预分配即可）。返回 `(pts 秒, 宽, 高)`；`None` = 流末尾。
    ///
    /// 相比 [`Self::next_frame_bgra`] 省掉内部 `to_vec` 的一次分配+整帧拷贝——播放每帧
    /// 一次（release 下输出 ~2.3MB），是解码→上屏路径里的冗余复制之一。
    pub fn next_frame_bgra_into(&mut self, out: &mut [u8]) -> Result<Option<(f64, u32, u32)>> {
        let _ffmpeg_guard = FFMPEG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        self.drain_until(|this, f| this.convert_bgra_into(f, out))
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

    /// 推进到**下一帧**并返回 GPU 纹理句柄（零回读）——[`HwFrame`]（pts/宽高/
    /// `ID3D11Texture2D*`/数组下标），**不经过** `av_hwframe_transfer_data` 回读与 sws。
    /// `None` = 已到流末尾（之后可再 [`Self::seek`] 复用）；EOF/错误语义与
    /// [`Self::next_frame_bgra_into`] 完全一致（EOF = drain 后无帧）。
    ///
    /// 前置：[`Self::open_external_hw`] 打开。线程归属调用方（worker 拉帧）；返回的
    /// `HwFrame` 为 `Send`，可跨线程持有/释放（槽位引用计数原子，见 [`HwFramePin`]）。
    pub fn next_frame_hw(&mut self) -> Result<Option<HwFrame>> {
        let _ffmpeg_guard = FFMPEG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        self.drain_until(|this, f| Ok(this.hw_frame_from(f)))
    }

    /// 把解码出的一个 HW 帧复制为可独立存活的 [`HwFrame`]：无 pts 的帧跳过（`None`）；
    /// 非 HW 帧防御性跳过（外部设备路径下不应发生）。`av_frame_ref` 把 AVBufferRef 引用
    /// 复制进私有帧——帧池槽位被钉住（见 [`HwFramePin`]），局部 `f` 在下一拍被
    /// `receive_frame` 覆写不影响本帧（我们持有的是引用拷贝而非 `f` 本体）。
    fn hw_frame_from(&self, f: &ffmpeg::frame::Video) -> Option<HwFrame> {
        let Some(pts) = f.pts() else {
            return None;
        };
        if !is_hw_frame(f) {
            return None; // 防御：外部设备路径下不应产出软件帧
        }
        unsafe {
            let raw = f.as_ptr();
            // D3D11 帧布局：data[0] = `ID3D11Texture2D*`；data[1] = 数组纹理元素下标（intptr_t）
            let tex = (*raw).data[0] as usize;
            let array_index = (*raw).data[1] as usize as u32;

            let mut pin_frame = ffmpeg::ffi::av_frame_alloc();
            if pin_frame.is_null() {
                return None;
            }
            if ffmpeg::ffi::av_frame_ref(pin_frame, raw) < 0 {
                ffmpeg::ffi::av_frame_free(&mut pin_frame);
                return None;
            }

            let secs = self.pts_seconds(pts);
            Some(HwFrame {
                pts_seconds: secs,
                width: f.width(),
                height: f.height(),
                tex,
                array_index,
                pin: Box::new(HwFramePin { frame: pin_frame }),
            })
        }
    }

    /// 取（或惰性创建）sws 缩放器：输入 = `src_format` 的 `src_w×src_h`，输出 =
    /// `out_width×out_height` BGRA。首个解码帧调用时建一次，之后复用。
    fn scaler_for(
        &mut self,
        src_format: ffmpeg::format::Pixel,
        src_w: u32,
        src_h: u32,
    ) -> Result<&mut ffmpeg::software::scaling::Context> {
        if self.scaler.is_none() {
            let scaler = ffmpeg::software::scaling::Context::get(
                src_format,
                src_w,
                src_h,
                ffmpeg::format::Pixel::BGRA,
                self.out_width,
                self.out_height,
                ffmpeg::software::scaling::Flags::BILINEAR,
            )
            .context("初始化 sws 缩放失败")?;
            self.scaler = Some(std::mem::ManuallyDrop::new(scaler));
        }
        Ok(self.scaler.as_mut().expect("scaler 已就绪"))
    }

    /// 把解码帧转 BGRA 并写进 `out`（须 ≥ 输出 w×h×4）；无 pts 的帧跳过（返回 `None`）。
    /// 尺寸用 sws 输出（decode-to-display 下采样目标），与 `out_width/out_height` 一致。
    ///
    /// 硬件路径：解码帧为 opaque GPU 帧 → `av_hwframe_transfer_data` 回读到系统内存
    /// （NV12/P010 等软件格式）→ 按该真实格式惰性建 sws → 转 BGRA。
    /// 软件路径：直接按解码器输出格式建 sws（首个软件帧触发）。
    fn convert_bgra_into(
        &mut self,
        f: &ffmpeg::frame::Video,
        out: &mut [u8],
    ) -> Result<Option<(f64, u32, u32)>> {
        let Some(pts) = f.pts() else {
            return Ok(None);
        };
        let mut bgra = ffmpeg::frame::Video::empty();
        if self.hw.is_some() && is_hw_frame(f) {
            // 回读：dst 为空帧即可（transfer 内部按 sw_format 分配数据；Drop 时 av_frame_free）。
            let mut sw = ffmpeg::frame::Video::empty();
            let rc =
                unsafe { ffmpeg::ffi::av_hwframe_transfer_data(sw.as_mut_ptr(), f.as_ptr(), 0) };
            if rc < 0 {
                return Err(anyhow!("硬件帧回读失败 (err={rc})"));
            }
            let fmt = sw.format();
            let (w, h) = (sw.width(), sw.height());
            self.scaler_for(fmt, w, h)?
                .run(&sw, &mut bgra)
                .context("sws 转换失败")?;
        } else {
            let fmt = f.format();
            let (w, h) = (f.width(), f.height());
            self.scaler_for(fmt, w, h)?
                .run(f, &mut bgra)
                .context("sws 转换失败")?;
        }
        let need = (self.out_width as usize) * (self.out_height as usize) * 4;
        if out.len() < need {
            return Err(anyhow!("解码输出缓冲过小: {} < 需要 {}", out.len(), need));
        }
        out[..need].copy_from_slice(&bgra.data(0)[..need]);
        let secs = self.pts_seconds(pts);
        Ok(Some((secs, self.out_width, self.out_height)))
    }
}

// ---------------- 尺寸换算（decode-to-display）----------------

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
