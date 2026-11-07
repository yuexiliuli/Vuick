//! HEIF 显示变换（`transform.rs` `apply_rgba`）的 AVX2 向量内核——Step 1：irot 向量化。
//!
//! 字节恒等红线：旋转/镜像 = 双射置换（每输出像素从唯一输入像素拷贝、**零算术**），
//! 遍历顺序自由 ⇒ 块转置/行逆序对通用循环（C# `HeifCanvas` 语义）逐字节恒等，
//! 无近似/舍入/插值。
//!
//! - [`classify`]：由仿射线性部 (a,b,d,e) 判 8 形（带符号置换 = 正交矩阵）；非 8 形 → `None` → 通用回退。
//! - [`apply_simd`]：命中 {rot90, rot270, rot180} 且源矩形（输出矩形的 preimage）对齐在界时跑
//!   AVX2 内核；否则返回 `Ok(false)`（调用方走原通用循环）。identity/hflip/vflip/两镜像
//!   （IMG_0083 系列不命中）留待后续 Step，仍走通用循环。
//!
//! 叶子隔离（对标 `color_simd.rs` / `hevc/*_simd.rs`）：本模块是 `transform.rs`
//! `#![forbid(unsafe_code)]` 的全部 unsafe 下沉点；调用方先 `is_x86_feature_detected!("avx2")`
//! 运行时检出再 `unsafe` 调用。

#![allow(unsafe_code)]
#![cfg(target_arch = "x86_64")]

use std::arch::x86_64::*;

use super::transform::TransformMap;
use crate::codecs::heif::error::HeifResult;

/// 8 形（正方形二面体群 D4）判形结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Form {
    Identity,
    Rot90,
    Rot180,
    Rot270,
    Hflip,
    Vflip,
    Diag,
    Antidiag,
}

/// 源矩形（preimage）：输出矩形四角经 `R⁻¹(out − t)` 逆映射的包围盒。
/// 正交矩阵 ⇒ 轴对齐矩形的原像仍轴对齐，包围盒即精确原像。
#[derive(Debug, Clone, Copy)]
struct SrcRect {
    x0: usize,
    y0: usize,
    w: usize,
    h: usize,
}

/// 由仿射线性部 (a,b,d,e) 判 8 形（带符号置换/正交矩阵）。判据：两行单位且正交 ⇒ 两列亦单位。
pub(super) fn classify(a: i32, b: i32, d: i32, e: i32) -> Option<Form> {
    if !matches!(a, -1 | 0 | 1)
        || !matches!(b, -1 | 0 | 1)
        || !matches!(d, -1 | 0 | 1)
        || !matches!(e, -1 | 0 | 1)
    {
        return None;
    }
    // 正交性证分类：row0·row0=1、row1·row1=1、row0·row1=0 ⇒ 逐列亦单位 → 8 形之一。
    if a * a + b * b != 1 || d * d + e * e != 1 || a * d + b * e != 0 {
        return None;
    }
    match (a, b, d, e) {
        (1, 0, 0, 1) => Some(Form::Identity),
        (-1, 0, 0, 1) => Some(Form::Hflip),
        (1, 0, 0, -1) => Some(Form::Vflip),
        (0, 1, -1, 0) => Some(Form::Rot90),
        (0, -1, 1, 0) => Some(Form::Rot270),
        (-1, 0, 0, -1) => Some(Form::Rot180),
        (0, 1, 1, 0) => Some(Form::Diag),
        (0, -1, -1, 0) => Some(Form::Antidiag),
        _ => None,
    }
}

/// 生产块尺寸（K53 微基准 `bench_block_variants` 决策：8 vs 16）。分派优先 BLOCK，miss 回 8。
pub(super) const BLOCK: usize = 8;

/// dev 测量开关（仿 `KIMG_HEIF_PARALLEL` 姿势）：`KIMG_HEIF_TRANSFORM_SIMD=0` → 强制通用循环，
/// 供同二进制同窗 before/after 定性能；默认空/AUTO → 启用。一次读取缓存，不进热路径。
static FORCE_GENERIC: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
fn simd_enabled() -> bool {
    *FORCE_GENERIC
        .get_or_init(|| {
            std::env::var("KIMG_HEIF_TRANSFORM_SIMD")
                .map(|v| v != "0")
                .unwrap_or(true)
        })
}

/// Step-1 irot 并行规划：判形 → 源矩形 → 对齐 → 块尺寸，串/并行分派共用同一份
/// 命中判据（保证两条路径**同一命中集合**，并行只改执行顺序不改判据）。
struct SimdPlan {
    form: Form,
    rect: SrcRect,
    /// 块尺寸：rot90/270 = 8 或 16；rot180 = 8（仅列向对齐）。
    block: usize,
}

/// 由 map 判出 Step-1 命中规划（否则 `None` → 调用方走通用循环）。判据与既有
/// `apply_simd` 逐条一致：AVX2 检出 + classify 命中 irot + 源矩形在界 + 对齐。
fn plan_irot(map: &TransformMap, canvas_w: i32, canvas_h: i32) -> Option<SimdPlan> {
    if !simd_enabled() || !std::is_x86_feature_detected!("avx2") {
        return None;
    }
    let form = classify(map.a, map.b, map.d, map.e)?;
    // Step 1 只加速 irot（rot90/rot180/rot270）；identity/hflip/vflip/两镜像留通用（IMG_0083 不命中）。
    if !matches!(form, Form::Rot90 | Form::Rot180 | Form::Rot270) {
        return None;
    }
    let rect = source_rect(map, canvas_w, canvas_h)?;
    let block = match form {
        Form::Rot180 => {
            // 行逆序 + 行内反序：只需列向 8 对齐（行循环任意 h/y0，指针 loadu/storeu 未对齐安全）。
            if rect.x0 % 8 != 0 || rect.w % 8 != 0 {
                return None;
            }
            8
        }
        Form::Rot90 | Form::Rot270 => {
            // 块转置：源矩形原点与尺寸均需 8 对齐，否则通用回退（clap 非对齐边界）。
            if rect.x0 % 8 != 0 || rect.y0 % 8 != 0 || rect.w % 8 != 0 || rect.h % 8 != 0 {
                return None;
            }
            if BLOCK == 16
                && rect.x0 % 16 == 0
                && rect.y0 % 16 == 0
                && rect.w % 16 == 0
                && rect.h % 16 == 0
            {
                16
            } else {
                8
            }
        }
        _ => unreachable!(),
    };
    Some(SimdPlan { form, rect, block })
}

/// 串行执行一个已命中的规划（`apply_simd` 与 `apply_simd_par` 的「核少/块少落串行」共用）。
/// 逐字节恒等：与既有内核完全同指令序列，仅分派层收敛为规划。
#[target_feature(enable = "avx2")]
unsafe fn exec_serial(
    plan: &SimdPlan,
    src: *const u8,
    cstride: usize,
    dst: *mut u8,
    ow: usize,
    oh: usize,
) {
    unsafe {
        match plan.form {
            Form::Rot180 => rot180_kernel(src, cstride, plan.rect, dst, ow, oh),
            Form::Rot90 => rot90_kernel(src, cstride, plan.rect, dst, ow, oh, plan.block),
            Form::Rot270 => rot270_kernel(src, cstride, plan.rect, dst, ow, oh, plan.block),
            _ => unreachable!(),
        }
    }
}

/// Step-1 分派入口：判形 → 源矩形 → 对齐 → AVX2 内核。返回 `true` = 已写全输出；
/// `false` = 未触碰 `out`（调用方回通用循环）。调用前置（`transform.rs` 已验）：
/// `canvas.len() >= canvas_w*canvas_h*4`、`out.len() >= map.width*map.height*4`、尺寸 > 0。
pub(super) fn apply_simd(
    canvas: &[u8],
    canvas_w: i32,
    canvas_h: i32,
    map: &TransformMap,
    out: &mut [u8],
) -> HeifResult<bool> {
    let Some(plan) = plan_irot(map, canvas_w, canvas_h) else {
        return Ok(false);
    };
    let src = canvas.as_ptr();
    let dst = out.as_mut_ptr();
    let cstride = (canvas_w as usize) * 4; // 源行字节跨步
    let ow = map.width as usize;
    let oh = map.height as usize;
    // SAFETY: plan_irot 已证源矩形在界、对齐、尺寸与 out 缓冲自洽；AVX2 已运行时检出。
    unsafe {
        exec_serial(&plan, src, cstride, dst, ow, oh);
    }
    Ok(true)
}

/// SIMD 命中条件下的输出块行带并行分派（调用方已保证 `!force_serial() && tiles>=2`，
/// mod.rs 路由；此处不再复查，靠文档契约锁定）。
///
/// 块行带划分（每块=读输入 8/16×8/16、写输出 8/16 行×8/16 列，块间纯置换、输出区域
/// 不相交——rot90/270 的源列带 bx ↔ 输出行带，rot180 的源行带 row ↔ 输出行带）：
/// `std::thread::scope` 把输出按连续块行带均分给 worker，每 worker 写 `out` 的
/// `split_at_mut` 独立段（该带独占的连续行段，零重叠零洞），输入 `canvas` 只读共享。
/// 与串行 [`apply_simd`] 逐字节恒等（仅执行顺序按带切分；每块内核指令序列逐字相同，
/// 双射零算术无跨块共享写）。核/块数过少（<2 带）→ 落串行 [`exec_serial`]，仍字节恒等。
///
/// 返回 `Ok(true)` = SIMD 已写全输出（含落串行执行的情形）；`Ok(false)` = 未命中 → 
/// 调用方走既有通用循环（与 [`apply_simd`] 的 miss 语义一致）。
pub(super) fn apply_simd_par(
    canvas: &[u8],
    canvas_w: i32,
    canvas_h: i32,
    map: &TransformMap,
    out: &mut [u8],
) -> HeifResult<bool> {
    let Some(plan) = plan_irot(map, canvas_w, canvas_h) else {
        return Ok(false);
    };
    let src = canvas.as_ptr();
    let cstride = (canvas_w as usize) * 4; // 源行字节跨步
    let ow = map.width as usize;
    let oh = map.height as usize;
    // 输出块行带总数：rot90/270 按块（每块写 block 行）、rot180 按行（每行写 1 行）。
    let total = match plan.form {
        Form::Rot180 => plan.rect.h,
        _ => plan.rect.w / plan.block,
    };
    let avail = std::thread::available_parallelism()
        .map(|p| p.get())
        .unwrap_or(1);
    let threads = avail.min(total).max(1);
    if threads < 2 {
        // 块带过少（8～16px 小图）/ 单核：线程开销 > 收益 → 串行执行（逐字节恒等）。
        // SAFETY: 同 apply_simd 的 SAFETY——规划已证对齐在界，out 缓冲自洽。
        unsafe {
            exec_serial(&plan, src, cstride, out.as_mut_ptr(), ow, oh);
        }
        return Ok(true);
    }
    // irot 正交不变式：rot90/270 的源矩形宽 == 输出高（oh），rot180 的源矩形高 == oh
    // （含 clap 情形：clap 矩形经旋转/平移后 preimage 尺寸 === 输出尺寸）。此不变式保证
    // 各带输出行占满 `out`（无洞），`split_at_mut` 逐段恰好拼满 out.len()。防御：万一
    // 破坏（理论不可达）→ 落串行，绝不 panic 或留洞。
    let full_rows = match plan.form {
        Form::Rot180 => plan.rect.h,
        _ => plan.rect.w,
    };
    if full_rows != oh {
        unsafe {
            exec_serial(&plan, src, cstride, out.as_mut_ptr(), ow, oh);
        }
        return Ok(true);
    }
    let chunk = total.div_ceil(threads);
    // rot90/rot180 的输出行随带号递增而**递减**（带首行 = oh − u1·scale），rot270 递增——
    // 按「输出行升序」的顺序分割 `out`，使 `split_at_mut` 的连续段恰为该带独占行段。
    let descending = matches!(plan.form, Form::Rot90 | Form::Rot180);
    std::thread::scope(|scope| {
        let mut rest: &mut [u8] = out;
        if descending {
            let mut u1 = total;
            while u1 > 0 {
                let u0 = u1.saturating_sub(chunk);
                let band_rows = match plan.form {
                    Form::Rot180 => u1 - u0,
                    _ => (u1 - u0) * plan.block,
                };
                let band_bytes = band_rows * ow * 4;
                let (seg, tail) = rest.split_at_mut(band_bytes);
                rest = tail;
                let plan_ref = &plan;
                scope.spawn(move || {
                    // SAFETY: seg 是 out 上该带独占的连续行段，各 worker 带互不相交并
                    // 恰好拼满 out（无洞，见 full_rows 不变式）；canvas 只读共享；plan 已证
                    // 源矩形对齐在界；AVX2 已运行时检出；scope join 后才归还 out 借用。
                    // 原始指针在 worker 内由 &[u8] / &mut [u8]（均 Send）派生，跨线程安全。
                    // plan_ref 是 &SimdPlan（Copy，Send+Sync），逐 worker 复制借用。
                    unsafe {
                        exec_band(plan_ref, canvas.as_ptr(), cstride, seg.as_mut_ptr(), ow, oh, u0, u1);
                    }
                });
                u1 = u0;
            }
        } else {
            let mut u0 = 0usize;
            while u0 < total {
                let u1 = (u0 + chunk).min(total);
                let band_rows = match plan.form {
                    Form::Rot180 => u1 - u0,
                    _ => (u1 - u0) * plan.block,
                };
                let band_bytes = band_rows * ow * 4;
                let (seg, tail) = rest.split_at_mut(band_bytes);
                rest = tail;
                let plan_ref = &plan;
                scope.spawn(move || {
                    // SAFETY: 同 descending 分支。
                    unsafe {
                        exec_band(plan_ref, canvas.as_ptr(), cstride, seg.as_mut_ptr(), ow, oh, u0, u1);
                    }
                });
                u0 = u1;
            }
        }
    });
    Ok(true)
}

/// 执行一个块行带（带内块范围 [u0,u1) 输出段 `dst` = 该带首行）。
#[target_feature(enable = "avx2")]
unsafe fn exec_band(
    plan: &SimdPlan,
    src: *const u8,
    cstride: usize,
    dst: *mut u8,
    ow: usize,
    _oh: usize,
    u0: usize,
    u1: usize,
) {
    unsafe {
        match plan.form {
            Form::Rot180 => rot180_kernel_band(src, cstride, plan.rect, dst, ow, u0, u1),
            Form::Rot90 => {
                if plan.block == 16 {
                    rot90_kernel16_band(src, cstride, plan.rect, dst, ow, u0 * 16, u1 * 16);
                } else {
                    rot90_kernel8_band(src, cstride, plan.rect, dst, ow, u0 * 8, u1 * 8);
                }
            }
            Form::Rot270 => {
                if plan.block == 16 {
                    rot270_kernel16_band(src, cstride, plan.rect, dst, ow, u0 * 16, u1 * 16);
                } else {
                    rot270_kernel8_band(src, cstride, plan.rect, dst, ow, u0 * 8, u1 * 8);
                }
            }
            _ => unreachable!(),
        }
    }
}

/// 输出矩形四角逆映射的包围盒 = 源矩形（in = Rᵀ·(out − t)，Rᵀ = [[a,d],[b,e]]，t = (c,f)）。
/// 原像任一角出界 → `None`（通用循环含越界守卫接管）。全部 i64 计算防溢出（线性部已有 ±1 界）。
fn source_rect(map: &TransformMap, canvas_w: i32, canvas_h: i32) -> Option<SrcRect> {
    let (a, b, c) = (i64::from(map.a), i64::from(map.b), i64::from(map.c));
    let (d, e, f) = (i64::from(map.d), i64::from(map.e), i64::from(map.f));
    let (ow, oh) = (i64::from(map.width), i64::from(map.height));
    let (cw, ch) = (i64::from(canvas_w), i64::from(canvas_h));
    if ow <= 0 || oh <= 0 || cw <= 0 || ch <= 0 {
        return None;
    }
    let corners = [(0i64, 0i64), (ow - 1, 0), (0, oh - 1), (ow - 1, oh - 1)];
    let mut x0 = i64::MAX;
    let mut x1 = i64::MIN;
    let mut y0 = i64::MAX;
    let mut y1 = i64::MIN;
    for (ox, oy) in corners {
        let rx = a * ox + d * oy - (a * c + d * f);
        let ry = b * ox + e * oy - (b * c + e * f);
        x0 = x0.min(rx);
        x1 = x1.max(rx);
        y0 = y0.min(ry);
        y1 = y1.max(ry);
    }
    if x0 < 0 || y0 < 0 || x1 >= cw || y1 >= ch {
        return None;
    }
    Some(SrcRect {
        x0: x0 as usize,
        y0: y0 as usize,
        w: (x1 - x0 + 1) as usize,
        h: (y1 - y0 + 1) as usize,
    })
}

/// AVX2 8×8（32-bit 元素 = RGBA 像素）寄存器转置：`t[k][lane] = r[lane][k]`。
/// 函数体仅有 SIMD 内建（无指针运算）→ `#[target_feature]` 隐式 unsafe 上下文，无需显式块。
#[target_feature(enable = "avx2")]
unsafe fn transpose8x8(r: &[__m256i; 8]) -> [__m256i; 8] {
    let (r0, r1, r2, r3, r4, r5, r6, r7) = (r[0], r[1], r[2], r[3], r[4], r[5], r[6], r[7]);
    // 128-bit lane 内 u32 交错。
    let a0 = _mm256_unpacklo_epi32(r0, r1);
    let a1 = _mm256_unpackhi_epi32(r0, r1);
    let a2 = _mm256_unpacklo_epi32(r2, r3);
    let a3 = _mm256_unpackhi_epi32(r2, r3);
    let a4 = _mm256_unpacklo_epi32(r4, r5);
    let a5 = _mm256_unpackhi_epi32(r4, r5);
    let a6 = _mm256_unpacklo_epi32(r6, r7);
    let a7 = _mm256_unpackhi_epi32(r6, r7);
    // 128-bit lane 内 64-bit 交错。
    let b0 = _mm256_unpacklo_epi64(a0, a2);
    let b1 = _mm256_unpackhi_epi64(a0, a2);
    let b2 = _mm256_unpacklo_epi64(a1, a3);
    let b3 = _mm256_unpackhi_epi64(a1, a3);
    let b4 = _mm256_unpacklo_epi64(a4, a6);
    let b5 = _mm256_unpackhi_epi64(a4, a6);
    let b6 = _mm256_unpacklo_epi64(a5, a7);
    let b7 = _mm256_unpackhi_epi64(a5, a7);
    // 跨 128-bit lane 组结果（0x20 = [a.lo, b.lo]，0x31 = [a.hi, b.hi]）。
    let t0 = _mm256_permute2x128_si256(b0, b4, 0x20);
    let t1 = _mm256_permute2x128_si256(b1, b5, 0x20);
    let t2 = _mm256_permute2x128_si256(b2, b6, 0x20);
    let t3 = _mm256_permute2x128_si256(b3, b7, 0x20);
    let t4 = _mm256_permute2x128_si256(b0, b4, 0x31);
    let t5 = _mm256_permute2x128_si256(b1, b5, 0x31);
    let t6 = _mm256_permute2x128_si256(b2, b6, 0x31);
    let t7 = _mm256_permute2x128_si256(b3, b7, 0x31);
    [t0, t1, t2, t3, t4, t5, t6, t7]
}

/// 8-lane 逆序（`vpermd` with idx=[7..0]）。函数体仅有 SIMD 内建 → 隐式 unsafe 上下文。
#[target_feature(enable = "avx2")]
unsafe fn reverse8(v: __m256i) -> __m256i {
    let rev = _mm256_setr_epi32(7, 6, 5, 4, 3, 2, 1, 0);
    _mm256_permutevar8x32_epi32(v, rev)
}

/// rot90: S(y0+j, x0+i) → O(oh-1-i, j)。块内源行 8×8 → 转置 t_q（lane j = M[j][q]），
/// t_q 存到输出行 (oh-1-8bx-q)、列 [8by .. 8by+7]。
#[target_feature(enable = "avx2")]
unsafe fn rot90_kernel(
    src: *const u8,
    cstride: usize,
    r: SrcRect,
    dst: *mut u8,
    ow: usize,
    oh: usize,
    block: usize,
) {
    unsafe {
        if block == 16 {
            rot90_kernel16(src, cstride, r, dst, ow, oh);
        } else {
            rot90_kernel8(src, cstride, r, dst, ow, oh);
        }
    }
}

#[target_feature(enable = "avx2")]
unsafe fn rot90_kernel8(src: *const u8, cstride: usize, r: SrcRect, dst: *mut u8, ow: usize, oh: usize) {
    unsafe {
        debug_assert!(r.x0 % 8 == 0 && r.y0 % 8 == 0 && r.w % 8 == 0 && r.h % 8 == 0);
        let mut by = 0usize;
        while by < r.h {
            let bypix = by;
            let ybase = (r.y0 + by) * cstride;
            let mut bx = 0usize;
            while bx < r.w {
                let xbase = (r.x0 + bx) * 4;
                let mut rows = [std::mem::MaybeUninit::<__m256i>::uninit(); 8];
                for j in 0..8 {
                    let p = src.add(ybase + j * cstride + xbase);
                    rows[j].write(_mm256_loadu_si256(p as *const __m256i));
                }
                let rows: [__m256i; 8] = rows.map(|m| m.assume_init());
                let t = transpose8x8(&rows);
                for q in 0..8 {
                    let row = oh - 1 - (bx + q);
                    _mm256_storeu_si256(dst.add((row * ow + bypix) * 4) as *mut __m256i, t[q]);
                }
                bx += 8;
            }
            by += 8;
        }
    }
}

/// rot90 16×16 块 = 16 行 × 2 半（lo/hi）四次 8×8 转置（顶/底 × 低/高），每输出行两笔 32B 存。
/// 输出行 (oh-1-16bx-q) = [At_q(rows0-7), At2_q(rows8-15)]，q∈0..7；q∈8..15 用 Bt/Bt2。
#[target_feature(enable = "avx2")]
unsafe fn rot90_kernel16(src: *const u8, cstride: usize, r: SrcRect, dst: *mut u8, ow: usize, oh: usize) {
    unsafe {
        debug_assert!(r.x0 % 16 == 0 && r.y0 % 16 == 0 && r.w % 16 == 0 && r.h % 16 == 0);
        let mut by = 0usize;
        while by < r.h {
            let outcol = by;
            let ybase = (r.y0 + by) * cstride;
            let mut bx = 0usize;
            while bx < r.w {
                let xbase = (r.x0 + bx) * 4;
                let mut lo = [std::mem::MaybeUninit::<__m256i>::uninit(); 16];
                let mut hi = [std::mem::MaybeUninit::<__m256i>::uninit(); 16];
                for j in 0..16 {
                    let p = src.add(ybase + j * cstride + xbase);
                    lo[j].write(_mm256_loadu_si256(p as *const __m256i));
                    hi[j].write(_mm256_loadu_si256(p.add(32) as *const __m256i));
                }
                let lo: [__m256i; 16] = lo.map(|m| m.assume_init());
                let hi: [__m256i; 16] = hi.map(|m| m.assume_init());
                let at = transpose8x8(&lo[..8].try_into().unwrap_or_else(|_| unreachable!()));
                let at2 = transpose8x8(&lo[8..].try_into().unwrap_or_else(|_| unreachable!()));
                let bt = transpose8x8(&hi[..8].try_into().unwrap_or_else(|_| unreachable!()));
                let bt2 = transpose8x8(&hi[8..].try_into().unwrap_or_else(|_| unreachable!()));
                for q in 0..8 {
                    let row = oh - 1 - (bx + q);
                    let base = dst.add((row * ow + outcol) * 4);
                    _mm256_storeu_si256(base as *mut __m256i, at[q]);
                    _mm256_storeu_si256(base.add(32) as *mut __m256i, at2[q]);
                }
                for q in 0..8 {
                    let row = oh - 1 - (bx + 8 + q);
                    let base = dst.add((row * ow + outcol) * 4);
                    _mm256_storeu_si256(base as *mut __m256i, bt[q]);
                    _mm256_storeu_si256(base.add(32) as *mut __m256i, bt2[q]);
                }
                bx += 16;
            }
            by += 16;
        }
    }
}

/// rot270: S(y0+j, x0+i) → O(i, ow-1-j)。块转置 t_q 反序后存输出行 q、列 [ow-8by-8 .. ow-8by-1]。
#[target_feature(enable = "avx2")]
unsafe fn rot270_kernel(
    src: *const u8,
    cstride: usize,
    r: SrcRect,
    dst: *mut u8,
    ow: usize,
    oh: usize,
    block: usize,
) {
    unsafe {
        if block == 16 {
            rot270_kernel16(src, cstride, r, dst, ow, oh);
        } else {
            rot270_kernel8(src, cstride, r, dst, ow, oh);
        }
    }
}

#[target_feature(enable = "avx2")]
unsafe fn rot270_kernel8(src: *const u8, cstride: usize, r: SrcRect, dst: *mut u8, ow: usize, _oh: usize) {
    unsafe {
        debug_assert!(r.x0 % 8 == 0 && r.y0 % 8 == 0 && r.w % 8 == 0 && r.h % 8 == 0);
        let mut by = 0usize;
        while by < r.h {
            let ybase = (r.y0 + by) * cstride;
            let mut bx = 0usize;
            while bx < r.w {
                let xbase = (r.x0 + bx) * 4;
                let mut rows = [std::mem::MaybeUninit::<__m256i>::uninit(); 8];
                for j in 0..8 {
                    let p = src.add(ybase + j * cstride + xbase);
                    rows[j].write(_mm256_loadu_si256(p as *const __m256i));
                }
                let rows: [__m256i; 8] = rows.map(|m| m.assume_init());
                let t = transpose8x8(&rows);
                let outcol = ow - by - 8;
                for q in 0..8 {
                    let row = bx + q;
                    _mm256_storeu_si256(
                        dst.add((row * ow + outcol) * 4) as *mut __m256i,
                        reverse8(t[q]),
                    );
                }
                bx += 8;
            }
            by += 8;
        }
    }
}

/// rot270 16×16 块：输出行 (16bx+q)，列 [ow-16by-16 .. ow-16by-1] 反序；低 32B = at2/bt2 反序、
/// 高 32B = at/bt 反序（列 (ow-16by-1-j)=M[j][q]，j>7 属顶半反向、j<8 属底半反向）。
#[target_feature(enable = "avx2")]
unsafe fn rot270_kernel16(src: *const u8, cstride: usize, r: SrcRect, dst: *mut u8, ow: usize, _oh: usize) {
    unsafe {
        debug_assert!(r.x0 % 16 == 0 && r.y0 % 16 == 0 && r.w % 16 == 0 && r.h % 16 == 0);
        let mut by = 0usize;
        while by < r.h {
            let ybase = (r.y0 + by) * cstride;
            let mut bx = 0usize;
            while bx < r.w {
                let xbase = (r.x0 + bx) * 4;
                let mut lo = [std::mem::MaybeUninit::<__m256i>::uninit(); 16];
                let mut hi = [std::mem::MaybeUninit::<__m256i>::uninit(); 16];
                for j in 0..16 {
                    let p = src.add(ybase + j * cstride + xbase);
                    lo[j].write(_mm256_loadu_si256(p as *const __m256i));
                    hi[j].write(_mm256_loadu_si256(p.add(32) as *const __m256i));
                }
                let lo: [__m256i; 16] = lo.map(|m| m.assume_init());
                let hi: [__m256i; 16] = hi.map(|m| m.assume_init());
                let at = transpose8x8(&lo[..8].try_into().unwrap_or_else(|_| unreachable!()));
                let at2 = transpose8x8(&lo[8..].try_into().unwrap_or_else(|_| unreachable!()));
                let bt = transpose8x8(&hi[..8].try_into().unwrap_or_else(|_| unreachable!()));
                let bt2 = transpose8x8(&hi[8..].try_into().unwrap_or_else(|_| unreachable!()));
                let outcol_low = ow - by - 16;
                for q in 0..8 {
                    let row = bx + q;
                    let base = dst.add((row * ow + outcol_low) * 4);
                    _mm256_storeu_si256(base as *mut __m256i, reverse8(at2[q]));
                    _mm256_storeu_si256(base.add(32) as *mut __m256i, reverse8(at[q]));
                }
                for q in 0..8 {
                    let row = bx + 8 + q;
                    let base = dst.add((row * ow + outcol_low) * 4);
                    _mm256_storeu_si256(base as *mut __m256i, reverse8(bt2[q]));
                    _mm256_storeu_si256(base.add(32) as *mut __m256i, reverse8(bt[q]));
                }
                bx += 16;
            }
            by += 16;
        }
    }
}

/// rot180: 每源行行内反序（8 像素块）写对应输出行，行序整体反（净 180°）。
/// 输出行 (oh-1-row)、列 [ow-(x0+cx)-8 .. ow-(x0+cx)-1] 反序。
#[target_feature(enable = "avx2")]
unsafe fn rot180_kernel(src: *const u8, cstride: usize, r: SrcRect, dst: *mut u8, ow: usize, oh: usize) {
    unsafe {
        debug_assert!(r.x0 % 8 == 0 && r.w % 8 == 0);
        let mut row = 0usize;
        while row < r.h {
            let srcrow = (r.y0 + row) * cstride;
            let outrow = oh - 1 - row;
            let mut cx = 0usize;
            while cx < r.w {
                let p = src.add(srcrow + (r.x0 + cx) * 4);
                let v = _mm256_loadu_si256(p as *const __m256i);
                // 输出列 = ow-1-cx-k（rect-局部 cx；全局 x0 平移已吸收进 ox=-x+c 的 c 中，
                // 勿再减 x0，否则 clap+rot180 平移情形下 ow-(x0+cx)-8 下溢）。
                let outcol = ow - cx - 8;
                _mm256_storeu_si256(
                    dst.add((outrow * ow + outcol) * 4) as *mut __m256i,
                    reverse8(v),
                );
                cx += 8;
            }
            row += 1;
        }
    }
}

// ---- 输出块行带内核（并行分派用）----
//
// 字节恒等：与上方串行内核指令序列逐字相同（同 load/transpose/reverse/store 内建 +
// 同算术），仅把「输出绝对行」换成「带内相对行」——dst 指向该带首行（带首行在 `out`
// 中的位置见 `apply_simd_par` 的 split 顺序），rel = 绝对行 − 带首行。输出带零重叠零洞
// （块带互不相交、并成整图），双射零算术 ⇒ 逐字节恒等。

/// rot90 8×8 块行带内核：处理源列带 [bx0, bx1)（像素，8 步进）的全部 by。带内相对行
/// `rel = bx1-1-bx-q`（bx1 为该带末端边界，行随 bx 反序）。dst = 输出带首行段。
#[target_feature(enable = "avx2")]
unsafe fn rot90_kernel8_band(
    src: *const u8,
    cstride: usize,
    r: SrcRect,
    dst: *mut u8,
    ow: usize,
    bx0: usize,
    bx1: usize,
) {
    unsafe {
        debug_assert!(
            r.x0 % 8 == 0
                && r.y0 % 8 == 0
                && r.w % 8 == 0
                && r.h % 8 == 0
                && bx0 % 8 == 0
                && bx1 % 8 == 0
                && bx0 < bx1
                && bx1 <= r.w
        );
        let mut by = 0usize;
        while by < r.h {
            let bypix = by;
            let ybase = (r.y0 + by) * cstride;
            let mut bx = bx0;
            while bx < bx1 {
                let xbase = (r.x0 + bx) * 4;
                let mut rows = [std::mem::MaybeUninit::<__m256i>::uninit(); 8];
                for j in 0..8 {
                    let p = src.add(ybase + j * cstride + xbase);
                    rows[j].write(_mm256_loadu_si256(p as *const __m256i));
                }
                let rows: [__m256i; 8] = rows.map(|m| m.assume_init());
                let t = transpose8x8(&rows);
                let rel_base = bx1 - 1 - bx;
                for q in 0..8 {
                    _mm256_storeu_si256(
                        dst.add(((rel_base - q) * ow + bypix) * 4) as *mut __m256i,
                        t[q],
                    );
                }
                bx += 8;
            }
            by += 8;
        }
    }
}

/// rot90 16×16 块行带内核（同 [`rot90_kernel16`]，输出行反序在带内：rel = bx1-1-bx-q）。
#[target_feature(enable = "avx2")]
unsafe fn rot90_kernel16_band(
    src: *const u8,
    cstride: usize,
    r: SrcRect,
    dst: *mut u8,
    ow: usize,
    bx0: usize,
    bx1: usize,
) {
    unsafe {
        debug_assert!(
            r.x0 % 16 == 0
                && r.y0 % 16 == 0
                && r.w % 16 == 0
                && r.h % 16 == 0
                && bx0 % 16 == 0
                && bx1 % 16 == 0
                && bx0 < bx1
                && bx1 <= r.w
        );
        let mut by = 0usize;
        while by < r.h {
            let outcol = by;
            let ybase = (r.y0 + by) * cstride;
            let mut bx = bx0;
            while bx < bx1 {
                let xbase = (r.x0 + bx) * 4;
                let mut lo = [std::mem::MaybeUninit::<__m256i>::uninit(); 16];
                let mut hi = [std::mem::MaybeUninit::<__m256i>::uninit(); 16];
                for j in 0..16 {
                    let p = src.add(ybase + j * cstride + xbase);
                    lo[j].write(_mm256_loadu_si256(p as *const __m256i));
                    hi[j].write(_mm256_loadu_si256(p.add(32) as *const __m256i));
                }
                let lo: [__m256i; 16] = lo.map(|m| m.assume_init());
                let hi: [__m256i; 16] = hi.map(|m| m.assume_init());
                let at = transpose8x8(&lo[..8].try_into().unwrap_or_else(|_| unreachable!()));
                let at2 = transpose8x8(&lo[8..].try_into().unwrap_or_else(|_| unreachable!()));
                let bt = transpose8x8(&hi[..8].try_into().unwrap_or_else(|_| unreachable!()));
                let bt2 = transpose8x8(&hi[8..].try_into().unwrap_or_else(|_| unreachable!()));
                for q in 0..8 {
                    let rel = bx1 - 1 - bx - q;
                    let base = dst.add((rel * ow + outcol) * 4);
                    _mm256_storeu_si256(base as *mut __m256i, at[q]);
                    _mm256_storeu_si256(base.add(32) as *mut __m256i, at2[q]);
                }
                for q in 0..8 {
                    let rel = bx1 - 1 - bx - 8 - q;
                    let base = dst.add((rel * ow + outcol) * 4);
                    _mm256_storeu_si256(base as *mut __m256i, bt[q]);
                    _mm256_storeu_si256(base.add(32) as *mut __m256i, bt2[q]);
                }
                bx += 16;
            }
            by += 16;
        }
    }
}

/// rot270 8×8 块行带内核：源列带内输出行随 bx 递增（rel = bx-bx0+q）。
#[target_feature(enable = "avx2")]
unsafe fn rot270_kernel8_band(
    src: *const u8,
    cstride: usize,
    r: SrcRect,
    dst: *mut u8,
    ow: usize,
    bx0: usize,
    bx1: usize,
) {
    unsafe {
        debug_assert!(
            r.x0 % 8 == 0
                && r.y0 % 8 == 0
                && r.w % 8 == 0
                && r.h % 8 == 0
                && bx0 % 8 == 0
                && bx1 % 8 == 0
                && bx0 < bx1
                && bx1 <= r.w
        );
        let mut by = 0usize;
        while by < r.h {
            let ybase = (r.y0 + by) * cstride;
            let mut bx = bx0;
            while bx < bx1 {
                let xbase = (r.x0 + bx) * 4;
                let mut rows = [std::mem::MaybeUninit::<__m256i>::uninit(); 8];
                for j in 0..8 {
                    let p = src.add(ybase + j * cstride + xbase);
                    rows[j].write(_mm256_loadu_si256(p as *const __m256i));
                }
                let rows: [__m256i; 8] = rows.map(|m| m.assume_init());
                let t = transpose8x8(&rows);
                let outcol = ow - by - 8;
                let rel_base = bx - bx0;
                for q in 0..8 {
                    _mm256_storeu_si256(
                        dst.add(((rel_base + q) * ow + outcol) * 4) as *mut __m256i,
                        reverse8(t[q]),
                    );
                }
                bx += 8;
            }
            by += 8;
        }
    }
}

/// rot270 16×16 块行带内核（同 [`rot270_kernel16`]，带内 rel = (bx-bx0)+q / +8+q）。
#[target_feature(enable = "avx2")]
unsafe fn rot270_kernel16_band(
    src: *const u8,
    cstride: usize,
    r: SrcRect,
    dst: *mut u8,
    ow: usize,
    bx0: usize,
    bx1: usize,
) {
    unsafe {
        debug_assert!(
            r.x0 % 16 == 0
                && r.y0 % 16 == 0
                && r.w % 16 == 0
                && r.h % 16 == 0
                && bx0 % 16 == 0
                && bx1 % 16 == 0
                && bx0 < bx1
                && bx1 <= r.w
        );
        let mut by = 0usize;
        while by < r.h {
            let ybase = (r.y0 + by) * cstride;
            let mut bx = bx0;
            while bx < bx1 {
                let xbase = (r.x0 + bx) * 4;
                let mut lo = [std::mem::MaybeUninit::<__m256i>::uninit(); 16];
                let mut hi = [std::mem::MaybeUninit::<__m256i>::uninit(); 16];
                for j in 0..16 {
                    let p = src.add(ybase + j * cstride + xbase);
                    lo[j].write(_mm256_loadu_si256(p as *const __m256i));
                    hi[j].write(_mm256_loadu_si256(p.add(32) as *const __m256i));
                }
                let lo: [__m256i; 16] = lo.map(|m| m.assume_init());
                let hi: [__m256i; 16] = hi.map(|m| m.assume_init());
                let at = transpose8x8(&lo[..8].try_into().unwrap_or_else(|_| unreachable!()));
                let at2 = transpose8x8(&lo[8..].try_into().unwrap_or_else(|_| unreachable!()));
                let bt = transpose8x8(&hi[..8].try_into().unwrap_or_else(|_| unreachable!()));
                let bt2 = transpose8x8(&hi[8..].try_into().unwrap_or_else(|_| unreachable!()));
                let outcol_low = ow - by - 16;
                for q in 0..8 {
                    let rel = (bx - bx0) + q;
                    let base = dst.add((rel * ow + outcol_low) * 4);
                    _mm256_storeu_si256(base as *mut __m256i, reverse8(at2[q]));
                    _mm256_storeu_si256(base.add(32) as *mut __m256i, reverse8(at[q]));
                }
                for q in 0..8 {
                    let rel = (bx - bx0) + 8 + q;
                    let base = dst.add((rel * ow + outcol_low) * 4);
                    _mm256_storeu_si256(base as *mut __m256i, reverse8(bt2[q]));
                    _mm256_storeu_si256(base.add(32) as *mut __m256i, reverse8(bt[q]));
                }
                bx += 16;
            }
            by += 16;
        }
    }
}

/// rot180 块行带内核：处理源行带 [row0, row1)（= 输出行带，每行 1 输出行）。带内相对行
/// `rel = row1-1-row`（行随 row 反序）。
#[target_feature(enable = "avx2")]
unsafe fn rot180_kernel_band(
    src: *const u8,
    cstride: usize,
    r: SrcRect,
    dst: *mut u8,
    ow: usize,
    row0: usize,
    row1: usize,
) {
    unsafe {
        debug_assert!(r.x0 % 8 == 0 && r.w % 8 == 0 && row0 < row1 && row1 <= r.h);
        let mut row = row0;
        while row < row1 {
            let srcrow = (r.y0 + row) * cstride;
            let rel = row1 - 1 - row;
            let mut cx = 0usize;
            while cx < r.w {
                let p = src.add(srcrow + (r.x0 + cx) * 4);
                let v = _mm256_loadu_si256(p as *const __m256i);
                // 输出列 = ow-1-cx-k（同 rot180_kernel 注释：勿再减 x0）。
                let outcol = ow - cx - 8;
                _mm256_storeu_si256(
                    dst.add((rel * ow + outcol) * 4) as *mut __m256i,
                    reverse8(v),
                );
                cx += 8;
            }
            row += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::transform::{
        apply_rgba, apply_rgba_generic, apply_rgba_par, compose,
    };
    use crate::codecs::heif::container::{PropClap, PropImir, PropIrot, Transform};

    /// xorshift64* 确定性 PRNG（跨平台稳定，无外部依赖）。
    struct Rng(u64);
    impl Rng {
        fn new(seed: u64) -> Self {
            Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1))
        }
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
        fn fill(&mut self, buf: &mut [u8]) {
            for chunk in buf.chunks_mut(8) {
                let w = self.next().to_le_bytes();
                for (i, b) in chunk.iter_mut().enumerate() {
                    *b = w[i];
                }
            }
        }
    }

    /// classify 全 8 形 + 非 8 形回退。
    #[test]
    fn classify_all_forms() {
        let cases = [
            ((1, 0, 0, 1), Some(Form::Identity)),
            ((-1, 0, 0, 1), Some(Form::Hflip)),
            ((1, 0, 0, -1), Some(Form::Vflip)),
            ((0, 1, -1, 0), Some(Form::Rot90)),
            ((0, -1, 1, 0), Some(Form::Rot270)),
            ((-1, 0, 0, -1), Some(Form::Rot180)),
            ((0, 1, 1, 0), Some(Form::Diag)),
            ((0, -1, -1, 0), Some(Form::Antidiag)),
        ];
        for ((a, b, d, e), want) in cases {
            assert_eq!(classify(a, b, d, e), want, "({a},{b},{d},{e}) 应判 {want:?}");
        }
        // 非 8 形（含范数/正交/越界值）→ None。
        for (a, b, d, e) in [
            (1, 1, 0, 0),
            (0, 2, 0, 1),
            (2, 0, 0, 1),
            (0, 0, 0, 0),
            (1, 0, 0, 0),
            (0, 0, 1, 0),
            (1, 0, 1, 0),
            (-1, -1, 0, 0),
            (1, 0, 0, 2),
        ] {
            assert_eq!(classify(a, b, d, e), None, "({a},{b},{d},{e}) 应判 None");
        }
    }

    /// 8×8 转置原语 ≡ 标量转置（随机 RGBA 块逐像素对拍）。
    #[test]
    fn transpose8x8_matches_scalar() {
        if !std::is_x86_feature_detected!("avx2") {
            eprintln!("SKIP: 无 AVX2");
            return;
        }
        let mut rng = Rng::new(0x77C1);
        let mut src = vec![0u8; 8 * 8 * 4];
        rng.fill(&mut src);
        // 标量转置：dst[j][i] = src_row_i 的像素 j（t[k] lane j = r[j] lane k）。
        let mut want = vec![0u8; 8 * 8 * 4];
        for i in 0..8 {
            for j in 0..8 {
                want[(j * 8 + i) * 4..(j * 8 + i) * 4 + 4]
                    .copy_from_slice(&src[(i * 8 + j) * 4..(i * 8 + j) * 4 + 4]);
            }
        }
        let mut rows = [std::mem::MaybeUninit::<__m256i>::uninit(); 8];
        for j in 0..8 {
            rows[j].write(unsafe {
                _mm256_loadu_si256(src.as_ptr().add(j * 32) as *const __m256i)
            });
        }
        let rows: [__m256i; 8] = rows.map(|m| unsafe { m.assume_init() });
        let t = unsafe { transpose8x8(&rows) };
        let mut got = vec![0u8; 8 * 8 * 4];
        for q in 0..8 {
            unsafe {
                _mm256_storeu_si256(got.as_mut_ptr().add(q * 32) as *mut __m256i, t[q]);
            }
        }
        assert_eq!(got, want, "transpose8x8 应与标量转置逐字节一致");
    }

    /// 构造一组合法变换，跑分派 (`apply_rgba`) 与强制通用 (`apply_rgba_generic`) 逐字节对拍。
    fn parity_case(
        rng: &mut Rng,
        cw: i32,
        ch: i32,
        transforms: &[Transform],
        label: &str,
    ) {
        let mut canvas = vec![0u8; (cw * ch * 4) as usize];
        rng.fill(&mut canvas);
        let map = compose(cw, ch, transforms).expect("compose");
        let (ow, oh) = (map.width as usize, map.height as usize);
        let mut a = vec![0xffu8; ow * oh * 4];
        let mut b = vec![0xffu8; ow * oh * 4];
        apply_rgba(&canvas, cw, ch, &map, &mut a).expect("dispatch");
        apply_rgba_generic(&canvas, cw, ch, &map, &mut b).expect("generic");
        assert_eq!(a.len(), b.len(), "{label}: 输出尺寸");
        for (i, (x, y)) in a.iter().zip(b.iter()).enumerate() {
            assert_eq!(x, y, "{label}: 首差 @ byte {i} (x={}, y={})", (i / 4) % ow, i / 4 / ow);
        }
    }

    fn irot(a: i32) -> Transform {
        Transform { ty: PropIrot, a, b: 0, clap: None }
    }
    fn imir(a: i32) -> Transform {
        Transform { ty: PropImir, a, b: 0, clap: None }
    }
    fn clap(c: [i64; 8]) -> Transform {
        Transform { ty: PropClap, a: 0, b: 0, clap: Some(c) }
    }

    /// 分派 ≡ 通用：覆盖 SIMD 命中（8 对齐 irot）、SIMD 回退（奇数尺寸/非 8 形/越界 clap），
    /// 以及含平移的 clap+irot 8 对齐（真验证 SIMD 的平移路径）。
    #[test]
    fn dispatch_matches_generic_randomised() {
        if !std::is_x86_feature_detected!("avx2") {
            eprintln!("SKIP: 无 AVX2");
            return;
        }
        let mut rng = Rng::new(0xFEED_0083);
        // 8 对齐画布（SIMD 命中）+ irot/imir 系列 + clap 平移。
        for &(cw, ch) in &[(8i32, 8i32), (16, 16), (24, 8), (8, 24), (32, 32), (4032, 3024)] {
            for &t in &[1i32, 2, 3] {
                parity_case(&mut rng, cw, ch, &[irot(t)], &format!("irot{t} {cw}x{ch}"));
            }
            for &t in &[0i32, 1] {
                parity_case(&mut rng, cw, ch, &[imir(t)], &format!("imir{t} {cw}x{ch}"));
            }
            parity_case(&mut rng, cw, ch, &[], &format!("identity {cw}x{ch}"));
            // 镜像合成（Diag/Antidiag 等非 Step-1 形式）回退通用。
            parity_case(&mut rng, cw, ch, &[irot(1), imir(1)], &format!("rot90+imir1 {cw}x{ch}"));
            parity_case(&mut rng, cw, ch, &[irot(3), imir(1)], &format!("rot270+imir1 {cw}x{ch}"));
            parity_case(&mut rng, cw, ch, &[imir(1), irot(1)], &format!("imir1+rot90 {cw}x{ch}"));
        }
        // clap + irot：8 对齐平移 preimage（SIMD 命中）+ 奇数/非对齐 clap（回退）。
        // clap[8]: cw=8,ch=8,xoff,yoff...
        // clap_rectangle 语义：w,h,xoffN/xoffD,yoffN,yoffD；x0=(cw-w)/2+xoff,y0=(ch-h)/2+yoff。
        // 24×24 画布中挖 8×8 于中心 → (8,8)。
        let centered8 = clap([8, 1, 8, 1, 0, 1, 0, 1]);
        for &t in &[1i32, 2, 3] {
            parity_case(&mut rng, 24, 24, &[centered8.clone(), irot(t)], &format!("clap8+irot{t}"));
        }
        // 非 8 对齐 clap（preimage 原点/尺寸非 8 倍数）→ 通用回退仍逐字节等。
        let odd_clap = clap([5, 1, 5, 1, 0, 1, 0, 1]); // 24 中挖 5×5 offset (1,1)?? 中心化 → (9,9)
        parity_case(&mut rng, 24, 32, &[odd_clap.clone(), irot(1)], "clap5+irot1");
        // 奇数画布 irot → 回退。
        parity_case(&mut rng, 7, 9, &[irot(1)], "irot1 7x9");
        parity_case(&mut rng, 9, 7, &[irot(3)], "irot3 9x7");
        parity_case(&mut rng, 15, 17, &[irot(2)], "irot2 15x17");
    }

    /// 直接调 SIMD 入口：8 对齐 irot 必须返回 true 且 ≡ 通用；非 8 对齐/非 Step-1 形式必须返回 false。
    #[test]
    fn apply_simd_hit_and_miss() {
        if !std::is_x86_feature_detected!("avx2") {
            eprintln!("SKIP: 无 AVX2");
            return;
        }
        let canvas = vec![0x3Cu8; 16 * 16 * 4];
        let map90 = compose(16, 16, &[irot(1)]).unwrap();
        let mut out = vec![0u8; 16 * 16 * 4];
        assert!(
            apply_simd(&canvas, 16, 16, &map90, &mut out).unwrap(),
            "8 对齐 rot90 应命中 SIMD"
        );
        let mut want = vec![0u8; 16 * 16 * 4];
        apply_rgba_generic(&canvas, 16, 16, &map90, &mut want).unwrap();
        assert_eq!(out, want, "命中路径输出必须 ≡ 通用");

        // 非 Step-1 形式（hflip）与奇数（7×9 rot90）→ false。
        let map_hf = compose(16, 16, &[imir(0)]).unwrap();
        let mut out2 = vec![0u8; 16 * 16 * 4];
        assert!(
            !apply_simd(&canvas, 16, 16, &map_hf, &mut out2).unwrap(),
            "hflip 不在 Step-1 范围，应回退"
        );
        let map_odd = compose(7, 9, &[irot(1)]).unwrap();
        let mut out3 = vec![0u8; 7 * 9 * 4];
        assert!(
            !apply_simd(&canvas, 7, 9, &map_odd, &mut out3).unwrap(),
            "奇数尺寸应回退"
        );
    }

    /// 并行分派 ≡ 串行逐字节对拍（块行带并行红线）：**命中（8/16 对齐 irot，含 clap 平移）时
    /// 并行结果与串行 `apply_rgba` 完全一致；miss（奇数 / 非 Step-1 形 / 非对齐 clap）时
    /// 并行入口回退串行，结果亦一致**。覆盖 irot90/270/180 × 8/16/24/32 对齐尺寸 × 奇数
    /// 回退 × clap 平移；同时验证 `tiles=1`（门控 → 恒串行）与 `tiles=4`（并行）双双 ≡ 串行。
    #[test]
    fn parallel_parity_irot_all_forms() {
        if !std::is_x86_feature_detected!("avx2") {
            eprintln!("SKIP: 无 AVX2");
            return;
        }
        let mut rng = Rng::new(0x41A0);
        // 对齐画布 + irot 三形（8 对齐 → SIMD 命中；块带在多核机上 ≥2 带即真并行）。
        for &(cw, ch) in &[(8i32, 8i32), (16, 16), (24, 8), (8, 24), (32, 32), (64, 48)] {
            for &t in &[1i32, 2, 3] {
                parallel_parity_case(&mut rng, cw, ch, &[irot(t)], &format!("irot{t} {cw}x{ch}"));
            }
        }
        // clap 8 对齐平移（SIMD 命中）：24×24 中心挖 8×8（只 1 带 → 落串行执行）与
        // 64×64 中心挖 32×32（→ 4+ 带真并行）。
        let centered8 = clap([8, 1, 8, 1, 0, 1, 0, 1]);
        let centered32 = clap([32, 1, 32, 1, 0, 1, 0, 1]);
        for &t in &[1i32, 2, 3] {
            parallel_parity_case(&mut rng, 24, 24, &[centered8.clone(), irot(t)], &format!("clap8+irot{t}"));
            parallel_parity_case(&mut rng, 64, 64, &[centered32.clone(), irot(t)], &format!("clap32+irot{t}"));
        }
        // 奇数 / 非对齐 clap → 并行入口回退串行，仍字节恒等。
        parallel_parity_case(&mut rng, 7, 9, &[irot(1)], "irot1 7x9");
        parallel_parity_case(&mut rng, 15, 17, &[irot(2)], "irot2 15x17");
        let odd_clap = clap([5, 1, 5, 1, 0, 1, 0, 1]);
        parallel_parity_case(&mut rng, 24, 32, &[odd_clap.clone(), irot(1)], "clap5+irot1");
    }

    /// `apply_rgba_par`（tiles=4 并行 / tiles=1 恒串行）双口径 ≡ `apply_rgba` 串行。
    fn parallel_parity_case(rng: &mut Rng, cw: i32, ch: i32, transforms: &[Transform], label: &str) {
        let mut canvas = vec![0u8; (cw * ch * 4) as usize];
        rng.fill(&mut canvas);
        let map = compose(cw, ch, transforms).expect("compose");
        let (ow, oh) = (map.width as usize, map.height as usize);
        let mut ser = vec![0xabu8; ow * oh * 4];
        apply_rgba(&canvas, cw, ch, &map, &mut ser).expect("serial");
        let mut par4 = vec![0xabu8; ow * oh * 4];
        apply_rgba_par(&canvas, cw, ch, &map, &mut par4, 4).expect("par tiles=4");
        assert_eq!(ser, par4, "{label}: tiles=4 并行 ≠ 串行");
        let mut par1 = vec![0xabu8; ow * oh * 4];
        apply_rgba_par(&canvas, cw, ch, &map, &mut par1, 1).expect("par tiles=1");
        assert_eq!(ser, par1, "{label}: tiles=1（门控串行）≠ 串行");
    }

    /// SIMD 并行入口 hit/miss：8 对齐 irot → `Ok(true)` 且输出 ≡ 通用；非 Step-1 形（hflip）、
    /// 奇数尺寸 → `Ok(false)`（调用方回通用串行）。小图（仅 1 带）亦 `Ok(true)`（落串行执行）。
    #[test]
    fn apply_simd_par_hit_and_miss() {
        if !std::is_x86_feature_detected!("avx2") {
            eprintln!("SKIP: 无 AVX2");
            return;
        }
        let canvas = vec![0x3Cu8; 16 * 16 * 4];
        let map90 = compose(16, 16, &[irot(1)]).unwrap();
        let mut out = vec![0u8; 16 * 16 * 4];
        assert!(
            apply_simd_par(&canvas, 16, 16, &map90, &mut out).unwrap(),
            "8 对齐 rot90 应命中（并行或落串行执行）"
        );
        let mut want = vec![0u8; 16 * 16 * 4];
        apply_rgba_generic(&canvas, 16, 16, &map90, &mut want).unwrap();
        assert_eq!(out, want, "并行命中输出必须 ≡ 通用");

        // 非 Step-1 形式（hflip）与奇数（7×9 rot90）→ false（回退通用串行）。
        let map_hf = compose(16, 16, &[imir(0)]).unwrap();
        let mut out2 = vec![0u8; 16 * 16 * 4];
        assert!(
            !apply_simd_par(&canvas, 16, 16, &map_hf, &mut out2).unwrap(),
            "hflip 不在 Step-1 范围，应回退"
        );
        let map_odd = compose(7, 9, &[irot(1)]).unwrap();
        let mut out3 = vec![0u8; 7 * 9 * 4];
        assert!(
            !apply_simd_par(&canvas, 7, 9, &map_odd, &mut out3).unwrap(),
            "奇数尺寸应回退"
        );
    }

    /// IMG_0083 真实 map 核对（README §5 SKIP 约定）：语料缺 → SKIP 即常态。
    /// 验证 compose 出的 map 判形 = Rot90 且 preimage 8 对齐在界（= 命中 SIMD 的适配条件）。
    #[test]
    fn img_0083_map_classifies_rot90_aligned() {
        let corpus =
            PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../Benchmarks/corpus/heif/IMG_0083.HEIC"));
        if !corpus.exists() {
            eprintln!("SKIP: missing {corpus:?}");
            return;
        }
        let data = std::fs::read(&corpus).expect("read corpus");
        let c = crate::codecs::heif::container::HeifContainer::parse(&data).expect("parse");
        let map = compose(c.canvas_width, c.canvas_height, &c.transforms).expect("compose");
        let form = classify(map.a, map.b, map.d, map.e).expect("必须命中 8 形");
        eprintln!(
            "IMG_0083: canvas={}x{} map=({},{},{},{},{},{}) form={:?} out={}x{}",
            c.canvas_width,
            c.canvas_height,
            map.a,
            map.b,
            map.c,
            map.d,
            map.e,
            map.f,
            form,
            map.width,
            map.height
        );
        assert_eq!(form, Form::Rot270, "IMG_0083 实测 map=(0,-1,3023,1,0,0) 应判 Rot270");
        let rect = source_rect(&map, c.canvas_width, c.canvas_height).expect("preimage 应在界");
        assert_eq!((rect.x0, rect.y0), (0, 0), "纯 irot 无 clap → 原点 0");
        assert!(
            rect.w % 8 == 0 && rect.h % 8 == 0 && rect.x0 % 8 == 0 && rect.y0 % 8 == 0,
            "IMG_0083 preimage 应 8 对齐命中 SIMD（w={} h={}）",
            rect.w,
            rect.h
        );
        eprintln!("IMG_0083 preimage: x0={} y0={} w={} h={}（SIMD 命中）", rect.x0, rect.y0, rect.w, rect.h);
    }

    // ---- 性能探针（release: cargo test -p kimg_heif --release --lib transform_simd -- --ignored）----

    fn median_ms(rounds: &[f64]) -> f64 {
        let mut v = rounds.to_vec();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v[v.len() / 2]
    }

    /// 块尺寸 8 vs 16（K53）：IMG_0083 同尺寸画布（4032×3024）rot90/270 direct apply，
    /// 通用 / 8 块 / 16 块 同窗交错多轮中位，输出决策行。`BLOCK` 默认取 8（基于本探针结论）。
    #[test]
    #[ignore]
    fn bench_block_variants() {
        if !std::is_x86_feature_detected!("avx2") {
            eprintln!("SKIP: 无 AVX2");
            return;
        }
        let (cw, ch) = (4032i32, 3024i32);
        let mut canvas = vec![0u8; (cw * ch * 4) as usize];
        let mut rng = Rng::new(0xB10C);
        rng.fill(&mut canvas);
        let map90 = compose(cw, ch, &[irot(1)]).unwrap();
        let map270 = compose(cw, ch, &[irot(3)]).unwrap();
        let (ow, oh) = (map90.width as usize, map90.height as usize);
        let rect90 = source_rect(&map90, cw, ch).unwrap();
        let rect270 = source_rect(&map270, cw, ch).unwrap();
        let cstride = (cw as usize) * 4;

        // 正确性预检：8/16 块输出均 ≡ 通用（防「快但坏」）。
        let mut want = vec![0u8; ow * oh * 4];
        apply_rgba_generic(&canvas, cw, ch, &map90, &mut want).unwrap();
        let mut o8 = vec![0u8; ow * oh * 4];
        unsafe {
            rot90_kernel8(canvas.as_ptr(), cstride, rect90, o8.as_mut_ptr(), ow, oh);
        }
        assert_eq!(want, o8, "rot90 8 块应 ≡ 通用");
        let mut o16 = vec![0u8; ow * oh * 4];
        unsafe {
            rot90_kernel16(canvas.as_ptr(), cstride, rect90, o16.as_mut_ptr(), ow, oh);
        }
        assert_eq!(want, o16, "rot90 16 块应 ≡ 通用");

        let mut gen90: Vec<f64> = Vec::new();
        let mut s890: Vec<f64> = Vec::new();
        let mut s1690: Vec<f64> = Vec::new();
        let mut gen270: Vec<f64> = Vec::new();
        let mut s8270: Vec<f64> = Vec::new();
        let mut s16270: Vec<f64> = Vec::new();
        let mut pre = vec![0u8; ow * oh * 4];
        // 同窗交错（各 5 轮，轮间静置防热量偏置）。
        for _ in 0..5 {
            std::thread::sleep(std::time::Duration::from_millis(250));
            // rot90 三路径。
            let t = std::time::Instant::now();
            apply_rgba_generic(&canvas, cw, ch, &map90, &mut pre).unwrap();
            gen90.push(t.elapsed().as_secs_f64() * 1e3);
            let t = std::time::Instant::now();
            unsafe { rot90_kernel8(canvas.as_ptr(), cstride, rect90, pre.as_mut_ptr(), ow, oh) };
            s890.push(t.elapsed().as_secs_f64() * 1e3);
            let t = std::time::Instant::now();
            unsafe { rot90_kernel16(canvas.as_ptr(), cstride, rect90, pre.as_mut_ptr(), ow, oh) };
            s1690.push(t.elapsed().as_secs_f64() * 1e3);
            // rot270 三路径。
            let t = std::time::Instant::now();
            apply_rgba_generic(&canvas, cw, ch, &map270, &mut pre).unwrap();
            gen270.push(t.elapsed().as_secs_f64() * 1e3);
            let t = std::time::Instant::now();
            unsafe { rot270_kernel8(canvas.as_ptr(), cstride, rect270, pre.as_mut_ptr(), ow, oh) };
            s8270.push(t.elapsed().as_secs_f64() * 1e3);
            let t = std::time::Instant::now();
            unsafe { rot270_kernel16(canvas.as_ptr(), cstride, rect270, pre.as_mut_ptr(), ow, oh) };
            s16270.push(t.elapsed().as_secs_f64() * 1e3);
        }
        println!(
            "perf BLOCK rot90 {cw}x{ch}: generic={:.2} ms 8-block={:.2} ms 16-block={:.2} ms (8/generic={:.2}x, 16/8={:.2}x)",
            median_ms(&gen90),
            median_ms(&s890),
            median_ms(&s1690),
            median_ms(&gen90) / median_ms(&s890),
            median_ms(&s890) / median_ms(&s1690),
        );
        println!(
            "perf BLOCK rot270 {cw}x{ch}: generic={:.2} ms 8-block={:.2} ms 16-block={:.2} ms (8/generic={:.2}x, 16/8={:.2}x)",
            median_ms(&gen270),
            median_ms(&s8270),
            median_ms(&s16270),
            median_ms(&gen270) / median_ms(&s8270),
            median_ms(&s8270) / median_ms(&s16270),
        );
    }

    /// 全链 before/after（release）：`decode_rgba` 多轮中位，覆盖 IMG_0083（irot，真走
    /// transform）+ 两个 grid（tiles≥2，color/transform 并行观感）。
    /// 用 env 门控分窗测（同窗同二进制）：
    /// - `KIMG_HEIF_TRANSFORM_PARALLEL=0` = before（transform 落串行 SIMD）；
    ///   缺省 = after（transform 块行带并行）。
    /// - `KIMG_HEIF_PARALLEL=0` 定单核口径；reps 用 `KIMG_HEIF_REPS`（缺省 7）。
    /// 输出含门控状态，便于同窗对表。
    #[test]
    #[ignore]
    fn perf_decode_irot_before_after() {
        let corpus_root =
            PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../Benchmarks/corpus/heif"));
        let files = [
            "IMG_0083.HEIC",
            "hevc-main-3072x4096-grid.heic",
            "hevc-main-8192x6144-grid.heic",
        ];
        let reps = std::env::var("KIMG_HEIF_REPS")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(7);
        let simd = simd_enabled();
        let serial = std::env::var("KIMG_HEIF_PARALLEL").map(|v| v == "0").unwrap_or(false);
        let tpar = std::env::var("KIMG_HEIF_TRANSFORM_PARALLEL")
            .map(|v| v == "0")
            .unwrap_or(false);
        for name in files {
            let corpus = corpus_root.join(name);
            if !corpus.exists() {
                eprintln!("SKIP: missing {corpus:?}");
                continue;
            }
            let data = std::fs::read(&corpus).expect("read corpus");
            let info = crate::codecs::heif::read_info(&data).expect("read_info");
            let (w, h) = (info.width as usize, info.height as usize);
            let mut out = vec![0u8; w * h * 4];
            // warmup 2 轮（含 const init/映射缓存等一次性开销）。
            for _ in 0..2 {
                crate::codecs::heif::decode_rgba(&data, &mut out).expect("warmup");
            }
            let mut ms: Vec<f64> = Vec::with_capacity(reps);
            for _ in 0..reps {
                let t = std::time::Instant::now();
                crate::codecs::heif::decode_rgba(&data, &mut out).expect("decode_rgba");
                ms.push(t.elapsed().as_secs_f64() * 1e3);
            }
            ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let med = ms[ms.len() / 2];
            println!(
                "perf 全链 {name} {w}x{h}: simd={simd} serial={serial} tpar_off={tpar} median={med:.1} ms (rounds={})",
                ms.iter().map(|m| format!("{m:.1}")).collect::<Vec<_>>().join(","),
            );
        }
    }

    use std::path::PathBuf;
}
