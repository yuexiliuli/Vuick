//! VP8 有损静态解码器（标量正确版）：RFC 6386 keyframe 全链路（帧内预测 / 4x4 IDCT + WHT /
//! 解量化 / 环路滤波 / YUV→RGBA fancy）。
//!
//! 移植蓝本：SimdCodecs `Vp8BoolDecoder.cs`/`Vp8Decoder.cs`/`Vp8LoopFilter.cs`/`Vp8Tables.cs`
//! 与 `WebpDecoder.cs` 有损路径的标量实现（MIT），已由 SimdCodecs 测试集对 dwebp golden
//! 逐字节验证；本模块保持其算术/舍入/顺序语义完全一致（任务主判：与 image-webp 逐字节一致）。
//!
//! 阶段划分（对应派单 V1..V5）：
//! - V1：bool 解码器 + 静态表 + 帧头/分区解析（本文件首部 + 单测）。
//! - V2：宏块重建（帧内预测 + IDCT/WHT + 解量化 + 写回 Y/U/V 平面）。
//! - V3：环路滤波（Simple + Normal）。
//! - V4：YUV→RGBA（fancy 上采样 + 定点转换，供 `webp::mod` 集成）。
//! - V5：`webp_oracle` 侧对拍收口（tests 层）。
//!
//! 输入安全（K42）：所有尺寸乘法 `checked`、分区/表越界在入口校验后抛结构化 `Vp8Error`；
//! 畸形输入不 panic 不 OOM（上层按「不可接管 → 回 image-webp 兜底」处理）。

use std::fmt;

use super::container::MAX_PIXELS;

// ---------------------------------------------------------------------------
// 错误与解码结果
// ---------------------------------------------------------------------------

/// VP8 解码错误（结构化，供上层映射为「降级 image-webp」或解析失败）。
#[derive(Debug, Clone)]
pub(crate) struct Vp8Error(pub(crate) String);

impl fmt::Display for Vp8Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Vp8Error {}

impl Vp8Error {
    fn new<S: Into<String>>(msg: S) -> Self {
        Vp8Error(msg.into())
    }
}

type Result<T> = std::result::Result<T, Vp8Error>;

/// VP8 解码产物：`rgba` 为「行定稿直出」产物（fancy 上采样 + YUV→RGBA4），由
/// `decode_from_header` 在滤波每宏块行后即时写出并缓存——`yuv_to_rgba_fancy`
/// 取用后置空，避免二次全帧 pass。opaque 有损路径（经 `decode_vp8`，T1 起）与
/// alpha/兼容路径一样填 `rgba`（w*h*4，α=255 熔入发射内层、单遍直出）；`rgb`（RGB3 直出，
/// T1 前 opaque 形态）恒空（dormancy，K24）。`y/u/v` 紧密 4:2:0 平面（环路滤波后）在生产
/// 路径默认为空（直出为输出，紧密平面纯浪费，见 `decode_vp8` 的 `need_tight`），仅供
/// `decode_vp8_tight`（测试/兼容 API）与手工构造回退
/// `yuv_to_rgba_fancy`/`yuv_to_rgb_fancy` 使用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Vp8Yuv {
    pub width: u32,
    pub height: u32,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
    pub rgba: Vec<u8>,
    /// opaque 有损直出 RGB3（w*h*3）；非 RGB3 路径恒空。
    pub rgb: Vec<u8>,
}

// ---------------------------------------------------------------------------
// 静态表
// ---------------------------------------------------------------------------

// 静态表随 vp8_tables.rs 整体引入；COEFF_BANDS / COEFFICIENT_TREE / Y_MODE_TREE / PCAT1/2 为
// 规范参考数据（当前实现内联化了树遍历与 cat1/2，定义了带状偏移与树读），保留供对照不删。
#[allow(dead_code)]
mod vp8_tables {
    include!("vp8_tables.rs");
}

use vp8_tables::*;

// ---------------------------------------------------------------------------
// V1：RFC 6386 §7 boolean 熵解码器（宽窗口版）
// ---------------------------------------------------------------------------

/// 熵 renorm 跳过开关：`KIMG_WEBP_ENT_RENORM_SKIP`（默认「开」＝启用跳过）：
/// range≥128 时 renorm 位移恒为 0（no-op），跳过 lzcnt+shl；设 `0`/`false`/`off` 装回
/// 无条件 renorm（同二进制 A/B 与双路径字节恒等交叉验证）。首次访问读一次 env 并缓存
/// （同 simd.rs `webp_no_simd`/`yuv_u16_enabled` 的惰性 OnceLock 姿势，不引新依赖）。
/// `#[cfg(test)]` 可经 `set_renorm_skip_override` 进程内翻转（优先于 env，RC2 同窗 A/B 用）。
#[inline]
fn ent_renorm_skip() -> bool {
    #[cfg(test)]
    if let Some(v) = renorm_skip_override::get() {
        return v;
    }
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let v = std::env::var("KIMG_WEBP_ENT_RENORM_SKIP")
            .unwrap_or_default()
            .trim()
            .to_string();
        !(v == "0" || v.eq_ignore_ascii_case("false") || v.eq_ignore_ascii_case("off"))
    })
}

/// `#[cfg(test)]` renorm_skip 进程内覆盖（优先于 env；RC2 同窗 A/B 用，同 simd `part_override` 姿势）。
#[cfg(test)]
mod renorm_skip_override {
    use std::sync::atomic::{AtomicI8, Ordering::Relaxed};
    static V: AtomicI8 = AtomicI8::new(-1);
    pub fn set(v: Option<bool>) {
        let val = match v {
            Some(true) => 1,
            Some(false) => 0,
            None => -1,
        };
        V.store(val, Relaxed);
    }
    pub fn get() -> Option<bool> {
        match V.load(Relaxed) {
            1 => Some(true),
            0 => Some(false),
            _ => None,
        }
    }
}

#[cfg(test)]
pub(crate) fn set_renorm_skip_override(v: Option<bool>) {
    renorm_skip_override::set(v);
}

/// RFC 6386 §7 宽窗口 bool 解码器：`value` 持有 8 位比较窗口 + 其下 `bits` 位后续输入，
/// 一次 refill 读 7 字节。越过数据末尾后输入读作零位（§7.3 参考行为）。`range` 保持「减一」：
/// split-1 = ((range-1)*p)>>8 恰一次乘移，判定 = 窗口 > split-1。
pub(crate) struct Vp8BoolDecoder<'a> {
    data: &'a [u8],
    /// 已消费字节数。
    pos: usize,
    /// 窗口（值 + 后续位）。
    value: u64,
    /// 有效位数（可为负：低位未读，下次 refill）。
    bits: i32,
    /// 范围减一（恒在 [127,255] 区间外延）。
    range_minus1: u32,
}

impl<'a> Vp8BoolDecoder<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Vp8BoolDecoder {
            data,
            pos: 0,
            value: 0,
            bits: -8,
            range_minus1: 254,
        }
    }

    /// 冷路径（帧头/分区解析）`&mut` 外壳，委托热路径按值实现，保证单一语义源。
    #[inline(always)]
    pub(crate) fn read_bit(&mut self, probability: u8) -> u32 {
        let (s, bit) = self.read_bit_val(probability);
        *self = s;
        bit
    }

    /// 热路径按值读位（概率 0..=255）：熵态 {value,bits,range_minus1} 由调用者以值携带
    /// （`Copy`，小结构），整个宏块内联后落寄存器、无跨调用点内存往返——对照 C#
    /// `ReadBit(ref decoder, ref value, ref bits, ref range, prob)` 的 Load/Store 模式。
    /// cold 入口委托标量热路径 `ent_read_bit`（算术单源），一次调用只读一次 OnceLock。
    #[inline(always)]
    pub(crate) fn read_bit_val(mut self, probability: u8) -> (Self, u32) {
        let renorm_skip = ent_renorm_skip();
        let bit = ent_read_bit(
            self.data,
            &mut self.pos,
            &mut self.value,
            &mut self.bits,
            &mut self.range_minus1,
            renorm_skip,
            probability,
        );
        (self, bit)
    }

    /// 热路径按值读「概率 128 符号位」并算术取符号（C# `ReadSigned`）：符号是抛硬币，
    /// 分支约半数预测失败；用 `mask = (split - (window))>>31` 全表达式消除分支，
    /// 与 `read_bit(128)+符号分支` 逐位同语义。`magnitude` 为系数幅值，返回取符号结果。
    /// cold 入口委托标量热路径 `ent_read_signed`（算术单源）。
    #[inline(always)]
    pub(crate) fn read_signed_val(mut self, magnitude: i32) -> (Self, i32) {
        let v = ent_read_signed(
            self.data,
            &mut self.pos,
            &mut self.value,
            &mut self.bits,
            &mut self.range_minus1,
            magnitude,
        );
        (self, v)
    }

    /// 读带符号值：`bitCount` 位无符号 + 1 位符号（概率 128）。
    pub(crate) fn read_signed_literal(&mut self, bit_count: u32) -> i32 {
        let value = self.read_literal(bit_count);
        if self.read_bit(128) != 0 {
            -value
        } else {
            value
        }
    }

    /// 读 `bit_count` 位等概率字面量。
    pub(crate) fn read_literal(&mut self, bit_count: u32) -> i32 {
        let mut v = 0i32;
        for _ in 0..bit_count {
            v = (v << 1) | self.read_bit(128) as i32;
        }
        v
    }
}

impl fmt::Debug for Vp8BoolDecoder<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Vp8BoolDecoder")
            .field("pos", &self.pos)
            .field("remaining", &(self.data.len() - self.pos))
            .field("bits", &self.bits)
            .finish()
    }
}

impl Clone for Vp8BoolDecoder<'_> {
    fn clone(&self) -> Self {
        Vp8BoolDecoder {
            data: self.data,
            pos: self.pos,
            value: self.value,
            bits: self.bits,
            range_minus1: self.range_minus1,
        }
    }
}

// 全字段 Copy（含 &'a [u8]），热路径按值携带熵态必须。
impl Copy for Vp8BoolDecoder<'_> {}

// ---------------------------------------------------------------------------
// 熵热路径标量化（TOP1/TOP4：反 40B-byval 结构 + (Self,u32) 元组逐位回写栈）
// ---------------------------------------------------------------------------
// SimdCodecs 对热环的解法是 `ReadBit(ref Vp8BoolDecoder source, ref ulong value, ref int bits,
// ref uint range, int probability)`：熵态三标量 {value,bits,range} 由调用方局部变量携带（读位
// 时只动局部，宏块进出各一次 Load/Store），`source` 仅保输入游标 {data,pos}，每 56 位
// （refill）才触碰一次。本实现同构：`ent_read_bit`/`ent_read_signed`/`ent_refill` 为纯标量
// in/out（调用方局部变量，mem2reg 恒落寄存器），`data`/`pos` 只在 refill 分支使用——避免
// 旧 `read_bit_val`/`tokens` 的「40B 结构按值 + (Self,u32) 返回值」形态把 {value,bits,range}
// 每比特挤出寄存器经栈 store→load 回环（Haswell store-forward 3-5c，在误预测后清空的
// OoO 窗口全量暴露——照片端被窗口吞掉、高熵噪声端全量显现，正解释 2.09×/1.24× 差序）。
// `renorm_skip`（ent_renorm_skip() 的 OnceLock 判定）由调用方每宏块入口求一次并全程以 bool
// 参数携带（TOP4：主环零 OnceLock load）。cold 路径（帧头解析等）经 `read_bit_val`/
// `read_signed_val` 委托本组函数，算术单源不分裂。

/// refill：`bits < 0` 时读入后续位（快速路径整 8 字节非对齐直读、只消费 7 字节；尾部逐字节、
/// 越过末尾补零）。与 `Vp8BoolDecoder::refill`/`refill_tail` 逐位同算术。
#[inline(always)]
fn ent_refill(data: &[u8], pos: &mut usize, value: &mut u64, bits: &mut i32) {
    if data.len() - *pos >= 8 {
        // 安全性：分支保证 *pos + 8 <= len；`read_unaligned` 自行处理非对齐载荷（同 refill）。
        let raw = unsafe { (data.as_ptr().add(*pos) as *const u64).read_unaligned() };
        let raw = u64::from_be(raw);
        *value = (*value << 56) | (raw >> 8);
        *pos += 7;
        *bits += 56;
    } else {
        loop {
            let next = if *pos < data.len() {
                let b = data[*pos];
                *pos += 1;
                u64::from(b)
            } else {
                0
            };
            *value = (*value << 8) | next;
            *bits += 8;
            if *bits >= 0 {
                break;
            }
        }
    }
}

/// 位读判定前置（单源）：本次符号的 refill + split 预求值，返回（比较窗口值, split-1, rm1）。
/// `ent_read_bit`（cmov 全算形）与 RC3 预测分支形共用同一算术——两者输出逐字节恒等是结构性的
/// （仅控制流不同）。`bit=1` 侧还需 `*value -= (sp+1) << bits`、`range = rm1 - sp`；`bit=0` 侧
/// `range = sp + 1`；renorm 由共享 `ent_renorm_apply` 落 `range_minus1`/`bits`。
#[inline(always)]
fn ent_split_pre(
    data: &[u8],
    pos: &mut usize,
    value: &mut u64,
    bits: &mut i32,
    range_minus1: &mut u32,
    probability: u8,
) -> (u64, u32, u32) {
    stage_clock::bit();
    let prob = u32::from(probability);
    if *bits < 0 {
        ent_refill(data, pos, value, bits);
    }
    let rm1 = *range_minus1;
    let split_minus1 = (rm1 * prob) >> 8;
    (*value >> (*bits as u32), split_minus1, rm1)
}

/// renorm 应用（单源）：`range`（位判定结果）落 `range_minus1`/`bits`。与 read_bit_val 同：
/// `range < 128 || !renorm_skip` 恒等（renorm_skip=false 时时走 lzcnt）。
#[inline(always)]
fn ent_renorm_apply(bits: &mut i32, range_minus1: &mut u32, range: u32, renorm_skip: bool) {
    if range < 128 || !renorm_skip {
        let shift = range.leading_zeros() - 24;
        *range_minus1 = (range << shift) - 1;
        *bits -= shift as i32;
    } else {
        *range_minus1 = range - 1;
    }
}

/// 热路径读位（概率 0..=255）：`read_bit_val` 的标量 in/out 版，逐位算术完全一致，唯熵态不再
/// 装进 40B 结构按值周转。`renorm_skip` 由调用方宏块入口判定一次（TOP4）。**cmov 全算形**：
/// 位判定选 `range`/`bit` 走无分支 select，供抛硬币/近硬币点用（RC3 预测分支形只留给高偏斜点）。
#[inline(always)]
fn ent_read_bit(
    data: &[u8],
    pos: &mut usize,
    value: &mut u64,
    bits: &mut i32,
    range_minus1: &mut u32,
    renorm_skip: bool,
    probability: u8,
) -> u32 {
    let (vs, sp, rm1) = ent_split_pre(data, pos, value, bits, range_minus1, probability);
    let (range, bit) = if vs > u64::from(sp) {
        *value -= (u64::from(sp + 1)) << (*bits as u32);
        (rm1 - sp, 1)
    } else {
        (sp + 1, 0)
    };
    ent_renorm_apply(bits, range_minus1, range, renorm_skip);
    bit
}

/// 热路径读「概率 128 符号位」并算术取符号（`read_signed_val` 的标量 in/out 版，逐位一致）。
#[cfg(feature = "stage_profile")]
macro_rules! probe_skew {
    ($site:expr, $b:expr) => {
        skew_clock::bump($site, $b as u32)
    };
}
#[cfg(not(feature = "stage_profile"))]
macro_rules! probe_skew {
    ($site:expr, $b:expr) => {};
}
#[inline(always)]
fn ent_read_signed(
    data: &[u8],
    pos: &mut usize,
    value: &mut u64,
    bits: &mut i32,
    range_minus1: &mut u32,
    magnitude: i32,
) -> i32 {
    stage_clock::bit();
    if *bits < 0 {
        ent_refill(data, pos, value, bits);
    }
    let rm1 = *range_minus1 as i32;
    let split_minus1 = rm1 >> 1;
    let mask = (split_minus1 - (*value >> (*bits as u32)) as i32) >> 31;
    let dec_bit = (split_minus1 + 1) & mask;
    *value -= (dec_bit as u32 as u64) << (*bits as u32);
    let new_range = (split_minus1 + 1 + ((rm1 - 2 * split_minus1 - 1) & mask)) as u32;
    let sh = 1u32 - (new_range >> 7);
    *range_minus1 = (new_range << sh) - 1;
    *bits -= sh as i32;
    probe_skew!(skew_clock::SIGN, mask as u32);
    (magnitude ^ mask) - mask
}

// ---------------------------------------------------------------------------
// RC3 探针：决策点偏斜计数（仅 `stage_profile` feature；验证「cmov 全算 → 预测分支形」
// 是否值得投，K60 ≥8% 才投）。feature 关闭时 `probe_skew!` 展开为空、本模块不存在，
// 产品路径零代码影响（行为/字节不变）。
// ---------------------------------------------------------------------------
#[cfg(feature = "stage_profile")]
pub(crate) mod skew_clock {
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

    #[derive(Clone, Copy, Debug)]
    pub(crate) struct Counts {
        pub c0: u64,
        pub c1: u64,
    }

    macro_rules! sites_const {
        ($($name:ident = $v:expr),* $(,)?) => {
            $( pub(crate) const $name: usize = $v; )*
        };
    }
    sites_const! {
        EOB = 0, ZERO = 1, ONE = 2, LT5 = 3, TWO = 4, F34 = 5, G5 = 6,
        F67 = 7, F159 = 8, F165 = 9, F145 = 10, CAT8 = 11, CAT9 = 12,
        CAT10 = 13, EXTRA = 14, SIGN = 15, SEGMENT = 16, YMODE = 17,
        UVMODE = 18, SKIP = 19, BMODE = 20,
    }
    /// 决策点总数（与上方站点枚举一一对应）。
    pub(crate) const N: usize = 21;
    pub(crate) const NAMES: [&str; N] = [
        "EOB", "ZERO", "ONE", "LT5", "TWO", "F34", "G5", "F67", "F159", "F165",
        "F145", "CAT8", "CAT9", "CAT10", "EXTRA", "SIGN", "SEGMENT", "YMODE",
        "UVMODE", "SKIP", "BMODE",
    ];

    static C0: [AtomicU64; N] = [const { AtomicU64::new(0) }; N];
    static C1: [AtomicU64; N] = [const { AtomicU64::new(0) }; N];

    #[inline(always)]
    pub(crate) fn bump(site: usize, bit: u32) {
        if bit != 0 {
            C1[site].fetch_add(1, Relaxed);
        } else {
            C0[site].fetch_add(1, Relaxed);
        }
    }

    pub(crate) fn reset() {
        for i in 0..N {
            C0[i].store(0, Relaxed);
            C1[i].store(0, Relaxed);
        }
    }

    pub(crate) fn all() -> Vec<(&'static str, Counts)> {
        let mut out = Vec::with_capacity(N);
        for i in 0..N {
            out.push((NAMES[i], Counts { c0: C0[i].load(Relaxed), c1: C1[i].load(Relaxed) }));
        }
        out
    }
}

// ---------------------------------------------------------------------------
// V1：帧头解析
// ---------------------------------------------------------------------------

/// 段特性（RFC 6386 §10）。
#[derive(Debug, Clone, Default)]
pub(crate) struct SegmentFeatures {
    pub enabled: bool,
    pub update_map: bool,
    pub absolute: bool,
    pub quant: [i16; 4],
    pub filter: [i16; 4],
    pub map_probabilities: [u8; 3],
}

/// 环路滤波参数（RFC 6386 §14.4）。
#[derive(Debug, Clone, Default)]
pub(crate) struct FilterSettings {
    pub simple: bool,
    pub level: u8,
    pub sharpness: u8,
    pub delta_enabled: bool,
    pub ref_deltas: [i8; 4],
    pub mode_deltas: [i8; 4],
}

/// 量化参数（RFC 6386 §14.1）。
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Quantizers {
    pub base: i32,
    pub y_dc_delta: i32,
    pub y2_dc_delta: i32,
    pub y2_ac_delta: i32,
    pub uv_dc_delta: i32,
    pub uv_ac_delta: i32,
}

/// 每个段的解量化因子与环路滤波阈值（每帧一次计算）。
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct SegmentParameters {
    pub y_dc: i32,
    pub y_ac: i32,
    pub y2_dc: i32,
    pub y2_ac: i32,
    pub uv_dc: i32,
    pub uv_ac: i32,
    pub filter_level_i16: u8,
    pub filter_level_i4: u8,
}

/// 解析完毕的帧头 + 分区（V1 产物；V2 起由 `decode_vp8` 消费）。
#[derive(Debug, Clone)]
pub(crate) struct FrameHeader<'a> {
    pub width: u32,
    pub height: u32,
    pub mb_width: usize,
    pub mb_height: usize,
    pub segments: SegmentFeatures,
    pub filter: FilterSettings,
    pub quantizers: Quantizers,
    /// 系数概率（4*8*3*11，eob/zero/one... 11 节点）。
    pub coefficient_probabilities: [u8; 4 * 8 * 3 * 11],
    /// skip 标志是否存在。
    pub skip_flag_present: bool,
    /// skip 概率（不存在时为 0）。
    pub skip_probability: u8,
    /// 宏块数据分区（数量 = partition_count）。
    pub partitions: Vec<&'a [u8]>,
    /// 帧头解码器（宏块头模式继续消费；保留给 V2）。
    pub header: Vp8BoolDecoder<'a>,
}

/// 解析完整 VP8 keyframe 帧头与 token 分区表。
pub(crate) fn parse_frame_header(frame: &[u8]) -> Result<FrameHeader<'_>> {
    if frame.len() < 10 {
        return Err(Vp8Error::new("VP8 keyframe 头被截断"));
    }
    let frame_tag = u32::from(frame[0]) | (u32::from(frame[1]) << 8) | (u32::from(frame[2]) << 16);
    if (frame_tag & 1) != 0 {
        return Err(Vp8Error::new("VP8 帧间预测（非 keyframe）暂不支持"));
    }
    let partition_length = (frame_tag >> 5) as usize;
    if partition_length > frame.len() - 10 || frame[3] != 0x9d || frame[4] != 0x01 || frame[5] != 0x2a {
        return Err(Vp8Error::new("VP8 keyframe 头或首分区非法"));
    }
    let coded_width = u32::from(u16::from_le_bytes([frame[6], frame[7]])) & 0x3fff;
    let coded_height = u32::from(u16::from_le_bytes([frame[8], frame[9]])) & 0x3fff;
    if coded_width == 0 || coded_height == 0 {
        return Err(Vp8Error::new("VP8 尺寸为零"));
    }
    if u64::from(coded_width) * u64::from(coded_height) > MAX_PIXELS {
        return Err(Vp8Error::new("VP8 像素超限"));
    }

    let header_partition = &frame[10..10 + partition_length];
    let mut header = Vp8BoolDecoder::new(header_partition);
    if header.read_bit(128) != 0 {
        return Err(Vp8Error::new("VP8 保留色空间标志非零"));
    }
    let _ = header.read_bit(128);

    let segments = read_segments(&mut header)?;
    let filter = read_filter(&mut header)?;
    let partition_count = 1usize << header.read_literal(2) as usize;
    let quantizers = read_quantizers(&mut header)?;
    // 保留位（RFC 6386 §19.1 之后、系数概率之前）。
    let _ = header.read_bit(128);
    let mut coefficient_probabilities = [128u8; 4 * 8 * 3 * 11];
    coefficient_probabilities.copy_from_slice(&DEFAULT_COEFF_PROBS);
    read_coefficient_updates(&mut header, &mut coefficient_probabilities)?;
    let skip_flag_present = header.read_bit(128) != 0;
    let skip_probability = if skip_flag_present {
        header.read_literal(8) as u8
    } else {
        0
    };

    let token_frame = &frame[10 + partition_length..];
    let partitions = read_partitions(token_frame, partition_count)?;

    Ok(FrameHeader {
        width: coded_width,
        height: coded_height,
        mb_width: ((coded_width + 15) / 16) as usize,
        mb_height: ((coded_height + 15) / 16) as usize,
        segments,
        filter,
        quantizers,
        coefficient_probabilities,
        skip_flag_present,
        skip_probability,
        partitions,
        header,
    })
}

/// RFC 6386 §19.3 段特性。
fn read_segments(d: &mut Vp8BoolDecoder<'_>) -> Result<SegmentFeatures> {
    let enabled = d.read_bit(128) != 0;
    if !enabled {
        return Ok(SegmentFeatures::default());
    }
    let update_map = d.read_bit(128) != 0;
    let update_data = d.read_bit(128) != 0;
    let mut s = SegmentFeatures {
        enabled,
        update_map,
        absolute: false,
        quant: [0; 4],
        filter: [0; 4],
        map_probabilities: [255; 3],
    };
    if update_data {
        s.absolute = d.read_bit(128) != 0;
        for q in s.quant.iter_mut() {
            if d.read_bit(128) != 0 {
                *q = d.read_signed_literal(7) as i16;
            }
        }
        for f in s.filter.iter_mut() {
            if d.read_bit(128) != 0 {
                *f = d.read_signed_literal(6) as i16;
            }
        }
    }
    if update_map {
        for p in s.map_probabilities.iter_mut() {
            if d.read_bit(128) != 0 {
                *p = d.read_literal(8) as u8;
            }
        }
    }
    Ok(s)
}

/// RFC 6386 §19.3 环路滤波。
fn read_filter(d: &mut Vp8BoolDecoder<'_>) -> Result<FilterSettings> {
    let simple = d.read_bit(128) != 0;
    let level = d.read_literal(6) as u8;
    let sharpness = d.read_literal(3) as u8;
    let mut f = FilterSettings {
        simple,
        level,
        sharpness,
        delta_enabled: false,
        ref_deltas: [0; 4],
        mode_deltas: [0; 4],
    };
    let delta_enabled = d.read_bit(128) != 0;
    if delta_enabled && d.read_bit(128) != 0 {
        f.delta_enabled = true;
        for r in f.ref_deltas.iter_mut() {
            if d.read_bit(128) != 0 {
                *r = d.read_signed_literal(6) as i8;
            }
        }
        for m in f.mode_deltas.iter_mut() {
            if d.read_bit(128) != 0 {
                *m = d.read_signed_literal(6) as i8;
            }
        }
    }
    Ok(f)
}

/// RFC 6386 §19.3 量化索引 + 各 delta。
fn read_quantizers(d: &mut Vp8BoolDecoder<'_>) -> Result<Quantizers> {
    let base = d.read_literal(7);
    let read_delta = |d: &mut Vp8BoolDecoder<'_>| -> i32 {
        if d.read_bit(128) == 0 {
            0
        } else {
            d.read_signed_literal(4)
        }
    };
    Ok(Quantizers {
        base,
        y_dc_delta: read_delta(d),
        y2_dc_delta: read_delta(d),
        y2_ac_delta: read_delta(d),
        uv_dc_delta: read_delta(d),
        uv_ac_delta: read_delta(d),
    })
}

/// RFC 6386 §13.4 系数概率更新（默认表 + 按扑克概率逐节点替换）。
fn read_coefficient_updates(d: &mut Vp8BoolDecoder<'_>, probabilities: &mut [u8; 4 * 8 * 3 * 11]) -> Result<()> {
    for (update_index, slot) in probabilities.iter_mut().enumerate() {
        if d.read_bit(UPDATE_COEFF_PROBS[update_index]) != 0 {
            *slot = d.read_literal(8) as u8;
        }
    }
    Ok(())
}

/// RFC 6386 §19.4 token 分区表：前 (count-1) 个分区带 3 字节尺寸，最后分区取余量。
fn read_partitions(data: &[u8], count: usize) -> Result<Vec<&[u8]>> {
    if count > 8 {
        return Err(Vp8Error::new("VP8 分区数超限"));
    }
    let size_count = count - 1;
    let table_length = size_count * 3;
    if data.len() < table_length {
        return Err(Vp8Error::new("VP8 token 分区尺寸表被截断"));
    }
    let mut sizes = [0usize; 7];
    for i in 0..size_count {
        let off = i * 3;
        sizes[i] = data[off] as usize | (data[off + 1] as usize) << 8 | (data[off + 2] as usize) << 16;
    }
    let mut offset = table_length;
    let mut parts = Vec::with_capacity(count);
    for i in 0..count {
        let size = if i == size_count {
            data.len() - offset
        } else {
            sizes[i]
        };
        if size > data.len() - offset {
            return Err(Vp8Error::new("VP8 token 分区超出帧"));
        }
        parts.push(&data[offset..offset + size]);
        offset += size;
    }
    Ok(parts)
}

/// 每段解量化因子与滤波阈值（RFC 6386 §14.1/§19.3 段 delta 融合）。
fn compute_segment_parameters(
    segments: &SegmentFeatures,
    quantizers: &Quantizers,
    filter: &FilterSettings,
) -> [SegmentParameters; 4] {
    let mut out = [SegmentParameters::default(); 4];
    for (segment, p) in out.iter_mut().enumerate() {
        let segment_quant = if segments.enabled {
            i32::from(segments.quant[segment])
        } else {
            0
        };
        let q_index = if segments.enabled && segments.absolute {
            segment_quant
        } else {
            quantizers.base + segment_quant
        }
        .clamp(0, 127);
        let segment_filter = if segments.enabled {
            i32::from(segments.filter[segment])
        } else {
            0
        };
        let filter_level = if segments.enabled && segments.absolute {
            segment_filter
        } else {
            i32::from(filter.level) + segment_filter
        };
        let mut level_i16 = filter_level;
        let mut level_i4 = filter_level;
        if filter.delta_enabled {
            level_i16 += i32::from(filter.ref_deltas[0]);
            level_i4 += i32::from(filter.ref_deltas[0]) + i32::from(filter.mode_deltas[0]);
        }
        p.filter_level_i16 = level_i16.clamp(0, 63) as u8;
        p.filter_level_i4 = level_i4.clamp(0, 63) as u8;
        p.y_dc = i32::from(DC_QUANTIZERS[(q_index + quantizers.y_dc_delta).clamp(0, 127) as usize]);
        p.y_ac = i32::from(AC_QUANTIZERS[q_index as usize]);
        p.y2_dc = 2 * i32::from(DC_QUANTIZERS[(q_index + quantizers.y2_dc_delta).clamp(0, 127) as usize]);
        p.y2_ac =
            (155 * i32::from(AC_QUANTIZERS[(q_index + quantizers.y2_ac_delta).clamp(0, 127) as usize]) / 100)
                .max(8);
        p.uv_dc =
            i32::from(DC_QUANTIZERS[(q_index + quantizers.uv_dc_delta).clamp(0, 127) as usize]).min(132);
        p.uv_ac = i32::from(AC_QUANTIZERS[(q_index + quantizers.uv_ac_delta).clamp(0, 127) as usize]);
    }
    out
}

// ---------------------------------------------------------------------------
// V2：宏块重建（帧内预测 / IDCT / WHT / 解量化 / 写回 YUV）
// ---------------------------------------------------------------------------

/// 平面存储：每行开头 1 字节左边界（列 -1，=129），随后是编码列 0..cw。帧行 r 列 c 的
/// 存储下标 = `r * pitch + 1 + c`；列 -1 = `r * pitch + 0`。
/// 帧内预测的「上一行」「上一行右侧」均取自平面未滤波重建值（V3 环路滤波延后到整帧之后，
/// 与 C# 增量滤波结果完全一致——预测只读未滤波行，滤波只读已重建行）。
#[inline]
fn plane_at(r: usize, pitch: usize, c: usize) -> usize {
    r * pitch + 1 + c
}

#[inline]
fn plane_at_l(r: usize, pitch: usize) -> usize {
    r * pitch
}

/// RFC 6386 §14.4 一维变换（p0=p0,p2=p2,p1=p1,p3=p3 的按名实参顺序固定）。
#[inline]
fn transform_1d(p0: i32, p2: i32, p1: i32, p3: i32) -> (i32, i32, i32, i32) {
    let a0 = p0 + p2;
    let b0 = p0 - p2;
    let c0 = ((p1 * 35468) >> 16) - (p3 + ((p3 * 20091) >> 16));
    let d0 = p1 + ((p1 * 20091) >> 16) + ((p3 * 35468) >> 16);
    (a0 + d0, b0 + c0, b0 - c0, a0 - d0)
}

/// RFC 6386 §14.4 逆 DCT：纵过（结果回绕 i16）→ 横过（(x+4)>>3 舍入并回绕）。
fn inverse_dct_short(input: &[i16; 16], output: &mut [i16; 16], has_ac_coefficients: bool) {
    if !has_ac_coefficients {
        let dc = ((i32::from(input[0]) + 4) >> 3) as i16;
        output.fill(dc);
        return;
    }
    let mut horizontal = [0i16; 16];
    for column in 0..4 {
        // 实参顺序：p0=input[c], p2=input[8+c], p1=input[4+c], p3=input[12+c]（C# 同款）。
        let (a, b, c, d) = transform_1d(
            i32::from(input[column]),
            i32::from(input[8 + column]),
            i32::from(input[4 + column]),
            i32::from(input[12 + column]),
        );
        horizontal[column] = a as i16;
        horizontal[4 + column] = b as i16;
        horizontal[8 + column] = c as i16;
        horizontal[12 + column] = d as i16;
    }
    for row in 0..4 {
        let i = row * 4;
        let (a, b, c, d) = transform_1d(
            i32::from(horizontal[i]),
            i32::from(horizontal[i + 2]),
            i32::from(horizontal[i + 1]),
            i32::from(horizontal[i + 3]),
        );
        output[i] = ((a + 4) >> 3) as i16;
        output[i + 1] = ((b + 4) >> 3) as i16;
        output[i + 2] = ((c + 4) >> 3) as i16;
        output[i + 3] = ((d + 4) >> 3) as i16;
    }
}

/// 逆变换并把残差加到 4x4 像素（注意残差为 i16，逐像素 clamp [0,255]）。
/// `pub(crate)`：`webp::simd` 的 `inverse_dct_add_two` 标量回退复用（恒 has_ac=true，
/// 与既有短路语义逐字节一致）。
pub(crate) fn inverse_dct_add(
    coefficients: &[i16; 16],
    plane: &mut [u8],
    pitch: usize,
    row0: usize,
    col0: usize,
    has_ac: bool,
) {
    let mut residual = [0i16; 16];
    inverse_dct_short(coefficients, &mut residual, has_ac);
    for row in 0..4 {
        for col in 0..4 {
            let idx = plane_at(row0 + row, pitch, col0 + col);
            plane[idx] = (i32::from(plane[idx]) + i32::from(residual[row * 4 + col])).clamp(0, 255) as u8;
        }
    }
}

/// RFC 6386 §14.3 逆 Walsh-Hadamard：Y2 块输出写为 16 个 luma 块的 DC。
fn inverse_walsh_to_blocks(input: &[i16; 16], blocks: &mut [[i16; 16]; 25], has_ac: bool) {
    if !has_ac {
        let dc = ((i32::from(input[0]) + 3) >> 3) as i16;
        for i in 0..16 {
            blocks[i][0] = dc;
        }
        return;
    }
    let mut horizontal = [0i16; 16];
    for column in 0..4 {
        let a = i32::from(input[column]) + i32::from(input[12 + column]);
        let b = i32::from(input[4 + column]) + i32::from(input[8 + column]);
        let c = i32::from(input[4 + column]) - i32::from(input[8 + column]);
        let d = i32::from(input[column]) - i32::from(input[12 + column]);
        horizontal[column] = (a + b) as i16;
        horizontal[4 + column] = (c + d) as i16;
        horizontal[8 + column] = (a - b) as i16;
        horizontal[12 + column] = (d - c) as i16;
    }
    for row in 0..4 {
        let i = row * 4;
        let a = i32::from(horizontal[i]) + i32::from(horizontal[i + 3]);
        let b = i32::from(horizontal[i + 1]) + i32::from(horizontal[i + 2]);
        let c = i32::from(horizontal[i + 1]) - i32::from(horizontal[i + 2]);
        let d = i32::from(horizontal[i]) - i32::from(horizontal[i + 3]);
        blocks[i][0] = ((a + b + 3) >> 3) as i16;
        blocks[i + 1][0] = ((c + d + 3) >> 3) as i16;
        blocks[i + 2][0] = ((a - b + 3) >> 3) as i16;
        blocks[i + 3][0] = ((d - c + 3) >> 3) as i16;
    }
}

// ---------------------------------------------------------------------------
// 帧内预测（RFC 6386 §12.2 / §12.3，标量）
// ---------------------------------------------------------------------------

#[inline]
fn clamp_byte(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

#[inline]
fn avg2(a: u8, b: u8) -> u8 {
    ((i32::from(a) + i32::from(b) + 1) >> 1) as u8
}

#[inline]
fn avg3(a: u8, b: u8, c: u8) -> u8 {
    ((i32::from(a) + 2 * i32::from(b) + i32::from(c) + 2) >> 2) as u8
}

/// 16x16 luma / 8x8 chroma 预测（mode 0 DC / 1 V / 2 H / 3 TM）。
/// `above[0]` = 左上角，`above[1 + i]` = 上一行列 i（共 size 个）。左列读平面（列 -1 为边界）。
/// `pub(crate)`：`webp::simd` 的向量版以此为标量回退/对拍基准（D1）。
pub(crate) fn predict_large(
    plane: &mut [u8],
    pitch: usize,
    row0: usize,
    col0: usize,
    size: usize,
    mode: u8,
    has_top: bool,
    has_left: bool,
    above: &[u8],
) {
    match mode {
        1 => {
            // V：整块 = 上一行。
            for r in 0..size {
                for c in 0..size {
                    let idx = plane_at(row0 + r, pitch, col0 + c);
                    plane[idx] = above[1 + c];
                }
            }
        }
        2 => {
            // H：整行 = 左列。
            for r in 0..size {
                let l = plane[plane_at_l(row0 + r, pitch) + col0];
                for c in 0..size {
                    let idx = plane_at(row0 + r, pitch, col0 + c);
                    plane[idx] = l;
                }
            }
        }
        3 => {
            // TM：clamp(左 + 上 - 左上)。
            let top_left = i32::from(above[0]);
            for r in 0..size {
                let left = i32::from(plane[plane_at_l(row0 + r, pitch) + col0]);
                for c in 0..size {
                    let idx = plane_at(row0 + r, pitch, col0 + c);
                    plane[idx] = clamp_byte(left + i32::from(above[1 + c]) - top_left);
                }
            }
        }
        _ => {
            // DC：仅累计界内边缘。
            let mut sum = 0i32;
            for i in 0..size {
                if has_top {
                    sum += i32::from(above[1 + i]);
                }
                if has_left {
                    sum += i32::from(plane[plane_at_l(row0 + i, pitch) + col0]);
                }
            }
            let value = if has_top && has_left {
                (sum + size as i32) / (2 * size as i32)
            } else if has_top || has_left {
                (sum + (size as i32) / 2) / size as i32
            } else {
                128
            } as u8;
            for r in 0..size {
                for c in 0..size {
                    let idx = plane_at(row0 + r, pitch, col0 + c);
                    plane[idx] = value;
                }
            }
        }
    }
}

/// 构造 4x4 块的 above 行（`above[0]`=左上，`above[1..9]`=上 8 像素）：
/// 顶行块取宏块 above 缓冲；下方块取已重建上一行；右列块的 above-right 取自宏块 above 缓冲
/// 第 16..19（RFC 6386 §12.3 的 16×8 边界不可重排语义）。
/// I4 热路径逐块调用（16×/宏块 × ~80% 宏块）：读用裸指针免边界检查。
/// 越界不变量：`block_y>0` 时读行 `row_above=row0-1`，列范围
/// `col0-1 ..= col0+7`——`col0=dx+bx*4`，bx≤3 → 最大列 `dx+15 < cw`，最小 `dx-1` 为左边界
/// 129 列（`pitch` 行首格）；故最大平面下标 `(ch-1)*pitch+1+cw-1 < ch*pitch`（=y 平面长度）
/// 恒不越界（`plane_at`/`plane_at_l` 行布局见 645 注释；`debug_assert` 守形状边界）。
fn subblock_above(
    plane: &[u8],
    pitch: usize,
    row0: usize,
    col0: usize,
    block_x: usize,
    block_y: usize,
    above_y: &[u8; 21],
) -> [u8; 9] {
    if block_y == 0 {
        let base = 4 * block_x;
        let mut a = [0u8; 9];
        a[0] = above_y[base];
        a[1..9].copy_from_slice(&above_y[base + 1..base + 9]);
        return a;
    }
    let row_above = row0 - 1;
    let base = unsafe { plane.as_ptr().add(row_above * pitch) };
    let mut a = [0u8; 9];
    debug_assert!(row_above * pitch + col0 + 8 < plane.len());
    if block_x < 3 {
        a[0] = unsafe { *base.add(col0) };
        for k in 0..8 {
            a[1 + k] = unsafe { *base.add(col0 + 1 + k) };
        }
    } else {
        // block_x == 3：右侧 4 像素取宏块 aboveY[16..19]。
        a[0] = unsafe { *base.add(col0) };
        for k in 0..4 {
            a[1 + k] = unsafe { *base.add(col0 + 1 + k) };
        }
        for k in 0..4 {
            a[5 + k] = above_y[17 + k];
        }
    }
    a
}

/// 4x4 luma 预测（10 模式）。`above[0]`=左上，`above[1..9]`=上 8 像素；左列读平面。
/// `pub(crate)`：`webp::simd` 的向量版以此为标量回退/对拍基准（D1）。
#[allow(clippy::too_many_lines)]
pub(crate) fn predict_subblock(
    plane: &mut [u8],
    pitch: usize,
    row0: usize,
    col0: usize,
    mode: u8,
    above: &[u8; 9],
) {
    let mut left = [0u8; 4];
    for i in 0..4 {
        left[i] = plane[plane_at_l(row0 + i, pitch) + col0];
    }
    let p = above[0];
    // edge = [L3 L3'? 不：edge[0..9]]：edge = [L3 L2 L1 L0 P A0 A1 A2 A3]。
    let edge = [
        left[3],
        left[2],
        left[1],
        left[0],
        p,
        above[1],
        above[2],
        above[3],
        above[4],
    ];
    let at = |r: usize, c: usize| plane_at(row0 + r, pitch, col0 + c);
    match mode {
        0 => {
            let sum = 4u32 + (0..4).map(|i| u32::from(above[1 + i])).sum::<u32>()
                + (0..4).map(|i| u32::from(left[i])).sum::<u32>();
            let v = (sum >> 3) as u8;
            for r in 0..4 {
                for c in 0..4 {
                    plane[at(r, c)] = v;
                }
            }
        }
        1 => {
            // TM。
            let p = i32::from(above[0]);
            for r in 0..4 {
                let lv = i32::from(left[r]);
                for c in 0..4 {
                    plane[at(r, c)] = clamp_byte(lv + i32::from(above[1 + c]) - p);
                }
            }
        }
        2 => {
            // VE。
            for r in 0..4 {
                for c in 0..4 {
                    let a = if c == 0 { p } else { above[c] };
                    plane[at(r, c)] = avg3(a, above[1 + c], above[2 + c]);
                }
            }
        }
        3 => {
            // HE。
            for r in 0..4 {
                let prev = if r == 0 { p } else { left[r - 1] };
                let next = left[(r + 1).min(3)];
                let v = avg3(prev, left[r], next);
                for c in 0..4 {
                    plane[at(r, c)] = v;
                }
            }
        }
        4 => {
            // LD。
            for r in 0..4 {
                for c in 0..4 {
                    let idx = r + c + 1;
                    plane[at(r, c)] = if idx < 7 {
                        avg3(above[idx], above[1 + idx], above[2 + idx])
                    } else {
                        avg3(above[7], above[8], above[8])
                    };
                }
            }
        }
        5 => {
            // RD。
            for r in 0..4 {
                for c in 0..4 {
                    let base = 4 + c as i32 - r as i32;
                    let e = |off: i32| edge[(base + off) as usize];
                    plane[at(r, c)] = avg3(e(-1), e(0), e(1));
                }
            }
        }
        6 => {
            // VR。
            for r in 0..4 {
                for c in 0..4 {
                    let v = match r {
                        0 => avg2(edge[4 + c], edge[5 + c]),
                        1 => avg3(edge[3 + c], edge[4 + c], edge[5 + c]),
                        2 => {
                            if c == 0 {
                                avg3(edge[2], edge[3], edge[4])
                            } else {
                                avg2(edge[3 + c], edge[4 + c])
                            }
                        }
                        _ => {
                            let (a, b, d) = if c == 0 {
                                (edge[1], edge[2], edge[3])
                            } else {
                                (edge[2 + c], edge[3 + c], edge[4 + c])
                            };
                            avg3(a, b, d)
                        }
                    };
                    plane[at(r, c)] = v;
                }
            }
        }
        7 => {
            // VL。
            for r in 0..4 {
                for c in 0..4 {
                    let v = match r {
                        0 => avg2(above[1 + c], above[2 + c]),
                        1 => avg3(above[1 + c], above[2 + c], above[3 + c]),
                        2 => {
                            if c < 3 {
                                avg2(above[2 + c], above[3 + c])
                            } else {
                                avg3(above[5], above[6], above[7])
                            }
                        }
                        _ => {
                            if c == 3 {
                                avg3(above[6], above[7], above[8])
                            } else {
                                avg3(above[2 + c], above[3 + c], above[4 + c])
                            }
                        }
                    };
                    plane[at(r, c)] = v;
                }
            }
        }
        8 => {
            // HD。
            for r in 0..4 {
                for c in 0..4 {
                    let index = match r {
                        0 => match c {
                            0 => 3usize,
                            1 => 4,
                            2 => 5,
                            _ => 6,
                        },
                        1 => match c {
                            0 => 2usize,
                            1 => 3,
                            2 => 3,
                            _ => 4,
                        },
                        2 => match c {
                            0 => 1usize,
                            1 => 2,
                            2 => 2,
                            _ => 3,
                        },
                        _ => match c {
                            0 => 0usize,
                            1 => 1,
                            2 => 1,
                            _ => 2,
                        },
                    };
                    let half = (r == 0 && c == 0)
                        || (r == 1 && (c == 0 || c == 2))
                        || (r == 2 && (c == 0 || c == 2))
                        || (r == 3 && (c == 0 || c == 2));
                    plane[at(r, c)] = if half {
                        avg2(edge[index], edge[index + 1])
                    } else {
                        avg3(edge[index - 1], edge[index], edge[index + 1])
                    };
                }
            }
        }
        _ => {
            // HU。
            for r in 0..4 {
                for c in 0..4 {
                    let v = if r == 3 || (r == 2 && c >= 2) {
                        left[3]
                    } else {
                        match r {
                            0 => match c {
                                0 => avg2(left[0], left[1]),
                                1 => avg3(left[0], left[1], left[2]),
                                2 => avg2(left[1], left[2]),
                                _ => avg3(left[1], left[2], left[3]),
                            },
                            1 => match c {
                                0 => avg2(left[1], left[2]),
                                1 => avg3(left[1], left[2], left[3]),
                                2 => avg2(left[2], left[3]),
                                _ => avg3(left[2], left[3], left[3]),
                            },
                            _ => {
                                if c == 0 {
                                    avg2(left[2], left[3])
                                } else {
                                    avg3(left[2], left[3], left[3])
                                }
                            }
                        }
                    };
                    plane[at(r, c)] = v;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// V2：系数 token 解码与宏块重建
// ---------------------------------------------------------------------------

/// 每位置的 (band0 行偏移)（RFC 6386 §13.3）：band(position) × 3 × 11。
const BAND_ROW_OFFSETS: [usize; 16] = [
    0, 33, 66, 99, 198, 132, 165, 198, 198, 198, 198, 198, 198, 198, 198, 231,
];

/// 各块在 above/left 非零掩码中的位。
const BLOCK_ABOVE_CONTEXT_BIT: [u32; 25] = [
    0, 1, 2, 3, 0, 1, 2, 3, 0, 1, 2, 3, 0, 1, 2, 3, 4, 5, 4, 5, 6, 7, 6, 7, 24,
];
const BLOCK_LEFT_CONTEXT_BIT: [u32; 25] = [
    0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 24,
];

/// Y2 块的掩码位（低 32 位掩码的位 24）。
const NZ_Y2: u32 = 1 << 24;

/// RFC 6386 §13.2 DCT_CAT3..6 子树（p8 之后）附额外位。熵态以调用方局部标量携带（寄存器驻留，
/// 同 `tokens` 标量化 Load/Store 姿势；`data`/`pos` 仅在 refill 触碰）。概率读用裸指针游标 `p`
/// （`tokens` 传入，指向当前 p_idx 处）：读 `p[k]`（k=8..10）。不变量与 `tokens` 同证：
/// p_idx ≤ 253、p_idx+10 ≤ 263 < 264（每型概率表长）→ 恒不越界。
///
/// **RC3**：CAT8/9/10（p[8..10]，偏斜极高但低频）随 ZERO/G5 同机制做预测分支形——位判定与
/// 调用点 `if b==0` 体合并为同一分支（`ent_branch=true`，默认）；false 走旧 cmov 形。两形输出
/// 逐字节一致（共享 `ent_split_pre`/`ent_renorm_apply` 单源算术）。
#[inline(always)]
fn read_category_3_to_6(
    data: &[u8],
    pos: &mut usize,
    value: &mut u64,
    bits: &mut i32,
    range_minus1: &mut u32,
    renorm_skip: bool,
    ent_branch: bool,
    p: *const u8,
) -> i32 {
    let (extra, base): (&[u8], i32) = {
        if ent_branch {
            // 预测分支形：CAT8 位判定直接路由（多数方 bit=0 只算本路熵态，单误判点）。
            let (vs, sp, rm1) = ent_split_pre(data, pos, value, bits, range_minus1, unsafe { *p.add(8) });
            if vs > u64::from(sp) {
                // bit=1（少数方 0.2%）：CAT10 分支（偏斜 0.94，多数方 bit=0）。
                *value -= (u64::from(sp + 1)) << (*bits as u32);
                ent_renorm_apply(bits, range_minus1, rm1 - sp, renorm_skip);
                probe_skew!(skew_clock::CAT8, 1);
                let (vs, sp, rm1) = ent_split_pre(data, pos, value, bits, range_minus1, unsafe { *p.add(10) });
                if vs > u64::from(sp) {
                    *value -= (u64::from(sp + 1)) << (*bits as u32);
                    ent_renorm_apply(bits, range_minus1, rm1 - sp, renorm_skip);
                    probe_skew!(skew_clock::CAT10, 1);
                    (&PCAT6, 67)
                } else {
                    ent_renorm_apply(bits, range_minus1, sp + 1, renorm_skip);
                    probe_skew!(skew_clock::CAT10, 0);
                    (&PCAT5, 35)
                }
            } else {
                // bit=0（多数方 99.8%，预测命中）：CAT9 分支（偏斜 0.893，多数方 bit=0）。
                ent_renorm_apply(bits, range_minus1, sp + 1, renorm_skip);
                probe_skew!(skew_clock::CAT8, 0);
                let (vs, sp, rm1) = ent_split_pre(data, pos, value, bits, range_minus1, unsafe { *p.add(9) });
                if vs > u64::from(sp) {
                    *value -= (u64::from(sp + 1)) << (*bits as u32);
                    ent_renorm_apply(bits, range_minus1, rm1 - sp, renorm_skip);
                    probe_skew!(skew_clock::CAT9, 1);
                    (&PCAT4, 19)
                } else {
                    ent_renorm_apply(bits, range_minus1, sp + 1, renorm_skip);
                    probe_skew!(skew_clock::CAT9, 0);
                    (&PCAT3, 11)
                }
            }
        } else {
            // cmov 全算（A/B 对照侧）：与分支形逐字节一致，唯指令选择不同。
            let b = ent_read_bit(data, pos, value, bits, range_minus1, renorm_skip, unsafe { *p.add(8) });
            probe_skew!(skew_clock::CAT8, b);
            if b == 0 {
                let b = ent_read_bit(data, pos, value, bits, range_minus1, renorm_skip, unsafe { *p.add(9) });
                probe_skew!(skew_clock::CAT9, b);
                if b == 0 {
                    (&PCAT3, 11)
                } else {
                    (&PCAT4, 19)
                }
            } else {
                let b = ent_read_bit(data, pos, value, bits, range_minus1, renorm_skip, unsafe { *p.add(10) });
                probe_skew!(skew_clock::CAT10, b);
                if b == 0 {
                    (&PCAT5, 35)
                } else {
                    (&PCAT6, 67)
                }
            }
        }
    };
    let mut mag = 0i32;
    let mut i = 0;
    while extra[i] != 0 {
        let b = ent_read_bit(data, pos, value, bits, range_minus1, renorm_skip, extra[i]);
        probe_skew!(skew_clock::EXTRA, b);
        mag = (mag << 1) | b as i32;
        i += 1;
    }
    base + mag
}

/// 读一个块的系数 token（RFC 6386 §13.2），去量化后按 zigzag 写回 `output`（调用前须清零）。
/// 返回最后非零 token 的 zigzag 位置 + 1（无 token 返回 0）。熵态以调用方局部标量携带 +
/// `#[inline(always)]` 使每宏块 25 次调用内联成单函数、{value,bits,range} 全程寄存器驻留
/// （C# `Tokens(ref v, ref b, ref r)` 同款 Load/Store；`data`/`pos` 仅在 refill 触碰）。
#[inline(always)]
fn tokens(
    data: &[u8],
    pos: &mut usize,
    value: &mut u64,
    bits: &mut i32,
    range_minus1: &mut u32,
    renorm_skip: bool,
    ent_branch: bool,
    probabilities: &[u8],
    context: usize,
    mut position: usize,
    output: &mut [i16; 16],
    dc_factor: i32,
    ac_factor: i32,
) -> usize {
    // 熵概率读不变量（证明）：`BAND_ROW_OFFSETS` 最大 231（position≤15），context≤2、
    // next_context∈{11,22} ⇒ p_idx ≤ 253；概率表按型分块 8*33=264 字节、`probabilities` 切片长
    // 恒 ≥ 264（`parse_residuals` 传 `probs[ty*264..]`，表总长 1056）⇒ p_idx+10 ≤ 263 < 264。
    // 故裸指针游标 `p = base.add(p_idx)` 读 `p[k]`（k≤10）恒不越界——用指针游标省去每次
    // `probabilities[p_idx+k]` 的边界检查，读到的概率字节与下标版逐字节相同（libwebp vp8_dec.c
    // 的 `const uint8_t* p` 滑动游标同款姿势，参考不抄）。
    //
    // 环内 `get_unchecked` 不变量（A 记账压紧·减少每系数索引算术）：`position` 两侧自增后必先
    // 判 `position == 16` break 才续用（1208-1272），故三处 `BAND_ROW_OFFSETS[position]` /
    // `ZIGZAG[position]`（1217/1258/1267）恒有 position ≤ 15 < 16——去掉必然不触发的边界检查
    // 每次索引省 1-2 uop（cmp+溢出分支），读值与下标版逐字节相同。
    let p_idx = BAND_ROW_OFFSETS[position] + context * 11;
    debug_assert!(p_idx + 10 < probabilities.len());
    let base = probabilities.as_ptr();
    let mut p = unsafe { base.add(p_idx) };
    stage_clock::acct();
    let mut last = 0usize;

    // ── RC3：ZERO/G5 两高偏斜决策点（p[1]/p[6]）在「预测分支形（默认）」与「cmov 全算
    // （KIMG_WEBP_ENT_BRANCH=0 强制）」间运行时切换。两形体共用下方局部宏（体只写一份），
    // bmi/base 两变体同调本函数 → 宏单源同步获得同一分支结构（K 约束）。分支形把位判定与
    // 调用点 `if b==0` 体合并为同一分支（避免同一位双分支双误判；多数方命中只算本路熵态）。
    /// G5 的多数方（bit=0，偏斜 89.6%）体：F67→F159 / F165+F145 链，产出 (mag, 22)。
    /// F67/F159/F165/F145 偏斜 54-64% 净负，保留 cmov 形不动（抛硬币/近硬币点禁止改）。
    macro_rules! g5_zero_body {
        () => {{
            let b = ent_read_bit(data, pos, value, bits, range_minus1, renorm_skip, unsafe { *p.add(7) });
            probe_skew!(skew_clock::F67, b);
            if b == 0 {
                let b = ent_read_bit(data, pos, value, bits, range_minus1, renorm_skip, 159);
                probe_skew!(skew_clock::F159, b);
                (5 + b as i32, 22)
            } else {
                let b = ent_read_bit(data, pos, value, bits, range_minus1, renorm_skip, 165);
                probe_skew!(skew_clock::F165, b);
                let high = b << 1;
                let b = ent_read_bit(data, pos, value, bits, range_minus1, renorm_skip, 145);
                probe_skew!(skew_clock::F145, b);
                (7 + (high | b) as i32, 22)
            }
        }};
    }
    /// 非零 token 体：幅值树（ONE/LT5/TWO/F34/G5）→ 符号 → 去量 → zigzag 写回 → 下一位置 EOB。
    /// 以块表达式产出 (mag, next_context) 后走共享尾部；G5 决策点据此切换分支/cmov 两形。
    macro_rules! nonzero_token_body {
        () => {{
            let (mag, next_context): (i32, usize) = {
                let b = ent_read_bit(data, pos, value, bits, range_minus1, renorm_skip, unsafe { *p.add(2) });
                probe_skew!(skew_clock::ONE, b);
                if b == 0 {
                    (1, 11)
                } else {
                    let b = ent_read_bit(data, pos, value, bits, range_minus1, renorm_skip, unsafe { *p.add(3) });
                    probe_skew!(skew_clock::LT5, b);
                    if b == 0 {
                        let b = ent_read_bit(data, pos, value, bits, range_minus1, renorm_skip, unsafe { *p.add(4) });
                        probe_skew!(skew_clock::TWO, b);
                        if b == 0 {
                            (2, 22)
                        } else {
                            let b = ent_read_bit(data, pos, value, bits, range_minus1, renorm_skip, unsafe { *p.add(5) });
                            probe_skew!(skew_clock::F34, b);
                            (3 + b as i32, 22)
                        }
                    } else {
                        // G5 决策（p[6]，多数方 bit=0 偏斜 89.6%）：分支形与 cmov 形运行时切换。
                        if ent_branch {
                            let (vs, sp, rm1) = ent_split_pre(data, pos, value, bits, range_minus1, unsafe { *p.add(6) });
                            if vs > u64::from(sp) {
                                // bit=1（少数方 10.4%）：DCT_CAT3..6。
                                *value -= (u64::from(sp + 1)) << (*bits as u32);
                                ent_renorm_apply(bits, range_minus1, rm1 - sp, renorm_skip);
                                probe_skew!(skew_clock::G5, 1);
                                let cat = read_category_3_to_6(data, pos, value, bits, range_minus1, renorm_skip, ent_branch, p);
                                (cat, 22)
                            } else {
                                // bit=0（多数方 89.6%，预测命中）：F67 链。
                                ent_renorm_apply(bits, range_minus1, sp + 1, renorm_skip);
                                probe_skew!(skew_clock::G5, 0);
                                g5_zero_body!()
                            }
                        } else {
                            let b = ent_read_bit(data, pos, value, bits, range_minus1, renorm_skip, unsafe { *p.add(6) });
                            probe_skew!(skew_clock::G5, b);
                            if b == 0 {
                                g5_zero_body!()
                            } else {
                                let cat = read_category_3_to_6(data, pos, value, bits, range_minus1, renorm_skip, ent_branch, p);
                                (cat, 22)
                            }
                        }
                    }
                }
            };
            let factor = if position == 0 { dc_factor } else { ac_factor };
            // 无分支符号（C# ReadSigned：符号是抛硬币，分支一半预测失败）。
            let signed = ent_read_signed(data, pos, value, bits, range_minus1, mag);
            stage_clock::dq();
            output[unsafe { *ZIGZAG.get_unchecked(position) } as usize] = (signed * factor) as i16;
            stage_clock::acct();
            last = position + 1;
            stage_clock::acct();
            position += 1;
            if position == 16 {
                break;
            }
            stage_clock::acct();
            p = unsafe { base.add(*BAND_ROW_OFFSETS.get_unchecked(position) + next_context) };
            let b = ent_read_bit(data, pos, value, bits, range_minus1, renorm_skip, unsafe { *p });
            probe_skew!(skew_clock::EOB, b);
            if b == 0 {
                break; // 下一位置 EOB。
            }
        }};
    }
    /// 零 token 体：前进到下一位置（零 token 不写字），位置到 16 退出。
    macro_rules! zero_token_body {
        () => {{
            stage_clock::acct();
            position += 1;
            if position == 16 {
                break;
            }
            stage_clock::acct();
            p = unsafe { base.add(*BAND_ROW_OFFSETS.get_unchecked(position)) };
            continue;
        }};
    }

    let b = ent_read_bit(data, pos, value, bits, range_minus1, renorm_skip, unsafe { *p });
    probe_skew!(skew_clock::EOB, b);
    if b != 0 {
        loop {
            // ZERO 决策（p[1]，多数方 bit=0 偏斜 85.2%）：分支形与 cmov 形运行时切换。分支形把
            // 位判定与 `if b==0` 体合并为同一分支（单误判点；多数方命中只算本路熵态），两形共享
            // 单源算术（`ent_split_pre`/`ent_renorm_apply`）→ 输出逐字节一致。
            if ent_branch {
                let (vs, sp, rm1) = ent_split_pre(data, pos, value, bits, range_minus1, unsafe { *p.add(1) });
                if vs > u64::from(sp) {
                    // bit=1（少数方 14.8%）：零未命中，更新熵态后落入非零 token 体。
                    *value -= (u64::from(sp + 1)) << (*bits as u32);
                    ent_renorm_apply(bits, range_minus1, rm1 - sp, renorm_skip);
                    probe_skew!(skew_clock::ZERO, 1);
                    nonzero_token_body!();
                } else {
                    // bit=0（多数方 85.2%，预测命中）：只算本路熵态，直接落零 token 体。
                    ent_renorm_apply(bits, range_minus1, sp + 1, renorm_skip);
                    probe_skew!(skew_clock::ZERO, 0);
                    zero_token_body!();
                }
            } else {
                let b = ent_read_bit(data, pos, value, bits, range_minus1, renorm_skip, unsafe { *p.add(1) });
                probe_skew!(skew_clock::ZERO, b);
                if b == 0 {
                    zero_token_body!();
                }
                nonzero_token_body!();
            }
        }
    }
    last
}

/// 解析一个宏块全部系数（Y2 先于 luma 当 i16）。返回两块掩码：低 32 = 每块非零，高 32 = 有 AC。
/// 每宏块一次 Load/Store（对照 C# `ParseResiduals` 的 `decoder.Load(out v, out b, out r)` /
/// `decoder.Store(v, b, r)`）：熵态三标量出结构入本地 SSA，函数边界用 `&mut` 只在进出各一次
/// {value,bits,range}，`data`/`pos` 仅在 refill 触碰；内部 tokens 内联后全为寄存器。
///
/// **RC1 双路径**：`parse_residuals` 为分发器，按运行时一次判定进 `parse_residuals_bmi`
/// （`#[target_feature(enable="lzcnt,bmi1,bmi2")]` 快道）或 `parse_residuals_base`（基线
/// bsr+xor/mov→cl）。两变体函数体由下方宏展开自同一算术源——保证「bmi ≡ baseline」逐字节
/// 一致是结构性的（仅指令选择不同）：inlined `tokens`/`ent_read_bit` 的 `range.leading_zeros()`
/// 快道发 `lzcnt`（Haswell 1c vs bsr 3c）、变长移位发 `shrx/shlx`（免 `mov→cl` 依赖链）；
/// 基线路径与 RC1 前逐指令相同（bsr+xorl $31）。分发每宏块一次（`ent_bmi_enabled()` 惰性
/// OnceLock 原子读 + 恒定可预测分支），t1 主循环（MT 与串行共用 `entropy_macroblock`）都受益。
macro_rules! parse_residuals_impl {
    (
        $d:ident, $probabilities:ident, $mb:ident, $sp:ident, $i16:ident, $above_nz:ident, $left_nz:ident
    ) => ({
        // Load（每宏块一次，仿 C# `ParseResiduals` 的 `decoder.Load(out v, out b, out r)`）：
        // 熵态三标量出结构入调用方局部变量，走完 Store 回——热环内 {value,bits,range} 全程 SSA
        // 寄存器，无 40B-byval 结构的逐位回写栈（Top1 主攻点）。`renorm_skip` 也在此判定一次
        // （TOP4：OnceLock 每宏块一读，主环零 OnceLock load）。
        let renorm_skip = ent_renorm_skip();
        // RC3：分支形开关每宏块判定一次（OnceLock 一读 + 恒定可预测分支），主环零开关开销。
        let ent_branch = super::simd::ent_branch_enabled();
        let data = $d.data;
        let mut pos = $d.pos;
        let mut value = $d.value;
        let mut bits = $d.bits;
        let mut range_minus1 = $d.range_minus1;
        let mut above = *$above_nz;
        let mut left = *$left_nz;
        let mut masks = 0u64;
        let mut block = if $i16 { 24usize } else { 0usize };
        let (luma_type, luma_start, luma_dc) = if $i16 {
            (0usize, 1usize, 1i32)
        } else {
            (3usize, 0usize, $sp.y_dc)
        };
        loop {
            let (ty, start, dc, ac) = if block < 16 {
                (luma_type, luma_start, luma_dc, $sp.y_ac)
            } else if block < 24 {
                (2usize, 0usize, $sp.uv_dc, $sp.uv_ac)
            } else {
                (1usize, 0usize, $sp.y2_dc, $sp.y2_ac)
            };
            let above_bit = BLOCK_ABOVE_CONTEXT_BIT[block];
            let left_bit = BLOCK_LEFT_CONTEXT_BIT[block];
            let context = (((above >> above_bit) & 1) + ((left >> left_bit) & 1)) as usize;
            stage_clock::acct();
            let last = tokens(
                data,
                &mut pos,
                &mut value,
                &mut bits,
                &mut range_minus1,
                renorm_skip,
                ent_branch,
                &$probabilities[ty * 8 * 33..],
                context,
                start,
                &mut $mb[block],
                dc,
                ac,
            );
            let nonzero = if last > 0 { 1u32 } else { 0u32 };
            above = (above & !(1 << above_bit)) | (nonzero << above_bit);
            left = (left & !(1 << left_bit)) | (nonzero << left_bit);
            masks |= u64::from(nonzero) << block;
            if last > 1 {
                masks |= 1u64 << (block + 32);
            }
            stage_clock::acct();
            if block == 24 {
                block = 0;
            } else {
                block += 1;
                if block == 24 {
                    break;
                }
            }
        }
        // Store（每宏块一次，仿 C# `decoder.Store(v, b, r)`）。
        $d.data = data;
        $d.pos = pos;
        $d.value = value;
        $d.bits = bits;
        $d.range_minus1 = range_minus1;
        *$above_nz = above;
        *$left_nz = left;
        masks
    })
}

/// 基线变体：与 RC1 前完全相同的指令选择（bsr+xorl $31 / mov→cl），旧 CPU 与
/// `KIMG_WEBP_ENT_NO_BMI=1` 强制侧走本条。字节恒等由「bmi≡baseline」硬闸对拍。
#[inline(never)]
fn parse_residuals_base(
    d: &mut Vp8BoolDecoder<'_>,
    probabilities: &[u8],
    mb: &mut [[i16; 16]; 25],
    sp: &SegmentParameters,
    i16: bool,
    above_nz: &mut u32,
    left_nz: &mut u32,
) -> u64 {
    parse_residuals_impl!(d, probabilities, mb, sp, i16, above_nz, left_nz)
}

/// BMI1/2+lzcnt 快道变体：`leading_zeros`→`lzcnt`、变长移位→`shrx/shlx`（免 cl 依赖）；
/// 语义与基线逐字节一致（同宏体），仅指令选择不同。**Safety**：仅当 `ent_bmi_enabled()`
/// 为真（`bmi2` 可用 ⇒ `bmi1`/`lzcnt` 亦可用）时被调用——`lzcnt` 在无 BMI1 CPU 上会被解码成
/// `bsr`（语义反转→错字节），故绝不裸发；非 x86_64 本函数不存在（cfg 剔除）。
#[cfg(target_arch = "x86_64")]
#[inline(never)]
#[target_feature(enable = "lzcnt,bmi1,bmi2")]
unsafe fn parse_residuals_bmi(
    d: &mut Vp8BoolDecoder<'_>,
    probabilities: &[u8],
    mb: &mut [[i16; 16]; 25],
    sp: &SegmentParameters,
    i16: bool,
    above_nz: &mut u32,
    left_nz: &mut u32,
) -> u64 {
    parse_residuals_impl!(d, probabilities, mb, sp, i16, above_nz, left_nz)
}

/// 熵系数解码分发器：见上方「RC1 双路径」说明。快道与基线由同一宏体展开（算术单源不分裂），
/// 仅指令选择不同（`parse_residuals_impl!`）。
#[inline(never)]
fn parse_residuals(
    d: &mut Vp8BoolDecoder<'_>,
    probabilities: &[u8],
    mb: &mut [[i16; 16]; 25],
    sp: &SegmentParameters,
    i16: bool,
    above_nz: &mut u32,
    left_nz: &mut u32,
) -> u64 {
    #[cfg(target_arch = "x86_64")]
    if super::simd::ent_bmi_enabled() {
        // Safety：ent_bmi_enabled() 保证 bmi2（⇒bmi1/lzcnt）可用；快道与基线逐字节一致
        // （「bmi≡baseline」硬闸覆盖），仅指令选择不同。
        return unsafe { parse_residuals_bmi(d, probabilities, mb, sp, i16, above_nz, left_nz) };
    }
    parse_residuals_base(d, probabilities, mb, sp, i16, above_nz, left_nz)
}

/// 树遍历：node + read_bit_val(prob[node>>1]) 沿树下行，叶子（≤0）返回 -child。熵态以调用方
/// 局部标量携带（同 `read_macroblock_modes` Load/Store；`data`/`pos` 仅在 refill 触碰）。
/// 概率读用裸指针（去边界检查）：全部三处调用（SEGMENT_ID / KEYFRAME_Y_MODE / UV_MODE）的可达
/// 节点 node∈{0,2,4,6}（逐树验证于 vp8_tables.rs）⇒ node>>1 ≤ 3 且 < 表长；
/// 三处 probs 表长 3/4/3，len-3 表实际最大下标 2。新增调用点须复查本不变量，
/// 否则改回 `probabilities[node >> 1]` 有界读。
#[inline(always)]
fn read_tree(
    data: &[u8],
    pos: &mut usize,
    value: &mut u64,
    bits: &mut i32,
    range_minus1: &mut u32,
    renorm_skip: bool,
    tree: &[i8],
    probabilities: &[u8],
) -> u32 {
    let mut node = 0usize;
    let base = probabilities.as_ptr();
    loop {
        debug_assert!((node >> 1) < probabilities.len());
        let bit = ent_read_bit(data, pos, value, bits, range_minus1, renorm_skip, unsafe { *base.add(node >> 1) });
        debug_assert!((node + bit as usize) < tree.len());
        #[cfg(feature = "stage_profile")]
        {
            // 树身份区分决策点：三棵调用树各用一处 site 计数（段/亮度模式/色度模式）。
            let site = if std::ptr::eq(tree, &SEGMENT_ID_TREE) {
                skew_clock::SEGMENT
            } else if std::ptr::eq(tree, &KEYFRAME_Y_MODE_TREE) {
                skew_clock::YMODE
            } else {
                skew_clock::UVMODE
            };
            skew_clock::bump(site, bit);
        }
        // 子节点索引裸指针读（并行去边界检查）：树是静态表，`node + bit` 恒 ≤ 表长-1
        // （逐树验证于 vp8_tables.rs；同概率裸读不变量，K32 贴既有机制）。
        let child = unsafe { *tree.as_ptr().add(node + bit as usize) };
        if child <= 0 {
            return -child as u32;
        }
        node = child as usize;
    }
}

/// RFC 6386 §11.5 宏块模式（segment / skip / ymode / 16 个 bmode（仅 B_PRED） / uvmode）。
/// 熵态三标量 {value,bits,range} 每宏块一次 Load/Store（对照 C# `ReadMacroBlockModes` 的
/// `header.Load(out v, out b, out r)` / `header.Store(v, b, r)`）；`data`/`pos` 仅在 refill 触碰。
/// 函数边界 `&mut` + 熵态局部标量拷贝（byval 会逐位回写内存，本形态保寄存器驻留）。
///
/// **RC1 双路径**（同 `parse_residuals`）：分发器按运行时一次判定进 `read_macroblock_modes_bmi`
/// （lzcnt/bmi 快道）或 `read_macroblock_modes_base`（基线 bsr）。两变体函数体由宏
/// `read_macroblock_modes_impl!` 展开自同一算术源——树路由（`read_tree`/内联 `ent_read_bit`）的
/// 指令选择不同不影响字节。`ent_read_signed`（符号位）恒等 sh=1-(range>>7) 免 lzcnt，保持不动。
macro_rules! read_macroblock_modes_impl {
    (
        $header:ident, $segment_probabilities:ident, $skip_probability:ident,
        $above_modes:ident, $left_modes:ident, $block_modes:ident
    ) => ({
        let renorm_skip = ent_renorm_skip();
        let data = $header.data;
        let mut pos = $header.pos;
        let mut value = $header.value;
        let mut bits = $header.bits;
        let mut range_minus1 = $header.range_minus1;
        let segment = match $segment_probabilities {
            Some(probs) => read_tree(
                data,
                &mut pos,
                &mut value,
                &mut bits,
                &mut range_minus1,
                renorm_skip,
                &SEGMENT_ID_TREE,
                probs,
            ),
            None => 0,
        };
        let skip = if $skip_probability < 0 {
            0
        } else {
            let b = ent_read_bit(
                data,
                &mut pos,
                &mut value,
                &mut bits,
                &mut range_minus1,
                renorm_skip,
                $skip_probability as u8,
            );
            probe_skew!(skew_clock::SKIP, b);
            b
        };
        let y_mode = read_tree(
            data,
            &mut pos,
            &mut value,
            &mut bits,
            &mut range_minus1,
            renorm_skip,
            &KEYFRAME_Y_MODE_TREE,
            &KEYFRAME_Y_MODE_PROBS,
        );
        if y_mode == 4 {
            for i in 0..16 {
                let above = if i < 4 { $above_modes[i] } else { $block_modes[i - 4] };
                let leftv = if (i & 3) == 0 {
                    $left_modes[i >> 2]
                } else {
                    $block_modes[i - 1]
                };
                let p = &KEYFRAME_BLOCK_MODE_PROBS[(usize::from(above) * 10 + usize::from(leftv)) * 9..]
                    [..9];
                let mut node = 0usize;
                // 裸指针（同 read_tree 不变量）：`p` 恒长 9、可达 node≤16 ⇒ node>>1 ≤ 8 < 9；
                // `BLOCK_MODE_TREE` 静态表长 18，可达 node≤16 ⇒ node+bit ≤ 17 < 18 → 均去边界检查读。
                let pb = p.as_ptr();
                let tb = BLOCK_MODE_TREE.as_ptr();
                loop {
                    debug_assert!((node >> 1) < p.len());
                    let bit = ent_read_bit(data, &mut pos, &mut value, &mut bits, &mut range_minus1, renorm_skip, unsafe { *pb.add(node >> 1) });
                    debug_assert!((node + bit as usize) < BLOCK_MODE_TREE.len());
                    probe_skew!(skew_clock::BMODE, bit);
                    let child = unsafe { *tb.add(node + bit as usize) };
                    if child <= 0 {
                        $block_modes[i] = -child as u8;
                        break;
                    }
                    node = child as usize;
                }
            }
            for i in 0..4 {
                $above_modes[i] = $block_modes[12 + i];
                $left_modes[i] = $block_modes[i * 4 + 3];
            }
        } else {
            let derived = match y_mode {
                0 => 0u8,
                1 => 2,
                2 => 3,
                _ => 1,
            };
            for m in $above_modes.iter_mut() {
                *m = derived;
            }
            for m in $left_modes.iter_mut() {
                *m = derived;
            }
        }
        let uv_mode = read_tree(
            data,
            &mut pos,
            &mut value,
            &mut bits,
            &mut range_minus1,
            renorm_skip,
            &UV_MODE_TREE,
            &KEYFRAME_UV_MODE_PROBS,
        );
        $header.data = data;
        $header.pos = pos;
        $header.value = value;
        $header.bits = bits;
        $header.range_minus1 = range_minus1;
        segment | (skip << 2) | (y_mode << 3) | (uv_mode << 6)
    })
}

/// 基线变体：与 RC1 前完全相同的指令选择（bsr+xorl $31 / mov→cl）；旧 CPU 与
/// `KIMG_WEBP_ENT_NO_BMI=1` 强制侧走本条。字节恒等由「bmi≡baseline」硬闸对拍。
#[inline(never)]
fn read_macroblock_modes_base(
    header: &mut Vp8BoolDecoder<'_>,
    segment_probabilities: Option<&[u8; 3]>,
    skip_probability: i32,
    above_modes: &mut [u8; 4],
    left_modes: &mut [u8; 4],
    block_modes: &mut [u8; 16],
) -> u32 {
    read_macroblock_modes_impl!(header, segment_probabilities, skip_probability, above_modes, left_modes, block_modes)
}

/// BMI1/2+lzcnt 快道变体：`leading_zeros`→`lzcnt`、变长移位→`shrx/shlx`（免 cl 依赖）；
/// 语义与基线逐字节一致（同宏体），仅指令选择不同。**Safety**：仅当 `ent_bmi_enabled()`
/// 为真（`bmi2` 可用 ⇒ `bmi1`/`lzcnt` 亦可用）时被调用——`lzcnt` 在无 BMI1 CPU 上会被解码成
/// `bsr`（语义反转→错字节），故绝不裸发；非 x86_64 本函数不存在（cfg 剔除）。
#[cfg(target_arch = "x86_64")]
#[inline(never)]
#[target_feature(enable = "lzcnt,bmi1,bmi2")]
unsafe fn read_macroblock_modes_bmi(
    header: &mut Vp8BoolDecoder<'_>,
    segment_probabilities: Option<&[u8; 3]>,
    skip_probability: i32,
    above_modes: &mut [u8; 4],
    left_modes: &mut [u8; 4],
    block_modes: &mut [u8; 16],
) -> u32 {
    read_macroblock_modes_impl!(header, segment_probabilities, skip_probability, above_modes, left_modes, block_modes)
}

/// 宏块模式解码分发器：见「RC1 双路径」说明。树路由/符号位/renorm 全部熵位读落在快道或基线
/// 变体内，两变体由同一宏体展开（算术单源不分裂），仅指令选择不同。
#[inline(never)]
fn read_macroblock_modes(
    header: &mut Vp8BoolDecoder<'_>,
    segment_probabilities: Option<&[u8; 3]>,
    skip_probability: i32,
    above_modes: &mut [u8; 4],
    left_modes: &mut [u8; 4],
    block_modes: &mut [u8; 16],
) -> u32 {
    #[cfg(target_arch = "x86_64")]
    if super::simd::ent_bmi_enabled() {
        // Safety：ent_bmi_enabled() 保证 bmi2（⇒bmi1/lzcnt）可用；快道与基线逐字节一致
        // （「bmi≡baseline」硬闸覆盖），仅指令选择不同。
        return unsafe {
            read_macroblock_modes_bmi(
                header,
                segment_probabilities,
                skip_probability,
                above_modes,
                left_modes,
                block_modes,
            )
        };
    }
    read_macroblock_modes_base(
        header,
        segment_probabilities,
        skip_probability,
        above_modes,
        left_modes,
        block_modes,
    )
}

/// 构造宏块 (mbx, mby) 的 above 缓冲：`above_y[0]`=左上、`[1..17]`=上 16、`[17..21]`=右上 4；
/// chrona `above_u/v[0]`=左上、`[1..9]`=上 8。mbY==0 全 127；左列帧边缘 129。
#[allow(clippy::too_many_arguments)]
fn prepare_above(
    above_y: &mut [u8; 21],
    above_u: &mut [u8; 9],
    above_v: &mut [u8; 9],
    y: &[u8],
    u: &[u8],
    v: &[u8],
    y_pitch: usize,
    uv_pitch: usize,
    mbx: usize,
    mby: usize,
    mbw: usize,
) {
    if mby == 0 {
        above_y.fill(127);
        above_u.fill(127);
        above_v.fill(127);
        return;
    }
    let y_row = mby * 16 - 1;
    let uv_row = mby * 8 - 1;
    let yl = y_row * y_pitch;
    let ul = uv_row * uv_pitch;
    above_y[0] = if mbx == 0 {
        129
    } else {
        y[yl + mbx * 16] // 列 mbx*16-1
    };
    above_u[0] = if mbx == 0 {
        129
    } else {
        u[ul + mbx * 8]
    };
    above_v[0] = if mbx == 0 {
        129
    } else {
        v[ul + mbx * 8]
    };
    for c in 0..16 {
        above_y[1 + c] = y[yl + 1 + mbx * 16 + c];
    }
    for c in 0..8 {
        above_u[1 + c] = u[ul + 1 + mbx * 8 + c];
        above_v[1 + c] = v[ul + 1 + mbx * 8 + c];
    }
    let right = mbx * 16 + 16;
    if mbx + 1 < mbw {
        // 非末列宏块：右上 4 像素取上一行真实重建值（右侧宏块上行的列 right..right+4；
        // 右缘不完整宏块情形下这些列可能落在 padded 区，但同为上一行已重建定稿的字节）。
        for i in 0..4 {
            above_y[17 + i] = y[yl + 1 + right + i];
        }
    } else {
        // 末列宏块：镜像本宏块上行第 16 列（列 mbx*16+15，padded 末位）——与参照
        // image-webp `create_border_luma` 的 `mbx == mbw-1` 分支逐字节一致。此前用
        // `(right + i).min(width - 1)` 夹到可见末列，右缘不完整宏块下与该参照偏差
        // （RFC 6386 右缘 B_PRED 子块 LD/VL 引用右上像素时错一源，宽 W%16 图右缘偏出）。
        let mirror = y[yl + 1 + mbx * 16 + 15];
        for a in above_y[17..21].iter_mut() {
            *a = mirror;
        }
    }
}

/// 分阶段计时（性能探针用）：
/// - 普通构建（无 feature、非 test）：全部为 `f()` 直通（零开销、不手写重复）。
/// - `cfg(test)`：累计各阶段耗时到进程级原子（只读不改写，字节不受影响）。探针
///   `lossy_stage_split_big` 在 release 测试二进制里跑真墙钟，输出熵/预测/变换/滤波占比。
/// - `cfg(feature="stage_profile")`：release 分账用，各阶段**恒计时**到进程级原子（跨滤波
///   线程安全），供 imgbench `vp8_stage_probe`（K59 统一基座 / K53 测量先行）。
pub(crate) mod stage_clock {
    #[cfg(test)]
    use std::sync::atomic::AtomicBool;
    #[cfg(any(test, feature = "stage_profile"))]
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
    #[cfg(any(test, feature = "stage_profile"))]
    use std::time::Instant;

    #[cfg(test)]
    static ON: AtomicBool = AtomicBool::new(false);
    #[cfg(test)]
    static ENTROPY: AtomicU64 = AtomicU64::new(0);
    #[cfg(test)]
    static PREDICT: AtomicU64 = AtomicU64::new(0);
    #[cfg(test)]
    static IDCT: AtomicU64 = AtomicU64::new(0);
    #[cfg(test)]
    static FILTER: AtomicU64 = AtomicU64::new(0);

    // release stage_profile 分账累计（进程级原子：滤波线程/主线程分属两线程，均 fetch_add）。
    #[cfg(feature = "stage_profile")]
    static P_ENTROPY: AtomicU64 = AtomicU64::new(0);
    #[cfg(feature = "stage_profile")]
    static P_PREDICT: AtomicU64 = AtomicU64::new(0);
    #[cfg(feature = "stage_profile")]
    static P_IDCT: AtomicU64 = AtomicU64::new(0);
    #[cfg(feature = "stage_profile")]
    static P_FILTER: AtomicU64 = AtomicU64::new(0);
    #[cfg(feature = "stage_profile")]
    static P_YUV: AtomicU64 = AtomicU64::new(0);
    // 主线程 scope 前后串行段（帧头解析+熵 init / 末段紧密平面拷贝）分账累计（K53 测量先行）。
    #[cfg(feature = "stage_profile")]
    static P_HEADER: AtomicU64 = AtomicU64::new(0);
    #[cfg(feature = "stage_profile")]
    static P_COPY: AtomicU64 = AtomicU64::new(0);
    // 管道 scope 墙钟（主线程，join 阻塞含 worker 全跑；K53 定位 pipeline slack）。
    #[cfg(feature = "stage_profile")]
    static P_SCOPE: AtomicU64 = AtomicU64::new(0);
    // 管道前 setup（分配+平面 memset+装饰器/熵 init）墙钟（主线程；K53 分账）。
    #[cfg(feature = "stage_profile")]
    static P_SETUP: AtomicU64 = AtomicU64::new(0);
    // 管道前 setup 与 scope 之间的剩余（token 解码器/above/row_filters 分配）墙钟（主线程）。
    #[cfg(feature = "stage_profile")]
    static P_MID: AtomicU64 = AtomicU64::new(0);
    // scope join 后、返回前的尾段（Vp8Yuv 构建 + planes 释放）墙钟（主线程）。
    #[cfg(feature = "stage_profile")]
    static P_END: AtomicU64 = AtomicU64::new(0);

    // ---- 细分探针（2026-11-01，Task A/B：IDCT 内 i16 vs i4、熵内 bool/记账/去量化）----
    // IDCT 内部子段墙钟（t2 重建线程，跨线程原子累计，同 P_IDCT 姿势）。
    #[cfg(feature = "stage_profile")]
    static P_IDCT_I16: AtomicU64 = AtomicU64::new(0);
    #[cfg(feature = "stage_profile")]
    static P_IDCT_I4: AtomicU64 = AtomicU64::new(0);
    #[cfg(feature = "stage_profile")]
    static P_IDCT_WHT: AtomicU64 = AtomicU64::new(0);
    #[cfg(feature = "stage_profile")]
    static P_IDCT_UV: AtomicU64 = AtomicU64::new(0);
    // i4 段内子段（2026-11-02，i4 帧内预测向量化决策）：B_PRED 子块预测 vs IDCT-add 各占多少。
    #[cfg(feature = "stage_profile")]
    static P_I4_PREDICT: AtomicU64 = AtomicU64::new(0);
    #[cfg(feature = "stage_profile")]
    static P_I4_IDCT_ADD: AtomicU64 = AtomicU64::new(0);
    #[cfg(feature = "stage_profile")]
    static P_I4_PREDICT_BLOCKS: AtomicU64 = AtomicU64::new(0);
    #[cfg(feature = "stage_profile")]
    static P_I4_IDCT_ADD_BLOCKS: AtomicU64 = AtomicU64::new(0);
    // i4 段内 shell 计数（2026-11-02，K83 修正：撤 per-block Instant、改「外层粗钟 + 原子计数折算」）：
    // subblock_above 抽取次数（恒=predict 块数）/ `mb[i].fill(0)` 清零次数（恒=add 块数）/
    // has-ac `inverse_dct_add_one` 路径块数（拆 add 的 AC 与 DC-only 分账）。
    #[cfg(feature = "stage_profile")]
    static P_I4_ABOVE: AtomicU64 = AtomicU64::new(0);
    #[cfg(feature = "stage_profile")]
    static P_I4_FILL0: AtomicU64 = AtomicU64::new(0);
    #[cfg(feature = "stage_profile")]
    static P_I4_ADD_AC: AtomicU64 = AtomicU64::new(0);
    // 每文件宏块模式计数（i4/i16/WHT 执行次数）；供「i4 宏块占比 / WHT 次数」汇总。
    #[cfg(feature = "stage_profile")]
    static P_MB_I16: AtomicU64 = AtomicU64::new(0);
    #[cfg(feature = "stage_profile")]
    static P_MB_I4: AtomicU64 = AtomicU64::new(0);
    #[cfg(feature = "stage_profile")]
    static P_MB_WHT: AtomicU64 = AtomicU64::new(0);
    // 熵内部子段：模式段 / 残差段墙钟。
    #[cfg(feature = "stage_profile")]
    static P_ENT_MODES: AtomicU64 = AtomicU64::new(0);
    #[cfg(feature = "stage_profile")]
    static P_ENT_TOKENS: AtomicU64 = AtomicU64::new(0);
    // 熵内部三类操作计数（换算时间占比用，K53 口径）：
    // P_BITS = read_bit_val/read_signed_val 抛币总次数（header 段读也计入，靠段内 delta 归位）；
    // P_MODES_BITS / P_TOKENS_BITS = 模式段 / 残差段各自的 bool 读次数（段闭包内 delta 累加）；
    // P_ACCT = token 记账/游标/索引次数；P_DQ = 去量化+zigzag 写回次数。
    #[cfg(feature = "stage_profile")]
    static P_BITS: AtomicU64 = AtomicU64::new(0);
    #[cfg(feature = "stage_profile")]
    static P_MODES_BITS: AtomicU64 = AtomicU64::new(0);
    #[cfg(feature = "stage_profile")]
    static P_TOKENS_BITS: AtomicU64 = AtomicU64::new(0);
    #[cfg(feature = "stage_profile")]
    static P_ACCT: AtomicU64 = AtomicU64::new(0);
    #[cfg(feature = "stage_profile")]
    static P_DQ: AtomicU64 = AtomicU64::new(0);

    #[cfg(test)]
    pub fn set(on: bool) {
        ON.store(on, Relaxed);
    }
    #[cfg(test)]
    pub fn reset() {
        ENTROPY.store(0, Relaxed);
        PREDICT.store(0, Relaxed);
        IDCT.store(0, Relaxed);
        FILTER.store(0, Relaxed);
    }
    /// 返回四阶段累计毫秒：(entropy, predict, idct, filter)。
    #[cfg(test)]
    pub fn totals() -> (f64, f64, f64, f64) {
        (
            ENTROPY.load(Relaxed) as f64 / 1e6,
            PREDICT.load(Relaxed) as f64 / 1e6,
            IDCT.load(Relaxed) as f64 / 1e6,
            FILTER.load(Relaxed) as f64 / 1e6,
        )
    }

    /// 清零一次解码的分账累计（`stage_profile` 入口 `stage_decode` 调用）。
    #[cfg(feature = "stage_profile")]
    pub fn reset_profile() {
        P_ENTROPY.store(0, Relaxed);
        P_PREDICT.store(0, Relaxed);
        P_IDCT.store(0, Relaxed);
        P_FILTER.store(0, Relaxed);
        P_YUV.store(0, Relaxed);
        P_HEADER.store(0, Relaxed);
        P_COPY.store(0, Relaxed);
        P_SCOPE.store(0, Relaxed);
        P_SETUP.store(0, Relaxed);
        P_MID.store(0, Relaxed);
        P_END.store(0, Relaxed);
        P_IDCT_I16.store(0, Relaxed);
        P_IDCT_I4.store(0, Relaxed);
        P_IDCT_WHT.store(0, Relaxed);
        P_IDCT_UV.store(0, Relaxed);
        P_I4_PREDICT.store(0, Relaxed);
        P_I4_IDCT_ADD.store(0, Relaxed);
        P_I4_PREDICT_BLOCKS.store(0, Relaxed);
        P_I4_IDCT_ADD_BLOCKS.store(0, Relaxed);
        P_I4_ABOVE.store(0, Relaxed);
        P_I4_FILL0.store(0, Relaxed);
        P_I4_ADD_AC.store(0, Relaxed);
        P_MB_I16.store(0, Relaxed);
        P_MB_I4.store(0, Relaxed);
        P_MB_WHT.store(0, Relaxed);
        P_ENT_MODES.store(0, Relaxed);
        P_ENT_TOKENS.store(0, Relaxed);
        P_BITS.store(0, Relaxed);
        P_MODES_BITS.store(0, Relaxed);
        P_TOKENS_BITS.store(0, Relaxed);
        P_ACCT.store(0, Relaxed);
        P_DQ.store(0, Relaxed);
    }
    /// 读取一次解码各阶段累计 ns：(entropy, predict, idct, filter, yuv, header, copy, scope, setup, mid, end)。
    #[cfg(feature = "stage_profile")]
    pub fn profile_totals() -> (u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) {
        (
            P_ENTROPY.load(Relaxed),
            P_PREDICT.load(Relaxed),
            P_IDCT.load(Relaxed),
            P_FILTER.load(Relaxed),
            P_YUV.load(Relaxed),
            P_HEADER.load(Relaxed),
            P_COPY.load(Relaxed),
            P_SCOPE.load(Relaxed),
            P_SETUP.load(Relaxed),
            P_MID.load(Relaxed),
            P_END.load(Relaxed),
        )
    }

    /// 细分探针读取（IDCT 内部子段 / 宏块模式计数 / 熵内部子段与操作计数）。feature 外不存在。
    #[cfg(feature = "stage_profile")]
    #[derive(Default, Clone, Copy)]
    pub(crate) struct ProfileSub {
        pub idct_i16: u64,
        pub idct_i4: u64,
        pub idct_wht: u64,
        pub idct_uv: u64,
        pub i4_predict_ns: u64,
        pub i4_idct_add_ns: u64,
        pub i4_predict_blocks: u64,
        pub i4_idct_add_blocks: u64,
        /// subblock_above 抽取次数（恒 = predict 块数）笼罩子块循环壳构成分账。
        pub i4_above_blocks: u64,
        /// `mb[i].fill(0)` 清零次数（恒 = add 块数）。
        pub i4_fill0_blocks: u64,
        /// has-ac `inverse_dct_add_one` 块数（add = AC 路径 + 标量 DC-only 捷径）。
        pub i4_add_ac_blocks: u64,
        pub mb_i16: u64,
        pub mb_i4: u64,
        pub mb_wht: u64,
        pub ent_modes: u64,
        pub ent_tokens: u64,
        /// 模式段 bool 读次数（P_BITS delta）。
        pub modes_bits: u64,
        /// 残差段 bool 读次数（P_BITS delta）。
        pub tokens_bits: u64,
        /// 去量化+zigzag 写回次数。
        pub dq: u64,
        /// token 记账/游标/索引次数。
        pub acct: u64,
    }
    /// 读取细分探针累计。
    #[cfg(feature = "stage_profile")]
    pub fn profile_sub() -> ProfileSub {
        ProfileSub {
            idct_i16: P_IDCT_I16.load(Relaxed),
            idct_i4: P_IDCT_I4.load(Relaxed),
            idct_wht: P_IDCT_WHT.load(Relaxed),
            idct_uv: P_IDCT_UV.load(Relaxed),
            i4_predict_ns: P_I4_PREDICT.load(Relaxed),
            i4_idct_add_ns: P_I4_IDCT_ADD.load(Relaxed),
            i4_predict_blocks: P_I4_PREDICT_BLOCKS.load(Relaxed),
            i4_idct_add_blocks: P_I4_IDCT_ADD_BLOCKS.load(Relaxed),
            i4_above_blocks: P_I4_ABOVE.load(Relaxed),
            i4_fill0_blocks: P_I4_FILL0.load(Relaxed),
            i4_add_ac_blocks: P_I4_ADD_AC.load(Relaxed),
            mb_i16: P_MB_I16.load(Relaxed),
            mb_i4: P_MB_I4.load(Relaxed),
            mb_wht: P_MB_WHT.load(Relaxed),
            ent_modes: P_ENT_MODES.load(Relaxed),
            ent_tokens: P_ENT_TOKENS.load(Relaxed),
            modes_bits: P_MODES_BITS.load(Relaxed),
            tokens_bits: P_TOKENS_BITS.load(Relaxed),
            dq: P_DQ.load(Relaxed),
            acct: P_ACCT.load(Relaxed),
        }
    }

    /// 熵·模式段子计时：累加墙钟到 `P_ENT_MODES`，并把段内 `read_bit_val`/`read_signed_val`
    /// 调用数（`P_BITS` delta）累加进 `P_MODES_BITS`。feature 外纯透传（不改变行为）。
    #[inline(always)]
    pub fn entropy_modes<R>(f: impl FnOnce() -> R) -> R {
        #[cfg(feature = "stage_profile")]
        {
            let t = Instant::now();
            let b0 = P_BITS.load(Relaxed);
            let r = f();
            P_ENT_MODES.fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
            P_MODES_BITS.fetch_add(P_BITS.load(Relaxed) - b0, Relaxed);
            return r;
        }
        #[cfg(not(feature = "stage_profile"))]
        {
            f()
        }
    }
    /// 熵·残差段子计时：同熵·模式段，占 `P_ENT_TOKENS` / `P_TOKENS_BITS`。
    #[inline(always)]
    pub fn entropy_tokens<R>(f: impl FnOnce() -> R) -> R {
        #[cfg(feature = "stage_profile")]
        {
            let t = Instant::now();
            let b0 = P_BITS.load(Relaxed);
            let r = f();
            P_ENT_TOKENS.fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
            P_TOKENS_BITS.fetch_add(P_BITS.load(Relaxed) - b0, Relaxed);
            return r;
        }
        #[cfg(not(feature = "stage_profile"))]
        {
            f()
        }
    }
    /// IDCT·i16（16x16/8x8：2 块 SSE2 `inverse_dct_add_two`/`dc_add_two`）子计时。
    #[inline(always)]
    pub fn idct_i16<R>(f: impl FnOnce() -> R) -> R {
        #[cfg(feature = "stage_profile")]
        {
            let t = Instant::now();
            let r = f();
            P_IDCT_I16.fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
            return r;
        }
        #[cfg(not(feature = "stage_profile"))]
        {
            f()
        }
    }
    /// IDCT·i4x4 标量逐块（含 `predict_subblock` 子块预测 + `inverse_dct_add`）子计时。
    #[inline(always)]
    pub fn idct_i4<R>(f: impl FnOnce() -> R) -> R {
        #[cfg(feature = "stage_profile")]
        {
            let t = Instant::now();
            let r = f();
            P_IDCT_I4.fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
            return r;
        }
        #[cfg(not(feature = "stage_profile"))]
        {
            f()
        }
    }
    /// IDCT·WHT（`inverse_walsh_to_blocks`）子计时。
    #[inline(always)]
    pub fn idct_wht<R>(f: impl FnOnce() -> R) -> R {
        #[cfg(feature = "stage_profile")]
        {
            let t = Instant::now();
            let r = f();
            P_IDCT_WHT.fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
            return r;
        }
        #[cfg(not(feature = "stage_profile"))]
        {
            f()
        }
    }
    /// IDCT·UV 成对（i16 与 i4 共用的色度 2 块路径；单独列以便 IDCT 内分账闭包对齐）。
    #[inline(always)]
    pub fn idct_uv<R>(f: impl FnOnce() -> R) -> R {
        #[cfg(feature = "stage_profile")]
        {
            let t = Instant::now();
            let r = f();
            P_IDCT_UV.fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
            return r;
        }
        #[cfg(not(feature = "stage_profile"))]
        {
            f()
        }
    }
    /// i4 段内·B_PRED 子块预测（per-block `predict_subblock` + `subblock_above`）。
    /// K83 修正（2026-11-02）：不再逐块 `Instant::now()`（插桩通胀），只累加原子计数
    /// （predict 块 + above 抽取各一次，恒 16×mark）；时间由外层 `idct_i4` 粗钟与
    /// `probe_i4_op_costs` 折算。feature 外纯透传（不改变行为）。
    #[inline(always)]
    pub fn i4_predict<R>(f: impl FnOnce() -> R) -> R {
        #[cfg(feature = "stage_profile")]
        {
            P_I4_PREDICT_BLOCKS.fetch_add(1, Relaxed);
            P_I4_ABOVE.fetch_add(1, Relaxed);
        }
        f()
    }
    /// i4 段内·IDCT-add（`inverse_dct_add_one` / 标量 `inverse_dct_add` + `fill(0)`）。
    /// K83 修正（2026-11-02）：同 `i4_predict`，只累加原子计数（add 块 + fill(0) 各一次）。
    #[inline(always)]
    pub fn i4_idct_add<R>(f: impl FnOnce() -> R) -> R {
        #[cfg(feature = "stage_profile")]
        {
            P_I4_IDCT_ADD_BLOCKS.fetch_add(1, Relaxed);
            P_I4_FILL0.fetch_add(1, Relaxed);
        }
        f()
    }
    /// i4 段内·has-ac `inverse_dct_add_one` 路径计数（拆 add 的 AC 与 DC-only 分账）。
    /// feature 外空操作（不改变行为）。
    #[inline(always)]
    pub fn i4_add_ac() {
        #[cfg(feature = "stage_profile")]
        {
            P_I4_ADD_AC.fetch_add(1, Relaxed);
        }
    }
    /// 宏块模式计数：每宏块恰好一次（i16⇔y_mode!=4 之外即 i4）。feature 外空操作。
    #[inline(always)]
    pub fn mb_mode(is_i16: bool) {
        #[cfg(feature = "stage_profile")]
        {
            if is_i16 {
                P_MB_I16.fetch_add(1, Relaxed);
            } else {
                P_MB_I4.fetch_add(1, Relaxed);
            }
        }
        #[cfg(not(feature = "stage_profile"))]
        {}
    }
    /// WHT 执行次数（`mb[24]` WHT 实际运行的宏块计数）。
    #[inline(always)]
    pub fn mb_wht() {
        #[cfg(feature = "stage_profile")]
        {
            P_MB_WHT.fetch_add(1, Relaxed);
        }
        #[cfg(not(feature = "stage_profile"))]
        {}
    }
    /// bool 位读取计数（`read_bit_val` / `read_signed_val` 抛币各一次）。
    #[inline(always)]
    pub fn bit() {
        #[cfg(feature = "stage_profile")]
        {
            P_BITS.fetch_add(1, Relaxed);
        }
        #[cfg(not(feature = "stage_profile"))]
        {}
    }
    /// token 记账/游标/索引计数（position/p_idx/last/块序 bookkeeping）。
    #[inline(always)]
    pub fn acct() {
        #[cfg(feature = "stage_profile")]
        {
            P_ACCT.fetch_add(1, Relaxed);
        }
        #[cfg(not(feature = "stage_profile"))]
        {}
    }
    /// 去量化+zigzag 写回计数（每非零系数一次）。
    #[inline(always)]
    pub fn dq() {
        #[cfg(feature = "stage_profile")]
        {
            P_DQ.fetch_add(1, Relaxed);
        }
        #[cfg(not(feature = "stage_profile"))]
        {}
    }

    #[inline(always)]
    pub fn entropy<R>(f: impl FnOnce() -> R) -> R {
        #[cfg(feature = "stage_profile")]
        {
            let t = Instant::now();
            let r = f();
            P_ENTROPY.fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
            return r;
        }
        #[cfg(not(feature = "stage_profile"))]
        {
            #[cfg(test)]
            {
                if !ON.load(Relaxed) {
                    return f();
                }
                let t = Instant::now();
                let r = f();
                ENTROPY.fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
                return r;
            }
            #[cfg(not(test))]
            {
                f()
            }
        }
    }
    #[inline(always)]
    pub fn predict<R>(f: impl FnOnce() -> R) -> R {
        #[cfg(feature = "stage_profile")]
        {
            let t = Instant::now();
            let r = f();
            P_PREDICT.fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
            return r;
        }
        #[cfg(not(feature = "stage_profile"))]
        {
            #[cfg(test)]
            {
                if !ON.load(Relaxed) {
                    return f();
                }
                let t = Instant::now();
                let r = f();
                PREDICT.fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
                return r;
            }
            #[cfg(not(test))]
            {
                f()
            }
        }
    }
    #[inline(always)]
    pub fn idct<R>(f: impl FnOnce() -> R) -> R {
        #[cfg(feature = "stage_profile")]
        {
            let t = Instant::now();
            let r = f();
            P_IDCT.fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
            return r;
        }
        #[cfg(not(feature = "stage_profile"))]
        {
            #[cfg(test)]
            {
                if !ON.load(Relaxed) {
                    return f();
                }
                let t = Instant::now();
                let r = f();
                IDCT.fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
                return r;
            }
            #[cfg(not(test))]
            {
                f()
            }
        }
    }
    #[inline(always)]
    pub fn filter<R>(f: impl FnOnce() -> R) -> R {
        #[cfg(feature = "stage_profile")]
        {
            let t = Instant::now();
            let r = f();
            P_FILTER.fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
            return r;
        }
        #[cfg(not(feature = "stage_profile"))]
        {
            #[cfg(test)]
            {
                if !ON.load(Relaxed) {
                    return f();
                }
                let t = Instant::now();
                let r = f();
                FILTER.fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
                return r;
            }
            #[cfg(not(test))]
            {
                f()
            }
        }
    }
    /// YUV 定稿直出（fancy 上采样 + YUV→RGBA）阶段计时：普通/测试构建直通，`stage_profile`
    /// 下累计到 `P_YUV`。`emit_rows_into_rgba`（串行 + P-B 滤波线程两路径）调用。
    #[inline(always)]
    pub fn yuv<R>(f: impl FnOnce() -> R) -> R {
        #[cfg(feature = "stage_profile")]
        {
            let t = Instant::now();
            let r = f();
            P_YUV.fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
            return r;
        }
        #[cfg(not(feature = "stage_profile"))]
        {
            f()
        }
    }
    /// 帧头解析 + 熵 init（主线程，管道开启前）计时：`stage_profile` 下累计到 `P_HEADER`。
    #[inline(always)]
    pub fn header<R>(f: impl FnOnce() -> R) -> R {
        #[cfg(feature = "stage_profile")]
        {
            let t = Instant::now();
            let r = f();
            P_HEADER.fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
            return r;
        }
        #[cfg(not(feature = "stage_profile"))]
        {
            f()
        }
    }
    /// 末段紧密平面抽取（主线程，scope join 后）计时：`stage_profile` 下累计到 `P_COPY`。
    #[inline(always)]
    pub fn copy<R>(f: impl FnOnce() -> R) -> R {
        #[cfg(feature = "stage_profile")]
        {
            let t = Instant::now();
            let r = f();
            P_COPY.fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
            return r;
        }
        #[cfg(not(feature = "stage_profile"))]
        {
            f()
        }
    }
    /// 管道 scope 整段墙钟：`stage_profile` 下累计到 `P_SCOPE`（主线程 join 阻塞全景）。
    #[inline(always)]
    pub fn scope_wall<R>(f: impl FnOnce() -> R) -> R {
        #[cfg(feature = "stage_profile")]
        {
            let t = Instant::now();
            let r = f();
            P_SCOPE.fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
            return r;
        }
        #[cfg(not(feature = "stage_profile"))]
        {
            f()
        }
    }
    /// 管道前 setup（分配+平面 memset）墙钟：`stage_profile` 下累计到 `P_SETUP`。
    #[inline(always)]
    pub fn setup<R>(f: impl FnOnce() -> R) -> R {
        #[cfg(feature = "stage_profile")]
        {
            let t = Instant::now();
            let r = f();
            P_SETUP.fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
            return r;
        }
        #[cfg(not(feature = "stage_profile"))]
        {
            f()
        }
    }
    /// setup 与 scope 之间的剩余（token 解码器/above/row_filters）：累计到 `P_MID`。
    #[inline(always)]
    pub fn mid<R>(f: impl FnOnce() -> R) -> R {
        #[cfg(feature = "stage_profile")]
        {
            let t = Instant::now();
            let r = f();
            P_MID.fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
            return r;
        }
        #[cfg(not(feature = "stage_profile"))]
        {
            f()
        }
    }
    /// scope join 后、返回前尾段（Vp8Yuv 构建 + planes 释放）：累计到 `P_END`。
    #[inline(always)]
    pub fn end<R>(f: impl FnOnce() -> R) -> R {
        #[cfg(feature = "stage_profile")]
        {
            let t = Instant::now();
            let r = f();
            P_END.fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
            return r;
        }
        #[cfg(not(feature = "stage_profile"))]
        {
            f()
        }
    }
}

/// VP8 有损阶帧分账（`stage_profile` feature 下由 `stage_decode` 填充；供 imgbench
/// `vp8_stage_probe`，K59 统一基座）。本结构体无条件编译（供签名线程），计时仅 feature 下发生，
/// feature 关闭时零运行时影响。
#[cfg_attr(not(feature = "stage_profile"), allow(dead_code))]
#[derive(Default, Clone, Copy, Debug)]
pub struct Vp8StageProfile {
    pub width: u32,
    pub height: u32,
    /// 熵解码（宏块模式 + 残差 token 解析 + bool+去量化）。
    pub entropy_ns: u64,
    /// 帧内预测（prepare_above + 16x16/8x8 平面预测）。
    pub predict_ns: u64,
    /// 逆变换（WHT + 4x4 IDCT(成对/单块) + B_PRED 子块预测）。
    pub idct_ns: u64,
    /// 环路滤波（filter_macroblock_row）。
    pub filter_ns: u64,
    /// YUV 定稿直出（fancy 上采样 + YUV→RGBA）。
    pub yuv_ns: u64,
    /// 帧头解析 + 熵 init（主线程，管道开启前；K53 分账）。
    pub header_ns: u64,
    /// 末段紧密平面抽取（主线程，scope join 后；K53 分账）。
    pub copy_ns: u64,
    /// 管道 `std::thread::scope` 整段墙钟（主线程 join 阻塞，含 worker 全跑 + 调度等待）。
    pub scope_ns: u64,
    /// 管道前 setup（planes memset + rgba 分配；主线程）。
    pub setup_ns: u64,
    /// setup 与 scope 之间剩余（token 解码器/above/row_filters 分配）。
    pub mid_ns: u64,
    /// scope join 后、返回前尾段（Vp8Yuv 构建）。
    pub end_ns: u64,
    /// 全链墙钟（含帧头解析与平面拷贝）。
    pub total_ns: u64,
    /// 是否走了 P-B 滤波线程管线（true 时 filter/yuv 在独立线程累计）。
    pub pipelined: bool,
    /// IDCT 内部子段墙钟（仅 `stage_profile` 探针填充；feature 外恒 0）。
    pub idct_i16_ns: u64,
    pub idct_i4_ns: u64,
    pub idct_wht_ns: u64,
    pub idct_uv_ns: u64,
    /// i4 段内子段墙钟：B_PRED 子块预测 vs IDCT-add（仅 `stage_profile` 探针填充）。
    pub i4_predict_ns: u64,
    pub i4_idct_add_ns: u64,
    /// i4 段内块计数：predict 子块次数（恒 16×mb_i4）/ idct-add 子块次数（非零块）。
    pub i4_predict_blocks: u64,
    pub i4_idct_add_blocks: u64,
    /// i4 段内 shell 计数（折算用）：above 抽取 / fill(0) / has-ac add 块数。
    pub i4_above_blocks: u64,
    pub i4_fill0_blocks: u64,
    pub i4_add_ac_blocks: u64,
    /// 宏块模式计数（每文件）：i16 宏块数 / i4(B_PRED) 宏块数 / WHT 执行次数。
    pub mb_i16: u64,
    pub mb_i4: u64,
    pub mb_wht: u64,
    /// 熵内部墙钟：模式段 / 残差段。
    pub ent_modes_ns: u64,
    pub ent_tokens_ns: u64,
    /// 熵内部操作计数：模式段 bool 读 / 残差段 bool 读 / 记账·游标·索引 / 去量化+zigzag 写回。
    pub modes_bits: u64,
    pub tokens_bits: u64,
    pub acct_count: u64,
    pub dq_count: u64,
}

/// 完整解码：解析帧头 + 重建全部宏块 → 紧密 YUV 4:2:0（V2 暂不含环路滤波，V3 接入）。
/// 生产路径（`need_tight=false`）跳过紧密 y/u/v 抽平面与分配——输出以 `Vp8Yuv.rgba`（行定稿
/// 直出 RGBA4，α=255）为准，`y/u/v` 为空 Vec；测试/兼容 API 用 [`decode_vp8_tight`] 取紧密平面。
pub(crate) fn decode_vp8(frame: &[u8], width: u32, height: u32) -> Result<Vp8Yuv> {
    let fh = stage_clock::header(|| parse_frame_header(frame))?;
    if fh.width != width || fh.height != height {
        return Err(Vp8Error::new("VP8 与容器尺寸不一致"));
    }
    decode_from_header(fh, false, 4, None)
}

/// lossy-alpha 专用：同 [`decode_vp8`]（`need_tight=false`），但行定稿直出时把 `alpha` 平面
/// （w*h 字节，`decode_and_unfilter_alpha` 的整帧结果）逐行并入 RGBA 发射内层——每像素 4 字节
/// 一次写好（YUV→RGB 结果 R/G/B 与 alpha 同拍），消灭「RGBA4 全帧发射 + 事后独立标量逐像素
/// 写 alpha」两遍合成。`alpha=None`（无 ALPH chunk 的 malformed 输入）时 α 恒 255，与
/// `decode_vp8` 逐字节一致。产物落 `Vp8Yuv.rgba`。
pub(crate) fn decode_vp8_with_alpha(
    frame: &[u8],
    width: u32,
    height: u32,
    alpha: Option<&[u8]>,
) -> Result<Vp8Yuv> {
    let fh = stage_clock::header(|| parse_frame_header(frame))?;
    if fh.width != width || fh.height != height {
        return Err(Vp8Error::new("VP8 与容器尺寸不一致"));
    }
    if let Some(a) = alpha {
        debug_assert_eq!(a.len(), (width as usize) * (height as usize));
    }
    decode_from_header(fh, false, 4, alpha)
}

/// 【dormancy，K24】opaque 有损 RGB3 直出旧入口：同 [`decode_vp8`]（`need_tight=false`）且行定稿
/// 直出 **RGB3**（w*h*3，alpha 不写），产物落 `Vp8Yuv.rgb`。自 T1 起 opaque 有损改用
/// [`decode_vp8`]（RGBA4 α=255 单遍直出，`to_rgba8`/`into_rgba8` 零拷贝），本入口与
/// `emit_rows_into_rgb3`/`yuv_to_rgb_fancy` 保留作旧口径参考（`ours_backend_webpdecoder`/
/// 硬闸对拍）+ 手工构造回退，不删（K24）。每像素前 3 字节仍与 `decode_vp8` 的 RGBA 恒等。
#[allow(dead_code)]
pub(crate) fn decode_vp8_rgb3(frame: &[u8], width: u32, height: u32) -> Result<Vp8Yuv> {
    let fh = stage_clock::header(|| parse_frame_header(frame))?;
    if fh.width != width || fh.height != height {
        return Err(Vp8Error::new("VP8 与容器尺寸不一致"));
    }
    decode_from_header(fh, false, 3, None)
}

/// 测试/兼容路径：同 [`decode_vp8`] 且一并抽紧密 y/u/v 平面（V2/V3 YUV golden 对拍用）。
#[cfg(test)]
pub(crate) fn decode_vp8_tight(frame: &[u8], width: u32, height: u32) -> Result<Vp8Yuv> {
    let fh = stage_clock::header(|| parse_frame_header(frame))?;
    if fh.width != width || fh.height != height {
        return Err(Vp8Error::new("VP8 与容器尺寸不一致"));
    }
    decode_from_header(fh, true, 4, None)
}

/// 阶段分账解码（`stage_profile` feature 门控；供 imgbench `vp8_stage_probe`，K59 统一基座）。
/// 与 [`decode_vp8`] 语义逐字节一致，另按【熵 / 预测 / 逆变换 / 环路滤波 / YUV 直出】统计各段
/// 墙钟（进程级原子，P-B 滤波线程路径跨线程累计）。`frame` 为 VP8 帧载荷。仅计时（返回 profile）。
#[cfg(feature = "stage_profile")]
pub fn stage_decode(frame: &[u8], width: u32, height: u32) -> Result<Vp8StageProfile> {
    use std::time::Instant;
    stage_clock::reset_profile();
    let t0 = Instant::now();
    let yuv = decode_vp8(frame, width, height)?;
    let total_ns = t0.elapsed().as_nanos() as u64;
    std::hint::black_box(&yuv.rgba);
    let (e, p, i, f, y, h, c, s, su, m, e2) = stage_clock::profile_totals();
    let sub = stage_clock::profile_sub();
    Ok(Vp8StageProfile {
        width,
        height,
        entropy_ns: e,
        predict_ns: p,
        idct_ns: i,
        filter_ns: f,
        yuv_ns: y,
        header_ns: h,
        copy_ns: c,
        scope_ns: s,
        setup_ns: su,
        mid_ns: m,
        end_ns: e2,
        total_ns,
        pipelined: filter_mt_enabled(
            (width as usize + 15) / 16,
            (height as usize + 15) / 16,
        ),
        idct_i16_ns: sub.idct_i16,
        idct_i4_ns: sub.idct_i4,
        idct_wht_ns: sub.idct_wht,
        idct_uv_ns: sub.idct_uv,
        i4_predict_ns: sub.i4_predict_ns,
        i4_idct_add_ns: sub.i4_idct_add_ns,
        i4_predict_blocks: sub.i4_predict_blocks,
        i4_idct_add_blocks: sub.i4_idct_add_blocks,
        i4_above_blocks: sub.i4_above_blocks,
        i4_fill0_blocks: sub.i4_fill0_blocks,
        i4_add_ac_blocks: sub.i4_add_ac_blocks,
        mb_i16: sub.mb_i16,
        mb_i4: sub.mb_i4,
        mb_wht: sub.mb_wht,
        ent_modes_ns: sub.ent_modes,
        ent_tokens_ns: sub.ent_tokens,
        modes_bits: sub.modes_bits,
        tokens_bits: sub.tokens_bits,
        acct_count: sub.acct,
        dq_count: sub.dq,
    })
}

/// 熵内部三类操作的单位耗时探针（`stage_profile` feature 门控；供 imgbench `vp8_stage_probe`
/// 把操作计数换算成时间占比，K53 测量口径）。返回每操作纳秒：`(bool 位读取, 记账·游标, 去量化+zigzag 写回)`。
/// 在同一（已插桩）构建里测量 → 与产线探针的分账自洽（换算分母与实测熵占比同源）。
#[cfg(feature = "stage_profile")]
pub fn probe_entropy_op_costs(reps: u32) -> (f64, f64, f64) {
    use std::time::Instant;
    // 1) bool 位读取：read_bit_val 实函数热循环（含 refill 分支，概率用 128 典型值）。
    let data = vec![0x55u8; 4096];
    let mut d = Vp8BoolDecoder::new(&data);
    let mut acc = 0u64;
    let t = Instant::now();
    for _ in 0..reps {
        let (s, b) = d.read_bit_val(128);
        d = s;
        acc = acc.wrapping_add(u64::from(b));
    }
    let c_bit = t.elapsed().as_nanos() as f64 / reps as f64;
    // 2) 记账·游标·索引：tokens 里 position/last/p_idx + BAND_ROW_OFFSETS 序列。
    let mut pos = 0usize;
    let mut last = 0usize;
    let mut p_i = 0usize;
    let t = Instant::now();
    for _ in 0..reps {
        pos += 1;
        if pos == 16 {
            pos = 0;
        }
        p_i = BAND_ROW_OFFSETS[pos] + 11;
        last = pos + 1;
    }
    std::hint::black_box((p_i, last, pos));
    let c_acct = t.elapsed().as_nanos() as f64 / reps as f64;
    // 3) 去量化+zigzag 写回：乘 factor + ZIGZAG 索引 + s16 存储。
    let mut out = [0i16; 16];
    let mut pos = 0usize;
    let mut signed = if (acc & 1) == 0 { 17i32 } else { -17i32 };
    let t = Instant::now();
    for _ in 0..reps {
        pos = (pos + 1) & 15;
        out[ZIGZAG[pos] as usize] = (signed.wrapping_mul(53)) as i16;
    }
    std::hint::black_box(&out);
    std::hint::black_box(signed);
    let c_dq = t.elapsed().as_nanos() as f64 / reps as f64;
    std::hint::black_box(acc);
    (c_bit, c_acct, c_dq)
}

/// i4 段内各操作的单位耗时（ns/op），供「原子计数 → 时间」折算。
#[cfg(feature = "stage_profile")]
#[derive(Default, Clone, Copy)]
pub struct I4OpCosts {
    /// `subblock_above` 一抽（含 block_y==0 / 中列 / 右列三分支）。
    pub c_above: f64,
    /// `predict_subblock` 本体（above 已备好；左列读取 + 10 模式写回，等权平均）。
    pub c_predict: f64,
    /// `inverse_dct_add(has_ac=false)` DC-only 标量捷径。
    pub c_add_dc: f64,
    /// `inverse_dct_add_one` has-ac 路径（SSE2 单块核，与产线同门控）。
    pub c_add_ac: f64,
    /// `[i16;16].fill(0)`。
    pub c_fill: f64,
}

/// i4 段内五类操作的单位耗时探针（`stage_profile` feature 门控；供 imgbench `vp8_stage_probe`
/// 把 predict / idct_add / fill(0) / subblock_above 原子计数折算成时间）。K83 修正方案
/// （2026-11-02）：per-block 墙钟插桩通胀（残差 ~18% 不可信）→ 外层 `idct_i4` 粗钟给总账，
/// 块内操作按本探针单位成本折算，差即循环壳净成本。与产线同构建测量（同 release / 同 feature），
/// 模拟单 i4 宏块 16 块暖缓存布局（pitch=17、块按 bx/by 顺序扫），代表真实解码 L1 缓存域。
#[cfg(feature = "stage_profile")]
pub fn probe_i4_op_costs(reps: u32) -> I4OpCosts {
    use std::time::Instant;
    let pitch = 17usize;
    let mut plane = vec![128u8; pitch * 64];
    let above_y = [42u8; 21];
    // 每块一个固定 above 行（形态同真实 `subblock_above` 输出；模式 0..10 等权循环）。
    let mut a_blk = [[0u8; 9]; 16];
    for (i, a) in a_blk.iter_mut().enumerate() {
        for (j, v) in a.iter_mut().enumerate() {
            *v = (i as u8).wrapping_mul(13).wrapping_add(j as u8);
        }
    }
    let modes = [0u8, 1, 2, 3, 4, 5, 6, 7, 8, 9, 0, 1, 2, 3, 4, 5];
    let mut acc = 0u64;

    // 1) subblock_above 本体（三分支按块位均摊）。
    let t = Instant::now();
    for k in 0..reps {
        let bx = (k & 3) as usize;
        let by = ((k >> 2) & 3) as usize;
        let a = subblock_above(&plane, pitch, by * 4, bx * 4, bx, by, &above_y);
        acc = acc.wrapping_add(u64::from(a[0] + a[8]));
    }
    std::hint::black_box(&plane);
    let c_above = t.elapsed().as_nanos() as f64 / reps as f64;

    // 2) predict_subblock 本体（above 已备好，仅左列读取 + 写回）。
    let t = Instant::now();
    for k in 0..reps {
        let bx = (k & 3) as usize;
        let by = ((k >> 2) & 3) as usize;
        let i = (k & 15) as usize;
        super::simd::predict_subblock(&mut plane, pitch, by * 4, bx * 4, modes[i], &a_blk[i]);
    }
    std::hint::black_box(&plane);
    let c_predict = t.elapsed().as_nanos() as f64 / reps as f64;

    // 3) inverse_dct_add DC-only 标量捷径（has_ac=false，非零 DC）。
    let mut coeff = [0i16; 16];
    coeff[0] = 19;
    let t = Instant::now();
    for k in 0..reps {
        let bx = (k & 3) as usize;
        let by = ((k >> 2) & 3) as usize;
        inverse_dct_add(&coeff, &mut plane, pitch, by * 4, bx * 4, false);
    }
    std::hint::black_box(&plane);
    let c_add_dc = t.elapsed().as_nanos() as f64 / reps as f64;

    // 4) inverse_dct_add_one（has-ac，AC 系数非零；SSE2 单块核，与产线同门控）。
    let mut c = [0i16; 16];
    c[0] = 17;
    c[1] = 23;
    c[2] = -9;
    c[3] = 11;
    c[4] = -7;
    let t = Instant::now();
    for k in 0..reps {
        let bx = (k & 3) as usize;
        let by = ((k >> 2) & 3) as usize;
        super::simd::inverse_dct_add_one(&c, &mut plane, pitch, by * 4, bx * 4);
    }
    std::hint::black_box(&plane);
    let c_add_ac = t.elapsed().as_nanos() as f64 / reps as f64;

    // 5) [i16;16].fill(0)。
    let mut blk = [7i16; 16];
    let t = Instant::now();
    for _ in 0..reps {
        std::hint::black_box(&mut blk);
        blk.fill(0);
        acc = acc.wrapping_add(u64::from(blk[0] as u16));
    }
    std::hint::black_box(&blk);
    let c_fill = t.elapsed().as_nanos() as f64 / reps as f64;

    std::hint::black_box(acc);
    I4OpCosts {
        c_above,
        c_predict,
        c_add_dc,
        c_add_ac,
        c_fill,
    }
}

/// 探针自身开销校准（`stage_profile` feature 门控）：i4 段外层粗钟是 per-MB `Instant::now()`、
/// 段内是 per-block Relaxed 原子计数 —— 二者本身都落在粗钟总账内，须从 shell 中扣除以得
/// 「真循环壳」。返回 ns：`(单次同线程 Relaxed fetch_add, 单次 Instant::now())`。
#[cfg(feature = "stage_profile")]
pub fn probe_clock_costs(reps: u32) -> (f64, f64) {
    use std::time::Instant;
    use std::sync::atomic::AtomicU64;
    let cnt = AtomicU64::new(0);
    let t = Instant::now();
    for _ in 0..reps {
        cnt.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    let c_atomic = t.elapsed().as_nanos() as f64 / reps as f64;
    std::hint::black_box(cnt.load(std::sync::atomic::Ordering::Relaxed));
    let t = Instant::now();
    for _ in 0..reps {
        std::hint::black_box(Instant::now());
    }
    let c_instant = t.elapsed().as_nanos() as f64 / reps as f64;
    (c_atomic, c_instant)
}

/// 未初始化 `Vec<u8>`：调用方必须保证返回后 `len` 字节在被读取前全部写入（见各调用点
/// Safety 注释）。省去 `vec![0u8; len]` 的全量 memset——大缓冲（RGBA 输出/紧密平面）零化
/// 在主线程关键路径上（P-B 直出已覆盖全部字节），逐字节恒等。
#[inline]
unsafe fn uninit_vec_u8(len: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(len);
    v.set_len(len);
    v
}

// ---------------------------------------------------------------------------
// 新 MT 管道（t1=串行熵 / t2=预测+IDCT+重建+滤波+YUV）的宏块行包与共享阶段函数。
// 串行路径（`filter_mt_enabled`==false）与 MT 两路径共用 `entropy_macroblock` /
// `reconstruct_macroblock` 同一函数体 → 逐字节一致是结构性成立（硬闸
// `threads2_matches_serial_byte_exact` / `static_lossy_threads2_matches_serial_byte_exact`）。
// ---------------------------------------------------------------------------

/// 单宏块熵解码产物（t1 → t2 的唯一交接对象）。含去量化后的 i16 系数（DC 捷径/双指针核在
/// `reconstruct_macroblock` 按串行同款复用，字节不变）。`Default` 全零 = 未写系数位恒 0。
struct MbPacket {
    y_mode: u8,
    uv_mode: u8,
    /// y_mode==4（16x16）时为真；调用方沿用本仓 `i16` 命名习惯（值命名空间可遮蔽原语类型名）。
    i16: bool,
    nonzero_mask: u32,
    ac_mask: u32,
    b_modes: [u8; 16],
    mb: [[i16; 16]; 25],
}

impl Default for MbPacket {
    fn default() -> Self {
        MbPacket {
            y_mode: 0,
            uv_mode: 0,
            i16: false,
            nonzero_mask: 0,
            ac_mask: 0,
            b_modes: [0; 16],
            mb: [[0i16; 16]; 25],
        }
    }
}

/// 单一宏块行的完成包：`mbs[mbx]`（t2 重建输入）+ `filters[mbx]`（t2 滤波输入，原行滤波参数表
/// `(level, inner)`）。随通道按序交给 t2，t2 依序重建行 r 后滤行 r-1（串行同序）。整包按值
/// 移动（零大块拷贝）；t1 只持有通道发送端。
struct RowPacket {
    filters: Vec<(u8, u8)>,
    mbs: Vec<MbPacket>,
}

/// 单宏块熵解码（模式 + 残差 token + 去量化），产出 [`MbPacket`] 与行滤波参数 `(level, inner)`。
/// 逐条对应原 `decode_from_header::reconstruct_row` 的「阶段一」，只是把 `mb`/`b_modes` 收进
/// 包（每宏块零初始化，与串行 mb 复用数组在「写入前全零」语义上等价——未写位恒 0）。
/// 熵态（`header`/`td` bool 解码器与 `above_nz`/`left_nz`/`above_b`/`left_b`）全程留在 t1。
#[allow(clippy::too_many_arguments)]
fn entropy_macroblock(
    header: &mut Vp8BoolDecoder<'_>,
    td: &mut Vp8BoolDecoder<'_>,
    segment_probs: Option<&[u8; 3]>,
    skip_probability: i32,
    probs: &[u8],
    seg_params: &[SegmentParameters; 4],
    above_b: &mut [u8; 4],
    left_b: &mut [u8; 4],
    above_nz: &mut u32,
    left_nz: &mut u32,
    pkt: &mut MbPacket,
) -> (u8, u8) {
    // 阶段①（熵计时）：模式/段/skip 解码。
    let modes = stage_clock::entropy(|| {
        stage_clock::entropy_modes(|| {
            read_macroblock_modes(header, segment_probs, skip_probability, above_b, left_b, &mut pkt.b_modes)
        })
    });
    let segment = (modes & 3) as usize;
    let skip = (modes & 4) != 0;
    let y_mode = ((modes >> 3) & 7) as u8;
    let uv_mode = (modes >> 6) as u8;
    let is_i16 = y_mode != 4;
    let sp = seg_params[segment];
    // 惰性零化（A：MbPacket 帧内池 + 仅 touched-宏块零化；不在任何熵计时闭合内，避免把「内容
    // 无关写」算进熵账，也避免探针 Instant/原子与 memset 相互干扰）。
    // - 非跳过：`parse_residuals` 的 `tokens` 只写非零系数位、未写位必须为 0（`MbPacket::default`
    //   等价语义），因此整包零化——复用槽位即使残留脏块也被清掉，逐字节不变由结构保证。
    // - 跳过：重建侧只把 `mb[0..16][0]` 当 DC 门控读（i16 的 WHT/UV 段与 i4 均不读其余位），
    //   仅零化 16 个 DC 槽（32B），免去整包 830B 内容无关写。
    let masks = if skip {
        for b in pkt.mb.iter_mut().take(16) {
            b[0] = 0;
        }
        let keep = if is_i16 { 0 } else { NZ_Y2 };
        *above_nz &= keep;
        *left_nz &= keep;
        0u64
    } else {
        pkt.mb = [[0i16; 16]; 25];
        // 阶段②（熵计时）：残差 token + 去量化。
        stage_clock::entropy(|| {
            stage_clock::entropy_tokens(|| {
                parse_residuals(td, probs, &mut pkt.mb, &sp, is_i16, above_nz, left_nz)
            })
        })
    };
    pkt.y_mode = y_mode;
    pkt.uv_mode = uv_mode;
    pkt.i16 = is_i16;
    pkt.nonzero_mask = masks as u32;
    pkt.ac_mask = (masks >> 32) as u32;
    let level = if is_i16 { sp.filter_level_i16 } else { sp.filter_level_i4 };
    let inner = if pkt.nonzero_mask == 0 && is_i16 { 0u8 } else { 1u8 };
    (level, inner)
}

/// 单宏块帧内预测 + 逆变换 + 残差写回（原 `decode_from_header`「阶段二/三」，逐字节不变）。
/// t2 依序重建时 `prepare_above` 读到的上一行是**已重建未滤波**像素，与串行（整帧重建毕再滤波）
/// 语义一致 → 字节恒等不依赖行顺序，只依赖「预测读未滤、滤波读已建」。`pkt.mb` 可变：
/// WHT 把 DC 写入 luma 块、用后清零，与串行共享 `mb` 数组的写序完全同构。
#[allow(clippy::too_many_arguments)]
fn reconstruct_macroblock(
    y: &mut [u8],
    y_pitch: usize,
    u: &mut [u8],
    v: &mut [u8],
    uv_pitch: usize,
    above_y: &mut [u8; 21],
    above_u: &mut [u8; 9],
    above_v: &mut [u8; 9],
    mbx: usize,
    mby: usize,
    mbw: usize,
    pkt: &mut MbPacket,
) {
    let nonzero_mask = pkt.nonzero_mask;
    let ac_mask = pkt.ac_mask;
    let is_i16 = pkt.i16;
    let y_mode = pkt.y_mode;
    let uv_mode = pkt.uv_mode;
    let dy = mby * 16;
    let dx = mbx * 16;
    let dyu = mby * 8;
    let dxu = mbx * 8;

    stage_clock::predict(|| {
        prepare_above(
            above_y, above_u, above_v,
            y, u, v, y_pitch, uv_pitch, mbx, mby, mbw,
        );
        if is_i16 {
            super::simd::predict_large(y, y_pitch, dy, dx, 16, y_mode, mby > 0, mbx > 0, &above_y[..17]);
        }
        super::simd::predict_large(u, uv_pitch, dyu, dxu, 8, uv_mode, mby > 0, mbx > 0, &above_u[..9]);
        super::simd::predict_large(v, uv_pitch, dyu, dxu, 8, uv_mode, mby > 0, mbx > 0, &above_v[..9]);
    });

    stage_clock::idct(|| {
        let mb = &mut pkt.mb;
        let b_modes = &pkt.b_modes;
        stage_clock::mb_mode(is_i16);
        if is_i16 && (nonzero_mask & NZ_Y2) != 0 {
            stage_clock::idct_wht(|| {
                let y2 = mb[24];
                inverse_walsh_to_blocks(&y2, mb, (ac_mask & NZ_Y2) != 0);
                mb[24].fill(0);
            });
            stage_clock::mb_wht();
        }

        if is_i16 {
            stage_clock::idct_i16(|| {
                for by in 0..4 {
                    let row0 = dy + by * 4;
                    for pair_base in [0usize, 2] {
                        let a = by * 4 + pair_base;
                        let b = a + 1;
                        if ((nonzero_mask >> a) & 1) != 0
                            || mb[a][0] != 0
                            || ((nonzero_mask >> b) & 1) != 0
                            || mb[b][0] != 0
                        {
                            if (((ac_mask >> a) | (ac_mask >> b)) & 1) != 0 {
                                super::simd::inverse_dct_add_two(
                                    &mb[a], &mb[b], y, y_pitch, row0, dx + pair_base * 4,
                                );
                            } else {
                                super::simd::dc_add_two(
                                    mb[a][0], mb[b][0], y, y_pitch, row0, dx + pair_base * 4,
                                );
                            }
                            mb[a].fill(0);
                            mb[b].fill(0);
                        }
                    }
                }
            });
        } else {
            // I4：把 predict/add 的 SIMD 特性判定提升出子块循环——`part_simd_on` 含 CPU 特性
            // 探测与多个 OnceLock，逐块调用在汇编上是真函数 call（热路径每块付一次）；判定结果
            // 在整帧解码内恒定（env/特性探测只跑一次），本宏块开头取一次。对比旧路径二处省写：
            // ① add 分支去 `ac_mask` 判断并统一走单块核——仅 DC 块原走标量 `inverse_dct_add
            //   (has_ac=false)`（15.95ns/块，反比 `inverse_dct_add_one` 的 SSE2 单块核 10.56ns/块
            //   慢），且 `idct_add_one_sse2` 对仅 DC 块与标量捷径逐字节一致
            //   （`inverse_dct_has_ac_shortcut_equivalence`）；② 去 `let coeff = mb[block]` 全量
            //   16 系数拷贝（原实现 32B 栈拷 + 回读每块都付，零块也付），核直接借用 `&mb[block]`。
            let pred_simd = super::simd::part_simd_on("pred", "ssse3");
            let dct_simd = super::simd::part_simd_on("dct", "sse2");
            stage_clock::idct_i4(|| {
                // 逐 4×4 子块重建。子块间串行依赖：块 (bx,by) 预测读左/上已重建像素、add 随后
                // 覆写，predict→add 逐块次序不可重排（S7 同版约束）。外层按子块行 / 内层按列扫，
                // `row0`/`col0` 用循环增量直算（删 block→bx/by 反解 &3 / >>2），字节输出与旧
                // 路径逐块次序完全一致。
                let mut block = 0usize;
                for by in 0..4usize {
                    let row0 = dy + by * 4;
                    for bx in 0..4usize {
                        let col0 = dx + bx * 4;
                        stage_clock::i4_predict(|| {
                            let above = subblock_above(y, y_pitch, row0, col0, bx, by, above_y);
                            if pred_simd {
                                // Safety：`pred_simd` 已在循环外判定 `part_simd_on("pred","ssse3")`
                                // 为真且本块内恒定，SSSE3 核指令可用。
                                unsafe {
                                    super::simd::predict_subblock_forced(y, y_pitch, row0, col0, b_modes[block], &above)
                                }
                            } else {
                                super::simd::predict_subblock(y, y_pitch, row0, col0, b_modes[block], &above);
                            }
                        });
                        if ((nonzero_mask >> block) & 1) != 0 || mb[block][0] != 0 {
                            // 块级 AC 分发（镜像 i16/UV 的 `ac_mask` 双块位判定）：`nonzero 位 1 且
                            // ac 位 0` = 纯 DC 块（`tokens` 返回 last==1，非 DC 系数恒 0）——走单块
                            // DC 捷径 `dc_add_one_forced`（(dc+4)>>3 广播，~20 uops，替代全 4×4
                            // IDCT ~118 uops，字节恒等）；ac 位 1 = 有 AC，仍走全 IDCT 核。
                            let has_ac = ((ac_mask >> block) & 1) != 0;
                            stage_clock::i4_idct_add(|| {
                                // 计数器语义：`i4_add_ac` 只计 AC 块；DC-only 块不计（探针折算
                                // `add_dc_n = i4_idct_add_blocks − i4_add_ac_blocks` 即纯 DC 块数）。
                                if has_ac {
                                    stage_clock::i4_add_ac();
                                    if dct_simd {
                                        // Safety：`dct_simd` 已在循环外判定 `part_simd_on("dct","sse2")`
                                        // 为真且本块内恒定，SSE2 核指令可用。
                                        unsafe {
                                            super::simd::idct_add_one_forced(&mb[block], y, y_pitch, row0, col0)
                                        }
                                    } else {
                                        super::simd::inverse_dct_add_one(&mb[block], y, y_pitch, row0, col0);
                                    }
                                } else if dct_simd {
                                    // Safety：同 `has_ac` 分支——SSE2 核指令可用；纯 DC 分发由
                                    // `has_ac==false` 保证（非 DC 系数全 0），捷径输出与全 IDCT 恒等。
                                    unsafe {
                                        super::simd::dc_add_one_forced(mb[block][0], y, y_pitch, row0, col0)
                                    }
                                } else {
                                    // 标量回退：`inverse_dct_add_one` 恒 has_ac=true，对纯 DC 块与
                                    // DC 捷径逐字节一致（既有 `inverse_dct_has_ac_shortcut_equivalence`）。
                                    super::simd::inverse_dct_add_one(&mb[block], y, y_pitch, row0, col0);
                                }
                                mb[block].fill(0);
                            });
                        }
                        block += 1;
                    }
                }
            });
        }

        stage_clock::idct_uv(|| {
            if (nonzero_mask & 0xff0000) != 0 {
                for a in [16usize, 18, 20, 22] {
                    let b = a + 1;
                    let row0 = dyu + ((a >> 1) & 1) * 4;
                    let nz_a = ((nonzero_mask >> a) & 1) != 0 || mb[a][0] != 0;
                    let nz_b = ((nonzero_mask >> b) & 1) != 0 || mb[b][0] != 0;
                    if !(nz_a || nz_b) {
                        continue;
                    }
                    let has_ac = ((ac_mask >> a) | (ac_mask >> b)) & 1 != 0;
                    if a < 20 {
                        if has_ac {
                            super::simd::inverse_dct_add_two(&mb[a], &mb[b], u, uv_pitch, row0, dxu);
                        } else {
                            super::simd::dc_add_two(mb[a][0], mb[b][0], u, uv_pitch, row0, dxu);
                        }
                    } else if has_ac {
                        super::simd::inverse_dct_add_two(&mb[a], &mb[b], v, uv_pitch, row0, dxu);
                    } else {
                        super::simd::dc_add_two(mb[a][0], mb[b][0], v, uv_pitch, row0, dxu);
                    }
                    mb[a].fill(0);
                    mb[b].fill(0);
                }
            }
        });
    });
}

fn decode_from_header(fh: FrameHeader<'_>, need_tight: bool, channels: usize, alpha: Option<&[u8]>) -> Result<Vp8Yuv> {
    debug_assert!(channels == 3 || channels == 4);
    let width = fh.width as usize;
    let height = fh.height as usize;
    let mbw = fh.mb_width;
    let mbh = fh.mb_height;
    let cw = mbw * 16;
    let ch = mbh * 16;
    let uw = mbw * 8;
    let uh = mbh * 8;
    let y_pitch = cw + 1;
    let uv_pitch = uw + 1;
    let seg_params = compute_segment_parameters(&fh.segments, &fh.quantizers, &fh.filter);

    // L3 分配收敛：y/u/v 三平面合一缓冲（按首尾分段），堆分配 3 → 1。解码全程不 realloc，
    // 依赖其长度固定即指针稳定（与下方裸指针同前提）。字节恒等：合并仅改变分配粒度，
    // 各段内容、写入次序与既有三 Vec 完全一致（全部字节仍在 129 左列初始化 + 重建覆盖内）。
    let y_vec_len = ch * y_pitch;
    let uv_vec_len = uh * uv_pitch;
    let (mut planes, mut rgba, mut out_y, mut out_u, mut out_v) = stage_clock::setup(|| {
        let mut planes = vec![0u8; y_vec_len + 2 * uv_vec_len];
        {
            let (y, uv_rest) = planes.split_at_mut(y_vec_len);
            let (u, v) = uv_rest.split_at_mut(uv_vec_len);
            for r in 0..ch {
                y[r * y_pitch] = 129;
            }
            for r in 0..uh {
                u[r * uv_pitch] = 129;
                v[r * uv_pitch] = 129;
            }
        }
        // 行定稿直出缓冲（整帧；worker/串行路径按 MB 行滚动填写）。`channels`=3（RGB3 opaque
        // 有损直出）/4（RGBA）。
        // L3：未初始化分配——两条直出路径（pipelined `filter_t3_loop` / 串行
        // `filter_and_emit_serial`）都逐行写满 `[0,height)×width×channels`，返回前无任何字节读，
        // 错误路径直接 Err（不构造 Vp8Yuv），故省略全量 memset 逐字节不变（仅省零化墙钟）。
        let rgba = unsafe { uninit_vec_u8(width * height * channels) };
        // 紧密平面输出缓冲（真实宽高；仅供测试/兼容 API 消费，生产 rgba 路径弃用 = 纯浪费，
        // 由 `need_tight` 门控跳过以省 1-2 遍整帧写）。L3 未初始化：MT 由 t3 直出、串行由尾段
        // `extract_tight_rows` 写满，返回前不被读，省略 memset 逐字节不变。
        let cwu = (width + 1) / 2;
        let chu = (height + 1) / 2;
        let (out_y, out_u, out_v) = if need_tight {
            (
                unsafe { uninit_vec_u8(width * height) },
                unsafe { uninit_vec_u8(cwu * chu) },
                unsafe { uninit_vec_u8(cwu * chu) },
            )
        } else {
            (Vec::new(), Vec::new(), Vec::new())
        };
        (planes, rgba, out_y, out_u, out_v)
    });
    let (mut y, uv_rest) = planes.split_at_mut(y_vec_len);
    let (mut u, mut v) = uv_rest.split_at_mut(uv_vec_len);

    let mut header = fh.header;
    let (
        partition_count,
        mut token_decoders,
        probs,
        skip_probability,
        segment_probs,
        mut above_b,
        mut above_nz,
        mut above_y,
        mut above_u,
        mut above_v,
        mut row_filters,
        pipelined,
    ) = stage_clock::mid(|| {
        let partition_count = fh.partitions.len();
        let token_decoders: Vec<Vp8BoolDecoder<'_>> =
            fh.partitions.iter().map(|p| Vp8BoolDecoder::new(p)).collect();
        let probs = fh.coefficient_probabilities;
        let skip_probability = if fh.skip_flag_present {
            i32::from(fh.skip_probability)
        } else {
            -1
        };
        let segment_probs: Option<&[u8; 3]> = if fh.segments.enabled && fh.segments.update_map {
            Some(&fh.segments.map_probabilities)
        } else {
            None
        };
        let mut above_b = vec![0u8; mbw * 4];
        let mut above_nz = vec![0u32; mbw];
        let above_y = [127u8; 21];
        let above_u = [127u8; 9];
        let above_v = [127u8; 9];
        // 每行宏块的 (滤波等级, 是否滤内部边)，仅串行路径消费（MT 行包自带滤波参数）。
        let row_filters = vec![(0u8, 0u8); mbw * mbh];
        let pipelined = filter_mt_enabled(mbw, mbh);
        (
            partition_count,
            token_decoders,
            probs,
            skip_probability,
            segment_probs,
            above_b,
            above_nz,
            above_y,
            above_u,
            above_v,
            row_filters,
            pipelined,
        )
    });

    if pipelined {
        // 新 3 线程管道：t1(主) = 串行熵/去量化 → 依序产**行包**；t2 = 预测+IDCT+重建（planes_A，
        // 恒未滤波）→ 依序把重建定稿行打包 `RowPixels` 交 t3；t3 = 在自有 planes_B 上滤波 +
        // 行定稿直出 RGBA + 抽紧密平面。通道均 `sync_channel(2)` 按值移动，容量 2 = 上一级最多
        // 领先 2 行，自然背压限内存。y/u/v(planes_A)/planes_B/rgba/out_* 各线程独占，零 unsafe。
        // 字节恒等论证见 `recon_loop` / `filter_t3_loop`：planes_A 恒未滤波、planes_B 同源拷贝、
        // 滤波/直出/抽取行序与串行完全一致 → 逐字节同（硬闸 threads2/static_lossy 覆盖）。
        // 熵态（bool 解码器 / above_nz / above_b / left_*）全留 thread1。
    std::thread::scope(|s| {
        // 通道在 scope 闭包内创建：闭包返回时 `tx`/`tx2` 随其 drop（先于 scope 回收子线程），
        // t2/t3 的 `rx.iter()` 随即收到断开（Err）→ 收尾退出，无死锁。
        stage_clock::scope_wall(|| {
        let (tx, rx) = std::sync::mpsc::sync_channel::<RowPacket>(2);
        let (tx2, rx2) = std::sync::mpsc::sync_channel::<RowPixels>(2);
            // A：t1→t2 行包回收通道（t2 重建完把 `mbs`/`filters` Vec 空包送回 t1 复用——
            // 消灭每行 `with_capacity(mbw)` ×2 的分配/首触缺页，≥2304 MB 大图帧内 ~540 次 alloc
            // 摊销到 ~4 个槽位）。回收通道一律 `try_send`/`try_recv`：满/空时双方不阻塞、
            // 各自退回新建兜底，杜绝跨通道死锁。
            let (tx_ar, rx_ar) = std::sync::mpsc::sync_channel::<RowPacket>(2);
            // B：t2→t3 行像素带回收（t3 滤波完把 strip Vec 送回 t2 复用）。
            let (tx_br, rx_br) = std::sync::mpsc::sync_channel::<RowPixels>(2);
            let yb = &mut *y;
            let ub = &mut *u;
            let vb = &mut *v;
            let rb = &mut *rgba;
            let oy = &mut *out_y;
            let ou = &mut *out_u;
            let ov = &mut *out_v;
            s.spawn(move || {
                recon_loop(rx, tx2, rx_br, tx_ar, yb, y_pitch, ub, vb, uv_pitch, mbw)
            });
            s.spawn(move || {
                filter_t3_loop(rx2, tx_br, y_pitch, uv_pitch, rb, oy, ou, ov, mbw, mbh, width, height, &fh.filter, need_tight, channels, alpha)
            });
            for mby in 0..mbh {
                // 每宏块行重置 4 个左邻 b 模式（C# `leftB.Clear()` 同款）。
                let mut left_b = [0u8; 4];
                let mut left_nz = 0u32;
                // 行内复用该分区的 token 解码器（&mut 边界，熵态在宏块级函数内部寄存器驻留）。
                let td = &mut token_decoders[mby & (partition_count - 1)];
                // 行包帧内池：优先复用 t2 送回的空包（`mbs`/`filters` Vec 容量保活、页已暖），
                // 池空（首几行或回收慢）才新建兜底。`resize(mbw, default)`：回收包 len 恒为
                // mbw → no-op；新建包一次性全量零化打底（后续槽位由 `entropy_macroblock` 惰性零化接管）。
                let mut row = match rx_ar.try_recv() {
                    Ok(rp) => rp,
                    Err(_) => RowPacket {
                        filters: Vec::with_capacity(mbw),
                        mbs: Vec::with_capacity(mbw),
                    },
                };
                row.filters.clear();
                // 不用 `resize`（那要求 MbPacket: Clone，使综述里 `#[derive(Clone)]` 变成"被依赖"伪死代码）：
                // 回收包 len 恒为 mbw → no-op；新建包按 `Default` 逐槽构造（仅 Default，无 Clone 需求）。
                if row.mbs.len() < mbw {
                    row.mbs.extend((0..mbw - row.mbs.len()).map(|_| MbPacket::default()));
                }
                for mbx in 0..mbw {
                    let above_b_slot: &mut [u8; 4] =
                        (&mut above_b[mbx * 4..mbx * 4 + 4]).try_into().unwrap();
                    // 就地填充复用槽位：`entropy_macroblock` 内按 skip 惰性零化（`mb`）并覆写全部
                    // 头字段（y/uv/i16/masks/b_modes），未写位恒 0 语义由该函数建立 → 免去每宏块
                    // `MbPacket::default()` 整包 830B 栈零化 + 拷贝入 Vec。
                    let (level, inner) = entropy_macroblock(
                        &mut header,
                        td,
                        segment_probs,
                        skip_probability,
                        &probs,
                        &seg_params,
                        above_b_slot,
                        &mut left_b,
                        &mut above_nz[mbx],
                        &mut left_nz,
                        &mut row.mbs[mbx],
                    );
                    row.filters.push((level, inner));
                }
                if tx.send(row).is_err() {
                    // worker 已退出（异常收尾）：中断产出，scope join 兜底。
                    break;
                }
            }
        });
        });
        // scope join 完成：t3 已滤波+直出+抽紧密平面全部行。

    } else {
        // 串行：逐宏块「熵 → 预测+IDCT+重建」（与 MT 共用 `entropy_macroblock` /
        // `reconstruct_macroblock`，字节恒等结构性成立），整帧重建毕再滤波直出。
        // 串行逐宏块「熵 → 预测+IDCT+重建」：单 `MbPacket` 槽位整帧复用——`entropy_macroblock`
        // 内按 skip 惰性零化建立「未写位恒 0」语义，重建用后按块 `fill(0)`，逐字节与旧
        // 「每宏块 `MbPacket::default()`」等价（硬闸 threads2/static_lossy 覆盖），且省去整帧
        // ~27MB 的 830B 内容无关零化（跳过宏块仅零化 16 个 DC 槽 32B）。
        let mut pkt = MbPacket::default();
        for mby in 0..mbh {
            // 每宏块行重置 4 个左邻 b 模式（C# `leftB.Clear()` 同款）。
            let mut left_b = [0u8; 4];
            let mut left_nz = 0u32;
            // 行内复用该分区的 token 解码器（&mut 边界，熵态在宏块级函数内部寄存器驻留）。
            let td = &mut token_decoders[mby & (partition_count - 1)];
            for mbx in 0..mbw {
                let above_b_slot: &mut [u8; 4] =
                    (&mut above_b[mbx * 4..mbx * 4 + 4]).try_into().unwrap();
                let (level, inner) = entropy_macroblock(
                    &mut header,
                    td,
                    segment_probs,
                    skip_probability,
                    &probs,
                    &seg_params,
                    above_b_slot,
                    &mut left_b,
                    &mut above_nz[mbx],
                    &mut left_nz,
                    &mut pkt,
                );
                row_filters[mby * mbw + mbx] = (level, inner);
                reconstruct_macroblock(
                    &mut y, y_pitch, &mut u, &mut v, uv_pitch,
                    &mut above_y, &mut above_u, &mut above_v,
                    mbx, mby, mbw, &mut pkt,
                );
            }
        }

        // 串行：整帧重建后按宏块行「环路滤波 → 行定稿直出」（等价 C# 逐行滤波 +
        // `RgbaSink.Emit`：预测读未滤波行、滤波读已重建行，顺序一致；每滤完一行立即输出
        // 已定稿行 `[emitted, mbY*16+8)`，无需等整帧、无第二趟）。
        let mut scratch = EmitScratch::new(width);
        filter_and_emit_serial(
            &mut y, y_pitch, &mut u, &mut v, uv_pitch, mbw, mbh, &fh.filter, &row_filters,
            width, height, &mut rgba, &mut scratch, channels, alpha,
        );

        // 抽紧密平面（仅测试/兼容 API 路径 need_tight；生产 rgba 直出路径跳过 = 省整帧写）。
        // L3：out_* 已未初始化分配，下方逐行写满（读已整帧滤波的 y/u/v padded 平面），
        // 返回前不被读，省略 memset 逐字节不变。
        if need_tight {
            stage_clock::copy(|| {
                extract_tight_rows(
                    &y, y_pitch, &u, &v, uv_pitch,
                    0, height, width, &mut out_y, &mut out_u, &mut out_v,
                );
            });
        }
    }

    // scope join 后、返回前尾段（Vp8Yuv 构建）：主线程墙钟全部归账，无隐藏盲区（K53）。
    // `rgba` 缓冲按 `channels` 已写满（3=RGB3 / 4=RGBA4）；路由到对应字段，另一个恒空。
    stage_clock::end(|| {
        let (rgba_field, rgb_field) = if channels == 3 {
            (Vec::new(), rgba)
        } else {
            (rgba, Vec::new())
        };
        Ok(Vp8Yuv {
            width: fh.width,
            height: fh.height,
            y: out_y,
            u: out_u,
            v: out_v,
            rgba: rgba_field,
            rgb: rgb_field,
        })
    })
}

// ---------------------------------------------------------------------------
// V3：环路滤波（RFC 6386 §15，标量；C# `Vp8LoopFilter.cs`/标量 FilterEdge 同款算术）

/// 有符号 8bit 饱和：`p+delta` 到 [-128,127]。
#[inline]
fn clamp_signed(v: i32) -> i32 {
    v.clamp(-128, 127)
}

/// RFC 6386 §15.2：偏置后的调整钳到 int8 后右移 3（先钳再移，防止 124/125 移位到 16）。
#[inline]
fn filter_tap(adjustment: i32, bias: i32) -> i32 {
    clamp_signed(adjustment + bias) >> 3
}

/// 简单滤波（只动 p0/q0）。`center` 为存储下标，`sample_step`=相邻 p/q 步长（纵 1 / 横 stride），
/// `pixel_advance`=相邻样本步长（纵 stride / 横 1）。
fn filter_simple_scalar(
    plane: &mut [u8],
    center: usize,
    sample_step: usize,
    pixel_advance: usize,
    length: usize,
    edge: i32,
) {
    for i in 0..length {
        let sample = center + i * pixel_advance;
        let p1 = i32::from(plane[sample - 2 * sample_step]);
        let p0 = i32::from(plane[sample - sample_step]);
        let q0 = i32::from(plane[sample]);
        let q1 = i32::from(plane[sample + sample_step]);
        if (p0 - q0).abs() * 2 + (p1 - q1).abs() / 2 > edge {
            continue;
        }
        let adjustment = clamp_signed(clamp_signed(p1 - q1) + 3 * (q0 - p0));
        let q_adj = filter_tap(adjustment, 4);
        let p_adj = filter_tap(adjustment, 3);
        plane[sample - sample_step] = (p0 + p_adj).clamp(0, 255) as u8;
        plane[sample] = (q0 - q_adj).clamp(0, 255) as u8;
    }
}

/// 宏块边界 Normal 滤波（hv 高方差只动 p0/q0，否则 27/18/9 加权铺到 p2..q2）。
fn filter_normal_macroblock_scalar(
    plane: &mut [u8],
    center: usize,
    sample_step: usize,
    pixel_advance: usize,
    length: usize,
    interior: i32,
    edge: i32,
    hev: i32,
) {
    for i in 0..length {
        let sample = center + i * pixel_advance;
        let p3 = i32::from(plane[sample - 4 * sample_step]);
        let p2 = i32::from(plane[sample - 3 * sample_step]);
        let p1 = i32::from(plane[sample - 2 * sample_step]);
        let p0 = i32::from(plane[sample - sample_step]);
        let q0 = i32::from(plane[sample]);
        let q1 = i32::from(plane[sample + sample_step]);
        let q2 = i32::from(plane[sample + 2 * sample_step]);
        let q3 = i32::from(plane[sample + 3 * sample_step]);
        if (p0 - q0).abs() * 2 + (p1 - q1).abs() / 2 > edge
            || (p3 - p2).abs() > interior
            || (p2 - p1).abs() > interior
            || (p1 - p0).abs() > interior
            || (q1 - q0).abs() > interior
            || (q2 - q1).abs() > interior
            || (q3 - q2).abs() > interior
        {
            continue;
        }
        let high_variance = (p1 - p0).abs() > hev || (q1 - q0).abs() > hev;
        let adjustment = clamp_signed(clamp_signed(p1 - q1) + 3 * (q0 - p0));
        if high_variance {
            let q_adj = filter_tap(adjustment, 4);
            let p_adj = filter_tap(adjustment, 3);
            plane[sample - sample_step] = (p0 + p_adj).clamp(0, 255) as u8;
            plane[sample] = (q0 - q_adj).clamp(0, 255) as u8;
            continue;
        }
        let outer = (27 * adjustment + 63) >> 7;
        let middle = (18 * adjustment + 63) >> 7;
        let inn = (9 * adjustment + 63) >> 7;
        plane[sample - sample_step] = (p0 + outer).clamp(0, 255) as u8;
        plane[sample] = (q0 - outer).clamp(0, 255) as u8;
        plane[sample - 2 * sample_step] = (p1 + middle).clamp(0, 255) as u8;
        plane[sample + sample_step] = (q1 - middle).clamp(0, 255) as u8;
        plane[sample - 3 * sample_step] = (p2 + inn).clamp(0, 255) as u8;
        plane[sample + 2 * sample_step] = (q2 - inn).clamp(0, 255) as u8;
    }
}

/// 内部边 Normal 滤波（hv 时 adjustment 含 p1-q1，否则只有 3(q0-p0)）。
fn filter_normal_inner_scalar(
    plane: &mut [u8],
    center: usize,
    sample_step: usize,
    pixel_advance: usize,
    length: usize,
    interior: i32,
    edge: i32,
    hev: i32,
) {
    for i in 0..length {
        let sample = center + i * pixel_advance;
        let p3 = i32::from(plane[sample - 4 * sample_step]);
        let p2 = i32::from(plane[sample - 3 * sample_step]);
        let p1 = i32::from(plane[sample - 2 * sample_step]);
        let p0 = i32::from(plane[sample - sample_step]);
        let q0 = i32::from(plane[sample]);
        let q1 = i32::from(plane[sample + sample_step]);
        let q2 = i32::from(plane[sample + 2 * sample_step]);
        let q3 = i32::from(plane[sample + 3 * sample_step]);
        if (p0 - q0).abs() * 2 + (p1 - q1).abs() / 2 > edge
            || (p3 - p2).abs() > interior
            || (p2 - p1).abs() > interior
            || (p1 - p0).abs() > interior
            || (q1 - q0).abs() > interior
            || (q2 - q1).abs() > interior
            || (q3 - q2).abs() > interior
        {
            continue;
        }
        let high_variance = (p1 - p0).abs() > hev || (q1 - q0).abs() > hev;
        let adjustment = if high_variance {
            clamp_signed(clamp_signed(p1 - q1) + 3 * (q0 - p0))
        } else {
            clamp_signed(3 * (q0 - p0))
        };
        let q_adj = filter_tap(adjustment, 4);
        let p_adj = filter_tap(adjustment, 3);
        plane[sample - sample_step] = (p0 + p_adj).clamp(0, 255) as u8;
        plane[sample] = (q0 - q_adj).clamp(0, 255) as u8;
        if high_variance {
            continue;
        }
        let inn = (q_adj + 1) >> 1;
        plane[sample - 2 * sample_step] = (p1 + inn).clamp(0, 255) as u8;
        plane[sample + sample_step] = (q1 - inn).clamp(0, 255) as u8;
    }
}

/// 单条边滤波（中心为存储下标：`y*stride + 1 + x`）。
/// `pub(crate)`：`webp::simd` 的向量版以此为标量回退/对拍基准（D2）。
#[allow(clippy::too_many_arguments)]
pub(crate) fn filter_edge(
    plane: &mut [u8],
    stride: usize,
    x: usize,
    y: usize,
    length: usize,
    vertical: bool,
    macroblock_edge: bool,
    simple: bool,
    interior: i32,
    edge: i32,
    hev: i32,
) {
    let center = y * stride + 1 + x;
    let sample_step = if vertical { 1 } else { stride };
    let pixel_advance = if vertical { stride } else { 1 };
    if simple {
        filter_simple_scalar(plane, center, sample_step, pixel_advance, length, edge);
    } else if macroblock_edge {
        filter_normal_macroblock_scalar(plane, center, sample_step, pixel_advance, length, interior, edge, hev);
    } else {
        filter_normal_inner_scalar(plane, center, sample_step, pixel_advance, length, interior, edge, hev);
    }
}

/// 每宏块行滤波：按 C# `FilterMacroblockRow` 的顺序（左→内竖→上→内横），逐宏块滤波。
/// `row_filters` 为**本行** `mbw` 个 `(level, inner)`（调用方按行切窗/按行包传递，见
/// `filter_and_emit_serial` 与 `worker_loop`）。本函数是环路滤波的**串行语义源**：串行与
/// 新 MT 两路径只调度它，行内/宏块内/边顺序与算术完全不变，故任一路径的输出与串行逐字节
/// 一致（硬闸：`threads2_matches_serial_byte_exact`）。
#[allow(clippy::too_many_arguments)]
fn filter_macroblock_row(
    y: &mut [u8],
    y_pitch: usize,
    u: &mut [u8],
    v: &mut [u8],
    uv_pitch: usize,
    mbw: usize,
    mby: usize,
    filter: &FilterSettings,
    row_filters: &[(u8, u8)],
) {
    let y_row = mby * 16;
    let uv_row = mby * 8;
    stage_clock::filter(|| {
    for mbx in 0..mbw {
        let (level, inner) = row_filters[mbx];
        if level == 0 {
            continue;
        }
        let level_i = i32::from(level);
        let mut interior = level_i;
        if filter.sharpness != 0 {
            interior >>= if filter.sharpness > 4 { 2 } else { 1 };
            interior = interior.min(9 - i32::from(filter.sharpness));
        }
        interior = interior.max(1);
        let edge_limit = 2 * (level_i + 2) + interior;
        let sub_limit = 2 * level_i + interior;
        let hev = if level >= 40 { 2 } else if level >= 15 { 1 } else { 0 };
        let skip_inner = inner == 0;
        let simple = filter.simple;
        let x = mbx * 16;
        let uvx = mbx * 8;

        // SIMD 熔合路径（D2v3）：一次宏块全 12 核（C# FilterMacroblock 同款），边次序与下方
        // 标量完全一致 → 逐字节相同；`KIMG_WEBP_NO_SIMD=1` / `KIMG_WEBP_NO_SIMD_FLT=1` /
        // `KIMG_WEBP_SIMD_FLT=0` 回退标量（门控见 simd::part_simd_on）。
        #[cfg(target_arch = "x86_64")]
        {
            if super::simd::part_simd_on("flt", "sse2") {
                unsafe {
                    let yc = y.as_mut_ptr().add(y_row * y_pitch + 1 + x);
                    let uc = u.as_mut_ptr().add(uv_row * uv_pitch + 1 + uvx);
                    let vc = v.as_mut_ptr().add(uv_row * uv_pitch + 1 + uvx);
                    super::simd::lf_filter_macroblock(
                        yc,
                        y_pitch as isize,
                        uc,
                        vc,
                        uv_pitch as isize,
                        mbx > 0,
                        mby > 0,
                        !skip_inner,
                        simple,
                        interior,
                        edge_limit,
                        sub_limit,
                        hev,
                    );
                }
                continue;
            }
        }

        if mbx > 0 {
            filter_edge(y, y_pitch, x, y_row, 16, true, true, simple, interior, edge_limit, hev);
            if !simple {
                filter_edge(u, uv_pitch, uvx, uv_row, 8, true, true, false, interior, edge_limit, hev);
                filter_edge(v, uv_pitch, uvx, uv_row, 8, true, true, false, interior, edge_limit, hev);
            }
        }
        if !skip_inner {
            for edge in [4usize, 8, 12] {
                filter_edge(y, y_pitch, x + edge, y_row, 16, true, false, simple, interior, sub_limit, hev);
            }
            if !simple {
                filter_edge(u, uv_pitch, uvx + 4, uv_row, 8, true, false, false, interior, sub_limit, hev);
                filter_edge(v, uv_pitch, uvx + 4, uv_row, 8, true, false, false, interior, sub_limit, hev);
            }
        }
        if mby > 0 {
            filter_edge(y, y_pitch, x, y_row, 16, false, true, simple, interior, edge_limit, hev);
            if !simple {
                filter_edge(u, uv_pitch, uvx, uv_row, 8, false, true, false, interior, edge_limit, hev);
                filter_edge(v, uv_pitch, uvx, uv_row, 8, false, true, false, interior, edge_limit, hev);
            }
        }
        if !skip_inner {
            for edge in [4usize, 8, 12] {
                filter_edge(y, y_pitch, x, y_row + edge, 16, false, false, simple, interior, sub_limit, hev);
            }
            if !simple {
                filter_edge(u, uv_pitch, uvx, uv_row + 4, 8, false, false, false, interior, sub_limit, hev);
                filter_edge(v, uv_pitch, uvx, uv_row + 4, 8, false, false, false, interior, sub_limit, hev);
            }
        }
    }
    });
}

/// 环路滤波双线程门控（auto 宏块总数下限）。`mbw*mbh >= MT_MIN_MB_TOTAL`（≈0.6MP，如 768×768；
/// 2026-11 微调 4096→2304：3600-MB 级（1280×720/800×1144）文件原本恒定串行、落入分账探针
/// 「最差 lw_ratio 档」（1.13-1.20x libwebp），线程开拔开销 ~20-50µs 相对其 ~15-25ms 解码可
/// 忽略，滤波+YUV 重叠收益 > 开销；字节一致由 `threads2_matches_serial_byte_exact` 全覆盖
/// （与阈值无关）。
const MT_MIN_MB_TOTAL: usize = 2304;

/// `KIMG_WEBP_THREADS=auto|0|1|2`（默认 auto）：
/// - `0` / `1` / `off` / `serial`：强制串行（重建毕→整帧滤波；输出恒与并行逐字节一致）。
/// - `2` / `on` / `two`：强制 P-B 真重叠管道（重建∥滤波，见 `decode_from_header` 下游）。
/// - 其余 / 缺省（auto）：宽图（宏块总数≥`MT_MIN_MB_TOTAL`）开管道，窄/小图关（进程开销>收益）。
/// 每次解码读一次 env（pn gsimd `KIMG_PNG_LEVEL` 同款，测试可翻转）。
fn filter_mt_enabled(mbw: usize, mbh: usize) -> bool {
    let mode = std::env::var("KIMG_WEBP_THREADS").unwrap_or_default();
    let mode = mode.trim();
    let force_off = matches!(mode, "0" | "1" | "off" | "serial");
    let force_two = matches!(mode, "2" | "on" | "two");
    if mbh == 0 || force_off {
        return false;
    }
    if force_two {
        return true;
    }
    mbw * mbh >= MT_MIN_MB_TOTAL
}

/// t2 → t3 的交接包：宏观行 r 重建定稿后的整行 padded 像素（与 planes_A 布局逐字节一致：16 行 luma +
/// 8 行 U + 8 行 V，全含左侧 129 列）+ 本行滤波参数表。随 `sync_channel(2)` 按值移动（零大块
/// 二次拷贝；容量 2 = t2 最多领先 t3 两行，自然背压限内存）。t3 借此在**自有** planes_B 上重建
/// 同布局，滤波/直出几何与 planes_A 完全同源 → 字节恒等由构造保证。
struct RowPixels {
    /// 16 × y_pitch 字节（luma padded 行，含左列）。
    luma: Vec<u8>,
    /// 8 × uv_pitch（U） + 8 × uv_pitch（V）。
    chroma: Vec<u8>,
    /// 本行宏块滤波参数 (level, inner)，t3 滤波输入。
    filters: Vec<(u8, u8)>,
}

/// 新 MT 管道 t2：依序消费 t1 行包，**预测+IDCT+重建**行 r 到 planes_A（本线程独占、恒未滤波），
/// 随即把本 MB 行重建定稿像素打包成 [`RowPixels`] 交给 t3。预测读上一行未滤波像素 —— planes_A
/// 从不滤波 → 与串行「predict 用未滤波值」逐字节同。`above_*` 为本线程局部（planes_A 行窗口）。
#[allow(clippy::too_many_arguments)]
fn recon_loop(
    rx: std::sync::mpsc::Receiver<RowPacket>,
    tx2: std::sync::mpsc::SyncSender<RowPixels>,
    rx_br: std::sync::mpsc::Receiver<RowPixels>,
    tx_ar: std::sync::mpsc::SyncSender<RowPacket>,
    y: &mut [u8],
    y_pitch: usize,
    u: &mut [u8],
    v: &mut [u8],
    uv_pitch: usize,
    mbw: usize,
) {
    let mut above_y = [127u8; 21];
    let mut above_u = [127u8; 9];
    let mut above_v = [127u8; 9];
    for (r, mut rowpkt) in rx.iter().enumerate() {
        for (mbx, pkt) in rowpkt.mbs.iter_mut().enumerate() {
            reconstruct_macroblock(
                y, y_pitch, u, v, uv_pitch,
                &mut above_y, &mut above_u, &mut above_v,
                mbx, r, mbw, pkt,
            );
        }
        // 打包本 MB 行（行 r 已在 planes_A 定稿重建）→ t3。B：strip Vec 帧内池——优先复用
        // t3 滤波完送回（经 `rx_br`，`luma`/`chroma` 容量保活、页已暖），池空才新建；
        // 复用前 `clear()` 重写全部字节（未读残留，dirty 语义由「整行覆写」保证）。
        let RowPacket { filters, mbs } = rowpkt;
        let luma_len = 16 * y_pitch;
        let uv_len = 8 * uv_pitch;
        let mut luma = match rx_br.try_recv() {
            Ok(s) => s.luma,
            Err(_) => Vec::with_capacity(luma_len),
        };
        luma.clear();
        for i in 0..16 {
            let o = (r * 16 + i) * y_pitch;
            luma.extend_from_slice(&y[o..o + y_pitch]);
        }
        let mut chroma = match rx_br.try_recv() {
            Ok(s) => s.chroma,
            Err(_) => Vec::with_capacity(2 * uv_len),
        };
        chroma.clear();
        for i in 0..8 {
            let o = (r * 8 + i) * uv_pitch;
            chroma.extend_from_slice(&u[o..o + uv_pitch]);
        }
        for i in 0..8 {
            let o = (r * 8 + i) * uv_pitch;
            chroma.extend_from_slice(&v[o..o + uv_pitch]);
        }
        // `filters` 随像素带进 t3（滤波输入），体积仅 mbw×2B 级，克隆代价 ~0.2KB/行可忽略；
        // 原 `filters`/`mbs` Vec 整体送回 t1 复用（A 池）。
        let pkt = RowPixels { luma, chroma, filters: filters.clone() };
        if tx2.send(pkt).is_err() {
            // t3 已退出（异常收尾）：中断产出，外层 scope join 兜底。
            break;
        }
        let _ = tx_ar.try_send(RowPacket { filters, mbs });
    }
}

/// 从已定稿（滤波后）padded 平面抽紧密平面行段 `[first, end)`（luma 行 first..end、chroma 行
/// first/2 ..= (end-1)/2）。语义与旧整帧尾段拷贝逐字节一致（旧拷贝 = 本函数 0..height 单次调用）。
#[allow(clippy::too_many_arguments)]
fn extract_tight_rows(
    y: &[u8],
    y_pitch: usize,
    u: &[u8],
    v: &[u8],
    uv_pitch: usize,
    first: usize,
    end: usize,
    width: usize,
    out_y: &mut [u8],
    out_u: &mut [u8],
    out_v: &mut [u8],
) {
    let cwu = (width + 1) / 2;
    if end == 0 {
        return;
    }
    for r in first..end {
        out_y[r * width..(r + 1) * width].copy_from_slice(&y[r * y_pitch + 1..r * y_pitch + 1 + width]);
    }
    let cfirst = first / 2;
    let clast = (end - 1) / 2;
    for c in cfirst..=clast {
        let off = c * cwu;
        out_u[off..off + cwu].copy_from_slice(&u[c * uv_pitch + 1..c * uv_pitch + 1 + cwu]);
        out_v[off..off + cwu].copy_from_slice(&v[c * uv_pitch + 1..c * uv_pitch + 1 + cwu]);
    }
}

/// 新 MT 管道 t3：依序消费 t2 的重建行包，在**自有** planes_B（同布局 padded 缓冲，经
/// [`RowPixels`] 逐行填入）上做【环路滤波 → 行定稿直出 RGBA → 抽紧密平面】。本体恒等论证：
/// - plans_B 内容 = planes_A（未滤波重建）逐字节拷贝 → 滤波输入与串行「滤波读已重建行」同。
/// - 滤波/直出/紧密抽取完全复用 `filter_macroblock_row` / `emit_rows_into_rgba` /
///   `extract_tight_rows`，行序 m 升序、段序 `[emitted, m*16+8)` 与串行一致 → 逐字节同。
/// - planes_A 恒未滤波（t2 独占），故 t3 滤波不需要等 t2 的下一步预测 —— 无 1 行滞后约束。
/// 所有权：本线程独占 planes_B / rgba / out_*，零 unsafe，无共享可变内存。
#[allow(clippy::too_many_arguments)]
fn filter_t3_loop(
    rx: std::sync::mpsc::Receiver<RowPixels>,
    tx_br: std::sync::mpsc::SyncSender<RowPixels>,
    y_pitch: usize,
    uv_pitch: usize,
    rgba: &mut [u8],
    out_y: &mut [u8],
    out_u: &mut [u8],
    out_v: &mut [u8],
    mbw: usize,
    mbh: usize,
    width: usize,
    height: usize,
    filter: &FilterSettings,
    need_tight: bool,
    channels: usize,
    alpha: Option<&[u8]>,
) {
    // t3 独占的全 padded 重建缓冲（planes_B）：与 planes_A 同布局（含左列；字节在写入前恒 0，
    // 滤波/直出只读已写入行段，未初始化亦安全——保持 memset 以结构性地杜绝任何读空风险）。
    let y_vec_len = mbh * 16 * y_pitch;
    let uv_vec_len = mbh * 8 * uv_pitch;
    let mut planes_b = vec![0u8; y_vec_len + 2 * uv_vec_len];
    let (yb, rest) = planes_b.split_at_mut(y_vec_len);
    let (ub, vb) = rest.split_at_mut(uv_vec_len);
    let cw = (width + 1) >> 1;
    let ch = (height + 1) >> 1;
    let mut scratch = EmitScratch::new(width);
    let mut emitted = 0usize;
    for (r, pkt) in rx.iter().enumerate() {
        // 本 MB 行重建行像素写入 planes_B（布局与 planes_A 一致 → 滤波/直出几何完全同源）。
        for i in 0..16 {
            let dst = (r * 16 + i) * y_pitch;
            yb[dst..dst + y_pitch].copy_from_slice(&pkt.luma[i * y_pitch..(i + 1) * y_pitch]);
        }
        for i in 0..8 {
            let dst = (r * 8 + i) * uv_pitch;
            ub[dst..dst + uv_pitch].copy_from_slice(&pkt.chroma[i * uv_pitch..(i + 1) * uv_pitch]);
            vb[dst..dst + uv_pitch].copy_from_slice(&pkt.chroma[(8 + i) * uv_pitch..(8 + i + 1) * uv_pitch]);
        }
        // 本行滤波 + 行定稿直出 + 抽紧密平面（串行同序：先滤 m 再 emit m；末段 end=height 同语义）。
        filter_macroblock_row(yb, y_pitch, ub, vb, uv_pitch, mbw, r, filter, &pkt.filters);
        let end = if r == mbh - 1 { height } else { r * 16 + 8 };
        if end > emitted {
            if channels == 3 {
                emit_rows_into_rgb3(yb, y_pitch, ub, vb, uv_pitch, width, cw, ch, emitted, end, rgba, &mut scratch);
            } else {
                emit_rows_into_rgba(yb, y_pitch, ub, vb, uv_pitch, width, cw, ch, emitted, end, rgba, &mut scratch, alpha);
            }
            if need_tight {
                stage_clock::copy(|| {
                    extract_tight_rows(
                        yb, y_pitch, ub, vb, uv_pitch,
                        emitted, end, width, out_y, out_u, out_v,
                    );
                });
            }
            emitted = end;
        }
        // B 回收：把用过的 strip（luma/chroma/filters 全部重写后送回）交还 t2 复用；
        // 通道满则丢弃（t2 回退新建兜底），`try_send` 不阻塞 → 无死锁。
        let _ = tx_br.try_send(pkt);
    }
}

// ---------------------------------------------------------------------------
// V4：YUV→RGBA（fancy 上采样 + 定点转换；C# `WebpDecoder.cs` 有损路径标量同款）
// ---------------------------------------------------------------------------

/// fancy 上采样一条 chroma 行（`current` 当前 chroma 行，`adjacent` 相邻行，各 `cw` 字节）。
/// 输出 `width` 字节。算术 = `3*cur+adj` 的 t 行 + 偶数/奇数对的 `(m*t ± 邻居 + 8)>>4`。
/// `pub(crate)`：SIMD 版（`webp::simd::upsample_fancy_row`）以本标量为参考并逐字节对拍；
/// 向量核与标量一致性见 `simd.rs` 单测。
pub(crate) fn upsample_fancy(current: &[u8], adjacent: &[u8], width: usize) -> Vec<u8> {
    let cw = current.len();
    let mut t = vec![0u16; cw];
    for col in 0..cw {
        t[col] = 3 * u16::from(current[col]) + u16::from(adjacent[col]);
    }
    let mut out = vec![0u8; width];
    let first = t[0];
    out[0] = ((4 * first + 8) >> 4) as u8;
    if width > 1 {
        let next = if cw > 1 { t[1] } else { first };
        out[1] = ((3 * first + next + 8) >> 4) as u8;
    }
    for col in 1..cw.saturating_sub(1) {
        let c = t[col];
        let oc = col * 2;
        out[oc] = ((3 * c + t[col - 1] + 8) >> 4) as u8;
        out[oc + 1] = ((3 * c + t[col + 1] + 8) >> 4) as u8;
    }
    if cw > 1 {
        let last_col = cw - 1;
        let c = t[last_col];
        let oc = last_col * 2;
        out[oc] = ((3 * c + t[last_col - 1] + 8) >> 4) as u8;
        if oc + 1 < width {
            out[oc + 1] = ((4 * c + 8) >> 4) as u8;
        }
    }
    out
}

/// 定点 YUV→RGB（与 C# `WritePixel` 完全同序：`(s*k)>>8` 各项、bias 减后再 `>>6`，无重结合）。
/// `pub(crate)`：`webp::simd::yuv_row_to_rgba` 的标量回退/尾列直接调用本函数（单一算术源）。
#[inline]
pub(crate) fn yuv_to_rgb(y: u8, u: u8, v: u8) -> (u8, u8, u8) {
    let y_term = (i32::from(y) * 19077) >> 8;
    let red = (y_term + ((i32::from(v) * 26149) >> 8) - 14234) >> 6;
    let green = (y_term - ((i32::from(u) * 6419) >> 8) - ((i32::from(v) * 13320) >> 8) + 8708) >> 6;
    let blue = (y_term + ((i32::from(u) * 33050) >> 8) - 17685) >> 6;
    (clamp_byte(red), clamp_byte(green), clamp_byte(blue))
}

/// 行定稿直出 RGBA（复用工作区）。
struct EmitScratch {
    u_up: Vec<u8>,
    v_up: Vec<u8>,
    t_ws: Vec<u16>,
}

impl EmitScratch {
    fn new(width: usize) -> Self {
        let cw = (width + 1) >> 1;
        EmitScratch {
            u_up: vec![0u8; width],
            v_up: vec![0u8; width],
            // U/V 两行 t 工作区共存（`upsample_fancy_into_uv` 各用 cw+8）。
            t_ws: vec![0u16; 2 * (cw + 8)],
        }
    }
}

/// 从**滤波后** padded 平面把已定稿行 `[first, end)` fancy 上采样 + YUV→RGBA 直出到 `rgba`。
/// 算术与 `yuv_to_rgba_fancy`（旧二趟）完全一致，只是读 padded 平面（跳过左侧 129 列）而非
/// tight 平面（tight 只是顺序拷贝，字节相同）。行 `end` 的选取保证 `[first,end)` 各行及其
/// 上采样相邻 chroma 行均已环路滤波定稿（C# `RgbaSink.Emit` 的 `end=mbY*16+8` 语义，注释于
/// `decode_from_header`）。
#[allow(clippy::too_many_arguments)]
fn emit_rows_into_rgba(
    y: &[u8],
    y_pitch: usize,
    u: &[u8],
    v: &[u8],
    uv_pitch: usize,
    width: usize,
    cw: usize,
    ch: usize,
    first: usize,
    end: usize,
    rgba: &mut [u8],
    ws: &mut EmitScratch,
    alpha: Option<&[u8]>,
) {
    stage_clock::yuv(|| {
    for row in first..end {
        let chroma_row = row >> 1;
        let adjacent = if row % 2 == 0 {
            if chroma_row == 0 {
                0
            } else {
                chroma_row - 1
            }
        } else if chroma_row == ch - 1 {
            chroma_row
        } else {
            chroma_row + 1
        };
        let yba = row * y_pitch + 1;
        let u0 = chroma_row * uv_pitch + 1;
        let u1 = adjacent * uv_pitch + 1;
        super::simd::upsample_fancy_into_uv(
            &u[u0..u0 + cw], &u[u1..u1 + cw],
            &v[u0..u0 + cw], &v[u1..u1 + cw],
            &mut ws.u_up, &mut ws.v_up, &mut ws.t_ws,
        );
        // lossy-alpha：alpha 行随本行直出并入发射内层（单遍 RGBA；`None` 时 α 恒 255）。
        super::simd::yuv_row_to_rgba_alpha(
            &y[yba..yba + width], &ws.u_up, &ws.v_up,
            alpha.map(|a| &a[row * width..(row + 1) * width]),
            &mut rgba[row * width * 4..(row + 1) * width * 4],
        );
    }
    });
}

/// 从**滤波后** padded 平面把已定稿行 `[first, end)` fancy 上采样 + YUV→RGB3 直出（3 字节/像素，
/// alpha 不写）。算术与 [`emit_rows_into_rgba`] 完全一致，仅末段走 `yuv_row_to_rgb3`（3 步长）；
/// 每像素前 3 字节恒等于 RGBA 路径 → opaque 有损 RGB3 直出与旧「RGBA4→4→3」同像素（硬闸）。
#[allow(clippy::too_many_arguments)]
fn emit_rows_into_rgb3(
    y: &[u8],
    y_pitch: usize,
    u: &[u8],
    v: &[u8],
    uv_pitch: usize,
    width: usize,
    cw: usize,
    ch: usize,
    first: usize,
    end: usize,
    rgb: &mut [u8],
    ws: &mut EmitScratch,
) {
    stage_clock::yuv(|| {
    for row in first..end {
        let chroma_row = row >> 1;
        let adjacent = if row % 2 == 0 {
            if chroma_row == 0 {
                0
            } else {
                chroma_row - 1
            }
        } else if chroma_row == ch - 1 {
            chroma_row
        } else {
            chroma_row + 1
        };
        let yba = row * y_pitch + 1;
        let u0 = chroma_row * uv_pitch + 1;
        let u1 = adjacent * uv_pitch + 1;
        super::simd::upsample_fancy_into_uv(
            &u[u0..u0 + cw], &u[u1..u1 + cw],
            &v[u0..u0 + cw], &v[u1..u1 + cw],
            &mut ws.u_up, &mut ws.v_up, &mut ws.t_ws,
        );
        super::simd::yuv_row_to_rgb3(&y[yba..yba + width], &ws.u_up, &ws.v_up, &mut rgb[row * width * 3..(row + 1) * width * 3]);
    }
    });
}

/// 串行路径：逐宏块行「环路滤波 → 行定稿直出」（RGBA4 当 `channels=4`；RGB3 当 `channels=3`）。
/// `end = mbY==末行 ? height : mbY*16+8`：行 `[emitted, mbY*16+8)` 只被第 mbY 行的滤波读写定稿
/// （下一行滤波只触及本行 13..15 及 chroma 5..7），故滤完即出，无需等待整帧。上采样相邻
/// chroma 行最远取 `mbY*8+4`，同属本 MB 行、已在本次滤波定稿。
#[allow(clippy::too_many_arguments)]
fn filter_and_emit_serial(
    y: &mut [u8],
    y_pitch: usize,
    u: &mut [u8],
    v: &mut [u8],
    uv_pitch: usize,
    mbw: usize,
    mbh: usize,
    filter: &FilterSettings,
    row_filters: &[(u8, u8)],
    width: usize,
    height: usize,
    rgba: &mut [u8],
    ws: &mut EmitScratch,
    channels: usize,
    alpha: Option<&[u8]>,
) {
    let cw = (width + 1) >> 1;
    let ch = (height + 1) >> 1;
    let mut emitted = 0usize;
    for mby in 0..mbh {
        filter_macroblock_row(y, y_pitch, u, v, uv_pitch, mbw, mby, filter, &row_filters[mby * mbw..(mby + 1) * mbw]);
        let end = if mby == mbh - 1 { height } else { mby * 16 + 8 };
        if end > emitted {
            if channels == 3 {
                emit_rows_into_rgb3(y, y_pitch, u, v, uv_pitch, width, cw, ch, emitted, end, rgba, ws);
            } else {
                emit_rows_into_rgba(y, y_pitch, u, v, uv_pitch, width, cw, ch, emitted, end, rgba, ws, alpha);
            }
            emitted = end;
        }
    }
}

/// YUV 4:2:0 → RGBA8（fancy 上采样默认，等价 image-webp Bilinear / dwebp fancy 列）。
/// `decode_vp8(_with_alpha)` 已把行定稿直出的 RGBA（lossy-alpha 已含真实 A 通道）缓存在
/// `yuv.rgba`，此处直接取走（`mem::take`，零重算、零拷贝）；若为空（手工构造的 `Vp8Yuv`）
/// 回退旧整帧二趟路径，回退路径 alpha 恒 255，输出逐字节一致。
pub(crate) fn yuv_to_rgba_fancy(yuv: &mut Vp8Yuv) -> Vec<u8> {
    if !yuv.rgba.is_empty() {
        return std::mem::take(&mut yuv.rgba);
    }
    let w = yuv.width as usize;
    let h = yuv.height as usize;
    let cw = (w + 1) >> 1;
    let ch = (h + 1) >> 1;
    let mut rgba = vec![0u8; w * h * 4];
    let mut u_up = vec![0u8; w];
    let mut v_up = vec![0u8; w];
    // 复用上采样工作区（U/V 两行 t 缓冲共存，`upsample_fancy_into_uv` 各用 cw+8），避免逐行分配。
    let mut t_ws = vec![0u16; 2 * (cw + 8)];
    for row in 0..h {
        let chroma_row = row >> 1;
        let adjacent = if row % 2 == 0 {
            if chroma_row == 0 {
                0
            } else {
                chroma_row - 1
            }
        } else if chroma_row == ch - 1 {
            chroma_row
        } else {
            chroma_row + 1
        };
        let u_row = &yuv.u[chroma_row * cw..(chroma_row + 1) * cw];
        let u_adj = &yuv.u[adjacent * cw..(adjacent + 1) * cw];
        let v_row = &yuv.v[chroma_row * cw..(chroma_row + 1) * cw];
        let v_adj = &yuv.v[adjacent * cw..(adjacent + 1) * cw];
        super::simd::upsample_fancy_into_uv(u_row, u_adj, v_row, v_adj, &mut u_up, &mut v_up, &mut t_ws);
        let y_row = &yuv.y[row * w..(row + 1) * w];
        let out = &mut rgba[row * w * 4..(row + 1) * w * 4];
        super::simd::yuv_row_to_rgba(y_row, &u_up, &v_up, out);
    }
    rgba
}

/// 【dormancy，K24】YUV 4:2:0 → RGB3（w*h*3，alpha 不写）：T1 前 opaque 静态有损直出入口。
/// [`decode_vp8_rgb3`] 已把行定稿直出的 RGB3 缓存在 `yuv.rgb`，此处直接取走（`mem::take`，
/// 零重算、零拷贝）；若为空（手工构造的 `Vp8Yuv`）回退旧整帧二趟路径（走 `yuv_row_to_rgb3`，
/// 逐字节与 RGBA 前 3 字节一致）。opaque 有损现走 `yuv_to_rgba_fancy`（RGBA4 直出），保留
/// 作硬闸/测试参考与手工构造回退（K24）。
#[allow(dead_code)]
pub(crate) fn yuv_to_rgb_fancy(yuv: &mut Vp8Yuv) -> Vec<u8> {
    if !yuv.rgb.is_empty() {
        return std::mem::take(&mut yuv.rgb);
    }
    let w = yuv.width as usize;
    let h = yuv.height as usize;
    let cw = (w + 1) >> 1;
    let ch = (h + 1) >> 1;
    let mut rgb = vec![0u8; w * h * 3];
    let mut u_up = vec![0u8; w];
    let mut v_up = vec![0u8; w];
    let mut t_ws = vec![0u16; 2 * (cw + 8)];
    for row in 0..h {
        let chroma_row = row >> 1;
        let adjacent = if row % 2 == 0 {
            if chroma_row == 0 {
                0
            } else {
                chroma_row - 1
            }
        } else if chroma_row == ch - 1 {
            chroma_row
        } else {
            chroma_row + 1
        };
        let u_row = &yuv.u[chroma_row * cw..(chroma_row + 1) * cw];
        let u_adj = &yuv.u[adjacent * cw..(adjacent + 1) * cw];
        let v_row = &yuv.v[chroma_row * cw..(chroma_row + 1) * cw];
        let v_adj = &yuv.v[adjacent * cw..(adjacent + 1) * cw];
        super::simd::upsample_fancy_into_uv(u_row, u_adj, v_row, v_adj, &mut u_up, &mut v_up, &mut t_ws);
        let y_row = &yuv.y[row * w..(row + 1) * w];
        let out = &mut rgb[row * w * 3..(row + 1) * w * 3];
        super::simd::yuv_row_to_rgb3(y_row, &u_up, &v_up, out);
    }
    rgb
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

// 外部语料定位助手（与集成测试共享，见 tests/fixtures/webp/mod.rs）。
#[cfg(test)]
#[path = "../../../tests/fixtures/webp/mod.rs"]
mod webp_fixtures_support;

#[cfg(test)]
mod v1_tests {
    use super::*;

    /// 存在性驱动取夹具：缺失打印 SKIP 后返回 None（AGENTS §5：缺失属常态，不崩整测试）。
    fn fixture_bytes(name: &str) -> Option<Vec<u8>> {
        let p = webp_fixtures_support::webp_fixture_path(name)?;
        Some(std::fs::read(&p).expect("read fixture"))
    }

    /// 从完整 webp 文件取 VP8 载荷（裸容器 'VP8 '，无 VP8X：首 chunk 即 VP8 ）。
    fn vp8_payload(data: &[u8]) -> &[u8] {
        let c = super::super::container::parse(data).expect("容器解析应成功");
        assert_eq!(c.family, super::super::container::Family::StaticLossy);
        c.bitstream.unwrap()
    }

    /// 已知 fixture 的帧头字段正确性（keyframe、尺寸、mb 数、分区数）。
    fn assert_header(name: &str, w: u32, h: u32, parts: usize) {
        let Some(data) = fixture_bytes(name) else {
            eprintln!("SKIP {name} 外部语料缺失");
            return;
        };
        let payload = vp8_payload(&data);
        let fh = parse_frame_header(payload).unwrap_or_else(|e| panic!("{name} 头解析失败: {e}"));
        assert_eq!(
            (fh.width, fh.height),
            (w, h),
            "{name} 尺寸不符（帧头 vs 预期）"
        );
        assert_eq!(fh.mb_width, ((w + 15) / 16) as usize, "{name} mb_width");
        assert_eq!(fh.mb_height, ((h + 15) / 16) as usize, "{name} mb_height");
        assert_eq!(fh.partitions.len(), parts, "{name} 分区数");
    }

    #[test]
    fn tiny_1x1_lossy_header() {
        // tiny.webp：2x2 有损。
        assert_header("tiny.webp", 2, 2, 1);
    }

    #[test]
    fn flat_gray_header() {
        // flat-gray-16x16：纯 128 灰，常为 1 分区低码率。
        assert_header("flat-gray-16x16.webp", 16, 16, 1);
    }

    #[test]
    fn gradient_header() {
        assert_header("gradient-16x16.webp", 16, 16, 1);
    }

    #[test]
    fn multi_partition_header() {
        // noise-64x48-vp8-partitions8：高分辨率噪声 → 8 分区。
        assert_header("noise-64x48-vp8-partitions8.webp", 64, 48, 8);
        assert_header("noise-64x48-vp8-partitions4.webp", 64, 48, 4);
        assert_header("noise-64x48-vp8-partitions2.webp", 64, 48, 2);
    }

    #[test]
    fn segment_features_fixture() {
        // noise-32x32-segments4：显式段（4 段）。
        let Some(data) = fixture_bytes("noise-32x32-segments4.webp") else {
            eprintln!("SKIP noise-32x32-segments4.webp 外部语料缺失");
            return;
        };
        let fh = parse_frame_header(vp8_payload(&data)).unwrap();
        assert!(fh.segments.enabled, "segments4 应开段");
    }

    #[test]
    fn all_lossy_fixtures_parse_header() {
        // 头均可解析；尺寸与容器一致（容器早验，此处复核）。
        let names = [
            "lossy-alpha-m0-best.webp", // 注意：此处仅有损（无 alpha）体积小；它有 ALPH → 家族 LossyAlpha，跳过。
            "lossy-alpha-m0-fast.webp",
            "lossy-alpha-m0-none.webp",
            "lossy-alpha-m1-best.webp",
            "lossy-alpha-m1-fast.webp",
            "lossy-alpha-m1-none.webp",
            "lossy-alpha-q50.webp",
            "noise-17x17-f30-strong-s0.webp",
            "noise-17x17.webp",
            "noise-32x32-q0.webp",
            "noise-32x32-q5.webp",
            "noise-32x32-q50.webp",
            "noise-32x32-q95.webp",
            "noise-64x48-vp8-partitions2.webp",
            "noise-64x48-vp8-partitions4.webp",
            "odd-33x19-libvpx.webp",
            "testsrc2-320x240-f0.webp",
        ];
        let mut checked = 0usize;
        for name in names {
            let Some(p) = webp_fixtures_support::webp_fixture_path(name) else {
                eprintln!("SKIP {name} 外部语料缺失");
                continue;
            };
            let data = std::fs::read(&p).unwrap();
            let c = super::super::container::parse(&data).unwrap();
            if c.family != super::super::container::Family::StaticLossy {
                continue; // 带 alpha 的 lossy 家族 V6 才接
            }
            let fh = parse_frame_header(c.bitstream.unwrap())
                .unwrap_or_else(|e| panic!("{name} 头解析失败: {e}"));
            assert_eq!((fh.width, fh.height), (c.width, c.height), "{name}");
            checked += 1;
        }
        assert!(checked >= 4, "应当至少解析 4 个有损 fixture，实 {checked}");
        eprintln!("VP8_HEADER_PARSED\t{checked}");
    }
}

/// V2：宏块重建冒烟（未含环路滤波；V3 接入后换 YUV golden 对拍）。
#[cfg(test)]
mod v2_tests {
    use super::*;

    /// 存在性驱动取夹具：缺失打印 SKIP 后返回 None。
    fn fixture_bytes(name: &str) -> Option<Vec<u8>> {
        let p = webp_fixtures_support::webp_fixture_path(name)?;
        Some(std::fs::read(&p).expect("read fixture"))
    }

    fn decode_lossy(name: &str) -> Option<(Vp8Yuv, (u32, u32))> {
        let data = fixture_bytes(name)?;
        let c = super::super::container::parse(&data).ok()?;
        if c.family != super::super::container::Family::StaticLossy {
            return None;
        }
        let yuv = decode_vp8_tight(c.bitstream.unwrap(), c.width, c.height).ok()?;
        Some((yuv, (c.width, c.height)))
    }

    #[test]
    fn flat_gray_decodes() {
        let Some((yuv, (w, h))) = decode_lossy("flat-gray-16x16.webp") else {
            eprintln!("SKIP flat-gray-16x16.webp 外部语料缺失");
            return;
        };
        assert_eq!((yuv.width, yuv.height), (w, h));
        assert_eq!(yuv.y.len(), (w * h) as usize);
        // 纯灰：Y 应接近 128。
        let mean = yuv.y.iter().map(|&b| u32::from(b)).sum::<u32>() / yuv.y.len() as u32;
        assert!(
            (mean as i32 - 128).abs() < 40,
            "flat-gray Y 均值应接近 128，实 {mean}"
        );
        assert!(
            yuv.y.iter().all(|&b| b < 160 && b > 90),
            "纯灰 Y 值应在窄带内"
        );
    }

    #[test]
    fn gradient_decodes_nonblank() {
        let Some((yuv, (w, h))) = decode_lossy("gradient-16x16.webp") else {
            eprintln!("SKIP gradient-16x16.webp 外部语料缺失");
            return;
        };
        assert_eq!((yuv.width, yuv.height), (w, h));
        let y_span = yuv.y.iter().max().unwrap() - yuv.y.iter().min().unwrap();
        assert!(y_span > 30, "gradient 应有亮度梯度（span={y_span}）");
    }

    #[test]
    fn all_static_lossy_decode_to_yuv() {
        // 全部静态有损（无 ALPH）fixture 都可在 V2 重建为正确尺寸 YUV 且不 panic。
        let names = [
            "astronaut-192-q0-normal.webp",
            "astronaut-192-q10-simple.webp",
            "astronaut-192-q15-normal.webp",
            "astronaut-192-q20-simple.webp",
            "astronaut-192-q5-normal.webp",
            "flat-gray-16x16.webp",
            "gradient-16x16.webp",
            "noise-17x17-f30-strong-s0.webp",
            "noise-17x17.webp",
            "noise-32x32-f30-strong-s0.webp",
            "noise-32x32-q0.webp",
            "noise-32x32-q100.webp",
            "noise-32x32-q5.webp",
            "noise-32x32-q50.webp",
            "noise-32x32-q95.webp",
            "noise-32x32-segments4.webp",
            "noise-32x32.webp",
            "noise-64x48-vp8-partitions2.webp",
            "noise-64x48-vp8-partitions4.webp",
            "noise-64x48-vp8-partitions8.webp",
            "odd-33x19-libvpx.webp",
            "testsrc2-320x240-f0.webp",
            "testsrc2-320x240-q60-segments4-sns100-f60.webp",
            "testsrc2-320x240-q75-f100-s3.webp",
            "testsrc2-320x240-q75.webp",
            "tiny.webp",
            "webp-metadata.webp",
        ];
        let mut checked = 0usize;
        for name in names {
            // 存在性驱动：缺失 SKIP，不崩整测试。
            let Some(p) = webp_fixtures_support::webp_fixture_path(name) else {
                eprintln!("SKIP {name} 外部语料缺失");
                continue;
            };
            let data = std::fs::read(&p).unwrap();
            let c = super::super::container::parse(&data).unwrap();
            if c.family != super::super::container::Family::StaticLossy {
                continue;
            }
            let yuv = decode_vp8_tight(c.bitstream.unwrap(), c.width, c.height)
                .unwrap_or_else(|e| panic!("{name} V2 解码失败: {e}"));
            let (w, h) = (c.width as usize, c.height as usize);
            assert_eq!(yuv.y.len(), w * h, "{name} Y 长度");
            assert_eq!(yuv.u.len(), ((w + 1) / 2) * ((h + 1) / 2), "{name} U 长度");
            assert_eq!(yuv.v.len(), ((w + 1) / 2) * ((h + 1) / 2), "{name} V 长度");
            // 不空白：全黑全白图在本 corpus 不存在，抽样首位差应 >0（除 flat/gradient 已单测外）。
            checked += 1;
        }
        assert!(checked >= 6, "应至少 V2 重建 6 个有损 fixture，实 {checked}");
        eprintln!("VP8_V2_DECODED\t{checked}");
    }
}

/// V3：YUV 与 dwebp golden 逐字节对拍（golden TSV 的 YUV 列 = dwebp -yuv 平面，环路滤波后）。
#[cfg(test)]
mod v3_yuv_golden_tests {    use super::*;

    /// golden TSV 保留在仓内；图片取外部语料。
    const GOLDEN_TSV: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/webp/webp-oracle-golden.tsv");

    fn golden_yuv_sha(name: &str) -> Option<String> {
        let path = std::path::Path::new(GOLDEN_TSV);
        let text = std::fs::read_to_string(path).unwrap();
        for line in text.lines() {
            let line = line.trim_end();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() == 6 && parts[0] == name {
                return Some(parts[3].to_lowercase());
            }
        }
        None
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(bytes);
        format!("{:x}", h.finalize())
    }

    #[test]
    fn yuv_matches_golden_per_fixture() {
        let mut mismatches = Vec::new();
        let mut checked = 0usize;
        let mut skipped = 0usize;
        // 从外部语料目录按名收集（含用户语料 / imgbench-corpus / SimdCodecs 夹具）。
        let mut names = webp_fixtures_support::webp_fixture_names();
        names.sort();
        for name in names {
            if !name.ends_with(".webp") || name.starts_with("anim-") {
                continue;
            }
            let Some(expect) = golden_yuv_sha(&name) else { continue };
            let Some(path) = webp_fixtures_support::webp_fixture_path(&name) else {
                eprintln!("SKIP {name} 外部语料缺失");
                skipped += 1;
                continue;
            };
            let Some((yuv, w, h)) = (|| {
                let data = std::fs::read(&path).ok()?;
                let c = super::super::container::parse(&data).ok()?;
                if c.family != super::super::container::Family::StaticLossy {
                    return None;
                }
                let yuv = decode_vp8_tight(c.bitstream?, c.width, c.height).ok()?;
                Some((yuv, c.width as usize, c.height as usize))
            })() else {
                continue;
            };
            let mut flat = yuv.y.clone();
            let cu = (w + 1) / 2;
            let chh = (h + 1) / 2;
            flat.extend_from_slice(&yuv.u[..cu * chh]);
            flat.extend_from_slice(&yuv.v[..cu * chh]);
            let got = sha256_hex(&flat);
            checked += 1;
            if got != expect {
                mismatches.push((name, expect, got));
            }
        }
        for (name, expect, got) in &mismatches {
            eprintln!("VP8_YUV_MISMATCH\t{name}\ngolden={expect}\nours  ={got}");
        }
        eprintln!("VP8_YUV_GOLDEN_CHECKED\t{checked}, skipped={skipped}");
        assert!(
            checked >= 6,
            "应至少 6 个有损 fixture 有 YUV golden，实 {checked}"
        );
        // 目标：全 0 差异（重建+滤波逐字节对齐 dwebp -yuv）。
        assert!(
            mismatches.is_empty(),
            "{} 个 fixture YUV 与 golden 不符（V2/V3 需修）",
            mismatches.len()
        );
    }
}

/// V4/V5：RGBA fancy 输出与 image-webp 逐字节对拍（主判；全部静态有损 fixture）。
#[cfg(test)]
mod v4_rgba_parity_tests {
    use std::io::Cursor;

    use super::*;
    use crate::codecs::webp::{BACKEND_AUTO, BACKEND_OURS, WebPDecoder, set_webp_backend};
    use crate::ImageDecoder;

    /// image-webp dev oracle：直连 `image_webp::WebPDecoder` 解静态图为 RGBA8。
    fn imagewebp_rgba(data: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
        let mut dec = image_webp::WebPDecoder::new(Cursor::new(data)).ok()?;
        let (w, h) = dec.dimensions();
        let has_alpha = dec.has_alpha();
        let n = dec.output_buffer_size()?;
        let mut buf = vec![0u8; n];
        dec.read_image(&mut buf).ok()?;
        if has_alpha {
            Some((w, h, buf))
        } else {
            let mut rgba = Vec::with_capacity(w as usize * h as usize * 4);
            for px in buf.chunks_exact(3) {
                rgba.extend_from_slice(&[px[0], px[1], px[2], 255]);
            }
            Some((w, h, rgba))
        }
    }

    fn ours_rgba(data: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
        let c = super::super::container::parse(data).ok()?;
        if !matches!(
            c.family,
            super::super::container::Family::StaticLossy
                | super::super::container::Family::LossyAlpha
        ) {
            return None;
        }
        let ours = crate::codecs::webp::decode_static_lossy(data).ok()?.expect("自研有损应可接管");
        if ours.width != c.width || ours.height != c.height {
            return None;
        }
        // 有 alpha：RGBA4 原样；opaque 有损自 T1 起亦持 RGBA4（α=255，`ours.rgba` 直出）→
        // 同上分支；`rgb` 遗留恒空（dormancy），else 分支仅作手工构造回退。
        if !ours.rgba.is_empty() {
            Some((ours.width, ours.height, ours.rgba.as_bytes().to_vec()))
        } else {
            let mut rgba = Vec::with_capacity((ours.rgb.len() / 3) * 4);
            for px in ours.rgb.chunks_exact(3) {
                rgba.extend_from_slice(&[px[0], px[1], px[2], 255]);
            }
            Some((ours.width, ours.height, rgba))
        }
    }

    #[test]
    fn lossy_rgba_matches_imagewebp_byte_exact() {
        let mut mismatches = Vec::new();
        let mut checked = 0usize;
        let mut skipped = 0usize;
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
            let data = std::fs::read(&path).expect("read fixture");
            let c = match super::super::container::parse(&data) {
                Ok(c) => c,
                Err(_) => {
                    eprintln!("SKIP {name} 容器解析失败");
                    skipped += 1;
                    continue;
                }
            };
            if c.family != super::super::container::Family::StaticLossy
                && c.family != super::super::container::Family::LossyAlpha
            {
                continue;
            }
            // 任一通道解不了 → SKIP（外部语料可能含本实现暂不接的文件，不崩整测试）。
            let (Some((ow, oh, orba)), Some((iw, ih, irba))) =
                (ours_rgba(&data), imagewebp_rgba(&data))
            else {
                eprintln!("SKIP {name} 自研或 image-webp 解码失败");
                skipped += 1;
                continue;
            };
            assert_eq!((ow, oh), (iw, ih), "{name} 尺寸 OURS vs image-webp");
            assert_eq!(orba.len(), irba.len(), "{name} 长度");
            let mut diffs = 0usize;
            for (a, b) in orba.iter().zip(irba.iter()) {
                if a != b {
                    diffs += 1;
                }
            }
            checked += 1;
            if diffs != 0 {
                // 金标准（SimdCodecs 52 图名）不匹配=失败；越界语料（用户/imgbench-corpus）
                // 仅记录不判失败（AGENTS §5：找到几个测几个，别让外部语料崩整测试）。
                if webp_fixtures_support::is_golden_static_name(&name) {
                    mismatches.push((name, diffs));
                } else {
                    eprintln!("VP8_EXTRA_DIFF\t{name}\t{diffs} bytes（非金标准外部语料，仅记录）");
                }
            }
        }
        for (name, diffs) in &mismatches {
            eprintln!("VP8_RGBA_DIFF\t{name}\t{diffs} bytes");
        }
        eprintln!("VP8_RGBA_PARITY_CHECKED\t{checked}, skipped={skipped}, mismatches={}", mismatches.len());
        set_webp_backend(BACKEND_AUTO); // 复位：避免污染同 binary 其它测试的后端全局态
        assert!(
            checked >= 6,
            "应至少 6 个有损 fixture 对拍，实 {checked}"
        );
        assert!(
            mismatches.is_empty(),
            "{} 个 fixture RGBA 与 image-webp 逐字节不符（V4 fancy 需修）",
            mismatches.len()
        );
    }

    /// 冒烟：经 `WebPDecoder`（OURS 后端）解码同 fixture 与直接调用一致，且格式为 Rgba8
    /// （T1：opaque 有损恒定 RGBA8 α=255，`read_image` RGBA 目标直拷，`into_rgba8` 零拷贝）。
    #[test]
    fn ours_backend_webpdecoder() {
        set_webp_backend(BACKEND_OURS);
        let Some(path) = webp_fixtures_support::webp_fixture_path("testsrc2-320x240-q75.webp")
        else {
            eprintln!("SKIP testsrc2-320x240-q75.webp 外部语料缺失");
            return;
        };
        let data = std::fs::read(path).expect("read fixture");
        let dec = WebPDecoder::new(Cursor::new(&data)).unwrap();
        let (w, h) = dec.dimensions();
        assert_eq!((w, h), (320, 240));
        assert_eq!(dec.color_type(), crate::ColorType::Rgba8);
        let mut buf = vec![0u8; dec.total_bytes() as usize];
        dec.read_image(&mut buf).unwrap();
        assert_eq!(buf.len(), (w * h * 4) as usize);
        // α 通道恒 0xFF（opaque 有损单遍直出熔入）。
        assert!(buf.chunks_exact(4).all(|px| px[3] == 0xFF));
        set_webp_backend(BACKEND_AUTO);
    }
}

/// 诊断：有损解码分阶段计时（判别 YUV 转换 vs 帧重建占比；`#[ignore]` 手动 release 跑）。
#[cfg(test)]
mod perf_stage_tests {
    use super::*;

    /// 手动探针取夹具：外部语料缺失直接 panic（手动运行现场可见原因）。
    fn corpus_path(name: &str) -> std::path::PathBuf {
        webp_fixtures_support::webp_fixture_path(name)
            .unwrap_or_else(|| panic!("夹具缺失（外部语料）：{name}"))
    }

    /// 确定性 LCG（xorshift64*），探针用。
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x.wrapping_mul(0x2545F4914F6CDD1D)
        }
        fn byte(&mut self) -> u8 {
            (self.next() >> 40) as u8
        }
    }

    /// 全链（重建+YUV→RGBA）SIMD 开/关逐字节一致（D1/D2 接入后沿用的硬闸直证）。
    #[test]
    fn full_decode_simd_matches_scalar() {
        let _ = super::super::simd::set_simd_part_override("pred", Some(false));
        let _ = super::super::simd::set_simd_part_override("flt", Some(false));
        let Some(scalar) = helper::decode_testsrc2() else {
            eprintln!("SKIP full_decode_simd_matches_scalar: testsrc2 外部语料缺失");
            return;
        };
        let _ = super::super::simd::set_simd_part_override("pred", Some(true));
        let _ = super::super::simd::set_simd_part_override("flt", Some(true));
        let Some(simd) = helper::decode_testsrc2() else {
            eprintln!("SKIP full_decode_simd_matches_scalar: testsrc2 外部语料缺失");
            return;
        };
        let _ = super::super::simd::set_simd_part_override("pred", None);
        let _ = super::super::simd::set_simd_part_override("flt", None);
        assert_eq!(scalar, simd, "testsrc2 全链 SIMD vs 标量逐字节");
        eprintln!("VP8_FULL_DECODE_SIMD_PARITY_OK");
    }

    /// 原有诊断：有损解码分阶段计时（判别 YUV 转换 vs 帧重建占比；`#[ignore]` 手动 release 跑）。
    #[test]
    #[ignore = "手动运行"]
    fn lossy_stage_split() {
        use std::time::Instant;
        for name in [
            "testsrc2-320x240-q75.webp",
            "astronaut-192-q10-normal.webp",
            "noise-64x48-vp8-partitions8.webp",
        ] {
            let data = std::fs::read(corpus_path(name)).expect("read fixture");
            let c = super::super::container::parse(&data).unwrap();
            for _ in 0..3 {
                let t0 = Instant::now();
                let mut yuv = decode_vp8(c.bitstream.unwrap(), c.width, c.height).unwrap();
                let t1 = Instant::now();
                let rgba = yuv_to_rgba_fancy(&mut yuv);
                let t2 = Instant::now();
                let (_r, _g, _b) = (rgba[0], rgba[1], rgba[2]);
                eprintln!(
                    "VP8_STAGE {name} {}x{}: decode_vp8={:.3}ms yuv_to_rgba={:.3}ms total={:.3}ms",
                    yuv.width,
                    yuv.height,
                    t1.duration_since(t0).as_secs_f64() * 1e3,
                    t2.duration_since(t1).as_secs_f64() * 1e3,
                    t2.duration_since(t0).as_secs_f64() * 1e3,
                );
            }
        }
    }

    /// 大图分阶段占比（release 手动跑 `-- --ignored`）：真墙钟输出熵/预测/变换/滤波各占比，
    /// 用于定 ①熵批量位读 / ②IDCT SIMD / ③环路滤波 的投入方向（K53 测量先行）。
    #[test]
    #[ignore = "手动运行"]
    fn lossy_stage_split_big() {
        use std::time::Instant;
        let big = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("target")
            .join("imgbench-corpus")
            .join("WEBP")
            .join("gen28-photo-lossy-q75.webp");
        let (name, data) = if big.is_file() {
            ("gen28-photo-lossy-q75.webp".to_string(), std::fs::read(&big).unwrap())
        } else {
            ("testsrc2-320x240-q75.webp".to_string(), std::fs::read(corpus_path("testsrc2-320x240-q75.webp")).unwrap())
        };
        let c = super::super::container::parse(&data).unwrap();
        let (w, h) = (c.width, c.height);
        let rounds = 7usize;
        let mut rec_ms = 0f64;
        let mut rgba_ms = 0f64;
        let mut e_ms = 0f64;
        let mut p_ms = 0f64;
        let mut i_ms = 0f64;
        let mut f_ms = 0f64;
        stage_clock::set(true);
        for _ in 0..2 {
            let _ = decode_vp8(c.bitstream.unwrap(), w, h).unwrap();
        }
        for _ in 0..rounds {
            stage_clock::reset();
            let t0 = Instant::now();
            let mut yuv = decode_vp8(c.bitstream.unwrap(), w, h).unwrap();
            let t1 = Instant::now();
            let _rgba = yuv_to_rgba_fancy(&mut yuv);
            let t2 = Instant::now();
            let (e, p, i, f) = stage_clock::totals();
            rec_ms += t1.duration_since(t0).as_secs_f64() * 1e3;
            rgba_ms += t2.duration_since(t1).as_secs_f64() * 1e3;
            e_ms += e;
            p_ms += p;
            i_ms += i;
            f_ms += f;
        }
        stage_clock::set(false);
        let n = rounds as f64;
        let rec = rec_ms / n;
        let rgba = rgba_ms / n;
        let ent = e_ms / n;
        let prd = p_ms / n;
        let idt = i_ms / n;
        let flt = f_ms / n;
        let inner = ent + prd + idt + flt;
        eprintln!(
            "WEBP_STAGE {name} {w}x{h} rec={rec:.2}ms yuv_rgba={rgba:.2}ms full={:.2}ms | \
             entropy={ent:.2}ms({:.1}%) predict={prd:.2}ms({:.1}%) idct={idt:.2}ms({:.1}%) \
             filter+copy={flt:.2}ms({:.1}%) inner_sum={inner:.2} rec_minus_inner={:.2}ms",
            rec + rgba,
            ent / rec * 100.0,
            prd / rec * 100.0,
            idt / rec * 100.0,
            flt / rec * 100.0,
            rec - inner
        );
    }

    /// RC3 探针：各熵决策点结果偏斜（`stage_profile` feature 门控；`#[ignore]` 手动 debug 跑）。
    /// 遍历全部静态有损 fixture，累计每个决策点 bit0/bit1 次数 + 熵预算（tokens+modes 墙钟）。
    /// 判定「cmov 全算 → 预测分支形」是否值得投（K60 ≥8% 熵墙钟才投）；纯测量不改产品行为。
    #[test]
    #[ignore = "手动运行：RC3 偏斜探针"]
    #[cfg(feature = "stage_profile")]
    fn ent_skew_by_decision() {
        use std::time::Instant;
        let names = webp_fixtures_support::webp_fixture_names();
        let t0 = Instant::now();
        let mut decoded = 0usize;
        let mut skipped = 0usize;
        let mut ent_ns: u64 = 0;
        let mut bits: u64 = 0;
        let mut acct: u64 = 0;
        let mut dq: u64 = 0;
        // 32767 是 max dimension false positive 硬上限检查用的哨兵，正常 fixture 不触发。
        let mut total_px: u64 = 0;
        skew_clock::reset();
        for name in &names {
            let Some(path) = webp_fixtures_support::webp_fixture_path(name) else {
                continue;
            };
            let Ok(data) = std::fs::read(&path) else { continue };
            let Ok(c) = super::super::container::parse(&data) else {
                skipped += 1;
                continue;
            };
            if c.family != super::super::container::Family::StaticLossy
                && c.family != super::super::container::Family::LossyAlpha
            {
                skipped += 1;
                continue;
            }
            let Some(frame) = c.bitstream else {
                skipped += 1;
                continue;
            };
            total_px += u64::from(c.width) * u64::from(c.height);
            match stage_decode(&frame, c.width, c.height) {
                Ok(p) => {
                    ent_ns += p.ent_tokens_ns + p.ent_modes_ns;
                    bits += p.tokens_bits + p.modes_bits;
                    acct += p.acct_count;
                    dq += p.dq_count;
                    decoded += 1;
                }
                Err(e) => {
                    eprintln!("SKIP {name}: {e}");
                    skipped += 1;
                }
            }
        }
        let (c_bit, c_acct, c_dq) = probe_entropy_op_costs(2_000_000);
        eprintln!(
            "RC3_SKEW fixtures_decoded={decoded} skipped={skipped} px={total_px} wall={:.2}s",
            t0.elapsed().as_secs_f64()
        );
        eprintln!("RC3_SKEW ent_ns={ent_ns} bits={bits} acct={acct} dq={dq}");
        eprintln!("RC3_SKEW c_bit={c_bit:.3}ns c_acct={c_acct:.3}ns c_dq={c_dq:.3}ns");
        eprintln!(
            "\nRC3_SKEW 决策点偏斜表（熵墙钟预算 = ent_tokens+ent_modes {:.3}ms, bits={})",
            ent_ns as f64 / 1e6,
            bits
        );
        eprintln!("RC3_SKEW\t点 | count0/count1 | 偏斜m | 少数r | 仿uop净(sym) | 误判罚(sym) | 次数");
        let mut all_sym_cycles = 0f64;
        for (name, counts) in skew_clock::all() {
            let total = counts.c0 + counts.c1;
            if total == 0 {
                continue;
            }
            let hi = counts.c0.max(counts.c1);
            let m = hi as f64 / total as f64;
            let r = 1.0 - m;
            let net = (1.0 - 2.0 * r) * 7.0;
            let misp = r * 20.0;
            let sym = net - misp;
            all_sym_cycles += sym * total as f64;
            eprintln!(
                "RC3_SKEW\t{name} | {}/{} | {:.4} | {:.4} | {:+.2} | {:.2} | {}",
                counts.c0, counts.c1, m, r, sym, misp, total
            );
        }
        // 折入熵墙钟%：模型每符号（一次 bit 解码）≈7 cycle（汇编取证 ent_read_bit ~28-30uop
        // 吞吐受限）；以同构建 c_bit_ns/7 把模型 cycle→ns 换算，占比 = 净省 ns / 实测熵墙钟 ns。
        let cyc_ns = c_bit / 7.0;
        let save_ns = all_sym_cycles * cyc_ns;
        let pct = save_ns / ent_ns.max(1) as f64 * 100.0;
        eprintln!(
            "\nRC3_SKEW 合计（全部点，含抛硬币点 cost>0）净省 ≈ {:.2} Mcy → {:.2} ms → 熵墙钟 {:.2}%",
            all_sym_cycles / 1e6,
            save_ns / 1e6,
            pct
        );
        // 候选点 = 偏斜 ≥85% 且 net>0（分支形确有正收益者）。
        eprintln!("RC3_SKEW 候选点（偏斜≥85% 且净收益>0）：");
        let mut c_cycles = 0f64;
        for (name, counts) in skew_clock::all() {
            let total = counts.c0 + counts.c1;
            if total == 0 {
                continue;
            }
            let hi = counts.c0.max(counts.c1);
            let m = hi as f64 / total as f64;
            let r = 1.0 - m;
            let sym = (1.0 - 2.0 * r) * 7.0 - r * 20.0;
            if m >= 0.85 && sym > 0.0 {
                c_cycles += sym * total as f64;
                eprintln!(
                    "RC3_SKEW\t   {name}: m={:.4} r={:.4} net={:+.2}cy/sym × {total}次 = {:.2} Mcy ({:.3}ms)",
                    m,
                    r,
                    sym,
                    sym * total as f64 / 1e6,
                    sym * total as f64 * cyc_ns / 1e6
                );
            }
        }
        let c_ns = c_cycles * cyc_ns;
        let c_pct = c_ns / ent_ns.max(1) as f64 * 100.0;
        eprintln!(
            "RC3_SKEW 候选点净省 ≈ {:.2} Mcy → {:.3} ms → 熵墙钟 {:.2}%",
            c_cycles / 1e6,
            c_ns / 1e6,
            c_pct
        );
        eprintln!(
            "RC3_SKEW 口径：c_bit={c_bit:.3}ns/bit（同构建），模型 7cy/bit ⇒ 1cy ≈ {:.4}ns；熵墙钟=实测 ent_tokens+ent_modes",
            cyc_ns
        );
        assert!(decoded > 0, "应至少解出 >0 个静态有损 fixture，实 {decoded}");
    }

    /// 熔合 SIMD vs 标量全路径逐字节：随机小帧 + 随机滤波强度（level/inner/sharpness/simple
    /// 全跨），以生产 `filter_macroblock_row` 为对象 —— 覆盖真实强度求值、宏块左/上/内边全组合
    /// 与「后边读前边已滤像素」的顺序依赖。SIMD 路由经进程内 override 强制开/关对拍。
    #[test]
    fn filter_macroblock_simd_matches_scalar() {
        use super::super::simd::{set_simd_part_override, simd_on};
        if !simd_on("sse2") {
            eprintln!("SIMD_TEST SKIP (无 sse2)");
            return;
        }
        let mut rng = Lcg(0xf1a7b0c0ffee1234);
        let (mbw, mbh) = (5usize, 2usize);
        let y_pitch = mbw * 16 + 32;
        let uv_pitch = mbw * 8 + 32;
        let y_rows = mbh * 16 + 32;
        let uv_rows = mbh * 8 + 32;
        let mut checked = 0usize;
        for simple in [false, true] {
            for sharpness in [0u8, 1, 4, 5, 7] {
                for _trial in 0..4 {
                    let mut rf = Vec::with_capacity(mbw * mbh);
                    for _ in 0..mbw * mbh {
                        // level 覆盖：0（跳过）、hev 阈值上下 15/39/40、普通 2/63、随机 ≤63。
                        let level = match rng.next() % 7 {
                            0 => 0u8,
                            1 => 2,
                            2 => 15,
                            3 => 39,
                            4 => 40,
                            5 => 63,
                            _ => (rng.byte() % 64),
                        };
                        let inner = if (rng.next() & 1) == 0 { 1u8 } else { 0u8 };
                        rf.push((level, inner));
                    }
                    let mut flt = FilterSettings::default();
                    flt.simple = simple;
                    flt.sharpness = sharpness;
                    let mut ay = vec![0u8; y_rows * y_pitch];
                    let mut au = vec![0u8; uv_rows * uv_pitch];
                    let mut av = vec![0u8; uv_rows * uv_pitch];
                    fill_rand_byte(&mut rng, &mut ay);
                    fill_rand_byte(&mut rng, &mut au);
                    fill_rand_byte(&mut rng, &mut av);
                    let (mut by, mut bu, mut bv) = (ay.clone(), au.clone(), av.clone());
                    for mby in 0..mbh {
                        set_simd_part_override("flt", Some(true));
                        filter_macroblock_row(&mut ay, y_pitch, &mut au, &mut av, uv_pitch, mbw, mby, &flt, &rf[mby * mbw..(mby + 1) * mbw]);
                        set_simd_part_override("flt", Some(false));
                        filter_macroblock_row(&mut by, y_pitch, &mut bu, &mut bv, uv_pitch, mbw, mby, &flt, &rf[mby * mbw..(mby + 1) * mbw]);
                        assert_eq!(ay, by, "Y 熔合 vs 标量 simple={simple} sharp={sharpness} mby={mby}");
                        assert_eq!(au, bu, "U 熔合 vs 标量 simple={simple} sharp={sharpness} mby={mby}");
                        assert_eq!(av, bv, "V 熔合 vs 标量 simple={simple} sharp={sharpness} mby={mby}");
                        checked += 1;
                    }
                    set_simd_part_override("flt", None);
                }
            }
        }
        eprintln!("FILTER_MACROBLOCK_SIMD_PARITY\t{checked}");
    }

    fn fill_rand_byte(rng: &mut Lcg, plane: &mut [u8]) {
        for b in plane.iter_mut() {
            *b = rng.byte();
        }
    }

    mod helper {
        use super::*;
        /// 解 testsrc2-320x240-q75（全链 YUV+RGBA），返回 (y, u, v, rgba)；夹具缺失返回 None。
        pub fn decode_testsrc2() -> Option<(Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>)> {
            let p = webp_fixtures_support::webp_fixture_path("testsrc2-320x240-q75.webp")?;
            let data = std::fs::read(p).expect("read fixture");
            let c = super::super::super::container::parse(&data).expect("容器解析");
            let mut yuv = decode_vp8_tight(c.bitstream.unwrap(), c.width, c.height).expect("VP8 解码");
            let rgba = yuv_to_rgba_fancy(&mut yuv);
            Some((yuv.y, yuv.u, yuv.v, rgba))
        }
    }

    /// D1/D2 SIMD 性能探针（release 手动跑 `-- --ignored`）：中大图全链 ours-SIMD
    /// 各部件开/关 + 与 image-webp 中位比值；输出 `WEBP_SIMD_D1/D2 before/after/ratio`。
    #[test]
    #[ignore = "手动运行"]
    fn simd_d1_d2_probe() {
        use std::time::Instant;
        let mut images: Vec<(String, Vec<u8>)> = Vec::new();
        for name in [
            "testsrc2-320x240-q75.webp",
            "astronaut-192-q20-normal.webp",
            "noise-64x48-vp8-partitions8.webp",
        ] {
            if let Some(p) = webp_fixtures_support::webp_fixture_path(name) {
                images.push((name.to_string(), std::fs::read(p).unwrap()));
            } else {
                eprintln!("WEBP_SIMD_PROBE note 夹具缺失（外部语料）：{name}");
            }
        }
        // 大图语料：target/imgbench-corpus/WEBP（缺则 SKIP）。
        let big = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("target")
            .join("imgbench-corpus")
            .join("WEBP")
            .join("gen28-photo-lossy-q75.webp");
        if big.is_file() {
            images.push(("gen28-photo-lossy-q75.webp".to_string(), std::fs::read(&big).unwrap()));
        } else {
            eprintln!("WEBP_SIMD_PROBE note 大图语料缺失（target/imgbench-corpus/WEBP/gen28-photo-lossy-q75.webp）");
        }

        fn median(mut f: impl FnMut(), n: usize) -> f64 {
            f();
            let mut acc = Vec::with_capacity(n);
            for _ in 0..n {
                let t = Instant::now();
                f();
                acc.push(t.elapsed().as_secs_f64() * 1e3);
            }
            acc.sort_by(|a, b| a.partial_cmp(b).unwrap());
            acc[acc.len() / 2]
        }

        for (name, bytes) in &images {
            let c = super::super::container::parse(bytes).unwrap();
            let (w, h) = (c.width, c.height);
            // 轮数：大图 9 轮取中位压噪声；小图同样 9（微秒级也稳）。
            let rounds = 9usize;
            let run = |pred: Option<bool>, flt: Option<bool>, dct: Option<bool>| -> (f64, f64) {
                super::super::simd::set_simd_part_override("pred", pred);
                super::super::simd::set_simd_part_override("flt", flt);
                super::super::simd::set_simd_part_override("dct", dct);
                // 返回 (仅重建 decode_vp8, 全链 decode+rgba)。
                (
                    median(
                        || {
                            let _yuv = decode_vp8(c.bitstream.unwrap(), w, h).unwrap();
                        },
                        rounds,
                    ),
                    median(
                        || {
                            let mut yuv = decode_vp8(c.bitstream.unwrap(), w, h).unwrap();
                            let _rgba = yuv_to_rgba_fancy(&mut yuv);
                        },
                        rounds,
                    ),
                )
            };
            let (rec_all, full_all) = run(Some(true), Some(true), Some(true));
            let (rec_pred_off, full_pred_off) = run(Some(false), Some(true), Some(true));
            let (rec_flt_off, full_flt_off) = run(Some(true), Some(false), Some(true));
            let (rec_dct_off, full_dct_off) = run(Some(true), Some(true), Some(false));
            let (rec_scalar, full_scalar) = run(Some(false), Some(false), Some(false));
            // 复位（探针结束不污染同进程其它测试）。
            super::super::simd::set_simd_part_override("pred", None);
            super::super::simd::set_simd_part_override("flt", None);
            super::super::simd::set_simd_part_override("dct", None);
            // 以「仅重建」为主标尺（D1/D2 影响的是帧重建；yuv→rgba 恒定但大图分配噪声大）。
            eprintln!(
                "WEBP_SIMD_D1 {name} {}x{} rec_before={rec_pred_off:.3}ms rec_after={rec_all:.3}ms ratio={:.3} \
                 full_before={full_pred_off:.3}ms full_after={full_all:.3}ms ratio={:.3}",
                w, h, rec_all / rec_pred_off, full_all / full_pred_off
            );
            eprintln!(
                "WEBP_SIMD_D2 {name} {}x{} rec_before={rec_flt_off:.3}ms rec_after={rec_all:.3}ms ratio={:.3} \
                 full_before={full_flt_off:.3}ms full_after={full_all:.3}ms ratio={:.3}",
                w, h, rec_all / rec_flt_off, full_all / full_flt_off
            );
            eprintln!(
                "WEBP_SIMD_D3 {name} {}x{} rec_before={rec_dct_off:.3}ms rec_after={rec_all:.3}ms ratio={:.3} \
                 full_before={full_dct_off:.3}ms full_after={full_all:.3}ms ratio={:.3}",
                w, h, rec_all / rec_dct_off, full_all / full_dct_off
            );
            eprintln!(
                "WEBP_SIMD_TOTAL {name} {}x{} rec_scalar={rec_scalar:.3} rec_all={rec_all:.3} ratio={:.3} \
                 full_scalar={full_scalar:.3} full_all={full_all:.3} ratio={:.3}",
                w, h, rec_all / rec_scalar, full_all / full_scalar
            );
        }

        // 内核级微基准：直接以分派包装循环调用，量化每个内核 SIMD vs 标量（脱离解码器噪声）。
        eprintln!("WEBP_SIMD_KERNELS start");
        let mut krng = Lcg(0xfedcba9876543210);
        let kpitch = 96usize;
        let mut kplane = vec![0u8; 96 * 96];
        for b in kplane.iter_mut() {
            *b = krng.byte();
        }
        let inc = |r: &mut Lcg| -> u8 { match r.next() % 6 { 0 => 0, 1 => 255, 2 => 128, _ => r.byte() } };
        // D1 4×4 微基准。
        {
            let mut above = [0u8; 9];
            let mode_cycle = [0u8, 1, 2, 3, 4, 5, 6, 7, 8, 9];
            let mut sub = |mode: u8, iters: usize, simd: bool| -> f64 {
                super::super::simd::set_simd_part_override("pred", Some(simd));
                for b in kplane.iter_mut() {
                    *b = inc(&mut krng);
                }
                above = [0u8; 9];
                for i in 0..9 {
                    above[i] = inc(&mut krng);
                }
                median(
                    || {
                        for i in 0..iters {
                            let m = mode_cycle[(i + mode as usize) % 10];
                            let r0 = 4 + (i % 20);
                            let c0 = 4 + (i / 20) % 20;
                            super::super::simd::predict_subblock(&mut kplane, kpitch, r0, c0, m, &above);
                        }
                    },
                    5,
                )
            };
            let it = 120_000usize;
            let st = sub(0, it, false);
            let sv = sub(0, it, true);
            eprintln!("WEBP_SIMD_KERNEL subblock4x4 iters={it} scalar={st:.3}ms simd={sv:.3}ms ratio={:.3}", sv / st);
        }
        // D1 16×16 / 8×8 宏基准。
        {
            let mut large = |size: usize, mode: u8, iters: usize, simd: bool| -> f64 {
                super::super::simd::set_simd_part_override("pred", Some(simd));
                for b in kplane.iter_mut() {
                    *b = inc(&mut krng);
                }
                let above: Vec<u8> = (0..=size).map(|_| inc(&mut krng)).collect();
                median(
                    || {
                        for i in 0..iters {
                            super::super::simd::predict_large(
                                &mut kplane, kpitch, 4 + (i % 40), 4 + (i % 40), size, mode, true, true, &above,
                            );
                        }
                    },
                    5,
                )
            };
            let it = 40_000usize;
            for size in [16usize, 8] {
                for mode in 0u8..4 {
                    let st = large(size, mode, it, false);
                    let sv = large(size, mode, it, true);
                    eprintln!(
                        "WEBP_SIMD_KERNEL predict_large size={size} mode={mode} iters={it} scalar={st:.3}ms simd={sv:.3}ms ratio={:.3}",
                        sv / st
                    );
                }
            }
        }
        // D2 滤波微基准：Simple / Inner / Macroblock × 竖/横 × 16/8 lane。
        {
            let mut fe = |kind: u8, vertical: bool, length: usize, iters: usize, simd: bool| -> f64 {
                super::super::simd::set_simd_part_override("flt", Some(simd));
                for b in kplane.iter_mut() {
                    *b = inc(&mut krng);
                }
                let (simple, mbe) = match kind {
                    0 => (true, true),
                    1 => (false, false),
                    _ => (false, true),
                };
                let (interior, edge, hev) = if simple {
                    (1i32, 18i32, 2i32)
                } else if mbe {
                    (7, 30, 1)
                } else {
                    (5, 24, 1)
                };
                median(
                    || {
                        for i in 0..iters {
                            let x = 8 + (i % 16);
                            let y = 8 + (i / 16) % 32;
                            super::filter_edge(
                                &mut kplane, kpitch, x, y, length, vertical, mbe, simple, interior, edge, hev,
                            );
                        }
                    },
                    5,
                )
            };
            let it = 60_000usize;
            for kind in 0u8..3 {
                for vertical in [true, false] {
                    for length in [16usize, 8] {
                        let st = fe(kind, vertical, length, it, false);
                        let sv = fe(kind, vertical, length, it, true);
                        eprintln!(
                            "WEBP_SIMD_KERNEL filter kind={} vertical={} len={} iters={it} scalar={st:.3}ms simd={sv:.3}ms ratio={:.3}",
                            kind, vertical, length, sv / st
                        );
                    }
                }
            }
        }
        super::super::simd::set_simd_part_override("pred", None);
        super::super::simd::set_simd_part_override("flt", None);
    }
}


/// V7：确定性 fuzz（位翻转/截断/插字节），property = 永不 panic（K27 落仓）。
#[cfg(test)]
mod v7_fuzz_tests {
    use std::io::Cursor;

    use super::*;

    /// 确定性 LCG（xorshift64*），不依赖 rand。
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x.wrapping_mul(0x2545F4914F6CDD1D)
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    #[test]
    fn mutated_inputs_never_panic() {
        let seeds = [
            "tiny.webp",
            "flat-gray-16x16.webp",
            "gradient-16x16.webp",
            "lossless-z0.webp",
            "noise-32x32-q50.webp",
            "lossy-alpha-q50.webp",
        ];
        let mut rng = Lcg(0x9E3779B97F4A7C15);
        let mut rounds = 0usize;
        for seed in seeds {
            // 存在性驱动：缺失 SKIP，不崩整测试。
            let Some(p) = webp_fixtures_support::webp_fixture_path(seed) else {
                eprintln!("SKIP fuzz seed {seed} 外部语料缺失");
                continue;
            };
            let base = std::fs::read(&p).unwrap();
            for round in 0..40 {
                let kind = rng.below(4);
                let mut data = base.clone();
                match kind {
                    0 => {
                        // 1-3 位翻转。
                        let flips = 1 + rng.below(3);
                        for _ in 0..flips {
                            let off = rng.below(data.len());
                            let bit = rng.below(8);
                            data[off] ^= 1 << bit;
                        }
                    }
                    1 => {
                        // 截断到随机长度。
                        let len = rng.below(data.len() + 1);
                        data.truncate(len);
                    }
                    2 => {
                        // 随机位置插字节（0..30 字节）。
                        let count = rng.below(30) + 1;
                        let off = rng.below(data.len() + 1);
                        let mut ins = vec![0u8; count];
                        for b in ins.iter_mut() {
                            *b = rng.next() as u8;
                        }
                        data.splice(off..off, ins);
                    }
                    _ => {
                        // 区块随机字节改写（≤16 字节）。
                        let count = rng.below(16) + 1;
                        let hi = data.len().saturating_sub(count).max(1);
                        let off = rng.below(hi);
                        let end = (off + count).min(data.len());
                        for b in data[off..end].iter_mut() {
                            *b = rng.next() as u8;
                        }
                    }
                }
                // ① 容器解析不 panic。
                let _ = super::super::container::parse(&data);
                // ② 全链路 WebPDecoder 不 panic（AUTO 后端：自研可接管家族走 Ours/其余 image-webp）。
                let _ = crate::codecs::webp::WebPDecoder::new(Cursor::new(&data));
                // ③ 自研入口不 panic（防止几何放大：仅当容器解析成功且像素量受控时深解）。
                if let Ok(c) = super::super::container::parse(&data) {
                    let px = u64::from(c.width) * u64::from(c.height);
                    if px > 0 && px <= (1 << 20) {
                        if c.family.is_static_lossless() {
                            let _ = crate::codecs::webp::decode_static_lossless(&data);
                        } else if matches!(
                            c.family,
                            super::super::container::Family::StaticLossy
                                | super::super::container::Family::LossyAlpha
                        ) {
                            let _ = crate::codecs::webp::decode_static_lossy(&data);
                        }
                    }
                }
                let _ = round;
                rounds += 1;
            }
        }
        assert!(rounds >= 200, "fuzz 轮次应 ≥200，实 {rounds}");
        eprintln!("VP8_FUZZ_ROUNDS\t{rounds}");
    }
}

// ---------------------------------------------------------------------------
// 双线程环路滤波（MT）：一致性硬闸 + 中位墙钟探针
// ---------------------------------------------------------------------------

/// 双线程分带滤波（`KIMG_WEBP_THREADS=2`）一致性与收益探针。
#[cfg(test)]
mod mt_tests {
    use super::*;
    use super::super::container;

    /// 进程内 env 修改锁（pn gsimd parity 同款：并发测试下串行化 env 翻转，防联动交错）。
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// 按 `threads`（`KIMG_WEBP_THREADS` 值）解码静态有损整帧：返回 YUV + RGBA。
    fn decode_with_threads(data: &[u8], threads: &str) -> Option<(Vp8Yuv, Vec<u8>)> {
        unsafe {
            std::env::set_var("KIMG_WEBP_THREADS", threads);
        }
        let c = container::parse(data).ok()?;
        let mut yuv = decode_vp8(c.bitstream?, c.width, c.height).ok()?;
        let rgba = yuv_to_rgba_fancy(&mut yuv);
        Some((yuv, rgba))
    }

    /// 硬闸：threads=2（双带并行）与 threads=0（现串行）输出**逐字节一致**（YUV+RGBA 全平面）。
    /// 覆盖全部静态有损 fixture（外部语料存在性驱动；缺则 SKIP），含宽图（gen28 2400×3600）、
    /// 奇数高/矮图、单/多宏块行等边界——并行路径的每一字节都与串行硬性相同。
    #[test]
    fn threads2_matches_serial_byte_exact() {
        let _g = ENV_LOCK.lock().unwrap();
        let mut checked = 0usize;
        let mut names: Vec<String> = webp_fixtures_support::webp_fixture_names();
        names.sort();
        for name in names {
            let Some(p) = webp_fixtures_support::webp_fixture_path(&name) else {
                continue;
            };
            let data = std::fs::read(&p).expect("read fixture");
            let Ok(c) = container::parse(&data) else {
                continue;
            };
            if c.family != container::Family::StaticLossy
                && c.family != container::Family::LossyAlpha
            {
                continue;
            }
            let Some((serial_yuv, serial_rgba)) = decode_with_threads(&data, "0") else {
                eprintln!("SKIP {name} 串行解码失败（外部语料不可解）");
                continue;
            };
            let Some((para_yuv, para_rgba)) = decode_with_threads(&data, "2") else {
                panic!("{name} threads=2 解码失败（threads=0 已成功）");
            };
            assert_eq!(
                serial_yuv,
                para_yuv,
                "{name}: threads=2 与 threads=0 YUV 平面逐字节一致"
            );
            assert_eq!(
                serial_rgba,
                para_rgba,
                "{name}: threads=2 与 threads=0 RGBA 逐字节一致"
            );
            checked += 1;
        }
        unsafe {
            std::env::remove_var("KIMG_WEBP_THREADS");
        }
        assert!(
            checked >= 3,
            "应至少对拍 3 个有损 fixture（当前 {checked}；缺外部语料）"
        );
        eprintln!("WEBP_MT_PARITY_OK\t{checked}");
    }

    /// 大图双线程滤波中位墙钟（release 手动跑 `-- --ignored`）：输出
    /// `WEBP_MT <name> <w>x<h> before=..ms after=..ms ratio=..`，供「是否默认开」决策（K53；
    /// before=threads=0 串行中位，after=threads=2 中位，ratio=before/after）。
    #[test]
    #[ignore = "手动运行"]
    fn lossy_mt_probe() {
        use std::time::Instant;
        let big = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("target")
            .join("imgbench-corpus")
            .join("WEBP")
            .join("gen28-photo-lossy-q75.webp");
        let (name, data) = if big.is_file() {
            (
                "gen28-photo-lossy-q75.webp".to_string(),
                std::fs::read(&big).unwrap(),
            )
        } else {
            let p = webp_fixtures_support::webp_fixture_path("testsrc2-320x240-q75.webp")
                .unwrap_or_else(|| panic!("夹具缺失（外部语料）：testsrc2-320x240-q75.webp"));
            ("testsrc2-320x240-q75.webp".to_string(), std::fs::read(&p).unwrap())
        };
        let c = container::parse(&data).unwrap();
        let (w, h) = (c.width, c.height);
        let _g = ENV_LOCK.lock().unwrap();
        let rounds = 9usize;
        let dec = || {
            let _ = decode_vp8(c.bitstream.unwrap(), w, h).unwrap();
        };
        let median = |v: &mut Vec<f64>| {
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            v[v.len() / 2]
        };
        unsafe {
            std::env::set_var("KIMG_WEBP_THREADS", "0");
        }
        for _ in 0..2 {
            dec();
        }
        let mut t0 = Vec::new();
        for _ in 0..rounds {
            let t = Instant::now();
            dec();
            t0.push(t.elapsed().as_secs_f64() * 1e3);
        }
        unsafe {
            std::env::set_var("KIMG_WEBP_THREADS", "2");
        }
        for _ in 0..2 {
            dec();
        }
        let mut t2 = Vec::new();
        for _ in 0..rounds {
            let t = Instant::now();
            dec();
            t2.push(t.elapsed().as_secs_f64() * 1e3);
        }
        unsafe {
            std::env::remove_var("KIMG_WEBP_THREADS");
        }
        let before = median(&mut t0);
        let after = median(&mut t2);
        // 双线程与串行逐字节一致（探针内复核；若不一致抛错，WEBP_MT 前中止）。
        unsafe {
            std::env::set_var("KIMG_WEBP_THREADS", "0");
        }
        let sy = decode_vp8(c.bitstream.unwrap(), w, h).unwrap();
        unsafe {
            std::env::set_var("KIMG_WEBP_THREADS", "2");
        }
        let py = decode_vp8(c.bitstream.unwrap(), w, h).unwrap();
        unsafe {
            std::env::remove_var("KIMG_WEBP_THREADS");
        }
        assert_eq!(sy, py, "WEBP_MT 探针：双线程与串行 YUV 逐字节一致");
        eprintln!(
            "WEBP_MT {name} {w}x{h} before={before:.3}ms after={after:.3}ms ratio={:.3}",
            before / after
        );
    }

    /// RC1 硬闸「bmi ≡ baseline」：同一宏体（`parse_residuals_impl!` /
    /// `read_macroblock_modes_impl!`）展开的快道（lzcnt/bmi）与基线（bsr）逐字节一致——双路径仅
    /// 指令选择不同，字节必须恒等。对有损 + lossy-alpha fixture 双态对拍三路：串行·基线 vs
    /// 串行·BMI vs 双线程·BMI 全等。CPU 无 bmi2 → SKIP（快道不可合法执行，本闸自动退化为
    /// 基线自对，不崩）。配合既有 MT parity（默认 ent=auto→bmi）即把「双路径字节恒等」钉全
    /// （AGENTS §5：缺夹具 SKIP 是常态不是回归）。
    #[test]
    fn ent_bmi_matches_baseline_byte_exact() {
        let _g = ENV_LOCK.lock().unwrap();
        // bmi2 可用性直接问 CPU（绕过 env 部件开关；覆盖 `set_simd_part_override` 不越权）。
        let bmi2 = {
            #[cfg(target_arch = "x86_64")]
            {
                std::arch::is_x86_feature_detected!("bmi2")
            }
            #[cfg(not(target_arch = "x86_64"))]
            {
                false
            }
        };
        let mut checked = 0usize;
        let mut names: Vec<String> = webp_fixtures_support::webp_fixture_names();
        names.sort();
        for name in names {
            let Some(p) = webp_fixtures_support::webp_fixture_path(&name) else {
                continue;
            };
            let data = std::fs::read(&p).expect("read fixture");
            let Ok(c) = container::parse(&data) else {
                continue;
            };
            if c.family != container::Family::StaticLossy
                && c.family != container::Family::LossyAlpha
            {
                continue;
            }
            // 基线（串行）：强制 ent 走 `*_base`（bsr 指令选择）。
            super::super::simd::set_simd_part_override("ent", Some(false));
            let Some(base) = decode_with_threads(&data, "0") else {
                eprintln!("SKIP {name} 基线串行解码失败（外部语料不可解）");
                continue;
            };
            if !bmi2 {
                eprintln!("SKIP {name}（CPU 无 bmi2，快道不可用，仅基线自对）");
                continue;
            }
            // 快道（串行 + 双线程）：强制 ent 走 `*_bmi`（lzcnt/bmi 指令选择）。
            super::super::simd::set_simd_part_override("ent", Some(true));
            let Some(bmi_serial) = decode_with_threads(&data, "0") else {
                panic!("{name} 快道串行解码失败（基线已成功）");
            };
            let Some(bmi_para) = decode_with_threads(&data, "2") else {
                panic!("{name} 快道 threads=2 解码失败（基线已成功）");
            };
            assert_eq!(
                base.0, bmi_serial.0,
                "{name}: entropy BMI vs baseline 串行 YUV/RGBA 逐字节"
            );
            assert_eq!(
                base.1, bmi_serial.1,
                "{name}: entropy BMI vs baseline 串行 RGBA 逐字节"
            );
            assert_eq!(
                bmi_serial.0, bmi_para.0,
                "{name}: entropy BMI 串行 vs 双线程 YUV/RGBA 逐字节"
            );
            assert_eq!(
                bmi_serial.1, bmi_para.1,
                "{name}: entropy BMI 串行 vs 双线程 RGBA 逐字节"
            );
            checked += 1;
        }
        super::super::simd::set_simd_part_override("ent", None);
        unsafe {
            std::env::remove_var("KIMG_WEBP_THREADS");
        }
        assert!(
            checked >= 3 || !bmi2,
            "应至少对拍 3 个有损 fixture（当前 {checked}；缺外部语料；无 bmi2 时退化为基线自对，不判失败）"
        );
        eprintln!("WEBP_ENT_BMI_PARITY_OK\t{checked}\t(serial_bmi≡baseline ∧ MT_bmi≡serial_bmi)");
    }

    /// RC3 硬闸「branch ≡ cmov」：同宏体（`tokens`/`read_category_3_to_6`，bmi/base 共享）的
    /// 预测分支形（ZERO/G5/CAT8-10，默认开）与 cmov 全算形（`set_ent_branch_override(false)`）
    /// 输出**逐字节一致**——纯控制流重构，双形仅分支/codegen 不同。对有损 + lossy-alpha fixture
    /// 三路对拍：串行·cmov vs 串行·branch vs 双线程·branch 全等（同 `ent_bmi_matches_baseline_byte_exact`
    /// 姿势；生产默认熵路径 auto→bmi，RC1 已独立保证 bmi≡base，故本闸不重复翻 ent 双模）。
    /// 缺夹具 SKIP 是常态不是回归（AGENTS §5）。
    #[test]
    fn ent_branch_matches_cmov_byte_exact() {
        let _g = ENV_LOCK.lock().unwrap();
        let mut checked = 0usize;
        let mut names: Vec<String> = webp_fixtures_support::webp_fixture_names();
        names.sort();
        for name in names {
            let Some(p) = webp_fixtures_support::webp_fixture_path(&name) else {
                continue;
            };
            let data = std::fs::read(&p).expect("read fixture");
            let Ok(c) = container::parse(&data) else {
                continue;
            };
            if c.family != container::Family::StaticLossy
                && c.family != container::Family::LossyAlpha
            {
                continue;
            }
            // cmov 全算（对照侧）：分支形强制关。
            super::super::simd::set_ent_branch_override(Some(false));
            let Some(cmov) = decode_with_threads(&data, "0") else {
                eprintln!("SKIP {name} 基线串行解码失败（外部语料不可解）");
                continue;
            };
            // 预测分支形（默认侧）：串行 + 双线程。
            super::super::simd::set_ent_branch_override(Some(true));
            let Some(branch_serial) = decode_with_threads(&data, "0") else {
                panic!("{name} 分支形串行解码失败（基线已成功）");
            };
            let Some(branch_para) = decode_with_threads(&data, "2") else {
                panic!("{name} 分支形 threads=2 解码失败（基线已成功）");
            };
            assert_eq!(
                cmov.0, branch_serial.0,
                "{name}: 熵分支形 vs cmov 串行 YUV/RGBA 逐字节"
            );
            assert_eq!(
                cmov.1, branch_serial.1,
                "{name}: 熵分支形 vs cmov 串行 RGBA 逐字节"
            );
            assert_eq!(
                branch_serial.0, branch_para.0,
                "{name}: 熵分支形串行 vs 双线程 YUV/RGBA 逐字节"
            );
            assert_eq!(
                branch_serial.1, branch_para.1,
                "{name}: 熵分支形串行 vs 双线程 RGBA 逐字节"
            );
            checked += 1;
        }
        super::super::simd::set_ent_branch_override(None);
        unsafe {
            std::env::remove_var("KIMG_WEBP_THREADS");
        }
        assert!(
            checked >= 3,
            "应至少对拍 3 个有损 fixture（当前 {checked}；缺外部语料）"
        );
        eprintln!(
            "WEBP_ENT_BRANCH_PARITY_OK\t{checked}\t(serial_cmov≡branch ∧ MT_branch≡serial_branch)"
        );
    }

    /// RC3 A/B 同窗对拍（release 手动跑 `-- --ignored`）：进程内 override 只翻「分支形开/关」，
    /// 同二进制同布局——隔离的恰是本改动的控制流差异（比跨构建对比更公平）。
    /// 输出 tri：`entropy_stage`（stage_clock 熵段累计，含 per-MB Instant 插桩，看相对变化）、
    /// `decode`（`decode_vp8` 整帧墙钟，无插桩，主标尺）、串行与双线程两档。
    /// 裁决（K55/K60）：熵墙钟或 71 decode 净提升 ≥8%（噪声带外）→ 默认开；≤0 → 回退全部 RC3；
    /// 0~8% → 报真实数字由主 agent 定。
    #[test]
    #[ignore = "手动运行（release）"]
    fn ent_branch_ab_probe() {
        use std::time::Instant;
        let corpus = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("target")
            .join("imgbench-corpus")
            .join("WEBP");
        // 4K 高熵组（ZERO/G5 触发量大，双路径收益主战场）+ 纯 VP8 photo 各档（低熵，须无回归）。
        let files: [&str; 6] = [
            "syn-noise-3840x2160.webp",
            "syn-mandel-3840x2160.webp",
            "syn-testsrc2-3840x2160.webp",
            "strip-h6-lossy.webp",
            "img_v3_0216r_0836ba29-7fe2-4c91-b768-77565ad841eg_MIDDLE_WEBP.webp",
            "try-it-in-slintpad.webp",
        ];
        let median = |v: &mut Vec<f64>| {
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            v[v.len() / 2]
        };
        let measure =
            |data: &[u8], w: u32, h: u32, threads: &str, branch: bool, rounds: usize, clock: bool| -> (f64, f64) {
                super::super::simd::set_ent_branch_override(Some(branch));
                unsafe {
                    std::env::set_var("KIMG_WEBP_THREADS", threads);
                }
                let mut wall = Vec::new();
                let mut ent = Vec::new();
                for _ in 0..2 {
                    let _ = decode_vp8(data, w, h).unwrap();
                }
                for _ in 0..rounds {
                    if clock {
                        stage_clock::reset();
                        stage_clock::set(true);
                    }
                    let t = Instant::now();
                    let _ = decode_vp8(data, w, h).unwrap();
                    wall.push(t.elapsed().as_secs_f64() * 1e3);
                    let e = if clock {
                        let (ev, _, _, _) = stage_clock::totals();
                        stage_clock::set(false);
                        ev
                    } else {
                        0.0
                    };
                    ent.push(e);
                }
                (median(&mut wall), median(&mut ent))
            };
        let _g = ENV_LOCK.lock().unwrap();
        for name in files {
            let p = corpus.join(name);
            if !p.is_file() {
                eprintln!("SKIP {name}（外部语料缺失）");
                continue;
            }
            let data = std::fs::read(&p).unwrap();
            let Ok(c) = container::parse(&data) else {
                eprintln!("SKIP {name}（container 不可解析）");
                continue;
            };
            if c.family != container::Family::StaticLossy {
                eprintln!("SKIP {name}（family != StaticLossy，非本双路径测试对象）");
                continue;
            }
            let Some(bs) = c.bitstream else {
                eprintln!("SKIP {name}（bitstream=None）");
                continue;
            };
            let (w, h) = (c.width, c.height);
            if decode_vp8(bs, w, h).is_err() {
                eprintln!("SKIP {name}（decode_vp8 不可解，排除非本测试对象）");
                continue;
            }
            let rounds = if w * h >= 3840 * 2160 { 7 } else { 11 };
            for threads in ["0", "2"] {
                // 同窗交错 A/B（K68：单窗 off→on 顺序块会被热迁移/频率漂移污染）：
                // 每轮 off/on 交错采样，两边同窗均完成；`measure` 只做单侧采样。
                super::super::simd::set_ent_branch_override(Some(false));
                unsafe {
                    std::env::set_var("KIMG_WEBP_THREADS", threads);
                }
                let mut w_off = Vec::new();
                let mut e_off = Vec::new();
                let mut w_on = Vec::new();
                let mut e_on = Vec::new();
                let run = |branch: bool, wall: &mut Vec<f64>, ent: &mut Vec<f64>| {
                    super::super::simd::set_ent_branch_override(Some(branch));
                    for _ in 0..1 {
                        let _ = decode_vp8(bs, w, h).unwrap();
                    }
                    stage_clock::reset();
                    stage_clock::set(true);
                    let t = Instant::now();
                    let _ = decode_vp8(bs, w, h).unwrap();
                    wall.push(t.elapsed().as_secs_f64() * 1e3);
                    let (ev, _, _, _) = stage_clock::totals();
                    ent.push(ev);
                    stage_clock::set(false);
                };
                for _ in 0..2 {
                    run(false, &mut w_off, &mut e_off);
                    run(true, &mut w_on, &mut e_on);
                }
                for _ in 0..rounds {
                    run(false, &mut w_off, &mut e_off);
                    run(true, &mut w_on, &mut e_on);
                }
                super::super::simd::set_ent_branch_override(None);
                let (w_off_m, e_off_m) = (median(&mut w_off), median(&mut e_off));
                let (w_on_m, e_on_m) = (median(&mut w_on), median(&mut e_on));
                eprintln!(
                    "WEBP_ENT_BRANCH_AB {name} {w}x{h} thr={threads}: decode {}ms vs {}ms (ratio {:.3}) | \
                     ent_stage {}ms vs {}ms (ratio {:.3})  [interleaved; off=cmov on=branch]",
                    w_off_m,
                    w_on_m,
                    w_off_m / w_on_m,
                    e_off_m,
                    e_on_m,
                    e_off_m / e_on_m
                );
            }
        }
        super::super::simd::set_ent_branch_override(None);
        // 整 lossy 集汇总（74//71 有损 fixture：每只 decode_vp8 一次，off/on 各一遍，墙钟累加；
        // 覆盖率反映「整体 lossy(71)」验收口径，大图（4K noise）主导；此处 clean 墙钟（无插桩）。
        for threads in ["0", "2"] {
            let mut off_sum = 0.0f64;
            let mut on_sum = 0.0f64;
            let mut n = 0usize;
            for name in webp_fixtures_support::webp_fixture_names() {
                let Some(p) = webp_fixtures_support::webp_fixture_path(&name) else {
                    continue;
                };
                let data = std::fs::read(&p).expect("read fixture");
                let Ok(c) = container::parse(&data) else {
                    continue;
                };
                if c.family != container::Family::StaticLossy {
                    continue;
                }
                let Some(bs) = c.bitstream else {
                    continue;
                };
                let (w, h) = (c.width, c.height);
                if decode_vp8(bs, w, h).is_err() {
                    continue; // 非本解码器可解（如动画帧包），不在 lossy(71) 口径内
                }
                let (wo, _) = measure(bs, w, h, threads, false, 1, false);
                let (won, _) = measure(bs, w, h, threads, true, 1, false);
                off_sum += wo;
                on_sum += won;
                n += 1;
            }
            eprintln!(
                "WEBP_ENT_BRANCH_CORPUS_SUM thr={threads} fixtures={n}: decode {}ms vs {}ms (ratio {:.3})",
                off_sum,
                on_sum,
                off_sum / on_sum
            );
        }
        unsafe {
            std::env::remove_var("KIMG_WEBP_THREADS");
        }
    }

    /// RC1 A/B 同窗对拍（release 手动跑 `-- --ignored`）：进程内 override 只翻「熵走快道还是
    /// 基线」，同二进制同布局——隔离的恰是本改动的纯指令选择差异（比跨构建对比更公平）。
    /// 输出 tri：`entropy_stage`（stage_clock 熵段累计，含 per-MB Instant 插桩，看相对变化）、
    /// `decode`（`decode_vp8` 整帧墙钟，无插桩，主标尺）、串行与双线程两档。
    #[test]
    #[ignore = "手动运行（release）"]
    fn ent_bmi_ab_probe() {
        use std::time::Instant;
        let corpus = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("target")
            .join("imgbench-corpus")
            .join("WEBP");
        // 4K 高熵组（renorm 触发 65-85%，双路径收益主战场）+ 纯 VP8 photo 各档（低熵，须无回归）。
        let files: [&str; 6] = [
            "syn-noise-3840x2160.webp",
            "syn-mandel-3840x2160.webp",
            "syn-testsrc2-3840x2160.webp",
            "strip-h6-lossy.webp",
            "img_v3_0216r_0836ba29-7fe2-4c91-b768-77565ad841eg_MIDDLE_WEBP.webp",
            "try-it-in-slintpad.webp",
        ];
        let median = |v: &mut Vec<f64>| {
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            v[v.len() / 2]
        };
        let measure = |data: &[u8], w: u32, h: u32, threads: &str, bmi: bool, rounds: usize, clock: bool| -> (f64, f64) {
            super::super::simd::set_simd_part_override("ent", Some(bmi));
            unsafe {
                std::env::set_var("KIMG_WEBP_THREADS", threads);
            }
            let mut wall = Vec::new();
            let mut ent = Vec::new();
            for _ in 0..2 {
                let _ = decode_vp8(data, w, h).unwrap();
            }
            for _ in 0..rounds {
                if clock {
                    stage_clock::reset();
                    stage_clock::set(true);
                }
                let t = Instant::now();
                let _ = decode_vp8(data, w, h).unwrap();
                wall.push(t.elapsed().as_secs_f64() * 1e3);
                let e = if clock {
                    let (ev, _, _, _) = stage_clock::totals();
                    stage_clock::set(false);
                    ev
                } else {
                    0.0
                };
                ent.push(e);
            }
            (median(&mut wall), median(&mut ent))
        };
        let _g = ENV_LOCK.lock().unwrap();
        for name in files {
            let p = corpus.join(name);
            if !p.is_file() {
                eprintln!("SKIP {name}（外部语料缺失）");
                continue;
            }
            let data = std::fs::read(&p).unwrap();
            let Ok(c) = container::parse(&data) else {
                eprintln!("SKIP {name}（container 不可解析）");
                continue;
            };
            if c.family != container::Family::StaticLossy {
                eprintln!("SKIP {name}（family != StaticLossy，非本双路径测试对象）");
                continue;
            }
            let Some(bs) = c.bitstream else {
                eprintln!("SKIP {name}（bitstream=None）");
                continue;
            };
            let (w, h) = (c.width, c.height);
            if decode_vp8(bs, w, h).is_err() {
                eprintln!("SKIP {name}（decode_vp8 不可解，排除非本测试对象）");
                continue;
            }
            let rounds = if w * h >= 3840 * 2160 { 7 } else { 11 };
            for threads in ["0", "2"] {
                // 同窗交错 A/B（K68：单窗 off→on 顺序块会被热迁移/频率漂移污染）：
                // 每轮 off/on 交错采样，两边同窗均完成；`measure` 只做单侧采样。
                super::super::simd::set_simd_part_override("ent", Some(false));
                unsafe {
                    std::env::set_var("KIMG_WEBP_THREADS", threads);
                }
                let mut w_off = Vec::new();
                let mut e_off = Vec::new();
                let mut w_on = Vec::new();
                let mut e_on = Vec::new();
                let run = |bmi: bool, wall: &mut Vec<f64>, ent: &mut Vec<f64>| {
                    super::super::simd::set_simd_part_override("ent", Some(bmi));
                    for _ in 0..1 {
                        let _ = decode_vp8(bs, w, h).unwrap();
                    }
                    stage_clock::reset();
                    stage_clock::set(true);
                    let t = Instant::now();
                    let _ = decode_vp8(bs, w, h).unwrap();
                    wall.push(t.elapsed().as_secs_f64() * 1e3);
                    let (ev, _, _, _) = stage_clock::totals();
                    ent.push(ev);
                    stage_clock::set(false);
                };
                for _ in 0..2 {
                    run(false, &mut w_off, &mut e_off);
                    run(true, &mut w_on, &mut e_on);
                }
                for _ in 0..rounds {
                    run(false, &mut w_off, &mut e_off);
                    run(true, &mut w_on, &mut e_on);
                }
                super::super::simd::set_simd_part_override("ent", None);
                let (w_off_m, e_off_m) = (median(&mut w_off), median(&mut e_off));
                let (w_on_m, e_on_m) = (median(&mut w_on), median(&mut e_on));
                eprintln!(
                    "WEBP_ENT_AB {name} {w}x{h} thr={threads}: decode {}ms vs {}ms (ratio {:.3}) | \
                     ent_stage {}ms vs {}ms (ratio {:.3})  [interleaved]",
                    w_off_m,
                    w_on_m,
                    w_off_m / w_on_m,
                    e_off_m,
                    e_on_m,
                    e_off_m / e_on_m
                );
            }
        }
        super::super::simd::set_simd_part_override("ent", None);
        // 整 lossy 集汇总（74//71 有损 fixture：每只 decode_vp8 一次，off/on 各一遍，墙钟累加；
        // 覆盖率反映「整体 lossy(71)」验收口径，大图（4K noise）主导；此处 clean 墙钟（无插桩）。
        for threads in ["0", "2"] {
            let mut off_sum = 0.0f64;
            let mut on_sum = 0.0f64;
            let mut n = 0usize;
            for name in webp_fixtures_support::webp_fixture_names() {
                let Some(p) = webp_fixtures_support::webp_fixture_path(&name) else {
                    continue;
                };
                let data = std::fs::read(&p).expect("read fixture");
                let Ok(c) = container::parse(&data) else {
                    continue;
                };
                if c.family != container::Family::StaticLossy {
                    continue;
                }
                let Some(bs) = c.bitstream else {
                    continue;
                };
                let (w, h) = (c.width, c.height);
                if decode_vp8(bs, w, h).is_err() {
                    continue; // 非本解码器可解（如动画帧包），不在 lossy(71) 口径内
                }
                let (wo, _) = measure(bs, w, h, threads, false, 1, false);
                let (won, _) = measure(bs, w, h, threads, true, 1, false);
                off_sum += wo;
                on_sum += won;
                n += 1;
            }
            eprintln!(
                "WEBP_ENT_CORPUS_SUM thr={threads} fixtures={n}: decode {}ms vs {}ms (ratio {:.3})",
                off_sum,
                on_sum,
                off_sum / on_sum
            );
        }
        unsafe {
            std::env::remove_var("KIMG_WEBP_THREADS");
        }
    }

    /// RC2 A/B：熵快道（ent 强制 ON）上 `KIMG_WEBP_ENT_RENORM_SKIP` 开（默认，range≥128 跳过
    /// lzcnt+shl）vs 关（无条件 renorm）——lzcnt=1c 后「跳过」收益变化，本探针重新对拍。
    /// 进程内 override（`set_renorm_skip_override`）交错同窗，测 noise/testsrc2/photo。
    /// 【评审实测结论（2026-11-03）】净口径「持平偏正」：noise 反而嫌 skipped（decode
    /// ratio~0.96，无 skip 略快），testsrc2/photo 持平/略负——即 skip 分支整体不带来可测收益，
    /// 亦无系统性伤害。因净口径不显著，保留默认开（既有语义）、不做外提/移除（证据驱动不硬动）。
    #[test]
    #[ignore = "手动运行（release；RC2）"]
    fn ent_renorm_skip_ab_probe() {
        use std::time::Instant;
        let corpus = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("target")
            .join("imgbench-corpus")
            .join("WEBP");
        let files: [&str; 3] = [
            "syn-noise-3840x2160.webp",
            "syn-testsrc2-3840x2160.webp",
            "strip-h6-lossy.webp",
        ];
        let median = |v: &mut Vec<f64>| {
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            v[v.len() / 2]
        };
        let _g = ENV_LOCK.lock().unwrap();
        super::super::simd::set_simd_part_override("ent", Some(true));
        unsafe {
            std::env::set_var("KIMG_WEBP_THREADS", "0");
        }
        for name in files {
            let p = corpus.join(name);
            if !p.is_file() {
                eprintln!("SKIP {name}（外部语料缺失）");
                continue;
            }
            let data = std::fs::read(&p).unwrap();
            let Ok(c) = container::parse(&data) else {
                continue;
            };
            let Some(bs) = c.bitstream else {
                continue;
            };
            let (w, h) = (c.width, c.height);
            let rounds = if w * h >= 3840 * 2160 { 7 } else { 11 };
            let run = |skip: bool, wall: &mut Vec<f64>, ent: &mut Vec<f64>| {
                set_renorm_skip_override(Some(skip));
                for _ in 0..1 {
                    let _ = decode_vp8(bs, w, h).unwrap();
                }
                stage_clock::reset();
                stage_clock::set(true);
                let t = Instant::now();
                let _ = decode_vp8(bs, w, h).unwrap();
                wall.push(t.elapsed().as_secs_f64() * 1e3);
                let (ev, _, _, _) = stage_clock::totals();
                ent.push(ev);
                stage_clock::set(false);
            };
            let (mut w_on, mut e_on, mut w_off, mut e_off) =
                (Vec::new(), Vec::new(), Vec::new(), Vec::new());
            for _ in 0..2 {
                run(true, &mut w_on, &mut e_on);
                run(false, &mut w_off, &mut e_off);
            }
            for _ in 0..rounds {
                run(true, &mut w_on, &mut e_on);
                run(false, &mut w_off, &mut e_off);
            }
            eprintln!(
                "WEBP_ENT_RENORM_AB {name} {w}x{h} thr=0: decode(skipOn) {}ms vs {}ms (ratio {:.3}) | \
                 ent_stage {}ms vs {}ms (ratio {:.3})  [skip=ON vs OFF, 快道]",
                median(&mut w_on),
                median(&mut w_off),
                median(&mut w_on) / median(&mut w_off),
                median(&mut e_on),
                median(&mut e_off),
                median(&mut e_on) / median(&mut e_off)
            );
        }
        set_renorm_skip_override(None);
        super::super::simd::set_simd_part_override("ent", None);
        unsafe {
            std::env::remove_var("KIMG_WEBP_THREADS");
        }
    }
}
