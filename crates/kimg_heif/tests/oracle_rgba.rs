//! HEIF 全链（解码 → grid 合成 → 色彩转换 → transform）oracle 测试：
//! - `decode_rgba` 全链 RGBA8：对 manifest 全部可解文件（含 clap/irot/imir transform）逐字节对比
//!   `target/heif/oracle/<md5>.rgba`（w,h i32LE + RGBA8 行主序无跨步，**显示朝向**含变换后尺寸）。
//!   目标全部 mismatches=0（14 张手机 IMG_* + mangrove 的 transform=1 亦应解锁）。
//! - `decode_yuv` 整图 YUV：对 manifest 全部可解文件（transforms=0/1 皆可）逐字节对比
//!   `target/heif/oracle/<md5>.yuv`（**无头**：Y+Cb+Cr，各 u16LE，画布尺度 4:2:0；IMG_* 竖图
//!   的整图 YUV 是 transform 前横向画布——`decode_yuv` 正好返回该画布）。
//! 缺语料/dump → 打印 `SKIP: missing …` 后 return（SKIP 是常态不是失败）；失配 → 打印首差并 panic。
//! 运行：`cargo test -p kimg_heif --test oracle_rgba -- --test-threads=1 --nocapture`

use std::path::PathBuf;

use kimg_heif::{decode_rgba, decode_yuv};

const ORACLE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/heif/oracle");
const CORPUS_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../Benchmarks/corpus/heif");

fn i32_at(data: &[u8], off: usize) -> i32 {
    i32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]])
}

/// 逐字节统计两缓冲区失配；返回 (失配数, 首差字节下标)。
fn count_mismatches(a: &[u8], b: &[u8]) -> (usize, Option<usize>) {
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
    (count, first)
}

/// 读 manifest（md5,file,…）里非 ERR 的可解文件（有 `.rgba` oracle 者），按 oracle 尺寸升序返回
/// （小文件先跑，mango/fuji → IMG_* → 大 grid）。
fn rgba_manifest_rows() -> Vec<(String, String)> {
    let manifest_path = PathBuf::from(ORACLE_DIR).join("manifest.csv");
    let manifest = match std::fs::read_to_string(&manifest_path) {
        Ok(m) => m,
        Err(e) => {
            println!("SKIP: missing {manifest_path:?} ({e})");
            return Vec::new();
        }
    };
    let mut rows: Vec<(String, String, u64)> = Vec::new();
    for line in manifest.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("md5,") {
            continue;
        }
        let cols: Vec<&str> = line.split(',').collect();
        if cols.len() < 2 || cols[2] == "ERR" {
            continue;
        }
        let md5 = cols[0].to_string();
        let file = cols[1].to_string();
        let rgba_path = PathBuf::from(ORACLE_DIR).join(format!("{md5}.rgba"));
        let size = std::fs::metadata(&rgba_path).map(|m| m.len()).unwrap_or(u64::MAX);
        rows.push((md5, file, size));
    }
    rows.sort_by_key(|r| r.2);
    rows.into_iter().map(|(m, f, _)| (m, f)).collect()
}

/// 打印容器 transform 摘要（用于报告该文件哪类变换；parse 不解码 HEVC payload，开销小）。
fn print_transforms(file: &str, data: &[u8]) {
    let Ok(c) = kimg_heif::codecs::heif::container::HeifContainer::parse(data) else {
        println!("  {file}: 容器解析失败（不解码）");
        return;
    };
    if c.transforms.is_empty() {
        println!("  {file}: transforms=0（无变换）");
        return;
    }
    let names: Vec<String> = c
        .transforms
        .iter()
        .map(|t| match t.ty {
            kimg_heif::codecs::heif::container::PropClap => format!("clap(pr={:?})", t.clap),
            kimg_heif::codecs::heif::container::PropIrot => format!("irot(a={})", t.a),
            _ => format!("imir(a={})", t.a),
        })
        .collect();
    println!("  {file}: transforms={} {}", c.transforms.len(), names.join(" → "));
}

/// `decode_rgba` 全链 RGBA：manifest 全部可解文件逐字节对比 C# `<md5>.rgba`。
#[test]
fn rgba_full_chain_matches_csharp() {
    let dir = PathBuf::from(ORACLE_DIR);
    if !dir.is_dir() {
        println!("SKIP: missing {dir:?}");
        return;
    }
    let rows = rgba_manifest_rows();
    if rows.is_empty() {
        println!("SKIP: missing {}/manifest.csv 或全部无可解文件", dir.display());
        return;
    }
    let mut checked = 0usize;
    let mut skipped = 0usize;
    let mut failures: Vec<String> = Vec::new();

    for (md5, file) in rows {
        let corpus = PathBuf::from(CORPUS_DIR).join(&file);
        let rgba_path = dir.join(format!("{md5}.rgba"));
        if !corpus.exists() {
            println!("SKIP: missing corpus {corpus:?}");
            skipped += 1;
            continue;
        }
        if !rgba_path.exists() {
            println!("SKIP: missing oracle {rgba_path:?}");
            skipped += 1;
            continue;
        }
        let data = std::fs::read(&corpus).expect("read corpus");
        print_transforms(&file, &data);
        let want = std::fs::read(&rgba_path).expect("read rgba oracle");
        if want.len() < 8 {
            println!("SKIP: {file}: rgba 过短");
            skipped += 1;
            continue;
        }
        let w = i32_at(&want, 0);
        let h = i32_at(&want, 4);
        if w <= 0 || h <= 0 || want.len() != 8 + (w as usize) * (h as usize) * 4 {
            println!(
                "SKIP: {file}: rgba 尺寸异常 w={w} h={h} len={}",
                want.len()
            );
            skipped += 1;
            continue;
        }
        let expect = (w as usize) * (h as usize) * 4;
        let mut out = vec![0u8; expect];
        let (dw, dh) = match decode_rgba(&data, &mut out) {
            Ok(v) => v,
            Err(e) => {
                failures.push(format!("{file}: decode_rgba 返回错误：{e}"));
                println!("FAIL {file}: decode_rgba 返回错误：{e}");
                continue;
            }
        };
        if (dw, dh) != (w, h) {
            failures.push(format!(
                "{file}: 尺寸不符 ours=({dw}x{dh}) c#=({w}x{h})"
            ));
            println!("FAIL {file}: 尺寸不符 ours=({dw}x{dh}) c#=({w}x{h})");
            continue;
        }
        let (mismatches, first) = count_mismatches(&out, &want[8..]);
        if mismatches == 0 {
            checked += 1;
            println!("PASS {file}: {w}x{h} RGBA mismatches=0");
        } else {
            let fi = first.unwrap_or(0);
            let (fx, fy) = (fi % (w as usize * 4) / 4, fi / (w as usize * 4));
            failures.push(format!(
                "{file}: {mismatches}/{expect} 字节失配；首差 @ {fi} (x={fx}, y={fy}) ours={} c#={}",
                out[fi],
                want[8 + fi]
            ));
            println!(
                "FAIL {file}: {mismatches}/{expect} 字节失配；首差 @ {fi} (x={fx}, y={fy}) ours={} c#={}",
                out[fi],
                want[8 + fi]
            );
        }
    }

    if !failures.is_empty() {
        panic!("RGBA 全链失配（{}）：\n{}", failures.len(), failures.join("\n"));
    }
    println!("oracle_rgba rgba: checked={checked} skipped={skipped}");
}

/// 逐样本统计两 u16 平面失配；返回 (失配数, 首差下标)。
fn count_u16_mismatches(a: &[u16], b: &[u16]) -> (usize, Option<usize>) {
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
    (count, first)
}

/// 把整图 `.yuv` 的 `[off..off+n*2)` 按 u16LE 解读。
fn to_u16s(data: &[u8], off: usize, n: usize) -> Vec<u16> {
    data[off..off + n * 2]
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect()
}

/// `decode_yuv` 整图 YUV（含 grid 合成）：对 manifest 全部可解文件逐字节对比 C# `<md5>.yuv`。
#[test]
fn whole_yuv_full_chain_matches_csharp() {
    let dir = PathBuf::from(ORACLE_DIR);
    if !dir.is_dir() {
        println!("SKIP: missing {dir:?}");
        return;
    }
    let manifest_path = dir.join("manifest.csv");
    let manifest = match std::fs::read_to_string(&manifest_path) {
        Ok(m) => m,
        Err(e) => {
            println!("SKIP: missing {manifest_path:?} ({e})");
            return;
        }
    };

    let mut checked = 0usize;
    let mut skipped = 0usize;
    let mut failures: Vec<String> = Vec::new();

    for (_line_no, line) in manifest.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("md5,") {
            continue;
        }
        let cols: Vec<&str> = line.split(',').collect();
        if cols.len() < 2 || cols[2] == "ERR" {
            continue;
        }
        let md5 = cols[0];
        let file = cols[1];

        let corpus = PathBuf::from(CORPUS_DIR).join(file);
        let whole_path = dir.join(format!("{md5}.yuv"));
        if !corpus.exists() {
            println!("SKIP: missing corpus {corpus:?}");
            skipped += 1;
            continue;
        }
        if !whole_path.exists() {
            println!("SKIP: missing oracle {whole_path:?}");
            skipped += 1;
            continue;
        }
        let data = std::fs::read(&corpus).expect("read corpus");
        let whole = std::fs::read(&whole_path).expect("read whole yuv oracle");

        let yuv = match decode_yuv(&data) {
            Ok(v) => v,
            Err(e) => {
                failures.push(format!("{file}: decode_yuv 返回错误：{e}"));
                println!("FAIL {file}: decode_yuv 返回错误：{e}");
                continue;
            }
        };
        let w = yuv.width as usize;
        let h = yuv.height as usize;
        let cw = (w + 1) >> 1;
        let ch = (h + 1) >> 1;
        let y_n = w * h;
        let c_n = cw * ch;
        if whole.len() != (y_n + 2 * c_n) * 2 {
            failures.push(format!(
                "{file}: 整图 yuv 长度 {} 与画布 {w}x{h} 4:2:0 不符（期望 {}）",
                whole.len(),
                (y_n + 2 * c_n) * 2
            ));
            println!(
                "FAIL {file}: 整图 yuv 长度 {} 与画布 {w}x{h} 4:2:0 不符",
                whole.len()
            );
            continue;
        }
        let mut total = 0usize;
        let mut any_first = None;
        for (name, plane, off) in [
            ("Y", &yuv.y, 0usize),
            ("Cb", &yuv.cb, y_n),
            ("Cr", &yuv.cr, y_n + c_n),
        ] {
            let oracle = to_u16s(&whole, off * 2, plane.len());
            let (count, first) = count_u16_mismatches(plane, &oracle);
            if count != 0 {
                let cw_plane = if name == "Y" { w } else { cw };
                println!(
                    "  {name}: {count}/{} 失配；首差样本 {:?} (x={:?}, y={:?}) ours={:?} c#={:?}",
                    plane.len(),
                    first,
                    first.map(|f| f % cw_plane),
                    first.map(|f| f / cw_plane),
                    first.map(|f| plane[f]),
                    first.map(|f| oracle[f]),
                );
                total += count;
                if any_first.is_none() {
                    any_first = first;
                }
            }
        }
        if total != 0 {
            failures.push(format!(
                "{file}: 整图 YUV {total} 样本失配（首差 {any_first:?}）"
            ));
            println!("FAIL {file}: 整图 YUV {total} 样本失配");
            continue;
        }
        checked += 1;
        println!(
            "PASS {file}: {w}x{h} bd={} YUV mismatches=0",
            yuv.bit_depth
        );
    }

    if !failures.is_empty() {
        panic!(
            "整图 YUV 全链失配（{}）：\n{}",
            failures.len(),
            failures.join("\n")
        );
    }
    println!("oracle_rgba whole_yuv: checked={checked} skipped={skipped}");
}
