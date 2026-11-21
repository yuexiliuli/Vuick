//! 裸 unsafe 叶子模块（唯一 `#![allow(unsafe_code)]` 免灾区）：
//! WebP ARGB 转换与 Subtract-Green 的 `#[target_feature]` SIMD 内核 + 安全门面。
//! 上层 `simd/argb.rs` 以 `#![forbid(unsafe_code)]` + 薄委托显式依赖本模块。
//! 算法 / 常量 / 调用顺序与隔离前逐字节一致（字节恒等是硬约束，禁改算术）。

#![allow(unsafe_code)]

use super::*;

// VP8 D1 帧内预测 / VP8L 预测器·色变换逆变换的裸 unsafe 叶子子模块
// （`simd/predict.rs` / `simd/predictor.rs` 的薄委托依赖本模块，见 `predictor.rs`）。
pub(super) mod predictor;

// YUV→RGB 定点转换 / fancy 上采样的裸 unsafe 叶子子模块
// （`simd/yuv.rs` / `simd/upsample.rs` 的薄委托依赖本模块）。
pub(super) mod upsample;
pub(super) mod yuv;

// D2 环路滤波（D2v1 弃用休眠集群 / D2v2 libwebp 风格 `lf_edge_*` 内核 / D2v3 宏块熔合
// `lf_filter_macroblock`）的裸 unsafe 叶子子模块（`simd/loop_filter.rs` / `simd/lf_edge.rs`
// 的薄委托依赖本模块）。
pub(super) mod loop_filter;

// IDCT 逆变换 / DC 捷径的两遍 butterfly + i16±i32 域 pass2 的裸 unsafe 叶子子模块
// （`simd/idct.rs` 的薄委托依赖本模块）。
pub(super) mod idct;

// 宏块重建 / i4 子块壳批量化（pipeline/mbdec 域：`reconstruct_macroblock` /
// `reconstruct_i4_mb_ssse3`）的裸 unsafe 叶子子模块——`vp8/pipeline.rs` 的薄委托依赖本模块。
// 与其余 raw 子模块不同，本模块经 `vp8/*` 薄委托消费（跨出 simd 同级），故声明为
// `pub(crate)`（`reconstruct_macroblock`/`reconstruct_i4_mb_ssse3` 内核整块迁入，
// 调用方 `gates` 门控提升/字节恒等语义随函数原样带过来）。
pub(crate) mod pipeline;

// 宏块熵解码（`parse_residuals*` / `read_macroblock_modes*` 分发器与 BMI 快道内核、
// `read_tree`/树路由宏）的裸 unsafe 叶子子模块——`vp8/mbdec.rs` 的薄委托依赖本模块。
// 同 `pipeline`：跨 simd 同级被 vp8 消费，声明为 `pub(crate)`。
pub(crate) mod mbdec;

// VP8L Huffman 符号热读（`read_symbol` 裸指针表区 + 探针计数）的裸 unsafe 叶子子模块
// ——`codecs/webp/vp8l.rs` 的薄委托依赖本模块。同 `pipeline`：跨 simd 同级被 vp8l 消费，
// 声明为 `pub(crate)`。
pub(crate) mod vp8l;

// ---------------------------------------------------------------------------
// 安全门面：特性检测分支 + 忙循环（内核调用在 `unsafe {}` 内，本模块允许 unsafe）
// ---------------------------------------------------------------------------

/// ARGB 像素（u32：A<<24|R<<16|G<<8|B，内存小端字节序 [B,G,R,A]）→ RGBA8 字节。
/// 即 vp8l.rs `to_rgba` 的字节序交换（B/R 对调）；AVX2 每 8 像素一次 shuffle，SSSE3 4 像素
/// 收尾、尾列标量（AVX2 隐含 SSSE3）。字节恒等：shuffle 逐字节独立。
pub(super) fn argb_to_rgba(pixels: &[u32], out: &mut [u8]) {
    debug_assert_eq!(out.len(), pixels.len() * 4);
    let n = pixels.len();
    #[cfg(target_arch = "x86_64")]
    if simd_on("avx2") && n >= 8 {
        let mut i = 0usize;
        unsafe {
            while i + 8 <= n {
                argb8_to_rgba_avx2(pixels.as_ptr().add(i).cast::<u8>(), out.as_mut_ptr().add(i * 4));
                i += 8;
            }
            while i + 4 <= n {
                argb4_to_rgba_ssse3(pixels.as_ptr().add(i).cast::<u8>(), out.as_mut_ptr().add(i * 4));
                i += 4;
            }
            while i < n {
                let p = *pixels.get_unchecked(i);
                let o = i * 4;
                *out.get_unchecked_mut(o) = (p >> 16) as u8;
                *out.get_unchecked_mut(o + 1) = (p >> 8) as u8;
                *out.get_unchecked_mut(o + 2) = p as u8;
                *out.get_unchecked_mut(o + 3) = (p >> 24) as u8;
                i += 1;
            }
        }
        return;
    }
    #[cfg(target_arch = "x86_64")]
    if simd_on("ssse3") && n >= 4 {
        let mut i = 0usize;
        unsafe {
            while i + 4 <= n {
                argb4_to_rgba_ssse3(pixels.as_ptr().add(i).cast::<u8>(), out.as_mut_ptr().add(i * 4));
                i += 4;
            }
            while i < n {
                let p = *pixels.get_unchecked(i);
                let o = i * 4;
                *out.get_unchecked_mut(o) = (p >> 16) as u8;
                *out.get_unchecked_mut(o + 1) = (p >> 8) as u8;
                *out.get_unchecked_mut(o + 2) = p as u8;
                *out.get_unchecked_mut(o + 3) = (p >> 24) as u8;
                i += 1;
            }
        }
        return;
    }
    for (i, &p) in pixels.iter().enumerate() {
        out[i * 4] = (p >> 16) as u8;
        out[i * 4 + 1] = (p >> 8) as u8;
        out[i * 4 + 2] = p as u8;
        out[i * 4 + 3] = (p >> 24) as u8;
    }
}

/// 原地版 ARGB→RGBA 收尾（`decode()` 复用同一缓冲，省整帧 RGBA 拷贝/分配）：
/// 每个 u32 的字节 0(B) 与 2(R) 对调（字节 3=A、1=G 原样）→ 内存序即 [R,G,B,A]。
/// 与 `argb_to_rgba` 逐字节同语义；AVX2/SSSE3 用 32/16 字节 shuffle 原地读写
/// （load→shuffle→store 同一地址，32 字节全量覆写，安全）。
pub(super) fn argb_to_rgba_inplace(pixels: &mut [u32]) {
    let n = pixels.len();
    #[cfg(target_arch = "x86_64")]
    if simd_on("avx2") && n >= 8 {
        unsafe {
            let p = pixels.as_mut_ptr().cast::<u8>();
            let mut i = 0usize;
            while i + 8 <= n {
                argb8_to_rgba_inplace_avx2(p.add(i * 4));
                i += 8;
            }
            while i + 4 <= n {
                argb4_to_rgba_inplace_ssse3(p.add(i * 4));
                i += 4;
            }
            while i < n {
                let b = *pixels.get_unchecked(i);
                *pixels.get_unchecked_mut(i) =
                    (b & 0xff00_ff00) | ((b & 0xff) << 16) | ((b >> 16) & 0xff);
                i += 1;
            }
        }
        return;
    }
    #[cfg(target_arch = "x86_64")]
    if simd_on("ssse3") && n >= 4 {
        unsafe {
            let p = pixels.as_mut_ptr().cast::<u8>();
            let mut i = 0usize;
            while i + 4 <= n {
                argb4_to_rgba_inplace_ssse3(p.add(i * 4));
                i += 4;
            }
            while i < n {
                let b = *pixels.get_unchecked(i);
                *pixels.get_unchecked_mut(i) =
                    (b & 0xff00_ff00) | ((b & 0xff) << 16) | ((b >> 16) & 0xff);
                i += 1;
            }
        }
        return;
    }
    for px in pixels {
        let b = *px;
        *px = (b & 0xff00_ff00) | ((b & 0xff) << 16) | ((b >> 16) & 0xff);
    }
}

/// Subtract-Green 逆变换（标量语义 `apply_subtract_green`）：R/G 通道 + G 通道（mod 256）。
/// 像素内存字节 [B,G,R,A]：把 G 字节加到 B 与 R 字节，G/A 原样保留。AVX2 每 8 像素、
/// SSE4.1 每 4 像素、尾列标量（AVX2 隐含 SSE4.1）。
pub(super) fn subtract_green(pixels: &mut [u32]) {
    let n = pixels.len();
    #[cfg(target_arch = "x86_64")]
    if simd_on("avx2") && n >= 8 {
        let mut i = 0usize;
        unsafe {
            while i + 8 <= n {
                subtract_green8_avx2(pixels.as_mut_ptr().add(i).cast::<u8>());
                i += 8;
            }
            while i + 4 <= n {
                subtract_green4_sse41(pixels.as_mut_ptr().add(i).cast::<u8>());
                i += 4;
            }
            while i < n {
                let p = *pixels.get_unchecked(i);
                let green = (p >> 8) & 0xff;
                let red = (((p >> 16) & 0xff).wrapping_add(green)) & 0xff;
                let blue = ((p & 0xff).wrapping_add(green)) & 0xff;
                *pixels.get_unchecked_mut(i) = (p & 0xff00_ff00) | (red << 16) | blue;
                i += 1;
            }
        }
        return;
    }
    #[cfg(target_arch = "x86_64")]
    if simd_on("sse4.1") && n >= 8 {
        let mut i = 0usize;
        unsafe {
            while i + 4 <= n {
                subtract_green4_sse41(pixels.as_mut_ptr().add(i).cast::<u8>());
                i += 4;
            }
            while i < n {
                let p = *pixels.get_unchecked(i);
                let green = (p >> 8) & 0xff;
                let red = (((p >> 16) & 0xff).wrapping_add(green)) & 0xff;
                let blue = ((p & 0xff).wrapping_add(green)) & 0xff;
                *pixels.get_unchecked_mut(i) = (p & 0xff00_ff00) | (red << 16) | blue;
                i += 1;
            }
        }
        return;
    }
    for p in pixels {
        let green = (*p >> 8) & 0xff;
        let red = (((*p >> 16) & 0xff).wrapping_add(green)) & 0xff;
        let blue = ((*p & 0xff).wrapping_add(green)) & 0xff;
        *p = (*p & 0xff00_ff00) | (red << 16) | blue;
    }
}

// ---------------------------------------------------------------------------
// VP8 熵解码 / 位流 / 缓冲操作的裸 unsafe 叶子安全门面
// （`vp8/bpdec.rs` / `vp8/tables.rs` / `vp8/subbuffers.rs` / `vp8/decode.rs` 薄委托，
//  各文件本体 `#![forbid(unsafe_code)]`，本组门面为唯一下沉点）。
// 可见性 `pub(in super::super)` = webp 子树——`vp8/*` 安全 fn 直接调用；
// 全部 `#[inline(always)]`，内联后 codegen 与隔离前逐指令一致（硬约束：字节恒等 + 零新增边界检查）。
// ---------------------------------------------------------------------------

/// 非对齐 8 字节读（`ent_refill` 快道）：调用方保证 `pos + 8 <= data.len()`。
#[inline(always)]
pub(in super::super) fn read_u64_unaligned(data: &[u8], pos: usize) -> u64 {
    // SAFETY: 调用方（ent_refill）先分支守卫 `len - pos >= 8`，两分支恒不越界。
    unsafe { (data.as_ptr().add(pos) as *const u64).read_unaligned() }
}

/// 熵移位数下界假设：`b < 64`（调用方上界不变量证明，bits ∈ [0,55]）。内联后 `llvm.assume`
/// 与返回值同一 SSA 绑定——x86 变量移位前 `andl $63` 谓恒等折叠被删除（省 1-2 uop，原
/// `core::hint::assert_unchecked` 语义，debug 仍为真实断言）。返回原值直接作移位位数使用。
#[inline(always)]
pub(in super::super) fn ent_shift_amt_lt_64(b: u32) -> u32 {
    // SAFETY: b < 64 恒成立（ent_split_pre/ent_read_bit/ent_read_signed 上界证明）。
    unsafe { core::hint::assert_unchecked(b < 64) };
    b
}

/// 常量表无检查下标读（tables.rs `ZIGZAG` / `BAND_ROW_OFFSETS`）：调用方保证 `idx < slice.len()`
/// （position ≤ 15 < 16 环序证明）。返回 Copy 值；与 `get_unchecked` 同指令（常数位移寻址）。
#[inline(always)]
pub(in super::super) fn unchecked_index<T: Copy>(slice: &[T], idx: usize) -> T {
    // SAFETY: 调用方证明 idx < len——去掉必然不触发的边界检查（保移位省 cmp+溢出分支）。
    unsafe { *slice.get_unchecked(idx) }
}

/// 熵概率表滑动游标（tables.rs `tokens` 的 `p`）：封装「裸指针基址 + 偏移读」不变量
/// （p_idx ≤ 253、p_idx + k ≤ 263 < 264，每型概率表长 264）。`Copy`，内联后逐位同原指针游标
/// 寻址（`movzx …, byte ptr [reg + k]`）。
#[derive(Clone, Copy)]
pub(in super::super) struct Vp8ProbCursor(*const u8);

impl Vp8ProbCursor {
    /// 从切片按下标建游标：调用方保证 `idx < slice.len()`（tokens 的 p_idx 上界证明）。
    #[inline(always)]
    pub(in super::super) fn from_slice_unchecked(slice: &[u8], idx: usize) -> Self {
        // SAFETY: idx < len 由调用方不变量证明。
        Self(unsafe { slice.as_ptr().add(idx) })
    }

    /// 读当前位概率字节（`p[0]`）。
    #[inline(always)]
    pub(in super::super) fn read(self) -> u8 {
        // SAFETY: 游标恒指概率表内（p_idx ≤ 253，+0 ≤ 263 < 264）。
        unsafe { *self.0 }
    }

    /// 读 `self + k` 位概率字节（`p[k]`，k ≤ 10）。
    #[inline(always)]
    pub(in super::super) fn read_offset(self, k: usize) -> u8 {
        // SAFETY: p_idx + k ≤ 263 < 264（调用方证明，p_idx ≤ 253、k ≤ 10）。
        unsafe { *self.0.add(k) }
    }
}

/// 缓冲 `set_len` 门面（subbuffers 池）：调用方保证 `need ≤ capacity`（已 reserve）且新增字节
/// 在读前全部写入（未初始化语义，同 `uninit_vec_u8`）。
#[inline(always)]
pub(in super::super) fn vec_set_len<T>(v: &mut Vec<T>, need: usize) {
    // SAFETY: need ≤ capacity 由调用方 reserve 保证；T 无 Drop glue（u8/u32/(u8,u8)）。
    unsafe { v.set_len(need) };
}

/// 未初始化 `Vec<u8>`：调用方保证长度字节在读前全部写入（decode 各发射路径全字节覆盖）。
/// 省去 `vec![0u8; len]` 的全量 memset——主线程关键路径上（P-B 直出已覆盖全部字节），
/// 与解码前语义逐字节恒等。
#[inline]
#[allow(clippy::uninit_vec)]
pub(in super::super) fn uninit_vec_u8(len: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(len);
    // SAFETY: u8 无 Drop glue；调用方保证 len 字节读前全写（decode 发射路径覆盖全部字节）。
    unsafe { v.set_len(len) };
    v
}

// ---------------------------------------------------------------------------
// VP8L 无损解码的裸 unsafe 叶子安全门面
// （`codecs/webp/vp8l.rs` 薄委托依赖本模块；Huffman 主环下标写 / 组引用 / 短回看拷贝 /
//  RGB 缓冲重借的唯一下沉点）。全部 `#[inline(always)]`，内联后 codegen 与隔离前逐指令一致
// （硬约束：字节恒等 + 零新增边界检查）。
// ---------------------------------------------------------------------------

/// 无检查下标写（`decode_pixels` 写 `result`）：调用方保证 `idx < dest.len()`
/// （position 单调推进 < pixel_count 前置证明，写入为每像素关键路径）。
#[inline(always)]
pub(in super::super) fn unchecked_write<T>(dest: &mut [T], idx: usize, value: T) {
    // SAFETY: idx < dest.len() 由调用方不变量证明——去掉必然不触发的边界检查（保移位快道）。
    unsafe { *dest.get_unchecked_mut(idx) = value }
}

/// `Option<T>` 切片取 `Some` 引用（`group_ref` 安全门面）：调用方保证
/// `idx < options.len() && options[idx].is_some()`（`select_group` 返回前已校验并 Err）。
/// 与 `get_unchecked + as_ref + unwrap_unchecked` 逐条同指令（内联后零分支）。
#[inline(always)]
pub(in super::super) fn option_slice_ref<T>(options: &[Option<T>], idx: usize) -> &T {
    // SAFETY: 调用方证明 idx < len 且 is_some（select_group 已校验，组引用在块边界才解析）。
    unsafe { options.get_unchecked(idx).as_ref().unwrap_unchecked() }
}

/// 重叠前向逐写拷贝（`copy_backward` ≤16 短环，保留重叠语义）：
/// `p[src+i] -> p[dst+i]`（i 递增，T 为 Copy）。调用方保证 `src+n`、`dst+n`
/// 均在已初始化长度内（copy_backward 前置 `distance <= position`、`position+length <= len`）。
#[inline(always)]
pub(in super::super) fn ptr_copy_forward<T>(p: *mut T, src: usize, dst: usize, n: usize) {
    // SAFETY: 见上——copy_backward 已 debug_assert 界内、distance<=position（重叠回看）。
    unsafe {
        for i in 0..n {
            let v = p.add(src + i).read();
            p.add(dst + i).write(v);
        }
    }
}

/// u32 ARGB 缓冲的只读字节视图（`RgbaBytes::as_bytes` U32 臂，零拷贝收尾）。
/// 调用方保证缓冲内所有 u32 均为合法位型（解码产物）、无写入别名（只读重借）。
#[inline(always)]
pub(in super::super) fn u32_slice_as_bytes(src: &[u32]) -> &[u8] {
    // SAFETY: u32 在位型均为合法位型且只读重借（无写入别名），len*4 字节均有效。
    unsafe { std::slice::from_raw_parts(src.as_ptr().cast::<u8>(), src.len() * 4) }
}

/// 调用方 u8 缓冲重借为 `&mut [u32]`（`decode_into_rgba8` 对齐快道）。调用方保证
/// `out` 已 4 字节对齐（`out.as_ptr() % align_of::<u32>() == 0`）且 `len` 个 u32 全可写
/// （`out.len() >= len*4`）。重借存活期不触碰原 `out`；缓冲由调用方持有、无 Drop 冲突。
#[inline(always)]
pub(in super::super) fn bytes_as_mut_u32(out: &mut [u8], len: usize) -> &mut [u32] {
    // SAFETY: 调用方证明对齐与容量（decode_into_rgba8 对齐分支前置）。
    unsafe { std::slice::from_raw_parts_mut(out.as_mut_ptr().cast::<u32>(), len) }
}

// ---------------------------------------------------------------------------
// D2 环路滤波宏块熔合安全入口（`vp8/filter.rs::filter_macroblock_row` SIMD 熔合路径）。
// 指针算术 + `raw::loop_filter::lf_filter_macroblock` 的 unsafe 调用整体下沉本文件；
// 内联后与原调用点（裸指针派点 + unsafe 分派）逐指令一致，字节恒等。
// ---------------------------------------------------------------------------

/// 宏块级熔合环路滤波（生产；`lf_filter_macroblock` 的安全切片门面）。指针语义同内核注释：
/// `yc` 指向 (行 mby*16、列 mbx*16) 的 q0（= y_row*y_pitch + 1 + x），`uc`/`vc` 同理指向色度 q0；
/// 跨块读（左列 -4 / 上行 -4）落在解码平面 4 像素边距内（与标量 `filter_edge` 同界）。参数为
/// 滤波强度实值（调用方已算好 interior/edge_limit/sub_limit/hev），left/top/inner/simple 同内核。
#[cfg(target_arch = "x86_64")]
#[inline(always)]
#[allow(clippy::too_many_arguments)]
pub(in super::super) fn lf_filter_macroblock_safe(
    y: &mut [u8],
    y_pitch: usize,
    u: &mut [u8],
    v: &mut [u8],
    uv_pitch: usize,
    y_row: usize,
    x: usize,
    uv_row: usize,
    uvx: usize,
    left: bool,
    top: bool,
    inner: bool,
    simple: bool,
    interior: i32,
    edge_limit: i32,
    sub_limit: i32,
    hev: i32,
) {
    // SAFETY: 指针/边界语义与直接调用 `lf_filter_macroblock` 完全一致（q0 定位 + 4 像素边距
    // 内跨块读），仅把派点从安全层下沉到此 unsafe 免灾区。
    unsafe {
        let yc = y.as_mut_ptr().add(y_row * y_pitch + 1 + x);
        let uc = u.as_mut_ptr().add(uv_row * uv_pitch + 1 + uvx);
        let vc = v.as_mut_ptr().add(uv_row * uv_pitch + 1 + uvx);
        loop_filter::lf_filter_macroblock(
            yc,
            y_pitch as isize,
            uc,
            vc,
            uv_pitch as isize,
            left,
            top,
            inner,
            simple,
            interior,
            edge_limit,
            sub_limit,
            hev,
        )
    }
}

// ---------------------------------------------------------------------------
// `#[target_feature]` SIMD 内核（原样搬入 argb.rs）
// ---------------------------------------------------------------------------

/// AVX2：8 像素 [B,G,R,A]×8 → [R,G,B,A]×8（原地 B/R 对调）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn argb8_to_rgba_inplace_avx2(px: *mut u8) {
    use std::arch::x86_64::*;
    unsafe {
    let v = _mm256_loadu_si256(px as *const __m256i);
    // 低 16 字节 4 像素与高 16 字节 4 像素同一 mask（高半区索引 16..31 映射到 shuf 常量）。
    let shuf = _mm256_setr_epi8(2, 1, 0, 3, 6, 5, 4, 7, 10, 9, 8, 11, 14, 13, 12, 15,
                                2, 1, 0, 3, 6, 5, 4, 7, 10, 9, 8, 11, 14, 13, 12, 15);
    let out_v = _mm256_shuffle_epi8(v, shuf);
    _mm256_storeu_si256(px as *mut __m256i, out_v);
    }
}

/// AVX2：8 像素 [B,G,R,A]×8 → [R,G,B,A]×8（写独立输出缓冲）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn argb8_to_rgba_avx2(px: *const u8, out: *mut u8) {
    use std::arch::x86_64::*;
    unsafe {
    let v = _mm256_loadu_si256(px as *const __m256i);
    let shuf = _mm256_setr_epi8(2, 1, 0, 3, 6, 5, 4, 7, 10, 9, 8, 11, 14, 13, 12, 15,
                                2, 1, 0, 3, 6, 5, 4, 7, 10, 9, 8, 11, 14, 13, 12, 15);
    let out_v = _mm256_shuffle_epi8(v, shuf);
    _mm256_storeu_si256(out as *mut __m256i, out_v);
    }
}

/// SSSE3：4 像素 [B,G,R,A]×4 → [R,G,B,A]×4（字节重排：每像素取原 2,1,0,3）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "ssse3")]
unsafe fn argb4_to_rgba_ssse3(px: *const u8, out: *mut u8) {
    use std::arch::x86_64::*;
    unsafe {
    let v = _mm_loadu_si128(px as *const __m128i); // [B0,G0,R0,A0, B1,G1,R1,A1, ...]
    let shuf = _mm_setr_epi8(2, 1, 0, 3, 6, 5, 4, 7, 10, 9, 8, 11, 14, 13, 12, 15);
    let out_v = _mm_shuffle_epi8(v, shuf);
    _mm_storeu_si128(out as *mut __m128i, out_v);
    }
}

/// SSSE3：4 像素原地 B/R 对调（load→shuffle→store 同一地址，16 字节全量覆写）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "ssse3")]
unsafe fn argb4_to_rgba_inplace_ssse3(px: *mut u8) {
    use std::arch::x86_64::*;
    unsafe {
    let v = _mm_loadu_si128(px as *const __m128i);
    let shuf = _mm_setr_epi8(2, 1, 0, 3, 6, 5, 4, 7, 10, 9, 8, 11, 14, 13, 12, 15);
    let out_v = _mm_shuffle_epi8(v, shuf);
    _mm_storeu_si128(px as *mut __m128i, out_v);
    }
}

/// SSE4.1：4 像素 Subtract-Green。`sum = px + G`（全通道 +G，mod 256）后，用 blendv 把
/// G(byte1)/A(byte3) 恢复为原值，B(byte0)/R(byte2) 保留加和。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse4.1")]
unsafe fn subtract_green4_sse41(px: *mut u8) {
    use std::arch::x86_64::*;
    unsafe {
    let v = _mm_loadu_si128(px as *const __m128i);
    let gb = _mm_shuffle_epi8(v, _mm_setr_epi8(1, 1, 1, 1, 5, 5, 5, 5, 9, 9, 9, 9, 13, 13, 13, 13));
    let sum = _mm_add_epi8(v, gb);
    // mask 0xFF00FF00（LE bytes [00,FF,00,FF]）：byte1/byte3 取原值（P），byte0/2 取 sum。
    let res = _mm_blendv_epi8(sum, v, _mm_set1_epi32(0xFF00FF00u32 as i32));
    _mm_storeu_si128(px as *mut __m128i, res);
    }
}

/// AVX2：8 像素 Subtract-Green（同 `subtract_green4_sse41` 算术，256 位 1 步 8 像素）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,sse4.1")]
unsafe fn subtract_green8_avx2(px: *mut u8) {
    use std::arch::x86_64::*;
    unsafe {
    let v = _mm256_loadu_si256(px as *const __m256i);
    // 每个 16 字节半区：byte1 广播到该像素 4 字节（G→[G,G,G,G]）。
    let gb = _mm256_shuffle_epi8(v, _mm256_setr_epi8(1, 1, 1, 1, 5, 5, 5, 5, 9, 9, 9, 9, 13, 13, 13, 13,
                                                     1, 1, 1, 1, 5, 5, 5, 5, 9, 9, 9, 9, 13, 13, 13, 13));
    let sum = _mm256_add_epi8(v, gb);
    let res = _mm256_blendv_epi8(sum, v, _mm256_set1_epi32(0xFF00FF00u32 as i32));
    _mm256_storeu_si256(px as *mut __m256i, res);
    }
}
