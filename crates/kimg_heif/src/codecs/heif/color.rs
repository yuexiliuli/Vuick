//! 色彩转换（C# `HeifColor.cs` 一比一移植）：Y′CbCr → RGBA8 / RGBA64 / gray 的逐系数转换。
//! H.265 只定义解码采样，不定义这一步，所以它在此冻结（同 C# 旨意）：任何算术改动都是输出改动。
//! 像素计算逐行对齐 C# 标量内核：整数运算、16.16 固定系数（`round(value·2^16)`）、`+2^15 >> 16`
//! 舍入、`[0,255]` / `[0,65535]` 裁剪。ICC 不处理（直通/忽略）。

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
        if width >= 8 && !k.identity && kimg_core::simd::detect("avx2") {
            // target_feature 内核（SAFE 签名：调用前置与标量相同 y.len()>=width、cb/cr.len()>=
            // (width+1)>>1、out.len()>=width*4，见 color_simd 模块文档）；AVX2 已运行时检出，
            // 叶子隔离（与 hevc/transform_simd 同一姿势）。
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
/// 分配中间缓冲并返回；需要直接写进既有目标时用 [`yuv_to_rgba_into`]（消除一次整图拷贝）。
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
    let mut out = vec![0u8; width * height * 4];
    yuv_to_rgba_into(
        y, cb, cr, width, height, bit_depth, matrix_coefficients, full_range, &mut out,
    )?;
    Ok(out)
}

/// 并行色彩的最小行数门槛（行数过少线程开销 > 收益；`KIMG_HEIF_PARALLEL=0` 恒串行）。
const COLOR_PAR_MIN_ROWS: usize = 4;
/// 行带并行每带最小学（防 tiny dest 过分割：可用核再多也不切出更细的行带）。
const COLOR_PAR_BAND_MIN_ROWS: usize = 8;

/// 整图 Y′CbCr → RGBA8 直写：与 [`yuv_to_rgba`] 逐字节恒等（同一系数/逐行分派/alpha=255），
/// 但不分配中间缓冲，直接把 `width*height*4` 字节（R G B A）写进 `dest`
/// （`dest.len() < width*height*4` 时返回 `Err`，不写任何字节）。
/// 供 `decode_rgba` 无 transform 时直写 `destination` 使用。
pub fn yuv_to_rgba_into(
    y: &[u16],
    cb: &[u16],
    cr: &[u16],
    width: usize,
    height: usize,
    bit_depth: u8,
    matrix_coefficients: i32,
    full_range: bool,
    dest: &mut [u8],
) -> HeifResult<()> {
    color_band(
        y,
        cb,
        cr,
        width,
        height,
        bit_depth,
        matrix_coefficients,
        full_range,
        dest,
        0,
        height,
        height,
    )
}

/// 行带 [r0, r1) 的色彩转换（[`yuv_to_rgba_into`] 的行带切片版本）：与整图逐行同算术
/// （同一 [`to_rgba_420_row`] 分派）、逐字节恒等，只处理输出行 `[r0, r1)`。
///
/// `dest` 是「从输出行 `r0` 起」的切片：整图调用传 `r0=0, r1=out_h` 且 `dest` 为整图
/// （`out_h == height`）；行带调用传 `split_at_mut` 得到的**子切片**（长度恰好
/// `(r1-r0)*width*4`）。容量按带校验（`dest.len() < (r1-r0)*width*4` 返回 `Err`，不写
/// 任何字节），平面尺寸/位深校验与 [`yuv_to_rgba_into`] 完全一致。`out_h` 为输出总高
/// （须 `r1 <= out_h`；现调用恒等于 `height`）。chroma 邻近行只读共享，无写冲突。
pub(crate) fn color_band(
    y: &[u16],
    cb: &[u16],
    cr: &[u16],
    width: usize,
    height: usize,
    bit_depth: u8,
    matrix_coefficients: i32,
    full_range: bool,
    dest: &mut [u8],
    r0: usize,
    r1: usize,
    out_h: usize,
) -> HeifResult<()> {
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
    if r1 > out_h || dest.len() < (r1 - r0) * width * 4 {
        return Err(HeifError::invalid("目标缓冲不足。"));
    }
    let k = Coefficients::for_matrix(matrix_coefficients, full_range, i32::from(bit_depth));
    for row in r0..r1 {
        let c_off = (row >> 1) * cw;
        let start = (row - r0) * width * 4;
        to_rgba_420_row(
            &y[row * width..(row + 1) * width],
            &cb[c_off..c_off + cw],
            &cr[c_off..c_off + cw],
            &mut dest[start..start + width * 4],
            width,
            &k,
        );
    }
    Ok(())
}

/// 并行行带色彩转换（[`yuv_to_rgba_into`] 的并行版）：逐输出行独立（行间无写依赖，
/// chroma 邻近行只读共享），按连续行带 `split_at_mut` 分块交 `std::thread::scope` worker
/// 并行处理，逐字节恒等——仅执行顺序按带切分，per-row 算术与 [`color_band`] 完全一致。
///
/// 门控（`decode_rgba` 的 `tiles>=2 && !force_serial()` 路由之外的兜底）：`KIMG_HEIF_PARALLEL=0`
/// （[`super::force_serial`]）或行数 < [`COLOR_PAR_MIN_ROWS`] 或可用核 <2 → 落单线程整图
/// （与 [`yuv_to_rgba_into`] 逐字节恒等）。供 `decode_rgba`（tiles>=2 的并行路径）使用。
pub(crate) fn yuv_to_rgba_into_par(
    y: &[u16],
    cb: &[u16],
    cr: &[u16],
    width: usize,
    height: usize,
    bit_depth: u8,
    matrix_coefficients: i32,
    full_range: bool,
    dest: &mut [u8],
) -> HeifResult<()> {
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
    if dest.len() < width * height * 4 {
        return Err(HeifError::invalid("目标缓冲不足。"));
    }
    let avail = std::thread::available_parallelism()
        .map(|p| p.get())
        .unwrap_or(1);
    // 线程数上限 = 可用核 ∩ 保证每带至少 COLOR_PAR_BAND_MIN_ROWS 行（防 tiny dest 过分割）。
    let threads = avail
        .min(height.div_ceil(COLOR_PAR_BAND_MIN_ROWS))
        .max(1);
    if super::force_serial() || height < COLOR_PAR_MIN_ROWS || threads < 2 {
        return color_band(
            y, cb, cr, width, height, bit_depth, matrix_coefficients, full_range, dest, 0,
            height, height,
        );
    }
    // 行带划分：chunk = ceil(height/threads)，带 i = [i*chunk, min((i+1)*chunk, height))。
    let chunk = height.div_ceil(threads);
    let first_err: std::sync::Mutex<Option<HeifError>> = std::sync::Mutex::new(None);
    std::thread::scope(|scope| {
        let mut rest = dest;
        let mut r0 = 0usize;
        while r0 < height {
            let r1 = (r0 + chunk).min(height);
            let band_bytes = (r1 - r0) * width * 4;
            let (seg, tail) = rest.split_at_mut(band_bytes);
            rest = tail;
            let first = r0;
            let err_slot = &first_err;
            scope.spawn(move || {
                // 已整体校验；带内 color_band 必 Ok（平面/dest 尺寸均就绪）。仍收集首个
                // Err 防静默半写（评审 NIT：勿吞错误）。
                if let Err(e) = color_band(
                    y, cb, cr, width, height, bit_depth, matrix_coefficients, full_range,
                    seg, first, r1, height,
                ) {
                    let mut slot = err_slot.lock().unwrap();
                    if slot.is_none() {
                        *slot = Some(e);
                    }
                }
            });
            r0 = r1;
        }
    });
    if let Some(e) = first_err.into_inner().unwrap() {
        return Err(e);
    }
    Ok(())
}

/// 并行版 [`yuv_to_rgba`]：分配整图 RGBA 后走 [`yuv_to_rgba_into_par`]（tiles>=2 的
/// transform 路径 color 段）。单线程口径（`KIMG_HEIF_PARALLEL=0` / 小图）与 [`yuv_to_rgba`]
/// 逐字节恒等。
pub(crate) fn yuv_to_rgba_par(
    y: &[u16],
    cb: &[u16],
    cr: &[u16],
    width: usize,
    height: usize,
    bit_depth: u8,
    matrix_coefficients: i32,
    full_range: bool,
) -> HeifResult<Vec<u8>> {
    let mut out = vec![0u8; width * height * 4];
    yuv_to_rgba_into_par(
        y,
        cb,
        cr,
        width,
        height,
        bit_depth,
        matrix_coefficients,
        full_range,
        &mut out,
    )?;
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

    /// 直写路径（`yuv_to_rgba_into`）≡ 返回路径（`yuv_to_rgba`）：随机 YUV 平面 ×
    /// 矩阵 {0(identity),2,1,9} × full_range {false,true} × 位深 {8,10} × 尺寸
    /// （含 1×1、奇偶宽高、8 对齐行），逐字节相等；dest 先填 0xFF 哨兵，确认直写
    /// 覆盖每个字节（不留旧值）。另验证 dest 过小时返回 Err。
    #[test]
    fn yuv_to_rgba_into_matches_return_path() {
        let mut rng = Rng(0xBAD1_C0FF_EE11_2BAD);
        let mut tried = 0usize;
        for &(w, h) in &[(1usize, 1usize), (2, 3), (8, 2), (9, 17), (17, 9), (32, 33)] {
            for &bd in &[8u8, 10u8] {
                for &matrix in &[0, 2, 1, 9] {
                    for &full in &[false, true] {
                        let cw = (w + 1) >> 1;
                        let ch = (h + 1) >> 1;
                        let max_sample = if bd == 10 { 1023u16 } else { 255u16 };
                        let y: Vec<u16> = rand_vec(&mut rng, w * h, max_sample);
                        let cb: Vec<u16> = rand_vec(&mut rng, cw * ch, max_sample);
                        let cr: Vec<u16> = rand_vec(&mut rng, cw * ch, max_sample);
                        let reference = yuv_to_rgba(&y, &cb, &cr, w, h, bd, matrix, full)
                            .expect("yuv_to_rgba");
                        let mut dest = vec![0xFFu8; reference.len()];
                        yuv_to_rgba_into(&y, &cb, &cr, w, h, bd, matrix, full, &mut dest)
                            .expect("yuv_to_rgba_into");
                        assert_eq!(
                            dest, reference,
                            "直写≠返回路径 (w={w} h={h} bd={bd} m={matrix} full={full})"
                        );
                        tried += 1;
                    }
                }
            }
        }
        assert!(tried > 0, "直写对拍未执行任何一轮");
        // dest 过小 → Err（不写）。
        let mut tiny = [0u8; 8];
        assert!(
            yuv_to_rgba_into(&[0u16; 4], &[0; 1], &[0; 1], 2, 2, 8, 2, false, &mut tiny).is_err(),
            "dest 过小应返回 Err"
        );
    }

    /// 行带并行 ≡ 单线程整图：随机平面 × 矩阵 {0,2,9} × full_range {false,true} × 位深
    /// {8,10} × 高 {8,16,33,64}（覆盖奇数、带数 < / > 线程数、8 倍数），
    /// `yuv_to_rgba_into_par`（正常环境真走行带并行）与 `yuv_to_rgba_into` 逐字节相等；
    /// 高位深/identity 一并覆盖。行数过小的尺寸另点验（落单线程也不失配）。
    #[test]
    fn parallel_rows_match_serial_whole() {
        let mut rng = Rng(0xBA11_5E1F_A1C0_0BC0);
        let mut tried = 0usize;
        for &(w, h) in &[
            (17usize, 8usize),
            (32, 16),
            (9, 33),
            (64, 64),
            (7, 9),
            (48, 21),
            (31, 2),
        ] {
            for &bd in &[8u8, 10u8] {
                for &matrix in &[0, 2, 9] {
                    for &full in &[false, true] {
                        let cw = (w + 1) >> 1;
                        let ch = (h + 1) >> 1;
                        let max_sample = if bd == 10 { 1023u16 } else { 255u16 };
                        let y: Vec<u16> = rand_vec(&mut rng, w * h, max_sample);
                        let cb: Vec<u16> = rand_vec(&mut rng, cw * ch, max_sample);
                        let cr: Vec<u16> = rand_vec(&mut rng, cw * ch, max_sample);
                        let mut serial = vec![0xFFu8; w * h * 4];
                        yuv_to_rgba_into(&y, &cb, &cr, w, h, bd, matrix, full, &mut serial)
                            .expect("yuv_to_rgba_into");
                        let mut par = vec![0xFFu8; w * h * 4];
                        yuv_to_rgba_into_par(&y, &cb, &cr, w, h, bd, matrix, full, &mut par)
                            .expect("yuv_to_rgba_into_par");
                        assert_eq!(
                            serial, par,
                            "并行≠串行整图 (w={w} h={h} bd={bd} m={matrix} full={full})"
                        );
                        let par_alloc = yuv_to_rgba_par(&y, &cb, &cr, w, h, bd, matrix, full)
                            .expect("yuv_to_rgba_par");
                        assert_eq!(
                            serial, par_alloc,
                            "并行分配版≠串行整图 (w={w} h={h} bd={bd} m={matrix} full={full})"
                        );
                        tried += 1;
                    }
                }
            }
        }
        assert!(tried > 0, "并行对拍未执行任何一轮");
    }

    /// `color_band` 行带切分与整图逐字节一致（环境无关，直接验带 API）：把整图按
    /// {1,2,3,5} 带切分（含奇数尾带/带数 <、> 线程数），每带写独立 `split_at_mut` 子切片，
    /// 拼接后与 `yuv_to_rgba_into` 相等；高位深/奇数带宽一并覆盖。
    #[test]
    fn color_band_partitions_match_whole() {
        let mut rng = Rng(0x9E11_5E1F_51A1_0D5A);
        let mut tried = 0usize;
        for &(w, h) in &[(8usize, 8usize), (17, 16), (9, 33), (32, 64), (6, 5)] {
            for &bd in &[8u8, 10u8] {
                for &matrix in &[0, 2, 9] {
                    for &full in &[false, true] {
                        let cw = (w + 1) >> 1;
                        let ch = (h + 1) >> 1;
                        let max_sample = if bd == 10 { 1023u16 } else { 255u16 };
                        let y: Vec<u16> = rand_vec(&mut rng, w * h, max_sample);
                        let cb: Vec<u16> = rand_vec(&mut rng, cw * ch, max_sample);
                        let cr: Vec<u16> = rand_vec(&mut rng, cw * ch, max_sample);
                        let mut whole = vec![0u8; w * h * 4];
                        yuv_to_rgba_into(&y, &cb, &cr, w, h, bd, matrix, full, &mut whole)
                            .expect("yuv_to_rgba_into");
                        for parts in [1usize, 2, 3, 5] {
                            if parts > h {
                                continue;
                            }
                            let chunk = h.div_ceil(parts);
                            let mut got = vec![0xFFu8; w * h * 4];
                            let mut base = 0usize;
                            let mut r0 = 0usize;
                            while r0 < h {
                                let r1 = (r0 + chunk).min(h);
                                let band_bytes = (r1 - r0) * w * 4;
                                let seg = &mut got[base..base + band_bytes];
                                color_band(&y, &cb, &cr, w, h, bd, matrix, full, seg, r0, r1, h)
                                    .expect("color_band");
                                base += band_bytes;
                                r0 = r1;
                            }
                            assert_eq!(
                                got, whole,
                                "color_band {parts} 带切分≠整图 (w={w} h={h} bd={bd} m={matrix} full={full})"
                            );
                            tried += 1;
                        }
                    }
                }
            }
        }
        assert!(tried > 0, "color_band 对拍未执行任何一轮");
    }
}


