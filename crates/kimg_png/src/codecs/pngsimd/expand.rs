//! PNG 行样本展开（移植自 SimdCodecs `PngRgba8Expander.cs`），输出语义 = png crate `Transformations::EXPAND`。
//!
//! 关键对齐点（字节级必须与 png crate EXPAND 完全一致）：
//! - 索引色：256 项 RGBA 查找表（PLTE 缺项为不透明黑；tRNS 只填前若干项 alpha，超过调色板项数的 tRNS 整块忽略）；
//!   **越界索引返回黑色不透明（PNG crate 语义，不报错）**——注意与 SimdCodecs 抛异常不同，此处以 png crate 为准；
//! - 灰阶 sub-byte：`level * 255 / max` 缩放，tRNS 比较针对**未缩放**的原始样本（png `expand_gray_u8_with_trns`）；
//! - 8/16 位 tRNS 透明色：8 位比低字节、16 位比完整大端字节（png `parse_trns` 掩码后存字节，见 `chunk.rs`）。
//!
//! SIMD：RGB8→RGBA8 的 SSSE3 字节重排（`_mm_shuffle_epi8` + 不透明 alpha 掩码，与 C# `Rgb8` 同掩码），
//! 配标量回退；测试保证标量/向量逐字节一致。

use crate::codecs::pngsimd::chunk::FileHeader;

/// 展开种类：`None` 表示源行字节 == 输出行字节（非隔行直接路径，见 `mod.rs`）。
#[derive(Clone, Copy, Debug)]
pub(crate) enum ExpandKind {
    /// 恒等：源字节 == 输出字节（仅 Adam7 隔行内部需要，作为展开的特例）。
    Copy,
    /// 灰阶 sub-byte → L8。
    SubByteL { depth: u8 },
    /// 灰阶 sub-byte + tRNS → La8（key 为原始样本值）。
    SubByteLa { depth: u8, key: u8 },
    /// 灰阶 8 位 + tRNS → La8。
    G8ToLa8 { key: u8 },
    /// 灰阶 16 位 + tRNS → La16。
    G16ToLa16 { key: [u8; 2] },
    /// RGB 8 位 + tRNS → Rgba8（SSSE3 重排 + key 清 alpha）。
    Rgb8ToRgba8 { key: [u8; 4] },
    /// RGB 16 位 + tRNS → Rgba16。
    Rgb16ToRgba16 { key: [u8; 6] },
    /// 索引色 → Rgb8 / Rgba8（rgba 由是否有 tRNS 决定）。
    Palette { depth: u8, rgba: bool },
}

/// 由头信息选出展开种类（对应 png crate `create_transform_fn` 的 EXPAND 分支）。
pub(crate) fn build_expand_kind(h: &FileHeader) -> ExpandKind {
    let trns = h.has_trns;
    match h.color_type {
        0 => match h.bit_depth {
            1 | 2 | 4 => {
                if trns {
                    ExpandKind::SubByteLa {
                        depth: h.bit_depth,
                        key: h.transparent_color[0],
                    }
                } else {
                    ExpandKind::SubByteL { depth: h.bit_depth }
                }
            }
            8 => {
                if trns {
                    ExpandKind::G8ToLa8 {
                        key: h.transparent_color[0],
                    }
                } else {
                    ExpandKind::Copy
                }
            }
            _ => {
                // 16
                if trns {
                    ExpandKind::G16ToLa16 {
                        key: [h.transparent_color[0], h.transparent_color[1]],
                    }
                } else {
                    ExpandKind::Copy
                }
            }
        },
        2 => match h.bit_depth {
            8 => {
                if trns {
                    ExpandKind::Rgb8ToRgba8 {
                        key: [
                            h.transparent_color[0],
                            h.transparent_color[1],
                            h.transparent_color[2],
                            0xFF,
                        ],
                    }
                } else {
                    ExpandKind::Copy
                }
            }
            _ => {
                if trns {
                    let k = &h.transparent_color;
                    ExpandKind::Rgb16ToRgba16 {
                        key: [k[0], k[1], k[2], k[3], k[4], k[5]],
                    }
                } else {
                    ExpandKind::Copy
                }
            }
        },
        3 => ExpandKind::Palette {
            depth: h.bit_depth,
            rgba: trns,
        },
        4 | 6 => ExpandKind::Copy,
        _ => unreachable!("色彩类型已在前置校验"),
    }
}

/// 索引色展开用 256 项 RGBA 查找表（语义与 png crate `create_rgba_palette` 一致）。
#[derive(Clone, Debug)]
pub(crate) struct PaletteTable {
    /// 256 项 `[r,g,b,a]`，缺项为黑色不透明。
    rgba: Box<[[u8; 4]; 256]>,
}

impl PaletteTable {
    pub(crate) fn from_header(h: &FileHeader) -> Self {
        let mut rgba = Box::new([[0, 0, 0, 0xFF]; 256]);
        // PLTE：每项 3 字节。
        for (i, e) in h.palette.chunks_exact(3).take(256).enumerate() {
            rgba[i] = [e[0], e[1], e[2], 0xFF];
        }
        // tRNS：只在前段填 alpha；tRNS 长于调色板项数 → 整块忽略（png 接受语义）。
        let entries = h.palette.len() / 3;
        let trns = if h.transparent_color.len() <= entries {
            &h.transparent_color[..]
        } else {
            &[]
        };
        for (alpha, slot) in trns.iter().copied().zip(rgba.iter_mut()) {
            slot[3] = alpha;
        }
        // 已覆盖 alpha 的项保持；未覆盖项（含越界）默认为 0xFF——上面的初始化已保证。
        PaletteTable { rgba }
    }

    #[inline]
    fn entry(&self, i: usize) -> &[u8; 4] {
        &self.rgba[i & 0xFF]
    }
}

/// 灰阶 sub-byte 的缩放表（`level * 255 / max`）。
fn gray_levels(depth: u8) -> [u8; 16] {
    let max = ((1u16 << depth) - 1) as u8;
    let mut levels = [0u8; 16];
    for (i, l) in levels.iter_mut().enumerate().take((max as usize) + 1) {
        *l = ((i as u16 * 255) / (max as u16)) as u8;
    }
    levels
}

/// 输出每像素字节数（= png crate EXPAND 输出色彩类型 × 位深的每像素字节）。
pub(crate) fn output_bpp(h: &FileHeader) -> usize {
    let trns = h.has_trns;
    match h.color_type {
        0 => match h.bit_depth {
            16 => {
                if trns {
                    4
                } else {
                    2
                }
            }
            _ => {
                if trns {
                    2
                } else {
                    1
                }
            }
        },
        2 => match h.bit_depth {
            16 => {
                if trns {
                    8
                } else {
                    6
                }
            }
            _ => {
                if trns {
                    4
                } else {
                    3
                }
            }
        },
        3 => {
            if trns {
                4
            } else {
                3
            }
        }
        4 => {
            if h.bit_depth == 16 {
                4
            } else {
                2
            }
        }
        6 => {
            if h.bit_depth == 16 {
                8
            } else {
                4
            }
        }
        _ => unreachable!("色彩类型已在前置校验"),
    }
}

/// 展开一行：`src` 为反滤波后的源行（打包 sub-byte 或样本字节），`dst` 为宽度 `width` 的输出行。
pub(crate) fn expand_row(
    kind: &ExpandKind,
    table: &PaletteTable,
    src: &[u8],
    dst: &mut [u8],
    width: usize,
) {
    if matches!(kind, ExpandKind::Copy) {
        debug_assert_eq!(dst.len(), src.len());
        dst.copy_from_slice(src);
        return;
    }
    debug_assert!(dst.len() >= output_len_for(kind, width));
    match *kind {
        ExpandKind::Copy => unreachable!(),
        ExpandKind::SubByteL { depth } => subbyte_gray(src, dst, width, depth, None),
        ExpandKind::SubByteLa { depth, key } => subbyte_gray(src, dst, width, depth, Some(key)),
        ExpandKind::G8ToLa8 { key } => gray8_la8(src, dst, width, key),
        ExpandKind::G16ToLa16 { key } => gray16_la16(src, dst, width, key),
        ExpandKind::Rgb8ToRgba8 { key } => rgb8_rgba8(kind, table, src, dst, width, key),
        ExpandKind::Rgb16ToRgba16 { key } => rgb16_rgba16(src, dst, width, key),
        ExpandKind::Palette { depth, rgba } => palette_expand(table, src, dst, width, depth, rgba),
    }
}

fn output_len_for(kind: &ExpandKind, width: usize) -> usize {
    match *kind {
        ExpandKind::Copy => width * 3, // 仅用于占位，Copy 实际按 src 长度
        ExpandKind::SubByteL { .. } => width,
        ExpandKind::SubByteLa { .. } | ExpandKind::G8ToLa8 { .. } => width * 2,
        ExpandKind::G16ToLa16 { .. } => width * 4,
        ExpandKind::Rgb8ToRgba8 { .. } => width * 4,
        ExpandKind::Rgb16ToRgba16 { .. } => width * 8,
        ExpandKind::Palette { rgba, .. } => width * if rgba { 4 } else { 3 },
    }
}

// ---------------------------------------------------------------------------
// 灰阶 sub-byte（1/2/4 位）→ L8 或 La8
// ---------------------------------------------------------------------------
fn subbyte_gray(src: &[u8], dst: &mut [u8], width: usize, depth: u8, key: Option<u8>) {
    let levels = gray_levels(depth);
    let per_byte = 8 / depth as usize;
    let mask = ((1u16 << depth) - 1) as u8;
    let two_chan = key.is_some();
    let mut x = 0;
    // 整字节批量。
    for &b in src.iter() {
        if x >= width {
            break;
        }
        let n = per_byte.min(width - x);
        for k in 0..n {
            let level = (b >> (8 - depth as usize - (k * depth as usize))) & mask;
            let g = levels[level as usize];
            if two_chan {
                dst[2 * x] = g;
                dst[2 * x + 1] = if Some(level) == key { 0 } else { 0xFF };
            } else {
                dst[x] = g;
            }
            x += 1;
        }
    }
}

// ---------------------------------------------------------------------------
// 灰阶 8 位 + tRNS → La8
// ---------------------------------------------------------------------------
fn gray8_la8(src: &[u8], dst: &mut [u8], width: usize, key: u8) {
    for i in 0..width {
        let g = src[i];
        dst[2 * i] = g;
        dst[2 * i + 1] = if g == key { 0 } else { 0xFF };
    }
}

// ---------------------------------------------------------------------------
// 灰阶 16 位 + tRNS → La16（源为大端）
// ---------------------------------------------------------------------------
fn gray16_la16(src: &[u8], dst: &mut [u8], width: usize, key: [u8; 2]) {
    for i in 0..width {
        let s = &src[2 * i..2 * i + 2];
        dst[4 * i..4 * i + 2].copy_from_slice(s);
        let a = &mut dst[4 * i + 2..4 * i + 4];
        if s == key {
            a.copy_from_slice(&[0, 0]);
        } else {
            a.copy_from_slice(&[0xFF, 0xFF]);
        }
    }
}

// ---------------------------------------------------------------------------
// RGB 8 位 + tRNS → Rgba8（SSSE3 重排；key 含 alpha 255）
// ---------------------------------------------------------------------------
fn rgb8_rgba8(
    _kind: &ExpandKind,
    _table: &PaletteTable,
    src: &[u8],
    dst: &mut [u8],
    width: usize,
    key: [u8; 4],
) {
    let mut x = 0usize;
    // AVX2 主径：每批 8 像素（24 源字节 → 32 输出字节，256 位 shuffle+blend）。
    // SSSE3 主径：每批 4 像素（16 源字节 → 16 输出字节）。一律多读尾字节保证边界安全，不越界。
    // 两者都返回「处理掉的像素数」，链式接力后再由标量尾补齐。
    #[cfg(target_arch = "x86_64")]
    if kimg_core::simd::gate("KIMG_PNG_NO_SIMD", "avx2") {
        unsafe {
            x = pg_simd::rgb8_to_rgba_avx2(&src[3 * x..], &mut dst[4 * x..], width - x);
        }
    }
    #[cfg(target_arch = "x86_64")]
    if x < width
        && kimg_core::simd::gate("KIMG_PNG_NO_SIMD", "ssse3")
    {
        unsafe {
            x += pg_simd::rgb8_to_rgba_ssse3(&src[3 * x..], &mut dst[4 * x..], width - x);
        }
    }
    // 标量尾补齐向量主径未覆盖的像素（最多 3 个），保证全宽度写出。
    for x in x..width {
        let i = &src[3 * x..3 * x + 3];
        dst[4 * x..4 * x + 3].copy_from_slice(i);
        dst[4 * x + 3] = 0xFF;
    }
    clear_key_alpha8(dst, width, key);
}

/// 把前 `width` 个 RGBA 像素里等于 `key`（含不透明 alpha）的 alpha 清 0（C# `ClearKeyAlpha8` 标量版）。
fn clear_key_alpha8(rgba: &mut [u8], width: usize, key: [u8; 4]) {
    let mut x = 0;
    #[cfg(target_arch = "x86_64")]
    {
        if kimg_core::simd::gate("KIMG_PNG_NO_SIMD", "sse2") {
            unsafe {
                x = pg_simd::clear_key_alpha8_sse2(rgba, width, key);
            }
        }
    }
    for px in rgba[4 * x..4 * width].chunks_exact_mut(4) {
        if px == key {
            px[3] = 0;
        }
    }
}

// ---------------------------------------------------------------------------
// RGB 16 位 + tRNS → Rgba16（源为大端；完整 16 位样本比对）
// ---------------------------------------------------------------------------
fn rgb16_rgba16(src: &[u8], dst: &mut [u8], width: usize, key: [u8; 6]) {
    for x in 0..width {
        let s = &src[6 * x..6 * x + 6];
        dst[8 * x..8 * x + 6].copy_from_slice(s);
        let a = &mut dst[8 * x + 6..8 * x + 8];
        if s == key {
            a.copy_from_slice(&[0, 0]);
        } else {
            a.copy_from_slice(&[0xFF, 0xFF]);
        }
    }
}

// ---------------------------------------------------------------------------
// 索引色 → Rgb8 / Rgba8（sub-byte 先解包出索引）
// ---------------------------------------------------------------------------
fn palette_expand(
    table: &PaletteTable,
    src: &[u8],
    dst: &mut [u8],
    width: usize,
    depth: u8,
    rgba: bool,
) {
    let mut x = 0;
    if depth < 8 {
        let per_byte = 8 / depth as usize;
        let mask = ((1u16 << depth) - 1) as u8;
        'outer: for &b in src.iter() {
            let n = per_byte.min(width.saturating_sub(x));
            if n == 0 {
                break;
            }
            for k in 0..n {
                let idx = (b >> (8 - depth as usize - (k * depth as usize))) & mask;
                emit_palette(table, dst, x, idx as usize, rgba);
                x += 1;
                if x == width {
                    break 'outer;
                }
            }
        }
    } else {
        for (i, &idx) in src.iter().enumerate().take(width) {
            emit_palette(table, dst, i, idx as usize, rgba);
        }
    }
}

#[inline]
fn emit_palette(table: &PaletteTable, dst: &mut [u8], x: usize, idx: usize, rgba: bool) {
    let e = table.entry(idx);
    if rgba {
        dst[4 * x..4 * x + 4].copy_from_slice(e);
    } else {
        dst[3 * x..3 * x + 3].copy_from_slice(&e[..3]);
    }
}

// ---------------------------------------------------------------------------
// x86_64 SSSE3 / SSE2 展开
// ---------------------------------------------------------------------------

#[cfg(target_arch = "x86_64")]
mod pg_simd {
    use std::arch::x86_64::*;

    /// RGB8 → RGBA8（alpha=255）AVX2：每批 8 像素（24 源字节 → 32 输出字节）。
    /// 8 像素 = 24 字节跨 16 字节道界且不在像素边界上，故不直接用 32 字节 shuffle；改为两次
    /// 128 位载入（偏移 3x 与 3x+12，各含 4 像素整 12 字节 + 4 尾字节）各做一次 SSSE3 重排，
    /// 拼成 256 位一次性写出（少一半循环/存储指令；SSSE3 主径照旧仅作回退）。返回处理掉的像素数。
    #[target_feature(enable = "avx2", enable = "ssse3")]
    pub(crate) unsafe fn rgb8_to_rgba_avx2(src: &[u8], dst: &mut [u8], width: usize) -> usize {
        unsafe {
            let m = _mm_setr_epi8(0, 1, 2, -128, 3, 4, 5, -128, 6, 7, 8, -128, 9, 10, 11, -128);
            let alpha = _mm_setr_epi8(0, 0, 0, -1, 0, 0, 0, -1, 0, 0, 0, -1, 0, 0, 0, -1);
            let s = src.as_ptr();
            let d = dst.as_mut_ptr();
            let mut x = 0usize;
            // 两次 16 字节载入（3x 与 3x+12）都须整段在行内：3x+12+16<=3w ⇒ 3x+28<=3w。
            while 3 * x + 28 <= 3 * width {
                let v0 = _mm_loadu_si128(s.add(3 * x) as *const __m128i);
                let v1 = _mm_loadu_si128(s.add(3 * x + 12) as *const __m128i);
                let out0 = _mm_or_si128(_mm_shuffle_epi8(v0, m), alpha);
                let out1 = _mm_or_si128(_mm_shuffle_epi8(v1, m), alpha);
                _mm256_storeu_si256(d.add(4 * x) as *mut __m256i, _mm256_set_m128i(out1, out0));
                x += 8;
            }
            x
        }
    }

    /// RGB8 → RGBA8（alpha=255）：16 字节一次处理 5.33 像素的外循环与 C# `Rgb8` 同款，
    /// `while 3x + 16 <= 3·width`。掩码 `m` 与源行字节一一对应。
    #[target_feature(enable = "ssse3")]
    pub(crate) unsafe fn rgb8_to_rgba_ssse3(src: &[u8], dst: &mut [u8], width: usize) -> usize {
        unsafe {
            let m = _mm_setr_epi8(0, 1, 2, -128, 3, 4, 5, -128, 6, 7, 8, -128, 9, 10, 11, -128);
            let alpha = _mm_setr_epi8(0, 0, 0, -1, 0, 0, 0, -1, 0, 0, 0, -1, 0, 0, 0, -1);
            let s = src.as_ptr();
            let d = dst.as_mut_ptr();
            let mut x = 0usize;
            while 3 * x + 16 <= 3 * width {
                let v = _mm_loadu_si128(s.add(3 * x) as *const __m128i);
                let out = _mm_or_si128(_mm_shuffle_epi8(v, m), alpha);
                _mm_storeu_si128(d.add(4 * x) as *mut __m128i, out);
                x += 4;
            }
            x
        }
    }

    /// 把前 `width` 个 RGBA 像素中等于 key 的 alpha 清 0（SSE2 每 4 像素，常 4 字节比较；返回标量起点）。
    #[target_feature(enable = "sse2")]
    pub(crate) unsafe fn clear_key_alpha8_sse2(
        rgba: &mut [u8],
        width: usize,
        key: [u8; 4],
    ) -> usize {
        unsafe {
            let k = u32::from_ne_bytes(key);
            let key4 = _mm_set1_epi32(k as i32);
            // alpha 是像素的字节 3（内存序第 4 字节）→ u32 高字节：掩码 0xFF000000。
            let alpha_mask = _mm_set1_epi32(0xFF00_0000u32 as i32);
            let d = rgba.as_mut_ptr();
            let mut x = 0usize;
            while x + 4 <= width {
                let v = _mm_loadu_si128(d.add(4 * x) as *const __m128i);
                let eq = _mm_cmpeq_epi32(v, key4);
                // eq 的每 32 位道 = 0 或 -1；只有 alpha 字节可能被清零：v & ~clear。
                let clear = _mm_and_si128(eq, alpha_mask);
                let out = _mm_andnot_si128(clear, v);
                _mm_storeu_si128(d.add(4 * x) as *mut __m128i, out);
                x += 4;
            }
            x
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(color_type: u8, bit_depth: u8, trns: &[u8], palette: &[u8]) -> FileHeader {
        FileHeader {
            width: 0,
            height: 0,
            bit_depth,
            color_type,
            interlace: 0,
            palette: palette.to_vec(),
            transparent_color: trns.to_vec(),
            has_trns: !trns.is_empty(),
            exif: None,
            idat_chunks: Vec::new(),
        }
    }

    fn lcg(seed: &mut u64) -> u8 {
        *seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (*seed >> 56) as u8
    }

    /// A2 断言：sub-byte / palette / tRNS 合成路径**不得**走 Copy（恒等）——它们必须进
    /// `decode_scanlines` 的展开分支，否则源行以输出行字节数被误读（`srow != out_row`）。
    /// Copy 只允许在「源字节 == 输出字节」的恒等组合（灰阶/RGB 8·16 位无 tRNS，灰α/RGBA）。
    #[test]
    fn synth_paths_not_copy_identity_only() {
        // 合成路径（srow != out_row 或需调色板/tRNS 变换）：必须非 Copy。
        for (ct, depth, trns, palette) in [
            (0u8, 1u8, false, false), // sub-byte 灰阶
            (0, 2, false, false),
            (0, 4, false, false),
            (0, 4, true, false), // sub-byte + tRNS
            (0, 8, true, false), // 灰阶 8 + tRNS → La8
            (0, 16, true, false),
            (2, 8, true, false), // RGB + tRNS → Rgba
            (2, 16, true, false),
            (3, 1, false, true), // 调色板（全部位深，恒不恒等）
            (3, 2, false, true),
            (3, 4, false, true),
            (3, 8, false, true),
            (3, 8, true, true), // 调色板 + tRNS → Rgba
        ] {
            let file_header = {
                let mut fh = FileHeader {
                    width: 64,
                    height: 1,
                    bit_depth: depth,
                    color_type: ct,
                    interlace: 0,
                    palette: if palette {
                        vec![0u8; 3 * 32]
                    } else {
                        Vec::new()
                    },
                    transparent_color: if trns { vec![0u8; 3] } else { Vec::new() },
                    has_trns: trns,
                    exif: None,
                    idat_chunks: Vec::new(),
                };
                if ct == 0 || ct == 2 {
                    // 灰阶/RGB 的 tRNS 样本键占 2/6 字节（低字节语义见 chunk.rs 解析）。
                    fh.transparent_color = if trns {
                        vec![0x00u8; if ct == 0 { 2 } else { 6 }]
                    } else {
                        Vec::new()
                    };
                    fh.has_trns = trns;
                }
                fh
            };
            let kind = build_expand_kind(&file_header);
            assert!(
                !matches!(kind, ExpandKind::Copy),
                "(ct={ct} depth={depth} trns={trns} pal={palette}) 合成路径不得走 Copy，得 {kind:?}"
            );
        }
        // 恒等组合：必须 Copy。
        for (ct, depth, trns) in [
            (0u8, 8u8, false),
            (0, 16, false),
            (2, 8, false),
            (2, 16, false),
            (4, 8, false),
            (4, 16, false),
            (6, 8, false),
            (6, 16, false),
        ] {
            let fh = FileHeader {
                width: 64,
                height: 1,
                bit_depth: depth,
                color_type: ct,
                interlace: 0,
                palette: Vec::new(),
                transparent_color: Vec::new(),
                has_trns: trns,
                exif: None,
                idat_chunks: Vec::new(),
            };
            let kind = build_expand_kind(&fh);
            assert!(
                matches!(kind, ExpandKind::Copy),
                "(ct={ct} depth={depth} trns={trns}) 恒等路径应为 Copy，得 {kind:?}"
            );
        }
    }

    /// 参考实现：逐像素展开（不经 SIMD）。
    fn reference_expand(
        kind: &ExpandKind,
        table: &PaletteTable,
        src: &[u8],
        width: usize,
    ) -> Vec<u8> {
        let bpp = output_len_for_scalar(kind, width, src.len());
        let mut dst = vec![0u8; bpp];
        match *kind {
            ExpandKind::Copy => dst[..src.len()].copy_from_slice(src),
            ExpandKind::SubByteL { depth } => {
                for x in 0..width {
                    let level = sample_at(src, x, depth);
                    dst[x] = ((level as u16 * 255) / ((1u16 << depth) - 1)) as u8;
                }
            }
            ExpandKind::SubByteLa { depth, key } => {
                for x in 0..width {
                    let level = sample_at(src, x, depth);
                    dst[2 * x] = ((level as u16 * 255) / ((1u16 << depth) - 1)) as u8;
                    dst[2 * x + 1] = if level == key { 0 } else { 0xFF };
                }
            }
            ExpandKind::G8ToLa8 { key } => {
                for x in 0..width {
                    dst[2 * x] = src[x];
                    dst[2 * x + 1] = if src[x] == key { 0 } else { 0xFF };
                }
            }
            ExpandKind::G16ToLa16 { key } => {
                for x in 0..width {
                    dst[4 * x..4 * x + 2].copy_from_slice(&src[2 * x..2 * x + 2]);
                    let a = if src[2 * x..2 * x + 2] == key {
                        [0, 0]
                    } else {
                        [0xFF, 0xFF]
                    };
                    dst[4 * x + 2..4 * x + 4].copy_from_slice(&a);
                }
            }
            ExpandKind::Rgb8ToRgba8 { key } => {
                for x in 0..width {
                    dst[4 * x..4 * x + 3].copy_from_slice(&src[3 * x..3 * x + 3]);
                    dst[4 * x + 3] = 0xFF;
                }
                for px in dst.chunks_exact_mut(4) {
                    if px == &key {
                        px[3] = 0;
                    }
                }
            }
            ExpandKind::Rgb16ToRgba16 { key } => {
                for x in 0..width {
                    dst[8 * x..8 * x + 6].copy_from_slice(&src[6 * x..6 * x + 6]);
                    let a = if src[6 * x..6 * x + 6] == key {
                        [0, 0]
                    } else {
                        [0xFF, 0xFF]
                    };
                    dst[8 * x + 6..8 * x + 8].copy_from_slice(&a);
                }
            }
            ExpandKind::Palette { depth, rgba } => {
                for x in 0..width {
                    let idx = sample_at(src, x, depth) as usize;
                    let e = table.entry(idx);
                    if rgba {
                        dst[4 * x..4 * x + 4].copy_from_slice(e);
                    } else {
                        dst[3 * x..3 * x + 3].copy_from_slice(&e[..3]);
                    }
                }
            }
        }
        dst
    }

    fn output_len_for_scalar(kind: &ExpandKind, width: usize, src_len: usize) -> usize {
        match *kind {
            ExpandKind::Copy => src_len,
            ExpandKind::SubByteL { .. } => width * 1,
            ExpandKind::SubByteLa { .. } | ExpandKind::G8ToLa8 { .. } => width * 2,
            ExpandKind::G16ToLa16 { .. } => width * 4,
            ExpandKind::Rgb8ToRgba8 { .. } => width * 4,
            ExpandKind::Rgb16ToRgba16 { .. } => width * 8,
            ExpandKind::Palette { rgba, .. } => width * if rgba { 4 } else { 3 },
        }
    }

    fn sample_at(src: &[u8], x: usize, depth: u8) -> u8 {
        let per_byte = 8 / depth as usize;
        let byte = x / per_byte;
        let k = x % per_byte;
        (src[byte] >> (8 - depth as usize - k * depth as usize)) & (((1u16 << depth) - 1) as u8)
    }

    #[test]
    fn expand_matches_reference_all_kinds() {
        let cases: Vec<(u8, u8, &[u8], &[u8])> = vec![
            (0, 1, &[], &[]),
            (0, 2, &[], &[]),
            (0, 4, &[], &[]),
            (0, 1, &[0x01, 0x00], &[]),
            (0, 4, &[0x0F, 0x00], &[]),
            (0, 8, &[0x80, 0x00], &[]),
            (0, 16, &[0x12, 0x34], &[]),
            (2, 8, &[1, 2, 3, 255, 255, 254], &[]),
            (2, 16, &[0, 1, 0, 2, 0, 3], &[]),
            (3, 8, &[], &[10, 20, 30, 40, 50, 60, 70, 80, 90]),
            (3, 4, &[5, 9], &[10, 20, 30, 40, 50, 60, 70, 80, 90]),
            (3, 1, &[1], &[10, 20, 30]),
            (3, 2, &[2, 255], &[10, 20, 30]),
        ];
        for (ct, depth, trns, plte) in cases {
            let h = header(ct, depth, trns, plte);
            let kind = build_expand_kind(&h);
            let table = PaletteTable::from_header(&h);
            if matches!(kind, ExpandKind::Copy) {
                continue; // Copy 由非隔行直接路径与 Adam7 的 memcpy 覆盖
            }
            let width = 64;
            let src_len = kind_src_len(&kind, width);
            let mut seed = 0x0102030405060708u64;
            let src: Vec<u8> = (0..src_len).map(|_| lcg(&mut seed)).collect();
            let mut dst = vec![0u8; output_len_for_scalar(&kind, width, src_len)];
            expand_row(&kind, &table, &src, &mut dst, width);
            let expected = reference_expand(&kind, &table, &src, width);
            assert_eq!(
                dst, expected,
                "expand 不符 (ct={ct} depth={depth} trns={trns:?})"
            );
        }
    }

    fn kind_src_len(kind: &ExpandKind, width: usize) -> usize {
        match *kind {
            ExpandKind::Copy => width * 3,
            ExpandKind::SubByteL { depth }
            | ExpandKind::SubByteLa { depth, .. }
            | ExpandKind::Palette { depth, .. } => (width * depth as usize + 7) / 8,
            ExpandKind::G8ToLa8 { .. } => width,
            ExpandKind::G16ToLa16 { .. } => width * 2,
            ExpandKind::Rgb8ToRgba8 { .. } => width * 3,
            ExpandKind::Rgb16ToRgba16 { .. } => width * 6,
        }
    }

    /// RGB8→RGBA8：SSSE3 / AVX2 / 纯标量 三条路径字节级一致（含 SIMD 尾段的标量补齐）。
    /// 按运行 CPU 特性单开其 SIMD 主径，其余路径全开以互相锁定；宽度覆盖向量主径 + 各路尾数。
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn rgbsimd_paths_byte_identical() {
        let widths = [
            1usize, 3, 4, 7, 8, 9, 12, 15, 16, 17, 24, 25, 31, 32, 33, 56, 63, 64, 65, 96, 128,
            129, 1000,
        ];
        let mut seed = 0x0F0F_0F0F_0F0Fu64;
        for width in widths {
            let src: Vec<u8> = (0..3 * width).map(|_| lcg(&mut seed)).collect();
            // 纯标量参考。
            let mut scalar = vec![0u8; 4 * width];
            for x in 0..width {
                scalar[4 * x..4 * x + 3].copy_from_slice(&src[3 * x..3 * x + 3]);
                scalar[4 * x + 3] = 0xFF;
            }
            // SSSE3 路径（主径 + 标量尾）。
            let mut ssse3 = vec![0u8; 4 * width];
            if kimg_core::simd::detect("ssse3") {
                let n = unsafe { super::pg_simd::rgb8_to_rgba_ssse3(&src, &mut ssse3, width) };
                for x in n..width {
                    ssse3[4 * x..4 * x + 3].copy_from_slice(&src[3 * x..3 * x + 3]);
                    ssse3[4 * x + 3] = 0xFF;
                }
                assert_eq!(ssse3, scalar, "SSSE3 与标量不一致 width={width}");
            }
            // AVX2 路径（主径 + 标量尾）。
            let mut avx2 = vec![0u8; 4 * width];
            if kimg_core::simd::detect("avx2") {
                let n = unsafe { super::pg_simd::rgb8_to_rgba_avx2(&src, &mut avx2, width) };
                for x in n..width {
                    avx2[4 * x..4 * x + 3].copy_from_slice(&src[3 * x..3 * x + 3]);
                    avx2[4 * x + 3] = 0xFF;
                }
                assert_eq!(avx2, scalar, "AVX2 与标量不一致 width={width}");
            }
            // AVX2→SSSE3 接力（走 dispatch 同款串联）：两段 SIMD 后再标量尾 == 标量。
            let mut chained = vec![0u8; 4 * width];
            let mut x = 0usize;
            if kimg_core::simd::detect("avx2") {
                x = unsafe {
                    super::pg_simd::rgb8_to_rgba_avx2(
                        &src[3 * x..],
                        &mut chained[4 * x..],
                        width - x,
                    )
                };
            }
            if x < width && kimg_core::simd::detect("ssse3") {
                x += unsafe {
                    super::pg_simd::rgb8_to_rgba_ssse3(
                        &src[3 * x..],
                        &mut chained[4 * x..],
                        width - x,
                    )
                };
            }
            for xx in x..width {
                chained[4 * xx..4 * xx + 3].copy_from_slice(&src[3 * xx..3 * xx + 3]);
                chained[4 * xx + 3] = 0xFF;
            }
            assert_eq!(chained, scalar, "AVX2→SSSE3 接力与标量不一致 width={width}");
        }
    }
}
