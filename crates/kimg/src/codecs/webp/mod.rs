//! WebP 解码器：image-webp 0.2.4 兜底 + 自研容器/VP8L 静态无损内核（P0 基建 + P1 容器 + P2 VP8L）。
//!
//! 对外 API（`WebPDecoder<R>` 的 `ImageDecoder`+`AnimationDecoder`）与 image 0.25 一致。
//! 后端开关（M3）：`WEBP_BACKEND` 原子位 AUTO/OURS/IMAGEWEBP，环境变量 `KIMG_WEBP_BACKEND`
//! 仅首次惰性解析一次（防每次解码 `env::var` 的 std::env 并发问题）；`pub(crate) set_webp_backend`
//! 供测试直设。家族判定（VP8/VP8L/动画/ALPH）在容器解析后按 VP8X features byte + 是否含 ANIM
//! chunk 判定（P1 起），不按首 4 字节盲判。

use std::io::{BufRead, Cursor, Seek};
use std::marker::PhantomData;
use std::sync::atomic::{AtomicU8, Ordering};

use crate::animation::{Delay, Frame, Frames, LoopCount};
use crate::buffer::{RgbImage, RgbaImage};
use crate::color::ColorType;
use crate::error::{DecodingError, ImageError, ImageResult};
use crate::format::ImageFormat;
use crate::metadata::Orientation;
use crate::pixel::Rgba;
use crate::{AnimationDecoder, ImageDecoder};

pub(crate) mod container;
pub(crate) mod simd;
pub(crate) mod vp8;
pub(crate) mod vp8l;

// 外部语料定位助手（与集成测试共享，见 tests/fixtures/webp/mod.rs）。
#[cfg(test)]
#[path = "../../../tests/fixtures/webp/mod.rs"]
mod webp_fixtures_support;

// ---------------------------------------------------------------------------
// 后端开关（M3）
// ---------------------------------------------------------------------------

/// 自动（默认）：本阶段=image-webp（M6：默认不翻转；待 P3 有损族就绪后一起接管）。
pub const BACKEND_AUTO: u8 = 0;
/// 强制自研（仅已实现家族=VP8L 静态无损 + VP8 静态有损，未实现家族仍回 image-webp）。
pub const BACKEND_OURS: u8 = 1;
/// 强制 image-webp（含自研已实现家族）。
pub const BACKEND_IMAGEWEBP: u8 = 2;

static WEBP_BACKEND: AtomicU8 = AtomicU8::new(BACKEND_AUTO);

/// 直设后端（测试/对照用；不触发环境变量解析）。
#[cfg_attr(not(test), allow(dead_code))]
pub fn set_webp_backend(v: u8) {
    debug_assert!(v <= BACKEND_IMAGEWEBP);
    WEBP_BACKEND.store(v, Ordering::SeqCst);
}

/// 解析后端：首次调用（状态为 AUTO 且从未被直设）才读一次环境变量 `KIMG_WEBP_BACKEND`
/// （`auto|ours|imagewebp`），并发下 CAS 保证只由先到者写入。此后恒定。
pub(crate) fn resolved_backend() -> u8 {
    let cur = WEBP_BACKEND.load(Ordering::Relaxed);
    if cur != BACKEND_AUTO {
        return cur;
    }
    let resolved = match std::env::var("KIMG_WEBP_BACKEND") {
        Ok(v) => match v.to_ascii_lowercase().as_str() {
            "ours" => BACKEND_OURS,
            "imagewebp" | "image-webp" => BACKEND_IMAGEWEBP,
            _ => BACKEND_AUTO,
        },
        Err(_) => BACKEND_AUTO,
    };
    match WEBP_BACKEND.compare_exchange(
        BACKEND_AUTO,
        resolved,
        Ordering::SeqCst,
        Ordering::SeqCst,
    ) {
        Ok(_) => resolved,
        Err(already_set) => already_set,
    }
}

/// 是否允许自研路径接管：仅 `OURS` 强制态（M6：本阶段默认 AUTO=image-webp，待有损族就绪后
/// 一起翻转默认；`KIMG_WEBP_BACKEND=ours` 或测试 `set_webp_backend(OURS)` 显式开启）。
fn ours_allowed() -> bool {
    resolved_backend() == BACKEND_OURS
}

// ---------------------------------------------------------------------------
// 解码器包装
// ---------------------------------------------------------------------------

/// 容器解析 + 自研解码产物（P1/P2 填充）。
#[derive(Default)]
pub(crate) struct OursStatic {
    /// 解码后的 RGBA8 像素（长度 = w*h*4）。
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// 实际是否存在 alpha（决定输出 RGBA8 还是 RGB8）。
    pub has_alpha: bool,
    pub exif: Option<Vec<u8>>,
    pub icc: Option<Vec<u8>>,
    pub xmp: Option<Vec<u8>>,
}

enum WebPDecoderInner<R> {
    /// image-webp 兜底：构造时统一读全量字节（做家族判定），重开 `Cursor<Vec<u8>>`。
    ImageWebp {
        dec: image_webp::WebPDecoder<Cursor<Vec<u8>>>,
        _r: PhantomData<R>,
    },
    /// 自研路径：容器 + VP8L 静态无损已解码缓冲（P2 起启用）。
    Ours(OursStatic),
}

/// WebP 解码器（支持有损/无损/带 alpha/动画；P2 起无损静态族可走自研内核）。
pub struct WebPDecoder<R> {
    inner: WebPDecoderInner<R>,
    orientation: Option<Orientation>,
}

impl<R: BufRead + Seek> WebPDecoder<R> {
    /// 构造：读全量字节 → 后端决策（自研接管 VP8L 静态无损 或 image-webp 兜底）。
    pub fn new(r: R) -> ImageResult<Self> {
        let mut data = Vec::new();
        {
            let mut br = r;
            br.read_to_end(&mut data)
                .map_err(ImageError::IoError)?;
        }

        if ours_allowed() {
            // 容器解析判定家族：VP8L 静态无损 / VP8 静态有损 → 自研接管（解码兜到
            // `decode_static_lossless` / `decode_static_lossy`；其余家族本期仍落 image-webp 兜底）。
            if let Ok(c) = container::parse(&data) {
                if c.family.is_static_lossless() {
                    if let Some(ours) = crate::codecs::webp::decode_static_lossless(&data)? {
                        return Ok(Self {
                            inner: WebPDecoderInner::Ours(ours),
                            orientation: None,
                        });
                    }
                }
                if c.family == container::Family::StaticLossy
                    || c.family == container::Family::LossyAlpha
                {
                    if let Some(ours) = crate::codecs::webp::decode_static_lossy(&data)? {
                        return Ok(Self {
                            inner: WebPDecoderInner::Ours(ours),
                            orientation: None,
                        });
                    }
                }
            }
        }

        let dec = image_webp::WebPDecoder::new(Cursor::new(data))
            .map_err(ImageError::from_kimg_webp)?;
        Ok(Self {
            inner: WebPDecoderInner::ImageWebp {
                dec,
                _r: PhantomData,
            },
            orientation: None,
        })
    }

    /// 是否为动画。
    pub fn has_animation(&self) -> bool {
        match &self.inner {
            WebPDecoderInner::ImageWebp { dec, .. } => dec.is_animated(),
            WebPDecoderInner::Ours(_) => false,
        }
    }

    /// 设置动画背景色。
    pub fn set_background_color(&mut self, color: Rgba<u8>) -> ImageResult<()> {
        match &mut self.inner {
            WebPDecoderInner::ImageWebp { dec, .. } => dec
                .set_background_color(color.0)
                .map_err(ImageError::from_kimg_webp),
            WebPDecoderInner::Ours(_) => Ok(()),
        }
    }
}

impl<R: BufRead + Seek> ImageDecoder for WebPDecoder<R> {
    fn dimensions(&self) -> (u32, u32) {
        match &self.inner {
            WebPDecoderInner::ImageWebp { dec, .. } => dec.dimensions(),
            WebPDecoderInner::Ours(o) => (o.width, o.height),
        }
    }

    fn color_type(&self) -> ColorType {
        match &self.inner {
            WebPDecoderInner::ImageWebp { dec, .. } => {
                if dec.has_alpha() {
                    ColorType::Rgba8
                } else {
                    ColorType::Rgb8
                }
            }
            WebPDecoderInner::Ours(o) => {
                if o.has_alpha {
                    ColorType::Rgba8
                } else {
                    ColorType::Rgb8
                }
            }
        }
    }

    fn exif_metadata(&mut self) -> ImageResult<Option<Vec<u8>>> {
        let exif = match &mut self.inner {
            WebPDecoderInner::ImageWebp { dec, .. } => dec
                .exif_metadata()
                .map_err(ImageError::from_kimg_webp)?,
            WebPDecoderInner::Ours(o) => o.exif.as_ref().cloned(),
        };
        self.orientation = Some(
            exif.as_ref()
                .and_then(|exif| Orientation::from_exif_chunk(exif))
                .unwrap_or(Orientation::NoTransforms),
        );
        Ok(exif)
    }

    fn icc_profile(&mut self) -> ImageResult<Option<Vec<u8>>> {
        Ok(match &mut self.inner {
            WebPDecoderInner::ImageWebp { dec, .. } => dec
                .icc_profile()
                .map_err(ImageError::from_kimg_webp)?,
            WebPDecoderInner::Ours(o) => o.icc.as_ref().cloned(),
        })
    }

    fn xmp_metadata(&mut self) -> ImageResult<Option<Vec<u8>>> {
        Ok(match &mut self.inner {
            WebPDecoderInner::ImageWebp { dec, .. } => dec
                .xmp_metadata()
                .map_err(ImageError::from_kimg_webp)?,
            WebPDecoderInner::Ours(o) => o.xmp.clone(),
        })
    }

    fn orientation(&mut self) -> ImageResult<Orientation> {
        if self.orientation.is_none() {
            let _ = self.exif_metadata()?;
        }
        Ok(self.orientation.unwrap())
    }

    fn read_image(self, buf: &mut [u8]) -> ImageResult<()> {
        assert_eq!(u64::try_from(buf.len()), Ok(self.total_bytes()));
        match self.inner {
            WebPDecoderInner::ImageWebp { mut dec, .. } => {
                dec.read_image(buf).map_err(ImageError::from_kimg_webp)
            }
            WebPDecoderInner::Ours(o) => {
                if o.has_alpha {
                    let target = (o.width as usize) * (o.height as usize) * 4;
                    if buf.len() != target {
                        return Err(ImageError::Decoding(DecodingError::new(
                            ImageFormat::WebP.into(),
                            "Ours 解码缓冲长度与像素缓冲不一致",
                        )));
                    }
                    buf.copy_from_slice(&o.rgba);
                } else {
                    // RGB8 目标：从 RGBA 抽 RGB（buf.len()==w*h*3）。
                    let target = (o.width as usize) * (o.height as usize) * 3;
                    if buf.len() != target {
                        return Err(ImageError::Decoding(DecodingError::new(
                            ImageFormat::WebP.into(),
                            "Ours 解码缓冲长度与像素缓冲不一致",
                        )));
                    }
                    for (px, out) in o.rgba.chunks_exact(4).zip(buf.chunks_exact_mut(3)) {
                        out.copy_from_slice(&px[..3]);
                    }
                }
                Ok(())
            }
        }
    }

    fn read_image_boxed(self: Box<Self>, buf: &mut [u8]) -> ImageResult<()> {
        (*self).read_image(buf)
    }
}

impl<'a, R: 'a + BufRead + Seek> AnimationDecoder<'a> for WebPDecoder<R> {
    fn loop_count(&self) -> LoopCount {
        let img = match &self.inner {
            WebPDecoderInner::ImageWebp { dec, .. } => dec,
            WebPDecoderInner::Ours(_) => return LoopCount::Finite(1.try_into().unwrap()),
        };
        match img.loop_count() {
            image_webp::LoopCount::Forever => LoopCount::Infinite,
            image_webp::LoopCount::Times(n) => LoopCount::Finite(n.into()),
        }
    }

    fn into_frames(self) -> Frames<'a> {
        struct FramesInner {
            decoder: Option<image_webp::WebPDecoder<Cursor<Vec<u8>>>>,
            current: u32,
            width: u32,
            height: u32,
        }
        impl Iterator for FramesInner {
            type Item = ImageResult<Frame>;

            fn next(&mut self) -> Option<Self::Item> {
                let dec = self.decoder.as_mut()?;
                if self.current == dec.num_frames() {
                    return None;
                }
                self.current += 1;

                let (img, delay) = if dec.has_alpha() {
                    let mut img = RgbaImage::new(self.width, self.height);
                    match dec.read_frame(img.as_mut()) {
                        Ok(delay) => (img, delay),
                        Err(image_webp::DecodingError::NoMoreFrames) => return None,
                        Err(e) => return Some(Err(ImageError::from_kimg_webp(e))),
                    }
                } else {
                    let mut img = RgbImage::new(self.width, self.height);
                    match dec.read_frame(img.as_mut()) {
                        Ok(delay) => {
                            // RGB → RGBA。
                            let mut rgba = RgbaImage::new(self.width, self.height);
                            for (x, y, p) in img.enumerate_pixels() {
                                rgba.put_pixel(x, y, Rgba([p[0], p[1], p[2], 255]));
                            }
                            (rgba, delay)
                        }
                        Err(image_webp::DecodingError::NoMoreFrames) => return None,
                        Err(e) => return Some(Err(ImageError::from_kimg_webp(e))),
                    }
                };

                Some(Ok(Frame::from_parts(
                    img,
                    0,
                    0,
                    Delay::from_numer_denom_ms(delay, 1),
                )))
            }
        }

        match self.inner {
            WebPDecoderInner::Ours(_) => {
                // 自研静态无损：非动画，无帧。
                Frames::new(Box::new(std::iter::empty()))
            }
            WebPDecoderInner::ImageWebp { dec, .. } => {
                let (width, height) = dec.dimensions();
                Frames::new(Box::new(FramesInner {
                    decoder: Some(dec),
                    current: 0,
                    width,
                    height,
                }))
            }
        }
    }
}

impl ImageError {
    pub(crate) fn from_kimg_webp(e: image_webp::DecodingError) -> ImageError {
        match e {
            image_webp::DecodingError::IoError(e) => ImageError::IoError(e),
            _ => ImageError::Decoding(DecodingError::new(ImageFormat::WebP.into(), e)),
        }
    }
}

/// 自研 VP8L 静态无损解码入口（P2）：后端允许 + 家族=StaticLossless 时调用，解析容器并解码。
/// 返回 `Some(ours)` 表示自研接管成功；`None` 表示不可接管（应回 image-webp 兜底）。
/// 自研解码出错也回 `Ok(None)`（降级 image-webp，守住「正确性优先」；后续自研稳后再考虑硬错）。
pub(crate) fn decode_static_lossless(
    data: &[u8],
) -> ImageResult<Option<crate::codecs::webp::OursStatic>> {
    let c = match container::parse(data) {
        Ok(c) => c,
        Err(_) => return Ok(None),
    };
    if !c.family.is_static_lossless() {
        return Ok(None);
    }
    let bitstream = match c.bitstream {
        Some(b) => b,
        None => return Ok(None),
    };
    let img = match vp8l::decode(bitstream) {
        Ok(i) => i,
        Err(e) => {
            // 自研解码失败 → 降级 image-webp（见上注释）。
            let _ = e;
            return Ok(None);
        }
    };
    Ok(Some(crate::codecs::webp::OursStatic {
        rgba: img.rgba,
        width: img.width,
        height: img.height,
        has_alpha: img.has_alpha,
        exif: c.exif.map(|e| e.to_vec()),
        icc: c.icc_profile.map(|e| e.to_vec()),
        xmp: c.xmp.map(|e| e.to_vec()),
    }))
}

/// 自研 VP8 静态有损解码入口（P3 有损族，V4+ 接管）：后端允许 + 家族=StaticLossy/LossyAlpha 时
/// 调用。解析容器 → VP8 YUV → fancy RGBA8（LossyAlpha 附 ALPH 反滤波 alpha）。出错/不可接管
/// 回 `Ok(None)`（降级 image-webp，正确性优先）。
pub(crate) fn decode_static_lossy(
    data: &[u8],
) -> ImageResult<Option<crate::codecs::webp::OursStatic>> {
    let c = match container::parse(data) {
        Ok(c) => c,
        Err(_) => return Ok(None),
    };
    if c.family != container::Family::StaticLossy && c.family != container::Family::LossyAlpha {
        return Ok(None);
    }
    let bitstream = match c.bitstream {
        Some(b) => b,
        None => return Ok(None),
    };
    let yuv = match vp8::decode_vp8(bitstream, c.width, c.height) {
        Ok(y) => y,
        Err(e) => {
            let _ = e;
            return Ok(None);
        }
    };
    let mut rgba = vp8::yuv_to_rgba_fancy(&yuv);
    let mut has_alpha = false;
    if c.family == container::Family::LossyAlpha {
        if let Some(alpha_range) = c.frames.first().and_then(|f| f.alpha.clone()) {
            if let Some(plane) = decode_and_unfilter_alpha(&data[alpha_range], c.width, c.height) {
                for (i, &a) in plane.iter().enumerate() {
                    rgba[i * 4 + 3] = a;
                }
                has_alpha = true;
            } else {
                // ALPH 解析失败 → 保守降级。
                return Ok(None);
            }
        }
    }
    Ok(Some(crate::codecs::webp::OursStatic {
        rgba,
        width: c.width,
        height: c.height,
        has_alpha,
        exif: c.exif.map(|e| e.to_vec()),
        icc: c.icc_profile.map(|e| e.to_vec()),
        xmp: c.xmp.map(|e| e.to_vec()),
    }))
}

/// 解码并反滤波 ALPH chunk（`alpha_chunk[0]` 为头字节）为 alpha 平面（w*h 字节）。
/// 与 C# `WebpDecoder.DecodeAndUnfilterAlpha` 同款（压缩 0/1 + 滤波 0-3 全支持）。
fn decode_and_unfilter_alpha(alpha_chunk: &[u8], width: u32, height: u32) -> Option<Vec<u8>> {
    if alpha_chunk.is_empty() {
        return None;
    }
    let header = alpha_chunk[0];
    let compression = header & 3;
    let filter = (header >> 2) & 3;
    let pixel_count = (width as usize) * (height as usize);
    let mut values = vec![0u8; pixel_count];
    match compression {
        0 => {
            if alpha_chunk.len() - 1 != pixel_count {
                return None;
            }
            values.copy_from_slice(&alpha_chunk[1..]);
        }
        1 => {
            values = match vp8l::decode_alpha(&alpha_chunk[1..], width, height) {
                Ok(v) => v,
                Err(_) => return None,
            };
        }
        _ => return None,
    }
    let width = width as usize;
    match filter {
        1 => {
            // 首个像素取上邻，其余取左邻。
            for row in 0..height as usize {
                let off = row * width;
                let above = if row == 0 { 0 } else { values[off - width] };
                values[off] = values[off].wrapping_add(above);
                for col in 1..width {
                    values[off + col] = values[off + col].wrapping_add(values[off + col - 1]);
                }
            }
        }
        2 => {
            // 首行水平铺，其后各像素取上邻。
            for row in 0..height as usize {
                let off = row * width;
                if row == 0 {
                    for col in 1..width {
                        values[off + col] = values[off + col].wrapping_add(values[off + col - 1]);
                    }
                } else {
                    let above_off = off - width;
                    for col in 0..width {
                        values[off + col] = values[off + col].wrapping_add(values[above_off + col]);
                    }
                }
            }
        }
        3 => {
            // 梯度：clamp(左 + 上 - 左上)。
            for row in 0..height as usize {
                let off = row * width;
                if row == 0 {
                    for col in 1..width {
                        values[off + col] = values[off + col].wrapping_add(values[off + col - 1]);
                    }
                } else {
                    let above_off = off - width;
                    let mut above = values[above_off];
                    values[off] = values[off].wrapping_add(above);
                    for col in 1..width {
                        let left = values[off + col - 1];
                        let top = values[above_off + col];
                        let predictor = (i32::from(left) + i32::from(top) - i32::from(above)).clamp(0, 255);
                        values[off + col] = values[off + col].wrapping_add(predictor as u8);
                        above = top;
                    }
                }
            }
        }
        _ => {}
    }
    Some(values)
}

#[cfg(test)]
mod tests {
    // backend 开关单测（不读环境变量路径：直设后断言 resolve）。
    use super::{resolved_backend, set_webp_backend, BACKEND_AUTO, BACKEND_IMAGEWEBP, BACKEND_OURS};

    #[test]
    fn backend_setter_and_resolve() {
        set_webp_backend(BACKEND_OURS);
        assert_eq!(resolved_backend(), BACKEND_OURS);
        set_webp_backend(BACKEND_IMAGEWEBP);
        assert_eq!(resolved_backend(), BACKEND_IMAGEWEBP);
        // 复位 AUTO：环境变量未设时解析回 AUTO（解析仅一次）。
        set_webp_backend(BACKEND_AUTO);
        let rb = resolved_backend();
        assert!(rb == BACKEND_AUTO || rb == BACKEND_OURS || rb == BACKEND_IMAGEWEBP);
    }
}

#[cfg(test)]
mod metadata_tests {
    //! P1 容器元数据对照：自研 `container::parse` vs image-webp（经 kimg `WebPDecoder`）在
    //! 全部 fixture 上一致；畸形输入不 panic 不 OOM。放在 crate 内测试（container 为 pub(crate)）。

    use std::io::Cursor;

    use super::container;
    use super::webp_fixtures_support;
    use crate::{AnimationDecoder, ImageDecoder};

    // golden TSV 保留在仓内；图片取外部语料（tests/fixtures/webp/mod.rs 定位）。

    fn all_fixtures() -> Vec<std::path::PathBuf> {
        // 从外部语料目录按名收集（含用户语料 / imgbench-corpus / SimdCodecs 夹具）。
        let mut names = webp_fixtures_support::webp_fixture_names();
        names.sort();
        names
            .into_iter()
            .filter_map(|n| webp_fixtures_support::webp_fixture_path(&n))
            .collect()
    }

    /// image-webp 视图；解析失败返回 None（供 SKIP 分流）。
    fn imagewebp_view(data: &[u8]) -> Option<(u32, u32, bool, bool, u32, bool, bool, bool)> {
        let mut dec = crate::codecs::webp::WebPDecoder::new(Cursor::new(data)).ok()?;
        let (w, h) = dec.dimensions();
        let alpha = dec.color_type() == crate::ColorType::Rgba8;
        let animated = dec.has_animation();
        let loop_count = if animated {
            match dec.loop_count() {
                crate::LoopCount::Infinite => 0,
                crate::LoopCount::Finite(n) => n.get(),
            }
        } else {
            0
        };
        let exif = dec.exif_metadata().ok().flatten().is_some();
        let icc = dec.icc_profile().ok().flatten().is_some();
        let xmp = dec.xmp_metadata().ok().flatten().is_some();
        Some((w, h, alpha, animated, loop_count, exif, icc, xmp))
    }

    #[test]
    fn container_metadata_matches_imagewebp_across_corpus() {
        let mut checked = 0usize;
        let mut skipped = 0usize;
        let mut mismatches = Vec::new();
        for path in all_fixtures() {
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            let data = std::fs::read(&path).unwrap();
            let c = match container::parse(&data) {
                Ok(c) => c,
                Err(e) => {
                    // 外部语料可能含本实现容器不接的文件 → SKIP 不判失败。
                    eprintln!("SKIP {name} 容器解析失败 {e:?}");
                    skipped += 1;
                    continue;
                }
            };
            let Some((dw, dh, dalpha, danim, dloop, dexif, dicc, dxmp)) = imagewebp_view(&data)
            else {
                eprintln!("SKIP {name} image-webp 解析失败");
                skipped += 1;
                continue;
            };
            if (c.width, c.height) != (dw, dh) {
                mismatches.push(format!(
                    "{name}: dims 容器 {}x{} vs image-webp {}x{}",
                    c.width, c.height, dw, dh
                ));
            }
            if (c.family == container::Family::Animated) != danim {
                mismatches.push(format!(
                    "{name}: animated 容器 {} vs image-webp {danim}",
                    c.family == container::Family::Animated
                ));
            }
            // 动画 has_alpha 是启发式且引擎各异（SimdCodecs 按首帧覆盖/不透明/背景置位推导，
            // image-webp 按自身帧序）→ 动画仅比对静态元数据；静态严格比对。
            if !danim && c.has_alpha != dalpha {
                mismatches.push(format!(
                    "{name}: has_alpha 容器 {} vs image-webp {}",
                    c.has_alpha, dalpha
                ));
            }
            if danim {
                let clo = if c.loop_count == 0 { 0 } else { c.loop_count };
                if clo != dloop {
                    mismatches.push(format!(
                        "{name}: loop_count 容器 {clo} vs image-webp {dloop}"
                    ));
                }
            }
            let e1 = c.exif.is_some();
            if e1 != dexif {
                mismatches.push(format!("{name}: exif 存在性 容器 {e1} vs image-webp {dexif}"));
            }
            if c.icc_profile.is_some() != dicc {
                mismatches.push(format!(
                    "{name}: icc 存在性 容器 {} vs image-webp {dicc}",
                    c.icc_profile.is_some()
                ));
            }
            if c.xmp.is_some() != dxmp {
                mismatches.push(format!("{name}: xmp 存在性 容器 {} vs image-webp {dxmp}", c.xmp.is_some()));
            }
            checked += 1;
        }
        for m in &mismatches {
            eprintln!("WEBP_CONTAINER_MISMATCH\t{m}");
        }
        assert!(
            mismatches.is_empty(),
            "{} 处容器元数据与 image-webp 不一致（见上）",
            mismatches.len()
        );
        eprintln!("WEBP_CONTAINER_CHECKED\t{checked}, skipped={skipped}");
        // 金标准（SimdCodecs 60 张）几乎全覆；外部语料缺失属常态。
        assert!(checked >= 59, "corpus 应几乎全覆（SimdCodecs 60 张 + 额外语料），当前 {checked}");
    }

    #[test]
    fn container_rejects_malformed_without_panic() {
        for c in [
            &[][..],
            b"RIFF".as_slice(),
            b"RIFF\x00\x00\x00\x00WEBP".as_slice(),
            b"RIFF\xff\xff\xff\x0fWEBPVP8L".as_slice(),
            &[0x52, 0x49, 0x46, 0x46, 0x04, 0x00, 0x00, 0x00, b'W', b'E', b'B', b'P', 0xff, 0xff, 0xff, 0xff][..],
        ] {
            let _ = container::parse(c); // 不 panic 即通过
            let _ = crate::codecs::webp::WebPDecoder::new(Cursor::new(c)); // 兜底路径不 panic
        }
    }

    #[test]
    fn container_family_classification_on_real_fixtures() {
        let mut counts: std::collections::BTreeMap<&'static str, usize> = Default::default();
        for path in all_fixtures() {
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            let data = std::fs::read(&path).unwrap();
            let label = match container::family_of(&data) {
                Some(container::Family::StaticLossy) => "static-lossy",
                Some(container::Family::StaticLossless) => "static-lossless",
                Some(container::Family::StaticLosslessAlpha) => "static-lossless-alpha",
                Some(container::Family::LossyAlpha) => "lossy-alpha",
                Some(container::Family::Animated) => "animated",
                None => "NONE",
            };
            *counts.entry(label).or_insert(0) += 1;
            eprintln!("WEBP_CONTAINER_FAMILY\t{name}\t{label}");
        }
        for (k, v) in &counts {
            eprintln!("WEBP_CONTAINER_FAMILY_SUMMARY\t{k}={v}");
        }
        // 8 个动画 fixture 必须全部判为动画（外部语料若含更多动画则只加不减）。
        assert!(
            counts.get("animated").copied().unwrap_or(0) >= 8,
            "动画夹具家族判定失败"
        );
    }
}

/// ALPH 滤波 1/2/3 parity（V6 收口补盲）：corpus 的 7 个有损-alpha fixture 全部 filter=0
/// （含 comp=0/1），滤波 1（水平）/2（垂直）/3（梯度）路径从未被覆盖——此测试以合成
/// VP8X+ALPH+VP8 容器（compression=0）+ 编码侧滤波字节，对 OURS/IMAGEWEBP 双后端解码
/// 逐字节对拍 + alpha 通道与真实 alpha 平面回比（K27：证明性实验落仓可复跑）。
#[cfg(test)]
mod alpha_filter_tests {
    use std::io::Cursor;

    use super::{
        container, decode_and_unfilter_alpha, set_webp_backend, WebPDecoder, BACKEND_AUTO,
        BACKEND_IMAGEWEBP, BACKEND_OURS,
    };
    use super::webp_fixtures_support;
    use crate::{ColorType, ImageDecoder};

    fn le24(v: u32) -> [u8; 3] {
        [v as u8, (v >> 8) as u8, (v >> 16) as u8]
    }

    /// libwebp 编码侧滤波（`DoHorizontalFilter_C` / `DoVerticalFilter_C` / `DoGradientFilter_C`）
    /// 的 wrap 减法复刻：`filtered = src - predictor`（模 256）。
    fn filter_encode(src: &[u8], w: usize, filter: u8) -> Vec<u8> {
        let h = src.len() / w;
        let mut out = src.to_vec();
        match filter {
            1 => {
                for c in 1..w {
                    out[c] = src[c].wrapping_sub(src[c - 1]);
                }
                for r in 1..h {
                    let off = r * w;
                    out[off] = src[off].wrapping_sub(src[off - w]);
                    for c in 1..w {
                        out[off + c] = src[off + c].wrapping_sub(src[off + c - 1]);
                    }
                }
            }
            2 => {
                for c in 1..w {
                    out[c] = src[c].wrapping_sub(src[c - 1]);
                }
                for r in 1..h {
                    let off = r * w;
                    for c in 0..w {
                        out[off + c] = src[off + c].wrapping_sub(src[off - w + c]);
                    }
                }
            }
            3 => {
                for c in 1..w {
                    out[c] = src[c].wrapping_sub(src[c - 1]);
                }
                for r in 1..h {
                    let off = r * w;
                    out[off] = src[off].wrapping_sub(src[off - w]);
                    for c in 1..w {
                        let pred =
                            (i32::from(src[off + c - 1]) + i32::from(src[off - w + c])
                                - i32::from(src[off - w + c - 1]))
                            .clamp(0, 255) as u8;
                        out[off + c] = src[off + c].wrapping_sub(pred);
                    }
                }
            }
            _ => {}
        }
        out
    }

    fn build_container(vp8: &[u8], w: u32, h: u32, alpha: &[u8], filter: u8) -> Vec<u8> {
        let mut vp8x = vec![0x10u8, 0, 0, 0]; // alpha flag
        vp8x.extend_from_slice(&le24(w - 1));
        vp8x.extend_from_slice(&le24(h - 1));
        // compression=0 + filter（低 2 位压缩、2-3 位滤波），后接 w*h 原始滤波字节。
        let mut alph = vec![filter << 2];
        alph.extend_from_slice(alpha);
        let mut chunks = Vec::new();
        let mut push_chunk = |tag: &[u8; 4], payload: &[u8], chunks: &mut Vec<u8>| {
            chunks.extend_from_slice(tag);
            chunks.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            chunks.extend_from_slice(payload);
            if payload.len() % 2 == 1 {
                chunks.push(0);
            }
        };
        push_chunk(b"VP8X", &vp8x, &mut chunks);
        push_chunk(b"ALPH", &alph, &mut chunks);
        push_chunk(b"VP8 ", vp8, &mut chunks);
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&((chunks.len() + 4) as u32).to_le_bytes());
        out.extend_from_slice(b"WEBP");
        out.extend_from_slice(&chunks);
        out
    }

    fn decode_rgba(data: &[u8], ours: bool) -> (Vec<u8>, bool) {
        set_webp_backend(if ours { BACKEND_OURS } else { BACKEND_IMAGEWEBP });
        let dec = WebPDecoder::new(Cursor::new(data)).expect("WebPDecoder::new 应成功");
        let has_a = dec.color_type() == ColorType::Rgba8;
        let mut buf = vec![0u8; dec.total_bytes() as usize];
        dec.read_image(&mut buf).expect("read_image 应成功");
        (buf, has_a)
    }

    #[test]
    fn lossy_alpha_filters_1_to_3_match_imagewebp() {
        let Some(p) = webp_fixtures_support::webp_fixture_path("lossy-alpha-q50.webp") else {
            eprintln!("SKIP lossy-alpha-q50.webp 外部语料缺失");
            return;
        };
        let data = std::fs::read(p).expect("读取夹具");
        let c = container::parse(&data).expect("容器解析");
        assert_eq!(c.family, container::Family::LossyAlpha);
        let vp8 = c.bitstream.unwrap().to_vec();
        let (w, h) = (c.width, c.height);
        let alpha_range = c.frames[0].alpha.clone().expect("alpha 范围");
        // 参考 alpha 平面：q50 夹具为 comp=1 + filter=0，`decode_and_unfilter_alpha` 直解。
        let true_alpha = decode_and_unfilter_alpha(&data[alpha_range], w, h).expect("参考 alpha 解码");
        assert_eq!(true_alpha.len(), (w as usize) * (h as usize));

        for filter in 1..=3 {
            let filtered = filter_encode(&true_alpha, w as usize, filter);
            let bytes = build_container(&vp8, w, h, &filtered, filter);
            let (ours, a1) = decode_rgba(&bytes, true);
            let (img, a2) = decode_rgba(&bytes, false);
            assert!(a1 && a2, "filter={filter} 双后端均应判为 RGBA8");
            assert_eq!(ours.len(), img.len(), "filter={filter} 缓冲长");
            assert_eq!(ours, img, "filter={filter} OURS vs image-webp 逐字节");
            for (i, &a) in true_alpha.iter().enumerate() {
                assert_eq!(ours[i * 4 + 3], a, "filter={filter} alpha 通道 {i}");
            }
            eprintln!("VP8_ALPHA_FILTER_PARITY_OK\tfilter={filter}\t({w}x{h})");
        }
        set_webp_backend(BACKEND_AUTO);
    }
}
