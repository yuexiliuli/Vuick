//! HEIF/HEIC 解码门面（对应 C# `HeifDecoder`/`HeifImageDecoder` 的 Rust 等价物）。
//!
//! Phase-1：容器/参数集/位读取/切片头可用；`read_info`（header-only）与 `heif_probe` 真实现，
//! `decode_yuv`/`decode_rgba` 是返回 `NotImplemented` 的桩；`HeicDecoder` 接入 kimg
//! `ImageDecoder` trait，尺寸/色彩类型可读、`read_image` 返回未实现错误（格式识别可测）。

use std::io::{BufRead, Seek};
use std::marker::PhantomData;

use kimg_core::decoder::ImageDecoder;
use kimg_core::error::{DecodingError, ImageError, ImageResult};
use kimg_core::format::ImageFormat;
use kimg_core::ColorType;

pub mod canvas;
pub mod color;
pub mod container;
pub mod error;
pub mod hevc;
pub mod isobmff;
pub mod metadata;
pub mod probe;

pub use error::{HeifError, HeifResult};

/// 从 HEIF 容器读出的元数据（C# `HeifInfo` 等价物）。
#[derive(Debug, Clone)]
pub struct HeifInfo {
    /// 主图像像素宽（transform 之后）。
    pub width: i32,
    /// 主图像像素高。
    pub height: i32,
    /// HEVC 编码采样位深（Main / Main 10 为 8 / 10）。
    pub bit_depth: i32,
    /// 主图像由多少个 grid tile 组成；未平铺为 1。
    pub tile_count: usize,
    /// `colr` box 里的 ICC profile 字节，缺省 None。
    pub icc_profile: Option<Vec<u8>>,
    /// Exif Orientation 值（由 irot/imir 推导；Phase-1 恒 1 = 正常）。
    pub orientation: u8,
}

/// 解码出的 YUV 平面（canvas 级，transform 前；C# `DecodeYuv` 等价物签名）。
#[derive(Debug)]
pub struct YuvPlanes {
    pub y: Vec<u16>,
    pub cb: Vec<u16>,
    pub cr: Vec<u16>,
    pub width: i32,
    pub height: i32,
    pub bit_depth: i32,
}

/// 读容器元数据，不解码任何 HEVC payload；返回主图像尺寸/位深/tile 数等。
pub fn read_info(data: &[u8]) -> HeifResult<HeifInfo> {
    let container = container::HeifContainer::parse(data)?;
    let sps = container.tile_configs[0].parse_sps()?;
    let (w, h) = container.display_size()?;
    Ok(HeifInfo {
        width: w,
        height: h,
        bit_depth: sps.bit_depth_y,
        tile_count: container.tiles.len(),
        icc_profile: container.icc_profile.clone(),
        orientation: 1,
    })
}

/// 解码 canvas 级 YUV 平面。**Phase-1 桩**：返回 `NotImplemented`。
pub fn decode_yuv(data: &[u8]) -> HeifResult<YuvPlanes> {
    let _ = data;
    Err(HeifError::not_implemented(
        "HEIF 画面解码（YUV 平面）尚未实现（Phase-1 仅容器/参数集/位读取/切片头）。",
    ))
}

/// 解码显示朝向的 RGBA。**Phase-1 桩**：返回 `NotImplemented`。
pub fn decode_rgba(data: &[u8], destination: &mut [u8]) -> HeifResult<(i32, i32)> {
    let _ = (data, destination);
    Err(HeifError::not_implemented(
        "HEIF 画面解码（RGBA）尚未实现（Phase-1 仅容器/参数集/位读取/切片头）。",
    ))
}

/// HEIF 解码器（接入 kimg `ImageDecoder` trait）：new 时读全量字节 → 容器解析拿尺寸/位深；
/// `read_image`（画面重建）返回未实现错误，保证格式识别与尺寸读取可测。
pub struct HeicDecoder<R> {
    width: u32,
    height: u32,
    color_type: ColorType,
    icc_profile: Option<Vec<u8>>,
    // 保存输入字节（供后续 read_image 落实现使用；现触达路径未用）。
    _data: Option<Vec<u8>>,
    _r: PhantomData<R>,
}

impl<R: BufRead + Seek> HeicDecoder<R> {
    pub fn new(r: R) -> ImageResult<Self> {
        let mut data = Vec::new();
        {
            let mut br = r;
            br.read_to_end(&mut data).map_err(ImageError::IoError)?;
        }
        let info = read_info(&data).map_err(|e| {
            ImageError::Decoding(DecodingError::new(ImageFormat::Heic.into(), e))
        })?;
        let color_type = if info.bit_depth > 8 {
            ColorType::Rgb16
        } else {
            ColorType::Rgb8
        };
        Ok(HeicDecoder {
            width: info.width as u32,
            height: info.height as u32,
            color_type,
            icc_profile: info.icc_profile,
            _data: Some(data),
            _r: PhantomData,
        })
    }
}

impl<R: BufRead + Seek> ImageDecoder for HeicDecoder<R> {
    fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn color_type(&self) -> ColorType {
        self.color_type
    }

    fn icc_profile(&mut self) -> ImageResult<Option<Vec<u8>>> {
        Ok(self.icc_profile.clone())
    }

    fn read_image(self, _buf: &mut [u8]) -> ImageResult<()> {
        Err(ImageError::Decoding(DecodingError::new(
            ImageFormat::Heic.into(),
            "HEIF 画面重建尚未实现（Phase-1 仅容器/参数集/位读取/切片头）。",
        )))
    }

    fn read_image_boxed(self: Box<Self>, buf: &mut [u8]) -> ImageResult<()> {
        (*self).read_image(buf)
    }
}

