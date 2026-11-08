//! ICO vs C# 参照字节恒等（ico_csharp_parity）：
//! 对 Benchmarks/corpus/ICO/*.ico + *.cur，若 target/csharp-ref/ico/<stem>.<w>x<h>.rgba 在（C# 对
//! Best 条目解码的 RGBA8）→ kimg_ico `IcoDecoder`（Best 条目）RGBA8 与之**逐字节**相等，且 kimg
//! 尺寸与 dump 文件名 <w>x<h> 一致（守卫「两侧同选 Best 条目」）；不等 fail（列文件）；缺 dump → SKIP。
//! 结束打印 parity ok=N skip=M。
//! 先跑 pwsh -File Benchmarks\ico\scripts\run_csharp_ref.ps1 生成语料 + dump（缺失则 SKIP 而非回归）。
//! 选帧口径（语料即统一 32bpp）：kimg best(位深→面积) 与 C# best(面积→位深) 全 32bpp 时同选一条。
//! 若 alpha/条目尺寸与 C# 有分歧（如 32bpp alpha 恰 0/255 也出问题）→ 停下精读两侧实现对齐，
//! 不与代码硬凑。

use std::io::Cursor;
use std::path::PathBuf;

use kimg_core::ImageDecoder as _;
use kimg_core::color::ColorType;
use kimg_ico::IcoDecoder;

/// 找 refout 下 `<stem>.*.rgba`，解析其 <w>x<h>。
fn find_dump(refout: &std::path::Path, stem: &str) -> Option<(PathBuf, u32, u32)> {
    let rd = std::fs::read_dir(refout).ok()?;
    let mut hits: Vec<PathBuf> = Vec::new();
    for e in rd.flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        if n.ends_with(".rgba") && n.starts_with(stem) {
            hits.push(e.path());
        }
    }
    // 取字典序最大（同 stem 多 size 时取最后；语料同 stem 只一个 size）。
    let p = hits.into_iter().max()?;
    let fname = p.file_name().unwrap().to_string_lossy().into_owned();
    // <stem>.<w>x<h>.rgba
    let body = fname
        .strip_prefix(stem)
        .unwrap_or(&fname)
        .trim_start_matches('.');
    let end = body.strip_suffix(".rgba")?;
    let mut it = end.split('x');
    let w: u32 = it.next()?.trim().parse().ok()?;
    let h: u32 = it.next()?.trim().parse().ok()?;
    Some((p, w, h))
}

#[test]
fn ico_csharp_parity_corpus() {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let root = std::path::Path::new(manifest).join("..").join("..");
    let corpus = root.join("Benchmarks").join("corpus").join("ICO");
    let refout = root.join("target").join("csharp-ref").join("ico");

    let mut paths: Vec<PathBuf> = match std::fs::read_dir(&corpus) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .filter(|e| {
                let ext = e.path().extension().map(|x| x.to_string_lossy().to_lowercase());
                ext.as_deref() == Some("ico") || ext.as_deref() == Some("cur")
            })
            .map(|e| e.path())
            .collect(),
        Err(_) => {
            eprintln!("SKIP: 缺少语料目录 {corpus:?}，跳过 ico_csharp_parity（先跑 run_csharp_ref.ps1）");
            return;
        }
    };
    paths.sort();
    if paths.is_empty() {
        eprintln!("SKIP: 语料目录 {corpus:?} 无 .ico/.cur 文件");
        return;
    }

    let mut ok = 0usize;
    let mut skip = 0usize;
    for p in &paths {
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        let stem = name.rsplit('.').nth(1).unwrap_or(&name);
        let bytes = std::fs::read(p).unwrap();

        // 定位 C# dump（refout/<stem>.<w>x<h>.rgba）；缺 → SKIP。
        let Some((dump_path, dump_w, dump_h)) = find_dump(&refout, stem) else {
            eprintln!("SKIP [{name}] 缺 C# dump（refout 无 {stem}.*.rgba）");
            skip += 1;
            continue;
        };

        let dec = match IcoDecoder::new(Cursor::new(&bytes)) {
            Ok(d) => d,
            Err(e) => panic!("[{name}] kimg 解码头失败: {e:?}"),
        };
        assert_eq!(
            dec.color_type(),
            ColorType::Rgba8,
            "[{name}] kimg 输出非 RGBA8"
        );
        let (w, h) = dec.dimensions();
        // 守卫左右同选一条：kimg 尺寸必须等于 C# dump 的尺寸。
        assert_eq!(
            (w, h),
            (dump_w, dump_h),
            "[{name}] 尺寸分歧：kimg Best({w}x{h}) ≠ C# Best({dump_w}x{dump_h})"
        );
        let mut buf = vec![0u8; dec.total_bytes() as usize];
        dec.read_image(&mut buf)
            .unwrap_or_else(|e| panic!("[{name}] kimg 解码失败: {e:?}"));

        let dump = std::fs::read(&dump_path).unwrap();
        if dump.len() != buf.len() {
            panic!(
                "[{name}] dump 长度分歧：kimg {}B vs C# {}B（{w}x{h}）",
                buf.len(),
                dump.len()
            );
        }
        let hit = buf.iter().zip(dump.iter()).position(|(a, b)| a != b);
        if let Some(p) = hit {
            panic!(
                "[{name}] RGBA 不一致 @字节 {p}（{w}x{h}）：kimg {:?} vs C# {:?}",
                &buf[p..(p + 8).min(buf.len())],
                &dump[p..(p + 8).min(dump.len())]
            );
        }
        eprintln!("ok [{name}] {w}x{h} 字节恒等（dump {}B）", dump.len());
        ok += 1;
    }
    eprintln!("parity ok={ok} skip={skip}");
    assert!(
        ok > 0 || skip > 0,
        "既无 ok 也无 skip（corpus/refout 状态异常？）"
    );
}
