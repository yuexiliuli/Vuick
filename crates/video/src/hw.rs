//! 双设备 GPU 直通（零回读）的 worker 侧 D3D11 引擎。
//!
//! # 为什么是「双设备」
//! 单设备共享 immediate context 的跨线程做法会真机崩溃（exit 2173，K22）；
//! deferred context 跨线程拷贝在本栈有数据竞争与音画错位（AV）。双设备是标准的线程安全
//! 姿势：解码 + 拷贝工作在 **worker 自有**的 `ID3D11Device/ID3D11DeviceContext`（线程 A）
//! 上完成，帧纹理经 **shared handle + keyed mutex**（`D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX`，
//! 无 CPU 回读、无 sws、无共享上下文）移动到 gpui 渲染设备（线程 B）采样。
//!
//! # 数据通路
//! ```text
//! worker 线程（设备 A，自有 immediate ctx）          渲染/UI 线程（gpui 设备 B）
//!   decode: open_hw(path, deviceA, ctxA)               paint_external_yuv(shared_handle)
//!   hw frame (tex, array_index)                          external_views(handle):
//!   copy: AcquireSync(0) → CopySubresourceRegion           OpenSharedHandle on B →
//!         → Flush() → ReleaseSync(0)                          AcquireSync(0) → 建两平面 SRV
//!   → send (pts,w,h,shared_handle)                          draw (Y t0 / UV t2 / instance t1)
//!   main 归还槽 → recycle                                   → ReleaseSync(0) 帧末
//! ```
//!
//! # 生命周期纪律
//! 目标共享纹理由 **worker 线程池**创建/持有（[`SharedCopier`] 的 `pool`），跨线程只传
//! `shared_handle`（DXGI 资源句柄数值）；主线程用后经释放消息把槽回池复用（
//! [`SharedCopier::recycle`]）。句柄本身不携带所有权——渲染侧的打开引用（OpenSharedHandle）
//! 由 gpui_windows 的 `external_views` 缓存持有，纹理在 worker 池 Drop/clear 前不会失效。
//!
//! 本模块另提供测试自建设备（硬件 → WARP 兜底）与 `record_hw_paint` 测试探针，供
//! `#[gpui::test]` 在无真实 GUI 时走完整双设备链路（见 viewer 的
//! `gpu_passthrough_first_frame_paints_shared_handle`）。

use windows::core::Interface as _;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_FLAG, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, ID3D11Device,
    ID3D11DeviceContext, ID3D11Texture2D, ID3D11VideoDevice,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{IDXGIKeyedMutex, IDXGIResource};
#[cfg(test)]
use std::sync::atomic::Ordering;

/// 键控互斥获取超时（ms）：正常时渲染端每帧末 ReleaseSync，worker 的 acquire 基本零等待。
/// 用有限超时而非 INFINITE——若渲染端因设备丢失/异常未按时释放，worker 不会被永久卡死，
/// 超时后把该槽重建并报错回退到 SAFE BGRA 路径（K22 教训：跨线程 GPU 姿势先锁死风险）。
const ACQUIRE_TIMEOUT_MS: u32 = 2_000;

/// worker 线程自有的 D3D11 设备（含 immediate context）。**只在该线程使用**。
pub struct WorkerD3D {
    /// `ID3D11Device`（worker 线程独享）。
    pub device: ID3D11Device,
    /// `ID3D11DeviceContext`（worker 线程独享）。
    pub context: ID3D11DeviceContext,
    /// 驱动名（"hardware" / "warp"），诊断打印用（cfg(test) 测试打印；非测试构建零读）。
    #[allow(dead_code)]
    pub kind: &'static str,
}

/// 建 worker 设备：硬件驱动优先、WARP 兜底；两者都不可用 → `None`（调用方回退 SAFE 路径）。
/// 与 `media-ffmpeg` 的 `hw_external_tests` 同款姿势（生产可调，非 cfg(test)）。
pub fn create_worker_d3d11_device() -> Option<WorkerD3D> {
    fn attempt(driver: D3D_DRIVER_TYPE) -> Option<(ID3D11Device, ID3D11DeviceContext)> {
        let adapter: Option<&windows::Win32::Graphics::Dxgi::IDXGIAdapter> = None; // null adapter = 按 driver type 自选
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        let hr = unsafe {
            D3D11CreateDevice(
                adapter,
                driver,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_FLAG(D3D11_CREATE_DEVICE_VIDEO_SUPPORT.0 as u32),
                None, // 全部 feature level
                D3D11_SDK_VERSION,
                Some(&mut device),
                None, // 不取 feature level
                Some(&mut context),
            )
        };
        if hr.is_err() || device.is_none() || context.is_none() {
            return None;
        }
        Some((device.unwrap(), context.unwrap()))
    }

    if let Some(d) = attempt(D3D_DRIVER_TYPE_HARDWARE) {
        return Some(WorkerD3D { device: d.0, context: d.1, kind: "hardware" });
    }
    if let Some(d) = attempt(D3D_DRIVER_TYPE_WARP) {
        return Some(WorkerD3D { device: d.0, context: d.1, kind: "warp" });
    }
    None
}

/// 预检 worker D3D11 设备是否**真正具备 D3D11VA 视频解码能力**（能否解 NV12）：双设备 GPU
/// 直通把该设备注入 FFmpeg `open_hw` 之前调用。
///
/// 动机（真实崩溃根因，2026-11-10）：在「标称硬件、但无真实 D3D11VA 解码」的适配器
///（本机 = Microsoft Basic Display / VM 虚拟 GPU；`D3D11CreateDevice(HARDWARE)` 能成功建出
/// `kind="hardware"` 的设备，却不具备可用的视频解码能力）上，直接把该设备注入 FFmpeg 的
/// 外部 D3D11VA 解码器初始化会在 `d3d11.dll` 内崩溃（`STATUS_ACCESS_VIOLATION`，
/// 本仓 `video::decode::open_hw` FFI 处复现，d3d11.dll+0x5D3EE 与故障 IP 交替出现内存越界）。
/// FFmpeg 自建设备的路径会在其内部探测到不支持并优雅回软解，唯独**注入外部设备**没有这层
/// 探测、直接建解码器帧池 → AV。
///
/// 预检只走 D3D11 标准 COM 调用（`ID3D11VideoDevice` 的 profile/format 查询，不碰 FFmpeg、
/// 不碰解码器、不建帧池），失败即视为无能力；真实 GPU 上该预检通过、双设备直通行为不变，
/// 不通过 → 上层回落到 SAFE BGRA 硬解/软解（本机实证存活）。
pub(crate) fn worker_d3d11va_supported(device: &ID3D11Device) -> bool {
    use windows::core::Interface as _;
    let Ok(video) = device.cast::<ID3D11VideoDevice>() else {
        // 设备不暴露 D3D11VideoDevice = 该项能力缺失，直接判不支持
        return false;
    };
    // 遍历全部解码 profile，任一个能解 NV12（软解帧/FFmpeg D3D11VA 输出的标准格式）即支持
    let count = unsafe { video.GetVideoDecoderProfileCount() };
    for index in 0..count {
        let Ok(profile) = (unsafe { video.GetVideoDecoderProfile(index) }) else {
            continue;
        };
        if let Ok(supported) =
            unsafe { video.CheckVideoDecoderFormat(&profile, DXGI_FORMAT_NV12) }
        {
            if supported.as_bool() {
                return true;
            }
        }
    }
    false
}

/// 一帧解码帧拷进共享 NV12 槽后的产物（跨线程只传 `shared_handle` 数值）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SharedFrame {
    /// DXGI 共享句柄（usize 数值；渲染端 `OpenSharedResource` 用它恢复纹理）。
    pub shared_handle: usize,
    /// 拷贝后的显示宽。
    pub w: u32,
    /// 拷贝后的显示高。
    pub h: u32,
}

/// 一次 `SharedCopier::copy` 失败的原因分类——worker 据此区分「可重试吸收的背压」与
/// 「必须走软重开恢复的真实失败」：
/// - [`CopyError::Backpressure`]：共享槽池瞬满/获取互斥超时等渲染端瞬时滞后 → 重试同一帧吸收，
///   **永不**计入设备丢失计数；
/// - [`CopyError::Unsupported`]：解码帧格式非 NV12（10-bit P010 等深色格式）——该文件不走
///   GPU 直通，按「不支持直通」走黑名单 + 软重开恢复（软回退决策在 worker，不在本方法）；
/// - [`CopyError::Sync`] / [`CopyError::Resource`]：键控互斥/驱动器创建等设备侧故障 → 预算内
///   可重试，预算耗尽按设备丢失计数（软重开兜底）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyError {
    /// 共享槽池瞬满（槽池耗尽 / 尺寸不匹配且池满）——渲染端未及时归还槽，**可重试**（背压 ≠ 设备丢失）。
    Backpressure,
    /// 解码帧格式非 NV12（10-bit P010 等）——该文件不支持 GPU 直通，应软重开。
    Unsupported,
    /// AcquireSync/ReleaseSync 键控互斥失败（含超时）——渲染端/设备侧故障，预算内可重试。
    Sync,
    /// 共享 NV12 目标纹理创建失败（驱动不支持）——本机/当前尺寸直通不可用，应软重开。
    Resource,
}

/// 池中一块共享 NV12 目标纹理（worker 线程持有 COM 引用；`raw_handle` 供跨线程传递）。
struct SharedSlot {
    /// DXGI 共享句柄（数值）。
    raw_handle: usize,
    /// 池持有的纹理引用（Drop/clear 时统一释放）。
    tex: ID3D11Texture2D,
    /// 键控互斥（与 `tex` 同一对象 QI 出的接口；AcquireSync/ReleaseSync 用它）。
    mutex: IDXGIKeyedMutex,
    w: u32,
    h: u32,
    /// true = 已交给主线程、待回收。
    in_use: bool,
}

/// 双设备直通的 worker 侧拷贝引擎：把解码帧（D3D11VA 数组纹理的一个阵元）GPU 侧拷进
/// **共享** NV12 目标纹理（键控互斥保护跨设备访问），再 `ReleaseSync` 让渲染设备可取。
///
/// 生命周期：`copy` 在池里挑空闲且尺寸匹配的槽，否则新建（`max_pool` 封顶）；`recycle`
/// 由 worker 在收到主线程槽归还消息时把槽标记回空闲；转换/换代/切文件时 `clear` 统一
/// 释放全部槽的 COM 引用。设备/上下文始终在 **worker 线程**内使用（K22：各线程独享上下文）。
pub struct SharedCopier {
    /// `ID3D11Device`（worker 线程独享；与解码 reader 共用同一设备）。
    device: ID3D11Device,
    /// `ID3D11DeviceContext`（worker 线程独享）。
    context: ID3D11DeviceContext,
    /// 共享 NV12 目标纹理池（`in_use` = 已交给主线程、待回收）。
    pool: Vec<SharedSlot>,
    /// 池封顶（同时存活的目标纹理数；有界通道 2 槽 + 主线程当前帧 + 在途 ≈ ≤4-6）。
    max_pool: usize,
}

impl SharedCopier {
    /// 以 worker 自有设备/上下文新建拷贝引擎（取得设备所有权；调用方不再另行释放）。
    pub fn new(worker: WorkerD3D) -> Self {
        Self {
            device: worker.device,
            context: worker.context,
            pool: Vec::new(),
            // 渲染端滞后突发（seek/拖拽瞬间共享槽瞬满）的容差余量：8 → 16。每 1280×720
            // NV12 槽 ≈1.4MB、受单个 worker 上界约束；余量让「拷贝背压重试 + reap 回收」
            // 有喘息空间，不因槽池瞬满触发黑名单式软降级（背压 ≠ 设备丢失，见 workers 重试）
            max_pool: 16,
        }
    }

    /// 在池中取（或新建）一块**尺寸匹配且未在途**的共享 NV12 目标纹理，把解码帧
    /// `src_tex` 的第 `array_index` 个阵元 GPU 侧拷进该纹理并 `Flush` + `ReleaseSync`，
    /// 返回共享句柄（跨线程传递给主线程的唯一载体；主线程用后经释放消息回池）。
    ///
    /// 失败 → [`CopyError`]：背压（[`CopyError::Backpressure`]）由 worker 重试同一帧吸收；
    /// 格式不支持（[`CopyError::Unsupported`]）与设备侧故障（[`CopyError::Sync`] /
    /// [`CopyError::Resource`]）按 worker 的「设备丢失」逻辑做黑名单 + 软重开恢复（软回退
    /// 决策在 worker，本方法不再承诺就地回退软解）。任何失败路径都把槽**还回空闲**
    ///（防 in_use 永久占死池位 → 池越用越少）。
    pub fn copy(
        &mut self,
        w: u32,
        h: u32,
        src_tex: usize,
        array_index: u32,
    ) -> Result<SharedFrame, CopyError> {
        // 测试钩子：把接下来若干次 copy 强制报「槽回压 / 格式不支持」（模拟渲染端未及时归还槽
        // → 槽池瞬满；或 P010 等深色格式），供回归断言拷贝错误分类；生产构建编译期折叠为零开销
        #[cfg(test)]
        {
            let remaining = COPY_FAIL_REMAINING.load(Ordering::Relaxed);
            if remaining > 0 {
                COPY_FAIL_REMAINING.store(remaining - 1, Ordering::Relaxed);
                COPY_FAIL_FIRED.store(true, Ordering::Relaxed);
                return Err(CopyError::Backpressure);
            }
            let unsupported = COPY_UNSUPPORTED_REMAINING.load(Ordering::Relaxed);
            if unsupported > 0 {
                COPY_UNSUPPORTED_REMAINING.store(unsupported - 1, Ordering::Relaxed);
                COPY_UNSUPPORTED_FIRED.store(true, Ordering::Relaxed);
                return Err(CopyError::Unsupported);
            }
        }
        let idx = self.ensure_slot(w, h)?;
        let out: Result<SharedFrame, CopyError> = (|| {
            let slot = &self.pool[idx];
            unsafe {
                // 键控互斥获取：客户端写入完成并 ReleaseSync 前阻塞；超时不 INFINITE（见模块头）
                slot
                    .mutex
                    .AcquireSync(0, ACQUIRE_TIMEOUT_MS)
                    .map_err(|_| CopyError::Sync)?;

                // 解码器数组纹理借用包装（不 AddRef；用后 forget，引用由 FFmpeg 帧池管理）
                let src = ID3D11Texture2D::from_raw(src_tex as *mut _);
                let mut src_desc = std::mem::zeroed::<D3D11_TEXTURE2D_DESC>();
                src.GetDesc(&mut src_desc);
                if src_desc.Format != DXGI_FORMAT_NV12 {
                    // 源格式不被 GPU 直通支持（10-bit P010 等）：软回退决策由 worker 负责
                    //（黑名单 + 软重开），本方法只报告「不支持直通」，不再就地承诺回退软解
                    std::mem::forget(src);
                    let _ = slot.mutex.ReleaseSync(0);
                    return Err(CopyError::Unsupported);
                }
                debug_assert!(
                    array_index < src_desc.ArraySize,
                    "D3D11VA 帧数组下标越界: idx={array_index} ArraySize={}",
                    src_desc.ArraySize
                );

                // NV12 数组阵元 = **单一子资源**（D3D11CalcSubresource 无平面项；MipLevels=1 时
                // SrcSubresource 即 ArraySlice）——一次 `CopySubresourceRegion` 拷一**整**阵元的
                // Y+UV 两平面（与 FFmpeg d3d11va `av_hwframe_transfer_data` / mpv hwdec_d3d11va
                // 同款姿势；源 BindFlags DECODER 与否无关，源 USAGE_DEFAULT 即可作拷贝源）。
                // 实测本机 Intel Arc：错误的 2 调用「2i/2i+1 平面拆开」会让号 1 阵元读到号 0
                // 数据（OOB 子资源下标被驱动别名回 0）——直通画面错帧的直接成因。正确姿势即
                // 上面的**单调用整阵元拷贝**（D3D11 平面数组阵元 = 单一子资源，与 FFmpeg d3d11va
                // / mpv hwdec_d3d11va 同款；见 hw::tests::test_shared_copier_array_pixel_roundtrip）。
                let y_box = windows::Win32::Graphics::Direct3D11::D3D11_BOX {
                    left: 0,
                    top: 0,
                    front: 0,
                    right: w,
                    bottom: h,
                    back: 1,
                };
                self.context.CopySubresourceRegion(
                    &slot.tex,
                    0, // 目标 = 单切片共享框的**唯一**子资源 0
                    0,
                    0,
                    0,
                    &src,
                    array_index, // 源 = 数组纹理第 array_index 个阵元（整阵元含 Y+UV）
                    Some(&y_box),
                );
                // 提交到 GPU 队列（不等待完成；随后 ReleaseSync 保证渲染端 acquire 到的是已
                // 提交的写入——DXGI 键控互斥 release 在已提交工作完成后才被观测到）
                self.context.Flush();
                // 借用包装**先 forget** 再 ReleaseSync：后者失败走 `?` 早退时 src 已不 Drop
                //（from_raw 不 AddRef，若在此 Drop 会让借用 COM 引用计数下溢）
                std::mem::forget(src);
                // 交还互斥：渲染端可 acquire 本槽
                slot
                    .mutex
                    .ReleaseSync(0)
                    .map_err(|_| CopyError::Sync)?;
            }
            let raw_handle = self.pool[idx].raw_handle;
            Ok(SharedFrame {
                shared_handle: raw_handle,
                w,
                h,
            })
        })();
        if out.is_err() {
            // 拷贝失败（背压/格式/设备侧）把槽**还回空闲**：留在 in_use=true 会永久占死一块
            // 池位、池越用越少——背压重试后仍失败由上层按分类跳过本帧或软重开（不消费槽）
            self.pool[idx].in_use = false;
        }
        out
    }

    /// 主线程归还的共享槽标记回空闲（供下一帧复用）；未知句柄（换代/切文件后）安全 no-op。
    pub fn recycle(&mut self, shared_handle: usize) {
        if let Some(slot) = self.pool.iter_mut().find(|s| s.raw_handle == shared_handle) {
            slot.in_use = false;
        }
    }

    /// 清空池（转换/换代/切文件统一释放全部槽的 COM 引用）。
    #[allow(dead_code)] // worker 换代/切文件入口；当前实现在换代时随 active 整体 drop（等价）
    pub fn clear(&mut self) {
        self.pool.clear();
    }

    /// 取（或新建）一块空闲且尺寸匹配的共享槽下标（无则新建进池，池满则复用最旧空闲并
    /// 重建为新尺寸）。COM 引用由池持有（生命期随池）。
    fn ensure_slot(&mut self, w: u32, h: u32) -> Result<usize, CopyError> {
        if let Some((idx, slot)) = self
            .pool
            .iter_mut()
            .enumerate()
            .find(|(_, s)| !s.in_use && s.w == w && s.h == h)
        {
            slot.in_use = true;
            return Ok(idx);
        }
        if self.pool.len() < self.max_pool {
            let slot = create_shared_slot(&self.device, w, h).map_err(|_| CopyError::Resource)?;
            self.pool.push(slot);
            return Ok(self.pool.len() - 1);
        }
        // 池满且无尺寸匹配：换掉最旧空闲的一块（尺寸必然不同）——重建为当前尺寸；若无任何
        // 空闲槽（全在途）＝渲染端未及时归还 → 背压（可重试，≠ 设备丢失）
        let (idx, _) = self
            .pool
            .iter_mut()
            .enumerate()
            .find(|(_, s)| !s.in_use)
            .ok_or(CopyError::Backpressure)?;
        let slot = create_shared_slot(&self.device, w, h).map_err(|_| CopyError::Resource)?;
        self.pool[idx] = slot;
        Ok(idx)
    }
}

/// 建一块**共享**的 SRV-capable NV12 目标纹理（DEFAULT 用法、单数组切片、SHADER_RESOURCE
/// 绑定 + 键控互斥），并取回其 DXGI 共享句柄与键控互斥接口。
///
/// 共享标记组合：实测本机（含 Intel Arc 硬件）`D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX`
/// **单独设置**（0x100，已隐含共享 + 关联键控互斥）可用；再叠加 `D3D11_RESOURCE_MISC_SHARED`
/// （0x2）反而 `E_INVALIDARG`。少数驱动可能只认叠加形式——从「仅 KEYEDMUTEX」到
/// 「KEYEDMUTEX|SHARED」逐一尝试，都失败才 `Err`（上层据此回退 SAFE 路径）。
fn create_shared_slot(
    device: &ID3D11Device,
    w: u32,
    h: u32,
) -> anyhow::Result<SharedSlot> {
    use windows::Win32::Graphics::Direct3D11::D3D11_RESOURCE_MISC_FLAG;
    let combos = [
        D3D11_RESOURCE_MISC_FLAG(0x100), // D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX
        D3D11_RESOURCE_MISC_FLAG(0x100) | D3D11_RESOURCE_MISC_FLAG(0x2), // + D3D11_RESOURCE_MISC_SHARED
    ];
    let mut texture: Option<ID3D11Texture2D> = None;
    let mut created: Option<ID3D11Texture2D> = None;
    for &misc in &combos {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: misc.0 as u32,
        };
        unsafe {
            let _ = device.CreateTexture2D(&desc, None, Some(&mut texture));
        }
        if let Some(t) = texture.take() {
            created = Some(t);
            break;
        }
    }
    let texture = created.ok_or_else(|| {
        anyhow::anyhow!("共享 NV12 目标纹理创建失败（驱动不支持 keyed-mutex 共享平面纹理）")
    })?;
    // DXGI 共享句柄（渲染端 OpenSharedResource 的载体）
    let dxgi: IDXGIResource = texture.cast().map_err(|_| {
        anyhow::anyhow!("共享纹理 QueryInterface IDXGIResource 失败")
    })?;
    let handle = unsafe { dxgi.GetSharedHandle()? }.0 as usize;
    // 键控互斥（同一对象的接口；AcquireSync/ReleaseSync 保护跨设备读写）
    let mutex = texture.cast().map_err(|_| {
        anyhow::anyhow!("共享纹理 QueryInterface IDXGIKeyedMutex 失败")
    })?;
    Ok(SharedSlot {
        raw_handle: handle,
        tex: texture,
        mutex,
        w,
        h,
        in_use: true,
    })
}

// ---------------- 测试探针（同旧 M2e 姿势） ----------------

/// 测试探针：记录最近一次 canvas paint 里 `paint_external_yuv` 收到的共享句柄。
/// 生产构建为 no-op（零开销）；test 构建由 `#[gpui::test]` 读 [`take_hw_paint_probe`]，
/// 断言「GPU 帧真的走进了外部 YUV 精灵的绘制路径」。0 = 尚未绘制过。
pub fn record_hw_paint(shared_handle: usize) {
    #[cfg(test)]
    HW_PAINT_PROBE.with(|p| *p.borrow_mut() = shared_handle);
    #[cfg(not(test))]
    {
        let _ = shared_handle;
    }
}

#[cfg(test)]
thread_local! {
    static HW_PAINT_PROBE: std::cell::RefCell<usize> = const { std::cell::RefCell::new(0) };
}

/// 取走最近一次 `paint_external_yuv` 收到的共享句柄（0 = 尚未绘制过）。
#[cfg(test)]
pub fn take_hw_paint_probe() -> usize {
    HW_PAINT_PROBE.with(|p| p.replace(0))
}

/// 测试专用：把接下来 N 次 `SharedCopier::copy` 强制报「槽回压」错误（模拟渲染端未及时归还
/// 共享槽 → 槽池瞬满 / 获取互斥超时），供回归断言「拷贝背压 ≠ 设备丢失：不黑名单、不重开软解」。
/// 生产零字段零开销（cfg(test) 折叠）。
#[cfg(test)]
static COPY_FAIL_REMAINING: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(-1);
/// 注入已实际触发过至少一次（测试等待把「错误已发生」与「后续恢复/断言」解耦）。
#[cfg(test)]
static COPY_FAIL_FIRED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// 测试专用：把接下来 N 次 `SharedCopier::copy` 强制报「格式不支持」（模拟 P010 等 10-bit
/// 深色源），供回归断言「Unsupported ≠ 背压：按设备丢失走黑名单 + 软重开恢复」。
#[cfg(test)]
static COPY_UNSUPPORTED_REMAINING: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(-1);
/// 注入已实际触发过至少一次（同 `COPY_FAIL_FIRED` 解耦语义）。
#[cfg(test)]
static COPY_UNSUPPORTED_FIRED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 测试专用：让接下来 `n` 次 `SharedCopier::copy` 失败（n 次后自动撤销）。配合调用方 RAII
/// 守卫（Drop 复位）保证任何退出路径都不把注入泄漏给后续测试。
#[cfg(test)]
pub fn copy_fail_for(n: u32) {
    COPY_FAIL_REMAINING.store(n as i64, Ordering::Relaxed);
    COPY_FAIL_FIRED.store(false, Ordering::Relaxed);
}

/// 测试专用：让接下来 `n` 次 `SharedCopier::copy` 强制报「格式不支持」（n 次后自动撤销）。
#[cfg(test)]
pub fn copy_unsupported_for(n: u32) {
    COPY_UNSUPPORTED_REMAINING.store(n as i64, Ordering::Relaxed);
    COPY_UNSUPPORTED_FIRED.store(false, Ordering::Relaxed);
}

/// 测试专用：撤销拷贝失败注入（背压与格式不支持一并撤销）。
#[cfg(test)]
pub fn copy_fail_disable() {
    COPY_FAIL_REMAINING.store(-1, Ordering::Relaxed);
    COPY_UNSUPPORTED_REMAINING.store(-1, Ordering::Relaxed);
}

/// 测试专用：注入是否已实际触发过至少一次。
#[cfg(test)]
pub fn copy_fail_fired() -> bool {
    COPY_FAIL_FIRED.load(Ordering::Relaxed)
}

/// 测试专用：格式不支持注入是否已实际触发过至少一次。
#[cfg(test)]
pub fn copy_unsupported_fired() -> bool {
    COPY_UNSUPPORTED_FIRED.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::c_void;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_CPU_ACCESS_READ, D3D11_CPU_ACCESS_WRITE, D3D11_MAP_READ, D3D11_MAP_WRITE,
        D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
    };
    use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;

    // 16×8 NV12（WARP 对过小尺寸/非 4 倍宽的平面纹理可能 E_INVALIDARG，取标准倍数）：
    // Y 平面 = 16×8 字节；UV 平面 = 8×4×2 = 64 字节。
    const W: u32 = 16;
    const H: u32 = 8;

    /// 建测试 NV12 纹理（同旧 M2e `make_nv12` 姿势）。
    fn make_nv12(
        device: &ID3D11Device,
        usage: windows::Win32::Graphics::Direct3D11::D3D11_USAGE,
        cpu: u32,
        array: u32,
    ) -> ID3D11Texture2D {
        use windows::Win32::Graphics::Direct3D11::D3D11_BIND_SHADER_RESOURCE;
        let desc = D3D11_TEXTURE2D_DESC {
            Width: W,
            Height: H,
            MipLevels: 1,
            ArraySize: array,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: usage,
            BindFlags: if usage == D3D11_USAGE_STAGING {
                0
            } else {
                D3D11_BIND_SHADER_RESOURCE.0 as u32
            },
            CPUAccessFlags: cpu,
            MiscFlags: 0,
        };
        let mut tex: Option<ID3D11Texture2D> = None;
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex)) }.unwrap_or_else(|e| {
            panic!("CreateTexture2D NV12(array={array}, usage={usage:?}, cpu={cpu:#x}) 失败: {e}")
        });
        tex.expect("CreateTexture2D 成功必有对象")
    }

    /// 填单切片 NV12 STAGING 的某一平面（平面 0=Y、1=UV）：NV12 只能以**整片**映射。
    fn fill_plane(context: &ID3D11DeviceContext, tex: &ID3D11Texture2D, plane: u32, value: u8) {
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe { context.Map(tex, 0, D3D11_MAP_WRITE, 0, Some(&mut mapped)) }
            .expect("Map 整片（NV12 只能整片映射）");
        let row = mapped.RowPitch as usize;
        let ptr = mapped.pData as *mut u8;
        let offset = if plane == 0 { 0 } else { row * (H as usize) };
        let rows = if plane == 0 { H as usize } else { (H / 2) as usize };
        let buf = unsafe { std::slice::from_raw_parts_mut(ptr.add(offset), row * rows) };
        buf.fill(value);
        unsafe { context.Unmap(tex, 0) };
    }

    /// 读回单切片 NV12 STAGING 某一平面的第一个字节。
    fn read_plane_byte(context: &ID3D11DeviceContext, tex: &ID3D11Texture2D, plane: u32) -> u8 {
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe { context.Map(tex, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }
            .expect("Map 整片读回");
        let row = mapped.RowPitch as usize;
        let offset = if plane == 0 { 0 } else { row * (H as usize) };
        let b = unsafe { *(mapped.pData as *const u8).add(offset) };
        unsafe { context.Unmap(tex, 0) };
        b
    }

    /// 建 D3D11 设备 B（渲染设备）：硬件优先、WARP 兜底。返回 `(device, context)`。
    fn make_device_b() -> Option<(ID3D11Device, ID3D11DeviceContext)> {
        fn attempt(driver: D3D_DRIVER_TYPE) -> Option<(ID3D11Device, ID3D11DeviceContext)> {
            let adapter: Option<&windows::Win32::Graphics::Dxgi::IDXGIAdapter> = None;
            let mut device: Option<ID3D11Device> = None;
            let mut context: Option<ID3D11DeviceContext> = None;
            let hr = unsafe {
                D3D11CreateDevice(
                    adapter,
                    driver,
                    HMODULE::default(),
                    D3D11_CREATE_DEVICE_FLAG(D3D11_CREATE_DEVICE_VIDEO_SUPPORT.0 as u32),
                    None,
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    Some(&mut context),
                )
            };
            if hr.is_err() || device.is_none() || context.is_none() {
                return None;
            }
            Some((device.unwrap(), context.unwrap()))
        }
        attempt(D3D_DRIVER_TYPE_HARDWARE).or_else(|| attempt(D3D_DRIVER_TYPE_WARP))
    }

    /// **跨设备 keyed-mutex 纹理移动**（双设备架构的核心实证）：
    /// 设备 A = worker（SharedCopier 的槽池）写入已知平面（Y=0x12、UV=0x34）；
    /// 设备 B = 新建 D3D11 设备 → `OpenSharedResource` 打开该句柄 → `AcquireSync(0)` →
    /// `CopyResource` 到 B 侧 staging → `ReleaseSync(0)` → 读回断言两平面原样到达。
    ///
    /// 证明：曲线共享纹理可经 shared handle 在**两个独立 D3D11 设备**间以 keyed mutex 完成
    /// 帧移动（WARP↔WARP 与 hardware↔hardware 链路均覆盖；CPU 零回读、零共享上下文）。
    #[test]
    fn test_shared_handle_cross_device_roundtrip() {
        let Some(worker) = create_worker_d3d11_device() else {
            eprintln!("SKIP: D3D11 硬件/WARP 设备均不可用");
            return;
        };
        let Some((dev_b, ctx_b)) = make_device_b() else {
            eprintln!("SKIP: 无设备 B（渲染侧 D3D11 设备）");
            return;
        };
        let device_a = &worker.device;
        let ctx_a = &worker.context;

        // 设备 A 上准备带已知平面的源纹理（单切片 NV12 DEFAULT，经 staging 注入）
        let src_default = make_nv12(device_a, D3D11_USAGE_DEFAULT, 0, 1);
        let src_stage = make_nv12(
            device_a,
            D3D11_USAGE_STAGING,
            D3D11_CPU_ACCESS_READ.0 as u32 | D3D11_CPU_ACCESS_WRITE.0 as u32,
            1,
        );
        fill_plane(ctx_a, &src_stage, 0, 0x12); // Y = 0x12
        fill_plane(ctx_a, &src_stage, 1, 0x34); // UV = 0x34
        unsafe { ctx_a.CopyResource(&src_default, &src_stage) };
        let src_raw = src_default.as_raw() as usize;

        // worker 拷贝引擎：把源阵元 0 拷进共享槽并 ReleaseSync
        let mut copier = SharedCopier::new(WorkerD3D {
            device: device_a.clone(),
            context: ctx_a.clone(),
            kind: worker.kind,
        });
        let frame = copier.copy(W, H, src_raw, 0).expect("共享槽 GPU 侧拷贝成功");
        assert_ne!(frame.shared_handle, 0, "共享句柄不应为 0");

        // 设备 B：OpenSharedResource 打开共享句柄 → 键控互斥读 → 拷回 staging → 断言
        let mut opened: Option<ID3D11Texture2D> = None;
        unsafe {
            dev_b
                .OpenSharedResource(HANDLE(frame.shared_handle as *mut c_void), &mut opened)
                .expect("设备 B 打开共享句柄失败");
        }
        let opened = opened.expect("OpenSharedResource 成功必有纹理");
        let mutex_b: IDXGIKeyedMutex = opened.cast().expect("打开纹理应带键控互斥");
        unsafe {
            mutex_b.AcquireSync(0, ACQUIRE_TIMEOUT_MS).expect("B 侧 acquire 成功");
        }
        let back = make_nv12(
            &dev_b,
            D3D11_USAGE_STAGING,
            D3D11_CPU_ACCESS_READ.0 as u32 | D3D11_CPU_ACCESS_WRITE.0 as u32,
            1,
        );
        unsafe { ctx_b.CopyResource(&back, &opened) };
        unsafe {
            mutex_b.ReleaseSync(0).expect("B 侧 release 成功");
        }
        let y = read_plane_byte(&ctx_b, &back, 0);
        let uv = read_plane_byte(&ctx_b, &back, 1);
        eprintln!(
            "[dual roundtrip] 设备A={} 设备B=渲染 共享句柄={:#x} Y={y:#x} UV={uv:#x}（期望 0x12/0x34）",
            worker.kind, frame.shared_handle
        );
        assert_eq!(y, 0x12, "跨设备后 Y 平面应原样到达");
        assert_eq!(uv, 0x34, "跨设备后 UV 平面应原样到达（半分辨率平面拷贝）");

        // 槽回池复用：recycle 后再次 copy 应复用同一句柄（不新建、不耗尽）
        copier.recycle(frame.shared_handle);
        assert!(
            (0..(copier.max_pool - 1)).all(|_| {
                let f2 = copier
                    .copy(W, H, src_raw, 0)
                    .expect("复用槽再次拷贝成功");
                copier.recycle(f2.shared_handle);
                f2.shared_handle == frame.shared_handle
            }),
            "recycle 后应复用同一共享槽句柄（池不增长）"
        );
    }

    /// 把共享槽（worker 设备上的单切片 NV12 共享纹理）拷回 staging 读 (Y@0, UV@rowH)。
    /// 槽是单切片 → 读回无「平面/阵元下标」歧义（既有 make_nv12 通路的 Map 姿势）。
    fn read_slot_planes(
        device: &ID3D11Device,
        ctx: &ID3D11DeviceContext,
        handle: usize,
    ) -> (u8, u8) {
        use windows::core::Interface as _;
        use windows::Win32::Foundation::HANDLE;
        let mut opened: Option<ID3D11Texture2D> = None;
        unsafe { device.OpenSharedResource(HANDLE(handle as *mut c_void), &mut opened) }
            .expect("OpenSharedResource 打开共享槽失败");
        let opened = opened.expect("OpenSharedResource 成功必有纹理");
        let mutex: IDXGIKeyedMutex = opened.cast().expect("共享槽应带键控互斥");
        unsafe { mutex.AcquireSync(0, ACQUIRE_TIMEOUT_MS).expect("读槽 acquire 成功") };
        let back = make_nv12(
            device,
            D3D11_USAGE_STAGING,
            D3D11_CPU_ACCESS_READ.0 as u32 | D3D11_CPU_ACCESS_WRITE.0 as u32,
            1,
        );
        unsafe { ctx.CopyResource(&back, &opened) };
        unsafe { mutex.ReleaseSync(0).expect("读槽 release 成功") };
        (read_plane_byte(ctx, &back, 0), read_plane_byte(ctx, &back, 1))
    }

    /// **[D3D11 平面数组拷贝语义取证]**（决定 `SharedCopier::copy` 的 SrcSubresource 下标方案）：
    /// 建 **DECODER-only（无 SHR）** NV12 **数组**纹理（ArraySize=2，镜像解码池），逐阵元经
    /// staging 填互异已知图案（e0: Y=0x10 UV=0x80；e1: Y=0x30 UV=0xC0），随后：
    ///   1) 子资源内容探针：把子资源下标 s∈0..1 各自拷回单切片 staging，打印 (Y@0, UV@rowH)
    ///      ——揭示「子资源下标 → 阵元/平面」在本驱动上的真实布局（探针只取两模型都合法的 0/1）；
    ///   2) 对 e0、e1 各跑一次**当前** `SharedCopier::copy`，把共享槽拷回 staging 断言 Y/UV 像素。
    /// 判定：若 (1) 显示「每阵元=单一子资源」且 (2) 对 e1 读回 ≠ e1 图案 → 当前 2i/2i+1 下标错，
    /// 须改为规范单调用（SrcSubresource=阵元下标，一次拷 Y+UV）；若 (1) 显示「平面独立子资源」
    /// 且 (2) 两阵元都吻合 → 本驱动行为与「子资源无平面项」的规范读数不同，当前写法即正确
    /// （不改写、记录证据）。
    #[test]
    fn test_shared_copier_array_pixel_roundtrip() {
        let Some(worker) = create_worker_d3d11_device() else {
            eprintln!("SKIP: D3D11 硬件/WARP 设备均不可用");
            return;
        };
        let device = &worker.device;
        let ctx = &worker.context;

        // 无 SHR、仅 DECODER（0x20）的 NV12 数组纹理——与 D3D11VA 解码池同款绑定
        let desc = D3D11_TEXTURE2D_DESC {
            Width: W,
            Height: H,
            MipLevels: 1,
            ArraySize: 2,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: 0x20,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut arr: Option<ID3D11Texture2D> = None;
        if let Err(e) = unsafe { device.CreateTexture2D(&desc, None, Some(&mut arr)) } {
            // 个别驱动（如仅 WARP 的机器）不支持 D3D11_BIND_DECODER 数组纹理 → SKIP 不 FAIL
            eprintln!("SKIP: CreateTexture2D DECODER-only NV12 数组失败（驱动不支持 DECODER 绑定）: {e}");
            return;
        }
        let arr = arr.expect("创建数组纹理必有对象");

        // 逐阵元填充：单切片 staging+default（既有 make_nv12 姿势）→ CopySubresourceRegion 进
        // 数组（按「阵元 = 子资源下标」的规范姿势；若本驱动为平面独立子资源，此填充会落在
        // 各阵元的 Y 平面、UV 平面另在下标 2i+1——由探针揭示）
        let pairs: [(u8, u8); 2] = [(0x10, 0x80), (0x30, 0xC0)];
        for (i, &(py, puv)) in pairs.iter().enumerate() {
            let def = make_nv12(device, D3D11_USAGE_DEFAULT, 0, 1);
            let stg = make_nv12(
                device,
                D3D11_USAGE_STAGING,
                D3D11_CPU_ACCESS_READ.0 as u32 | D3D11_CPU_ACCESS_WRITE.0 as u32,
                1,
            );
            fill_plane(ctx, &stg, 0, py);
            fill_plane(ctx, &stg, 1, puv);
            unsafe { ctx.CopyResource(&def, &stg) };
            unsafe {
                ctx.CopySubresourceRegion(&arr, i as u32, 0, 0, 0, &def, 0, None);
            }
        }
        let arr_raw = arr.as_raw() as usize;

        // 1) 子资源布局探针（s=0/1 在两模型下都合法，各拷回单切片后读 Y/UV）
        let stg = make_nv12(
            device,
            D3D11_USAGE_STAGING,
            D3D11_CPU_ACCESS_READ.0 as u32 | D3D11_CPU_ACCESS_WRITE.0 as u32,
            1,
        );
        for s in 0..2u32 {
            unsafe {
                let _ = ctx.CopySubresourceRegion(&stg, 0, 0, 0, 0, &arr, s, None);
            }
            let y = read_plane_byte(ctx, &stg, 0);
            let uv = read_plane_byte(ctx, &stg, 1);
            eprintln!(
                "[array probe] src_sub={s} → 单切片回读 Y={y:#04x} UV={uv:#04x}（填充名义分配：e0=10/80 e1=30/c0）"
            );
        }

        // 2) 当前 `SharedCopier::copy` 逐阵元：读回共享槽断言像素（copier 取克隆，worker 引用
        // 留给 `read_slot_planes`）
        let mut copier = SharedCopier::new(WorkerD3D {
            device: device.clone(),
            context: ctx.clone(),
            kind: worker.kind,
        });
        for (i, &(py, puv)) in pairs.iter().enumerate() {
            let frame = copier
                .copy(W, H, arr_raw, i as u32)
                .expect("数组阵元拷贝应成功");
            let (y, uv) = read_slot_planes(device, ctx, frame.shared_handle);
            eprintln!(
                "[passthrough copy] element={i} → 槽 Y={y:#04x} UV={uv:#04x}（期望 Y={py:#04x} UV={puv:#04x}）"
            );
            assert_eq!(y, py, "阵元 {i} 的 Y 平面应原样到达共享槽");
            assert_eq!(uv, puv, "阵元 {i} 的 UV 平面应原样到达共享槽");
            copier.recycle(frame.shared_handle);
        }
    }
}
