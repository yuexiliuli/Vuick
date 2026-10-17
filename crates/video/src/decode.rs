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
#[allow(dead_code)] // 便捷壳 [`VideoReader::next_frame_bgra`] 用；播放 worker 走 `_into`（复用缓冲）
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

// 字幕 FFI 类型
#[repr(C)]
#[allow(dead_code)] // FFI 布局定义，实际用独立函数取字幕
struct SubtitleParamsFFI {
    stream_count: u32,
    format: u32,
}

#[repr(C)]
struct SubtitleEntryFFI {
    start_pts: f64,
    end_pts: f64,
    text_len: usize,
}

type FnOpenSubtitle = unsafe extern "C" fn(*const CChar, *mut *mut std::ffi::c_void) -> CInt;
type FnSubtitleCount = unsafe extern "C" fn(*mut std::ffi::c_void) -> usize;
type FnSubtitleGetInfo = unsafe extern "C" fn(*mut std::ffi::c_void, usize, *mut SubtitleEntryFFI) -> CInt;
type FnSubtitleGetText = unsafe extern "C" fn(*mut std::ffi::c_void, usize, *mut u8, usize) -> CInt;
type FnSubtitleAt = unsafe extern "C" fn(*mut std::ffi::c_void, f64) -> CInt;
type FnSubtitleSeek = unsafe extern "C" fn(*mut std::ffi::c_void, f64) -> CInt;
type FnCloseSubtitle = unsafe extern "C" fn(*mut std::ffi::c_void);

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
    #[allow(dead_code)] // FFI 符号，实际在 SubtitleReader 中用
    open_subtitle: libloading::Symbol<'static, FnOpenSubtitle>,
    #[allow(dead_code)]
    subtitle_count: libloading::Symbol<'static, FnSubtitleCount>,
    #[allow(dead_code)]
    subtitle_get_info: libloading::Symbol<'static, FnSubtitleGetInfo>,
    #[allow(dead_code)]
    subtitle_get_text: libloading::Symbol<'static, FnSubtitleGetText>,
    #[allow(dead_code)]
    subtitle_at: libloading::Symbol<'static, FnSubtitleAt>,
    #[allow(dead_code)]
    subtitle_seek: libloading::Symbol<'static, FnSubtitleSeek>,
    #[allow(dead_code)]
    close_subtitle: libloading::Symbol<'static, FnCloseSubtitle>,
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
        open_subtitle: sym!(b"vff_open_subtitle\0", FnOpenSubtitle),
        subtitle_count: sym!(b"vff_subtitle_count\0", FnSubtitleCount),
        subtitle_get_info: sym!(b"vff_subtitle_get_info\0", FnSubtitleGetInfo),
        subtitle_get_text: sym!(b"vff_subtitle_get_text\0", FnSubtitleGetText),
        subtitle_at: sym!(b"vff_subtitle_at\0", FnSubtitleAt),
        subtitle_seek: sym!(b"vff_subtitle_seek\0", FnSubtitleSeek),
        close_subtitle: sym!(b"vff_close_subtitle\0", FnCloseSubtitle),
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
    /// 输出顶格容量（源 w×h×4；decode-to-display 输出 ≤ 源尺寸）：`_into` 缓冲预分配用，
    /// 打开时算一次缓存（省掉逐帧 FFI 取参数做容量判断）。
    cap: usize,
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
        // 直接读参数算顶格容量（不经 `VideoReader` 临时值——其 Drop 会 close 句柄，造成 UAF）
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
        unsafe { (b.video_params)(handle, &mut ffi) }
        Ok(Self {
            handle,
            cap: (ffi.width as usize)
                .saturating_mul(ffi.height as usize)
                .saturating_mul(4)
                .max(1),
        })
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

    /// 输出顶格容量（源 w×h×4，decode-to-display 输出 ≤ 源）：解码缓冲**上界**。
    /// 调用方用它在「未知实际输出尺寸」时安全扩容（首帧前）；首帧后按实际 w×h×4 收窄复用。
    pub fn source_cap(&self) -> usize {
        self.cap
    }

    /// 推进到**下一帧**并转成 BGRA。返回 [`DecodedFrame`]；
    /// `None` = 已到流末尾（之后可再 `seek` 复用）。
    ///
    /// 便捷壳：内部每调用分配一次（一次性调用/测试用；播放 worker 走 [`Self::next_frame_bgra_into`]
    /// 复用持久缓冲，避免逐帧 3.69MB 全零初始化热点）。
    #[allow(dead_code)] // 语义/一次性调用入口；播放 worker 走 `_into`（见 [`Self::next_frame_bgra_into`]）
    pub fn next_frame_bgra(&mut self) -> Result<Option<DecodedFrame>> {
        let mut buf = vec![0u8; self.cap];
        match self.next_frame_bgra_into(&mut buf)? {
            Some((pts, w, h)) => {
                let need = (w as usize) * (h as usize) * 4;
                buf.truncate(need);
                Ok(Some((pts, w, h, buf)))
            }
            None => Ok(None),
        }
    }

    /// 推进到**下一帧**并把 BGRA 直接写进调用方提供缓冲 `out`（须 ≥ 输出 w×h×4）。返回
    /// `(pts 秒, 宽, 高)`；`None` = 流末尾（之后可再 `seek` 复用）。
    ///
    /// **消除逐帧全零初始化**：缓冲由调用方常驻复用，容量/尺寸管理交调用方（worker 用
    /// [`Self::source_cap`] 兜底顶格、首帧后按实际输出收窄），本方法不做任何分配/清零；
    /// 与 cdylib `vff_video_frame` 的「调用方预分配输出缓冲」约定对齐。
    pub fn next_frame_bgra_into(&mut self, out: &mut [u8]) -> Result<Option<(f64, u32, u32)>> {
        let b = bridge().context("视频运行库不可用")?;
        let mut pts = 0.0f64;
        let mut w = 0u32;
        let mut h = 0u32;
        let rc = unsafe {
            (b.video_frame)(self.handle, out.as_mut_ptr(), out.len(), &mut pts, &mut w, &mut h)
        };
        match rc {
            1 => Ok(Some((pts, w, h))),
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

// ---------------- 字幕 Reader ----------------

/// 字幕格式枚举。
#[derive(Clone, Copy, Debug, PartialEq)]
#[allow(dead_code)] // 保留供后续 FFI 扩展用
pub enum SubtitleFormat {
    ASS = 0,
    SSA = 1,
    SRT = 2,
    Unknown = 3,
}

impl From<u32> for SubtitleFormat {
    fn from(v: u32) -> Self {
        match v {
            0 => Self::ASS,
            1 => Self::SSA,
            2 => Self::SRT,
            _ => Self::Unknown,
        }
    }
}

/// 字幕顶层参数。
#[derive(Clone, Copy, Debug, PartialEq)]
#[allow(dead_code)] // 保留供后续 FFI 扩展用
pub struct SubtitleParams {
    pub stream_count: u32,
    pub format: SubtitleFormat,
}

/// 一条解码后的字幕条目（decode 侧为直通类型；渲染层
/// [`super::subtitle`] 同款 `pub use crate::decode::SubtitleEntry` 复用它，
/// viewer 直接赋给 `subtitle::SubtitleEntry` 字段，不用双份结构体互转）。
#[derive(Clone, Debug)]
pub struct SubtitleEntry {
    pub start_pts: f64,
    pub end_pts: f64,
    pub text: String,
}

/// 可 seek 的字幕解码器：提取 ASS/SSA/SRT 字幕。
pub struct SubtitleReader {
    handle: Handle,
    subtitles: Vec<SubtitleEntry>,
}

impl Drop for SubtitleReader {
    fn drop(&mut self) {
        if let Some(b) = bridge() {
            unsafe { (b.close_subtitle)(self.handle) }
        }
        self.handle = std::ptr::null_mut();
    }
}

impl SubtitleReader {
    /// 打开媒体并提取字幕流。
    pub fn open(path: &Path) -> Result<Self> {
        let b = bridge().context("视频运行库不可用")?;
        let cpath = utf8_cstr(path)?;
        let mut handle: Handle = std::ptr::null_mut();
        let rc = unsafe { (b.open_subtitle)(cpath.as_ptr(), &mut handle) };
        if rc != 0 || handle.is_null() {
            return Err(anyhow::anyhow!("打开字幕失败：{}", path.display()));
        }
        
        // 读取所有字幕
        let count = unsafe { (b.subtitle_count)(handle) };
        let mut subtitles = Vec::with_capacity(count);
        
        for i in 0..count {
            let mut info = SubtitleEntryFFI {
                start_pts: 0.0,
                end_pts: 0.0,
                text_len: 0,
            };
            if unsafe { (b.subtitle_get_info)(handle, i, &mut info) } != 0 {
                continue;
            }
            
            let mut text_buf = vec![0u8; info.text_len];
            if unsafe { (b.subtitle_get_text)(handle, i, text_buf.as_mut_ptr(), text_buf.len()) } != 0 {
                continue;
            }
            
            let text = String::from_utf8_lossy(&text_buf).to_string();
            subtitles.push(SubtitleEntry {
                start_pts: info.start_pts,
                end_pts: info.end_pts,
                text,
            });
        }
        
        Ok(Self { handle, subtitles })
    }
    
    /// 获取所有字幕条目。
    pub fn subtitles(&self) -> &[SubtitleEntry] {
        &self.subtitles
    }
    
    /// 获取当前时间应显示的字幕。
    #[allow(dead_code)] // 保留供后续扩展用
    pub fn subtitle_at(&self, seconds: f64) -> Option<&SubtitleEntry> {
        self.subtitles.iter().find(|s| seconds >= s.start_pts && seconds <= s.end_pts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::Instant;

    /// 基准文件：`VFF_BENCH_VIDEO` 指定，缺省仓库 `target/视频测试.mp4`（1280x720 h264@30）。
    fn bench_video() -> std::path::PathBuf {
        std::env::var("VFF_BENCH_VIDEO")
            .ok()
            .map(std::path::PathBuf::from)
            .filter(|p| p.is_file())
            .unwrap_or_else(|| {
                std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("../../target/视频测试.mp4")
            })
    }

    /// 字幕 FFI 全链路回归（评审补）：`SubtitleReader::open` 经
    /// `vff_open_subtitle → count → get_info → get_text` 取回 `SubtitleEntry`，
    /// 验证 UTF-8 长度/缓冲往返不截断、首条窗口与文本与 cdylib 侧提取一致。
    /// 缺夹具 SKIP（与 video-ffmpeg 侧同口径，SKIP 是常态不是回归）。
    #[test]
    fn subtitle_ffi_roundtrip_extracts_without_truncation() {
        if !available() {
            eprintln!("SKIP: video_ffmpeg.dll 不可用");
            return;
        }
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/视频测试字幕.mkv");
        if !path.exists() {
            eprintln!("SKIP: missing target/视频测试字幕.mkv（夹具不入库）");
            return;
        }
        let r = SubtitleReader::open(&path).expect("打开字幕失败");
        let subs = r.subtitles();
        assert!(!subs.is_empty(), "应从 MKV 提取到字幕，实得 {} 条", subs.len());
        let first = &subs[0];
        assert!(
            (first.start_pts - 20.05).abs() < 0.05,
            "首条 start_pts 应≈20.05s，实得 {:.4}",
            first.start_pts
        );
        assert!(
            (first.end_pts - 22.03).abs() < 0.1,
            "首条 end_pts 应≈22.03s，实得 {:.4}",
            first.end_pts
        );
        // 文本往返：不截断、不丢尾（`text_len` 与缓冲一致，lossy 无损）
        assert!(
            !first.text.is_empty(),
            "首条文本应非空（纯文本已剥离 ASS 标签），实得 {:?}",
            first.text
        );
        assert!(
            first.text.trim() == first.text,
            "文本不应带尾随空白（往返后应 trim 干净），实得 {:?}",
            first.text
        );
        assert!(
            r.subtitle_at(20.5).is_some(),
            "20.5s 应命中活动字幕（首条 [20.05, 22.03]）"
        );
        assert!(
            r.subtitle_at(22.04).is_none(),
            "22.04s 应不命中（超出首条窗口）"
        );
    }

    /// 走完 `frames` 帧并积累每帧墙钟（EOF 则 seek(0) 复跑凑数，同 video-ffmpeg 姿势）。
    fn collect(r: &mut VideoReader, frames: usize) -> Vec<f64> {
        let mut out: Vec<f64> = Vec::with_capacity(frames);
        let cap = r.source_cap();
        let mut buf = vec![0u8; cap];
        while out.len() < frames {
            let t = Instant::now();
            match r.next_frame_bgra_into(&mut buf) {
                Ok(Some(_)) => out.push(t.elapsed().as_secs_f64() * 1e3),
                Ok(None) => r.seek(0.0).unwrap(),
                Err(e) => panic!("帧解码失败: {e:#}"),
            }
        }
        out
    }

    fn stat(v: &[f64]) -> (f64, f64, f64) {
        let mut s = v.to_vec();
        s.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let mean = v.iter().sum::<f64>() / v.len() as f64;
        (s[0], s[s.len() / 2], mean)
    }

    /// 主侧解码路径逐帧墙钟对照（`--ignored` 手动跑）：量化「每帧 alloc + 全零初始化」常量。
    /// - `旧路径`：worker 逐帧 `next_frame_bgra()`（内部 `vec![0;N]`，3.69MB @720p 全零 memset）；
    /// - `新路径`：常驻缓冲 `next_frame_bgra_into` 复用 + 一次 `to_vec` memcpy（现 worker 姿势）。
    /// EOF→seek(0) 复跑凑数；前 20 帧预热剔除（scaler 惰性初始化）。
    #[test]
    #[ignore = "手动测量用"]
    fn bench_frame_alloc_paths() {
        if !available() {
            eprintln!("SKIP: video_ffmpeg.dll 不可用");
            return;
        }
        let path = bench_video();
        if !path.exists() {
            eprintln!("SKIP: missing target/视频测试.mp4（夹具不入库）");
            return;
        }
        const FRAMES: usize = 240;
        const WARMUP: usize = 20;
        let mut r = VideoReader::open_scaled(&path, 1920).expect("打开失败");
        let p = r.params();
        eprintln!(
            "[BENCH] 文件={} 源={}x{} 输出顶格={} cap={}",
            path.display(),
            p.width,
            p.height,
            r.source_cap() / 4,
            r.source_cap()
        );

        // 旧路径：每帧 `next_frame_bgra`（分配 + 全零初始化 + sws 写入）
        let mut old: Vec<f64> = Vec::with_capacity(FRAMES);
        while old.len() < FRAMES {
            let t = Instant::now();
            match r.next_frame_bgra() {
                Ok(Some(_)) => old.push(t.elapsed().as_secs_f64() * 1e3),
                Ok(None) => r.seek(0.0).unwrap(),
                Err(e) => panic!("旧路径解码失败: {e:#}"),
            }
        }

        // 新路径：常驻缓冲 `_into` 复用 + to_vec（与视频 worker 逐帧姿势一致）
        let new = collect(&mut r, FRAMES);

        let (omin, omed, ome) = stat(&old[WARMUP..]);
        let (nmin, nmed, nme) = stat(&new[WARMUP..]);
        eprintln!(
            "[BENCH] 旧路径 next_frame_bgra（alloc+全零初始化）: min={omin:.3} 中位={omed:.3} 均值={ome:.3} ms/帧"
        );
        eprintln!(
            "[BENCH] 新路径 _into+to_vec（常驻复用）         : min={nmin:.3} 中位={nmed:.3} 均值={nme:.3} ms/帧"
        );
        eprintln!(
            "[BENCH] alloc/zero-init 常量差 ≈ {:.3} ms/帧（新路径省下）",
            ome - nme
        );
    }
}

