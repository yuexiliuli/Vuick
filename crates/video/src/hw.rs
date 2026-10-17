//! M2e：D3D11VA 硬件解码帧的 GPU 侧 NV12 拷贝与目标纹理池（零 CPU 回读）。
//!
//! 解码器吐出的帧是 **D3D11 数组纹理**里的一个切片（`ID3D11Texture2D*` + `array_index`）。
//! 本机驱动（Intel Arc 实测）拒绝把 `D3D11_BIND_SHADER_RESOURCE` 直接挂到解码纹理上
//!（E_INVALIDARG），且解码数组纹理的阵元必须**尽快归还帧池**（headroom ≈1-2 帧）。因此
//! 视频 worker 在解码线程把该帧 **GPU 侧拷进一块可复用的 SRV-capable NV12 目标纹理**
//!（`CopySubresourceRegion` 按 Y/UV 两平面分别拷贝），再 `Flush()` 提交到 GPU 队列，
//! 随后立即 `vff_video_frame_release` 归还解码槽位；目标纹理由主线程经释放消息回池复用。
//! 主渲染（directx_renderer 的 `external_yuv` 管线）对目标纹理直接建两平面 SRV 采样，
//! 整个过程 CPU 端零回读、零 sws。
//!
//! 本模块另提供测试自建 D3D11 设备（硬件优先 → WARP 兜底）与 `paint_external_yuv`
//! 测试探针，供 `#[gpui::test]` 基准在无真实 GUI 时走完整硬解链路。
//!
//! 目标纹理的 COM 引用由**池内的 `ID3D11Texture2D` 包装持有**（Drop/clear 时统一释放）；
//! 交给主线程/主渲染的只是裸 `raw` 句柄——生命周期完全由 worker 线程池掌控，跨线程只传
//! 无所有权指针，无跨线程释放竞态。

/// 一帧解码纹理拷进的目标 NV12 纹理（SRV-capable、单数组切片）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CopiedFrame {
    /// 目标纹理 `ID3D11Texture2D*`（转 usize；与解码器同一设备）。
    pub tex: usize,
    /// 拷贝后的显示宽。
    pub w: u32,
    /// 拷贝后的显示高。
    pub h: u32,
}

/// 池中一块目标纹理（`tex_obj` 持有 COM 引用，`raw` 供跨线程传递）。
struct DestTex {
    /// 目标纹理 `ID3D11Texture2D*`（转 usize，无所有权：池经 `tex_obj` 持有引用）。
    raw: usize,
    /// 池持有的 COM 引用（Drop/clear 时统一释放；本字段在 `copy_impl` 里借读）。
    tex_obj: windows::Win32::Graphics::Direct3D11::ID3D11Texture2D,
    w: u32,
    h: u32,
    /// true = 已交给主线程、待回收。
    in_use: bool,
}

/// D3D11VA 帧 → SRV-capable NV12 目标纹理所用的目标纹理池（**worker 线程独享**）。
///
/// 生命周期：`copy` 在池里挑一块空闲且尺寸匹配的纹理，否则新建（`max_pool` 封顶）；
/// `recycle` 由 worker 在收到主线程释放消息时把纹理标记回空闲。主线程换帧时把旧 `tex`
/// 经通道发回 → worker 再 `recycle`；纹理始终在解码线程内创建/销毁（池 Drop 释放），
/// 跨线程只传裸指针。设备/上下文裸指针只在本线程借用包装（`from_raw` 不 AddRef、用后
/// `forget`），引用由 `gpui_windows`（真实 GUI）或测试自建设备持有。
pub struct HwFrameCopier {
    /// `ID3D11Device*` 裸指针。
    device: usize,
    /// `ID3D11DeviceContext*` 裸指针。
    context: usize,
    /// 目标纹理池（`in_use` = 已交给主线程、待回收）。
    pool: Vec<DestTex>,
    /// 池封顶（同时存活的目标纹理数；有界通道 2 槽 + 主线程 1-2 帧在途 ≈ ≤4）。
    max_pool: usize,
}

impl HwFrameCopier {
    /// 以进程级 D3D11 设备/上下文裸指针新建拷贝引擎。
    pub fn new(device: usize, context: usize) -> Self {
        Self {
            device,
            context,
            pool: Vec::new(),
            max_pool: 8,
        }
    }

    /// 把解码帧 `src_tex` 的第 `array_index` 个阵元 GPU 侧拷进目标 NV12 纹理并 `Flush`，
    /// 返回目标纹理句柄（`Send` 给主线程的唯一载体；主线程用后经释放消息回池）。
    pub fn copy(
        &mut self,
        w: u32,
        h: u32,
        src_tex: usize,
        array_index: u32,
    ) -> anyhow::Result<CopiedFrame> {
        use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11DeviceContext};
        use windows::core::Interface as _;
        // 借用设备/上下文包装（不 AddRef；本线程内用完 forget，引用由持有者管理）
        let device = unsafe { ID3D11Device::from_raw(self.device as *mut _) };
        let context = unsafe { ID3D11DeviceContext::from_raw(self.context as *mut _) };
        let result = self.copy_impl(&device, &context, w, h, src_tex, array_index);
        std::mem::forget(device);
        std::mem::forget(context);
        result
    }

    /// 把主线程回收的纹理标记回空闲（供下一帧复用）。
    pub fn recycle(&mut self, tex: usize) {
        if let Some(slot) = self.pool.iter_mut().find(|t| t.raw == tex) {
            slot.in_use = false;
        }
    }

    fn copy_impl(
        &mut self,
        device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
        context: &windows::Win32::Graphics::Direct3D11::ID3D11DeviceContext,
        w: u32,
        h: u32,
        src_tex: usize,
        array_index: u32,
    ) -> anyhow::Result<CopiedFrame> {
        use windows::Win32::Graphics::Direct3D11::{D3D11_BOX, D3D11_TEXTURE2D_DESC, ID3D11Texture2D};
        use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_NV12;
        use windows::core::Interface as _;

        // 取（或新建）目标纹理：返回其在池中的下标；COM 引用由池持有
        let idx = self.ensure_dest(device, w, h)?;
        unsafe {
            // 解码器数组纹理借用包装（不 AddRef；用后 forget，引用由 FFmpeg 帧池管理）
            let src = ID3D11Texture2D::from_raw(src_tex as *mut _);
            let mut src_desc = std::mem::zeroed::<D3D11_TEXTURE2D_DESC>();
            src.GetDesc(&mut src_desc);
            // 10-bit / 其它平面格式（P010 等）无法直接拷进 NV12 目标纹理：报错让上层
            // 回落软解，绝不静默按错格式上屏（颜色失真）。NV12 = 2 平面（Y + 交错 UV）。
            if src_desc.Format != DXGI_FORMAT_NV12 {
                std::mem::forget(src);
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
            let dest = &self.pool[idx];

            // NV12 两平面分别拷贝（目标为 1 mip × 1 切片）。D3D11 平面数组纹理的子资源
            // 扁平号 = 阵元 × PlaneCount + 平面（NV12 每阵元 2 平面，总子资源 = N×2）：
            //  - Y  平面：src 子资源 = array_index*2 + 0（阵元 idx、平面 0）→ dst 子资源 0
            //  - UV 平面：src 子资源 = array_index*2 + 1（阵元 idx、平面 1）→ dst 子资源 1
            // （旧误用 array_index / ArraySize+array_index 的「每阵元 1 平面」模型——会拷到
            // 别的阵元/别平面，硬解画面成花屏；拷贝机制回归见
            // tests::test_hw_copy_y_and_uv_planes_to_dest，数组子资源编号依据 D3D11
            // ComputeSubresourceIndex（NV12 PlaneCount=2））
            let y_box = D3D11_BOX {
                left: 0,
                top: 0,
                front: 0,
                right: w,
                bottom: h,
                back: 1,
            };
            context.CopySubresourceRegion(
                &dest.tex_obj,
                0,
                0,
                0,
                0,
                &src,
                array_index * 2,
                Some(&y_box),
            );
            let uv_box = D3D11_BOX {
                left: 0,
                top: 0,
                front: 0,
                right: w / 2,
                bottom: h / 2,
                back: 1,
            };
            context.CopySubresourceRegion(
                &dest.tex_obj,
                1,
                0,
                0,
                0,
                &src,
                array_index * 2 + 1,
                Some(&uv_box),
            );
            // 提交到 GPU 队列（不等待完成；主渲染同设备队列串行读）
            context.Flush();
            std::mem::forget(src);
        }
        let tex = self.pool[idx].raw;
        Ok(CopiedFrame { tex, w, h })
    }

    /// 取一块空闲且尺寸匹配的目标纹理的下标（无则新建进池，池已满则复用最旧空闲并改
    /// 尺寸）。返回下标让调用方从池里借目标纹理；COM 引用由池持有（生命期随池）。
    fn ensure_dest(
        &mut self,
        device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
        w: u32,
        h: u32,
    ) -> anyhow::Result<usize> {
        use windows::core::Interface as _;

        if let Some((idx, slot)) = self
            .pool
            .iter_mut()
            .enumerate()
            .find(|(_, t)| !t.in_use && t.w == w && t.h == h)
        {
            slot.in_use = true;
            return Ok(idx);
        }
        if self.pool.len() < self.max_pool {
            let tex_obj = create_dest_texture(device, w, h)?;
            let raw = tex_obj.as_raw() as usize;
            self.pool.push(DestTex {
                raw,
                tex_obj,
                w,
                h,
                in_use: true,
            });
            return Ok(self.pool.len() - 1);
        }
        // 池满且无尺寸匹配：换掉最旧空闲的一块（尺寸必然不同，见上）——**重建为当前
        // 尺寸**而非只改 w/h：CopySubresourceRegion 只覆写左上角新尺寸区域，若沿用旧
        // 大纹理会残留旧尺寸数据，且 SRV/采样按旧尺寸解析 → 半幅/花屏。重建 Drop 旧
        // 包装（Release COM 引用），新纹理进同槽。
        let (idx, slot) = self
            .pool
            .iter_mut()
            .enumerate()
            .find(|(_, t)| !t.in_use)
            .ok_or_else(|| anyhow::anyhow!("NV12 目标纹理池耗尽（主线程未及时回收）"))?;
        let tex_obj = create_dest_texture(device, w, h)?;
        let raw = tex_obj.as_raw() as usize;
        *slot = DestTex {
            raw,
            tex_obj,
            w,
            h,
            in_use: true,
        };
        Ok(idx)
    }
}

/// 建一块 SRV-capable 的 NV12 目标纹理（DEFAULT 用法、单数组切片、仅 SHADER_RESOURCE
/// 绑定）：与 gpui 主渲染的外部 NV12 管线（两平面 PlaneSlice SRV）要求严格一致。
fn create_dest_texture(
    device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
    w: u32,
    h: u32,
) -> anyhow::Result<windows::Win32::Graphics::Direct3D11::ID3D11Texture2D> {
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, ID3D11Texture2D,
    };
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
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
        MiscFlags: 0,
    };
    let mut texture: Option<ID3D11Texture2D> = None;
    unsafe {
        device.CreateTexture2D(&desc, None, Some(&mut texture))?;
    }
    Ok(texture.expect("CreateTexture2D 成功必有对象"))
}

/// 测试探针：记录最近一次 canvas paint 里 `paint_external_yuv` 收到的纹理句柄。
/// 生产构建为 no-op（零开销）；test 构建由 `#[gpui::test]` 读 [`take_hw_paint_probe`]，
/// 断言「HW 帧真的走进了外部 YUV 精灵的绘制路径」。
pub fn record_hw_paint(tex: usize) {
    #[cfg(test)]
    HW_PAINT_PROBE.with(|p| *p.borrow_mut() = tex);
    #[cfg(not(test))]
    {
        let _ = tex;
    }
}

#[cfg(test)]
thread_local! {
    static HW_PAINT_PROBE: std::cell::RefCell<usize> = const { std::cell::RefCell::new(0) };
}

/// 取走最近一次 `paint_external_yuv` 收到的纹理句柄（0 = 尚未绘制过）。
#[cfg(test)]
pub fn take_hw_paint_probe() -> usize {
    HW_PAINT_PROBE.with(|p| p.replace(0))
}

/// 测试自建 D3D11 设备（硬件驱动优先、WARP 兜底；两者都不可用 → `None`）。返回的设备/
/// 上下文由调用方持有到测试结束（`open_hw` 会再 AddRef 一份归 FFmpeg）。
/// 与 `video-ffmpeg` 的 `hw_external_tests.rs` 同款姿势。
#[cfg(test)]
pub struct TestDevice {
    /// `ID3D11Device`（测试持有保活）。
    pub device: windows::Win32::Graphics::Direct3D11::ID3D11Device,
    /// `ID3D11DeviceContext`（测试持有保活）。
    pub context: windows::Win32::Graphics::Direct3D11::ID3D11DeviceContext,
    /// 驱动名（"hardware" / "warp"），诊断打印用。
    pub kind: &'static str,
}

/// 建测试设备。
#[cfg(test)]
pub fn make_test_d3d11_device() -> Option<TestDevice> {
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::{
        D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP,
    };
    use windows::Win32::Graphics::Direct3D11::{
        D3D11CreateDevice, D3D11_CREATE_DEVICE_FLAG, D3D11_SDK_VERSION, ID3D11Device,
        ID3D11DeviceContext,
    };
    use windows::Win32::Graphics::Dxgi::IDXGIAdapter;

    fn attempt(driver: D3D_DRIVER_TYPE) -> Option<(ID3D11Device, ID3D11DeviceContext)> {
        let adapter: Option<&IDXGIAdapter> = None;
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

    if let Some(d) = attempt(D3D_DRIVER_TYPE_HARDWARE) {
        return Some(TestDevice { device: d.0, context: d.1, kind: "hardware" });
    }
    if let Some(d) = attempt(D3D_DRIVER_TYPE_WARP) {
        return Some(TestDevice { device: d.0, context: d.1, kind: "warp" });
    }
    None
}

/// 借用测试设备裸指针 `(device, context)`（供 `set_d3d11_device_context_for_test`）。
#[cfg(test)]
pub fn test_device_raw(dev: &TestDevice) -> (usize, usize) {
    use windows::core::Interface as _;
    (dev.device.as_raw() as usize, dev.context.as_raw() as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::core::Interface as _;
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_CPU_ACCESS_READ, D3D11_CPU_ACCESS_WRITE, D3D11_MAP_READ, D3D11_MAP_WRITE,
        D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
        ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
    };
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};

    // 16×8 NV12（WARP 对过小尺寸/非 4 倍宽的平面纹理可能 E_INVALIDARG，取标准倍数）：
    // Y 平面 = 16×8 字节；UV 平面 = 8×4×2 = 64 字节。
    const W: u32 = 16;
    const H: u32 = 8;

    /// 建测试 NV12 纹理：DEFAULT 用法需至少一个绑定标志（无 bind/无 CPU access 的
    /// 纹理会被 D3D11 判 E_INVALIDARG）；STAGING 用法以 CPU access 作为有效标志。
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

    /// 填单切片 NV12 STAGING 的某一平面（平面 0=Y、1=UV）：NV12 只能以**整片**映射
    /// （子资源 0 = Y 平面按行、随后 UV 平面），逐平面 Map 这两子资源会 E_INVALIDARG。
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

    /// 读回单切片 NV12 STAGING 某一平面的第一个字节（整片映射同 `fill_plane`）。
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

    /// 探针：单切片 NV12 的 UV 平面经整片 CopyResource 是否能原样往返（Map 整片布局
    /// Y@[0,rowPitch*H)、UV@[rowPitch*H, …)）。钉死 staging/整片/平面子资源的 D3D 行为。
    #[test]
    fn test_nv12_uv_roundtrip_via_copyresource() {
        let Some(dev) = make_test_d3d11_device() else {
            eprintln!("SKIP: 无 D3D11 设备");
            return;
        };
        let device = &dev.device;
        let context = &dev.context;
        let stage_a = make_nv12(
            device,
            D3D11_USAGE_STAGING,
            D3D11_CPU_ACCESS_READ.0 as u32 | D3D11_CPU_ACCESS_WRITE.0 as u32,
            1,
        );
        let def = make_nv12(device, D3D11_USAGE_DEFAULT, 0, 1);
        let stage_b = make_nv12(
            device,
            D3D11_USAGE_STAGING,
            D3D11_CPU_ACCESS_READ.0 as u32 | D3D11_CPU_ACCESS_WRITE.0 as u32,
            1,
        );
        fill_plane(context, &stage_a, 1, 0x3B);
        fill_plane(context, &stage_a, 0, 0x5A);
        unsafe {
            context.CopyResource(&def, &stage_a);
            context.CopyResource(&stage_b, &def);
        }
        let y = read_plane_byte(context, &stage_b, 0);
        let uv = read_plane_byte(context, &stage_b, 1);
        eprintln!("probe CopyResource roundtrip: Y={y:#x} UV={uv:#x}（期望 0x5a/0x3b）");
        assert_eq!(y, 0x5A);
        assert_eq!(uv, 0x3B, "整片 CopyResource 应原样带 UV 平面");
    }

    /// M2e 平台语义断言（实证，不靠口头承诺）——拷贝引擎的 Y/UV 双平面目标写入：
    /// 源为**单切片** NV12 DEFAULT（整片填字经 CopyResource 注入，该路径 WARP 已验证
    /// 可靠：`test_nv12_uv_roundtrip_via_copyresource`），经 `copier.copy`（Y 子资源 0、
    /// UV 子资源 1、Flush）拷进池目标纹理，读回断言 Y 平面与 UV 平面都在正确位置。
    /// 这覆盖 `copy_impl` 的目标写入（两平面子资源下标 0/1）、拷贝 box、Flush 与池机制。
    ///
    /// 解码器**数组纹理**的阵元子资源编号（`array_index*2+平面`）无法在 WARP 上实证：
    /// WARP 对 ArraySize≥2 的 NV12 数组，CopySubresourceRegion/UpdateSubresource 的
    /// 平面子资源寻址不可靠（本机实测填/读第 1 阵元 Y 平面失败）。D3D11 官方子资源公式
    /// `sub = arraySlice×PlaneCount + planeStride×mip + plane`（NV12 PlaneCount=2）即
    /// `array_index*2+平面`，与 M2d SRV（FirstArraySlice/PlaneSlice 实证组件的
    /// `(阵元,平面)` 语义）自洽；真机 GUI 像素级校验列为残余风险。
    #[test]
    fn test_hw_copy_y_and_uv_planes_to_dest() {
        let Some(dev) = make_test_d3d11_device() else {
            eprintln!("SKIP: D3D11 硬件/WARP 设备均不可用");
            return;
        };
        let device = &dev.device;
        let context = &dev.context;

        let src_default = make_nv12(device, D3D11_USAGE_DEFAULT, 0, 1);
        let src_stage = make_nv12(
            device,
            D3D11_USAGE_STAGING,
            D3D11_CPU_ACCESS_READ.0 as u32 | D3D11_CPU_ACCESS_WRITE.0 as u32,
            1,
        );
        fill_plane(context, &src_stage, 0, 0x12); // Y = 0x12
        fill_plane(context, &src_stage, 1, 0x34); // UV = 0x34
        unsafe { context.CopyResource(&src_default, &src_stage) };
        let src_raw = src_default.as_raw() as usize;

        let (raw_dev, raw_ctx) = (dev.device.as_raw() as usize, dev.context.as_raw() as usize);
        let mut copier = HwFrameCopier::new(raw_dev, raw_ctx);
        let copied = copier.copy(W, H, src_raw, 0).expect("GPU 侧 NV12 拷贝成功");
        let dest = unsafe { ID3D11Texture2D::from_raw(copied.tex as *mut _) };
        let back = make_nv12(
            device,
            D3D11_USAGE_STAGING,
            D3D11_CPU_ACCESS_READ.0 as u32 | D3D11_CPU_ACCESS_WRITE.0 as u32,
            1,
        );
        unsafe { context.CopyResource(&back, &dest) };
        let y = read_plane_byte(context, &back, 0);
        let uv = read_plane_byte(context, &back, 1);
        assert_eq!(y, 0x12, "目标 Y 平面应收到源 Y 平面数据");
        assert_eq!(uv, 0x34, "目标 UV 平面应收到源 UV 平面数据（NV12 半分辨率平面拷贝）");
        std::mem::forget(dest);
    }
}


