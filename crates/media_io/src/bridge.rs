//! 媒体运行库共享 FFI 桥：**运行时动态加载** `media_ffmpeg.dll`（libloading），供
//! video / music 消费方复用（本批只沉淀桥层与共享类型，不迁任何消费方）。
//!
//! 姿势逐字照抄 `crates/video/src/decode.rs` 的 `Bridge`（find_lib / sym! 宏 /
//! 符号 'static 提升 / OnceLock 缓存 / available / dll_last_error / open_error_with_detail）
//! 与 `crates/music/src/decode.rs` 的音频子集类型（AudioParams / ParamFFI 布局）。
//! **全符号表 = 视频全集**：video 消费全集、music 只取音频子集调用，桥层不特判消费方。
//!
//! FFI 约定见 `media-ffmpeg/src/lib.rs`：句柄透明、输出缓冲由调用方预分配、路径按 UTF-8 传。
//! 许可证同 video：本 crate 与 FFmpeg 是「libloading 动态桥 + 独立共享库」关系（LGPL v2.1+
//! 外部共享库），本 crate Apache-2.0 不受传染。
//!
//! # 线程纪律
//! 句柄只在解码线程内部创建/使用/释放（裸指针非 `Send`）。FFmpeg 并发闸（FFMPEG_LOCK）
//! 在 cdylib 内，跨操作的并发串行由它保证。

use std::path::Path;
use std::sync::OnceLock;

use anyhow::{Context as _, Result};

// ---------------- 共享 FFI 类型 ----------------

/// 音频顶层参数（取自 `vff_audio_params`；music 消费方同款语义）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AudioParams {
    /// 输出采样率（统一重采样到该率）。
    pub sample_rate: u32,
    /// 输出声道数（统一 2，立体声交错）。
    pub channels: u32,
    /// 音轨总时长（秒）。
    pub duration: f64,
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

/// 与 `media-ffmpeg` 的 `ParamFFI` 布局一致（`#[repr(C)]` 布局与 cdylib 侧逐字段对齐）。
/// 视频侧全字段有效；音频侧宽高/帧率为 0、has_audio=1。
#[repr(C)]
pub struct ParamFFI {
    pub width: u32,
    pub height: u32,
    pub duration: f64,
    pub frame_rate_num: u32,
    pub frame_rate_den: u32,
    pub has_audio: u8,
    pub sample_rate: u32,
    pub channels: u32,
}

/// 透明句柄（解码器内部对象；消费方一般只透传指针值）。
pub type Handle = *mut std::ffi::c_void;
/// FFI `char*`。
pub type CChar = std::ffi::c_char;
/// FFI `int`。
pub type CInt = std::ffi::c_int;

// ---------------- 视频符号签名（逐字照抄 video/decode.rs 的 FFI 类型） ----------------

/// 软解打开：路径 UTF-8 + 长边封顶尺寸 + 输出句柄。
pub type FnOpen = unsafe extern "C" fn(*const CChar, u32, *mut Handle) -> CInt;
/// 安全硬解（FFmpeg 自有设备，无注入）：打开/取 IS/HW/seek 预滚免转换跳帧。
pub type FnOpenHwPrefer = unsafe extern "C" fn(*const CChar, u32, *mut Handle) -> CInt;
pub type FnVideoIsHw = unsafe extern "C" fn(Handle) -> CInt;
pub type FnVideoSetSkipNonkey = unsafe extern "C" fn(Handle, CInt);
pub type FnSkipToBgra = unsafe extern "C" fn(
    Handle,
    f64,
    *mut u8,
    usize,
    *mut f64,
    *mut u32,
    *mut u32,
    *mut u64,
) -> CInt;
pub type FnOpenAudio = unsafe extern "C" fn(*const CChar, u32, *mut Handle) -> CInt;
/// 参数读取（`vff_video_params` / `vff_audio_params`）：C 侧为 `void`（输出经预分配的 `out`
/// 回传），非 `-> CInt`。显式写成 `-> ()` 防误加返回值造成 ABI 违例（Win64 忘读 RAX 类 UB）；
/// 消费方均忽略返回值、直接读 `ParamFFI` 字段。
pub type FnParams = unsafe extern "C" fn(Handle, *mut ParamFFI) -> ();
pub type FnSeek = unsafe extern "C" fn(Handle, f64) -> CInt;
pub type FnFrame = unsafe extern "C" fn(Handle, *mut u8, usize, *mut f64, *mut u32, *mut u32) -> CInt;
pub type FnPcm = unsafe extern "C" fn(Handle, *mut f32, usize, *mut f64, *mut usize) -> CInt;
pub type FnClose = unsafe extern "C" fn(Handle);
/// 双设备 GPU 直通（零回读）：外部 D3D11 设备注入解码——`device`/`device_context` 为
/// worker 自有 `ID3D11Device*`/`ID3D11DeviceContext*` 的 usize 裸指针。
pub type FnOpenVideoHw = unsafe extern "C" fn(*const CChar, u32, usize, usize, *mut Handle) -> CInt;
pub type FnVideoFrameHw = unsafe extern "C" fn(
    Handle,
    *mut f64,
    *mut u32,
    *mut u32,
    *mut usize,
    *mut u32,
) -> *mut std::ffi::c_void;
pub type FnVideoFrameRelease = unsafe extern "C" fn(*mut std::ffi::c_void);
/// 能力诊断：驱动是否支持「D3D11VA 解码数组纹理带 SHADER_RESOURCE」。纯探测、无解码器
/// 副作用，仅供能力自诊断/测试（双设备直通不再以它为门）。
pub type FnSupportsShaderBoundTextures = unsafe extern "C" fn(usize, usize) -> CInt;

// ---------------- 字幕 FFI 类型与符号签名 ----------------

/// 字幕顶层参数 FFI（布局定义；实际用独立函数取字幕）。
/// dormancy 休眠保留（K24）：全仓无消费方——实际取值在 `media_ffmpeg` 内自带同名
/// `SubtitleParamsFFI` 逐项读取，本布局声明无人引用。
#[repr(C)]
#[allow(dead_code)] // dormancy 休眠保留（K24）
pub struct SubtitleParamsFFI {
    pub stream_count: u32,
    pub format: u32,
}

/// 单条字幕条目 FFI（布局定义；文本另经 `vff_subtitle_get_text` 取）。
#[repr(C)]
pub struct SubtitleEntryFFI {
    pub start_pts: f64,
    pub end_pts: f64,
    pub text_len: usize,
}

pub type FnOpenSubtitle = unsafe extern "C" fn(*const CChar, *mut *mut std::ffi::c_void) -> CInt;
pub type FnSubtitleCount = unsafe extern "C" fn(*mut std::ffi::c_void) -> usize;
pub type FnSubtitleGetInfo =
    unsafe extern "C" fn(*mut std::ffi::c_void, usize, *mut SubtitleEntryFFI) -> CInt;
pub type FnSubtitleGetText =
    unsafe extern "C" fn(*mut std::ffi::c_void, usize, *mut u8, usize) -> CInt;
pub type FnCloseSubtitle = unsafe extern "C" fn(*mut std::ffi::c_void);

/// R2-4：打开失败分类（dll 新增 additive 导出；老 dll 缺符号 → 桥可选解析、回退通用文案）。
pub type FnLastError = unsafe extern "C" fn() -> *const CChar;

/// 已加载的动态函数集（持有 `Library` 保句柄存活；符号指针在进程生命周期内有效）。
/// **全符号表 = 视频全集**（含音频 / 硬件 / 字幕 / d3d11 诊断集合）；字段全部 `pub`
/// 供 video / music 消费方直接取符号。
pub struct Bridge {
    pub open_video: libloading::Symbol<'static, FnOpen>,
    pub open_video_hw_prefer: libloading::Symbol<'static, FnOpenHwPrefer>,
    pub video_is_hw: libloading::Symbol<'static, FnVideoIsHw>,
    pub video_set_skip_nonkey: libloading::Symbol<'static, FnVideoSetSkipNonkey>,
    pub video_skip_to_bgra: libloading::Symbol<'static, FnSkipToBgra>,
    pub open_audio: libloading::Symbol<'static, FnOpenAudio>,
    /// 音频顶层参数读取（`vff_audio_params`；music 消费方音频子集依赖）。视频侧亦持该
    /// 类型但只用 video_params——全符号表同时覆盖两宿主的音频/视频参数入口。
    pub audio_params: libloading::Symbol<'static, FnParams>,
    pub video_params: libloading::Symbol<'static, FnParams>,
    pub video_seek: libloading::Symbol<'static, FnSeek>,
    pub audio_seek: libloading::Symbol<'static, FnSeek>,
    pub video_frame: libloading::Symbol<'static, FnFrame>,
    pub audio_pcm: libloading::Symbol<'static, FnPcm>,
    pub close_video: libloading::Symbol<'static, FnClose>,
    pub close_audio: libloading::Symbol<'static, FnClose>,
    // 双设备 GPU 直通：外部设备注入符号（kernel 已导出；见签名注释）
    pub open_video_hw: libloading::Symbol<'static, FnOpenVideoHw>,
    pub video_frame_hw: libloading::Symbol<'static, FnVideoFrameHw>,
    pub video_frame_release: libloading::Symbol<'static, FnVideoFrameRelease>,
    /// 能力诊断 FFI（仅供测试 `dual_gate_capability` 用，非直通门控）。
    pub d3d11_supports_shader_bound_textures: libloading::Symbol<'static, FnSupportsShaderBoundTextures>,
    pub open_subtitle: libloading::Symbol<'static, FnOpenSubtitle>,
    pub subtitle_count: libloading::Symbol<'static, FnSubtitleCount>,
    pub subtitle_get_info: libloading::Symbol<'static, FnSubtitleGetInfo>,
    pub subtitle_get_text: libloading::Symbol<'static, FnSubtitleGetText>,
    pub close_subtitle: libloading::Symbol<'static, FnCloseSubtitle>,
    /// R2-4（可选）：打开失败分类串读取口。老 dll 无 `vff_last_error_message` → `None`，
    /// 打开失败回落通用文案（ABI 向后兼容）；新 dll → 打开失败时把分类带进错误提示。
    pub last_error: Option<libloading::Symbol<'static, FnLastError>>,
    /// 保底：Library 被进程级持有，符号才始终有效。
    pub _lib: libloading::Library,
}

/// find_lib 的路径候选取出（**可纯测**）：`[exe_dir/name, exe_dir/../name]`。
fn candidate_paths(exe_dir: &Path) -> [std::path::PathBuf; 2] {
    let name = "media_ffmpeg.dll";
    [exe_dir.join(name), exe_dir.join("..").join(name)]
}

fn find_lib() -> std::path::PathBuf {
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        for c in candidate_paths(dir) {
            if c.is_file() {
                return c;
            }
        }
    }
    // 兜底：交给 LoadLibrary 的 exe 目录 / CWD / PATH / 系统目录搜索
    std::path::PathBuf::from("media_ffmpeg.dll")
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
        video_set_skip_nonkey: sym!(b"vff_video_set_skip_nonkey\0", FnVideoSetSkipNonkey),
        video_skip_to_bgra: sym!(b"vff_video_skip_to_bgra\0", FnSkipToBgra),
        open_audio: sym!(b"vff_open_audio\0", FnOpenAudio),
        audio_params: sym!(b"vff_audio_params\0", FnParams),
        video_params: sym!(b"vff_video_params\0", FnParams),
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
        close_subtitle: sym!(b"vff_close_subtitle\0", FnCloseSubtitle),
        // R2-4 additive：新 dll 才导出 `vff_last_error_message`——**可选解析**（.ok()），
        // 老 dll 缺符号不使整桥加载失败，打开失败回落通用文案。
        last_error: {
            let s = unsafe { lib.get::<FnLastError>(b"vff_last_error_message\0") }.ok();
            s.map(|sym| unsafe {
                std::mem::transmute::<libloading::Symbol<'_, FnLastError>, libloading::Symbol<'static, FnLastError>>(sym)
            })
        },
        _lib: lib,
    })
}

/// 取进程级桥（加载失败缓存为 `None`，进程内只尝试一次）。**全符号表必载**：
/// video 消费全集、music 只取音频子集，桥层不特判消费方。
pub fn bridge() -> Option<&'static Bridge> {
    static B: OnceLock<Option<Bridge>> = OnceLock::new();
    B.get_or_init(load_bridge).as_ref()
}

/// 运行库是否可用（`media_ffmpeg.dll` 与其 FFmpeg 依赖能否被加载，进程内缓存一次）。
/// 缺库 → false → 查看器在窗口内报错而非害 exe 无法启动。
pub fn available() -> bool {
    bridge().is_some()
}

/// 同 [`available`] 的别名（music 侧沿用 `bridge_ok` 命名；语义逐字一致）。
pub fn bridge_ok() -> bool {
    bridge().is_some()
}

/// R2-4：取 dll 侧最近一次打开失败分类串（`vff_last_error_message` 可选符号；桥里有且
/// 是在**同一线程**先失败后立即读取——视频/音频 worker 在 open 失败处同线程调用）。
/// 老 dll / 桥不可用 → `None`（上层回落通用「打开媒体失败」文案）。
pub fn dll_last_error() -> Option<String> {
    let b = bridge()?;
    let sym = b.last_error.as_ref()?;
    let p = unsafe { (sym)() };
    if p.is_null() {
        return None;
    }
    let cstr = unsafe { std::ffi::CStr::from_ptr(p) };
    let s = cstr.to_string_lossy();
    if s.trim().is_empty() {
        return None;
    }
    Some(s.trim_end().to_string())
}

/// R2-4：打开失败错误构造——dll 侧有分类串则附到主文案后（`（原因: …）`），无（老 dll /
/// 桥不可用 / 串为空）则原样返回通用文案。分类串取首行并截断，避免长 anyhow 链刷屏。
/// 供 video / music 消费方打开失败处统一复用。
pub fn open_error_with_detail(base: String) -> anyhow::Error {
    let Some(detail) = dll_last_error() else {
        return anyhow::anyhow!("{base}");
    };
    let first_line = detail.split('\n').next().unwrap_or("").trim();
    let capped: String = first_line.chars().take(200).collect();
    anyhow::anyhow!("{base}（{}）", capped)
}

/// 把路径转 CString 传给 dll（UTF-8 字节直传；含 NUL 报错链）。video / music 消费方共用。
pub fn utf8_cstr(path: &Path) -> Result<std::ffi::CString> {
    let bytes = path.as_os_str().as_encoded_bytes();
    std::ffi::CString::new(bytes).context("路径含 NUL")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// find_lib 路径候选取出（纯函数，不触及文件系统加载）：`[exe_dir/name, exe_dir/../name]`。
    #[test]
    fn candidate_paths_yields_exe_dir_and_parent() {
        let dir = Path::new("C:/some/app/bin");
        let cands = candidate_paths(dir);
        assert_eq!(cands[0], dir.join("media_ffmpeg.dll"));
        assert_eq!(cands[1], dir.join("..").join("media_ffmpeg.dll"));
    }

    /// ParamFFI → AudioParams 换算（纯类型转换，语义同 music `params()`）。
    #[test]
    fn audio_params_from_ffi() {
        let ffi = ParamFFI {
            width: 0,
            height: 0,
            duration: 12.5,
            frame_rate_num: 0,
            frame_rate_den: 0,
            has_audio: 1,
            sample_rate: 48_000,
            channels: 2,
        };
        let ap: AudioParams = ffi.into();
        assert_eq!(ap.sample_rate, 48_000);
        assert_eq!(ap.channels, 2);
        assert_eq!(ap.duration, 12.5);
    }

    /// ABI 签名一致性守卫：`FnParams` 必须与 dll `vff_video_params` / `vff_audio_params` 的
    /// C `void` 签名一致（无返回值）。若有人误把声明改回 `-> CInt`，此处 void mock 函数将无法
    /// 赋给 `FnParams`，编译即失败——把 headless 无法触发的 ABI/UB 违例前移到编译期。
    #[test]
    fn params_ffi_type_matches_void_c_signature() {
        // 与 C `void` 对齐的 mock 签名（输出经 `out` 裸指针回传，无返回值）
        unsafe extern "C" fn void_like(_h: Handle, _out: *mut ParamFFI) {}
        // 赋值约束 FnParams 恰为 void 返回；改回 `-> CInt` 则此处类型不匹配、编译失败
        let f: FnParams = void_like;
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
        // 调用结果为 `()`（void），可作 `let _: ()` 绑定；`-> CInt` 时本行亦类型不匹配
        let _: () = unsafe { f(std::ptr::null_mut(), &mut ffi) };
    }
}
