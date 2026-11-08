//! GIF vs C# 参照字节恒等（gif_csharp_parity）：
//! 对 Benchmarks/corpus/GIF/*.gif，若 target/csharp-ref/gif/<stem>.<w>x<h>.rgba 在（C# 全帧 RGBA8
//! 拼接，帧数 × w*h*4，静态=1 帧）→ kimg_gif 全帧 RGBA（静态 read_image / 动画 into_frames 逐帧拼接）
//! 与之**逐字节**相等；不等 fail（列文件）；缺 dump → SKIP。结束打印 parity ok=N skip=M。
//! 先跑 pwsh -File Benchmarks\gif\scripts\run_csharp_ref.ps1 生成语料 + dump（缺失则 SKIP 而非回归）。
//! 与 gif_oracle（gif crate 参照）互补：本测试直接对 C# 输出做字节恒等（kimg 为同源移植，
//! 预期一致；若出现 dispose/透明语义分歧 → 停下精读两侧实现对齐，不与代码硬凑）。

use std::io::Cursor;
use std::path::PathBuf;

use kimg_core::AnimationDecoder as _;
use kimg_core::ImageDecoder as _;
use kimg_gif::GifDecoder;

/// kimg 全帧 RGBA（Vec<帧>，每帧 w*h*4；静态先行 read_image 并断言与首帧一致）。
fn kimg_frames(bytes: &[u8], label: &str) -> Result<(u32, u32, Vec<Vec<u8>>), String> {
    let dec = GifDecoder::new(Cursor::new(bytes)).map_err(|e| format!("解码头失败: {e:?}"))?;
    let (w, h) = dec.dimensions();
    let mut frames = Vec::new();
    for fr in dec.into_frames() {
        let fr = fr.map_err(|e| format!("逐帧解码失败 @{} : {e:?}", frames.len()))?;
        frames.push(fr.buffer().as_raw().clone());
    }
    // 静态（1 帧）：另验 read_image 路径与合成首帧一致（覆盖另一条生产路径）。
    if frames.len() == 1 {
        let dec = GifDecoder::new(Cursor::new(bytes)).map_err(|e| format!("{e:?}"))?;
        let mut buf = vec![0u8; (w as usize) * (h as usize) * 4];
        dec.read_image(&mut buf)
            .map_err(|e| format!("read_image 失败: {e:?}"))?;
        assert_eq!(
            buf, frames[0],
            "[{label}] read_image 与 into_frames 首帧不一致"
        );
    }
    Ok((w, h, frames))
}

#[test]
fn gif_csharp_parity_corpus() {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let root = std::path::Path::new(manifest).join("..").join("..");
    let corpus = root.join("Benchmarks").join("corpus").join("GIF");
    let refout = root.join("target").join("csharp-ref").join("gif");

    let mut paths: Vec<PathBuf> = match std::fs::read_dir(&corpus) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().map(|x| x == "gif").unwrap_or(false))
            .map(|e| e.path())
            .collect(),
        Err(_) => {
            eprintln!("SKIP: 缺少语料目录 {corpus:?}，跳过 gif_csharp_parity（先跑 run_csharp_ref.ps1）");
            return;
        }
    };
    paths.sort();
    if paths.is_empty() {
        eprintln!("SKIP: 语料目录 {corpus:?} 无 .gif 文件");
        return;
    }

    let mut ok = 0usize;
    let mut skip = 0usize;
    for p in &paths {
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        let stem = name.rsplit('.').nth(1).unwrap_or(&name);
        let bytes = std::fs::read(p).unwrap();
        let (w, h, frames) = match kimg_frames(&bytes, &name) {
            Ok((w, h, frames)) => {
                // 校验每帧尺寸一致（防御性）。
                for (i, f) in frames.iter().enumerate() {
                    assert_eq!(
                        f.len(),
                        (w as usize) * (h as usize) * 4,
                        "[{name}] 第 {i} 帧尺寸 {w}x{h}x4 不匹配"
                    );
                }
                (w, h, frames)
            }
            Err(e) => panic!("[{name}] kimg 解码失败: {e}"),
        };

        // 定位 C# dump：refout/<stem>.*.rgba（ref 已按 stem 前缀命名）。
        let dump_path = {
            let mut hits: Vec<PathBuf> = Vec::new();
            if let Ok(rd) = std::fs::read_dir(&refout) {
                for e in rd.flatten() {
                    let n = e.file_name().to_string_lossy().into_owned();
                    if n.ends_with(".rgba") && n.starts_with(stem) {
                        hits.push(e.path());
                    }
                }
            }
            hits.into_iter().max()
        };
        let Some(dump_path) = dump_path else {
            eprintln!("SKIP [{name}] 缺 C# dump（refout 无 {stem}.*.rgba）");
            skip += 1;
            continue;
        };
        let dump = std::fs::read(&dump_path).unwrap();
        let frame_len = (w as usize) * (h as usize) * 4;
        let kimg_concat: Vec<u8> = frames.concat();
        if dump.len() != kimg_concat.len() {
            panic!(
                "[{name}] 帧数不匹配：kimg {} 帧 {}B vs C# dump {}B（dump 折合 {} 帧）",
                frames.len(),
                kimg_concat.len(),
                dump.len(),
                dump.len() / frame_len
            );
        }
        let hit = kimg_concat.iter().zip(dump.iter()).position(|(a, b)| a != b);
        if let Some(p) = hit {
            let frame = p / frame_len;
            let off = p % frame_len;
            panic!(
                "[{name}] RGBA 不一致 @字节 {p}（第 {frame} 帧 @{off}）：kimg {:?} vs C# {:?}",
                &kimg_concat[p..(p + 8).min(kimg_concat.len())],
                &dump[p..(p + 8).min(dump.len())]
            );
        }
        eprintln!("ok [{name}] {w}x{h} {} 帧 字节恒等（dump {}B）", frames.len(), dump.len());
        ok += 1;
    }
    eprintln!("parity ok={ok} skip={skip}");
    assert!(
        ok > 0 || skip > 0,
        "既无 ok 也无 skip（corpus/refout 状态异常？）"
    );
}
