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

/// VP8 解码产物：紧密 YUV 4:2:0 平面（y=w*h，u/v=(w+1)/2 × (h+1)/2），环路滤波后。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Vp8Yuv {
    pub width: u32,
    pub height: u32,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
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
    #[inline(always)]
    pub(crate) fn read_bit_val(mut self, probability: u8) -> (Self, u32) {
        let prob = u32::from(probability);
        if self.bits < 0 {
            self = self.refill();
        }
        let range_minus1 = self.range_minus1;
        let split_minus1 = (range_minus1 * prob) >> 8;
        let (range, bit);
        if (self.value >> (self.bits as u32)) > u64::from(split_minus1) {
            self.value -= (u64::from(split_minus1 + 1)) << (self.bits as u32);
            range = range_minus1 - split_minus1;
            bit = 1;
        } else {
            range = split_minus1 + 1;
            bit = 0;
        }
        // range ∈ [1,255]，移回 [128,255]。
        let shift = range.leading_zeros() - 24;
        self.range_minus1 = (range << shift) - 1;
        self.bits -= shift as i32;
        (self, bit)
    }

    /// 热路径按值读「概率 128 符号位」并算术取符号（C# `ReadSigned`）：符号是抛硬币，
    /// 分支约半数预测失败；用 `mask = (split - (window))>>31` 全表达式消除分支，
    /// 与 `read_bit(128)+符号分支` 逐位同语义。`magnitude` 为系数幅值，返回取符号结果。
    #[inline(always)]
    pub(crate) fn read_signed_val(mut self, magnitude: i32) -> (Self, i32) {
        if self.bits < 0 {
            self = self.refill();
        }
        // range 持有减一状态 [127,254]；prob=128 时 split-1 = (range*128)>>8 = range>>1。
        let range = self.range_minus1 as i32;
        let split_minus1 = range >> 1;
        // mask = -1 ⇔ 窗口 > split-1（bit=1）；否则 0。窗口 < 256，强转 i32 安全。
        let mask = (split_minus1 - (self.value >> (self.bits as u32)) as i32) >> 31;
        // bit=1 时减 (split+1) << bits（C# `(split+1) & mask` 的存储位数形式）。
        let dec_bit = (split_minus1 + 1) & mask;
        self.value -= (dec_bit as u32 as u64) << (self.bits as u32);
        // bit=1: range - split；bit=0: split+1。
        let new_range = (split_minus1 + 1 + ((range - 2 * split_minus1 - 1) & mask)) as u32;
        let shift = new_range.leading_zeros() - 24;
        self.range_minus1 = (new_range << shift) - 1;
        self.bits -= shift as i32;
        // mask=0 → magnitude；mask=-1 → ~m + 1 = -m（补码）。
        (self, (magnitude ^ mask) - mask)
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

    /// 需要输入：`bits < 0` 时 refill。整 8 字节非对齐直读（区内安全），只消费 7 字节。
    #[inline(always)]
    fn refill(self) -> Self {
        if self.data.len() - self.pos >= 8 {
            // 安全性：分支保证 pos + 8 <= len，且输入缓冲由调用方保证至少按需可读 8 字节
            // （同 `data[pos..pos+8]` 切片界）；`read_unaligned` 自行处理非对齐载荷，
            // 只读取内存不越界。`pos` 恒 <= len（仅消费路径推进），`as_ptr().add` 不越界。
            // `u64::from_be` = C# `ReverseEndianness(ReadUnaligned<ulong>)` 的大端解释。
            let raw = unsafe { (self.data.as_ptr().add(self.pos) as *const u64).read_unaligned() };
            let raw = u64::from_be(raw);
            let mut s = self;
            s.value = (s.value << 56) | (raw >> 8);
            s.pos += 7;
            s.bits += 56;
            s
        } else {
            self.refill_tail()
        }
    }

    /// 尾部逐字节补充，越过末尾补零（仅数据末尾触发，冷路径但也必须内联：byval 结构体参数
    /// 在 Win64 上会把调用方结构体槽位地址传进来 → 熵态 alloca 逃逸 → SROA 放弃提升、逐位回写栈；
    /// `#[inline(always)]` 消除该函数边界，保住寄存器驻留）。
    #[inline(always)]
    fn refill_tail(mut self) -> Self {
        loop {
            let next = if self.pos < self.data.len() {
                let b = self.data[self.pos];
                self.pos += 1;
                u64::from(b)
            } else {
                0
            };
            self.value = (self.value << 8) | next;
            self.bits += 8;
            if self.bits >= 0 {
                break;
            }
        }
        self
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
fn subblock_above(
    plane: &[u8],
    pitch: usize,
    row0: usize,
    col0: usize,
    block_x: usize,
    block_y: usize,
    above_y: &[u8; 21],
) -> [u8; 9] {
    let mut a = [0u8; 9];
    if block_y == 0 {
        let base = 4 * block_x;
        a[0] = above_y[base];
        for k in 0..8 {
            a[1 + k] = above_y[base + 1 + k];
        }
        return a;
    }
    let row_above = row0 - 1;
    a[0] = plane[plane_at_l(row_above, pitch) + col0];
    if block_x < 3 {
        for k in 0..8 {
            a[1 + k] = plane[plane_at(row_above, pitch, col0 + k)];
        }
        return a;
    }
    // block_x == 3：右侧 4 像素取宏块 aboveY[16..19]。
    for k in 0..4 {
        a[1 + k] = plane[plane_at(row_above, pitch, col0 + k)];
    }
    for k in 0..4 {
        a[5 + k] = above_y[17 + k];
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

/// RFC 6386 §13.2 DCT_CAT3..6 子树（p8 之后）附额外位。熵态按值携带（寄存器驻留）。
#[inline(always)]
fn read_category_3_to_6<'a>(
    mut d: Vp8BoolDecoder<'a>,
    probs: &[u8],
    p_idx: usize,
) -> (Vp8BoolDecoder<'a>, i32) {
    let (extra, base): (&[u8], i32) = {
        let (dd, b) = d.read_bit_val(probs[p_idx + 8]);
        d = dd;
        if b == 0 {
            let (dd, b) = d.read_bit_val(probs[p_idx + 9]);
            d = dd;
            if b == 0 {
                (&PCAT3, 11)
            } else {
                (&PCAT4, 19)
            }
        } else {
            let (dd, b) = d.read_bit_val(probs[p_idx + 10]);
            d = dd;
            if b == 0 {
                (&PCAT5, 35)
            } else {
                (&PCAT6, 67)
            }
        }
    };
    let mut value = 0i32;
    let mut i = 0;
    while extra[i] != 0 {
        let (dd, b) = d.read_bit_val(extra[i]);
        d = dd;
        value = (value << 1) | b as i32;
        i += 1;
    }
    (d, base + value)
}

/// 读一个块的系数 token（RFC 6386 §13.2），去量化后按 zigzag 写回 `output`（调用前须清零）。
/// 返回最后非零 token 的 zigzag 位置 + 1（无 token 返回 0）。熵态按值携带 + `#[inline(always)]`
/// 使每宏块 25 次调用内联成单函数、{value,bits,range} 全程寄存器驻留（C# `Tokens(ref …)` 同款）。
#[inline(always)]
fn tokens<'a>(
    mut d: Vp8BoolDecoder<'a>,
    probabilities: &[u8],
    context: usize,
    mut position: usize,
    output: &mut [i16; 16],
    dc_factor: i32,
    ac_factor: i32,
) -> (Vp8BoolDecoder<'a>, usize) {
    let mut p_idx = BAND_ROW_OFFSETS[position] + context * 11;
    let mut last = 0usize;
    let (dd, b) = d.read_bit_val(probabilities[p_idx]);
    d = dd;
    if b != 0 {
        loop {
            let (dd, b) = d.read_bit_val(probabilities[p_idx + 1]);
            d = dd;
            if b == 0 {
                position += 1;
                if position == 16 {
                    break;
                }
                p_idx = BAND_ROW_OFFSETS[position];
                continue;
            }
            let (value, next_context): (i32, usize) = {
                let (dd, b) = d.read_bit_val(probabilities[p_idx + 2]);
                d = dd;
                if b == 0 {
                    (1, 11)
                } else {
                    let (dd, b) = d.read_bit_val(probabilities[p_idx + 3]);
                    d = dd;
                    if b == 0 {
                        let (dd, b) = d.read_bit_val(probabilities[p_idx + 4]);
                        d = dd;
                        if b == 0 {
                            (2, 22)
                        } else {
                            let (dd, b) = d.read_bit_val(probabilities[p_idx + 5]);
                            d = dd;
                            (3 + b as i32, 22)
                        }
                    } else {
                        let (dd, b) = d.read_bit_val(probabilities[p_idx + 6]);
                        d = dd;
                        if b == 0 {
                            let (dd, b) = d.read_bit_val(probabilities[p_idx + 7]);
                            d = dd;
                            if b == 0 {
                                let (dd, b) = d.read_bit_val(159);
                                d = dd;
                                (5 + b as i32, 22)
                            } else {
                                let (dd, b) = d.read_bit_val(165);
                                d = dd;
                                let high = b << 1;
                                let (dd, b) = d.read_bit_val(145);
                                d = dd;
                                (7 + (high | b) as i32, 22)
                            }
                        } else {
                            let (dd, cat) = read_category_3_to_6(d, probabilities, p_idx);
                            d = dd;
                            (cat, 22)
                        }
                    }
                }
            };
            let factor = if position == 0 { dc_factor } else { ac_factor };
            // 无分支符号（C# ReadSigned：符号是抛硬币，分支一半预测失败）。
            let (dd, signed) = d.read_signed_val(value);
            d = dd;
            output[ZIGZAG[position] as usize] = (signed * factor) as i16;
            last = position + 1;
            position += 1;
            if position == 16 {
                break;
            }
            p_idx = BAND_ROW_OFFSETS[position] + next_context;
            let (dd, b) = d.read_bit_val(probabilities[p_idx]);
            d = dd;
            if b == 0 {
                break; // 下一位置 EOB。
            }
        }
    }
    (d, last)
}

/// 解析一个宏块全部系数（Y2 先于 luma 当 i16）。返回两块掩码：低 32 = 每块非零，高 32 = 有 AC。
/// 每宏块一次 Load/Store（对照 C# `ParseResiduals`）。函数边界用 `&mut` + 熵态局部标量拷贝：
/// byval 结构体参数在 Win64 ABI 会逐位回写内存（asm 实证 24/32/36(%ptr) 每 bit store），
/// `&mut` 只在进出各一次 {data,pos,value,bits,range}；内部 tokens 内联后全为 SSA 寄存器。
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
    let mut st = *d;
    let mut above = *above_nz;
    let mut left = *left_nz;
    let mut masks = 0u64;
    let mut block = if i16 { 24usize } else { 0usize };
    let (luma_type, luma_start, luma_dc) = if i16 {
        (0usize, 1usize, 1i32)
    } else {
        (3usize, 0usize, sp.y_dc)
    };
    loop {
        let (ty, start, dc, ac) = if block < 16 {
            (luma_type, luma_start, luma_dc, sp.y_ac)
        } else if block < 24 {
            (2usize, 0usize, sp.uv_dc, sp.uv_ac)
        } else {
            (1usize, 0usize, sp.y2_dc, sp.y2_ac)
        };
        let above_bit = BLOCK_ABOVE_CONTEXT_BIT[block];
        let left_bit = BLOCK_LEFT_CONTEXT_BIT[block];
        let context = (((above >> above_bit) & 1) + ((left >> left_bit) & 1)) as usize;
        let (s2, last) = tokens(
            st,
            &probabilities[ty * 8 * 33..],
            context,
            start,
            &mut mb[block],
            dc,
            ac,
        );
        st = s2;
        let nonzero = if last > 0 { 1u32 } else { 0u32 };
        above = (above & !(1 << above_bit)) | (nonzero << above_bit);
        left = (left & !(1 << left_bit)) | (nonzero << left_bit);
        masks |= u64::from(nonzero) << block;
        if last > 1 {
            masks |= 1u64 << (block + 32);
        }
        if block == 24 {
            block = 0;
        } else {
            block += 1;
            if block == 24 {
                break;
            }
        }
    }
    *d = st;
    *above_nz = above;
    *left_nz = left;
    masks
}

/// 树遍历：node + read_bit_val(prob[node>>1]) 沿树下行，叶子（≤0）返回 -child。熵态按值携带。
#[inline(always)]
fn read_tree<'a>(
    mut d: Vp8BoolDecoder<'a>,
    tree: &[i8],
    probabilities: &[u8],
) -> (Vp8BoolDecoder<'a>, u32) {
    let mut node = 0usize;
    loop {
        let (dd, bit) = d.read_bit_val(probabilities[node >> 1]);
        d = dd;
        let child = tree[node + bit as usize];
        if child <= 0 {
            return (d, -child as u32);
        }
        node = child as usize;
    }
}

/// RFC 6386 §11.5 宏块模式（segment / skip / ymode / 16 个 bmode（仅 B_PRED） / uvmode）。
/// 每宏块一次 Load/Store（对照 C# `ReadMacroBlockModes`）。函数边界 `&mut` + 熵态局部拷贝，
/// 与 parse_residuals 同理（byval 会逐位回写内存，本形态保寄存器驻留）。
#[inline(never)]
fn read_macroblock_modes(
    header: &mut Vp8BoolDecoder<'_>,
    segment_probabilities: Option<&[u8; 3]>,
    skip_probability: i32,
    above_modes: &mut [u8; 4],
    left_modes: &mut [u8; 4],
    block_modes: &mut [u8; 16],
) -> u32 {
    let mut st = *header;
    let segment = match segment_probabilities {
        Some(probs) => {
            let (s, v) = read_tree(st, &SEGMENT_ID_TREE, probs);
            st = s;
            v
        }
        None => 0,
    };
    let skip = if skip_probability < 0 {
        0
    } else {
        let (s, b) = st.read_bit_val(skip_probability as u8);
        st = s;
        b
    };
    let (s, y_mode) = read_tree(st, &KEYFRAME_Y_MODE_TREE, &KEYFRAME_Y_MODE_PROBS);
    st = s;
    if y_mode == 4 {
        for i in 0..16 {
            let above = if i < 4 { above_modes[i] } else { block_modes[i - 4] };
            let leftv = if (i & 3) == 0 {
                left_modes[i >> 2]
            } else {
                block_modes[i - 1]
            };
            let p = &KEYFRAME_BLOCK_MODE_PROBS[(usize::from(above) * 10 + usize::from(leftv)) * 9..]
                [..9];
            let mut node = 0usize;
            loop {
                let (s, bit) = st.read_bit_val(p[node >> 1]);
                st = s;
                let child = BLOCK_MODE_TREE[node + bit as usize];
                if child <= 0 {
                    block_modes[i] = -child as u8;
                    break;
                }
                node = child as usize;
            }
        }
        for i in 0..4 {
            above_modes[i] = block_modes[12 + i];
            left_modes[i] = block_modes[i * 4 + 3];
        }
    } else {
        let derived = match y_mode {
            0 => 0u8,
            1 => 2,
            2 => 3,
            _ => 1,
        };
        for m in above_modes.iter_mut() {
            *m = derived;
        }
        for m in left_modes.iter_mut() {
            *m = derived;
        }
    }
    let (s, uv_mode) = read_tree(st, &UV_MODE_TREE, &KEYFRAME_UV_MODE_PROBS);
    *header = s;
    segment | (skip << 2) | (y_mode << 3) | (uv_mode << 6)
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
    width: usize,
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
    if right + 4 <= width {
        for i in 0..4 {
            above_y[17 + i] = y[yl + 1 + right + i];
        }
    } else {
        for i in 0..4 {
            above_y[17 + i] = y[yl + 1 + (right + i).min(width - 1)];
        }
    }
}

/// 分阶段计时（性能探针用）：`#[cfg(not(test))]` 下为 `f()` 直通（零开销、不手写重复），
/// `#[cfg(test)]` 下累计各阶段耗时（只读不改写，字节不受影响）。探针 `lossy_stage_split_big`
/// 在 release 测试二进制里跑真墙钟，输出熵/预测/变换/滤波各占比以定热点投入方向（K53）。
pub(crate) mod stage_clock {
    #[cfg(test)]
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
    #[cfg(test)]
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

    #[cfg(test)]
    #[inline]
    pub fn entropy<R>(f: impl FnOnce() -> R) -> R {
        if !ON.load(Relaxed) {
            return f();
        }
        let t = Instant::now();
        let r = f();
        ENTROPY.fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
        r
    }
    #[cfg(test)]
    #[inline]
    pub fn predict<R>(f: impl FnOnce() -> R) -> R {
        if !ON.load(Relaxed) {
            return f();
        }
        let t = Instant::now();
        let r = f();
        PREDICT.fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
        r
    }
    #[cfg(test)]
    #[inline]
    pub fn idct<R>(f: impl FnOnce() -> R) -> R {
        if !ON.load(Relaxed) {
            return f();
        }
        let t = Instant::now();
        let r = f();
        IDCT.fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
        r
    }
    #[cfg(test)]
    #[inline]
    pub fn filter<R>(f: impl FnOnce() -> R) -> R {
        if !ON.load(Relaxed) {
            return f();
        }
        let t = Instant::now();
        let r = f();
        FILTER.fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
        r
    }

    #[cfg(not(test))]
    #[inline(always)]
    pub fn entropy<R>(f: impl FnOnce() -> R) -> R {
        f()
    }
    #[cfg(not(test))]
    #[inline(always)]
    pub fn predict<R>(f: impl FnOnce() -> R) -> R {
        f()
    }
    #[cfg(not(test))]
    #[inline(always)]
    pub fn idct<R>(f: impl FnOnce() -> R) -> R {
        f()
    }
    #[cfg(not(test))]
    #[inline(always)]
    pub fn filter<R>(f: impl FnOnce() -> R) -> R {
        f()
    }
}

/// 完整解码：解析帧头 + 重建全部宏块 → 紧密 YUV 4:2:0（V2 暂不含环路滤波，V3 接入）。
pub(crate) fn decode_vp8(frame: &[u8], width: u32, height: u32) -> Result<Vp8Yuv> {
    let fh = parse_frame_header(frame)?;
    if fh.width != width || fh.height != height {
        return Err(Vp8Error::new("VP8 与容器尺寸不一致"));
    }
    decode_from_header(fh)
}

fn decode_from_header(fh: FrameHeader<'_>) -> Result<Vp8Yuv> {
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

    let mut y = vec![0u8; ch * y_pitch];
    let mut u = vec![0u8; uh * uv_pitch];
    let mut v = vec![0u8; uh * uv_pitch];
    for r in 0..ch {
        y[r * y_pitch] = 129;
    }
    for r in 0..uh {
        u[r * uv_pitch] = 129;
        v[r * uv_pitch] = 129;
    }

    let mut header = fh.header;
    let partition_count = fh.partitions.len();
    let mut token_decoders: Vec<Vp8BoolDecoder<'_>> =
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
    let mut b_modes = [0u8; 16];
    let mut mb = [[0i16; 16]; 25];
    let mut above_y = [127u8; 21];
    let mut above_u = [127u8; 9];
    let mut above_v = [127u8; 9];

    // 每行宏块的 (滤波等级, 是否滤内部边)，供 V3。
    let mut row_filters = vec![(0u8, 0u8); mbw * mbh];

    // 滤波线程共享视图的裸指针（重建闭包借用前取；y/u/v/row_filters 长度固定不扩容，
    // 解码全程不 reallocate，指针始终有效）。P-B 管道语义见 `filter_pipeline_thread`。
    let (y_ptr, u_ptr, v_ptr) = (y.as_mut_ptr(), u.as_mut_ptr(), v.as_mut_ptr());
    let (y_len, uv_len) = (y.len(), u.len()); // u 与 v 等长
    let rf_ptr = row_filters.as_mut_ptr();
    let rf_len = row_filters.len();

    // P-B 真重叠管道（libwebp `-mt` 式：重建主线程逐行产出、滤波线程滞后 1 行消费）vs 串行
    // （重建毕→整帧滤波）。两种路径输出逐字节一致（硬闸 `threads2_matches_serial_byte_exact`
    // 与集成 `static_lossy_threads2_matches_serial_byte_exact`）。
    let pipelined = filter_mt_enabled(mbw, mbh);
    let mut reconstruct_row = |mby: usize| {
        // 每宏块行重置 4 个左邻 b 模式（C# `leftB.Clear()` 同款）。
        let mut left_b = [0u8; 4];
        let mut left_nz = 0u32;
        // 行内复用该分区的 token 解码器（&mut 边界，熵态在宏块级函数内部寄存器驻留）。
        let td = &mut token_decoders[mby & (partition_count - 1)];
        for mbx in 0..mbw {
            // 阶段一：熵解码。探针计时关闭时为零开销直通（字节不变）。
            let (y_mode, uv_mode, i16, sp, masks) = stage_clock::entropy(|| {
                let modes = {
                    let above_m: &mut [u8; 4] = (&mut above_b[mbx * 4..mbx * 4 + 4]).try_into().unwrap();
                    read_macroblock_modes(
                        &mut header,
                        segment_probs,
                        skip_probability,
                        above_m,
                        &mut left_b,
                        &mut b_modes,
                    )
                };
                let segment = (modes & 3) as usize;
                let skip = (modes & 4) != 0;
                let y_mode = ((modes >> 3) & 7) as u8;
                let uv_mode = (modes >> 6) as u8;
                let i16 = y_mode != 4;
                let sp = seg_params[segment];
                let masks = if skip {
                    let keep = if i16 { 0 } else { NZ_Y2 };
                    above_nz[mbx] &= keep;
                    left_nz &= keep;
                    0u64
                } else {
                    parse_residuals(td, &probs, &mut mb, &sp, i16, &mut above_nz[mbx], &mut left_nz)
                };
                (y_mode, uv_mode, i16, sp, masks)
            });
            let nonzero_mask = masks as u32;
            let ac_mask = (masks >> 32) as u32;
            let (lvl, inner) = {
                let l = if i16 { sp.filter_level_i16 } else { sp.filter_level_i4 };
                (l, if nonzero_mask == 0 && i16 { 0u8 } else { 1u8 })
            };
            row_filters[mby * mbw + mbx] = (lvl, inner);

            let dy = mby * 16;
            let dx = mbx * 16;
            let dyu = mby * 8;
            let dxu = mbx * 8;

            // 阶段二：帧内预测。
            stage_clock::predict(|| {
                prepare_above(
                    &mut above_y, &mut above_u, &mut above_v,
                    &y, &u, &v, y_pitch, uv_pitch, mbx, mby, width,
                );
                if i16 {
                    super::simd::predict_large(&mut y, y_pitch, dy, dx, 16, y_mode, mby > 0, mbx > 0, &above_y[..17]);
                }
                super::simd::predict_large(&mut u, uv_pitch, dyu, dxu, 8, uv_mode, mby > 0, mbx > 0, &above_u[..9]);
                super::simd::predict_large(&mut v, uv_pitch, dyu, dxu, 8, uv_mode, mby > 0, mbx > 0, &above_v[..9]);
            });

            // 阶段三：逆变换 + 残差写回（亮块成对批处理；色度暂逐块隔离验证）。
            stage_clock::idct(|| {
                if i16 && (nonzero_mask & NZ_Y2) != 0 {
                    let y2 = mb[24];
                    inverse_walsh_to_blocks(&y2, &mut mb, (ac_mask & NZ_Y2) != 0);
                    mb[24].fill(0);
                }

                if i16 {
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
                                let ca = mb[a];
                                let cb = mb[b];
                                super::simd::inverse_dct_add_two(
                                    &ca, &cb, &mut y, y_pitch, row0, dx + pair_base * 4,
                                );
                                mb[a].fill(0);
                                mb[b].fill(0);
                            }
                        }
                    }
                } else {
                    for block in 0..16 {
                        let bx = block & 3;
                        let by = block >> 2;
                        let row0 = dy + by * 4;
                        let col0 = dx + bx * 4;
                        let above = subblock_above(&y, y_pitch, row0, col0, bx, by, &above_y);
                        super::simd::predict_subblock(&mut y, y_pitch, row0, col0, b_modes[block], &above);
                        let coeff = mb[block];
                        if ((nonzero_mask >> block) & 1) != 0 || coeff[0] != 0 {
                            inverse_dct_add(&coeff, &mut y, y_pitch, row0, col0, ((ac_mask >> block) & 1) != 0);
                            mb[block].fill(0);
                        }
                    }
                }

                if (nonzero_mask & 0xff0000) != 0 {
                    // 色度 8 块 = 4 个水平对（V 上原 brow = (block>>1)&1 循环 0,1,0,1）：
                    // 对首块号 a ∈ {16,18,20,22}，行偏移 dyu + ((a>>1)&1)*4，列恒为 dxu
                    // （对内第二个块 +4 由 inverse_dct_add_two 处理）；a<20 走 u，否则走 v。
                    for a in [16usize, 18, 20, 22] {
                        let b = a + 1;
                        let row0 = dyu + ((a >> 1) & 1) * 4;
                        let ca = mb[a];
                        let cb = mb[b];
                        let nz_a = ((nonzero_mask >> a) & 1) != 0 || ca[0] != 0;
                        let nz_b = ((nonzero_mask >> b) & 1) != 0 || cb[0] != 0;
                        if !(nz_a || nz_b) {
                            continue;
                        }
                        if a < 20 {
                            super::simd::inverse_dct_add_two(&ca, &cb, &mut u, uv_pitch, row0, dxu);
                        } else {
                            super::simd::inverse_dct_add_two(&ca, &cb, &mut v, uv_pitch, row0, dxu);
                        }
                        mb[a].fill(0);
                        mb[b].fill(0);
                    }
                }
            });
        }
    };

    // P-B 管道：主线程重建，滤波线程并行消费（滞后 1 行）。
    if pipelined {
        std::thread::scope(|s| {
            let (tx, rx) = std::sync::mpsc::channel::<usize>();
            let filter_cfg = &fh.filter;
            let args = FilterThreadArgs {
                y_ptr,
                y_len,
                y_pitch,
                u_ptr,
                v_ptr,
                uv_pitch,
                uv_len,
                rf_ptr,
                rf_len,
                mbw,
                mbh,
            };
            s.spawn(move || filter_pipeline_thread(rx, args, filter_cfg));
            for mby in 0..mbh {
                reconstruct_row(mby);
                let _ = tx.send(mby);
            }
            // 全部行重建完成 → 滤波线程滤末行后退出（末行 m=mbh-1 的滞后=1 由 k==mbh 满足）。
            let _ = tx.send(mbh);
        });

    } else {
        for mby in 0..mbh {
            reconstruct_row(mby);
        }

        // 整帧重建后按宏块行做环路滤波（等价 C# 逐行滤波：预测读未滤波行、滤波读已重建行，顺序一致）。
        apply_loop_filter(
            &mut y, y_pitch, &mut u, &mut v, uv_pitch, mbw, mbh, &fh.filter, &row_filters,
        );
    }

    // 抽紧密平面（真实宽高）。
    let cwu = (width + 1) / 2;
    let chu = (height + 1) / 2;
    let mut out_y = vec![0u8; width * height];
    for r in 0..height {
        out_y[r * width..(r + 1) * width]
            .copy_from_slice(&y[r * y_pitch + 1..r * y_pitch + 1 + width]);
    }
    let mut out_u = vec![0u8; cwu * chu];
    let mut out_v = vec![0u8; cwu * chu];
    for r in 0..chu {
        out_u[r * cwu..(r + 1) * cwu].copy_from_slice(&u[r * uv_pitch + 1..r * uv_pitch + 1 + cwu]);
        out_v[r * cwu..(r + 1) * cwu].copy_from_slice(&v[r * uv_pitch + 1..r * uv_pitch + 1 + cwu]);
    }

    Ok(Vp8Yuv {
        width: fh.width,
        height: fh.height,
        y: out_y,
        u: out_u,
        v: out_v,
    })
}

// ---------------------------------------------------------------------------
// V3：环路滤波（RFC 6386 §15，标量；C# `Vp8LoopFilter.cs`/标量 FilterEdge 同款算术）
// ---------------------------------------------------------------------------

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
/// 本函数是环路滤波的**串行语义源**：双线程分带只调度它，行内/宏块内/边顺序与算术完全不变，
/// 故任一“带”的输出与串行路径逐字节一致（硬闸：`threads2_matches_serial_byte_exact`）。
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
    for mbx in 0..mbw {
        let (level, inner) = row_filters[mby * mbw + mbx];
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
}

/// 环路滤波双线程门控（auto 宏块总数下限）。`mbw*mbh >= MT_MIN_MB_TOTAL`（≈1MP，如 1024×1024、
/// 2400×3600=33750 MB）才开双带并行；窄/小图线程开销>收益，保持串行（复用滤波「向量化负收益
/// 即回退」的教训；收益实测见探针 `lossy_mt_probe` 与 `WEBP_MT`）。
const MT_MIN_MB_TOTAL: usize = 4096;

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

/// 串行环路滤波：整帧重建完成后逐宏块行滤波（环路滤波的**语义源**；P-B 滤波线程逐行调用
/// `filter_macroblock_row`，与其输出逐字节一致——见 `threads2_matches_serial_byte_exact`）。
fn apply_loop_filter(
    y: &mut [u8],
    y_pitch: usize,
    u: &mut [u8],
    v: &mut [u8],
    uv_pitch: usize,
    mbw: usize,
    mbh: usize,
    filter: &FilterSettings,
    row_filters: &[(u8, u8)],
) {
    for mby in 0..mbh {
        filter_macroblock_row(y, y_pitch, u, v, uv_pitch, mbw, mby, filter, row_filters);
    }
}

/// P-B 真重叠管道：滤波线程主体（libwebp `-mt` 式「重建线程逐行产出、滤波线程滞后 N 行消费」，
/// 由 `decode_from_header` 在宽图/强制 `2` 时启用）。
///
/// 滞后 1 行的字节一致论证（重建主线程与滤波线程旁路并行）：
/// - 行 `m` 的环滤只读写像素行区间 `[m*16-4, m*16+15]`（H_top 读至 m*16-4、写至 m*16-3；
///   V/H_inner 均在 `[m*16, m*16+15]`）→ 滤行 m 最多写到 `(m+1)*16-1`。
/// - 重建线程对行 r 的**读**（帧内预测 `prepare_above` 读上一行）结束于重建完 r+1；此后重建
///   只写 `(r+2)*16` 起的行。本线程滤行 m 前先收到「行 k=m+1 重建完成」（滞后 1）→ 滤波
///   写集 `≤(m+1)*16-1` 与重建写集 `≥(m+2)*16` 恒不相交；滤波读集也在行 m+1 定稿之后 →
///   无 C11 数据竞争，且每行滤波仍在与其他行完全相同的已发布值上、同串行顺序执行 →
///   输出与串行 `apply_loop_filter` 逐字节一致（硬闸测试复核）。
///
/// 信号：`rx` 收到 `k`（0..=mbh-1 表示行 k 已重建完成；`k == mbh` 表示全部完成）。
/// 共享平面经裸指针传入（y/u/v/row_filters 长度固定不扩容；借用隔离由上述互斥时间窗 +
/// mpsc happens-before 保证，思路同 pngsimd pipeline `InflateShared` 契约）。
struct FilterThreadArgs {
    y_ptr: *mut u8,
    y_len: usize,
    y_pitch: usize,
    u_ptr: *mut u8,
    v_ptr: *mut u8,
    uv_pitch: usize,
    uv_len: usize,
    rf_ptr: *mut (u8, u8),
    rf_len: usize,
    mbw: usize,
    mbh: usize,
}

// SAFETY：只携带裸指针与整数，不携带任何 Rust 引用；共享内存的借用隔离由
// `filter_pipeline_thread` 的滞后时间窗契约（mpsc happens-before）保证，见其上注释。
unsafe impl Send for FilterThreadArgs {}

#[allow(clippy::too_many_arguments)]
fn filter_pipeline_thread(
    rx: std::sync::mpsc::Receiver<usize>,
    args: FilterThreadArgs,
    filter: &FilterSettings,
) {
    let FilterThreadArgs {
        y_ptr,
        y_len,
        y_pitch,
        u_ptr,
        v_ptr,
        uv_pitch,
        uv_len,
        rf_ptr,
        rf_len,
        mbw,
        mbh,
    } = args;
    let mut next = 0usize;
    let mut avail: Option<usize> = None;
    loop {
        if next >= mbh {
            break;
        }
        let need = next + 1; // 滞后 1：滤行 next 需行 next+1 重建完成
        while avail.map_or(true, |k| k < need) {
            match rx.recv() {
                Ok(k) => avail = Some(k),
                Err(_) => {
                    // 通道关闭：主线程已结束（正常收尾经 k==mbh 表达；异常由主线程 panic 接管）。
                    return;
                }
            }
        }
        // SAFETY：滞后时间窗契约（上文）——此刻滤波读写集与重建写集不相交，相关行已发布定稿。
        let y = unsafe { std::slice::from_raw_parts_mut(y_ptr, y_len) };
        let u = unsafe { std::slice::from_raw_parts_mut(u_ptr, uv_len) };
        let v = unsafe { std::slice::from_raw_parts_mut(v_ptr, uv_len) };
        let rf = unsafe { std::slice::from_raw_parts_mut(rf_ptr, rf_len) };
        filter_macroblock_row(y, y_pitch, u, v, uv_pitch, mbw, next, filter, rf);
        next += 1;
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

/// YUV 4:2:0 → RGBA8（fancy 上采样默认，等价 image-webp Bilinear / dwebp fancy 列）。alpha 恒 255。
pub(crate) fn yuv_to_rgba_fancy(yuv: &Vp8Yuv) -> Vec<u8> {
    let w = yuv.width as usize;
    let h = yuv.height as usize;
    let cw = (w + 1) >> 1;
    let ch = (h + 1) >> 1;
    let mut rgba = vec![0u8; w * h * 4];
    let mut u_up = vec![0u8; w];
    let mut v_up = vec![0u8; w];
    // 复用上采样工作区（t 行缓冲），避免逐行分配。
    let mut t_ws = vec![0u16; cw + 8];
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
        super::simd::upsample_fancy_into(u_row, u_adj, &mut u_up, &mut t_ws);
        super::simd::upsample_fancy_into(v_row, v_adj, &mut v_up, &mut t_ws);
        let y_row = &yuv.y[row * w..(row + 1) * w];
        let out = &mut rgba[row * w * 4..(row + 1) * w * 4];
        super::simd::yuv_row_to_rgba(y_row, &u_up, &v_up, out);
    }
    rgba
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
        let yuv = decode_vp8(c.bitstream.unwrap(), c.width, c.height).ok()?;
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
            let yuv = decode_vp8(c.bitstream.unwrap(), c.width, c.height)
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
                let yuv = decode_vp8(c.bitstream?, c.width, c.height).ok()?;
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
    use crate::codecs::webp::{BACKEND_AUTO, BACKEND_IMAGEWEBP, BACKEND_OURS, WebPDecoder, set_webp_backend};
    use crate::ImageDecoder;

    fn imagewebp_rgba(data: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
        set_webp_backend(BACKEND_IMAGEWEBP);
        let dec = WebPDecoder::new(Cursor::new(data)).ok()?;
        let (w, h) = dec.dimensions();
        let has_alpha = dec.color_type() == crate::ColorType::Rgba8;
        let mut buf = vec![0u8; dec.total_bytes() as usize];
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
        Some((ours.width, ours.height, ours.rgba))
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

    /// 冒烟：经 `WebPDecoder`（OURS 后端）解码同 fixture 与直接调用一致，且格式为 Rgb8。
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
        assert_eq!(dec.color_type(), crate::ColorType::Rgb8);
        let mut buf = vec![0u8; dec.total_bytes() as usize];
        dec.read_image(&mut buf).unwrap();
        assert_eq!(buf.len(), (w * h * 3) as usize);
        set_webp_backend(BACKEND_IMAGEWEBP);
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
                let yuv = decode_vp8(c.bitstream.unwrap(), c.width, c.height).unwrap();
                let t1 = Instant::now();
                let rgba = yuv_to_rgba_fancy(&yuv);
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
            let yuv = decode_vp8(c.bitstream.unwrap(), w, h).unwrap();
            let t1 = Instant::now();
            let _rgba = yuv_to_rgba_fancy(&yuv);
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

    mod helper {
        use super::*;
        /// 解 testsrc2-320x240-q75（全链 YUV+RGBA），返回 (y, u, v, rgba)；夹具缺失返回 None。
        pub fn decode_testsrc2() -> Option<(Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>)> {
            let p = webp_fixtures_support::webp_fixture_path("testsrc2-320x240-q75.webp")?;
            let data = std::fs::read(p).expect("read fixture");
            let c = super::super::super::container::parse(&data).expect("容器解析");
            let yuv = decode_vp8(c.bitstream.unwrap(), c.width, c.height).expect("VP8 解码");
            let rgba = yuv_to_rgba_fancy(&yuv);
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
                            let yuv = decode_vp8(c.bitstream.unwrap(), w, h).unwrap();
                            let _rgba = yuv_to_rgba_fancy(&yuv);
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
        let yuv = decode_vp8(c.bitstream?, c.width, c.height).ok()?;
        let rgba = yuv_to_rgba_fancy(&yuv);
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
}
