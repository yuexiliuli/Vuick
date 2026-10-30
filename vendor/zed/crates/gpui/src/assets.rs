use crate::{DevicePixels, Pixels, Result, SharedString, Size, size};
use smallvec::SmallVec;

use kimg::{Delay, Frame};
use std::{
    borrow::Cow,
    fmt,
    hash::Hash,
    sync::atomic::{AtomicUsize, Ordering::SeqCst},
};

/// A source of assets for this app to use.
pub trait AssetSource: 'static + Send + Sync {
    /// Load the given asset from the source path.
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>>;

    /// List the assets at the given path.
    fn list(&self, path: &str) -> Result<Vec<SharedString>>;
}

impl AssetSource for () {
    fn load(&self, _path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        Ok(None)
    }

    fn list(&self, _path: &str) -> Result<Vec<SharedString>> {
        Ok(vec![])
    }
}

/// A unique identifier for the image cache
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct ImageId(pub usize);

#[derive(PartialEq, Eq, Hash, Clone)]
#[expect(missing_docs)]
pub struct RenderImageParams {
    pub image_id: ImageId,
    pub frame_index: usize,
    /// 该帧是否为 R8 单通道感知亮度（1BPP，PDF 瓦片/封面 R8 通道）。`true` 时图片 key 走
    /// Monochrome atlas（R8_UNORM）、上传/存储按 1BPP——由 [`AtlasKey::texture_kind`] 路由。
    pub luma: bool,
}

/// R8 单通道（luma）帧：原始字节 = 宽×高×1 感知亮度（0..=255，BT.601 加权）。
/// 与 [`Frame`]（RGBA）平行共存，仅由 [`RenderImage::new_luma`] 使用（PDF 瓦片/封面 R8
/// 通道：GPU 重着色只读亮度、数学无损的 -4× 存储/上传）。上传进 Monochrome atlas
/// （`DXGI_FORMAT_R8_UNORM`，glyph 同款、无新增 GPU 格式）。
#[derive(Clone)]
pub struct LumaFrame {
    buffer: Vec<u8>,
    width: u32,
    height: u32,
    delay: Delay,
}

impl LumaFrame {
    /// 用紧凑感知亮度字节（`width * height * 1`）构造一帧 R8 luma 位图。
    pub fn new(buffer: Vec<u8>, width: u32, height: u32) -> Self {
        Self {
            buffer,
            width,
            height,
            delay: Delay::from_numer_denom_ms(100, 1),
        }
    }

    fn buffer(&self) -> &[u8] {
        &self.buffer
    }

    fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn delay(&self) -> Delay {
        self.delay
    }
}

/// A cached and processed image, in BGRA format
pub struct RenderImage {
    /// The ID associated with this image
    pub id: ImageId,
    /// The scale factor of this image on render.
    pub(crate) scale_factor: f32,
    data: SmallVec<[Frame; 1]>,
    /// R8 单通道 luma 帧（PDF 瓦片/封面 R8 通道，1BPP）：与 `data`（RGBA）互斥共存——
    /// 有 luma 帧时 `as_bytes`/`size`/`delay` 优先从本字段取，上传按 1BPP 进 Monochrome atlas。
    luma_data: SmallVec<[LumaFrame; 1]>,
}

impl PartialEq for RenderImage {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for RenderImage {}

impl RenderImage {
    /// Create a new image from the given data.
    pub fn new(data: impl Into<SmallVec<[Frame; 1]>>) -> Self {
        static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

        Self {
            id: ImageId(NEXT_ID.fetch_add(1, SeqCst)),
            scale_factor: 1.0,
            data: data.into(),
            luma_data: SmallVec::new(),
        }
    }

    /// 创建 R8 单通道（luma）图像：每帧字节 = 宽×高×1 感知亮度（0..=255）。用于 PDF 瓦片/
    /// 封面 R8 通道——上传进 Monochrome atlas（1BPP）、shader 按 R8 读取，存储与传输 -4×。
    pub fn new_luma(data: impl Into<SmallVec<[LumaFrame; 1]>>) -> Self {
        static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

        Self {
            id: ImageId(NEXT_ID.fetch_add(1, SeqCst)),
            scale_factor: 1.0,
            data: SmallVec::new(),
            luma_data: data.into(),
        }
    }

    /// 第 `frame_index` 帧是否为 R8 单通道 luma（对应 `as_bytes` 返回 1BPP 亮度字节）。
    pub fn is_luma(&self, frame_index: usize) -> bool {
        self.luma_data.get(frame_index).is_some()
    }

    /// Convert this image into a byte slice.
    pub fn as_bytes(&self, frame_index: usize) -> Option<&[u8]> {
        if let Some(frame) = self.luma_data.get(frame_index) {
            Some(frame.buffer())
        } else {
            self.data
                .get(frame_index)
                .map(|frame| frame.buffer().as_raw().as_slice())
        }
    }

    /// Get the size of this image, in pixels.
    pub fn size(&self, frame_index: usize) -> Size<DevicePixels> {
        if let Some(frame) = self.luma_data.get(frame_index) {
            let (width, height) = frame.dimensions();
            size(width.into(), height.into())
        } else if let Some(frame) = self.data.get(frame_index) {
            let (width, height) = frame.buffer().dimensions();
            size(width.into(), height.into())
        } else {
            Size::default()
        }
    }

    /// Get the size of this image, in pixels for display, adjusted for the scale factor.
    pub(crate) fn render_size(&self, frame_index: usize) -> Size<Pixels> {
        self.size(frame_index)
            .map(|v| (v.0 as f32 / self.scale_factor).into())
    }

    /// Get the delay of this frame from the previous
    pub fn delay(&self, frame_index: usize) -> Delay {
        if let Some(frame) = self.luma_data.get(frame_index) {
            frame.delay()
        } else {
            self.data
                .get(frame_index)
                .map(|frame| frame.delay())
                .unwrap_or(Delay::from_numer_denom_ms(100, 1))
        }
    }

    /// Get the number of frames for this image.
    pub fn frame_count(&self) -> usize {
        self.data.len().max(self.luma_data.len())
    }
}

impl fmt::Debug for RenderImage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ImageData")
            .field("id", &self.id)
            .field("size", &self.size(0))
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smallvec::SmallVec;

    #[test]
    fn empty_render_image_does_not_panic() {
        let image = RenderImage::new(SmallVec::new());
        assert_eq!(image.frame_count(), 0);
        assert_eq!(image.size(0), Size::default());
        assert_eq!(image.as_bytes(0), None);
        assert_eq!(image.render_size(0), Size::default());
        assert_eq!(image.delay(0), Delay::from_numer_denom_ms(100, 1));
        let _ = format!("{image:?}");
    }

    /// R8 luma 帧查询回路：`is_luma`/`as_bytes`（1BPP）/`size`/`frame_count` 从 luma 帧取，
    /// 越界帧回落空值，与 RGBA 帧互斥不混读。
    #[test]
    fn luma_render_image_bytes_and_size() {
        let mut luma = Vec::<u8>::new();
        luma.extend_from_slice(&[0u8, 64, 128, 200, 255]);
        let img = super::RenderImage::new_luma(vec![super::LumaFrame::new(luma, 5, 1)]);
        assert!(img.is_luma(0), "第 0 帧应为 luma");
        assert_eq!(img.as_bytes(0), Some(&[0u8, 64, 128, 200, 255][..]), "1BPP 字节直出");
        assert_eq!(
            img.size(0),
            size(DevicePixels(5), DevicePixels(1)),
            "luma 尺寸按帧维度取"
        );
        assert_eq!(img.frame_count(), 1, "luma 单帧 frame_count==1");
        assert!(!img.is_luma(1), "越界帧非 luma");
        assert_eq!(img.as_bytes(1), None, "越界帧无字节");
    }
}
