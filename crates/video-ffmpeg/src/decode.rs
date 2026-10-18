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

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, Once};

use anyhow::{Context as _, Result, anyhow};

use ffmpeg_next as ffmpeg;
use ffmpeg::media::Type;
// windows-rs：IUnknown 的 vtable/from_raw/as_raw 是 `Interface` trait 的方法，须显式引 trait。
use windows::core::Interface as _;

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

/// 硬件解码默认开关。2026-10 本机探针（`bench_seek_preroll_hw_vs_sw` / `bench_decode_throughput`，
/// D3D11VA/DXVA2 设备均可建 rc=0、硬解输出帧能量与软解逐落点一致）结论：**硬解不更快，默认保持
/// 软解**——GPU decode 0.50ms/帧 省掉软解 2.91ms/帧，但每帧整帧回读 `av_hwframe_transfer_data`
/// ≈2.2ms + sws 更贵（1.66 vs 0.89ms/帧）把收益吃光：稳态 4.88 vs 4.59ms/帧、长 GOP seek 预滚
/// 整体 2579 vs 2350ms；且预滚帧在 worker 判定 pts 前也逐帧读完回读，硬解只赔不赚。
///
/// ⚠️ 若要「硬解默认开 + 失败自动回退软解」：把下面 `HW_DEFAULT` 改为 `true` 即可——`open_scaled`
/// 默认路径（`hw_required=false`→`try_open_hw_decoder` 按此开关）即「先试硬解、失败回软解」，
/// `VFF_VIDEO_SW=1|on|true` 仍确定性强制软解。翻转前先在目标机器复跑上述两个 `--ignored` 探针
/// 确认硬解净收益为正（本机为负，翻转会反而略慢 seek2first）。
const HW_DEFAULT: bool = false;

/// 硬件解码是否启用：默认见 [`HW_DEFAULT`]；`VFF_VIDEO_SW=1|on|true` 强制软解（优先级最高，
/// 测试/诊断确定性回退）；`VFF_VIDEO_HW=1|on|true` 显式开启、`=0|off|false` 显式关闭。
fn hw_enabled() -> bool {
    if matches!(
        std::env::var("VFF_VIDEO_SW"),
        Ok(v)
            if v.eq_ignore_ascii_case("1")
                || v.eq_ignore_ascii_case("on")
                || v.eq_ignore_ascii_case("true")
    ) {
        return false;
    }
    if matches!(
        std::env::var("VFF_VIDEO_HW"),
        Ok(v)
            if v.eq_ignore_ascii_case("1")
                || v.eq_ignore_ascii_case("on")
                || v.eq_ignore_ascii_case("true")
    ) {
        return true;
    }
    if matches!(
        std::env::var("VFF_VIDEO_HW"),
        Ok(v)
            if v.eq_ignore_ascii_case("0")
                || v.eq_ignore_ascii_case("off")
                || v.eq_ignore_ascii_case("false")
    ) {
        return false;
    }
    HW_DEFAULT
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

// ---------------- 外部 D3D11 设备注入（零回读 GPU 直通）----------------

/// D3D11VA 设备上下文的进程级互斥桥（可重入）：[`ffmpeg::ffi::AVD3D11VADeviceContext`] 的
/// `lock`/`unlock` 回调经此互斥量串行化对 `device_context`/`video_context` 的访问。
///
/// FFmpeg 头文件明确要求「底层锁必须可重入」——D3D11VA 解码/帧池操作同一线程内可能嵌套
/// 加锁，而 `std::sync::Mutex` 本身**不可**重入，直接用会自我死锁。故用「线程局部深度计数」
/// 包裹：深度 0→1 时才真正获取互斥量（同线程嵌套只递增深度），不同线程仍互斥
///（进程内全部解码操作已被 [`FFMPEG_LOCK`] 串行化，实际单线程触碰设备，此处仅兜底）。
static D3D11_LOCK: Mutex<()> = Mutex::new(());

std::thread_local! {
    /// 当前线程进入 D3D11 设备锁的嵌套深度（0 = 未持锁）。
    static D3D11_LOCK_DEPTH: Cell<u32> = const { Cell::new(0) };
    /// 深度 0→1 时真正持有的互斥量守卫（Drop 即释放；仅持锁线程访问）。
    static D3D11_LOCK_GUARD: RefCell<Option<std::sync::MutexGuard<'static, ()>>> =
        const { RefCell::new(None) };
}

/// D3D11 设备锁回调（extern "C" 内禁止 panic —— unwind 跨 ABI 会 abort，一律取回中毒锁兜底）。
unsafe extern "C" fn d3d11_lock_cb(_lock_ctx: *mut std::ffi::c_void) {
    D3D11_LOCK_DEPTH.with(|depth| {
        if depth.get() == 0 {
            let guard = D3D11_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            D3D11_LOCK_GUARD.with(|g| *g.borrow_mut() = Some(guard));
        }
        depth.set(depth.get() + 1);
    });
}

/// D3D11 设备解锁回调（与 [`d3d11_lock_cb`] 镜像：深度 1→0 时释放互斥量守卫）。
unsafe extern "C" fn d3d11_unlock_cb(_lock_ctx: *mut std::ffi::c_void) {
    D3D11_LOCK_DEPTH.with(|depth| {
        let d = depth.get();
        if d == 0 {
            return; // 防御：不应出现 unlock 先于 lock
        }
        if d == 1 {
            D3D11_LOCK_GUARD.with(|g| g.borrow_mut().take());
        }
        depth.set(d - 1);
    });
}

/// 对调用方 D3D11 接口指针做一次「借用 AddRef」的 RAII：构造时 `IUnknown::AddRef`、Drop 时
/// `Release`；[`Self::disarm`] 放弃 Drop 的 Release —— 成功注入 FFmpeg 后接口引用改由
/// FFmpeg 的 uninit 回调统一 Release（`AVD3D11VADeviceContext` 释放语义：dealloc 时 release
/// 用户分配的 device/device_context）。
struct AddRefGuard {
    unk: Option<windows::core::IUnknown>,
}

impl AddRefGuard {
    /// 对 `raw` 调用一次 AddRef；`raw` 为空 → `None`（无引用需管理）。
    fn new(raw: *mut std::ffi::c_void) -> Option<Self> {
        if raw.is_null() {
            return None;
        }
        // SAFETY: 调用方保证 `raw` 是有效的 COM 接口指针（IUnknown vtable 打头）——
        // FFI 契约（`vff_open_video_hw`）要求传 `ID3D11Device*`/`ID3D11DeviceContext*`。
        let unk = unsafe { windows::core::IUnknown::from_raw(raw) };
        unsafe {
            (unk.vtable().AddRef)(unk.as_raw());
        }
        Some(Self { unk: Some(unk) })
    }

    /// 放弃 Drop 时的 Release：引用所有权移交他人（此处即 FFmpeg uninit 释放）。
    fn disarm(&mut self) {
        self.unk = None;
    }
}

impl Drop for AddRefGuard {
    fn drop(&mut self) {
        if let Some(unk) = self.unk.take() {
            unsafe { (unk.vtable().Release)(unk.as_raw()) };
        }
    }
}

/// 构造「注入外部 D3D11 设备」的设备上下文 `AVBufferRef`（`AVHWDeviceType::D3D11VA`）：
/// `av_hwdevice_ctx_alloc` → 填 [`ffmpeg::ffi::AVD3D11VADeviceContext`]（device/device_context/
/// `BindFlags`（`shader_resource` 控制是否 OR `D3D11_BIND_SHADER_RESOURCE`）/可重入
/// lock/unlock 桥）→ `av_hwdevice_ctx_init`。
///
/// 引用纪律：init 前对调用方 `device`/`device_context` 各 AddRef 一次；`av_hwdevice_ctx_init`
/// 成功或失败都让 FFmpeg 的 uninit 回调负责 Release（`av_buffer_unref` 触发；失败时先 disarm
/// guard 防止双 Release）。`lock_ctx` 置空（全局桥，无需每设备状态）。
///
/// `shader_resource=true`（规范默认）时给帧纹理挂 `D3D11_BIND_SHADER_RESOURCE`，调用方
/// 可直接把解码纹理绑 SRV 采样（零回读 GPU 直通的前提）；但实测部分驱动（Intel Arc
/// 32.0.101.x 等）拒绝 `D3D11_BIND_DECODER | D3D11_BIND_SHADER_RESOURCE` 的 NV12 数组
/// 纹理（`CreateTexture2D` E_INVALIDARG），此时需 `shader_resource=false` 重开
/// （解码照常，仅纹理不再可被直接采样——见 [`VideoReader::open_external_hw`] 的回退逻辑）。
///
/// 返回 `AvBuf` 已持有对设备上下文 `AVBufferRef` 的引用（含 uninit 时的接口释放），
/// 调用方（[`VideoReader::open_external_hw`]）再 `av_buffer_ref` 一份挂给解码器。
fn build_external_d3d11_device_ctx(
    device: *mut std::ffi::c_void,
    device_context: *mut std::ffi::c_void,
    shader_resource: bool,
) -> Result<AvBuf> {
    use ffmpeg::ffi::{
        AVD3D11VADeviceContext, AVHWDeviceContext, AVHWDeviceType, av_buffer_unref,
        av_hwdevice_ctx_alloc, av_hwdevice_ctx_init,
    };

    let mut dev_guard = AddRefGuard::new(device).ok_or_else(|| anyhow!("外部 D3D11 设备指针为空"))?;
    let mut ctx_guard = AddRefGuard::new(device_context);

    let mut device_buf: *mut ffmpeg::ffi::AVBufferRef =
        unsafe { av_hwdevice_ctx_alloc(AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA) };
    if device_buf.is_null() {
        // guard Drop → Release（引用回滚，调用方自身引用不受影响）
        return Err(anyhow!("av_hwdevice_ctx_alloc(D3D11VA) 返回空"));
    }

    unsafe {
        let hwdev = (*device_buf).data as *mut AVHWDeviceContext;
        if hwdev.is_null() {
            av_buffer_unref(&mut device_buf);
            return Err(anyhow!("AVHWDeviceContext 为空"));
        }
        let hwctx = (*hwdev).hwctx as *mut AVD3D11VADeviceContext;
        if hwctx.is_null() {
            av_buffer_unref(&mut device_buf);
            return Err(anyhow!("D3D11VA 设备上下文 hwctx 为空"));
        }
        (*hwctx).device = device as *mut ffmpeg::ffi::ID3D11Device;
        (*hwctx).device_context = device_context as *mut ffmpeg::ffi::ID3D11DeviceContext;
        if shader_resource {
            // `D3D11_BIND_SHADER_RESOURCE = 0x2`：帧纹理需可绑为 ShaderResource（调用方 GPU
            // 直通着色器直接采样 YUV/NV12 纹理）——libavcodec 的 D3D11VA hwaccel 自行 OR
            // `D3D11_BIND_DECODER`，此处只需保证可被调用方绑定采样。部分驱动不支持该组合
            //（见函数注释），由 [`VideoReader::open_external_hw`] 探测后回退。
            (*hwctx).BindFlags |= 0x2;
        }
        // 可重入互斥桥（字段类型为 `*mut libc::c_void`，与 std::ffi::c_void 布局一致，经
        // transmute 贴合 bindgen 生成的函数指针签名；锁本身就是进程级全局静态量）。
        (*hwctx).lock = Some(std::mem::transmute(
            d3d11_lock_cb as unsafe extern "C" fn(*mut std::ffi::c_void),
        ));
        (*hwctx).unlock = Some(std::mem::transmute(
            d3d11_unlock_cb as unsafe extern "C" fn(*mut std::ffi::c_void),
        ));
        (*hwctx).lock_ctx = std::ptr::null_mut();
    }

    let rc = unsafe { av_hwdevice_ctx_init(device_buf) };
    if rc < 0 {
        // FFmpeg 的 uninit 回调会 Release 我们 AddRef 的 device/device_context → 先移交再释放
        dev_guard.disarm();
        if let Some(g) = ctx_guard.as_mut() {
            g.disarm();
        }
        unsafe { av_buffer_unref(&mut device_buf) };
        return Err(anyhow!("av_hwdevice_ctx_init(D3D11VA) 失败 (err={rc})"));
    }

    // 成功：接口引用归 FFmpeg（uninit 统一 Release），本 guard 不再管理
    dev_guard.disarm();
    if let Some(g) = ctx_guard.as_mut() {
        g.disarm();
    }
    Ok(AvBuf(device_buf))
}

/// 探测驱动是否支持「D3D11VA 解码数组纹理带 SHADER_RESOURCE」：用**带 SHR** 的设备上下文
/// 建一个最小帧池（NV12 数组 ×4，BindFlags = `DECODER | SHADER_RESOURCE`，与 libavcodec
/// D3D11VA frame_params 一致）跑 `av_hwframe_ctx_init`——成功 = 支持；失败
///（`CreateTexture2D` E_INVALIDARG，实测 Intel Arc 32.0.101.x 等驱动拒绝该组合）=
/// 不支持、需回退无 SHR 的设备上下文。
///
/// 纯 device/frames 上下文、**不开解码器** → 失败无副作用（表由 [`VideoReader::open_external_hw`]
/// 在打开解码器前调用；只用 `AVHWFramesContext` 最小池探测，顺带验证帧池纹理可建）。
/// `pub(crate)`：经 lib.rs 的 [`vff_d3d11_supports_shader_bound_textures`] 导出给主程序做
/// 双设备直通的门控能力查询（与 `open_external_hw` 内部的探测同源）。
pub(crate) fn d3d11_supports_shader_bound_textures(
    device: *mut std::ffi::c_void,
    device_context: *mut std::ffi::c_void,
) -> bool {
    use ffmpeg::ffi::{AVBufferRef, AVD3D11VAFramesContext, AVHWFramesContext};
    let Ok(dev) = build_external_d3d11_device_ctx(device, device_context, true) else {
        return false;
    };
    let ok = unsafe {
        let frames =
            ffmpeg::ffi::av_hwframe_ctx_alloc(ffmpeg::ffi::av_buffer_ref(dev.0));
        if frames.is_null() {
            false
        } else {
            let fc = (*frames).data as *mut AVHWFramesContext;
            (*fc).format = ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_D3D11;
            (*fc).sw_format = ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_NV12;
            (*fc).width = 1280; // 尺寸无关紧要（创建失败与尺寸无关）
            (*fc).height = 720;
            (*fc).initial_pool_size = 4;
            let fhw = (*fc).hwctx as *mut AVD3D11VAFramesContext;
            (*fhw).BindFlags |= 0x20 | 0x2; // DECODER | SHADER_RESOURCE
            let rc = ffmpeg::ffi::av_hwframe_ctx_init(frames);
            ffmpeg::ffi::av_buffer_unref(&mut (frames as *mut AVBufferRef));
            rc >= 0
        }
    };
    drop(dev);
    ok
}

/// 单帧 HW 帧的 `AVBufferRef` 引用钉：把解码帧的 AVBufferRef 引用复制一份私有持有，
/// 帧池槽位（D3D11 数组纹理元素）在被引用期间不归还解码器；Drop 时 `av_frame_unref` +
/// `av_frame_free` 释放该引用 → 槽位归还帧池复用（原子计数，任意线程可释放）。
///
/// `*mut AVFrame` 是 raw 指针（[`HwFramePin`] 显式 `unsafe impl Send`），整个结构因此可
/// 跨线程 —— 与 [`HwFrame`] 一起从解码 worker 线程安全移动到渲染线程/FFI 释放侧。
struct HwFramePin {
    /// 私有帧（`av_frame_alloc` + `av_frame_ref` 结果的持有者）。
    #[allow(dead_code)] // 仅 Drop 时经 unref/free 释放；字段本身不读
    frame: *mut ffmpeg::ffi::AVFrame,
}

impl Drop for HwFramePin {
    fn drop(&mut self) {
        unsafe {
            ffmpeg::ffi::av_frame_unref(self.frame);
            ffmpeg::ffi::av_frame_free(&mut self.frame);
        }
    }
}

// SAFETY: `HwFramePin` 内的 `*mut AVFrame` 只能被「唯一持有者」经 `av_frame_unref/free`
// 释放，不涉及并发读写同一帧（解码器在 `receive_frame` 写的是它自己的局部帧，与我们的
// pin 引用相互独立）；AVBufferRef 引用计数本身是原子的 → 可安全跨线程传递/释放。
unsafe impl Send for HwFramePin {}

/// 一帧零回读 GPU 解码结果：`tex` = `ID3D11Texture2D*`（调用方直接作 D3D11 纹理采样，
/// 不做 `av_hwframe_transfer_data` 回读、不做 sws）；`array_index` = 数组纹理元素下标
/// （D3D11VA 池 = 单张 NV12 数组纹理）。`pin` 持有 AVBufferRef 引用钉住该槽位，直到
/// 框架被 `drop`（或 FFI `vff_video_frame_release`）释放。
///
/// `Send`：解码 worker 线程产出后移交渲染线程持有/释放；释放只动 AVBufferRef 的原子引用计数，
/// 不触解码器共享状态，故无需 [`FFMPEG_LOCK`]（与 ffmpeg-next `Frame` 的 Drop 同纪律——
/// 否则持锁 drop 与同样持锁的解码循环互相死锁）。
pub struct HwFrame {
    /// 帧 pts（秒，流 time_base）。
    pub pts_seconds: f64,
    /// 显示宽。
    pub width: u32,
    /// 显示高。
    pub height: u32,
    /// `ID3D11Texture2D*`（转 usize 的透明句柄，指向帧池数组纹理基址）。
    pub tex: usize,
    /// 数组纹理元素下标（该帧在池中的槽位，∈ [0, ArraySize)）。
    pub array_index: u32,
    /// 钉住帧池槽位的 AVBufferRef 引用（Drop 即归还）。
    #[allow(dead_code)] // 字段本身不读、仅做 RAII 保活（见类型注释）
    pin: Box<dyn Any + Send>,
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
                let p_sec = pts_to_seconds(p, self.time_base);
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
                let pts = pts_to_seconds(decoded.pts().unwrap_or(0), self.time_base);
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

/// 帧 pts（第纳秒）转秒（time_base 换算）——视频/音频共用的公共换算。
#[inline]
fn pts_to_seconds(pts: i64, time_base: ffmpeg::Rational) -> f64 {
    pts as f64 * f64::from(time_base.numerator()) / f64::from(time_base.denominator())
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

    /// 视频夹具：§5 手动装配（不入库），320×240 / 2s@30fps / H.264(yuv420p)+AAC 2ch/48k，
    /// 关键帧间隔 15 帧（seek(1.0) 贴近落点满足 `|pts-1.0|<0.15`）。
    fn fixture() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target")
            .join("video测试.mp4")
    }

    /// 缺夹具时 SKIP 不 FAIL（媒体夹具不入库，同 viewer 约定）；返回已存在的路径。
    macro_rules! fixture_or_skip {
        () => {{
            let p = fixture();
            if !p.exists() {
                eprintln!("SKIP: missing target/video测试.mp4（媒体夹具不入库，手动装配）");
                return;
            }
            p
        }};
    }

    #[test]
    fn open_reads_params() {
        let r = VideoReader::open(&fixture_or_skip!()).unwrap();
        let p = r.params();
        assert_eq!((p.width, p.height), (320, 240));
        assert!((1.9..=2.5).contains(&p.duration), "duration={}", p.duration);
        assert!(p.frame_rate.is_some(), "应识别出帧率");
    }

    /// decode-to-display 下采样：长边封顶后保持宽高比（320×240 → 160×120），
    /// 逐帧输出与内存按输出尺寸走；`params()` 仍报源宽高供显示适配。
    #[test]
    fn open_scaled_caps_longest_side_and_keeps_source_params() {
        let mut r = VideoReader::open_scaled(&fixture_or_skip!(), 160).unwrap();
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
        let r = VideoReader::open_scaled(&fixture_or_skip!(), 4096).unwrap();
        assert_eq!(r.params(), VideoReader::open(&fixture_or_skip!()).unwrap().params());
        let mut r = VideoReader::open_scaled(&fixture_or_skip!(), 4096).unwrap();
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
        let mut r = VideoReader::open(&fixture_or_skip!()).unwrap();
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
        let mut r = VideoReader::open(&fixture_or_skip!()).unwrap();
        r.seek(1.0).unwrap();
        let (pts, ..) = r.next_frame_bgra().unwrap().expect("seek 后应出帧");
        // 回拉关键帧最多落后一点；testsrc 全 intraframe 应贴近
        assert!((pts - 1.0).abs() < 0.15, "seek 落点偏差过大 pts={pts}");
    }

    #[test]
    fn reaches_eof_then_seek_reuses() {
        let mut r = VideoReader::open(&fixture_or_skip!()).unwrap();
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
            .join("../../target")
            .join("video测试有声.mp4")
    }

    /// 缺音频夹具时 SKIP 不 FAIL（不入库，同 viewer 约定）；返回已存在的路径。
    macro_rules! audio_fixture_or_skip {
        () => {{
            let p = audio_fixture();
            if !p.exists() {
                eprintln!("SKIP: missing target/video测试有声.mp4（音频夹具不入库，手动装配）");
                return;
            }
            p
        }};
    }

    #[test]
    fn audio_open_and_decode() {
        let mut r = AudioReader::open_pcm_f32(&audio_fixture_or_skip!(), 48000).unwrap();
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
        let mut r = AudioReader::open_pcm_f32(&audio_fixture_or_skip!(), 48000).unwrap();
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
        let path = audio_fixture_or_skip!();
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
        let mut r = VideoReader::open_scaled(&fixture_or_skip!(), 160).unwrap();
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
        match VideoReader::open_hw_only(&fixture_or_skip!(), u32::MAX) {
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
        let secs = r.pts_seconds(pts);
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
            // 软路径确定性强制软解（无视环境里可能残留的 VFF_VIDEO_HW；ignore 手动跑，进程内自控）
            // SAFETY: `--ignored` 手动/串行诊断用，无并行测试竞争该 env。
            unsafe { std::env::set_var("VFF_VIDEO_SW", "1") };
            let r = VideoReader::open_scaled_impl(&candidate, 1920, false)
                .context("软路径打开失败")?;
            unsafe { std::env::remove_var("VFF_VIDEO_SW") };
            assert!(r.hw.is_none(), "软路径必须无硬件（VFF_VIDEO_SW=1 强制软解）");
            info(&r);
            bench_leg_canonical("软解", r, frames, warmup);
            unsafe { std::env::set_var("VFF_VIDEO_SW", "1") };
            let r2 = VideoReader::open_scaled_impl(&candidate, 1920, false)
                .context("软路径拆账重开失败")?;
            unsafe { std::env::remove_var("VFF_VIDEO_SW") };
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

    /// seek 预滚成本对照（SW vs HW，`--ignored` 手动跑）：长 GOP 中段落点 seek 后解码到
    /// 「首个 pts ≥ 落点」帧的 wall clock 与预滚帧数——即「点击进度条后 seek2first」的
    /// 解码阶段子项。软路径经 `VFF_VIDEO_SW=1` 强制（默认硬解的机器上也确定性走软解）；
    /// 硬路径走 `open_hw_only`（force，无软回退）。断言首个命中帧输出有效（非全黑/纯色，
    /// 只测能量不写内容——夹具敏感）。缺夹具 SKIP（`target\视频测试字幕.mkv` 不入库）。
    #[test]
    #[ignore = "手动测量用"]
    fn bench_seek_preroll_hw_vs_sw() -> anyhow::Result<()> {
        use std::time::Instant;

        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/视频测试字幕.mkv");
        if !path.exists() {
            eprintln!("SKIP: missing target/视频测试字幕.mkv（长 GOP 夹具不入库）");
            return Ok(());
        }
        // 落点：开头近距 / 长 GOP 中段最坏档（≈停下 6-15s 长帧距）/ 两处中段 / 近结尾
        let targets = [12.0, 90.0, 333.3, 601.7, 900.1, 1200.5, 1500.3, 1766.0];
        // SAFETY: 探针为 `--ignored` 手动/串行诊断用，无并行测试竞争该 env；跑完还原。
        unsafe { std::env::set_var("VFF_VIDEO_SW", "1") };
        let t0 = Instant::now();
        let sw = VideoReader::open_scaled_impl(&path, 1920, false)
            .context("软路径打开失败")?;
        let sw_open_ms = t0.elapsed().as_secs_f64() * 1e3;
        unsafe { std::env::remove_var("VFF_VIDEO_SW") };
        assert!(sw.hw.is_none(), "VFF_VIDEO_SW=1 软路径必须无硬件");

        let hw = match VideoReader::open_hw_only(&path, 1920) {
            Ok(r) => {
                assert!(r.hw.is_some(), "open_hw_only 必须启用硬件");
                Some(r)
            }
            Err(e) => {
                eprintln!("[SEEKPREROLL] 硬解不可用，仅软解对照: {e:#}");
                None
            }
        };

        eprintln!(
            "[SEEKPREROLL] 文件={} 硬解可用={}",
            path.display(),
            hw.is_some()
        );

        // 单腿测量：8 落点 seek→首帧 ≥ 落点。返回每个落点的 (首帧 pts, 预滚帧数, wall, energy)。
        let measure = |mut r: VideoReader, name: &str| -> anyhow::Result<Vec<(f64, f64, f64, f64)>> {
            let mut rows = Vec::with_capacity(targets.len());
            for &t in &targets {
                r.seek(t)?;
                let cap = (r.out_width as usize) * (r.out_height as usize) * 4;
                let mut out = vec![0u8; cap];
                let t1 = Instant::now();
                let mut n: u64 = 0;
                let mut first: f64 = -1.0;
                let mut energy = 0.0;
                loop {
                    n += 1;
                    match r.next_frame_bgra_into(&mut out)? {
                        Some((pts, ..)) if pts >= t => {
                            first = pts;
                            energy = frame_energy(&out, r.out_width, r.out_height);
                            break;
                        }
                        Some(_) => continue, // 预滚帧只解不取
                        None => break,       // 近结尾 EOF 且无 ≥ 落点帧：认首帧缺席
                    }
                }
                let wall_ms = t1.elapsed().as_secs_f64() * 1e3;
                eprintln!(
                    "[SEEKPREROLL] {name} target={t:.1}s → 首帧 pts={first:.2}s 预滚帧={n} seek2first={wall_ms:.1}ms energy={energy:.1}",
                );
                rows.push((t, first, wall_ms, energy));
            }
            Ok(rows)
        };

        let t0 = Instant::now();
        let sw_rows = measure(sw, "SW")?;
        eprintln!(
            "[SEEKPREROLL] SW 整体 wall={:.1}ms（open={sw_open_ms:.1}ms + 8 落点 seek+预滚）",
            t0.elapsed().as_secs_f64() * 1e3
        );
        if let Some(hw) = hw {
            let t0 = Instant::now();
            let hw_rows = measure(hw, "HW")?;
            eprintln!(
                "[SEEKPREROLL] HW 整体 wall={:.1}ms（含 8 落点 seek+预滚）",
                t0.elapsed().as_secs_f64() * 1e3
            );
            // 内容有效性用「软硬同落点 energy 对照」判：真实暗场两路都低（相互印证），
            // 任一路独高另一路独低 → 可疑花屏/黑帧（GPU 回读垃圾）。
            for (i, swt) in sw_rows.iter().enumerate() {
                let (t, sw_e, hw_e) = (swt.0, swt.3, hw_rows[i].3);
                let both_low = sw_e < 3.0 && hw_e < 3.0; // 双方都判暗场 → 一致，可信
                if !both_low {
                    assert!(
                        (sw_e - hw_e).abs() < 30.0 || (sw_e > 3.0 && hw_e > 3.0),
                        "target={t}s 软硬 energy 背离（SW={sw_e:.1} HW={hw_e:.1}）疑似花屏/黑帧"
                    );
                }
            }
        }
        Ok(())
    }

    /// BGRA 帧内容能量（粗判非黑/非花屏）：等距抽样像素算平均绝对偏差；真实画面远高于
    /// 纯黑/纯色的 ~0。只判「帧有效」，不含具体内容断言（夹具敏感）。
    fn frame_energy(bgra: &[u8], w: u32, h: u32) -> f64 {
        let px = w as usize * h as usize;
        if px == 0 {
            return 0.0;
        }
        const STEP: usize = 8;
        let n = (px / STEP).max(1);
        let mut samples: Vec<i64> = Vec::with_capacity(n);
        for i in (0..px).step_by(STEP) {
            let b = i * 4;
            samples.push((bgra[b] as i64 + bgra[b + 1] as i64 + bgra[b + 2] as i64) / 3);
        }
        let mean = samples.iter().sum::<i64>() as f64 / samples.len() as f64;
        samples
            .iter()
            .map(|s| (*s as f64 - mean).abs())
            .sum::<f64>()
            / samples.len() as f64
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

// ---------------- 字幕（ASS/SSA）----------------

/// 字幕格式枚举。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SubtitleFormat {
    ASS,
    SSA,
    SRT,
    Unknown,
}

/// 字幕顶层参数。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SubtitleParams {
    /// 字幕流数量。
    pub stream_count: u32,
    /// 首选字幕格式。
    pub format: SubtitleFormat,
}

/// 一条解码后的字幕条目。
#[derive(Clone, Debug)]
pub struct SubtitleEntry {
    /// 开始时间（秒）。
    pub start_pts: f64,
    /// 结束时间（秒）。
    pub end_pts: f64,
    /// 字幕文本（ASS 格式可能含样式标记）。
    pub text: String,
}

/// 可 seek 的字幕解码器：提取 ASS/SSA/SRT 字幕。
///
/// 与 [`VideoReader`] / [`AudioReader`] 同款姿势：调用方搬进后台线程逐条拉取。
///
/// **释放纪律**：`Drop` 把 FFmpeg 成员（`Input`/decoder）手动 drop 并持有
/// [`FFMPEG_LOCK`] —— 同视频/音频的串行纪律。
pub struct SubtitleReader {
    ictx: std::mem::ManuallyDrop<ffmpeg::format::context::Input>,
    /// 已确定的输出参数（仅供 [`Self::params`] 返回）。
    params: SubtitleParams,
    /// 预加载的字幕清单（打开时一次性加载全部）。
    subtitles: Vec<SubtitleEntry>,
    /// 当前读取位置索引。
    current_index: usize,
}

impl Drop for SubtitleReader {
    fn drop(&mut self) {
        let _ffmpeg_guard = FFMPEG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            std::mem::ManuallyDrop::drop(&mut self.ictx);
        }
    }
}

impl SubtitleReader {
    /// 打开媒体并提取字幕流。
    ///
    /// **锁纪律**：`format::input`（avformat_open_input）与并发 open/decode 互占死
    /// （FFMPEG_LOCK 注释与 AudioReader 释放纪律同源），故打开/查流/定格式在全局锁内
    /// 完成；拿到 `ictx` 后**提前释放锁**再对全文件做 `packets()` 扫描——扫描是纯
    /// demux（不开解码器、逐包读独立文件句柄），与视频/音频 worker 的 open/decode/seek
    /// 并存（并发压力测试 `subtitle_scan_concurrent_with_video_seek_stress` 实证：多轮
    /// 不崩、不死锁、seek 不被拖慢）。若在锁内扫完整 415MB 文件（≈0.4-0.6s），期间
    /// **所有** A/V FFI 调用被闸阻塞 → 打开后窗口期点击进度条 seek 卡顿（复现测试
    /// `subtitle_scan_does_not_block_video_seek` 量化 delta≈385ms 即为该因）。
    pub fn open(path: &Path) -> Result<Self> {
        let (mut ictx, stream_index, time_base, format) = {
            let _ffmpeg_guard = FFMPEG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            ffmpeg::init().context("ffmpeg init 失败")?;
            quiet_ffmpeg_log_once();

            let ictx = ffmpeg::format::input(path)
                .with_context(|| format!("打开媒体失败：{}", path.display()))?;

            // 查找字幕流
            let stream = ictx
                .streams()
                .best(Type::Subtitle)
                .ok_or_else(|| anyhow!("媒体中没有字幕流：{}", path.display()))?;

            let stream_index = stream.index();
            let time_base = stream.time_base();

            // 检测字幕格式
            let codec_id = stream.parameters().id();
            let format = match codec_id {
                ffmpeg::codec::Id::ASS => SubtitleFormat::ASS,
                ffmpeg::codec::Id::SSA => SubtitleFormat::SSA,
                ffmpeg::codec::Id::SUBRIP => SubtitleFormat::SRT,
                ffmpeg::codec::Id::HDMV_PGS_SUBTITLE => SubtitleFormat::Unknown, // 图形字幕不支持
                _ => SubtitleFormat::Unknown,
            };

            (ictx, stream_index, time_base, format)
        };
        // ← 全局锁已提前释放：以下全文件扫描为无锁并发纯 demux

        let params = SubtitleParams {
            stream_count: 1,
            format,
        };
        
        // 一次性加载所有字幕（`ictx.packets()` 产出 `(Stream, Packet)` 元组，迭代无失败分支）；
        // `stream_index/time_base/format` 只在 open 期扫描解析用，不作暂存（无流级重定位消费者）
        let mut subtitles = Vec::new();
        for (stream, pkt) in ictx.packets() {
            if stream.index() == stream_index {
                // 解析字幕包
                if let Some(entry) = Self::parse_subtitle_packet(&pkt, time_base, format) {
                    subtitles.push(entry);
                }
            }
        }

        // 按开始时间排序
        subtitles.sort_by(|a, b| a.start_pts.partial_cmp(&b.start_pts).unwrap_or(std::cmp::Ordering::Equal));

        // 兜底封闭窗口：包 duration 缺失/为 0 的事件会把 `end_pts` 留成 `start_pts`（永不结束）。
        // 以「下一条的开始」封闭；末条给默认窗口。本仓 `视频测试字幕.mkv` 各包都带 duration，
        // 此兜底只护其它源。
        for i in 0..subtitles.len() {
            if subtitles[i].end_pts <= subtitles[i].start_pts {
                let next_start = subtitles.get(i + 1).map(|n| n.start_pts);
                subtitles[i].end_pts = next_start
                    .unwrap_or(subtitles[i].start_pts + Self::guess_subtitle_duration(format));
            }
        }
        
        Ok(Self {
            ictx: std::mem::ManuallyDrop::new(ictx),
            params,
            subtitles,
            current_index: 0,
        })
    }
    
    /// 解析字幕包为 `SubtitleEntry`。
    ///
    /// ASS/SSA 事件在容器里有两种封存姿势，这里都兼容：
    /// 1. **老式「Dialogue: 」整行**（行内自带 `Layer,Start,End,Style,…` 时间）——
    ///    [`Self::parse_ass_dialogue`]；
    /// 2. **Matroska 新块格式**（`Layer,Style,Name,MarginL,MarginR,MarginV,Effect,Text`，
    ///    开始/结束时间不在行内，由**包 PTS + duration** 提供）。本仓 `视频测试字幕.mkv`
    ///    即此姿势：首包数据 `0,0,Default,,0,0,0,,<字幕文本>`，pts=20050ms、
    ///    duration=1980ms → start≈20.05s、end≈22.03s。
    fn parse_subtitle_packet(
        pkt: &ffmpeg::Packet,
        time_base: ffmpeg::Rational,
        format: SubtitleFormat,
    ) -> Option<SubtitleEntry> {
        let data = pkt.data()?;
        let Ok(text) = std::str::from_utf8(data) else {
            return None;
        };
        let text = text.trim();
        if text.is_empty() {
            return None;
        }

        if format == SubtitleFormat::ASS || format == SubtitleFormat::SSA {
            // 老式整行：行内时间优先（以字幕作者写入的时间为准）
            if text.starts_with("Dialogue:") {
                return Self::parse_ass_dialogue(text);
            }
            // Matroska 新块格式（mkvmerge 惯例，实测 `视频测试字幕.mkv` 首包即此布局）：
            //   `readorder,layer,style,name,marginL,marginR,marginV,effect,text`
            // 前 8 个字段 + 文本；文本字段可含逗号 → `splitn(9, ',')`。
            // 少数 mux 不计 readorder（7 前导）时退到 `splitn(8, ',')` 兜底。
            let dialogue = {
                let pieces8: Vec<&str> = text.splitn(9, ',').collect();
                if pieces8.len() >= 9 {
                    pieces8[8].trim()
                } else {
                    let pieces7: Vec<&str> = text.splitn(8, ',').collect();
                    if pieces7.len() >= 8 {
                        pieces7[7].trim()
                    } else {
                        return None;
                    }
                }
            };
            if dialogue.is_empty() {
                return None;
            }
            let (start_pts, end_pts) = Self::packet_window(pkt, time_base);
            return Some(SubtitleEntry {
                start_pts,
                end_pts,
                text: dialogue.to_string(),
            });
        } else if format == SubtitleFormat::SRT {
            // SRT 无内嵌时间字段：开始/结束由包 PTS + duration 提供
            let (start_pts, end_pts) = Self::packet_window(pkt, time_base);
            return Some(SubtitleEntry {
                start_pts,
                end_pts,
                text: text.to_string(),
            });
        }

        None
    }

    /// 字幕时间窗口：包 PTS * 时间基 = 开始秒；PTS + 包 duration * 时间基 = 结束秒。
    /// `duration` 缺失（为 0）时结束 = 开始（由 open 后置兜底按「下一条开始/默认窗」封闭）。
    fn packet_window(pkt: &ffmpeg::Packet, time_base: ffmpeg::Rational) -> (f64, f64) {
        let tb = f64::from(time_base);
        let start = pkt.pts().unwrap_or(0) as f64 * tb;
        let end = start + pkt.duration() as f64 * tb;
        (start, end)
    }

    /// 解析「Dialogue: …」整行（行内 `Layer,Start,End,Style,Name,MarginL,MarginR,MarginV,
    /// Effect,Text`，时间格式 `H:MM:SS.cc`）。
    fn parse_ass_dialogue(text: &str) -> Option<SubtitleEntry> {
        // ASS Dialogue 格式：Dialogue: Layer,Start,End,Style,Name,MarginL,MarginR,MarginV,Effect,Text
        if !text.starts_with("Dialogue:") {
            return None;
        }

        // `Dialogue:` 之后是 ` Layer,Start,End,...`（9 个字段 + 文本；文本可含逗号 → splitn(10, ','))
        let parts: Vec<&str> = text["Dialogue:".len()..].splitn(10, ',').collect();
        if parts.len() < 10 {
            return None;
        }

        let start_secs = Self::parse_ass_time(parts[1])?;
        let end_secs = Self::parse_ass_time(parts[2])?;
        // 提取层保留原始文本（样式标记由 viewer 渲染层统一清理，见 video::subtitle::parse_ass_text）
        let text_content = parts[9].trim();

        Some(SubtitleEntry {
            start_pts: start_secs,
            end_pts: end_secs,
            text: text_content.to_string(),
        })
    }

    /// 解析 ASS 时间戳（格式：H:MM:SS.cc 或 H:MM:SS.ccc）。
    fn parse_ass_time(s: &str) -> Option<f64> {
        let s = s.trim();
        // 格式：H:MM:SS.cc 或 H:MM:SS.ccc
        let parts: Vec<&str> = s.split(':').collect();
        if parts.len() != 3 {
            return None;
        }

        let hours: f64 = parts[0].parse().ok()?;
        let minutes: f64 = parts[1].parse().ok()?;
        let seconds: f64 = parts[2].parse().ok()?;

        Some(hours * 3600.0 + minutes * 60.0 + seconds)
    }
    
    /// 猜测字幕持续时间（用于 SRT 等无明确结束时间的格式）。
    fn guess_subtitle_duration(format: SubtitleFormat) -> f64 {
        match format {
            SubtitleFormat::ASS | SubtitleFormat::SSA => 2.0, // ASS 有明确结束时间
            SubtitleFormat::SRT => 2.0,
            SubtitleFormat::Unknown => 2.0,
        }
    }
    
    /// 已确定的输出参数。
    pub fn params(&self) -> SubtitleParams {
        self.params
    }
    
    /// 获取所有字幕条目。
    pub fn subtitles(&self) -> &[SubtitleEntry] {
        &self.subtitles
    }
    
    /// 跳转到指定时间（秒），返回该时间之后的第一个字幕索引。
    pub fn seek(&mut self, seconds: f64) -> usize {
        // 二分查找第一个 start_pts >= seconds 的字幕
        let idx = self.subtitles.partition_point(|s| s.start_pts < seconds);
        self.current_index = idx;
        idx
    }
    
    /// 取下一条字幕（按时间顺序）。
    #[allow(dead_code)] // 预留「逐条拉取」遍历 API（配合 [`Self::seek`] 定位起点）；
    /// 当前 FFI/上层走预加载清单 `subtitles()` + `subtitle_at()`，未消费本方法。
    pub fn next_subtitle(&mut self) -> Option<&SubtitleEntry> {
        if self.current_index < self.subtitles.len() {
            let entry = &self.subtitles[self.current_index];
            self.current_index += 1;
            Some(entry)
        } else {
            None
        }
    }
    
    /// 获取当前时间应显示的字幕。
    ///
    /// 线性扫描（窗口可重叠、含端点，与 `vff_subtitle_at` 同口径）。显示路径已被
    /// `video::subtitle::find_active_subtitle` 取代（dormancy）——本方法仅供本 crate
    /// 单测对拍（生产 FFI `vff_subtitle_at` 保留符号、同款 dormancy 注释，见 lib.rs）。
    #[allow(dead_code)] // 仅单测对拍用（dormancy：显示已走 find_active_subtitle）
    pub fn subtitle_at(&self, seconds: f64) -> Option<&SubtitleEntry> {
        for entry in &self.subtitles {
            if seconds >= entry.start_pts && seconds <= entry.end_pts {
                return Some(entry);
            }
        }
        None
    }
}

#[cfg(test)]
mod subtitle_tests {
    use super::*;
    use std::path::PathBuf;

    /// 回归（A3）：真实 MKV 内封 ASS 提取。[`SubtitleReader::open`] 应从
    /// `target/视频测试字幕.mkv`（ASS 流 index=2，codec=ass）按「Matroska 新块格式 +
    /// 包 PTS/duration」提取字幕：首条 start≈20.05s、end≈22.03s、文本非空且无 ASS 标签。
    /// 缺夹具 SKIP（夹具不入库，与 A1/A2 同口径，SKIP 是常态不是回归）。
    #[test]
    fn subtitle_reader_extracts_ass_mkv() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/视频测试字幕.mkv");
        if !path.exists() {
            eprintln!("SKIP: missing target/视频测试字幕.mkv（夹具不入库）");
            return;
        }
        let r = SubtitleReader::open(&path).unwrap();
        let subs = r.subtitles();
        assert!(
            !subs.is_empty(),
            "应从 MKV 提取到字幕，实得 {} 条（txt 应非空）",
            subs.len()
        );
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
        assert!(
            !first.text.is_empty(),
            "首条文本应非空（纯文本已剥离 ASS 标签），实得 {:?}",
            first.text
        );
        assert!(
            !first.text.contains("<font") && !first.text.contains("<b"),
            "首条文本不应含 ASS 标签，实得 {:?}",
            first.text
        );
        // 活动字幕命中：位于 [start, end] 内应选中该条，之外不应命中
        assert!(
            r.subtitle_at(20.5).is_some(),
            "20.5s 应命中活动字幕（首条 [20.05, 22.03]）"
        );
    }

    /// 回归（K-锁）：字幕全文件扫描不得长期占全局 FFMPEG_LOCK 阻塞视频 seek。
    ///
    /// 现象：打开 `目标/视频测试字幕.mkv`（415MB、511 条 ASS）后点击进度条卡顿——
    /// 前序实现 `SubtitleReader::open` 持锁扫完整文件（≈0.6s），期间**所有** A/V FFI
    /// 调用被闸阻塞。本测试量化：起字幕扫描线程并发跑同款视频 seek+出帧，断言
    /// seek 相对增量 < 250ms（修复前 delta≈扫描持锁时长 → FAIL；修复后扫描在锁外
    /// 纯 demux → delta≈0 → PASS）。
    #[test]
    fn subtitle_scan_does_not_block_video_seek() {
        let big = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/视频测试字幕.mkv");
        let small = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/video测试.mp4");
        if !big.exists() {
            eprintln!("SKIP: missing target/视频测试字幕.mkv（夹具不入库）");
            return;
        }
        if !small.exists() {
            eprintln!("SKIP: missing target/video测试.mp4（夹具不入库）");
            return;
        }

        // 小文件预开视频 reader（长边 160 封顶 → 固定小缓冲），seek 复用常驻 reader
        let mut v = VideoReader::open_scaled(&small, 160).unwrap();
        let mut buf = vec![0u8; 160 * 120 * 4];
        v.seek(0.5).unwrap();
        assert!(
            v.next_frame_bgra_into(&mut buf).unwrap().is_some(),
            "预热 seek 应出帧"
        );

        let timed_seeks = [0.3f64, 1.1f64];
        let base = {
            let t = std::time::Instant::now();
            for &s in &timed_seeks {
                v.seek(s).unwrap();
                assert!(
                    v.next_frame_bgra_into(&mut buf).unwrap().is_some(),
                    "基线 seek {s} 应出帧"
                );
            }
            t.elapsed()
        };

        // 并发：字幕扫描线程先起（当前实现抓全局锁扫完整 415MB），主线程随后跑同款 seek
        let path2 = big.clone();
        let h = std::thread::spawn(move || {
            let r = SubtitleReader::open(&path2);
            let n = r.as_ref().map(|s| s.subtitles().len()).unwrap_or(0);
            // Input 非 Send 时不能跨线程返回 → 线程内销毁（Drop 回锁关闭）
            drop(r);
            n
        });
        // 给扫描 ~80ms 抓锁（持锁态覆盖首次 seek；修复后纯放行不阻塞）
        std::thread::sleep(std::time::Duration::from_millis(80));
        let with = {
            let t = std::time::Instant::now();
            for &s in &timed_seeks {
                v.seek(s).unwrap();
                assert!(
                    v.next_frame_bgra_into(&mut buf).unwrap().is_some(),
                    "并发 seek {s} 应出帧"
                );
            }
            t.elapsed()
        };
        let count = h.join().expect("字幕扫描线程不应死锁");
        assert!(count > 0, "并发提取应得字幕，实得 {count} 条");

        eprintln!(
            "[sub-lock] base_seek={base:?} with_concurrent_scan={with:?} delta={:?}",
            with - base
        );
        // 相对增量红线：并发字幕扫描不得把视频 seek 拖慢超过 +250ms（修复前 ≈ 扫描持锁时长 → FAIL）
        assert!(
            with - base < std::time::Duration::from_millis(250),
            "字幕全文件扫描不应阻塞视频 seek：delta={:?}（应 <250ms）",
            with - base
        );
    }

    /// 并发压力（K-锁 实证纪律）：字幕 open 与视频 seek+解码并行多轮，断言
    /// 不崩、不死锁、每轮 seek 都出帧、扫描都提到字幕——给「扫描移出锁（非锁并发
    /// 纯 demux）安全」的实证依据（K14/K15：平台语义先实证再采信）。
    #[test]
    fn subtitle_scan_concurrent_with_video_seek_stress() {
        let big = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/视频测试字幕.mkv");
        let small = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/video测试.mp4");
        if !big.exists() {
            eprintln!("SKIP: missing target/视频测试字幕.mkv（夹具不入库）");
            return;
        }
        if !small.exists() {
            eprintln!("SKIP: missing target/video测试.mp4（夹具不入库）");
            return;
        }

        let mut v = VideoReader::open_scaled(&small, 160).unwrap();
        let mut buf = vec![0u8; 160 * 120 * 4];
        for round in 0..3 {
            let p = big.clone();
            let h = std::thread::spawn(move || {
                let r = SubtitleReader::open(&p);
                let n = r.as_ref().map(|s| s.subtitles().len()).unwrap_or(0);
                drop(r);
                n
            });
            // 边扫描边做 seek+解码（复用缓冲，让两路真正并行撞期）
            for s in [0.7f64, 1.4f64] {
                v.seek(s).unwrap();
                assert!(
                    v.next_frame_bgra_into(&mut buf).unwrap().is_some(),
                    "stress round {round} seek {s} 应出帧"
                );
            }
            let count = h.join().expect("round {round}: 扫描线程不应死锁");
            assert!(count > 0, "round {round} 应提取字幕，实得 {count} 条");
        }
    }

    /// 日志过滤器（env_facs_q 收敛）：只匹配 HE-AAC SBR 断链坏帧的良性报错，其余
    /// 解码错误（如真实 decode error）必须照常透传。
    #[test]
    fn log_filter_predicate_only_matches_env_facsq() {
        assert!(ffmpeg_log_line_is_benign("[aac @ 0000] env_facs_q 253 is invalid"));
        assert!(ffmpeg_log_line_is_benign("Reinit context to ... env_facs_q 254 is invalid"));
        assert!(!ffmpeg_log_line_is_benign("[h264 @ 0000] decode error"));
        assert!(!ffmpeg_log_line_is_benign("udta parsing failed retrying raw"));
        assert!(!ffmpeg_log_line_is_benign(""));
    }

    /// 过滤器在真实 seek 路径上的落点：打开 415MB MKV 音轨、seek 落点、读到首个
    /// clean 帧——必经解码坏帧丢弃路径（若本机构造触发 `env_facs_q`，计数递增证明
    /// 过滤器挂载生效且被丢弃；不触发则计数不变，路径仍应不 panic、不解码错）。
    #[test]
    fn audio_seek_mkv_env_facsq_filter_path_runs() {
        let mkv = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/视频测试字幕.mkv");
        if !mkv.exists() {
            eprintln!("SKIP: missing target/视频测试字幕.mkv（夹具不入库）");
            return;
        }
        // 前置：挂入过滤回调（quiet 幂等）
        ffmpeg::init().unwrap();
        quiet_ffmpeg_log_once();

        let before = FFMPEG_LOG_FILTERED.load(Ordering::Relaxed);
        let mut a = AudioReader::open_pcm_f32(&mkv, 48000).unwrap();
        let mut decoded = 0usize;
        for target in [35.0f64, 95.0, 142.0] {
            a.seek(target).unwrap();
            // 读到落点后的首个 clean 块（≤ 0.5s 样本）即证明 seek+坏帧丢弃路径走通
            let mut got = 0usize;
            while got < 48000 / 2 {
                if let Ok(Some((_, samples))) = a.next_pcm_f32() {
                    got += samples.len();
                } else {
                    break;
                }
            }
            decoded += got;
        }
        let after = FFMPEG_LOG_FILTERED.load(Ordering::Relaxed);
        eprintln!(
            "[env-facsq] filtered_count before={before} after={after} 解码样本={decoded}"
        );
        assert!(decoded > 0, "MKV 音轨 seek 后应解出音频，实得 {decoded}");
        assert!(after >= before, "过滤计数不倒退");
    }
}
