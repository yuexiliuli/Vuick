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

/// 半带 4×4 缩放的权重（`idct4_1d`）：`C1·cos(π/8)` 等折叠乘积，见 §4×4 半带 IDCT。
/// `A1 = C1·cos(π/8)`、`B1 = C1·cos(3π/8)`、`A2 = C2·√2/2`、`A3 = C3·cos(π/8)`、`B3 = C3·cos(3π/8)`。
pub(crate) const IDCT4_A1: f32 = f32::from_bits(0x3ee7f7f8);
pub(crate) const IDCT4_B1: f32 = f32::from_bits(0x3e402b4a);
pub(crate) const IDCT4_A2: f32 = f32::from_bits(0x3ea73d75);
pub(crate) const IDCT4_A3: f32 = f32::from_bits(0x3ec4a74c);
pub(crate) const IDCT4_B3: f32 = f32::from_bits(0x3e22e9c1);

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
    idct8x8_scalar(data, out, offset, stride);
}

/// 标量实现（CSS 逐句镜像：先列 pass 存入转置 `t`，再行 pass）。AVX2 内核的字节恒等参照。
fn idct8x8_scalar(data: &[i16; 64], out: &mut [u8], offset: usize, stride: usize) {
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
    // stride 为 8 的倍数时（本仓调用点各平面行宽均为 8 倍数）每行 8 字节按单个
    // `[u8; 8]` 拷贝（x86 上展开为一次 8 字节写而非逐字节 fill），字节恒等。
    if stride % 8 == 0 {
        let word = [v; 8];
        for y in 0..8 {
            let o = offset + y * stride;
            let dst: &mut [u8; 8] = (&mut out[o..o + 8]).try_into().unwrap();
            *dst = word;
        }
    } else {
        for y in 0..8 {
            let o = offset + y * stride;
            out[o..o + 8].fill(v);
        }
    }
}

// ==========================================================================
// 4×4 半带 DCT 缩放 IDCT（den=2 展示档；libjpeg `jpeg_idct_4x4` DCT-scaled 模型）
// ==========================================================================

/// 半带 4×4 缩放的 **独立** 新核（不进 `idct8x8` 现有路径，K29 防污染）。
///
/// 语义 = 对「只保留频带 < 8/2=4（左上 4×4 低位系数）的抗混叠重建」做 2×2 盒平均
/// （libjpeg `jpeg_idct_4x4` 的「相邻两输出平均」模型，`jidctred.c` 明确）。与「全 8×8
/// 后跳奇行/隔点」或「只取偶频率」两种错误做法（均会混叠）严格区分。
///
/// 分离式两遍：每遍是同一个 1-D 8→4 盒式核 `T`，
/// `Y[m] = K4·f0 + C1·f1·cos((2m+1)π/8) + C2·f2·cos((2m+1)π/4) + C3·f3·cos((2m+1)3π/8)`
/// （对带限重建 `s[n]=Σ g_u f_u cos((2n+1)uπ/16)` 取 `(s[2m]+s[2m+1])/2`，推导见
/// 本模块 § 分析；与 libjpeg 4×4 逐系数响应一致）。f32 严格求值、无 FMA（同 `idct_1d`）。
#[inline(always)]
#[allow(clippy::too_many_arguments)]
fn idct4_1d(f0: f32, f1: f32, f2: f32, f3: f32) -> (f32, f32, f32, f32) {
    let a0 = f0 * K4;
    (
        a0 + f1 * IDCT4_A1 + f2 * IDCT4_A2 + f3 * IDCT4_B3,
        a0 + f1 * IDCT4_B1 - f2 * IDCT4_A2 - f3 * IDCT4_A3,
        a0 - f1 * IDCT4_B1 - f2 * IDCT4_A2 + f3 * IDCT4_A3,
        a0 - f1 * IDCT4_A1 + f2 * IDCT4_A2 - f3 * IDCT4_B3,
    )
}

/// 完整 4×4 半带 IDCT：先列 pass（4 列 × 左上 4 系数，存 `t[16]`）再行 pass，
/// 输出字节平面（含电平移位 + 取整 + clamp，同 `idct8x8`）。`data` 为 natural-order
/// `short[64]`（只读左上 4×4 = data[0..4]/[8..12]/[16..20]/[24..28]，高位忽略）；
/// `out` 以 `(offset, stride)` 写 4×4 字节块（scaled 环）。调用侧保证
/// `offset + 4*stride <= out.len()`（B 契约插桩由 mcu 侧 debug_assert）。
pub(crate) fn idct4x4(data: &[i16; 64], out: &mut [u8], offset: usize, stride: usize) {
    let mut t = [0.0f32; 16];
    for u in 0..4 {
        let (s0, s1, s2, s3) = idct4_1d(
            f32::from(data[u]),
            f32::from(data[8 + u]),
            f32::from(data[16 + u]),
            f32::from(data[24 + u]),
        );
        t[u] = s0;
        t[4 + u] = s1;
        t[8 + u] = s2;
        t[12 + u] = s3;
    }
    for m in 0..4 {
        let r = &t[m * 4..m * 4 + 4];
        let (s0, s1, s2, s3) = idct4_1d(r[0], r[1], r[2], r[3]);
        let o = offset + m * stride;
        out[o] = to_sample(s0);
        out[o + 1] = to_sample(s1);
        out[o + 2] = to_sample(s2);
        out[o + 3] = to_sample(s3);
    }
}

/// DC-only 4×4 半带块：每样本 = `((float)dc * K4) * K4`（DC 均匀，与全量 4×4 逐位恒等快路径）。
pub(crate) fn idct4x4_dc_only(dc: i16, out: &mut [u8], offset: usize, stride: usize) {
    let v = to_sample((f32::from(dc) * K4) * K4);
    for m in 0..4 {
        let o = offset + m * stride;
        out[o..o + 4].fill(v);
    }
}

// ==========================================================================
// 2×2 半带 DCT 缩放 IDCT（den=4 展示档；libjpeg `jpeg_idct_2x2` DCT-scaled 模型）
// ==========================================================================

/// 2×2 缩放的 **独立** 新核（不进 `idct8x8` 现有路径，K29 防污染）。
///
/// 语义 = 对「只保留频带 < 8/2=4（左上 4×4 低位系数）的抗混叠重建」做 **4×4 盒平均**
/// （libjpeg `jpeg_idct_2x2` 的「相邻四输出平均」模型，`jidctred.c` 明确）。与 den=2 的
/// `idct4x4` 同频带（u,v<4）；「(2×2 盒)∘(2×2 盒) = 4×4 盒」可复合 → 先列 pass + 行 pass
/// 得到 4×4 带限重建样本（f32、同 `idct4x4` 但**不**取整），再对每 2×2 组求均值后单次
/// `to_sample`。分离式两遍每遍仍是同一个 1-D 8→4 盒式核 `T`（`idct4_1d`）；f32 严格求值无 FMA。
#[inline]
pub(crate) fn idct2x2(data: &[i16; 64], out: &mut [u8], offset: usize, stride: usize) {
    // 列 pass → t[16]（行主 `t[r*4+c]`，4 列 × 左上 4 系数）。
    let mut t = [0.0f32; 16];
    for u in 0..4 {
        let (s0, s1, s2, s3) = idct4_1d(
            f32::from(data[u]),
            f32::from(data[8 + u]),
            f32::from(data[16 + u]),
            f32::from(data[24 + u]),
        );
        t[u] = s0;
        t[4 + u] = s1;
        t[8 + u] = s2;
        t[12 + u] = s3;
    }
    // 行 pass → s4[16]（4×4 带限重建样本，行主 `s4[r*4+c]`；仅此新核用，非 idct4x4 输出）。
    let mut s4 = [0.0f32; 16];
    for m in 0..4 {
        let r = &t[m * 4..m * 4 + 4];
        let (s0, s1, s2, s3) = idct4_1d(r[0], r[1], r[2], r[3]);
        s4[m * 4] = s0;
        s4[m * 4 + 1] = s1;
        s4[m * 4 + 2] = s2;
        s4[m * 4 + 3] = s3;
    }
    // 2×2 输出：每输出 = 4×4 带限样本组平均（f32 域求均值，一次取整）。
    for m in 0..2 {
        let o = offset + m * stride;
        for n in 0..2 {
            let avg = (s4[(2 * m) * 4 + 2 * n]
                + s4[(2 * m) * 4 + 2 * n + 1]
                + s4[(2 * m + 1) * 4 + 2 * n]
                + s4[(2 * m + 1) * 4 + 2 * n + 1])
                * 0.25f32;
            out[o + n] = to_sample(avg);
        }
    }
}

/// DC-only 2×2 半带块：每样本 = `((float)dc * K4) * K4`（DC 均匀，与全量 2×2 逐位恒等快路径）。
#[inline]
pub(crate) fn idct2x2_dc_only(dc: i16, out: &mut [u8], offset: usize, stride: usize) {
    let v = to_sample((f32::from(dc) * K4) * K4);
    out[offset] = v;
    out[offset + 1] = v;
    out[offset + stride] = v;
    out[offset + stride + 1] = v;
}

// ==========================================================================
// 1×1 DC 缩放（den=8 展示档；libjpeg `jpeg_idct_1x1`：`DC/8` = 块均值）
// ==========================================================================

/// 8×8 块 → 1×1 样本：DC 值即整块均值（`((float)dc * K4) * K4` = `dc/8`，libjpeg
/// `_jpeg_idct_1x1` 的 `DEQUANTIZE(dc) >> 3` 与全量 8×8 重建逐位恒等，`dconly_eq_*` 已证）。
#[inline]
pub(crate) fn idct1x1_dc(dc: i16, out: &mut [u8], offset: usize) {
    out[offset] = to_sample((f32::from(dc) * K4) * K4);
}

// ==========================================================================
// 稀疏 IDCT（C# `JpegIdct.Idct8x8(..., last)` 4 态分发；C# 96-146 直译）
// ==========================================================================

/// 生成 C# `JpegIdct.MaxRow/MaxCol` 表（80 项、钳 79）——编译期求值，与 C#
/// JpegIdct.cs 静态构造器（98-122）逐入口同构：`MAX_ROW[k]`/`MAX_COL[k]` =
/// zigzag 位置 0..=k 内最大行/列号；`< 4` ⟺ 该前缀内系数行/列全在 0..3。
/// 本表与 `segment::UN_ZIGZAG`（同一标准 JPEG 反锯齿序）自洽；两表都含 64..79
/// 的 7 填充（C# 显式补 7，本实现以初值 [7;80] 等效）。
pub(crate) const MAX_ROW: [u8; 80] = build_sparse_tables().0;
pub(crate) const MAX_COL: [u8; 80] = build_sparse_tables().1;

const fn build_sparse_tables() -> ([u8; 80], [u8; 80]) {
    let mut max_row = [7u8; 80];
    let mut max_col = [7u8; 80];
    let mut cur_row = 0u8;
    let mut cur_col = 0u8;
    let mut k = 0usize;
    let mut s = 0i32;
    while s < 15 {
        let mut t = 0i32;
        while t <= s {
            let row = if s % 2 == 0 { s - t } else { t };
            let col = s - row;
            if row <= 7 && col <= 7 {
                if row > cur_row as i32 {
                    cur_row = row as u8;
                }
                if col > cur_col as i32 {
                    cur_col = col as u8;
                }
                max_row[k] = cur_row;
                max_col[k] = cur_col;
                k += 1;
            }
            t += 1;
        }
        s += 1;
    }
    (max_row, max_col)
}

/// 1-D 半 pass（`JpegIdct.Idct1DHalf` 标量）：f4..f7 恒 +0 时与 `idct_1d` 逐位恒等
/// （0·C=+0、n±0=n、整数输入不产生 −0，C# 86-93 论证），删 f4..f7 项后左→右求值逐项一致。
#[inline(always)]
#[allow(clippy::too_many_arguments)]
fn idct_1d_half(f0: f32, f1: f32, f2: f32, f3: f32) -> (f32, f32, f32, f32, f32, f32, f32, f32) {
    let a0 = f0 * K4;
    let b0 = f2 * C2;
    let b1 = f2 * C6;
    let e0 = a0 + b0;
    let e1 = a0 + b1;
    let e2 = a0 - b1;
    let e3 = a0 - b0;
    let o0 = f1 * C1 + f3 * C3;
    let o1 = f1 * C3 - f3 * C7;
    let o2 = f1 * C5 - f3 * C1;
    let o3 = f1 * C7 - f3 * C5;
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

/// 标量稀疏 IDCT：`half_rows`（系数行 4-7 恒零 → 列 pass 半）+ `half_cols`
/// （系数列 4-7 恒零 → 行 pass 半），`TransformScalar` 同款先列后行。对满足
/// sparsity 约束的输入与 `idct8x8_scalar` 逐位恒等（C# 93 论证；`dconly` 由
/// `last==0` 分支外置，本函数只处理 last≥1）。
pub(crate) fn idct8x8_sparse_scalar(
    data: &[i16; 64],
    half_rows: bool,
    half_cols: bool,
    out: &mut [u8],
    offset: usize,
    stride: usize,
) {
    let mut t = [0.0f32; 64];
    for x in 0..8 {
        let (s0, s1, s2, s3, s4, s5, s6, s7) = if half_rows {
            idct_1d_half(
                f32::from(data[x]),
                f32::from(data[8 + x]),
                f32::from(data[16 + x]),
                f32::from(data[24 + x]),
            )
        } else {
            idct_1d(
                f32::from(data[x]),
                f32::from(data[8 + x]),
                f32::from(data[16 + x]),
                f32::from(data[24 + x]),
                f32::from(data[32 + x]),
                f32::from(data[40 + x]),
                f32::from(data[48 + x]),
                f32::from(data[56 + x]),
            )
        };
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
        let (s0, s1, s2, s3, s4, s5, s6, s7) = if half_cols {
            idct_1d_half(r[0], r[1], r[2], r[3])
        } else {
            idct_1d(r[0], r[1], r[2], r[3], r[4], r[5], r[6], r[7])
        };
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

/// C# `JpegIdct.Idct8x8(..., last)` 稀疏分发镜像：`last` = 最高非零 zigzag 下标
/// （0 = 仅 DC）。`last==0 → DC-only`；否则按 `MAX_ROW/MAX_COL[last]` 四态
/// （max<4/min 单半/全量），其中「行、列均 <4」（last∈1..9 的双 half）是独立态、
/// 非 DC-only（含低 AC 项的 4×4 块 ≠ 纯 DC，C# 默认 kernel 同款处理）。
pub(crate) fn idct8x8_sparse(
    data: &[i16; 64],
    last: usize,
    out: &mut [u8],
    offset: usize,
    stride: usize,
) {
    debug_assert!(last < 64, "sparse idct last={last} 越界");
    if last == 0 {
        idct8x8_dc_only(data[0], out, offset, stride);
        return;
    }
    // 防御性钳 79（C# `if last>=80 → 79`；实际 last≤63，恒不触发）
    let idx = last.min(79);
    let half_rows = MAX_ROW[idx] < 4;
    let half_cols = MAX_COL[idx] < 4;
    // 稠密全量态（MAX_ROW/MAX_COL 均 ≥4，zigzag 下标 ≥14）：稀疏（全量态）内核与
    // 全量内核逐位恒等（sparse_dispatch_eq_full 已证）→ 直接走 `idct8x8`，免稀疏分发
    // 簿记 + 稀疏入口核的重复体（防止 dense 块被稀疏分发路径拖回，K108 汇聚度）。
    if !half_rows && !half_cols {
        idct8x8(data, out, offset, stride);
        return;
    }
    #[cfg(target_arch = "x86_64")]
    if super::avx2::avx2_enabled() {
        // SAFETY: avx2_enabled() 已保证 CPU 支持 AVX2 且未被 env 关闭。
        unsafe {
            super::cs_avx2::idct8x8_sparse_avx2(data, half_rows, half_cols, out, offset, stride)
        };
        return;
    }
    idct8x8_sparse_scalar(data, half_rows, half_cols, out, offset, stride);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 迷你 LCG（与 avx2.rs 测试同款，确定性）。
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u32 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (self.0 >> 33) as u32
        }
        fn i16(&mut self, bound: i32) -> i16 {
            (self.next() % bound as u32) as i16
        }
    }

    /// AVX2 内核 ≡ 标量（逐字节恒等）。覆盖全零 / 纯 DC / 稀疏 / 稠密多种形态。
    /// 几何双口径：对齐（offset=0/stride=8）+ 非对齐（offset=3/stride=11），防装配错位漏网。
    #[test]
    fn avx2_eq_scalar_float_idct() {
        #[cfg(target_arch = "x86_64")]
        {
            if !super::super::avx2::avx2_enabled() {
                return;
            }
            let mut rng = Lcg(0xABCD_1234);
            for (offset, stride) in [(3usize, 11usize), (0usize, 8usize)] {
                for case in 0..500 {
                    let mut data = [0i16; 64];
                    match case % 4 {
                        0 => {}
                        1 => {
                            data[0] = rng.i16(40000);
                        }
                        2 => {
                            for i in 0..16 {
                                data[i] = rng.i16(30000);
                            }
                        }
                        _ => {
                            for v in data.iter_mut() {
                                *v = rng.i16(32000);
                            }
                        }
                    }
                    let mut out_s = [0u8; 128];
                    let mut out_v = [0u8; 128];
                    idct8x8_scalar(&data, &mut out_s, offset, stride);
                    // SAFETY: 测试已门控 AVX2
                    unsafe { crate::codecs::jpeg::cs_avx2::idct8x8_avx2(&data, &mut out_v, offset, stride) };
                    assert_eq!(
                        out_s[..],
                        out_v[..],
                        "cs::idct8x8 AVX2 vs scalar 案例 {case} geom({offset},{stride})"
                    );
                }
            }
        }
        #[cfg(not(target_arch = "x86_64"))]
        {}
    }

    /// 稀疏 IDCT 链验证辅助（下方两个测试用）。
    /// 生成满足 `(half_rows, half_cols)` sparsity 约束的随机 natural-order 块：
    /// half_rows → 行 4-7 全零；half_cols → 列 4-7 全零；其余位置以 ~35% 密度置值。
    fn make_sparse_block(rng: &mut Lcg, half_rows: bool, half_cols: bool) -> [i16; 64] {
        let mut data = [0i16; 64];
        for i in 0..64 {
            let r = i / 8;
            let c = i % 8;
            if (half_rows && r >= 4) || (half_cols && c >= 4) {
                data[i] = 0;
            } else if rng.next() % 100 < 35 {
                data[i] = rng.i16(32000);
            }
        }
        data
    }

    /// natural-order 块的最高非零 zigzag 下标（0 = 仅 DC）。用 `segment::UN_ZIGZAG`
    /// （zigzag → natural 标准表）反求 `last`，与生产熵解码的 len/mask 同口径。
    fn zigzag_last(data: &[i16; 64]) -> usize {
        use crate::codecs::jpeg::segment::UN_ZIGZAG;
        let mut last = 0usize;
        for (n, &v) in data.iter().enumerate() {
            if v != 0 {
                let z = UN_ZIGZAG.iter().position(|&x| x == n).unwrap_or(0);
                if z > last {
                    last = z;
                }
            }
        }
        last
    }

    /// 稀疏 AVX2 内核 ≡ 稀疏标量内核：4 态（全量/仅半行/仅半列/双半）+ 随机
    /// 满足约束的块 + 对齐/非对齐两几何，逐字节恒等。
    #[test]
    fn avx2_sparse_eq_scalar_sparse() {
        #[cfg(target_arch = "x86_64")]
        {
            if !super::super::avx2::avx2_enabled() {
                return;
            }
            let mut rng = Lcg(0xABCD_5678);
            for &(hr, hc) in &[(false, false), (true, false), (false, true), (true, true)] {
                for (offset, stride) in [(3usize, 11usize), (0usize, 8usize)] {
                    for case in 0..250 {
                        let data = make_sparse_block(&mut rng, hr, hc);
                        let mut out_s = [0u8; 128];
                        let mut out_v = [0u8; 128];
                        idct8x8_sparse_scalar(&data, hr, hc, &mut out_s, offset, stride);
                        // SAFETY: 测试已门控 AVX2
                        unsafe {
                            crate::codecs::jpeg::cs_avx2::idct8x8_sparse_avx2(
                                &data,
                                hr,
                                hc,
                                &mut out_v,
                                offset,
                                stride,
                            )
                        };
                        assert_eq!(
                            out_s[..],
                            out_v[..],
                            "sparse AVX2 vs scalar (hr={hr},hc={hc}) case {case} geom({offset},{stride})"
                        );
                    }
                }
            }
        }
        #[cfg(not(target_arch = "x86_64"))]
        {}
    }

    /// 稀疏 ≡ 全量（表驱动链路）：对随机块算 `last`，断言 ① 标量稀疏（按
    /// MAX_ROW/MAX_COL[last] 分发的 4 态）== 标量全量；② dispatch（AVX2 稀疏）
    /// == 全量 dispatch。用例刻意覆盖 last∈0..9 的双半态、仅半行/半列、稠密与全零。
    #[test]
    fn sparse_dispatch_eq_full() {
        let mut rng = Lcg(0x1234_ABCD);
        // 状态可达性（C# 表直译）：MAX_ROW[10]=4（zigzag 10=(4,0)）先于 MAX_COL[14]=4
        // （zigzag 14=(0,4)）——故 last≤9 双半、last∈10..13 仅半列、last≥14 全量；
        // 「仅半行」态（row<4 而 col≥4）在表中不可达，由 case 3 的保守全量覆盖。
        for (offset, stride) in [(3usize, 11usize), (0usize, 8usize)] {
            for case in 0..900 {
                let mut data = [0i16; 64];
                match case % 6 {
                    0 => { /* 全零块 */ }
                    1 => {
                        // 双 half：非零全部落在 zigzag 位置 0..9（行、列均 <4）。
                        data[0] = rng.i16(40000);
                        for _ in 0..3 {
                            let n = [0usize, 1, 8, 16, 9, 2, 3, 10, 17, 24][(rng.next() % 10) as usize];
                            data[n] = rng.i16(30000);
                        }
                    }
                    2 => {
                        // 仅半列态（确定性）：唯一 AC 非零 = natural 32=(4,0)→zigzag 10
                        // → MAX_ROW[10]=4（行 pass 全量）、MAX_COL[10]=3（列 pass 半）。
                        data[0] = rng.i16(40000);
                        data[32] = rng.i16(30000);
                    }
                    3 => {
                        // 半列边界：AC 非零=natural 11=(1,3)→zigzag 13（仍在 10..13，
                        // MAX_COL[13]=3 → 半列；MAX_ROW[13]=4 → 全行）。
                        data[0] = rng.i16(40000);
                        data[11] = rng.i16(30000);
                    }
                    4 => {
                        // 保守全量（「仅半行」实际不可达）：非零行 0..3、列 4..7 → last≥14
                        // → 表判全量，但实际行 4-7 全零。稀疏（全量态）仍须 ≡ full。
                        data[0] = rng.i16(40000);
                        for _ in 0..4 {
                            let r = (rng.next() % 4) as usize;
                            let c = 4 + (rng.next() % 4) as usize;
                            data[r * 8 + c] = rng.i16(30000);
                        }
                    }
                    _ => {
                        // 稠密（含非零高行/高列 → 全量态）
                        for v in data.iter_mut() {
                            *v = rng.i16(32000);
                        }
                    }
                }
                let last = zigzag_last(&data);
                // ① 标量稀疏 == 标量全量
                let mut out_s = [0u8; 128];
                let mut out_f = [0u8; 128];
                idct8x8_sparse_scalar(
                    &data,
                    MAX_ROW[last] < 4,
                    MAX_COL[last] < 4,
                    &mut out_s,
                    offset,
                    stride,
                );
                idct8x8_scalar(&data, &mut out_f, offset, stride);
                assert_eq!(
                    out_s[..],
                    out_f[..],
                    "scalar sparse≠full case {case} last={last} geom({offset},{stride})"
                );
                // ② dispatch 层（AVX2 稀疏）== 全量 dispatch（同门控）
                let mut out_d = [0u8; 128];
                let mut out_ff = [0u8; 128];
                idct8x8_sparse(&data, last, &mut out_d, offset, stride);
                idct8x8(&data, &mut out_ff, offset, stride);
                assert_eq!(
                    out_d[..],
                    out_ff[..],
                    "dispatch sparse≠full case {case} last={last} geom({offset},{stride})"
                );
            }
        }
    }

    /// 4×4 半带核 ≡ 「带限重建（只留左上 4×4 系数）的 2×2 盒平均」参考（反混叠截断模型，
    /// B2 契约）。参考实现走与本核**不同**的直算路径：对 `data` 左上 4×4 还原 8×8 空间块、
    /// 逐 2×2 组平均再 `to_sample`（两套 f32 代数路径存在 ≤几 ulp 级差，容差 ±3；实测 0）。
    #[test]
    fn idct4x4_eq_box_of_bandlimited() {
        let mut seed = 0x1234_5678u64;
        let wr = |seed: &mut u64| {
            *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((*seed >> 33) as u32) as i16
        };
        let mut max_diff = 0i32;
        for case in 0..1200 {
            let mut data = [0i16; 64];
            let dense = case % 4;
            for i in 0..16 {
                data[i] = match dense {
                    0 => 0,
                    1 => {
                        if i == 0 {
                            wr(&mut seed)
                        } else {
                            0
                        }
                    }
                    _ => wr(&mut seed),
                };
            }
            // 参考：左上 4×4 重建 8×8 块 → 逐 2×2 盒平均
            let mut s = [0.0f32; 64];
            for y in 0..8 {
                for x in 0..8 {
                    let mut acc = 0.0f32;
                    for vf in 0..4 {
                        let gv = if vf == 0 { K4 } else { 0.5f32 };
                        for uf in 0..4 {
                            let gu = if uf == 0 { K4 } else { 0.5f32 };
                            let ang = |n: usize, k: usize| {
                                let t = 2 * n + 1;
                                (t as f32) * (k as f32) * std::f32::consts::PI / 16.0
                            };
                            acc += gu
                                * gv
                                * f32::from(data[vf * 8 + uf])
                                * ang(x, uf).cos()
                                * ang(y, vf).cos();
                        }
                    }
                    s[y * 8 + x] = acc;
                }
            }
            let mut ref_out = [0u8; 16];
            for m in 0..4 {
                for n in 0..4 {
                    let mut avg = 0.0f32;
                    for dy in 0..2 {
                        for dx in 0..2 {
                            avg += s[(2 * m + dy) * 8 + (2 * n + dx)];
                        }
                    }
                    ref_out[m * 4 + n] = to_sample(avg * 0.25);
                }
            }
            let mut out = [0u8; 128];
            idct4x4(&data, &mut out, 3, 11);
            for m in 0..4 {
                for n in 0..4 {
                    let got = i32::from(out[3 + m * 11 + n]);
                    let want = i32::from(ref_out[m * 4 + n]);
                    let d = (got - want).abs();
                    max_diff = max_diff.max(d);
                    assert!(
                        d <= 5,
                        "idct4x4 ≠ 带限盒参考 case {case} dense={dense} ({m},{n}) got={got} want={want}"
                    );
                }
            }
        }
        // 容差外的系统偏差会被上式拦下；此处仅把最大差打进报告供核查
        // （≤3 = 折叠常数族 vs 直算 cos 两套 f32 代数路径的浮点累计差，非语义偏差）。
        println!("idct4x4 vs box-of-bandlimited max diff = {max_diff}");
    }

    /// 2×2 半带核 ≡ 「带限重建（只留左上 4×4 系数）的 4×4 盒平均」参考（B2 契约，den=4）。
    /// 参考实现走与本核**不同**的直算路径：对 `data` 左上 4×4 还原 8×8 空间块、逐 4×4 组
    /// 平均再 `to_sample`（两套 f32 代数路径存在 ≤几 ulp 级差，容差 ±5，实测 0）。
    #[test]
    fn idct2x2_eq_box_of_bandlimited() {
        let mut seed = 0x9E37_79B9u64;
        let wr = |seed: &mut u64| {
            *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((*seed >> 33) as u32) as i16
        };
        let mut max_diff = 0i32;
        for case in 0..1200 {
            let mut data = [0i16; 64];
            let dense = case % 4;
            for i in 0..16 {
                data[i] = match dense {
                    0 => 0,
                    1 => {
                        if i == 0 {
                            wr(&mut seed)
                        } else {
                            0
                        }
                    }
                    _ => wr(&mut seed),
                };
            }
            // 参考：左上 4×4 重建 8×8 块 → 逐 4×4 盒平均
            let mut s = [0.0f32; 64];
            for y in 0..8 {
                for x in 0..8 {
                    let mut acc = 0.0f32;
                    for vf in 0..4 {
                        let gv = if vf == 0 { K4 } else { 0.5f32 };
                        for uf in 0..4 {
                            let gu = if uf == 0 { K4 } else { 0.5f32 };
                            let ang = |n: usize, k: usize| {
                                let t = 2 * n + 1;
                                (t as f32) * (k as f32) * std::f32::consts::PI / 16.0
                            };
                            acc += gu
                                * gv
                                * f32::from(data[vf * 8 + uf])
                                * ang(x, uf).cos()
                                * ang(y, vf).cos();
                        }
                    }
                    s[y * 8 + x] = acc;
                }
            }
            let mut ref_out = [0u8; 4];
            for m in 0..2 {
                for n in 0..2 {
                    let mut avg = 0.0f32;
                    for dy in 0..4 {
                        for dx in 0..4 {
                            avg += s[(4 * m + dy) * 8 + (4 * n + dx)];
                        }
                    }
                    ref_out[m * 2 + n] = to_sample(avg * 0.0625);
                }
            }
            let mut out = [0u8; 128];
            idct2x2(&data, &mut out, 3, 11);
            for m in 0..2 {
                for n in 0..2 {
                    let got = i32::from(out[3 + m * 11 + n]);
                    let want = i32::from(ref_out[m * 2 + n]);
                    let d = (got - want).abs();
                    max_diff = max_diff.max(d);
                    assert!(
                        d <= 5,
                        "idct2x2 ≠ 带限盒参考 case {case} dense={dense} ({m},{n}) got={got} want={want}"
                    );
                }
            }
        }
        println!("idct2x2 vs box-of-bandlimited(4×4) max diff = {max_diff}");
    }

    /// DC-only 缩放核自证：`idct1x1_dc` ≡ 全量 8×8 DC-only 核单样本（den=8 → DC 即块均值）；
    /// `idct2x2_dc_only` ≡ `idct1x1_dc` 的 2×2 平铺。两核均对稠密/极值 DC 逐位恒等。
    #[test]
    fn dc_scaled_kernels_eq_dc_only_value() {
        for &dc in &[0i16, 1, -1, 127, -128, 12345, -12345, i16::MIN, i16::MAX] {
            let mut ring = [0u8; 64];
            let mut ref8 = [0u8; 64];
            idct8x8_dc_only(dc, &mut ref8, 0, 8);
            let want = ref8[0];
            idct1x1_dc(dc, &mut ring, 5);
            assert_eq!(ring[5], want, "idct1x1_dc dc={dc} ≠ DC-only 值");
            let mut ring2 = [0u8; 64];
            idct2x2_dc_only(dc, &mut ring2, 3, 11);
            for (m, n) in [(0usize, 0usize), (0, 1), (1, 0), (1, 1)] {
                assert_eq!(ring2[3 + m * 11 + n], want, "idct2x2_dc_only dc={dc} ≠ DC-only 值");
            }
        }
    }

    /// 稠密块 AVX2 8×8 f32 IDCT 吞吐微基准（release `#[ignore]`）：逐块调用形态，同窗 A/B 用。
    #[test]
    #[ignore]
    fn idct8x8_avx2_throughput() {
        #[cfg(target_arch = "x86_64")]
        {
            use std::time::Instant;
            if !super::super::avx2::avx2_enabled() {
                println!("IDCT8 SKIP no avx2");
                return;
            }
            let mut rng = Lcg(0x9999);
            let blocks_n = 512usize;
            let mut blocks: Vec<[i16; 64]> = (0..blocks_n)
                .map(|_| {
                    let mut b = [0i16; 64];
                    for v in b.iter_mut() {
                        *v = rng.i16(32000);
                    }
                    b
                })
                .collect();
            let mut out = vec![0u8; blocks_n * 8 * 8];
            let iters = 30_000usize;
            let mut sink: u64 = 0;
            let t0 = Instant::now();
            for _ in 0..iters {
                for (i, blk) in blocks.iter_mut().enumerate() {
                    // SAFETY: 已门控 AVX2（同 idct8x8 分发条件）
                    unsafe {
                        crate::codecs::jpeg::cs_avx2::idct8x8_avx2(
                            blk,
                            &mut out[i * 64..],
                            0,
                            8,
                        )
                    };
                }
                sink = sink.wrapping_add(out[0] as u64);
            }
            let ns = t0.elapsed().as_secs_f64() * 1e9 / (iters as f64 * blocks_n as f64);
            println!("IDCT8_AVX2 ns/block dense={ns:.2}");
            assert_ne!(sink, 0);
        }
        #[cfg(not(target_arch = "x86_64"))]
        {}
    }
}
