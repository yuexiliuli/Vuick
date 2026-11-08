//! TIFF A1+A2 parity：自研 `TiffDecoder` vs tiff crate（dev oracle）逐字节对照。
//!
//! 覆盖：
//! 1. A1 矩阵：{gray8(BlackIsZero/WhiteIsZero), rgb8, rgba8} × {Uncompressed, PackBits} ×
//!    {1×1, 7×17, 64×33(多 strip, RPS=16), 单行} — fixture 由 tiff crate 编码器生成（II 端序）；
//! 2. A2 压缩矩阵：{LZW, Deflate(8), ZlibPkzip(32946)} × {无 predictor, Horizontal(2)} ×
//!    {L8/RGB8/RGBA8 × 多尺寸/多 strip/单行} + LZW 大随机图触发表满与 9→12 位宽跳变 +
//!    Uncompressed+predictor(2) + PackBits+predictor(2)；
//! 3. 手写 MM(BE) Uncompressed RGB8（无 oracle 编码器写 BE 或按 RPS 排布时手写字节 + 手写期望）；
//! 4. ICC + Orientation 元数据（手写 writer 构造）；
//! 5. 负例：坏 magic / 截断 IFD / StripOffset 越界 / BitsPerSample=16 / PlanarConfig=2 /
//!    Photometric=Palette / Predictor(3) / CCITT —— 断言自研与 oracle 均 Err、且不 panic；
//!    A2 增加解压层负例（坏 zlib 头 / LZW 截断）走 `expect_read_err` 全 read 路径断言 Err。

use std::io::Cursor;

use kimg_core::color::{ColorType, ExtendedColorType};
use kimg_core::error::ImageError;
use kimg_tiff::ImageDecoder as _;
use kimg_tiff::TiffDecoder;
use tiff::decoder::DecodingResult;
use tiff::encoder::colortype::{Gray8, RGB8, RGBA8};
use tiff::encoder::{Compression, DeflateLevel, Predictor, TiffEncoder};

// ---------------------------------------------------------------------------
// 手写 TIFF 构造（II/MM 均可；未压缩；支持多 strip / ICC / Orientation / 负例）
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Be {
    Le,
    Be,
}

fn w16(out: &mut Vec<u8>, be: Be, v: u16) {
    match be {
        Be::Le => out.extend(v.to_le_bytes()),
        Be::Be => out.extend(v.to_be_bytes()),
    }
}

fn w32(out: &mut Vec<u8>, be: Be, v: u32) {
    match be {
        Be::Le => out.extend(v.to_le_bytes()),
        Be::Be => out.extend(v.to_be_bytes()),
    }
}

struct MiniOpts {
    width: u32,
    height: u32,
    bits: &'static [u16],
    comp: u16,
    photo: u16,
    spp: u16,
    rps: u32,
    planar: u16,
    predictor: u16,
    sample_format: u16,
    extra: &'static [u16],
    orientation: Option<u16>,
    icc: Option<Vec<u8>>,
}

impl MiniOpts {
    fn rgb8(width: u32, height: u32) -> Self {
        Self::rgbx(width, height, &[8, 8, 8], 3, &[])
    }
    /// RGB/RGBA 系：自定 bits/spp/extra（评审补回归：SPP4×ExtraSamples 组合）。
    fn rgbx(width: u32, height: u32, bits: &'static [u16], spp: u16, extra: &'static [u16]) -> Self {
        MiniOpts {
            width,
            height,
            bits,
            comp: 1,
            photo: 2,
            spp,
            rps: height,
            planar: 1,
            predictor: 1,
            sample_format: 1,
            extra,
            orientation: None,
            icc: None,
        }
    }
    fn gray8(width: u32, height: u32, white_is_zero: bool) -> Self {
        MiniOpts {
            width,
            height,
            bits: &[8],
            comp: 1,
            photo: if white_is_zero { 0 } else { 1 },
            spp: 1,
            rps: height,
            planar: 1,
            predictor: 1,
            sample_format: 1,
            extra: &[],
            orientation: None,
            icc: None,
        }
    }
}

fn add_u16(defs: &mut Vec<(u16, u16, Vec<u8>)>, be: Be, tag: u16, v: u16) {
    let mut b = Vec::new();
    w16(&mut b, be, v);
    defs.push((tag, 3, b));
}

fn add_u32(defs: &mut Vec<(u16, u16, Vec<u8>)>, be: Be, tag: u16, v: u32) {
    let mut b = Vec::new();
    w32(&mut b, be, v);
    defs.push((tag, 4, b));
}

fn add_u16s(defs: &mut Vec<(u16, u16, Vec<u8>)>, be: Be, tag: u16, vals: &[u16]) {
    let mut b = Vec::new();
    for &v in vals {
        w16(&mut b, be, v);
    }
    defs.push((tag, 3, b));
}

fn add_bytes(defs: &mut Vec<(u16, u16, Vec<u8>)>, tag: u16, typ: u16, v: &[u8]) {
    defs.push((tag, typ, v.to_vec()));
}

/// 手写 TIFF（未压缩，Chunky），strip_data 顺序按行、跨 strip 连续。
/// strip 数 = ceil(height / rps)；每 strip 末行按图像高度截断。
fn mini_tiff(be: Be, o: &MiniOpts, strip_data: &[u8]) -> Vec<u8> {
    let rps = if o.rps == 0 { o.height.max(1) } else { o.rps };
    let n = o.height.div_ceil(rps) as usize;
    let row_bytes = o.width as usize * o.spp as usize;
    let chunk_rows: Vec<usize> = (0..n)
        .map(|i| {
            let start = i as u32 * rps;
            (o.height - start).min(rps) as usize
        })
        .collect();
    let chunk_sizes: Vec<usize> = chunk_rows.iter().map(|&r| r * row_bytes).collect();
    assert_eq!(chunk_sizes.iter().sum::<usize>(), strip_data.len());

    // 条目定义（值字节已端序化；>4 字节走 data 区，占位后再填 offset）
    let mut defs: Vec<(u16, u16, Vec<u8>)> = Vec::new();
    add_u32(&mut defs, be, 256, o.width);
    add_u32(&mut defs, be, 257, o.height);
    add_u16s(&mut defs, be, 258, o.bits);
    add_u16(&mut defs, be, 259, o.comp);
    add_u16(&mut defs, be, 262, o.photo);
    // 273/279 占位（长度确定：n*4；稍后填真实的 strip offset/count）
    defs.push((273, 4, vec![0u8; n * 4]));
    add_u16(&mut defs, be, 277, o.spp);
    add_u32(&mut defs, be, 278, o.rps);
    defs.push((279, 4, vec![0u8; n * 4]));
    add_u16(&mut defs, be, 284, o.planar);
    add_u16(&mut defs, be, 317, o.predictor);
    add_u16(&mut defs, be, 339, o.sample_format);
    if !o.extra.is_empty() {
        add_u16s(&mut defs, be, 338, o.extra);
    }
    if let Some(orient) = o.orientation {
        add_u16(&mut defs, be, 274, orient);
    }
    if let Some(icc) = o.icc.as_deref() {
        add_bytes(&mut defs, 34675, 7, icc); // UNDEFINED
    }

    // 分配 data 区位置（仅 >4 字节条目入 data 区）
    let ifd_size = 2 + defs.len() * 12 + 4;
    let mut data_pos = 8usize + ifd_size;
    let mut blob_pos: Vec<Option<usize>> = vec![None; defs.len()];
    for (i, (_, _, b)) in defs.iter().enumerate() {
        if b.len() > 4 {
            blob_pos[i] = Some(data_pos);
            data_pos += b.len();
        }
    }
    let strip_data_off = data_pos;

    // 填充 273/279
    {
        let mut off = 0usize;
        let mut offs = Vec::with_capacity(n);
        let mut lens = Vec::with_capacity(n);
        for &s in &chunk_sizes {
            offs.push(u32::try_from(strip_data_off + off).unwrap());
            lens.push(u32::try_from(s).unwrap());
            off += s;
        }
        let idx = defs.iter().position(|d| d.0 == 273).unwrap();
        let mut b = Vec::new();
        for &v in &offs {
            w32(&mut b, be, v);
        }
        defs[idx] = (273, 4, b);
        let idx = defs.iter().position(|d| d.0 == 279).unwrap();
        let mut b = Vec::new();
        for &v in &lens {
            w32(&mut b, be, v);
        }
        defs[idx] = (279, 4, b);
    }

    // 组装
    let mut out = Vec::new();
    match be {
        Be::Le => out.extend([0x49, 0x49, 0x2a, 0x00]),
        Be::Be => out.extend([0x4d, 0x4d, 0x00, 0x2a]),
    }
    w32(&mut out, be, 8);
    w16(&mut out, be, defs.len() as u16);
    for (i, (tag, typ, b)) in defs.iter().enumerate() {
        w16(&mut out, be, *tag);
        w16(&mut out, be, *typ);
        // count = 元素个数（值字节数 ÷ 元素字节数）
        let elem = match *typ {
            1 | 2 | 6 | 7 => 1,
            3 | 8 => 2,
            4 | 9 | 11 | 13 => 4,
            5 | 10 | 12 => 8,
            _ => 1,
        };
        w32(&mut out, be, (b.len() / elem) as u32);
        if b.len() <= 4 {
            out.extend_from_slice(b);
            // 值字段恒 4 字节（不足补 0），保证每条 IFD 记录恰 12 字节
            out.extend(std::iter::repeat(0u8).take(4 - b.len()));
        } else {
            w32(&mut out, be, blob_pos[i].unwrap() as u32);
        }
    }
    w32(&mut out, be, 0); // next IFD
    for (i, (_, _, b)) in defs.iter().enumerate() {
        if b.len() > 4 {
            assert_eq!(out.len(), blob_pos[i].unwrap());
            out.extend_from_slice(b);
        }
    }
    assert_eq!(out.len(), strip_data_off);
    out.extend_from_slice(strip_data);
    out
}

/// 把 LE mini 文件里某个 LONG 单值标签的内联 4 字节值原地改写（负例/定向补丁用）。
fn patch_le_long_tag32(bytes: &mut [u8], tag: u16, value: u32) {
    let ifd0 = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
    let count = u16::from_le_bytes(bytes[ifd0..ifd0 + 2].try_into().unwrap()) as usize;
    for i in 0..count {
        let e = ifd0 + 2 + i * 12;
        let t = u16::from_le_bytes(bytes[e..e + 2].try_into().unwrap());
        if t == tag {
            bytes[e + 8..e + 12].copy_from_slice(&value.to_le_bytes());
            return;
        }
    }
    panic!("patch_le_long_tag32: 未找到 tag {tag}");
}

/// 把 LE 文件里某个 SHORT 单值标签的内联值改为新值。
fn patch_le_short_tag16(bytes: &mut [u8], tag: u16, value: u16) {
    let ifd0 = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
    let count = u16::from_le_bytes(bytes[ifd0..ifd0 + 2].try_into().unwrap()) as usize;
    for i in 0..count {
        let e = ifd0 + 2 + i * 12;
        let t = u16::from_le_bytes(bytes[e..e + 2].try_into().unwrap());
        if t == tag {
            bytes[e + 8..e + 10].copy_from_slice(&value.to_le_bytes());
            return;
        }
    }
    panic!("patch_le_short_tag16: 未找到 tag {tag}");
}

// ---------------------------------------------------------------------------
// fixture 生成（tiff crate 编码器）
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Layout {
    Gray8,
    Rgb8,
    Rgba8,
}

fn spp(l: Layout) -> usize {
    match l {
        Layout::Gray8 => 1,
        Layout::Rgb8 => 3,
        Layout::Rgba8 => 4,
    }
}

fn color_type_of(l: Layout) -> ColorType {
    match l {
        Layout::Gray8 => ColorType::L8,
        Layout::Rgb8 => ColorType::Rgb8,
        Layout::Rgba8 => ColorType::Rgba8,
    }
}

/// 确定性数据：造字面段 + 长重复段（使 PackBits 两种包都出现）。
fn make_data(len: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(len);
    let mut i = 0usize;
    while v.len() < len {
        let seg = (i % 13) as u8;
        if seg >= 10 {
            // 长重复段（10..40 字节）
            let count = ((i * 7) % 33 + 8).min(len - v.len());
            v.extend(std::iter::repeat(seg.wrapping_mul(11)).take(count));
        } else {
            v.push(seg.wrapping_mul(37) ^ (i as u8).wrapping_mul(7));
        }
        i += 1;
    }
    v
}

/// 用 tiff crate 编码器生成 fixture（II 端序），可选 Horizontal predictor。
fn encode_pred(
    layout: Layout,
    w: u32,
    h: u32,
    comp: Compression,
    rps: Option<u32>,
    predictor: bool,
    data: &[u8],
) -> Vec<u8> {
    let mut cursor = Cursor::new(Vec::new());
    let mut t = TiffEncoder::new(&mut cursor).unwrap().with_compression(comp);
    if predictor {
        t = t.with_predictor(Predictor::Horizontal);
    }
    match layout {
        Layout::Gray8 => {
            let mut image = t.new_image::<Gray8>(w, h).unwrap();
            if let Some(r) = rps {
                image.rows_per_strip(r).unwrap();
            }
            image.write_data(data).unwrap();
        }
        Layout::Rgb8 => {
            let mut image = t.new_image::<RGB8>(w, h).unwrap();
            if let Some(r) = rps {
                image.rows_per_strip(r).unwrap();
            }
            image.write_data(data).unwrap();
        }
        Layout::Rgba8 => {
            let mut image = t.new_image::<RGBA8>(w, h).unwrap();
            if let Some(r) = rps {
                image.rows_per_strip(r).unwrap();
            }
            image.write_data(data).unwrap();
        }
    }
    drop(t);
    cursor.into_inner()
}

/// 用 tiff crate 编码器生成 A1 fixture（II 端序，无 predictor）。
fn encode(layout: Layout, w: u32, h: u32, comp: Compression, rps: Option<u32>, data: &[u8]) -> Vec<u8> {
    encode_pred(layout, w, h, comp, rps, false, data)
}

// ---------------------------------------------------------------------------
// oracle 端
// ---------------------------------------------------------------------------

/// tiff crate 解码出 (kimg ColorType, dims, 原始 8bit 字节)。
fn oracle_decode(bytes: &[u8]) -> Result<(ColorType, (u32, u32), Vec<u8>), tiff::TiffError> {
    let mut d = tiff::decoder::Decoder::new(Cursor::new(bytes))?;
    let ct = d.colortype()?;
    let dim = d.dimensions()?;
    let kt = match ct {
        tiff::ColorType::Gray(8) => ColorType::L8,
        tiff::ColorType::RGB(8) => ColorType::Rgb8,
        tiff::ColorType::RGBA(8) => ColorType::Rgba8,
        other => panic!("oracle 出现非 A1 色型 {other:?}"),
    };
    let bytes = match d.read_image()? {
        DecodingResult::U8(v) => v,
        other => panic!("oracle 出现非 8bit 结果: {other:?}"),
    };
    Ok((kt, dim, bytes))
}

/// 三方对照：oracle == expected == 自研（逐字节）。
fn check_parity(bytes: &[u8], ct: ColorType, dim: (u32, u32), expected: &[u8]) {
    let (okt, odim, obytes) = oracle_decode(bytes)
        .unwrap_or_else(|e| panic!("oracle 应能解码: {e:?}"));
    assert_eq!(okt, ct, "oracle 色型");
    assert_eq!(odim, dim, "oracle 尺寸");
    assert_eq!(obytes, expected, "oracle 输出必须等于 expected");

    let d = TiffDecoder::new(Cursor::new(bytes))
        .unwrap_or_else(|e| panic!("自研应能构造: {e:?}"));
    assert_eq!(d.dimensions(), dim, "自研尺寸");
    assert_eq!(d.color_type(), ct, "自研色型");
    assert_eq!(d.original_color_type(), ExtendedColorType::from(ct), "自研原始色型");
    let total = usize::try_from(d.total_bytes()).unwrap();
    let mut buf = vec![0u8; total];
    d.read_image(&mut buf)
        .unwrap_or_else(|e| panic!("自研 read_image 应成功: {e:?}"));
    assert_eq!(&buf[..], expected, "自研输出必须逐字节等于 oracle/expected");
}

/// 自研构造应返回 Err，取回错误（TiffDecoder 不实现 Debug，不能直接用 unwrap_err）。
fn expect_err(bytes: &[u8]) -> ImageError {
    match TiffDecoder::new(Cursor::new(bytes)) {
        Err(e) => e,
        Ok(_) => panic!("应当返回 Err"),
    }
}

/// oracle 完整解码（new + read_image）是否失败。
fn oracle_fails_full(bytes: &[u8]) -> bool {
    tiff::decoder::Decoder::new(Cursor::new(bytes))
        .and_then(|mut d| d.read_image())
        .is_err()
}

// ---------------------------------------------------------------------------
// A2 负例辅助：解压层错误路径（越界/压缩流损坏），与 A1 的 `expect_err`（new() 即拒）区分
// ---------------------------------------------------------------------------

/// 自研完整解码（new + read_image）应失败，取回错误。A1 `expect_err` 只查 `new()`，
/// A2 负例（坏 zlib / LZW 截断）在解压层才失败，故需整条 read 路径。
fn expect_read_err(bytes: &[u8]) -> ImageError {
    match TiffDecoder::new(Cursor::new(bytes)) {
        Err(e) => e,
        Ok(d) => {
            let total = usize::try_from(d.total_bytes()).unwrap();
            let mut buf = vec![0u8; total];
            if let Err(e) = d.read_image(&mut buf) {
                e
            } else {
                panic!("read_image 应当返回 Err");
            }
        }
    }
}

/// 取 LE 文件首个 strip 数据偏移（解析 IFD0 的 StripOffsets(273) 首值，兼容单值内联/数组）。
fn first_strip_data_offset(bytes: &[u8]) -> usize {
    let ifd0 = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
    let count = u16::from_le_bytes(bytes[ifd0..ifd0 + 2].try_into().unwrap()) as usize;
    for i in 0..count {
        let e = ifd0 + 2 + i * 12;
        let t = u16::from_le_bytes(bytes[e..e + 2].try_into().unwrap());
        if t == 273 {
            let n = u32::from_le_bytes(bytes[e + 4..e + 8].try_into().unwrap());
            if n == 1 {
                return u32::from_le_bytes(bytes[e + 8..e + 12].try_into().unwrap()) as usize;
            }
            let arr_off = u32::from_le_bytes(bytes[e + 8..e + 12].try_into().unwrap()) as usize;
            return u32::from_le_bytes(bytes[arr_off..arr_off + 4].try_into().unwrap()) as usize;
        }
    }
    panic!("first_strip_data_offset: 未找到 StripOffsets(273)");
}

/// 把单 strip LE 文件的 strip 压缩数据尾部截掉 `k` 字节，并同步改小 279 StripByteCounts，
/// 使文件界内结构合法（不触发 new() 越界）、但压缩流变短——负例走「解压层报错」。
fn truncate_strip_data_tail(bytes: &[u8], k: usize) -> Vec<u8> {
    let strip_off = first_strip_data_offset(bytes);
    let orig = bytes.len() - strip_off;
    assert!(k < orig, "截断量须小于 strip 数据长度");
    let new_cnt = orig - k;
    let mut b = bytes.to_vec();
    patch_le_long_tag32(&mut b, 279, new_cnt as u32);
    b.truncate(strip_off + new_cnt);
    b
}

// ---------------------------------------------------------------------------
// A1 矩阵
// ---------------------------------------------------------------------------

#[test]
fn matrix_uncompressed_l8_rgb8_rgba8() {
    let sizes: &[(u32, u32)] = &[(1, 1), (7, 17), (64, 33), (1, 11), (31, 1)];
    for &(w, h) in sizes {
        for layout in [Layout::Gray8, Layout::Rgb8, Layout::Rgba8] {
            let data = make_data((w * h) as usize * spp(layout));
            let rps = Some(16);
            let bytes = encode(layout, w, h, Compression::Uncompressed, rps, &data);
            // 多 strip：仅当 h>rps 才真正多 strip；rps 覆盖到编码器由 rows_per_strip(16) 决定
            check_parity(&bytes, color_type_of(layout), (w, h), &data);
        }
    }
}

#[test]
fn matrix_packbits_l8_rgb8_rgba8() {
    let sizes: &[(u32, u32)] = &[(1, 1), (7, 17), (64, 33), (1, 9), (29, 1)];
    for &(w, h) in sizes {
        for layout in [Layout::Gray8, Layout::Rgb8, Layout::Rgba8] {
            let data = make_data((w * h) as usize * spp(layout));
            for rps in [Some(16u32), None] {
                let bytes = encode(layout, w, h, Compression::Packbits, rps, &data);
                check_parity(&bytes, color_type_of(layout), (w, h), &data);
            }
        }
    }
}

#[test]
fn gray_white_is_zero_matches_oracle() {
    // 编码器只写 BlackIsZero(1)；把 262 改为 WhiteIsZero(0) 后 oracle 与自研都应反向输出。
    let (w, h) = (11u32, 9u32);
    let data = make_data((w * h) as usize);
    let mut bytes = encode(Layout::Gray8, w, h, Compression::Uncompressed, Some(16), &data);
    patch_le_short_tag16(&mut bytes, 262, 0);
    let expected: Vec<u8> = data.iter().map(|&b| !b).collect();
    check_parity(&bytes, ColorType::L8, (w, h), &expected);
}

#[test]
fn max_multistrip_boundary() {
    // 64×33 RPS=16：3 个 strip（16/16/1），末 strip 截断到 1 行
    let (w, h) = (64u32, 33u32);
    for layout in [Layout::Gray8, Layout::Rgb8, Layout::Rgba8] {
        let data = make_data((w * h) as usize * spp(layout));
        let bytes = encode(layout, w, h, Compression::Uncompressed, Some(16), &data);
        check_parity(&bytes, color_type_of(layout), (w, h), &data);
        let bytes = encode(layout, w, h, Compression::Packbits, Some(16), &data);
        check_parity(&bytes, color_type_of(layout), (w, h), &data);
    }
}

// ---------------------------------------------------------------------------
// MM (BE) 端序：手写文件 + oracle 对照
// ---------------------------------------------------------------------------

#[test]
fn mm_big_endian_rgb8() {
    let (w, h) = (3u32, 2u32);
    let data = make_data((w * h) as usize * 3);
    let bytes = mini_tiff(Be::Be, &MiniOpts::rgb8(w, h), &data);
    check_parity(&bytes, ColorType::Rgb8, (w, h), &data);
}

#[test]
fn mm_big_endian_gray_and_multistrip() {
    // 多 strip + BE + 灰度 BlackIsZero
    let (w, h) = (5u32, 11u32);
    let data = make_data((w * h) as usize);
    let mut o = MiniOpts::gray8(w, h, false);
    o.rps = 4; // ceil(11/4)=3 strips
    let bytes = mini_tiff(Be::Be, &o, &data);
    check_parity(&bytes, ColorType::L8, (w, h), &data);
}

#[test]
fn le_white_is_zero_handwritten() {
    let (w, h) = (4u32, 3u32);
    let data = make_data((w * h) as usize);
    let bytes = mini_tiff(Be::Le, &MiniOpts::gray8(w, h, true), &data);
    let expected: Vec<u8> = data.iter().map(|&b| !b).collect();
    check_parity(&bytes, ColorType::L8, (w, h), &expected);
}

// ---------------------------------------------------------------------------
// 元数据：ICC + Orientation
// ---------------------------------------------------------------------------

#[test]
fn icc_profile_and_orientation() {
    let (w, h) = (2u32, 2u32);
    let data = make_data(w as usize * h as usize * 3);
    let icc = (0u8..64).collect::<Vec<u8>>();
    let mut o = MiniOpts::rgb8(w, h);
    o.orientation = Some(6);
    o.icc = Some(icc.clone());
    let bytes = mini_tiff(Be::Le, &o, &data);

    // 像素仍 parity
    let bytes_clone = bytes.clone();
    check_parity(&bytes_clone, ColorType::Rgb8, (w, h), &data);

    let mut d = TiffDecoder::new(Cursor::new(&bytes)).unwrap();
    assert_eq!(d.icc_profile().unwrap(), Some(icc.to_vec()), "ICC 应原样返回");
    assert_eq!(
        d.orientation().unwrap(),
        kimg_core::metadata::Orientation::Rotate90,
        "Orientation=6 → Rotate90"
    );
    // oracle 侧亦可读到 ICC/Orientation
    let mut od = tiff::decoder::Decoder::new(Cursor::new(&bytes)).unwrap();
    assert_eq!(
        od.get_tag_u8_vec(tiff::tags::Tag::IccProfile).unwrap(),
        icc.to_vec()
    );
    let orient = od
        .find_tag(tiff::tags::Tag::Orientation)
        .unwrap()
        .and_then(|v| v.into_u16().ok())
        .unwrap();
    assert_eq!(orient, 6);
}

// ---------------------------------------------------------------------------
// 负例
// ---------------------------------------------------------------------------

#[test]
fn negative_bad_magic() {
    let bytes = b"not-a-tiff-magic!!".to_vec();
    assert!(TiffDecoder::new(Cursor::new(&bytes)).is_err(), "坏 magic 必须 Err");
    assert!(oracle_fails_full(&bytes));
}

#[test]
fn negative_short_ifd() {
    // 合法头 + IFD 偏移越界（文件只有 8 字节头）
    let mut bytes = vec![0x49, 0x49, 0x2a, 0x00];
    bytes.extend(8u32.to_le_bytes());
    assert!(TiffDecoder::new(Cursor::new(&bytes)).is_err(), "截断 IFD 必须 Err");
    assert!(oracle_fails_full(&bytes));
}

#[test]
fn negative_strip_offset_out_of_bounds() {
    let (w, h) = (1u32, 1u32);
    let data = make_data(3);
    let mut bytes = mini_tiff(Be::Le, &MiniOpts::rgb8(w, h), &data);
    // 把单 strip offset 改成指向文件外的巨大偏移
    patch_le_long_tag32(&mut bytes, 273, 0x0FFF_FFFF);
    let err = expect_err(&bytes);
    assert!(!matches!(err, ImageError::IoError(_)), "越界应报解析类错误: {err:?}");
    assert!(oracle_fails_full(&bytes), "oracle 对越界 strip 也应报错");
}

#[test]
fn negative_bits16_unsupported() {
    let (w, h) = (2u32, 2u32);
    // 16bit 文件：解码在 new() 即拒绝，strip 数据不必真实 2 字节/样本
    let data = vec![0u8; w as usize * h as usize];
    let mut o = MiniOpts::gray8(w, h, false);
    o.bits = &[16];
    let bytes = mini_tiff(Be::Le, &o, &data);
    let err = expect_err(&bytes);
    assert!(matches!(err, ImageError::Unsupported(_)), "16bit 必须 Unsupported: {err:?}");
    assert!(oracle_fails_full(&bytes));
}

#[test]
fn negative_planar_separate_unsupported() {
    let (w, h) = (2u32, 2u32);
    let data = vec![7u8; w as usize * h as usize * 3];
    let mut o = MiniOpts::rgb8(w, h);
    o.planar = 2;
    let bytes = mini_tiff(Be::Le, &o, &data);
    let err = expect_err(&bytes);
    assert!(matches!(err, ImageError::Unsupported(_)), "Separate 必须 Unsupported: {err:?}");
    assert!(oracle_fails_full(&bytes));
}

#[test]
fn negative_palette_unsupported() {
    let (w, h) = (2u32, 2u32);
    let data = vec![0u8; w as usize * h as usize];
    let mut o = MiniOpts::gray8(w, h, false);
    o.photo = 3; // Palette
    let bytes = mini_tiff(Be::Le, &o, &data);
    let err = expect_err(&bytes);
    assert!(matches!(err, ImageError::Unsupported(_)), "Palette 必须 Unsupported: {err:?}");
    assert!(oracle_fails_full(&bytes));
}

/// Predictor(3) 浮点差分：自研与 oracle 均拒（oracle 对 Uint 采样格式报 FloatingPointPredictor）。
#[test]
fn negative_predictor_floating_unsupported() {
    let (w, h) = (2u32, 2u32);
    let data = vec![1u8; w as usize * h as usize * 3];
    let mut o = MiniOpts::rgb8(w, h);
    o.predictor = 3; // FloatingPoint —— 不支持
    let bytes = mini_tiff(Be::Le, &o, &data);
    let err = expect_err(&bytes);
    assert!(
        matches!(err, ImageError::Unsupported(_)),
        "浮点差分 Predictor(3) 必须 Unsupported: {err:?}"
    );
    assert!(oracle_fails_full(&bytes), "oracle 对 Predictor(3) 也应拒绝");
}

/// 压缩不支持 + predictor(2) 组合：CCITT Fax3(3) 在 new() 即拒 Unsupported（先于 predictor 校验）。
#[test]
fn negative_ccitt_fax3_with_predictor_unsupported() {
    let (w, h) = (2u32, 2u32);
    let data = vec![3u8; w as usize * h as usize];
    let mut o = MiniOpts::gray8(w, h, false);
    o.comp = 3; // Fax3 / CCITT —— A3 后置
    o.predictor = 2;
    let bytes = mini_tiff(Be::Le, &o, &data);
    let err = expect_err(&bytes);
    assert!(
        matches!(err, ImageError::Unsupported(_)),
        "CCITT+预测应为 Unsupported: {err:?}"
    );
}

// ---------------------------------------------------------------------------
// 评审回归：ExtraSamples 对齐 tiff crate（SPP4 全收 / RGB3+extra 拒收） + 灰边界 + 炸弹
// ---------------------------------------------------------------------------

/// SPP4 + ExtraSamples=[2]（UnassociatedAlpha）→ Rgba8，逐字节对拍。
#[test]
fn spp4_extra_unassociated_rgba8() {
    let (w, h) = (3u32, 2u32);
    let data = make_data((w * h) as usize * 4);
    let o = MiniOpts::rgbx(w, h, &[8, 8, 8, 8], 4, &[2]);
    let bytes = mini_tiff(Be::Le, &o, &data);
    check_parity(&bytes, ColorType::Rgba8, (w, h), &data);
}

/// SPP4 + ExtraSamples=[1]（AssociatedAlpha）→ oracle 同为 RGBA8，直通 4 字节/像素。
#[test]
fn spp4_extra_associated_rgba8() {
    let (w, h) = (2u32, 3u32);
    let data = make_data((w * h) as usize * 4);
    let o = MiniOpts::rgbx(w, h, &[8, 8, 8, 8], 4, &[1]);
    let bytes = mini_tiff(Be::Le, &o, &data);
    check_parity(&bytes, ColorType::Rgba8, (w, h), &data);
}

/// SPP4 + ExtraSamples=[0]（Unspecified 非 alpha）→ tiff crate 输出 RGB3（丢弃附加样本），
/// 自研以 RgbCompact 对齐；逐字节对拍。
#[test]
fn spp4_extra_unspecified_compacts_to_rgb8() {
    let (w, h) = (3u32, 2u32);
    let data = make_data((w * h) as usize * 4);
    let o = MiniOpts::rgbx(w, h, &[8, 8, 8, 8], 4, &[0]);
    let bytes = mini_tiff(Be::Le, &o, &data);
    let expected: Vec<u8> = data.chunks_exact(4).flat_map(|px| px[..3].to_vec()).collect();
    assert_eq!(expected.len(), (w * h) as usize * 3);
    check_parity(&bytes, ColorType::Rgb8, (w, h), &expected);
}

/// SPP3 + ExtraSamples=[2]（photometric_samples=1 + 附加样本）→ tiff crate 拒绝、自研 Unsupported。
#[test]
fn rgb3_extra_unassociated_unsupported() {
    let (w, h) = (2u32, 2u32);
    let data = make_data((w * h) as usize * 3);
    let o = MiniOpts::rgbx(w, h, &[8, 8, 8], 3, &[2]);
    let bytes = mini_tiff(Be::Le, &o, &data);
    let err = expect_err(&bytes);
    assert!(matches!(err, ImageError::Unsupported(_)), "SPP3+extra 必须 Unsupported: {err:?}");
    assert!(oracle_fails_full(&bytes), "oracle 对 SPP3+extra 也应拒绝");
}

/// 灰反转显式边界断言：(0,127,128,255) 反转 → (255,128,127,0)，并 oracle 对拍。
#[test]
fn gray_white_is_zero_boundary_values() {
    let (w, h) = (4u32, 1u32);
    let data = vec![0u8, 127, 128, 255];
    let expected = vec![255u8, 128, 127, 0];
    let bytes = mini_tiff(Be::Le, &MiniOpts::gray8(w, h, true), &data);
    check_parity(&bytes, ColorType::L8, (w, h), &expected);
}

/// 手写 2³² 高度炸弹：单值 273/279（count=1）+ RPS=1 → strip_count≈2³²。
/// 修复前 `replicate` 会在 new() 内预分配 2×~16GB → OOM；修复后应在一致性校验处
/// （`StripOffsets 条目数与实际 strip 数不一致`）快速 Err、绝不触碰分配炸弹。
fn bomb_tiff() -> Vec<u8> {
    let mut b: Vec<u8> = vec![0x49, 0x49, 0x2a, 0x00];
    b.extend(8u32.to_le_bytes());
    let mut entries: Vec<(u16, u16, Vec<u8>)> = Vec::new();
    let add_shot = |entries: &mut Vec<(u16, u16, Vec<u8>)>, tag: u16, v: u16| {
        let mut val = Vec::new();
        val.extend(v.to_le_bytes());
        entries.push((tag, 3, val));
    };
    let add_lng = |entries: &mut Vec<(u16, u16, Vec<u8>)>, tag: u16, v: u32| {
        let mut val = Vec::new();
        val.extend(v.to_le_bytes());
        entries.push((tag, 4, val));
    };
    add_lng(&mut entries, 256, 100); // 宽
    add_lng(&mut entries, 257, u32::MAX); // 高 = 2³²−1 → strip_count ≈ 2³²
    add_shot(&mut entries, 258, 8);
    add_shot(&mut entries, 259, 1);
    add_shot(&mut entries, 262, 1);
    add_lng(&mut entries, 273, 200); // 单值 StripOffsets（count=1）
    add_shot(&mut entries, 277, 1);
    add_lng(&mut entries, 278, 1); // RowsPerStrip=1 → 单值数组仅 1 个 strip 合法；此处非法
    add_lng(&mut entries, 279, 1); // 单值 StripByteCounts（count=1）
    b.extend((entries.len() as u16).to_le_bytes());
    for (tag, typ, val) in entries {
        b.extend(tag.to_le_bytes());
        b.extend(typ.to_le_bytes());
        let elem = if typ == 3 { 2u32 } else { 4u32 };
        b.extend(((val.len() as u32) / elem).to_le_bytes());
        b.extend_from_slice(&val);
        while b.len() % 4 != 0 {
            b.push(0);
        }
    }
    b.extend(0u32.to_le_bytes()); // next IFD
    b
}

#[test]
fn negative_huge_height_bomb_is_err_fast() {
    let bytes = bomb_tiff();
    let err = expect_err(&bytes);
    assert!(
        !matches!(err, ImageError::IoError(_)),
        "炸弹应为一致性/解析类错误（快速失败、不 OOM），实为 {err:?}"
    );
    assert!(oracle_fails_full(&bytes), "oracle 对炸弹也应拒绝");
}

// ---------------------------------------------------------------------------
// set_limits：分配上限对 bomb 的护栏
// ---------------------------------------------------------------------------

#[test]
fn set_limits_rejects_oversized_decode() {
    let (w, h) = (2000u32, 2000u32);
    let data = make_data((w * h) as usize * 4);
    let mut o = MiniOpts::rgb8(w, h);
    o.bits = &[8, 8, 8, 8];
    o.spp = 4;
    let bytes = mini_tiff(Be::Le, &o, &data);
    let mut d = TiffDecoder::new(Cursor::new(&bytes)).unwrap();
    let mut limits = kimg_core::limits::Limits::default();
    limits.max_alloc = Some(4 * 1024 * 1024); // 16MB 图像 > 4MB 上限
    d.set_limits(limits).unwrap();
    let total = usize::try_from(d.total_bytes()).unwrap();
    let mut buf = vec![0u8; total];
    let err = d.read_image(&mut buf).unwrap_err();
    assert!(
        matches!(err, ImageError::Limits(_)),
        "超 max_alloc 必须 Limits 错误: {err:?}"
    );
}

// ---------------------------------------------------------------------------
// A2：Deflate(8)/ZlibPkzip(32946)/LZW × Horizontal predictor(2) 对拍矩阵 + 负例
// ---------------------------------------------------------------------------

/// 确定性伪随机数据（xorshift32）：高熵以触发 LZW 表满（+4096 项）与码宽 9→12 全程跳变。
fn random_data(len: usize) -> Vec<u8> {
    let mut x: u32 = 0x1234_5678;
    let mut v = Vec::with_capacity(len);
    for _ in 0..len {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        v.push((x >> 24) as u8);
    }
    v
}

#[test]
fn matrix_deflate_zlibpkzip_predictor() {
    let sizes: &[(u32, u32)] = &[(7, 17), (64, 33), (31, 1)];
    for &(w, h) in sizes {
        for layout in [Layout::Gray8, Layout::Rgb8, Layout::Rgba8] {
            let data = make_data((w * h) as usize * spp(layout));
            let comp = Compression::Deflate(DeflateLevel::default());
            // Deflate(8) 无 predictor
            let bytes = encode_pred(layout, w, h, comp, Some(16), false, &data);
            check_parity(&bytes, color_type_of(layout), (w, h), &data);
            // Deflate(8) 带 horizontal predictor(2)
            let bytes = encode_pred(layout, w, h, comp, Some(16), true, &data);
            check_parity(&bytes, color_type_of(layout), (w, h), &data);
            // ZlibPkzip(32946)：编码器只写 8，把 259 压缩标签改写成 0x80B2（字节载荷同为 zlib）
            let mut biz = encode_pred(layout, w, h, comp, Some(16), true, &data);
            patch_le_short_tag16(&mut biz, 259, 0x80B2);
            check_parity(&biz, color_type_of(layout), (w, h), &data);
        }
    }
}

#[test]
fn matrix_lzw_predictor_and_width_jump() {
    let sizes: &[(u32, u32)] = &[(7, 17), (64, 33), (31, 1)];
    for &(w, h) in sizes {
        for layout in [Layout::Gray8, Layout::Rgb8, Layout::Rgba8] {
            let data = make_data((w * h) as usize * spp(layout));
            let bytes = encode_pred(layout, w, h, Compression::Lzw, Some(16), false, &data);
            check_parity(&bytes, color_type_of(layout), (w, h), &data);
            let bytes = encode_pred(layout, w, h, Compression::Lzw, Some(16), true, &data);
            check_parity(&bytes, color_type_of(layout), (w, h), &data);
        }
    }
    // 大随机图：迫使 LZW 表满（4096 项）→ 9→10→11→12 位宽全程跳变（含满表后不再增项）
    let big: &[(u32, u32, Layout)] = &[(160, 120, Layout::Gray8), (96, 96, Layout::Rgb8)];
    for &(w, h, layout) in big {
        let data = random_data((w * h) as usize * spp(layout));
        let bytes = encode_pred(layout, w, h, Compression::Lzw, None, false, &data);
        check_parity(&bytes, color_type_of(layout), (w, h), &data);
        let bytes = encode_pred(layout, w, h, Compression::Lzw, None, true, &data);
        check_parity(&bytes, color_type_of(layout), (w, h), &data);
    }
}

#[test]
fn uncompressed_horizontal_predictor() {
    let sizes: &[(u32, u32)] = &[(3, 5), (1, 17), (64, 33)];
    for &(w, h) in sizes {
        for layout in [Layout::Gray8, Layout::Rgb8, Layout::Rgba8] {
            let data = make_data((w * h) as usize * spp(layout));
            let bytes = encode_pred(layout, w, h, Compression::Uncompressed, Some(16), true, &data);
            check_parity(&bytes, color_type_of(layout), (w, h), &data);
        }
    }
}

/// PackBits + Horizontal predictor(2)：共享 path（PackBits 解压已 A1 覆盖、predictor 已 A2 覆盖）
/// 的连接验证，低风险加覆盖更全。
#[test]
fn packbits_horizontal_predictor() {
    let sizes: &[(u32, u32)] = &[(7, 17), (64, 33), (31, 1)];
    for &(w, h) in sizes {
        for layout in [Layout::Gray8, Layout::Rgb8, Layout::Rgba8] {
            let data = make_data((w * h) as usize * spp(layout));
            let bytes = encode_pred(layout, w, h, Compression::Packbits, Some(16), true, &data);
            check_parity(&bytes, color_type_of(layout), (w, h), &data);
        }
    }
}

#[test]
fn negative_bad_zlib_header() {
    let (w, h) = (8u32, 3u32);
    let data = make_data((w * h) as usize * 3);
    let mut bytes =
        encode_pred(Layout::Rgb8, w, h, Compression::Deflate(DeflateLevel::default()), None, false, &data);
    let off = first_strip_data_offset(&bytes);
    bytes[off] = 0xFF; // 破坏 zlib CMF（0x78 → 0xFF，CM≠8）→ fdeflate/flate2 均应失败
    let err = expect_read_err(&bytes);
    assert!(
        !matches!(err, ImageError::Unsupported(_)),
        "坏 zlib 头应为解码错误而非 Unsupported: {err:?}"
    );
    assert!(oracle_fails_full(&bytes), "oracle 对坏 zlib 头也应失败");
}

#[test]
fn negative_lzw_truncated() {
    let (w, h) = (7u32, 17u32);
    let data = make_data((w * h) as usize * 3);
    let bytes = encode_pred(Layout::Rgb8, w, h, Compression::Lzw, None, false, &data);
    // sanity：未被截断的文件两者都应成功
    check_parity(&bytes, ColorType::Rgb8, (w, h), &data);
    // 截掉 strip 尾部 8 字节（同步改小 279，保持文件界内合法）→ LZW 流短、EOI 丢失
    let trunc = truncate_strip_data_tail(&bytes, 8);
    let err = expect_read_err(&trunc);
    assert!(
        !matches!(err, ImageError::Unsupported(_)),
        "LZW 截断应为解码错误而非 Unsupported: {err:?}"
    );
    assert!(oracle_fails_full(&trunc), "oracle 对 LZW 截断也应失败");
}

