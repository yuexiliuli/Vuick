//! CABAC 算术解码引擎与上下文模型（H.265 §9.3.2、§9.3.4.3）。
//! 逐函数移植自 C# `Hevc\HevcCabac.cs`（类 `HevcCabac` 与静态表类 `HevcCabacTables`），
//! 与 C# 字节恒等（表逐元素一致、位移/溢出按 C# 语义 wrapping 处理）。
//!
//! 引擎从去转义后的 RBSP（见 [`crate::codecs::heif::hevc::nal::Nal`]）按位位置直接读 bin，
//! 与 §9.3.4.3.2–9.3.4.3.5 一致：重归一化里的 `read_bits(1)`、初始化时的 9 位 `ivlOffset`、
//! 以及 [`HevcCabac::decode_bypass`] 的字节对齐旁路。位位置经 [`HevcCabac::bit_position`]
//! 暴露，供 WPP 波前入口使用（每行的子流从切片头的 entry_point_offset_minus1 给出的字节偏移开始）。
//!
//! 上下文每项一个字节：高位 6 位是 `pStateIdx`（0..63），最低位是 `valMps`。
//! [`HevcCabacTables::initialize`] 按 §9.3.2.2 用 Table 9-5..9-37 的 `initValue` 推导。

use crate::codecs::heif::error::{HeifError, HeifResult};

/// CABAC 上下文索引常量与规范查表（与 C# `HevcCabacTables` 逐一对应）。
pub struct HevcCabacTables;

impl HevcCabacTables {
    pub const SaoMerge: usize = 0; // 9-5
    pub const SaoType: usize = 1; // 9-6
    pub const SplitCu: usize = 2; // 9-7, ctxInc 0..2
    pub const TransquantBypass: usize = 5; // 9-8
    pub const PartMode: usize = 6; // 9-11, intra 只用 bin 0
    pub const PrevIntraLuma: usize = 7; // 9-12
    pub const IntraChroma: usize = 8; // 9-13
    pub const SplitTransform: usize = 9; // 9-20, ctxInc 5 - log2TrafoSize (0..2)
    pub const CbfLuma: usize = 12; // 9-21, ctxInc 0..1
    pub const CbfChroma: usize = 14; // 9-22, ctxInc trafoDepth 0..4
    pub const CuQpDeltaAbs: usize = 19; // 9-24, ctxInc 0..1
    pub const TransformSkip: usize = 21; // 9-25, luma 0, chroma 1
    pub const LastX: usize = 23; // 9-26, 18
    pub const LastY: usize = 41; // 9-27, 18
    pub const CodedSubBlock: usize = 59; // 9-28, 4
    pub const Sig: usize = 63; // 9-29, 44
    pub const Greater1: usize = 107; // 9-30, 24
    pub const Greater2: usize = 131; // 9-31, 6
    pub const Count: usize = 137;

    /// Table 9-52：`rangeTabLps[pStateIdx * 4 + qRangeIdx]`。
    pub const RangeTabLps: [u8; 256] = [
        128, 176, 208, 240,
        128, 167, 197, 227,
        128, 158, 187, 216,
        123, 150, 178, 205,
        116, 142, 169, 195,
        111, 135, 160, 185,
        105, 128, 152, 175,
        100, 122, 144, 166,
        95, 116, 137, 158,
        90, 110, 130, 150,
        85, 104, 123, 142,
        81, 99, 117, 135,
        77, 94, 111, 128,
        73, 89, 105, 122,
        69, 85, 100, 116,
        66, 80, 95, 110,
        62, 76, 90, 104,
        59, 72, 86, 99,
        56, 69, 81, 94,
        53, 65, 77, 89,
        51, 62, 73, 85,
        48, 59, 69, 80,
        46, 56, 66, 76,
        43, 53, 63, 72,
        41, 50, 59, 69,
        39, 48, 56, 65,
        37, 45, 54, 62,
        35, 43, 51, 59,
        33, 41, 48, 56,
        32, 39, 46, 53,
        30, 37, 43, 50,
        29, 35, 41, 48,
        27, 33, 39, 45,
        26, 31, 37, 43,
        24, 30, 35, 41,
        23, 28, 33, 39,
        22, 27, 32, 37,
        21, 26, 30, 35,
        20, 24, 29, 33,
        19, 23, 27, 31,
        18, 22, 26, 30,
        17, 21, 25, 28,
        16, 20, 23, 27,
        15, 19, 22, 25,
        14, 18, 21, 24,
        14, 17, 20, 23,
        13, 16, 19, 22,
        12, 15, 18, 21,
        12, 14, 17, 20,
        11, 14, 16, 19,
        11, 13, 15, 18,
        10, 12, 15, 17,
        10, 12, 14, 16,
        9, 11, 13, 15,
        9, 11, 12, 14,
        8, 10, 12, 14,
        8, 9, 11, 13,
        7, 9, 11, 12,
        7, 9, 10, 12,
        7, 8, 10, 11,
        6, 8, 9, 11,
        6, 7, 9, 10,
        6, 7, 8, 9,
        2, 2, 2, 2,
    ];

    /// Table 9-53：`transIdxLps`。
    pub const TransIdxLps: [u8; 64] = [
        0, 0, 1, 2, 2, 4, 4, 5, 6, 7, 8, 9, 9, 11, 11, 12,
        13, 13, 15, 15, 16, 16, 18, 18, 19, 19, 21, 21, 22, 22, 23, 24,
        24, 25, 26, 26, 27, 27, 28, 29, 29, 30, 30, 30, 31, 32, 32, 33,
        33, 33, 34, 34, 35, 35, 35, 36, 36, 36, 37, 37, 37, 38, 38, 63,
    ];

    /// §9.3.4.3.2.2 在打包上下文字节 `pStateIdx << 1 | valMps` 上的状态转移：
    /// MPS 后 pStateIdx 走 transIdxMps（+1 直到 62）；LPS 后走 transIdxLps，且 pStateIdx 为 0 时 valMps 翻转。
    pub const TransMps: [u8; 128] = build_transitions(true);
    /// 见 [`HevcCabacTables::TransMps`]。
    pub const TransLps: [u8; 128] = build_transitions(false);

    /// I 片布局（initType 0）下每个上下文的 `initValue`（Table 9-4 选出的 9-5..9-31 的 initType-0 列）。
    pub const InitValue: [u8; 137] = [
        153,                                    // sao_merge_left/up_flag
        200,                                    // sao_type_idx
        139, 141, 157,                          // split_cu_flag
        154,                                    // cu_transquant_bypass_flag
        184,                                    // part_mode
        184,                                    // prev_intra_luma_pred_flag
        63,                                     // intra_chroma_pred_mode
        153, 138, 138,                          // split_transform_flag
        111, 141,                               // cbf_luma
        94, 138, 182, 154, 154,                 // cbf_cb / cbf_cr（ctxIdx 0..3, 12）
        154, 154,                               // cu_qp_delta_abs
        139, 139,                               // transform_skip_flag luma (0), chroma (3)
        110, 110, 124, 125, 140, 153, 125, 127, 140, 109, 111, 143, 127, 111, 79, 108, 123, 63, // last_sig_coeff_x_prefix
        110, 110, 124, 125, 140, 153, 125, 127, 140, 109, 111, 143, 127, 111, 79, 108, 123, 63, // last_sig_coeff_y_prefix
        91, 171, 134, 141,                      // coded_sub_block_flag
        111, 111, 125, 110, 110, 94, 124, 108, 124, 107, 125, 141, 179, 153, 125, 107, // sig_coeff_flag 0..15
        125, 141, 179, 153, 125, 107, 125, 141, 179, 153, 125, 140, 139, 182, 182, 152, // 16..31
        136, 152, 136, 153, 136, 139, 111, 136, 139, 111,                            // 32..41
        141, 111,                                                                     // 126..127
        140, 92, 137, 138, 140, 152, 138, 139, 153, 74, 149, 92, 139, 107, 122, 152,  // coeff_abs_level_greater1_flag
        140, 179, 166, 182, 140, 227, 122, 197,
        138, 153, 136, 167, 152, 152,           // coeff_abs_level_greater2_flag
    ];

    /// §9.3.2.2：一个 `initValue` 条目的打包 `pStateIdx << 1 | valMps`。
    pub fn initialize(init_value: i32, clipped_slice_qp_y: i32) -> i32 {
        let slope_idx = init_value >> 4;
        let offset_idx = init_value & 15;
        let m = slope_idx * 5 - 45;
        let n = (offset_idx << 3) - 16;
        let pre_ctx_state = (((m * clipped_slice_qp_y) >> 4) + n).clamp(1, 126);
        let val_mps = if pre_ctx_state <= 63 { 0 } else { 1 };
        let p_state_idx = if val_mps != 0 { pre_ctx_state - 64 } else { 63 - pre_ctx_state };
        (p_state_idx << 1) | val_mps
    }
}

/// 生成 128 项的状态转移表（MPS/LPS）。
const fn build_transitions(mps: bool) -> [u8; 128] {
    let mut table = [0u8; 128];
    let mut state = 0usize;
    while state < 128 {
        let state_idx = state >> 1;
        let mut val_mps = (state & 1) as u8;
        let next: usize = if mps {
            if state_idx >= 62 {
                state_idx
            } else {
                state_idx + 1
            }
        } else {
            let n = HevcCabacTables::TransIdxLps[state_idx] as usize;
            if state_idx == 0 {
                val_mps ^= 1;
            }
            n
        };
        table[state] = ((next as u8) << 1) | val_mps;
        state += 1;
    }
    table
}

/// CABAC 算术解码引擎（C# `HevcCabac`）。
pub struct HevcCabac<'a> {
    input: &'a [u8],
    ctx: [u8; HevcCabacTables::Count],
    range: u32,
    // 标准的 9 位 ivlOffset 后面跟着 _bits 位前瞻位：_value = ivlOffset·2^_bits + 流接下来的 _bits 位。
    // 拿 ivlOffset 与 ivlCurrRange 相比就是拿 _value 与 ivlCurrRange·2^_bits 相比，
    // 相减就是在减它，重归一化里的 read_bits(1) 就是 _bits − 1，
    // 因此引擎算的正好是标准的 ivlOffset，而每几个字节才碰一次内存。
    value: u64,
    bits: i32,
    /// `_input` 的下一个待加载字节下标；超出末尾的字节读作 0。
    next: usize,
}

impl<'a> HevcCabac<'a> {
    /// 在切片的去转义 RBSP 上、从 `byte_offset` 开始初始化引擎。
    pub fn new(rbsp: &'a [u8], byte_offset: usize) -> HevcCabac<'a> {
        let mut engine = HevcCabac {
            input: rbsp,
            ctx: [0; HevcCabacTables::Count],
            range: 0,
            value: 0,
            bits: 0,
            next: 0,
        };
        engine.init_engine(byte_offset);
        engine
    }

    /// 切片数据的当前位位置：引擎读过的位数（§9.3.2.6 的语义）。
    pub fn bit_position(&self) -> i64 {
        self.next as i64 * 8 - self.bits as i64
    }

    /// 在 `byte_offset` 重启算术引擎（§9.3.2.6）。
    pub fn init_engine(&mut self, byte_offset: usize) {
        // 9.3.2.6：ivlCurrRange = 510，ivlOffset = read_bits(9)。
        self.range = 510;
        self.next = byte_offset;
        self.value = 0;
        self.bits = -9;
        self.refill();
    }

    /// 一次装载整字节直至缓冲 47..54 位前瞻（ivlOffset 的 9 位在上面，能塞进 64 位）。
    fn refill(&mut self) {
        // C# count = (54 - _bits) >> 3 —— C# 位移越界是 mod-位数，此处用 wrapping_shl/shr 保恒等。
        let count = (54 - self.bits) >> 3;
        let next = self.next;
        if self.input.len() >= 8 && next <= self.input.len() - 8 {
            let mut word_bytes = [0u8; 8];
            word_bytes.copy_from_slice(&self.input[next..next + 8]);
            let word = u64::from_be_bytes(word_bytes);
            self.value = self
                .value
                .wrapping_shl((count * 8) as u32)
                | word.wrapping_shr((64 - count * 8) as u32);
        } else {
            for i in 0..count {
                let at = next as i64 + i as i64;
                let byte = if (at as u64) < self.input.len() as u64 {
                    self.input[at as usize]
                } else {
                    0
                };
                self.value = self.value.wrapping_shl(8) | u64::from(byte);
            }
        }
        self.next = (next as i64 + count as i64).max(0) as usize;
        self.bits += count * 8;
    }

    /// 为当前切片初始化上下文变量（§9.3.2.2）。`init_type` 是 0 表示 I 片，本解码器只处理 I 片。
    pub fn init_contexts(&mut self, slice_qp_y: i32, init_type: i32) -> HeifResult<()> {
        if init_type != 0 {
            return Err(HeifError::invalid("只支持 I 片（CABAC initType 必须为 0）。"));
        }
        let qp = slice_qp_y.clamp(0, 51);
        for (i, v) in HevcCabacTables::InitValue.iter().enumerate() {
            self.ctx[i] = HevcCabacTables::initialize(i32::from(*v), qp) as u8;
        }
        Ok(())
    }

    /// 把上下文变量复制出去（§9.3.2.4 的存储过程）。
    pub fn save_contexts(&self, destination: &mut [u8]) -> HeifResult<()> {
        if destination.len() < HevcCabacTables::Count {
            return Err(HeifError::invalid("CABAC 上下文目标缓冲区过小。"));
        }
        destination[..HevcCabacTables::Count].copy_from_slice(&self.ctx);
        Ok(())
    }

    /// 恢复由 [`HevcCabac::save_contexts`] 保存的上下文（§9.3.2.5 的 WPP 与依赖片同步）。
    pub fn load_contexts(&mut self, source: &[u8]) -> HeifResult<()> {
        if source.len() < HevcCabacTables::Count {
            return Err(HeifError::invalid("CABAC 上下文源缓冲区过小。"));
        }
        self.ctx.copy_from_slice(&source[..HevcCabacTables::Count]);
        Ok(())
    }

    /// DecodeDecision（§9.3.4.3.2）：对上下文 `ctx_idx` 解一个常规 bin。
    pub fn decode_bin(&mut self, ctx_idx: usize) -> HeifResult<u32> {
        if ctx_idx >= HevcCabacTables::Count {
            return Err(HeifError::invalid("CABAC 上下文索引越界。"));
        }
        if self.bits < 8 {
            self.refill(); // 一次决策最多读 7 位
        }
        let state = self.ctx[ctx_idx];
        let table_idx = (((state >> 1) << 2) | (((self.range >> 6) & 3) as u8)) as usize;
        let lps_range = u32::from(HevcCabacTables::RangeTabLps[table_idx]);
        let mut range = self.range - lps_range;
        let scaled = (range as u64).wrapping_shl(self.bits as u32);
        if self.value < scaled {
            // MPS：ivlCurrRange 保持 ≥ 128，所以重归一化至多读一位。
            // 解码出的 bin = 当前状态的 valMps（原 state 的最低位），随后才迁移。
            let bin = u32::from(state & 1);
            self.ctx[ctx_idx] = HevcCabacTables::TransMps[state as usize];
            if range < 256 {
                range <<= 1;
                self.bits -= 1;
            }
            self.range = range;
            return Ok(bin);
        }

        self.value = self.value.wrapping_sub(scaled);
        // LPS：ivlCurrRange = rangeLps（≥ 6），一步重归一化到 ≥ 256。
        // 解码出的 bin = 原 state 的 valMps 取反。
        let bin = u32::from(state & 1) ^ 1;
        let shift = lps_range.leading_zeros() as i32 - 23;
        self.range = lps_range << shift;
        self.bits -= shift;
        self.ctx[ctx_idx] = HevcCabacTables::TransLps[state as usize];
        Ok(bin)
    }

    /// DecodeBypass（§9.3.4.3.4）。
    pub fn decode_bypass(&mut self) -> HeifResult<u32> {
        if self.bits < 1 {
            self.refill();
        }
        self.bits -= 1;
        let scaled = (self.range as u64).wrapping_shl(self.bits as u32);
        if self.value >= scaled {
            self.value = self.value.wrapping_sub(scaled);
            return Ok(1);
        }
        Ok(0)
    }

    /// DecodeTerminate（§9.3.4.3.5）：用于 end_of_slice_segment_flag 与 end_of_subset_one_bit。
    pub fn decode_terminate(&mut self) -> HeifResult<u32> {
        if self.bits < 8 {
            self.refill();
        }
        self.range = self.range.wrapping_sub(2);
        if self.value >= (self.range as u64).wrapping_shl(self.bits as u32) {
            return Ok(1); // 解码终止；无需重归一化
        }
        if self.range < 256 {
            self.range <<= 1;
            self.bits -= 1;
        }
        Ok(0)
    }

    /// §9.3.3 的二元化及其到旁路/终止/上下文 bin 的映射（§9.3.4.1）：连读 `count` 位旁路。
    pub fn decode_bypass_bits(&mut self, count: usize) -> HeifResult<u32> {
        let mut value: u32 = 0;
        for _ in 0..count {
            value = value.wrapping_shl(1) | self.decode_bypass()?;
        }
        Ok(value)
    }

    /// 截断 unary：上下文 bin 0..ctxEnd-1，然后变长旁路 bin 到 cMax。
    pub fn decode_tu(&mut self, c_max: i32, ctx_offset: usize) -> HeifResult<u32> {
        let mut value: u32 = 0;
        while (value as i64) < (c_max as i64) {
            if self.decode_bin(ctx_offset + value as usize)? == 1 {
                value += 1;
            } else {
                break;
            }
        }
        Ok(value)
    }

    /// 定长旁路读 k 阶 Exp-Golomb 码（§9.3.3.3）。
    pub fn decode_exp_golomb(&mut self, order: i32) -> HeifResult<u32> {
        let mut value: u32 = 0;
        let mut order = order;
        while self.decode_bypass()? == 1 {
            value = value.wrapping_add(1u32.wrapping_shl(order as u32));
            order += 1;
        }
        Ok(value.wrapping_add(self.decode_bypass_bits(order.max(0) as usize)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[path = "../../golden_cabac.rs"]
    mod golden;

    /// 上下文索引常量：Count、InitValue 表长与 C# 常量一致，且各符号偏移都在 0..Count 内。
    #[test]
    fn context_index_constants_and_counts() {
        assert_eq!(HevcCabacTables::Count, golden::COUNT);
        assert_eq!(HevcCabacTables::InitValue.len(), golden::INIT_LEN);
        assert_eq!(golden::INIT_VALUE.len(), golden::COUNT);
        let offsets = [
            HevcCabacTables::SaoMerge,
            HevcCabacTables::SaoType,
            HevcCabacTables::SplitCu,
            HevcCabacTables::TransquantBypass,
            HevcCabacTables::PartMode,
            HevcCabacTables::PrevIntraLuma,
            HevcCabacTables::IntraChroma,
            HevcCabacTables::SplitTransform,
            HevcCabacTables::CbfLuma,
            HevcCabacTables::CbfChroma,
            HevcCabacTables::CuQpDeltaAbs,
            HevcCabacTables::TransformSkip,
            HevcCabacTables::LastX,
            HevcCabacTables::LastY,
            HevcCabacTables::CodedSubBlock,
            HevcCabacTables::Sig,
            HevcCabacTables::Greater1,
            HevcCabacTables::Greater2,
        ];
        for off in offsets {
            assert!(off < HevcCabacTables::Count, "偏移 {off} 越界");
        }
    }

    /// 规范查表逐元素与 C# 生成的 golden 一致。
    #[test]
    fn normative_tables_match_csharp() {
        assert_eq!(&HevcCabacTables::RangeTabLps[..], &golden::RANGE_TAB_LPS[..]);
        assert_eq!(&HevcCabacTables::TransIdxLps[..], &golden::TRANS_IDX_LPS[..]);
        assert_eq!(&HevcCabacTables::TransMps[..], &golden::TRANS_MPS[..]);
        assert_eq!(&HevcCabacTables::TransLps[..], &golden::TRANS_LPS[..]);
        assert_eq!(&HevcCabacTables::InitValue[..], &golden::INIT_VALUE[..]);
    }

    /// §9.3.2.2 的 Initialize：在 qp=26/0/51 下全部 137 个上下文与 C# 一致。
    #[test]
    fn initialize_matches_csharp_at_qps() {
        for qp in [26i32, 0, 51] {
            let golden_arr = match qp {
                26 => &golden::INIT_Q26,
                0 => &golden::INIT_Q0,
                _ => &golden::INIT_Q51,
            };
            for (i, v) in HevcCabacTables::InitValue.iter().enumerate() {
                let packed = HevcCabacTables::initialize(i32::from(*v), qp);
                assert_eq!(packed, golden_arr[i], "qp={qp} ctx={i}");
                assert!((0..=127).contains(&packed), "打包上下文越界: {packed}");
            }
        }
    }

    /// InitContexts 填表：I 片（initType 0），非 0 必须报错。
    #[test]
    fn init_contexts_rejects_non_i_and_size() {
        let mut c = HevcCabac::new(&[0u8; 32], 0);
        c.init_contexts(26, 0).unwrap();
        let mut saved = vec![0u8; 137];
        c.save_contexts(&mut saved).unwrap();
        assert_eq!(saved, golden::INIT_Q26.map(|v| v as u8));
        assert!(c.init_contexts(26, 1).is_err(), "非 I 片必须报错");
        let mut tiny = [0u8; 10];
        assert!(c.save_contexts(&mut tiny).is_err(), "目标缓冲区过小必须报错");
    }

    fn input_bytes() -> Vec<u8> {
        vec![
            0x4D, 0x21, 0x3C, 0x9A, 0x55, 0x0F, 0xE0, 0x2B, 0x88, 0x11, 0x77, 0xAB, 0xCD, 0xEF,
            0x01, 0x23, 0x45, 0x67, 0x89,
        ]
    }

    /// 与 C# 金钥一致：40 个常规 bin（轮换 20 个上下文）+ 旁路/TU/EG/终止 + 位位置 + 演变后的上下文。
    #[test]
    fn decode_sequence_matches_csharp() {
        let input = input_bytes();
        let cctx = [
            0usize, 1, 2, 3, 5, 6, 7, 8, 12, 14, 19, 21, 23, 41, 59, 63, 107, 131, 135, 136,
        ];
        let mut c = HevcCabac::new(&input, 0);
        c.init_contexts(26, 0).unwrap();
        let mut bins = Vec::with_capacity(40);
        for i in 0..40 {
            bins.push(c.decode_bin(cctx[i % cctx.len()]).unwrap());
        }
        assert_eq!(bins.as_slice(), &golden::BINS[..]);
        assert_eq!(c.decode_bypass_bits(8).unwrap(), golden::BYPASS8);
        assert_eq!(c.decode_tu(3, 2).unwrap(), golden::TU);
        assert_eq!(c.decode_exp_golomb(0).unwrap(), golden::EG0);
        assert_eq!(c.decode_exp_golomb(2).unwrap(), golden::EG2);
        assert_eq!(c.decode_terminate().unwrap(), golden::TERM);
        assert_eq!(c.bit_position(), golden::POS);
        let mut saved = [0u8; HevcCabacTables::Count];
        c.save_contexts(&mut saved).unwrap();
        assert_eq!(&saved[..], &golden::CTX_AFTER[..]);
    }

    /// 第二个金钥：byteOffset = 3、qp = 0 的序列；随后 LoadContexts 恢复后继续解码。
    #[test]
    fn decode_byte_offset_and_load_contexts_matches_csharp() {
        let input = input_bytes();
        let mut c = HevcCabac::new(&input, 3);
        c.init_contexts(0, 0).unwrap();
        let mut bins = Vec::with_capacity(24);
        for i in 0..24 {
            bins.push(c.decode_bin((i * 7) % 137).unwrap());
        }
        assert_eq!(bins.as_slice(), &golden::BINS2[..]);
        assert_eq!(c.decode_bypass_bits(4).unwrap(), golden::BYPASS4);
        assert_eq!(c.decode_terminate().unwrap(), golden::TERM2);
        assert_eq!(c.bit_position(), golden::POS2);

        let mut saved = [0u8; HevcCabacTables::Count];
        c.save_contexts(&mut saved).unwrap();
        let mut c2 = HevcCabac::new(&input, 0);
        c2.init_contexts(51, 0).unwrap();
        c2.load_contexts(&saved).unwrap();
        let mut bins3 = Vec::with_capacity(8);
        for i in 0..8 {
            bins3.push(c2.decode_bin((i * 3) % 137).unwrap());
        }
        assert_eq!(bins3.as_slice(), &golden::BINS3[..]);
        assert_eq!(c2.bit_position(), golden::POS3);
    }

    /// 上下文索引越界必须报错而非 panic。
    #[test]
    fn out_of_bounds_context_index_is_error() {
        let input = input_bytes();
        let mut c = HevcCabac::new(&input, 0);
        c.init_contexts(26, 0).unwrap();
        assert!(c.decode_bin(HevcCabacTables::Count).is_err());
        assert!(c.decode_bin(usize::MAX).is_err());
    }

    /// 越界读（输入很短）不允许 panic，且要有一段确定输出闭环（不抛不挂）。
    #[test]
    fn truncated_input_never_panics() {
        let mut c = HevcCabac::new(&[0xAB, 0xCD, 0xEF], 0);
        c.init_contexts(26, 0).unwrap();
        for _ in 0..200 {
            let _ = c.decode_bin(0).unwrap();
        }
        let _ = c.decode_bypass_bits(16).unwrap();
        let _ = c.decode_terminate().unwrap();
    }
}
