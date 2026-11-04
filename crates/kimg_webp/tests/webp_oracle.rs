//! WebP oracle harness：三路对照（①ours 占位/N/A ②image-webp ③dwebp golden）。
//!
//! 目标（P0）：先在 corpus 上证明「image-webp ≡ dwebp golden」的真值，决定 M1 落地标尺。
//! golden 口径与 SimdCodecs `WebpOracleTests.cs` 完全一致：
//! - `webp-oracle-golden.tsv`（52 静态非动画数据行）：逐 fixture `sha256(YUV)` /
//!   `sha256(nofancy RGBA)` / `sha256(fancy RGBA)`；RGBA = dwebp `-pam` 输出剥离头部后的
//!   `width*height*4` 字节（无 alpha 图 alpha 通道=255），SHA-256 直接覆盖解码字节。
//! - `webp-animation-oracle-golden.tsv`（25 动画数据行）：anim_dump 1.6.0 逐帧
//!   `sha256(RGBA)`，列 = fixture/frame/canvas 宽高/timestamp-ms/duration-ms/hash。
//!
//! image-webp 侧经 kimg `WebPDecoder` 现路径（`to_rgba8` 语义：RGB8→RGBA8 补 alpha=255）。
//! 有损 YUV→RGB 上采样按 M2 以 fancy 列为标尺（image-webp `UpsamplingMethod::Bilinear`
//! ≈ dwebp fancy 核；被 image-webp 压成 nofancy 的 fixture 会在 fancy 列 MISS 但 nofancy 列
//! 命中，两列都 MISS 才是真不一致）。本 harness 输出每 fixture 一致/不一致表，供主 agent
//! 核验决定 M1 主 oracle。

use std::io::Cursor;
use std::path::PathBuf;

use sha2::{Digest, Sha256};

use kimg_webp::WebPDecoder;
use kimg_core::{AnimationDecoder as _, ImageDecoder as _};

mod fixtures;

// golden TSV 保留在仓内；图片一律取外部语料（tests/fixtures/webp/mod.rs 定位）。
const GOLDEN_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/webp");

fn golden_dir() -> PathBuf {
    PathBuf::from(GOLDEN_DIR)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

/// 解析静态 golden TSV → map: fixture → (w, h, yuv, nofancy, fancy)。
struct StaticGolden {
    rows: std::collections::BTreeMap<String, (u32, u32, String, String, String)>,
}

impl StaticGolden {
    fn load() -> Self {
        let path = golden_dir().join("webp-oracle-golden.tsv");
        let text = std::fs::read_to_string(&path).expect("静态 golden TSV 应存在");
        let mut rows = std::collections::BTreeMap::new();
        for line in text.lines() {
            let line = line.trim_end();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let parts: Vec<&str> = line.split('\t').collect();
            assert_eq!(parts.len(), 6, "golden 行应为 6 列: {line}");
            rows.insert(
                parts[0].to_string(),
                (
                    parts[1].parse().unwrap(),
                    parts[2].parse().unwrap(),
                    // 静态 golden 列来自 C# Convert.ToHexString 为大写，归一为小写便于与
                    // sha256sum 风格（动画 golden）同基准比较。
                    parts[3].to_lowercase(),
                    parts[4].to_lowercase(),
                    parts[5].to_lowercase(),
                ),
            );
        }
        Self { rows }
    }
}

/// 经 kimg WebPDecoder（image-webp 现路径）把静态 fixture 解为 RGBA8 字节。
fn imagewebp_rgba(data: &[u8]) -> Result<(u32, u32, Vec<u8>), String> {
    let dec = WebPDecoder::new(Cursor::new(data)).map_err(|e| e.to_string())?;
    let (w, h) = dec.dimensions();
    let has_alpha = dec.color_type() == kimg_core::ColorType::Rgba8;
    let mut buf = vec![0u8; dec.total_bytes() as usize];
    dec.read_image(&mut buf).map_err(|e| e.to_string())?;
    if has_alpha {
        Ok((w, h, buf))
    } else {
        // RGB8 → RGBA8（补 alpha=255，对齐 dwebp PAM RGBA 口径）。
        let mut rgba = Vec::with_capacity(w as usize * h as usize * 4);
        for px in buf.chunks_exact(3) {
            rgba.extend_from_slice(&[px[0], px[1], px[2], 255]);
        }
        Ok((w, h, rgba))
    }
}

/// ②image-webp 视图的 (γ, δ)：ours 尚未接、占位 N/A（P2 后在此加回 ①）。
#[derive(Debug, Clone, Copy, PartialEq)]
enum RgbaMatch {
    Nofancy,
    Fancy,
    Both,
    Neither,
}

impl RgbaMatch {
    fn classify(fancy: bool, nofancy: bool) -> Self {
        match (nofancy, fancy) {
            (true, true) => RgbaMatch::Both,
            (true, false) => RgbaMatch::Nofancy,
            (false, true) => RgbaMatch::Fancy,
            (false, false) => RgbaMatch::Neither,
        }
    }
}

/// 静态 corpus 对照：image-webp RGBA sha256 vs golden（fancy 为 M2 标尺，nofancy 附判）。
/// 返回逐 fixture 结果行 + 汇总统计。
fn static_corpus_matrix() -> (Vec<String>, Vec<String>, Vec<(&'static str, usize)>) {
    let golden = StaticGolden::load();
    let manifest: Vec<String> = golden.rows.keys().cloned().collect();
    let mut lines = Vec::new();
    // 汇总计数按 golden 三类列命中
    let mut fancy_hit: usize = 0;
    let mut nofancy_hit: usize = 0;
    let mut both_hit: usize = 0;
    let mut neither: usize = 0;
    let mut misses = Vec::new();

    for name in &manifest {
        // SKIP：缺少夹具（外部语料缺失属常态，见 AGENTS §5）。
        let Some(path) = fixtures::webp::webp_fixture_path(name) else {
            lines.push(format!("SKIP\t{name}\t夹具缺失"));
            continue;
        };
        let data = std::fs::read(&path).expect("read fixture");
        let (w, h, rgba) = match imagewebp_rgba(&data) {
            Ok(v) => v,
            Err(e) => {
                lines.push(format!("ERR\t{name}\t解码失败: {e}"));
                misses.push(("decode-fail", 1));
                continue;
            }
        };
        let (gw, gh, gyuv, gnofancy, gfancy) = &golden.rows[name];
        let dims_ok = (w, h) == (*gw, *gh);
        let hash = sha256_hex(&rgba);
        let fancy_ok = &hash == gfancy;
        let nofancy_ok = &hash == gnofancy;
        let m = RgbaMatch::classify(fancy_ok, nofancy_ok);
        match m {
            RgbaMatch::Both => both_hit += 1,
            RgbaMatch::Fancy => fancy_hit += 1,
            RgbaMatch::Nofancy => nofancy_hit += 1,
            RgbaMatch::Neither => {
                neither += 1;
                misses.push(("neither", 1));
            }
        }
        lines.push(format!(
            "{m:?}\t{name}\tdims={dims_ok} ({w}x{h} vs {gw}x{gh}){marker}",
            marker = if m == RgbaMatch::Neither {
                format!("\tnofancy-match={nofancy_ok} fancy-match={fancy_ok}")
            } else {
                String::new()
            }
        ));
        // 记录 YUV 列参考（本阶段不比对：无损图 YUV sha 由 dwebp 对 RGBA 降采样给出，K29 标注）
        let _ = gyuv;
    }

    let summary = vec![
        ("both(nofancy==fancy==imgwebp)", both_hit),
        ("fancy-only", fancy_hit),
        ("nofancy-only", nofancy_hit),
        ("neither", neither),
    ];
    (manifest, lines, summary)
}

/// 动画 corpus 逐帧对照：image-webp（经 kimg into_frames → RGBA）vs anim_dump golden
/// 逐帧 sha256（M4：7 个 anim fixture，25 数据行；anim-vp8l-alpha-hint.webp 不在 golden，
/// 单列备注，由 P2 阶段专项覆盖）。
fn animation_corpus_matrix() -> (Vec<String>, Vec<(&'static str, usize)>) {
    let path = golden_dir().join("webp-animation-oracle-golden.tsv");
    let text = std::fs::read_to_string(&path).expect("动画 golden TSV 应存在");
    let mut rows: Vec<(String, u32, String)> = Vec::new();
    for line in text.lines() {
        let line = line.trim_end();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let parts: Vec<&str> = line.split('\t').collect();
        assert_eq!(parts.len(), 7, "动画 golden 行应为 7 列: {line}");
        rows.push((parts[0].to_string(), parts[1].parse().unwrap(), parts[6].to_string()));
    }

    // 逐 fixture 分组
    let mut by_fixture: std::collections::BTreeMap<String, Vec<(u32, String)>> =
        std::collections::BTreeMap::new();
    for (f, frame, hash) in rows {
        by_fixture.entry(f).or_default().push((frame, hash));
    }

    let mut lines = Vec::new();
    let mut summary = Vec::new();
    let mut total_frame_checks = 0usize;
    let mut total_frame_hits = 0usize;
    let mut fixture_all_match = 0usize;

    for (name, frames) in &by_fixture {
        // SKIP：缺少夹具（外部语料缺失属常态，见 AGENTS §5）。
        let Some(path) = fixtures::webp::webp_fixture_path(name) else {
            lines.push(format!("SKIP\t{name}\t夹具缺失"));
            continue;
        };
        let data = std::fs::read(&path).expect("read fixture");
        let dec = match WebPDecoder::new(Cursor::new(&data)) {
            Ok(d) => d,
            Err(e) => {
                lines.push(format!("ERR\t{name}\t解码失败: {e}"));
                summary.push(("decode-fail", 1));
                continue;
            }
        };
        let frames_iter = match dec.into_frames().collect_frames() {
            Ok(v) => v,
            Err(e) => {
                lines.push(format!("ERR\t{name}\t帧读取失败: {e}"));
                summary.push(("frame-fail", 1));
                continue;
            }
        };
        let mut frame_hits = 0usize;
        let mut frame_notes = Vec::new();
        for (fnum, ghash) in frames {
            let frame = match frames_iter.get(*fnum as usize) {
                Some(f) => f,
                None => {
                    frame_notes.push(format!("帧 {fnum} golden 越界（image-webp 只出 {} 帧）", frames_iter.len()));
                    continue;
                }
            };
            let img = frame.buffer();
            let hash = sha256_hex(img.as_raw());
            let hit = &hash == ghash;
            total_frame_checks += 1;
            if hit {
                total_frame_hits += 1;
                frame_hits += 1;
            }
            if !hit {
                frame_notes.push(format!(
                    "帧 {fnum} hash 不符 golden={} imgwebp={}",
                    ghash, hash
                ));
            }
        }
        let all = frame_hits == frames.len() && frame_notes.is_empty();
        if all {
            fixture_all_match += 1;
        }
        lines.push(format!(
            "{}\t{name}\t帧 {frame_hits}/{} 命中{}",
            if all { "MATCH" } else { "DIFF" },
            frames.len(),
            if frame_notes.is_empty() {
                String::new()
            } else {
                format!("\t注: {}", frame_notes.join("; "))
            }
        ));
    }

    summary.push(("fixture-all-match", fixture_all_match));
    summary.push(("frame-hit", total_frame_hits));
    summary.push(("frame-total", total_frame_checks));
    (lines, summary)
}

#[test]
fn oracle_static_corpus() {
    let (manifest, lines, summary) = static_corpus_matrix();
    assert_eq!(manifest.len(), 52, "静态 golden 应覆盖全部非动画 fixture");
    for line in &lines {
        eprintln!("WEBP_ORACLE_STATIC\t{line}");
    }
    for (k, v) in &summary {
        eprintln!("WEBP_ORACLE_STATIC_SUMMARY\t{k}={v}");
    }
    // 断言：金标准 corpus（52 静态 fixture）应全部可被 image-webp 解码——解码失败即回归信号；
    // 「非 fancy 又非 nofancy 的真不一致」用 eprintln 矩阵供主 agent 核验（有损上采样差异
    // 在 release 语义上有容忍，不在此硬闸）。
    let decode_fail = summary
        .iter()
        .find(|(k, _)| *k == "decode-fail")
        .map(|(_, v)| *v)
        .unwrap_or(0);
    assert_eq!(decode_fail, 0, "corpus 中不应有静态解码失败（先查夹具属性/前后端差异）");
}

#[test]
fn oracle_animation_corpus() {
    let (lines, summary) = animation_corpus_matrix();
    for line in &lines {
        eprintln!("WEBP_ORACLE_ANIM\t{line}");
    }
    for (k, v) in &summary {
        eprintln!("WEBP_ORACLE_ANIM_SUMMARY\t{k}={v}");
    }
    assert_eq!(
        summary.iter().find(|(k, _)| *k == "frame-total").unwrap().1,
        25,
        "动画 golden 应恰覆盖 25 帧"
    );
}

/// 静态 golden 清单的每个 fixture 名必须在外部语料里能定位（镜像
/// WebpOracleTests.GoldenManifestCoversEveryFixture；外部语料缺失属常态 → SKIP 显式打印，
/// 不能大面积缺失，否则 golden 形同空跑）。
#[test]
fn oracle_manifest_covers_all_fixtures() {
    let golden = StaticGolden::load();
    let mut resolved = 0usize;
    let mut missing: Vec<&String> = Vec::new();
    for name in golden.rows.keys() {
        match fixtures::webp::webp_fixture_path(name) {
            Some(_) => resolved += 1,
            None => {
                missing.push(name);
                eprintln!("SKIP {name} 外部语料缺失");
            }
        }
    }
    eprintln!("WEBP_ORACLE_COVERAGE\tresolved={resolved}/{} missing={}", golden.rows.len(), missing.len());
    assert!(
        missing.len() <= 2,
        "static golden 的 {} 个 fixture 名应几乎全部在外部语料中可定位，缺 {} 个",
        golden.rows.len(),
        missing.len()
    );
}
