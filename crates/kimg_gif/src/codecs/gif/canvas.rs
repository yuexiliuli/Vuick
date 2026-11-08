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

/// 全覆 Keep 帧快道：帧矩形覆盖整个逻辑屏（covers_full）且 disposal==Keep 时，
/// **不变式 out 逐像素 == canvas** 成立：
/// - 原 blit 对每像素 `outp = if cur alpha==0 { prev } else { cur }`，Keep 下 canvas 与 out
///   同写 `outp`（canvas[dst] = outp，write_px(out, outp)）；
/// - 透明像素（cur==0 即 idx==trans）取 prev=画布现像，Keep 下画布保持该现像 → 二者仍相等；
/// - 非透明像素 lut 直写（alpha 恒 0xFF），canvas 与 out 同取 cur。
/// 故快道只需 u32 1 写/px 推进画布（透明像素跳过=保持画布现像），帧末整画布一次拷贝出输出；
/// 省掉原 blit 每像素的「prev 画布读 + write_px 4 字节写 + disposal 分支」。输出逐字节与原一致。
fn blit_full_cover_keep(
    canvas: &mut Canvas,
    indices: &[u8],
    frame: &GifFrame,
    lut: &[u32; 256],
    w: usize,
    h: usize,
) {
    let cv = &mut canvas.canvas;
    let fw = frame.w as usize;
    let trans = frame.transparent; // i32（-1 = 无透明索引；GCE 透明恒 0..=255）
    for r in 0..indices.len().div_ceil(fw).min(frame.h as usize) {
        let y = if frame.interlaced { interlaced_row(r, frame.h as usize) } else { r };
        if y >= h {
            continue;
        }
        let src = r * fw;
        let dst = y * w;
        for x in 0..w {
            let idx = indices[src + x];
            // 透明：画布保持现像（= 原 outp=prev）；非透明：LUT 直写。
            if i32::from(idx) != trans {
                cv[dst + x] = lut[idx as usize];
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
    if blit_fast_enabled() && covers_full && frame.disposal == Disposal::Keep {
        // 全覆 Keep 快道（env 门控，默认开）：只写画布 + 帧末一次整画布拷贝，字节恒等（见
        // blit_full_cover_keep 不变式）。非全覆 / 非 Keep 走下方原路径（含原 copy_canvas_into + blit 双写）。
        blit_full_cover_keep(canvas, indices, frame, &lut, w, h);
        copy_canvas_into(out, &canvas.canvas);
        return Ok(());
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
    if !covers_full {
        out.fill(0); // 透明黑底
    }
    let lut = build_palette(data, frame);
    blit(out, None, indices, frame, &lut, w, h);
    Ok(())
}

#[cfg(test)]
mod tests {
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
    }

    /// 双路对拍：原路径（copy+blit 双写）vs 快道（blit_full_cover_keep + 帧末整拷贝）。
    /// 断言两路输出字节一致且画布终态一致（对「快道适用」帧，原本就是唯一正确结果）。
    fn assert_fast_parity(c: &Case) {
        // 参照路：原 composite_into 行为（decode 外置，直接喂 indices）。
        let mut ref_canvas = Canvas::new(c.w, c.h);
        let mut ref_out = vec![0u8; c.w * c.h * 4];
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
        blit_full_cover_keep(&mut fast_canvas, &c.indices, &c.frame, &c.lut, c.w, c.h);
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
    fn blit_full_cover_keep_matches_original() {
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
        });
        // 2) 全覆 Keep + 透明索引打孔（透明像素须保留画布现像）。
        cases.push(Case {
            w: 16,
            h: 12,
            indices: index_grid(&mut seed, 16 * 12, 16),
            frame: frame(0, 0, 16, 12, Disposal::Keep, 3, false),
            lut: opaque_lut(&mut seed),
            label: "full_keep_trans_holes",
        });
        // 3) 全覆 Keep + 透明 + 隔行（interlaced_row 走查）。
        cases.push(Case {
            w: 21,
            h: 17,
            indices: index_grid(&mut seed, 21 * 17, 8),
            frame: frame(0, 0, 21, 17, Disposal::Keep, 0, true),
            lut: opaque_lut(&mut seed),
            label: "full_keep_trans_interlaced",
        });
        // 4) 帧矩形大于逻辑屏（全覆但超尺寸：indices=fw*fh，只消费 w×h）。
        cases.push(Case {
            w: 10,
            h: 10,
            indices: index_grid(&mut seed, 20 * 15, 32),
            frame: frame(0, 0, 20, 15, Disposal::Keep, 7, false),
            lut: opaque_lut(&mut seed),
            label: "full_keep_oversized_frame",
        });
        // 5) 大尺寸纯不透明（接近真实 001__anim_opaque）。
        cases.push(Case {
            w: 320,
            h: 240,
            indices: index_grid(&mut seed, 320 * 240, 256),
            frame: frame(0, 0, 320, 240, Disposal::Keep, -1, false),
            lut: opaque_lut(&mut seed),
            label: "full_keep_320x240",
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
        });

        for c in &cases {
            assert_fast_parity(c);
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
