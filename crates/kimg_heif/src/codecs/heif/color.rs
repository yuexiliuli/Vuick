//! 色彩转换（C# `HeifColor.cs` 一比一移植）：Y′CbCr → RGBA8 / RGBA64 / gray 的逐系数转换。
//! H.265 只定义解码采样，不定义这一步，所以它在此冻结（同 C# 旨意）：任何算术改动都是输出改动。
//! 像素计算逐行对齐 C# 标量内核：整数运算、16.16 固定系数（`round(value·2^16)`）、`+2^15 >> 16`
//! 舍入、`[0,255]` / `[0,65535]` 裁剪。ICC 不处理（直通/忽略）。

#![forbid(unsafe_code)]

use crate::codecs::heif::error::{HeifError, HeifResult};

/// 固定点系数（对应 C# `HeifColor.Coefficients`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Coefficients {
    pub y_offset: i32,
    pub c_offset: i32,
    pub y: i32,
    pub cr_r: i32,
    pub cb_g: i32,
    pub cr_g: i32,
    pub cb_b: i32,
    pub identity: bool,
}

impl Coefficients {
    /// 8 位转换（C# `HeifColor.For`）：`matrix_coefficients`（H.273 / H.265 Table E.5）选 Kr/Kb，
    /// full/log 范围与位深决定标度与偏移，RGB 由
    /// R = Y + 2(1−Kr)·Cr、B = Y + 2(1−Kb)·Cb、G = Y − 2Kb(1−Kb)/Kg·Cb − 2Kr(1−Kr)/Kg·Cr
    /// （Kg = 1−Kr−Kb）算出；每系数 `Fixed` 一次（双精度），像素内只用整数。
    pub(crate) fn for_matrix(matrix_coefficients: i32, full_range: bool, bit_depth: i32) -> Coefficients {
        let scale = 1 << (bit_depth - 8);
        let (y_mul, c_mul, y_offset) = if full_range {
            let m = 255.0 / ((1 << bit_depth) - 1) as f64;
            (m, m, 0)
        } else {
            (
                255.0 / (219 * scale) as f64,
                255.0 / (224 * scale) as f64,
                16 * scale,
            )
        };
        let c_offset = 1 << (bit_depth - 1);
        if matrix_coefficients == 0 {
            // Identity (GBR)：每通道都像 luma 一样缩放；C# 把 cOffset 传成 yOffset。
            return Coefficients {
                y_offset,
                c_offset: y_offset,
                y: fixed(y_mul),
                cr_r: 0,
                cb_g: 0,
                cr_g: 0,
                cb_b: 0,
                identity: true,
            };
        }
        let (kr, kb) = match matrix_coefficients {
            1 => (0.2126, 0.0722),
            4 => (0.30, 0.11),
            7 => (0.212, 0.087),
            9 | 10 => (0.2627, 0.0593),
            _ => (0.299, 0.114),
        };
        let kg = 1.0 - kr - kb;
        Coefficients {
            y_offset,
            c_offset,
            y: fixed(y_mul),
            cr_r: fixed(2.0 * (1.0 - kr) * c_mul),
            cb_g: fixed(2.0 * kb * (1.0 - kb) / kg * c_mul),
            cr_g: fixed(2.0 * kr * (1.0 - kr) / kg * c_mul),
            cb_b: fixed(2.0 * (1.0 - kb) * c_mul),
            identity: false,
        }
    }

    /// 16 位转换（C# `HeifColor.For16`）：同 [`Self::for_matrix`] 的矩阵/偏移/舍入，输出标度 65535
    /// 换 255 —— full 65535/(2^b−1)，limited 65535/(219·2^(b−8)) 与 65535/(224·2^(b−8))。
    /// 预留供 >8 位图像的 RGBA64 输出路径使用（现 oracle 只断言 8 位输出，故暂时无调用点）。
    #[allow(dead_code)]
    pub(crate) fn for_matrix16(matrix_coefficients: i32, full_range: bool, bit_depth: i32) -> Coefficients {
        let scale = 1 << (bit_depth - 8);
        let (y_mul, c_mul, y_offset) = if full_range {
            let m = 65535.0 / ((1 << bit_depth) - 1) as f64;
            (m, m, 0)
        } else {
            (
                65535.0 / (219 * scale) as f64,
                65535.0 / (224 * scale) as f64,
                16 * scale,
            )
        };
        let c_offset = 1 << (bit_depth - 1);
        if matrix_coefficients == 0 {
            return Coefficients {
                y_offset,
                c_offset: y_offset,
                y: fixed(y_mul),
                cr_r: 0,
                cb_g: 0,
                cr_g: 0,
                cb_b: 0,
                identity: true,
            };
        }
        let (kr, kb) = match matrix_coefficients {
            1 => (0.2126, 0.0722),
            4 => (0.30, 0.11),
            7 => (0.212, 0.087),
            9 | 10 => (0.2627, 0.0593),
            _ => (0.299, 0.114),
        };
        let kg = 1.0 - kr - kb;
        Coefficients {
            y_offset,
            c_offset,
            y: fixed(y_mul),
            cr_r: fixed(2.0 * (1.0 - kr) * c_mul),
            cb_g: fixed(2.0 * kb * (1.0 - kb) / kg * c_mul),
            cr_g: fixed(2.0 * kr * (1.0 - kr) / kg * c_mul),
            cb_b: fixed(2.0 * (1.0 - kb) * c_mul),
            identity: false,
        }
    }
}

/// C# `Math.Round(value * 65536, AwayFromZero)`；Rust `f64::round` 同为「四舍五入远离零」，
/// 双精度计算与 .NET 逐位一致，因此系数逐字节相等。
fn fixed(value: f64) -> i32 {
    (value * 65536.0).round() as i32
}

/// 8 位转换的一行（C# `HeifColor.ToRgba420RowScalar`）：Y[x] 取色度 cb/cr[x/2]（4:2:0 最近邻复制），
/// 每像素写 R G B A 4 字节。`out` 至少 `width*4`。
///
/// 标量定义（向量核的兜底/对照，且承担 identity 与 `width<8`、AVX2 缺失时的全部行处理）：
/// 从像素 0 开始处理整行；行内分派（向量车身+标量补尾）见 [`to_rgba_420_row`]。
pub(crate) fn to_rgba_420_row_scalar(
    y: &[u16],
    cb: &[u16],
    cr: &[u16],
    out: &mut [u8],
    width: usize,
    k: &Coefficients,
) {
    to_rgba_420_row_scalar_from(y, cb, cr, out, 0, width, k);
}

/// 整行分派：AVX2 且 `width>=8` 且非 identity → 8-lane 向量核（`color_simd::to_rgba_420_row_avx2`）
/// 处理前 `floor(width/8)*8` 像素，尾段（`width%8`）回标量；其余情形（identity / `width<8` /
/// 无 AVX2）整行走标量。输出与 [`to_rgba_420_row_scalar`] 逐位一致。
pub(crate) fn to_rgba_420_row(
    y: &[u16],
    cb: &[u16],
    cr: &[u16],
    out: &mut [u8],
    width: usize,
    k: &Coefficients,
) {
    #[cfg(target_arch = "x86_64")]
    let done = {
        if width >= 8 && !k.identity && std::is_x86_feature_detected!("avx2") {
            // color_simd::to_rgba_420_row_avx2 是安全包装（仿 hevc/transform_simd::inverse_add_avx2
            // 先例：内部自证缓冲前置再把 unsafe 内核封在 unsafe{} 内），调用方无需 unsafe；AVX2
            // 已在此运行时检出（叶子隔离，见 color_simd 模块文档）。
            super::color_simd::to_rgba_420_row_avx2(y, cb, cr, out, width, k)
        } else {
            0
        }
    };
    #[cfg(not(target_arch = "x86_64"))]
    let done = 0;
    if done == 0 {
        // 全程标量（identity / width<8 / 无 AVX2）：整行走规范标量行内核，保持单一标量定义。
        to_rgba_420_row_scalar(y, cb, cr, out, width, k);
    } else {
        // 向量核处理前 done 像素后，尾段（width-done）回标量补全。
        to_rgba_420_row_scalar_from(y, cb, cr, out, done, width, k);
    }
}

/// 标量行内核从像素 `start` 写到 `width`（向量核车身之后的分派点），像素 `x` 的色度取
/// `cb/cr[x>>1]`、亮度取 `y[x]`，与 [`to_rgba_420_row_scalar`] 在 `start==0` 时逐位一致。
fn to_rgba_420_row_scalar_from(
    y: &[u16],
    cb: &[u16],
    cr: &[u16],
    out: &mut [u8],
    start: usize,
    width: usize,
    k: &Coefficients,
) {
    let chroma = (width + 1) >> 1;
    debug_assert!(
        y.len() >= width && cb.len() >= chroma && cr.len() >= chroma && out.len() >= width * 4,
        "colour row 不匹配其缓冲。"
    );
    for x in start..width {
        let luma = i32::from(y[x]).wrapping_sub(k.y_offset);
        let u = i32::from(cb[x >> 1]).wrapping_sub(k.c_offset);
        let v = i32::from(cr[x >> 1]).wrapping_sub(k.c_offset);
        let (r, g, b) = if k.identity {
            (
                k.y.wrapping_mul(v).wrapping_add(32768) >> 16,
                k.y.wrapping_mul(luma).wrapping_add(32768) >> 16,
                k.y.wrapping_mul(u).wrapping_add(32768) >> 16,
            )
        } else {
            let yy = k.y.wrapping_mul(luma).wrapping_add(32768);
            (
                yy.wrapping_add(k.cr_r.wrapping_mul(v)) >> 16,
                yy.wrapping_sub(k.cb_g.wrapping_mul(u)).wrapping_sub(k.cr_g.wrapping_mul(v)) >> 16,
                yy.wrapping_add(k.cb_b.wrapping_mul(u)) >> 16,
            )
        };
        out[x * 4] = r.clamp(0, 255) as u8;
        out[x * 4 + 1] = g.clamp(0, 255) as u8;
        out[x * 4 + 2] = b.clamp(0, 255) as u8;
        out[x * 4 + 3] = 255;
    }
}

/// 16 位转换的一行（C# `HeifColor.ToRgba64420Row` 标量）：产物在 64 位整数里算，输出 u16
/// `[0,65535]`，每像素写 R G B A。同 8 位转换的矩阵/偏移/舍入（`For16` 系数）。
/// 预留供 >8 位图像的 RGBA64 输出路径使用（现 oracle 只断言 8 位输出，故暂时无调用点）。
#[allow(dead_code)]
pub(crate) fn to_rgba64_420_row_scalar(
    y: &[u16],
    cb: &[u16],
    cr: &[u16],
    out: &mut [u16],
    width: usize,
    k: &Coefficients,
) {
    let chroma = (width + 1) >> 1;
    debug_assert!(
        y.len() >= width && cb.len() >= chroma && cr.len() >= chroma && out.len() >= width * 4,
        "colour row 不匹配其缓冲。"
    );
    for x in 0..width {
        let luma = i64::from(y[x]) - i64::from(k.y_offset);
        let u = i64::from(cb[x >> 1]) - i64::from(k.c_offset);
        let v = i64::from(cr[x >> 1]) - i64::from(k.c_offset);
        let (r, g, b) = if k.identity {
            (
                (i64::from(k.y) * v + 32768) >> 16,
                (i64::from(k.y) * luma + 32768) >> 16,
                (i64::from(k.y) * u + 32768) >> 16,
            )
        } else {
            let yy = i64::from(k.y) * luma + 32768;
            (
                (yy + i64::from(k.cr_r) * v) >> 16,
                (yy - i64::from(k.cb_g) * u - i64::from(k.cr_g) * v) >> 16,
                (yy + i64::from(k.cb_b) * u) >> 16,
            )
        };
        out[x * 4] = r.clamp(0, 65535) as u16;
        out[x * 4 + 1] = g.clamp(0, 65535) as u16;
        out[x * 4 + 2] = b.clamp(0, 65535) as u16;
        out[x * 4 + 3] = 65535;
    }
}

/// 8 位 grey 的一行（C# `HeifColor.ToGrayRow`）：输出存储 luma，像 RGB 路径的 Y 项一样按范围映射
/// `gray = clamp((Y·(y−YOffset) + 2^15) >> 16, 0, 255)`——中性色度像素转出的 R=G=B 恰等于它。
/// 不适用于 identity 矩阵（其 Y 平面是 G，不是 luma）。
/// 预留供 Gray8 输出路径使用（现 oracle 只断言 RGBA8，故暂时无调用点）。
#[allow(dead_code)]
pub(crate) fn to_gray_row(y: &[u16], out: &mut [u8], width: usize, k: &Coefficients) {
    debug_assert!(y.len() >= width && out.len() >= width);
    for x in 0..width {
        let value = k
            .y
            .wrapping_mul(i32::from(y[x]).wrapping_sub(k.y_offset))
            .wrapping_add(32768)
            >> 16;
        out[x] = value.clamp(0, 255) as u8;
    }
}

/// 整图 Y′CbCr → RGBA8（`yuv_to_rgba`）：`width×height` 亮度平面 + 4:2:0 色度平面
/// （行主序、无跨步），逐行走 [`to_rgba_420_row`]（内部 AVX2/标量分派），输出
/// `width*height*4` 字节（R G B A）。
/// 参数照 C# `HeifColor.For(matrix_coefficients, full_range, bit_depth)`；位深 8/10 直接映射到 8 位输出。
pub fn yuv_to_rgba(
    y: &[u16],
    cb: &[u16],
    cr: &[u16],
    width: usize,
    height: usize,
    bit_depth: u8,
    matrix_coefficients: i32,
    full_range: bool,
) -> HeifResult<Vec<u8>> {
    let cw = (width + 1) >> 1;
    let ch = (height + 1) >> 1;
    if y.len() < width * height || cb.len() < cw * ch || cr.len() < cw * ch {
        return Err(HeifError::invalid("YUV 平面尺寸不足。"));
    }
    if !(8..=16).contains(&bit_depth) {
        return Err(HeifError::invalid(format!(
            "不支持的位深 {bit_depth}；只支持 8–16。"
        )));
    }
    let k = Coefficients::for_matrix(matrix_coefficients, full_range, i32::from(bit_depth));
    let mut out = vec![0u8; width * height * 4];
    for row in 0..height {
        let c_off = (row >> 1) * cw;
        to_rgba_420_row(
            &y[row * width..(row + 1) * width],
            &cb[c_off..c_off + cw],
            &cr[c_off..c_off + cw],
            &mut out[row * width * 4..(row + 1) * width * 4],
            width,
            &k,
        );
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coefficients_match_csharp_for_8bit() {
        // 常数来自 .NET 双精度同口径计算（`HeifColor.For`）。
        let k = Coefficients::for_matrix(2, false, 8);
        assert_eq!(k.y_offset, 16);
        assert_eq!(k.c_offset, 128);
        assert_eq!(k.y, 76309);
        assert_eq!(k.cr_r, 104597);
        assert_eq!(k.cb_g, 25675);
        assert_eq!(k.cr_g, 53279);
        assert_eq!(k.cb_b, 132201);
        assert!(!k.identity);

        let k = Coefficients::for_matrix(1, true, 8);
        assert_eq!(k.y_offset, 0);
        assert_eq!(k.c_offset, 128);
        assert_eq!(k.y, 65536);
        assert_eq!(k.cr_r, 103206);
        assert_eq!(k.cb_g, 12276);
        assert_eq!(k.cr_g, 30679);
        assert_eq!(k.cb_b, 121609);

        let k = Coefficients::for_matrix(1, false, 8);
        assert_eq!(k.y_offset, 16);
        assert_eq!(k.c_offset, 128);
        assert_eq!(k.y, 76309);
        assert_eq!(k.cr_r, 117489);
        assert_eq!(k.cb_g, 13975);
        assert_eq!(k.cr_g, 34925);
        assert_eq!(k.cb_b, 138438);
    }

    #[test]
    fn coefficients_identity_and_10bit() {
        let k = Coefficients::for_matrix(0, false, 8);
        assert!(k.identity);
        assert_eq!(k.y_offset, 16);
        assert_eq!(k.c_offset, 16);
        assert_eq!(k.y, 76309);

        let k = Coefficients::for_matrix(0, true, 10);
        assert!(k.identity);
        assert_eq!(k.y_offset, 0);
        assert_eq!(k.c_offset, 0);
        assert_eq!(k.y, 16336);

        let k = Coefficients::for_matrix(2, false, 10);
        assert_eq!(k.y_offset, 64);
        assert_eq!(k.c_offset, 512);
        assert_eq!(k.y, 19077);
        assert_eq!(k.cr_r, 26149);
        assert_eq!(k.cb_g, 6419);
        assert_eq!(k.cr_g, 13320);
        assert_eq!(k.cb_b, 33050);

        let k = Coefficients::for_matrix(9, true, 10);
        assert_eq!(k.y_offset, 0);
        assert_eq!(k.c_offset, 512);
        assert_eq!(k.y, 16336);
        assert_eq!(k.cr_r, 24089);
        assert_eq!(k.cb_g, 2688);
        assert_eq!(k.cr_g, 9334);
        assert_eq!(k.cb_b, 30734);
    }

    #[test]
    fn coefficients16_match_csharp_for_10bit() {
        // For16(2, false, 10)：标度 65535（相对 8 位 ×257 的整体）。
        let k = Coefficients::for_matrix16(2, false, 10);
        assert_eq!(k.y_offset, 64);
        assert_eq!(k.c_offset, 512);
        assert_eq!(k.y, 4902856);
        assert_eq!(k.cr_r, 6720371);
        assert_eq!(k.cb_g, 1649589);
        assert_eq!(k.cr_g, 3423153);
        assert_eq!(k.cb_b, 8493935);
        assert!(!k.identity);
    }

    #[test]
    fn rgba_row_known_samples() {
        // 有限范围 BT.601：Y=16+Cb=128+Cr=128 → 灰 0；Y=235+Cb=128+Cr=128 → 白 255。
        let k = Coefficients::for_matrix(2, false, 8);
        let mut out = [0u8; 8];
        to_rgba_420_row_scalar(&[16], &[128, 128], &[128, 128], &mut out[..4], 1, &k);
        assert_eq!(out[..4], [0, 0, 0, 255]);
        to_rgba_420_row_scalar(&[235], &[128, 128], &[128, 128], &mut out[4..], 1, &k);
        assert_eq!(out[4..], [255, 255, 255, 255]);
    }

    #[test]
    fn rgba_row_clips() {
        // 越界色度被裁剪：Y=64（白带暗）、Cb=Cr=0 → R/B 下溢剪到 0，G 正常（膝盖偏青）。
        // 手工按 C# 算术复算：luma=48，u=v=-128，yy=3695600，
        //   r=(yy+CrR·v)>>16 = -148 → 0；g=(yy−CbG·u−CrG·v)>>16 = 210；b=(yy+CbB·u)>>16 = -202 → 0。
        let k = Coefficients::for_matrix(2, false, 8);
        let mut out = [0u8; 4];
        to_rgba_420_row_scalar(&[64], &[0], &[0], &mut out, 1, &k);
        assert_eq!(out, [0, 210, 0, 255]);
    }

    #[test]
    fn yuv_to_rgba_dim_check() {
        assert!(yuv_to_rgba(&[0u16; 4], &[0; 1], &[0; 1], 2, 2, 8, 2, false).is_ok());
        assert!(yuv_to_rgba(&[0u16; 3], &[0; 1], &[0; 1], 2, 2, 8, 2, false).is_err());
    }

    /// xorshift64* 确定性 PRNG（跨平台稳定，无 rand 依赖）。
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
    }

    fn rand_vec(rng: &mut Rng, len: usize, max: u16) -> Vec<u16> {
        (0..len)
            .map(|_| (rng.next() % (max as u64 + 1)) as u16)
            .collect()
    }

    /// 找出两行输出第一个不同像素的 (x, ch)（RGBA 首差定位）。
    fn first_diff(a: &[u8], b: &[u8], width: usize) -> Option<(usize, usize)> {
        assert_eq!(a.len(), b.len(), "输出段长度不一致");
        for x in 0..width {
            for ch in 0..4 {
                if a[x * 4 + ch] != b[x * 4 + ch] {
                    return Some((x, ch));
                }
            }
        }
        None
    }

    /// 整行分派（向量车身 + 标量补尾）≡ 整行标量：随机 Y/Cb/Cr × 矩阵 {601,709,2020} ×
    /// full_range {true,false} × 位深 {8,10} × 宽 {8,9,17,31,32,64}（覆盖尾段与 8 对齐）。
    #[test]
    fn to_rgba_420_row_dispatch_matches_scalar_randomised() {
        if !std::is_x86_feature_detected!("avx2") {
            eprintln!("SKIP: 无 AVX2，分派退化为标量，无向量路径可对拍");
            return;
        }
        let mut rng = Rng(0xA5A5_5A5A_F00F_FACE);
        let mut tried = 0usize;
        for &bd in &[8u8, 10u8] {
            for &matrix in &[2, 1, 9] {
                // matrix 2 = BT.601（unspecified），1 = BT.709，9 = BT.2020。
                for &full in &[false, true] {
                    for &width in &[8usize, 9, 17, 31, 32, 64] {
                        let k = Coefficients::for_matrix(matrix, full, bd as i32);
                        let chroma = (width + 1) >> 1;
                        let max_sample = if bd == 10 { 1023u16 } else { 255u16 };
                        let y = rand_vec(&mut rng, width, max_sample);
                        let cb = rand_vec(&mut rng, chroma, max_sample);
                        let cr = rand_vec(&mut rng, chroma, max_sample);
                        let mut ref_out = vec![0u8; width * 4];
                        to_rgba_420_row_scalar(&y, &cb, &cr, &mut ref_out, width, &k);
                        let mut dispatch_out = vec![0u8; width * 4];
                        to_rgba_420_row(&y, &cb, &cr, &mut dispatch_out, width, &k);
                        if let Some((x, ch)) = first_diff(&ref_out, &dispatch_out, width) {
                            panic!(
                                "分派≠标量 @ (x={x}, ch={ch}) matrix={matrix} full={full} bd={bd} \
                                 width={width}: 标量={} 分派={}",
                                ref_out[x * 4 + ch],
                                dispatch_out[x * 4 + ch]
                            );
                        }
                        tried += 1;
                    }
                }
            }
        }
        assert!(tried > 0, "分派随机对拍未执行任何一轮");
    }

    /// identity 矩阵：分派必须仍走标量（不走向量核），与整行标量逐字节相等。
    #[test]
    fn to_rgba_420_row_identity_stays_scalar() {
        let mut rng = Rng(0xDEAD_BEEF_3133_7F00);
        for &bd in &[8u8, 10u8] {
            for &full in &[false, true] {
                for &width in &[8usize, 32, 33] {
                    let k = Coefficients::for_matrix(0, full, bd as i32);
                    assert!(k.identity);
                    let chroma = (width + 1) >> 1;
                    let max_sample = if bd == 10 { 1023u16 } else { 255u16 };
                    let y = rand_vec(&mut rng, width, max_sample);
                    let cb = rand_vec(&mut rng, chroma, max_sample);
                    let cr = rand_vec(&mut rng, chroma, max_sample);
                    let mut ref_out = vec![0u8; width * 4];
                    to_rgba_420_row_scalar(&y, &cb, &cr, &mut ref_out, width, &k);
                    let mut out = vec![0u8; width * 4];
                    to_rgba_420_row(&y, &cb, &cr, &mut out, width, &k);
                    assert_eq!(
                        out, ref_out,
                        "identity 分派应恒走标量（bd={bd} full={full} width={width}）"
                    );
                }
            }
        }
    }

    /// 整图层级：随机多行（奇数行带宽/高）yuv_to_rgba 经分派后与单行标量拼接逐字节相等。
    #[test]
    fn yuv_to_rgba_dispatch_matches_scalar_full() {
        let mut rng = Rng(0x0DDB_A11_CAFE_F00D);
        for &(w, h) in &[(8usize, 2usize), (9, 1), (17, 3), (31, 2), (32, 3), (64, 2)] {
            for &(bd, matrix, full) in &[(8u8, 2, false), (10, 1, true), (8, 9, true)] {
                let cw = (w + 1) >> 1;
                let ch = (h + 1) >> 1;
                let max_sample = if bd == 10 { 1023u16 } else { 255u16 };
                let y: Vec<u16> = rand_vec(&mut rng, w * h, max_sample);
                let cb: Vec<u16> = rand_vec(&mut rng, cw * ch, max_sample);
                let cr: Vec<u16> = rand_vec(&mut rng, cw * ch, max_sample);
                let k = Coefficients::for_matrix(matrix, full, bd as i32);
                // 期望：逐行标量拼接。
                let mut expected = vec![0u8; w * h * 4];
                for row in 0..h {
                    let c_off = (row >> 1) * cw;
                    to_rgba_420_row_scalar(
                        &y[row * w..(row + 1) * w],
                        &cb[c_off..c_off + cw],
                        &cr[c_off..c_off + cw],
                        &mut expected[row * w * 4..(row + 1) * w * 4],
                        w,
                        &k,
                    );
                }
                let got =
                    yuv_to_rgba(&y, &cb, &cr, w, h, bd, matrix, full).expect("yuv_to_rgba");
                assert_eq!(got, expected, "整图分派≠逐行标量 (w={w} h={h} bd={bd} m={matrix} full={full})");
            }
        }
    }
}
