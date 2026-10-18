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
//!         → ReleaseSync(0) → Flush                           AcquireSync(0) → 建两平面 SRV
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
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{IDXGIKeyedMutex, IDXGIResource};

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
/// 与 `video-ffmpeg` 的 `hw_external_tests` 同款姿势（生产可调，非 cfg(test)）。
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
                D3D11_CREATE_DEVICE_FLAG(0),
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
            max_pool: 8,
        }
    }

    /// 在池中取（或新建）一块**尺寸匹配且未在途**的共享 NV12 目标纹理，把解码帧
    /// `src_tex` 的第 `array_index` 个阵元 GPU 侧拷进该纹理并 `Flush` + `ReleaseSync`，
    /// 返回共享句柄（跨线程传递给主线程的唯一载体；主线程用后经释放消息回池）。
    ///
    /// 失败（格式非 NV12 / 获取互斥超时 / 池耗尽）→ `Err`，外层据此回退 SAFE BGRA 路径。
    pub fn copy(
        &mut self,
        w: u32,
        h: u32,
        src_tex: usize,
        array_index: u32,
    ) -> anyhow::Result<SharedFrame> {
        let idx = self.ensure_slot(w, h)?;
        let slot = &self.pool[idx];
        unsafe {
            // 键控互斥获取：客户端写入完成并 ReleaseSync 前阻塞；超时不 INFINITE（见模块头）
            slot.mutex.AcquireSync(0, ACQUIRE_TIMEOUT_MS)?;

            // 解码器数组纹理借用包装（不 AddRef；用后 forget，引用由 FFmpeg 帧池管理）
            let src = ID3D11Texture2D::from_raw(src_tex as *mut _);
            let mut src_desc = std::mem::zeroed::<D3D11_TEXTURE2D_DESC>();
            src.GetDesc(&mut src_desc);
            if src_desc.Format != DXGI_FORMAT_NV12 {
                std::mem::forget(src);
                let _ = slot.mutex.ReleaseSync(0);
                return Err(anyhow::anyhow!(
                    "D3D11VA 帧格式 {:?} 非 NV12（10-bit P010 等暂不支持硬件直通），回退软解",
                    src_desc.Format
                ));
            }
            debug_assert!(
                array_index < src_desc.ArraySize,
                "D3D11VA 帧数组下标越界: idx={array_index} ArraySize={}",
                src_desc.ArraySize
            );

            // NV12 两平面分别拷贝（目标 = 1 mip × 1 切片非数组 NV12）：
            //  - Y  平面：src 子资源 = array_index*2 + 0 → dst 子资源 0
            //  - UV 平面：src 子资源 = array_index*2 + 1 → dst 子资源 1
            // （D3D11 ComputeSubresourceIndex（NV12 PlaneCount=2）——与 M2c/M2e 拷贝机制一致）
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
                0,
                0,
                0,
                0,
                &src,
                array_index * 2,
                Some(&y_box),
            );
            let uv_box = windows::Win32::Graphics::Direct3D11::D3D11_BOX {
                left: 0,
                top: 0,
                front: 0,
                right: w / 2,
                bottom: h / 2,
                back: 1,
            };
            self.context.CopySubresourceRegion(
                &slot.tex,
                1,
                0,
                0,
                0,
                &src,
                array_index * 2 + 1,
                Some(&uv_box),
            );
            // 提交到 GPU 队列（不等待完成；随后 ReleaseSync 保证渲染端 acquire 到的是已
            // 提交的写入——DXGI 键控互斥 release 在已提交工作完成后才被观测到）
            self.context.Flush();
            // 交还互斥：渲染端可 acquire 本槽
            slot.mutex.ReleaseSync(0)?;
            std::mem::forget(src);
        }
        let raw_handle = self.pool[idx].raw_handle;
        Ok(SharedFrame {
            shared_handle: raw_handle,
            w,
            h,
        })
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
    fn ensure_slot(&mut self, w: u32, h: u32) -> anyhow::Result<usize> {
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
            let slot = create_shared_slot(&self.device, w, h)?;
            self.pool.push(slot);
            return Ok(self.pool.len() - 1);
        }
        // 池满且无尺寸匹配：换掉最旧空闲的一块（尺寸必然不同）——重建为当前尺寸。
        let (idx, _) = self
            .pool
            .iter_mut()
            .enumerate()
            .find(|(_, s)| !s.in_use)
            .ok_or_else(|| anyhow::anyhow!("NV12 共享槽池耗尽（主线程未及时归还槽）"))?;
        let slot = create_shared_slot(&self.device, w, h)?;
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
                    D3D11_CREATE_DEVICE_FLAG(0),
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
}
