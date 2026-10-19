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

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

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
// 安全硬解（FFmpeg 自有设备，无注入）：打开/取 IS/HW/seek 预滚免转换跳帧
type FnOpenHwPrefer = unsafe extern "C" fn(*const CChar, u32, *mut Handle) -> CInt;
type FnVideoIsHw = unsafe extern "C" fn(Handle) -> CInt;
type FnSkipToBgra = unsafe extern "C" fn(
    Handle,
    f64,
    *mut u8,
    usize,
    *mut f64,
    *mut u32,
    *mut u32,
    *mut u64,
) -> CInt;
type FnOpenAudio = unsafe extern "C" fn(*const CChar, u32, *mut Handle) -> CInt;
type FnParams = unsafe extern "C" fn(Handle, *mut ParamFFI);
type FnSeek = unsafe extern "C" fn(Handle, f64) -> CInt;
type FnFrame = unsafe extern "C" fn(Handle, *mut u8, usize, *mut f64, *mut u32, *mut u32) -> CInt;
type FnPcm = unsafe extern "C" fn(Handle, *mut f32, usize, *mut f64, *mut usize) -> CInt;
type FnClose = unsafe extern "C" fn(Handle);
// 双设备 GPU 直通（零回读）：外部 D3D11 设备注入解码——`device`/`device_context` 为
// worker 自有 `ID3D11Device*`/`ID3D11DeviceContext*` 的 usize 裸指针（见 [`VideoReader::open_hw`]）。
type FnOpenVideoHw = unsafe extern "C" fn(*const CChar, u32, usize, usize, *mut Handle) -> CInt;
type FnVideoFrameHw = unsafe extern "C" fn(
    Handle,
    *mut f64,
    *mut u32,
    *mut u32,
    *mut usize,
    *mut u32,
) -> *mut std::ffi::c_void;
type FnVideoFrameRelease = unsafe extern "C" fn(*mut std::ffi::c_void);
// 能力诊断：驱动是否支持「D3D11VA 解码数组纹理带 SHADER_RESOURCE」——进/出参姿势同
// [`FnOpenVideoHw`]（worker 自有设备/上下文的 usize 裸指针）；1 = 支持、0 = 不支持/空指针。
// 纯探测、无解码器副作用。**仅供能力自诊断/测试**——双设备直通不再以它为门（解码池恒
// DECODER-only，渲染端只采样共享槽）。
type FnSupportsShaderBoundTextures = unsafe extern "C" fn(usize, usize) -> CInt;

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
    open_video_hw_prefer: libloading::Symbol<'static, FnOpenHwPrefer>,
    video_is_hw: libloading::Symbol<'static, FnVideoIsHw>,
    video_skip_to_bgra: libloading::Symbol<'static, FnSkipToBgra>,
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
    // 双设备 GPU 直通：外部设备注入符号（kernel 已导出；见 FFI 类型注释）
    open_video_hw: libloading::Symbol<'static, FnOpenVideoHw>,
    video_frame_hw: libloading::Symbol<'static, FnVideoFrameHw>,
    video_frame_release: libloading::Symbol<'static, FnVideoFrameRelease>,
    #[allow(dead_code)] // 能力诊断 FFI（仅供测试 `dual_gate_capability` 用，非直通门控）
    d3d11_supports_shader_bound_textures: libloading::Symbol<'static, FnSupportsShaderBoundTextures>,
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
        open_video_hw_prefer: sym!(b"vff_open_video_hw_prefer\0", FnOpenHwPrefer),
        video_is_hw: sym!(b"vff_video_is_hw\0", FnVideoIsHw),
        video_skip_to_bgra: sym!(b"vff_video_skip_to_bgra\0", FnSkipToBgra),
        open_audio: sym!(b"vff_open_audio\0", FnOpenAudio),
        video_params: sym!(b"vff_video_params\0", FnParams),
        audio_params: sym!(b"vff_audio_params\0", FnParams),
        video_seek: sym!(b"vff_video_seek\0", FnSeek),
        audio_seek: sym!(b"vff_audio_seek\0", FnSeek),
        video_frame: sym!(b"vff_video_frame\0", FnFrame),
        audio_pcm: sym!(b"vff_audio_pcm\0", FnPcm),
        close_video: sym!(b"vff_close_video\0", FnClose),
        close_audio: sym!(b"vff_close_audio\0", FnClose),
        open_video_hw: sym!(b"vff_open_video_hw\0", FnOpenVideoHw),
        video_frame_hw: sym!(b"vff_video_frame_hw\0", FnVideoFrameHw),
        video_frame_release: sym!(b"vff_video_frame_release\0", FnVideoFrameRelease),
        d3d11_supports_shader_bound_textures: sym!(
            b"vff_d3d11_supports_shader_bound_textures\0",
            FnSupportsShaderBoundTextures
        ),
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

// ---------------- 解码偏好旋钮（测试隔离用） ----------------

/// 进程级解码偏好（生产默认 `0` = auto）：
/// - `0` auto：`open_scaled_hw`（硬件优先、FFmpeg 自有设备、不支持/失败自动回软解）；
/// - `1` 强制软解：`open_scaled`（软解锚点基准与用户显式软解用——软解时序恒定可对表）；
/// - `2` 强制硬件优先：`open_scaled_hw`（硬件 seek 基准用；内核仍自行软回退，仅避免再读
///   调用方设备/上下文——旧 M2e 设备注入已废除）。
static VIDEO_DECODE_MODE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// 当前解码偏好（video worker 打开 reader 时读取；经 [`set_decode_mode`] 设置，生产随控制条
/// 按钮在 `0` auto / `1` 强制软解 间切换，默认 `0`）。
pub(crate) fn decode_mode() -> u8 {
    VIDEO_DECODE_MODE.load(std::sync::atomic::Ordering::Relaxed)
}

/// 设置进程级解码偏好（取值见 [`VIDEO_DECODE_MODE`]）：对后续（重）打开的 reader 生效。
/// 视频 worker 打开前读 [`decode_mode`] 选 Auto/Soft——模式是 worker 常驻 reader 的复用键
/// 一部分（`path + cap + 模式`），模式一变即不复用、必然重开（见 viewer）。生产经控制条
/// 「解码偏好」按钮调用；测试亦经此入口（见图 [`set_decode_mode_for_test`]）。
pub(crate) fn set_decode_mode(m: u8) {
    VIDEO_DECODE_MODE.store(m, std::sync::atomic::Ordering::Relaxed);
}

/// 测试专用：覆盖解码偏好（等价 [`set_decode_mode`]，命名保留测试显式表达）。配合
/// [`DecodeModeGuard`] 在测试结束时复位——与既有设备注入测试的 `DeviceReset` 守卫同款姿势
/// （panic 中断也复位）。
#[cfg(test)]
pub fn set_decode_mode_for_test(m: u8) {
    set_decode_mode(m);
}

/// 测试专用 RAII 守卫：构造置偏好、Drop 恢复默认 auto（`0`），任何返回/panic 路径都不
/// 把硬解/软解偏好泄漏给后续测试。
#[cfg(test)]
pub struct DecodeModeGuard;

#[cfg(test)]
impl DecodeModeGuard {
    /// 置解码偏好并在 Drop 时复位 `0`。
    pub fn new(m: u8) -> Self {
        set_decode_mode_for_test(m);
        Self
    }
}

#[cfg(test)]
impl std::ops::Drop for DecodeModeGuard {
    fn drop(&mut self) {
        set_decode_mode_for_test(0);
    }
}

// ---------------- 硬解设备丢失黑名单（进程级，worker 自恢复用） ----------------

/// 已被判定「硬解设备丢失」的路径集合（进程内运行时状态）：这些路径后续
/// [`VideoReader::open_scaled_hw`] 一律直接回软解，避免在已失效的 D3D11VA/DXVA2 设备上
/// 反复重试 hw init / 解码（TDR / driver reset / device-removed 后 D3D11 设备不可再信）；
/// 其它路径不受影响、仍正常尝试硬解。生产者是视频 worker（单线程写），消费者是 worker
/// 自身与测试线程（读），偶尔也有 hover worker 并发读——低频访问，用一个小互斥锁保护
///（集合元素级即可，不锁解码路径）。
static HW_BLACKLIST: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();

fn hw_blacklist() -> &'static std::sync::Mutex<HashSet<PathBuf>> {
    HW_BLACKLIST.get_or_init(|| std::sync::Mutex::new(HashSet::new()))
}

/// 将 `path` 加入硬解黑名单（视频 worker 判定设备丢失后调用；进程内生效，仅内存态）。
pub(crate) fn blacklist_hw_path(path: &Path) {
    hw_blacklist()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(path.to_path_buf());
}

/// `path` 是否已被判「该文件硬解设备丢失」（打开前短路到软解）。
pub(crate) fn hw_path_blacklisted(path: &Path) -> bool {
    hw_blacklist()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(path)
}

/// 测试专用：清空硬解黑名单（测试卫生——黑名单进程级持久，防泄漏污染后续测试）。
#[cfg(test)]
pub fn hw_blacklist_clear_for_test() {
    hw_blacklist()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
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

    /// 硬件优先打开（FFmpeg 自有 D3D11VA/DXVA2 设备，无调用方注入、无共享 immediate context
    /// ——worker 线程独享，线程安全；设备/解码器不支持自动回软解；`VFF_VIDEO_SW=1` 强制软解）。
    /// 与 [`Self::open_scaled`] 同 semantics（`max_dim` sws 封顶、params() 源尺寸）；seek 预滚
    /// 用 [`Self::skip_to_bgra_into`] 跳帧免转换，是 seek 墙钟的主要加速。
    pub fn open_scaled_hw(path: &Path, max_dim: u32) -> Result<Self> {
        // 设备丢失黑名单路径：直接回软解（不重试已失效的 D3D11VA/DXVA2 设备；`max_dim`
        // 封顶语义与软解 open 一致，params() 仍报源尺寸——与内核自动回软退同口径）。
        if hw_path_blacklisted(path) {
            return Self::open_scaled(path, max_dim);
        }
        let b = bridge().context("视频运行库不可用（缺 video_ffmpeg.dll 或 FFmpeg 运行库）")?;
        let cpath = utf8_cstr(path)?;
        let mut handle: Handle = std::ptr::null_mut();
        let rc = unsafe { (b.open_video_hw_prefer)(cpath.as_ptr(), max_dim, &mut handle) };
        if rc != 0 || handle.is_null() {
            return Err(anyhow::anyhow!("打开媒体失败: {}", path.display()));
        }
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

    /// 是否硬件解码（D3D11VA/DXVA2，FFmpeg 自有设备）：worker 据此决定预滚走
    /// [`Self::skip_to_bgra_into`]（软解保持旧逐帧转换口径，硬解跳帧免转换）。
    pub fn is_hw(&self) -> bool {
        let b = bridge().expect("reader 持有期桥必在");
        unsafe { (b.video_is_hw)(self.handle) != 0 }
    }

    /// **seek 预滚**：`seek(target)` 后逐帧解码，落点前帧只解码不转换，第一个 `pts ≥ target`
    /// 的帧一次性转 BGRA 写进 `out`（须 ≥ 输出 w×h×4）。返回 `(pts, 宽, 高, 已跳帧数)`；
    /// `None` = EOF 仍未到 target。软解 reader 也适用（语义不变、耗时可略降：省每帧 sws）。
    /// 与硬件路径共用同一个 FFI（cdylib 内按 reader 实际 HW/软状态执行）。
    pub fn skip_to_bgra_into(
        &mut self,
        target: f64,
        out: &mut [u8],
    ) -> Result<Option<(f64, u32, u32, u64)>> {
        let b = bridge().context("视频运行库不可用")?;
        let mut pts = 0.0f64;
        let mut w = 0u32;
        let mut h = 0u32;
        let mut skipped = 0u64;
        let rc = unsafe {
            (b.video_skip_to_bgra)(
                self.handle,
                target,
                out.as_mut_ptr(),
                out.len(),
                &mut pts,
                &mut w,
                &mut h,
                &mut skipped,
            )
        };
        match rc {
            1 => Ok(Some((pts, w, h, skipped))),
            0 => Ok(None),
            _ => Err(anyhow::anyhow!("seek 预滚解码失败")),
        }
    }

    /// 已解析的顶层参数。
    pub fn params(&self) -> VideoParams {        let b = bridge().expect("reader 持有期桥必在");
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

    // ---------------- 双设备 GPU 直通（零回读）：外部 D3D11 设备注入 ----------------

    /// 诊断用能力探测：驱动是否支持「D3D11VA 解码数组纹理带 SHADER_RESOURCE」。**NOT 直通
    /// 门控**——双设备直通恒用 DECODER-only 解码池（渲染端只采样共享 NV12 槽的平面 SRV），
    /// 本探测仅供能力自诊断/测试（返回 false 不会停用直通）。
    /// `device`/`device_context` 为 worker 自有 D3D11 设备/上下文的 usize 裸指针（与
    /// [`Self::open_hw`] 传参同款）。桥不可用或设备指针为空 → 优雅 `false`（不 panic）。
    #[allow(dead_code)] // 能力诊断：仅供测试（`viewer::tests::dual_gate_capability`）与能力自诊断
    pub fn supports_shader_bound_textures(device: usize, device_context: usize) -> bool {
        let Some(b) = bridge() else {
            return false;
        };
        if device == 0 {
            return false;
        }
        unsafe { (b.d3d11_supports_shader_bound_textures)(device, device_context) != 0 }
    }

    /// 打开**外部设备硬件解码**（不可用即 `Err`，上层据此回退 SAFE BGRA 路径）：`device`/
    /// `device_context` 为调用方 **worker 自有** `ID3D11Device*`/`ID3D11DeviceContext*` 的
    /// usize 裸指针（`crate::hw::create_worker_d3d11_device` 创建，与渲染线程设备**不同**——
    /// 双设备架构，帧经 shared handle + keyed mutex 移动，不做共享上下文）。解码器按源全
    /// 尺寸硬解、**不做** sws——帧尺寸即源尺寸（[`HwFrame`] 自带宽高）。`cap` 参数无意义
    /// （仅接口对齐软解 `open_scaled`）；返回 reader 的 `source_cap()` 恒 1（HW 帧不走回读缓冲）。
    pub fn open_hw(path: &Path, device: usize, device_context: usize) -> Result<Self> {
        let b = bridge().context("视频运行库不可用（缺 video_ffmpeg.dll 或 FFmpeg 运行库）")?;
        let cpath = utf8_cstr(path)?;
        let mut handle: Handle = std::ptr::null_mut();
        let rc = unsafe {
            (b.open_video_hw)(cpath.as_ptr(), u32::MAX, device, device_context, &mut handle)
        };
        if rc != 0 || handle.is_null() {
            return Err(anyhow::anyhow!("硬件解码打开失败: {}", path.display()));
        }
        // 尺寸/时长经 `vff_video_params` 读取（与软解同姿势；HW 帧不经 sws，cap 不预分配）
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
        Ok(Self { handle, cap: 1 })
    }

    /// 把 HW 帧的释放句柄归还解码器（**必须**在 GPU 侧拷贝完成后调用）：释放 AVBufferRef
    /// 引用，帧池槽位回归解码器复用——**不释放持续解码会耗尽帧池**（headroom ≈1-2 帧）。
    /// 对 null / EOF(`0x1`) / 错误(`usize::MAX`) 哨兵是安全 no-op（cdylib 内已判空）。
    pub fn release_hw_frame(frame_ptr: *mut std::ffi::c_void) {
        if let Some(b) = bridge() {
            unsafe { (b.video_frame_release)(frame_ptr) }
        }
    }

    /// 推进到**下一帧**，返回外部设备 D3D11VA 硬件帧（零回读：不做 `av_hwframe_transfer_data`、
    /// 不做 sws）。`Ok(None)` = 流末尾（之后可再 [`Self::seek`] 复用）；帧的 `frame_ptr` 用后
    /// **必须**经 [`Self::release_hw_frame`] 释放。
    pub fn next_frame_hw(&mut self) -> Result<Option<HwFrame>> {
        let b = bridge().context("视频运行库不可用")?;
        let mut pts = 0.0f64;
        let mut w = 0u32;
        let mut h = 0u32;
        let mut tex = 0usize;
        let mut idx = 0u32;
        let fp = unsafe {
            (b.video_frame_hw)(self.handle, &mut pts, &mut w, &mut h, &mut tex, &mut idx)
        };
        let p = fp as usize;
        if fp.is_null() {
            Err(anyhow::anyhow!("硬件解码返回空句柄"))
        } else if p == 0x1 {
            Ok(None) // EOF 哨兵
        } else if p == usize::MAX {
            Err(anyhow::anyhow!("硬件解码帧失败"))
        } else {
            Ok(Some(HwFrame {
                frame_ptr: fp,
                pts,
                w,
                h,
                tex,
                array_index: idx,
            }))
        }
    }
}

/// 一帧外部设备 D3D11VA 硬件解码结果（零回读，透明句柄）：`tex` = 帧池 `ID3D11Texture2D*`
/// 数组纹理基址（转 usize）、`array_index` ∈ [0, 帧池容量)。`frame_ptr` 为释放句柄，用后
/// 必须经 [`VideoReader::release_hw_frame`] 归还帧池槽位（headroom ≈1-2 帧，持久不释放会耗尽）。
#[derive(Debug, Clone, Copy)]
pub struct HwFrame {
    /// 释放句柄（`vff_video_frame_hw` 的返回值）。
    pub frame_ptr: *mut std::ffi::c_void,
    /// 帧 pts（秒）。
    pub pts: f64,
    /// 显示宽。
    pub w: u32,
    /// 显示高。
    pub h: u32,
    /// `ID3D11Texture2D*`（转 usize；数组纹理基址）。
    pub tex: usize,
    /// 数组纹理元素下标（该帧在帧池中的槽位）。
    pub array_index: u32,
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
}

/// 仅测试构建存在：当前时间应显示的字幕（生产显示走 `crate::subtitle::find_active_subtitle`；
/// 本测试读口**直接委托它**——单一真值、重叠窗口"最近开始"口径不漂移；FFI
/// `vff_subtitle_at` 在 video-ffmpeg 侧同款语义，见其 `active_subtitle_index`）。
#[cfg(test)]
impl SubtitleReader {
    pub fn subtitle_at(&self, seconds: f64) -> Option<&SubtitleEntry> {
        crate::subtitle::find_active_subtitle(&self.subtitles, seconds)
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

