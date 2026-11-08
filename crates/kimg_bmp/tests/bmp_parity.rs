//! BMP 解码字节级金指纹回归测试。
//!
//! 目标：锁定 `codecs::bmp.rs`（手写 BMP 解码器）全部路径的输出，保证后续 SIMD 优化
//! 「输出字节与改动前逐字节一致」。口径：同一输入 → `kimg_bmp::BmpDecoder` → 统一转
//! RGBA8 → sha256(RGBA 字节) 与金指纹常量比对（与旧 `kimg::load_from_memory().to_rgba8()`
//! 输出字节一致，金指纹不改）。
//!
//! 语料（缺则 SKIP，仓库惯例）：`target/imgbench-corpus/BMP/*.bmp`（26 张：001–020 为用户
//! 新增 10–26MB 大图 + 021–026 既有 24bit 大图，逐文件金指纹哈希表覆盖）。合成用例
//! 不依赖语料，始终运行。
//!
//! ICO 相关（`target/imgbench-corpus/ico/1.ico` 与 AND 掩码路径）随拆分迁往 kimg_ico crate
//! （由另一 agent 处理），本文件不再覆盖——原 corpus_ico_golden 测试已注释停用。
//!
//! 金指纹先用改动前代码采集（discovery：expected 传空串则只打印不断言），随后硬编码为
//! 常量成为优化后的回归闸。SKIP 是常态不是回归（语料手工装配，不入库）。

use kimg_core::ImageDecoder;
use sha2::{Digest, Sha256};

/// 解码为 RGBA8 并对字节取 sha256（hex）。解码失败即 panic（合成用例必须可解）。
fn decode_rgba_sha(data: &[u8]) -> String {
    let decoder = kimg_bmp::BmpDecoder::new(std::io::Cursor::new(data)).expect("BMP 解码失败");
    let color_type = decoder.color_type();
    let mut raw = vec![0u8; decoder.total_bytes() as usize];
    decoder.read_image(&mut raw).expect("BMP 解码失败");
    // BMP 只输出 RGB8/RGBA8：RGB8 补 alpha=255 → RGBA8（与旧 to_rgba8() 口径一致）。
    let rgba = match color_type {
        kimg_core::color::ColorType::Rgba8 => raw,
        kimg_core::color::ColorType::Rgb8 => {
            let mut out = Vec::with_capacity(raw.len() / 3 * 4);
            for px in raw.chunks_exact(3) {
                out.extend_from_slice(&[px[0], px[1], px[2], 0xFF]);
            }
            out
        }
        other => panic!("BMP 解码输出非 RGB(A)8: {other:?}"),
    };
    let mut h = Sha256::new();
    h.update(&rgba);
    format!("{:x}", h.finalize())
}

/// 金指纹断言：expected 为空串 = discovery 模式（仅打印，采集改动前 golden）。
fn check_hash(label: &str, data: &[u8], expected: &str) {
    let actual = decode_rgba_sha(data);
    eprintln!("[bmp-parity] {label} = {actual}");
    if expected.is_empty() {
        return;
    }
    assert_eq!(actual, expected, "「{label}」解码输出与金指纹不一致");
}

/// 解码是否返回错误（极简地构造输入的负路径断言：RLE 越界 / delta 越界必须 Err，
/// 三者模式 SIMD / 标量 / RLE 旧路径行为一致）。
fn decode_is_err(data: &[u8]) -> bool {
    let decoder = kimg_bmp::BmpDecoder::new(std::io::Cursor::new(data));
    let Ok(d) = decoder else {
        return true;
    };
    let mut raw = vec![0u8; d.total_bytes() as usize];
    d.read_image(&mut raw).is_err()
}

/// 确定性伪随机字节（可复现合成图内容）。
fn lcg(seed: &mut u64) -> u8 {
    *seed = seed
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    (*seed >> 56) as u8
}

// ---------------------------------------------------------------------------
// 合成 BMP 构造器
// ---------------------------------------------------------------------------

/// 构造标准 BMP（40 字节 Info 头或 56 字节 V3 头）。
/// `masks`：compression==3 时的 (r,g,b,a) 掩码（56 头时 a 生效读取）。
/// `palette`：已拼接的调色板原始字节（BGRX/项）。
/// `pixels`：已含行内填充的图像数据。
fn make_bmp(
    width: i32,
    height: i32,
    bit_count: u16,
    compression: u32,
    header_size: u32,
    masks: Option<(u32, u32, u32, u32)>,
    palette: &[u8],
    pixels: &[u8],
    top_down: bool,
) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"BM");
    // file_size / reserved / offbits 占位，末尾回填。
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());

    let header_start = out.len();
    out.extend_from_slice(&header_size.to_le_bytes());
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(&(if top_down { -height } else { height }).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // planes
    out.extend_from_slice(&bit_count.to_le_bytes());
    out.extend_from_slice(&compression.to_le_bytes());
    out.extend_from_slice(&(pixels.len() as u32).to_le_bytes()); // image_size
    out.extend_from_slice(&0u32.to_le_bytes()); // xppm
    out.extend_from_slice(&0u32.to_le_bytes()); // yppm
    out.extend_from_slice(&0u32.to_le_bytes()); // colors_used
    out.extend_from_slice(&0u32.to_le_bytes()); // important
    assert_eq!(out.len() - header_start, 40, "Info 头固定 40 字节");

    // 解码器视角：bitfields 掩码紧跟 40 字节 Info 字段在偏移 54 处读取。
    // 40 头（Info）只读 r,g,b 共 12 字节，随后 seek 到 header_end+12=66；
    // 56 头（V3）读 r,g,b,a 共 16 字节，随后 seek 到 header_end=70。
    if compression == 3 {
        let (r, g, b, a) = masks.expect("bitfields 必须给掩码");
        out.extend_from_slice(&r.to_le_bytes());
        out.extend_from_slice(&g.to_le_bytes());
        out.extend_from_slice(&b.to_le_bytes());
        if header_size == 56 {
            out.extend_from_slice(&a.to_le_bytes());
        } else {
            assert_eq!(header_size, 40, "bitfields 非 V3 固定 40 头");
        }
    } else {
        assert_eq!(header_size, 40, "非 bitfields 用例固定 40 头");
    }
    let _ = &header_size;
    out.extend_from_slice(palette);
    let offbits = out.len() as u32;
    out.extend_from_slice(pixels);

    // 回填 file_size 与 offbits。
    let size = out.len() as u32;
    out[2..6].copy_from_slice(&size.to_le_bytes());
    out[10..14].copy_from_slice(&offbits.to_le_bytes());
    out
}

/// 行宽到 4 字节对齐。
fn align4(n: usize) -> usize {
    (n + 3) & !3
}

/// 构造调色板路径（1/2/4/8-bit，BI_RGB）→ 各行打包字节 + 调色板。
/// 调色板项为 BGRX（Info 头 bpc=4，解码按 [b,g,r,skip] 取）。
fn synth_palette_data(width: i32, height: i32, bit_count: u16) -> (Vec<u8>, Vec<u8>) {
    let entries = 1usize << bit_count;
    let mut palette = Vec::with_capacity(entries * 4);
    let mut seed = 0x1122_3344_5566_7788u64 + u64::from(bit_count);
    for i in 0..entries {
        palette.extend_from_slice(&[i as u8, 0, 0, 0xFF]); // B=idx, G=0, R=0（isotropic）
        let _ = i;
    }
    // 用伪随机重写 B/G/R，让调色板映射更丰富。
    palette.clear();
    for _ in 0..entries {
        palette.extend_from_slice(&[lcg(&mut seed), lcg(&mut seed), lcg(&mut seed), lcg(&mut seed)]);
    }

    let row_bytes = (i32::from(bit_count) * width + 31) / 32 * 4;
    let mut pixels = Vec::with_capacity((row_bytes as usize) * height as usize);
    for y in 0..height as usize {
        let mut row = vec![0u8; row_bytes as usize];
        match bit_count {
            1 => {
                for x in 0..width as usize {
                    let b = x / 8;
                    let bit = 7 - (x % 8);
                    if y % 3 == 0 && x % 5 < 2 {
                        row[b] |= 1 << bit;
                    }
                }
            }
            2 => {
                for x in 0..width as usize {
                    let b = x / 4;
                    let shift = 6 - 2 * (x % 4);
                    let v = ((x + y) % 4) as u8;
                    row[b] |= v << shift;
                }
            }
            4 => {
                for x in 0..width as usize {
                    let b = x / 2;
                    let v = ((x * 3 + y) % 16) as u8;
                    if x % 2 == 0 {
                        row[b] |= v << 4;
                    } else {
                        row[b] |= v;
                    }
                }
            }
            8 => {
                for x in 0..width as usize {
                    let v = ((x + y) % 256) as u8;
                    row[x] = v;
                }
            }
            _ => unreachable!(),
        }
        pixels.extend_from_slice(&row);
        let _ = y;
    }
    (pixels, palette)
}

/// 构造 16bit 裸像素行流（width*2 + 行填充）。
fn synth_16bit_data(width: i32, height: i32, val: impl Fn(usize, usize) -> u16) -> Vec<u8> {
    let mut out = Vec::new();
    for y in 0..height as usize {
        for x in 0..width as usize {
            out.extend_from_slice(&val(x, y).to_le_bytes());
        }
        if width % 2 == 1 {
            out.extend_from_slice(&[0, 0]);
        }
    }
    out
}

/// 构造 ICO 式 16bpp 裸条目（无文件头、高度折半、强制 alpha）→ 触发
/// `BmpDecoder::new_with_ico_format` 的 4ch（RGBA8）16bit 路径。compression==0 = 555
/// （R5_G5_B5）、compression==3 = 565（需显式 (r,g,b) 掩码）。`top_down` 编码为负高。
fn make_ico_bmp_entry(
    width: i32,
    height: i32,
    bit_count: u16,
    compression: u32,
    masks: Option<(u32, u32, u32)>,
    pixels: &[u8],
    top_down: bool,
) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&40u32.to_le_bytes()); // header_size
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(&(if top_down { -2 * height } else { 2 * height }).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // planes
    out.extend_from_slice(&bit_count.to_le_bytes());
    out.extend_from_slice(&compression.to_le_bytes());
    out.extend_from_slice(&(pixels.len() as u32).to_le_bytes()); // image_size
    out.extend_from_slice(&0i32.to_le_bytes()); // xppm
    out.extend_from_slice(&0i32.to_le_bytes()); // yppm
    out.extend_from_slice(&0u32.to_le_bytes()); // colors_used
    out.extend_from_slice(&0u32.to_le_bytes()); // important
    assert_eq!(out.len(), 40, "Info 头固定 40 字节");
    if compression == 3 {
        let (r, g, b) = masks.expect("bitfields 必须给掩码");
        out.extend_from_slice(&r.to_le_bytes());
        out.extend_from_slice(&g.to_le_bytes());
        out.extend_from_slice(&b.to_le_bytes());
    }
    out.extend_from_slice(pixels);
    out
}

/// 构造 ICO 式调色板裸条目（无文件头、BI_RGB、高度折半前为 2*height、强制 alpha=4ch）：
/// 40 头 + 调色板(BGRX) + 像素 → 触发 `BmpDecoder::new_with_ico_format` 的 RGBA8 调色板
/// 路径（8/4bit 的 channels==4 SIMD 分支仅经此可达）。`palette` 与 `pixels` 即
/// `synth_palette_data` 输出（BGRX 每项 / 行内 4 字节对齐像素）。
fn make_ico_palette_entry(
    width: i32,
    height: i32,
    bit_count: u16,
    palette: &[u8],
    pixels: &[u8],
) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&40u32.to_le_bytes()); // header_size
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(&(2 * height).to_le_bytes()); // new_with_ico_format 会折半
    out.extend_from_slice(&1u16.to_le_bytes()); // planes
    out.extend_from_slice(&bit_count.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
    out.extend_from_slice(&(pixels.len() as u32).to_le_bytes()); // image_size
    out.extend_from_slice(&0i32.to_le_bytes()); // xppm
    out.extend_from_slice(&0i32.to_le_bytes()); // yppm
    out.extend_from_slice(&0u32.to_le_bytes()); // colors_used
    out.extend_from_slice(&0u32.to_le_bytes()); // important
    assert_eq!(out.len(), 40, "Info 头固定 40 字节");
    out.extend_from_slice(palette);
    out.extend_from_slice(pixels);
    out
}

/// 经 `new_with_ico_format` 把 ICO 条目解码为 RGBA8 后取 sha256（4ch 16bit 路径专用）。
fn decode_ico_rgba_sha(data: &[u8]) -> String {
    let decoder = kimg_bmp::BmpDecoder::new_with_ico_format(std::io::Cursor::new(data))
        .expect("ICO BMP 解码失败");
    assert_eq!(
        decoder.color_type(),
        kimg_core::color::ColorType::Rgba8,
        "ICO 16bpp 应输出 RGBA8"
    );
    let mut raw = vec![0u8; decoder.total_bytes() as usize];
    decoder.read_image(&mut raw).expect("ICO BMP 解码失败");
    let mut h = Sha256::new();
    h.update(&raw);
    format!("{:x}", h.finalize())
}

/// 构造 16bpp 测试 BMP：`ico4`=4ch ICO 条目（新 helper）/ 3ch 文件头（`make_bmp`）；
/// `fmt_565`=565 掩码，否则 555；`top_down` 负高。
fn build_16bit_bmp(w: i32, h: i32, fmt_565: bool, ico4: bool, top_down: bool) -> Vec<u8> {
    let val = |x: usize, y: usize| -> u16 {
        if fmt_565 {
            let r = ((x * 3 + y) & 0x1F) as u16;
            let g = ((x + y * 5) & 0x3F) as u16;
            let b = ((x * 2 + y * 3) & 0x1F) as u16;
            (r << 11) | (g << 5) | b
        } else {
            let r = ((x * 7 + y) & 0x1F) as u16;
            let g = ((x + y * 11) & 0x1F) as u16;
            let b = ((x * 3 + y * 5) & 0x1F) as u16;
            (r << 10) | (g << 5) | b
        }
    };
    let px = synth_16bit_data(w, h, val);
    if ico4 {
        let (compression, masks) = if fmt_565 {
            (3u32, Some((0xF800u32, 0x07E0, 0x001F)))
        } else {
            (0u32, None)
        };
        make_ico_bmp_entry(w, h, 16, compression, masks, &px, top_down)
    } else {
        let (compression, masks) = if fmt_565 {
            (3u32, Some((0xF800u32, 0x07E0, 0x001F, 0u32)))
        } else {
            (0u32, None)
        };
        make_bmp(w, h, 16, compression, 40, masks, &[], &px, top_down)
    }
}

/// 构造 24bit 裸像素行流（BGR 每像素 + 行对齐）。
fn synth_24bit_data(width: i32, height: i32) -> Vec<u8> {
    let mut out = Vec::new();
    let row = align4(width as usize * 3);
    let mut seed = 0xA0A0_B0B0_C0C0_D0D0u64;
    for _ in 0..height {
        for _ in 0..width {
            out.extend_from_slice(&[lcg(&mut seed), lcg(&mut seed), lcg(&mut seed)]); // B,G,R
        }
        out.resize(out.len() + (row - width as usize * 3), 0);
    }
    out
}

/// 构造 32bit 裸像素行流（BGRX 每像素，无行填充）。
fn synth_32bit_data(width: i32, height: i32, alpha: u8) -> Vec<u8> {
    let mut out = Vec::new();
    let mut seed = 0x0D0D_0E0E_0F0F_10AAu64;
    for _ in 0..height {
        for _ in 0..width {
            out.extend_from_slice(&[lcg(&mut seed), lcg(&mut seed), lcg(&mut seed), alpha]);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// RLE 微编码器（便于生成有效 RLE 流）
// ---------------------------------------------------------------------------

/// RLE8 行编码：游程 >= 2 走 run，否则归入 absolute 段。
fn rle8_rows(rows: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for row in rows {
        let mut i = 0;
        while i < row.len() {
            if i + 1 < row.len() && row[i] == row[i + 1] {
                let mut j = i + 1;
                while j < row.len() && row[j] == row[i] && j - i < 255 {
                    j += 1;
                }
                out.extend_from_slice(&[(j - i) as u8, row[i]]);
                i = j;
            } else {
                let mut j = i;
                while j < row.len() && (j + 1 >= row.len() || row[j] != row[j + 1]) && j - i < 255
                {
                    j += 1;
                }
                let mut n = j - i;
                out.push(0);
                out.push(n as u8);
                out.extend_from_slice(&row[i..j]);
                if n % 2 == 1 {
                    out.push(0); // absolute 补偶
                }
                let _ = &mut n;
                i = j;
            }
        }
        out.extend_from_slice(&[0, 0]); // EOL
    }
    out.extend_from_slice(&[0, 1]); // EOF
    out
}

/// RLE4 行编码（nibble 序列）：游程 >= 2 走 run（hi/lo nibble 相同），否则 absolute。
fn rle4_rows(rows: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for row in rows {
        let mut i = 0;
        while i < row.len() {
            if i + 1 < row.len() && row[i] == row[i + 1] {
                let mut j = i + 1;
                while j < row.len() && row[j] == row[i] && j - i < 255 {
                    j += 1;
                }
                let v = row[i] << 4 | row[i];
                out.extend_from_slice(&[(j - i) as u8, v]);
                i = j;
            } else {
                let mut j = i;
                while j < row.len() && (j + 1 >= row.len() || row[j] != row[j + 1]) && j - i < 255
                {
                    j += 1;
                }
                let npix = j - i;
                let nbytes = (npix + 1) / 2;
                out.push(0);
                out.push(npix as u8);
                let mut k = 0;
                while k < nbytes {
                    let hi = row[i + 2 * k + 0];
                    let lo = if 2 * k + 1 < npix { row[i + 2 * k + 1] } else { 0 };
                    out.push(hi << 4 | lo);
                    k += 1;
                }
                if nbytes % 2 == 1 {
                    out.push(0);
                }
                i = j;
            }
        }
        out.extend_from_slice(&[0, 0]); // EOL
    }
    out.extend_from_slice(&[0, 1]); // EOF
    out
}

// ---------------------------------------------------------------------------
// 合成用例：各解码路径金指纹
// ---------------------------------------------------------------------------

/// 调色板 1/2/4/8-bit（BI_RGB）。
#[test]
fn synth_palette_paths() {
    let (w, h) = (40i32, 5i32);
    let goldens = [
        (
            1u16,
            "9000768644b55756d9a32a1fc101f0c103c4eace073bd8917d5dbf802db19b72",
        ),
        (
            2u16,
            "d4bdd150e51d463ca727e47efe72bc56d3e53e543fc5e8cd8b4b291e85cb20e1",
        ),
        (
            4u16,
            "1cbdda77f6240a37344aca4af8d0186ca68e912ec1c8d2a2143cfb9d4ff6ebea",
        ),
        (
            8u16,
            "6dba6bd463b0bf03b44b10dd4b8fd4077a0ea25a3ed54f728c9b69d795a3842c",
        ),
    ];
    for (bit_count, golden) in goldens {
        let (pixels, palette) = synth_palette_data(w, h, bit_count);
        let bmp = make_bmp(w, h, bit_count, 0, 40, None, &palette, &pixels, false);
        check_hash(&format!("palette-{bit_count}bpp"), &bmp, golden);
    }
    // 8-bit 追加大尺寸（覆盖行边界/尾部）。
    let (pixels, palette) = synth_palette_data(512, 64, 8);
    let bmp = make_bmp(512, 64, 8, 0, 40, None, &palette, &pixels, false);
    check_hash("palette-8bpp-large", &bmp, "d5c542faaa905024e468b59294cc2bd418e36f0598edc9b44dc50bc59b5f0177");
}

/// 8-bit 调色板难宽（SIMD 守界/标量尾/4 字节行填充边界）金指纹。
/// 宽度 5/7 行有 4 字节填充且 SIMD 完全不触发（纯标量）；10/17 均只触发首组 AVX2
/// （x=8 处 3x+28>3w 守界停：10→1 组+2 尾；17→1 组+9 尾）。
#[test]
fn synth_palette8_awkward_widths() {
    let (h, _) = (4i32, 0);
    let widths: [i32; 4] = [5, 7, 10, 17];
    let names: [&str; 4] = ["w5", "w7", "w10", "w17"];
    for (w, name) in widths.iter().zip(&names) {
        let (pixels, palette) = synth_palette_data(*w, h, 8);
        let bmp = make_bmp(*w, h, 8, 0, 40, None, &palette, &pixels, false);
        let golden = match *name {
            "w5" => "20c20725bd125dd0b2a0fe7efceaf1c2f4b5d62d694811aa6679f49371351a76",
            "w7" => "4b431e9c7882a1e9046d1bd26ca526946d496e2bcd9f2fc167af75164de7bcc1",
            "w10" => "1a0d07b0a0159193de292f6840aa138a258e6b66e57d32c8bd9ce5dd04e4574c",
            "w17" => "ce1d1d4d174bcc7f56ceef24355687fdbf382f972a1e06866fd8cd2eee9b0803",
            _ => "",
        };
        check_hash(&format!("palette8-{name}"), &bmp, golden);
    }
    // 4-bit 窄宽（<18 使 SIMD 守界不触发 → start=0 标量尾整行；行含 4 字节填充）。
    let h4 = 4i32;
    for w in [5i32, 17] {
        let (pixels, palette) = synth_palette_data(w, h4, 4);
        let bmp = make_bmp(w, h4, 4, 0, 40, None, &palette, &pixels, false);
        let golden = if w == 5 {
            "60770cbb2ded2ce70173faff7dec8f8557097622e229e5db83e6781fa9eb2be7"
        } else {
            "8ebd5efa5138a32d4dd61a27f329e62a094c857547414b04b576f92f5a1ca7c1"
        };
        check_hash(&format!("palette4-w{w}"), &bmp, golden);
    }
    // 4-bit 宽 22：恰 1 组 SSSE3 SIMD（16px）+ 6 像素标量尾，钉 SIMD/标量尾交界
    // （x=16 处 3x+52=100>66=3w 守界停）。
    let (pixels, palette) = synth_palette_data(22, h4, 4);
    let bmp = make_bmp(22, h4, 4, 0, 40, None, &palette, &pixels, false);
    check_hash("palette4-w22", &bmp, "8c62f7854f2e35d7953b6524325f7895ae1605d78a6f55ae560d883b59daa434");
}

/// ICO 式调色板 4ch（RGBA8）金指纹：8/4bit 的 channels==4 SIMD 分支仅经
/// `new_with_ico_format`（高度折半 + 强制 alpha）可达，普通 make_bmp 覆盖不到。
/// 宽 w=20（8bit）触发两组 AVX2+标量尾；w=60（4bit）三组+尾；窄 w=5 纯标量尾（含
/// 4 字节行填充）。双模式（SIMD / KIMG_BMP_NO_SIMD=1）同哈希。
#[test]
fn synth_palette_ico4_paths() {
    let h = 6i32;
    for (w, label) in [(20i32, "w20"), (5i32, "w5")] {
        let (pixels, palette) = synth_palette_data(w, h, 8);
        let bmp = make_ico_palette_entry(w, h, 8, &palette, &pixels);
        let golden = if w == 20 {
            "4f972afd5f6896c5f26f1e545e3a83f39a126a06646432f3d215bbe006982aa8"
        } else {
            "6727b987732a14af025052bebbb88831c78d8df1c3d22d7b388f69a1ee8a0f60"
        };
        assert_sha(&format!("ico-palette8-{label}"), &decode_ico_rgba_sha(&bmp), golden);
    }
    for (w, label) in [(60i32, "w60"), (5i32, "w5")] {
        let (pixels, palette) = synth_palette_data(w, h, 4);
        let bmp = make_ico_palette_entry(w, h, 4, &palette, &pixels);
        let golden = if w == 60 {
            "447660992c690d84b3726dc07d1f6d8841bcb45b0fcf2c74b428cec94cfe2209"
        } else {
            "4863f2b2b6b0ac6a5f327f94f3476f23ffc6014af61a0ed8845232c89d374a69"
        };
        assert_sha(&format!("ico-palette4-{label}"), &decode_ico_rgba_sha(&bmp), golden);
    }
}

/// 对已算出的 hash 作断言（discovery：expected 空串只打印不断言，用于采集金指纹）。
fn assert_sha(label: &str, actual: &str, expected: &str) {
    eprintln!("[bmp-parity] {label} = {actual}");
    if !expected.is_empty() {
        assert_eq!(actual, expected, "「{label}」解码输出与金指纹不一致");
    }
}

/// 16bit 555（BI_RGB） / 24bit / 32bit（BI_RGB，X 通道丢弃）。
#[test]
fn synth_16_24_32_paths() {
    // 16bit 555：R5G5B5（r<<10 | g<<5 | b）。
    let (w, h) = (17i32, 6i32);
    let px16 = synth_16bit_data(w, h, |x, y| {
        let r = (x * 5 + y) as u16 & 0x1F;
        let g = (x + y * 7) as u16 & 0x1F;
        let b = (x * 3 + y * 2) as u16 & 0x1F;
        (r << 10) | (g << 5) | b
    });
    let bmp16 = make_bmp(w, h, 16, 0, 40, None, &[], &px16, false);
    // 2026-11-08：16bit 展宽改位重复（C# 语义）后重算；同路径 001/002/screen__rgb555 已与 C# 全等。
    check_hash("16bpp-555", &bmp16, "35ce10b24e8f461d1757c690143a467d2578e253ef8d923044a1dd67e75d5358");

    // 24bit。
    let px24 = synth_24bit_data(w, h);
    let bmp24 = make_bmp(w, h, 24, 0, 40, None, &[], &px24, false);
    check_hash("24bpp", &bmp24, "96aa4c0af3ae14231364a306a8f355965b7576624c5c4cde64bbdaf6044fa1be");
    // 24bit 大尺寸 + 偶数宽（无行填充）。
    let px24b = synth_24bit_data(512, 64);
    let bmp24b = make_bmp(512, 64, 24, 0, 40, None, &[], &px24b, false);
    check_hash("24bpp-large", &bmp24b, "ea2c7a12f8c1b3f076a73073f814d813dc917d1161a2cc103c4a851246874ece");

    // 32bit（BI_RGB，alpha 被丢弃，输出 3 通道）。
    let px32 = synth_32bit_data(w, h, 0xAB);
    let bmp32 = make_bmp(w, h, 32, 0, 40, None, &[], &px32, false);
    // 2026-11-08：BI_RGB 32bit 通道旋转修复（[B,G,R,X]→[R,G,B]）；同路径 bgra32 已与 C# RGB 全等。
    check_hash("32bpp-birgb", &bmp32, "833ef3f0965ad687a1dfdb16cf65948109e32cec128f1a6ed31660ea1a0b19c6");
    let px32b = synth_32bit_data(512, 64, 0x7F);
    let bmp32b = make_bmp(512, 64, 32, 0, 40, None, &[], &px32b, false);
    check_hash("32bpp-birgb-large", &bmp32b, "776b83b7e02cbe89e4ed1512977205fa7290987de2a6ddd8b6ea86ff9a0a1c5a");
}

/// BITFIELDS16 / BITFIELDS32（含 8888 快路径与通用抽位路径）。
#[test]
fn synth_bitfields16() {
    let (w, h) = (19i32, 5i32);
    // 565：r=0xF800 g=0x07E0 b=0x001F（len 5/6/5 → LOOKUP_5/6BIT 语义）。
    let px565 = synth_16bit_data(w, h, |x, y| {
        let r = ((x * 3 + y)) as u16 & 0x1F;
        let g = ((x + y * 5)) as u16 & 0x3F;
        let b = ((x * 2 + y * 3)) as u16 & 0x1F;
        (r << 11) | (g << 5) | b
    });
    let bmp = make_bmp(
        w,
        h,
        16,
        3,
        40,
        Some((0xF800, 0x07E0, 0x001F, 0)),
        &[],
        &px565,
        false,
    );
    // 2026-11-08：16bit 展宽改位重复后重算；同路径 __rgb565 已与 C# 全等。
    check_hash("bitfields16-565", &bmp, "5f60777a4b18143a2caa32e90d4d0d515c135360e5720f92b58e824d9f40fa64");

    // 反序 555（r=0x001F g=0x03E0 b=0x7C00）→ 不同 shift 的同款查表。
    let px555r = synth_16bit_data(w, h, |x, y| {
        let r = ((x + y)) as u16 & 0x1F;
        let g = ((x * 4 + y)) as u16 & 0x1F;
        let b = ((x + y * 6)) as u16 & 0x1F;
        (r) | (g << 5) | (b << 10)
    });
    let bmp = make_bmp(
        w,
        h,
        16,
        3,
        40,
        Some((0x001F, 0x03E0, 0x7C00, 0)),
        &[],
        &px555r,
        false,
    );
    // 2026-11-08：16bit 展宽改位重复后重算。
    check_hash("bitfields16-555-rev", &bmp, "4e22cd41cbfc3a5f9937aadc5517b3534f850fa1a896cab374e9bbadd4e10a78");

    // 4-4-4 masks → LOOKUP_4BIT。
    let px444 = synth_16bit_data(w, h, |x, y| {
        let r = (x * 3 + y) as u16 & 0xF;
        let g = (x + y) as u16 & 0xF;
        let b = (x * 2 + y * 2) as u16 & 0xF;
        (r << 8) | (g << 4) | b
    });
    let bmp = make_bmp(
        w,
        h,
        16,
        3,
        40,
        Some((0x0F00, 0x00F0, 0x000F, 0)),
        &[],
        &px444,
        false,
    );
    check_hash("bitfields16-444", &bmp, "a1b594059f1113ac0d668dbddf345962b766bd21f8febbe1595f3ebe0bda2a04");

    // 16bit 大尺寸（覆盖行读批量 + 尾像素）。
    let px16b = synth_16bit_data(513, 8, |x, y| {
        (((x * 3 + y) & 0x1F) as u16) << 10
            | (((x + y) & 0x1F) as u16) << 5
            | (((x * y) & 0x1F) as u16)
    });
    let bmp = make_bmp(
        513,
        8,
        16,
        3,
        40,
        Some((0x7C00, 0x03E0, 0x001F, 0)),
        &[],
        &px16b,
        false,
    );
    // 2026-11-08：16bit 展宽改位重复后重算。
    check_hash("bitfields16-large", &bmp, "d81e1174a82e747ecf49697c5ccbc924b5767281f414b11d4a883bc56412458b");
}

/// BITFIELDS32：8888 带 alpha（RGBA 快路径）、888 无 alpha（RGB32 快路径）、通用抽位路径。
#[test]
fn synth_bitfields32() {
    let (w, h) = (21i32, 5i32);

    // 8888 RGBA：V3(56) 头 + a 掩码 → add_alpha → RGBA32 shuffle 快路径。
    let px = synth_32bit_data(w, h, 0x5A);
    let bmp = make_bmp(
        w,
        h,
        32,
        3,
        56,
        Some((0x00FF0000, 0x0000FF00, 0x000000FF, 0xFF000000)),
        &[],
        &px,
        false,
    );
    check_hash("bitfields32-8888-rgba", &bmp, "a48d40420c9fe6a684b8d5d22dfeedce4a3b5ef7368da464aff7b38de518f47c");

    // 888 无 alpha（r=0x00FF0000→byte2/g=0x0000FF00→byte1/b=0x000000FF→byte0，源 [B,G,R,X] →
    // [R,G,B]）：掩码位序不落 byte3/2/1，**不命中** DropByte::{First,Last} 快路径，走通用
    // read_32bit_bitfields 按位移抽位。
    let bmp = make_bmp(
        w,
        h,
        32,
        3,
        40,
        Some((0x00FF0000, 0x0000FF00, 0x000000FF, 0)),
        &[],
        &px,
        false,
    );
    check_hash("bitfields32-888-bgr", &bmp, "79c48f6255bef9ca4280540338292f2fda36e2f1fd7079e1182f216005aeddc5");

    // 888 无 alpha **DropByte::First 快路径**：掩码 r=0xFF000000(byte3)/g=0x00FF0000(byte2)/
    // b=0x0000FF00(byte1)，源像素字节序 [X,B,G,R]（byte0=X 被丢弃）→ [R,G,B]（C# Sdcb 同：
    // Shuffle32 输出 [src3,src2,src1,FF]）。
    let (wf, hf) = (23i32, 5i32);
    let mut pxf = Vec::new();
    for y in 0..hf {
        for x in 0..wf {
            let r = (x * 3 + y) as u8;
            let g = (x + y * 5) as u8;
            let b = (x * 2 + y * 2) as u8;
            let xbyte = if (x + y) % 2 == 0 { 0xCC } else { 0x33 };
            pxf.extend_from_slice(&[xbyte, b, g, r]); // [X,B,G,R]
        }
    }
    let bmp = make_bmp(
        wf,
        hf,
        32,
        3,
        40,
        Some((0xFF000000, 0x00FF0000, 0x0000FF00, 0)),
        &[],
        &pxf,
        false,
    );
    // 2026-11-08：新建用例，哈希已与 C# Sdcb dump（同 23x5 [X,B,G,R] 文件）sha256 逐字节核验一致。
    check_hash("bitfields32-888-xbgr-first", &bmp, "5c8dd7e5f0384dcd2be9a14022d753d768c8a1663be258d5e8b3ec1f02fc22af");

    // 通用抽位路径：5-6-5-16 高位窄域 → 走 read_32bit_bitfields（LUT 5/6bit + alpha 8bit）。
    let px2 = synth_32bit_data(w, h, 0xFF);
    let mut px2 = px2;
    // 逐像素重写为高位 5-6-5 + 低位 alpha。
    for i in 0..(w * h) as usize {
        let x = i as i32 % w;
        let y = i as i32 / w;
        let r = ((x * 3 + y) & 0x1F) as u32;
        let g = ((x + y * 5) & 0x3F) as u32;
        let b = ((x * 2 + y) & 0x1F) as u32;
        let a = ((x * 7 + y * 3) & 0xFF) as u32;
        let v = (r << 27) | (g << 21) | (b << 16) | (a << 8);
        px2[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
    }
    let bmp = make_bmp(
        w,
        h,
        32,
        3,
        56,
        Some((0xF8000000, 0x07E00000, 0x001F0000, 0x0000FF00)),
        &[],
        &px2,
        false,
    );
    // 2026-11-08：通用抽位路径 5/6bit 展宽改位重复后重算（widen 值同 565 已 C# 全等）。
    check_hash("bitfields32-generic", &bmp, "8b6a047da19c0f6f2ec486b07bdd0fd9de63cedd440a84832b0d9a0b29218849");

    // 大尺寸 8888。
    let pxb = synth_32bit_data(512, 64, 0x42);
    let bmp = make_bmp(
        512,
        64,
        32,
        3,
        56,
        Some((0x00FF0000, 0x0000FF00, 0x000000FF, 0xFF000000)),
        &[],
        &pxb,
        false,
    );
    check_hash("bitfields32-8888-large", &bmp, "65a0cb3b0ae33d4f59119e63464dcfef4dfc80ede8b126a5a22e9c89018cfa5f");
}

/// A8R8G8B8（r/g/b/a 掩码 byte2/1/0/3，源 [B,G,R,A]→RGBA）边界扫描：窄宽/奇数宽
/// （AVX2 8px/批、SSSE3 4px/批、标量尾的批界）+ top-down 负高。金指纹先用改动前通用
/// 抽位路径采集；修复后走 (4,4) bgra_to_rgba SIMD 快路径，SIMD/标量两跑须同哈希。
#[test]
fn synth_32bit_8888_fastpath() {
    for (label, w, h, top, golden) in [
        ("8888-narrow3", 3i32, 4i32, false, "7123482d3ef0815d564f3b69e24960ddb27afe5afe80ea1ee8a92c919cff61cf"),
        ("8888-narrow5", 5i32, 4i32, false, "b0a73297e35045a9f470bc0f95e15dae6dd7cb6c192f7e46e45b028ef50e9ead"),
        ("8888-narrow7", 7i32, 4i32, false, "896e53b4336db4236a867a27902246d1e6917bd156d823d725e7cba6fca289b4"),
        ("8888-topdown", 45i32, 5i32, true, "8dc70a2bda2d7ef2bc933b340cd4d04b75a0136ed91d50f9c1f077fd2594387d"),
    ] {
        let px = synth_32bit_data(w, h, 0x9C);
        let bmp = make_bmp(
            w,
            h,
            32,
            3,
            56,
            Some((0x00FF0000, 0x0000FF00, 0x000000FF, 0xFF000000)),
            &[],
            &px,
            top,
        );
        check_hash(&format!("bitfields32-{label}"), &bmp, golden);
    }
}

/// A8R8G8B8 反位负例：r/g/b 落 byte3/2/1、a 落 byte0（源 [A,B,G,R]）。掩码位序非
/// A8R8G8B8 → **不得**命中 (4,4) 快路径（飞点：`bf.a == r8` 要求 alpha 在 byte3），
/// 必须走通用抽位路径输出 [R,G,B,A]=[raw3,raw2,raw1,raw0]。
#[test]
fn synth_32bit_8888_abgr_negative() {
    let (w, h) = (21i32, 6i32);
    let mut px = synth_32bit_data(w, h, 0x5F);
    for i in 0..(w * h) as usize {
        let x = i as i32 % w;
        let y = i as i32 / w;
        let r = (x * 3 + y) as u8;
        let g = (x + y * 5) as u8;
        let b = (x * 2 + y * 2) as u8;
        let a = (x * 7 + y * 3) as u8;
        px[i * 4..i * 4 + 4].copy_from_slice(&[a, b, g, r]); // [A,B,G,R]
    }
    let bmp = make_bmp(
        w,
        h,
        32,
        3,
        56,
        Some((0xFF000000, 0x00FF0000, 0x0000FF00, 0x000000FF)),
        &[],
        &px,
        false,
    );
    check_hash("bitfields32-8888-abgr-negative", &bmp, "1eb593a22358bfa43e11ed2ed007711a3ec2c901e21bff1fac524aad22f0c442");
}

/// RLE8 / RLE4（含 run / absolute / delta / top-down 负高）。
#[test]
fn synth_rle_paths() {
    let mut pal8 = Vec::new();
    let mut seed = 0xBEEF_CAFE_1234_5678u64;
    for _ in 0..256 {
        pal8.extend_from_slice(&[lcg(&mut seed), lcg(&mut seed), lcg(&mut seed), 0xFF]);
    }

    // RLE8：16x4，行内容含 run/absolute 混合。
    let rows8: Vec<Vec<u8>> = vec![
        vec![3, 3, 3, 7, 7, 9, 9, 9, 9, 1, 1, 1, 1, 1, 2, 2],
        vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
        vec![200, 200, 200, 200, 200, 200, 200, 200, 42, 42, 42, 42, 42, 42, 1, 1],
        vec![9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9],
    ];
    let enc8 = rle8_rows(&rows8);
    let bmp8 = make_bmp(16, 4, 8, 1, 40, None, &pal8, &enc8, false);
    check_hash("rle8", &bmp8, "77707d071faed1161a070f660f438db3de5890840072eb26480953c361e995f3");

    // RLE4：16x4（nibble 序列）。
    let rows4: Vec<Vec<u8>> = vec![
        vec![3, 3, 3, 7, 7, 9, 1, 1, 1, 1, 2, 2, 2, 2, 5, 5],
        vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
        vec![15, 15, 15, 15, 15, 15, 15, 15, 0, 0, 0, 0, 0, 0, 0, 0],
        vec![4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4],
    ];
    let mut pal4 = Vec::new();
    for _ in 0..16 {
        pal4.extend_from_slice(&[lcg(&mut seed), lcg(&mut seed), lcg(&mut seed), 0xFF]);
    }
    let enc4 = rle4_rows(&rows4);
    let bmp4 = make_bmp(16, 4, 4, 2, 40, None, &pal4, &enc4, false);
    // 2026-11-08：RLE4 修复（4bit run 高 nibble/C# 交替 + absolute 补偶跳过）后重算；
    // 同路径 001/002/screen__rle4 已与 C# 逐字节全等。
    check_hash("rle4", &bmp4, "dba5165ad722697017153275f459a48ef47aa70855b44c33cf77467cd1cfe1b1");

    // RLE8 含 delta 跳行：row 内 delta(0,1) → 填零当前行余段 + 整行空行 + 落到下一行 x 处。
    // （注意 absolute 字节数 n 在 escape 后 0/1/2 为 EOL/EOF/Delta 保留，abs 最小须 >=3；
    //  top-down RLE 被解码器拒绝，valid 组合仅 bottom-up。）
    let mut enc8d = Vec::new();
    enc8d.extend_from_slice(&[12, 3]); // 行：run 12 色 3
    enc8d.extend_from_slice(&[0, 3, 9, 9, 9]); // 行：absolute 3（不需补偶，3 为奇即 pad 0）
    enc8d.push(0);
    enc8d.extend_from_slice(&[0, 0]); // EOL
    enc8d.extend_from_slice(&[3, 5]); // 行：run 3 色 5
    enc8d.extend_from_slice(&[0, 2, 0, 1]); // 行：delta xd=0 yd=1
    enc8d.extend_from_slice(&[0, 0]); // EOL
    enc8d.extend_from_slice(&[0, 1]); // EOF
    let bmp8d = make_bmp(16, 4, 8, 1, 40, None, &pal8, &enc8d, false);
    // 2026-11-08：absolute 补偶不再当像素 → delta 跳行后 EOL 填黑补齐（与 C# 一致）重算。
    check_hash("rle8-delta", &bmp8d, "04d19d611d52192bb2f8292c8ee9e64da9a1da7ffe1b52ce05f7d18531ec022b");
}

/// RLE 边界合成：run 越界裁切 / EOL 提前 / delta 后 absolute（跨行填黑）/ 4bit 奇数补偶 + 两色交替 run。
/// 这些是批量写（fill_repeat / 成对展开）与逐像素写必须等价的语义边界。
#[test]
fn synth_rle_edges() {
    let mut pal = Vec::new();
    let mut seed = 0xDEAD_BEEF_0000_0001u64;
    for _ in 0..256 {
        pal.extend_from_slice(&[lcg(&mut seed), lcg(&mut seed), lcg(&mut seed), 0xFF]);
    }

    // RLE8 run 越界 → 静默裁到行宽（Imagemagick 兼容）：run 12 色 3 只写 8 像素。
    let a = make_bmp(8, 1, 8, 1, 40, None, &pal, &[12, 3, 0, 0, 0, 1], false);
    check_hash("rle8-run-overrun", &a, "0707afdcf2f3cbcee3cac2ce3d20a4867bd62f0ea5d407c7b4631906f8c918f8");

    // RLE8 EOL 提前 → 行尾余白填黑：run 3 色 1 后立即 EOL。
    let b = make_bmp(8, 1, 8, 1, 40, None, &pal, &[3, 1, 0, 0, 0, 1], false);
    check_hash("rle8-eol-early", &b, "f955ffcf37e14205dbedea6ebd8882fa69e5fad087cd2a0bcd8b695313f91c70");

    // RLE8 delta yd=1 落行后再 absolute（跨行填黑 + 落点续写 + 余白填黑）。
    // 第 1 行（底）：run4 色 2 → delta(xd=1,yd=1)：填当前行余段、整行跳行(填 0)、
    //   落第 3 行 x=4 填 4 像素 0 + xd=1 填 0 → absolute 3 色 5（payload 3 奇 → 补偶 1 B）→ EOL。
    let c = make_bmp(
        8,
        3,
        8,
        1,
        40,
        None,
        &pal,
        &[4, 2, 0, 2, 1, 1, 0, 3, 5, 5, 5, 0, 0, 0, 0, 1],
        false,
    );
    check_hash("rle8-delta-absolute", &c, "9a1c20b4d64188a4450b039ede526a97f7673f09483351b579dcc92aabdc6d99");

    // RLE8 delta yd=0（同行 x 平移，不跨行）：run2 色4 → delta(xd=3,yd=0) → 同行跳过 3 填零 →
    // run2 色6 → EOL 余 1 填黑。覆盖「yd=0 不走跳行块、只做同行 xd 平移」支路。
    let e = make_bmp(8, 1, 8, 1, 40, None, &pal, &[2, 4, 0, 2, 3, 0, 2, 6, 0, 0, 0, 1], false);
    check_hash("rle8-delta-yd0", &e, "5f3dd009fafa227336e4ec1c285104668ba02de9f6339bcc8f26572ed26cb855");

    // RLE8 delta yd=2（多行跳 + `for _ in 1..yd` 整行填零）：run3 色1 → delta(xd=2,yd=2) →
    // 填当前行余段、跳过 1 行整行填零、落第 3 行 x=3 填 0 + xd=2 填 0 → run3 色2 → EOL；尾行全黑。
    let f = make_bmp(
        8,
        4,
        8,
        1,
        40,
        None,
        &pal,
        &[3, 1, 0, 2, 2, 2, 3, 2, 0, 0, 0, 0, 0, 1],
        false,
    );
    check_hash("rle8-delta-yd2", &f, "b9ae9d4173d032f4aa8bcdf2ad2ae02e05142daab43a8b537baca8607e13b6e8");

    // RLE4 absolute 奇像素补偶 + 两色交替 run（hi≠lo）：行 16 像素，
    // absolute 5（payload 3B，奇数补 1 偶字节）+ run 7（nibble 0x9C → 9/C 交替）→ EOL 余 4 填黑。
    let d = make_bmp(
        16,
        1,
        4,
        2,
        40,
        None,
        &pal[..64],
        &[
            0, 5, 0x12, 0x34, 0x50, 0, // absolute 5 + 补偶
            7, 0x9C, // run 7（hi=9 / lo=C 交替）
            0, 0,    // EOL：余 4 像素填黑
            0, 1,    // EOF
        ],
        false,
    );
    check_hash("rle4-oddpad-alternate", &d, "dae8edbf1ce233120f41ed3b180c6341c433f45662fb9f971f312dd664a8dfcb");
}

/// RLE 段式展开专项（R5 索引缓冲 + 段表路径独有组合）：同行 delta(xd,yd=0) 平移在
/// already-decoded 段之间制造「填零洞」→ 多段展开；RLE4 奇数游程使后续 segment 落在
/// 奇像素偏移（段内 nibble 对齐翻转）。SIMD 与标量两跑须哈希一致。
#[test]
fn synth_rle_gap_edges() {
    let mut pal = Vec::new();
    let mut seed = 0x0BAD_CAFE_0000_0002u64;
    for _ in 0..256 {
        pal.extend_from_slice(&[lcg(&mut seed), lcg(&mut seed), lcg(&mut seed), 0xFF]);
    }

    // RLE8：run4 色7 → delta(xd=5,yd=0) 同行洞 [4,9) → absolute 7（payload 7 奇 → 补偶）→ EOL。
    // 行 16 像素：[0,4)=7, [4,9)=0, [9,16)=9，两段一洞。
    let a = make_bmp(
        16,
        1,
        8,
        1,
        40,
        None,
        &pal,
        &[4, 7, 0, 2, 5, 0, 0, 7, 9, 9, 9, 9, 9, 9, 9, 0, 0, 0, 0, 1],
        false,
    );
    check_hash("rle8-gap-abs", &a, "7f19c86e970b149422da0b10b494ed767ddbab0ee8b80294d7679ffaaa046b9d");

    // RLE4：run3(0xA5 交替) → absolute 5（落点 x=3 奇偏移，payload 3B 补偶）→ delta(xd=4,yd=0)
    // 同行洞 [8,12) → absolute 4 → EOL。覆盖「段内奇/偶 nibble 对齐翻转 + 一洞两段」。
    let b = make_bmp(
        16,
        1,
        4,
        2,
        40,
        None,
        &pal[..64],
        &[3, 0xA5, 0, 5, 0x12, 0x34, 0x50, 0, 0, 2, 4, 0, 0, 4, 0x67, 0x89, 0, 0, 0, 1],
        false,
    );
    check_hash("rle4-gap-abs-odd", &b, "00c39836706f39a05b7702b2b391b8f900449b917f89e5224c6212869a43319f");

    // RLE8：同行双洞（两次 delta yd=0）→ 三段两洞：run2 色3 → 洞3 → run1 色2 → 洞2 → absolute 8。
    let c = make_bmp(
        16,
        1,
        8,
        1,
        40,
        None,
        &pal,
        &[2, 3, 0, 2, 3, 0, 1, 2, 0, 2, 2, 0, 0, 8, 8, 8, 8, 8, 8, 8, 8, 0, 0, 0, 1],
        false,
    );
    check_hash("rle8-gap-2holes", &c, "1c39cbc5271f6a4cf4ae324610cb7152a83da36c90d6e549b25f60c54e74b4c5");

    // RLE8 双段 ≥ 阈值（8px）+ 同行洞：abs8 → delta(xd=8,yd=0) 洞 [8,16) → abs8（w=24）。
    // gap 两侧段同时 ≥ 阈值 → push_seg 不跨洞合并、两段各自独立 SIMD 展开。
    let mut pd = vec![0u8, 8];
    pd.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
    pd.extend_from_slice(&[0, 2, 8, 0, 0, 8]);
    pd.extend_from_slice(&[9, 10, 11, 12, 13, 14, 15, 16]);
    pd.extend_from_slice(&[0, 0, 0, 1]);
    let d = make_bmp(24, 1, 8, 1, 40, None, &pal, &pd, false);
    check_hash("rle8-gap-dual-simd", &d, "e225286db8f82cbe069550f4d548c9619f6b54de6ad974a1b5b6509bf183ff2c");

    // RLE4 双段 ≥ 阈值（32px）+ 同行洞：abs32 → delta(xd=16,yd=0) 洞 [32,48) → abs32（w=64）。
    let mut pe = vec![0u8, 32];
    for i in 0..16u8 {
        pe.push((i.wrapping_mul(5) & 0xF) << 4 | (i.wrapping_mul(5) + 1) & 0xF);
    }
    pe.extend_from_slice(&[0, 2, 16, 0, 0, 32]);
    for i in 0..16u8 {
        pe.push((i.wrapping_mul(7) + 3 & 0xF) << 4 | (i.wrapping_mul(7) + 4) & 0xF);
    }
    pe.extend_from_slice(&[0, 0, 0, 1]);
    let e = make_bmp(80, 1, 4, 2, 40, None, &pal[..64], &pe, false);
    check_hash("rle4-gap-dual-simd", &e, "b69723af1b604d849629870537d83df5dfdce0dbf59a61b00ff3dd2d572fc67f");

    // RLE8 delta yd>0 跳行前该行上有大绝对段（≥ 阈值）：abs16 → delta(xd=0,yd=1) 弃行
    // （弃行须展开 16px 段 + 行尾填黑）→ 落下一行前 16 列填黑 → abs8 续满。锁「跳行前
    // 大段 + 落行前导填零 + 后续段」组合。
    let mut pf = vec![0u8, 16];
    pf.extend_from_slice(&[20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35]);
    pf.extend_from_slice(&[0, 2, 0, 1, 0, 8]);
    pf.extend_from_slice(&[40, 41, 42, 43, 44, 45, 46, 47]);
    pf.extend_from_slice(&[0, 0, 0, 1]);
    let f = make_bmp(24, 2, 8, 1, 40, None, &pal, &pf, false);
    check_hash("rle8-delta-jump-bigseg", &f, "b099913b29ab2b627815a0186ef49de9abae5f8df7173496ba1c2a41fe583bb3");
}

/// 长段展开专项（R5 SIMD 批路径）：3ch 行内核 SIMD 批守卫（8bit `3x+28<=3w` / 4bit
/// `3x+52<=3w`）要求段长足够——上述 16px 行全走标量尾。此处用 80px 宽 + 段长 ≥68，
/// 强制命中 AVX2 gather / SSSE3 pshufb 批，锁 SIMD 与标量两跑哈希一致。
#[test]
fn synth_rle_long_absolute() {
    let mut pal = Vec::new();
    let mut seed = 0xC1A0_5EED_0000_0101u64;
    for _ in 0..256 {
        pal.extend_from_slice(&[lcg(&mut seed), lcg(&mut seed), lcg(&mut seed), 0xFF]);
    }
    let w = 80usize;
    let abs_bytes: Vec<u8> = (0..82).map(|i| ((i * 7 + 3) % 256) as u8).collect();

    // RLE8：整行 absolute 80（payload 80 偶 → 无补偶）→ 单段长 80。
    let mut p8 = vec![0u8, 0x50];
    p8.extend_from_slice(&abs_bytes[..80]);
    p8.extend_from_slice(&[0, 0, 0, 1]);
    let a = make_bmp(w as i32, 1, 8, 1, 40, None, &pal, &p8, false);
    check_hash("rle8-long-abs", &a, "8d440d30d5490db0e3a24af163d87ca250e399ff2d89772db9750b42382b0923");

    // RLE4：run5(0xD0 交替) → absolute 75（payload 38 偶，落点 x=5 奇偏移）→ 段 [0,80)。
    let mut p4 = vec![5u8, 0xD0, 0u8, 75];
    for i in 0..38 {
        p4.push(((i * 11) & 0xF) as u8);
    }
    p4.extend_from_slice(&[0, 0, 0, 1]);
    let b = make_bmp(w as i32, 1, 4, 2, 40, None, &pal[..64], &p4, false);
    check_hash("rle4-long-mixed", &b, "94622ed12771348cb8cb0b5b38bdeda0995352d5e40ea5fef79abc47db9fa404");

    // RLE8：run7 色3 → delta(xd=5,yd=0) 洞 [7,12) → absolute 68（payload 68 偶）→ 两段一洞。
    let mut p8b = vec![7u8, 3, 0u8, 2, 5, 0, 0u8, 68];
    p8b.extend_from_slice(&abs_bytes[..68]);
    p8b.extend_from_slice(&[0, 0, 0, 1]);
    let c = make_bmp(w as i32, 1, 8, 1, 40, None, &pal, &p8b, false);
    check_hash("rle8-long-delta-gap", &c, "600447b4f4c08bf94c85ff8afbb04ae253634f1d28137ec6de23280b030921af");
}

/// RLE 错误负路径（评审 N2）：越界 Err 语义从口头声明变可测——RLE4 run 越界、4bit/8bit
/// absolute 越界、delta 落行 x>w、delta 同行平移 x+xd>w，均须解码返回 Err。
/// 该断言随套件在 SIMD / 标量 / KIMG_BMP_RLE_LEGACY 三模式下运行，三路径须一致。
#[test]
fn synth_rle_err_paths() {
    let mut pal = Vec::new();
    let mut seed = 0x0BAD_CAFE_0000_0003u64;
    for _ in 0..256 {
        pal.extend_from_slice(&[lcg(&mut seed), lcg(&mut seed), lcg(&mut seed), 0xFF]);
    }

    // RLE4 run 越界：x+n=0+10 > w=8 → Err。
    let a = make_bmp(8, 1, 4, 2, 40, None, &pal[..64], &[10, 0x11], false);
    assert!(decode_is_err(&a), "RLE4 run 越界应 Err");

    // 4bit absolute 越界：x+9 > w=8 → Err（payload ceil(9/2)=5 先读后判界）。
    let b = make_bmp(8, 1, 4, 2, 40, None, &pal[..64], &[0, 9, 1, 2, 3, 4, 5], false);
    assert!(decode_is_err(&b), "4bit absolute 越界应 Err");

    // 8bit absolute 越界：x+9 > w=8 → Err。
    let c = make_bmp(8, 1, 8, 1, 40, None, &pal, &[0, 9, 1, 2, 3, 4, 5, 6, 7, 8], false);
    assert!(decode_is_err(&c), "8bit absolute 越界应 Err");

    // delta yd>0 落行 x>w：RLE8 run12 裁到 w=8 后 x=12，delta(0,1) 落下一行检查 x>w → Err。
    let d = make_bmp(8, 2, 8, 1, 40, None, &pal, &[12, 3, 0, 2, 0, 1], false);
    assert!(decode_is_err(&d), "delta 落行 x>w 应 Err");

    // delta 同行平移 x+xd>w：run4 后 delta(xd=6,yd=0) → 4+6>8 → Err。
    let e = make_bmp(8, 1, 8, 1, 40, None, &pal, &[4, 1, 0, 2, 6, 0], false);
    assert!(decode_is_err(&e), "delta x+xd>w 应 Err");
}

/// 负高（top-down）24bit 与 16bit。
#[test]
fn synth_topdown_paths() {
    let (w, h) = (29i32, 5i32);
    let px24 = synth_24bit_data(w, h);
    let bmp = make_bmp(w, h, 24, 0, 40, None, &[], &px24, true);
    check_hash("topdown-24bpp", &bmp, "6efe713df756b10be026e299cb04f100ab5ed6bf96c548e88b802f11b314e3b3");

    let px16 = synth_16bit_data(w, h, |x, y| {
        (((x + y) & 0x1F) as u16) << 10
            | (((x * 3 + y) & 0x1F) as u16) << 5
            | (((x + y * 2) & 0x1F) as u16)
    });
    let bmp = make_bmp(w, h, 16, 0, 40, None, &[], &px16, true);
    // 2026-11-08：16bit 展宽改位重复后重算。
    check_hash("topdown-16bpp", &bmp, "530c94beac51eb483b5abfc69946b3cdb9e083a856b386d3bb68784911ab65bc");
}

// ---------------------------------------------------------------------------
// 16bit SIMD 专项（2026-11-08 评审 N1/N2 补）：4ch ICO 内核 + 窄行宽边界扫描
// ---------------------------------------------------------------------------

/// 评审 N1：4ch（ICO 强制 alpha）16bit 内核 `rgb555/565_to_rgba_*` 的字节恒等闸。该路径仅经
/// `new_with_ico_format` 可达（文件头 16bit 无 alpha 恒 3ch），用合成 ICO 条目触发。金指纹先在
/// SIMD 下 discovery 采集；同套件 SIMD / 标量（KIMG_BMP_NO_SIMD=1）两跑须同哈希。
#[test]
fn synth_16bit_4ch_ico() {
    let goldens: [(&str, &str); 4] = [
        (
            "555-45x7",
            "c7e3cb1e544425f2ea9810b6661e5132f03cbc9a65c3155afef4a10c4afefc1b",
        ),
        (
            "565-45x7",
            "e74f4a2829d808f0ea3264609441e5cb070713619bb78f7b824d73150171515b",
        ),
        (
            "555-3x2",
            "2b6937fe007c2f16ca31eaeb7ce8f98b83601014c4b57bac6a3f7cc576feadfa",
        ),
        (
            "565-5x3",
            "ba927ed42748a3460ec0b0f759e506a9e8883e2118e1bc225b7f1fc25ec2d960",
        ),
    ];
    // AVX2 5 组 + SSSE3 1 组 + 标量尾（45x7）；纯标量尾（3x2）；SSSE3 单组（5x3）。
    for (label, fmt565, w, h) in [
        ("555-45x7", false, 45, 7),
        ("565-45x7", true, 45, 7),
        ("555-3x2", false, 3, 2),
        ("565-5x3", true, 5, 3),
    ] {
        let bmp = build_16bit_bmp(w, h, fmt565, true, false);
        let got = decode_ico_rgba_sha(&bmp);
        eprintln!("[bmp-parity] ico16-{label} = {got}");
        let golden = goldens
            .iter()
            .find(|(l, _)| *l == label)
            .map(|(_, g)| *g)
            .unwrap();
        assert_eq!(got, golden, "「ico16-{label}」4ch 输出与金指纹不一致");
    }
}

/// 评审 N2：窄行宽 16bpp 边界扫描（宽 1..=20）× {555,565} × {3ch RGB, 4ch ICO-A} ×
/// {正高, top-down}。覆盖 4/8/12/16 的 AVX2 整组/半组进带、1/2/3 纯标量尾、奇数宽行填充。
/// 每构型把 20 个 `w:hash` 拼接为一条金指纹；同套件 SIMD / 标量两跑须同哈希。
#[test]
fn synth_16bit_narrow_widths() {
    // 每构型 20 行 `w:hash`（宽 1..=20）拼接金指纹；先在 SIMD 下 discovery 采集，
    // 已核对与既有 565 大宽金指纹一致（宽度 19 的 565-rgb-up == bitfields16-565常量）。
    let goldens: [(&str, &str); 8] = [
        ("555-rgb-up", r#"1:0249378b20157a0d92ef21ac34da4418e2cc90ec6d572fb682db1d1582afaff7
2:0e062cf183efbfcffc3b2f8dcb0d9bd7436372b71eb0abad395a44adb675aa90
3:962ae8d7ca5e9cee61c4b60cf53386c3cafbd6874171acacacfbf91495ed9acf
4:369522f17654cf57ed2983098671f551911d8e7aae3a18bb2682247d9413d5b0
5:c163d78f574c5850fdf42d9d5e05491c925a25a142fdcf208759fa0f0fd4ada7
6:72eba2bffb00efcf5fda8fcd09c3acf544b9c435eb44155855bf63ee0ba9e973
7:c6d3ea0f514833bc88a3215314492d49bfbb65e47280559cd946e62c2fdf9feb
8:0dc5829e7b5de5d5b0a681df5a2c708b2c26b57ffeca26e9662eaac4c60c454e
9:2c763922b0f76b3cea2c392b481372369e2c7738aa8ff2fa488db0f4a82eadad
10:3613c2512a1f234641a6e73613a101c07e1aeb864dcd1f6249beff0551d92ff1
11:c73c74a7255c27339f6761f43ad589680dda4f97ac1507d3ae91600c7ffe7c43
12:a87da3fab0a83ce8f37d53645cd66246fad68cda6314b3143d17c3184ec3e083
13:1792083d82455340a1d5f99b36659643eec97878ebb7d2fdbd71d7a95beb38fd
14:6d034c068176a4a987a3eabb32373037ee1f1c30407ad02b3a15c0cb81080d95
15:8c07b39b1334436daac889b2b0e9dcf0424574653a42dcfe8077e7bced535b20
16:8d8f4779a2296c8a9c9b5ec3282f49fe98df5bb803cc200a537779cccdaff090
17:f5c2dc400f097cd7613bb21e71d18c86421b346d9a27f76ffca2cfa20b5c3184
18:b9136a35adc27e2859c37fa9d4569be1f8a9b15e19eacf16d31769e776031979
19:dbe9a67b67cb05bc1e9fb14f85bf66f63482a68f72144431e8fb974d23361c15
20:0b9a9cadee8de01aed7e20c9bd8e97517ed6a410c761e633f2d3a7008b2a32bd"#),
        ("555-rgb-dn", r#"1:e2784574cbfa6946a1faf3b1374cbf89de7419fd10531ce7b427726824c19e2b
2:c06d44c38b4a762024170364c5f4f57db075091d65040e74d4993355dc473c96
3:8a6643ca66757b7cc2b67e85764990e9113603f33798ad204ae18eee0258d0eb
4:71d3d30e877369c403832de5141978f99587a94665901aa9d5807947823dcf8d
5:ad2661bfc40cbdeec95225556c87565af1e8ed80b3e6a62a66d114066fed464e
6:283584dc304b1ac8986f9af9a979a6c20fbec0ed47ba44343de2f6c4b3856696
7:6857663efde9446a139d34f758d3a2810d4c80955ab1b860ecbdc9c26f631394
8:07c45b7698b1bf7958ce48425d7ea50d01e91854a66ecedafb5b28a6f90ab6aa
9:d0a82a02b94516e54866841cabd767ab9f5c2b9bb10f0bae0a20d94c84efe918
10:376d02fb09dbca37951592f3813ad183c9a067594aa26303fa78e253bffefb28
11:3d4adbe7e6fafa87c2465f0bb03220bda189faae55905821b41d505f8af38d69
12:14165502414bb7376d072d73b59c69a2f2b4ce5fabbf23179689b9bf3f717db5
13:6ca529ab21b6236d6b65d8f2a55a728f7bee797749e9b63ff7e52f8f03648334
14:5e557ad650b45f446a10a0768bd2f0ae1229d10ad3649ef6fd6e4788d3c2520e
15:ee44cab1c11eb9430257b5e9eb674f823140a7a7e54953f513eb6b6110e56ed8
16:967e578b814dcf022a8f06e3d9be4aa6335335a046eea726d056a0c8cdff1ba3
17:56146a10896b9cf99d5795a20b96b30319006593baa79d4c65ad36343b44f580
18:358c8b9c542a950691a5173b46e2357cfabb98bcb65fb30e703248b40cfb1821
19:1705ea79361733ea1acb15eb143c6f7a1138c40a637ddedae7d16f5505eecb84
20:ca7541f0265bd7c3b1f09474391226816377869e49f34ad2136944b633870762"#),
        ("555-ico-up", r#"1:0249378b20157a0d92ef21ac34da4418e2cc90ec6d572fb682db1d1582afaff7
2:0e062cf183efbfcffc3b2f8dcb0d9bd7436372b71eb0abad395a44adb675aa90
3:962ae8d7ca5e9cee61c4b60cf53386c3cafbd6874171acacacfbf91495ed9acf
4:369522f17654cf57ed2983098671f551911d8e7aae3a18bb2682247d9413d5b0
5:c163d78f574c5850fdf42d9d5e05491c925a25a142fdcf208759fa0f0fd4ada7
6:72eba2bffb00efcf5fda8fcd09c3acf544b9c435eb44155855bf63ee0ba9e973
7:c6d3ea0f514833bc88a3215314492d49bfbb65e47280559cd946e62c2fdf9feb
8:0dc5829e7b5de5d5b0a681df5a2c708b2c26b57ffeca26e9662eaac4c60c454e
9:2c763922b0f76b3cea2c392b481372369e2c7738aa8ff2fa488db0f4a82eadad
10:3613c2512a1f234641a6e73613a101c07e1aeb864dcd1f6249beff0551d92ff1
11:c73c74a7255c27339f6761f43ad589680dda4f97ac1507d3ae91600c7ffe7c43
12:a87da3fab0a83ce8f37d53645cd66246fad68cda6314b3143d17c3184ec3e083
13:1792083d82455340a1d5f99b36659643eec97878ebb7d2fdbd71d7a95beb38fd
14:6d034c068176a4a987a3eabb32373037ee1f1c30407ad02b3a15c0cb81080d95
15:8c07b39b1334436daac889b2b0e9dcf0424574653a42dcfe8077e7bced535b20
16:8d8f4779a2296c8a9c9b5ec3282f49fe98df5bb803cc200a537779cccdaff090
17:f5c2dc400f097cd7613bb21e71d18c86421b346d9a27f76ffca2cfa20b5c3184
18:b9136a35adc27e2859c37fa9d4569be1f8a9b15e19eacf16d31769e776031979
19:dbe9a67b67cb05bc1e9fb14f85bf66f63482a68f72144431e8fb974d23361c15
20:0b9a9cadee8de01aed7e20c9bd8e97517ed6a410c761e633f2d3a7008b2a32bd"#),
        ("555-ico-dn", r#"1:e2784574cbfa6946a1faf3b1374cbf89de7419fd10531ce7b427726824c19e2b
2:c06d44c38b4a762024170364c5f4f57db075091d65040e74d4993355dc473c96
3:8a6643ca66757b7cc2b67e85764990e9113603f33798ad204ae18eee0258d0eb
4:71d3d30e877369c403832de5141978f99587a94665901aa9d5807947823dcf8d
5:ad2661bfc40cbdeec95225556c87565af1e8ed80b3e6a62a66d114066fed464e
6:283584dc304b1ac8986f9af9a979a6c20fbec0ed47ba44343de2f6c4b3856696
7:6857663efde9446a139d34f758d3a2810d4c80955ab1b860ecbdc9c26f631394
8:07c45b7698b1bf7958ce48425d7ea50d01e91854a66ecedafb5b28a6f90ab6aa
9:d0a82a02b94516e54866841cabd767ab9f5c2b9bb10f0bae0a20d94c84efe918
10:376d02fb09dbca37951592f3813ad183c9a067594aa26303fa78e253bffefb28
11:3d4adbe7e6fafa87c2465f0bb03220bda189faae55905821b41d505f8af38d69
12:14165502414bb7376d072d73b59c69a2f2b4ce5fabbf23179689b9bf3f717db5
13:6ca529ab21b6236d6b65d8f2a55a728f7bee797749e9b63ff7e52f8f03648334
14:5e557ad650b45f446a10a0768bd2f0ae1229d10ad3649ef6fd6e4788d3c2520e
15:ee44cab1c11eb9430257b5e9eb674f823140a7a7e54953f513eb6b6110e56ed8
16:967e578b814dcf022a8f06e3d9be4aa6335335a046eea726d056a0c8cdff1ba3
17:56146a10896b9cf99d5795a20b96b30319006593baa79d4c65ad36343b44f580
18:358c8b9c542a950691a5173b46e2357cfabb98bcb65fb30e703248b40cfb1821
19:1705ea79361733ea1acb15eb143c6f7a1138c40a637ddedae7d16f5505eecb84
20:ca7541f0265bd7c3b1f09474391226816377869e49f34ad2136944b633870762"#),
        ("565-rgb-up", r#"1:aa1ebf59e4ba904edae07c8aad8f87a689702f773404e59f83c0b6a4a2c3d68b
2:216be6572a7465f3d8260f340c6f5fba51a375b321a8cb15ef5e3b2c106f741b
3:6dcb527e90d9aa229cd0abfbc00ad1e1844a0b7ea764af7a4261f307777b967c
4:1f4321e947bf2beb59b309d6814339bfdc8b433cd8cc5a5b5ebdc2b22db773ca
5:5de20e0aebf1ba1af241028532bbd0dd4b1e6ffb5530af841738c36adc7348b4
6:dbdb8ec80e7fdb5715a9e50585af73f6345835afcb8e98d7e0e92918bf182c3e
7:b17c2ede95fd85e866a933adb7478daa3f59a9cee43af537c76d1a449c0d8bbe
8:ed51a3ae4f48413842e5aad8f9c0410876a7a2629ad82cff84410e1840954f31
9:a861511117065c27362fadc86206f8be77f256f8fbb8d21d3338778cf493e38c
10:7e25d566f604d76fd06d5edeaabd1d31bad5a1ccb20f2fd06e24779ff6303b6e
11:3dcc944d771992de7a42504cd1cfb777a6c85e2d6a167224a49266b5da1acd7d
12:4b165e0cb4858dd2b3152830eb1e6852009497fa66304d1d94cfcd5eb7c5ac7c
13:6c583e2871c6749084e74c05f0ccbb4d839943af1e59bc1bea43582f8a61638c
14:85c88e5d2d18d408b0b5a18b127f8ac8e8cc280cdc94466945ac46fad8e09f3e
15:55e6c65cdb3c3e577d5edd09f7109abf1c61f6bd81abdda39a8734cb681e37d5
16:af752190d106135ac1f836c03dca3844d682115b8bf174416811400af11d4360
17:c9a054a382b8e1f19293f461bf63f41677fb6c56d0bffa5f42645ed7bee141f3
18:4ef9390c9096fe0d5e7ad06dd7b9a449f2ef6da3b2a581610d9ecac82233a13b
19:5f60777a4b18143a2caa32e90d4d0d515c135360e5720f92b58e824d9f40fa64
20:c5e3c3f6d760a49453d42d890e589ce4fb2a7f197c418d7fc8c8ea95bfaa9c99"#),
        ("565-rgb-dn", r#"1:c4272242f899adad68c20b841f133258ffccd6f118808686de3297a948741f39
2:6c789f5eefb78581219d79132cb5238afad04b59dc6356ddf3ddef444ec8098e
3:ffb2ca73af28db94fd8f79bf080034852600c3e5956f9b3fef215cbf9800deb8
4:ec12974f8a8220eafa74b94eb9277d40115ef4a4cc0b3ac24a695eb472eabfc5
5:e433ce9a30a31ef52e062efaee4c1d4cffd25278e80feb4af4e61f37cb0c21a5
6:25eede4c3cac080d71805081a045521c98b509376d33adbc75331f15d6889cbe
7:1347685e2fea84bf5dab8c6ea8c6f3165fb62594f69d2937a9b38ff79a738ca7
8:56bf45c69c545367290be1f58b0215e918b0102564885bc765ffb073e73661fd
9:5b108468c32df2bc9d4f76d18dbedc668538fe7a1d14bb3cff214c4ee95038c7
10:9ec301f6c72f8f43867c272a36ddaacff5cd864d0b4eb7eb840eef5c083e06dd
11:1200842b0071a8cf31e02a8d6ebe31f659bede84864e00a531b9b64c1eb45ec6
12:497056ad9ab4375f17d69c8af99d5dd032d206b15cd8ccd15c061ce9b070287f
13:0a2e294398e7cb96d8c1d02dbacb9eb22844076cb6554da26400e925543e429c
14:967abb18f6b1e6ea6320e9c5d5d39fc83f0fa698dd1b75cd8e8885cc59af9a79
15:61877b8c982053e93d77d428815186f68587d4005671fcb0a2259981456861dd
16:9d41a946a76c0354485fda2aaa3f50b669b2df09da9b66505506d388b89d4b30
17:09dd54f80c57a25df312c33becf9abfe062cd769dafbcb10c8b2d5d9af2d4010
18:c2c607e43906cc4a54d7bbe5ab5dd8fbf6b61e2ed27a5c79c078f5c9d0832630
19:f9ebf77a7ccbe77b429f2980e5f346f2562c5554f10c431ca4c2a7c9060be1ca
20:53f16933af090d81494d18a5c27958ac712256a8c24447222b0266786f62591e"#),
        ("565-ico-up", r#"1:aa1ebf59e4ba904edae07c8aad8f87a689702f773404e59f83c0b6a4a2c3d68b
2:216be6572a7465f3d8260f340c6f5fba51a375b321a8cb15ef5e3b2c106f741b
3:6dcb527e90d9aa229cd0abfbc00ad1e1844a0b7ea764af7a4261f307777b967c
4:1f4321e947bf2beb59b309d6814339bfdc8b433cd8cc5a5b5ebdc2b22db773ca
5:5de20e0aebf1ba1af241028532bbd0dd4b1e6ffb5530af841738c36adc7348b4
6:dbdb8ec80e7fdb5715a9e50585af73f6345835afcb8e98d7e0e92918bf182c3e
7:b17c2ede95fd85e866a933adb7478daa3f59a9cee43af537c76d1a449c0d8bbe
8:ed51a3ae4f48413842e5aad8f9c0410876a7a2629ad82cff84410e1840954f31
9:a861511117065c27362fadc86206f8be77f256f8fbb8d21d3338778cf493e38c
10:7e25d566f604d76fd06d5edeaabd1d31bad5a1ccb20f2fd06e24779ff6303b6e
11:3dcc944d771992de7a42504cd1cfb777a6c85e2d6a167224a49266b5da1acd7d
12:4b165e0cb4858dd2b3152830eb1e6852009497fa66304d1d94cfcd5eb7c5ac7c
13:6c583e2871c6749084e74c05f0ccbb4d839943af1e59bc1bea43582f8a61638c
14:85c88e5d2d18d408b0b5a18b127f8ac8e8cc280cdc94466945ac46fad8e09f3e
15:55e6c65cdb3c3e577d5edd09f7109abf1c61f6bd81abdda39a8734cb681e37d5
16:af752190d106135ac1f836c03dca3844d682115b8bf174416811400af11d4360
17:c9a054a382b8e1f19293f461bf63f41677fb6c56d0bffa5f42645ed7bee141f3
18:4ef9390c9096fe0d5e7ad06dd7b9a449f2ef6da3b2a581610d9ecac82233a13b
19:5f60777a4b18143a2caa32e90d4d0d515c135360e5720f92b58e824d9f40fa64
20:c5e3c3f6d760a49453d42d890e589ce4fb2a7f197c418d7fc8c8ea95bfaa9c99"#),
        ("565-ico-dn", r#"1:c4272242f899adad68c20b841f133258ffccd6f118808686de3297a948741f39
2:6c789f5eefb78581219d79132cb5238afad04b59dc6356ddf3ddef444ec8098e
3:ffb2ca73af28db94fd8f79bf080034852600c3e5956f9b3fef215cbf9800deb8
4:ec12974f8a8220eafa74b94eb9277d40115ef4a4cc0b3ac24a695eb472eabfc5
5:e433ce9a30a31ef52e062efaee4c1d4cffd25278e80feb4af4e61f37cb0c21a5
6:25eede4c3cac080d71805081a045521c98b509376d33adbc75331f15d6889cbe
7:1347685e2fea84bf5dab8c6ea8c6f3165fb62594f69d2937a9b38ff79a738ca7
8:56bf45c69c545367290be1f58b0215e918b0102564885bc765ffb073e73661fd
9:5b108468c32df2bc9d4f76d18dbedc668538fe7a1d14bb3cff214c4ee95038c7
10:9ec301f6c72f8f43867c272a36ddaacff5cd864d0b4eb7eb840eef5c083e06dd
11:1200842b0071a8cf31e02a8d6ebe31f659bede84864e00a531b9b64c1eb45ec6
12:497056ad9ab4375f17d69c8af99d5dd032d206b15cd8ccd15c061ce9b070287f
13:0a2e294398e7cb96d8c1d02dbacb9eb22844076cb6554da26400e925543e429c
14:967abb18f6b1e6ea6320e9c5d5d39fc83f0fa698dd1b75cd8e8885cc59af9a79
15:61877b8c982053e93d77d428815186f68587d4005671fcb0a2259981456861dd
16:9d41a946a76c0354485fda2aaa3f50b669b2df09da9b66505506d388b89d4b30
17:09dd54f80c57a25df312c33becf9abfe062cd769dafbcb10c8b2d5d9af2d4010
18:c2c607e43906cc4a54d7bbe5ab5dd8fbf6b61e2ed27a5c79c078f5c9d0832630
19:f9ebf77a7ccbe77b429f2980e5f346f2562c5554f10c431ca4c2a7c9060be1ca
20:53f16933af090d81494d18a5c27958ac712256a8c24447222b0266786f62591e"#),
    ];
    for (label, fmt565, ico4, top) in [
        ("555-rgb-up", false, false, false),
        ("555-rgb-dn", false, false, true),
        ("555-ico-up", false, true, false),
        ("555-ico-dn", false, true, true),
        ("565-rgb-up", true, false, false),
        ("565-rgb-dn", true, false, true),
        ("565-ico-up", true, true, false),
        ("565-ico-dn", true, true, true),
    ] {
        let mut lines = Vec::new();
        for w in 1i32..=20 {
            let bmp = build_16bit_bmp(w, 5, fmt565, ico4, top);
            let h = if ico4 {
                decode_ico_rgba_sha(&bmp)
            } else {
                decode_rgba_sha(&bmp)
            };
            lines.push(format!("{w}:{h}"));
        }
        let joined = lines.join("\n");
        eprintln!("[bmp-parity] narrow-{label} =\n{joined}");
        let golden = goldens
            .iter()
            .find(|(l, _)| *l == label)
            .map(|(_, g)| *g)
            .expect("金指纹缺该构型");
        assert_eq!(joined, golden, "「narrow-{label}」扫描与金指纹不一致");
    }
}

// ---------------------------------------------------------------------------
// 语料金指纹（缺语料 SKIP）
// ---------------------------------------------------------------------------

fn corpus_dir(sub: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("target")
        .join("imgbench-corpus")
        .join(sub)
}

/// `target/imgbench-corpus/BMP/*.bmp`（26 张，001–026；金指纹为逐文件哈希表）。
/// simd 优化前、字节正确解码器（8c4957d 验证）采集；语料由 `测试图片\BMP` 快照拷贝而来（不入库）。
/// 目录缺失/为空 → SKIP；某张个别缺失 → 不在目录里即不测（SKIP 常态，非回归）；
/// 目录出现哈希表未收录文件 → 显式失败提示更新金指纹，不静默跳过。
#[test]
fn corpus_bmp_golden() {
    let dir = corpus_dir("BMP");
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.path().extension().map(|x| x == "bmp").unwrap_or(false))
                .map(|e| e.path())
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    if files.is_empty() {
        eprintln!("[bmp-parity] SKIP: target/imgbench-corpus/BMP 缺失或无 .bmp（expected 26）");
        return;
    }
    // 文件名 → RGBA8 sha256 金指纹（decode_rgba_sha 口径）。
    let golden = |name: &str| -> &'static str {
        match name {
            "001.bmp" => "320b18512b9e42d859ba4c80efad01f9046c40e8c19727067ef26f66dfe0dd11",
            "002.bmp" => "060ad11dadf5899978849402d6cc02b21867dd8acc63bfe05661f4b772fdcb23",
            "003.bmp" => "1dfb3ea5ab0e86093231e3587c3ae226cc63c3a95cb64e2783600ef6bcdc493b",
            "004.bmp" => "085caaf8e590f0bfbee92aa09ee97e1336f80246f6c3c8deab9798a3255b24d1",
            "005.bmp" => "bb09008ce983f1d092e442adb6aded50f4cc437aa78745a507e7bcd64f88e80f",
            "006.bmp" => "176169c66f86d188fe073d5f7e9e8e647497acebeb3d37ff343d71823c2bd121",
            "007.bmp" => "17655d60a0b27e0c55f96646675f697e6b083aa4fe4576db1a9d986f3c0d7136",
            "008.bmp" => "c43e9036eb756c0572307ec69a58dc2c39c9587236dd38962df5572b43e3bad3",
            "009.bmp" => "fc918a5c9c785095e9d27d60d1f37943b5ecafa095400266b2c5fa2840059e41",
            "010.bmp" => "19863e795613e634f8d1b3aaae6994519bb761b0d12319e73593b043c33d1c4b",
            "011.bmp" => "eaf003b1262946c7cc19ecac4e00908b2070e4d5216b3a6899028b6de0907887",
            "012.bmp" => "0ec66577d7414087f2056338e72eff7fc66483bd40c384007d960d33666b0bc4",
            "013.bmp" => "18d830f9445c486baac7267df573606ed87010fac0b315e815202b65bd502e53",
            "014.bmp" => "582df53507bbe72bad784cca0f5f8458f2ac8ed505d534f186d9b622fba73de9",
            "015.bmp" => "416cc7c8b185aefe8ccd9a544ffe2ef965411b40a89974c66575f8102ceb12b5",
            "016.bmp" => "e680e82737c297f515afa4793b52ae764361a70fda84431451ac8fdbdbe87c4b",
            "017.bmp" => "e278ee01e28c94a3fe41336163e6723aa62dcb2ee1f0f810b801b3af8ec1b9b1",
            "018.bmp" => "6a6bbfac581631ef07ca02c518357c50eee88dd9347b6fc2ec5f066534f4fda0",
            "019.bmp" => "72f332679ab27f2c86d711b774335c405415030d24c38379f493b14095c69da3",
            "020.bmp" => "2a35336f65f668df5d5cdb227b4a138cdbb4241789fa384a0ccd62095beced83",
            "021.bmp" => "9049d20f7888e157e719793424f07c9ec8bcb550c0d7007af28460634eaad42a",
            "022.bmp" => "7afcf57fb458db540cfb5f05e41022286f0a9d681dbe109f256e2bd1361ab65e",
            "023.bmp" => "0afbb0257407a7b431b3a50a02cbfb330d79984e542becf86bc8182cd3315b4e",
            "024.bmp" => "3effb555238f2be177240c4994ec8b15731618781e73b02b7f554b2de1aa34d8",
            "025.bmp" => "213c242a9b1c8ceb9faa4e7a25e5c9a8141858b4134310373c953814a5d48c0a",
            "026.bmp" => "05176c60ffae131c9f45fdd9ddb559ae9fea2d7f8c2c6c416cb5484430cbf91c",
            other => panic!("语料目录出现未收录文件 {other}（需更新哈希表或移除多余语料）"),
        }
    };
    for p in &files {
        let data = std::fs::read(p).expect("读语料");
        let name = p.file_name().unwrap().to_string_lossy();
        check_hash(&format!("corpus-{name}"), &data, golden(name.as_ref()));
    }
}

// ICO 语料金指纹：已随拆分迁往 kimg_ico crate（另一 agent 处理），此处注释停用。
// `target/imgbench-corpus/ico/1.ico`（ICO 容器，6 条 32bpp BMP 条目 → AND 掩码路径）。
// #[test]
// fn corpus_ico_golden() {
//     let p = corpus_dir("ico").join("1.ico");
//     if !p.exists() {
//         eprintln!("[bmp-parity] SKIP: target/imgbench-corpus/ico/1.ico 缺失");
//         return;
//     }
//     let data = std::fs::read(&p).expect("读语料");
//     check_hash(
//         "corpus-ico-1.ico",
//         &data,
//         "d7f175f9acb776816c1bb26fa1a054ba78ea5b9075aae1889005a7395b6422a1",
//     );
// }
