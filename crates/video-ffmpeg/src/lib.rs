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

#[cfg(test)]
mod hw_external_tests;

use decode::{AudioReader, SubtitleReader, VideoReader};
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

/// 打开视频（**硬件优先**：FFmpeg 自有 D3D11VA/DXVA2 设备，无调用方设备注入、无共享
/// immediate context——worker 线程独享上下文，线程安全；设备/解码器不支持自动回软解）。
/// `max_dim` 为 sws 长边封顶。成功写 `*out` 句柄返回 0；-1 = 打开失败。
/// seek 预滚用 [`vff_video_skip_to_bgra`]（跳帧免转换，seek 墙钟主要加速）；逐帧播放仍走
/// [`vff_video_frame`]（硬件路径由内核自行回读+sws，输出 BGRA 与软解逐字一致）。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vff_open_video_hw_prefer(
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
    let r = match VideoReader::open_scaled_hw(p, max_dim) {
        Ok(r) => r,
        Err(_) => return -1,
    };
    unsafe { *out = Box::into_raw(Box::new(r)) as Handle }
    0
}

/// 当前 reader 是否为硬件解码（D3D11VA/DXVA2）。1 = HW；0 = 软解。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vff_video_is_hw(h: Handle) -> c_int {
    reader::<VideoReader>(h).is_hw() as c_int
}

/// 拖拽快扫预览模式：1 = `skip_frame=NONKEY`（解码器只解关键帧，Video Decode 引擎每轮
/// seek 从「关键帧→落点整段 P/B"」降到「1 个 I 帧」）；0 = 全帧精确（seek/播放）。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vff_video_set_skip_nonkey(h: Handle, nonkey: c_int) {
    reader::<VideoReader>(h).set_skip_nonkey(nonkey != 0)
}

/// 诊断/测试用能力探测：驱动是否支持「D3D11VA 解码数组纹理带 SHADER_RESOURCE」。1 = 支持；
/// 0 = 不支持（Intel Arc 等驱动实测 `CreateTexture2D` E_INVALIDARG）。
/// **NOT 直通门控**：双设备直通恒用 DECODER-only 解码池（渲染端只采样共享 NV12 槽），本
/// 探测仅供能力自诊断与一致性测试，返回 0 不会、也不应停用直通。纯 device/frames 上下文
/// 探测、**不开解码器**，无副作用；`device` 为空 → 0。与内部同名函数同源（同一函数）。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vff_d3d11_supports_shader_bound_textures(
    device: *mut c_void,
    device_context: *mut c_void,
) -> c_int {
    // 本函数是**独立成组**的 FFmpeg 入口（门控在 worker 开媒体时调，可能与 hover/音频
    // worker 的解码并发跑）：所有 FFI 调用必须过 FFMPEG_LOCK 串行（decode.rs 注释同款
    // 纪律）——否则并发的 av_hwdevice/frame ctx init 与解码互卡（72.MP4 实测卡死同源）。
    let _g = decode::FFMPEG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    decode::d3d11_supports_shader_bound_textures(device, device_context) as c_int
}

/// **seek 预滚**：`seek(target)` 后逐帧解码，落点前帧**只解码不转换**（硬件 ≈0.5ms/帧、
/// 软解 ≈2.9ms/帧），第一个 `pts ≥ target` 的帧一次性转 BGRA 拷进 `out_buf`，并写出
/// `*pts/*w/*hh/*skipped`（已跳帧数，含 demux 包级已越过的前帧计数口径同旧预滚）。
/// 返回：1 = 命中（帧已拷入）；0 = EOF（末端仍无 ≥target 帧）；-1 = 错误/缓冲过小。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vff_video_skip_to_bgra(
    h: Handle,
    target: f64,
    out_buf: *mut u8,
    out_cap: usize,
    pts: *mut f64,
    w: *mut u32,
    hh: *mut u32,
    skipped: *mut u64,
) -> c_int {
    if out_buf.is_null() {
        return -1;
    }
    let r = reader::<VideoReader>(h);
    let out = unsafe { std::slice::from_raw_parts_mut(out_buf, out_cap) };
    match r.skip_to_bgra_into(target, out) {
        Ok(Some((p, ww, hhh, sk))) => {
            unsafe {
                *pts = p;
                *w = ww;
                *hh = hhh;
                *skipped = sk;
            }
            1
        }
        Ok(None) => 0,
        Err(_) => -1,
    }
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
    if out_buf.is_null() {
        return -1;
    }
    let r = reader::<VideoReader>(h);
    // 直接写进调用方预分配缓冲（`next_frame_bgra_into` 内部不再 to_vec，省一次整帧分配+拷贝）
    let out = unsafe { std::slice::from_raw_parts_mut(out_buf, out_cap) };
    match r.next_frame_bgra_into(out) {
        Ok(Some((p, ww, hhh))) => {
            unsafe {
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

/// 打开视频（**外部 D3D11 设备注入**，零回读 GPU 直通）：`device_ptr` = `ID3D11Device*`
/// （必填）、`device_ctx_ptr` = `ID3D11DeviceContext*`（可空，null 由 FFmpeg 自行
/// `GetImmediateContext`）。两接口都只借用引用（本库各自 AddRef，`vff_close_video` 时由
/// FFmpeg 统一 Release）。`max_dim` 保留（HW 路径不做 sws，忽略）。成功写 `*out` 句柄
/// 返回 0；-1 = 打开失败 / 设备指针为空 / 解码器不支持。
///
/// 取帧走 [`vff_video_frame_hw`]（返回 `ID3D11Texture2D*`，不回读、不 sws）。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vff_open_video_hw(
    path: *const c_char,
    max_dim: u32,
    device_ptr: *mut c_void,
    device_ctx_ptr: *mut c_void,
    out: *mut Handle,
) -> c_int {
    if out.is_null() || device_ptr.is_null() {
        return -1;
    }
    let Some(p) = path_from(path) else {
        return -1;
    };
    let r = match VideoReader::open_external_hw(p, max_dim, device_ptr, device_ctx_ptr) {
        Ok(r) => r,
        Err(_) => return -1,
    };
    unsafe { *out = Box::into_raw(Box::new(r)) as Handle }
    0
}

/// 取下一帧 GPU 纹理句柄（零回读）。状态由**返回值指针**编码（句柄本身非空即帧，无法用
/// OutParam 表达 EOF/错误，故用保留地址作哨兵）：
/// - **非空且 ≠ 两个哨兵**：`*mut decode::HwFrame` 帧句柄，字段已写入
///   `*out_pts/*out_w/*out_h/*out_tex/*out_idx`；用完**必须** [`vff_video_frame_release`]
///   释放（Drop 帧的 AVBufferRef 引用 → 帧池槽位回归解码器复用）。
/// - `0x1 as *mut c_void`：**EOF**（流末尾，之后可再 `vff_video_seek` 复用）；未写输出。
/// - `usize::MAX as *mut c_void`：**解码错误**；未写输出。
///
/// 说明：堆分配的帧句柄地址至少按 `usize` 对齐，绝不会是 `0x1` 或 `usize::MAX`，哨兵无歧义。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vff_video_frame_hw(
    h: Handle,
    out_pts: *mut f64,
    out_w: *mut u32,
    out_h: *mut u32,
    out_tex: *mut usize,
    out_idx: *mut u32,
) -> *mut c_void {
    let r = reader::<VideoReader>(h);
    match r.next_frame_hw() {
        Ok(Some(frame)) => {
            unsafe {
                *out_pts = frame.pts_seconds;
                *out_w = frame.width;
                *out_h = frame.height;
                *out_tex = frame.tex;
                *out_idx = frame.array_index;
            }
            Box::into_raw(Box::new(frame)) as *mut c_void
        }
        Ok(None) => 0x1 as *mut c_void,
        Err(_) => usize::MAX as *mut c_void,
    }
}

/// 释放 [`vff_video_frame_hw`] 返回的帧句柄（Drop AVBufferRef 引用 → 帧池槽位回归解码器
/// 复用；**再解码不释放会导致帧池耗尽**）。对 null / EOF(`0x1`) / 错误(`usize::MAX`) 哨兵
/// 调用是安全的 no-op；传其它任意非法/未对齐指针是未定义行为（与其余 FFI 句柄同契约）。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vff_video_frame_release(frame_ptr: *mut c_void) {
    let p = frame_ptr as usize;
    if !frame_ptr.is_null() && p != 0x1 && p != usize::MAX {
        drop(unsafe { Box::from_raw(frame_ptr as *mut decode::HwFrame) });
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

// ---------------- 字幕 ----------------

/// 字幕格式 FFI 枚举。
#[repr(C)]
pub enum SubtitleFormatFFI {
    ASS = 0,
    SSA = 1,
    SRT = 2,
    Unknown = 3,
}

/// 字幕参数 FFI 结构。
#[repr(C)]
pub struct SubtitleParamsFFI {
    pub stream_count: u32,
    pub format: SubtitleFormatFFI,
}

/// 打开字幕流：成功写 `*out` 句柄返回 0。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vff_open_subtitle(
    path: *const c_char,
    out: *mut *mut c_void,
) -> c_int {
    if out.is_null() {
        return -1;
    }
    let Some(p) = path_from(path) else {
        return -1;
    };
    let r = match SubtitleReader::open(p) {
        Ok(r) => r,
        Err(_) => return -1,
    };
    unsafe { *out = Box::into_raw(Box::new(r)) as *mut c_void }
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn vff_subtitle_params(h: *mut c_void, out: *mut SubtitleParamsFFI) {
    let r = reader::<SubtitleReader>(h);
    let p = r.params();
    let format_ffi = match p.format {
        decode::SubtitleFormat::ASS => SubtitleFormatFFI::ASS,
        decode::SubtitleFormat::SSA => SubtitleFormatFFI::SSA,
        decode::SubtitleFormat::SRT => SubtitleFormatFFI::SRT,
        decode::SubtitleFormat::Unknown => SubtitleFormatFFI::Unknown,
    };
    unsafe {
        *out = SubtitleParamsFFI {
            stream_count: p.stream_count,
            format: format_ffi,
        };
    }
}

/// 字幕条目 FFI 结构（文本由调用方预分配缓冲）。
#[repr(C)]
pub struct SubtitleEntryFFI {
    pub start_pts: f64,
    pub end_pts: f64,
    /// 文本长度（UTF-8 字节数）。
    pub text_len: usize,
}

/// 获取字幕清单数量。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vff_subtitle_count(h: *mut c_void) -> usize {
    let r = reader::<SubtitleReader>(h);
    r.subtitles().len()
}

/// 获取指定索引的字幕条目信息（返回文本长度，调用方据此分配缓冲）。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vff_subtitle_get_info(
    h: *mut c_void,
    index: usize,
    out: *mut SubtitleEntryFFI,
) -> c_int {
    let r = reader::<SubtitleReader>(h);
    let Some(entry) = r.subtitles().get(index) else {
        return -1;
    };
    unsafe {
        *out = SubtitleEntryFFI {
            start_pts: entry.start_pts,
            end_pts: entry.end_pts,
            text_len: entry.text.len(),
        };
    }
    0
}

/// 获取指定索引的字幕文本（调用方预分配 `text_buf`，容量≥`text_len`）。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vff_subtitle_get_text(
    h: *mut c_void,
    index: usize,
    text_buf: *mut u8,
    text_cap: usize,
) -> c_int {
    let r = reader::<SubtitleReader>(h);
    let Some(entry) = r.subtitles().get(index) else {
        return -1;
    };
    if text_buf.is_null() || entry.text.len() > text_cap {
        return -1;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(entry.text.as_ptr(), text_buf, entry.text.len());
    }
    0
}

/// 获取当前时间应显示的字幕索引（-1 = 无）。
///
/// **dormancy（dormant）**：显示路径已被视频端 `find_active_subtitle` 取代（crates/video
/// 逐帧在 render 用预加载清单二分命中），本符号**不再被生产消费**——仅因主程序 FFI
/// 加载器（`crates/video/src/decode.rs` 的 `load_bridge`）仍按 `vff_subtitle_at\0` 解析
/// 符号而**保留导出**（缺符号会导致整桥加载失败 → 视频不可用）。语义与生产
/// `find_active_subtitle` 同口径（重叠窗口取「最近开始」），经 `active_subtitle_index`
/// 单一实现以消除三份拷贝的匹配口径漂移。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vff_subtitle_at(h: *mut c_void, seconds: f64) -> c_int {
    let r = reader::<SubtitleReader>(h);
    match r.active_subtitle_index(seconds) {
        Some(i) => i as c_int,
        None => -1,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn vff_subtitle_seek(h: *mut c_void, seconds: f64) -> c_int {
    let r = reader::<SubtitleReader>(h);
    r.seek(seconds) as c_int
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn vff_close_subtitle(h: *mut c_void) {
    if !h.is_null() {
        drop(unsafe { Box::from_raw(h as *mut SubtitleReader) });
    }
}
