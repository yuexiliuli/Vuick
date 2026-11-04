    use std::io::Cursor;

    use super::*;
    use crate::codecs::webp::{BACKEND_AUTO, BACKEND_OURS, WebPDecoder, set_webp_backend};
    use crate::ImageDecoder;

    /// image-webp dev oracle：直连 `image_webp::WebPDecoder` 解静态图为 RGBA8。
    fn imagewebp_rgba(data: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
        let mut dec = image_webp::WebPDecoder::new(Cursor::new(data)).ok()?;
        let (w, h) = dec.dimensions();
        let has_alpha = dec.has_alpha();
        let n = dec.output_buffer_size()?;
        let mut buf = vec![0u8; n];
        dec.read_image(&mut buf).ok()?;
        if has_alpha {
            Some((w, h, buf))
        } else {
            let mut rgba = Vec::with_capacity(w as usize * h as usize * 4);
            for px in buf.chunks_exact(3) {
                rgba.extend_from_slice(&[px[0], px[1], px[2], 255]);
            }
            Some((w, h, rgba))
        }
    }

    fn ours_rgba(data: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
        let c = super::super::container::parse(data).ok()?;
        if !matches!(
            c.family,
            super::super::container::Family::StaticLossy
                | super::super::container::Family::LossyAlpha
        ) {
            return None;
        }
        let ours = crate::codecs::webp::decode_static_lossy(data).ok()?.expect("自研有损应可接管");
        if ours.width != c.width || ours.height != c.height {
            return None;
        }
        // 有 alpha：RGBA4 原样；opaque 有损自 T1 起亦持 RGBA4（α=255，`ours.rgba` 直出）→
        // 同上分支；`rgb` 遗留恒空（dormancy），else 分支仅作手工构造回退。
        if !ours.rgba.is_empty() {
            Some((ours.width, ours.height, ours.rgba.as_bytes().to_vec()))
        } else {
            let mut rgba = Vec::with_capacity((ours.rgb.len() / 3) * 4);
            for px in ours.rgb.chunks_exact(3) {
                rgba.extend_from_slice(&[px[0], px[1], px[2], 255]);
            }
            Some((ours.width, ours.height, rgba))
        }
    }

    #[test]
    fn lossy_rgba_matches_imagewebp_byte_exact() {
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
            let Some(path) = webp_fixtures_support::webp_fixture_path(&name) else {
                eprintln!("SKIP {name} 外部语料缺失");
                skipped += 1;
                continue;
            };
            let data = std::fs::read(&path).expect("read fixture");
            let c = match super::super::container::parse(&data) {
                Ok(c) => c,
                Err(_) => {
                    eprintln!("SKIP {name} 容器解析失败");
                    skipped += 1;
                    continue;
                }
            };
            if c.family != super::super::container::Family::StaticLossy
                && c.family != super::super::container::Family::LossyAlpha
            {
                continue;
            }
            // 任一通道解不了 → SKIP（外部语料可能含本实现暂不接的文件，不崩整测试）。
            let (Some((ow, oh, orba)), Some((iw, ih, irba))) =
                (ours_rgba(&data), imagewebp_rgba(&data))
            else {
                eprintln!("SKIP {name} 自研或 image-webp 解码失败");
                skipped += 1;
                continue;
            };
            assert_eq!((ow, oh), (iw, ih), "{name} 尺寸 OURS vs image-webp");
            assert_eq!(orba.len(), irba.len(), "{name} 长度");
            let mut diffs = 0usize;
            for (a, b) in orba.iter().zip(irba.iter()) {
                if a != b {
                    diffs += 1;
                }
            }
            checked += 1;
            if diffs != 0 {
                // 金标准（SimdCodecs 52 图名）不匹配=失败；越界语料（用户/imgbench-corpus）
                // 仅记录不判失败（AGENTS §5：找到几个测几个，别让外部语料崩整测试）。
                if webp_fixtures_support::is_golden_static_name(&name) {
                    mismatches.push((name, diffs));
                } else {
                    eprintln!("VP8_EXTRA_DIFF\t{name}\t{diffs} bytes（非金标准外部语料，仅记录）");
                }
            }
        }
        for (name, diffs) in &mismatches {
            eprintln!("VP8_RGBA_DIFF\t{name}\t{diffs} bytes");
        }
        eprintln!("VP8_RGBA_PARITY_CHECKED\t{checked}, skipped={skipped}, mismatches={}", mismatches.len());
        set_webp_backend(BACKEND_AUTO); // 复位：避免污染同 binary 其它测试的后端全局态
        assert!(
            checked >= 6,
            "应至少 6 个有损 fixture 对拍，实 {checked}"
        );
        assert!(
            mismatches.is_empty(),
            "{} 个 fixture RGBA 与 image-webp 逐字节不符（V4 fancy 需修）",
            mismatches.len()
        );
    }

    /// 冒烟：经 `WebPDecoder`（OURS 后端）解码同 fixture 与直接调用一致，且格式为 Rgba8
    /// （T1：opaque 有损恒定 RGBA8 α=255，`read_image` RGBA 目标直拷，`into_rgba8` 零拷贝）。
    #[test]
    fn ours_backend_webpdecoder() {
        set_webp_backend(BACKEND_OURS);
        let Some(path) = webp_fixtures_support::webp_fixture_path("testsrc2-320x240-q75.webp")
        else {
            eprintln!("SKIP testsrc2-320x240-q75.webp 外部语料缺失");
            return;
        };
        let data = std::fs::read(path).expect("read fixture");
        let dec = WebPDecoder::new(Cursor::new(&data)).unwrap();
        let (w, h) = dec.dimensions();
        assert_eq!((w, h), (320, 240));
        assert_eq!(dec.color_type(), crate::ColorType::Rgba8);
        let mut buf = vec![0u8; dec.total_bytes() as usize];
        dec.read_image(&mut buf).unwrap();
        assert_eq!(buf.len(), (w * h * 4) as usize);
        // α 通道恒 0xFF（opaque 有损单遍直出熔入）。
        assert!(buf.chunks_exact(4).all(|px| px[3] == 0xFF));
        set_webp_backend(BACKEND_AUTO);
    }
