//! 并行 grid 多 tile 解码的「串行 ≡ 并行」parity 测试：
//! - `decode_yuv`（串行 tile 解码）与 `decode_yuv_par`（并行 tile 解码）对每个可解文件
//!   产出必须逐字节一致（Y/Cb/Cr 平面 + width/height/bit_depth）。
//! - 目标 0 失配：并行只改 tile 解码调度，字节恒等红线不回退。
//! manifest 列 `md5,file,w,h,bitdepth,tiles`；profile-4 的 ERR 行（w="ERR"）跳过不 fail。
//! 缺 oracle/corpus → 打印 `SKIP: missing …` 后 return（SKIP 是常态不是失败）。
//! 运行：`cargo test -p kimg_heif --test oracle_parallel -- --test-threads=1 --nocapture`

use std::path::PathBuf;

use kimg_heif::codecs::heif::{decode_yuv, decode_yuv_par, YuvPlanes};

const ORACLE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/heif/oracle");
const CORPUS_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../Benchmarks/corpus/heif");

/// 两个 YUV 结果是否逐平面一致；返回失配说明（空 = 完全一致）。
fn yuvs_identical(a: &YuvPlanes, b: &YuvPlanes) -> Result<(), String> {
    if a.width != b.width || a.height != b.height || a.bit_depth != b.bit_depth {
        return Err(format!(
            "尺寸/位深不符：串行 {}x{} bd={} vs 并行 {}x{} bd={}",
            a.width, a.height, a.bit_depth, b.width, b.height, b.bit_depth
        ));
    }
    for (name, pa, pb) in [("Y", &a.y, &b.y), ("Cb", &a.cb, &b.cb), ("Cr", &a.cr, &b.cr)] {
        if pa.len() != pb.len() {
            return Err(format!("{name} 平面长度不符：{} vs {}", pa.len(), pb.len()));
        }
        if let Some(i) = pa.iter().zip(pb.iter()).position(|(x, y)| x != y) {
            return Err(format!("{name} 首差 @ {i}：串行={} 并行={}", pa[i], pb[i]));
        }
    }
    Ok(())
}

#[test]
fn parallel_matches_serial() {
    let manifest_path = PathBuf::from(ORACLE_DIR).join("manifest.csv");
    let manifest = match std::fs::read_to_string(&manifest_path) {
        Ok(m) => m,
        Err(e) => {
            println!("SKIP: missing {manifest_path:?} ({e})");
            return;
        }
    };

    let mut checked = 0usize;
    let mut skipped = 0usize;
    let mut multi_tile = 0usize;
    let mut rejected = 0usize;
    let mut short = 0usize;
    let mut failures: Vec<String> = Vec::new();

    for (line_no, line) in manifest.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("md5,") {
            continue;
        }
        let cols: Vec<&str> = line.split(',').collect();
        if cols.len() < 2 {
            println!("note: manifest 第 {} 行字段过少，跳过", line_no + 1);
            short += 1;
            skipped += 1;
            continue;
        }
        let file = cols[1];
        // ERR 行（w="ERR"，profile-4 等被 C# 拒绝条目）：错误描述含逗号会拆字段（<6 列），
        // 先按第 3 列判 ERR、不依赖完整 6 列，计入 rejected。
        if cols.len() >= 3 && cols[2] == "ERR" {
            println!("note: {} 是拒绝条目（{file}，profile-4），跳过", line_no + 1);
            rejected += 1;
            skipped += 1;
            continue;
        }
        if cols.len() < 6 {
            println!("note: manifest 第 {} 行字段不足（<6 列），跳过", line_no + 1);
            short += 1;
            skipped += 1;
            continue;
        }
        let tiles: usize = cols[5].parse().unwrap_or(0);

        let corpus = PathBuf::from(CORPUS_DIR).join(file);
        let data = match std::fs::read(&corpus) {
            Ok(d) => d,
            Err(e) => {
                println!("SKIP: missing corpus {corpus:?} ({e})");
                skipped += 1;
                continue;
            }
        };

        let serial = match decode_yuv(&data) {
            Ok(v) => v,
            Err(e) => {
                failures.push(format!("{file}: 串行 decode_yuv 失败：{e}"));
                println!("FAIL {file}: 串行 decode_yuv 失败：{e}");
                continue;
            }
        };
        let par = match decode_yuv_par(&data) {
            Ok(v) => v,
            Err(e) => {
                failures.push(format!("{file}: 并行 decode_yuv_par 失败：{e}"));
                println!("FAIL {file}: 并行 decode_yuv_par 失败：{e}");
                continue;
            }
        };
        if let Err(msg) = yuvs_identical(&serial, &par) {
            failures.push(format!("{file}: {msg}"));
            println!("FAIL {file}: {msg}");
            continue;
        }
        checked += 1;
        if tiles >= 2 {
            multi_tile += 1;
        }
        println!(
            "PASS {file}: {}x{} bd={} tiles={} 串≡并 0 失配",
            serial.width, serial.height, serial.bit_depth, tiles,
        );
    }

    if !failures.is_empty() {
        panic!(
            "串≡并 parity 失配（{}）：\n{}",
            failures.len(),
            failures.join("\n")
        );
    }
    println!(
        "oracle_parallel: checked={checked} skipped={skipped} (rejected={rejected} short={short}) multi_tile={multi_tile}"
    );
    println!("  断言：并行 tile 解码与既有串行路径逐字节一致（0 失配）。");
}
