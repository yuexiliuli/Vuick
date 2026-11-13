#![forbid(unsafe_code)]
//! VP8L 静态无损解码器（RFC 9649 §3.5-3.8；移植自 SimdCodecs `Vp8LDecoder.cs`，MIT；**标量正确版**）。
//!
//! 覆盖：VP8L 头（字节 0=0x2f + 位缓存维度）、熵编码（简单/普通前缀码 + Huffman 表构建：
//! canonical+bit-reversed + 二级表）、颜色缓存（lazy 填充 + 哈希）、LZ77 回看（含重叠拷贝）、
//! 5 预测器 + Subtract-Green + Color-Indexing + Color-Transform 逆变换、ToRgba（ARGB→RGBA8）。
//! 像素内部以 `u32` ARGB（`A<<24|R<<16|G<<8|B`）承载，输出抽为 RGBA8 字节。
//!
//! 输入安全（K42）：所有尺寸乘法 `checked`、位读取越界在 BitReader 截断守卫处抛错、Huffman
//! 表构建校验 Kraft 完备性与符号越界；任何畸形输入 → 结构化 `Vp8LError`，不 panic 不 OOM。
//! ALPH 无损熵路径复用点：`decode_to_argb`/`items` 与容器 alpha 分支解耦，P5 有损-alpha 阶段
//! 复用本模块的熵解码（届时经 `DecodeAlpha` 收敛到 `argb >> 8`）。

use std::fmt;

use super::container::MAX_PIXELS;
// 探针计数跨模块共用（stage_decode 重置/回读），仅 `stage_profile` feature 编译。
#[cfg(feature = "stage_profile")]
use super::simd::raw::vp8l::{SUBTABLE_HITS, SYMBOL_READS};

// 外部语料定位助手（与集成测试共享，见 tests/fixtures/webp/mod.rs）。
#[cfg(test)]
#[path = "../../../tests/fixtures/webp/mod.rs"]
mod webp_fixtures_support;

/// VP8L 解码错误（结构化，供上层映射为 `ImageError::Decoding` 或兜底）。
#[derive(Debug, Clone)]
pub(crate) struct Vp8LError(pub(crate) String);

impl fmt::Display for Vp8LError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Vp8LError {}

impl Vp8LError {
    fn new<S: Into<String>>(msg: S) -> Self {
        Vp8LError(msg.into())
    }
}

type Result<T> = std::result::Result<T, Vp8LError>;

/// 解码结果。
pub(crate) struct DecodedImage {
    pub width: u32,
    pub height: u32,
    /// VP8L 头 alpha 位。
    pub has_alpha: bool,
    /// RGBA8 紧凑像素（w*h*4）。
    pub rgba: RgbaBytes,
}

/// RGBA8 像素缓冲。两种后端保底，均满足「归还 Layout == 分配 Layout」：
/// - `U32`（无损路径）：接管 ARGB `Vec<u32>` 原地收尾后的缓冲，零拷贝、省一次整帧拷贝；
///   所有权由 `Vec<u32>` 持有，归还恒为原 Layout（align 4、size = 4×u32 容量），
///   `GlobalAlloc::dealloc` 同 Layout 契约由 Vec 的 Drop 保证（不降 align）。
/// - `U8`（有损路径）：解码直出的 `Vec<u8>`，沿用既有分配。
/// `as_bytes()` 只借出不可变 `&[u8]`（U32 臂为按字节只读重借，无写入别名；U32 均为合法位型）。
/// 不实现 Clone（单一所有权）。
pub(crate) struct RgbaBytes {
    inner: RgbaBytesKind,
}

enum RgbaBytesKind {
    U32(Vec<u32>),
    U8(Vec<u8>),
}

impl RgbaBytes {
    /// 空缓冲（未分配像素）。
    pub(crate) fn empty() -> Self {
        RgbaBytes {
            inner: RgbaBytesKind::U32(Vec::new()),
        }
    }

    /// 接管已完成 ARGB→RGBA8（内存字节序 [R,G,B,A]）的 `Vec<u32>`，零拷贝收尾。
    /// 前置：每个 u32 已完成字节序交换且 len×4 字节均已写出。
    pub(crate) fn from_argb_vec(inner: Vec<u32>) -> Self {
        RgbaBytes {
            inner: RgbaBytesKind::U32(inner),
        }
    }

    /// 接收直出的 `Vec<u8>` RGBA8 缓冲（有损路径；沿用既有分配）。
    pub(crate) fn from_vec_u8(inner: Vec<u8>) -> Self {
        RgbaBytes {
            inner: RgbaBytesKind::U8(inner),
        }
    }

    /// RGBA8 只读字节视图。
    pub(crate) fn as_bytes(&self) -> &[u8] {
        match &self.inner {
            RgbaBytesKind::U32(v) => {
                // 只读按字节重借（下沉 `simd::raw::u32_slice_as_bytes`）：u32 缓冲内均为合法位型，
                // 无写入别名，sound。
                super::simd::raw::u32_slice_as_bytes(v)
            }
            RgbaBytesKind::U8(v) => v.as_slice(),
        }
    }

    /// RGBA8 字节数（= w*h*4）。
    /// 休眠：仅 `stage_profile` 探针测试用（产线经 `as_bytes()` 取视图），无生产调用者。
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn len(&self) -> usize {
        match &self.inner {
            RgbaBytesKind::U32(v) => v.len() * 4,
            RgbaBytesKind::U8(v) => v.len(),
        }
    }

    /// 休眠：无生产调用者，仅探针/备用用。
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn is_empty(&self) -> bool {
        match &self.inner {
            RgbaBytesKind::U32(v) => v.is_empty(),
            RgbaBytesKind::U8(v) => v.is_empty(),
        }
    }
}

impl Default for RgbaBytes {
    fn default() -> Self {
        RgbaBytes::empty()
    }
}

/// VP8L 无损解码分阶段计时（K59/K60 测量先行，供 imgbench 阶段分账探针）。
/// 字段为本轮解码各阶段墙钟纳秒：`transforms_head_ns`=变换头解析（含各变换数据子图熵解码、
/// 构建）；`entropy_ns`=主图像流熵解码（**不含**颜色缓存段）；`cache_ns`=颜色缓存 lazy 填充 +
/// 命中（熵解码内部分离段）；`transforms_ns`=逆变换应用（predictor/color/subtract-green/
/// color-indexing）；`to_rgba_ns`=收尾 ARGB→RGBA8 拷贝；`total_ns`=全链墙钟。
/// `cache_symbols/cache_fill_pixels` 为缓存符号与填充像素计数（聚类依据）。
/// 本结构体无条件编译（供内部签名线程），计时器体仅在 `stage_profile` feature 下启用，
/// feature 关闭时经 `None` 透传、零运行时影响。
#[cfg_attr(not(feature = "stage_profile"), allow(dead_code))]
#[derive(Default, Clone, Copy, Debug)]
pub struct Vp8lStageProfile {
    pub width: u32,
    pub height: u32,
    pub transforms_head_ns: u64,
    pub entropy_ns: u64,
    pub cache_ns: u64,
    pub transforms_ns: u64,
    pub to_rgba_ns: u64,
    pub total_ns: u64,
    pub cache_symbols: u64,
    pub cache_fill_pixels: u64,
    /// 主熵流 `read_symbol` 总调用次数（含码长树等小头）。
    pub symbol_reads: u64,
    /// 其中命中二级表（码长 > ROOT_BITS）的次数 → 子表命中率。
    pub subtable_hits: u64,
}

// ---------------------------------------------------------------------------
// 位读取器（LSB-first，64 位缓冲；对齐 SimdCodecs BitReader）
// ---------------------------------------------------------------------------

struct BitReader<'a> {
    data: &'a [u8],
    /// 已消费字节数。
    pos: usize,
    /// 越过数据末尾后补的零字节数（截断守卫）。
    padding: u32,
    buffer: u64,
    bits: u32,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        BitReader {
            data,
            pos: 0,
            padding: 0,
            buffer: 0,
            bits: 0,
        }
    }

    fn bit_position(&self) -> u64 {
        ((self.data.len() - self.pos + self.padding as usize) as u64)
            .saturating_mul(8)
            .saturating_sub(u64::from(self.bits))
    }

    fn throw_if_truncated(&self) -> Result<()> {
        if self.padding != 0 && self.bit_position() > (self.data.len() as u64) * 8 {
            return Err(Vp8LError::new("VP8L 数据被截断"));
        }
        Ok(())
    }

    fn load(&self) -> (u64, u32) {
        (self.buffer, self.bits)
    }

    fn store(&mut self, buffer: u64, bits: u32) {
        self.buffer = buffer;
        self.bits = bits;
    }

    fn read_bit(&mut self) -> Result<u32> {
        self.read_bits(1)
    }

    fn read_bits(&mut self, count: u32) -> Result<u32> {
        if count > 24 {
            return Err(Vp8LError::new("VP8L 位计数非法"));
        }
        let mut buffer = self.buffer;
        let mut bits = self.bits;
        if bits < count {
            self.refill(&mut buffer, &mut bits)?;
        }
        let result = (buffer as u32) & ((1u32 << count) - 1);
        buffer >>= count;
        bits -= count;
        self.buffer = buffer;
        self.bits = bits;
        Ok(result)
    }

    fn read_symbol(&mut self, table: &[u32], root_mask: u32) -> Result<u32> {
        let mut buffer = self.buffer;
        let mut bits = self.bits;
        if bits < 15 {
            self.refill(&mut buffer, &mut bits)?;
        }
        let sym = read_symbol(table.as_ptr(), 0, root_mask, &mut buffer, &mut bits);
        self.buffer = buffer;
        self.bits = bits;
        Ok(sym)
    }

    /// 把 `bits`（入口 <56）至少补到 56。
    /// `#[inline(always)]`：对齐 vp8.rs 参考（81a9438）熵态寄存器驻留手法。
    #[inline(always)]
    fn refill(&mut self, buffer: &mut u64, bits: &mut u32) -> Result<()> {
        if self.pos + 8 <= self.data.len() {
            // 整 8 字节非对齐直读（区长度已保证读前 pos+8<=len，x86 LE 单条加载）；
            // 裸读下沉 `simd::raw::read_u64_unaligned`（叶子安全门面，同不变量）。
            let loaded = super::simd::raw::read_u64_unaligned(self.data, self.pos);
            *buffer |= loaded << *bits;
            let take = (63 - *bits) >> 3;
            self.pos += take as usize;
            *bits |= 56;
            Ok(())
        } else {
            self.buffer = *buffer;
            self.bits = *bits;
            self.refill_tail()?;
            *buffer = self.buffer;
            *bits = self.bits;
            Ok(())
        }
    }

    fn refill_tail(&mut self) -> Result<()> {
        while self.bits < 56 {
            let next: u64 = if self.pos < self.data.len() {
                let b = u64::from(self.data[self.pos]);
                self.pos += 1;
                b
            } else {
                if self.padding >= 8 {
                    return Err(Vp8LError::new("VP8L 数据被截断"));
                }
                self.padding += 1;
                0
            };
            self.buffer |= next << self.bits;
            self.bits += 8;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Huffman 符号解码（对齐 SimdCodecs，标量）
// ---------------------------------------------------------------------------

// 常数供建表层与 `simd::raw::vp8l::read_symbol` 共用（pub(crate)）。
pub(crate) const ROOT_BITS: u32 = 8;
pub(crate) const SUBTABLE_FLAG: u32 = 0x100;

// 探针计数已随 read_symbol 下沉 `simd::raw::vp8l`（stage_decode 经 `use` 回读）。

/// 解码一个符号（入口 ≥15 位缓冲）。裸指针表区读取下沉 `simd::raw::vp8l::read_symbol`
/// （本文件 forbid；热路径 + 探针计数见该叶子模块，`table` 为表池基址、`offset` 为通道偏移）——
/// `BitReader::read_symbol` 与其共享。
#[inline(always)]
fn read_symbol(
    table: *const u32,
    offset: usize,
    root_mask: u32,
    buffer: &mut u64,
    bits: &mut u32,
) -> u32 {
    super::simd::raw::vp8l::read_symbol(table, offset, root_mask, buffer, bits)
}

/// LZ77 长度/距离的前缀值（≤18 额外位）。
fn prefix_value(code: u32, buffer: &mut u64, bits: &mut u32) -> u32 {
    if code < 4 {
        return code + 1;
    }
    let extra = (code - 2) >> 1;
    let offset = (2 + (code & 1)) << extra;
    let extra_val = (*buffer as u32) & ((1u32 << extra) - 1);
    *buffer >>= extra;
    *bits -= extra;
    offset + extra_val + 1
}

/// 距离表（R8 顺序，与 SimdCodecs `DistanceMap`/RFC 9649 一致）。
const DISTANCE_MAP: [(i32, i32); 120] = [
    (0, 1), (1, 0), (1, 1), (-1, 1), (0, 2), (2, 0), (1, 2), (-1, 2),
    (2, 1), (-2, 1), (2, 2), (-2, 2), (0, 3), (3, 0), (1, 3), (-1, 3),
    (3, 1), (-3, 1), (2, 3), (-2, 3), (3, 2), (-3, 2), (0, 4), (4, 0),
    (1, 4), (-1, 4), (4, 1), (-4, 1), (3, 3), (-3, 3), (2, 4), (-2, 4),
    (4, 2), (-4, 2), (0, 5), (3, 4), (-3, 4), (4, 3), (-4, 3), (5, 0),
    (1, 5), (-1, 5), (5, 1), (-5, 1), (2, 5), (-2, 5), (5, 2), (-5, 2),
    (4, 4), (-4, 4), (3, 5), (-3, 5), (5, 3), (-5, 3), (0, 6), (6, 0),
    (1, 6), (-1, 6), (6, 1), (-6, 1), (2, 6), (-2, 6), (6, 2), (-6, 2),
    (4, 5), (-4, 5), (5, 4), (-5, 4), (3, 6), (-3, 6), (6, 3), (-6, 3),
    (0, 7), (7, 0), (1, 7), (-1, 7), (5, 5), (-5, 5), (7, 1), (-7, 1),
    (4, 6), (-4, 6), (6, 4), (-6, 4), (2, 7), (-2, 7), (7, 2), (-7, 2),
    (3, 7), (-3, 7), (7, 3), (-7, 3), (5, 6), (-5, 6), (6, 5), (-6, 5),
    (8, 0), (4, 7), (-4, 7), (7, 4), (-7, 4), (8, 1), (8, 2), (6, 6),
    (-6, 6), (8, 3), (5, 7), (-5, 7), (7, 5), (-7, 5), (8, 4), (6, 7),
    (-6, 7), (7, 6), (-7, 6), (8, 5), (7, 7), (-7, 7), (8, 6), (8, 7),
];

fn distance(value: u32, width: u32) -> Result<u32> {
    if value > 120 {
        return Ok(value - 120);
    }
    if value < 1 || value > 120 {
        return Err(Vp8LError::new("VP8L 距离码非法"));
    }
    let (x, y) = DISTANCE_MAP[(value - 1) as usize];
    let d = x + y * width as i32;
    Ok(if d < 1 { 1 } else { d as u32 })
}

/// 重叠/非重叠 LZ77 回看拷贝（对齐 C# `CopyBackward`）。
/// 调用点已自证 `distance <= position` 与 `position + length <= len`，
/// 本函数不再重复全量界查（≤16 短环走裸指针；distance==1 走批量填充）。
fn copy_backward(pixels: &mut [u32], position: usize, distance: usize, length: usize) {
    debug_assert!(distance <= position);
    debug_assert!(position + length <= pixels.len());
    let source = position - distance;
    if length <= 16 {
        // 短距离去界查（前置不变式已穷尽入界）。前向逐写保持重叠语义；
        // 裸指针读写下沉 `simd::raw::ptr_copy_forward`（同序逐元素，内联后同指令）。
        super::simd::raw::ptr_copy_forward(pixels.as_mut_ptr(), source, position, length);
        return;
    }
    if distance >= length {
        pixels.copy_within(source..source + length, position);
        return;
    }
    if distance == 1 {
        let v = pixels[source];
        // 相邻回写 = 值先读自 source（= position-1）后批量填充，无重叠读依赖；
        // `fill` 让 LLVM 生成宽存储。
        pixels[position..position + length].fill(v);
        return;
    }
    let mut copied = 0;
    while copied < length {
        let chunk = (length - copied).min(distance + copied);
        pixels.copy_within(source..source + chunk, position + copied);
        copied += chunk;
    }
}

// ---------------------------------------------------------------------------
// 变换与像素运算（标量）
// ---------------------------------------------------------------------------

struct Transform {
    ty: u8,
    width: u32,
    height: u32,
    /// 块位宽 / color-index 打包位。
    bits: u32,
    data: Option<Vec<u32>>,
    table: Option<Vec<u32>>,

}

// 以下标量助手对 simd.rs 公开（SIMD 段核/标量尾共用同一语义源，保证字节一致）。
pub(crate) fn add_argb(a: u32, b: u32) -> u32 {
    (((((a >> 24) & 0xff) + ((b >> 24) & 0xff)) & 0xff) << 24)
        | ((((a >> 16) & 0xff) + ((b >> 16) & 0xff)) & 0xff) << 16
        | ((((a >> 8) & 0xff) + ((b >> 8) & 0xff)) & 0xff) << 8
        | ((a & 0xff) + (b & 0xff)) & 0xff
}

pub(crate) fn color_delta(transform: u32, color: u32) -> i32 {
    let t = (transform as u8 as i8) as i32;
    let c = (color as u8 as i8) as i32;
    t.wrapping_mul(c) >> 5
}

pub(crate) fn average(a: u32, b: u32) -> u32 {
    ((((a >> 24) & 0xff) + ((b >> 24) & 0xff)) >> 1) << 24
        | ((((a >> 16) & 0xff) + ((b >> 16) & 0xff)) >> 1) << 16
        | ((((a >> 8) & 0xff) + ((b >> 8) & 0xff)) >> 1) << 8
        | (((a & 0xff) + (b & 0xff)) >> 1)
}

fn clamp_byte(v: i32) -> u32 {
    v.clamp(0, 255) as u32
}

pub(crate) fn clamp_add(a: u32, b: u32, c: u32) -> u32 {
    // 逐字节 clamp(a + b - c, 0, 255)。与 libwebp `ClampedAddSubtractFull`（i16 lane）一致。
    (clamp_byte((a >> 24) as i32 + (b >> 24) as i32 - (c >> 24) as i32) << 24)
        | (clamp_byte(
            ((a >> 16) & 0xff) as i32 + ((b >> 16) & 0xff) as i32 - ((c >> 16) & 0xff) as i32,
        ) << 16)
        | (clamp_byte(
            ((a >> 8) & 0xff) as i32 + ((b >> 8) & 0xff) as i32 - ((c >> 8) & 0xff) as i32,
        ) << 8)
        | clamp_byte((a & 0xff) as i32 + (b & 0xff) as i32 - (c & 0xff) as i32)
}

pub(crate) fn clamp_half(a: u32, b: u32) -> u32 {
    // 逐字节 clamp(a + (a - b)/2, 0, 255)（`/2` 截断除，与 libwebp `ClampedAddSubtractHalf` 一致）。
    (clamp_byte((a >> 24) as i32 + ((a >> 24) as i32 - (b >> 24) as i32) / 2) << 24)
        | (clamp_byte(
            ((a >> 16) & 0xff) as i32 + (((a >> 16) & 0xff) as i32 - ((b >> 16) & 0xff) as i32) / 2,
        ) << 16)
        | (clamp_byte(
            ((a >> 8) & 0xff) as i32 + (((a >> 8) & 0xff) as i32 - ((b >> 8) & 0xff) as i32) / 2,
        ) << 8)
        | clamp_byte((a & 0xff) as i32 + (((a & 0xff) as i32 - (b & 0xff) as i32) / 2))
}

pub(crate) fn select(left: u32, top: u32, top_left: u32) -> u32 {
    let la = (left >> 24) as i32;
    let lr = ((left >> 16) & 0xff) as i32;
    let lg = ((left >> 8) & 0xff) as i32;
    let lb = (left & 0xff) as i32;
    let ta = (top >> 24) as i32;
    let tr = ((top >> 16) & 0xff) as i32;
    let tg = ((top >> 8) & 0xff) as i32;
    let tb = (top & 0xff) as i32;
    let tla = (top_left >> 24) as i32;
    let tlr = ((top_left >> 16) & 0xff) as i32;
    let tlg = ((top_left >> 8) & 0xff) as i32;
    let tlb = (top_left & 0xff) as i32;
    let dl = (ta - tla).abs() + (tr - tlr).abs() + (tg - tlg).abs() + (tb - tlb).abs();
    let dt = (la - tla).abs() + (lr - tlr).abs() + (lg - tlg).abs() + (lb - tlb).abs();
    if dl < dt { left } else { top }
}

pub(crate) const BLACK: u32 = 0xff00_0000;

fn predict(mode: u32, x: u32, y: u32, _width: u32, left: u32, top: u32, top_left: u32, top_right: u32) -> Result<u32> {
    if x == 0 && y == 0 {
        return Ok(BLACK);
    }
    if y == 0 {
        return Ok(left);
    }
    if x == 0 {
        return Ok(top);
    }
    Ok(match mode {
        0 => BLACK,
        1 => left,
        2 => top,
        3 => top_right,
        4 => top_left,
        5 => average(average(left, top_right), top),
        6 => average(left, top_left),
        7 => average(left, top),
        8 => average(top_left, top),
        9 => average(top, top_right),
        10 => average(average(left, top_left), average(top, top_right)),
        11 => select(left, top, top_left),
        12 => clamp_add(left, top, top_left),
        13 => clamp_half(average(left, top), top_left),
        _ => return Err(Vp8LError::new("VP8L 预测器模式非法")),
    })
}

/// 预测器逆变换（标量回退见下方；SIMD 快路径在 `simd::predictor_inverse`）。
fn apply_predictor(pixels: &mut [u32], t: &Transform) -> Result<()> {
    let width = t.width as usize;
    let height = t.height as usize;
    let data = t.data.as_ref().expect("predictor data");
    // SIMD 快路径：块段分派（段内 mode 恒定取一次）+ 5/6/7/12/13 等核；字节==标量。
    // 非法模式（>13）由其预检返回 None → 回退下方标量（产出结构化 Err）。
    #[cfg(target_arch = "x86_64")]
    if super::simd::vp8l_predictor_ready() {
        return super::simd::predictor_inverse(pixels, width, height, t.bits, data)
            .ok_or_else(|| Vp8LError::new("VP8L 预测器模式非法"));
    }
    let bits = t.bits as usize;
    let block_w = (width + (1usize << bits) - 1) >> bits;
    for y in 0..height {
        for x in 0..width {
            let index = y * width + x;
            let left = if x == 0 { 0 } else { pixels[index - 1] };
            let top = if y == 0 { 0 } else { pixels[index - width] };
            let top_left = if x == 0 || y == 0 { 0 } else { pixels[index - width - 1] };
            let top_right = if y == 0 {
                0
            } else if x == width - 1 {
                pixels[index - x]
            } else {
                pixels[index - width + 1]
            };
            let mode = (data[(y >> t.bits) * block_w + (x >> t.bits)] >> 8) & 0xff;
            let prediction = predict(mode, x as u32, y as u32, width as u32, left, top, top_left, top_right)?;
            pixels[index] = add_argb(pixels[index], prediction);
        }
    }
    Ok(())
}

/// Color-Transform 逆变换（标量，对齐 C# `ApplyColorTransformScalar`）。
fn apply_color_transform(pixels: &mut [u32], t: &Transform) {
    let width = t.width as usize;
    let height = t.height as usize;
    let bits = t.bits as usize;
    let data = t.data.as_ref().expect("color transform data");
    if super::simd::color_transform_inverse(pixels, width, height, t.bits, data) {
        return;
    }
    let block_w = (width + (1usize << bits) - 1) >> bits;
    for y in 0..height {
        for x in 0..width {
            let index = y * width + x;
            let cte = data[(y >> t.bits) * block_w + (x >> t.bits)];
            let green = (pixels[index] >> 8) & 0xff;
            let red = (pixels[index] >> 16) & 0xff;
            let blue = pixels[index] & 0xff;
            let red_delta = color_delta(cte & 0xff, green);
            let blue_delta = color_delta((cte >> 8) & 0xff, green);
            let red_for_blue = (red as i32 + red_delta) & 0xff;
            let new_blue = (blue as i32 + blue_delta + color_delta((cte >> 16) & 0xff, red_for_blue as u32)) & 0xff;
            pixels[index] = (pixels[index] & 0xff00_ff00) | ((red_for_blue as u32) << 16) | (new_blue as u32);
        }
    }
}

/// Subtract-Green 逆变换（SIMD 版 + 标量回退见 `simd::subtract_green`）。
fn apply_subtract_green(pixels: &mut [u32], t: &Transform) {
    let count = t.width as usize * t.height as usize;
    super::simd::subtract_green(&mut pixels[..count]);
}

/// Color-Indexing 逆变换（标量，原地倒序展开；越界索引用 0）。
fn apply_color_indexing(pixels: &mut [u32], t: &Transform) {
    let bits = t.bits;
    let packed = 1usize << bits;
    let bits_per_index = 8 / packed;
    let mask = (1u32 << bits_per_index) - 1;
    let width = t.width as usize;
    let height = t.height as usize;
    let encoded_width = (width + packed - 1) >> bits;
    let palette = t.table.as_ref().expect("color indexing table");

    if bits == 0 {
        // 每像素一个索引：green 通道。
        for px in pixels.iter_mut().take(width * height) {
            let idx = ((*px >> 8) & 0xff) as usize;
            *px = palette.get(idx).copied().unwrap_or(0);
        }
        return;
    }

    for y in (0..height).rev() {
        let in_row = y * encoded_width;
        let out_row = y * width;
        for x in (0..width).rev() {
            let indices = (pixels[in_row + (x >> bits)] >> 8) & 0xff;
            let idx = ((indices >> ((x & (packed - 1)) * bits_per_index)) & mask) as usize;
            pixels[out_row + x] = palette.get(idx).copied().unwrap_or(0);
        }
    }
}

/// 应用单个逆变换。
fn apply_transform(pixels: &mut [u32], t: &Transform) -> Result<()> {
    match t.ty {
        0 => apply_predictor(pixels, t),
        1 => {
            apply_color_transform(pixels, t);
            Ok(())
        }
        2 => {
            apply_subtract_green(pixels, t);
            Ok(())
        }
        3 => {
            apply_color_indexing(pixels, t);
            Ok(())
        }
        _ => Err(Vp8LError::new("未知 VP8L 变换类型")),
    }
}

// ---------------------------------------------------------------------------
// Huffman 组 / 表
// ---------------------------------------------------------------------------

struct HuffmanGroup {
    green: usize,
    red: usize,
    blue: usize,
    alpha: usize,
    distance: usize,
    green_mask: u32,
    red_mask: u32,
    blue_mask: u32,
    alpha_mask: u32,
    distance_mask: u32,
    trivial_literal: bool,
    literal: u32,
}

/// 表池（对齐 C# `uint[] tables` + tablesUsed）。
struct HuffmanTables {
    data: Vec<u32>,
    used: usize,
}

impl HuffmanTables {
    fn new() -> Self {
        HuffmanTables {
            data: vec![0; 512],
            used: 0,
        }
    }

    fn ensure(&mut self, size: usize) {
        if self.data.len() - self.used < size {
            let needed = (self.used + size).max(self.data.len() * 2);
            self.data.resize(needed, 0);
        }
    }
}

/// 校验码长（0-15 / Kraft 完备）并返回解码表大小。
fn huffman_table_size(lengths: &[u32]) -> Result<usize> {
    let mut counts = [0u32; 16];
    let mut max_length = 0u32;
    let mut symbol_count = 0usize;
    for &length in lengths {
        if length > 15 {
            return Err(Vp8LError::new("VP8L 码长非法"));
        }
        if length != 0 {
            counts[length as usize] += 1;
            max_length = max_length.max(length);
            symbol_count += 1;
        }
    }
    if symbol_count <= 1 {
        return Ok(1);
    }
    let mut code = 0u32;
    for length in 1..=max_length {
        code = (code + counts[(length - 1) as usize]) << 1;
    }
    if code + counts[max_length as usize] != 1u32 << max_length {
        return Err(Vp8LError::new("VP8L 前缀码不完备"));
    }
    let root = (ROOT_BITS).min(max_length) as usize;
    if max_length <= ROOT_BITS {
        return Ok(1 << root);
    }
    let mut subtable_bits = [0u8; 1 << ROOT_BITS as usize];
    collect_subtable_bits(lengths, &counts, &mut subtable_bits);
    let mut size = 1 << root;
    for &sub in subtable_bits.iter() {
        if sub != 0 {
            size += 1 << sub;
        }
    }
    Ok(size)
}

/// 为每个根槽求其二级表索引位宽 = 该槽最长码长 - 8。
fn collect_subtable_bits(lengths: &[u32], counts: &[u32; 16], subtable_bits: &mut [u8]) {
    subtable_bits.fill(0);
    let mut next = [0u32; 16];
    let mut code = 0u32;
    for length in 1..16usize {
        code = (code + counts[length - 1]) << 1;
        next[length] = code;
    }
    for (_, &length) in lengths.iter().enumerate() {
        if length == 0 {
            continue;
        }
        let canonical = next[length as usize];
        next[length as usize] += 1;
        if length > ROOT_BITS {
            let slot = (reverse_bits(canonical, length) & ((1 << ROOT_BITS) - 1)) as usize;
            subtable_bits[slot] = subtable_bits[slot].max((length - ROOT_BITS) as u8);
        }
    }
}

fn reverse_bits(value: u32, count: u32) -> u32 {
    let mut reversed = 0u32;
    for i in 0..count {
        reversed = (reversed << 1) | ((value >> i) & 1);
    }
    reversed
}

/// 填充一张解码表（已校验码长），返回 (root_mask, single_symbol)。single_symbol<0 表示非单符号。
fn fill_huffman_table(lengths: &[u32], table: &mut [u32]) -> Result<(u32, i32)> {
    let mut counts = [0u32; 16];
    let mut max_length = 0u32;
    let mut symbol_count = 0usize;
    let mut only_symbol = 0u32;
    for (symbol, &length) in lengths.iter().enumerate() {
        if length > 15 {
            return Err(Vp8LError::new("VP8L 码长非法"));
        }
        if length != 0 {
            counts[length as usize] += 1;
            max_length = max_length.max(length);
            symbol_count += 1;
            only_symbol = symbol as u32;
        }
    }

    if symbol_count <= 1 {
        // 单符号（或无符号→符号 0）不耗位。
        table[0] = only_symbol << 16;
        return Ok((0, only_symbol as i32));
    }

    let mut next = [0u32; 16];
    let mut code = 0u32;
    for length in 1..=max_length {
        code = (code + counts[(length - 1) as usize]) << 1;
        next[length as usize] = code;
    }
    if code + counts[max_length as usize] != 1u32 << max_length {
        return Err(Vp8LError::new("VP8L 前缀码不完备"));
    }

    let root = (ROOT_BITS).min(max_length) as usize;
    let root_size = 1 << root;
    let mut subtable_bits = [0u8; 1 << ROOT_BITS as usize];
    let mut subtable_start = [0usize; 1 << ROOT_BITS as usize];
    if max_length > ROOT_BITS {
        collect_subtable_bits(lengths, &counts, &mut subtable_bits);
        let mut start = root_size;
        for slot in 0..root_size {
            let sub_bits = subtable_bits[slot];
            if sub_bits != 0 {
                subtable_start[slot] = start;
                table[slot] = ((start as u32) << 16) | SUBTABLE_FLAG | sub_bits as u32;
                start += 1 << sub_bits;
            }
        }
    }

    for (symbol, &length) in lengths.iter().enumerate() {
        if length == 0 {
            continue;
        }
        let reversed = reverse_bits(next[length as usize], length);
        next[length as usize] += 1;
        let entry = ((symbol as u32) << 16) | length;
        if length <= root as u32 {
            let mut slot = reversed as usize;
            while slot < root_size {
                table[slot] = entry;
                slot += 1 << length;
            }
        } else {
            let slot = (reversed & ((1 << ROOT_BITS) - 1)) as usize;
            let sub_len = 1usize << subtable_bits[slot];
            let base = subtable_start[slot];
            let mut index = (reversed >> ROOT_BITS) as usize;
            while index < sub_len {
                table[base + index] = entry;
                index += 1 << (length - ROOT_BITS);
            }
        }
    }

    Ok(((root_size - 1) as u32, -1))
}

// ---------------------------------------------------------------------------
// 前缀码读取 + 组读取（对齐 C# ReadHuffmanCode / ReadHuffmanGroup）
// ---------------------------------------------------------------------------

const CODE_LENGTH_CODE_ORDER: [usize; 19] = [17, 18, 0, 1, 2, 3, 4, 5, 16, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];

/// 读一个前缀码并追加解码表；返回 (table_offset, root_mask, single_symbol)。
fn read_huffman_code(
    reader: &mut BitReader<'_>,
    alphabet_size: u32,
    tables: &mut HuffmanTables,
) -> Result<(usize, u32, i32)> {
    let mut lengths = vec![0u32; alphabet_size as usize];
    if reader.read_bit()? != 0 {
        // 简单码：1 或 2 符号。
        let count = reader.read_bit()? + 1;
        let first_wide = reader.read_bit()?;
        let first = reader.read_bits(1 + 7 * first_wide)?;
        if first >= alphabet_size {
            return Err(Vp8LError::new("VP8L 简单前缀符号越界"));
        }
        lengths[first as usize] = 1;
        if count == 2 {
            let second = reader.read_bits(8)?;
            if second >= alphabet_size {
                return Err(Vp8LError::new("VP8L 简单前缀符号越界"));
            }
            lengths[second as usize] = 1;
        }
        return build_huffman_table(&lengths, tables);
    }

    // 普通码：code-length 树。
    let mut code_length_lengths = [0u32; 19];
    let number_of_code_lengths = 4 + reader.read_bits(4)?;
    for i in 0..number_of_code_lengths {
        code_length_lengths[CODE_LENGTH_CODE_ORDER[i as usize]] = reader.read_bits(3)?;
    }

    // code-length 树 ≤7 位，无需二级表；128 项足够。
    let mut code_length_table = [0u32; 1 << 7];
    let code_length_mask = fill_huffman_table(&code_length_lengths, &mut code_length_table)?.0;

    let length_n_bits;
    if reader.read_bit()? == 0 {
        length_n_bits = 0;
    } else {
        length_n_bits = 2 + 2 * reader.read_bits(3)?;
    }
    let max_symbol = if length_n_bits == 0 {
        alphabet_size
    } else {
        2 + reader.read_bits(length_n_bits)?
    };
    if max_symbol > alphabet_size {
        return Err(Vp8LError::new("VP8L 前缀字母表过大"));
    }

    let mut index = 0usize;
    let mut symbols_read = 0u32;
    let mut previous = 8u32;
    while symbols_read < max_symbol && index < alphabet_size as usize {
        let value = reader.read_symbol(&code_length_table, code_length_mask)?;
        symbols_read += 1;
        if value < 16 {
            lengths[index] = value;
            index += 1;
            if value != 0 {
                previous = value;
            }
        } else if value == 16 {
            let repeat = 3 + reader.read_bits(2)?;
            let end = index + repeat as usize;
            if end > alphabet_size as usize {
                return Err(Vp8LError::new("VP8L 前缀字母表溢出 (16-repeat)"));
            }
            lengths[index..end].fill(previous);
            index = end;
        } else {
            // 17 或 18：清零重复。
            let repeat = if value == 17 { 3 + reader.read_bits(3)? } else { 11 + reader.read_bits(7)? };
            let end = index + repeat as usize;
            if end > alphabet_size as usize {
                return Err(Vp8LError::new("VP8L 前缀字母表溢出 (17/18-repeat)"));
            }
            lengths[index..end].fill(0);
            index = end;
        }
    }

    Ok(build_huffman_table(&lengths, tables)?)
}

/// 在表池里为 lengths 构建表；返回 (offset, root_mask, single_symbol)。
/// 码长来自位流（攻击者可控），Kraft 不完备 / 码长非法 → 结构化 Err（不 panic，
/// 上层按「不可接管 → 回 image-webp 兜底」处理）。K42。
fn build_huffman_table(
    lengths: &[u32],
    tables: &mut HuffmanTables,
) -> Result<(usize, u32, i32)> {
    let size = huffman_table_size(lengths)?;
    tables.ensure(size);
    let (root_mask, single) =
        fill_huffman_table(lengths, &mut tables.data[tables.used..tables.used + size])?;
    let offset = tables.used;
    tables.used += size;
    Ok((offset, root_mask, single))
}

fn read_huffman_group(reader: &mut BitReader<'_>, green_alphabet: u32, tables: &mut HuffmanTables) -> Result<HuffmanGroup> {
    let (green, green_mask, _) = read_huffman_code(reader, green_alphabet, tables)?;
    let (red, red_mask, red_single) = read_huffman_code(reader, 256, tables)?;
    let (blue, blue_mask, blue_single) = read_huffman_code(reader, 256, tables)?;
    let (alpha, alpha_mask, alpha_single) = read_huffman_code(reader, 256, tables)?;
    let (distance, distance_mask, _) = read_huffman_code(reader, 40, tables)?;
    let trivial_literal = red_single >= 0 && blue_single >= 0 && alpha_single >= 0;
    let literal = if trivial_literal {
        ((alpha_single as u32) << 24) | ((red_single as u32) << 16) | (blue_single as u32)
    } else {
        0
    };
    Ok(HuffmanGroup {
        green,
        red,
        blue,
        alpha,
        distance,
        green_mask,
        red_mask,
        blue_mask,
        alpha_mask,
        distance_mask,
        trivial_literal,
        literal,
    })
}

// ---------------------------------------------------------------------------
// 图像流解码（对齐 C# DecodeImageStream / DecodePixels）
// ---------------------------------------------------------------------------

fn select_group(
    groups: &[Option<HuffmanGroup>],
    entropy: &[u32],
    entropy_width: u32,
    prefix_bits: u32,
    x: u32,
    y: u32,
) -> Result<usize> {
    let ex = (x >> prefix_bits) as usize;
    let ey = (y >> prefix_bits) as usize;
    let idx = ((entropy[ey * entropy_width as usize + ex] >> 8) & 0xffff) as usize;
    if idx >= groups.len() || groups[idx].is_none() {
        return Err(Vp8LError::new("VP8L meta prefix 码越界"));
    }
    Ok(idx)
}

fn group_ref(groups: &[Option<HuffmanGroup>], idx: usize) -> &HuffmanGroup {
    // 前置：select_group 只返回 `idx < len && groups[idx].is_some()` 的组。
    // 无检查引用取下沉 `simd::raw::option_slice_ref`（内联后同 get_unchecked+unwrap_unchecked
    // 逐指令；组引用仅在块边界/拷贝后解析，非每像素热点）。
    super::simd::raw::option_slice_ref(groups, idx)
}

/// 解码一个熵编码子图（宽高为编码尺寸）。
fn decode_image_stream(
    reader: &mut BitReader<'_>,
    width: u32,
    height: u32,
    has_meta_prefix: bool,
) -> Result<Vec<u32>> {
    let pixel_count = (width as usize) * (height as usize);
    let mut result = vec![0u32; pixel_count];
    decode_image_stream_into(reader, width, height, has_meta_prefix, &mut result, None)?;
    Ok(result)
}

/// 版本：写入外部缓冲（缓冲长度须恰为 width*height；供主图像流在颜色索引
/// 原地扩展时分配 pixelCount 尺寸、只填前 encoded 个）。
/// `profile`：仅 `stage_profile` feature 时由顶层探针传入 `Some`；其余路径 `None` 透传
/// （结构体无条件编译，feature 关闭零运行时影响）。
#[allow(clippy::too_many_arguments)]
fn decode_image_stream_into(
    reader: &mut BitReader<'_>,
    width: u32,
    height: u32,
    has_meta_prefix: bool,
    result: &mut [u32],
    profile: Option<&mut Vp8lStageProfile>,
) -> Result<()> {
    if width == 0 || height == 0 || u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Err(Vp8LError::new("VP8L 子图像尺寸非法"));
    }
    if result.len() != (width as usize) * (height as usize) {
        return Err(Vp8LError::new("VP8L 子图像输出缓冲尺寸不符"));
    }
    let pixel_count = result.len();

    let mut entropy: Option<Vec<u32>> = None;
    let mut groups: Vec<Option<HuffmanGroup>> = Vec::new();
    let mut tables = HuffmanTables::new();
    let mut cache: Option<Vec<u32>> = None;
    let group_count;
    let entropy_width;
    let prefix_bits;

    // 颜色缓存。
    let cache_bits;
    if reader.read_bit()? != 0 {
        cache_bits = reader.read_bits(4)?;
        if !(1..=11).contains(&cache_bits) {
            return Err(Vp8LError::new("VP8L 颜色缓存尺寸非法"));
        }
    } else {
        cache_bits = 0;
    }

    if has_meta_prefix && reader.read_bit()? != 0 {
        prefix_bits = reader.read_bits(3)? + 2;
        entropy_width = (width + (1 << prefix_bits) - 1) >> prefix_bits;
        let entropy_height = (height + (1 << prefix_bits) - 1) >> prefix_bits;
        if u64::from(entropy_width) * u64::from(entropy_height) > MAX_PIXELS {
            return Err(Vp8LError::new("VP8L meta prefix 尺寸非法"));
        }
        let entropy_image = decode_image_stream(reader, entropy_width, entropy_height, false)?;
        let mut max = 0;
        for &e in &entropy_image {
            max = max.max((e >> 8) & 0xffff);
        }
        group_count = max + 1;
        entropy = Some(entropy_image);
    } else {
        prefix_bits = 31;
        entropy_width = 1;
        group_count = 1;
    }

    let green_alphabet = 256 + 24 + if cache_bits == 0 { 0 } else { 1 << cache_bits };
    // 组资源护栏：熵图中未实际引用的组下标不建表、不占表池（对齐 libwebp 对
    // num_htree_groups_max 的收缩）。否则稀疏高位下标（如 0xffff）会诱使位流驱动
    // 65536 组全建表，用 ~KB 级恶意位流放大 ~百 MB 分配（K42：畸形输入不 OOM）。
    let used = match &entropy {
        Some(img) => {
            let mut used = vec![false; group_count as usize];
            for &e in img {
                let idx = ((e >> 8) & 0xffff) as usize;
                if idx < used.len() {
                    used[idx] = true;
                }
            }
            used
        }
        None => vec![true],
    };
    // 组头按位流顺序 0..group_count 逐个读取（顺序即组号）；未用组读入共享 scratch
    // 后丢弃，只消费同量位，不推进 `tables`——表池分配上限 = 实际引用组数。
    groups.resize_with(group_count as usize, || None);
    let mut scratch = HuffmanTables::new();
    for i in 0..group_count {
        if used[i as usize] {
            groups[i as usize] = Some(read_huffman_group(reader, green_alphabet, &mut tables)?);
        } else {
            read_huffman_group(reader, green_alphabet, &mut scratch)?;
            scratch.used = 0;
        }
    }

    let cache_length = if cache_bits == 0 { 0 } else { 1 << cache_bits };
    if cache_length != 0 {
        cache = Some(vec![0; cache_length as usize]);
    }

    let single_group = [0u32];
    let entropy_arr: &[u32] = match &entropy {
        Some(e) => e,
        None => &single_group,
    };
    decode_pixels(
        reader,
        result,
        pixel_count,
        width,
        entropy_arr,
        entropy.is_some(),
        entropy_width,
        prefix_bits,
        &groups,
        group_count,
        &tables,
        cache.as_deref_mut(),
        cache_bits,
        profile,
    )?;
    reader.throw_if_truncated()?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
// 隔离熵主环的寄存器分配：内联进 decode_image_stream_into 后巨型函数（~1600 行 asm）
// 逼迫 LLVM 把 loop 常数（buffer/bits/position/x/y/g/tbl/result）全栈式溢出重载。
// 独立成函数后 LLVM 只面对本循环活区间，栈流量应显著下降（asm 审计举证见报告）。
#[inline(never)]
fn decode_pixels(
    reader: &mut BitReader<'_>,
    result: &mut [u32],
    pixel_count: usize,
    width: u32,
    entropy: &[u32],
    spatial_groups: bool,
    entropy_width: u32,
    prefix_bits: u32,
    groups: &[Option<HuffmanGroup>],
    _group_count: u32,
    tables: &HuffmanTables,
    mut cache: Option<&mut [u32]>,
    cache_bits: u32,
    mut profile: Option<&mut Vp8lStageProfile>,
) -> Result<()> {
    // feature 关闭时 `profile` 未消费传播，借位一次防 unused 告警（零运行成本）。
    #[cfg(not(feature = "stage_profile"))]
    let _ = &mut profile;
    let (mut buffer, mut bits) = reader.load();
    let block_mask = (1u32 << prefix_bits).wrapping_sub(1);
    let mut position = 0usize;
    let mut cached = 0usize;
    let mut x = 0u32;
    let mut y = 0u32;
    let mut group_idx = select_group(groups, entropy, entropy_width, prefix_bits, 0, 0)?;
    // 块边界处解析一次组引用（select_group 已保证非 None）；组切换点同步刷新。
    let mut g = group_ref(groups, group_idx);
    // 表池基址提升（对齐 C# `ref uint tableBase`）：解码期不再增长，偏移恒有效。
    let tbl = tables.data.as_ptr();

    while position < pixel_count {
        if bits < 32 {
            reader.refill(&mut buffer, &mut bits)?;
        }
        let symbol = read_symbol(tbl, g.green, g.green_mask, &mut buffer, &mut bits);
        if symbol < 256 {
            let color;
            if g.trivial_literal {
                color = g.literal | (symbol << 8);
            } else {
                let red = read_symbol(tbl, g.red, g.red_mask, &mut buffer, &mut bits);
                if bits < 32 {
                    reader.refill(&mut buffer, &mut bits)?;
                }
                let blue = read_symbol(tbl, g.blue, g.blue_mask, &mut buffer, &mut bits);
                let alpha = read_symbol(tbl, g.alpha, g.alpha_mask, &mut buffer, &mut bits);
                color = (alpha << 24) | (red << 16) | (symbol << 8) | blue;
            }
            // 写入在 `position < pixel_count` 循环前置下恒界内（无检查写下沉 leaf 门面）。
            super::simd::raw::unchecked_write(result, position, color);
            position += 1;
            x += 1;
            if x == width {
                x = 0;
                y += 1;
                if position == pixel_count {
                    break;
                }
                group_idx = select_group(groups, entropy, entropy_width, prefix_bits, x, y)?;
                g = group_ref(groups, group_idx);
            } else if (x & block_mask) == 0 {
                group_idx = select_group(groups, entropy, entropy_width, prefix_bits, x, y)?;
                g = group_ref(groups, group_idx);
            }
            continue;
        }

        if symbol < 280 {
            // 长度码。
            let length = prefix_value(symbol - 256, &mut buffer, &mut bits) as usize;
            if bits < 32 {
                reader.refill(&mut buffer, &mut bits)?;
            }
            let distance_code = read_symbol(tbl, g.distance, g.distance_mask, &mut buffer, &mut bits);
            if bits < 32 {
                reader.refill(&mut buffer, &mut bits)?;
            }
            let dist = distance(prefix_value(distance_code, &mut buffer, &mut bits), width)? as usize;
            if dist > position {
                return Err(Vp8LError::new("VP8L 回看距离非法"));
            }
            if length > pixel_count - position {
                return Err(Vp8LError::new("VP8L 回看拷贝越界"));
            }
            copy_backward(result, position, dist, length);
            position += length;
            if spatial_groups && position < pixel_count {
                y = (position / width as usize) as u32;
                x = (position % width as usize) as u32;
                group_idx = select_group(groups, entropy, entropy_width, prefix_bits, x, y)?;
                g = group_ref(groups, group_idx);
            }
            continue;
        }

        // 颜色缓存码：绿字母表 ≥ 280 必有缓存。
        #[cfg(feature = "stage_profile")]
        let cache_t0 = std::time::Instant::now();
        let cache = cache.as_mut().ok_or_else(|| Vp8LError::new("VP8L 颜色缓存缺失"))?;
        let shift = 32 - cache_bits;
        #[cfg(feature = "stage_profile")]
        let fill_from = cached;
        for i in cached..position {
            let value = result[i];
            let hash = (0x1e35a7bdu32.wrapping_mul(value)) >> shift;
            cache[hash as usize] = value;
        }
        cached = position;
        super::simd::raw::unchecked_write(result, position, cache[(symbol - 280) as usize]);
        #[cfg(feature = "stage_profile")]
        {
            if let Some(p) = profile.as_mut() {
                p.cache_symbols += 1;
                p.cache_fill_pixels += (position - fill_from) as u64;
                p.cache_ns += cache_t0.elapsed().as_nanos() as u64;
            }
        }
        position += 1;
        x += 1;
        if x == width {
            x = 0;
            y += 1;
            if position == pixel_count {
                break;
            }
            group_idx = select_group(groups, entropy, entropy_width, prefix_bits, x, y)?;
            g = group_ref(groups, group_idx);
        } else if (x & block_mask) == 0 {
            group_idx = select_group(groups, entropy, entropy_width, prefix_bits, x, y)?;
            g = group_ref(groups, group_idx);
        }
    }

    reader.store(buffer, bits);
    Ok(())
}

// ---------------------------------------------------------------------------
// 顶层：主图像流（含变换）+ 读头 + ToRgba
// ---------------------------------------------------------------------------

fn read_transform(reader: &mut BitReader<'_>, ty: u8, width: u32, height: u32) -> Result<Transform> {
    if ty == 0 || ty == 1 {
        let bits = reader.read_bits(3)? + 2;
        let block_width = (width + (1 << bits) - 1) >> bits;
        let block_height = (height + (1 << bits) - 1) >> bits;
        let data = decode_image_stream(reader, block_width, block_height, false)?;
        return Ok(Transform {
            ty,
            width,
            height,
            bits,
            data: Some(data),
            table: None,

        });
    }
    if ty == 2 {
        return Ok(Transform {
            ty,
            width,
            height,
            bits: 0,
            data: None,
            table: None,

        });
    }
    // ty == 3：color indexing。
    let table_size = reader.read_bits(8)? + 1;
    let bits_per_pixel = if table_size <= 2 {
        3
    } else if table_size <= 4 {
        2
    } else if table_size <= 16 {
        1
    } else {
        0
    };
    let table_width = table_size;
    let table_data = decode_image_stream(reader, table_width, 1, false)?;
    let mut table = vec![0u32; table_size as usize];
    let mut previous = 0u32;
    for (i, &packed) in table_data.iter().enumerate() {
        let alpha = ((packed >> 24) & 0xff) + ((previous >> 24) & 0xff);
        let red = ((packed >> 16) & 0xff) + ((previous >> 16) & 0xff);
        let green = ((packed >> 8) & 0xff) + ((previous >> 8) & 0xff);
        let blue = (packed & 0xff) + (previous & 0xff);
        previous = ((alpha & 0xff) << 24) | ((red & 0xff) << 16) | ((green & 0xff) << 8) | (blue & 0xff);
        table[i] = previous;
    }
    Ok(Transform {
        ty,
        width,
        height,
        bits: bits_per_pixel,
        data: None,
        table: Some(table),

    })
}

/// 无初始化像素缓冲（跳过 `vec![0u32; n]` 的整帧清零——解码随后全量覆写 = 纯浪费）。
/// sound：`u32` 无 Drop glue，中途 Err 时整体 Drop 仅归还分配、不读元素；解码期「读未写槽」
/// 的唯一风险由 [`decode_image_into`] 的全覆写不变式排除。
fn uninit_u32_vec(len: usize) -> Vec<u32> {
    let mut v = Vec::with_capacity(len);
    // set_len 下沉 `simd::raw::vec_set_len`（叶子安全门面；u32 无 Drop glue，不变量见上）。
    super::simd::raw::vec_set_len(&mut v, len);
    v
}

/// 解码主图像（含变换序列）为 ARGB 像素（内部分配，无预清零）。
fn decode_image(reader: &mut BitReader<'_>, width: u32, height: u32) -> Result<Vec<u32>> {
    let pixel_count = (width as usize) * (height as usize);
    let mut pixels = uninit_u32_vec(pixel_count);
    decode_image_into(reader, width, height, &mut pixels)?;
    Ok(pixels)
}

/// 解码主图像（含变换序列）到外部缓冲 `pixels`（长度须恰为 width*height）。
///
/// 全覆写不变式（预清零 / 调用方复用缓冲均安全）：
/// - 熵流按位序**顺序**写满 `pixels[..encoded_count]`（`decode_pixels` 的
///   `while position < pixel_count` 单调推进，position 恒先写后读；`copy_backward`
///   只读已写前缀）；
/// - 若存在颜色索引变换（缩小编码尺寸 → encoded_count < pixel_count），其按编码逆序
///   应用时**先于**其它逆变换把尾段 `pixels[encoded_count..]` 全量写出（对齐 libwebp
///   的 ApplyInverseTransforms 尺寸演绎），故任何读邻像素的逆变换发生前全数组已写；
/// - 中途 `Err` 直接返回，调用方 Drop/忽略，不读未写槽。
fn decode_image_into(
    reader: &mut BitReader<'_>,
    width: u32,
    height: u32,
    pixels: &mut [u32],
) -> Result<()> {
    let pixel_count = (width as usize) * (height as usize);
    if pixels.len() != pixel_count {
        return Err(Vp8LError::new("VP8L 主图像输出缓冲尺寸不符"));
    }
    let mut transforms: Vec<Transform> = Vec::new();
    let mut seen = [false; 4];
    let mut encoded_width = width;
    // 刻意只缩横不缩纵（对齐 libwebp DecodeImageStream 的「颜色索引仅缩宽」怪癖）：
    // 这是 decode_image_into「全覆写不变式」的几何前提，勿单独改纵。
    let encoded_height = height;

    while reader.read_bit()? != 0 {
        let ty = reader.read_bits(2)? as usize;
        if seen[ty] {
            return Err(Vp8LError::new("VP8L 变换序列非法（重复类型）"));
        }
        seen[ty] = true;
        let transform = read_transform(reader, ty as u8, encoded_width, encoded_height)?;
        if ty == 3 {
            encoded_width = (encoded_width + (1 << transform.bits) - 1) >> transform.bits;
        }
        transforms.push(transform);
    }

    let encoded_count = (encoded_width as usize) * (encoded_height as usize);
    decode_image_stream_into(
        reader,
        encoded_width,
        encoded_height,
        true,
        &mut pixels[..encoded_count],
        None,
    )?;
    for t in transforms.iter().rev() {
        apply_transform(pixels, t)?;
    }
    Ok(())
}

fn read_header(bitstream: &[u8]) -> Result<(u32, u32, bool)> {
    if bitstream.len() < 5 || bitstream[0] != 0x2f {
        return Err(Vp8LError::new("VP8L 流头截断"));
    }
    let header = u32::from_le_bytes([bitstream[1], bitstream[2], bitstream[3], bitstream[4]]);
    let width = 1 + (header & 0x3fff);
    let height = 1 + ((header >> 14) & 0x3fff);
    let has_alpha = ((header >> 28) & 1) != 0;
    if (header >> 29) != 0 || u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Err(Vp8LError::new("VP8L 尺寸或版本非法"));
    }
    Ok((width, height, has_alpha))
}

/// ARGB 像素 → RGBA8 字节（内存序交换 B/R；SIMD 版 + 标量回退见 `simd::argb_to_rgba`）。
/// 生产路径 `decode` 已改走原地 `argb_to_rgba_inplace`；此处仅 stage_profile 探针使用。
#[cfg_attr(not(feature = "stage_profile"), allow(dead_code))]
fn to_rgba(pixels: &[u32], out: &mut [u8]) {
    super::simd::argb_to_rgba(pixels, out)
}

/// 解码 VP8L 位流（VP8L chunk 完整载荷，首字节 0x2f）为 RGBA8。
pub(crate) fn decode(bitstream: &[u8]) -> Result<DecodedImage> {
    let (width, height, has_alpha) = read_header(bitstream)?;
    let pixel_count = (width as usize) * (height as usize);
    let mut reader = BitReader::new(&bitstream[5..]);
    let mut pixels = decode_image(&mut reader, width, height)?;
    if pixels.len() != pixel_count {
        return Err(Vp8LError::new("VP8L 像素数不一致"));
    }
    // 原地 ARGB→RGBA8（字节序交换）后直接把 u32 缓冲收进 `RgbaBytes`（零拷贝）：省一次
    // 整帧 RGBA 分配 + 全量拷贝（大图整帧各 ~36MB 级）。逐字节=旧 `to_rgba` 路径。
    // 归还 Layout = 原始 Vec<u32> 分配（align 4），由 RgbaBytes 内部 Vec 的 Drop 保证同契约。
    super::simd::argb_to_rgba_inplace(&mut pixels);
    let rgba = RgbaBytes::from_argb_vec(pixels);
    Ok(DecodedImage {
        width,
        height,
        has_alpha,
        rgba,
    })
}

/// 直接解码 VP8L 位流到调用方 RGBA8 缓冲（`C# DecodeTo` 镜像：暖态零分配）。
/// `out` 长度须恰为 w*h*4。`out` 4 字节对齐时整帧零拷贝直写 + 原地字节交换；未对齐时
/// 兜底 = 内部缓冲 + 一次拷贝（常态调用方 `Vec<u8>` 均满对齐，走快道）。
pub(crate) fn decode_into_rgba8(bitstream: &[u8], out: &mut [u8]) -> Result<()> {
    let (width, height, _has_alpha) = read_header(bitstream)?;
    let pixel_count = (width as usize) * (height as usize);
    if out.len() != pixel_count * 4 {
        return Err(Vp8LError::new("VP8L 输出缓冲长度须为 w*h*4"));
    }
    if out.as_ptr() as usize % std::mem::align_of::<u32>() == 0 {
        // 把调用方缓冲重借为 `&mut [u32]`：熵解码/逆变换以 ARGB 直写其中（全覆写不变式见
        // decode_image_into），再原地字节交换为 RGBA8。重借下沉 `simd::raw::bytes_as_mut_u32`
        // （语义等同 bytemuck cast_slice_mut），重借存活期不触碰 `out`；缓冲由调用方持有、
        // 无 Drop 冲突。调用点已保证 4 字节对齐与容量（out.len() == pixel_count*4）。
        let px = super::simd::raw::bytes_as_mut_u32(out, pixel_count);
        let mut reader = BitReader::new(&bitstream[5..]);
        decode_image_into(&mut reader, width, height, px)?;
        super::simd::argb_to_rgba_inplace(px);
        Ok(())
    } else {
        let mut reader = BitReader::new(&bitstream[5..]);
        let mut pixels = decode_image(&mut reader, width, height)?;
        if pixels.len() != pixel_count {
            return Err(Vp8LError::new("VP8L 像素数不一致"));
        }
        super::simd::argb_to_rgba_inplace(&mut pixels);
        let rgba = RgbaBytes::from_argb_vec(pixels);
        out.copy_from_slice(rgba.as_bytes());
        Ok(())
    }
}

/// 校准单次 `Instant::now()` 调用固有开销（ns/次），用于扣除逐缓存符号计时器的测量噪音。
/// `black_box` 阻止 QPC 调用被优化消除。仅 `stage_profile` feature 编译。
#[cfg(feature = "stage_profile")]
fn calibrate_instant_ns() -> u64 {
    use std::time::Instant;
    const ITERS: u32 = 40_000;
    let t0 = Instant::now();
    for _ in 0..ITERS {
        std::hint::black_box(Instant::now());
    }
    t0.elapsed().as_nanos() as u64 / ITERS as u64
}

/// 阶段分账解码（`stage_profile` feature 门控；供 imgbench 探针，K59 统一基座）。
/// 与 [`decode`] 语义逐字节一致，另按【变换头 / 主熵流(+色缓存段) / 逆变换 / 收尾拷贝】
/// 统计各段墙钟。`bitstream` 为 VP8L chunk 载荷（首字节 0x2f）。仅计时（丢弃像素）。
#[cfg(feature = "stage_profile")]
pub fn stage_decode(bitstream: &[u8]) -> Result<Vp8lStageProfile> {
    let (width, height, _) = read_header(bitstream)?;
    let pixel_count = (width as usize) * (height as usize);
    let mut rgba = vec![0u8; pixel_count * 4];
    let profile = stage_decode_into(bitstream, &mut rgba)?;
    std::hint::black_box(&rgba);
    Ok(profile)
}

/// `stage_decode` 的写缓冲版：`rgba` 长度须为 w*h*4，解码后将 RGBA8 像素写入（供 K27 证据
/// 测试与生产路径 [`decode`] 逐字节对拍）。
#[cfg(feature = "stage_profile")]
pub fn stage_decode_into(bitstream: &[u8], rgba: &mut [u8]) -> Result<Vp8lStageProfile> {
    use std::time::Instant;
    let (width, height, _has_alpha) = read_header(bitstream)?;
    let pixel_count = (width as usize) * (height as usize);
    let mut reader = BitReader::new(&bitstream[5..]);
    let t0 = Instant::now();

    let mut transforms: Vec<Transform> = Vec::new();
    let mut seen = [false; 4];
    let mut encoded_width = width;
    let encoded_height = height;
    while reader.read_bit()? != 0 {
        let ty = reader.read_bits(2)? as usize;
        if seen[ty] {
            return Err(Vp8LError::new("VP8L 变换序列非法（重复类型）"));
        }
        seen[ty] = true;
        let transform = read_transform(&mut reader, ty as u8, encoded_width, encoded_height)?;
        if ty == 3 {
            encoded_width = (encoded_width + (1 << transform.bits) - 1) >> transform.bits;
        }
        transforms.push(transform);
    }
    let transforms_head_ns = t0.elapsed().as_nanos() as u64;

    let mut pixels = uninit_u32_vec(pixel_count);
    let encoded_count = (encoded_width as usize) * (encoded_height as usize);
    let mut profile = Vp8lStageProfile {
        width,
        height,
        ..Default::default()
    };
    // 重置符号级探针计数，只统计主熵流窗口。
    #[cfg(feature = "stage_profile")]
    {
        SYMBOL_READS.with(|c| c.set(0));
        SUBTABLE_HITS.with(|c| c.set(0));
    }
    let t1 = Instant::now();
    decode_image_stream_into(
        &mut reader,
        encoded_width,
        encoded_height,
        true,
        &mut pixels[..encoded_count],
        Some(&mut profile),
    )?;
    reader.throw_if_truncated()?;
    #[cfg(feature = "stage_profile")]
    {
        profile.symbol_reads = SYMBOL_READS.with(|c| c.get());
        profile.subtable_hits = SUBTABLE_HITS.with(|c| c.get());
    }
    let entropy_wall = t1.elapsed().as_nanos() as u64;
    // 扣除逐缓存符号计时器固有开销（同进程 QPC 校准）：缓存分支内 1 次 `t0.elapsed()` 落入
    // cache_ns、1 次 `cache_t0 = Instant::now()` 落入熵墙钟。二者各以 cache_symbols×单次 QPC
    // 从相应桶扣回，得真实段耗时（probe 口径；feature 关闭无此逻辑）。
    let per_instant = calibrate_instant_ns();
    let cache_timer_cost = profile.cache_symbols * per_instant;
    // cache 桶扣其内置 `t0.elapsed()` 开销；熵墙钟内含 cache 分支的 `cache_t0 = Instant::now()`
    // （cache_symbols×QPC），从熵桶再扣一份——先按未扣 cache 原始值相减，避免两次扣除相消。
    let cache_raw = profile.cache_ns;
    profile.cache_ns = cache_raw.saturating_sub(cache_timer_cost);
    profile.entropy_ns =
        entropy_wall.saturating_sub(cache_raw).saturating_sub(cache_timer_cost);

    let t2 = Instant::now();
    for t in transforms.iter().rev() {
        apply_transform(&mut pixels, t)?;
    }
    let transforms_ns = t2.elapsed().as_nanos() as u64;

    let t3 = Instant::now();
    if rgba.len() != pixel_count * 4 {
        return Err(Vp8LError::new("VP8L 探针输出缓冲尺寸不符"));
    }
    to_rgba(&pixels, rgba);
    let to_rgba_ns = t3.elapsed().as_nanos() as u64;

    profile.transforms_head_ns = transforms_head_ns;
    profile.transforms_ns = transforms_ns;
    profile.to_rgba_ns = to_rgba_ns;
    profile.total_ns = Instant::now().duration_since(t0).as_nanos() as u64;
    Ok(profile)
}

/// 解码 ALPH chunk 的 VP8L 载荷（**无 5 字节 VP8L 头**，尺寸由调用方给定）为 alpha 平面
/// （ALPH 语义 = 绿色通道，RFC 9649 §2）。对应 C# `DecodeAlphaTo`（复用变换序列 + 熵解码）。
pub(crate) fn decode_alpha(stream: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    if (u64::from(width) * u64::from(height)) > MAX_PIXELS {
        return Err(Vp8LError::new("VP8L alpha 像素超限"));
    }
    let pixel_count = (width as usize) * (height as usize);
    let mut reader = BitReader::new(stream);
    let pixels = decode_image(&mut reader, width, height)?;
    if pixels.len() != pixel_count {
        return Err(Vp8LError::new("VP8L alpha 像素数不一致"));
    }
    let mut alpha = vec![0u8; pixel_count];
    for (i, &p) in pixels.iter().enumerate() {
        alpha[i] = ((p >> 8) & 0xff) as u8;
    }
    Ok(alpha)
}

/// 诊断：VP8L 解码分阶段合计（熵流 vs 逆变换 vs 输出抽 RGB；`#[ignore]` 手动 release 跑）。
#[cfg(test)]
mod perf_stage_tests {
    use super::*;

    #[test]
    #[ignore = "手动运行"]
    fn lossless_stage_split() {
        use std::time::Instant;
        // 大图生成语料（bench 产物；缺则用小 fixture 替代）。
        let gen_path = std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../target/imgbench-corpus/WEBP/gen28-photo-lossless.webp"
        ));
        let (name, data) = if gen_path.is_file() {
            ("gen28-photo-lossless", std::fs::read(gen_path).unwrap())
        } else {
            let p = webp_fixtures_support::webp_fixture_path("lossless-near60.webp")
                .unwrap_or_else(|| panic!("夹具缺失（外部语料）：lossless-near60.webp"));
            ("lossless-near60", std::fs::read(p).unwrap())
        };
        let c = crate::codecs::webp::container::parse(&data).unwrap();
        let bs = c.bitstream.unwrap().to_vec();
        let (w, h, _ha) = read_header(&bs).unwrap();
        for _ in 0..3 {
            let mut reader = BitReader::new(&bs[5..]);
            let t0 = Instant::now();
            let mut transforms: Vec<Transform> = Vec::new();
            let mut seen = [false; 4];
            let mut encoded_width = w;
            let mut encoded_height = h;
            while reader.read_bit().unwrap() != 0 {
                let ty = reader.read_bits(2).unwrap() as usize;
                if seen[ty] {
                    panic!("重复变换");
                }
                seen[ty] = true;
                let transform = read_transform(&mut reader, ty as u8, encoded_width, encoded_height).unwrap();
                if ty == 3 {
                    encoded_width = (encoded_width + (1 << transform.bits) - 1) >> transform.bits;
                }
                transforms.push(transform);
            }
            eprintln!("VP8L_TRANSFORMS {:?}", transforms.iter().map(|t| (t.ty, t.bits)).collect::<Vec<_>>());
            let t1 = Instant::now();
            let pixel_count = (w as usize) * (h as usize);
            let mut pixels = vec![0u32; pixel_count];
            let encoded_count = (encoded_width as usize) * (encoded_height as usize);
            decode_image_stream_into(
                &mut reader,
                encoded_width,
                encoded_height,
                true,
                &mut pixels[..encoded_count],
                None,
            )
            .unwrap();
            let t2 = Instant::now();
            for t in transforms.iter().rev() {
                let t0 = Instant::now();
                if t.ty == 0 {
                    let data = t.data.as_ref().unwrap();
                    let block_w = ((w as usize) + (1usize << t.bits) - 1) >> t.bits;
                    let block_h = ((h as usize) + (1usize << t.bits) - 1) >> t.bits;
                    let mut hist = [0u32; 14];
                    for by in 0..block_h {
                        for bx in 0..block_w {
                            let m = (data[by * block_w + bx] >> 8) & 0xff;
                            if m < 14 {
                                hist[m as usize] += 1;
                            }
                        }
                    }
                    eprintln!("VP8L_PREDICTOR_MODE_HIST {hist:?} blocks={}x{}", block_w, block_h);
                }
                apply_transform(&mut pixels, t).unwrap();
                eprintln!(
                    "VP8L_TRANSFORM_TIME ty={} bits={} {:.3}ms",
                    t.ty,
                    t.bits,
                    t0.elapsed().as_secs_f64() * 1e3
                );
            }
            let t3 = Instant::now();
            let mut rgba = vec![0u8; pixels.len() * 4];
            to_rgba(&pixels, &mut rgba);
            let _ = rgba.len();
            eprintln!(
                "VP8L_STAGE {name} {}x{}: transforms_head={:.3}ms entropy={:.3}ms transforms={:.3}ms to_rgba={:.3}ms total={:.3}ms",
                w,
                h,
                t1.duration_since(t0).as_secs_f64() * 1e3,
                t2.duration_since(t1).as_secs_f64() * 1e3,
                t3.duration_since(t2).as_secs_f64() * 1e3,
                Instant::now().duration_since(t3).as_secs_f64() * 1e3,
                Instant::now().duration_since(t0).as_secs_f64() * 1e3,
            );
        }
    }

    /// 探针路径 `stage_decode` 与生产路径 `decode` 逐字节一致（K27：证明性断言落仓可复跑）。
    /// `stage_profile` feature 下跑：`cargo test -p kimg vp8l --features stage_profile -- --ignored --test-threads=1 stage_decode_matches_decode`
    #[cfg(feature = "stage_profile")]
    #[test]
    #[ignore = "手动运行（需 feature + 外部语料）"]
    fn stage_decode_matches_decode() {
        let gen_path = std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../target/imgbench-corpus/WEBP/gen28-photo-lossless.webp"
        ));
        let data = if gen_path.is_file() {
            std::fs::read(gen_path).unwrap()
        } else {
            let p = webp_fixtures_support::webp_fixture_path("lossless-near60.webp")
                .unwrap_or_else(|| panic!("夹具缺失（外部语料）：lossless-near60.webp"));
            std::fs::read(p).unwrap()
        };
        let c = crate::codecs::webp::container::parse(&data).unwrap();
        let bs = c.bitstream.unwrap().to_vec();
        let prod = decode(&bs).unwrap();
        let mut rgba = vec![0u8; prod.rgba.len()];
        let prof = stage_decode_into(&bs, &mut rgba).unwrap();
        assert_eq!((prof.width, prof.height), (prod.width, prod.height));
        // ARGB→RGBA8 字节逐字节一致（探针路径 == 生产路径）。
        assert_eq!(&rgba[..], prod.rgba.as_bytes());
        eprintln!(
            "VP8L_STAGE_DECODE_PARITY_OK {}x{} entropy={}ms cache={}ms transforms={}ms total={}ms",
            prof.width,
            prof.height,
            (prof.entropy_ns + prof.cache_ns) / 1_000_000,
            prof.cache_ns / 1_000_000,
            prof.transforms_ns / 1_000_000,
            prof.total_ns / 1_000_000,
        );
    }
}

// ---------------------------------------------------------------------------
// 单元测试：bit reader / 距离 / 前缀值 / 表构建对已知样本
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {    use super::*;

    #[test]
    fn bit_reader_order() {
        // 字节 0b1011_0101 LSB-first：读 4 位 → 0b0101=5，再 3 位 → 0b011=3，再 1 位 → 1。
        let data = [0b1011_0101u8];
        let mut r = BitReader::new(&data);
        assert_eq!(r.read_bits(4).unwrap(), 0b0101);
        assert_eq!(r.read_bits(3).unwrap(), 0b011);
        assert_eq!(r.read_bits(1).unwrap(), 1);
    }

    #[test]
    fn prefix_value_codes() {
        let mut b = 0xFFFF_FFFFu64; // 低位全 1
        let mut bits = 32u32;
        // code 0..3 → 1..4 不耗位。
        assert_eq!(prefix_value(0, &mut b, &mut bits), 1);
        assert_eq!(prefix_value(3, &mut b, &mut bits), 4);
        // code 4：extraBits=1, offset=(2+(4&1))<<1=4, extra=1 → 6。
        assert_eq!(prefix_value(4, &mut b, &mut bits), 6);
        assert_eq!(bits, 31); // 前 3 个不耗位 + 这里 extra 1
    }

    #[test]
    fn distance_map_values() {
        assert_eq!(distance(1, 16).unwrap(), 16); // (0,1) → 16
        assert_eq!(distance(2, 16).unwrap(), 1); // (1,0) → 1
        assert_eq!(distance(4, 16).unwrap(), 15); // (-1,1) → -1+16=15
        assert_eq!(distance(121, 16).unwrap(), 1); // >120 → 121-120
    }

    #[test]
    fn copy_backward_overlap() {
        let mut p = vec![0u32; 8];
        p[0] = 1;
        p[1] = 2;
        p[2] = 3;
        copy_backward(&mut p, 3, 2, 5);
        // source = 1; 重复 {2,3} → [_, 1,2,3, 2,3,2,3]
        assert_eq!(&p[3..8], &[2, 3, 2, 3, 2]);
    }

    /// 畸形/截断位流 → 结构化 Err 或不 panic（K42：不 panic 不 OOM）。
    #[test]
    fn malformed_truncated_never_panics() {
        let Some(p) = webp_fixtures_support::webp_fixture_path("lossless-palette-16.webp") else {
            eprintln!("SKIP malformed_truncated_never_panics: lossless-palette-16.webp 外部语料缺失");
            return;
        };
        let data = std::fs::read(&p).unwrap();
        // 从 open 的真实 fixture 切出 VP8L 载荷。
        let c = crate::codecs::webp::container::parse(&data).unwrap();
        let bs = c.bitstream.unwrap();
        // 各种截断前缀：均不得 panic（Err 或 Ok 均可）。
        for cut in [0usize, 1, 2, 4, 5, 6, 7, 8, 9, bs.len() / 2, bs.len() - 1, bs.len()] {
            let _ = decode(&bs[..cut.min(bs.len())]);
        }
        // 全零 / 全 FF 的假流。
        let _ = decode(&[0u8; 32]);
        let _ = decode(&[0x2f, 0x00, 0x00, 0x00, 0x00]);
        let _ = decode(&[0x2f, 0xff, 0xff, 0xff, 0x7f]);
    }

    /// LSB-first 位写入器（VP8L 位流）：先写的位落在低比特位。
    struct BitWriter {
        bytes: Vec<u8>,
        cur: u8,
        nbits: u32,
    }

    impl BitWriter {
        fn new() -> Self {
            BitWriter { bytes: Vec::new(), cur: 0, nbits: 0 }
        }
        fn flush(&mut self) {
            if self.nbits > 0 {
                self.bytes.push(self.cur);
                self.cur = 0;
                self.nbits = 0;
            }
        }
        fn bit(&mut self, b: u32) {
            self.cur |= ((b & 1) as u8) << self.nbits;
            self.nbits += 1;
            if self.nbits == 8 {
                self.flush();
            }
        }
        fn bits(&mut self, v: u32, n: u32) {
            for i in 0..n {
                self.bit((v >> i) & 1);
            }
        }
        fn finish(mut self) -> Vec<u8> {
            self.flush();
            self.bytes
        }
    }

    /// 畸形输入：普通 Huffman 码长序列构成「非完备前缀码」（Kraft 不闭合）。
    /// 必须走 Err（→ image-webp 兜底）而不是 panic（K42：不 panic 不 OOM）。
    #[test]
    fn incomplete_huffman_code_does_not_panic() {
        let mut w = BitWriter::new();
        // VP8L 头：1x1 无 alpha 无版本位。
        w.bytes.push(0x2f);
        w.bytes.extend_from_slice(&0u32.to_le_bytes());
        w.bit(0); // 无变换
        w.bit(0); // 无颜色缓存
        w.bit(0); // 无 meta prefix
        w.bit(0); // green 普通码
        w.bits(2, 4); // num_code_lengths = 6
        // code-length-code 码长：order[0..6] = [17,18,0,1,2,3]
        // L17=2, L18=0, L0=0, L1=1, L2=2, L3=0 → 树符号 {1:1位, 2:2位, 17:2位}（完备）。
        w.bits(2, 3);
        w.bits(0, 3);
        w.bits(0, 3);
        w.bits(1, 3);
        w.bits(2, 3);
        w.bits(0, 3);
        w.bit(0); // 不用 length 位 → max_symbol = 整字母表
        // green 码长序列：符号 1（码0）→ 长 1；符号 2（canonical '10'，反序发射 1,0）→ 长 2；
        // 其余 278 个清 0（符号 17 反序 '11' + 3 位 repeat）。[1,2,0,0,...] Kraft 不闭合。
        w.bit(0);
        w.bit(1);
        w.bit(0);
        for _ in 0..27 {
            w.bit(1);
            w.bit(1); // 17：repeat 10
            w.bits(7, 3);
        }
        w.bit(1);
        w.bit(1); // 17：repeat 8 → index=280
        w.bits(5, 3);
        let data = w.finish();
        let r = decode(&data);
        // 不允许 panic；Err（降级）或 Ok 均可。
        assert!(r.is_err(), "非完备前缀码应返回 Err 而非 panic 或 Ok");
    }

    /// 元前缀熵图引用高位组下标（65535），但位流只含少量实际组头。
    /// 回归护栏：未用组走 scratch 丢弃路径（不撑爆表池），高位组正确解析，不 panic。
    #[test]
    fn sparse_meta_prefix_group_index_bounded() {
        fn simple_group_headers(w: &mut BitWriter, count: u32, green_symbol: u32) {
            // 每个组 = 5 棵树，一律 1-symbol 简单码（3 标志位 + 8 符号位）。
            for _ in 0..count {
                for (sym, n) in [(green_symbol, 8), (0x12, 8), (0x34, 8), (0xFF, 8), (0x00, 8)] {
                    w.bit(1); // simple
                    w.bit(0); // 1 symbol
                    w.bit(1); // wide（8 位）
                    w.bits(sym, n);
                }
            }
        }

        let mut w = BitWriter::new();
        // 2x2 无 alpha 头。
        w.bytes.push(0x2f);
        w.bytes.extend_from_slice(&0x4001u32.to_le_bytes());
        w.bit(0); // 无变换
        w.bit(0); // 无颜色缓存
        w.bit(1); // meta prefix
        w.bits(0, 3); // prefix_bits = 2 → 熵图 1x1
        // 熵图（1x1，无潜 meta）：像素 0x00FFFF00 → 组下标 (>>8)&0xffff = 0xffff。
        w.bit(0); // 缓存 0
        for (sym, n) in [(0xFF, 8), (0xFF, 8), (0x00, 8), (0x00, 8), (0x00, 8)] {
            w.bit(1);
            w.bit(0);
            w.bit(1);
            w.bits(sym, n);
        }
        // 组头：0..=65535，全部同构 1-symbol——未用下标丢弃，65535 真实建表。
        simple_group_headers(&mut w, 65536, 0xAB);
        let data = w.finish();
        let img = decode(&data).expect("稀疏高位组下标应可解码（不 panic 不 OOM）");
        assert_eq!((img.width, img.height), (2, 2));
        // 4 像素全为 0xFF1234AB → RGBA 字节 {0x12, 0xAB, 0x34, 0xFF}。
        assert!(img.rgba.as_bytes().chunks_exact(4).all(|p| p == [0x12, 0xAB, 0x34, 0xFF]));
    }

    #[test]
    fn simple_huffman_table() {
        // 两个符号全 1 位：符号 0 码长 1、符号 1 码长 1。
        let lengths = [1u32, 1];
        let size = huffman_table_size(&lengths).unwrap();
        assert_eq!(size, 2);
        let (mask, single) = fill_huffman_table(&lengths, &mut vec![0u32; size]).unwrap();
        assert_eq!(single, -1);
        assert_eq!(mask, 1);
        let mut table = [0u32; 2];
        fill_huffman_table(&lengths, &mut table).unwrap();
        // canonical: 符号0码=0(bit-rev 0)→slot0; 符号1码=1→slot1。
        assert_eq!(table[0] >> 16, 0);
        assert_eq!(table[1] >> 16, 1);
    }
}

// ---------------------------------------------------------------------------
// P2 parity：自研 vp8l vs image-webp（kimg WebPDecoder 现路径）逐字节 + dwebp golden
// ---------------------------------------------------------------------------

#[cfg(test)]
mod parity_tests {
    use std::io::Cursor;

    use sha2::{Digest, Sha256};

    use super::decode;
    use super::decode_into_rgba8;
    use super::webp_fixtures_support;
    use crate::codecs::webp::container;
    use crate::codecs::webp::WebPDecoder;
    use crate::ImageDecoder as _;

    /// golden TSV 保留在仓内；图片取外部语料。
    const GOLDEN_TSV: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/webp/webp-oracle-golden.tsv");

    /// 直连 image_webp（dev oracle）解为 RGBA8 字节。
    fn reference_rgba(data: &[u8]) -> (u32, u32, Vec<u8>) {
        reference_rgba_impl(data)
    }

    /// 直连 image_webp（dev oracle）解为 RGBA8（eager：解码失败即 panic）。
    /// 与 [`reference_rgba_opt`] 双胞胎收敛为单一实现（见 opt），此处仅 expect 薄包装，
    /// 两侧调用点结果语义不变（eager / Option 经包装体现，断言数据不变）。
    fn reference_rgba_impl(data: &[u8]) -> (u32, u32, Vec<u8>) {
        reference_rgba_opt(data).expect("image_webp 解码失败")
    }

    /// 报告首个像素差（对齐 SimdCodecs DescribeRgbaMismatch 口径）。
    fn describe_diff(name: &str, ours: &[u8], ref_: &[u8]) {
        let n = ours.len().min(ref_.len());
        let mut first = None;
        let mut count = 0usize;
        let mut max_diff = 0i32;
        for i in 0..n {
            let d = ours[i] as i32 - ref_[i] as i32;
            if d != 0 {
                if first.is_none() {
                    first = Some(i);
                }
                count += 1;
                max_diff = max_diff.max(d.abs());
            }
        }
        let len_diff = ours.len() as i64 - ref_.len() as i64;
        eprintln!(
            "VP8L_DIFF {name} bytes_mismatch={count} max|d|={max_diff} len_diff={len_diff} first_pixel={:?}",
            first.map(|i| (i / 4, i % 4))
        );
    }

    #[test]
    fn vp8l_static_lossless_parity_vs_imagewebp() {
        let mut checked = 0usize;
        let mut skipped = 0usize;
        let mut fails = Vec::new();
        // 从外部语料目录按名收集（含用户语料 / imgbench-corpus / SimdCodecs 夹具）。
        let mut names = webp_fixtures_support::webp_fixture_names();
        names.sort();
        for name in names {
            if !name.ends_with(".webp") || name.starts_with("anim-") {
                continue;
            }
            let Some(path) = webp_fixtures_support::webp_fixture_path(&name) else {
                eprintln!("SKIP {name} 外部语料缺失");
                skipped += 1;
                continue;
            };
            let data = std::fs::read(&path).unwrap();
            let c = match container::parse(&data) {
                Ok(c) => c,
                Err(_) => continue,
            };
            if !c.family.is_static_lossless() {
                continue;
            }
            let bitstream = c.bitstream.unwrap();
            let golden = webp_fixtures_support::is_golden_static_name(&name);
            let ours = match decode(bitstream) {
                Ok(o) => o,
                Err(e) => {
                    // 金标准（SimdCodecs 52 图名）解码失败=回归；越界语料 SKIP 不判失败。
                    if golden {
                        fails.push((name.clone(), format!("自研解码失败 {e}")));
                    } else {
                        eprintln!("SKIP {name} 自研 VP8L 解码失败 {e}（非金标准外部语料）");
                        skipped += 1;
                    }
                    continue;
                }
            };
            let Some((rw, rh, ref_rgba)) = reference_rgba_opt(&data) else {
                eprintln!("SKIP {name} image-webp 解码失败");
                skipped += 1;
                continue;
            };
            if (ours.width, ours.height) != (rw, rh) {
                fails.push((name.clone(), format!("尺寸 {}x{} vs {}x{}", ours.width, ours.height, rw, rh)));
                continue;
            }
            if ours.rgba.as_bytes() != &ref_rgba[..] {
                describe_diff(&name, ours.rgba.as_bytes(), &ref_rgba);
                // 金标准不匹配=失败；越界语料（gen*/用户大图）仅记录不判失败。
                if golden {
                    fails.push((name.clone(), "像素逐字节不一致".into()));
                } else {
                    eprintln!("VP8L_EXTRA_DIFF {name}（非金标准外部语料，仅记录）");
                }
            } else {
                checked += 1;
            }
        }
        for (name, why) in &fails {
            eprintln!("VP8L_PARITY_FAIL\t{name}\t{why}");
        }
        eprintln!("VP8L_PARITY_CHECKED\t{checked}, skipped={skipped}");
        // 11 个 lossless 静态 fixture（SimdCodecs 金光标）应全过。
        assert!(checked >= 11, "lossless 静态 parity 应全过，当前 {checked}（fail {}）", fails.len());
        assert!(fails.is_empty(), "{} 个 lossless fixture parity 失败", fails.len());
    }

    /// 直写 [`decode_into_rgba8`] 与 [`decode`] 路径逐字节一致；调用方复用缓冲（预填垃圾，
    /// 验证全覆写不变式不读旧值）与未对齐（偏移 1 → 兜底拷贝路径）均应产出同字节。
    #[test]
    fn decode_into_rgba8_matches_decode() {
        let mut checked = 0usize;
        let mut names = webp_fixtures_support::webp_fixture_names();
        names.sort();
        for name in names {
            if !name.ends_with(".webp") || name.starts_with("anim-") {
                continue;
            }
            let Some(path) = webp_fixtures_support::webp_fixture_path(&name) else {
                continue;
            };
            let data = std::fs::read(&path).unwrap();
            let c = match container::parse(&data) {
                Ok(c) => c,
                Err(_) => continue,
            };
            if !c.family.is_static_lossless() {
                continue;
            }
            let bs = c.bitstream.unwrap();
            let expect = match decode(bs) {
                Ok(o) => o.rgba.as_bytes().to_vec(),
                Err(_) => continue, // 非金标准越界语料解不了 → 跳过
            };
            // 对齐快道 + 复用缓冲预填垃圾（证明不复用旧值）。
            let mut aligned = vec![0xA5u8; expect.len()];
            decode_into_rgba8(bs, &mut aligned).unwrap();
            assert_eq!(aligned, expect, "{name} 对齐直写应等于 decode");
            // 未对齐兜底（偏移到 4 字节不对齐）。
            let mut unaligned = vec![0xA5u8; expect.len() + 3];
            let align = std::mem::align_of::<u32>() as usize;
            let mut start = 0usize;
            while (unaligned.as_ptr() as usize + start) % align == 0 {
                start += 1;
                assert!(start <= 3);
            }
            decode_into_rgba8(bs, &mut unaligned[start..start + expect.len()]).unwrap();
            assert_eq!(
                &unaligned[start..start + expect.len()],
                &expect[..],
                "{name} 未对齐兜底应等于 decode"
            );
            checked += 1;
        }
        eprintln!("WEBP_DECODE_INTO_RGBA8_CHECKED\t{checked}");
        assert!(checked >= 1, "应至少对拍 1 个 VP8L 静态 fixture（当前 {checked}）");
    }

    /// 直连 image_webp（dev oracle）解为 RGBA8；解码失败返回 None（供 SKIP 分流）。
    fn reference_rgba_opt(data: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
        let mut dec = image_webp::WebPDecoder::new(Cursor::new(data)).ok()?;
        let (w, h) = dec.dimensions();
        let has_alpha = dec.has_alpha();
        let n = dec.output_buffer_size()?;
        let mut buf = vec![0u8; n];
        dec.read_image(&mut buf).ok()?;
        if has_alpha {
            Some((w, h, buf))
        } else {
            let mut rgba = Vec::with_capacity(buf.len() / 3 * 4);
            for px in buf.chunks_exact(3) {
                rgba.extend_from_slice(&[px[0], px[1], px[2], 255]);
            }
            Some((w, h, rgba))
        }
    }

    #[test]
    fn vp8l_static_lossless_matches_dwebp_golden() {
        let text = std::fs::read_to_string(GOLDEN_TSV).unwrap();
        let mut rows = std::collections::BTreeMap::new();
        for line in text.lines() {
            let line = line.trim_end();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let p: Vec<&str> = line.split('\t').collect();
            rows.insert(p[0].to_lowercase(), (p[4].to_lowercase(), p[5].to_lowercase()));
        }
        let mut checked = 0usize;
        let mut skipped = 0usize;
        // 以静态 golden 清单为准：只对 golden 里确为静态无损的 fixture 校验（外部语料里
        // 的 gen*/用户大图不在 golden 内，不参与本轮）。缺失 → SKIP。
        for (name_lc, _) in rows.iter() {
            let Some(path) = webp_fixtures_support::webp_fixture_path(name_lc) else {
                eprintln!("SKIP {name_lc} 外部语料缺失");
                skipped += 1;
                continue;
            };
            let data = std::fs::read(&path).unwrap();
            let c = match container::parse(&data) {
                Ok(c) => c,
                Err(_) => continue,
            };
            if !c.family.is_static_lossless() {
                continue;
            }
            let ours = decode(c.bitstream.unwrap()).expect("自研解码");
            let mut h = Sha256::new();
            h.update(ours.rgba.as_bytes());
            let hash = format!("{:x}", h.finalize());
            let (nofancy, fancy) = &rows[name_lc];
            // lossless：fancy==nofancy；dwebp RGBA 与自研必须逐字节同哈希。
            assert_eq!(hash, *fancy, "{name_lc} 自研与 dwebp golden(fancy) 不一致");
            assert_eq!(hash, *nofancy, "{name_lc} 自研与 dwebp golden(nofancy) 不一致");
            checked += 1;
        }
        eprintln!("VP8L_GOLDEN_CHECKED\t{checked}, skipped={skipped}");
        assert_eq!(checked, 11, "lossless golden 覆盖应恰为 11");
    }

    #[test]
    fn ours_backend_routes_static_lossless_through_vp8l() {
        // 显式 OURS：静态无损必须走自研路径（与 image_webp oracle 逐字节同）。
        let name = "lossless-near60.webp";
        let Some(path) = webp_fixtures_support::webp_fixture_path(name) else {
            eprintln!("SKIP {name} 外部语料缺失");
            return;
        };
        let data = std::fs::read(&path).unwrap();

        crate::codecs::webp::set_webp_backend(crate::codecs::webp::BACKEND_OURS);
        let ours = reference_rgba_via_decoder(&data);
        crate::codecs::webp::set_webp_backend(crate::codecs::webp::BACKEND_AUTO);
        let forced = reference_rgba_impl(&data).2;

        assert_eq!(ours, forced, "OURS 路由（VP8L 静态）应与 image_webp oracle 逐字节一致");
    }

    /// 经 kimg WebPDecoder（后端由当前静态开关决定）解为 RGBA8。
    fn reference_rgba_via_decoder(data: &[u8]) -> Vec<u8> {
        let dec = WebPDecoder::new(Cursor::new(data)).expect("kimg WebPDecoder 解码失败");
        let has_alpha = dec.color_type() == crate::ColorType::Rgba8;
        let mut buf = vec![0u8; dec.total_bytes() as usize];
        dec.read_image(&mut buf).expect("kimg read_image 失败");
        if has_alpha {
            buf
        } else {
            let mut rgba = Vec::with_capacity(buf.len() / 3 * 4);
            for px in buf.chunks_exact(3) {
                rgba.extend_from_slice(&[px[0], px[1], px[2], 255]);
            }
            rgba
        }
    }

    /// P2 性能初步（可后置基线）：ours(VP8L 标量) vs image-webp（kimg 现路径）lossless 场景
    /// 每 fixture 多轮中位（debug）。仅记录相对倍率（标量版略慢属正常，P7 再优化）。
    #[test]
    #[ignore]
    fn perf_ours_vs_imagewebp_lossless() {
        use std::time::Instant;
        let mut ours_total = std::time::Duration::ZERO;
        let mut iw_total = std::time::Duration::ZERO;
        let mut count = 0usize;
        // 5 轮取中位（debug 编译噪声大，取 min 更有代表性）。
        let mut names = webp_fixtures_support::webp_fixture_names();
        names.sort();
        for _round in 0..5 {
            for name in &names {
                let Some(path) = webp_fixtures_support::webp_fixture_path(name) else {
                    continue;
                };
                let data = std::fs::read(&path).unwrap();
                let c = match container::parse(&data) {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                if !c.family.is_static_lossless() {
                    continue;
                }
                let bs = c.bitstream.unwrap();
                let t0 = Instant::now();
                let _ = decode(bs).unwrap();
                let t1 = Instant::now();
                let _ = reference_rgba(&data);
                let t2 = Instant::now();
                ours_total += t1 - t0;
                iw_total += t2 - t1;
                count += 1;
            }
        }
        let ours_med = ours_total / count as u32;
        let iw_med = iw_total / count as u32;
        eprintln!(
            "VP8L_PERF ours={}us imagewebp={}us ratio={:.3} (count={} rounds=5 min 中位约)",
            ours_med.as_micros(),
            iw_med.as_micros(),
            ours_med.as_secs_f64() / iw_med.as_secs_f64(),
            count
        );
    }
}

