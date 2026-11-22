//! 硬件解码（D3D11VA / DXVA2 / 外部 D3D11 设备注入）子模块：从 `decode.rs` 按职责拆出。
//!
//! 组合关系：`decode::VideoReader`（模块根）在此打开/注入硬解设备与帧池，逐帧交给本模块
//! 的类型/函数（`try_open_hw_decoder` / `HwFrame` / `HwFramePin` …）消化；FFMPEG_LOCK 串行
//! 纪律在模块根与 lib.rs 持有，本模块只负责 D3D11 设备内部的可重入锁（`D3D11_LOCK`）与
//! 引用计数（`AvBuf` / `AddRefGuard`）。

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::sync::Mutex;

use anyhow::{Result, anyhow};

use ffmpeg_next as ffmpeg;
// 仅外部 D3D11 设备注入子项使用（Interface trait 的 cast 方法）；该子项 cfg(windows) 门控，
// Linux 不编译（Phase 1 Linux 解锁）。
#[cfg(windows)]
use windows::core::Interface as _;

// ---------------- 硬件解码（D3D11VA / DXVA2）----------------

/// FFmpeg `AVBufferRef` 所有权包装：Drop 时 `av_buffer_unref` 释放引用。
pub(crate) struct AvBuf(pub(crate) *mut ffmpeg::ffi::AVBufferRef);

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
pub(crate) struct HardwareDecode {
    /// 仅作 RAII 保活（Drop 时 `av_buffer_unref`）；「读取」即持有引用的副作用。
    #[allow(dead_code)]
    pub(crate) device: AvBuf,
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
pub(crate) fn try_open_hw_decoder(
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
pub(crate) fn is_hw_frame(frame: &ffmpeg::frame::Video) -> bool {
    unsafe {
        // AVFrame.format 在绑定里展开成底层 i32；repr(i32) 枚举按位重建判别值再查描述符。
        let fmt: ffmpeg::ffi::AVPixelFormat =
            std::mem::transmute((*frame.as_ptr()).format);
        let desc = ffmpeg::ffi::av_pix_fmt_desc_get(fmt);
        !desc.is_null() && (*(desc)).flags & (ffmpeg::ffi::AV_PIX_FMT_FLAG_HWACCEL as u64) != 0
    }
}

// ---------------- 外部 D3D11 设备注入（零回读 GPU 直通）----------------
// 本段整块仅 Windows（target 门控依赖 + 各 item cfg(windows)）：引用 windows crate 的
// COM 类型与 ffmpeg-sys 的 D3D11VA 结构体（AVD3D11VADeviceContext/AVD3D11VAFramesContext，
// Windows FFmpeg 绑定专属）。Linux 上该注入函数不编译（Phase 1 Linux 解锁）。

/// D3D11VA 设备上下文的进程级互斥桥（可重入）：[`ffmpeg::ffi::AVD3D11VADeviceContext`] 的
/// `lock`/`unlock` 回调经此互斥量串行化对 `device_context`/`video_context` 的访问。
///
/// FFmpeg 头文件明确要求「底层锁必须可重入」——D3D11VA 解码/帧池操作同一线程内可能嵌套
/// 加锁，而 `std::sync::Mutex` 本身**不可**重入，直接用会自我死锁。故用「线程局部深度计数」
/// 包裹：深度 0→1 时才真正获取互斥量（同线程嵌套只递增深度），不同线程仍互斥
///（进程内全部解码操作已被 [`FFMPEG_LOCK`] 串行化，实际单线程触碰设备，此处仅兜底）。
#[cfg(windows)]
static D3D11_LOCK: Mutex<()> = Mutex::new(());

#[cfg(windows)]
std::thread_local! {
    /// 当前线程进入 D3D11 设备锁的嵌套深度（0 = 未持锁）。
    static D3D11_LOCK_DEPTH: Cell<u32> = const { Cell::new(0) };
    /// 深度 0→1 时真正持有的互斥量守卫（Drop 即释放；仅持锁线程访问）。
    static D3D11_LOCK_GUARD: RefCell<Option<std::sync::MutexGuard<'static, ()>>> =
        const { RefCell::new(None) };
}

/// D3D11 设备锁回调（extern "C" 内禁止 panic —— unwind 跨 ABI 会 abort，一律取回中毒锁兜底）。
#[cfg(windows)]
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
#[cfg(windows)]
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
#[cfg(windows)]
struct AddRefGuard {
    unk: Option<windows::core::IUnknown>,
}

#[cfg(windows)]
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

#[cfg(windows)]
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
/// `shader_resource=true` 时给帧纹理挂 `D3D11_BIND_SHADER_RESOURCE`（仅供
/// [`d3d11_supports_shader_bound_textures`] 能力探测用）；**生产解码路径恒传 `false`**
///（DECODER-only，见 [`VideoReader::open_external_hw`] 文档）——Arc 等驱动拒绝
/// `D3D11_BIND_DECODER | D3D11_BIND_SHADER_RESOURCE` 的 NV12 数组纹理（`CreateTexture2D`
/// E_INVALIDARG），而拷贝源只需 `USAGE_DEFAULT`、渲染只采样共享槽，故无需 SHR。
///
/// 返回 `AvBuf` 已持有对设备上下文 `AVBufferRef` 的引用（含 uninit 时的接口释放），
/// 调用方（[`VideoReader::open_external_hw`]）再 `av_buffer_ref` 一份挂给解码器。
#[cfg(windows)]
pub(crate) fn build_external_d3d11_device_ctx(
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
        // 可重入互斥桥（字段类型为 `*mut libc::c_void`，与 std::ffi::c_void 布局一致，
        // 锁本身就是进程级全局静态量）。
        (*hwctx).lock = Some(d3d11_lock_cb as unsafe extern "C" fn(*mut std::ffi::c_void));
        (*hwctx).unlock = Some(d3d11_unlock_cb as unsafe extern "C" fn(*mut std::ffi::c_void));
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

/// 预检外部注入的 D3D11 设备是否**真正具备 D3D11VA 视频解码能力**（能否解 NV12）。
///
/// 动机（真实崩溃根因，2026-11-10）：在「标称硬件、但无真实 D3D11VA 解码」的适配器
///（本机 = Microsoft Basic Display / VM 虚拟 GPU；`D3D11CreateDevice(HARDWARE)` 能成功建出
/// 设备、却不具备可用的视频解码能力）上，把该设备直接交给 FFmpeg 建外部 D3D11VA 解码器/
/// 帧池会在 `d3d11.dll` 内 `STATUS_ACCESS_VIOLATION`（本仓 `open_external_hw` 复现，
/// d3d11.dll+0x5D3EE 与 WARP 工作线程故障 IP 交替出现内存越界；`media_ffmpeg` 的
/// `hw_external_*` 测试也同源崩溃）。FFmpeg 自建设备的路径会在其内部探测到不支持并优雅回
/// 软解，唯独**注入外部设备**没有这层探测、直接建解码器帧池 → AV。
///
/// 预检只走 D3D11 标准 COM 调用（`ID3D11VideoDevice` 的 profile/format 查询，不碰 FFmpeg、
/// 不碰解码器、不建帧池），失败即视为无能力；真实 GPU 上预检通过、注入解码行为不变，
/// 不通过 → 调用方回落到 SAFE BGRA 硬解/软解（本机实证存活）。
#[cfg(windows)]
pub(crate) fn device_supports_d3d11va(device_raw: *mut std::ffi::c_void) -> bool {
    use windows::core::Interface as _;
    use windows::Win32::Graphics::Direct3D11::ID3D11VideoDevice;
    use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_NV12;
    if device_raw.is_null() {
        return false;
    }
    // edition 2024 下 `let Ok(..) = unsafe { .. } else` 非法：先求值再 let-else
    let unk = unsafe { windows::core::IUnknown::from_raw_borrowed(&device_raw) };
    let Some(unk) = unk else {
        return false;
    };
    let cast = unk.cast::<ID3D11VideoDevice>();
    let Ok(video) = cast else {
        return false;
    };
    // 遍历全部解码 profile，任一个能解 NV12（软解帧/FFmpeg D3D11VA 输出的标准格式）即支持
    let count = unsafe { video.GetVideoDecoderProfileCount() };
    for index in 0..count {
        let prof = unsafe { video.GetVideoDecoderProfile(index) };
        let Ok(profile) = prof else {
            continue;
        };
        let ok = unsafe { video.CheckVideoDecoderFormat(&profile, DXGI_FORMAT_NV12) };
        if let Ok(supported) = ok
            && supported.as_bool()
        {
            return true;
        }
    }
    false
}

/// 诊断/测试用探测：驱动是否支持「D3D11VA 解码数组纹理带 SHADER_RESOURCE」——用**带 SHR**
/// 的设备上下文建一个最小帧池（NV12 数组 ×4，BindFlags = `DECODER | SHADER_RESOURCE`，与
/// libavcodec D3D11VA frame_params 一致）跑 `av_hwframe_ctx_init`：成功 = 支持；失败
///（`CreateTexture2D` E_INVALIDARG，实测 Intel Arc 32.0.101.x 等驱动拒绝该组合）=
/// 不支持。
///
/// **NOT 直通门控**：双设备直通不再依赖本探测——解码器配置恒 DECODER-only（见
/// [`VideoReader::open_external_hw`] 的文档），渲染端只采样共享槽。本函数仅供
/// `hw_external_tests` 一致性检查与主程序能力自诊断（FFI
/// [`vff_d3d11_supports_shader_bound_textures`]）使用，结果只反映驱动约束、不影响解码路径。
/// 纯 device/frames 上下文、**不开解码器** → 失败无副作用。
#[cfg(windows)]
pub(crate) fn d3d11_supports_shader_bound_textures(
    device: *mut std::ffi::c_void,
    device_context: *mut std::ffi::c_void,
) -> bool {
    // 先预检设备是否具备基础 D3D11VA 解码能力：无能力适配器（本机 Basic Display）上直接
    // 建 SHR 帧池同样会在 d3d11.dll 内 AV（与 open_external_hw 同源崩溃）——提前判不支持。
    if !device_supports_d3d11va(device) {
        return false;
    }
    use ffmpeg::ffi::{AVD3D11VAFramesContext, AVHWFramesContext};
    let Ok(dev) = build_external_d3d11_device_ctx(device, device_context, true) else {
        return false;
    };
    let ok = unsafe {
        let mut frames =
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
            ffmpeg::ffi::av_buffer_unref(&mut frames);
            rc >= 0
        }
    };
    drop(dev);
    ok
}

/// Intel Arc 驱动 teardown 兼容护栏：在外部注入的 D3D11 设备/上下文上预先跑一次最小的
/// **DECODER-only** `AVHWFramesContext` 帧池初始化（成功即弃、不开解码器）。
///
/// 实测发现（本机 Arc 32.0.101.8861，`hw_external_*` 套件）：`open_external_hw` 若从未在
/// 注入设备上 init 过任何 `AVHWFramesContext`，窗口释放 `ID3D11DeviceContext` 时**偶发**
/// 0xc0000005（drop(reader) 正常、最后 context Release AV）。机制/根因未完全定位——引用
/// 计数上探测/预热的起止恒平衡，且正常解码的 reader 首帧也会建帧池，故不构成
/// 「未 init 必崩」的因果陈述；此处如实按经验记录。旧实现里的「带 SHR 失败帧池探测」
///（[`d3d11_supports_shader_bound_textures`]）恰也包含一次 `av_hwframe_ctx_init`，曾被误作
/// 直通门控必需；废除门控后由本护栏承接同一「预跑一次帧池初始化」职责：DECODER-only
/// 构建通常成功（无失败日志噪音），仅首次打开时一次、热路径零成本，其它驱动无害。
///
/// R2-3：接收**已经构建好**的设备 ctx（`&AvBuf`，调用方 [`decode::VideoReader::open_external_hw`]
/// 与真实解码路径共用），不再内部二次 `build_external_d3d11_device_ctx`——每次 open 对同一
/// D3D11 设备/上下文的双重 alloc/init/AddRef/Release 消除。帧池上下文经 `av_buffer_ref` 自持
/// 另一份引用、用完即 `av_buffer_unref`，不改 `dev` 的引用余额；同线程、同设备类型，符合
/// K22/K23（各线程独享 context、AVBufferRef 引用计数正确）。
#[cfg(windows)]
pub(crate) fn warm_external_d3d11_frames_pool_with(dev: &AvBuf) {
    use ffmpeg::ffi::{AVD3D11VAFramesContext, AVHWFramesContext};
    unsafe {
        let mut frames = ffmpeg::ffi::av_hwframe_ctx_alloc(ffmpeg::ffi::av_buffer_ref(dev.0));
        if frames.is_null() {
            return;
        }
        let fc = (*frames).data as *mut AVHWFramesContext;
        (*fc).format = ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_D3D11;
        (*fc).sw_format = ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_NV12;
        (*fc).width = 1280;
        (*fc).height = 720;
        // `initial_pool_size` 必须取小：本机 Arc 实测「单张 NV12 数组纹理」池 ≥4 时
        // `CreateTexture2D` E_INVALIDARG（0x80070057，曾污染每次 open 的 stderr），≤2
        // 成功（探针对照 pool=1/2 → rc=0、pool=4/20 → E_INVALIDARG）。本护栏是一次性的
        // teardown 兼容预跑、池即建即弃，最小池足够；解码器的真实帧池由 libavcodec 在
        // 首帧按其自身参数另建，与此无关。
        (*fc).initial_pool_size = 2;
        let fhw = (*fc).hwctx as *mut AVD3D11VAFramesContext;
        (*fhw).BindFlags = 0x20; // DECODER only（必成功）
        let _ = ffmpeg::ffi::av_hwframe_ctx_init(frames);
        ffmpeg::ffi::av_buffer_unref(&mut frames);
    }
}

/// 单帧 HW 帧的 `AVBufferRef` 引用钉：把解码帧的 AVBufferRef 引用复制一份私有持有，
/// 帧池槽位（D3D11 数组纹理元素）在被引用期间不归还解码器；Drop 时 `av_frame_unref` +
/// `av_frame_free` 释放该引用 → 槽位归还帧池复用（原子计数，任意线程可释放）。
///
/// `*mut AVFrame` 是 raw 指针（[`HwFramePin`] 显式 `unsafe impl Send`），整个结构因此可
/// 跨线程 —— 与 [`HwFrame`] 一起从解码 worker 线程安全移动到渲染线程/FFI 释放侧。
pub(crate) struct HwFramePin {
    /// 私有帧（`av_frame_alloc` + `av_frame_ref` 结果的持有者）。
    #[allow(dead_code)] // 仅 Drop 时经 unref/free 释放；字段本身不读
    pub(crate) frame: *mut ffmpeg::ffi::AVFrame,
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
    pub(crate) pin: Box<dyn Any + Send>,
}
