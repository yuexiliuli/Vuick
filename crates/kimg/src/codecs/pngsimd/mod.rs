//! 自研 PNG 解码引擎（SimdCodecs 移植第一炮）：块解析 → IDAT 有界流式解压 → 逐行反滤波 → EXPAND 展开。
//!
//! 输出语义与 png crate `Transformations::EXPAND` 逐字节一致（灰度<8 扩 8、调色板扩 RGB/RGBA、tRNS 扩 alpha、
//! 16 位保留大端），见 `expand.rs` 详注。非隔行「恒等展开」路径直接在目标缓冲内反滤波（零拷贝）；
//! Adam7 按遍展开后散射回全帧。
//!
//! 注释中文；不改动其它 crate；对外 API 由上层 `codecs::png::PngDecoder` 不变承载。

pub(crate) mod chunk;
pub(crate) mod expand;
pub(crate) mod predict;

use std::io::Read;

use crate::codecs::pngsimd::chunk::{pass_extent, FileHeader, ADAM7_PASSES};
use crate::codecs::pngsimd::expand::{output_bpp, ExpandKind};
use crate::codecs::pngsimd::predict::{packed_row_bytes, prediction_bpp, unfilter_row};
use crate::color::ColorType;
use crate::error::{
    DecodingError, ImageError, ImageFormatHint, ImageResult, LimitError, LimitErrorKind,
};
use crate::format::ImageFormat;

/// 运行时 CPU 特性查询（x86_64；非 x86_64 一律 false 走标量回退）。
#[inline]
pub(crate) fn simd_ok(feature: &str) -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        use std::arch::is_x86_feature_detected;
        match feature {
            "sse2" => is_x86_feature_detected!("sse2"),
            "ssse3" => is_x86_feature_detected!("ssse3"),
            "avx2" => is_x86_feature_detected!("avx2"),
            _ => false,
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        let _ = feature;
        false
    }
}

/// 解析好的 PNG 引擎：持有整份输入（供解码时按 IDAT 偏移流式喂给 inflate，避免重新拼接拷贝）与头信息。
pub(crate) struct PngEngine {
    data: Vec<u8>,
    header: FileHeader,
}

fn err_decode(msg: impl Into<String>) -> ImageError {
    ImageError::Decoding(DecodingError::new(
        ImageFormatHint::Exact(ImageFormat::Png),
        msg.into(),
    ))
}

fn err_limits() -> ImageError {
    ImageError::Limits(LimitError::from_kind(LimitErrorKind::InsufficientMemory))
}

impl PngEngine {
    /// 从读取器读入全部字节并解析（kimg 的输入通常在内存中；读整份换取解析/装 IDAT 的简洁与零寻址）。
    pub(crate) fn parse(r: &mut impl Read) -> ImageResult<PngEngine> {
        let mut data = Vec::new();
        r.read_to_end(&mut data).map_err(ImageError::IoError)?;
        let header = chunk::parse_png(&data)?;
        Ok(PngEngine { data, header })
    }

    pub(crate) fn width(&self) -> u32 {
        self.header.width
    }

    pub(crate) fn height(&self) -> u32 {
        self.header.height
    }

    /// EXPAND 输出色彩类型（= 现 kimg png crate 路径 `output_color_type` 的结果）。
    pub(crate) fn color_type(&self) -> ColorType {
        output_color_type(&self.header)
    }

    /// 原始 eXIf 块。
    pub(crate) fn exif(&self) -> Option<Vec<u8>> {
        self.header.exif.clone()
    }

    /// 输出缓冲总字节。
    pub(crate) fn total_bytes(&self) -> u64 {
        u64::from(self.header.width)
            * u64::from(self.header.height)
            * output_bpp(&self.header) as u64
    }

    /// 解码整帧到 `buf`（长度需等于 `total_bytes`）。字节级 = png crate EXPAND 输出。
    pub(crate) fn decode(&self, buf: &mut [u8], max_alloc: Option<u64>) -> ImageResult<()> {
        debug_assert_eq!(buf.len() as u64, self.total_bytes());
        let h = &self.header;
        let inflated = inflate_idat(h, &self.data, max_alloc)?;
        if h.interlace == 1 {
            decode_adam7(h, &inflated, buf)
        } else {
            decode_scanlines(h, &inflated, buf)
        }
    }

    /// 仅诊断：分阶段计时。
    #[cfg(test)]
    fn decode_timed(&self, buf: &mut [u8]) -> ImageResult<()> {
        let h = &self.header;
        let t0 = std::time::Instant::now();
        let inflated = inflate_idat(h, &self.data, None)?;
        let t1 = std::time::Instant::now();
        if h.interlace == 1 {
            decode_adam7(h, &inflated, buf)?;
        } else {
            decode_scanlines(h, &inflated, buf)?;
        }
        let t2 = std::time::Instant::now();
        eprintln!(
            "[diag] inflate={:.3}ms rows={:.3}ms total={:.3}ms inflated={}",
            t1.duration_since(t0).as_secs_f64() * 1e3,
            t2.duration_since(t1).as_secs_f64() * 1e3,
            t2.duration_since(t0).as_secs_f64() * 1e3,
            inflated.len(),
        );
        Ok(())
    }
}

/// 跨多个 IDAT 块按 32KB 递送压缩字节（`next` 填满 buf，返回实际字节数；块边界处续接，不拼接拷贝）。
struct IdatReader<'a> {
    data: &'a [u8],
    chunks: &'a [(usize, usize)],
    ci: usize,
    co: usize,
}

impl<'a> IdatReader<'a> {
    fn new(data: &'a [u8], chunks: &'a [(usize, usize)]) -> Self {
        IdatReader {
            data,
            chunks,
            ci: 0,
            co: 0,
        }
    }

    /// 每个 IDAT 块内部为连续字节；把下一段 ≤`fill.len()` 的压缩数据拷入 `fill`，返回实际字节数。
    #[inline]
    fn next(&mut self, fill: &mut [u8]) -> usize {
        let mut filled = 0usize;
        while filled < fill.len() && self.ci < self.chunks.len() {
            let (off, len) = self.chunks[self.ci];
            let avail = len - self.co;
            if avail == 0 {
                self.ci += 1;
                self.co = 0;
                continue;
            }
            let n = avail.min(fill.len() - filled);
            fill[filled..filled + n]
                .copy_from_slice(&self.data[off + self.co..off + self.co + n]);
            self.co += n;
            filled += n;
        }
        filled
    }
}

/// 把 IDAT 解压到有界缓冲（长度 = 反滤波后线性尺寸，解析期已用 1032:1 预检防炸弹）。
///
/// 性能关键（实测 debug）：fdeflate 的 `read` 必须**分块（32KB，同 png crate 的
/// `CHUNK_BUFFER_SIZE`）喂入且 `end_of_input=false`**，末尾才用一次 `end_of_input=true`
/// （空输入）flush 校验 Adler-32。一次性把整段 IDAT 交给 `read`（或每调用都传 eoi=true，
/// 如 `decompress_to_vec_bounded` 内部）会触发慢路径：同样 8.3MB，3ms vs 53ms（~18×）。
/// 输出缓冲按期望尺寸预先分配，`out_pos` 直接喂给 fdeflate 供回看引用，零重分配。
fn inflate_idat(
    h: &FileHeader,
    data: &[u8],
    max_alloc: Option<u64>,
) -> ImageResult<Vec<u8>> {
    /// fdeflate 输入分块大小（同 png crate `CHUNK_BUFFER_SIZE`）。
    const CHUNK: usize = 32 * 1024;

    let expected = chunk::inflated_size(h.width, h.height, h.bit_depth, h.color_type, h.interlace == 1);
    if let Some(max) = max_alloc {
        if expected > max {
            return Err(err_limits());
        }
    }
    let expected =
        usize::try_from(expected).map_err(|_| err_decode("解压尺寸超出平台表示范围"))?;

    let mut dc = fdeflate::Decompressor::new();
    // 与 png crate 解码路径一致：跳过 Adler-32 校验（其默认 ignore_adler32=true）。
    // 校验的开销在 debug 下是灾难性的大（同上 8.3MB：2.5ms vs 52ms）；zlib 流其余结构
    // 校验仍生效，目标目录与被替换的 png crate 路径行为持平。真需要校验可去掉此行。
    dc.ignore_adler32();
    let mut inflated = vec![0u8; expected];
    let mut reader = IdatReader::new(data, &h.idat_chunks);
    let mut chunk_buf = [0u8; CHUNK];
    let mut out_pos = 0usize;
    'main: loop {
        if dc.is_done() {
            break;
        }
        let got = reader.next(&mut chunk_buf);
        if got == 0 {
            break; // 压缩输入耗尽，走末尾 flush
        }
        let mut input = &chunk_buf[..got];
        loop {
            if dc.is_done() {
                break 'main;
            }
            let (consumed, produced) = dc
                .read(input, &mut inflated, out_pos, false)
                .map_err(|e| err_decode(format!("IDAT 解压失败：{e:?}")))?;
            input = &input[consumed..];
            out_pos += produced;
            if input.is_empty() {
                break; // 本块消费完，取下一块
            }
            if produced == 0 {
                break 'main; // 输出已满且无进展 → 长度不符/炸弹，由末尾检查报错
            }
        }
    }
    if !dc.is_done() {
        let (_, produced) = dc
            .read(&[], &mut inflated, out_pos, true)
            .map_err(|e| err_decode(format!("IDAT 解压失败：{e:?}")))?;
        out_pos += produced;
    }
    if !dc.is_done() || out_pos != expected {
        return Err(err_decode(format!(
            "IDAT 流长度不符或未正常结束：期望 {expected} 字节，实际 {out_pos}"
        )));
    }
    Ok(inflated)
}

/// 从解压缓冲读 1 个 filter 字节（越界 = 流过短）。
#[inline]
fn next_byte(inf: &[u8], pos: &mut usize) -> ImageResult<u8> {
    if *pos >= inf.len() {
        return Err(err_decode("IDAT 流过早结束"));
    }
    let b = inf[*pos];
    *pos += 1;
    Ok(b)
}

/// 从解压缓冲取出 `len` 字节行（越界 = 流过短）。
#[inline]
fn next_row<'a>(inf: &'a [u8], pos: &mut usize, len: usize) -> ImageResult<&'a [u8]> {
    let start = *pos;
    let end = start.checked_add(len).ok_or_else(|| err_decode("行偏移溢出"))?;
    if end > inf.len() {
        return Err(err_decode("IDAT 流过早结束"));
    }
    *pos = end;
    Ok(&inf[start..end])
}

/// EXPAND 输出色彩类型（与 png crate `output_color_type` 语义逐项对齐，含 tRNS 扩 alpha）。
pub(crate) fn output_color_type(h: &FileHeader) -> ColorType {
    use ColorType::*;
    let trns = h.has_trns;
    match (h.color_type, h.bit_depth) {
        (0, 16) => {
            if trns {
                La16
            } else {
                L16
            }
        }
        (0, _) => {
            if trns {
                La8
            } else {
                L8
            }
        }
        (2, 16) => {
            if trns {
                Rgba16
            } else {
                Rgb16
            }
        }
        (2, _) => {
            if trns {
                Rgba8
            } else {
                Rgb8
            }
        }
        (3, _) => {
            if trns {
                Rgba8
            } else {
                Rgb8
            }
        }
        (4, 16) => La16,
        (4, _) => La8,
        (6, 16) => Rgba16,
        (6, _) => Rgba8,
        _ => unreachable!("色彩类型已在前置校验"),
    }
}

// ---------------------------------------------------------------------------
// 非隔行
// ---------------------------------------------------------------------------

fn decode_scanlines(h: &FileHeader, inflated: &[u8], buf: &mut [u8]) -> ImageResult<()> {
    let width = h.width as usize;
    let height = h.height as usize;
    let spp = h.samples_per_pixel();
    let depth = h.bit_depth as usize;
    let fbpp = prediction_bpp(spp, depth);
    let srow = packed_row_bytes(width, spp, depth);
    let out_bpp = output_bpp(h);
    let out_row = width * out_bpp;

    let kind = expand::build_expand_kind(h);
    let table = expand::PaletteTable::from_header(h);
    let mut pos = 0usize;

    if matches!(kind, ExpandKind::Copy) {
        // 恒等路径：先从解压缓冲拷贝到目标行，再就地反滤波（源行字节 == 输出行字节）。
        // 上一行取自目标缓冲（已反滤波完成），零中间样本分配。
        debug_assert_eq!(srow, out_row);
        let zeros = vec![0u8; srow];
        for y in 0..height {
            let f = next_byte(inflated, &mut pos)?;
            let src = next_row(inflated, &mut pos, out_row)?;
            let (row, above): (&mut [u8], &[u8]) = if y == 0 {
                let (_, rest) = buf.split_at_mut(0);
                let (row, _) = rest.split_at_mut(out_row);
                (row, &zeros)
            } else {
                let (before, rest) = buf.split_at_mut(y * out_row);
                let (row, _) = rest.split_at_mut(out_row);
                let above = &before[(y - 1) * out_row..y * out_row];
                (row, above)
            };
            row.copy_from_slice(src);
            unfilter_row(f, row, above, fbpp);
        }
    } else {
        // 需要展开：行缓冲复用（cur/above 两槽，槽位轮流当上一行），展开写目标行。
        debug_assert_eq!(buf.len(), out_row * height);
        let mut cur = vec![0u8; srow];
        let mut above = vec![0u8; srow];
        for y in 0..height {
            let f = next_byte(inflated, &mut pos)?;
            let src = next_row(inflated, &mut pos, srow)?;
            cur.copy_from_slice(src);
            unfilter_row(f, &mut cur, &above, fbpp);
            let dst = &mut buf[y * out_row..y * out_row + out_row];
            expand::expand_row(&kind, &table, &cur, dst, width);
            std::mem::swap(&mut cur, &mut above);
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Adam7 隔行
// ---------------------------------------------------------------------------

fn decode_adam7(h: &FileHeader, inflated: &[u8], buf: &mut [u8]) -> ImageResult<()> {
    let width = h.width as usize;
    let height = h.height as usize;
    let spp = h.samples_per_pixel();
    let depth = h.bit_depth as usize;
    let fbpp = prediction_bpp(spp, depth);
    let out_bpp = output_bpp(h);
    let out_row = width * out_bpp;

    let kind = expand::build_expand_kind(h);
    let table = expand::PaletteTable::from_header(h);
    let max_srow = packed_row_bytes(width, spp, depth);

    let mut cur = vec![0u8; max_srow];
    let mut above = vec![0u8; max_srow];
    let mut expanded = vec![0u8; width * out_bpp];
    let mut pos = 0usize;

    for (x0, y0, dx, dy) in ADAM7_PASSES {
        let pw = pass_extent(width, x0, dx);
        let ph = pass_extent(height, y0, dy);
        if pw == 0 || ph == 0 {
            continue;
        }
        let srow = packed_row_bytes(pw, spp, depth);
        above[..srow].fill(0);
        for r in 0..ph {
            let f = next_byte(inflated, &mut pos)?;
            let src = next_row(inflated, &mut pos, srow)?;
            cur[..srow].copy_from_slice(src);
            let (crow, _) = cur.split_at_mut(srow);
            let (ab, _) = above.split_at(srow);
            unfilter_row(f, crow, ab, fbpp);
            let erow = &mut expanded[..pw * out_bpp];
            expand::expand_row(&kind, &table, crow, erow, pw);
            let y = y0 + r * dy;
            let dst_start = y * out_row + x0 * out_bpp;
            scatter(&expanded[..pw * out_bpp], &mut buf[dst_start..], pw, out_bpp, dx);
            std::mem::swap(&mut cur, &mut above);
        }
    }

    Ok(())
}

/// 把 `count` 个（每像素 `pixel_bytes` 字节、相邻）像素写到目标行每隔 `step` 行的像素位上（Adam7 散射）。
fn scatter(src: &[u8], dst: &mut [u8], count: usize, pixel_bytes: usize, step: usize) {
    let stride = step * pixel_bytes;
    for i in 0..count {
        let s = &src[i * pixel_bytes..i * pixel_bytes + pixel_bytes];
        let d = &mut dst[i * stride..i * stride + pixel_bytes];
        d.copy_from_slice(s);
    }
}

#[cfg(test)]
mod diag_tests {
    use super::*;

    /// 分阶段计时诊断：RGBA8 大图（跑 `cargo test -p kimg --lib pngsimd::diag -- --ignored --nocapture`）。`#[ignore]` 手动。
    #[test]
    #[ignore = "手动运行"]
    fn decode_phase_timing_rgba8() {
        let w = 1920u32;
        let h = 1080u32;
        let row = w as usize * 4;
        let mut img = Vec::with_capacity(row * h as usize);
        let mut lcg = 0x9E3779B97F4A7C15u64;
        for _ in 0..(row * h as usize) {
            lcg = lcg.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            img.push((lcg >> 40) as u8);
        }
        let mut bytes = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut bytes, w, h);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            enc.set_compression(png::Compression::Fast);
            let mut writer = enc.write_header().unwrap();
            writer.write_image_data(&img).unwrap();
        }
        // 抽出 IDAT（首个）载荷 = 真正的 zlib 流。
        let idat = {
            let mut pos = 8usize;
            loop {
                let len =
                    u32::from_be_bytes([bytes[pos], bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]])
                        as usize;
                if &bytes[pos + 4..pos + 8] == b"IDAT" {
                    break bytes[pos + 8..pos + 8 + len].to_vec();
                }
                pos += 12 + len;
            }
        };

        // 1) decompress_to_vec_bounded
        let expected = 8_295_480usize;
        let mut durations = Vec::new();
        for _ in 0..3 {
            let t = std::time::Instant::now();
            let _ = fdeflate::decompress_to_vec_bounded(&idat, expected);
            durations.push(t.elapsed().as_secs_f64() * 1e3);
        }
        eprintln!("[diag-inflate] bounded helper = {durations:?}ms");

        // 2) 手动 Decompressor 循环（固定 64KB 输出缓冲）
        let mut durations = Vec::new();
        for _ in 0..3 {
            let t = std::time::Instant::now();
            let mut dc = fdeflate::Decompressor::new();
            let mut input: &[u8] = &idat;
            let mut output = vec![0u8; 64 * 1024];
            let mut total_out = 0usize;
            loop {
                if dc.is_done() {
                    break;
                }
                let (consumed, produced) = match dc.read(&input, &mut output, 0, true) {
                    Ok(v) => v,
                    Err(e) => {
                        panic!("diag inflate err {e:?}");
                    }
                };
                input = &input[consumed..];
                total_out += produced;
            }
            durations.push(t.elapsed().as_secs_f64() * 1e3);
            assert_eq!(total_out, expected);
        }
        eprintln!("[diag-inflate] manual loop 64KB = {durations:?}ms");

        // 2b) 两阶段（eoi=false 流式 + 末尾 eoi=true flush）+ ignore_adler32
        let mut durations = Vec::new();
        for _ in 0..3 {
            let t = std::time::Instant::now();
            let mut dc = fdeflate::Decompressor::new();
            dc.ignore_adler32();
            let mut inflated = vec![0u8; expected];
            let mut input: &[u8] = &idat;
            let mut out_pos = 0usize;
            loop {
                if dc.is_done() {
                    break;
                }
                let (consumed, produced) = dc.read(&input, &mut inflated, out_pos, false).unwrap();
                input = &input[consumed..];
                out_pos += produced;
                if consumed == 0 && produced == 0 {
                    break;
                }
            }
            if !dc.is_done() {
                let (_, produced) = dc.read(&[], &mut inflated, out_pos, true).unwrap();
                out_pos += produced;
            }
            assert_eq!(out_pos, expected);
            durations.push(t.elapsed().as_secs_f64() * 1e3);
        }
        eprintln!("[diag-inflate] two-phase + ignore_adler32 = {durations:?}ms");

        // 2c) 两阶段不 ignore_adler32（与 2b 仅差 adler 校验）
        let mut durations = Vec::new();
        for _ in 0..3 {
            let t = std::time::Instant::now();
            let mut dc = fdeflate::Decompressor::new();
            let mut inflated = vec![0u8; expected];
            let mut input: &[u8] = &idat;
            let mut out_pos = 0usize;
            loop {
                if dc.is_done() {
                    break;
                }
                let (consumed, produced) = dc.read(&input, &mut inflated, out_pos, false).unwrap();
                input = &input[consumed..];
                out_pos += produced;
                if consumed == 0 && produced == 0 {
                    break;
                }
            }
            if !dc.is_done() {
                let (_, produced) = dc.read(&[], &mut inflated, out_pos, true).unwrap();
                out_pos += produced;
            }
            assert_eq!(out_pos, expected);
            durations.push(t.elapsed().as_secs_f64() * 1e3);
        }
        eprintln!("[diag-inflate] two-phase + adler = {durations:?}ms");

        // 2d) 32KB 分块喂入（png 同款节奏）+ ignore_adler32
        let mut durations = Vec::new();
        const CHUNK: usize = 32 * 1024;
        for _ in 0..3 {
            let t = std::time::Instant::now();
            let mut dc = fdeflate::Decompressor::new();
            dc.ignore_adler32();
            let mut inflated = vec![0u8; expected];
            let mut in_off = 0usize;
            let mut out_pos = 0usize;
            loop {
                if dc.is_done() {
                    break;
                }
                let end = (in_off + CHUNK).min(idat.len());
                let (consumed, produced) = dc
                    .read(&idat[in_off..end], &mut inflated, out_pos, false)
                    .unwrap();
                in_off += consumed;
                out_pos += produced;
                if end == idat.len() && consumed == 0 {
                    break;
                }
            }
            if !dc.is_done() {
                let (_, produced) = dc.read(&[], &mut inflated, out_pos, true).unwrap();
                out_pos += produced;
            }
            assert_eq!(out_pos, expected);
            durations.push(t.elapsed().as_secs_f64() * 1e3);
        }
        eprintln!("[diag-inflate] two-phase chunked32k + ignore_adler32 = {durations:?}ms");

        // 2e) 32KB 分块 + 保留 adler 校验（确认 adler 是否是慢源）
        let mut durations = Vec::new();
        for _ in 0..3 {
            let t = std::time::Instant::now();
            let mut dc = fdeflate::Decompressor::new();
            let mut inflated = vec![0u8; expected];
            let mut in_off = 0usize;
            let mut out_pos = 0usize;
            loop {
                if dc.is_done() {
                    break;
                }
                let end = (in_off + CHUNK).min(idat.len());
                let (consumed, produced) = dc
                    .read(&idat[in_off..end], &mut inflated, out_pos, false)
                    .unwrap();
                in_off += consumed;
                out_pos += produced;
                if end == idat.len() && consumed == 0 {
                    break;
                }
            }
            if !dc.is_done() {
                let (_, produced) = dc.read(&[], &mut inflated, out_pos, true).unwrap();
                out_pos += produced;
            }
            assert_eq!(out_pos, expected);
            durations.push(t.elapsed().as_secs_f64() * 1e3);
        }
        eprintln!("[diag-inflate] two-phase chunked32k + adler = {durations:?}ms");
        // 3) png crate 全解码（参照）
        let mut durations = Vec::new();
        for _ in 0..3 {
            let t = std::time::Instant::now();
            let dec = png::Decoder::new(&bytes[..]);
            let mut r = dec.read_info().unwrap();
            let mut out = vec![0u8; r.output_buffer_size()];
            r.next_frame(&mut out).unwrap();
            durations.push(t.elapsed().as_secs_f64() * 1e3);
        }
        eprintln!("[diag-inflate] png crate full decode = {durations:?}ms");

        // 引擎实测
        let mut durations = Vec::new();
        for _ in 0..3 {
            let t = std::time::Instant::now();
            PngEngine::parse(&mut &bytes[..]).unwrap();
            durations.push(t.elapsed().as_secs_f64() * 1e3);
        }
        eprintln!("[diag-parse] parse (whole file) = {durations:?}ms  input={}B", bytes.len());

        // 输出分配（vec![0; N] debug 零化）
        let mut durations = Vec::new();
        for _ in 0..3 {
            let t = std::time::Instant::now();
            let _ = vec![0u8; 8_295_480];
            durations.push(t.elapsed().as_secs_f64() * 1e3);
        }
        eprintln!("[diag-parse] alloc+zero 8.3MB = {durations:?}ms");

        // 引擎实测
        let engine = PngEngine::parse(&mut &bytes[..]).unwrap();
        let mut buf = vec![0u8; engine.total_bytes() as usize];
        for _ in 0..3 {
            engine.decode_timed(&mut buf).unwrap();
        }
    }

    /// 分阶段计时诊断：纯标量、纯 SIMD 反滤波对照。`#[ignore]` 手动。
    #[test]
    #[ignore = "手动运行"]
    fn unfilter_rate_rgba8() {
        // 直接生成一个模拟行缓冲（随机 + 固定 filter）测速率。
        let n = 1920 * 4usize;
        let rows = 1080usize;
        // 总工作量 ~13MB。
        let mut t_simd = std::time::Duration::ZERO;
        let mut t_scal = std::time::Duration::ZERO;
        for filter in 1..=4u8 {
            let mut row = vec![0u8; n];
            let above = vec![0u8; n];
            let mut lcg = 0xABCDEF12345678u64;
            for b in row.iter_mut() {
                lcg = lcg.wrapping_mul(6364136223846793005).wrapping_add(1);
                *b = (lcg >> 40) as u8;
            }
            
            let t0 = std::time::Instant::now();
            for _ in 0..rows {
                unfilter_row(filter, &mut row, &above, 4);
                            }
            t_simd += t0.elapsed();
            let t1 = std::time::Instant::now();
            for _ in 0..rows {
                scal_avg_etc(filter, &mut row, &above, 4);
            }
            t_scal += t1.elapsed();
            eprintln!(
                "[diag-unfilter] filter={filter} simd={:?} scalar={:?}",
                t_simd, t_scal
            );
        }
        eprintln!("[diag-unfilter] simd={:?} scalar={:?}", t_simd, t_scal);
    }

    fn scal_avg_etc(filter: u8, row: &mut [u8], above: &[u8], bpp: usize) {
        // 纯标量反滤波（与 unfilter_row 对照），慢速参考。
        for i in 0..row.len() {
            match filter {
                1 => row[i] = row[i].wrapping_add(row[i.wrapping_sub(bpp.max(1)).min(i)]),
                2 => row[i] = row[i].wrapping_add(above[i]),
                3 => {
                    let l = if i >= bpp { row[i - bpp] } else { 0 };
                    row[i] = row[i].wrapping_add(((u16::from(l) + u16::from(above[i])) >> 1) as u8)
                }
                _ => {
                    let l = if i >= bpp { i32::from(row[i - bpp]) } else { 0 };
                    let a = i32::from(above[i]);
                    let c = if i >= bpp { i32::from(above[i - bpp]) } else { 0 };
                    let p = l + a - c;
                    let pa = (p - l).abs();
                    let pb = (p - a).abs();
                    let pc = (p - c).abs();
                    let pred = if pa <= pb && pa <= pc {
                        l
                    } else if pb <= pc {
                        a
                    } else {
                        c
                    };
                    row[i] = row[i].wrapping_add(pred as u8);
                }
            }
        }
    }
}
