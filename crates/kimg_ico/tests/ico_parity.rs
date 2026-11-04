//! kimg_ico 回归测试：手写合成 ICO（内嵌 BMP / 内嵌 PNG）必跑用例 + 外部语料金指纹（缺语料 SKIP）。
//!
//! ICO 布局：ICONDIR（6B：reserved u16 + type u16 + count u16）+ count × ICONDIRENTRY（16B），
//! 条目负载为内嵌 PNG 块（8B PNG 签名开头）或「无 BITMAPFILEHEADER 的 BITMAPINFOHEADER +
//! 像素 + AND 掩码」的 BMP。IcoDecoder 委托 kimg_bmp / kimg_png 解码内部位图。
//!
//! 口径：`kimg_ico::IcoDecoder` → `kimg_core::ImageDecoder` trait 的 dimensions / color_type /
//! read_image，输出（RGBA8）与期望逐字节比对；语料用例取 RGBA8 sha256 金指纹（与 kimg_bmp 惯例
//! 一致，SKIP 是常态不是回归）。

use std::io::Cursor;

use kimg_core::color::{ColorType, ExtendedColorType};
use kimg_core::ImageDecoder;
use kimg_ico::IcoDecoder;
use kimg_png::PngEncoder;
use sha2::{Digest, Sha256};

// ---------------------------------------------------------------------------
// ICO 容器构造
// ---------------------------------------------------------------------------

/// ICONDIRENTRY（16B）：width/height 对 256 存 0，其余按实际尺寸。
fn ico_entry(width: u8, height: u8, bits: u16, image_length: u32, image_offset: u32) -> Vec<u8> {
    let mut e = Vec::with_capacity(16);
    e.push(width);
    e.push(height);
    e.push(0); // colors
    e.push(0); // reserved
    e.extend_from_slice(&1u16.to_le_bytes()); // planes
    e.extend_from_slice(&bits.to_le_bytes());
    e.extend_from_slice(&image_length.to_le_bytes());
    e.extend_from_slice(&image_offset.to_le_bytes());
    e
}

/// 组装单条目 ICO（ICONDIR + 目录项 + 负载）。
fn build_ico(width: u8, height: u8, bits: u16, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved
    out.extend_from_slice(&1u16.to_le_bytes()); // type = ICO
    out.extend_from_slice(&1u16.to_le_bytes()); // count
    let image_offset: u32 = 6 + 16;
    out.extend_from_slice(&ico_entry(
        width,
        height,
        bits,
        payload.len() as u32,
        image_offset,
    ));
    out.extend_from_slice(payload);
    out
}

/// 构造 ICO 内嵌 32bpp BMP 负载（无 BITMAPFILEHEADER）。
/// BITMAPINFOHEADER 的 height 为实际高的两倍（像素 + AND 掩码，ICO 规范）；`pixels`
/// 为底 → 顶的 BGRA 字节流，`mask` 为 AND 掩码（每行 ceil(width/32)*4 字节，位 7..0 = x0..）。
fn bmp_in_ico_payload(width: u32, height: u32, pixels_bgra: &[u8], mask: &[u8]) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&40u32.to_le_bytes()); // BITMAPINFOHEADER 大小
    p.extend_from_slice(&(width as i32).to_le_bytes());
    p.extend_from_slice(&((height * 2) as i32).to_le_bytes()); // 加倍高度
    p.extend_from_slice(&1u16.to_le_bytes()); // planes
    p.extend_from_slice(&32u16.to_le_bytes()); // bpp
    p.extend_from_slice(&0u32.to_le_bytes()); // compression = BI_RGB
    p.extend_from_slice(&(pixels_bgra.len() as u32).to_le_bytes()); // image_size
    p.extend_from_slice(&0u32.to_le_bytes()); // xppm
    p.extend_from_slice(&0u32.to_le_bytes()); // yppm
    p.extend_from_slice(&0u32.to_le_bytes()); // colors_used
    p.extend_from_slice(&0u32.to_le_bytes()); // important
    assert_eq!(p.len(), 40, "BITMAPINFOHEADER 固定 40 字节");
    p.extend_from_slice(pixels_bgra);
    p.extend_from_slice(mask);
    p
}

// ---------------------------------------------------------------------------
// 合成必跑用例
// ---------------------------------------------------------------------------

/// ICO 内嵌 32bpp BMP + AND 掩码：dimensions / color_type / 掩码置透明逐字节断言。
#[test]
fn synth_ico_bmp_entry() {
    const W: u32 = 4;
    const H: u32 = 3;

    // 逐行计算：期望输出为顶 → 底 RGBA；文件像素/掩码为底 → 顶（与解码器行向一致）。
    // 掩码行 = ceil(W/32)*4 字节，位 7..0 = x0..；文件第 f 行（0=底）作用于 buf 第 H-1-f 行。
    let mask_row_bytes = (W.div_ceil(32) as usize) * 4;
    let mut row_rgba: Vec<Vec<u8>> = Vec::new(); // 顶 → 底
    let mut row_mask: Vec<Vec<u8>> = Vec::new(); // 顶 → 底
    for yt in 0..H as usize {
        let mut rgba = Vec::new();
        let mut mask_row = vec![0u8; mask_row_bytes];
        for x in 0..W as usize {
            let r = (x as u8 * 40 + yt as u8 * 10 + 1) & 0xFF;
            let g = (x as u8 * 8 + yt as u8 * 30 + 2) & 0xFF;
            let b = (x as u8 * 20 + yt as u8 * 5 + 3) & 0xFF;
            // 每行一个透明像素（各行位置不同），其余不透明。
            let transparent = x == (yt + 1) % W as usize;
            if transparent {
                mask_row[x / 8] |= 0x80 >> (x % 8);
            }
            rgba.extend_from_slice(&[r, g, b, if transparent { 0 } else { 0xFF }]);
        }
        row_rgba.push(rgba);
        row_mask.push(mask_row);
    }

    // 组装底 → 顶的文件字节（期望输出为顶 → 底，另行正序拼接）。
    let mut expected = Vec::new();
    let mut file_px = Vec::new();
    let mut file_mask = Vec::new();
    for yt in (0..H as usize).rev() {
        for px in row_rgba[yt].chunks_exact(4) {
            file_px.extend_from_slice(&[px[2], px[1], px[0], px[3]]); // RGBA → 文件 BGRA
        }
        file_mask.extend_from_slice(&row_mask[yt]);
    }
    for yt in 0..H as usize {
        expected.extend_from_slice(&row_rgba[yt]);
    }

    let payload = bmp_in_ico_payload(W, H, &file_px, &file_mask);
    let ico = build_ico(W as u8, H as u8, 32, &payload);

    let dec = IcoDecoder::new(Cursor::new(&ico)).expect("ICO(内嵌 BMP) 解码失败");
    assert_eq!(dec.dimensions(), (W, H));
    assert_eq!(dec.color_type(), ColorType::Rgba8);
    assert_eq!(dec.total_bytes(), (W * H * 4) as u64);

    let mut buf = vec![0u8; (W * H * 4) as usize];
    dec.read_image(&mut buf).expect("ICO(内嵌 BMP) 读取失败");
    assert_eq!(buf, expected, "内嵌 BMP 输出（含掩码透明）与期望不一致");
}

/// ICO 内嵌 PNG（kimg_png::PngEncoder 生成 RGBA8）：无损回环逐字节断言。
#[test]
fn synth_ico_png_entry() {
    const W: u32 = 5;
    const H: u32 = 4;

    let mut rgba = Vec::new();
    for y in 0..H {
        for x in 0..W {
            let a = if (x + y) % 2 == 0 { 0xFF } else { 0x80 };
            rgba.extend_from_slice(&[(x as u8 * 30) & 0xFF, (y as u8 * 50) & 0xFF, ((x + y) as u8 * 11) & 0xFF, a]);
        }
    }

    let mut png = Vec::new();
    PngEncoder::new(&mut png)
        .write_image(&rgba, W, H, ExtendedColorType::Rgba8)
        .expect("PNG 编码失败");
    let ico = build_ico(W as u8, H as u8, 32, &png);

    let dec = IcoDecoder::new(Cursor::new(&ico)).expect("ICO(内嵌 PNG) 解码失败");
    assert_eq!(dec.dimensions(), (W, H));
    assert_eq!(dec.color_type(), ColorType::Rgba8);

    let mut buf = vec![0u8; (W * H * 4) as usize];
    dec.read_image(&mut buf).expect("ICO(内嵌 PNG) 读取失败");
    assert_eq!(buf, rgba, "内嵌 PNG 无损回环输出与期望不一致");
}

// ---------------------------------------------------------------------------
// 外部语料金指纹（缺语料 SKIP）
// ---------------------------------------------------------------------------

fn corpus_dir(sub: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("target")
        .join("imgbench-corpus")
        .join(sub)
}

/// `target/imgbench-corpus/ico/1.ico`（ICO 容器，6 条 32bpp BMP 条目 → AND 掩码路径）。
/// 由 kimg_bmp 的 `corpus_ico_golden` 注释块迁移而来；RGBA8 sha256 金指纹在拆分后
/// （ico.rs 行为与改动前逐字一致）采集。目录或文件缺失 → SKIP（常态非回归）。
#[test]
fn corpus_ico_golden() {
    let p = corpus_dir("ico").join("1.ico");
    if !p.exists() {
        eprintln!("[ico-parity] SKIP: target/imgbench-corpus/ico/1.ico 缺失");
        return;
    }
    let data = std::fs::read(&p).expect("读语料");
    // 金指纹即 kimg_bmp bmp_parity.rs 注释块里的哈希（与拆分前 `load_from_memory().to_rgba8()`
    // 口径一致：本 IcoDecoder 解码实测复现完全同值）。
    let expected = "d7f175f9acb776816c1bb26fa1a054ba78ea5b9075aae1889005a7395b6422a1";
    let dec = IcoDecoder::new(Cursor::new(&data)).expect("ICO 语料解码失败");
    assert_eq!(dec.color_type(), ColorType::Rgba8, "ICO 语料输出非 RGBA8");
    let mut buf = vec![0u8; dec.total_bytes() as usize];
    dec.read_image(&mut buf).expect("ICO 语料读取失败");
    let mut h = Sha256::new();
    h.update(&buf);
    let actual = format!("{:x}", h.finalize());
    eprintln!("[ico-parity] corpus-ico-1.ico = {actual}");
    if expected.is_empty() {
        return; // discovery 模式：仅打印，采集金指纹。
    }
    assert_eq!(actual, expected, "「corpus-ico-1.ico」解码输出与金指纹不一致");
}
