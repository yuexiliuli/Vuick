#![forbid(unsafe_code)]
use super::*;

// ---------------------------------------------------------------------------
// V1：RFC 6386 §7 boolean 熵解码器（宽窗口版）
// ---------------------------------------------------------------------------

/// 熵 renorm 跳过开关：`KIMG_WEBP_ENT_RENORM_SKIP`（默认「开」＝启用跳过）：
/// range≥128 时 renorm 位移恒为 0（no-op），跳过 lzcnt+shl；设 `0`/`false`/`off` 装回
/// 无条件 renorm（同二进制 A/B 与双路径字节恒等交叉验证）。首次访问读一次 env 并缓存
/// （同 simd.rs `webp_no_simd`/`yuv_u16_enabled` 的惰性 OnceLock 姿势，不引新依赖）。
/// `#[cfg(test)]` 可经 `set_renorm_skip_override` 进程内翻转（优先于 env，RC2 同窗 A/B 用）。
#[inline]
pub(crate) fn ent_renorm_skip() -> bool {
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
    pub(crate) data: &'a [u8],
    /// 已消费字节数。
    pub(crate) pos: usize,
    /// 窗口（值 + 后续位）。
    pub(crate) value: u64,
    /// 有效位数（可为负：低位未读，下次 refill）。
    pub(crate) bits: i32,
    /// 范围减一（恒在 [127,255] 区间外延）。
    pub(crate) range_minus1: u32,
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
    /// 休眠：生产改直调 `ent_read_signed`，无生产调用者，仅测试/探针用。
    #[cfg_attr(not(test), allow(dead_code))]
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
        // 安全性：分支保证 *pos + 8 <= len；非对齐直读下沉 `simd::raw::read_u64_unaligned`。
        let raw = super::simd::raw::read_u64_unaligned(data, *pos);
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
pub(crate) fn ent_split_pre(
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
    // bits 上界不变量（证明）：初始 -8；refill 快道 +56（自 <0）→[48,55]、慢道 +8 出环 ≥0→[0,7]；
    // renorm 仅 `*bits -= shift`（shift∈[0,7]，`ent_renorm_apply`/`ent_read_signed` 均 ±≤8）。
    // 故本点（refill 后）恒有 bits ∈ [0,55] < 64。单点载入 `b` 并对其下界假设，移位直接用该 `b`
    // （保证假设与移位共用同一 SSA 值，KnownBits 传导后 x86 变量移位前 `andl $63` 判恒等折叠删除；
    // 掩码值本仅喂移位计数，字节恒等不变）。debug 构建假设转真实断言（验证期兜底）。
    // Safety：上方不变量证明 b<64 恒成立；下界假设下沉 `simd::raw::ent_shift_amt_lt_64`
    // （内联后 `llvm.assume` 与原 b 同一 SSA，`andl $63` 折叠删除，codegen 与隔离前一致）。
    let b = super::simd::raw::ent_shift_amt_lt_64(*bits as u32);
    let rm1 = *range_minus1;
    // p255 命中占比探针（仅测试）：诊断 flat 低比特率图在熵决策点的 p==255 占比（曾证
    // syn-bars 79.6% / grad 49.3% / testsrc2·mandel·slintpad ≈20%）。据此评估过的「p==255
    // 免乘特化」（(rm1*255)>>8 ≡ rm1-1，rm1∈[127,255] 精确恒等）同窗 A/B 实测 ≈0（见
    // 2026-11-06 批注），未达 ≥3% 派单门槛 → 不投（K107），此处仅保留计数探针供复用。
    #[cfg(test)]
    p255_probe::bump(probability == 255);
    let split_minus1 = (rm1 * prob) >> 8;
    (*value >> b, split_minus1, rm1)
}

/// renorm 应用（单源）：`range`（位判定结果）落 `range_minus1`/`bits`。跳过条件
/// `renorm_skip && range >= 128` 与 `range < 128 || !renorm_skip` 德摩根等价
/// （renorm_skip=false 时恒走 lzcnt），输出逐字节一致。
#[inline(always)]
pub(crate) fn ent_renorm_apply(bits: &mut i32, range_minus1: &mut u32, range: u32, renorm_skip: bool) {
    // renorm 跳过 = `renorm_skip && range >= 128`（原 `range < 128 || !renorm_skip` 的德摩根等价，
    // 输出逐字节一致）。写成分支嵌套：先测 renorm_skip（宏块级不变量，可留寄存器），再对 range
    // 直接 `cmp+jb`（用 `cmpl` 旗标，免 `seta` 物化 `range>=128` 后再 `testb` 合并）。
    if renorm_skip && range >= 128 {
        *range_minus1 = range - 1;
    } else {
        let shift = range.leading_zeros() - 24;
        *range_minus1 = (range << shift) - 1;
        *bits -= shift as i32;
    }
}

/// 热路径读位（概率 0..=255）：`read_bit_val` 的标量 in/out 版，逐位算术完全一致，唯熵态不再
/// 装进 40B 结构按值周转。`renorm_skip` 由调用方宏块入口判定一次（TOP4）。**cmov 全算形**：
/// 位判定选 `range`/`bit` 走无分支 select，供抛硬币/近硬币点用（RC3 预测分支形只留给高偏斜点）。
#[inline(always)]
pub(crate) fn ent_read_bit(
    data: &[u8],
    pos: &mut usize,
    value: &mut u64,
    bits: &mut i32,
    range_minus1: &mut u32,
    renorm_skip: bool,
    probability: u8,
) -> u32 {
    let (vs, sp, rm1) = ent_split_pre(data, pos, value, bits, range_minus1, probability);
    // 减法位移位数与 ent_split_pre 同源（其间 bits 不被改写）：单点载入并对同一 SSA 下界假设
    // （见 `ent_split_pre`，bits ∈ [0,55] < 64 可证；debug 转真实断言）。下界假设下沉
    // `simd::raw::ent_shift_amt_lt_64`（内联后 assume 与原 b 同一 SSA）。
    // Safety：b<64 恒成立。
    let b = super::simd::raw::ent_shift_amt_lt_64(*bits as u32);
    let (range, bit) = if vs > u64::from(sp) {
        *value -= (u64::from(sp + 1)) << b;
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
// pub(crate) 再导出：`simd::raw::mbdec` 迁入的 `read_macroblock_modes_impl!` 内
// `probe_skew!` 须在展开点（raw 子树）可解析——经 `vp8::probe_skew` 按名导入。
pub(crate) use probe_skew;
#[inline(always)]
pub(crate) fn ent_read_signed(
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
    // 同 `ent_split_pre` 的 bits 上界不变量（refill 后 bits ∈ [0,55] < 64，可证）；单点载入 `b`
    // 并对其下界假设、两处移位共用该 `b`（删 `andl $63`）；debug 转真实断言。下界假设下沉
    // `simd::raw::ent_shift_amt_lt_64`（内联后 assume 与原 b 同一 SSA）。
    // Safety：b<64 恒成立。
    let b = super::simd::raw::ent_shift_amt_lt_64(*bits as u32);
    let rm1 = *range_minus1 as i32;
    let split_minus1 = rm1 >> 1;
    let mask = (split_minus1 - (*value >> b) as i32) >> 31;
    let dec_bit = (split_minus1 + 1) & mask;
    *value -= (dec_bit as u32 as u64) << b;
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
// ---------------------------------------------------------------------------
// p255 命中占比探针（仅 `#[cfg(test)]`，生产零代码影响）：诊断 flat 低比特率图在熵决策点
// 的真实 p==255 占比——供「p255 免乘特化」的收益判定与派单门槛（K107 三件套）。
// ---------------------------------------------------------------------------
#[cfg(test)]
pub(crate) mod p255_probe {
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
    pub static TOTAL: AtomicU64 = AtomicU64::new(0);
    pub static HIT: AtomicU64 = AtomicU64::new(0);
    #[inline(always)]
    pub fn bump(hit: bool) {
        TOTAL.fetch_add(1, Relaxed);
        if hit {
            HIT.fetch_add(1, Relaxed);
        }
    }
    pub fn reset() {
        TOTAL.store(0, Relaxed);
        HIT.store(0, Relaxed);
    }
    pub fn report() -> (u64, u64) {
        (TOTAL.load(Relaxed), HIT.load(Relaxed))
    }
}

#[cfg(test)]
mod p255_share_tests {
    use super::*;

    /// 解码指定静态有损 fixture，返回 `(总 ent_split_pre 调用数, 其中 p==255 数)`。
    fn p255_share(name: &str) -> Option<(u64, u64)> {
        let p = super::super::webp_fixtures_support::webp_fixture_path(name)?;
        let data = std::fs::read(&p).ok()?;
        let c = super::super::super::container::parse(&data).ok()?;
        if c.family != super::super::super::container::Family::StaticLossy {
            return None;
        }
        let (w, h) = (c.width, c.height);
        p255_probe::reset();
        let _ = super::super::decode::decode_vp8_tight(c.bitstream.unwrap(), w, h);
        Some(p255_probe::report())
    }

    #[test]
    fn p255_share_in_flat_lossy() {
        for name in [
            "syn-bars-3840x2160.webp",
            "syn-testsrc2-3840x2160.webp",
            "syn-mandel-3840x2160.webp",
            "try-it-in-slintpad.webp",
            "syn-grad-1280x720.webp",
        ] {
            let Some((total, hit)) = p255_share(name) else {
                eprintln!("VP8_P255_SHARE SKIP {name} 外部语料缺失");
                continue;
            };
            let pct = if total > 0 { hit as f64 * 100.0 / total as f64 } else { 0.0 };
            eprintln!(
                "VP8_P255_SHARE {name}\ttotal={total}\thit_p255={hit}\t{:.1}%",
                pct
            );
        }
    }
}

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

