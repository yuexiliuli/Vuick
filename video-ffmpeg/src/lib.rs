//! FFmpeg 视频/音频解码内核（cdylib）——供主程序**运行时**经 libloading 加载。
//!
//! # 为什么要拆成独立 cdylib
//! 主 exe 若直接链接 `ffmpeg_next` 会得到 FFmpeg shared DLL 的**加载时静态导入**，缺库时
//! exe 根本无法启动。把解码内核独立成 `video_ffmpeg.dll` 后，主 exe 只在用户真正打开视频
//! 时用 `LoadLibrary` 载入本库；本库及其依赖的 FFmpeg DLL 缺失 → 加载失败 → 主程序在窗口内
//! 提示「缺少视频运行库」，而 exe 本身可正常启动、其它功能不受影响。
//!
//! # FFI 约定
//! - 句柄为透明指针（`*mut c_void`），主程序只持有不解析；每个 open 对应一次 `close_*`。
//! - 视频帧/音频样本由**调用方预分配输出缓冲**：主程序按 `ParamFFI` 尺寸分配后传入，
//!   本库把结果拷入，避免跨 DLL 边界 malloc/free 不匹配。
//! - 路径按 **UTF-8 文件名** 传（`Path::as_encoded_bytes`），FFmpeg Windows 自带 UTF-8→UTF-16
//!   转换，中文路径（fixtures）可用。
//! - 所有解码操作在本库内部串行化（`decode::FFMPEG_LOCK`），与旧版同一纪律。
//!
//! 导出函数均为 `unsafe extern "C"`：跨 DLL 边界的 C ABI 约定由调用方（主程序）遵守，
//! 具体前置条件见 `gpui-editor/src/video/decode.rs` 加载器——此处统一豁免逐条 `# Safety`。
#![allow(clippy::missing_safety_doc)]

mod decode;

use decode::{AudioReader, VideoReader};
use std::ffi::{c_char, c_int, c_void, CStr};

/// 跨 FFI 顶层参数（视频 + 音频并集；视频侧音频字段为 0，反之宽度/高度为 0）。
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

type Handle = *mut c_void;

/// 把 UTF-8 文件名 C 串转成路径。
fn path_from<'a>(p: *const c_char) -> Option<&'a std::path::Path> {
    if p.is_null() {
        return None;
    }
    let s = unsafe { CStr::from_ptr(p) }.to_str().ok()?;
    Some(std::path::Path::new(s))
}

#[inline]
fn reader<'a, T>(h: Handle) -> &'a mut T {
    unsafe { &mut *(h as *mut T) }
}

// ---------------- 视频 ----------------

/// 打开视频：`max_dim` 为 sws 长边封顶（`u32::MAX` 全尺寸）。成功写 `*out` 句柄返回 0。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vff_open_video(
    path: *const c_char,
    max_dim: u32,
    out: *mut Handle,
) -> c_int {
    if out.is_null() {
        return -1;
    }
    let Some(p) = path_from(path) else {
        return -1;
    };
    let r = match VideoReader::open_scaled(p, max_dim) {
        Ok(r) => r,
        Err(_) => return -1,
    };
    unsafe { *out = Box::into_raw(Box::new(r)) as Handle }
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn vff_video_params(h: Handle, out: *mut ParamFFI) {
    let p = reader::<VideoReader>(h).params();
    unsafe {
        *out = ParamFFI {
            width: p.width,
            height: p.height,
            duration: p.duration,
            frame_rate_num: p.frame_rate.map(|r| r.0).unwrap_or(0),
            frame_rate_den: p.frame_rate.map(|r| r.1).unwrap_or(0),
            has_audio: p.has_audio as u8,
            sample_rate: 0,
            channels: 0,
        };
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn vff_video_seek(h: Handle, seconds: f64) -> c_int {
    reader::<VideoReader>(h).seek(seconds).map(|_| 0).unwrap_or(-1)
}

/// 取下一帧 BGRA。返回：1 = 有帧（已拷入 `out_buf`，`*pts/*w/*hh` 已写）；
/// 0 = EOF；-1 = 错误 / 缓冲过小（`out_cap` 须 ≥ w×h×4）。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vff_video_frame(
    h: Handle,
    out_buf: *mut u8,
    out_cap: usize,
    pts: *mut f64,
    w: *mut u32,
    hh: *mut u32,
) -> c_int {
    let r = reader::<VideoReader>(h);
    match r.next_frame_bgra() {
        Ok(Some((p, ww, hhh, data))) => {
            if out_buf.is_null() || data.len() > out_cap {
                return -1;
            }
            unsafe {
                std::ptr::copy_nonoverlapping(data.as_ptr(), out_buf, data.len());
                *pts = p;
                *w = ww;
                *hh = hhh;
            }
            1
        }
        Ok(None) => 0,
        Err(_) => -1,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn vff_close_video(h: Handle) {
    if !h.is_null() {
        drop(unsafe { Box::from_raw(h as *mut VideoReader) });
    }
}

// ---------------- 音频 ----------------

/// 打开音轨并重采样到 `sample_rate`/立体声/F32 交错。成功写 `*out` 句柄返回 0。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vff_open_audio(
    path: *const c_char,
    sample_rate: u32,
    out: *mut Handle,
) -> c_int {
    if out.is_null() {
        return -1;
    }
    let Some(p) = path_from(path) else {
        return -1;
    };
    let r = match AudioReader::open_pcm_f32(p, sample_rate) {
        Ok(r) => r,
        Err(_) => return -1,
    };
    unsafe { *out = Box::into_raw(Box::new(r)) as Handle }
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn vff_audio_params(h: Handle, out: *mut ParamFFI) {
    let p = reader::<AudioReader>(h).params();
    unsafe {
        *out = ParamFFI {
            width: 0,
            height: 0,
            duration: p.duration,
            frame_rate_num: 0,
            frame_rate_den: 0,
            has_audio: 1,
            sample_rate: p.sample_rate,
            channels: p.channels,
        };
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn vff_audio_seek(h: Handle, seconds: f64) -> c_int {
    reader::<AudioReader>(h).seek(seconds).map(|_| 0).unwrap_or(-1)
}

/// 取下一块 F32 交错样本。返回：1 = 有数据（`*pts/*written` 已写，样本拷入 `out_buf`）；
/// 0 = EOF；-1 = 错误 / 缓冲过小。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vff_audio_pcm(
    h: Handle,
    out_buf: *mut f32,
    out_cap: usize,
    pts: *mut f64,
    written: *mut usize,
) -> c_int {
    let r = reader::<AudioReader>(h);
    match r.next_pcm_f32() {
        Ok(Some((p, samples))) => {
            if out_buf.is_null() || samples.len() > out_cap {
                return -1;
            }
            unsafe {
                std::ptr::copy_nonoverlapping(samples.as_ptr(), out_buf, samples.len());
                *pts = p;
                *written = samples.len();
            }
            1
        }
        Ok(None) => 0,
        Err(_) => -1,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn vff_close_audio(h: Handle) {
    if !h.is_null() {
        drop(unsafe { Box::from_raw(h as *mut AudioReader) });
    }
}
