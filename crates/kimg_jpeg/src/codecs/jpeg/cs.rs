//! C# 逐字节恒等的数值核心（等效 Sdcb.SimdCodecs.Jpeg `DecodeTo` 标量路径）。
//!
//! 语义契约见 `docs/jpeg/CS_ALGORITHM.md`（最高权威），有歧义一律以
//! `sample/SimdCodecs/src/Sdcb.SimdCodecs.Jpeg/` 下的 C# 源码为准。全部浮点为
//! IEEE 单精度、严格求值（无 FMA、无重结合——Rust 缺省不收缩 `a*b+c`，也不写
//! `mul_add`）。本模块只做标量；AVX2 向量化是下一轮。
//!
//! 三条链路：f32 两遍 IDCT（列→行）→ 16× 整数色度上采样（`Row16`）→ T.871
//! 浮点颜色换算 + 打包。灰度走同一 f32 IDCT + 同一取整，无颜色换算。

// ==========================================================================
// 常量（C# 编译期 `(float)double` 的精确位模式，见 CS_ALGORITHM.md §10）
// ==========================================================================

/// `1/(2√2)` = 0.35355338
pub(crate) const K4: f32 = f32::from_bits(0x3eb504f3);
/// `C1 = cos(π/16)/2` = 0.49039263
pub(crate) const C1: f32 = f32::from_bits(0x3efb14be);
/// `C2 = cos(2π/16)/2` = 0.46193975
pub(crate) const C2: f32 = f32::from_bits(0x3eec835e);
/// `C3 = cos(3π/16)/2` = 0.41573480
pub(crate) const C3: f32 = f32::from_bits(0x3ed4db31);
/// `C5 = cos(5π/16)/2` = 0.27778512
pub(crate) const C5: f32 = f32::from_bits(0x3e8e39da);
/// `C6 = cos(6π/16)/2` = 0.19134171
pub(crate) const C6: f32 = f32::from_bits(0x3e43ef15);
/// `C7 = cos(7π/16)/2` = 0.09754516
pub(crate) const C7: f32 = f32::from_bits(0x3dc7c5c2);
/// 电平移位偏置 `128.5f`（取整在加偏置后的 trunc）
pub(crate) const BIAS: f32 = f32::from_bits(0x43008000);

/// `(float)(1.402 / 16)` = 0.087625
pub(crate) const KR: f32 = f32::from_bits(0x3db374bc);
/// `(float)(-(0.114*1.772/0.587)/16)` = −0.021508519
pub(crate) const KGB: f32 = f32::from_bits(0xbcb032a2);
/// `(float)(-(0.299*1.402/0.587)/16)` = −0.04463352
pub(crate) const KGR: f32 = f32::from_bits(0xbd36d1a3);
/// `(float)(1.772 / 16)` = 0.11075
pub(crate) const KB: f32 = f32::from_bits(0x3de2d0e5);

// 上采样核类型（`JpegResample`）。语义：0..4 与 C# 常量一一对应。
pub(crate) const R_ONE_TO_ONE: u8 = 0;
pub(crate) const R_V2: u8 = 1;
pub(crate) const R_H2: u8 = 2;
pub(crate) const R_HV2: u8 = 3;
pub(crate) const R_GENERIC: u8 = 4;

// ==========================================================================
// 2-D f32 IDCT（`JpegIdct.Idct8x8` 标量路径）
// ==========================================================================

/// `trunc(s + 128.5)` 后 clamp[0,255]（`ToSample`；非 `round`）。
#[inline(always)]
pub(crate) fn to_sample(s: f32) -> u8 {
    ((s + BIAS) as i32).clamp(0, 255) as u8
}

/// 1-D 长度 8 变换（`Idct1D` 标量），严格左→右求值、逐积逐和单独舍入。
#[inline(always)]
#[allow(clippy::too_many_arguments)]
fn idct_1d(f0: f32, f1: f32, f2: f32, f3: f32, f4: f32, f5: f32, f6: f32, f7: f32) -> (f32, f32, f32, f32, f32, f32, f32, f32) {
    let a0 = (f0 + f4) * K4;
    let a1 = (f0 - f4) * K4;
    let b0 = f2 * C2 + f6 * C6;
    let b1 = f2 * C6 - f6 * C2;
    let e0 = a0 + b0;
    let e1 = a1 + b1;
    let e2 = a1 - b1;
    let e3 = a0 - b0;
    let o0 = f1 * C1 + f3 * C3 + f5 * C5 + f7 * C7;
    let o1 = f1 * C3 - f3 * C7 - f5 * C1 - f7 * C5;
    let o2 = f1 * C5 - f3 * C1 + f5 * C7 + f7 * C3;
    let o3 = f1 * C7 - f3 * C5 + f5 * C3 - f7 * C1;
    let s0 = e0 + o0;
    let s1 = e1 + o1;
    let s2 = e2 + o2;
    let s3 = e3 + o3;
    let s4 = e3 - o3;
    let s5 = e2 - o2;
    let s6 = e1 - o1;
    let s7 = e0 - o0;
    (s0, s1, s2, s3, s4, s5, s6, s7)
}

/// 完整 8×8：先列 pass（存入转置 `t`）再行 pass，输出字节平面（含电平移位+取整+clamp）。
/// `data` 为 natural-order `short[64]`；`out` 以 `(offset, stride)` 写 8×8 字节块。
pub(crate) fn idct8x8(data: &[i16; 64], out: &mut [u8], offset: usize, stride: usize) {
    #[cfg(target_arch = "x86_64")]
    if super::avx2::avx2_enabled() {
        // SAFETY: avx2_enabled() 已保证 CPU 支持 AVX2 且未被 env 关闭。
        unsafe { super::cs_avx2::idct8x8_avx2(data, out, offset, stride) };
        return;
    }
    let mut t = [0.0f32; 64];
    for x in 0..8 {
        let (s0, s1, s2, s3, s4, s5, s6, s7) = idct_1d(
            f32::from(data[x]),
            f32::from(data[8 + x]),
            f32::from(data[16 + x]),
            f32::from(data[24 + x]),
            f32::from(data[32 + x]),
            f32::from(data[40 + x]),
            f32::from(data[48 + x]),
            f32::from(data[56 + x]),
        );
        t[x] = s0;
        t[8 + x] = s1;
        t[16 + x] = s2;
        t[24 + x] = s3;
        t[32 + x] = s4;
        t[40 + x] = s5;
        t[48 + x] = s6;
        t[56 + x] = s7;
    }
    for y in 0..8 {
        let r = &t[y * 8..y * 8 + 8];
        let (s0, s1, s2, s3, s4, s5, s6, s7) = idct_1d(r[0], r[1], r[2], r[3], r[4], r[5], r[6], r[7]);
        let o = offset + y * stride;
        out[o] = to_sample(s0);
        out[o + 1] = to_sample(s1);
        out[o + 2] = to_sample(s2);
        out[o + 3] = to_sample(s3);
        out[o + 4] = to_sample(s4);
        out[o + 5] = to_sample(s5);
        out[o + 6] = to_sample(s6);
        out[o + 7] = to_sample(s7);
    }
}

/// DC-only 块：每个样本 = `((float)dc * K4) * K4`（与全量变换逐位恒等，快路径）。
pub(crate) fn idct8x8_dc_only(dc: i16, out: &mut [u8], offset: usize, stride: usize) {
    let v = to_sample((f32::from(dc) * K4) * K4);
    for y in 0..8 {
        let o = offset + y * stride;
        out[o..o + 8].fill(v);
    }
}

// ==========================================================================
// 16× 整数色度上采样（`JpegResample.Row` / `Row16`；纯整数，无取整）
// ==========================================================================

/// 核输出样本数：`OneToOne|V2 → w`；`H2|Hv2 → 2w`；`Generic → w*hs`。
pub(crate) fn output_count(kind: u8, w: usize, hs: usize) -> usize {
    match kind {
        R_ONE_TO_ONE | R_V2 => w,
        R_H2 | R_HV2 => 2 * w,
        _ => w * hs,
    }
}

/// `T(i) = 3·near[i] + far[i]`。
#[inline(always)]
fn t(plane: &[u8], near: usize, far: usize, i: usize) -> i32 {
    3 * i32::from(plane[near + i]) + i32::from(plane[far + i])
}

/// `Hv2Row`（`JpegResample.cs:132`）：`T=3n+f`，边缘竖直-only 混合。
fn hv2_row(dst: &mut [u16], plane: &[u8], near: usize, far: usize, w: usize) {
    let mut t1 = t(plane, near, far, 0);
    if w == 1 {
        let v = (t1 << 2) as u16;
        dst[0] = v;
        dst[1] = v;
        return;
    }
    dst[0] = (t1 << 2) as u16;
    for i in 1..w {
        let t0 = t1;
        t1 = t(plane, near, far, i);
        dst[2 * i - 1] = (3 * t0 + t1) as u16;
        dst[2 * i] = (3 * t1 + t0) as u16;
    }
    dst[2 * w - 1] = (t(plane, near, far, w - 1) << 2) as u16;
}

/// `Row16`：不取整的 16× 标度上采样（ushort 0..4080），直接喂颜色换算。
pub(crate) fn row16(
    kind: u8,
    dst: &mut [u16],
    plane: &[u8],
    in_near: usize,
    in_far: usize,
    w: usize,
    hs: usize,
) {
    debug_assert!(dst.len() >= output_count(kind, w, hs));
    #[cfg(target_arch = "x86_64")]
    if super::avx2::avx2_enabled() {
        // SAFETY: avx2_enabled() 已保证 CPU 支持 AVX2 且未被 env 关闭。
        unsafe { super::cs_avx2::row16_avx2(kind, dst, plane, in_near, in_far, w, hs) };
        return;
    }
    match kind {
        R_ONE_TO_ONE => {
            for i in 0..w {
                dst[i] = (u16::from(plane[in_near + i])) << 4;
            }
        }
        R_V2 => {
            for i in 0..w {
                dst[i] = ((3 * i32::from(plane[in_near + i]) + i32::from(plane[in_far + i])) << 2)
                    as u16;
            }
        }
        R_H2 => hv2_row(dst, plane, in_near, in_near, w),
        R_HV2 => hv2_row(dst, plane, in_near, in_far, w),
        _ => {
            for i in 0..w {
                let v = (u16::from(plane[in_near + i])) << 4;
                for j in 0..hs {
                    dst[i * hs + j] = v;
                }
            }
        }
    }
}

/// `Row`：`Row16` 后 `(v + 8) >> 4` 取整到 8 位（仅非颜色换算分量用，如 RGB/CMYK/K）。
pub(crate) fn row8(
    kind: u8,
    dst: &mut [u8],
    scratch: &mut [u16],
    plane: &[u8],
    in_near: usize,
    in_far: usize,
    w: usize,
    hs: usize,
) {
    #[cfg(target_arch = "x86_64")]
    if super::avx2::avx2_enabled() {
        // SAFETY: avx2_enabled() 已保证 CPU 支持 AVX2 且未被 env 关闭。
        unsafe {
            super::cs_avx2::row8_avx2(kind, dst, scratch, plane, in_near, in_far, w, hs)
        };
        return;
    }
    row16(kind, scratch, plane, in_near, in_far, w, hs);
    let n = output_count(kind, w, hs);
    for i in 0..n {
        dst[i] = ((scratch[i] + 8) >> 4) as u8;
    }
}

// ==========================================================================
// T.871 浮点颜色换算（`JpegColor`；严格左→右，无 FMA）
// ==========================================================================

/// `ToByte`：`+0.5f` 在浮点上，float min/max clamp [0,255]，然后 trunc。
#[inline(always)]
fn to_byte(v: f32) -> u8 {
    (v + 0.5f32).clamp(0.0, 255.0) as i32 as u8
}

/// `Qcc` 单像素核心。
#[inline(always)]
fn ycc(yf: f32, cb: f32, cr: f32) -> (u8, u8, u8) {
    let r = yf + cr * KR;
    let g = (yf + cb * KGB) + cr * KGR;
    let b = yf + cb * KB;
    (to_byte(r), to_byte(g), to_byte(b))
}

/// 16× 色度（subsampled）：`cb16 − 2048` 先 int 后转 f32。
pub(crate) fn ycc_row16(
    y: &[u8],
    cb16: &[u16],
    cr16: &[u16],
    out: &mut [u8],
    n: usize,
    out_bpp: usize,
) {
    debug_assert!(out.len() >= n * out_bpp);
    #[cfg(target_arch = "x86_64")]
    if super::avx2::avx2_enabled() {
        // SAFETY: avx2_enabled() 已保证 CPU 支持 AVX2 且未被 env 关闭。
        unsafe { super::cs_avx2::ycc_row16_avx2(y, cb16, cr16, out, n, out_bpp) };
        return;
    }
    for i in 0..n {
        let cb = (i32::from(cb16[i]) - 2048) as f32;
        let cr = (i32::from(cr16[i]) - 2048) as f32;
        let (r, g, b) = ycc(f32::from(y[i]), cb, cr);
        let o = i * out_bpp;
        out[o] = r;
        out[o + 1] = g;
        out[o + 2] = b;
        for _ in 3..out_bpp {
            out[o + 3] = 255;
        }
    }
}

/// 8 位全分辨率色度（4:4:4）：`(cb << 4) − 2048` 先 int 后转 f32。
pub(crate) fn ycc_row8(
    y: &[u8],
    cb: &[u8],
    cr: &[u8],
    out: &mut [u8],
    n: usize,
    out_bpp: usize,
) {
    debug_assert!(out.len() >= n * out_bpp);
    #[cfg(target_arch = "x86_64")]
    if super::avx2::avx2_enabled() {
        // SAFETY: avx2_enabled() 已保证 CPU 支持 AVX2 且未被 env 关闭。
        unsafe { super::cs_avx2::ycc_row8_avx2(y, cb, cr, out, n, out_bpp) };
        return;
    }
    for i in 0..n {
        let cb = ((i32::from(cb[i]) << 4) - 2048) as f32;
        let cr = ((i32::from(cr[i]) << 4) - 2048) as f32;
        let (r, g, b) = ycc(f32::from(y[i]), cb, cr);
        let o = i * out_bpp;
        out[o] = r;
        out[o + 1] = g;
        out[o + 2] = b;
        for _ in 3..out_bpp {
            out[o + 3] = 255;
        }
    }
}

/// `Blinn8x8`：`(x*k + 128 + ((x*k+128)>>8)) >> 8`（算术右移，溢出安全）。
#[inline(always)]
pub(crate) fn blinn(x: u8, k: u8) -> u8 {
    let t = i32::from(x) * i32::from(k) + 128;
    ((t + (t >> 8)) >> 8) as u8
}

/// 三平面直接复制（输入 RGB）。
#[inline(always)]
fn rgb_row(r: &[u8], g: &[u8], b: &[u8], out: &mut [u8], n: usize, out_bpp: usize) {
    for i in 0..n {
        let o = i * out_bpp;
        out[o] = r[i];
        out[o + 1] = g[i];
        out[o + 2] = b[i];
        for _ in 3..out_bpp {
            out[o + 3] = 255;
        }
    }
}

/// 灰度复制（输出 1 压缩 / 或每像素 1B / 或 3/4B 复制，`out_bpp` 控制）。
#[inline(always)]
fn gray_row(y: &[u8], out: &mut [u8], n: usize, out_bpp: usize) {
    match out_bpp {
        1 => out[..n].copy_from_slice(&y[..n]),
        _ => {
            for i in 0..n {
                let o = i * out_bpp;
                out[o] = y[i];
                for c in 1..out_bpp {
                    out[o + c] = 255;
                }
            }
        }
    }
}

/// CMYK：每墨道 `Blinn(ink, K)`。
pub(crate) fn cmyk_row(c: &[u8], m: &[u8], y: &[u8], k: &[u8], out: &mut [u8], n: usize, out_bpp: usize) {
    for i in 0..n {
        let kk = k[i];
        let o = i * out_bpp;
        out[o] = blinn(c[i], kk);
        out[o + 1] = blinn(m[i], kk);
        out[o + 2] = blinn(y[i], kk);
        for _ in 3..out_bpp {
            out[o + 3] = 255;
        }
    }
}

/// YCCK：YCbCr→RGB 反相后用 K 乘（16× 色度版）。
pub(crate) fn ycck_row16(
    y: &[u8],
    cb16: &[u16],
    cr16: &[u16],
    k: &[u8],
    out: &mut [u8],
    n: usize,
    out_bpp: usize,
) {
    for i in 0..n {
        let cb = (i32::from(cb16[i]) - 2048) as f32;
        let cr = (i32::from(cr16[i]) - 2048) as f32;
        let (r, g, b) = ycc(f32::from(y[i]), cb, cr);
        let kk = k[i];
        let o = i * out_bpp;
        out[o] = blinn(255 - r, kk);
        out[o + 1] = blinn(255 - g, kk);
        out[o + 2] = blinn(255 - b, kk);
        for _ in 3..out_bpp {
            out[o + 3] = 255;
        }
    }
}

/// YCCK（8 位全分辨率色度版）。
pub(crate) fn ycck_row8(
    y: &[u8],
    cb: &[u8],
    cr: &[u8],
    k: &[u8],
    out: &mut [u8],
    n: usize,
    out_bpp: usize,
) {
    for i in 0..n {
        let cb = ((i32::from(cb[i]) << 4) - 2048) as f32;
        let cr = ((i32::from(cr[i]) << 4) - 2048) as f32;
        let (r, g, b) = ycc(f32::from(y[i]), cb, cr);
        let kk = k[i];
        let o = i * out_bpp;
        out[o] = blinn(255 - r, kk);
        out[o + 1] = blinn(255 - g, kk);
        out[o + 2] = blinn(255 - b, kk);
        for _ in 3..out_bpp {
            out[o + 3] = 255;
        }
    }
}

/// 输入颜色空间类别（`pack_output_row` 用于选择行核）。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CsInput {
    /// 1 分量灰度（直接输出 Y 字节）。
    Luma,
    /// 3 分量 RGB 编码（平面直通）。
    Rgb,
    /// 3 分量 YCbCr（含 4:4:4 8 位色度）。
    Ycc,
    /// 4 分量 CMYK（Adobe transform 0）。
    Cmyk,
    /// 4 分量 YCCK（Adobe transform 2）。
    Ycck,
    /// 其余（RGBA/La/多带等：退化直通）。
    Other,
}

/// 输出行装配：按输入颜色空间与 16×/8 位色度分支写一行到 `out`（行首）。
/// `rows[k]` = 分量 k 的 `输出分辨率字节行`（`&[u8]`，8 位；直接来自环或 `row8`）。
/// `rows16[k]` = 分量 1/2 的 16× 色度行（`&[u16]`，仅 `chroma16` 用）。
pub(crate) fn pack_output_row(
    input: CsInput,
    out_bpp: usize,
    chroma16: bool,
    rows: &[&[u8]],
    rows16: &[&[u16]],
    out: &mut [u8],
    n: usize,
) {
    debug_assert!(out.len() >= n * out_bpp);
    match input {
        CsInput::Luma => gray_row(rows[0], out, n, out_bpp),
        CsInput::Rgb => rgb_row(rows[0], rows[1], rows[2], out, n, out_bpp),
        CsInput::Cmyk => cmyk_row(rows[0], rows[1], rows[2], rows[3], out, n, out_bpp),
        CsInput::Ycck => {
            if chroma16 {
                ycck_row16(rows[0], rows16[1], rows16[2], rows[3], out, n, out_bpp);
            } else {
                ycck_row8(rows[0], rows[1], rows[2], rows[3], out, n, out_bpp);
            }
        }
        CsInput::Ycc => {
            if chroma16 {
                ycc_row16(rows[0], rows16[1], rows16[2], out, n, out_bpp);
            } else {
                ycc_row8(rows[0], rows[1], rows[2], out, n, out_bpp);
            }
        }
        CsInput::Other => gray_row(rows[0], out, n, out_bpp),
    }
}
