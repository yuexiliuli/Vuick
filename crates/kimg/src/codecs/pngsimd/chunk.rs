//! PNG 块容器解析（移植自 SimdCodecs `PngReader.cs` 的 `ReadHeader` 后半）：签名、块 CRC32 校验、
//! IHDR / PLTE / tRNS / IDAT / eXIf，pHYs 等跳读，并把 tRNS 语义化与 png crate 完全对齐。
//!
//! 端到端输出语义 = png crate `Transformations::EXPAND`（见 `expand.rs`），tRNS 的字节语义在此对齐：
//! - 灰阶（color type 0）/ RGB（2）：tRNS 块存 16 位大端样本；位深 < 16 时 png crate `parse_trns`
//!   只保留每个样本的**低字节**（`vec[0]=vec[1]`），本实现照抄（`transparent_color` 存同款字节）；
//! - 索引（3）：tRNS 一块每项一个 alpha 字节，多出的当 255，超过调色板项数的整块忽略（png `create_rgba_palette`）；
//! - 灰α（4）/ RGBA（6）：不合法，报错（png `ColorWithBadTrns`）。

use crate::error::{DecodingError, ImageError, ImageFormatHint, ImageResult};
use crate::format::ImageFormat;

/// PNG 签名。
pub(crate) const SIGNATURE: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];

/// 解析结果：IHDR 与元数据 + IDAT 块偏移（供解码期按需流式解压，不预先拼接）。
#[derive(Debug)]
pub(crate) struct FileHeader {
    pub(crate) width: u32,
    pub(crate) height: u32,
    /// 源位深（1/2/4/8/16）。
    pub(crate) bit_depth: u8,
    /// 色彩类型（0/2/3/4/6）。
    pub(crate) color_type: u8,
    /// 是否 Adam7 隔行（0/1）。
    pub(crate) interlace: u8,
    /// PLTE 载荷（每项 3 字节）。
    pub(crate) palette: Vec<u8>,
    /// tRNS 语义化字节：与 png crate `info.trns` 逐字节一致（位深 <16 时已取低字节）。
    pub(crate) transparent_color: Vec<u8>,
    /// 是否有（影响输出色彩类型的）有效 tRNS 块。
    pub(crate) has_trns: bool,
    /// 原始 eXIf 载荷。
    pub(crate) exif: Option<Vec<u8>>,
    /// IDAT 载荷在输入缓冲中的 (offset, len)——多个 IDAT 连成一个 zlib 流（不重新拼接拷贝）。
    pub(crate) idat_chunks: Vec<(usize, usize)>,
}

impl FileHeader {
    /// 每像素样本数：Gray=1, RGB=3, Indexed=1, GrayAlpha=2, RGBA=4。
    pub(crate) fn samples_per_pixel(&self) -> usize {
        match self.color_type {
            0 | 3 => 1,
            4 => 2,
            2 => 3,
            6 => 4,
            _ => unreachable!("色彩类型已在前置校验"),
        }
    }
}

/// Adam7 七遍：pass k 持有列 `X0 + i·Dx`、行 `Y0 + j·Dy` 的像素（PNG 规范 §8.2）。
pub(crate) const ADAM7_PASSES: [(usize, usize, usize, usize); 7] = [
    (0, 0, 8, 8),
    (4, 0, 8, 8),
    (0, 4, 4, 8),
    (2, 0, 4, 4),
    (0, 2, 2, 4),
    (1, 0, 2, 2),
    (0, 1, 1, 2),
];

/// 一遍在一侧的像素数（图像不足以到达其起始位时返回 0）。
#[inline]
pub(crate) fn pass_extent(size: usize, start: usize, step: usize) -> usize {
    if size > start {
        (size - start + step - 1) / step
    } else {
        0
    }
}

/// deflate 理论最大膨胀率：1032:1（png 规范注释 / SimdCodecs 同）——截断的 IDAT 不可能
/// 膨胀出声明图像所需字节，伪造头在到达分配前就被拒（防解压炸弹，不 OOM）。
const MAX_INFLATE_RATIO: u64 = 1032;
const MAX_INFLATE_SLACK: u64 = 1024;

/// 载荷完整性取舍：IDAT 载荷跳过逐块 CRC32（内容完整性非显示必需——deflate 结构 + 末尾
/// Adler-32 已由解压后端校验，位翻转要么解压报错、要么像素失真，与 png crate 解码路径
/// 「内容位不逐块校验」等价；且 IDAT 是体积大头，大图下逐块 CRC 是纯固定开销）。关键头块
/// （IHDR/PLTE/tRNS，以及 eXIf 等小附属块）仍逐块校验。想恢复严格校验把此常量置 true。
const VERIFY_IDAT_CRC: bool = false;

fn err_decode(msg: impl Into<String>) -> ImageError {
    ImageError::Decoding(DecodingError::new(
        ImageFormatHint::Exact(ImageFormat::Png),
        msg.into(),
    ))
}

/// 解析整份 PNG 字节流。
pub(crate) fn parse_png(data: &[u8]) -> ImageResult<FileHeader> {
    if data.len() < SIGNATURE.len() || data[..SIGNATURE.len()] != SIGNATURE {
        return Err(err_decode("PNG 签名缺失——不是 PNG 文件"));
    }

    let mut pos = SIGNATURE.len();
    let mut width: Option<u32> = None;
    let mut height: Option<u32> = None;
    let mut bit_depth: Option<u8> = None;
    let mut color_type: Option<u8> = None;
    let mut interlace: u8 = 0;
    let mut palette: Option<Vec<u8>> = None;
    let mut trns: Option<Vec<u8>> = None;
    let mut exif: Option<Vec<u8>> = None;
    let mut idat_chunks: Vec<(usize, usize)> = Vec::new();
    let mut have_idat = false;

    // 逐块遍历：校验长度边界 + CRC32（类型+载荷）。
    while pos < data.len() {
        if pos + 8 > data.len() {
            return Err(err_decode(format!("块头在第 {pos} 字节被截断")));
        }
        let len = u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]);
        if len > i32::MAX as u32 {
            return Err(err_decode(format!("块长度 {len} 超过 int.MaxValue")));
        }
        let len = len as usize;
        if pos + 8 + len + 4 > data.len() {
            return Err(err_decode(format!("第 {pos} 字节的块延伸出文件末尾")));
        }
        let chunk_type = &data[pos + 4..pos + 8];
        let payload = &data[pos + 8..pos + 8 + len];
        let declared = u32::from_be_bytes([
            data[pos + 8 + len],
            data[pos + 8 + len + 1],
            data[pos + 8 + len + 2],
            data[pos + 8 + len + 3],
        ]);
        // 校验块 CRC：关键头块（IHDR/PLTE/tRNS 等小块）逐块校验；IDAT 按 `VERIFY_IDAT_CRC`
        // 取舍跳过（见常量注释），"目录也含类型 4 字节"（与 png crate 默认行为一致）。
        if VERIFY_IDAT_CRC || chunk_type != b"IDAT" {
            let mut crc_ctx = crc32fast::Hasher::new();
            crc_ctx.update(chunk_type);
            crc_ctx.update(payload);
            if declared != crc_ctx.finalize() {
                return Err(err_decode(format!(
                    "块 '{:?}' 在第 {pos} 字节 CRC 不匹配：声明 0x{declared:08X}",
                    String::from_utf8_lossy(chunk_type)
                )));
            }
        }

        match chunk_type {
            b"IHDR" => {
                if width.is_some() {
                    return Err(err_decode("存在多个 IHDR 块"));
                }
                if len != 13 {
                    return Err(err_decode(format!("IHDR 长度必须为 13，实际 {len}")));
                }
                width = Some(u32::from_be_bytes([
                    payload[0], payload[1], payload[2], payload[3],
                ]));
                height = Some(u32::from_be_bytes([
                    payload[4], payload[5], payload[6], payload[7],
                ]));
                bit_depth = Some(payload[8]);
                color_type = Some(payload[9]);
                let compression = payload[10];
                let filter = payload[11];
                interlace = payload[12];
                let (w, h) = (width.unwrap(), height.unwrap());
                if w == 0 || h == 0 {
                    return Err(err_decode(format!("IHDR 宽高必须为正，实际 {w}×{h}")));
                }
                if compression != 0 {
                    return Err(err_decode(format!(
                        "不支持的压缩方法 {compression}（PNG 仅定义 0）"
                    )));
                }
                if filter != 0 {
                    return Err(err_decode(format!(
                        "不支持的滤波方法 {filter}（PNG 仅定义 0）"
                    )));
                }
                if interlace > 1 {
                    return Err(err_decode(format!(
                        "未知隔行方法 {interlace}（PNG 定义 0 和 1）"
                    )));
                }
                validate_color_type_bit_depth(color_type.unwrap(), bit_depth.unwrap())?;
            }
            b"IDAT" => {
                if width.is_none() {
                    return Err(err_decode("IDAT 早于 IHDR"));
                }
                have_idat = true;
                idat_chunks.push((pos + 8, len));
            }
            b"IEND" => {
                break;
            }
            b"PLTE" => {
                if len == 0 || len % 3 != 0 {
                    return Err(err_decode(format!(
                        "PLTE 长度必须为正且为 3 的倍数，实际 {len}"
                    )));
                }
                palette = Some(payload.to_vec());
            }
            b"tRNS" => {
                if trns.is_some() {
                    return Err(err_decode("存在多个 tRNS 块"));
                }
                // 顺序/合法性随块出现即时校验（pn crate `parse_trns` 同款）：
                // 索引色 tRNS 必须在 PLTE 之后、IDAT 之前；灰α/RGBA 不允许携带。
                if let Some(ct) = color_type {
                    match ct {
                        3 => {
                            if palette.is_none() {
                                return Err(err_decode("索引色 tRNS 必须位于 PLTE 之后"));
                            }
                            if have_idat {
                                return Err(err_decode("索引色 tRNS 必须位于 IDAT 之前"));
                            }
                        }
                        4 | 6 => return Err(err_decode("GrayAlpha/RGBA 不允许携带 tRNS")),
                        _ => {}
                    }
                }
                trns = Some(payload.to_vec());
            }
            b"eXIf" => {
                exif = Some(payload.to_vec());
            }
            // pHYs / sRGB / gAMA / cHRM / iCCP / 文本 / 未知块：本轮只关心解码所需，一律跳读。
            _ => {}
        }
        pos += 8 + len + 4;
    }

    if width.is_none() {
        return Err(err_decode("未找到 IHDR 块"));
    }
    if !have_idat || idat_chunks.is_empty() {
        return Err(err_decode("未找到 IDAT 块"));
    }
    let (w, h, depth, ct) = (
        width.unwrap(),
        height.unwrap(),
        bit_depth.unwrap(),
        color_type.unwrap(),
    );
    if ct == 3 && palette.is_none() {
        return Err(err_decode("索引色 PNG 缺少必需的 PLTE 块"));
    }

    // 防炸弹预检：期望解压字节数 vs 压缩量（伪造头在分配前被拒）。
    // `inflated_size` 已对恶意超大尺寸饱和；此处比较用 saturating 防 `compressed*1032` 本身溢出。
    let inflated_size = inflated_size(w, h, depth, ct, interlace == 1);
    let compressed: u64 = idat_chunks.iter().map(|&(_, l)| l as u64).sum();
    if inflated_size
        > compressed
            .saturating_mul(MAX_INFLATE_RATIO)
            .saturating_add(MAX_INFLATE_SLACK)
    {
        return Err(err_decode(format!(
            "{compressed} 字节图像数据无法膨胀出 {w}×{h} 图像所需的 {inflated_size} 字节"
        )));
    }

    // tRNS 语义化（png crate `parse_trns` 同款，含 <16bit 取低字节）。
    let transparent_color = match trns.as_deref() {
        None => Vec::new(),
        Some(t) => match ct {
            0 => {
                if t.len() < 2 {
                    return Err(err_decode("灰阶 tRNS 长度不足 2"));
                }
                if depth < 16 {
                    vec![t[1]]
                } else {
                    t[..2].to_vec()
                }
            }
            2 => {
                if t.len() < 6 {
                    return Err(err_decode("RGB tRNS 长度不足 6"));
                }
                if depth < 16 {
                    vec![t[1], t[3], t[5]]
                } else {
                    t[..6].to_vec()
                }
            }
            3 => t.to_vec(),
            4 | 6 => unreachable!("块出现时已拒绝"),
            _ => unreachable!(),
        },
    };

    Ok(FileHeader {
        width: w,
        height: h,
        bit_depth: depth,
        color_type: ct,
        interlace,
        palette: palette.unwrap_or_default(),
        transparent_color,
        has_trns: trns.is_some(),
        exif,
        idat_chunks,
    })
}

/// 校验 (色彩类型, 位深) 组合（PNG 规范表 11.1）。
fn validate_color_type_bit_depth(color_type: u8, bit_depth: u8) -> ImageResult<()> {
    let ok = match color_type {
        0 => matches!(bit_depth, 1 | 2 | 4 | 8 | 16),
        2 => matches!(bit_depth, 8 | 16),
        3 => matches!(bit_depth, 1 | 2 | 4 | 8),
        4 => matches!(bit_depth, 8 | 16),
        6 => matches!(bit_depth, 8 | 16),
        _ => false,
    };
    if !ok {
        return Err(err_decode(format!(
            "非法的 (色彩类型={color_type}, 位深={bit_depth}) 组合（PNG 规范表 11.1）"
        )));
    }
    Ok(())
}

/// 图像数据解压应有的字节数：每行 (filter 字节 + 打包行字节)；隔行则按遍累加，空遍不贡献。
pub(crate) fn inflated_size(
    width: u32,
    height: u32,
    bit_depth: u8,
    color_type: u8,
    interlaced: bool,
) -> u64 {
    let samples = FileHeader {
        width: 0,
        height: 0,
        bit_depth,
        color_type,
        interlace: 0,
        palette: Vec::new(),
        transparent_color: Vec::new(),
        has_trns: false,
        exif: None,
        idat_chunks: Vec::new(),
    }
    .samples_per_pixel();
    let bits_per_pixel = samples * bit_depth as usize;
    let row_bytes_for = |w: usize| -> u64 { ((w as u64 * bits_per_pixel as u64) + 7) / 8 };
    // 恶意头（如 max×max）会让「每行字节数 × 行数」溢出 u64；此处饱和到 u64::MAX，
    // 后续防炸弹比例检查（`inflated_size > compressed*1032`）必然命中拒绝——不许 panic（K：解压炸弹 CHECKED）。
    if !interlaced {
        return (row_bytes_for(width as usize) + 1).saturating_mul(height as u64);
    }
    let mut total = 0u64;
    for (x0, y0, dx, dy) in ADAM7_PASSES {
        let pw = pass_extent(width as usize, x0, dx);
        let ph = pass_extent(height as usize, y0, dy);
        if pw > 0 && ph > 0 {
            total = total.saturating_add((row_bytes_for(pw) + 1).saturating_mul(ph as u64));
        }
    }
    total
}
