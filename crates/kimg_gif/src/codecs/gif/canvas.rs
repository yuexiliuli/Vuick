//! GIF 画布合成（移植 SimdCodecs `GifCanvas.cs`，语义按 kimg 既有 gif.rs 的
//! `blend_and_dispose_pixel` 链对齐）：逐帧把解码索引经 LUT 展开写进输出，同步维护持久画布。
//!
//! 性能手段：
//! - 调色板预先打成 `[u32;256]`（native 序 RGBA，表外=不透明黑），一次；展开每像素一次 u32 写。
//! - 画布为 `Vec<u32>`，整行地址算术 + 直接索引，不走 `get_pixel_mut`/`enumerate_pixels_mut`。
//! - 三路 disposal 快道（`KIMG_GIF_BLIT_FAST=0` 关，dormancy 保留原 copy+blit 双写）：
//!   Keep → 只写画布 u32 1 写/px + 帧末整拷贝（blit_keep）；Previous → 只写 out（trans 读旧画布
//!   现像、非 trans LUT 直写）+ 画布完全不写；Background → 同只写 out + bbox 画布行级整零（memset）。
//!   局部帧 bbox 外靠先整体 memcpy 画布到输出（全覆帧 bbox=全画布可省）；几何与原 blit 逐字节同构。
//! - LZW 表/码流/索引缓冲跨帧复用（`Scratch`），不逐帧重建。

use std::io;

use super::container::{clip_rect, Disposal, GifFrame};
use super::lzw::{LzwDecoder, gather_sub_blocks_into};

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
        if !gather_sub_blocks_into(data, frame.data_offset, &mut self.codes) {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "GIF 帧数据子块链被截断",
            ));
        }
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

/// 全覆 Keep 快道 env 门控：`KIMG_GIF_BLIT_FAST=0` 强制走原标量双写路径（dormancy 保留，A/B 用）。
/// 默认 1。只在 composite 派发层每帧读一次，不进入热循环。
fn blit_fast_enabled() -> bool {
    static ONCE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ONCE.get_or_init(|| {
        std::env::var("KIMG_GIF_BLIT_FAST")
            .map(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
            .unwrap_or(true)
    })
}

/// 任意几何 Keep 帧快道（R1 全覆快道推广到非全覆局部帧）：disposal==Keep 时，
/// **不变式 out 逐像素 == canvas** 成立：
/// - 原路径 bbox 内两处同写 `outp`（canvas[dst]=outp 且 write_px(out,outp)=outp），bbox 外
///   out 维持早先 copy_canvas_into 的画布拷贝 → 处理后整画布 out==canvas（透明像素
///   idx==trans 取 prev=画布现像，Keep 下画布保持该现像 → 跳过即等价；非透明 lut 直写双路同 cur）；
/// - R1 全覆是该不变式的特例（x0=0、cw=w、src_col_start=0）。
/// 故快道只需 u32 1 写/px 推进画布（透明像素跳过=保持画布现像），帧末整画布一次拷贝出输出；
/// 省掉原 blit 每像素的「prev 画布读 + write_px 4 字节写 + disposal 分支」。输出逐字节与原一致。
/// 几何与 `blit` 完全同构（同 clip_rect / src_col_start / src_end 截断 / interlaced_row），
/// 仅省略 out 侧写与 prev 读。Background/Previous 帧不适用（out≠画布，dispatch 层已排除）。
fn blit_keep(
    canvas: &mut Canvas,
    indices: &[u8],
    frame: &GifFrame,
    lut: &[u32; 256],
    w: usize,
    h: usize,
) {
    let (cx, _cy, cw, ch) = clip_rect(frame, w as u32, h as u32);
    if cw == 0 || ch == 0 {
        return; // 帧完全在逻辑屏外（含 x>=w / y>=h 全越界）
    }
    let cv = &mut canvas.canvas;
    let fw = frame.w as usize;
    let trans = frame.transparent; // i32（-1 = 无透明索引；GCE 透明恒 0..=255）
    let rows = indices.len().div_ceil(fw).min(frame.h as usize);
    let x0 = cx as usize;
    let x1 = (cx + cw) as usize;
    let src_col_start = x0.saturating_sub(frame.x as usize);
    for r in 0..rows {
        let yf = if frame.interlaced { interlaced_row(r, frame.h as usize) } else { r };
        let y = frame.y as usize + yf;
        if y >= h {
            continue;
        }
        let src = r * fw + src_col_start;
        let src_end = (r * fw + fw).min(indices.len());
        let dst = y * w + x0;
        for dx in 0..(x1 - x0) {
            if src + dx >= src_end {
                break;
            }
            let idx = indices[src + dx];
            // 透明：画布保持现像（= 原 outp=prev）；非透明：LUT 直写。
            if i32::from(idx) != trans {
                cv[dst + dx] = lut[idx as usize];
            }
        }
    }
}

/// Previous/Background 快道内核（R3，与 blit_keep 几何同构）：只写 out，画布按 disposal 语义推进。
///
/// 几何与 `blit` 完全同构（同 clip_rect / src_col_start / src_end 截断 / interlaced_row /
/// y≥h 跳过），故循环覆盖「原 blit 会处理的像素集」，out 逐字节 == 原 copy+blit 双写路径：
/// - 透明索引（idx==trans）：out = 画布现像（= 原 outp=prev；本行该地址本帧前未写，读到的
///   必是块前值；Previous 画布不写、Background 于同轮最后才写 0，顺序保旧值）；
/// - 非透明：out = LUT 直写（原 cur，alpha 恒 255）。
/// 画布推进：`clear=true`（Background）→ 同轮 bbox 整 0（原 `Disposal::Background => 0`）；
/// `clear=false`（Previous）→ 完全不写（原 `Disposal::Previous => prev`，值上=保持块前状态）。
fn blit_disp(
    out: &mut [u8],
    canvas: &mut [u32],
    indices: &[u8],
    frame: &GifFrame,
    lut: &[u32; 256],
    w: usize,
    h: usize,
    clear: bool,
) {
    let (cx, _cy, cw, ch) = clip_rect(frame, w as u32, h as u32);
    if cw == 0 || ch == 0 {
        return; // 帧完全在逻辑屏外（含 x>=w / y>=h 全越界）
    }
    let fw = frame.w as usize;
    let trans = frame.transparent; // i32（-1 = 无透明索引；GCE 透明恒 0..=255）
    let rows = indices.len().div_ceil(fw).min(frame.h as usize);
    let x0 = cx as usize;
    let x1 = (cx + cw) as usize;
    let width = x1 - x0;
    let src_col_start = x0.saturating_sub(frame.x as usize);
    for r in 0..rows {
        let yf = if frame.interlaced { interlaced_row(r, frame.h as usize) } else { r };
        let y = frame.y as usize + yf;
        if y >= h {
            continue;
        }
        let src = r * fw + src_col_start;
        let src_end = (r * fw + fw).min(indices.len());
        let dst = y * w + x0;
        let mut n = 0; // 本行已处理像素数；Background 画布也**只清这些**（与 blit 截断语义一致）
        for dx in 0..width {
            if src + dx >= src_end {
                break;
            }
            let idx = indices[src + dx];
            if i32::from(idx) == trans {
                // 透明：out = 画布现像（先读旧值；Background 的行级整清在本行写完后才发生）。
                write_px(out, dst + dx, canvas[dst + dx]);
            } else {
                write_px(out, dst + dx, lut[idx as usize]);
            }
            n += 1;
        }
        if clear && n > 0 {
            // Background：bbox 清透明黑——行级整零（memset 级 vectorized），非逐像素散写；
            // 只清已处理列，防止把「截断未处理像素」保留旧值的语义清掉。
            canvas[dst..dst + n].fill(0);
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

    let lut = build_palette(data, frame);
    if blit_fast_enabled() {
        match frame.disposal {
            // 任意几何 Keep 快道（R2）：只写画布 + 帧末一次整画布拷贝，字节恒等
            // （见 blit_keep 不变式：Keep 下处理后画布==输出，含非全覆局部帧）。
            Disposal::Keep => {
                blit_keep(canvas, indices, frame, &lut, w, h);
                copy_canvas_into(out, &canvas.canvas);
                return Ok(());
            }
            // R3：Previous/Background 快道——只写 out（bbox 内 trans 读旧画布、非 trans LUT
            // 直写；bbox 外靠 copy_canvas_into，全覆帧 bbox=全画布可省拷贝），画布按语义推进
            // （Previous 不写 / Background bbox 清 0）。逐字节同构原 copy+blit 双写（blit_disp）。
            Disposal::Previous | Disposal::Background => {
                if !covers_full {
                    copy_canvas_into(out, &canvas.canvas);
                }
                blit_disp(
                    out,
                    &mut canvas.canvas,
                    indices,
                    frame,
                    &lut,
                    w,
                    h,
                    frame.disposal == Disposal::Background,
                );
                return Ok(());
            }
        }
    }

    if !covers_full {
        // 局部帧（非 Keep/Previous/Background 或快道关）：输出先取画布现状，再叠绘 bbox。
        copy_canvas_into(out, &canvas.canvas);
    }
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
    // covers_full 时 blit(None) 全量覆盖每像素：非透明写 cur、透明写 0，与透明黑底 fill(0)
    // 逐字节等价 → 跳过 fill。仅局部帧才需先清透明黑底。
    if !covers_full {
        out.fill(0); // 透明黑底
    }
    let lut = build_palette(data, frame);
    blit(out, None, indices, frame, &lut, w, h);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::lzw::lzw_write_indices;
    use super::*;

    /// LCG 确定性伪随机（同 kimg 惯例），避免真随机不稳定的种子依赖。
    fn lcg(seed: &mut u64) -> u64 {
        *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        *seed >> 33
    }

    struct Case<'a> {
        w: usize,
        h: usize,
        indices: Vec<u8>,
        frame: GifFrame,
        lut: [u32; 256],
        label: &'a str,
        /// 预置非零画布（模拟前帧留下的持久画布内容；两路同种子同填，强化「透明保留 prev」语义）。
        preseed: bool,
    }

    /// 双路对拍：原路径（copy+blit 双写）vs 快道（blit_keep + 帧末整拷贝）。
    /// 断言两路输出字节一致且画布终态一致（对「快道适用」帧，原本就是唯一正确结果）。
    fn assert_fast_parity(c: &Case) {
        // 参照路：原 composite_into 行为（decode 外置，直接喂 indices）。
        let mut ref_canvas = Canvas::new(c.w, c.h);
        let mut ref_out = vec![0u8; c.w * c.h * 4];
        if c.preseed {
            let mut p = 0xABCD_EF01u64;
            for v in ref_canvas.canvas.iter_mut() {
                *v = lcg(&mut p) as u32;
            }
        }
        if !(c.frame.x == 0 && c.frame.y == 0 && c.frame.w as usize >= c.w && c.frame.h as usize >= c.h)
        {
            copy_canvas_into(&mut ref_out, &ref_canvas.canvas);
        }
        blit(
            &mut ref_out,
            Some(&mut ref_canvas.canvas),
            &c.indices,
            &c.frame,
            &c.lut,
            c.w,
            c.h,
        );

        // 快道路。
        let mut fast_canvas = Canvas::new(c.w, c.h);
        let mut fast_out = vec![0u8; c.w * c.h * 4];
        if c.preseed {
            let mut p = 0xABCD_EF01u64;
            for v in fast_canvas.canvas.iter_mut() {
                *v = lcg(&mut p) as u32;
            }
        }
        blit_keep(&mut fast_canvas, &c.indices, &c.frame, &c.lut, c.w, c.h);
        copy_canvas_into(&mut fast_out, &fast_canvas.canvas);

        assert_eq!(
            ref_out, fast_out,
            "[{}] 输出字节不一致",
            c.label
        );
        assert_eq!(
            ref_canvas.canvas, fast_canvas.canvas,
            "[{}] 画布终态不一致",
            c.label
        );
    }

    fn opaque_lut(seed: &mut u64) -> [u32; 256] {
        let mut lut = [0u32; 256];
        for i in 0..256 {
            // alpha 恒 0xFF（native u32 RGBA），与 build_palette 一致。
            lut[i] = (lcg(seed) as u32 & 0xFFFFFF) | 0xFF << 24;
        }
        lut
    }

    fn frame(x: u32, y: u32, w: u32, h: u32, disposal: Disposal, transparent: i32, interlaced: bool) -> GifFrame {
        GifFrame {
            x,
            y,
            w,
            h,
            interlaced,
            palette_offset: -1,
            palette_count: 0,
            min_code_size: 8,
            data_offset: 0,
            disposal,
            transparent,
            delay_centiseconds: 0,
        }
    }

    fn index_grid(seed: &mut u64, len: usize, span: u32) -> Vec<u8> {
        (0..len).map(|_| (lcg(seed) % u64::from(span)) as u8).collect()
    }

    #[test]
    fn blit_keep_matches_original() {
        let mut seed = 0x1234_5678;
        let mut cases: Vec<Case> = Vec::new();

        // 1) 全覆 Keep 不透明（无 GCE）。
        cases.push(Case {
            w: 8,
            h: 8,
            indices: index_grid(&mut seed, 8 * 8, 256),
            frame: frame(0, 0, 8, 8, Disposal::Keep, -1, false),
            lut: opaque_lut(&mut seed),
            label: "full_keep_opaque",
            preseed: false,
        });
        // 2) 全覆 Keep + 透明索引打孔（透明像素须保留画布现像）。
        cases.push(Case {
            w: 16,
            h: 12,
            indices: index_grid(&mut seed, 16 * 12, 16),
            frame: frame(0, 0, 16, 12, Disposal::Keep, 3, false),
            lut: opaque_lut(&mut seed),
            label: "full_keep_trans_holes",
            preseed: false,
        });
        // 3) 全覆 Keep + 透明 + 隔行（interlaced_row 走查）。
        cases.push(Case {
            w: 21,
            h: 17,
            indices: index_grid(&mut seed, 21 * 17, 8),
            frame: frame(0, 0, 21, 17, Disposal::Keep, 0, true),
            lut: opaque_lut(&mut seed),
            label: "full_keep_trans_interlaced",
            preseed: false,
        });
        // 4) 帧矩形大于逻辑屏（全覆但超尺寸：indices=fw*fh，只消费 w×h）。
        cases.push(Case {
            w: 10,
            h: 10,
            indices: index_grid(&mut seed, 20 * 15, 32),
            frame: frame(0, 0, 20, 15, Disposal::Keep, 7, false),
            lut: opaque_lut(&mut seed),
            label: "full_keep_oversized_frame",
            preseed: false,
        });
        // 5) 大尺寸纯不透明（接近真实 001__anim_opaque）。
        cases.push(Case {
            w: 320,
            h: 240,
            indices: index_grid(&mut seed, 320 * 240, 256),
            frame: frame(0, 0, 320, 240, Disposal::Keep, -1, false),
            lut: opaque_lut(&mut seed),
            label: "full_keep_320x240",
            preseed: false,
        });
        // 6) 透明打孔低频（1% 透明索引，覆盖「透明像素在循环中稀疏出现」）。
        let mut idx = vec![1u8; 320 * 240];
        for i in (0..320 * 240).step_by(100) {
            idx[i] = 0;
        }
        cases.push(Case {
            w: 320,
            h: 240,
            indices: idx,
            frame: frame(0, 0, 320, 240, Disposal::Keep, 0, false),
            lut: opaque_lut(&mut seed),
            label: "full_keep_sparse_trans",
            preseed: false,
        });

        // 7) 局部 Keep 不透明（x>0/y>0 偏移，bbox 内叠绘、bbox 外保留画布）。
        cases.push(Case {
            w: 16,
            h: 16,
            indices: index_grid(&mut seed, 6 * 5, 256),
            frame: frame(2, 3, 6, 5, Disposal::Keep, -1, false),
            lut: opaque_lut(&mut seed),
            label: "partial_keep_opaque",
            preseed: true,
        });
        // 8) 局部 Keep + 透明打孔（透明像素保留非零画布现像并入 out 拷贝）。
        cases.push(Case {
            w: 20,
            h: 16,
            indices: index_grid(&mut seed, 9 * 8, 16),
            frame: frame(4, 2, 9, 8, Disposal::Keep, 2, false),
            lut: opaque_lut(&mut seed),
            label: "partial_keep_trans_holes",
            preseed: true,
        });
        // 9) 局部 Keep + 透明 + 隔行（偏移下 interlaced_row 命中裁剪行）。
        cases.push(Case {
            w: 24,
            h: 20,
            indices: index_grid(&mut seed, 10 * 9, 8),
            frame: frame(3, 5, 10, 9, Disposal::Keep, 0, true),
            lut: opaque_lut(&mut seed),
            label: "partial_keep_trans_interlaced",
            preseed: true,
        });
        // 10) 局部 Keep 越界底右角（帧矩形伸到屏外，clip_rect 收窄 bbox）。
        cases.push(Case {
            w: 10,
            h: 10,
            indices: index_grid(&mut seed, 10 * 10, 64),
            frame: frame(8, 8, 10, 10, Disposal::Keep, -1, false),
            lut: opaque_lut(&mut seed),
            label: "partial_keep_clip_tr",
            preseed: true,
        });
        // 11) 局部 Keep 紧贴顶左缘 + 透明（x0=src_col_start 零偏走查）。
        cases.push(Case {
            w: 12,
            h: 12,
            indices: index_grid(&mut seed, 4 * 4, 8),
            frame: frame(1, 1, 4, 4, Disposal::Keep, 1, false),
            lut: opaque_lut(&mut seed),
            label: "partial_keep_edge_tl",
            preseed: true,
        });
        // 12) 局部 Keep 完全越界（clip 空矩形 → 两路均 no-op，画布原样拷出）。
        cases.push(Case {
            w: 8,
            h: 8,
            indices: index_grid(&mut seed, 4 * 4, 16),
            frame: frame(9, 9, 4, 4, Disposal::Keep, -1, false),
            lut: opaque_lut(&mut seed),
            label: "partial_keep_offscreen",
            preseed: true,
        });
        // 13) 局部 Keep 帧矩形大于逻辑屏但带偏移（非全覆：clip 到屏内，src 只采首列）。
        cases.push(Case {
            w: 8,
            h: 8,
            indices: index_grid(&mut seed, 20 * 20, 32),
            frame: frame(2, 2, 20, 20, Disposal::Keep, -1, false),
            lut: opaque_lut(&mut seed),
            label: "partial_keep_oversized_offset",
            preseed: true,
        });
        // 14) 局部 Keep + 索引截断（indices < fw*fh：两路 src_end 截断须逐字节一致）。
        cases.push(Case {
            w: 8,
            h: 8,
            indices: index_grid(&mut seed, 30, 64), // 6*6=36 缺 6 个
            frame: frame(1, 1, 6, 6, Disposal::Keep, 5, false),
            lut: opaque_lut(&mut seed),
            label: "partial_keep_truncated",
            preseed: true,
        });
        // 15) 全覆 Keep + 透明打孔 + 预置非零画布（透明保留 prev 在非零底上直断言——
        //     与第 2/3 例的区别：全覆透明此前只在零画布对拍过，preseed 暴露「透明误清零/误覆写」类错）。
        cases.push(Case {
            w: 24,
            h: 18,
            indices: index_grid(&mut seed, 24 * 18, 20),
            frame: frame(0, 0, 24, 18, Disposal::Keep, 2, false),
            lut: opaque_lut(&mut seed),
            label: "full_keep_preseed_trans",
            preseed: true,
        });

        for c in &cases {
            assert_fast_parity(c);
        }
    }

    /// `composite_into` 派发分支直测：用 `lzw_write_indices` 合成真实可解码帧数据（子块链 +
    /// 终止 0，data_offset=0 / min_code_size=8），Keep 帧经派发走快道、Background/Previous 走
    /// 原路，各与「手动原路径（copy-if-partial + blit）」对拍输出与画布逐字节一致——直接执行
    /// `composite_into` 的派发分支体（非复用下层函数），两态 env 下均成立（Keep 在快道关时
    /// 落入原路、参照相同，断言依然成立）。
    #[test]
    fn composite_into_dispatch_parity() {
        let mut seed = 0x1357_9BDFu64;
        let (cw, ch) = (16u32, 12u32);
        let (fw, fh) = (8u32, 6u32); // 局部帧（非全覆，x=2,y=1）
        let indices = index_grid(&mut seed, (fw * fh) as usize, 32);
        let mut data = Vec::new();
        lzw_write_indices(&mut data, &indices, 8).unwrap();

        // 经派发跑 `composite_into`。
        let dispatch = |d: Disposal| {
            let f = frame(2, 1, fw, fh, d, 3, false); // palette=-1 → LUT=[0xFF000000;256]
            let mut canvas = Canvas::new(cw as usize, ch as usize);
            let mut out = vec![0u8; (cw * ch * 4) as usize];
            composite_into(&mut canvas, &mut Scratch::new(), &data, &f, &mut out).unwrap();
            (out, canvas.canvas)
        };
        // 手动原路径参照（局部帧先 copy 画布到 out 再 blit 双写）。
        let manual = |d: Disposal| {
            let f = frame(2, 1, fw, fh, d, 3, false);
            let mut canvas = Canvas::new(cw as usize, ch as usize);
            let mut out = vec![0u8; (cw * ch * 4) as usize];
            copy_canvas_into(&mut out, &canvas.canvas);
            blit(
                &mut out,
                Some(&mut canvas.canvas),
                &indices,
                &f,
                &[0xFF000000u32; 256],
                cw as usize,
                ch as usize,
            );
            (out, canvas.canvas)
        };

        for (d, name) in [
            (Disposal::Keep, "Keep"),
            (Disposal::Background, "Background"),
            (Disposal::Previous, "Previous"),
        ] {
            let (d_out, d_cv) = dispatch(d);
            let (m_out, m_cv) = manual(d);
            assert_eq!(d_out, m_out, "[{name}] composite_into 派发输出 != 原路径参照");
            assert_eq!(d_cv, m_cv, "[{name}] composite_into 派发画布 != 原路径参照");
            if d == Disposal::Keep {
                // Keep 不变式经真实派发成立：out 逐字节 == 画布（快道/原路同满足）。
                let mut probe = vec![0u8; (cw * ch * 4) as usize];
                copy_canvas_into(&mut probe, &d_cv);
                assert_eq!(probe, d_out, "[Keep] 派发后 out != 画布（不变式破）");
            }
        }
    }

    /// Previous/Background 双路对拍：原路径（copy-if-partial + blit 双写）vs 快道（blit_disp）。
    /// 断言两路 `out` 逐字节一致且画布终态一致（Background：bbox 清透明黑；Previous：保持不变
    /// ——值上=块前状态，快道完全不写）。覆盖：全覆/局部、透明打孔、preseed 非零、interlaced、
    /// 裁边（越界底右角/完全越界空 clip）、索引截断、320x240 近真实；两 disposal 各跑一遍。
    fn assert_disp_parity(c: &Case, d: Disposal, label: &str) {
        let clear = d == Disposal::Background;
        let full =
            c.frame.x == 0 && c.frame.y == 0 && c.frame.w as usize >= c.w && c.frame.h as usize >= c.h;
        let mut f = c.frame.clone();
        f.disposal = d;

        // 参照路：原 composite_into（局部帧先 copy 画布到 out 再 blit 双写）。
        let mut ref_canvas = Canvas::new(c.w, c.h);
        let mut ref_out = vec![0u8; c.w * c.h * 4];
        if c.preseed {
            let mut p = 0xABCD_EF01u64;
            for v in ref_canvas.canvas.iter_mut() {
                *v = lcg(&mut p) as u32;
            }
        }
        if !full {
            copy_canvas_into(&mut ref_out, &ref_canvas.canvas);
        }
        blit(
            &mut ref_out,
            Some(&mut ref_canvas.canvas),
            &c.indices,
            &f,
            &c.lut,
            c.w,
            c.h,
        );

        // 快道路（blit_disp）。
        let mut fast_canvas = Canvas::new(c.w, c.h);
        let mut fast_out = vec![0u8; c.w * c.h * 4];
        if c.preseed {
            let mut p = 0xABCD_EF01u64;
            for v in fast_canvas.canvas.iter_mut() {
                *v = lcg(&mut p) as u32;
            }
        }
        if !full {
            copy_canvas_into(&mut fast_out, &fast_canvas.canvas);
        }
        blit_disp(
            &mut fast_out,
            &mut fast_canvas.canvas,
            &c.indices,
            &f,
            &c.lut,
            c.w,
            c.h,
            clear,
        );

        assert_eq!(ref_out, fast_out, "[{label}] 输出字节不一致");
        assert_eq!(ref_canvas.canvas, fast_canvas.canvas, "[{label}] 画布终态不一致");
        if d == Disposal::Previous {
            // Previous 画布=块前状态：与输入画布逐字节一致（快道未写画布）。
            let mut probe = Canvas::new(c.w, c.h);
            if c.preseed {
                let mut p = 0xABCD_EF01u64;
                for v in probe.canvas.iter_mut() {
                    *v = lcg(&mut p) as u32;
                }
            }
            assert_eq!(probe.canvas, fast_canvas.canvas, "[{label}] Previous 画布被改动");
        } else {
            // Background：bbox 内处理过的像素已清透明黑（0x00000000），bbox 外/未处理像素保留。
            for (i, (&refv, &fastv)) in ref_canvas.canvas.iter().zip(fast_canvas.canvas.iter()).enumerate() {
                assert_eq!(refv, fastv, "[{label}] Background 画布 @px {i}");
            }
        }
    }

    /// Previous/Background 快道直拍：`blit_disp` vs 原 `blit` 双写，逐字节对拍 + 画布终态断言，
    /// 两 disposal、多样例（部分共享 Keep 的几何模板但换 disposal，扩大覆盖）。
    #[test]
    fn blit_disp_matches_original() {
        let mut seed = 0xFEDC_BA98u64;
        let cases: Vec<Case> = vec![
            // 全覆 Previous/Background 不透明 + preseed（画布旧值须保留到 out 之外无扰）。
            Case {
                w: 16,
                h: 12,
                indices: index_grid(&mut seed, 16 * 12, 256),
                frame: frame(0, 0, 16, 12, Disposal::Previous, -1, false),
                lut: opaque_lut(&mut seed),
                label: "full_disp_preseed_opaque",
                preseed: true,
            },
            // 全覆 + 透明打孔 + preseed（非零底上透明保留旧画布/背景清 0）。
            Case {
                w: 24,
                h: 18,
                indices: index_grid(&mut seed, 24 * 18, 20),
                frame: frame(0, 0, 24, 18, Disposal::Previous, 2, false),
                lut: opaque_lut(&mut seed),
                label: "full_disp_preseed_trans",
                preseed: true,
            },
            // 全覆 + 透明 + 隔行。
            Case {
                w: 21,
                h: 17,
                indices: index_grid(&mut seed, 21 * 17, 8),
                frame: frame(0, 0, 21, 17, Disposal::Previous, 0, true),
                lut: opaque_lut(&mut seed),
                label: "full_disp_trans_interlaced",
                preseed: false,
            },
            // 局部 + 透明打孔 + preseed（bbox 外保留画布拷贝，bbox 内 trans 读旧画布）。
            Case {
                w: 20,
                h: 16,
                indices: index_grid(&mut seed, 9 * 8, 16),
                frame: frame(4, 2, 9, 8, Disposal::Previous, 2, false),
                lut: opaque_lut(&mut seed),
                label: "partial_disp_trans_preseed",
                preseed: true,
            },
            // 局部 + 透明 + 隔行（偏移下 interlaced_row 命中裁剪行）。
            Case {
                w: 24,
                h: 20,
                indices: index_grid(&mut seed, 10 * 9, 8),
                frame: frame(3, 5, 10, 9, Disposal::Previous, 0, true),
                lut: opaque_lut(&mut seed),
                label: "partial_disp_trans_interlaced",
                preseed: true,
            },
            // 局部越界底右角（clip_rect 收窄 bbox）。
            Case {
                w: 10,
                h: 10,
                indices: index_grid(&mut seed, 10 * 10, 64),
                frame: frame(8, 8, 10, 10, Disposal::Previous, -1, false),
                lut: opaque_lut(&mut seed),
                label: "partial_disp_clip_tr",
                preseed: true,
            },
            // 局部完全越界（空 clip → 两路 no-op，out 全画布、画布不变）。
            Case {
                w: 8,
                h: 8,
                indices: index_grid(&mut seed, 4 * 4, 16),
                frame: frame(9, 9, 4, 4, Disposal::Previous, -1, false),
                lut: opaque_lut(&mut seed),
                label: "partial_disp_offscreen",
                preseed: true,
            },
            // 局部 + 索引截断（src_end 截断：未处理像素画布/out 均不动）。
            Case {
                w: 8,
                h: 8,
                indices: index_grid(&mut seed, 30, 64),
                frame: frame(1, 1, 6, 6, Disposal::Previous, 5, false),
                lut: opaque_lut(&mut seed),
                label: "partial_disp_truncated",
                preseed: true,
            },
            // 大尺寸透明打孔稀疏（近真实 screen anim 形）。
            Case {
                w: 320,
                h: 240,
                indices: {
                    let mut sparse = vec![1u8; 320 * 240];
                    for i in (0..320 * 240).step_by(100) {
                        sparse[i] = 0;
                    }
                    sparse
                },
                frame: frame(0, 0, 320, 240, Disposal::Previous, 0, false),
                lut: opaque_lut(&mut seed),
                label: "full_disp_320x240_sparse_trans",
                preseed: false,
            },
        ];

        for d in [Disposal::Previous, Disposal::Background] {
            for c in &cases {
                assert_disp_parity(c, d, &format!("{}:{}", match d {
                    Disposal::Previous => "Previous",
                    Disposal::Background => "Background",
                    Disposal::Keep => "Keep",
                }, c.label));
            }
        }
    }

    /// SIMD 调色板展开可行性 microbench（release 跑，非默认测试）：
    /// `cargo test -p kimg_gif --release --lib -- --ignored blit_simd_headroom --nocapture --test-threads=1`
    /// 快道内层循环（全覆 Keep：`if idx != trans { dst[i] = lut[idx] }`）标量 vs AVX2 vpgatherdd 中位。
    /// 结论用于决定是否接入 SIMD（K107 门槛：需 ≥1.5× 且逐字节恒等，否则不硬投）。
    #[test]
    #[ignore = "仅 SIMD 决策用，release + --ignored 显式跑"]
    fn blit_simd_headroom() {
        if !is_x86_feature_detected!("avx2") {
            eprintln!("SIMD headroom: 无 AVX2，跳过");
            return;
        }
        const N: usize = 4 * 1024 * 1024;
        let mut seed = 0xDEAD_BEEFu64;
        let idx: Vec<u8> = (0..N)
            .map(|_| {
                seed = seed.wrapping_mul(0x9E3779B97F4A7C15) ^ (seed >> 29);
                (seed % 256) as u8
            })
            .collect();
        let lut: [u32; 256] = {
            let mut l = [0u32; 256];
            for i in 0..256 {
                l[i] = ((i as u32) * 2654435761) | 0xFF << 24;
            }
            l
        };
        let trans = 0u8;

        let mut dst_s = vec![0u32; N];
        let mut dst_v = vec![0u32; N];

        fn scalar_expand(idx: &[u8], lut: &[u32; 256], trans: u8, dst: &mut [u32]) {
            for i in 0..idx.len() {
                let v = idx[i];
                if v != trans {
                    dst[i] = lut[v as usize];
                }
            }
        }

        scalar_expand(&idx, &lut, trans, &mut dst_s);
        unsafe {
            lut_expand_avx2(&idx, &lut, trans, &mut dst_v);
        }
        assert_eq!(dst_s, dst_v, "SIMD 与标量逐字节不一致");

        let median = |f: &mut dyn FnMut()| {
            f();
            let mut acc = Vec::new();
            for _ in 0..9 {
                let t = std::time::Instant::now();
                f();
                acc.push(t.elapsed().as_secs_f64());
            }
            acc.sort_by(|a, b| a.partial_cmp(b).unwrap());
            acc[acc.len() / 2]
        };
        let s = median(&mut || {
            scalar_expand(&idx, &lut, trans, &mut dst_s);
            std::hint::black_box(&dst_s[0]);
        });
        let v = median(&mut || {
            unsafe {
                lut_expand_avx2(&idx, &lut, trans, &mut dst_v);
            }
            std::hint::black_box(&dst_v[0]);
        });
        eprintln!(
            "SIMD headroom: N={N} scalar={s:.5}s avx2={v:.5}s ratio={:.2}x",
            s / v
        );
    }

    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2")]
    unsafe fn lut_expand_avx2(idx: &[u8], lut: &[u32; 256], trans: u8, dst: &mut [u32]) {
        use std::arch::x86_64::*;
        let mut i = 0usize;
        let vtrans = _mm256_set1_epi32(i32::from(trans));
        let ptr = lut.as_ptr();
        while i + 8 <= dst.len() {
            // 2024 edition：unsafe fn 内对每个 unsafe 运算仍需显式 unsafe 块。
            unsafe {
                let bytes = _mm_loadl_epi64(idx.as_ptr().add(i) as *const __m128i);
                let dwords = _mm256_cvtepu8_epi32(bytes);
                let mask = _mm256_cmpeq_epi32(dwords, vtrans);
                let gathered = _mm256_i32gather_epi32(ptr.cast(), dwords, 4);
                let old = _mm256_loadu_si256(dst.as_ptr().add(i) as *const __m256i);
                let blended = _mm256_blendv_epi8(gathered, old, mask);
                _mm256_storeu_si256(dst.as_mut_ptr().add(i) as *mut __m256i, blended);
            }
            i += 8;
        }
        while i < dst.len() {
            let v = idx[i];
            if v != trans {
                dst[i] = lut[v as usize];
            }
            i += 1;
        }
    }
}
