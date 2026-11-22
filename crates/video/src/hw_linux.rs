//! 双设备 GPU 直通的 **Linux 软解回退桩**（模块级 Windows 门控的互补面，Phase 1 Linux
//! 编译解锁）。
//!
//! Windows 用真实现 `src/hw.rs`（D3D11/Dxgi，零回读 GPU 直通）；本 crate 的 worker / reader
//! / render 生产代码大量引用 `hw` 模块的 `SharedCopier` / `CopyError` / `SharedFrame` /
//! `record_hw_paint` 类型与函数（含 workers.rs 的拷贝失败分类、display/render 的帧上屏），
//! 直接删掉 `mod hw` 会让这些名字在 Linux 上无法解析。本桩以**同签名**提供这些名字，语义为
//! 「不建 D3D11 设备 → 直通不可用」：
//!
//! - [`create_worker_d3d11_device`] 恒返回 `None` → worker `open_reader_for_mode` 的
//!   Auto 直通分支不接管，自动回落既有 SAFE BGRA 硬解 / 软解（`open_scaled_hw` /
//!   `open_scaled`），与 Windows 上「无 D3D11VA 能力适配器」的回退行为同构；
//! - [`SharedCopier`] 等类型仅为让生产签名可解析而存在，运行期不会被构造（无设备即无
//!   copier，`dual` 恒 false、`FrameItem::Gpu` 永不产生）。
//!
//! 桩不产生任何 D3D11 / windows 依赖，Linux 构建图不含 `windows` / `windows-future`。
//! 测试专用探针（`record_hw_paint` / `copy_fail_*`）此处为 no-op / 恒 false，仅在 Linux
//! 测试构建下被引用（Linux 视频 GPU 直通测试本就不可测，SKIP 语义一致）。

#![allow(dead_code)] // 桩仅为保证生产签名可解析；运行期构造不发生，dead_code 预期内。

/// worker 侧 D3D11 设备（桩）：Linux 上不建设备，仅承载诊断用的驱动类别名（恒不实例化）。
pub struct WorkerD3D {
    /// 驱动名（"hardware" / "warp"），诊断打印用；桩上无意义（配合 [`create_worker_d3d11_device`] 恒 None）。
    #[allow(dead_code)]
    pub kind: &'static str,
}

/// 建 worker 设备（桩）：Linux 无 D3D11，恒返回 `None` → 生产 worker 自动走 SAFE/软解回退。
pub fn create_worker_d3d11_device() -> Option<WorkerD3D> {
    None
}

/// 一帧解码帧拷进共享 NV12 槽后的产物（桩：类型占位，运行期不产生）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SharedFrame {
    /// DXGI 共享句柄（usize 数值）。
    pub shared_handle: usize,
    /// 拷贝后的显示宽。
    pub w: u32,
    /// 拷贝后的显示高。
    pub h: u32,
}

/// [`SharedCopier::copy`] 失败原因分类（桩：变体与 Windows 侧一致，保证 workers.rs 的
/// 拷贝失败分类代码可解析；运行期无 copier、不触发）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyError {
    /// 共享槽池瞬满 / 获取互斥超时等渲染端瞬时滞后（可重试）。
    Backpressure,
    /// 解码帧格式非 NV12（10-bit P010 等）——不支持 GPU 直通。
    Unsupported,
    /// AcquireSync/ReleaseSync 键控互斥失败（含超时）。
    Sync,
    /// 共享 NV12 目标纹理创建失败。
    Resource,
}

/// 双设备直通 worker 侧拷贝引擎（桩：类型占位 + 恒失败；Linux 上 worker 从不构造它，
/// `copy` 仅作编译占位返回 [`CopyError::Backpressure`]）。
pub struct SharedCopier {
    #[allow(dead_code)]
    max_pool: usize,
}

impl SharedCopier {
    /// 桩构造（实际不建任何设备上下文）。
    pub fn new(_worker: WorkerD3D) -> Self {
        Self { max_pool: 0 }
    }

    /// 桩 `copy`：恒报背压（运行期不可达——无设备即无 copier）。
    pub fn copy(
        &mut self,
        _w: u32,
        _h: u32,
        _src_tex: usize,
        _array_index: u32,
    ) -> Result<SharedFrame, CopyError> {
        Err(CopyError::Backpressure)
    }

    /// 桩 `recycle`：主线程归还槽标记回空闲（无槽可回收，no-op）。
    pub fn recycle(&mut self, _shared_handle: usize) {}

    /// 桩 `clear`：清空池（无池，no-op）。
    #[allow(dead_code)]
    pub fn clear(&mut self) {}
}

/// 记录最近一次 canvas paint 的 `paint_external_yuv` 共享句柄（桩：no-op，与 Windows 侧
/// 生产构建的 no-op 语义一致；Linux 渲染路径不画外部 YUV）。
pub fn record_hw_paint(_shared_handle: usize) {}

#[cfg(test)]
thread_local! {
    static HW_PAINT_PROBE: std::cell::RefCell<usize> = const { std::cell::RefCell::new(0) };
}

/// 取走最近一次 `paint_external_yuv` 收到的共享句柄（桩：恒 0，Linux 无直通绘制）。
#[cfg(test)]
pub fn take_hw_paint_probe() -> usize {
    HW_PAINT_PROBE.with(|p| p.replace(0))
}

/// 测试专用：强制槽回压注入（桩：no-op——Linux 无共享槽逻辑）。
#[cfg(test)]
pub fn copy_fail_for(_n: u32) {}

/// 测试专用：强制格式不支持注入（桩：no-op）。
#[cfg(test)]
pub fn copy_unsupported_for(_n: u32) {}

/// 测试专用：撤销拷贝失败注入（桩：no-op）。
#[cfg(test)]
pub fn copy_fail_disable() {}

/// 测试专用：背压注入是否已触发（桩：恒 false）。
#[cfg(test)]
pub fn copy_fail_fired() -> bool {
    false
}

/// 测试专用：格式不支持注入是否已触发（桩：恒 false）。
#[cfg(test)]
pub fn copy_unsupported_fired() -> bool {
    false
}
