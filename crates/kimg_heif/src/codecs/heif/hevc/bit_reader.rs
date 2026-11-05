//! 读取 HEVC RBSP 的定长与 Exp-Golomb 字段（H.265 §7.2、§9.2）。
//! 输入是已经去掉 emulation-prevention 字节的 RBSP（见 [`crate::codecs::heif::hevc::nal::Nal`]）。
//! 读越界返回 `Err(HeifError::InvalidData)`，截断的参数集不可能被误读成零。
//! 移植自 C# `HevcBitReader.cs`。位移语义：C# 位移在越界时 mod-位数，Rust 用显式小位移/受检循环保持恒等。

use crate::codecs::heif::error::{HeifError, HeifResult};

#[derive(Clone)]
pub struct HevcBitReader<'a> {
    data: &'a [u8],
    bit: i64,
}

impl<'a> HevcBitReader<'a> {
    pub fn new(data: &'a [u8], byte_offset: usize) -> Self {
        HevcBitReader {
            data,
            bit: byte_offset as i64 * 8,
        }
    }

    #[inline]
    pub fn bit_position(&self) -> i64 {
        self.bit
    }

    #[inline]
    pub fn byte_aligned(&self) -> bool {
        (self.bit & 7) == 0
    }

    #[inline]
    pub fn byte_position(&self) -> usize {
        ((self.bit + 7) >> 3) as usize
    }

    #[inline]
    pub fn bits_left(&self) -> i64 {
        self.data.len() as i64 * 8 - self.bit
    }

    pub fn read_bit(&mut self) -> HeifResult<u32> {
        if self.bit >= self.data.len() as i64 * 8 {
            return Err(HeifError::invalid(
                "HEVC 参数集或切片头被截断。",
            ));
        }
        let value = u32::from(self.data[(self.bit >> 3) as usize] >> (7 - (self.bit & 7))) & 1;
        self.bit += 1;
        Ok(value)
    }

    #[inline]
    pub fn read_flag(&mut self) -> HeifResult<bool> {
        Ok(self.read_bit()? != 0)
    }

    /// u(n)，0 ≤ n ≤ 32。
    pub fn read_bits(&mut self, count: usize) -> HeifResult<u32> {
        let mut value: u32 = 0;
        for _ in 0..count {
            value = (value << 1) | self.read_bit()?;
        }
        Ok(value)
    }

    pub fn skip(&mut self, count: i64) -> HeifResult<()> {
        if count < 0 || count > self.bits_left() {
            return Err(HeifError::invalid(
                "HEVC 参数集或切片头被截断。",
            ));
        }
        self.bit += count;
        Ok(())
    }

    /// ue(v)。合规流里不可能出现 2^32 - 2 以上的值，超限拒绝。
    pub fn read_ue(&mut self) -> HeifResult<u32> {
        let mut leading_zeros: i32 = 0;
        loop {
            if self.read_bit()? == 0 {
                leading_zeros += 1;
                if leading_zeros > 31 {
                    return Err(HeifError::invalid("Exp-Golomb 码太长。"));
                }
            } else {
                break;
            }
        }
        if leading_zeros == 0 {
            return Ok(0);
        }
        let suffix = i64::from(self.read_bits(leading_zeros as usize)?);
        Ok((((1i64 << leading_zeros) - 1) + suffix) as u32)
    }

    /// 不得超过 `max` 的 ue(v)。
    pub fn read_ue_max(&mut self, max: u32, name: &str) -> HeifResult<i32> {
        let value = self.read_ue()?;
        if value > max {
            return Err(HeifError::invalid(format!(
                "HEVC {name} = {value} 越界（上限 {max}）。"
            )));
        }
        Ok(value as i32)
    }

    /// se(v)。
    pub fn read_se(&mut self) -> HeifResult<i32> {
        let k = i64::from(self.read_ue()?);
        // k 至多 2^32 - 2，折半后幅值落在 31 位内。
        let magnitude = (k + 1) >> 1;
        Ok(if k & 1 != 0 { magnitude as i32 } else { -magnitude as i32 })
    }

    pub fn read_se_range(&mut self, min: i32, max: i32, name: &str) -> HeifResult<i32> {
        let value = self.read_se()?;
        if value < min || value > max {
            return Err(HeifError::invalid(format!(
                "HEVC {name} = {value} 越界 [{min}, {max}]。"
            )));
        }
        Ok(value)
    }

    /// more_rbsp_data()：除 rbsp_trailing_bits 之外仍有内容时为真。
    pub fn more_rbsp_data(&self) -> bool {
        let total_bits = self.data.len() as i64 * 8;
        if self.bit >= total_bits {
            return false;
        }
        // 找最后一个 1 位（rbsp_stop_one_bit）；在当前位置之后仍存在数据 ⇔ 该位位于当前位置之后。
        let mut last = self.data.len() as isize - 1;
        while last >= 0 && self.data[last as usize] == 0 {
            last -= 1;
        }
        if last < 0 {
            return false;
        }
        let b = self.data[last as usize];
        let trailing = b.trailing_zeros() as i64;
        let stop_bit = last as i64 * 8 + (7 - trailing);
        self.bit < stop_bit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_bits_and_flags() {
        let data = [0b1010_0110u8, 0b1111_0000u8];
        let mut r = HevcBitReader::new(&data, 0);
        assert_eq!(r.read_bit().unwrap(), 1);
        assert_eq!(r.read_bit().unwrap(), 0);
        assert_eq!(r.read_bits(6).unwrap(), 0b1001_10);
        assert_eq!(r.read_bits(8).unwrap(), 0b1111_0000);
        assert_eq!(r.bits_left(), 0);
        assert!(r.read_bit().is_err(), "读越界必须报错而非 panic");
    }

    #[test]
    fn reads_ue_known_code_words() {
        // ue(v) 标准码字：0->"1"，1->"010"，2->"011"，3->"00100"，7->"0001000"。
        // 拼接：10100110 01000001 00000000
        let data = [0b1010_0110u8, 0b0100_0001u8, 0b0000_0000u8];
        let mut r = HevcBitReader::new(&data, 0);
        assert_eq!(r.read_ue().unwrap(), 0);
        assert_eq!(r.read_ue().unwrap(), 1);
        assert_eq!(r.read_ue().unwrap(), 2);
        assert_eq!(r.read_ue().unwrap(), 3);
        assert_eq!(r.read_ue().unwrap(), 7);
    }

    #[test]
    fn reads_se_known_code_words() {
        // se(v)：0->ue(0)="1"，1->ue(1)="010"，-1->ue(2)="011"，-2->ue(4)="00101"。
        // 拼接：1 010 011 00101 = 10100110 01010000
        let data = [0b1010_0110u8, 0b0101_0000u8];
        let mut r = HevcBitReader::new(&data, 0);
        assert_eq!(r.read_se().unwrap(), 0);
        assert_eq!(r.read_se().unwrap(), 1);
        assert_eq!(r.read_se().unwrap(), -1);
        assert_eq!(r.read_se().unwrap(), -2);
    }

    #[test]
    fn read_ue_large_value() {
        // 32 位大 ue：码字 000..01 + 31 位后缀（全 0）→ (1<<31)-1。
        let mut data = vec![0u8; 9];
        let mut bit_index = 0;
        for _ in 0..31 {
            bit_index += 1;
        }
        data[bit_index / 8] |= 1 << (7 - (bit_index % 8));
        bit_index += 1;
        for _ in 0..31 {
            bit_index += 1;
        }
        let _ = bit_index;
        let mut r = HevcBitReader::new(&data, 0);
        let v = r.read_ue().unwrap();
        assert_eq!(v, (1u64 << 31) as u32 - 1);
    }
}

