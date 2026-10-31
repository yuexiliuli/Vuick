//! GIF 容器解析（移植 SimdCodecs `GifContainer.cs`）：签名、逻辑屏、全局/局部色表、
//! 图形控制扩展（GCE）、NETSCAPE2.0 循环、数据子块链。不涉及像素解码。
//!
//! 与参考实现的差异（对齐 kimg 既有 gif.rs 的轻语义）：
//! - 无图像块的 GIF（仅逻辑屏 + 尾块）**允许**解析成功（kimg 的 `GifDecoder::new` 对
//!   1x1 空 GIF 可成功构造、帧流为空，见 tests/surface.rs）。
//! - disposal 非 2/3 一律当 `Keep`（含 0=Any 与 4..=7 未定义位）。
//! - 子块链被截断（缺终止 0 字节）不在此报错：记录数据偏移与完整性，由解码侧按
//!   kimg「遇 UnexpectedEof 报错、不画 partial」语义处理。

use crate::error::{ImageError, ImageResult};

/// 像素上限（同 WebP：2^26），单帧 RGBA 至多 256 MiB。
pub(crate) const MAX_PIXELS: u64 = 1 << 26;

/// 帧卸下方式（GIF89a §23，GCE）：非 2/3 一律归一为 `Keep`。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Disposal {
    /// 0（未指定）/1（不卸下）/4..=7（未定义）：帧保留。
    Keep,
    /// 2：清为透明黑。
    Background,
    /// 3：还原为绘制前内容。
    Previous,
}

/// 单帧描述：矩形、色表、控制扩展与 LZW 数据起点（照 `GifFrame.cs`）。
#[derive(Clone)]
pub(crate) struct GifFrame {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    pub interlaced: bool,
    /// 色表起始偏移（-1 = 无，表外索引画不透明黑）。
    pub palette_offset: i64,
    pub palette_count: usize,
    /// LZW 最小码宽（1..=11）。
    pub min_code_size: u8,
    /// 数据子块链起始偏移。
    pub data_offset: usize,
    pub disposal: Disposal,
    pub transparent: i32,
    pub delay_centiseconds: u16,
}

/// 解析后的 GIF 结构。
pub(crate) struct GifContainer {
    pub width: u32,
    pub height: u32,
    pub frames: Vec<GifFrame>,
    /// NETSCAPE2.0 / ANIMEXTS1.0 存储的循环次数（0 = 无限；无扩展 = None）。
    pub loop_count: Option<u16>,
    /// 是否干净收尾（读到尾块 0x3B 或未知引入符「文件到此结束」）。
    /// false = 文件在中途被截断（缺尾块）：迭代器在帧流末尾报 `UnexpectedEof`（对齐旧路径）。
    pub clean_end: bool,
}

/// 钳制到逻辑屏的矩形（x/y 截到屏内，宽高随之收窄；越界区域为空）。
#[inline]
pub(crate) fn clip_rect(frame: &GifFrame, w: u32, h: u32) -> (u32, u32, u32, u32) {
    let x0 = frame.x.min(w);
    let y0 = frame.y.min(h);
    let x1 = frame.x.saturating_add(frame.w).min(w);
    let y1 = frame.y.saturating_add(frame.h).min(h);
    (x0, y0, x1.saturating_sub(x0), y1.saturating_sub(y0))
}

fn invalid(msg: &'static str) -> ImageError {
    ImageError::Decoding(crate::error::DecodingError::new(
        crate::format::ImageFormat::Gif.into(),
        msg,
    ))
}

impl GifContainer {
    /// 解析整个 GIF 字节。非 GIF/过早截断/超像素上限 → `Err`；其余截断宽容处理。
    pub fn parse(data: &[u8]) -> ImageResult<GifContainer> {
        const SIG_87: &[u8] = b"GIF87a";
        const SIG_89: &[u8] = b"GIF89a";
        if data.len() < 6 || (data[..6] != *SIG_87 && data[..6] != *SIG_89) {
            return Err(invalid("不是 GIF：缺少 GIF87a/GIF89a 签名"));
        }
        if data.len() < 13 {
            return Err(invalid("GIF 在逻辑屏描述符中途结束"));
        }
        let rd = |i: usize| u16::from_le_bytes([data[i], data[i + 1]]) as u32;
        let mut width = rd(6);
        let mut height = rd(8);
        let packed = data[10];
        let mut pos = 13usize;
        let (mut global_offset, mut global_count) = (-1i64, 0usize);
        if packed & 0x80 != 0 {
            global_count = (2usize << (packed & 7)) as usize;
            if data.len() - pos < global_count * 3 {
                return Err(invalid("GIF 在全局色表中途结束"));
            }
            global_offset = pos as i64;
            pos += global_count * 3;
        }

        let mut frames: Vec<GifFrame> = Vec::new();
        let mut loop_count: Option<u16> = None;
        let mut disposal = Disposal::Keep;
        let mut transparent = -1i32;
        let mut delay = 0u16;
        let mut clean_end = false;

        while pos < data.len() {
            let introducer = data[pos];
            if introducer == 0x3B {
                clean_end = true; // 尾块
                break;
            }
            if introducer == 0x21 {
                if data.len() - pos < 2 {
                    break;
                }
                let label = data[pos + 1];
                let body = pos + 2;
                if label == 0xF9 && data.len() - body >= 6 && data[body] == 4 {
                    let flags = data[body + 1];
                    let method = (flags >> 2) & 7;
                    disposal = if method == 2 || method == 3 {
                        if method == 2 { Disposal::Background } else { Disposal::Previous }
                    } else {
                        Disposal::Keep
                    };
                    delay = u16::from_le_bytes([data[body + 2], data[body + 3]]);
                    transparent = if flags & 1 != 0 {
                        i32::from(data[body + 4])
                    } else {
                        -1
                    };
                } else if label == 0xFF
                    && data.len() - body >= 12
                    && data[body] == 11
                    && (data[body + 1..body + 12] == *b"NETSCAPE2.0"
                        || data[body + 1..body + 12] == *b"ANIMEXTS1.0")
                {
                    let sub = body + 12;
                    if data.len() - sub >= 4 && data[sub] == 3 && data[sub + 1] == 1 {
                        loop_count = Some(u16::from_le_bytes([data[sub + 2], data[sub + 3]]));
                    }
                } else if label == 0x01 {
                    // 纯文本扩展是图形渲染块：消费挂起的 GCE。
                    disposal = Disposal::Keep;
                    transparent = -1;
                    delay = 0;
                }
                if !skip_sub_blocks(data, body, &mut pos) {
                    break; // 扩展载荷被截断
                }
                continue;
            }

            if introducer != 0x2C {
                clean_end = true; // 非 GIF 可含块：文件到此结束
                break;
            }

            // 图像描述符：left/top/w/h/packed；随后局部色表（若有）与 LZW 最小码宽。
            if data.len() - pos < 11 {
                break;
            }
            let x = rd(pos + 1);
            let y = rd(pos + 3);
            let w = rd(pos + 5);
            let h = rd(pos + 7);
            let ipacked = data[pos + 9];
            pos += 10;
            let (mut poff, mut pcount) = (global_offset, global_count);
            if ipacked & 0x80 != 0 {
                pcount = 2usize << (ipacked & 7);
                if data.len() - pos < pcount * 3 + 1 {
                    break;
                }
                poff = pos as i64;
                pos += pcount * 3;
            }
            if pos >= data.len() {
                break;
            }
            let min_code_size = data[pos];
            pos += 1;
            if u64::from(w) * u64::from(h) > MAX_PIXELS {
                return Err(invalid("GIF 单帧超过像素上限"));
            }

            frames.push(GifFrame {
                x,
                y,
                w,
                h,
                interlaced: ipacked & 0x40 != 0,
                palette_offset: poff,
                palette_count: pcount,
                min_code_size,
                data_offset: pos,
                disposal,
                transparent,
                delay_centiseconds: delay,
            });
            disposal = Disposal::Keep;
            transparent = -1;
            delay = 0;
            if !skip_sub_blocks(data, pos, &mut pos) {
                break; // 数据中途截断：帧保留（解码侧按 EOF 报错）
            }
        }

        if width == 0 || height == 0 {
            for f in &frames {
                width = width.max(f.x.saturating_add(f.w));
                height = height.max(f.y.saturating_add(f.h));
            }
        }
        if width == 0 || height == 0 {
            return Err(invalid("GIF 画布为空：逻辑屏与全部图像均为零尺寸"));
        }
        if u64::from(width) * u64::from(height) > MAX_PIXELS {
            return Err(invalid("GIF 画布超过像素上限"));
        }

        Ok(GifContainer {
            width,
            height,
            frames,
            loop_count,
            clean_end,
        })
    }
}

/// 跳过子块链（长度字节 + 数据 … 至 0）。成功（遇到终止 0）置 `end` 为其后偏移并返回 true。
fn skip_sub_blocks(data: &[u8], mut pos: usize, end: &mut usize) -> bool {
    while pos < data.len() {
        let size = data[pos];
        let next = match pos.checked_add(1 + size as usize) {
            Some(n) => n,
            None => return false,
        };
        if size == 0 {
            *end = next;
            return true;
        }
        pos = next;
    }
    *end = data.len();
    false
}
