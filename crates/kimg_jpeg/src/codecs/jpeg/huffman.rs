//! Huffman 表构造（1:1 复刻 zune-jpeg `huffman.rs` 的表构造与哨兵语义）。

use crate::codecs::jpeg::error::DecodeError;

/// 位流解码的预读位数（9→11：慢路径符号全被单探测吞下，仅查表位宽变化、解码环语义零改）。
pub(crate) const HUFF_LOOKAHEAD: u8 = 11;

/// C# Sdcb.SimdCodecs.Jpeg `JpegHuffman` 快速表位宽（JpegHuffman.cs:12 `LutBits=11`）。
pub(crate) const CS_LUT_BITS: u32 = 11;
/// `JpegHuffman.LutFull = 1 << 10`：条目含完整（符号+幅值）解码。
pub(crate) const CS_LUT_FULL: i32 = 1 << 10;
/// 码长字段移位（低 5 位为消耗位数，第 5 位恒 0，`buf <<= (e & 31)` 一次消费）。
pub(crate) const CS_CODELEN_SHIFT: u32 = 6;
/// 有符号幅值字段移位（JpegHuffman.ValueShift=21，宽 11 位）。
pub(crate) const CS_VALUE_SHIFT: u32 = 21;

/// Huffman 表。
pub(crate) struct HuffmanTable {
    /// 码长 k 的最大码值（已预移位 16-k 位）；元素 0 未用。
    pub(crate) maxcode: [i32; 18],
    /// 码长 k 的偏移表；元素 0 未用。
    pub(crate) offset: [i32; 18],
    /// 快速查表；HE `HUFF_LOOKAHEAD` 之上存码长。
    pub(crate) lookup: [i32; 1 << HUFF_LOOKAHEAD],
    /// AC 快速表（decode 与 receive_extend 合体）。
    pub(crate) ac_lookup: Option<[i16; 1 << HUFF_LOOKAHEAD]>,
    /// C#-exact 11 位值打包快速表（`JpegHuffman.BuildLut` 布局）：
    /// 一次探测解出 (低 5 位消耗位数, 位 6-9 码长, 位 10 LutFull, 位 11-18 符号,
    /// 位 21-31 已扩展幅值)；EOB/ZRL(size==0) 单探测。基线热路径专用，
    /// 渐进路径仍走 `lookup`/`ac_lookup`。
    pub(crate) cs_lut: [i32; 1 << CS_LUT_BITS],
    /// 按码长递增排列的符号值。
    pub(crate) values: [u8; 256],
}

impl HuffmanTable {
    pub(crate) fn new(
        codes: &[u8; 17],
        values: [u8; 256],
        is_dc: bool,
        is_progressive: bool,
    ) -> Result<HuffmanTable, DecodeError> {
        let too_long_code = (i32::from(HUFF_LOOKAHEAD) + 1) << HUFF_LOOKAHEAD;
        let mut p = HuffmanTable {
            maxcode: [0; 18],
            offset: [0; 18],
            lookup: [too_long_code; 1 << HUFF_LOOKAHEAD],
            cs_lut: [0; 1 << CS_LUT_BITS],
            values,
            ac_lookup: None,
        };

        p.make_derived_table(is_dc, is_progressive, codes)?;

        Ok(p)
    }

    /// 由段内读到的符号切片构造（补齐至 256）。
    #[allow(dead_code)]
    pub(crate) fn new_unfilled(
        codes: &[u8; 17],
        values: &[u8],
        is_dc: bool,
        is_progressive: bool,
    ) -> Result<HuffmanTable, DecodeError> {
        let mut buf = [0; 256];
        buf[..values.len()].copy_from_slice(values);
        HuffmanTable::new(codes, buf, is_dc, is_progressive)
    }

    /// 计算派生表（maxcode/offset/lookup/ac_lookup）。
    fn make_derived_table(
        &mut self,
        is_dc: bool,
        _is_progressive: bool,
        bits: &[u8; 17],
    ) -> Result<(), DecodeError> {
        // 构建码长表
        let mut huff_size = [0; 257];
        let mut huff_code: [u32; 257] = [0; 257];
        let mut p = 0;

        for (l, &bl) in bits.iter().enumerate().take(16 + 1).skip(1) {
            let mut i = i32::from(bl);
            while i != 0 {
                huff_size[p] = l as u8;
                p += 1;
                i -= 1;
            }
        }

        huff_size[p] = 0;

        let num_symbols = p;
        // 生成码字并校验合法性
        let mut code = 0;
        let mut si = i32::from(huff_size[0]);

        p = 0;

        while huff_size[p] != 0 {
            while i32::from(huff_size[p]) == si {
                huff_code[p] = code;
                code += 1;
                p += 1;
            }
            self.maxcode[si as usize] = (code << (16 - si)) as i32;
            if (code as i32) >= (1 << si) {
                return Err(DecodeError::HuffmanDecode(
                    "Bad Huffman Table".to_string(),
                ));
            }

            code <<= 1;
            si += 1;
        }

        // 生成位序解码表（图 F.15）
        p = 0;

        for (l, &bl) in bits.iter().enumerate().take(16 + 1) {
            if bl == 0 {
                self.maxcode[l] = -1;
            } else {
                self.offset[l] = (p as i32) - (huff_code[p]) as i32;
                p += usize::from(bl);
            }
        }

        self.offset[17] = 0;
        self.maxcode[17] = 0x000F_FFFF;

        // 生成快速查表
        p = 0;

        for l in 1..=HUFF_LOOKAHEAD {
            for _ in 1..=i32::from(bits[usize::from(l)]) {
                let start = (huff_code[p] as usize) << (HUFF_LOOKAHEAD - l);
                for look_bits in (start..).take(1usize << (HUFF_LOOKAHEAD - l)) {
                    self.lookup[look_bits] =
                        (i32::from(l) << HUFF_LOOKAHEAD) | i32::from(self.values[p]);
                }

                p += 1;
            }
        }
        // 生成 C#-exact 11 位值打包快速表（对齐 `JpegHuffman.BuildLut`，JpegHuffman.cs:94）：
        // 每 ≤11 位码占据其全部 2^(11-s) 个前缀槽位；幅值也要得下时把已扩展幅值打包进
        // 高位、置 LutFull，使 (消耗位数, run/size, 幅值) 一次探测解出；size==0（EOB/ZRL）
        // 恒置 LutFull 单探测。布局：位 0-4 消耗位数、位 5 恒 0、位 6-9 码长、位 10 LutFull、
        // 位 11-18 符号、位 21-31 有符号幅值。零 = 码长超窗口（走慢路径）。
        let mut cs_lut = [0i32; 1 << CS_LUT_BITS];
        for i in 0..num_symbols {
            let s = u32::from(huff_size[i]);
            let sym = self.values[i];
            let size = u32::from(sym & 15);
            if s > CS_LUT_BITS {
                continue;
            }
            let first = (huff_code[i] << (CS_LUT_BITS - s)) as usize;
            let span = 1usize << (CS_LUT_BITS - s);
            for j in 0..span {
                let base =
                    (s as i32) | ((s as i32) << CS_CODELEN_SHIFT) | (i32::from(sym) << 11);
                let entry = if size == 0 {
                    base | CS_LUT_FULL
                } else if s + size <= CS_LUT_BITS {
                    // 窗口内幅值位 = 槽位低 size 位（紧跟 s 位码），按 JPEG EXTEND 扩展。
                    let bits = ((j >> (CS_LUT_BITS - s - size)) & ((1usize << size) - 1)) as i32;
                    let v = if bits < (1i32 << (size - 1)) {
                        bits - (1i32 << size) + 1
                    } else {
                        bits
                    };
                    ((s + size) as i32)
                        | ((s as i32) << CS_CODELEN_SHIFT)
                        | CS_LUT_FULL
                        | (i32::from(sym) << 11)
                        | (v << CS_VALUE_SHIFT)
                } else {
                    base
                };
                cs_lut[first + j] = entry;
            }
        }
        self.cs_lut = cs_lut;

        // AC 快速表
        if !is_dc {
            let mut fast = [255; 1 << HUFF_LOOKAHEAD];
            for i in 0..num_symbols {
                let s = huff_size[i];

                if s <= HUFF_LOOKAHEAD {
                    let c = (huff_code[i] << (HUFF_LOOKAHEAD - s)) as usize;
                    let m = (1 << (HUFF_LOOKAHEAD - s)) as usize;

                    for j in 0..m {
                        fast[c + j] = i as i16;
                    }
                }
            }

            let mut fast_ac = [0; 1 << HUFF_LOOKAHEAD];

            for i in 0..(1 << HUFF_LOOKAHEAD) {
                let fast_v = fast[i];

                if fast_v < 255 {
                    let rs = self.values[fast_v as usize];
                    let run = i16::from((rs >> 4) & 15);
                    let mag_bits = i16::from(rs & 15);
                    let len = i16::from(huff_size[fast_v as usize]);

                    if mag_bits != 0 && (len + mag_bits) <= i16::from(HUFF_LOOKAHEAD) {
                        let mut k = (((i as i16) << len) & ((1 << HUFF_LOOKAHEAD) - 1))
                            >> (i16::from(HUFF_LOOKAHEAD) - mag_bits);
                        let m = 1 << (mag_bits - 1);

                        if k < m {
                            k += (!0_i16 << mag_bits) + 1;
                        };

                        if (-128..=127).contains(&k) {
                            fast_ac[i] = (k << 8) + (run << 4) + (len + mag_bits);
                        }
                    }
                }
            }
            self.ac_lookup = Some(fast_ac);
        }

        // 校验 DC 符号范围
        if is_dc {
            for i in 0..num_symbols {
                let sym = self.values[i];

                if sym > 15 {
                    return Err(DecodeError::HuffmanDecode(
                        "Bad Huffman Table".to_string(),
                    ));
                }
            }
        }

        Ok(())
    }
}
