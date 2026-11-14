//! 音乐解码运行时桥：**动态加载** `media_ffmpeg.dll`，复用其导出的音频 FFI。
//!
//! 本地桥（find_lib / sym! / OnceLock 缓存 / 'static 符号提升 / utf8_cstr）已迁至共享
//! `media_io::bridge`（全符号表 = 视频全集，含音频子集 `vff_audio_params`；加载语义逐字
//! 一致——same find_lib 路径、进程内缓存一次、损坏 dll 同 fallback）。本模块只取音频子集
//! 符号：open_audio / audio_params / audio_seek / audio_pcm / close_audio；解码语义与
//! media-ffmpeg 的 `AudioReader` 一致：统一重采样为 **F32 交错立体声**（`sample_rate` Hz），
//! `next_pcm` 每次返回一块样本与块首 pts（秒），EOF 返回 `None`，seek 后内部 decode flush + drain。
//!
//! # 线程纪律
//! 句柄只在解码线程内部创建/使用/释放（裸指针非 `Send`，与视频同款对齐）。FFmpeg
//! 并发闸（FFMPEG_LOCK）在 media-ffmpeg dll 内部持有：进程内 FFMpeg FFI 已全局串行，
//! music 侧无需再加锁（probe 与播放可各占线程并发调用，dll 自行串行）。

use std::path::Path;

use anyhow::{Context as _, Result, anyhow};

// 共享桥（audio 子集符号 + 类型）；本模块保留 music 侧包装
// （START_CAP / reserve / -1 扩容重试 等 buffer 管理细节）。
use media_io::bridge::{bridge, utf8_cstr, AudioParams, Handle, ParamFFI};

/// 运行库是否可用（`media_ffmpeg.dll` 与其 FFmpeg 依赖能否被加载，进程内缓存一次）。
/// 缺库 → false → 播放走纯状态（不打开解码器，窗口报「缺运行库」提示方向）。
/// Phase A 集成测试用；Phase B UI 入场提示亦可用。
#[allow(dead_code)]
pub(crate) fn bridge_ok() -> bool {
    media_io::bridge::bridge_ok()
}

/// 可 seek 的音频解码器（句柄只在持有线程内使用/释放）。
pub(crate) struct AudioCodec {
    handle: Handle,
}

impl Drop for AudioCodec {
    fn drop(&mut self) {
        if let Some(b) = bridge() {
            unsafe { (b.close_audio)(self.handle) }
        }
        self.handle = std::ptr::null_mut();
    }
}

/// 打开媒体音轨并重采样到 `sample_rate`/立体声/F32 交错；失败返回可读错误（含
/// 「media_ffmpeg.dll 缺失（缺少视频/音乐运行库）」提示方向）。
pub(crate) fn open_audio(path: &Path, sample_rate: u32) -> Result<AudioCodec> {
    let b = bridge()
        .context("media_ffmpeg.dll 缺失（缺少视频/音乐运行库），音乐播放不可用")?;
    let cpath = utf8_cstr(path)?;
    let mut handle: Handle = std::ptr::null_mut();
    let rc = unsafe { (b.open_audio)(cpath.as_ptr(), sample_rate, &mut handle) };
    if rc != 0 || handle.is_null() {
        return Err(anyhow!("打开音频失败: {}", path.display()));
    }
    Ok(AudioCodec { handle })
}

impl AudioCodec {
    /// 已确定的输出参数（速率/声道/时长）。
    pub(crate) fn params(&self) -> AudioParams {
        let b = bridge().expect("codec 持有期桥必在");
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
        AudioParams {
            sample_rate: ffi.sample_rate,
            channels: ffi.channels,
            duration: ffi.duration,
        }
    }

    /// 跳转到 `seconds`：dll 内部 decode flush + drain 复位，落点前坏帧由内核跳过。
    pub(crate) fn seek(&mut self, seconds: f64) -> Result<()> {
        let b = bridge().context("media_ffmpeg.dll 缺失（缺少视频/音乐运行库）")?;
        let rc = unsafe { (b.audio_seek)(self.handle, seconds) };
        if rc != 0 {
            return Err(anyhow!("音频 seek 失败"));
        }
        Ok(())
    }

    /// 推进到**下一块** F32 交错样本，写入 `out`（复用调用方缓冲）；返回块首 pts（秒）；
    /// `None` = EOF（之后可再 `seek` 复用）。
    ///
    /// 缓冲直接按 sink 队列上限（约 2 秒×立体声）起步：dll 单块输出是**一帧**重采样结果，
    /// 起点过小会在低采样率源/高设备速率（upsample 比例大）时被 `vff_audio_pcm` 返回 -1，
    /// 而 retry 时该帧已随 `next_pcm_f32` 推进被丢弃（`-1` 不写缓冲）→ 起始丢帧/隔帧丢。
    /// 大起步 + 下方 -1 扩容仅作超上限的防御兜底（单帧 >2s×2ch 实际不可能）。
    ///
    /// **R2-2**：容量用 `reserve` 增长、`clear`+`set_len` 写回——消除旧 `resize(192000,0)`
    /// 每块 768KB 全零 memset（稳态容量已足，零写入）；采样字节与逐字语义不变。
    pub(crate) fn next_pcm(&mut self, out: &mut Vec<f32>) -> Result<Option<f64>> {
        let b = bridge().context("media_ffmpeg.dll 缺失（缺少视频/音乐运行库）")?;
        const START_CAP: usize = 48_000 * 2 * 2; // ≈2s 立体声
        if out.capacity() < START_CAP {
            out.reserve(START_CAP - out.capacity());
        }
        out.clear();
        loop {
            let mut pts = 0.0f64;
            let mut written = 0usize;
            let rc = unsafe {
                (b.audio_pcm)(
                    self.handle,
                    out.as_mut_ptr(),
                    out.capacity(),
                    &mut pts,
                    &mut written,
                )
            };
            match rc {
                1 => {
                    // SAFETY: dll 保证 `written ≤ out.capacity()`（越界返回 -1 不写缓冲）
                    unsafe { out.set_len(written) };
                    return Ok(Some(pts));
                }
                0 => {
                    out.clear();
                    return Ok(None);
                }
                -1 if out.capacity() < (1 << 22) => {
                    // 缓冲过小（dll 单块超过当前容量）：扩容重试
                    out.reserve(out.capacity() * 4);
                }
                _ => {
                    out.clear();
                    return Err(anyhow!("解码音频失败"));
                }
            }
        }
    }
}
