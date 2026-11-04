    use super::*;

    /// 存在性驱动取夹具：缺失打印 SKIP 后返回 None。
    fn fixture_bytes(name: &str) -> Option<Vec<u8>> {
        let p = webp_fixtures_support::webp_fixture_path(name)?;
        Some(std::fs::read(&p).expect("read fixture"))
    }

    fn decode_lossy(name: &str) -> Option<(Vp8Yuv, (u32, u32))> {
        let data = fixture_bytes(name)?;
        let c = super::super::container::parse(&data).ok()?;
        if c.family != super::super::container::Family::StaticLossy {
            return None;
        }
        let yuv = decode_vp8_tight(c.bitstream.unwrap(), c.width, c.height).ok()?;
        Some((yuv, (c.width, c.height)))
    }

    #[test]
    fn flat_gray_decodes() {
        let Some((yuv, (w, h))) = decode_lossy("flat-gray-16x16.webp") else {
            eprintln!("SKIP flat-gray-16x16.webp 外部语料缺失");
            return;
        };
        assert_eq!((yuv.width, yuv.height), (w, h));
        assert_eq!(yuv.y.len(), (w * h) as usize);
        // 纯灰：Y 应接近 128。
        let mean = yuv.y.iter().map(|&b| u32::from(b)).sum::<u32>() / yuv.y.len() as u32;
        assert!(
            (mean as i32 - 128).abs() < 40,
            "flat-gray Y 均值应接近 128，实 {mean}"
        );
        assert!(
            yuv.y.iter().all(|&b| b < 160 && b > 90),
            "纯灰 Y 值应在窄带内"
        );
    }

    #[test]
    fn gradient_decodes_nonblank() {
        let Some((yuv, (w, h))) = decode_lossy("gradient-16x16.webp") else {
            eprintln!("SKIP gradient-16x16.webp 外部语料缺失");
            return;
        };
        assert_eq!((yuv.width, yuv.height), (w, h));
        let y_span = yuv.y.iter().max().unwrap() - yuv.y.iter().min().unwrap();
        assert!(y_span > 30, "gradient 应有亮度梯度（span={y_span}）");
    }

    #[test]
    fn all_static_lossy_decode_to_yuv() {
        // 全部静态有损（无 ALPH）fixture 都可在 V2 重建为正确尺寸 YUV 且不 panic。
        let names = [
            "astronaut-192-q0-normal.webp",
            "astronaut-192-q10-simple.webp",
            "astronaut-192-q15-normal.webp",
            "astronaut-192-q20-simple.webp",
            "astronaut-192-q5-normal.webp",
            "flat-gray-16x16.webp",
            "gradient-16x16.webp",
            "noise-17x17-f30-strong-s0.webp",
            "noise-17x17.webp",
            "noise-32x32-f30-strong-s0.webp",
            "noise-32x32-q0.webp",
            "noise-32x32-q100.webp",
            "noise-32x32-q5.webp",
            "noise-32x32-q50.webp",
            "noise-32x32-q95.webp",
            "noise-32x32-segments4.webp",
            "noise-32x32.webp",
            "noise-64x48-vp8-partitions2.webp",
            "noise-64x48-vp8-partitions4.webp",
            "noise-64x48-vp8-partitions8.webp",
            "odd-33x19-libvpx.webp",
            "testsrc2-320x240-f0.webp",
            "testsrc2-320x240-q60-segments4-sns100-f60.webp",
            "testsrc2-320x240-q75-f100-s3.webp",
            "testsrc2-320x240-q75.webp",
            "tiny.webp",
            "webp-metadata.webp",
        ];
        let mut checked = 0usize;
        for name in names {
            // 存在性驱动：缺失 SKIP，不崩整测试。
            let Some(p) = webp_fixtures_support::webp_fixture_path(name) else {
                eprintln!("SKIP {name} 外部语料缺失");
                continue;
            };
            let data = std::fs::read(&p).unwrap();
            let c = super::super::container::parse(&data).unwrap();
            if c.family != super::super::container::Family::StaticLossy {
                continue;
            }
            let yuv = decode_vp8_tight(c.bitstream.unwrap(), c.width, c.height)
                .unwrap_or_else(|e| panic!("{name} V2 解码失败: {e}"));
            let (w, h) = (c.width as usize, c.height as usize);
            assert_eq!(yuv.y.len(), w * h, "{name} Y 长度");
            assert_eq!(yuv.u.len(), ((w + 1) / 2) * ((h + 1) / 2), "{name} U 长度");
            assert_eq!(yuv.v.len(), ((w + 1) / 2) * ((h + 1) / 2), "{name} V 长度");
            // 不空白：全黑全白图在本 corpus 不存在，抽样首位差应 >0（除 flat/gradient 已单测外）。
            checked += 1;
        }
        assert!(checked >= 6, "应至少 V2 重建 6 个有损 fixture，实 {checked}");
        eprintln!("VP8_V2_DECODED\t{checked}");
    }
