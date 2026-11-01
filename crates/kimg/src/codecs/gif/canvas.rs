//! GIF 画布合成（移植 SimdCodecs `GifCanvas.cs`，语义按 kimg 既有 gif.rs 的
//! `blend_and_dispose_pixel` 链对齐）：逐帧把解码索引经 LUT 展开写进输出，同步维护持久画布。
//!
//! 性能手段：
//! - 调色板预先打成 `[u32;256]`（native 序 RGBA，表外=不透明黑），一次；展开每像素一次 u32 写。
//! - 画布为 `Vec<u32>`，整行地址算术 + 直接索引，不走 `get_pixel_mut`/`enumerate_pixels_mut`。
//! - 全覆盖无透明 + Keep 帧走纯 LUT 直写热路径；偏置局部帧先整体 `memcpy` 画布到输出再覆盖 bbox。
//! - LZW 表/码流/索引缓冲跨帧复用（`Scratch`），不逐帧重建。

use std::io;

use super::container::{clip_rect, Disposal, GifFrame};
use super::lzw::{LzwDecoder, gather_sub_blocks};

/// 跨帧复用缓冲：LZW 表 + 码流 + 索引。分配只升不降。
///
/// `lzw` 走堆分配（`Box<LzwDecoder>`）：`LzwDecoder` 内含 4 条 4096 定长表（≈48KB），
/// 若按值嵌入会撑大 `GenericDecoder` 枚举（由最大变体定大小）到 ~49KB——启动首帧
/// 欢迎页经 `ImageReader::decode()` 在 1MB 主线程栈上构造该枚举即爆栈（STATUS_STACK_OVERFLOW，
/// 见 commit 6770194 引入自研 GIF 后的栈回溯）。装箱后 `GifDecoder` ≈ 百字节级，枚举随之缩小。
pub(crate) struct Scratch {
    pub lzw: Box<LzwDecoder>,
    codes: Vec<u8>,
    indices: Vec<u8>,
}

impl Scratch {
    pub fn new() -> Scratch {
        Scratch {
            lzw: Box::new(LzwDecoder::default()),
            codes: Vec::new(),
            indices: Vec::new(),
        }
    }

    /// 收集数据子块并 LZW 解码到 `frame.w*frame.h` 像素。
    /// 数据截断（子块链缺终止字节，或码流提前耗尽）→ `Err(UnexpectedEof)`（对齐 kimg
    /// 「遇 UnexpectedEof 报错、不画 partial」语义）。
    fn decode(&mut self, data: &[u8], frame: &GifFrame) -> io::Result<&[u8]> {
        let (codes, terminated) = gather_sub_blocks(data, frame.data_offset);
        if !terminated {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "GIF 帧数据子块链被截断",
            ));
        }
        self.codes.clear();
        self.codes.extend_from_slice(&codes);
        let total = (frame.w as usize)
            .checked_mul(frame.h as usize)
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "GIF 帧尺寸溢出"))?;
        self.indices.clear();
        self.indices.resize(total, 0);
        let written = self.lzw.decode(&self.codes, frame.min_code_size, &mut self.indices);
        if written < total {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "GIF 帧 LZW 码流中途截断",
            ));
        }
        Ok(&self.indices[..written])
    }
}

/// 持久画布（u32 native 序 RGBA；初始透明黑 0）。
pub(crate) struct Canvas {
    pub width: usize,
    pub height: usize,
    canvas: Vec<u32>,
}

impl Canvas {
    pub fn new(width: usize, height: usize) -> Canvas {
        Canvas {
            width,
            height,
            canvas: vec![0u32; width * height],
        }
    }
}

/// 调色板 LUT：打成 native 序 u32（R|G<<8|B<<16|A<<24，alpha 恒 255）；表外索引=不透明黑。
#[inline]
fn build_palette(data: &[u8], frame: &GifFrame) -> [u32; 256] {
    let mut lut = [0xFF000000u32; 256];
    if frame.palette_offset >= 0 && frame.palette_count > 0 {
        let off = frame.palette_offset as usize;
        let n = frame.palette_count.min(256);
        for i in 0..n {
            let r = data[off + 3 * i];
            let g = data[off + 3 * i + 1];
            let b = data[off + 3 * i + 2];
            lut[i] = u32::from(r) | (u32::from(g) << 8) | (u32::from(b) << 16) | (0xFF << 24);
        }
    }
    lut
}

/// 写 u32 像素到字节输出（native 序；LE 下即 R,G,B,A 四字节）。
#[inline]
fn write_px(out: &mut [u8], px: usize, v: u32) {
    let o = px << 2;
    out[o] = v as u8;
    out[o + 1] = (v >> 8) as u8;
    out[o + 2] = (v >> 16) as u8;
    out[o + 3] = (v >> 24) as u8;
}

/// 画布 u32 内容整拷贝到字节输出（对齐时走 u32 memcpy，否则逐像素）。
fn copy_canvas_into(out: &mut [u8], canvas: &[u32]) {
    debug_assert_eq!(out.len(), canvas.len() * 4);
    // 2024 edition 中 align_to_mut 为 unsafe：仅在 pre/suf 为空（即确实 4 字节对齐）时才使用 mid，
    // 此时得到的是合法的 u32 切片，无别名/对齐问题。
    let (pre, mid, suf) = unsafe { out.align_to_mut::<u32>() };
    if pre.is_empty() && suf.is_empty() {
        mid.copy_from_slice(canvas);
    } else {
        for (i, &v) in canvas.iter().enumerate() {
            write_px(out, i, v);
        }
    }
}

/// 存储行 r（GIF89a Appendix E 隔行）映射到帧内行号。
#[inline]
fn interlaced_row(r: usize, h: usize) -> usize {
    let p1 = (h + 7) / 8;
    if r < p1 {
        return r * 8;
    }
    let r = r - p1;
    let p2 = (h + 3) / 8;
    if r < p2 {
        return 4 + r * 8;
    }
    let r = r - p2;
    let p3 = (h + 1) / 4;
    if r < p3 {
        return 2 + r * 4;
    }
    let r = r - p3;
    1 + r * 2
}

/// 把 `indices` 按帧几何叠绘到已就绪的 `out`（= 画布副本或零底），并（可选）推进持久画布。
///
/// kimg 语义（`blend_and_dispose_pixel`）：`cur` alpha==0（透明索引的原始像素）→ 取画布现像；
/// 之后按本帧 disposal 更新画布：Keep→取合成结果、Background→透明黑、Previous→保持不动。
#[allow(clippy::too_many_arguments)]
fn blit(
    out: &mut [u8],
    mut canvas: Option<&mut [u32]>,
    indices: &[u8],
    frame: &GifFrame,
    lut: &[u32; 256],
    canvas_w: usize,
    canvas_h: usize,
) {
    let (cx, _cy, cw, ch) = clip_rect(frame, canvas_w as u32, canvas_h as u32);
    if cw == 0 || ch == 0 {
        return; // 帧完全在逻辑屏外
    }
    let fw = frame.w as usize;
    let trans = frame.transparent as i64;
    let dispose = frame.disposal;
    let rows = indices.len().div_ceil(fw).min(frame.h as usize);
    let x0 = cx as usize;
    let x1 = (cx + cw) as usize;
    let src_col_start = x0.saturating_sub(frame.x as usize);
    for r in 0..rows {
        let yf = if frame.interlaced { interlaced_row(r, frame.h as usize) } else { r };
        let y = frame.y as usize + yf;
        if y >= canvas_h {
            continue;
        }
        let mut s = r * fw + src_col_start;
        let src_end = (r * fw + fw).min(indices.len());
        let dst = y * canvas_w + x0;
        match &mut canvas {
            Some(c) => {
                for dx in 0..(x1 - x0) {
                    if s >= src_end {
                        break;
                    }
                    let idx = indices[s];
                    let cur = if i64::from(idx) == trans { 0 } else { lut[idx as usize] };
                    s += 1;
                    let prev = c[dst + dx];
                    let outp = if (cur >> 24) == 0 { prev } else { cur };
                    c[dst + dx] = match dispose {
                        Disposal::Keep => outp,
                        Disposal::Background => 0,
                        Disposal::Previous => prev,
                    };
                    write_px(out, dst + dx, outp);
                }
            }
            None => {
                for dx in 0..(x1 - x0) {
                    if s >= src_end {
                        break;
                    }
                    let idx = indices[s];
                    let cur = if i64::from(idx) == trans { 0 } else { lut[idx as usize] };
                    s += 1;
                    let outp = if (cur >> 24) == 0 { 0 } else { cur };
                    write_px(out, dst + dx, outp);
                }
            }
        }
    }
}

/// 迭代器用：合成第 `frame` 帧（画布推进 + 输出全量画布）。`out` 长 == canvas_w*canvas_h*4。
pub(crate) fn composite_into(
    canvas: &mut Canvas,
    scratch: &mut Scratch,
    data: &[u8],
    frame: &GifFrame,
    out: &mut [u8],
) -> io::Result<()> {
    let (w, h) = (canvas.width, canvas.height);
    let indices = scratch.decode(data, frame)?;
    let covers_full =
        frame.x == 0 && frame.y == 0 && frame.w as usize >= w && frame.h as usize >= h;

    if !covers_full {
        // 局部帧：输出先取画布现状，再叠绘 bbox。
        copy_canvas_into(out, &canvas.canvas);
    }
    let lut = build_palette(data, frame);
    blit(
        out,
        Some(&mut canvas.canvas),
        indices,
        frame,
        &lut,
        w,
        h,
    );
    Ok(())
}

/// 静态首帧用（`read_image`，不建持久画布）：前帧恒为透明黑，合成到 `out`（长 == w*h*4）。
pub(crate) fn composite_first_into(
    scratch: &mut Scratch,
    data: &[u8],
    frame: &GifFrame,
    w: usize,
    h: usize,
    out: &mut [u8],
) -> io::Result<()> {
    let indices = scratch.decode(data, frame)?;
    let covers_full = frame.x == 0 && frame.y == 0 && frame.w as usize >= w && frame.h as usize >= h;
    if !covers_full {
        out.fill(0); // 透明黑底
    }
    let lut = build_palette(data, frame);
    blit(out, None, indices, frame, &lut, w, h);
    Ok(())
}
