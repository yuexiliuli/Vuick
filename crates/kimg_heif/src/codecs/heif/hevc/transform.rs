//! 缩放（§8.6.3）与一个变换块的反变换（§8.6.2、§8.6.4），以及残差加回预测（§8.6.7）。
//! 逐函数移植自 C# `Hevc\HevcTransform.cs`，与 C# 字节恒等（表逐元素一致、累加/位移按 C# 语义 wrapping）。
//!
//! 反变换是 (8-314) 与 (8-316) 的两个矩阵乘积：先垂直一遍 `e[x][y] = Σj transMatrix[j][y]·d[x][j]`，
//! 在 (e + 64) >> 7 后截到 16 位，再水平一遍做 (8-299) 的最终 (r + 2^(bdShift−1)) >> bdShift。
//! 每个和都在 32 位整数内精确（|d| ≤ 2^15、|transMatrix| ≤ 90、至多 32 项），加的顺序不影响结果。
//!
//! 两个精确捷径让工作量正比于已编码系数：乘积只在块的非零最后列/行（`maxX`/`maxY`）内跑，
//! 垂直遍利用 DCT 基的对称性 `transMatrix[j][N−1−y] = (−1)^j·transMatrix[j][y]`：
//! 偶/奇 j 的部分和 E/O 给出输出 y 为 E + O、输出 N−1−y 为 E − O。

// 叶子隔离：unsafe SIMD 内核只许在 `transform_simd`（`#![allow(unsafe_code)]`）内，本模块禁止。
#![forbid(unsafe_code)]

use std::sync::LazyLock;

use crate::codecs::heif::error::{HeifError, HeifResult};

const LEVEL_SCALE: [i32; 6] = [40, 45, 51, 57, 64, 72];

/// transMatrix（8-319 .. 8-321）以 `Dct[k * 32 + n]` 存放：基函数 k（频率）在采样 n。
/// 由矩阵 33 个不同幅值（64·√2·cos(mπ/64) 的整数近似）生成。
const fn build_dct() -> [i8; 1024] {
    const MAGNITUDE: [u8; 33] = [
        64, 90, 90, 90, 89, 88, 87, 85, 83, 82, 80, 78, 75, 73, 70, 67,
        64, 61, 57, 54, 50, 46, 43, 38, 36, 31, 25, 22, 18, 13, 9, 4, 0,
    ];
    let mut result = [0i8; 1024];
    let mut k = 0usize;
    while k < 32 {
        let mut n = 0usize;
        while n < 32 {
            // cos((2n + 1) k π / 64)：把角度 (2n + 1)k 由对称性折到 [0, 64]，再带符号折到 [0, 32]。
            let mut a = ((2 * n + 1) * k) % 128;
            if a > 64 {
                a = 128 - a;
            }
            let v = if a > 32 {
                -(MAGNITUDE[64 - a] as i8)
            } else {
                MAGNITUDE[a] as i8
            };
            result[k * 32 + n] = v;
            n += 1;
        }
        k += 1;
    }
    result
}

/// 32 点 DCT 基阵。
static DCT_BUILT: [i8; 1024] = build_dct();

/// N 点变换的 transMatrix：取 32 点阵的第 0、32/N、2·32/N、… 行（8-319 .. 8-321）。
fn build_dct_n(log2_size: usize) -> Vec<i32> {
    let n = 1usize << log2_size;
    let step = 32 >> log2_size;
    let mut result = vec![0i32; n * n];
    for k in 0..n {
        for x in 0..n {
            result[k * n + x] = i32::from(DCT_BUILT[k * step * 32 + x]);
        }
    }
    result
}

/// `[log2Size - 2]` → N 点 DCT 矩阵（int 版）。
static DCT_N: LazyLock<[Vec<i32>; 4]> = LazyLock::new(|| {
    [build_dct_n(2), build_dct_n(3), build_dct_n(4), build_dct_n(5)]
});

/// 4×4 DST（8-317）：`Dst[k * 4 + n]`。
const DST: [i8; 16] = [
    29, 55, 74, 84,
    74, 74, 0, -74,
    84, -29, -74, 55,
    55, -84, 74, -29,
];

/// `Dst` 的 int 版。
const DST_N: [i32; 16] = [
    29, 55, 74, 84, 74, 74, 0, -74, 84, -29, -74, 55, 55, -84, 74, -29,
];

/// (8-309) 的缩放器：TransCoeffLevel → d。
pub struct Scaler {
    scale: i64,
    round: i64,
    bd_shift: i32,
}

impl Scaler {
    pub fn new(log2_size: i32, qp: i32, bit_depth: i32) -> HeifResult<Scaler> {
        if !(2..=5).contains(&log2_size) || !(8..=16).contains(&bit_depth) {
            return Err(HeifError::invalid("变换尺寸或位深越界。"));
        }
        // C# 无上界检查（10-bit/12-bit 高 QP 时 QP′=QpY+6·(bd−8) 可达 63，合法流）；
        // 仅防负 QP（C# 对负 qp 会数组越界），与 C# LevelScale[qp%6] 语义一致。
        if qp < 0 {
            return Err(HeifError::invalid("QP 为负。"));
        }
        let bd_shift = bit_depth + log2_size - 5;
        let scale = (LEVEL_SCALE[(qp % 6) as usize] as i64) << (qp / 6);
        let round = 1i64 << (bd_shift - 1);
        Ok(Scaler {
            scale,
            round,
            bd_shift,
        })
    }

    /// d = Clip3(−32768, 32767, (level · m · levelScale[qP % 6] << (qP / 6) + round) >> bdShift)。
    pub fn scale(&self, level: i32, m: i32) -> i32 {
        let lm = i64::from(level.wrapping_mul(m));
        let v = lm.wrapping_mul(self.scale).wrapping_add(self.round) >> self.bd_shift;
        v.clamp(i64::from(i16::MIN), i64::from(i16::MAX)) as i32
    }
}

/// 变换/反量化（C# `HevcTransform` 静态类）。
pub struct HevcTransform;

impl HevcTransform {
    /// 32 点 DCT 基阵。
    pub fn dct() -> &'static [i8; 1024] {
        &DCT_BUILT
    }

    /// 4×4 DST。
    pub fn dst() -> &'static [i8; 16] {
        &DST
    }

    /// N 点 DCT 矩阵（log2Size 2..=5）。
    pub fn dct_n(log2_size: usize) -> HeifResult<&'static Vec<i32>> {
        if !(2..=5).contains(&log2_size) {
            return Err(HeifError::invalid("DCT 尺寸 log2Size 越界（2..5）。"));
        }
        Ok(&DCT_N[log2_size - 2])
    }

    /// 反变换分派入口：AVX2（n≥8）8-lane → SSE4.1 4-lane（含 DST 4 点）→ 标量兜底。
    /// 三层与 C# 逐字节一致；SIMD 内核在 `transform_simd`（叶子隔离），标量路径原样保留。
    #[allow(clippy::too_many_arguments)]
    pub fn inverse_add(
        coeffs: &[i32],
        scratch: &mut [i32],
        log2_size: i32,
        dst4: bool,
        max_x: i32,
        max_y: i32,
        bit_depth: i32,
        dst: &mut [u16],
        stride: i32,
    ) -> HeifResult<()> {
        if !(2..=5).contains(&log2_size) || !(8..=16).contains(&bit_depth) {
            return Err(HeifError::invalid("变换尺寸或位深越界。"));
        }
        let n = 1usize << log2_size;
        // DST 仅 4 点（4×4）。防未来调用方误传 dst4=true & n≥8 让 SIMD DST 内核拿 DST_N（4 点基阵）
        // 按大步长/大块遍历越界（OOB/UB）。
        if dst4 && n != 4 {
            return Err(HeifError::invalid("DST 变换仅支持 4 点（dst4 要求 n==4）。"));
        }
        let max_x_u = usize::try_from(max_x).map_err(|_| HeifError::invalid("maxX 越界。"))?;
        let max_y_u = usize::try_from(max_y).map_err(|_| HeifError::invalid("maxY 越界。"))?;
        #[cfg(target_arch = "x86_64")]
        // avx2 内核还用 `_mm_cvtepu16_epi32`/`_mm_packus_epi32`/`_mm_mullo_epi32` 等 SSE4.1 指令；
        // 真实 AVX2 CPU 均含 SSE4.1，此处显式并查只为意图清晰（无 SSE4.1 则回落 sse41/标量路径）。
        if n >= 8
            && std::is_x86_feature_detected!("avx2")
            && std::is_x86_feature_detected!("sse4.1")
        {
            let m: &[i32] = if dst4 {
                &DST_N
            } else {
                HevcTransform::dct_n(log2_size as usize)?
            };
            return super::transform_simd::inverse_add_avx2(
                n, m, coeffs, scratch, max_x_u, max_y_u, bit_depth, dst, stride as usize,
            );
        }
        #[cfg(target_arch = "x86_64")]
        if std::is_x86_feature_detected!("sse4.1") {
            let m: &[i32] = if dst4 {
                &DST_N
            } else {
                HevcTransform::dct_n(log2_size as usize)?
            };
            return super::transform_simd::inverse_add_sse41(
                n, m, dst4, coeffs, scratch, max_x_u, max_y_u, bit_depth, dst, stride as usize,
            );
        }
        Self::inverse_add_impl(coeffs, scratch, log2_size, dst4, max_x, max_y, bit_depth, dst, stride)
    }

    /// 反变换的标量定义：与向量分层一样的遍与捷径，一次一个采样。
    /// 把缩放过后的系数 `coeffs`（行主序 N×N，列 `maxX` 与行 `maxY` 之外为零）反变换，
    /// 并把残差加进 `dst`（行距 `stride`）里的预测，截到采样范围。`scratch` 存中间 N×N 块。
    #[allow(clippy::too_many_arguments)]
    pub fn inverse_add_scalar(
        coeffs: &[i32],
        scratch: &mut [i32],
        log2_size: i32,
        dst4: bool,
        max_x: i32,
        max_y: i32,
        bit_depth: i32,
        dst: &mut [u16],
        stride: i32,
    ) -> HeifResult<()> {
        Self::inverse_add_impl(coeffs, scratch, log2_size, dst4, max_x, max_y, bit_depth, dst, stride)
    }

    /// 标量反变换的实现体（校验 + 垂直/水平两遍）。`inverse_add_scalar` 与 `inverse_add` 兜底共用。
    #[allow(clippy::too_many_arguments)]
    fn inverse_add_impl(
        coeffs: &[i32],
        scratch: &mut [i32],
        log2_size: i32,
        dst4: bool,
        max_x: i32,
        max_y: i32,
        bit_depth: i32,
        dst: &mut [u16],
        stride: i32,
    ) -> HeifResult<()> {
        if !(2..=5).contains(&log2_size) || !(8..=16).contains(&bit_depth) {
            return Err(HeifError::invalid("变换尺寸或位深越界。"));
        }
        let n = 1usize << log2_size;
        let n2 = n * n;
        let max_x = usize::try_from(max_x).map_err(|_| HeifError::invalid("maxX 越界。"))?;
        let max_y = usize::try_from(max_y).map_err(|_| HeifError::invalid("maxY 越界。"))?;
        // CheckBlock：核访问的全部下标的证明。
        let needed = (n as i64 - 1) * stride as i64 + n as i64;
        if coeffs.len() < n2
            || scratch.len() < n2
            || max_x >= n
            || max_y >= n
            || stride < n as i32
            || needed < 0
            || needed > dst.len() as i64
        {
            return Err(HeifError::invalid("变换块放不进缓冲。"));
        }
        let stride = stride as usize;
        let m: &[i32] = if dst4 {
            &DST_N
        } else {
            HevcTransform::dct_n(log2_size as usize)?
        };
        let g = scratch;

        // 垂直遍，列 0..maxX。
        if dst4 {
            for y in 0..n {
                for x in 0..=max_x {
                    let mut sum = 0i32;
                    for j in 0..=max_y {
                        sum = sum.wrapping_add(m[j * n + y].wrapping_mul(coeffs[j * n + x]));
                    }
                    g[y * n + x] = (sum.wrapping_add(64) >> 7).clamp(-32768, 32767);
                }
            }
        } else {
            // 偶数 maxY 之后那一行是零，所以奇数部分和可以把它算进去。
            let last_row = max_y | 1;
            for y in 0..n / 2 {
                for x in 0..=max_x {
                    let mut even = 0i32;
                    let mut odd = 0i32;
                    let mut j = 0usize;
                    while j <= last_row {
                        even = even.wrapping_add(m[j * n + y].wrapping_mul(coeffs[j * n + x]));
                        odd = odd
                            .wrapping_add(m[(j + 1) * n + y].wrapping_mul(coeffs[(j + 1) * n + x]));
                        j += 2;
                    }
                    g[y * n + x] = even.wrapping_add(odd).wrapping_add(64) >> 7;
                    g[y * n + x] = g[y * n + x].clamp(-32768, 32767);
                    g[(n - 1 - y) * n + x] = even.wrapping_sub(odd).wrapping_add(64) >> 7;
                    g[(n - 1 - y) * n + x] = g[(n - 1 - y) * n + x].clamp(-32768, 32767);
                }
            }
        }

        // 水平遍，项 0..maxX，最终位移（8-299）与重建（8-325）。
        let bd_shift = 20 - bit_depth;
        let round = 1i32 << (bd_shift - 1);
        let max = (1 << bit_depth) - 1;
        for y in 0..n {
            for x in 0..n {
                let mut sum = 0i32;
                for j in 0..=max_x {
                    sum = sum.wrapping_add(g[y * n + j].wrapping_mul(m[j * n + x]));
                }
                let residual = sum.wrapping_add(round) >> bd_shift;
                let off = y * stride + x;
                dst[off] = i32::from(dst[off])
                    .wrapping_add(residual)
                    .clamp(0, max) as u16;
            }
        }
        Ok(())
    }

    /// 变换跳过（8-298、8-299）：r = (d << tsShift + round) >> bdShift，原地。
    pub fn transform_skip(d: &mut [i32], log2_size: i32, bit_depth: i32) -> HeifResult<()> {
        if !(2..=5).contains(&log2_size) || !(8..=16).contains(&bit_depth) {
            return Err(HeifError::invalid("变换尺寸或位深越界。"));
        }
        let ts_shift = 5 + log2_size;
        let bd_shift = 20 - bit_depth;
        let round = 1i32 << (bd_shift - 1);
        let count = 1usize << (2 * log2_size);
        if d.len() < count {
            return Err(HeifError::invalid("变换跳过块放不进缓冲。"));
        }
        for v in d.iter_mut().take(count) {
            *v = v.wrapping_shl(ts_shift as u32).wrapping_add(round) >> bd_shift;
        }
        Ok(())
    }

    /// 已是采样单位的残差（变换跳过、bypass 时）的 (8-325) 重建。
    pub fn add_residual(
        residual: &[i32],
        log2_size: i32,
        bit_depth: i32,
        dst: &mut [u16],
        stride: i32,
    ) -> HeifResult<()> {
        if !(2..=5).contains(&log2_size) || !(8..=16).contains(&bit_depth) {
            return Err(HeifError::invalid("变换尺寸或位深越界。"));
        }
        let n = 1usize << log2_size;
        let needed = (n as i64 - 1) * stride as i64 + n as i64;
        if residual.len() < n * n || needed < 0 || needed > dst.len() as i64 {
            return Err(HeifError::invalid("残差块放不进缓冲。"));
        }
        let max = (1 << bit_depth) - 1;
        let stride = stride as usize;
        for y in 0..n {
            for x in 0..n {
                let off = y * stride + x;
                dst[off] = i32::from(dst[off])
                    .wrapping_add(residual[y * n + x])
                    .clamp(0, max) as u16;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[path = "../../golden_transform.rs"]
    mod golden;

    /// 32 点 DCT 基阵逐元素与 C# 生成的表一致。
    #[test]
    fn dct_table_matches_csharp() {
        assert_eq!(&HevcTransform::dct()[..], &golden::DCT[..]);
    }

    /// 4×4 DST 与 DctN 的 4x4 子矩阵从 DCT 表正确抽行。
    #[test]
    fn dst_and_dct4_subsample_matches() {
        assert_eq!(&HevcTransform::dst()[..], &golden::DST[..]);
        // dctN(2) 的 k 行等于 32 点 DCT 的第 k*8 行的前 4 列。
        let dct4 = HevcTransform::dct_n(2).unwrap();
        for k in 0..4usize {
            for x in 0..4usize {
                assert_eq!(dct4[k * 4 + x], i32::from(golden::DCT[k * 8 * 32 + x]));
            }
        }
    }

    /// Scaler：(8-309) 的缩放 + 截断，含正向/负向饱和。
    #[test]
    fn scaler_matches_csharp() {
        {
            let exp = &golden::SCALE_2_26_8;
            let sc = Scaler::new(2, 26, 8).unwrap();
            let pairs = [(40, 16), (1, 16), (255, 16), (40, 1), (40, 255), (-40, 16)];
            for (i, &(level, m)) in pairs.iter().enumerate() {
                assert_eq!(sc.scale(level, m), exp[i], "log2=2 qp=26 l={level} m={m}");
            }
        }
        {
            let exp = &golden::SCALE_3_30_8;
            let sc = Scaler::new(3, 30, 8).unwrap();
            let pairs = [(40, 16), (7, 3), (100, 100), (-100, 100)];
            for (i, &(level, m)) in pairs.iter().enumerate() {
                assert_eq!(sc.scale(level, m), exp[i], "log2=3 qp=30 l={level} m={m}");
            }
        }
        {
            let exp = &golden::SCALE_5_51_8;
            let sc = Scaler::new(5, 51, 8).unwrap();
            let pairs = [(40, 16), (1, 1), (2, 16), (3, 16)];
            for (i, &(level, m)) in pairs.iter().enumerate() {
                assert_eq!(sc.scale(level, m), exp[i], "log2=5 qp=51 l={level} m={m}");
            }
        }
        {
            let exp = &golden::SCALE_2_26_10;
            let sc = Scaler::new(2, 26, 10).unwrap();
            let pairs = [(40, 16), (40, 1), (255, 16), (-255, 16)];
            for (i, &(level, m)) in pairs.iter().enumerate() {
                assert_eq!(sc.scale(level, m), exp[i], "log2=2 qp=26 10bit l={level} m={m}");
            }
        }
        // 非法构造必须报错。
        assert!(Scaler::new(2, -1, 8).is_err());
        assert!(Scaler::new(6, 26, 8).is_err());
    }

    fn run_inverse(
        coeffs: &[i32],
        log2_size: i32,
        dst4: bool,
        max_x: i32,
        max_y: i32,
        bit_depth: i32,
        pred: &[u16],
    ) -> Vec<u16> {
        let n = 1usize << log2_size;
        let mut scratch = vec![0i32; n * n];
        let mut dst = pred.to_vec();
        HevcTransform::inverse_add_scalar(
            coeffs,
            &mut scratch,
            log2_size,
            dst4,
            max_x,
            max_y,
            bit_depth,
            &mut dst,
            n as i32,
        )
        .unwrap();
        dst
    }

    /// 逆 DCT/DST：4x4、8x8、16x16、32x32、10 位、带预测与双端饱和，与 C# 逐字节一致。
    #[test]
    fn inverse_add_matches_csharp() {
        let pred4 = {
            let mut p = vec![0u16; 16];
            for y in 0..4 {
                for x in 0..4 {
                    p[y * 4 + x] = (100 + y * 20 + x * 30) as u16;
                }
            }
            p
        };
        let pred_high = vec![200u16; 16];
        let coeffs_neg: Vec<i32> = golden::COEFFS4.iter().map(|v| -v).collect();

        assert_eq!(
            run_inverse(&golden::COEFFS4, 2, false, 3, 2, 8, &[0u16; 16])
                .as_slice(),
            &golden::IT4_DCT[..],
            "4x4 DCT"
        );
        assert_eq!(
            run_inverse(&golden::COEFFS4, 2, true, 3, 2, 8, &[0u16; 16]).as_slice(),
            &golden::IT4_DST[..],
            "4x4 DST"
        );
        assert_eq!(
            run_inverse(&golden::COEFFS8A, 3, false, 6, 5, 8, &[0u16; 64]).as_slice(),
            &golden::IT8A[..],
            "8x8 A"
        );
        assert_eq!(
            run_inverse(&golden::COEFFS8B, 3, false, 5, 2, 8, &[0u16; 64]).as_slice(),
            &golden::IT8B[..],
            "8x8 B"
        );
        assert_eq!(
            run_inverse(&golden::COEFFS16A, 4, false, 12, 12, 8, &[0u16; 256]).as_slice(),
            &golden::IT16A[..],
            "16x16"
        );
        assert_eq!(
            run_inverse(&golden::COEFFS32A, 5, false, 20, 10, 8, &[0u16; 1024]).as_slice(),
            &golden::IT32A[..],
            "32x32"
        );
        assert_eq!(
            run_inverse(&golden::COEFFS4, 2, false, 3, 2, 10, &[0u16; 16]).as_slice(),
            &golden::IT4_DCT_10BIT[..],
            "4x4 DCT 10 位"
        );
        assert_eq!(
            run_inverse(&golden::COEFFS8B, 3, false, 5, 2, 10, &[0u16; 64]).as_slice(),
            &golden::IT8_DCT_10BIT[..],
            "8x8 DCT 10 位"
        );
        assert_eq!(
            run_inverse(&golden::COEFFS4, 2, false, 3, 2, 8, &pred4).as_slice(),
            &golden::IT4_DCT_PRED[..],
            "4x4 DCT + 预测"
        );
        assert_eq!(
            run_inverse(&coeffs_neg, 2, false, 3, 2, 8, &pred_high).as_slice(),
            &golden::IT4_DCT_HIGHPRED_NEG[..],
            "4x4 DCT 饱和减"
        );
    }

    /// 越界参数必须报错而非 panic。
    #[test]
    fn inverse_add_invalid_arguments_error() {
        let mut scratch = vec![0i32; 16];
        let mut dst = vec![0u16; 16];
        let coeffs = golden::COEFFS4;
        assert!(HevcTransform::inverse_add_scalar(&coeffs, &mut scratch, 2, false, 4, 2, 8, &mut dst, 4).is_err(), "maxX==n");
        assert!(HevcTransform::inverse_add_scalar(&coeffs, &mut scratch, 2, false, -1, 2, 8, &mut dst, 4).is_err(), "maxX<0");
        assert!(HevcTransform::inverse_add_scalar(&coeffs, &mut scratch, 2, false, 3, 2, 8, &mut dst, 3).is_err(), "stride<n");
        assert!(HevcTransform::inverse_add_scalar(&coeffs, &mut [0i32; 8], 2, false, 3, 2, 8, &mut dst, 4).is_err(), "scratch 过小");
        assert!(HevcTransform::inverse_add_scalar(&golden::COEFFS8A, &mut [0i32; 64], 3, false, 6, 5, 8, &mut [0u16; 64], 8).is_ok(), "8x8 合法");
    }

    /// TransformSkip 与 AddResidual 与 C# 逐字节一致。
    #[test]
    fn transform_skip_and_add_residual_match_csharp() {
        let mut d = vec![0i32; 16];
        for (i, slot) in d.iter_mut().enumerate() {
            *slot = if i % 4 == 0 { (10 + i) as i32 } else { -(i as i32 + 3) };
        }
        HevcTransform::transform_skip(&mut d, 2, 8).unwrap();
        assert_eq!(d.as_slice(), &golden::TSKIP_2_8[..], "TSKIP 8 位");
        for (i, slot) in d.iter_mut().enumerate() {
            *slot = ((i * 7) % 200) as i32 - 100;
        }
        HevcTransform::transform_skip(&mut d, 2, 10).unwrap();
        assert_eq!(d.as_slice(), &golden::TSKIP_2_10[..], "TSKIP 10 位");

        let mut res = vec![0i32; 16];
        let mut dst = vec![0u16; 16];
        for i in 0..16 {
            res[i] = (i % 5) as i32 - 2;
            dst[i] = (i * 17 + 3) as u16;
        }
        HevcTransform::add_residual(&res, 2, 8, &mut dst, 4).unwrap();
        assert_eq!(dst.as_slice(), &golden::ADDRES_2_8[..], "ADDRES 8 位");
        for i in 0..16 {
            res[i] = 40 - ((i * 13) % 90) as i32;
            dst[i] = (1023 - i * 40) as u16;
        }
        HevcTransform::add_residual(&res, 2, 10, &mut dst, 4).unwrap();
        assert_eq!(dst.as_slice(), &golden::ADDRES_2_10[..], "ADDRES 10 位");

        assert!(HevcTransform::transform_skip(&mut [0i32; 8], 3, 8).is_err(), "d 过小");
    }
}
