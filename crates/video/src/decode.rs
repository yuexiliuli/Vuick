//! 视频解码核心（主 exe 侧）：**运行时动态加载** `video_ffmpeg.dll` 执行解码。
//!
//! 本模块原为直接链接 `ffmpeg_next` 的静态导入（缺 FFmpeg DLL 时 exe 无法启动）；现解码内核
//! 已迁至工作区成员 `video-ffmpeg`（cdylib，同样用 ffmpeg_next）。主 exe 只在真正打开视频时
//! 经 `libloading` 载入该 DLL：
//! - 载入成功 → 正常解码（FFmpeg DLL 随之被加载，须与 exe / cdylib 同目录或 PATH 上）；
//! - 载入失败（缺 `video_ffmpeg.dll` 或 FFmpeg 运行库）→ [`Self::available`] 为 false，
//!   查看器在窗口内报「缺少视频运行库」，**exe 照常启动**。
//!
//! 解码逻辑与逐帧回归测试均保留在 `video-ffmpeg` crate（`cargo test -p video-ffmpeg`）。
//! FFI 约定见 `video-ffmpeg/src/lib.rs`：句柄透明、输出缓冲由调用方预分配、路径按 UTF-8 传。
//!
//! # 线程纪律
//! 句柄只在解码线程内部创建/使用/释放（裸指针非 `Send`，对齐旧版的「不跨线程搬 reader」）。
//! FFmpeg 并发闸（FFMPEG_LOCK）在 cdylib 内，跨操作的并发串行由它保证。

use std::path::Path;
use std::sync::OnceLock;

use anyhow::{Context as _, Result};

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
#[allow(dead_code)] // cdylib 侧（video-ffmpeg 的 ParamFFI→AudioParams）在用；主侧暂不直接构造
pub struct AudioParams {
    /// 输出采样率（统一重采样到该率）。
    pub sample_rate: u32,
    /// 输出声道数（统一 2，STERO 交错）。
    pub channels: u32,
    /// 音轨总时长（秒）。
    pub duration: f64,
}

// ---------------- 运行时 FFI 桥 ----------------

/// 与 `video-ffmpeg` 的 `ParamFFI` 布局一致。
#[repr(C)]
struct ParamFFI {
    width: u32,
    height: u32,
    duration: f64,
    frame_rate_num: u32,
    frame_rate_den: u32,
    has_audio: u8,
    sample_rate: u32,
    channels: u32,
}

impl From<ParamFFI> for VideoParams {
    fn from(p: ParamFFI) -> Self {
        VideoParams {
            width: p.width,
            height: p.height,
            duration: p.duration,
            frame_rate: (p.frame_rate_num > 0 && p.frame_rate_den > 0)
                .then_some((p.frame_rate_num, p.frame_rate_den)),
            has_audio: p.has_audio != 0,
        }
    }
}

impl From<ParamFFI> for AudioParams {
    fn from(p: ParamFFI) -> Self {
        AudioParams {
            sample_rate: p.sample_rate,
            channels: p.channels,
            duration: p.duration,
        }
    }
}

type Handle = *mut std::ffi::c_void;
type CChar = std::ffi::c_char;
type CInt = std::ffi::c_int;

type FnOpen = unsafe extern "C" fn(*const CChar, u32, *mut Handle) -> CInt;
type FnOpenAudio = unsafe extern "C" fn(*const CChar, u32, *mut Handle) -> CInt;
type FnParams = unsafe extern "C" fn(Handle, *mut ParamFFI);
type FnSeek = unsafe extern "C" fn(Handle, f64) -> CInt;
type FnFrame = unsafe extern "C" fn(Handle, *mut u8, usize, *mut f64, *mut u32, *mut u32) -> CInt;
type FnPcm = unsafe extern "C" fn(Handle, *mut f32, usize, *mut f64, *mut usize) -> CInt;
type FnClose = unsafe extern "C" fn(Handle);

/// 已加载的动态函数集（持有 `Library` 保句柄存活；符号指针在进程生命周期内有效）。
struct Bridge {
    open_video: libloading::Symbol<'static, FnOpen>,
    open_audio: libloading::Symbol<'static, FnOpenAudio>,
    video_params: libloading::Symbol<'static, FnParams>,
    #[allow(dead_code)] // 音轨时长由视频 params 带出；保留以对齐 cdylib 接口
    audio_params: libloading::Symbol<'static, FnParams>,
    video_seek: libloading::Symbol<'static, FnSeek>,
    audio_seek: libloading::Symbol<'static, FnSeek>,
    video_frame: libloading::Symbol<'static, FnFrame>,
    audio_pcm: libloading::Symbol<'static, FnPcm>,
    close_video: libloading::Symbol<'static, FnClose>,
    close_audio: libloading::Symbol<'static, FnClose>,
    #[allow(dead_code)] // 保底：Library 被进程级持有，符号才始终有效
    _lib: libloading::Library,
}

fn find_lib() -> std::path::PathBuf {
    let name = "video_ffmpeg.dll";
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let cands = [dir.join(name), dir.join("..").join(name)];
        for c in cands {
            if c.is_file() {
                return c;
            }
        }
    }
    // 兜底：交给 LoadLibrary 的 exe 目录 / CWD / PATH / 系统目录搜索
    std::path::PathBuf::from(name)
}

fn load_bridge() -> Option<Bridge> {
    let path = find_lib();
    let lib = unsafe { libloading::Library::new(&path) }.ok()?;
    macro_rules! sym {
        ($n:literal, $ty:ty) => {{
            let s = unsafe { lib.get::<$ty>($n).ok()? };
            // 提升借期到 'static：Library 与本 struct 在进程级同生共死，符号不会失效。
            unsafe {
                std::mem::transmute::<libloading::Symbol<'_, $ty>, libloading::Symbol<'static, $ty>>(s)
            }
        }};
    }
    Some(Bridge {
        open_video: sym!(b"vff_open_video\0", FnOpen),
        open_audio: sym!(b"vff_open_audio\0", FnOpenAudio),
        video_params: sym!(b"vff_video_params\0", FnParams),
        audio_params: sym!(b"vff_audio_params\0", FnParams),
        video_seek: sym!(b"vff_video_seek\0", FnSeek),
        audio_seek: sym!(b"vff_audio_seek\0", FnSeek),
        video_frame: sym!(b"vff_video_frame\0", FnFrame),
        audio_pcm: sym!(b"vff_audio_pcm\0", FnPcm),
        close_video: sym!(b"vff_close_video\0", FnClose),
        close_audio: sym!(b"vff_close_audio\0", FnClose),
        _lib: lib,
    })
}

/// 视频运行库是否可用（`video_ffmpeg.dll` 与其 FFmpeg 依赖能否被加载，进程内缓存一次）。
/// 缺库 → false → 查看器在窗口内报错而非害 exe 无法启动。
pub fn available() -> bool {
    bridge().is_some()
}

fn bridge() -> Option<&'static Bridge> {
    static B: OnceLock<Option<Bridge>> = OnceLock::new();
    B.get_or_init(load_bridge).as_ref()
}

fn utf8_cstr(path: &Path) -> Result<std::ffi::CString> {
    let bytes = path.as_os_str().as_encoded_bytes();
    std::ffi::CString::new(bytes).context("路径含 NUL")
}

// ---------------- 视频 Reader ----------------

/// 可 seek 的持久视频解码器（运行时桥接 `video_ffmpeg.dll`）。**非 `Send`**：
/// 句柄只在解码线程本地创建/使用/释放（对齐旧版纪律）。
pub struct VideoReader {
    handle: Handle,
}

impl Drop for VideoReader {
    fn drop(&mut self) {
        if let Some(b) = bridge() {
            unsafe { (b.close_video)(self.handle) }
        }
        self.handle = std::ptr::null_mut();
    }
}

impl VideoReader {
    /// 打开媒体并定位视频流（失败返回带原因的 [Error] 链）；按源码全尺寸解码。
    #[allow(dead_code)] // 测试与外部语义入口用；查看器走 `open_scaled`（不额外预开 reader）
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_scaled(path, u32::MAX)
    }

    /// 同 [`Self::open`]，但 sws 输出按「长边 ≤ `max_dim`」封顶下采样（在 cdylib 内完成）。
    /// `params()` 仍报告源宽高——上层用它保持原始宽高比；逐帧输出尺寸见 [`DecodedFrame`]。
    pub fn open_scaled(path: &Path, max_dim: u32) -> Result<Self> {
        let b = bridge().context("视频运行库不可用（缺 video_ffmpeg.dll 或 FFmpeg 运行库）")?;
        let cpath = utf8_cstr(path)?;
        let mut handle: Handle = std::ptr::null_mut();
        let rc = unsafe { (b.open_video)(cpath.as_ptr(), max_dim, &mut handle) };
        if rc != 0 || handle.is_null() {
            return Err(anyhow::anyhow!("打开媒体失败: {}", path.display()));
        }
        Ok(Self { handle })
    }

    /// 已解析的顶层参数。
    pub fn params(&self) -> VideoParams {
        let b = bridge().expect("reader 持有期桥必在");
        let mut ffi = ParamFFI {
            width: 0,
            height: 0,
            duration: 0.0,
            frame_rate_num: 0,
            frame_rate_den: 0,
            has_audio: 0,
            sample_rate: 0,
            channels: 0,
        };
        unsafe { (b.video_params)(self.handle, &mut ffi) }
        ffi.into()
    }

    /// 跳转到 `seconds`（µs 级、回拉关键帧）：解码器 flush 后从 keyframe 前继续解，
    /// 调用方应丢弃 `pts < 目标` 的帧直至越过目标。
    pub fn seek(&mut self, seconds: f64) -> Result<()> {
        let b = bridge().context("视频运行库不可用")?;
        let rc = unsafe { (b.video_seek)(self.handle, seconds) };
        if rc != 0 {
            return Err(anyhow::anyhow!("seek 失败"));
        }
        Ok(())
    }

    /// 推进到**下一帧**并转成 BGRA。返回 [`DecodedFrame`]；
    /// `None` = 已到流末尾（之后可再 `seek` 复用）。
    pub fn next_frame_bgra(&mut self) -> Result<Option<DecodedFrame>> {
        let b = bridge().context("视频运行库不可用")?;
        // decode-to-display 输出尺寸 ≤ 源尺寸（长边封顶/等比），故源 w×h×4 缓冲必然够用。
        let params = self.params();
        let cap = (params.width as usize)
            .saturating_mul(params.height as usize)
            .saturating_mul(4)
            .max(1);
        let mut buf = vec![0u8; cap];
        let mut pts = 0.0f64;
        let mut w = 0u32;
        let mut h = 0u32;
        let rc = unsafe {
            (b.video_frame)(self.handle, buf.as_mut_ptr(), buf.len(), &mut pts, &mut w, &mut h)
        };
        match rc {
            1 => {
                let need = (w as usize) * (h as usize) * 4;
                buf.truncate(need);
                Ok(Some((pts, w, h, buf)))
            }
            0 => Ok(None),
            _ => Err(anyhow::anyhow!("解码帧失败")),
        }
    }
}

// ---------------- 音频 Reader ----------------

/// 可 seek 的音频解码器：把源音轨统一重采样为 **F32 交错立体声**（`out_rate` Hz）。
/// 行为与旧版一致；解码与重采样在 `video_ffmpeg.dll` 内完成。
pub struct AudioReader {
    handle: Handle,
}

impl Drop for AudioReader {
    fn drop(&mut self) {
        if let Some(b) = bridge() {
            unsafe { (b.close_audio)(self.handle) }
        }
        self.handle = std::ptr::null_mut();
    }
}

impl AudioReader {
    /// 打开媒体音轨并重采样到 `sample_rate`/立体声/F32 交错。
    pub fn open_pcm_f32(path: &Path, sample_rate: u32) -> Result<Self> {
        let b = bridge().context("视频运行库不可用（缺 video_ffmpeg.dll 或 FFmpeg 运行库）")?;
        let cpath = utf8_cstr(path)?;
        let mut handle: Handle = std::ptr::null_mut();
        let rc = unsafe { (b.open_audio)(cpath.as_ptr(), sample_rate, &mut handle) };
        if rc != 0 || handle.is_null() {
            return Err(anyhow::anyhow!("打开音频失败: {}", path.display()));
        }
        Ok(Self { handle })
    }

    /// 已确定的输出参数（速率/声道/时长）。
    #[allow(dead_code)] // cdylib 侧在用；主侧供音频 worker 可选读取
    pub fn params(&self) -> AudioParams {
        let b = bridge().expect("reader 持有期桥必在");
        let mut ffi = ParamFFI {
            width: 0,
            height: 0,
            duration: 0.0,
            frame_rate_num: 0,
            frame_rate_den: 0,
            has_audio: 0,
            sample_rate: 0,
            channels: 0,
        };
        unsafe { (b.audio_params)(self.handle, &mut ffi) }
        ffi.into()
    }

    /// 跳转到 `seconds`：decode flush + drain 复位。
    pub fn seek(&mut self, seconds: f64) -> Result<()> {
        let b = bridge().context("视频运行库不可用")?;
        let rc = unsafe { (b.audio_seek)(self.handle, seconds) };
        if rc != 0 {
            return Err(anyhow::anyhow!("音频 seek 失败"));
        }
        Ok(())
    }

    /// 推进到**下一块** F32 交错样本。`(pts 秒, 样本)`（样本数为偶数=立体声）；
    /// `None` = EOF（之后可再 `seek` 复用）。
    pub fn next_pcm_f32(&mut self) -> Result<Option<(f64, Vec<f32>)>> {
        let b = bridge().context("视频运行库不可用")?;
        // 音频块一般 ≤ 2s×通道；给足量缓冲（内部每次拉一小块交还）。
        let cap = 1 << 16; // 65536 样本（≈0.68s@48k 立体声）——见 worker 循环，多调几次拼块
        let mut buf = vec![0f32; cap];
        let mut pts = 0.0f64;
        let mut written = 0usize;
        let rc = unsafe {
            (b.audio_pcm)(
                self.handle,
                buf.as_mut_ptr(),
                buf.len(),
                &mut pts,
                &mut written,
            )
        };
        match rc {
            1 => {
                buf.truncate(written);
                Ok(Some((pts, buf)))
            }
            0 => Ok(None),
            _ => Err(anyhow::anyhow!("解码音频失败")),
        }
    }
}

