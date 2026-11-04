    use std::io::Cursor;

    use super::*;

    /// 确定性 LCG（xorshift64*），不依赖 rand。
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x.wrapping_mul(0x2545F4914F6CDD1D)
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    #[test]
    fn mutated_inputs_never_panic() {
        let seeds = [
            "tiny.webp",
            "flat-gray-16x16.webp",
            "gradient-16x16.webp",
            "lossless-z0.webp",
            "noise-32x32-q50.webp",
            "lossy-alpha-q50.webp",
        ];
        let mut rng = Lcg(0x9E3779B97F4A7C15);
        let mut rounds = 0usize;
        for seed in seeds {
            // 存在性驱动：缺失 SKIP，不崩整测试。
            let Some(p) = webp_fixtures_support::webp_fixture_path(seed) else {
                eprintln!("SKIP fuzz seed {seed} 外部语料缺失");
                continue;
            };
            let base = std::fs::read(&p).unwrap();
            for round in 0..40 {
                let kind = rng.below(4);
                let mut data = base.clone();
                match kind {
                    0 => {
                        // 1-3 位翻转。
                        let flips = 1 + rng.below(3);
                        for _ in 0..flips {
                            let off = rng.below(data.len());
                            let bit = rng.below(8);
                            data[off] ^= 1 << bit;
                        }
                    }
                    1 => {
                        // 截断到随机长度。
                        let len = rng.below(data.len() + 1);
                        data.truncate(len);
                    }
                    2 => {
                        // 随机位置插字节（0..30 字节）。
                        let count = rng.below(30) + 1;
                        let off = rng.below(data.len() + 1);
                        let mut ins = vec![0u8; count];
                        for b in ins.iter_mut() {
                            *b = rng.next() as u8;
                        }
                        data.splice(off..off, ins);
                    }
                    _ => {
                        // 区块随机字节改写（≤16 字节）。
                        let count = rng.below(16) + 1;
                        let hi = data.len().saturating_sub(count).max(1);
                        let off = rng.below(hi);
                        let end = (off + count).min(data.len());
                        for b in data[off..end].iter_mut() {
                            *b = rng.next() as u8;
                        }
                    }
                }
                // ① 容器解析不 panic。
                let _ = super::super::container::parse(&data);
                // ② 全链路 WebPDecoder 不 panic（AUTO 后端：自研可接管家族走 Ours/其余 image-webp）。
                let _ = crate::codecs::webp::WebPDecoder::new(Cursor::new(&data));
                // ③ 自研入口不 panic（防止几何放大：仅当容器解析成功且像素量受控时深解）。
                if let Ok(c) = super::super::container::parse(&data) {
                    let px = u64::from(c.width) * u64::from(c.height);
                    if px > 0 && px <= (1 << 20) {
                        if c.family.is_static_lossless() {
                            let _ = crate::codecs::webp::decode_static_lossless(&data);
                        } else if matches!(
                            c.family,
                            super::super::container::Family::StaticLossy
                                | super::super::container::Family::LossyAlpha
                        ) {
                            let _ = crate::codecs::webp::decode_static_lossy(&data);
                        }
                    }
                }
                let _ = round;
                rounds += 1;
            }
        }
        assert!(rounds >= 200, "fuzz 轮次应 ≥200，实 {rounds}");
        eprintln!("VP8_FUZZ_ROUNDS\t{rounds}");
    }
