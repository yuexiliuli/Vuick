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
    check_hash("16bpp-555", &bmp16, "d604076f1e4e304a2534377977c2e45986b2c3212138941a672c2516b14cce20");

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
    check_hash("32bpp-birgb", &bmp32, "706b18f4978ffa655a703fe6085d04a36869e99a39be415edd50b5e54984a2a1");
    let px32b = synth_32bit_data(512, 64, 0x7F);
    let bmp32b = make_bmp(512, 64, 32, 0, 40, None, &[], &px32b, false);
    check_hash("32bpp-birgb-large", &bmp32b, "d3a0e841313454a4403810f8a9d65e82a1a1f24e9afe93091825f6bf98a7cf5d");
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
    check_hash("bitfields16-565", &bmp, "aba7ce2cff7cf2cdc1d29ca5ad3e5ed6666e32db17500f78ad05ce17c6ddd253");

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
    check_hash("bitfields16-555-rev", &bmp, "161970e17415089cac9e316372dadc859f224ceb90be052db5e472c863945516");

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
    check_hash("bitfields16-large", &bmp, "3da5f3dc257df3493ee6d8b933cbc8b31b1cec4cdc10096eca0ac4afdaa55b37");
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

    // 888 无 alpha：Info(40) 头 → RGB32 快路径（丢弃 X）。
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
    check_hash("bitfields32-generic", &bmp, "f87cc032b30c08877eb0519bbf56eb281be2abb079802d649e9a01d06b577a75");

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
    check_hash("rle4", &bmp4, "2c893cd47b6757133987906e00c55e6dcce3c567b9df9e7f91ffecd3604cc6a6");

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
    check_hash("rle8-delta", &bmp8d, "5e93582d42eda8bcf1b08aebcd9da3ea178eea95703d6c95b2234a409a962868");
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
    check_hash("topdown-16bpp", &bmp, "1fcaffd3e325610d7a1f38890f550fb998cc6597c028be445200913633dca3ed");
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
