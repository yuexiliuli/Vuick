//! GIF89a Appendix F 的 LZW 解/编（移植 SimdCodecs `GifLzw.cs` / `GifLzwEncoder.cs`）：
//! 最小码宽码流、LSB-first 位累加、clear 重置、延迟 clear（表冻结 4096）、编码端表领先解码端 1 条。
//!
//! 解码用 4 条定长 4096 数组（Prefix/Suffix/First/Length），**帧间复用不重建**：
//! 每条 decode 只需重写 0..clear 的字面区间，其余槽位随字典定义覆盖写——零重启成本。
//! 位累加器 u64 + 最少 refill（每码仅一次 `while bitCount < codeSize` 补字节）。

/// 收集一帧的数据子块链为连续码流。返回 (码流, 是否带终止 0 字节)；中途截断则取残缺段。
pub(crate) fn gather_sub_blocks(data: &[u8], offset: usize) -> (Vec<u8>, bool) {
    let mut codes = Vec::new();
    let mut pos = offset;
    let mut terminated = false;
    while pos < data.len() {
        let size = data[pos];
        if size == 0 {
            terminated = true;
            break;
        }
        let take = (size as usize).min(data.len() - pos - 1);
        codes.extend_from_slice(&data[pos + 1..pos + 1 + take]);
        pos += 1 + size as usize;
    }
    (codes, terminated)
}

/// LZW 解码器（表跨帧复用）。
pub(crate) struct LzwDecoder {
    prefix: [u16; 4096],
    suffix: [u8; 4096],
    first: [u8; 4096],
    length: [usize; 4096],
}

impl Default for LzwDecoder {
    fn default() -> Self {
        LzwDecoder {
            prefix: [0; 4096],
            suffix: [0; 4096],
            first: [0; 4096],
            length: [0; 4096],
        }
    }
}

impl LzwDecoder {
    /// 解码 `codes` 为调色板索引；返回写入 `output` 的像素数（< output.len = 码流提前耗尽）。
    /// `min_code_size` 须 1..=11，否则返回 0（非法帧，由调用方按解码错误处理）。
    pub fn decode(&mut self, codes: &[u8], min_code_size: u8, output: &mut [u8]) -> usize {
        if !(1..=11).contains(&min_code_size) {
            return 0;
        }
        let clear = 1usize << min_code_size;
        let end = clear + 1;
        for c in 0..clear {
            self.prefix[c] = 0;
            self.suffix[c] = c as u8;
            self.first[c] = c as u8;
            self.length[c] = 1;
        }

        let mut code_size = (min_code_size + 1) as usize;
        let mut next = clear + 2;
        let mut previous = -1isize;
        let mut written = 0usize;
        let mut bits = 0u64;
        let mut bit_count = 0u32;
        let mut position = 0usize;

        while written < output.len() {
            while bit_count < code_size as u32 {
                if position == codes.len() {
                    return written; // 码流耗尽
                }
                bits |= u64::from(codes[position]) << bit_count;
                position += 1;
                bit_count += 8;
            }
            let mask = (1u64 << code_size) - 1;
            let code = (bits & mask) as usize;
            bits >>= code_size;
            bit_count -= code_size as u32;

            if code == clear {
                code_size = (min_code_size + 1) as usize;
                next = clear + 2;
                previous = -1;
                continue;
            }
            if code == end {
                return written;
            }

            if previous < 0 {
                // clear 后（或流首）的首码是单像素。
                if code > clear {
                    return written;
                }
            } else if next < 4096 {
                // 新表项 = 前串 + 本串首像素；`code == next`（KwKwK 情形）时本串首像素 = 前串自身首像素。
                if code > next {
                    return written;
                }
                self.prefix[next] = previous as u16;
                self.first[next] = self.first[previous as usize];
                self.suffix[next] = if code == next {
                    self.first[previous as usize]
                } else {
                    self.first[code]
                };
                self.length[next] = self.length[previous as usize] + 1;
                next += 1;
                if next == 1usize << code_size && code_size < 12 {
                    code_size += 1;
                }
            } else if code >= next {
                // 表已冻结（延迟 clear）且码值未定义：提前返回。
                return written;
            }

            // 从末像素沿 prefix 倒走写字符串；越界的像素跳过但串仍完整遍历。
            let count = self.length[code];
            let end_at = written.saturating_add(count);
            let mut at = end_at.wrapping_sub(1);
            let mut k = code;
            for _ in 0..count {
                if at < output.len() {
                    output[at] = self.suffix[k];
                }
                k = self.prefix[k] as usize;
                at = at.wrapping_sub(1);
            }
            written = end_at.min(output.len());
            previous = code as isize;
        }
        written
    }
}

/// 把每索引 < 2^minCodeSize 的像素流写为子块链（含终止 0 字节），返回后 W 已写完全部数据块。
pub(crate) fn lzw_write_indices<W: std::io::Write>(
    w: &mut W,
    indices: &[u8],
    min_code_size: u8,
) -> std::io::Result<()> {
    let min = min_code_size.clamp(1, 11);
    let clear = 1usize << min;
    let end = clear + 1;
    let mut keys = [0u32; 8192];
    let mut codes = [0u16; 8192];
    let mut writer = SubBlockWriter::new(w);

    // 码宽跟踪：影子解码器（与 `LzwDecoder` 同规则），保证发出码宽 = 解码端读码宽。
    // 每发一条码后 `feed` 它，解码端读这条码时的补表/加宽恰好在此反映。
    let mut shadow = ShadowDec::new(min, clear);
    write_bits(&mut writer, clear as u32, shadow.code_size)?;
    shadow.feed(clear as isize);

    if indices.is_empty() {
        write_bits(&mut writer, end as u32, shadow.code_size)?;
        return writer.finish();
    }

    let mut enc_next = clear + 2;
    let mut prefix = indices[0] as usize;
    for &pixel in &indices[1..] {
        let pixel = pixel as usize;
        let key = (prefix << 8) | pixel;
        let mut slot = hash(key);
        while keys[slot] != 0 && keys[slot] != (key as u32) + 1 {
            slot = (slot + 1) & (8192 - 1);
        }
        if keys[slot] != 0 {
            prefix = codes[slot] as usize;
            continue;
        }

        write_bits(&mut writer, prefix as u32, shadow.code_size)?;
        shadow.feed(prefix as isize);
        keys[slot] = (key as u32) + 1;
        codes[slot] = enc_next as u16;
        enc_next += 1;
        if enc_next == 4096 {
            write_bits(&mut writer, clear as u32, shadow.code_size)?;
            shadow.feed(clear as isize);
            keys.fill(0);
            enc_next = clear + 2;
        }
        prefix = pixel;
    }

    write_bits(&mut writer, prefix as u32, shadow.code_size)?;
    shadow.feed(prefix as isize);
    write_bits(&mut writer, end as u32, shadow.code_size)?;
    writer.finish()
}

#[inline]
fn hash(key: usize) -> usize {
    ((key as u32).wrapping_mul(0x9E3779B1) >> (32 - 13)) as usize
}

/// 解码端状态机的影子（镜像 `LzwDecoder::decode` 的码宽/next 推进规则）。
struct ShadowDec {
    min: u8,
    clear: usize,
    code_size: usize,
    next: usize,
    previous: isize,
}

impl ShadowDec {
    fn new(min: u8, clear: usize) -> ShadowDec {
        ShadowDec {
            min,
            clear,
            code_size: (min + 1) as usize,
            next: clear + 2,
            previous: -1,
        }
    }

    /// 处理一条「解码端将会读到」的码：clear 重置、end 忽略；否则按解码端补表/加宽规则推进。
    fn feed(&mut self, code: isize) {
        if code as usize == self.clear {
            self.code_size = (self.min + 1) as usize;
            self.next = self.clear + 2;
            self.previous = -1;
            return;
        }
        let code = code as usize;
        if self.previous < 0 {
            self.previous = code as isize;
            return;
        }
        if self.next < 4096 {
            self.next += 1;
            if self.next == 1usize << self.code_size && self.code_size < 12 {
                self.code_size += 1;
            }
        }
        self.previous = code as isize;
    }
}

/// 累计位流并打包成 ≤255 字节子块（`GifLzwEncoder.SubBlockWriter`）。
struct SubBlockWriter<'a, W: std::io::Write> {
    w: &'a mut W,
    block: [u8; 255],
    length: usize,
    acc: u64,
    count: u32,
}

impl<'a, W: std::io::Write> SubBlockWriter<'a, W> {
    fn new(w: &'a mut W) -> Self {
        SubBlockWriter {
            w,
            block: [0; 255],
            length: 0,
            acc: 0,
            count: 0,
        }
    }

    fn finish(mut self) -> std::io::Result<()> {
        if self.count > 0 {
            self.put(self.acc as u8)?;
        }
        if self.length > 0 {
            self.w.write_all(&[self.length as u8])?;
            self.w.write_all(&self.block[..self.length])?;
            self.length = 0;
        }
        self.w.write_all(&[0u8])
    }
}

fn write_bits<W: std::io::Write>(s: &mut SubBlockWriter<W>, code: u32, size: usize) -> std::io::Result<()> {
    s.acc |= u64::from(code) << s.count;
    s.count += size as u32;
    while s.count >= 8 {
        s.put(s.acc as u8)?;
        s.acc >>= 8;
        s.count -= 8;
    }
    Ok(())
}

impl<W: std::io::Write> SubBlockWriter<'_, W> {
    #[inline]
    fn put(&mut self, b: u8) -> std::io::Result<()> {
        self.block[self.length] = b;
        self.length += 1;
        if self.length == 255 {
            self.w.write_all(&[255])?;
            self.w.write_all(&self.block)?;
            self.length = 0;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// minCodeSize 2..=11（GIF 图像色表最小码宽 ∈ 2..=8；>8 为非标色表宽度）编码→解码回环。
    #[test]
    fn lzw_roundtrip_wide_min_code_sizes() {
        for min in 2u8..=11u8 {
            let span = 1usize << min;
            if span > 256 {
                continue; // 像素值须 < 2^min；256 之后留给 >8 专项
            }
            // 长度 ~600，含重复段触发 KwKwK 与表扩容。
            let mut px = Vec::new();
            let mut lcg = 0x9E37u64;
            for i in 0..600usize {
                lcg = lcg.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                let v = if i % 7 == 0 {
                    (i % span as usize) as u8
                } else {
                    ((lcg >> 33) % span as u64) as u8
                };
                px.push(v);
            }
            let mut bytes = Vec::new();
            lzw_write_indices(&mut bytes, &px, min).unwrap();
            // 经 gather_sub_blocks 剥离子块帧（多块 >255 字节时含内部分块长度字节）。
            let (payload, term) = gather_sub_blocks(&bytes, 0);
            assert!(term, "min={min} 子块链应干净终止");
            let mut dec = LzwDecoder::default();
            let mut out = vec![0u8; px.len()];
            let written = dec.decode(&payload, min, &mut out);
            out.truncate(written);
            assert_eq!(out, px, "min={min}");
        }
        // >8：min=9..=11（值域 2^9..2^11 的前 300 项）。
        for min in 9u8..=11u8 {
            let mut px = Vec::new();
            for i in 0..300usize {
                px.push((i % (1usize << min)) as u8);
            }
            let mut bytes = Vec::new();
            lzw_write_indices(&mut bytes, &px, min).unwrap();
            let (payload, _term) = gather_sub_blocks(&bytes, 0);
            let mut dec = LzwDecoder::default();
            let mut out = vec![0u8; px.len()];
            let written = dec.decode(&payload, min, &mut out);
            assert_eq!(written, px.len());
            assert_eq!(out, px, "min={min}");
        }
    }
}
