//! WebP RIFF 容器解析（移植自 SimdCodecs `WebpContainer.cs`，MIT）。
//!
//! 职责：RIFF chunk 四字节分发、VP8X features/ancillary、VP8 尺寸、VP8L 尺寸、ALPH 参数、
//! ANIM loop/bg、ANMF 偏移 geometry、ICCP/EXIF/XMP 标注，产出 `WebpContainerFrame` 列表 +
//! 家族判定（StaticLossy / StaticLossless / StaticLosslessAlpha / Animated / LossyAlpha）+ 元数据。
//!
//! 输入安全清单（K42）：
//! - 所有切片越界/长度校验在进入解析前完成（`data` 直接切割，所有 `slice` 先验证长度）；
//! - 尺寸乘法一律 `checked_mul`，超 `MAX_PIXELS`（1<<26）在头部解析即拒绝；
//! - chunk 声明长度 > 数据包剩余 → 中止（不 OOM、不 panic）。
//! 解析失败返回 `ContainerError`（结构化），调用方按「不可接管 → 回 image-webp 兜底」处理。

/// 像素上限（对齐 SimdCodecs `WebpLimits.MaximumPixels = 1<<26`）。
pub(crate) const MAX_PIXELS: u64 = 1u64 << 26;

/// 图像编码格式（静态时：有损/无损；动画时恒 Animated）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WebpFormat {
    Lossy,
    Lossless,
    Animated,
}

/// 家族判定（M3：对应 G 位语义；VP8X features + 是否含 ANIM/ALPH/VP8L 判定）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Family {
    /// VP8（有损），无 alpha。
    StaticLossy,
    /// VP8L（无损），无 alpha。
    StaticLossless,
    /// VP8L（无损），带 alpha。
    StaticLosslessAlpha,
    /// VP8 + ALPH（有损带 alpha，静态）。
    LossyAlpha,
    /// VP8X + ANIM（动画）。
    Animated,
}

impl Family {
    /// 是否静态无损（VP8L 家族，P2 自研接管范围）。
    pub(crate) fn is_static_lossless(self) -> bool {
        matches!(self, Family::StaticLossless | Family::StaticLosslessAlpha)
    }
}

/// 插值风格（M2 标注用）：本阶段不参与解码路径选择，P3 有损族接入时按 fancy 列对齐。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum FormatHint {
    /// Fancy（dwebp 默认；image-webp Bilinear 对齐列）。
    Fancy,
    /// 无损（无上采样）。
    Lossless,
}

/// 单帧（≈ SimdCodecs `WebpFrameData`；本仓用 Range 表达 bitstream/alpha 在整文件中的绝对位置）。
/// 字段由 P2（vp8l 解码）/ P6（动画合成）阶段消费；P0-P1 阶段仅“解析 + 家族判定”，故字段级
/// dead_code 静默。
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(crate) struct WebpContainerFrame {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub duration_ms: u32,
    /// false 即 ANMF BLEND 位被置位（0x02=不混合，见 RFC9649）。
    pub blend: bool,
    pub dispose_to_background: bool,
    pub format: WebpFormat,
    pub bitstream: std::ops::Range<usize>,
    pub alpha: Option<std::ops::Range<usize>>,
    pub has_alpha: bool,
}

/// 解析结果（含静态主 bitstream 与元数据）。
#[derive(Debug)]
#[allow(dead_code)]
pub(crate) struct WebpContainer<'a> {
    pub width: u32,
    pub height: u32,
    /// 启发式 alpha 判定（含首帧几何/后续 dispose 推导，见下方 `has_alpha` 计算）。
    pub has_alpha: bool,
    /// VP8X features 原始 alpha 位（0x10）。与 image-webp `has_alpha()` 口径一致（动画输出
    /// RGBA8 vs RGB8 的判定键）。动画时 `has_alpha` 可能与该位不一致（启发式差异），
    /// 故单独暴露供动画阶段复刻 image-webp 语义。
    pub alpha_flag: bool,
    pub format: WebpFormat,
    pub frame_count: u32,
    pub family: Family,
    pub frames: Vec<WebpContainerFrame>,
    /// 静态主 bitstream（VP8/VP8L 载荷）。
    pub bitstream: Option<&'a [u8]>,
    pub icc_profile: Option<&'a [u8]>,
    pub exif: Option<&'a [u8]>,
    pub xmp: Option<&'a [u8]>,
    pub has_alpha_chunk: bool,
    pub animation_background_color: u32,
    pub loop_count: u32,
}

/// 容器解析错误（不携带堆分配；`<- move` 语义便于上层轻量处理）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContainerError {
    NotWebp,
    RiffSizeExceedsData,
    ChunkHeaderTruncated,
    ChunkExceedsContainer,
    InvalidVp8x,
    MultipleBitstreams,
    InvalidVp8Header,
    Vp8PartitionExceedsFrame,
    InvalidVp8lHeader,
    MultipleAlpha,
    MultipleIcc,
    MultipleExif,
    MultipleXmp,
    InvalidAnim,
    MultipleAnim,
    AnmfRequiresVp8x,
    FrameHeaderTruncated,
    ReservedFlagsSet,
    FrameExceedsCanvas,
    SubchunkHeaderTruncated,
    SubchunkExceedsFrame,
    AlphaSubchunkMisplaced,
    FrameMultipleBitstreams,
    FrameDimensionMismatch,
    NoValidBitstream,
    PixelLimitExceeded,
    AnimRequiresFlag,
    AnimationIncomplete,
    FramesWithoutFlag,
    CanvasBitstreamMismatch,
    Vp8lVersionUnsupported,
}

/// 断言 chunk 载荷在数据包内并切出（输入安全：越界即 Err，不 panic）。
fn chunk_payload<'a>(data: &'a [u8], offset: usize, end: usize, what: ContainerError) -> Result<(&'a [u8], usize), ContainerError> {
    if offset + 8 > end {
        return Err(what);
    }
    let size_bytes = [data[offset + 4], data[offset + 5], data[offset + 6], data[offset + 7]];
    let len = u32::from_le_bytes(size_bytes) as usize;
    let payload_end = offset + 8 + len;
    let next = payload_end + (len & 1);
    if payload_end > end || next > end {
        return Err(ContainerError::ChunkExceedsContainer);
    }
    let payload = &data[offset + 8..payload_end];
    Ok((payload, next))
}

fn read_u24(p: &[u8]) -> u32 {
    u32::from(p[0]) | (u32::from(p[1]) << 8) | (u32::from(p[2]) << 16)
}

/// 读 VP8 key-frame 头（尺寸 + 首分区，校验 magic 与分区越界）。
fn read_vp8_dimensions(payload: &[u8]) -> Result<(u32, u32), ContainerError> {
    if payload.len() < 10 || (payload[0] & 1) != 0 || payload[3] != 0x9d || payload[4] != 0x01 || payload[5] != 0x2a {
        return Err(ContainerError::InvalidVp8Header);
    }
    let width = u16::from_le_bytes([payload[6], payload[7]]) as u32 & 0x3fff;
    let height = u16::from_le_bytes([payload[8], payload[9]]) as u32 & 0x3fff;
    if width == 0 || height == 0 {
        return Err(ContainerError::InvalidVp8Header);
    }
    let first_partition = (u32::from(payload[0]) | (u32::from(payload[1]) << 8) | (u32::from(payload[2]) << 16)) >> 5;
    if first_partition as usize > payload.len() - 10 {
        return Err(ContainerError::Vp8PartitionExceedsFrame);
    }
    Ok((width, height))
}

/// 读 VP8L 头（尺寸 + alpha 位 + version 校验）。
pub(crate) fn read_vp8l_dimensions(payload: &[u8]) -> Result<(u32, u32, bool), ContainerError> {
    if payload.len() < 5 || payload[0] != 0x2f {
        return Err(ContainerError::InvalidVp8lHeader);
    }
    let bits = u32::from_le_bytes([payload[1], payload[2], payload[3], payload[4]]);
    let width = 1 + (bits & 0x3fff);
    let height = 1 + ((bits >> 14) & 0x3fff);
    let has_alpha = ((bits >> 28) & 1) != 0;
    if (bits >> 29) != 0 {
        return Err(ContainerError::Vp8lVersionUnsupported);
    }
    Ok((width, height, has_alpha))
}

/// 校验容器整体像素上限（尺寸乘法 checked）。
fn check_max_pixels(w: u64, h: u64) -> Result<(), ContainerError> {
    let px = w.checked_mul(h).ok_or(ContainerError::PixelLimitExceeded)?;
    if px > MAX_PIXELS {
        return Err(ContainerError::PixelLimitExceeded);
    }
    Ok(())
}

/// 解析 ANMF 帧（子块内 VP8/VP8L 载荷位置以「整文件绝对偏移」记录，供动画阶段取数据）。
fn read_animation_frame(
    payload: &[u8],
    payload_offset: usize,
    canvas_width: u32,
    canvas_height: u32,
    frames: &mut Vec<WebpContainerFrame>,
) -> Result<(), ContainerError> {
    if payload.len() < 16 {
        return Err(ContainerError::FrameHeaderTruncated);
    }
    let x = read_u24(&payload[..3]) * 2;
    let y = read_u24(&payload[3..6]) * 2;
    let width = read_u24(&payload[6..9]) + 1;
    let height = read_u24(&payload[9..12]) + 1;
    let duration = read_u24(&payload[12..15]);
    let flags = payload[15];
    if (flags & 0xfc) != 0 {
        return Err(ContainerError::ReservedFlagsSet);
    }
    if u64::from(x) + u64::from(width) > u64::from(canvas_width)
        || u64::from(y) + u64::from(height) > u64::from(canvas_height)
    {
        return Err(ContainerError::FrameExceedsCanvas);
    }

    let mut bitstream: Option<std::ops::Range<usize>> = None;
    let mut alpha: Option<std::ops::Range<usize>> = None;
    let mut format = WebpFormat::Lossy;
    let mut has_alpha = false;
    let chunks = &payload[16..];
    let mut off = 0usize;
    while off < chunks.len() {
        let (chunk_payload, next) =
            chunk_payload(chunks, off, chunks.len(), ContainerError::SubchunkHeaderTruncated)
                // ANMF 子块语境下的越界归为「子块越界」（顶层仍是 ChunkExceedsContainer）。
                .map_err(|e| {
                    if e == ContainerError::ChunkExceedsContainer {
                        ContainerError::SubchunkExceedsFrame
                    } else {
                        e
                    }
                })?;
        let abs_payload_start = payload_offset + 16 + off + 8;
        let ty = &chunks[off..off + 4];
        let abs_range = abs_payload_start..abs_payload_start + chunk_payload.len();
        if ty == b"ALPH" {
            if alpha.is_some() || bitstream.is_some() {
                return Err(ContainerError::AlphaSubchunkMisplaced);
            }
            alpha = Some(abs_range);
            has_alpha = true;
        } else if ty == b"VP8 " || ty == b"VP8L" {
            if bitstream.is_some() {
                return Err(ContainerError::FrameMultipleBitstreams);
            }
            let (iw, ih, la) = if ty == b"VP8 " {
                let (w, h) = read_vp8_dimensions(chunk_payload)?;
                format = WebpFormat::Lossy;
                (w, h, false)
            } else {
                let (w, h, a) = read_vp8l_dimensions(chunk_payload)?;
                format = WebpFormat::Lossless;
                (w, h, a)
            };
            if iw != width || ih != height {
                return Err(ContainerError::FrameDimensionMismatch);
            }
            has_alpha |= la;
            bitstream = Some(abs_range);
        }
        off = next;
    }
    if bitstream.is_none() {
        return Err(ContainerError::NoValidBitstream);
    }
    if format == WebpFormat::Lossless && alpha.is_some() {
        return Err(ContainerError::AlphaSubchunkMisplaced); // VP8L 帧不可带独立 ALPH
    }
    frames.push(WebpContainerFrame {
        x,
        y,
        width,
        height,
        duration_ms: duration,
        blend: (flags & 0x02) == 0,
        dispose_to_background: (flags & 0x01) != 0,
        format,
        bitstream: bitstream.unwrap(),
        alpha,
        has_alpha,
    });
    Ok(())
}

/// 解析完整 WebP 容器；失败返回结构化错误。
pub(crate) fn parse(data: &[u8]) -> Result<WebpContainer<'_>, ContainerError> {
    if data.len() < 12 || &data[..4] != b"RIFF" || &data[8..12] != b"WEBP" {
        return Err(ContainerError::NotWebp);
    }
    let riff_len = u32::from_le_bytes([data[4], data[5], data[6], data[7]]) as u64;
    let riff_end_long = 8u64 + riff_len;
    if riff_len < 4 || riff_end_long > data.len() as u64 {
        return Err(ContainerError::RiffSizeExceedsData);
    }
    let riff_end = riff_end_long as usize;

    let mut extended = false;
    let mut animation_flag = false;
    let mut alpha_flag = false;
    let mut has_alpha_chunk = false;
    let mut has_anim_chunk = false;
    let mut canvas_w = 0u32;
    let mut canvas_h = 0u32;
    let mut image_w = 0u32;
    let mut image_h = 0u32;
    let mut primary: Option<std::ops::Range<usize>> = None;
    let mut primary_lossless = false;
    let mut primary_has_alpha = false;
    let mut icc: Option<std::ops::Range<usize>> = None;
    let mut exif: Option<std::ops::Range<usize>> = None;
    let mut xmp: Option<std::ops::Range<usize>> = None;
    let mut alpha_chunk: Option<std::ops::Range<usize>> = None;
    let mut anim_bg = 0u32;
    let mut loop_count = 0u32;
    let mut frames: Vec<WebpContainerFrame> = Vec::new();

    let mut offset = 12usize;
    while offset < riff_end {
        let (payload, next) =
            chunk_payload(data, offset, riff_end, ContainerError::ChunkHeaderTruncated)?;
        let ty = &data[offset..offset + 4];
        let abs_start = offset + 8;
        let abs_range = abs_start..abs_start + payload.len();
        if ty == b"VP8X" {
            if extended || payload.len() < 10 {
                return Err(ContainerError::InvalidVp8x);
            }
            extended = true;
            let flags = payload[0];
            animation_flag = (flags & 0x02) != 0;
            alpha_flag = (flags & 0x10) != 0;
            canvas_w = 1 + read_u24(&payload[4..7]);
            canvas_h = 1 + read_u24(&payload[7..10]);
        } else if ty == b"VP8 " {
            if primary.is_some() {
                return Err(ContainerError::MultipleBitstreams);
            }
            let (w, h) = read_vp8_dimensions(payload)?;
            image_w = w;
            image_h = h;
            if !extended {
                canvas_w = image_w;
                canvas_h = image_h;
            }
            primary = Some(abs_range);
        } else if ty == b"VP8L" {
            if primary.is_some() {
                return Err(ContainerError::MultipleBitstreams);
            }
            let (w, h, la) = read_vp8l_dimensions(payload)?;
            image_w = w;
            image_h = h;
            primary_has_alpha = la;
            if !extended {
                canvas_w = image_w;
                canvas_h = image_h;
            }
            alpha_flag |= la;
            primary = Some(abs_range);
            primary_lossless = true;
        } else if ty == b"ALPH" {
            if has_alpha_chunk {
                return Err(ContainerError::MultipleAlpha);
            }
            has_alpha_chunk = true;
            alpha_chunk = Some(abs_range);
        } else if ty == b"ICCP" {
            if icc.is_some() {
                return Err(ContainerError::MultipleIcc);
            }
            icc = Some(abs_range);
        } else if ty == b"EXIF" {
            if exif.is_some() {
                return Err(ContainerError::MultipleExif);
            }
            exif = Some(abs_range);
        } else if ty == b"XMP " {
            if xmp.is_some() {
                return Err(ContainerError::MultipleXmp);
            }
            xmp = Some(abs_range);
        } else if ty == b"ANIM" {
            if has_anim_chunk {
                return Err(ContainerError::MultipleAnim);
            }
            if payload.len() != 6 {
                return Err(ContainerError::InvalidAnim);
            }
            has_anim_chunk = true;
            anim_bg = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
            loop_count = u16::from_le_bytes([payload[4], payload[5]]) as u32;
        } else if ty == b"ANMF" {
            if !extended {
                return Err(ContainerError::AnmfRequiresVp8x);
            }
            read_animation_frame(payload, abs_start, canvas_w, canvas_h, &mut frames)?;
        }
        offset = next;
    }

    if offset != riff_end || canvas_w == 0 || canvas_h == 0 {
        return Err(ContainerError::NoValidBitstream);
    }
    // 头部解析即校验像素上限（K42）。
    check_max_pixels(u64::from(canvas_w), u64::from(canvas_h))?;

    let animated = animation_flag;
    if has_anim_chunk && !animated {
        return Err(ContainerError::AnimRequiresFlag);
    }
    if animated && (!has_anim_chunk || frames.is_empty()) {
        return Err(ContainerError::AnimationIncomplete);
    }
    if !animated && !frames.is_empty() {
        return Err(ContainerError::FramesWithoutFlag);
    }
    if !animated && primary.is_none() {
        return Err(ContainerError::NoValidBitstream);
    }
    // 静态 VP8X：画布必须与 bitstream 自身尺寸一致（防 1x1 膨胀到任意画布，libwebp 拒绝）。
    if !animated && extended && (image_w != canvas_w || image_h != canvas_h) {
        return Err(ContainerError::CanvasBitstreamMismatch);
    }

    let still = if !animated {
        let fmt = if primary_lossless { WebpFormat::Lossless } else { WebpFormat::Lossy };
        let alpha = alpha_chunk.or_else(|| None);
        Some(WebpContainerFrame {
            x: 0,
            y: 0,
            width: canvas_w,
            height: canvas_h,
            duration_ms: 0,
            blend: true,
            dispose_to_background: false,
            format: fmt,
            bitstream: primary.clone().unwrap(),
            alpha,
            has_alpha: primary_has_alpha || has_alpha_chunk,
        })
    } else {
        None
    };

    let has_alpha = alpha_flag
        || has_alpha_chunk
        || if animated {
            frames.iter().any(|f| f.has_alpha)
                || frames[0].x != 0
                || frames[0].y != 0
                || frames[0].width != canvas_w
                || frames[0].height != canvas_h
                || frames[..frames.len() - 1].iter().any(|f| f.dispose_to_background)
        } else {
            still.as_ref().map(|f| f.has_alpha).unwrap_or(false)
        };

    let format = if animated {
        WebpFormat::Animated
    } else if primary_lossless {
        WebpFormat::Lossless
    } else {
        WebpFormat::Lossy
    };
    let frame_count = if animated { frames.len() as u32 } else { 1 };

    let family = if animated {
        Family::Animated
    } else if primary_lossless {
        if primary_has_alpha || has_alpha_chunk || alpha_flag {
            Family::StaticLosslessAlpha
        } else {
            Family::StaticLossless
        }
    } else if primary_has_alpha || has_alpha_chunk || alpha_flag {
        Family::LossyAlpha
    } else {
        Family::StaticLossy
    };

    let frames_out = if animated { frames } else { vec![still.unwrap()] };

    Ok(WebpContainer {
        width: canvas_w,
        height: canvas_h,
        has_alpha,
        alpha_flag,
        format,
        frame_count,
        family,
        frames: frames_out,
        bitstream: primary.map(|r| &data[r]),
        icc_profile: icc.map(|r| &data[r]),
        exif: exif.map(|r| &data[r]),
        xmp: xmp.map(|r| &data[r]),
        has_alpha_chunk,
        animation_background_color: anim_bg,
        loop_count,
    })
}

/// 只读家族（供元数据测试/诊断；P2 解码走 `parse` 全量结果）。
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn family_of(data: &[u8]) -> Option<Family> {
    parse(data).ok().map(|c| c.family)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vp8l_bytes(w: u32, h: u32, alpha: bool, body: &[u8]) -> Vec<u8> {
        let mut bit = Vec::new();
        bit.push(0x2f);
        let ver_alpha = if alpha { 1u32 << 28 } else { 0 };
        let bits = ((w - 1) & 0x3fff) | ((((h - 1) & 0x3fff) as u32) << 14) | ver_alpha;
        bit.extend_from_slice(&bits.to_le_bytes());
        bit.extend_from_slice(body);
        bit
    }

    fn assemble(extended: Option<(&[u8], bool)>, tags: &[(&[u8; 4], &[u8])]) -> Vec<u8> {
        let mut chunks: Vec<u8> = Vec::new();
        if let Some((vp8x, _)) = extended {
            chunks.extend_from_slice(b"VP8X");
            chunks.extend_from_slice(&10u32.to_le_bytes());
            chunks.extend_from_slice(vp8x);
        }
        for (tag, payload) in tags {
            chunks.extend_from_slice(*tag);
            chunks.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            chunks.extend_from_slice(payload);
            if payload.len() % 2 == 1 {
                chunks.push(0);
            }
        }
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&((chunks.len() + 4) as u32).to_le_bytes());
        out.extend_from_slice(b"WEBP");
        out.extend_from_slice(&chunks);
        out
    }

    #[test]
    fn parse_static_lossless_no_alpha() {
        // VP8L 裸：2x3 无 alpha。
        let bit = vp8l_bytes(2, 3, false, &[0; 3]);
        let data = assemble(None, &[(&b"VP8L".clone(), &bit)]);
        let c = parse(&data).unwrap();
        assert_eq!(c.width, 2);
        assert_eq!(c.height, 3);
        assert_eq!(c.family, Family::StaticLossless);
        assert!(!c.has_alpha);
        assert_eq!(c.frames.len(), 1);
    }

    #[test]
    fn parse_static_lossless_alpha() {
        let bit = vp8l_bytes(1, 1, true, &[0; 1]);
        let data = assemble(None, &[(&b"VP8L".clone(), &bit)]);
        let c = parse(&data).unwrap();
        assert_eq!(c.family, Family::StaticLosslessAlpha);
        assert!(c.has_alpha);
    }

    #[test]
    fn parse_vp8x_lossless_alpha_flag() {
        // VP8X alpha 位（0x10）：画布 2x2（width-1/h-1 = 1,1 24-bit LE）。
        let vp8x = [0x10, 0, 0, 0, 1, 0, 0, 1, 0, 0];
        let bit = vp8l_bytes(2, 2, false, &[0; 3]);
        let data = assemble(Some((&vp8x, false)), &[(&b"VP8L".clone(), &bit)]);
        let c = parse(&data).unwrap();
        assert_eq!(c.family, Family::StaticLosslessAlpha);
        assert!(c.has_alpha);
    }

    #[test]
    fn parse_animated() {
        // VP8X animation 位（0x02）：画布 16x2（width-1=15, height-1=1 24-bit LE）。
        let vp8x = [0x02, 0, 0, 0, 15, 0, 0, 1, 0, 0];
        let bit = vp8l_bytes(4, 2, false, &[0; 10]);
        let anim = [0x00u8, 0x00, 0x00, 0x00, 0x00, 0x00]; // bg+loop
        // ANMF payload: x=0,y=0,w=4,h=2,dur=100,flags=0 + VP8L subchunk
        let le24 = |v: u32| [v as u8, (v >> 8) as u8, (v >> 16) as u8];
        let mut anmf = Vec::new();
        anmf.extend_from_slice(&le24(0));
        anmf.extend_from_slice(&le24(0));
        anmf.extend_from_slice(&le24(3)); // w-1=3
        anmf.extend_from_slice(&le24(1)); // h-1=1
        anmf.extend_from_slice(&le24(100)); // dur
        anmf.push(0); // flags
        anmf.extend_from_slice(b"VP8L");
        anmf.extend_from_slice(&(bit.len() as u32).to_le_bytes());
        anmf.extend_from_slice(&bit);
        if bit.len() % 2 == 1 {
            anmf.push(0); // 子块对齐填充（RFC 偶数对齐要求）
        }
        let data = assemble(
            Some((&vp8x, false)),
            &[
                (&b"ANIM".clone(), &anim),
                (&b"ANMF".clone(), &anmf),
            ],
        );
        let c = parse(&data).unwrap();
        assert_eq!(c.family, Family::Animated);
        assert_eq!(c.format, WebpFormat::Animated);
        assert_eq!(c.frames.len(), 1);
        assert_eq!(c.frames[0].duration_ms, 100);
        assert_eq!(c.loop_count, 0);
    }

    #[test]
    fn reject_malformed() {
        // 非 RIFF。
        assert_eq!(parse(b"JUNK").unwrap_err(), ContainerError::NotWebp);
        // RIFF 大小超数据。
        let mut bad = Vec::new();
        bad.extend_from_slice(b"RIFF");
        bad.extend_from_slice(&0xFFFFFFu32.to_le_bytes());
        bad.extend_from_slice(b"WEBP");
        assert_eq!(parse(&bad).unwrap_err(), ContainerError::RiffSizeExceedsData);
        // chunk 长度越界（不 panic）。
        let bit = vp8l_bytes(2, 2, false, &[0; 3]);
        let mut chunks = Vec::new();
        chunks.extend_from_slice(b"VP8L");
        chunks.extend_from_slice(&1000u32.to_le_bytes());
        chunks.extend_from_slice(&bit);
        let mut data = Vec::new();
        data.extend_from_slice(b"RIFF");
        data.extend_from_slice(&((chunks.len() + 4) as u32).to_le_bytes());
        data.extend_from_slice(b"WEBP");
        data.extend_from_slice(&chunks);
        assert_eq!(parse(&data).unwrap_err(), ContainerError::ChunkExceedsContainer);
        // 像素上限：画布 0x400000 x 0x400000（product 2^44 > MAX_PIXELS）→ 头部即拒。
        let vp8x = [0, 0, 0, 0, 0xFF, 0xFF, 0x3F, 0xFF, 0xFF, 0x3F];
        let bit = vp8l_bytes(2, 2, false, &[0; 3]);
        let data = assemble(Some((&vp8x, false)), &[(&b"VP8L".clone(), &bit)]);
        assert_eq!(parse(&data).unwrap_err(), ContainerError::PixelLimitExceeded);
    }
}
