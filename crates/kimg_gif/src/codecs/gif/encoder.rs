//! GIF89a 编码器（移植 SimdCodecs `GifEncoder.cs` + `PaletteQuantizer.cs` 的 Wu 方差量化）。
//!
//! 与参考的差异（对齐 kimg 既有 gif.rs 的流式 API 与旧路径字节结构）：
//! - kimg `GifEncoder` 是流式（每 `encode_frame` 立即写出），故**逐帧局部色表**（同旧 gif crate
//!   路径），不做 C# 的全局调色板 + 变化 bbox 差分；每帧全画布、disposal Background。
//! - 调色板选型：不透明色 + 透明索引装得下 → 精确无损（有序）；否则 Wu 方差量化到 ≤预算色。
//! - `speed`(1..=30) 仅保留签名（API 兼容），当前实现不做抖动（C# `Dither` 默认即关闭），注释说明。
//! - 循环语义直传 kimg：`Repeat::Finite(n≥1)` 存 n、`Infinite` 存 0、`Finite(0)` 不写扩展（同
//!   gif crate `set_repeat` 行为），不套 C# 的 n−1。

use std::io::Write;

use super::Repeat;
use super::lzw::lzw_write_indices;
use kimg_core::animation::Frame;
use kimg_core::error::{ImageError, ImageResult, ParameterError, ParameterErrorKind};

/// RGB u32（R|G<<8|B<<16）。
type Rgb = u32;

#[inline]
fn is_transparent(px: &[u8; 4]) -> bool {
    px[3] < 128
}

/// `GifEncoder` 公共外壳（逐帧流式写出；首帧写签名/逻辑屏/循环扩展）。
pub struct GifEncoder<W: Write> {
    w: Option<W>,
    /// 仅保留 API 签名（构造参数）；当前实现不抖动，speed 不影响输出。
    #[allow(dead_code)]
    speed: i32,
    repeat: Option<Repeat>,
    started: bool,
}

impl<W: Write> GifEncoder<W> {
    /// 以速度 1（质量优先）构造。
    pub fn new(w: W) -> GifEncoder<W> {
        Self::new_with_speed(w, 1)
    }

    /// 指定速度构造（1..=30）。`speed` 仅保留 API 签名：当前实现不抖动编码，
    /// 调色板恒为「精确 or Wu 量化」（C# `Dither` 默认关闭，行为一致），speed 不改变输出。
    pub fn new_with_speed(w: W, speed: i32) -> GifEncoder<W> {
        assert!(
            (1..=30).contains(&speed),
            "speed needs to be in the range [1, 30]"
        );
        GifEncoder {
            w: Some(w),
            speed,
            repeat: None,
            started: false,
        }
    }

    /// 设置循环次数（公开 API 兼容面）。时序保持：首帧写出前生效。
    pub fn set_repeat(&mut self, repeat: Repeat) -> ImageResult<()> {
        self.repeat = Some(repeat);
        Ok(())
    }

    /// 编码一帧动画。
    pub fn encode_frame(&mut self, img_frame: Frame) -> ImageResult<()> {
        let (delay_ms_num, delay_ms_den) = img_frame.delay().numer_denom_ms();
        let frame_delay = delay_ms_num / delay_ms_den;
        let rgba = img_frame.into_buffer();
        let (width, height) = gif_dimensions(rgba.width(), rgba.height())?;
        let w = self
            .w
            .as_mut()
            .ok_or_else(|| ImageError::IoError(std::io::Error::other("encoder 已消费")))?;
        if !self.started {
            write_header(w, width, height, self.repeat)?;
            self.started = true;
        }
        write_frame(w, rgba.as_raw(), width, height, frame_delay)
    }

    /// 编码多帧动画（公开 API 兼容面）。
    pub fn encode_frames<F>(&mut self, frames: F) -> ImageResult<()>
    where
        F: IntoIterator<Item = Frame>,
    {
        for img_frame in frames {
            self.encode_frame(img_frame)?;
        }
        Ok(())
    }
}

impl<W: Write> Drop for GifEncoder<W> {
    /// 末尾写尾块 0x3B（同旧 gif crate `Encoder` 的 Drop 行为；错误静默忽略）。
    fn drop(&mut self) {
        if self.started
            && let Some(w) = self.w.as_mut() {
                let _ = w.write_all(&[0x3B]);
            }
    }
}

fn gif_dimensions(width: u32, height: u32) -> ImageResult<(u16, u16)> {
    let width = u16::try_from(width).ok();
    let height = u16::try_from(height).ok();
    match (width, height) {
        (Some(w), Some(h)) => Ok((w, h)),
        _ => Err(ImageError::Parameter(ParameterError::from_kind(
            ParameterErrorKind::DimensionMismatch,
        ))),
    }
}

fn write_u16(w: &mut impl Write, v: u16) -> ImageResult<()> {
    w.write_all(&v.to_le_bytes()).map_err(ImageError::IoError)
}

fn write_u8(w: &mut impl Write, v: u8) -> ImageResult<()> {
    w.write_all(&[v]).map_err(ImageError::IoError)
}

/// 逻辑屏：无全局色表（packed=0），每帧自带局部色表；随后按需写 NETSCAPE 循环扩展。
fn write_header(
    w: &mut impl Write,
    width: u16,
    height: u16,
    repeat: Option<Repeat>,
) -> ImageResult<()> {
    w.write_all(b"GIF89a").map_err(ImageError::IoError)?;
    write_u16(w, width)?;
    write_u16(w, height)?;
    write_u8(w, 0x00)?; // 无全局色表
    write_u8(w, 0x00)?; // 背景索引（解码端不用于 disposal）
    write_u8(w, 0x00)?; // 无宽高比
    // 循环语义与 gif crate `set_repeat` 一致：Finite(0) 不写扩展、Infinite 存 0、Finite(n≥1) 存 n。
    match repeat {
        Some(Repeat::Finite(0)) | None => {}
        Some(Repeat::Infinite) => write_netscape(w, 0)?,
        Some(Repeat::Finite(n)) => write_netscape(w, n)?,
    }
    Ok(())
}

/// NETSCAPE2.0 循环扩展（计数 = 首播后的重复次数；0 = 无限）。
fn write_netscape(w: &mut impl Write, count: u16) -> ImageResult<()> {
    let mut ext = Vec::with_capacity(19);
    ext.extend_from_slice(&[0x21, 0xFF, 0x0B]);
    ext.extend_from_slice(b"NETSCAPE2.0");
    ext.extend_from_slice(&[0x03, 0x01]);
    ext.extend_from_slice(&count.to_le_bytes());
    ext.push(0x00); // 子块终
    w.write_all(&ext).map_err(ImageError::IoError)
}

/// 写一帧：GCE（当有透明或非零延迟）、图像描述符、局部色表、LZW 数据。
fn write_frame(
    w: &mut impl Write,
    rgba: &[u8],
    width: u16,
    height: u16,
    delay_ms: u32,
) -> ImageResult<()> {
    let (palette, indices, transparent) = quantize(rgba, usize::from(width) * usize::from(height));

    let table_entries = palette.len() + usize::from(transparent.is_some());
    let table_bits = bits_for(table_entries.max(1));
    let min_code = table_bits.max(2) as u8;

    let delay_centis = (delay_ms / 10).min(u16::MAX as u32) as u16;
    if transparent.is_some() || delay_centis > 0 {
        let flags = (2u8 << 2) | u8::from(transparent.is_some());
        let mut ext = vec![0x21, 0xF9, 0x04, flags];
        ext.extend_from_slice(&delay_centis.to_le_bytes());
        ext.push(transparent.unwrap_or(0));
        ext.push(0x00);
        w.write_all(&ext).map_err(ImageError::IoError)?;
    }

    write_u8(w, 0x2C)?;
    write_u16(w, 0)?;
    write_u16(w, 0)?;
    write_u16(w, width)?;
    write_u16(w, height)?;
    write_u8(w, 0x80 | (table_bits as u8 - 1))?; // 有局部色表、非隔行

    // 局部色表（2^table_bits 项 RGB 序：R,G,B；缺项补 0,0,0），随后才是 LZW 最小码宽。
    let mut lct = [0u8; 3 * 256];
    for (i, &c) in palette.iter().enumerate() {
        lct[3 * i] = c as u8;
        lct[3 * i + 1] = (c >> 8) as u8;
        lct[3 * i + 2] = (c >> 16) as u8;
    }
    w.write_all(&lct[..3 << table_bits])
        .map_err(ImageError::IoError)?;
    write_u8(w, min_code)?;

    lzw_write_indices(w, &indices, min_code).map_err(ImageError::IoError)
}

/// 1 → 2^1..2^4 需要的位宽。
fn bits_for(entries: usize) -> usize {
    let mut bits = 1;
    while (1usize << bits) < entries {
        bits += 1;
    }
    bits
}

/// 量化一帧：返回 (调色板 RGB, 每像素索引, 透明索引)。
fn quantize(rgba: &[u8], n_px: usize) -> (Vec<Rgb>, Vec<u8>, Option<u8>) {
    let mut any_transparent = false;
    let mut colors: Vec<Rgb> = Vec::new();
    let mut seen = std::collections::HashSet::with_capacity(256);
    for px in rgba.chunks_exact(4) {
        if is_transparent(px.try_into().unwrap()) {
            any_transparent = true;
        } else {
            let c = rgb_of(px);
            if seen.insert(c) {
                colors.push(c);
            }
        }
    }
    let budget = if any_transparent { 255usize } else { 256usize };

    let (palette, ti, exact) = if colors.len() <= budget {
        // 精确无损。透明索引 = palette.len()（≤255）；全透明则补一个黑项保证色表非空。
        colors.sort_unstable();
        let mut palette = colors;
        let ti = if any_transparent {
            let mut t = palette.len() as u8;
            if palette.is_empty() {
                palette.push(0);
                t = 1;
            }
            Some(t)
        } else {
            None
        };
        (palette, ti, true)
    } else {
        // Wu 方差量化到 ≤budget 色。
        let mut q = WuQuantizer::new();
        for px in rgba.chunks_exact(4) {
            if !is_transparent(px.try_into().unwrap()) {
                q.add(rgb_of(px));
            }
        }
        let palette = q.build(budget);
        let ti = if any_transparent { Some(palette.len() as u8) } else { None };
        (palette, ti, false)
    };

    // 每像素 → 索引。
    let mut indices = vec![0u8; n_px];
    if exact {
        let map: std::collections::HashMap<Rgb, u8> = palette
            .iter()
            .enumerate()
            .map(|(i, &c)| (c, i as u8))
            .collect();
        for (i, px) in rgba.chunks_exact(4).enumerate() {
            indices[i] = if is_transparent(px.try_into().unwrap()) {
                ti.unwrap()
            } else {
                map[&rgb_of(px)]
            };
        }
    } else {
        let cell = build_cell_map(&palette);
        for (i, px) in rgba.chunks_exact(4).enumerate() {
            indices[i] = if is_transparent(px.try_into().unwrap()) {
                ti.unwrap()
            } else {
                let c = rgb_of(px);
                cell[(((c >> 20) & 0xF) * 256 + ((c >> 12) & 0xF) * 16 + ((c >> 4) & 0xF)) as usize]
            };
        }
    }
    (palette, indices, ti)
}

#[inline]
fn rgb_of(px: &[u8]) -> Rgb {
    (u32::from(px[0])) | (u32::from(px[1]) << 8) | (u32::from(px[2]) << 16)
}

/// 16³ 色立方网格 → 最近调色板条目（一次构建 O(4096×P)，每像素 O(1) 查表）。
fn build_cell_map(palette: &[Rgb]) -> [u8; 4096] {
    let mut lut = [0u8; 4096];
    for cell in 0..4096u32 {
        let r0 = ((cell >> 8) & 15) * 16 + 8;
        let g0 = ((cell >> 4) & 15) * 16 + 8;
        let b0 = (cell & 15) * 16 + 8;
        let mut best = 0u8;
        let mut best_d = i64::MAX;
        for (i, &p) in palette.iter().enumerate() {
            let dr = i64::from(r0) - i64::from((p >> 16) as u8);
            let dg = i64::from(g0) - i64::from((p >> 8) as u8);
            let db = i64::from(b0) - i64::from(p as u8);
            let d = dr * dr + dg * dg + db * db;
            if d < best_d {
                best_d = d;
                best = i as u8;
                if d == 0 {
                    break;
                }
            }
        }
        lut[cell as usize] = best;
    }
    lut
}

/// Wu 方差最小量化器（32³ 直方图 + 累积矩 + 最大方差盒分裂），移植 `PaletteQuantizer.cs`。
struct WuQuantizer {
    weight: Vec<i64>,
    red: Vec<i64>,
    green: Vec<i64>,
    blue: Vec<i64>,
    squares: Vec<f64>,
    side: usize,
}

#[derive(Clone, Copy)]
struct BoxR {
    r0: usize,
    r1: usize,
    g0: usize,
    g1: usize,
    b0: usize,
    b1: usize,
}

impl WuQuantizer {
    const SIDE: usize = 33;

    fn new() -> WuQuantizer {
        let n = Self::SIDE * Self::SIDE * Self::SIDE;
        WuQuantizer {
            weight: vec![0i64; n],
            red: vec![0i64; n],
            green: vec![0i64; n],
            blue: vec![0i64; n],
            squares: vec![0f64; n],
            side: Self::SIDE,
        }
    }

    #[inline]
    fn at(&self, r: usize, g: usize, b: usize) -> usize {
        (r * self.side + g) * self.side + b
    }

    fn add(&mut self, rgb: Rgb) {
        let r = ((rgb >> 16) & 255) as usize;
        let g = ((rgb >> 8) & 255) as usize;
        let b = (rgb & 255) as usize;
        let i = self.at((r >> 3) + 1, (g >> 3) + 1, (b >> 3) + 1);
        self.weight[i] += 1;
        self.red[i] += r as i64;
        self.green[i] += g as i64;
        self.blue[i] += b as i64;
        // 二元组平方和
        let (r, g, b) = (r as i64, g as i64, b as i64);
        self.squares[i] += (r * r + g * g + b * b) as f64;
    }

    fn accumulate(&mut self) {
        let s = self.side;
        let acc = |m: &mut [i64]| {
            for r in 1..s {
                for g in 1..s {
                    for b in 1..s {
                        let i = (r * s + g) * s + b;
                        m[i] += m[i - 1];
                    }
                }
            }
            for r in 1..s {
                for b in 1..s {
                    for g in 1..s {
                        let i = (r * s + g) * s + b;
                        m[i] += m[i - s];
                    }
                }
            }
            for g in 1..s {
                for b in 1..s {
                    for r in 1..s {
                        let i = (r * s + g) * s + b;
                        m[i] += m[i - s * s];
                    }
                }
            }
        };
        acc(&mut self.weight);
        acc(&mut self.red);
        acc(&mut self.green);
        acc(&mut self.blue);
        // squares 是 f64：逐轴累积。
        for r in 1..s {
            for g in 1..s {
                for b in 1..s {
                    let i = (r * s + g) * s + b;
                    self.squares[i] += self.squares[i - 1];
                }
            }
        }
        for r in 1..s {
            for b in 1..s {
                for g in 1..s {
                    let i = (r * s + g) * s + b;
                    self.squares[i] += self.squares[i - s];
                }
            }
        }
        for g in 1..s {
            for b in 1..s {
                for r in 1..s {
                    let i = (r * s + g) * s + b;
                    self.squares[i] += self.squares[i - s * s];
                }
            }
        }
    }

    #[inline]
    fn volume(&self, x: &BoxR, m: &[i64]) -> i64 {
        let s = self.side;
        m[(x.r1 * s + x.g1) * s + x.b1] - m[(x.r1 * s + x.g1) * s + x.b0]
            - m[(x.r1 * s + x.g0) * s + x.b1]
            + m[(x.r1 * s + x.g0) * s + x.b0]
            - m[(x.r0 * s + x.g1) * s + x.b1]
            + m[(x.r0 * s + x.g1) * s + x.b0]
            + m[(x.r0 * s + x.g0) * s + x.b1]
            - m[(x.r0 * s + x.g0) * s + x.b0]
    }

    #[inline]
    fn volume_f(&self, x: &BoxR, m: &[f64]) -> f64 {
        let s = self.side;
        m[(x.r1 * s + x.g1) * s + x.b1] - m[(x.r1 * s + x.g1) * s + x.b0]
            - m[(x.r1 * s + x.g0) * s + x.b1]
            + m[(x.r1 * s + x.g0) * s + x.b0]
            - m[(x.r0 * s + x.g1) * s + x.b1]
            + m[(x.r0 * s + x.g1) * s + x.b0]
            + m[(x.r0 * s + x.g0) * s + x.b1]
            - m[(x.r0 * s + x.g0) * s + x.b0]
    }

    #[inline]
    fn variance(&self, x: &BoxR) -> f64 {
        let w = self.volume(x, &self.weight);
        if w == 0 {
            return 0.0;
        }
        let r = self.volume(x, &self.red) as f64;
        let g = self.volume(x, &self.green) as f64;
        let b = self.volume(x, &self.blue) as f64;
        self.volume_f(x, &self.squares) - (r * r + g * g + b * b) / w as f64
    }

    fn try_cut(&self, box_: &BoxR) -> Option<(BoxR, BoxR)> {
        let mut best = f64::NEG_INFINITY;
        let mut out = None;
        let axes = [
            (box_.r0, box_.r1, 0),
            (box_.g0, box_.g1, 1),
            (box_.b0, box_.b1, 2),
        ];
        for (lo, hi, axis) in axes {
            for cut in lo + 1..hi {
                let (a, b) = split(*box_, axis, cut);
                let wa = self.volume(&a, &self.weight);
                let wb = self.volume(&b, &self.weight);
                if wa == 0 || wb == 0 {
                    continue;
                }
                let ra = self.volume(&a, &self.red) as f64;
                let ga = self.volume(&a, &self.green) as f64;
                let ba = self.volume(&a, &self.blue) as f64;
                let rb = self.volume(&b, &self.red) as f64;
                let gb = self.volume(&b, &self.green) as f64;
                let bb = self.volume(&b, &self.blue) as f64;
                let score = (ra * ra + ga * ga + ba * ba) / wa as f64
                    + (rb * rb + gb * gb + bb * bb) / wb as f64;
                if score > best {
                    best = score;
                    out = Some((a, b));
                }
            }
        }
        out
    }

    fn build(&mut self, max_colors: usize) -> Vec<Rgb> {
        self.accumulate();
        let init = BoxR { r0: 0, r1: 32, g0: 0, g1: 32, b0: 0, b1: 32 };
        let mut boxes = vec![init];
        let mut variance = vec![self.variance(&init)];
        while boxes.len() < max_colors {
            let mut pick = None;
            for (i, &v) in variance.iter().enumerate() {
                if v > 0.0 && (pick.is_none() || v > variance[pick.unwrap()]) {
                    pick = Some(i);
                }
            }
            let p = match pick {
                Some(p) => p,
                None => break,
            };
            match self.try_cut(&boxes[p]) {
                Some((lower, upper)) => {
                    boxes[p] = lower;
                    variance[p] = self.variance(&lower);
                    boxes.push(upper);
                    variance.push(self.variance(&upper));
                }
                None => variance[p] = 0.0,
            }
        }
        let mut palette = Vec::new();
        for box_ in &boxes {
            let wgh = self.volume(box_, &self.weight);
            if wgh == 0 {
                continue;
            }
            let r = ((self.volume(box_, &self.red) + wgh / 2) / wgh) as u8;
            let g = ((self.volume(box_, &self.green) + wgh / 2) / wgh) as u8;
            let b = ((self.volume(box_, &self.blue) + wgh / 2) / wgh) as u8;
            palette.push(u32::from(r) << 16 | u32::from(g) << 8 | u32::from(b));
        }
        palette
    }
}

fn split(x: BoxR, axis: usize, cut: usize) -> (BoxR, BoxR) {
    match axis {
        0 => (
            BoxR { r1: cut, ..x },
            BoxR { r0: cut, ..x },
        ),
        1 => (
            BoxR { g1: cut, ..x },
            BoxR { g0: cut, ..x },
        ),
        _ => (
            BoxR { b1: cut, ..x },
            BoxR { b0: cut, ..x },
        ),
    }
}
