//! 位流解码（1:1 复刻 zune-jpeg `bitstream.rs` 的 baseline 部分）。
//!
//! u64 缓冲 + 4 字节预读 + marker/字节填充处理 + `overread_by` 语义逐行对齐。

use core::cmp::min;

use crate::codecs::jpeg::error::DecodeError;
use crate::codecs::jpeg::huffman::{
    HuffmanTable, HUFF_LOOKAHEAD, CS_CODELEN_SHIFT, CS_LUT_BITS, CS_LUT_FULL, CS_VALUE_SHIFT,
};
use crate::codecs::jpeg::io::{ByteReader, Marker};
use crate::codecs::jpeg::segment::{DCT_BLOCK, UN_ZIGZAG, ZIGZAG_QUANT_LEN};

/// 查表解 Huffman（复刻 zune `decode_huff!`：9 位快速路径 + 慢路径 + 哨兵）。
macro_rules! decode_huff {
    ($stream:tt,$symbol:tt,$table:tt) => {
        let mut code_length = $symbol >> HUFF_LOOKAHEAD;

        ($symbol) &= (1 << HUFF_LOOKAHEAD) - 1;

        if code_length > i32::from(HUFF_LOOKAHEAD) {
            // 符号无法在 HUFF_LOOKAHEAD 位内解析，找 9..16 位
            $symbol = ($stream).peek_bits::<16>() as i32;
            while code_length < 17 {
                if $symbol < $table.maxcode[code_length as usize] {
                    break;
                }
                code_length += 1;
            }

            if code_length == 17 {
                // 符号无法解码
                return Err(DecodeError::Format(format!(
                    "Bad Huffman Code 0x{:X}, corrupt JPEG",
                    $symbol
                )));
            }

            $symbol >>= (16 - code_length);
            ($symbol) = i32::from(
                ($table).values[ (($symbol + ($table).offset[code_length as usize]) & 0xFF) as usize],
            );
        }
        if code_length > i32::from(($stream).bits_left) {
            return Err(DecodeError::Format(format!(
                "Code length {code_length}  more than bits left {}",
                ($stream).bits_left
            )));
        }
        ($stream).drop_bits(code_length as u8);
    };
}

/// 位流读取器。
#[rustfmt::skip]
pub(crate) struct BitStream {
    pub(crate) buffer:       u64,
    aligned_buffer:          u64,
    pub(crate) bits_left:    u8,
    pub(crate) marker:       Option<Marker>,
    pub(crate) overread_by:  usize,
    pub(crate) seen_eoi:     bool,
    /// 渐进模式：successive low 位掩码（1 << Al）。
    successive_low_mask:     i16,
    /// 渐进 scan 谱选择起止（Ss/Se）。
    spec_start:              u8,
    spec_end:                u8,
    /// 渐进 EOB 运行计数值。
    pub(crate) eob_run:      i32,
}

impl BitStream {
    pub(crate) fn new() -> BitStream {
        BitStream {
            buffer:              0,
            aligned_buffer:      0,
            bits_left:           0,
            marker:              None,
            overread_by:         0,
            seen_eoi:            false,
            successive_low_mask: 1,
            spec_start:          0,
            spec_end:            0,
            eob_run:             0,
        }
    }

    /// 构造用于渐进解码的位流（以首个 scan 的 Al/Ss/Se 初始化）。
    #[allow(clippy::redundant_field_names)]
    pub(crate) fn new_progressive(al: u8, spec_start: u8, spec_end: u8) -> BitStream {
        BitStream {
            buffer:              0,
            aligned_buffer:      0,
            bits_left:           0,
            marker:              None,
            overread_by:         0,
            seen_eoi:            false,
            successive_low_mask: 1i16 << al,
            spec_start:          spec_start,
            spec_end:            spec_end,
            eob_run:             0,
        }
    }

    /// 更新渐进 scan 参数（每个新 SOS 后调用）。
    pub(crate) fn update_progressive_params(&mut self, _ah: u8, al: u8, spec_start: u8, spec_end: u8) {
        self.successive_low_mask = 1i16 << al;
        self.spec_start = spec_start;
        self.spec_end = spec_end;
    }

    /// 补充位缓冲（最多 32 位），并处理 marker/字节填充。
    #[inline(always)]
    pub fn refill<'a>(&mut self, reader: &mut ByteReader<'a>) -> Result<bool, DecodeError> {
        // 单字节 refill：读到 0xFF 时探测后续 marker
        macro_rules! refill {
            ($buffer:expr,$byte:expr,$bits_left:expr) => {
                // 读一个字节
                $byte = u64::from(reader.read_u8());
                self.overread_by += usize::from(reader.eof()?);
                // 追加到缓冲
                $buffer = ($buffer << 8) | $byte;
                // 增加位数
                $bits_left += 8;
                // 特判 0xFF
                if $byte == 0xff {
                    // 读取下一字节
                    let mut next_byte = u64::from(reader.read_u8());
                    // 字节填充：遇到 0xFF 则跳过
                    if next_byte != 0x00 {
                        while next_byte == 0xFF {
                            next_byte = u64::from(reader.read_u8());
                        }

                        if next_byte != 0x00 {
                            // 撤销刚才的 0xFF 追加并返回
                            $buffer >>= 8;
                            $bits_left -= 8;

                            if $bits_left != 0 {
                                self.aligned_buffer = $buffer << (64 - $bits_left);
                            }

                            let marker = Marker::from_u8(next_byte as u8);
                            self.marker = marker;

                            if let Some(Marker::UNKNOWN(_)) = marker {
                                return Err(DecodeError::Format(
                                    "Unknown marker in bit stream".to_string(),
                                ));
                            }
                            if next_byte == 0xD9 {
                                // EOI 特殊处理：补一个零字节，避免 panic
                                self.buffer <<= 8;
                                self.bits_left += 8;
                                self.aligned_buffer = self.buffer << (64 - self.bits_left);
                            }

                            return Ok(false);
                        }
                    }
                }
            };
        }

        // 32 位足够一次解码(16 位)与 receive_extend(最多 16 位)
        if self.bits_left < 32 {
            if self.marker.is_some() || self.seen_eoi {
                // 已发现 marker 或处于 EOI：填零
                self.buffer <<= 32;
                self.bits_left += 32;
                self.aligned_buffer = self.buffer << (64 - self.bits_left);
                return Ok(true);
            }

            if self.overread_by > 0 {
                if self.bits_left == 0 {
                    return Err(DecodeError::ExhaustedData);
                }
                // 已过 EOF：继续消费缓冲位，不再合成零填充
                return Ok(true);
            }

            // 常见路径：一次预读 4 字节，若无 0xFF 则直接消费
            if let Ok(bytes) = reader.read_fixed_bytes_or_error::<4>() {
                let msb_buf = u32::from_be_bytes(bytes);
                if !has_byte(msb_buf, 255) {
                    self.bits_left += 32;
                    self.buffer <<= 32;
                    self.buffer |= u64::from(msb_buf);
                    self.aligned_buffer = self.buffer << (64 - self.bits_left);
                    return Ok(true);
                }

                reader.rewind(4)?;
            }
            let mut byte;
            // 4 次单字节 refill
            refill!(self.buffer, byte, self.bits_left);
            refill!(self.buffer, byte, self.bits_left);
            refill!(self.buffer, byte, self.bits_left);
            refill!(self.buffer, byte, self.bits_left);
            self.aligned_buffer = self.buffer << (64 - self.bits_left);
        }
        Ok(true)
    }

    /// 解码 DC 系数到 `dc_prediction`。
    #[inline(always)]
    fn decode_dc<'a>(
        &mut self,
        reader: &mut ByteReader<'a>,
        dc_table: &HuffmanTable,
        dc_prediction: &mut i32,
    ) -> Result<(), DecodeError> {
        let (mut symbol, r);

        if self.bits_left < 32 {
            self.refill(reader)?;
        };
        symbol = self.peek_bits::<HUFF_LOOKAHEAD>();
        symbol = dc_table.lookup[symbol as usize];

        decode_huff!(self, symbol, dc_table);

        if symbol != 0 {
            r = self.get_bits(symbol as u8);
            symbol = huff_extend(r, symbol);
        }
        // 更新 DC 预测
        *dc_prediction = dc_prediction.wrapping_add(symbol);

        Ok(())
    }

    /// 丢弃 DC 系数（无需结果，供 `discard_mcu_block` 用）。
    fn discard_dc<'a>(
        &mut self,
        reader: &mut ByteReader<'a>,
        dc_table: &HuffmanTable,
    ) -> Result<(), DecodeError> {
        let mut symbol;

        if self.bits_left < 32 {
            self.refill(reader)?;
        };
        symbol = self.peek_bits::<HUFF_LOOKAHEAD>();
        symbol = dc_table.lookup[symbol as usize];

        decode_huff!(self, symbol, dc_table);

        if symbol != 0 {
            let _ = self.get_bits(symbol as u8);
        }

        Ok(())
    }

    /// 解码一个 MCU 块（DC 预测 + AC 系数），返回锯齿序非零系数数。
    /// 注：热路径已迁移至 `HotStream::decode_mcu_block`（寄存器化局部状态），
    /// 本函数保留作语义参照（dormancy，K24），勿删。
    #[inline(never)]
    #[allow(dead_code)]
    pub fn decode_mcu_block<'a>(
        &mut self,
        reader: &mut ByteReader<'a>,
        dc_table: &HuffmanTable,
        ac_table: &HuffmanTable,
        qt_table: &[i32; DCT_BLOCK],
        block: &mut [i16; 64],
        dc_prediction: &mut i32,
    ) -> Result<u16, DecodeError> {
        let ac_lookup = ac_table.ac_lookup.as_ref().unwrap();

        let (mut symbol, mut r, mut fast_ac);
        let mut pos: usize = 1;
        if self.bits_left < 1 && self.marker.is_some() {
            return Err(DecodeError::Format(
                "No more bytes left in stream before marker".to_string(),
            ));
        }
        // 解码 DC
        self.decode_dc(reader, dc_table, dc_prediction)?;

        block[0] = (i64::from(*dc_prediction) * i64::from(qt_table[0])) as i16;

        while pos < 64 {
            self.refill(reader)?;
            symbol = self.peek_bits::<HUFF_LOOKAHEAD>();
            fast_ac = ac_lookup[symbol as usize];
            symbol = ac_table.lookup[symbol as usize];

            if fast_ac != 0 {
                // FAST AC 路径
                pos += ((fast_ac >> 4) & 15) as usize;
                let t_pos = UN_ZIGZAG[min(pos, 63)] & 63;

                block[t_pos] = (i32::from(fast_ac >> 8) * qt_table[t_pos]) as i16;
                self.drop_bits((fast_ac & 15) as u8);
                pos += 1;
            } else {
                decode_huff!(self, symbol, ac_table);

                r = symbol >> 4;
                symbol &= 15;

                if symbol != 0 {
                    pos += r as usize;
                    r = self.get_bits(symbol as u8);
                    symbol = huff_extend(r, symbol);
                    let t_pos = UN_ZIGZAG[pos & 63] & 63;

                    block[t_pos] = (symbol * qt_table[t_pos]) as i16;

                    pos += 1;
                } else if r != 15 {
                    return Ok(pos as u16);
                } else {
                    pos += 16;
                }
            }
        }

        Ok(64)
    }

    /// 跳过（丢弃）一个 MCU 块，但更新 DC 预测。
    /// 注：热路径已迁移至 `HotStream::discard_mcu_block`，本函数保留作语义参照（dormancy，K24）。
    #[allow(dead_code)]
    pub fn discard_mcu_block<'a>(
        &mut self,
        reader: &mut ByteReader<'a>,
        dc_table: &HuffmanTable,
        ac_table: &HuffmanTable,
    ) -> Result<u16, DecodeError> {
        let ac_lookup = ac_table.ac_lookup.as_ref().unwrap();

        let (mut symbol, mut r, mut fast_ac);
        let mut pos: usize = 1;

        self.discard_dc(reader, dc_table)?;

        while pos < 64 {
            self.refill(reader)?;
            symbol = self.peek_bits::<HUFF_LOOKAHEAD>();
            fast_ac = ac_lookup[symbol as usize];
            symbol = ac_table.lookup[symbol as usize];

            if fast_ac != 0 {
                // FAST AC 路径
                pos += ((fast_ac >> 4) & 15) as usize;

                self.drop_bits((fast_ac & 15) as u8);
                pos += 1;
            } else {
                decode_huff!(self, symbol, ac_table);

                r = symbol >> 4;
                symbol &= 15;

                if symbol != 0 {
                    pos += r as usize;
                    let _ = self.get_bits(symbol as u8);

                    pos += 1;
                } else if r != 15 {
                    return Ok(pos as u16);
                } else {
                    pos += 16;
                }
            }
        }

        Ok(64)
    }

    /// 预读 `LOOKAHEAD` 位。
    #[inline(always)]
    #[allow(clippy::cast_possible_truncation)]
    const fn peek_bits<const LOOKAHEAD: u8>(&self) -> i32 {
        (self.aligned_buffer >> (64 - LOOKAHEAD)) as i32
    }

    /// 丢弃 `n` 位。
    #[inline]
    fn drop_bits(&mut self, n: u8) {
        self.bits_left = self.bits_left.saturating_sub(n);
        self.aligned_buffer <<= n;
    }

    /// 读取 `n_bits` 位并丢弃。
    #[inline(always)]
    #[allow(clippy::cast_possible_truncation)]
    fn get_bits(&mut self, n_bits: u8) -> i32 {
        let mask = (1_u64 << n_bits) - 1;

        self.aligned_buffer = self.aligned_buffer.rotate_left(u32::from(n_bits));
        let bits = (self.aligned_buffer & mask) as i32;
        self.bits_left = self.bits_left.wrapping_sub(n_bits);
        bits
    }

    /// RST 时重置流。
    #[cold]
    pub fn reset(&mut self) {
        self.bits_left = 0;
        self.marker = None;
        self.buffer = 0;
        self.aligned_buffer = 0;
        self.eob_run = 0;
    }

    /// 渐进 DC 首个 scan：解码 DC 差值并写入 `块[0]`（以 Al 位掩码缩放）。
    #[inline]
    pub(crate) fn decode_prog_dc_first<'a>(
        &mut self,
        reader: &mut ByteReader<'a>,
        dc_table: &HuffmanTable,
        block: &mut i16,
        dc_prediction: &mut i32,
    ) -> Result<(), DecodeError> {
        self.decode_dc(reader, dc_table, dc_prediction)?;
        *block = (*dc_prediction as i16).wrapping_mul(self.successive_low_mask);
        Ok(())
    }

    /// 渐进 DC refine scan：读 1 位修正值并入已有 DC 系数。
    #[inline]
    pub(crate) fn decode_prog_dc_refine<'a>(
        &mut self,
        reader: &mut ByteReader<'a>,
        block: &mut i16,
    ) -> Result<(), DecodeError> {
        if self.bits_left < 1 {
            self.refill(reader)?;
            // 若 refill 后仍不足位，说明撞到了 marker
            if self.bits_left < 1 {
                return Err(DecodeError::Format(
                    "Marker found where not expected in refine bit".to_string(),
                ));
            }
        }

        if self.get_bit() == 1 {
            *block = block.wrapping_add(self.successive_low_mask);
        }

        Ok(())
    }

    /// 读单个位。
    fn get_bit(&mut self) -> u8 {
        let k = (self.aligned_buffer >> 63) as u8;
        self.drop_bits(1);
        k
    }

    /// 渐进 AC 首个 scan：解谱段 [Ss..Se] 的首个非零系数（含 EOB RUN 计数）。
    #[allow(clippy::cast_sign_loss)]
    pub(crate) fn decode_mcu_ac_first<'a>(
        &mut self,
        reader: &mut ByteReader<'a>,
        ac_table: &HuffmanTable,
        block: &mut [i16; 64],
    ) -> Result<(), DecodeError> {
        let fast_ac = ac_table.ac_lookup.as_ref().unwrap();
        let bit = self.successive_low_mask;

        let mut k = usize::from(self.spec_start);
        let (mut symbol, mut r, mut fac);

        // EOB 运行交由 mcu.rs 的调用方处理（每块计数）
        'block: loop {
            self.refill(reader)?;

            symbol = self.peek_bits::<HUFF_LOOKAHEAD>();
            fac = fast_ac[symbol as usize];
            symbol = ac_table.lookup[symbol as usize];

            if fac != 0 {
                // 快路径
                k += ((fac >> 4) & 15) as usize; // run
                block[UN_ZIGZAG[min(k, 63)] & 63] = (fac >> 8).wrapping_mul(bit); // 值
                self.drop_bits((fac & 15) as u8);
                k += 1;
            } else {
                decode_huff!(self, symbol, ac_table);

                r = symbol >> 4;
                symbol &= 15;

                if symbol != 0 {
                    k += r as usize;
                    r = self.get_bits(symbol as u8);
                    symbol = huff_extend(r, symbol);
                    block[UN_ZIGZAG[k & 63] & 63] = (symbol as i16).wrapping_mul(bit);
                    k += 1;
                } else {
                    if r != 15 {
                        self.eob_run = 1 << r;
                        self.eob_run += self.get_bits(r as u8);
                        self.eob_run -= 1;
                        break;
                    }

                    k += 16;
                }
            }

            if k > usize::from(self.spec_end) {
                break 'block;
            }
        }
        Ok(())
    }

    /// 渐进 AC refine scan：修正既有非零系数并写入谱段内的新非零系数。
    #[allow(clippy::too_many_lines)]
    pub(crate) fn decode_mcu_ac_refine<'a>(
        &mut self,
        reader: &mut ByteReader<'a>,
        table: &HuffmanTable,
        block: &mut [i16; 64],
    ) -> Result<(), DecodeError> {
        let bit = self.successive_low_mask;

        let mut k = self.spec_start;
        let (mut symbol, mut r);

        if self.eob_run == 0 {
            'no_eob: loop {
                self.refill(reader)?;

                symbol = self.peek_bits::<HUFF_LOOKAHEAD>();
                symbol = table.lookup[symbol as usize];

                decode_huff!(self, symbol, table);

                r = symbol >> 4;
                symbol &= 15;

                if symbol == 0 {
                    if r != 15 {
                        // EOB run = 2^r + 附加位
                        self.eob_run = 1 << r;
                        self.eob_run += self.get_bits(r as u8);
                        break 'no_eob;
                    }
                } else {
                    // 与被参照实现的 libjpeg-turbo 一致：对畸形码仅警告不报错
                    // 取符号位
                    if self.get_bit() == 1 {
                        symbol = i32::from(bit);
                    } else {
                        symbol = i32::from(-bit);
                    }
                }

                // 跳过既有非零系数，并为其追加修正位（1 表示绝对值须增大）
                if k <= self.spec_end {
                    'advance_nonzero: loop {
                        let coefficient = &mut block[UN_ZIGZAG[usize::from(k) & 63] & 63];

                        if *coefficient != 0 {
                            if self.bits_left < 1 {
                                self.refill(reader)?;
                                if self.bits_left < 1 && self.marker.is_some() {
                                    return Err(DecodeError::Format(
                                        "Marker found where not expected in refine bit".to_string(),
                                    ));
                                }
                            }
                            if self.get_bit() == 1 && (*coefficient & bit) == 0 {
                                if *coefficient > 0 {
                                    *coefficient = coefficient.wrapping_add(bit);
                                } else {
                                    *coefficient = coefficient.wrapping_sub(bit);
                                }
                            }
                        } else {
                            r -= 1;

                            if r < 0 {
                                break 'advance_nonzero;
                            }
                        };

                        if k == self.spec_end {
                            break 'advance_nonzero;
                        }

                        k += 1;
                    }
                }

                if symbol != 0 {
                    let pos = UN_ZIGZAG[usize::from(k) & 63];
                    block[pos & 63] = symbol as i16;
                }

                k += 1;

                if k > self.spec_end {
                    break 'no_eob;
                }
            }
        }
        if self.eob_run > 0 {
            // 仅当块非纯零时处理（EOB 意味着后续块可能全零）
            if &block[1..] != &[0_i16; 63] {
                self.refill(reader)?;

                while k <= self.spec_end {
                    let coefficient = &mut block[UN_ZIGZAG[usize::from(k) & 63] & 63];

                    if *coefficient != 0 && self.get_bit() == 1 {
                        // 检查是否为已修正过的系数（避免重复加位）
                        if (*coefficient & bit) == 0 {
                            if *coefficient >= 0 {
                                *coefficient = coefficient.wrapping_add(bit);
                            } else {
                                *coefficient = coefficient.wrapping_sub(bit);
                            }
                        }
                    }
                    if self.bits_left < 1 {
                        // 到最后一刻才 refill
                        self.refill(reader)?;
                    }
                    k += 1;
                }
            }
            // 该块在 EOB 运行中计数完成
            self.eob_run -= 1;
        }
        Ok(())
    }
}

/// JPEG HUFF_EXTEND 等价实现。
#[inline(always)]
fn huff_extend(x: i32, s: i32) -> i32 {
    // 若 x<s 返回 x，否则返回 x+offset[s]，其中 offset[s] = ( (-1<<s)+1)
    (x) + ((((x) - (1 << ((s) - 1))) >> 31) & (((-1) << (s)) + 1))
}

/// 基线熵解码热路径的**寄存器化位流局部状态**。
///
/// 目的：把 `buffer/bits_left/aligned_buffer/marker/overread_by/seen_eoi` 提到
/// `inner_decode_mcu_width` 的局部对象上，整行 MCU 逐块解码共用一份，免逐块经
/// `&mut BitStream` 内存往返（省每块 6 字段快照/回写 + 独立函数调用/寄存器盘存）。
/// 位消耗顺序与 `BitStream::refill/decode_dc/decode_mcu_block/discard_mcu_block`
/// 逐位一致（refill 边界、marker 探测时机、`peek_bits` 的 aligned_buffer 计算、
/// EOB/run 逻辑全部照抄）；`mcu.rs` 在每行结束 / `handle_rst_main` 前后做一次
/// `write_back`/`reload` 同步。字节恒等由 `jpeg_parity`（AVX2/标量双路径）硬闸守护。
#[derive(Clone, Copy)]
pub(crate) struct HotStream {
    buffer: u64,
    aligned_buffer: u64,
    bits_left: u8,
    marker: Option<Marker>,
    overread_by: usize,
    seen_eoi: bool,
}

/// discard 路径专用零融合表占位（不落系数、不被读取，仅满足统一签名）。
static ZQ_ZERO: [u32; ZIGZAG_QUANT_LEN] = [0; ZIGZAG_QUANT_LEN];

/// P-A2 双符号 AC 打包解码开关：`KIMG_JPEG_DUAL_SYM=1|true|on` 开、缺省关。
/// 进程内一次读（OnceLock）；调用方（mcu.rs 每行）在块环外取一次为循环不变布尔，
/// 块环内按寄存器分支分发，热环内无每符号开销（K93 运行时双路径姿势）。
#[inline(always)]
pub(crate) fn dual_sym_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        let t = std::env::var("KIMG_JPEG_DUAL_SYM").unwrap_or_default();
        let t = t.trim();
        t == "1" || t.eq_ignore_ascii_case("true") || t.eq_ignore_ascii_case("on")
    })
}

/// 应用一个已解出的 AC 符号（run/size 展开 + 反量化落系数 + EOB/ZRL 处理）。
/// 与单符号路径的 apply 段逐位一致；`return Ok(..)` 从 `decode_block_inner_dual` 早退
/// （EOB）。仅 P-A2 双符号路径使用（单符号路径保持原样，不引此宏）。
macro_rules! apply_ac {
    ($rs:expr, $v:expr, $pos:expr, $block:ident, $zq:ident) => {{
        if ($rs & 15) != 0 {
            $pos += (($rs >> 4) & 15) as usize;
            if let Some(block) = $block.as_deref_mut() {
                // C# `ZigQ` 单装载：高 16 位=natural-order 目标下标（含 pos≥64 哨兵=63），
                // 低 16 位=量化值；省一次 UN_ZIGZAG + 量化表装载，`min(pos,63)` 因哨兵常消。
                let z = $zq[$pos];
                block[(z >> 16) as usize] = ($v * (z & 0xffff) as i32) as i16;
            }
            $pos += 1;
        } else if $rs != 0xF0 {
            return Ok($pos as u16); // EOB
        } else {
            $pos += 16; // ZRL
        }
    }};
}

impl HotStream {
    /// 从 `BitStream` 快照当前位流状态。
    #[inline]
    pub(crate) fn from_stream(s: &BitStream) -> HotStream {
        HotStream {
            buffer: s.buffer,
            aligned_buffer: s.aligned_buffer,
            bits_left: s.bits_left,
            marker: s.marker,
            overread_by: s.overread_by,
            seen_eoi: s.seen_eoi,
        }
    }

    /// 把局部状态回写进 `BitStream`（一行 MCU 结束 / `handle_rst_main` 前调用）。
    #[inline]
    pub(crate) fn write_back(&mut self, s: &mut BitStream) {
        s.buffer = self.buffer;
        s.aligned_buffer = self.aligned_buffer;
        s.bits_left = self.bits_left;
        s.marker = self.marker;
        s.overread_by = self.overread_by;
        s.seen_eoi = self.seen_eoi;
    }

    /// 当前 marker（P2：mcu.rs 免 per-MCU 回写的断点检查用；与 `write_back` 后查
    /// `stream.marker` 逐字段等价，断点时机不变）。
    #[inline]
    pub(crate) fn marker(&self) -> Option<Marker> {
        self.marker
    }

    /// 当前剩余位数（同上，等价 `stream.bits_left`）。
    #[inline]
    pub(crate) fn bits_left(&self) -> u8 {
        self.bits_left
    }

    /// `handle_rst_main`（RST reset 等）改动了 `BitStream` 后重新装载。
    #[inline]
    pub(crate) fn reload(&mut self, s: &BitStream) {
        *self = Self::from_stream(s);
    }

    /// 补充位缓冲（最多 32 位），并处理 marker/字节填充（1:1 复刻 `BitStream::refill`）。
    ///
    /// P-A1 测量否决：曾试 `#[cold]` + `#[inline(never)]`（v3），同窗两窗 ours
    /// 6.94-6.99 vs v1 6.77-6.84（~2% 净退化）——refill 每 ~3-4 符号触发一次，出线调用 +
    /// 状态内存往返的开销大于 I-cache/寄存器帧收益；维持内联（v1 选型）。
    #[inline(always)]
    fn refill<'a>(&mut self, reader: &mut ByteReader<'a>) -> Result<bool, DecodeError> {
        // 单字节 refill：读到 0xFF 时探测后续 marker
        macro_rules! refill_hot {
            ($buffer:expr,$byte:expr,$bits_left:expr) => {
                // 读一个字节
                $byte = u64::from(reader.read_u8());
                self.overread_by += usize::from(reader.eof()?);
                // 追加到缓冲
                $buffer = ($buffer << 8) | $byte;
                // 增加位数
                $bits_left += 8;
                // 特判 0xFF
                if $byte == 0xff {
                    // 读取下一字节
                    let mut next_byte = u64::from(reader.read_u8());
                    // 字节填充：遇到 0xFF 则跳过
                    if next_byte != 0x00 {
                        while next_byte == 0xFF {
                            next_byte = u64::from(reader.read_u8());
                        }

                        if next_byte != 0x00 {
                            // 撤销刚才的 0xFF 追加并返回
                            $buffer >>= 8;
                            $bits_left -= 8;

                            if $bits_left != 0 {
                                self.aligned_buffer = $buffer << (64 - $bits_left);
                            }

                            let marker = Marker::from_u8(next_byte as u8);
                            self.marker = marker;

                            if let Some(Marker::UNKNOWN(_)) = marker {
                                return Err(DecodeError::Format(
                                    "Unknown marker in bit stream".to_string(),
                                ));
                            }
                            if next_byte == 0xD9 {
                                // EOI 特殊处理：补一个零字节，避免 panic
                                self.buffer <<= 8;
                                self.bits_left += 8;
                                self.aligned_buffer = self.buffer << (64 - self.bits_left);
                            }

                            return Ok(false);
                        }
                    }
                }
            };
        }

        // 32 位足够一次解码(16 位)与 receive_extend(最多 16 位)
        if self.bits_left < 32 {
            if self.marker.is_some() || self.seen_eoi {
                // 已发现 marker 或处于 EOI：填零
                self.buffer <<= 32;
                self.bits_left += 32;
                self.aligned_buffer = self.buffer << (64 - self.bits_left);
                return Ok(true);
            }

            if self.overread_by > 0 {
                if self.bits_left == 0 {
                    return Err(DecodeError::ExhaustedData);
                }
                // 已过 EOF：继续消费缓冲位，不再合成零填充
                return Ok(true);
            }

            // 常见路径：一次预读 4 字节，若无 0xFF 则直接消费
            if let Ok(bytes) = reader.read_fixed_bytes_or_error::<4>() {
                let msb_buf = u32::from_be_bytes(bytes);
                if !has_byte(msb_buf, 255) {
                    self.bits_left += 32;
                    self.buffer <<= 32;
                    self.buffer |= u64::from(msb_buf);
                    self.aligned_buffer = self.buffer << (64 - self.bits_left);
                    return Ok(true);
                }

                reader.rewind(4)?;
            }
            let mut byte;
            // 4 次单字节 refill
            refill_hot!(self.buffer, byte, self.bits_left);
            refill_hot!(self.buffer, byte, self.bits_left);
            refill_hot!(self.buffer, byte, self.bits_left);
            refill_hot!(self.buffer, byte, self.bits_left);
            self.aligned_buffer = self.buffer << (64 - self.bits_left);
        }
        Ok(true)
    }

    /// 预读 `LOOKAHEAD` 位。
    #[inline(always)]
    #[allow(clippy::cast_possible_truncation)]
    fn peek_bits<const LOOKAHEAD: u8>(&self) -> i32 {
        (self.aligned_buffer >> (64 - LOOKAHEAD)) as i32
    }

    /// 丢弃 `n` 位。
    #[inline(always)]
    fn drop_bits(&mut self, n: u8) {
        self.bits_left = self.bits_left.saturating_sub(n);
        self.aligned_buffer <<= n;
    }

    /// 读取 `n_bits` 位并丢弃。
    #[inline(always)]
    #[allow(clippy::cast_possible_truncation)]
    fn get_bits(&mut self, n_bits: u8) -> i32 {
        let mask = (1_u64 << n_bits) - 1;

        self.aligned_buffer = self.aligned_buffer.rotate_left(u32::from(n_bits));
        let bits = (self.aligned_buffer & mask) as i32;
        self.bits_left = self.bits_left.wrapping_sub(n_bits);
        bits
    }

    /// 解码 DC 系数到 `dc_prediction`（1:1 复刻 `BitStream::decode_dc`）。
    /// 注：基线热路径已迁移至 `decode_block_inner`（cs_lut 单探测），本函数保留作
    /// 语义参照（dormancy，K24），勿删。
    #[inline(always)]
    #[allow(dead_code)]
    fn decode_dc<'a>(
        &mut self,
        reader: &mut ByteReader<'a>,
        dc_table: &HuffmanTable,
        dc_prediction: &mut i32,
    ) -> Result<(), DecodeError> {
        let (mut symbol, r);

        if self.bits_left < 32 {
            self.refill(reader)?;
        };
        symbol = self.peek_bits::<HUFF_LOOKAHEAD>();
        symbol = dc_table.lookup[symbol as usize];

        decode_huff!(self, symbol, dc_table);

        if symbol != 0 {
            r = self.get_bits(symbol as u8);
            symbol = huff_extend(r, symbol);
        }
        // 更新 DC 预测
        *dc_prediction = dc_prediction.wrapping_add(symbol);

        Ok(())
    }

    /// 丢弃 DC 系数（无需结果）。
    /// 注：基线热路径已迁移至 `decode_block_inner`（cs_lut 单探测），本函数保留作
    /// 语义参照（dormancy，K24），勿删。
    #[inline(always)]
    #[allow(dead_code)]
    fn discard_dc<'a>(
        &mut self,
        reader: &mut ByteReader<'a>,
        dc_table: &HuffmanTable,
    ) -> Result<(), DecodeError> {
        let mut symbol;

        if self.bits_left < 32 {
            self.refill(reader)?;
        };
        symbol = self.peek_bits::<HUFF_LOOKAHEAD>();
        symbol = dc_table.lookup[symbol as usize];

        decode_huff!(self, symbol, dc_table);

        if symbol != 0 {
            let _ = self.get_bits(symbol as u8);
        }

        Ok(())
    }

    /// 解码一个 MCU 块（DC 预测 + AC 系数），返回锯齿序非零系数数
    /// （1:1 复刻 `BitStream::decode_mcu_block`，状态在局部）。
    ///
    /// 快路径微优化：`ac_table.lookup` 只在慢路径需要 `symbol` 时才查询——快路径
    /// 占绝大多数符号，省一次表加载（慢路径复算一次，位消耗语义不变）。
    #[inline(always)]
    pub(crate) fn decode_mcu_block<'a>(
        &mut self,
        reader: &mut ByteReader<'a>,
        dc_table: &HuffmanTable,
        ac_table: &HuffmanTable,
        qt_table: &[i32; DCT_BLOCK],
        zq: &[u32; ZIGZAG_QUANT_LEN],
        block: &mut [i16; 64],
        dc_prediction: &mut i32,
    ) -> Result<u16, DecodeError> {
        self.decode_block_inner(reader, dc_table, ac_table, qt_table, zq, Some(block), dc_prediction)
    }

    /// 跳过（丢弃）一个 MCU 块；不落系数（位流消费与 `decode_mcu_block` 逐位一致）。
    /// 注：discard 不写系数，故融合表传常量零表占位（不读取）。
    #[inline(always)]
    pub(crate) fn discard_mcu_block<'a>(
        &mut self,
        reader: &mut ByteReader<'a>,
        dc_table: &HuffmanTable,
        ac_table: &HuffmanTable,
    ) -> Result<u16, DecodeError> {
        let mut disc_pred = 0;
        self.decode_block_inner(
            reader,
            dc_table,
            ac_table,
            &[0; DCT_BLOCK],
            &ZQ_ZERO,
            None,
            &mut disc_pred,
        )
    }

    /// P-A2：`decode_mcu_block` 的双符号变体（仅 `KIMG_JPEG_DUAL_SYM` 开启时由 mcu.rs
    /// 路由到此处；与单符号路径对同一有效流逐位一致，字节恒等由 parity 硬闸守护）。
    #[inline(always)]
    pub(crate) fn decode_mcu_block_dual<'a>(
        &mut self,
        reader: &mut ByteReader<'a>,
        dc_table: &HuffmanTable,
        ac_table: &HuffmanTable,
        qt_table: &[i32; DCT_BLOCK],
        zq: &[u32; ZIGZAG_QUANT_LEN],
        block: &mut [i16; 64],
        dc_prediction: &mut i32,
    ) -> Result<u16, DecodeError> {
        self.decode_block_inner_dual(reader, dc_table, ac_table, qt_table, zq, Some(block), dc_prediction)
    }

    /// P-A2：`discard_mcu_block` 的双符号变体（语义同上）。
    #[inline(always)]
    pub(crate) fn discard_mcu_block_dual<'a>(
        &mut self,
        reader: &mut ByteReader<'a>,
        dc_table: &HuffmanTable,
        ac_table: &HuffmanTable,
    ) -> Result<u16, DecodeError> {
        let mut disc_pred = 0;
        self.decode_block_inner_dual(
            reader,
            dc_table,
            ac_table,
            &[0; DCT_BLOCK],
            &ZQ_ZERO,
            None,
            &mut disc_pred,
        )
    }

    /// P-A2：由已探测的 `cs_lut` 条目 `e` 解一个 AC 符号（FULL / 码在窗内幅值出窗 / 慢路径
    /// 三路），与单符号路径对应分支逐位一致（位消费顺序、refill 边界由调用方保证）。
    /// 调用方必须保证窗口位数足够本次消费（FULL≤11、非 FULL≤len+t、慢≤31），否则结果
    /// 偏离单符号路径；P-A2 双符号路径以「逐步安全判据」保证后才直解，坏流则回退单符号。
    #[inline(always)]
    fn ac_symbol_from_e(
        &mut self,
        ac_table: &HuffmanTable,
        e: i32,
    ) -> Result<(u8, i32), DecodeError> {
        if (e & CS_LUT_FULL) != 0 {
            // 完整条目（含 EOB/ZRL 单探测）：一次消费（码长+幅值）。
            let n = (e & 31) as u8;
            self.aligned_buffer <<= n;
            self.bits_left = self.bits_left.saturating_sub(n);
            Ok((((e >> 11) & 255) as u8, e >> CS_VALUE_SHIFT))
        } else if e != 0 {
            // 码在窗内、幅值出窗。
            let len = ((e >> CS_CODELEN_SHIFT) & 15) as u8;
            self.aligned_buffer <<= len;
            let rs = ((e >> 11) & 255) as u8;
            let t = (rs & 15) as u8;
            let mag = (self.aligned_buffer >> (64 - t)) as i32;
            self.aligned_buffer <<= t;
            self.bits_left = self.bits_left.saturating_sub(len + t);
            Ok((rs, if t != 0 { huff_extend(mag, i32::from(t)) } else { 0 }))
        } else {
            // 码长 > 11：慢路径（探测时不移动缓冲，与单符号路径一致）。
            let mut symbol = ac_table.lookup[(self.aligned_buffer >> (64 - CS_LUT_BITS)) as usize];
            decode_huff!(self, symbol, ac_table);
            let rs = symbol as u8;
            let size = (rs & 15) as u8;
            let v = if size != 0 {
                let r = self.get_bits(size);
                huff_extend(r, i32::from(size))
            } else {
                0
            };
            Ok((rs, v))
        }
    }

    /// 熵解码共享内核（C# `JpegCore.DecodeBlockCore` 同构，JpegCore.cs:1021）：
    ///
    /// - DC/AC 都用 11 位值打包快速表 `cs_lut`（`JpegHuffman.BuildLut`）：一次探测解出
    ///   (低 5 位消耗位数, 位 6-9 码长, 位 10 LutFull, 位 11-18 run/size, 位 21-31 已扩展
    ///   幅值)，单次 `aligned_buffer <<= (e & 31)` 消费（位 5 恒 0，`e&31==e&63`）；幅值
    ///   出窗时退「丢码长 + 读 t 位扩展」；`e==0`（码长 > 11）退 `decode_huff!` 慢路径。
    ///   位消耗总量与旧实现逐符号一致，故 refill/marker 探测时机不变。
    /// - 反量化契约不变：DC=(i64 乘积)as i16、AC=(v·q)as i16、`min(pos,63)` 挡越界
    ///   （由 `zq` 哨兵条目在单装载内等价实现，见 `build_zigzag_quant`）。
    /// - `block=None` 只消费位流不落系数（discard 路径）。
    ///
    /// P-A1：`#[inline(never)]` 独立函数态——把熵块解算从巨型 const-generic
    /// `inner_decode_mcu_width`（单函数 ~3000 行 asm，寄存器榨干 → aligned_buffer/
    /// bits_left/zq/block 每符号栈帧访问）中提出，编译器给它完整寄存器帧（独立函数态
    /// ≈18-22 条/符号、0 栈访问）；外层只保留 MCU 循环与 post。非泛型 → 全部 4 个
    /// PROGRESSIVE/SAMPLED 实例共享同一份独立函数代码。
    #[inline(never)]
    fn decode_block_inner<'a>(
        &mut self,
        reader: &mut ByteReader<'a>,
        dc_table: &HuffmanTable,
        ac_table: &HuffmanTable,
        qt_table: &[i32; DCT_BLOCK],
        zq: &[u32; ZIGZAG_QUANT_LEN],
        mut block: Option<&mut [i16; 64]>,
        dc_prediction: &mut i32,
    ) -> Result<u16, DecodeError> {
        let dc_cs = &dc_table.cs_lut;
        let ac_cs = &ac_table.cs_lut;

        // marker 且 0 位在块起始：仅解码路径保留此防御检查（与旧 `decode_mcu_block`
        // 一致）；discard 路径旧实现无此检查，为保持其逐位语义不加。
        if block.is_some() && self.bits_left < 1 && self.marker.is_some() {
            return Err(DecodeError::Format(
                "No more bytes left in stream before marker".to_string(),
            ));
        }

        // --- DC ---
        if self.bits_left < 32 {
            self.refill(reader)?;
        }
        let mut e = dc_cs[(self.aligned_buffer >> (64 - CS_LUT_BITS)) as usize];
        let diff = if (e & CS_LUT_FULL) != 0 {
            // 值在窗口内：单次移位消费码长+幅值（DC 表符号恒 ≤15，建表已校验）。
            let n = (e & 31) as u8;
            self.aligned_buffer <<= n;
            self.bits_left = self.bits_left.saturating_sub(n);
            e >> CS_VALUE_SHIFT
        } else if e != 0 {
            // 码在窗内但幅值出窗：先丢码长，再读 t 位幅值并按 JPEG EXTEND 扩展。
            let len = ((e >> CS_CODELEN_SHIFT) & 15) as u8;
            let t = ((e >> 11) & 15) as u8;
            self.aligned_buffer <<= len;
            let mag = (self.aligned_buffer >> (64 - t)) as i32;
            self.aligned_buffer <<= t;
            self.bits_left = self.bits_left.saturating_sub(len + t);
            huff_extend(mag, i32::from(t))
        } else {
            // 码长 > 11：慢路径（zune 风格）。
            let mut symbol =
                dc_table.lookup[(self.aligned_buffer >> (64 - CS_LUT_BITS)) as usize];
            decode_huff!(self, symbol, dc_table);
            if symbol != 0 {
                let r = self.get_bits(symbol as u8);
                huff_extend(r, symbol)
            } else {
                0
            }
        };
        *dc_prediction = dc_prediction.wrapping_add(diff);
        if let Some(block) = block.as_deref_mut() {
            block[0] = (i64::from(*dc_prediction) * i64::from(qt_table[0])) as i16;
        }

        // --- AC ---
        let mut pos: usize = 1;
        loop {
            if self.bits_left < 32 {
                self.refill(reader)?;
            }
            e = ac_cs[(self.aligned_buffer >> (64 - CS_LUT_BITS)) as usize];
            let (rs, v) = if (e & CS_LUT_FULL) != 0 {
                // 完整条目（含 EOB/ZRL 单探测）：一次消费（码长+幅值）。
                let n = (e & 31) as u8;
                self.aligned_buffer <<= n;
                self.bits_left = self.bits_left.saturating_sub(n);
                (((e >> 11) & 255) as u8, e >> CS_VALUE_SHIFT)
            } else if e != 0 {
                // 码在窗内、幅值出窗。
                let len = ((e >> CS_CODELEN_SHIFT) & 15) as u8;
                self.aligned_buffer <<= len;
                let rs = ((e >> 11) & 255) as u8;
                let t = (rs & 15) as u8;
                let mag = (self.aligned_buffer >> (64 - t)) as i32;
                self.aligned_buffer <<= t;
                self.bits_left = self.bits_left.saturating_sub(len + t);
                (rs, if t != 0 { huff_extend(mag, i32::from(t)) } else { 0 })
            } else {
                // 码长 > 11：慢路径。
                let mut symbol =
                    ac_table.lookup[(self.aligned_buffer >> (64 - CS_LUT_BITS)) as usize];
                decode_huff!(self, symbol, ac_table);
                let rs = symbol as u8;
                let size = (rs & 15) as u8;
                let v = if size != 0 {
                    let r = self.get_bits(size);
                    huff_extend(r, i32::from(size))
                } else {
                    0
                };
                (rs, v)
            };

            if (rs & 15) != 0 {
                pos += ((rs >> 4) & 15) as usize;
                if let Some(block) = block.as_deref_mut() {
                    // C# `ZigQ` 单装载：高 16 位=natural-order 目标下标（含 pos≥64 哨兵=63），
                    // 低 16 位=量化值；省一次 UN_ZIGZAG + 量化表装载，`min(pos,63)` 因哨兵常消。
                    let z = zq[pos];
                    block[(z >> 16) as usize] = (v * (z & 0xffff) as i32) as i16;
                }
                pos += 1;
            } else if rs != 0xF0 {
                return Ok(pos as u16); // EOB
            } else {
                pos += 16; // ZRL
            }

            if pos >= 64 {
                break;
            }
        }

        Ok(64)
    }

    /// P-A2 双符号 AC 打包解码内核（`KIMG_JPEG_DUAL_SYM=1|true|on` 时由
    /// `decode_mcu_block_dual`/`discard_mcu_block_dual` 路由到此处；缺省关走
    /// `decode_block_inner` 单符号路径，本函数不执行）。
    ///
    /// DC 段与单符号路径逐位一致；AC 段每对符号共享「一次 refill 检查 + 一次循环背沿」，
    /// s2 仅当「逐步安全判据」成立时免 refill 直解——判据保证 s2 与单符号路径逐位一致：
    ///   - 探测需 ≥11 位（守卫 bits_left ≥ 16）；
    ///   - FULL 条目（值内嵌表项，消费 ≤11 位）⟹ 恒安全；
    ///   - 码在窗内、幅值出窗 → 精确需 bits_left ≥ len+t；
    ///   - 慢路径（码长 >11）→ 需 bits_left ≥ 31（最多 16+15 位）。
    /// 判据不满足 → 回退循环顶 = 单符号路径的「refill 检查 + 解符号」（逐位一致；仅
    /// marker 探测点可延后一个 refill：对有效流收敛、坏流走抛错路径，parity 兜底）。
    #[inline(never)]
    fn decode_block_inner_dual<'a>(
        &mut self,
        reader: &mut ByteReader<'a>,
        dc_table: &HuffmanTable,
        ac_table: &HuffmanTable,
        qt_table: &[i32; DCT_BLOCK],
        zq: &[u32; ZIGZAG_QUANT_LEN],
        mut block: Option<&mut [i16; 64]>,
        dc_prediction: &mut i32,
    ) -> Result<u16, DecodeError> {
        let dc_cs = &dc_table.cs_lut;
        let ac_cs = &ac_table.cs_lut;

        // marker 且 0 位在块起始：仅解码路径保留此防御检查（与单符号路径一致）。
        if block.is_some() && self.bits_left < 1 && self.marker.is_some() {
            return Err(DecodeError::Format(
                "No more bytes left in stream before marker".to_string(),
            ));
        }

        // --- DC（与单符号路径逐位一致）---
        if self.bits_left < 32 {
            self.refill(reader)?;
        }
        let e = dc_cs[(self.aligned_buffer >> (64 - CS_LUT_BITS)) as usize];
        let diff = if (e & CS_LUT_FULL) != 0 {
            // 值在窗口内：单次移位消费码长+幅值（DC 表符号恒 ≤15，建表已校验）。
            let n = (e & 31) as u8;
            self.aligned_buffer <<= n;
            self.bits_left = self.bits_left.saturating_sub(n);
            e >> CS_VALUE_SHIFT
        } else if e != 0 {
            // 码在窗内但幅值出窗：先丢码长，再读 t 位幅值并按 JPEG EXTEND 扩展。
            let len = ((e >> CS_CODELEN_SHIFT) & 15) as u8;
            let t = ((e >> 11) & 15) as u8;
            self.aligned_buffer <<= len;
            let mag = (self.aligned_buffer >> (64 - t)) as i32;
            self.aligned_buffer <<= t;
            self.bits_left = self.bits_left.saturating_sub(len + t);
            huff_extend(mag, i32::from(t))
        } else {
            // 码长 > 11：慢路径（zune 风格）。
            let mut symbol =
                dc_table.lookup[(self.aligned_buffer >> (64 - CS_LUT_BITS)) as usize];
            decode_huff!(self, symbol, dc_table);
            if symbol != 0 {
                let r = self.get_bits(symbol as u8);
                huff_extend(r, symbol)
            } else {
                0
            }
        };
        *dc_prediction = dc_prediction.wrapping_add(diff);
        if let Some(block) = block.as_deref_mut() {
            block[0] = (i64::from(*dc_prediction) * i64::from(qt_table[0])) as i16;
        }

        // --- AC（双符号打包）---
        let mut pos: usize = 1;
        loop {
            if self.bits_left < 32 {
                self.refill(reader)?;
            }
            // --- 符号 1（与单符号路径逐位一致）---
            let e = ac_cs[(self.aligned_buffer >> (64 - CS_LUT_BITS)) as usize];
            let (rs, v) = self.ac_symbol_from_e(ac_table, e)?;
            apply_ac!(rs, v, pos, block, zq);
            if pos >= 64 {
                break;
            }
            // --- 符号 2（免 refill 检查；逐步安全判据）---
            if self.bits_left >= 16 {
                let e2 = ac_cs[(self.aligned_buffer >> (64 - CS_LUT_BITS)) as usize];
                let safe = if (e2 & CS_LUT_FULL) != 0 {
                    true
                } else if e2 != 0 {
                    let len = ((e2 >> CS_CODELEN_SHIFT) & 15) as u8;
                    let t = ((e2 >> 11) & 15) as u8;
                    self.bits_left >= len + t
                } else {
                    self.bits_left >= 31
                };
                if safe {
                    let (rs, v) = self.ac_symbol_from_e(ac_table, e2)?;
                    apply_ac!(rs, v, pos, block, zq);
                    if pos >= 64 {
                        break;
                    }
                }
            }
        }

        Ok(64)
    }
}

const fn has_zero(v: u32) -> bool {
    !((((v & 0x7F7F_7F7F) + 0x7F7F_7F7F) | v) | 0x7F7F_7F7F) != 0
}

const fn has_byte(b: u32, val: u8) -> bool {
    has_zero(b ^ ((!0_u32 / 255) * (val as u32)))
}
