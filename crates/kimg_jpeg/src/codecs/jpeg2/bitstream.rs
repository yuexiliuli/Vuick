//! 位流解码（1:1 复刻 zune-jpeg `bitstream.rs` 的 baseline 部分）。
//!
//! u64 缓冲 + 4 字节预读 + marker/字节填充处理 + `overread_by` 语义逐行对齐。

use core::cmp::min;

use crate::codecs::jpeg2::error::DecodeError;
use crate::codecs::jpeg2::huffman::{HuffmanTable, HUFF_LOOKAHEAD};
use crate::codecs::jpeg2::io::{ByteReader, Marker};
use crate::codecs::jpeg2::segment::{DCT_BLOCK, UN_ZIGZAG};

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
}

impl BitStream {
    pub(crate) fn new() -> BitStream {
        BitStream {
            buffer:      0,
            aligned_buffer: 0,
            bits_left:   0,
            marker:      None,
            overread_by: 0,
            seen_eoi:    false,
        }
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
    #[inline(never)]
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
    }
}

/// JPEG HUFF_EXTEND 等价实现。
#[inline(always)]
fn huff_extend(x: i32, s: i32) -> i32 {
    // 若 x<s 返回 x，否则返回 x+offset[s]，其中 offset[s] = ( (-1<<s)+1)
    (x) + ((((x) - (1 << ((s) - 1))) >> 31) & (((-1) << (s)) + 1))
}

const fn has_zero(v: u32) -> bool {
    !((((v & 0x7F7F_7F7F) + 0x7F7F_7F7F) | v) | 0x7F7F_7F7F) != 0
}

const fn has_byte(b: u32, val: u8) -> bool {
    has_zero(b ^ ((!0_u32 / 255) * (val as u32)))
}
