//! TIFF 语料生成器（8bit 矩阵）：生成 `Benchmarks/corpus/TIFF` 下全部 8bit 测试 TIFF。
//!
//! 源：
//! - photo A/B：`Benchmarks/corpus/BMP/001.bmp`、`002.bmp`（经 kimg 解 RGBA；若尺寸超限
//!   则最近邻缩到最大边 1024 内 —— 001/002 原生 ~2484x3496，全尺寸 72 张会膨胀语料体积）。
//! - 合成 screen：1920×1080 桌面 UI 风（低色数高游程 → Deflate/LZW/PackBits 都有肉），确定性。
//!
//! 输出矩阵（每个源 × 3 布局 × 4 压缩 × 2 predictor）：
//! - 布局 {gray8, rgb8, rgba8}：gray = Rec.601 亮度，rgb/rgba 直取源 RGBA；
//! - 压缩 {none, packbits, lzw, deflate}（tiff crate 编码器，II 端序、classic、chunky、整图单 strip）；
//! - predictor {off, on(=Horizontal,2)}：4 种压缩都配 predictor 变体（PackBits+predictor 亦合法，
//!   自研与 C# 双端都支持 Predictor2）。
//! 命名 `<src>__<layout>_<comp>[_pred].tif`（compare_refs.py 按 `__` 后缀分类）。
//! 另加 2 条多 strip 用例（`001__rgb8_none_rps64.tif` RPS=64、`screen__rgba8_none_rps1.tif` RPS=1）。
//!
//! 幂等：先清空 outDir 再生成（防止旧语料残留混入）；末尾打印生成清单与计数。
//! 运行（release，长命令前台阻塞到结束；见 K86）：
//! ```text
//! cargo run --release --manifest-path Benchmarks/tiff/gen/Cargo.toml -- --out Benchmarks/corpus/TIFF
//! ```
//! `--out` 缺省 `CARGO_MANIFEST_DIR/../../corpus/TIFF`。

use std::io::Cursor;
use std::path::PathBuf;

use tiff::encoder::colortype::{Gray8, RGB8, RGBA8};
use tiff::encoder::{Compression, DeflateLevel, Predictor, TiffEncoder};

/// 最大边（photo 源缩略上限）。
const MAX_EDGE: u32 = 1024;
/// 合成 screen 尺寸。
const SCREEN_W: u32 = 1920;
const SCREEN_H: u32 = 1080;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Layout {
    Gray8,
    Rgb8,
    Rgba8,
}

impl Layout {
    fn name(self) -> &'static str {
        match self {
            Layout::Gray8 => "gray8",
            Layout::Rgb8 => "rgb8",
            Layout::Rgba8 => "rgba8",
        }
    }
}

struct Source {
    stem: String,
    w: u32,
    h: u32,
    /// RGBA8 紧密数据（w*h*4）。
    rgba: Vec<u8>,
}

fn main() {
    let mut out = std::env::var("KIMG_TIFF_CORPUS").ok();
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i + 1 < args.len() {
        if args[i] == "--out" {
            out = Some(args[i + 1].clone());
            i += 2;
        } else {
            i += 1;
        }
    }
    let default_out = format!("{}/../../corpus/TIFF", env!("CARGO_MANIFEST_DIR"));
    let out_dir = PathBuf::from(out.unwrap_or(default_out));
    let corpus_root = out_dir
        .parent()
        .expect("outDir 应位于 <corpusRoot>/TIFF")
        .to_path_buf();

    let bmp_dir = corpus_root.join("BMP");
    let photo_a = bmp_dir.join("001.bmp");
    let photo_b = bmp_dir.join("002.bmp");
    if !photo_a.is_file() || !photo_b.is_file() {
        eprintln!(
            "# ERR photo 源缺失: {} / {}（语料需先装配 Benchmarks/corpus/BMP/001.bmp + 002.bmp）",
            photo_a.display(),
            photo_b.display()
        );
        std::process::exit(2);
    }

    // 幂等：先清空 outDir。
    if out_dir.is_dir() {
        for e in std::fs::read_dir(&out_dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_file() {
                std::fs::remove_file(&p).unwrap();
            }
        }
    } else {
        std::fs::create_dir_all(&out_dir).unwrap();
    }

    let mut count = 0usize;
    let mut by_cell = std::collections::BTreeMap::<(Layout, &'static str, bool), usize>::new();
    let mut sources: Vec<Source> = Vec::new();

    let mut emit = |src: &Source, comp_name: &'static str, comp: Compression, pred: bool| {
        for layout in [Layout::Gray8, Layout::Rgb8, Layout::Rgba8] {
            let data = layout_data(src, layout);
            let suffix = if pred { "_pred" } else { "" };
            let name = format!("{}__{}_{}{}.tif", src.stem, layout.name(), comp_name, suffix);
            let bytes = encode_tiff(layout, src.w, src.h, comp, pred, None, &data);
            std::fs::write(out_dir.join(&name), &bytes).unwrap();
            println!(
                "# GEN {} {}x{} {}B layout={} comp={} pred={}",
                name,
                src.w,
                src.h,
                bytes.len(),
                layout.name(),
                comp_name,
                pred
            );
            *by_cell.entry((layout, comp_name, pred)).or_insert(0) += 1;
            count += 1;
        }
    };

    // photo A/B：解源 BMP → RGBA8 →（如需要）缩略到 MAX_EDGE 内。
    for (stem, path) in [("001", &photo_a), ("002", &photo_b)] {
        let src_img = kimg::load_from_memory(&std::fs::read(path).unwrap()).expect("源 BMP 可解码");
        let rgba = src_img.to_rgba8();
        let (w, h, raw) = maybe_downscale(rgba.width(), rgba.height(), rgba.as_raw());
        println!("# GEN source={stem} {w}x{h} (rgba={}B)", raw.len());
        let src = Source { stem: stem.to_string(), w, h, rgba: raw };
        for (cn, comp) in full_comps() {
            emit(&src, cn, comp, false);
            emit(&src, cn, comp, true);
        }
        sources.push(src);
    }

    // 合成 screen（桌面 UI 风：色块/渐变/条纹，低色数高游程 → 压缩有肉）。
    let (sw, sh) = (SCREEN_W, SCREEN_H);
    let screen = synth_screen(sw, sh);
    println!("# GEN source=screen {sw}x{sh} (synth, rgba={}B)", screen.len());
    let src = Source { stem: "screen".to_string(), w: sw, h: sh, rgba: screen };
    for (cn, comp) in full_comps() {
        emit(&src, cn, comp, false);
        emit(&src, cn, comp, true);
    }
    sources.push(src);

    // 多 strip 用例（RPS=64 与 RPS=1）：photo A RGB8 + screen RGBA8，各 1 条。
    let photo_a = sources.iter().find(|s| s.stem == "001").expect("001 source 已生成");
    {
        let name = "001__rgb8_none_rps64.tif";
        let w = photo_a.w;
        let h = photo_a.h;
        let data = layout_data(photo_a, Layout::Rgb8);
        let bytes = encode_tiff(Layout::Rgb8, w, h, Compression::Uncompressed, false, Some(64), &data);
        std::fs::write(out_dir.join(name), &bytes).unwrap();
        println!("# GEN {name} {w}x{h} {}B layout=rgb8 comp=none rps=64", bytes.len());
        count += 1;
    }
    {
        let screen_src = sources.iter().find(|s| s.stem == "screen").expect("screen source 已生成");
        let name = "screen__rgba8_none_rps1.tif";
        let bytes = encode_tiff(
            Layout::Rgba8,
            screen_src.w,
            screen_src.h,
            Compression::Uncompressed,
            false,
            Some(1),
            &screen_src.rgba,
        );
        std::fs::write(out_dir.join(name), &bytes).unwrap();
        println!(
            "# GEN {name} {}x{} {}B layout=rgba8 comp=none rps=1",
            screen_src.w,
            screen_src.h,
            bytes.len()
        );
        count += 1;
    }

    println!("# DONE gen files={count} outDir={}", out_dir.display());
    println!("# 矩阵计数（每源 × layout × comp[+_pred]）:");
    for ((layout, comp, pred), n) in &by_cell {
        println!(
            "    layout={} comp={}{} = {n} 文件/源",
            layout.name(),
            comp,
            if *pred { "+pred" } else { "" }
        );
    }
}

/// 全压缩集（tiff crate 编码器支持；II 端序 classic、chunky）。
fn full_comps() -> Vec<(&'static str, Compression)> {
    vec![
        ("none", Compression::Uncompressed),
        ("packbits", Compression::Packbits),
        ("lzw", Compression::Lzw),
        ("deflate", Compression::Deflate(DeflateLevel::default())),
    ]
}

/// RGBA8 → 各布局像素数据（gray=Rec.601 亮度；rgb/rgba 直取）。
fn layout_data(src: &Source, layout: Layout) -> Vec<u8> {
    match layout {
        Layout::Gray8 => {
            let mut out = Vec::with_capacity((src.w * src.h) as usize);
            for px in src.rgba.chunks_exact(4) {
                let l = ((u32::from(px[0]) * 299 + u32::from(px[1]) * 587 + u32::from(px[2]) * 114) / 1000) as u8;
                out.push(l);
            }
            out
        }
        Layout::Rgb8 => {
            let mut out = Vec::with_capacity((src.w * src.h * 3) as usize);
            for px in src.rgba.chunks_exact(4) {
                out.extend_from_slice(&px[..3]);
            }
            out
        }
        Layout::Rgba8 => src.rgba.clone(),
    }
}

/// 用 tiff crate 编码器生成（II 端序），可选 Horizontal predictor 与 RowsPerStrip。
fn encode_tiff(
    layout: Layout,
    w: u32,
    h: u32,
    comp: Compression,
    pred: bool,
    rps: Option<u32>,
    data: &[u8],
) -> Vec<u8> {
    let mut cursor = Cursor::new(Vec::new());
    let mut enc = TiffEncoder::new(&mut cursor).unwrap();
    enc = enc.with_compression(comp);
    if pred {
        enc = enc.with_predictor(Predictor::Horizontal);
    }
    match layout {
        Layout::Gray8 => {
            debug_assert_eq!(data.len(), (w * h) as usize);
            let mut image = enc.new_image::<Gray8>(w, h).unwrap();
            if let Some(r) = rps {
                image.rows_per_strip(r).unwrap();
            }
            image.write_data(data).unwrap();
        }
        Layout::Rgb8 => {
            debug_assert_eq!(data.len(), (w * h * 3) as usize);
            let mut image = enc.new_image::<RGB8>(w, h).unwrap();
            if let Some(r) = rps {
                image.rows_per_strip(r).unwrap();
            }
            image.write_data(data).unwrap();
        }
        Layout::Rgba8 => {
            debug_assert_eq!(data.len(), (w * h * 4) as usize);
            let mut image = enc.new_image::<RGBA8>(w, h).unwrap();
            if let Some(r) = rps {
                image.rows_per_strip(r).unwrap();
            }
            image.write_data(data).unwrap();
        }
    }
    cursor.into_inner()
}

/// 最近邻缩略：最大边 > cap 时等比降到 cap 内（确定性，逐像素取整采样）。
fn maybe_downscale(w: u32, h: u32, raw: &[u8]) -> (u32, u32, Vec<u8>) {
    let edge = w.max(h);
    if edge <= MAX_EDGE {
        return (w, h, raw.to_vec());
    }
    let nw = ((u64::from(w) * u64::from(MAX_EDGE)) / u64::from(edge)).max(1) as u32;
    let nh = ((u64::from(h) * u64::from(MAX_EDGE)) / u64::from(edge)).max(1) as u32;
    let mut out = vec![0u8; (nw * nh * 4) as usize];
    for y in 0..nh {
        let sy = ((u64::from(y) * u64::from(h)) / u64::from(nh)) as usize;
        for x in 0..nw {
            let sx = ((u64::from(x) * u64::from(w)) / u64::from(nw)) as usize;
            let src_o = (sy * w as usize + sx) * 4;
            let dst_o = (y as usize * nw as usize + x as usize) * 4;
            out[dst_o..dst_o + 4].copy_from_slice(&raw[src_o..src_o + 4]);
        }
    }
    (nw, nh, out)
}

/// 确定性合成 screen：垂直渐变背景 + 色块窗口/任务栏/进度条（低色数高游程）。
fn synth_screen(w: u32, h: u32) -> Vec<u8> {
    let mut px = vec![0u8; (w * h * 4) as usize];
    // 垂直渐变背景（浅灰 → 蓝灰）。
    for y in 0..h {
        let v = 52 + y * 28 / h;
        for x in 0..w {
            let u = v + (x * 6) / w;
            set_px(&mut px, w, x, y, u as u8, v as u8, (v + 8) as u8, 255);
        }
    }
    // 任务栏（底部深色条 + 高亮线）。
    fill_rect(&mut px, w, h, 0, h - 48, w, h, 28, 30, 40, 255);
    fill_rect(&mut px, w, h, 0, h - 49, w, h - 48, 90, 140, 210, 255);
    // 主窗口（左）。
    let (x0, y0, ww, hh) = (60u32, 70u32, 980u32, 620u32);
    shadow(&mut px, w, h, x0 + 10, y0 + 14, ww, hh, 10);
    fill_rect(&mut px, w, h, x0, y0, x0 + ww, y0 + 34, 56, 96, 152, 255);
    fill_rect(&mut px, w, h, x0, y0 + 34, x0 + ww, y0 + hh, 246, 248, 250, 255);
    for i in 0..9u32 {
        fill_rect(&mut px, w, h, x0 + 36 + i * 34, y0 + 10, x0 + 66 + i * 34, y0 + 24, 186, 200, 216, 255);
    }
    for i in 0..12u32 {
        let yy = y0 + 62 + i * 40;
        fill_rect(&mut px, w, h, x0 + 28, yy, x0 + 28 + 220 + (i % 3) * 90, yy + 14, 150, 158, 168, 255);
    }
    fill_rect(&mut px, w, h, x0, y0 + 34, x0 + 200, y0 + hh, 236, 240, 244, 255);
    fill_rect(&mut px, w, h, x0 + 8, y0 + 50, x0 + 192, y0 + 78, 96, 150, 216, 255);
    fill_rect(&mut px, w, h, x0 + 8, y0 + 92, x0 + 192, y0 + 116, 210, 216, 224, 255);
    fill_rect(&mut px, w, h, x0 + 8, y0 + 130, x0 + 192, y0 + 154, 210, 216, 224, 255);
    // 副窗口（右上，图表条）。
    let (bx0, by0, bw, bh) = (1110u32, 120u32, 640u32, 360u32);
    shadow(&mut px, w, h, bx0 + 8, by0 + 10, bw, bh, 8);
    fill_rect(&mut px, w, h, bx0, by0, bx0 + bw, by0 + 30, 120, 110, 108, 255);
    fill_rect(&mut px, w, h, bx0, by0 + 30, bx0 + bw, by0 + bh, 252, 252, 250, 255);
    for i in 0..9u32 {
        fill_rect(&mut px, w, h, bx0 + 30 + i * 60, by0 + bh - 60 - (i * 34 % 150), bx0 + 74 + i * 60, by0 + bh - 46, 66, 132, 210, 255);
    }
    // 对话框（半透明面板，真实 alpha）。
    let (dx0, dy0, dw, dh) = (240u32, 560u32, 460u32, 210u32);
    shadow(&mut px, w, h, dx0 + 8, dy0 + 10, dw, dh, 9);
    fill_rect(&mut px, w, h, dx0, dy0, dx0 + dw, dy0 + dh, 60, 90, 140, 90);
    fill_rect(&mut px, w, h, dx0 + 12, dy0 + 12, dx0 + dw - 12, dy0 + dh - 12, 250, 250, 250, 96);
    for i in 0..3u32 {
        fill_rect(&mut px, w, h, dx0 + 40 + i * 130, dy0 + dh - 54, dx0 + 150 + i * 130, dy0 + dh - 22, 40, 96, 170, 120);
    }
    // 进度条。
    fill_rect(&mut px, w, h, 80, h - 96, w - 80, h - 82, 210, 216, 224, 255);
    fill_rect(&mut px, w, h, 84, h - 92, 84 + (w - 168) * 3 / 5, h - 86, 72, 160, 96, 255);
    px
}

#[allow(clippy::too_many_arguments)]
fn set_px(px: &mut [u8], w: u32, x: u32, y: u32, r: u8, g: u8, b: u8, a: u8) {
    let o = (y * w + x) as usize * 4;
    px[o] = r;
    px[o + 1] = g;
    px[o + 2] = b;
    px[o + 3] = a;
}

#[allow(clippy::too_many_arguments)]
fn fill_rect(px: &mut [u8], w: u32, h: u32, x0: u32, y0: u32, x1: u32, y1: u32, r: u8, g: u8, b: u8, a: u8) {
    for y in y0.max(0).min(h)..y1.min(h) {
        for x in x0.max(0).min(w)..x1.min(w) {
            set_px(px, w, x, y, r, g, b, a);
        }
    }
}

/// 右下软阴影（半透明黑，alpha 向外衰减 → 真实 alpha）。
fn shadow(px: &mut [u8], w: u32, h: u32, x0: u32, y0: u32, ww: u32, _hh: u32, size: u32) {
    for y in y0..y0.saturating_add(size).min(h) {
        for x in x0..x0 + ww + size {
            if x >= w {
                break;
            }
            let in_h = y - y0;
            let in_w = x.saturating_sub(x0);
            let dist = in_h.max(in_w.saturating_sub(ww));
            let a = 70u32 * (size.saturating_sub(dist.min(size))) / size;
            if a == 0 {
                continue;
            }
            set_px(px, w, x, y, 0, 0, 0, a as u8);
        }
    }
}
