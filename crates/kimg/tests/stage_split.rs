//! 单文件「大图各环节分账」探针：全解(load_from_memory) / 转 RGBA8 / 盒式降采样(den 2/4/8)。
//! 复用 viewer 展示链路各阶段语义做墙钟，报告「解码熵 vs 转换 vs 降采样」三块与
//! 全解/展示档 RGBA 字节比、选档预测（pick den for need）。
//! 用法：`IMG_STAGE_FILE=<文件> cargo test -p kimg --release --test stage_split -- --ignored --nocapture`
//! debug 同款去 `--release`。`--ignored --nocapture` 运行。

use std::path::{Path, PathBuf};
use std::time::Instant;

fn stage_file() -> PathBuf {
    std::env::var("IMG_STAGE_FILE")
        .map(PathBuf::from)
        .expect("IMG_STAGE_FILE 未设置")
}

/// 选档复刻 viewer::pick_jpeg_denom（格式无关）：解码像素 ≥ need 的最小分母。
fn pick_denom(iw: u32, ih: u32, need: (f32, f32)) -> u8 {
    let (nw, nh) = (need.0.max(1.0), need.1.max(1.0));
    for den in [8u8, 4, 2, 1] {
        let w = (iw as f32 / den as f32).ceil();
        let h = (ih as f32 / den as f32).ceil();
        if w >= nw && h >= nh {
            return den;
        }
    }
    1
}

/// 4 通道盒式降采样（复刻 viewer::downsample_row 语义，仅计时用）。
fn box_4(buf: &[u8], iw: u32, ih: u32, den: u32) -> Vec<u8> {
    let ow = iw.div_ceil(den);
    let oh = ih.div_ceil(den);
    let mut out = vec![0u8; (ow as usize) * (oh as usize) * 4];
    for oy in 0..oh {
        let y0 = oy * den;
        let y1 = (y0 + den).min(ih);
        for ox in 0..ow {
            let x0 = ox * den;
            let x1 = (x0 + den).min(iw);
            let mut acc = [0u64; 4];
            let mut n = 0u64;
            for y in y0..y1 {
                for x in x0..x1 {
                    let i = ((y * iw + x) as usize) * 4;
                    acc[0] += buf[i] as u64;
                    acc[1] += buf[i + 1] as u64;
                    acc[2] += buf[i + 2] as u64;
                    acc[3] += buf[i + 3] as u64;
                    n += 1;
                }
            }
            let di = ((oy * ow + ox) as usize) * 4;
            for c in 0..4 {
                out[di + c] = (acc[c] / n) as u8;
            }
        }
    }
    out
}

fn median_5<F: FnMut()>(mut f: F) -> f64 {
    let mut v = Vec::with_capacity(5);
    for _ in 0..5 {
        let t = Instant::now();
        f();
        v.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[2]
}

#[test]
#[ignore]
fn stage_split_big_image() {
    let p = stage_file();
    let bytes = std::fs::read(&p).expect("读取文件失败");
    let path_str = p.display().to_string();
    eprintln!(
        "\n[STAGE] file={} size={:.2}MB",
        path_str,
        bytes.len() as f64 / (1024.0 * 1024.0)
    );

    let img0 = kimg::load_from_memory(&bytes).expect("全解失败");
    let (iw, ih) = img0.dimensions();
    let full_px = iw as u64 * ih as u64;
    eprintln!(
        "[STAGE] dims {}x{} MP={:.2} fullRGBA={:.1}MB",
        iw,
        ih,
        full_px as f64 / 1e6,
        full_px as f64 * 4.0 / (1024.0 * 1024.0)
    );

    // 选档预测（viewer 两处 need）
    let den_bench = pick_denom(iw, ih, (1376.0, 1032.0));
    let den_init = pick_denom(iw, ih, (1920.0, 1080.0));
    eprintln!(
        "[STAGE] pick_denom(need 1376x1032)={den_bench} | (1920x1080)={den_init} | 展示像素比 den/4={:.2}", 
        (full_px as f64) / ((iw as f64 / 4.0).ceil() * (ih as f64 / 4.0).ceil())
    );

    // 全解（load）与 全解+to_rgba8
    let t_load = median_5(|| {
        let img = kimg::load_from_memory(&bytes).expect("全解失败");
        std::hint::black_box(img);
    });
    let t_full = median_5(|| {
        let img = kimg::load_from_memory(&bytes).expect("全解失败");
        let rgba = img.to_rgba8();
        std::hint::black_box(rgba.into_raw());
    });

    // 转 RGBA8（单独：给定已 load 的图）
    let rgba0 = img0.to_rgba8();
    let raw0 = rgba0.as_raw();
    let t_rgba = median_5(|| {
        let rgba = img0.to_rgba8();
        std::hint::black_box(rgba.into_raw());
    });

    // 降采样各档
    for den in [2u8, 4, 8] {
        let n = iw.div_ceil(u32::from(den)) as u64 * ih.div_ceil(u32::from(den)) as u64;
        let t_box = median_5(|| {
            let out = box_4(raw0, iw, ih, u32::from(den));
            std::hint::black_box(out);
        });
        eprintln!(
            "[STAGE]   box den={den}: {:>9.2}ms | dispRGBA={:>8.1}MB (全解 {:.1}MB, {:.2}x 压缩)",
            t_box,
            n as f64 * 4.0 / (1024.0 * 1024.0),
            full_px as f64 * 4.0 / (1024.0 * 1024.0),
            full_px as f64 / n as f64
        );
    }
    eprintln!(
        "[STAGE] load={:.2}ms to_rgba8={:.2}ms full(load+to_rgba8)={:.2}ms | 转换占比 {:.0}%",
        t_load,
        t_rgba,
        t_full,
        t_rgba / t_full * 100.0
    );
}
