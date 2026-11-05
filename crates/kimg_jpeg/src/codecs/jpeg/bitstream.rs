//! 位流解码（1:1 复刻 zune-jpeg `bitstream.rs` 的 baseline 部分）。
//!
//! u64 缓冲 + 4 字节预读 + marker/字节填充处理 + `overread_by` 语义逐行对齐。

use core::cmp::min;

use crate::codecs::jpeg::error::DecodeError;
use crate::codecs::jpeg::huffman::{HuffmanTable, HUFF_LOOKAHEAD};
use crate::codecs::jpeg::io::{ByteReader, Marker};
use crate::codecs::jpeg::segment::{DCT_BLOCK, UN_ZIGZAG};

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
        block: &mut [i32; 64],
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

        block[0] = *dc_prediction * qt_table[0];

        while pos < 64 {
            self.refill(reader)?;
            symbol = self.peek_bits::<HUFF_LOOKAHEAD>();
            fast_ac = ac_lookup[symbol as usize];
            symbol = ac_table.lookup[symbol as usize];

            if fast_ac != 0 {
                // FAST AC 路径
                pos += ((fast_ac >> 4) & 15) as usize;
                let t_pos = UN_ZIGZAG[min(pos, 63)] & 63;

                block[t_pos] = i32::from(fast_ac >> 8) * (qt_table[t_pos]);
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

                    block[t_pos] = symbol * qt_table[t_pos];

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

    /// `handle_rst_main`（RST reset 等）改动了 `BitStream` 后重新装载。
    #[inline]
    pub(crate) fn reload(&mut self, s: &BitStream) {
        *self = Self::from_stream(s);
    }

    /// 补充位缓冲（最多 32 位），并处理 marker/字节填充（1:1 复刻 `BitStream::refill`）。
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

    /// 丢弃 DC 系数（无需结果）。
    #[inline(always)]
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
        block: &mut [i32; 64],
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

        block[0] = *dc_prediction * qt_table[0];

        while pos < 64 {
            self.refill(reader)?;
            symbol = self.peek_bits::<HUFF_LOOKAHEAD>();
            fast_ac = ac_lookup[symbol as usize];

            if fast_ac != 0 {
                // FAST AC 路径（多数符号）：只做一次查询就够
                pos += ((fast_ac >> 4) & 15) as usize;
                let t_pos = UN_ZIGZAG[min(pos, 63)] & 63;

                block[t_pos] = i32::from(fast_ac >> 8) * (qt_table[t_pos]);
                self.drop_bits((fast_ac & 15) as u8);
                pos += 1;
            } else {
                symbol = ac_table.lookup[symbol as usize];
                decode_huff!(self, symbol, ac_table);

                r = symbol >> 4;
                symbol &= 15;

                if symbol != 0 {
                    pos += r as usize;
                    r = self.get_bits(symbol as u8);
                    symbol = huff_extend(r, symbol);
                    let t_pos = UN_ZIGZAG[pos & 63] & 63;

                    block[t_pos] = symbol * qt_table[t_pos];

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

    /// 跳过（丢弃）一个 MCU 块，但更新 DC 预测（1:1 复刻 `BitStream::discard_mcu_block`）。
    #[inline(always)]
    pub(crate) fn discard_mcu_block<'a>(
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

            if fast_ac != 0 {
                // FAST AC 路径（只做一次查询）
                pos += ((fast_ac >> 4) & 15) as usize;

                self.drop_bits((fast_ac & 15) as u8);
                pos += 1;
            } else {
                symbol = ac_table.lookup[symbol as usize];
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
}

const fn has_zero(v: u32) -> bool {
    !((((v & 0x7F7F_7F7F) + 0x7F7F_7F7F) | v) | 0x7F7F_7F7F) != 0
}

const fn has_byte(b: u32, val: u8) -> bool {
    has_zero(b ^ ((!0_u32 / 255) * (val as u32)))
}
