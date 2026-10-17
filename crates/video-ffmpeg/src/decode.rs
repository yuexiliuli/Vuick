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
use std::sync::{Mutex, Once};

use anyhow::{Context as _, Result, anyhow};

use ffmpeg_next as ffmpeg;
use ffmpeg::media::Type;

/// 进程级 FFmpeg 闸：本构建（ffmpeg 9.0 gyan）里**并发的 FFmpeg 操作**会互相占死——
/// 实测切 72.MP4：新视频线程 `avformat_open_input` 与音频线程解码/再打开并行时直接卡死
/// 在 `format::input`（开放闸只串 open 无效，open 与解码也会互相卡）。因此**所有 FFI
/// 调用**（open / seek / next_frame / next_pcm）都过此互斥量串行；单次操作毫秒级，
/// 视频 2 帧通道 + 音频 1s 缓冲足以吸收串行抖动。打开后、解码途中均释放，不长期持锁。
static FFMPEG_LOCK: Mutex<()> = Mutex::new(());

/// FFmpeg 日志级别一次性收敛到 `Error`（进程内仅执行一次）：ffmpeg 默认 `Warning` 会把
/// 大量良性抖动打到 stderr（如 mp4/mov 的 `udta` 盒布局异常 → `UDTA parsing failed
/// retrying raw` 后仍能正常播放）。保留 `Error` 级让真正解码错误仍可见。用独立
/// `Once` 保证幂等（早于首次 open 由播放线程触发）。
static FFMPEG_LOG_QUIET: Once = Once::new();
fn quiet_ffmpeg_log_once() {
    FFMPEG_LOG_QUIET.call_once(|| {
        ffmpeg::log::set_level(ffmpeg::log::Level::Error);
        // 连续重复的同一错误只打一次（ffmpeg 自带「last message repeated N times」收敛）——
        // HE-AAC SBR 断链坏帧在反复 seek 时会重复触发同一 `env_facs_q ... is invalid`，
        // 不开这会逐帧刷屏把 stderr 灌爆（也是 UI 卡顿源之一）。
        ffmpeg::log::set_flags(ffmpeg::log::Flags::SKIP_REPEATED);
    });
}

// ---------------- 硬件解码（D3D11VA / DXVA2）----------------

/// FFmpeg `AVBufferRef` 所有权包装：Drop 时 `av_buffer_unref` 释放引用。
struct AvBuf(*mut ffmpeg::ffi::AVBufferRef);

impl Drop for AvBuf {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { ffmpeg::ffi::av_buffer_unref(&mut self.0) }
        }
    }
}

/// 已激活的硬件解码状态：持有一份 GPU 设备上下文引用，保证设备在 decoder 生命期内存活。
///
/// 解码器 avctx 在 open 时另持一份引用（`hw_device_ctx`），本引用只是兜底保活；
/// `VideoReader::Drop` 先关解码器（`avcodec_free_context` 释放 avctx 的引用），
/// 字段自动 drop 时再 `av_buffer_unref` 本引用，顺序安全。
struct HardwareDecode {
    /// 仅作 RAII 保活（Drop 时 `av_buffer_unref`）；「读取」即持有引用的副作用。
    #[allow(dead_code)]
    device: AvBuf,
}

/// 硬件解码是否启用（**默认关**，软件为本机实测更优基线）：`VFF_VIDEO_HW=1|on|true` 显式开启。
/// 硬件路径（D3D11VA/DXVA2）把解码搬上 GPU，但 GPUI 需 CPU 像素 → 每帧整帧回读，
/// 在强 CPU 上收益被回读抵销（实测反略高）；弱 CPU / 4K / 高码率机器可显式开启。
fn hw_enabled() -> bool {
    matches!(
        std::env::var("VFF_VIDEO_HW"),
        Ok(v)
            if v.eq_ignore_ascii_case("1")
                || v.eq_ignore_ascii_case("on")
                || v.eq_ignore_ascii_case("true")
    )
}

/// 尝试用硬件打开视频解码器：依次尝试 D3D11VA → DXVA2，任一环节失败（设备不可用 / 编解码器
/// 不支持 / 无帧池）返回 `None`，由调用方回落软件路径或报错。`force` 为测试/诊断用——
/// 绕过默认开关强制试硬件（保证硬件回归测试不被默认关策略屏蔽）。
///
/// `ffmpeg-next` 9.0 已移除 `hw_decode` 安全封装，这里直接操作 `ffmpeg::ffi` 原始符号，
/// 只在 open 期做「设备创建 + 挂接」；逐帧仍走受管 `scaler`，输出路径与软件版一致。
fn try_open_hw_decoder(
    parameters: &ffmpeg::codec::Parameters,
    force: bool,
) -> Option<(ffmpeg::codec::decoder::Video, HardwareDecode)> {
    use ffmpeg::ffi::{
        AVBufferRef, AVHWDeviceType, av_buffer_ref, av_hwdevice_ctx_create,
    };
    if !force && !hw_enabled() {
        return None;
    }
    const HW_TYPES: [AVHWDeviceType; 2] = [
        AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA,
        AVHWDeviceType::AV_HWDEVICE_TYPE_DXVA2,
    ];
    for hw_type in HW_TYPES {
        let mut device_ptr: *mut AVBufferRef = std::ptr::null_mut();
        let rc = unsafe {
            av_hwdevice_ctx_create(
                &mut device_ptr,
                hw_type,
                std::ptr::null(),
                std::ptr::null_mut(),
                0,
            )
        };
        if rc < 0 || device_ptr.is_null() {
            continue;
        }
        let device = AvBuf(device_ptr);
        let context = match ffmpeg::codec::context::Context::from_parameters(parameters.clone())
        {
            Ok(c) => c,
            Err(_) => continue,
        };
        let mut decoder = context.decoder();
        unsafe {
            (*decoder.as_mut_ptr()).hw_device_ctx = av_buffer_ref(device.0);
        }
        if unsafe { (*decoder.as_ptr()).hw_device_ctx }.is_null() {
            continue;
        }
        let opened = match decoder.video() {
            Ok(v) => v,
            Err(_) => continue,
        };
        return Some((opened, HardwareDecode { device }));
    }
    None
}

/// 解码帧是否为硬件（opaque）帧：按像素格式描述符的 `AV_PIX_FMT_FLAG_HWACCEL` 判定，
/// 不依赖具体像素格式名（D3D11 / DXVA2_VLD / ... 一概覆盖）。
fn is_hw_frame(frame: &ffmpeg::frame::Video) -> bool {
    unsafe {
        // AVFrame.format 在绑定里展开成底层 i32；repr(i32) 枚举按位重建判别值再查描述符。
        let fmt: ffmpeg::ffi::AVPixelFormat =
            std::mem::transmute((*frame.as_ptr()).format);
        let desc = ffmpeg::ffi::av_pix_fmt_desc_get(fmt);
        !desc.is_null() && (*(desc)).flags & (ffmpeg::ffi::AV_PIX_FMT_FLAG_HWACCEL as u64) != 0
    }
}

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
    resampler: std::mem::ManuallyDrop<ffmpeg::software::resampling::Context>,
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
    fn flush_resampler(&mut self) -> Vec<f32> {
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
                let p_sec = p as f64 * f64::from(self.time_base.numerator())
                    / f64::from(self.time_base.denominator());
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
                let pts = decoded.pts().unwrap_or(0) as f64
                    * f64::from(self.time_base.numerator())
                    / f64::from(self.time_base.denominator());
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
    /// 默认软件解码（本机实测更优）；设 `VFF_VIDEO_HW=1|on|true` 走硬件
    /// （D3D11VA → DXVA2，见 [`try_open_hw_decoder`]），硬件不可用自动回退软件。
    pub fn open_scaled(path: &Path, max_dim: u32) -> Result<Self> {
        Self::open_scaled_impl(path, max_dim, false)
    }

    /// 仅硬件解码打开（无软件回退）——测试/诊断用；硬件不可用即 `Err`。
    #[cfg(test)]
    fn open_hw_only(path: &Path, max_dim: u32) -> Result<Self> {
        Self::open_scaled_impl(path, max_dim, true)
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

        // 解码器：默认软件（本机实测更优），`hw_required` 时强制硬件（测试路径，失败直接 Err）。
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

    /// 跳转到 `seconds`（µs 级、回拉关键帧）：解码器 flush 后从 keyframe 前继续解，
    /// 调用方应丢弃 `pts < 目标` 的帧直至越过目标（或直接取首个可用帧，容忍轻微回拉）。
    pub fn seek(&mut self, seconds: f64) -> Result<()> {
        let _ffmpeg_guard = FFMPEG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
                        if let Some(r) = self.convert_bgra_into(&f, out)? {
                            return Ok(Some(r));
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
                if let Some(r) = self.convert_bgra_into(&f, out)? {
                    return Ok(Some(r));
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
        let secs = pts as f64 * f64::from(self.time_base.numerator())
            / f64::from(self.time_base.denominator());
        Ok(Some((secs, self.out_width, self.out_height)))
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
    use anyhow::Context as _;

    fn fixture() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures")
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
            .join("../../fixtures")
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

    /// 音频 reader 常驻复用（查看器音频 worker 复用姿势）：open 一次后连续 seek 回不同
    /// 落点，每次重新起播首块 pts 落在目标附近、不重开文件；`AudioReader::seek` 在
    /// 复位 demux/decoder 的同时冲掉 swr 内部残留（不残留旧段）。缺夹具 SKIP（音频
    /// 夹具不入库，同 viewer 约定）。
    #[test]
    fn audio_seek_reuses_reader_across_targets() {
        let path = audio_fixture();
        if !path.exists() {
            eprintln!("SKIP: missing audio fixture（不入库，本机无真音频）");
            return;
        }
        let mut r = AudioReader::open_pcm_f32(&path, 48000).unwrap();
        for target in [0.1, 1.0, 0.35, 1.7] {
            r.seek(target).unwrap();
            let mut got: Option<(f64, Vec<f32>)> = None;
            for _ in 0..200 {
                match r.next_pcm_f32() {
                    Ok(Some(x)) => {
                        got = Some(x);
                        break;
                    }
                    _ => break, // EOF / 错误
                }
            }
            let (pts, samples) = got.expect("seek 后应出音频（复用不重开）");
            assert!(
                (pts - target).abs() < 0.30,
                "音频复用 seek 首块落点偏离 target={target} pts={pts}"
            );
            assert!(!samples.is_empty(), "首块应有样本");
        }
    }

    #[test]
    fn audio_rejects_missing() {
        let bad = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("no-such.mp4");
        assert!(AudioReader::open_pcm_f32(&bad, 48000).is_err());
    }

    /// 回归：重采样器内部残留必须被**真实排空**（复用 reader seek 前不清残留会把旧段
    /// 夹进新起播）。ffmpeg-next `Context::flush` 对全空 out 帧会因 `swr_convert_frame`
    /// 的 `config_changed` 直接报 `Output changed`、一次都排不出——按输出规格预分配帧
    /// 后（[`AudioReader::flush_resampler`]）才排出；终止判据用「产出样本数」，不能用
    /// `delay()`（排空后 `swr_get_delay` 仍非零，按它判死循环）。用 out_rate≠源率强制
    /// 真实重采样制造残留。缺夹具 SKIP（同 viewer 约定）。
    #[test]
    fn audio_drain_clears_resampler_residue() {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/视频测试.mp4");
        if !path.exists() {
            eprintln!("SKIP: missing target/视频测试.mp4（夹具不入库）");
            return;
        }
        // 44100 ≠ 源率（48k）→ 重力采样，读几块必有内部残留
        let mut r = AudioReader::open_pcm_f32(&path, 44100).unwrap();
        let mut got_block = false;
        for _ in 0..5 {
            if r.next_pcm_f32().unwrap().is_none() {
                break;
            }
            got_block = true;
        }
        assert!(got_block, "夹具应能解码出音频块");
        let delay_before = r.resampler.delay();
        assert!(delay_before.is_some(), "真实重采样必然积累内部残留 delay={delay_before:?}");
        let drained = r.flush_resampler();
        assert!(
            !drained.is_empty(),
            "残留应被真实排空（空帧 flush 会 Err 落空，drain 长度应为 0 之前）len={}",
            drained.len()
        );
        assert!(
            drained.len() <= 2 * 44100,
            "返回长度应截到实际产出（planes 全容量未定段不得带出），len={}",
            drained.len()
        );
        let delay_after = r.resampler.delay();
        // 排空后不应再有余量可排（下一次 flush 产出 0）——以再排一次产出为判据
        let mut out = ffmpeg::frame::Audio::new(
            r.resampler.output().format,
            r.resampler.output().rate as usize,
            r.resampler.output().channel_layout,
        );
        let more = match r.resampler.flush(&mut out) {
            Ok(Some(_)) if out.samples() > 0 => out.samples(),
            _ => 0,
        };
        assert_eq!(more, 0, "排空后再 flush 不应有产出");
        let _ = delay_after; // 提示值：排空后 delay 仍可能非零（引擎内插延迟），不作断言
        // 排空后读取仍正常（不破坏后续解码）
        assert!(r.next_pcm_f32().is_ok(), "排空后读取应当正常");
    }

    /// 双路径（硬件优先 / 软件回退）都必须产出尺寸正确、alpha 不透明的 BGRA 帧。
    #[test]
    fn hw_or_sw_path_both_output_valid_bgra() {
        let mut r = VideoReader::open_scaled(&fixture(), 160).unwrap();
        eprintln!(
            "video-ffmpeg 解码路径: {}",
            if r.hw.is_some() { "硬件" } else { "软件回退" }
        );
        let (pts, w, h, bgra) = r.next_frame_bgra().unwrap().expect("帧应可解码");
        assert!(pts >= 0.0);
        assert_eq!((w, h), (160, 120));
        assert_eq!(bgra.len(), 160 * 120 * 4);
        assert!(
            bgra.as_chunks::<4>().0.iter().all(|px| px[3] == 255),
            "sws 输出应全不透明"
        );
    }

    /// 仅硬件路径（无软件回退）：本机支持硬件解码时逐帧解码 + seek 全通过；
    /// 不支持（无 GPU / 驱动 / 编解码器）则打印跳过，避免 CI 误红。
    #[test]
    fn hw_only_path_decodes_when_available() {
        match VideoReader::open_hw_only(&fixture(), u32::MAX) {
            Ok(mut r) => {
                assert!(r.hw.is_some(), "open_hw_only 必须启用硬件");
                for _ in 0..16 {
                    let (pts, w, h, bgra) =
                        r.next_frame_bgra().unwrap().expect("硬件帧应可解码");
                    assert!(pts >= 0.0);
                    assert_eq!((w, h), (320, 240), "硬件路径输出应按源尺寸");
                    assert_eq!(bgra.len(), 320 * 240 * 4);
                }
                r.seek(1.0).unwrap();
                assert!(
                    r.next_frame_bgra().unwrap().is_some(),
                    "硬件路径 seek 后应出帧"
                );
            }
            Err(e) => eprintln!("本机硬件解码不可用，跳过: {e:#}"),
        }
    }

    // ---------------- 解码吞吐 / 拆账基准（VFF_BENCH_* 环境, `--ignored` 手动跑） ----------------

    const ST_DEMUX: usize = 0;
    const ST_DECODE: usize = 1;
    const ST_TRANSFER: usize = 2;
    const ST_SWS: usize = 3;
    const ST_COPY: usize = 4;

    /// 基准文件：`VFF_BENCH_VIDEO` 指定，缺省仓库 `target/视频测试.mp4`（1280x720 h264@30）。
    fn bench_candidate() -> std::path::PathBuf {
        std::env::var("VFF_BENCH_VIDEO")
            .ok()
            .map(std::path::PathBuf::from)
            .filter(|p| p.is_file())
            .unwrap_or_else(|| {
                std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/视频测试.mp4")
            })
    }

    /// 生产路径逐帧墙钟（`next_frame_bgra_into` 原样调用）：
    /// 报 min / 中位 / 均值 / p95 ms·帧 + 统计窗口 fps + 「忙核@30fps」估算（均值 ms×30/1000）。
    /// 前 `warmup` 帧剔除（scaler / 硬件设备预热）；文件短于目标帧数时 EOF→seek(0) 复跑凑数。
    fn bench_leg_canonical(name: &str, mut r: VideoReader, frames: u32, warmup: u32) {
        let cap = (r.out_width as usize) * (r.out_height as usize) * 4;
        let mut out = vec![0u8; cap];
        let mut times: Vec<f64> = Vec::with_capacity(frames as usize);
        let mut n: u32 = 0;
        let mut win_start: Option<std::time::Instant> = None;
        while n < frames {
            let t = std::time::Instant::now();
            match r.next_frame_bgra_into(&mut out) {
                Ok(Some(_)) => {
                    let dt = t.elapsed().as_secs_f64() * 1e3;
                    n += 1;
                    if n > warmup {
                        if win_start.is_none() {
                            win_start = Some(std::time::Instant::now());
                        }
                        times.push(dt);
                    }
                }
                Ok(None) => r.seek(0.0).unwrap(),
                Err(e) => panic!("{name} 帧解码失败: {e:#}"),
            }
        }
        match win_start {
            Some(w0) => {
                let win = (std::time::Instant::now().duration_since(w0).as_secs_f64()).max(1e-6);
                let keep = times.len();
                let mut sorted = times.clone();
                sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
                let (min, med, p95) = (sorted[0], sorted[keep / 2], sorted[(keep as f64 * 0.95) as usize]);
                let mean = times.iter().sum::<f64>() / keep as f64;
                eprintln!(
                    "[BENCH] {name}: 采样={keep}帧 min={min:.3} 中位={med:.3} 均值={mean:.3} p95={p95:.3} ms/帧 | {:.0} fps | 忙核@30fps≈{:.2}",
                    keep as f64 / win,
                    mean * 30.0 / 1000.0
                );
            }
            None => eprintln!("[BENCH] {name}: 采样帧数不足（frames={frames} warmup={warmup}）"),
        }
    }

    /// 逐帧拆账：镜像 [`VideoReader::next_frame_bgra_into`]+[`VideoReader::convert_bgra_into`] 的
    /// 主循环（测试模块可访问私有字段），把每帧 CPU 墙钟按 demux / decode / transfer / sws / copy
    /// 累加后除以「产出帧数」报均值。软件路径 transfer 恒 0。
    ///
    /// ⚠️ 硬解 decode 墙钟常被 GPU 异步缓冲吸收（D3D11VA 提交即返回，真实成本滞后到回读
    /// `av_hwframe_transfer_data` 的等待上）——故 decode/transfer 分开看，净值看 canonical 全帧。
    fn bench_leg_breakdown(name: &str, mut r: VideoReader, frames: u32) -> anyhow::Result<()> {
        let cap = (r.out_width as usize) * (r.out_height as usize) * 4;
        let mut out = vec![0u8; cap];
        let mut stage = [0.0f64; 5];
        let mut n: u32 = 0;
        while n < frames {
            let t = std::time::Instant::now();
            let packet = r.next_video_packet();
            stage[ST_DEMUX] += t.elapsed().as_secs_f64();
            let packet = match packet {
                Some(p) => p,
                None => {
                    if !r.drained {
                        r.decoder.send_eof().ok();
                        r.drained = true;
                    }
                    let mut f = ffmpeg::frame::Video::empty();
                    loop {
                        let t = std::time::Instant::now();
                        let ok = r.decoder.receive_frame(&mut f).is_ok();
                        stage[ST_DECODE] += t.elapsed().as_secs_f64();
                        if !ok {
                            break;
                        }
                        if bench_convert_stage(&mut r, &f, &mut out, &mut stage)?.is_some() {
                            n += 1;
                        }
                    }
                    r.seek(0.0).unwrap();
                    continue;
                }
            };
            let t = std::time::Instant::now();
            r.decoder
                .send_packet(&packet)
                .context("send_packet 失败")?;
            stage[ST_DECODE] += t.elapsed().as_secs_f64();
            let mut f = ffmpeg::frame::Video::empty();
            loop {
                let t = std::time::Instant::now();
                let ok = r.decoder.receive_frame(&mut f).is_ok();
                stage[ST_DECODE] += t.elapsed().as_secs_f64();
                if !ok {
                    break;
                }
                if bench_convert_stage(&mut r, &f, &mut out, &mut stage)?.is_some() {
                    n += 1;
                }
            }
        }
        let per = |s: f64| s / n as f64 * 1e3;
        eprintln!(
            "[BENCH] {name} 拆账 ({n}帧): demux={:.3} decode={:.3} transfer={:.3} sws={:.3} copy={:.3} ms/帧",
            per(stage[ST_DEMUX]),
            per(stage[ST_DECODE]),
            per(stage[ST_TRANSFER]),
            per(stage[ST_SWS]),
            per(stage[ST_COPY])
        );
        Ok(())
    }

    /// 拆账用的转换阶段（镜像 [`VideoReader::convert_bgra_into`] 计时）：HW opaque 帧先回读
    /// （`av_hwframe_transfer_data`），再 sws 转 BGRA，再拷入输出缓冲。
    fn bench_convert_stage(
        r: &mut VideoReader,
        f: &ffmpeg::frame::Video,
        out: &mut [u8],
        stage: &mut [f64; 5],
    ) -> anyhow::Result<Option<(f64, u32, u32)>> {
        let Some(pts) = f.pts() else {
            return Ok(None);
        };
        let mut bgra = ffmpeg::frame::Video::empty();
        if r.hw.is_some() && is_hw_frame(f) {
            let mut sw = ffmpeg::frame::Video::empty();
            let t = std::time::Instant::now();
            let rc =
                unsafe { ffmpeg::ffi::av_hwframe_transfer_data(sw.as_mut_ptr(), f.as_ptr(), 0) };
            if rc < 0 {
                return Err(anyhow!("硬件帧回读失败 (err={rc})"));
            }
            stage[ST_TRANSFER] += t.elapsed().as_secs_f64();
            let fmt = sw.format();
            let (w, h) = (sw.width(), sw.height());
            let t = std::time::Instant::now();
            r.scaler_for(fmt, w, h)?
                .run(&sw, &mut bgra)
                .context("sws 转换失败")?;
            stage[ST_SWS] += t.elapsed().as_secs_f64();
        } else {
            let fmt = f.format();
            let (w, h) = (f.width(), f.height());
            let t = std::time::Instant::now();
            r.scaler_for(fmt, w, h)?
                .run(f, &mut bgra)
                .context("sws 转换失败")?;
            stage[ST_SWS] += t.elapsed().as_secs_f64();
        }
        let need = (r.out_width as usize) * (r.out_height as usize) * 4;
        let t = std::time::Instant::now();
        out[..need].copy_from_slice(&bgra.data(0)[..need]);
        stage[ST_COPY] += t.elapsed().as_secs_f64();
        let secs = pts as f64 * f64::from(r.time_base.numerator())
            / f64::from(r.time_base.denominator());
        Ok(Some((secs, r.out_width, r.out_height)))
    }

    /// 本机硬件解码设备可用性探针（`--ignored` 手动跑）：逐个尝试建 D3D11VA / DXVA2 设备上下文，
    /// 打印哪个成功（打开媒体逻辑按此顺序优先启用）。输出即证据。
    #[test]
    #[ignore = "手动测量用"]
    fn probe_hw_device() {
        use ffmpeg::ffi::{
            AVHWDeviceType, av_buffer_unref, av_hwdevice_ctx_create,
        };
        let names = [
            (AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA, "D3D11VA"),
            (AVHWDeviceType::AV_HWDEVICE_TYPE_DXVA2, "DXVA2"),
        ];
        for (t, name) in names {
            let mut p: *mut ffmpeg::ffi::AVBufferRef = std::ptr::null_mut();
            let rc = unsafe {
                av_hwdevice_ctx_create(&mut p, t, std::ptr::null(), std::ptr::null_mut(), 0)
            };
            eprintln!("[HWPROBE] {name}: av_hwdevice_ctx_create rc={rc} ok={}", !p.is_null());
            if !p.is_null() {
                unsafe { av_buffer_unref(&mut p); }
            }
        }
    }

    /// 解码吞吐 / 拆账测量（非回归，默认忽略手动跑）。
    ///
    /// 软硬件对照：软路径强制软件（`open_scaled_impl(.., false)`，无视 `VFF_VIDEO_HW` 环境）；
    /// 硬路径走 `open_hw_only`（force，无软回退，不可用即打印证据并跳过硬解对照）。
    ///
    /// 环境：
    /// - `VFF_BENCH_VIDEO` 基准文件（缺省 `target/视频测试.mp4`）
    /// - `VFF_BENCH_FRAMES` 采样帧数（默认 240，剔除前 20 帧预热；文件短→EOF seek 复跑）
    /// - `VFF_BENCH_PATH=sw|hw` 限定单路径；缺省软硬都跑（软先硬后，按外部进程采样内存用）
    #[test]
    #[ignore = "手动测量用"]
    fn bench_decode_throughput() -> anyhow::Result<()> {
        let candidate = bench_candidate();
        let frames: u32 = std::env::var("VFF_BENCH_FRAMES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(240);
        let warmup: u32 = 20.min(frames.saturating_sub(1));
        let mode = std::env::var("VFF_BENCH_PATH").unwrap_or_default();
        let info = |r: &VideoReader| {
            eprintln!(
                "[BENCH] 文件={} 源尺寸={}x{} 输出={}x{} 帧率={:?} 目标帧数={frames} warmup={warmup}",
                candidate.display(),
                r.params().width,
                r.params().height,
                r.out_width,
                r.out_height,
                r.params().frame_rate,
            );
        };
        if mode.is_empty() || mode == "sw" {
            let r = VideoReader::open_scaled_impl(&candidate, 1920, false)
                .context("软路径打开失败")?;
            assert!(r.hw.is_none(), "软路径必须无硬件（VFF_VIDEO_HW 混入环境）");
            info(&r);
            bench_leg_canonical("软解", r, frames, warmup);
            let r2 = VideoReader::open_scaled_impl(&candidate, 1920, false)
                .context("软路径拆账重开失败")?;
            bench_leg_breakdown("软解", r2, frames)?;
        }
        if mode.is_empty() || mode == "hw" {
            match VideoReader::open_hw_only(&candidate, 1920) {
                Ok(r) => {
                    assert!(r.hw.is_some(), "open_hw_only 必须启用硬件");
                    info(&r);
                    bench_leg_canonical("硬解", r, frames, warmup);
                    match VideoReader::open_hw_only(&candidate, 1920) {
                        Ok(r2) => bench_leg_breakdown("硬解", r2, frames)?,
                        Err(e) => eprintln!("硬解拆账重开失败: {e:#}"),
                    }
                }
                Err(e) => eprintln!(
                    "本机硬件解码不可用，跳过硬解对照: {e:#}\n证据：open_hw_only(path,1920) 返回 Err"
                ),
            }
        }
        Ok(())
    }

    /// 回归：HE-AAC(MKV) 反复 seek（等价反复点进度条）——seek 落点非 AAC 帧对齐 + MKV
    /// demux 落点可大幅回退（本夹具实测首块可落后目标 ≈-12s）+ HE-AAC SBR 断链坏帧
    ///（`env_facs_q ... is invalid`）。修复前：起播音在落点前（预滚整段被播放/越位），
    /// 边界坏帧刷屏且可能断流。断言：seek 后首块必须 ≥ 落点且落在落点后一帧内
    ///（[`AudioReader::skip_until`] 丢弃落点前包/坏帧）、样本有限非空、逐块 pts 单调
    /// 不回头。缺夹具 SKIP（`target\视频测试字幕.mkv` 不入库）。
    #[test]
    fn he_aac_mkv_seek_skips_preroll_and_starts_clean() {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/视频测试字幕.mkv");
        if !path.exists() {
            eprintln!("SKIP: missing target/视频测试字幕.mkv（夹具不入库，本机无 HE-AAC MKA 样例）");
            return;
        }
        let mut r = AudioReader::open_pcm_f32(&path, 48000).unwrap();
        // 覆盖：开头 / 中部 / 小数字 / 近结尾（避开 EOF 边缘），含来回跳
        let targets = [5.0, 123.456, 500.0, 999.9, 1300.5, 1650.0];
        for &t in &targets {
            r.seek(t).unwrap();
            let mut first: Option<f64> = None;
            let mut last = -1.0f64;
            let mut n = 0u32;
            for _ in 0..60 {
                match r.next_pcm_f32().unwrap_or_else(|e| panic!("seek {t} 读取失败: {e:#}")) {
                    Some((pts, samples)) => {
                        assert!(
                            pts >= last - 1e-9,
                            "seek {t} 后逐块 pts 应单调不回头: pts={pts} last={last}"
                        );
                        last = pts;
                        if first.is_none() {
                            first = Some(pts);
                            assert!(
                                pts >= t - 1e-6,
                                "seek {t} 后首块必须 ≥ 落点（否则落点前预滚被播放/越位），实得 {pts}"
                            );
                            assert!(
                                (pts - t).abs() < 0.1,
                                "seek {t} 后首块应落在落点后一帧内（warm 起播），实得 {pts}"
                            );
                            assert!(
                                !samples.is_empty() && samples.iter().all(|s| s.is_finite()),
                                "seek {t} 后首块样本应非空且有限（clean 帧，非 SBR 坏帧）len={}",
                                samples.len()
                            );
                        }
                        n += 1;
                        if n >= 6 {
                            break;
                        }
                    }
                    None => break, // 近结尾：落点后已无块，接受
                }
            }
            assert!(n >= 3, "seek {t} 后应连续读出多块，实得 {n}");
        }
    }

    /// 回归（A2）：**普通打开从头播放**（不经 seek）也套用 skip 口径——流头若带 `pts<0`
    /// 包/首个坏 AAC 帧，须在输出前丢弃到第一个 clean 帧，正常打开 stderr 不刷
    /// `env_facs_q`。断言：open 后首块 pts ≥ 0、样本有限非空、逐块 pts 单调。缺夹具 SKIP。
    #[test]
    fn open_from_start_skips_head_noise_first_frame_clean() {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/视频测试字幕.mkv");
        if !path.exists() {
            eprintln!("SKIP: missing target/视频测试字幕.mkv（夹具不入库）");
            return;
        }
        let mut r = AudioReader::open_pcm_f32(&path, 48000).unwrap();
        let mut last = -1.0f64;
        let mut first: Option<f64> = None;
        let mut n = 0u32;
        for _ in 0..12 {
            match r.next_pcm_f32().unwrap() {
                Some((pts, samples)) => {
                    assert!(pts >= last - 1e-9, "open 从头播放逐块 pts 应单调: pts={pts} last={last}");
                    last = pts;
                    if first.is_none() {
                        first = Some(pts);
                        assert!(pts >= -1e-6, "open 首块 pts 不应为负（负时间戳包被丢弃）: {pts}");
                        assert!(
                            !samples.is_empty() && samples.iter().all(|s| s.is_finite()),
                            "open 首块样本应非空且有限（clean 帧）"
                        );
                    }
                    n += 1;
                }
                None => break,
            }
        }
        assert!(n >= 3, "open 后应连续读出多块，实得 {n}");
    }
}
