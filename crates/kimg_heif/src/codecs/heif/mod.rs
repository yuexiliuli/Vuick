//! HEIF/HEIC 解码门面（对应 C# `HeifDecoder`/`HeifImageDecoder` 的 Rust 等价物）。
//!
//! Phase-3：容器/参数集/位读取/切片头/帧解码（含 in-loop 滤波）全部可用；`read_info`/`heif_probe`
//! （header-only）真实现；`decode_yuv`（canvas 级 YUV，transform 前）与 `decode_rgba`（含 transform
//! 显示朝向全链 RGBA）真实现；`HeicDecoder` 接入 kimg `ImageDecoder` trait，`read_image` 返回真 RGBA8。
//! transform 属性（clap/irot/imir）按文件关联顺序应用到 RGBA 画布（对齐 C# `HeifCanvas` 合成映射）。

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
pub mod stage_clock;
pub mod transform;

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

/// 把容器里每个画面 item（grid 行主序）解码为 [`canvas::TileYuv`]（含 in-loop 滤波，
/// 对应 C# `DecodeCodedImages`；`decode_item_flags` 全滤波口径 = `_t0.yuv` oracle）。
fn decode_tiles(data: &[u8], container: &container::HeifContainer<'_>) -> HeifResult<Vec<canvas::TileYuv>> {
    if container.tiles.is_empty() {
        return Err(HeifError::invalid("容器没有可解码的图像 item。"));
    }
    let mut tiles = Vec::with_capacity(container.tiles.len());
    for i in 0..container.tiles.len() {
        let config = &container.tile_configs[i];
        let item = &container.items.items[container.tiles[i]];
        let length = isobmff::item_container::ItemContainer::get_item_length(item);
        // 先校验 payload 不超出文件再分配（镜像 C# `ResolvePayloads` 的 `length > data.Length` 拒绝）。
        if length < 0 || length > data.len() as i64 {
            return Err(HeifError::invalid("图像 item 大于文件。"));
        }
        let mut payload = vec![0u8; length as usize];
        stage_clock::parse(|| container.items.resolve(item, &mut payload))?;
        let pic = hevc::decode_item_flags(config, &payload, false, false)?;
        tiles.push(canvas::TileYuv {
            y: pic.planes[0].clone(),
            cb: pic.planes[1].clone(),
            cr: pic.planes[2].clone(),
            y_w: pic.width,
            y_h: pic.height,
            c_w: pic.chroma_width,
            c_h: pic.chroma_height,
            crop_left: pic.crop_left,
            crop_right: pic.crop_right,
            crop_top: pic.crop_top,
            crop_bottom: pic.crop_bottom,
        });
    }
    Ok(tiles)
}

/// 解码 canvas 级 YUV 平面（transform 前；grid 合成后）。对应 C# `DecodeYuv`：每个 tile 的
/// conformance window 输出按 grid 位置贴到画布并裁剪到 grid 输出尺寸。transform 属相（clap/irot/
/// imir）不影响此步——它退回**变换前画布**（IMG_* 竖图返回横向画布，与 C# oracle `<md5>.yuv`
/// 逐字节一致，供 oracle 对照）。
pub fn decode_yuv(data: &[u8]) -> HeifResult<YuvPlanes> {
    let container = container::HeifContainer::parse(data)?;
    if container.tiles.is_empty() {
        return Err(HeifError::invalid("容器没有可解码的图像 item。"));
    }
    let sps = container.tile_configs[0].parse_sps()?;
    if sps.bit_depth_y != sps.bit_depth_c {
        return Err(HeifError::invalid("亮度与色度位深不同不受支持。"));
    }
    let tiles = decode_tiles(data, &container)?;
    let composed = canvas::compose_grid(
        &tiles,
        container.grid_columns,
        container.grid_rows,
        container.canvas_width,
        container.canvas_height,
    )?;
    Ok(YuvPlanes {
        y: composed.y,
        cb: composed.cb,
        cr: composed.cr,
        width: composed.width,
        height: composed.height,
        bit_depth: sps.bit_depth_y,
    })
}

/// 解码一张**单 tile** HEIC 图画为 4:2:0 YUV 平面（含 in-loop 滤波后的重建平面）。
/// grid 图像（多 tile）不支持，返回错误。供单 tile oracle 对照与新解码调试使用。
pub fn decode_yuv_bytes(data: &[u8]) -> HeifResult<YuvPlanes> {
    let container = container::HeifContainer::parse(data)?;
    if container.tiles.len() != 1 {
        return Err(HeifError::invalid("decode_yuv_bytes 只支持单 tile 图像。"));
    }
    let config = container.tile_configs[0].clone();
    let items = &container.items;
    let item = &items.items[container.tiles[0]];
    let length = isobmff::item_container::ItemContainer::get_item_length(item);
    // 先校验 payload 不超出文件再分配（镜像 C# Tiles.Extract 的 `length > data.Length` 拒绝）
    if length < 0 || length > data.len() as i64 {
        return Err(HeifError::invalid("图像 item 大于文件。"));
    }
    let mut payload = vec![0u8; length as usize];
    items.resolve(item, &mut payload)?;
    let pic = hevc::decode_item(&config, &payload)?;
    Ok(YuvPlanes {
        y: pic.planes[0].clone(),
        cb: pic.planes[1].clone(),
        cr: pic.planes[2].clone(),
        width: pic.width,
        height: pic.height,
        bit_depth: pic.bit_depth_y,
    })
}

/// 解码显示朝向的 RGBA。**真实现**：容器解析 → 逐 tile 解码（`decode_item_flags` 全滤波）→
/// `compose_grid` 合成整图 YUV → `color::yuv_to_rgba`（矩阵/full_range 取自容器 `nclx`，否则 SPS
/// VUI；8/10 位都走，产出 RGBA8）→ 按关联顺序应用 transform（clap/irot/imir，对准 C# `HeifCanvas`
/// 合成映射）写 `destination`。返回 `(宽, 高)` = 变换后显示尺寸（与 `ReadInfo().DisplaySize` 一致）。
pub fn decode_rgba(data: &[u8], destination: &mut [u8]) -> HeifResult<(i32, i32)> {
    let container = stage_clock::parse(|| container::HeifContainer::parse(data))?;
    if container.tiles.is_empty() {
        return Err(HeifError::invalid("容器没有可解码的图像 item。"));
    }
    let sps = container.tile_configs[0].parse_sps()?;
    if sps.bit_depth_y != sps.bit_depth_c {
        return Err(HeifError::invalid("亮度与色度位深不同不受支持。"));
    }
    let (matrix, full_range) = match container.nclx_colour {
        Some(n) => (n.matrix_coefficients, n.full_range),
        None => (sps.matrix_coefficients, sps.video_full_range),
    };
    let (cw, ch) = (container.canvas_width, container.canvas_height);
    if cw <= 0 || ch <= 0 {
        return Err(HeifError::invalid("画布尺寸非法。"));
    }
    let map = stage_clock::transform(|| transform::compose(cw, ch, &container.transforms))?;
    let (ow, oh) = (map.width, map.height);
    if ow <= 0 || oh <= 0 {
        return Err(HeifError::invalid("显示尺寸非法。"));
    }
    // 尺寸上限护栏：输出像素 × 4 字节不溢出（容器已限画布 ≤ 1<<28 像素；clap 只减 irot/imir 不增）。
    let need = (ow as usize)
        .checked_mul(oh as usize)
        .and_then(|p| p.checked_mul(4))
        .ok_or_else(|| HeifError::invalid("显示尺寸溢出。"))?;
    if destination.len() < need {
        return Err(HeifError::invalid(format!(
            "目标缓冲不足：需要 {need} 字节，实际 {} 字节。",
            destination.len()
        )));
    }
    let tiles = decode_tiles(data, &container)?;
    let composed = stage_clock::grid(|| {
        canvas::compose_grid(&tiles, container.grid_columns, container.grid_rows, cw, ch)
    })?;
    let rgba = stage_clock::color(|| {
        color::yuv_to_rgba(
            &composed.y,
            &composed.cb,
            &composed.cr,
            cw as usize,
            ch as usize,
            sps.bit_depth_y as u8,
            matrix,
            full_range,
        )
    })?;
    if container.transforms.is_empty() {
        destination[..need].copy_from_slice(&rgba);
    } else {
        stage_clock::transform(|| transform::apply_rgba(&rgba, cw, ch, &map, &mut destination[..need]))?;
    }
    Ok((ow, oh))
}

/// HEIF 解码器（接入 kimg `ImageDecoder` trait）：new 时读全量字节 → 容器解析拿尺寸/位深；
/// `read_image` 真解码 RGBA8（对齐 C# `HeifDecoder.Decode` 输出 = `PixelFormat.Rgba32`）。
pub struct HeicDecoder<R> {
    width: u32,
    height: u32,
    color_type: ColorType,
    icc_profile: Option<Vec<u8>>,
    // 保存输入字节（供 read_image 解码）。
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
        Ok(HeicDecoder {
            width: info.width as u32,
            height: info.height as u32,
            // 8/10 位都出 RGBA8（`decode_rgba` 经 `yuv_to_rgba` 把 10 位采样直接映射到 8 位输出，
            // 与 C# `HeifDecoder.Decode` 的 `Rgba32` 口径一致）。
            color_type: ColorType::Rgba8,
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

    fn read_image(self, buf: &mut [u8]) -> ImageResult<()> {
        let data = self._data.as_deref().ok_or_else(|| {
            ImageError::Decoding(DecodingError::new(
                ImageFormat::Heic.into(),
                "HEIF 输入数据缺失。",
            ))
        })?;
        decode_rgba(data, buf).map_err(|e| {
            ImageError::Decoding(DecodingError::new(ImageFormat::Heic.into(), e))
        })?;
        Ok(())
    }

    fn read_image_boxed(self: Box<Self>, buf: &mut [u8]) -> ImageResult<()> {
        (*self).read_image(buf)
    }
}

