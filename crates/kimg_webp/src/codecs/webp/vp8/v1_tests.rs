    use super::*;

    /// 存在性驱动取夹具：缺失打印 SKIP 后返回 None（AGENTS §5：缺失属常态，不崩整测试）。
    fn fixture_bytes(name: &str) -> Option<Vec<u8>> {
        let p = webp_fixtures_support::webp_fixture_path(name)?;
        Some(std::fs::read(&p).expect("read fixture"))
    }

    /// 从完整 webp 文件取 VP8 载荷（裸容器 'VP8 '，无 VP8X：首 chunk 即 VP8 ）。
    fn vp8_payload(data: &[u8]) -> &[u8] {
        let c = super::super::container::parse(data).expect("容器解析应成功");
        assert_eq!(c.family, super::super::container::Family::StaticLossy);
        c.bitstream.unwrap()
    }

    /// 已知 fixture 的帧头字段正确性（keyframe、尺寸、mb 数、分区数）。
    fn assert_header(name: &str, w: u32, h: u32, parts: usize) {
        let Some(data) = fixture_bytes(name) else {
            eprintln!("SKIP {name} 外部语料缺失");
            return;
        };
        let payload = vp8_payload(&data);
        let fh = parse_frame_header(payload).unwrap_or_else(|e| panic!("{name} 头解析失败: {e}"));
        assert_eq!(
            (fh.width, fh.height),
            (w, h),
            "{name} 尺寸不符（帧头 vs 预期）"
        );
        assert_eq!(fh.mb_width, ((w + 15) / 16) as usize, "{name} mb_width");
        assert_eq!(fh.mb_height, ((h + 15) / 16) as usize, "{name} mb_height");
        assert_eq!(fh.partitions.len(), parts, "{name} 分区数");
    }

    #[test]
    fn tiny_1x1_lossy_header() {
        // tiny.webp：2x2 有损。
        assert_header("tiny.webp", 2, 2, 1);
    }

    #[test]
    fn flat_gray_header() {
        // flat-gray-16x16：纯 128 灰，常为 1 分区低码率。
        assert_header("flat-gray-16x16.webp", 16, 16, 1);
    }

    #[test]
    fn gradient_header() {
        assert_header("gradient-16x16.webp", 16, 16, 1);
    }

    #[test]
    fn multi_partition_header() {
        // noise-64x48-vp8-partitions8：高分辨率噪声 → 8 分区。
        assert_header("noise-64x48-vp8-partitions8.webp", 64, 48, 8);
        assert_header("noise-64x48-vp8-partitions4.webp", 64, 48, 4);
        assert_header("noise-64x48-vp8-partitions2.webp", 64, 48, 2);
    }

    #[test]
    fn segment_features_fixture() {
        // noise-32x32-segments4：显式段（4 段）。
        let Some(data) = fixture_bytes("noise-32x32-segments4.webp") else {
            eprintln!("SKIP noise-32x32-segments4.webp 外部语料缺失");
            return;
        };
        let fh = parse_frame_header(vp8_payload(&data)).unwrap();
        assert!(fh.segments.enabled, "segments4 应开段");
    }

    #[test]
    fn all_lossy_fixtures_parse_header() {
        // 头均可解析；尺寸与容器一致（容器早验，此处复核）。
        let names = [
            "lossy-alpha-m0-best.webp", // 注意：此处仅有损（无 alpha）体积小；它有 ALPH → 家族 LossyAlpha，跳过。
            "lossy-alpha-m0-fast.webp",
            "lossy-alpha-m0-none.webp",
            "lossy-alpha-m1-best.webp",
            "lossy-alpha-m1-fast.webp",
            "lossy-alpha-m1-none.webp",
            "lossy-alpha-q50.webp",
            "noise-17x17-f30-strong-s0.webp",
            "noise-17x17.webp",
            "noise-32x32-q0.webp",
            "noise-32x32-q5.webp",
            "noise-32x32-q50.webp",
            "noise-32x32-q95.webp",
            "noise-64x48-vp8-partitions2.webp",
            "noise-64x48-vp8-partitions4.webp",
            "odd-33x19-libvpx.webp",
            "testsrc2-320x240-f0.webp",
        ];
        let mut checked = 0usize;
        for name in names {
            let Some(p) = webp_fixtures_support::webp_fixture_path(name) else {
                eprintln!("SKIP {name} 外部语料缺失");
                continue;
            };
            let data = std::fs::read(&p).unwrap();
            let c = super::super::container::parse(&data).unwrap();
            if c.family != super::super::container::Family::StaticLossy {
                continue; // 带 alpha 的 lossy 家族 V6 才接
            }
            let fh = parse_frame_header(c.bitstream.unwrap())
                .unwrap_or_else(|e| panic!("{name} 头解析失败: {e}"));
            assert_eq!((fh.width, fh.height), (c.width, c.height), "{name}");
            checked += 1;
        }
        assert!(checked >= 4, "应当至少解析 4 个有损 fixture，实 {checked}");
        eprintln!("VP8_HEADER_PARSED\t{checked}");
    }
