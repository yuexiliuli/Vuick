//! NAL 单元工具（H.265 §7.3.1）。
//! 移植自 C# `HevcBitReader.cs` 底部的 `Nal` 类。

use crate::codecs::heif::error::HeifError;

/// NAL 单元类型编号常量。
pub struct Nal;

impl Nal {
    pub const VpsNut: u8 = 32;
    pub const SpsNut: u8 = 33;
    pub const PpsNut: u8 = 34;

    /// 从一个 NAL 单元 payload 去掉 emulation_prevention_three_byte（§7.4.2）：
    /// 每个 0x000003 变成 0x0000。输出绝不比输入长。
    pub fn unescape(source: &[u8], destination: &mut [u8]) -> usize {
        let mut written = 0usize;
        let mut zeros = 0usize;
        for &b in source {
            if zeros >= 2 && b == 3 {
                zeros = 0;
                continue;
            }
            destination[written] = b;
            written += 1;
            zeros = if b == 0 { zeros + 1 } else { 0 };
        }
        written
    }

    /// [`Self::unescape_to_array`] 还给出其在 `source` 中去掉的 emulation_prevention_three_byte
    /// 的位置（升序）：切片头条目指针按转义字节计数（§7.4.7.1），解码器读的是去转义后的。
    pub fn unescape_to_array_removed(source: &[u8]) -> (Vec<u8>, Vec<usize>) {
        let mut removed = Vec::new();
        let mut buffer = vec![0u8; source.len()];
        let mut written = 0usize;
        let mut zeros = 0usize;
        for (i, &b) in source.iter().enumerate() {
            if zeros >= 2 && b == 3 {
                zeros = 0;
                removed.push(i);
                continue;
            }
            buffer[written] = b;
            written += 1;
            zeros = if b == 0 { zeros + 1 } else { 0 };
        }
        buffer.truncate(written);
        (buffer, removed)
    }

    /// 去转义后的字节（[`Self::unescape`] 的 Vec 变体）。
    pub fn unescape_to_array(source: &[u8]) -> Vec<u8> {
        let mut buffer = vec![0u8; source.len()];
        let length = Self::unescape(source, &mut buffer);
        buffer.truncate(length);
        buffer
    }

    /// 给定 [`Self::unescape_to_array_removed`] 的 removed 位置，求转义字节 `escaped` 的去转义位置；
    /// 该字节本身就是被去掉的转义字节时返回 -1。
    pub fn to_unescaped(escaped: i64, removed: &[usize]) -> i64 {
        let mut before = 0usize;
        while before < removed.len() && (removed[before] as i64) < escaped {
            before += 1;
        }
        if before < removed.len() && removed[before] as i64 == escaped {
            return -1;
        }
        escaped - before as i64
    }

    /// 去转义字节 `unescaped` 的转义位置（[`Self::to_unescaped`] 的逆）。
    pub fn to_escaped(unescaped: i64, removed: &[usize]) -> i64 {
        let mut escaped = unescaped;
        for &position in removed {
            if position as i64 <= escaped {
                escaped += 1;
            } else {
                break;
            }
        }
        escaped
    }

    /// 两字节头起始于 `nal[0]` 的 NAL 单元的 nal_unit_type。
    pub fn nal_type(nal: &[u8]) -> Result<u8, HeifError> {
        if nal.len() < 2 {
            return Err(HeifError::invalid("NAL 单元比它的头还短。"));
        }
        Ok((nal[0] >> 1) & 0x3F)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unescape_removes_emulation_prevention() {
        // 00 00 03 00 01 → 去 EPB 后 00 00 00 01，removed=[2]。
        let source = [0x00u8, 0x00, 0x03, 0x00, 0x01];
        let mut out = vec![0u8; source.len()];
        let len = Nal::unescape(&source, &mut out);
        assert_eq!(&out[..len], &[0x00, 0x00, 0x00, 0x01]);

        let (buf, removed) = Nal::unescape_to_array_removed(&source);
        assert_eq!(buf, vec![0x00, 0x00, 0x00, 0x01]);
        assert_eq!(removed, vec![2]);

        assert_eq!(Nal::unescape_to_array(&source), vec![0x00, 0x00, 0x00, 0x01]);
    }

    #[test]
    fn unescape_only_after_two_zeros() {
        // 1 03（前一个字节非 0）→ 保留 03。
        let source = [0x01u8, 0x03, 0x00, 0x00, 0x03];
        let (buf, removed) = Nal::unescape_to_array_removed(&source);
        assert_eq!(buf, vec![0x01, 0x03, 0x00, 0x00]);
        assert_eq!(removed, vec![4]);
    }

    #[test]
    fn to_unescaped_and_to_escaped_are_inverses() {
        let removed = vec![2usize, 9];
        // 转义位置 0..1（无 removed 前移）、2（removed）、3..8（前移 1）、9（removed）、10..（前移 2）。
        assert_eq!(Nal::to_unescaped(0, &removed), 0);
        assert_eq!(Nal::to_unescaped(2, &removed), -1);
        assert_eq!(Nal::to_unescaped(5, &removed), 4);
        assert_eq!(Nal::to_unescaped(9, &removed), -1);
        assert_eq!(Nal::to_unescaped(10, &removed), 8);

        assert_eq!(Nal::to_escaped(0, &removed), 0);
        assert_eq!(Nal::to_escaped(4, &removed), 5);
        assert_eq!(Nal::to_escaped(8, &removed), 10);
    }

    #[test]
    fn nal_type_known() {
        // SPS 类型 33：首字节 0x42（bit1..6 = 33）。
        assert_eq!(Nal::nal_type(&[0x42, 0x01]).unwrap(), 33);
        // IDR_N_LP 类型 19：0x26。
        assert_eq!(Nal::nal_type(&[0x26, 0x01]).unwrap(), 19);
        // 短于 2 字节 → 报错不 panic。
        assert!(Nal::nal_type(&[0x42]).is_err());
    }
}

