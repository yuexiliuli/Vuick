//! 从一段连续字节里读取 ISOBMFF box（移植 C# `BoxReader.cs`，共享源 → 按 HEIF 命名空间落本 crate）。

/// 四字节四字码（FourCC）常量与辅助函数的宿主。
pub struct BoxReader<'a> {
    data: &'a [u8],
    pos: usize,
    end: usize,
}

/// `TryReadBox` 成功返还的头部内容。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoxHeader {
    /// 4 字节四字码（大端 u32）。
    pub ty: u32,
    /// payload 起始偏移。
    pub payload: usize,
    /// 整个 box 的排他结束偏移。
    pub box_end: usize,
}

impl<'a> BoxReader<'a> {
    /// 在 `data[start..end]` 上初始化读取器。
    pub fn new(data: &'a [u8], start: usize, end: usize) -> Self {
        let end = end.min(data.len());
        BoxReader { data, pos: start, end }
    }

    /// 在整个切片上初始化读取器。
    pub fn new_full(data: &'a [u8]) -> Self {
        BoxReader::new(data, 0, data.len())
    }

    /// 当前读取位置。
    pub fn position(&self) -> usize {
        self.pos
    }

    /// 至少还剩 8 字节可供一个 box 头时返回真。
    pub fn has_box(&self) -> bool {
        self.pos + 8 <= self.end
    }

    /// 读取下一个 box 头（size + type）。
    ///
    /// 流耗尽或 .头非法时返回 `None`；成功时返还 box 头。
    pub fn try_read_box(&mut self) -> Option<BoxHeader> {
        if self.pos + 8 > self.end {
            return None;
        }
        let mut size: i64 = self.read_u32be(self.pos) as i64;
        let ty = self.read_u32be(self.pos + 4);
        let payload;
        if size == 1 {
            // Extended-size：随后 8 字节才是真实 size。
            if self.pos + 16 > self.end {
                return None;
            }
            size = self.read_u64be(self.pos + 8) as i64; // C# unchecked(long)：位保持
            payload = self.pos + 16;
        } else if size == 0 {
            // size=0：box 延伸到容器末尾。
            size = (self.end - self.pos) as i64;
            payload = self.pos + 8;
        } else {
            payload = self.pos + 8;
        }
        if size < 8 || self.pos as i64 + size > self.end as i64 {
            return None;
        }
        let box_end = self.pos + size as usize;
        self.pos = box_end;
        Some(BoxHeader { ty, payload, box_end })
    }

    /// 跳过 FullBox 的 version + flags（4 字节），返回 version 字节。
    /// 必须在 `try_read_box` 之后、该 box 是 FullBox 时调用。
    #[inline]
    pub fn read_full_box_header(&self, payload_start: usize) -> u8 {
        self.data[payload_start]
    }

    // ── 原始读取器（大端，调用方用 box 大小做边界守护）──

    #[inline]
    pub fn read_u32be(&self, offset: usize) -> u32 {
        (u32::from(self.data[offset]) << 24)
            | (u32::from(self.data[offset + 1]) << 16)
            | (u32::from(self.data[offset + 2]) << 8)
            | u32::from(self.data[offset + 3])
    }

    #[inline]
    pub fn read_u64be(&self, offset: usize) -> u64 {
        (u64::from(self.read_u32be(offset)) << 32) | u64::from(self.read_u32be(offset + 4))
    }

    #[inline]
    pub fn read_u16be(&self, offset: usize) -> u16 {
        (u16::from(self.data[offset]) << 8) | u16::from(self.data[offset + 1])
    }

    #[inline]
    pub fn read_u8(&self, offset: usize) -> u8 {
        self.data[offset]
    }

    /// `offset` 处的 4 字节四字码。
    #[inline]
    pub fn four_cc(&self, offset: usize) -> u32 {
        self.read_u32be(offset)
    }

    /// 底层数据切片（C# `Slice`：调用方已按当前 box 做边界守护）。
    #[inline]
    pub fn slice(&self, offset: usize, length: usize) -> &'a [u8] {
        &self.data[offset..offset + length]
    }

    /// 当前 box payload 内剩余字节作为切片。
    #[inline]
    pub fn slice_payload(&self, payload_start: usize, box_end: usize) -> &'a [u8] {
        &self.data[payload_start..box_end]
    }
}

// ── FourCC 四字码常量 ────────────────────────────────────────────────────────

pub const BOX_FTYP: u32 = 0x6679_7470; // 'ftyp'
pub const BOX_META: u32 = 0x6D65_7461; // 'meta'
pub const BOX_HDLR: u32 = 0x6864_6C72; // 'hdlr'
pub const BOX_PICT: u32 = 0x7069_6374; // 'pict'（handler type）
pub const BOX_PITM: u32 = 0x7069_746D; // 'pitm'
pub const BOX_IINF: u32 = 0x6969_6E66; // 'iinf'
pub const BOX_INFE: u32 = 0x696E_6665; // 'infe'
pub const BOX_ILOC: u32 = 0x696C_6F63; // 'iloc'
pub const BOX_IPRP: u32 = 0x6970_7270; // 'iprp'
pub const BOX_IPCO: u32 = 0x6970_636F; // 'ipco'
pub const BOX_IPMA: u32 = 0x6970_6D61; // 'ipma'
pub const BOX_IREF: u32 = 0x6972_6566; // 'iref'
pub const BOX_IDAT: u32 = 0x6964_6174; // 'idat'

/// 由 4 个字节合成四字码（大端）。
pub fn make_four_cc(a: u8, b: u8, c: u8, d: u8) -> u32 {
    (u32::from(a) << 24) | (u32::from(b) << 16) | (u32::from(c) << 8) | u32::from(d)
}

/// 由 4 字节切片合成四字码（大端）。
pub fn make_four_cc_slice(s: &[u8]) -> u32 {
    make_four_cc(s[0], s[1], s[2], s[3])
}
