//! parity 对照测试：kimg 与 image 0.25（dev-dependency 参考）在 imgbench 语料上的字节级一致性。
//!
//! 语料目录：`target/imgbench-corpus`（缺则 SKIP）。对每个 png/jpg/jpeg/webp/gif/bmp/ico 文件断言
//! kimg 解码结果与 image 0.25 解码结果逐字节一致；GIF/WebP 动画取全部帧逐一比对。
//! 此测试是第 3 阶段整仓替换 image 的守护（替换完成后移除）。

use std::io::{Cursor, Read};
use std::path::Path;

use kimg::animation::AnimationDecoder;
use image::AnimationDecoder as _;

/// 语料根目录。
fn corpus_dir() -> Option<std::path::PathBuf> {
    let dir = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/imgbench-corpus"));
    if dir.is_dir() {
        Some(dir.to_path_buf())
    } else {
        None
    }
}

fn is_target(ext: &str) -> bool {
    matches!(
        ext,
        "png"
            | "jpg"
            | "jpeg"
            | "webp"
            | "gif"
            | "bmp"
            | "ico"
            | "pnm"
            | "pgm"
            | "ppm"
            | "pbm"
            | "pam"
            | "tif"
            | "tiff"
    )
}

/// 解码的三种结果：一致 / 不一致 / 跳过（含失败一致）。
#[derive(Default, Clone, Copy)]
struct Stats {
    ok: usize,
    mismatch: usize,
    skipped: usize,
}

fn ext_of(path: &Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// 比较两动态图像的 RGBA8 像素（尺寸 + 字节）。
fn compare_rgba8(
    name: &str,
    kimg_img: &kimg::DynamicImage,
    image_img: &image::DynamicImage,
    stats: &mut Stats,
) {
    let a = kimg_img.to_rgba8();
    let b = image_img.to_rgba8();
    if a.dimensions() != b.dimensions() {
        stats.mismatch += 1;
        println!("[不一致] {name}: 尺寸 {a:?} vs {b:?}", a = a.dimensions(), b = b.dimensions());
        return;
    }
    if a.as_raw() != b.as_raw() {
        stats.mismatch += 1;
        // 打印首个差异位置以便定位。
        let diff = a
            .as_raw()
            .iter()
            .zip(b.as_raw().iter())
            .position(|(x, y)| x != y);
        println!(
            "[不一致] {name}: 首差异 @ {diff:?} (kimg={:?} image={:?})",
            diff.map(|i| a.as_raw()[i]),
            diff.map(|i| b.as_raw()[i]),
        );
        return;
    }
    stats.ok += 1;
}

/// 比较 GIF/WebP 全部帧（缓冲字节 + 延迟）。
fn compare_frames(
    name: &str,
    kimg_decoder: std::io::Cursor<&[u8]>,
    image_decoder: std::io::Cursor<&[u8]>,
    stats: &mut Stats,
) {
    // 双侧各自先收集出错帧（错误类型分属两个 crate 的 ImageError，避免 ? 混用）。
    let kframes: Vec<_> = match kimg::codecs::gif::GifDecoder::new(kimg_decoder) {
        Ok(d) => d.into_frames().collect(),
        Err(e) => {
            println!("[不一致] {name}: imgif 解码失败({e:?})");
            stats.mismatch += 1;
            return;
        }
    };
    let iframes: Vec<_> = match image::codecs::gif::GifDecoder::new(image_decoder) {
        Ok(d) => d.into_frames().collect(),
        Err(e) => {
            println!("[不一致] {name}: image gif 解码失败({e:?})");
            stats.mismatch += 1;
            return;
        }
    };
    if kframes.len() != iframes.len() {
        println!("[不一致] {name}: gif 帧数量 {} vs {}", kframes.len(), iframes.len());
        stats.mismatch += 1;
        return;
    }
    for (k, i) in kframes.iter().zip(iframes.iter()) {
        let ok = match (k, i) {
            (Ok(kf), Ok(im)) => {
                let kb = kf.buffer().as_raw();
                let ib = im.buffer().as_raw();
                if kb != ib || kf.delay().numer_denom_ms() != im.delay().numer_denom_ms() {
                    let diff = kb.iter().zip(ib.iter()).position(|(x, y)| x != y);
                    println!(
                        "[不一致] {name} 帧: 内容/延迟不同 首差异@ {diff:?} ({:?} vs {:?})",
                        diff.map(|i| kb[i]),
                        diff.map(|i| ib[i]),
                    );
                    false
                } else {
                    true
                }
            }
            (Err(ke), _) => {
                println!("[不一致] {name}: kimg gif 帧失败({ke:?})");
                false
            }
            (_, Err(ie)) => {
                println!("[不一致] {name}: image gif 帧失败({ie:?})");
                false
            }
        };
        if !ok {
            stats.mismatch += 1;
            return;
        }
    }
    stats.ok += 1;
}

/// 递归收集语料中的图像文件。
fn collect_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let path = e.path();
        if path.is_dir() {
            collect_files(&path, out);
        } else {
            let ext = ext_of(&path);
            if is_target(&ext) {
                out.push(path);
            }
        }
    }
}

#[test]
fn parity_corpus() {
    let Some(dir) = corpus_dir() else {
        println!("SKIP: 缺少 target/imgbench-corpus（手动装配后重跑）");
        return;
    };

    let mut files: Vec<_> = Vec::new();
    collect_files(&dir, &mut files);
    files.sort();
    files.dedup();

    let mut stats = Stats::default();
    let mut gif_files = 0usize;
    let mut webp_files = 0usize;

    for path in &files {
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let ext = ext_of(path);
        let mut bytes = Vec::new();
        std::fs::File::open(path)
            .and_then(|mut f| f.read_to_end(&mut bytes))
            .unwrap();

        // 1) 静态解码对照（含 GIF 首帧 / WebP 封面）。
        let kimg_res = kimg::load_from_memory(&bytes);
        let image_res = image::load_from_memory(&bytes);

        match (kimg_res, image_res) {
            (Ok(ki), Ok(ii)) => compare_rgba8(&name, &ki, &ii, &mut stats),
            (Err(_), Err(_)) => {
                // 双双失败也算一致（行为一致：拒绝该文件）。
                stats.ok += 1;
            }
            (Err(e), Ok(_)) => {
                stats.mismatch += 1;
                println!("[不一致] {name}: kimg 解码失败({e:?})，image 成功");
            }
            (Ok(_), Err(e)) => {
                stats.mismatch += 1;
                println!("[不一致] {name}: image 解码失败({e:?})，kimg 成功");
            }
        }

        // 2) 动画面别帧全量对照。
        match ext.as_str() {
            "gif" => {
                gif_files += 1;
                let c = Cursor::new(&bytes[..]);
                compare_frames(&name, c, Cursor::new(&bytes[..]), &mut stats);
            }
            "webp" => {
                webp_files += 1;
                let kd = kimg::codecs::webp::WebPDecoder::new(Cursor::new(&bytes[..]));
                let id = image::codecs::webp::WebPDecoder::new(Cursor::new(&bytes[..]));
                match (kd, id) {
                    (Ok(kd), Ok(id)) => {
                        let kf_iter = kd.into_frames();
                        let if_iter = id.into_frames();
                        let kf: Vec<_> = kf_iter.collect();
                        let im: Vec<_> = if_iter.collect();
                        if kf.len() != im.len() {
                            println!("[不一致] {name}: webp 帧数量 {} vs {}", kf.len(), im.len());
                            stats.mismatch += 1;
                        } else {
                            let mut frame_ok = true;
                            for (k, i) in kf.iter().zip(im.iter()) {
                                match (k, i) {
                                    (Ok(kf), Ok(im)) => {
                                        if kf.buffer().as_raw() != im.buffer().as_raw()
                                            || kf.delay().numer_denom_ms()
                                                != im.delay().numer_denom_ms()
                                        {
                                            println!("[不一致] {name}: webp 帧内容/延迟不同");
                                            stats.mismatch += 1;
                                            frame_ok = false;
                                            break;
                                        }
                                    }
                                    (Err(ke), Ok(_)) => {
                                        println!("[不一致] {name}: kimg webp 帧失败({ke:?})");
                                        stats.mismatch += 1;
                                        frame_ok = false;
                                        break;
                                    }
                                    (Ok(_), Err(ie)) => {
                                        println!("[不一致] {name}: image webp 帧失败({ie:?})");
                                        stats.mismatch += 1;
                                        frame_ok = false;
                                        break;
                                    }
                                    _ => {}
                                }
                            }
                            if frame_ok {
                                stats.ok += 1;
                            }
                        }
                    }
                    // 双双失败视为行为一致（与静态对照的容错口径一致）。
                    (Err(_), Err(_)) => stats.ok += 1,
                    (Err(ke), Ok(_)) => {
                        println!("[不一致] {name}: kimg webp 解码失败({ke:?})，image 成功");
                        stats.mismatch += 1;
                    }
                    (Ok(_), Err(ie)) => {
                        println!("[不一致] {name}: image webp 解码失败({ie:?})，kimg 成功");
                        stats.mismatch += 1;
                    }
                }
            }
            _ => {}
        }

        // 统计跳过（非本次覆盖格式）。
        if matches!(
            ext.as_str(),
            "pnm" | "pgm" | "ppm" | "pbm" | "pam" | "tif" | "tiff"
        ) {
            stats.skipped += 1;
        }
    }

    println!(
        "parity 结果: 共 {} 文件 — 一致 {} / 不一致 {} / 跳过 {} (gif 动画 {} 个, webp {} 个)",
        stats.ok + stats.mismatch + stats.skipped,
        stats.ok,
        stats.mismatch,
        stats.skipped,
        gif_files,
        webp_files,
    );
    assert_eq!(
        stats.mismatch,
        0,
        "存在与 image 0.25 不一致的解码，需修复到 0"
    );
}

/// 格式嗅探与扩展名一致性抽查。
#[test]
fn format_guess_parity() {
    // 简单断言 kimg 与 image 的 ImageFormat 对应扩展名行为一致。
    for ext in ["png", "jpg", "jpeg", "webp", "gif", "bmp", "ico", "tiff", "ppm"] {
        let k = kimg::ImageFormat::from_extension(ext);
        let i = image::ImageFormat::from_extension(ext);
        assert_eq!(k.is_some(), i.is_some(), "扩展名 {ext} 识别不一致");
    }
}
