//! progressive finish 逐块 `prog_mask` popcount 分布测量（`KIMG_JPEG_PROG_MASK_PROFILE=1`，
//! `#[ignore]`）。供候选 C（prog_mask 稀疏 IDCT 分层）可行性取证。
//!
//! 探针本体在 `mcu.rs finish_progressive_decoding`（env 门控，缺省关=零注入，不改算法路径）：
//! 每文件解码结束打印一行
//! `POPMASK n=<块数> dc_only=<..%> ac_le4=<..%> ac_516=<..%> ac_gt16=<..%> finish_probe_ms=<..>`。
//! dc_only = 非零 AC 为 0（mask==0）；ac_le4 = 非零 AC ≤4；ac_516 = 5-16；ac_gt16 = >16。
//!
//! 本测试是测量驱动：只在每文件解码**前**打印一行 `POPMASK FILE <name> dims=..`（文件名关联
//! 口径，与 STAGE/SCANP 同款，探针本体不持文件名、零生产语义影响），随后解 1 轮。
//!
//! 用法（长命令前台阻塞到结束，K86）：
//! ```text
//! $env:KIMG_JPEG_CORPUS='E:\Rust\gpui-editor\Benchmarks\corpus\JPEG'
//! $env:KIMG_JPEG_PROG_MASK_PROFILE='1'
//! cargo test -p kimg_jpeg --release --test prog_maskp -- --ignored --nocapture | Tee-Object -FilePath Benchmarks\jpeg\logs\prog_maskp_<ts>.log
//! ```

mod fixtures;

use std::io::Cursor;
use std::path::PathBuf;

use kimg_core::ImageDecoder as _;
use kimg_jpeg::codecs::jpeg::JpegDecoder as NewJpeg;

use fixtures::*;

#[test]
#[ignore]
fn maskp() {
    if std::env::var("KIMG_JPEG_PROG_MASK_PROFILE").as_deref() != Ok("1") {
        println!("POPMASK note: KIMG_JPEG_PROG_MASK_PROFILE=1 未设置，探针不打印 POPMASK 行（本测试仅驱动文件标记）");
    }
    let dir = corpus_dir();
    if !dir.is_dir() {
        println!("POPMASK SKIP: 语料目录不存在/不可读: {}", dir.display());
        return;
    }
    let files = collect_jpegs(&dir);
    let prog: Vec<PathBuf> = files
        .into_iter()
        .filter(|p| {
            p.file_name()
                .map(|n| n.to_string_lossy().contains("-prog"))
                .unwrap_or(false)
        })
        .collect();
    if prog.is_empty() {
        println!("POPMASK SKIP: 语料无 progressive 文件: {}", dir.display());
        return;
    }
    println!("POPMASK header corpus={} prog_files={}", dir.display(), prog.len());

    for p in &prog {
        let name = p
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let bytes = match std::fs::read(p) {
            Ok(b) => b,
            Err(e) => {
                println!("POPMASK WARN {name}: 读文件失败 {e}");
                continue;
            }
        };
        let Some(dec) = NewJpeg::new(Cursor::new(&bytes)).ok() else {
            println!("POPMASK WARN {name}: 解码头失败，跳过");
            continue;
        };
        let (w, h) = dec.dimensions();
        let n = dec.total_bytes() as usize;
        println!("POPMASK FILE {name} dims={w}x{h}");
        let mut buf = vec![0u8; n];
        let _ = NewJpeg::new(Cursor::new(&bytes))
            .expect("解码头已保证")
            .read_image(&mut buf);
    }
    println!("POPMASK done.");
}
