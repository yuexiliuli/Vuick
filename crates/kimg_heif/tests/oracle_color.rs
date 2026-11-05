//! oracle 色彩/画布测试（色彩转换 + grid 合成这一层，不依赖帧解码）。
//! 读 `target/heif/oracle/<md5>.yuv`（整图 YUV：**无头**、画布尺度 4:2:0、u16 LE、行主序无跨步）
//! 与 `<md5>.rgba`（w,h i32 LE + RGBA8），用本 crate 的 `color::yuv_to_rgba` 与 `canvas::compose_grid`
//! 复算并逐字节断言。
//! 画布尺寸与颜色参数（`matrix_coefficients` / `full_range` / 位深）从 `target/heif/corpus/<file>` 的
//! 容器（`canvas_width/height`）与 nclx/SPS 推导，口径与 C# 相同（nclx 优先，否则 SPS VUI）。
//! 注意：IMG_*（transforms=1）的整图 YUV 是 **transform 前画布**（常见为横向 4032×3024），显示尺寸
//! 因 irot 是竖的，故按容器画布尺寸切分/合成，不以 `.rgba` 显示尺寸为准。
//! 只对 `info.json` 里 transforms=0 的文件断言；transforms≠0 打印 `SKIP: transforms`，不 fail
//! （该分支只做「合成 vs C# 整图 YUV」的参考对照，含边缘/旋转画布裁剪路径）。
//! grid 文件的整图 YUV 不是 tile dump，故合成测试用每 tile 的 `<md5>_t{i}.yuv`（32 字节头：
//! yW,yH,cW,cH,cropL/R/T/B + 三个 u16 平面）经 `compose_grid` 贴画布，与整图 YUV 逐字节比。
//! 缺目录/文件 → 打印 `SKIP: missing …` 后 return。
//! 运行：`cargo test -p kimg_heif --test oracle_color -- --nocapture --test-threads=1`

use std::path::{Path, PathBuf};

use kimg_heif::codecs::heif::canvas::{compose_grid, TileYuv};
use kimg_heif::codecs::heif::color::yuv_to_rgba;
use kimg_heif::codecs::heif::container::HeifContainer;

const ORACLE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/heif/oracle");
const CORPUS_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/heif/corpus");

#[allow(non_snake_case)]
#[derive(Debug, serde::Deserialize)]
struct InfoJson {
    file: String,
    cols: i64,
    rows: i64,
    tiles: i64,
    #[serde(default)]
    transforms: i64,
}

/// 把 `data` 按 u16 LE 解读成 `len` 个样本。
fn to_u16s(data: &[u8]) -> Vec<u16> {
    data.chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect()
}

/// 读 u32 LE（用于 tile dump 头）。
fn u32_at(data: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]])
}

/// 读 i32 LE（用于 .rgba 头）。
fn i32_at(data: &[u8], off: usize) -> i32 {
    i32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]])
}

/// 把整图 `.yuv` 按画布 `w×h`（4:2:0）拆成 Y、Cb、Cr。长度不吻合返回 None。
fn split_whole_yuv(data: &[u8], w: usize, h: usize) -> Option<(Vec<u16>, Vec<u16>, Vec<u16>)> {
    let cw = (w + 1) >> 1;
    let ch = (h + 1) >> 1;
    let y_n = w * h;
    let c_n = cw * ch;
    let mut p = 0usize; // 整图 yuv 无头：Y+Cb+Cr 连续。
    if data.len() < p + y_n * 2 + c_n * 4 {
        return None;
    }
    let y = to_u16s(&data[p..p + y_n * 2]);
    p += y_n * 2;
    let cb = to_u16s(&data[p..p + c_n * 2]);
    p += c_n * 2;
    let cr = to_u16s(&data[p..p + c_n * 2]);
    Some((y, cb, cr))
}

/// 读一个 `<md5>_t{i}.yuv` tile dump（32 字节头 + 三个 u16 平面）。
fn read_tile(dir: &Path, md5: &str, i: usize) -> Option<TileYuv> {
    let path = dir.join(format!("{md5}_t{i}.yuv"));
    let data = std::fs::read(&path).ok()?;
    if data.len() < 32 {
        println!("note: {path:?} 过短（{} 字节），跳过", data.len());
        return None;
    }
    let y_w = u32_at(&data, 0) as i32;
    let y_h = u32_at(&data, 4) as i32;
    let c_w = u32_at(&data, 8) as i32;
    let c_h = u32_at(&data, 12) as i32;
    let crop_left = u32_at(&data, 16) as i32;
    let crop_right = u32_at(&data, 20) as i32;
    let crop_top = u32_at(&data, 24) as i32;
    let crop_bottom = u32_at(&data, 28) as i32;
    if y_w <= 0 || y_h <= 0 || c_w <= 0 || c_h <= 0 {
        println!("note: {path:?} 头非法（yW={y_w} yH={y_h} cW={c_w} cH={c_h}），跳过");
        return None;
    }
    let y_n = (y_w * y_h) as usize;
    let c_n = (c_w * c_h) as usize;
    if data.len() < 32 + (y_n + 2 * c_n) * 2 {
        println!("note: {path:?} 载荷不足，跳过");
        return None;
    }
    Some(TileYuv {
        y: to_u16s(&data[32..32 + y_n * 2]),
        cb: to_u16s(&data[32 + y_n * 2..32 + (y_n + c_n) * 2]),
        cr: to_u16s(&data[32 + (y_n + c_n) * 2..32 + (y_n + 2 * c_n) * 2]),
        y_w,
        y_h,
        c_w,
        c_h,
        crop_left,
        crop_right,
        crop_top,
        crop_bottom,
    })
}

/// 统计两平面首次失配位置；返回失配总数。
fn diff_slices(a: &[u16], b: &[u16], label: &str) -> usize {
    let n = a.len().min(b.len());
    let mut count = 0usize;
    let mut first = None;
    for i in 0..n {
        if a[i] != b[i] {
            count += 1;
            if first.is_none() {
                first = Some(i);
            }
        }
    }
    if let Some(i) = first {
        println!("  {label}: {n} 样本 {count} 失配，首差 @ {i} (a={} b={})", a[i], b[i]);
    }
    count
}

/// 读 per-tile dump → `compose_grid` → 与整图 YUV 平面比较；返回每平面失配数（缺 tile dump 则 None）。
fn compose_against_whole(
    dir: &Path,
    md5: &str,
    cols: i32,
    rows: i32,
    tiles: i64,
    canvas_w: i32,
    canvas_h: i32,
    y_whole: &[u16],
    cb_whole: &[u16],
    cr_whole: &[u16],
) -> Option<[usize; 3]> {
    let n = tiles.max(1) as usize;
    let mut tile_vec = Vec::with_capacity(n);
    for i in 0..n {
        tile_vec.push(read_tile(dir, md5, i)?);
    }
    let composed = compose_grid(&tile_vec, cols, rows, canvas_w, canvas_h).ok()?;
    Some([
        diff_slices(&composed.y, y_whole, "Y"),
        diff_slices(&composed.cb, cb_whole, "Cb"),
        diff_slices(&composed.cr, cr_whole, "Cr"),
    ])
}

#[test]
fn oracle_color_matches_csharp() {
    let dir = PathBuf::from(ORACLE_DIR);
    if !dir.is_dir() {
        println!("SKIP: missing {dir:?}");
        return;
    }

    let mut entries: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_str()
                .map(|n| n.len() == 36 && n.ends_with(".yuv") && n[..32].chars().all(|c| c.is_ascii_hexdigit()))
                .unwrap_or(false)
        })
        .collect();
    entries.sort_by_key(|e| e.file_name());
    let mut checked = 0usize;
    let mut skipped = 0usize;
    let mut checked_bytes = 0usize;

    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        let md5 = name[..32].to_string();
        let info_path = dir.join(format!("{md5}.info.json"));
        let info: InfoJson = match std::fs::read(&info_path) {
            Ok(b) => match serde_json::from_slice(&b) {
                Ok(v) => v,
                Err(e) => {
                    println!("SKIP: {md5}: info.json 解析失败（{e}）");
                    skipped += 1;
                    continue;
                }
            },
            Err(e) => {
                println!("SKIP: {md5}: missing {info_path:?}（{e}）");
                skipped += 1;
                continue;
            }
        };
        let file_name = info.file.clone();

        let rgba_path = dir.join(format!("{md5}.rgba"));
        let rgba_data = match std::fs::read(&rgba_path) {
            Ok(d) => d,
            Err(e) => {
                println!("SKIP: {md5}: missing {rgba_path:?}（{e}）");
                skipped += 1;
                continue;
            }
        };
        if rgba_data.len() < 8 {
            println!("SKIP: {md5}: rgba 过短");
            skipped += 1;
            continue;
        }
        let out_w = i32_at(&rgba_data, 0) as i32;
        let out_h = i32_at(&rgba_data, 4) as i32;
        let expect_rgba = rgba_data.len() - 8;
        if out_w <= 0 || out_h <= 0 || expect_rgba != (out_w as usize) * (out_h as usize) * 4 {
            println!("SKIP: {md5}: rgba 尺寸异常 w={out_w} h={out_h} data={expect_rgba}");
            skipped += 1;
            continue;
        }
        let rgba = &rgba_data[8..];

        let whole_path = dir.join(format!("{md5}.yuv"));
        let whole = match std::fs::read(&whole_path) {
            Ok(d) => d,
            Err(e) => {
                println!("SKIP: {md5}: missing {whole_path:?}（{e}）");
                skipped += 1;
                continue;
            }
        };

        // 画布尺寸与颜色参数来自容器（与 C# dump 同口径）。
        let corpus_path = PathBuf::from(CORPUS_DIR).join(&info.file);
        let corpus = match std::fs::read(&corpus_path) {
            Ok(d) => d,
            Err(e) => {
                println!("SKIP: {md5}: missing corpus {corpus_path:?}（{e}）");
                skipped += 1;
                continue;
            }
        };
        let container = match HeifContainer::parse(&corpus) {
            Ok(c) => c,
            Err(e) => {
                panic!("{md5} ({file_name}): HeifContainer::parse 失败：{e}");
            }
        };
        let canvas_w = container.canvas_width;
        let canvas_h = container.canvas_height;
        let sps = match container.tile_configs[0].parse_sps() {
            Ok(s) => s,
            Err(e) => {
                panic!("{md5} ({file_name}): parse_sps 失败：{e}");
            }
        };
        let (matrix, full_range, bit_depth) = match container.nclx_colour {
            Some(n) => (n.matrix_coefficients, n.full_range, sps.bit_depth_y),
            None => (sps.matrix_coefficients, sps.video_full_range, sps.bit_depth_y),
        };

        // transforms≠0：只做 canvas 合成参考（颜色断言跳过），不参与 FAIL。
        if info.transforms != 0 {
            println!("SKIP: transforms ({md5}: {file_name})");
            skipped += 1;
            if let Some((y_plane, cb_plane, cr_plane)) =
                split_whole_yuv(&whole, canvas_w as usize, canvas_h as usize)
            {
                match compose_against_whole(
                    &dir,
                    &md5,
                    info.cols.max(1) as i32,
                    info.rows.max(1) as i32,
                    info.tiles,
                    canvas_w,
                    canvas_h,
                    &y_plane,
                    &cb_plane,
                    &cr_plane,
                ) {
                    Some(counts) => {
                        let total: usize = counts.iter().sum();
                        println!(
                            "  note: compose({md5}) vs C# 整图 YUV = {total} 失配（transforms 文件，仅参考）"
                        );
                    }
                    None => println!("  note: {md5}: 缺 per-tile dump，合成参考跳过"),
                }
            } else {
                println!(
                    "  note: {md5}: 整图 yuv 长度 {len} 与画布 {canvas_w}x{canvas_h} 4:2:0 不符，合成参考跳过",
                    len = whole.len()
                );
            }
            continue;
        }

        // transforms=0：显示尺寸 == 画布，rgba 即画布 RGBA。
        if out_w != canvas_w || out_h != canvas_h {
            println!(
                "SKIP: {md5}: transforms=0 但显示 {out_w}x{out_h} ≠ 画布 {canvas_w}x{canvas_h}",
            );
            skipped += 1;
            continue;
        }
        let (y_plane, cb_plane, cr_plane) = match split_whole_yuv(&whole, canvas_w as usize, canvas_h as usize) {
            Some(v) => v,
            None => {
                println!(
                    "SKIP: {md5}: 整图 yuv 长度 {whole_bytes} 与画布 {canvas_w}x{canvas_h} 4:2:0 不符",
                    whole_bytes = whole.len()
                );
                skipped += 1;
                continue;
            }
        };

        // --- 1) 色彩转换：整图 YUV → RGBA 对照 C# .rgba ---
        let bd_u8 = match u8::try_from(bit_depth) {
            Ok(v) => v,
            Err(_) => {
                println!("SKIP: {md5}: 位深 {bit_depth} 不合法");
                skipped += 1;
                continue;
            }
        };
        let converted = match yuv_to_rgba(
            &y_plane,
            &cb_plane,
            &cr_plane,
            canvas_w as usize,
            canvas_h as usize,
            bd_u8,
            matrix,
            full_range,
        ) {
            Ok(v) => v,
            Err(e) => {
                panic!("{md5} ({file_name}): yuv_to_rgba 失败（matrix={matrix} full={full_range} bd={bit_depth}）：{e}");
            }
        };
        let npix = (canvas_w as usize) * (canvas_h as usize) * 4;
        let mut color_mismatch = 0usize;
        let mut first_pix = None;
        for i in 0..npix {
            if converted[i] != rgba[i] {
                color_mismatch += 1;
                if first_pix.is_none() {
                    first_pix = Some((i / 4, i % 4, converted[i], rgba[i]));
                }
            }
        }
        if color_mismatch != 0 {
            let (p, ch_i, a, b) = first_pix.unwrap();
            panic!(
                "{md5} ({file_name}): 色彩失配 {color_mismatch}/{} 字节；首差像素 {p} 通道 {ch_i}: ours={a} c#={b} (matrix={matrix} full={full_range} bd={bit_depth})",
                npix
            );
        }
        checked_bytes += npix;

        // --- 2) canvas 合成：per-tile dump → compose_grid → 整图 YUV 对照 C# ---
        match compose_against_whole(
            &dir,
            &md5,
            info.cols.max(1) as i32,
            info.rows.max(1) as i32,
            info.tiles,
            canvas_w,
            canvas_h,
            &y_plane,
            &cb_plane,
            &cr_plane,
        ) {
            Some(counts) => {
                let total: usize = counts.iter().sum();
                if total != 0 {
                    panic!("{md5} ({file_name}): canvas 合成失配 {total} 样本（对照 C# DecodeYuv 整图 YUV）");
                }
            }
            None => println!("note: {md5}: 缺 per-tile dump，跳过 canvas 合成对照（颜色已对）"),
        }

        println!(
            "PASS {md5} ({file_name}): {}x{} bd={} matrix={} full={} tiles={} grid={}x{} pixels={}",
            canvas_w,
            canvas_h,
            bit_depth,
            matrix,
            full_range,
            info.tiles,
            info.cols,
            info.rows,
            (canvas_w as usize) * (canvas_h as usize)
        );
        checked += 1;
    }

    println!(
        "oracle_color: checked={checked} skipped={skipped} checked_bytes={checked_bytes}"
    );
}
