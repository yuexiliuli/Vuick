//! 内存字节读取器 + JPEG Marker 枚举。
//!
//! `ByteReader` 复刻 zune-core `ZReader`（构建于 `ZCursor` 之上）在
//! `std` 下的语义：`read_u8` 在 EOF 返回 0 且不推进、`read_exact_bytes`
//! 失败时指针回退到本次读取起点（供位流 `refill` 4 字节预读失败时回退）、
//! `skip` 允许越过末尾。位流与段扫描共用同一读取器。

use crate::codecs::jpeg::error::DecodeError;

/// 基于内存切片的字节读取器（等价 zune `ZCursor<&[u8]>`）。
pub(crate) struct ByteReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> ByteReader<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        ByteReader { data, pos: 0 }
    }

    /// 当前绝对位置。
    #[allow(dead_code)]
    pub(crate) fn position(&self) -> usize {
        self.pos
    }

    /// 直接设定绝对位置（复用已解析头部状态时把读取器对准熵编码数据起点）。
    pub(crate) fn set_position(&mut self, pos: usize) {
        self.pos = pos;
    }

    /// 读单字节；EOF 返回 0 且不推进（复刻 zune `read_u8`/`read_byte_no_error`）。
    #[inline]
    pub(crate) fn read_u8(&mut self) -> u8 {
        if self.pos < self.data.len() {
            let b = self.data[self.pos];
            self.pos += 1;
            b
        } else {
            0
        }
    }

    /// 是否处于 EOF。
    pub(crate) fn eof(&mut self) -> Result<bool, DecodeError> {
        Ok(self.pos >= self.data.len())
    }

    /// 读单字节，EOF 报错。
    pub(crate) fn read_u8_err(&mut self) -> Result<u8, DecodeError> {
        if self.pos < self.data.len() {
            let b = self.data[self.pos];
            self.pos += 1;
            Ok(b)
        } else {
            Err(DecodeError::Io("read past end of buffer".to_string()))
        }
    }

    /// 精确读取 `buf.len()` 字节；不足时报错且指针回退到本次读取起点。
    pub(crate) fn read_exact_bytes(&mut self, buf: &mut [u8]) -> Result<(), DecodeError> {
        let end = self
            .pos
            .checked_add(buf.len())
            .ok_or_else(|| DecodeError::Io("buffer length overflow".to_string()))?;
        if end > self.data.len() {
            return Err(DecodeError::Io("not enough bytes in buffer".to_string()));
        }
        buf.copy_from_slice(&self.data[self.pos..end]);
        self.pos = end;
        Ok(())
    }

    /// 读取定长 `N` 字节；不足报错且不推进。
    pub(crate) fn read_fixed_bytes_or_error<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        let end = self.pos + N;
        if end > self.data.len() {
            return Err(DecodeError::Io("not enough bytes in buffer".to_string()));
        }
        let mut arr = [0u8; N];
        arr.copy_from_slice(&self.data[self.pos..end]);
        self.pos = end;
        Ok(arr)
    }

    /// 尽量读取（返回实读字节数，不足只读剩余）。
    pub(crate) fn read_bytes(&mut self, buf: &mut [u8]) -> Result<usize, DecodeError> {
        let available = self.data.len().saturating_sub(self.pos);
        let n = buf.len().min(available);
        if n > 0 {
            buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
            self.pos += n;
        }
        Ok(n)
    }

    /// 向后回退 `num` 字节。
    pub(crate) fn rewind(&mut self, num: usize) -> Result<(), DecodeError> {
        self.pos = self
            .pos
            .checked_sub(num)
            .ok_or_else(|| DecodeError::Io("negative seek".to_string()))?;
        Ok(())
    }

    /// 向前跳过 `num` 字节（允许越过末尾）。
    pub(crate) fn skip(&mut self, num: usize) -> Result<(), DecodeError> {
        self.pos += num;
        Ok(())
    }

    /// 从当前位向前 `position` 字节处窥视 `num_bytes`（不推进）。
    pub(crate) fn peek_at(&mut self, position: usize, num_bytes: usize) -> Result<&'a [u8], DecodeError> {
        let start = self.pos + position;
        let end = start.checked_add(num_bytes).ok_or_else(|| {
            DecodeError::Io("peek length overflow".to_string())
        })?;
        if end > self.data.len() {
            return Err(DecodeError::Io("peek out of bounds".to_string()));
        }
        Ok(&self.data[start..end])
    }

    /// 读大端 u16（不足报错）。
    pub(crate) fn get_u16_be_err(&mut self) -> Result<u16, DecodeError> {
        let mut b = [0u8; 2];
        self.read_exact_bytes(&mut b)?;
        Ok(u16::from_be_bytes(b))
    }

    /// 读大端 u16；不足返回 0（复刻 zune `get_u16_be`）。
    pub(crate) fn get_u16_be(&mut self) -> u16 {
        let mut b = [0u8; 2];
        let _ = self.read_bytes(&mut b);
        u16::from_be_bytes(b)
    }
}

/// JPEG Marker（zune-jpeg `Marker` 的等价子集）。
/// 变体名沿用 JPEG 标准段落缩写（SOF/DHT/…），与 `from_u8`/各处 match 的既有命名一致 → 放行。
#[allow(clippy::upper_case_acronyms)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Marker {
    /// Start Of Frame（参数为 SOF 序号 0..15）。
    SOF(u8),
    /// 定义 Huffman 表。
    DHT,
    /// 定义算术编码条件表。
    DAC,
    /// 重启标记（参数为 m & 7）。
    RST(u8),
    /// 图像开始。
    SOI,
    /// 图像结束。
    EOI,
    /// 扫描开始。
    SOS,
    /// 定义量化表。
    DQT,
    /// 定义行数。
    DNL,
    /// 定义重启间隔。
    DRI,
    /// 应用段。
    APP(u8),
    /// 注释。
    COM,
    /// 未知标记。
    UNKNOWN(u8),
}

impl Marker {
    pub(crate) fn from_u8(n: u8) -> Option<Marker> {
        use self::Marker::{APP, COM, DAC, DHT, DNL, DQT, DRI, EOI, RST, SOF, SOI, SOS, UNKNOWN};
        match n {
            0xFE => Some(COM),
            0xC0 => Some(SOF(0)),
            0xC1 => Some(SOF(1)),
            0xC2 => Some(SOF(2)),
            0xC4 => Some(DHT),
            0xCC => Some(DAC),
            0xD0 => Some(RST(0)),
            0xD1 => Some(RST(1)),
            0xD2 => Some(RST(2)),
            0xD3 => Some(RST(3)),
            0xD4 => Some(RST(4)),
            0xD5 => Some(RST(5)),
            0xD6 => Some(RST(6)),
            0xD7 => Some(RST(7)),
            0xD8 => Some(SOI),
            0xD9 => Some(EOI),
            0xDA => Some(SOS),
            0xDB => Some(DQT),
            0xDC => Some(DNL),
            0xDD => Some(DRI),
            0xE0 => Some(APP(0)),
            0xE1 => Some(APP(1)),
            0xE2 => Some(APP(2)),
            0xED => Some(APP(13)),
            0xEE => Some(APP(14)),
            _ => Some(UNKNOWN(n)),
        }
    }
}
