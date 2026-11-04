use super::*;


    /// golden TSV 保留在仓内；图片取外部语料。
    const GOLDEN_TSV: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/webp/webp-oracle-golden.tsv");

    fn golden_yuv_sha(name: &str) -> Option<String> {
        let path = std::path::Path::new(GOLDEN_TSV);
        let text = std::fs::read_to_string(path).unwrap();
        for line in text.lines() {
            let line = line.trim_end();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() == 6 && parts[0] == name {
                return Some(parts[3].to_lowercase());
            }
        }
        None
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(bytes);
        format!("{:x}", h.finalize())
    }

    #[test]
    fn yuv_matches_golden_per_fixture() {
        let mut mismatches = Vec::new();
        let mut checked = 0usize;
        let mut skipped = 0usize;
        // 从外部语料目录按名收集（含用户语料 / imgbench-corpus / SimdCodecs 夹具）。
        let mut names = webp_fixtures_support::webp_fixture_names();
        names.sort();
        for name in names {
            if !name.ends_with(".webp") || name.starts_with("anim-") {
                continue;
            }
            let Some(expect) = golden_yuv_sha(&name) else { continue };
            let Some(path) = webp_fixtures_support::webp_fixture_path(&name) else {
                eprintln!("SKIP {name} 外部语料缺失");
                skipped += 1;
                continue;
            };
            let Some((yuv, w, h)) = (|| {
                let data = std::fs::read(&path).ok()?;
                let c = super::super::container::parse(&data).ok()?;
                if c.family != super::super::container::Family::StaticLossy {
                    return None;
                }
                let yuv = decode_vp8_tight(c.bitstream?, c.width, c.height).ok()?;
                Some((yuv, c.width as usize, c.height as usize))
            })() else {
                continue;
            };
            let mut flat = yuv.y.clone();
            let cu = (w + 1) / 2;
            let chh = (h + 1) / 2;
            flat.extend_from_slice(&yuv.u[..cu * chh]);
            flat.extend_from_slice(&yuv.v[..cu * chh]);
            let got = sha256_hex(&flat);
            checked += 1;
            if got != expect {
                mismatches.push((name, expect, got));
            }
        }
        for (name, expect, got) in &mismatches {
            eprintln!("VP8_YUV_MISMATCH\t{name}\ngolden={expect}\nours  ={got}");
        }
        eprintln!("VP8_YUV_GOLDEN_CHECKED\t{checked}, skipped={skipped}");
        assert!(
            checked >= 6,
            "应至少 6 个有损 fixture 有 YUV golden，实 {checked}"
        );
        // 目标：全 0 差异（重建+滤波逐字节对齐 dwebp -yuv）。
        assert!(
            mismatches.is_empty(),
            "{} 个 fixture YUV 与 golden 不符（V2/V3 需修）",
            mismatches.len()
        );
    }
