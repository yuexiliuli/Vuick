    use super::*;

    /// 手动探针取夹具：外部语料缺失直接 panic（手动运行现场可见原因）。
    fn corpus_path(name: &str) -> std::path::PathBuf {
        webp_fixtures_support::webp_fixture_path(name)
            .unwrap_or_else(|| panic!("夹具缺失（外部语料）：{name}"))
    }

    /// 确定性 LCG（xorshift64*），探针用。
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
        fn byte(&mut self) -> u8 {
            (self.next() >> 40) as u8
        }
    }

    /// 全链（重建+YUV→RGBA）SIMD 开/关逐字节一致（D1/D2 接入后沿用的硬闸直证）。
    #[test]
    fn full_decode_simd_matches_scalar() {
        let _ = super::super::simd::set_simd_part_override("pred", Some(false));
        let _ = super::super::simd::set_simd_part_override("flt", Some(false));
        let Some(scalar) = helper::decode_testsrc2() else {
            eprintln!("SKIP full_decode_simd_matches_scalar: testsrc2 外部语料缺失");
            return;
        };
        let _ = super::super::simd::set_simd_part_override("pred", Some(true));
        let _ = super::super::simd::set_simd_part_override("flt", Some(true));
        let Some(simd) = helper::decode_testsrc2() else {
            eprintln!("SKIP full_decode_simd_matches_scalar: testsrc2 外部语料缺失");
            return;
        };
        let _ = super::super::simd::set_simd_part_override("pred", None);
        let _ = super::super::simd::set_simd_part_override("flt", None);
        assert_eq!(scalar, simd, "testsrc2 全链 SIMD vs 标量逐字节");
        eprintln!("VP8_FULL_DECODE_SIMD_PARITY_OK");
    }

    /// 原有诊断：有损解码分阶段计时（判别 YUV 转换 vs 帧重建占比；`#[ignore]` 手动 release 跑）。
    #[test]
    #[ignore = "手动运行"]
    fn lossy_stage_split() {
        use std::time::Instant;
        for name in [
            "testsrc2-320x240-q75.webp",
            "astronaut-192-q10-normal.webp",
            "noise-64x48-vp8-partitions8.webp",
        ] {
            let data = std::fs::read(corpus_path(name)).expect("read fixture");
            let c = super::super::container::parse(&data).unwrap();
            for _ in 0..3 {
                let t0 = Instant::now();
                let mut yuv = decode_vp8(c.bitstream.unwrap(), c.width, c.height).unwrap();
                let t1 = Instant::now();
                let rgba = yuv_to_rgba_fancy(&mut yuv);
                let t2 = Instant::now();
                let (_r, _g, _b) = (rgba[0], rgba[1], rgba[2]);
                eprintln!(
                    "VP8_STAGE {name} {}x{}: decode_vp8={:.3}ms yuv_to_rgba={:.3}ms total={:.3}ms",
                    yuv.width,
                    yuv.height,
                    t1.duration_since(t0).as_secs_f64() * 1e3,
                    t2.duration_since(t1).as_secs_f64() * 1e3,
                    t2.duration_since(t0).as_secs_f64() * 1e3,
                );
            }
        }
    }

    /// 大图分阶段占比（release 手动跑 `-- --ignored`）：真墙钟输出熵/预测/变换/滤波各占比，
    /// 用于定 ①熵批量位读 / ②IDCT SIMD / ③环路滤波 的投入方向（K53 测量先行）。
    #[test]
    #[ignore = "手动运行"]
    fn lossy_stage_split_big() {
        use std::time::Instant;
        let big = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("target")
            .join("imgbench-corpus")
            .join("WEBP")
            .join("gen28-photo-lossy-q75.webp");
        let (name, data) = if big.is_file() {
            ("gen28-photo-lossy-q75.webp".to_string(), std::fs::read(&big).unwrap())
        } else {
            ("testsrc2-320x240-q75.webp".to_string(), std::fs::read(corpus_path("testsrc2-320x240-q75.webp")).unwrap())
        };
        let c = super::super::container::parse(&data).unwrap();
        let (w, h) = (c.width, c.height);
        let rounds = 7usize;
        let mut rec_ms = 0f64;
        let mut rgba_ms = 0f64;
        let mut e_ms = 0f64;
        let mut p_ms = 0f64;
        let mut i_ms = 0f64;
        let mut f_ms = 0f64;
        stage_clock::set(true);
        for _ in 0..2 {
            let _ = decode_vp8(c.bitstream.unwrap(), w, h).unwrap();
        }
        for _ in 0..rounds {
            stage_clock::reset();
            let t0 = Instant::now();
            let mut yuv = decode_vp8(c.bitstream.unwrap(), w, h).unwrap();
            let t1 = Instant::now();
            let _rgba = yuv_to_rgba_fancy(&mut yuv);
            let t2 = Instant::now();
            let (e, p, i, f) = stage_clock::totals();
            rec_ms += t1.duration_since(t0).as_secs_f64() * 1e3;
            rgba_ms += t2.duration_since(t1).as_secs_f64() * 1e3;
            e_ms += e;
            p_ms += p;
            i_ms += i;
            f_ms += f;
        }
        stage_clock::set(false);
        let n = rounds as f64;
        let rec = rec_ms / n;
        let rgba = rgba_ms / n;
        let ent = e_ms / n;
        let prd = p_ms / n;
        let idt = i_ms / n;
        let flt = f_ms / n;
        let inner = ent + prd + idt + flt;
        eprintln!(
            "WEBP_STAGE {name} {w}x{h} rec={rec:.2}ms yuv_rgba={rgba:.2}ms full={:.2}ms | \
             entropy={ent:.2}ms({:.1}%) predict={prd:.2}ms({:.1}%) idct={idt:.2}ms({:.1}%) \
             filter+copy={flt:.2}ms({:.1}%) inner_sum={inner:.2} rec_minus_inner={:.2}ms",
            rec + rgba,
            ent / rec * 100.0,
            prd / rec * 100.0,
            idt / rec * 100.0,
            flt / rec * 100.0,
            rec - inner
        );
    }

    /// RC3 探针：各熵决策点结果偏斜（`stage_profile` feature 门控；`#[ignore]` 手动 debug 跑）。
    /// 遍历全部静态有损 fixture，累计每个决策点 bit0/bit1 次数 + 熵预算（tokens+modes 墙钟）。
    /// 判定「cmov 全算 → 预测分支形」是否值得投（K60 ≥8% 熵墙钟才投）；纯测量不改产品行为。
    #[test]
    #[ignore = "手动运行：RC3 偏斜探针"]
    #[cfg(feature = "stage_profile")]
    fn ent_skew_by_decision() {
        use std::time::Instant;
        let names = webp_fixtures_support::webp_fixture_names();
        let t0 = Instant::now();
        let mut decoded = 0usize;
        let mut skipped = 0usize;
        let mut ent_ns: u64 = 0;
        let mut bits: u64 = 0;
        let mut acct: u64 = 0;
        let mut dq: u64 = 0;
        // 32767 是 max dimension false positive 硬上限检查用的哨兵，正常 fixture 不触发。
        let mut total_px: u64 = 0;
        skew_clock::reset();
        for name in &names {
            let Some(path) = webp_fixtures_support::webp_fixture_path(name) else {
                continue;
            };
            let Ok(data) = std::fs::read(&path) else { continue };
            let Ok(c) = super::super::container::parse(&data) else {
                skipped += 1;
                continue;
            };
            if c.family != super::super::container::Family::StaticLossy
                && c.family != super::super::container::Family::LossyAlpha
            {
                skipped += 1;
                continue;
            }
            let Some(frame) = c.bitstream else {
                skipped += 1;
                continue;
            };
            total_px += u64::from(c.width) * u64::from(c.height);
            match stage_decode(&frame, c.width, c.height) {
                Ok(p) => {
                    ent_ns += p.ent_tokens_ns + p.ent_modes_ns;
                    bits += p.tokens_bits + p.modes_bits;
                    acct += p.acct_count;
                    dq += p.dq_count;
                    decoded += 1;
                }
                Err(e) => {
                    eprintln!("SKIP {name}: {e}");
                    skipped += 1;
                }
            }
        }
        let (c_bit, c_acct, c_dq) = probe_entropy_op_costs(2_000_000);
        eprintln!(
            "RC3_SKEW fixtures_decoded={decoded} skipped={skipped} px={total_px} wall={:.2}s",
            t0.elapsed().as_secs_f64()
        );
        eprintln!("RC3_SKEW ent_ns={ent_ns} bits={bits} acct={acct} dq={dq}");
        eprintln!("RC3_SKEW c_bit={c_bit:.3}ns c_acct={c_acct:.3}ns c_dq={c_dq:.3}ns");
        eprintln!(
            "\nRC3_SKEW 决策点偏斜表（熵墙钟预算 = ent_tokens+ent_modes {:.3}ms, bits={})",
            ent_ns as f64 / 1e6,
            bits
        );
        eprintln!("RC3_SKEW\t点 | count0/count1 | 偏斜m | 少数r | 仿uop净(sym) | 误判罚(sym) | 次数");
        let mut all_sym_cycles = 0f64;
        for (name, counts) in skew_clock::all() {
            let total = counts.c0 + counts.c1;
            if total == 0 {
                continue;
            }
            let hi = counts.c0.max(counts.c1);
            let m = hi as f64 / total as f64;
            let r = 1.0 - m;
            let net = (1.0 - 2.0 * r) * 7.0;
            let misp = r * 20.0;
            let sym = net - misp;
            all_sym_cycles += sym * total as f64;
            eprintln!(
                "RC3_SKEW\t{name} | {}/{} | {:.4} | {:.4} | {:+.2} | {:.2} | {}",
                counts.c0, counts.c1, m, r, sym, misp, total
            );
        }
        // 折入熵墙钟%：模型每符号（一次 bit 解码）≈7 cycle（汇编取证 ent_read_bit ~28-30uop
        // 吞吐受限）；以同构建 c_bit_ns/7 把模型 cycle→ns 换算，占比 = 净省 ns / 实测熵墙钟 ns。
        let cyc_ns = c_bit / 7.0;
        let save_ns = all_sym_cycles * cyc_ns;
        let pct = save_ns / ent_ns.max(1) as f64 * 100.0;
        eprintln!(
            "\nRC3_SKEW 合计（全部点，含抛硬币点 cost>0）净省 ≈ {:.2} Mcy → {:.2} ms → 熵墙钟 {:.2}%",
            all_sym_cycles / 1e6,
            save_ns / 1e6,
            pct
        );
        // 候选点 = 偏斜 ≥85% 且 net>0（分支形确有正收益者）。
        eprintln!("RC3_SKEW 候选点（偏斜≥85% 且净收益>0）：");
        let mut c_cycles = 0f64;
        for (name, counts) in skew_clock::all() {
            let total = counts.c0 + counts.c1;
            if total == 0 {
                continue;
            }
            let hi = counts.c0.max(counts.c1);
            let m = hi as f64 / total as f64;
            let r = 1.0 - m;
            let sym = (1.0 - 2.0 * r) * 7.0 - r * 20.0;
            if m >= 0.85 && sym > 0.0 {
                c_cycles += sym * total as f64;
                eprintln!(
                    "RC3_SKEW\t   {name}: m={:.4} r={:.4} net={:+.2}cy/sym × {total}次 = {:.2} Mcy ({:.3}ms)",
                    m,
                    r,
                    sym,
                    sym * total as f64 / 1e6,
                    sym * total as f64 * cyc_ns / 1e6
                );
            }
        }
        let c_ns = c_cycles * cyc_ns;
        let c_pct = c_ns / ent_ns.max(1) as f64 * 100.0;
        eprintln!(
            "RC3_SKEW 候选点净省 ≈ {:.2} Mcy → {:.3} ms → 熵墙钟 {:.2}%",
            c_cycles / 1e6,
            c_ns / 1e6,
            c_pct
        );
        eprintln!(
            "RC3_SKEW 口径：c_bit={c_bit:.3}ns/bit（同构建），模型 7cy/bit ⇒ 1cy ≈ {:.4}ns；熵墙钟=实测 ent_tokens+ent_modes",
            cyc_ns
        );
        assert!(decoded > 0, "应至少解出 >0 个静态有损 fixture，实 {decoded}");
    }

    /// 熔合 SIMD vs 标量全路径逐字节：随机小帧 + 随机滤波强度（level/inner/sharpness/simple
    /// 全跨），以生产 `filter_macroblock_row` 为对象 —— 覆盖真实强度求值、宏块左/上/内边全组合
    /// 与「后边读前边已滤像素」的顺序依赖。SIMD 路由经进程内 override 强制开/关对拍。
    #[test]
    fn filter_macroblock_simd_matches_scalar() {
        use super::super::simd::{set_simd_part_override, simd_on};
        if !simd_on("sse2") {
            eprintln!("SIMD_TEST SKIP (无 sse2)");
            return;
        }
        let mut rng = Lcg(0xf1a7b0c0ffee1234);
        let (mbw, mbh) = (5usize, 2usize);
        let y_pitch = mbw * 16 + 32;
        let uv_pitch = mbw * 8 + 32;
        let y_rows = mbh * 16 + 32;
        let uv_rows = mbh * 8 + 32;
        let mut checked = 0usize;
        for simple in [false, true] {
            for sharpness in [0u8, 1, 4, 5, 7] {
                for _trial in 0..4 {
                    let mut rf = Vec::with_capacity(mbw * mbh);
                    for _ in 0..mbw * mbh {
                        // level 覆盖：0（跳过）、hev 阈值上下 15/39/40、普通 2/63、随机 ≤63。
                        let level = match rng.next() % 7 {
                            0 => 0u8,
                            1 => 2,
                            2 => 15,
                            3 => 39,
                            4 => 40,
                            5 => 63,
                            _ => (rng.byte() % 64),
                        };
                        let inner = if (rng.next() & 1) == 0 { 1u8 } else { 0u8 };
                        rf.push((level, inner));
                    }
                    let mut flt = FilterSettings::default();
                    flt.simple = simple;
                    flt.sharpness = sharpness;
                    let mut ay = vec![0u8; y_rows * y_pitch];
                    let mut au = vec![0u8; uv_rows * uv_pitch];
                    let mut av = vec![0u8; uv_rows * uv_pitch];
                    fill_rand_byte(&mut rng, &mut ay);
                    fill_rand_byte(&mut rng, &mut au);
                    fill_rand_byte(&mut rng, &mut av);
                    let (mut by, mut bu, mut bv) = (ay.clone(), au.clone(), av.clone());
                    for mby in 0..mbh {
                        set_simd_part_override("flt", Some(true));
                        filter_macroblock_row(&mut ay, y_pitch, &mut au, &mut av, uv_pitch, mbw, mby, &flt, &rf[mby * mbw..(mby + 1) * mbw]);
                        set_simd_part_override("flt", Some(false));
                        filter_macroblock_row(&mut by, y_pitch, &mut bu, &mut bv, uv_pitch, mbw, mby, &flt, &rf[mby * mbw..(mby + 1) * mbw]);
                        assert_eq!(ay, by, "Y 熔合 vs 标量 simple={simple} sharp={sharpness} mby={mby}");
                        assert_eq!(au, bu, "U 熔合 vs 标量 simple={simple} sharp={sharpness} mby={mby}");
                        assert_eq!(av, bv, "V 熔合 vs 标量 simple={simple} sharp={sharpness} mby={mby}");
                        checked += 1;
                    }
                    set_simd_part_override("flt", None);
                }
            }
        }
        eprintln!("FILTER_MACROBLOCK_SIMD_PARITY\t{checked}");
    }

    fn fill_rand_byte(rng: &mut Lcg, plane: &mut [u8]) {
        for b in plane.iter_mut() {
            *b = rng.byte();
        }
    }

    mod helper {
        use super::*;
        /// 解 testsrc2-320x240-q75（全链 YUV+RGBA），返回 (y, u, v, rgba)；夹具缺失返回 None。
        pub fn decode_testsrc2() -> Option<(Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>)> {
            let p = webp_fixtures_support::webp_fixture_path("testsrc2-320x240-q75.webp")?;
            let data = std::fs::read(p).expect("read fixture");
            let c = super::super::super::container::parse(&data).expect("容器解析");
            let mut yuv = decode_vp8_tight(c.bitstream.unwrap(), c.width, c.height).expect("VP8 解码");
            let rgba = yuv_to_rgba_fancy(&mut yuv);
            Some((yuv.y, yuv.u, yuv.v, rgba))
        }
    }

    /// D1/D2 SIMD 性能探针（release 手动跑 `-- --ignored`）：中大图全链 ours-SIMD
    /// 各部件开/关 + 与 image-webp 中位比值；输出 `WEBP_SIMD_D1/D2 before/after/ratio`。
    #[test]
    #[ignore = "手动运行"]
    fn simd_d1_d2_probe() {
        use std::time::Instant;
        let mut images: Vec<(String, Vec<u8>)> = Vec::new();
        for name in [
            "testsrc2-320x240-q75.webp",
            "astronaut-192-q20-normal.webp",
            "noise-64x48-vp8-partitions8.webp",
        ] {
            if let Some(p) = webp_fixtures_support::webp_fixture_path(name) {
                images.push((name.to_string(), std::fs::read(p).unwrap()));
            } else {
                eprintln!("WEBP_SIMD_PROBE note 夹具缺失（外部语料）：{name}");
            }
        }
        // 大图语料：target/imgbench-corpus/WEBP（缺则 SKIP）。
        let big = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("target")
            .join("imgbench-corpus")
            .join("WEBP")
            .join("gen28-photo-lossy-q75.webp");
        if big.is_file() {
            images.push(("gen28-photo-lossy-q75.webp".to_string(), std::fs::read(&big).unwrap()));
        } else {
            eprintln!("WEBP_SIMD_PROBE note 大图语料缺失（target/imgbench-corpus/WEBP/gen28-photo-lossy-q75.webp）");
        }

        fn median(mut f: impl FnMut(), n: usize) -> f64 {
            f();
            let mut acc = Vec::with_capacity(n);
            for _ in 0..n {
                let t = Instant::now();
                f();
                acc.push(t.elapsed().as_secs_f64() * 1e3);
            }
            acc.sort_by(|a, b| a.partial_cmp(b).unwrap());
            acc[acc.len() / 2]
        }

        for (name, bytes) in &images {
            let c = super::super::container::parse(bytes).unwrap();
            let (w, h) = (c.width, c.height);
            // 轮数：大图 9 轮取中位压噪声；小图同样 9（微秒级也稳）。
            let rounds = 9usize;
            let run = |pred: Option<bool>, flt: Option<bool>, dct: Option<bool>| -> (f64, f64) {
                super::super::simd::set_simd_part_override("pred", pred);
                super::super::simd::set_simd_part_override("flt", flt);
                super::super::simd::set_simd_part_override("dct", dct);
                // 返回 (仅重建 decode_vp8, 全链 decode+rgba)。
                (
                    median(
                        || {
                            let _yuv = decode_vp8(c.bitstream.unwrap(), w, h).unwrap();
                        },
                        rounds,
                    ),
                    median(
                        || {
                            let mut yuv = decode_vp8(c.bitstream.unwrap(), w, h).unwrap();
                            let _rgba = yuv_to_rgba_fancy(&mut yuv);
                        },
                        rounds,
                    ),
                )
            };
            let (rec_all, full_all) = run(Some(true), Some(true), Some(true));
            let (rec_pred_off, full_pred_off) = run(Some(false), Some(true), Some(true));
            let (rec_flt_off, full_flt_off) = run(Some(true), Some(false), Some(true));
            let (rec_dct_off, full_dct_off) = run(Some(true), Some(true), Some(false));
            let (rec_scalar, full_scalar) = run(Some(false), Some(false), Some(false));
            // 复位（探针结束不污染同进程其它测试）。
            super::super::simd::set_simd_part_override("pred", None);
            super::super::simd::set_simd_part_override("flt", None);
            super::super::simd::set_simd_part_override("dct", None);
            // 以「仅重建」为主标尺（D1/D2 影响的是帧重建；yuv→rgba 恒定但大图分配噪声大）。
            eprintln!(
                "WEBP_SIMD_D1 {name} {}x{} rec_before={rec_pred_off:.3}ms rec_after={rec_all:.3}ms ratio={:.3} \
                 full_before={full_pred_off:.3}ms full_after={full_all:.3}ms ratio={:.3}",
                w, h, rec_all / rec_pred_off, full_all / full_pred_off
            );
            eprintln!(
                "WEBP_SIMD_D2 {name} {}x{} rec_before={rec_flt_off:.3}ms rec_after={rec_all:.3}ms ratio={:.3} \
                 full_before={full_flt_off:.3}ms full_after={full_all:.3}ms ratio={:.3}",
                w, h, rec_all / rec_flt_off, full_all / full_flt_off
            );
            eprintln!(
                "WEBP_SIMD_D3 {name} {}x{} rec_before={rec_dct_off:.3}ms rec_after={rec_all:.3}ms ratio={:.3} \
                 full_before={full_dct_off:.3}ms full_after={full_all:.3}ms ratio={:.3}",
                w, h, rec_all / rec_dct_off, full_all / full_dct_off
            );
            eprintln!(
                "WEBP_SIMD_TOTAL {name} {}x{} rec_scalar={rec_scalar:.3} rec_all={rec_all:.3} ratio={:.3} \
                 full_scalar={full_scalar:.3} full_all={full_all:.3} ratio={:.3}",
                w, h, rec_all / rec_scalar, full_all / full_scalar
            );
        }

        // 内核级微基准：直接以分派包装循环调用，量化每个内核 SIMD vs 标量（脱离解码器噪声）。
        eprintln!("WEBP_SIMD_KERNELS start");
        let mut krng = Lcg(0xfedcba9876543210);
        let kpitch = 96usize;
        let mut kplane = vec![0u8; 96 * 96];
        for b in kplane.iter_mut() {
            *b = krng.byte();
        }
        let inc = |r: &mut Lcg| -> u8 { match r.next() % 6 { 0 => 0, 1 => 255, 2 => 128, _ => r.byte() } };
        // D1 4×4 微基准。
        {
            let mut above = [0u8; 9];
            let mode_cycle = [0u8, 1, 2, 3, 4, 5, 6, 7, 8, 9];
            let mut sub = |mode: u8, iters: usize, simd: bool| -> f64 {
                super::super::simd::set_simd_part_override("pred", Some(simd));
                for b in kplane.iter_mut() {
                    *b = inc(&mut krng);
                }
                above = [0u8; 9];
                for i in 0..9 {
                    above[i] = inc(&mut krng);
                }
                median(
                    || {
                        for i in 0..iters {
                            let m = mode_cycle[(i + mode as usize) % 10];
                            let r0 = 4 + (i % 20);
                            let c0 = 4 + (i / 20) % 20;
                            super::super::simd::predict_subblock(&mut kplane, kpitch, r0, c0, m, &above);
                        }
                    },
                    5,
                )
            };
            let it = 120_000usize;
            let st = sub(0, it, false);
            let sv = sub(0, it, true);
            eprintln!("WEBP_SIMD_KERNEL subblock4x4 iters={it} scalar={st:.3}ms simd={sv:.3}ms ratio={:.3}", sv / st);
        }
        // D1 16×16 / 8×8 宏基准。
        {
            let mut large = |size: usize, mode: u8, iters: usize, simd: bool| -> f64 {
                super::super::simd::set_simd_part_override("pred", Some(simd));
                for b in kplane.iter_mut() {
                    *b = inc(&mut krng);
                }
                let above: Vec<u8> = (0..=size).map(|_| inc(&mut krng)).collect();
                median(
                    || {
                        for i in 0..iters {
                            super::super::simd::predict_large(
                                &mut kplane, kpitch, 4 + (i % 40), 4 + (i % 40), size, mode, true, true, &above,
                            );
                        }
                    },
                    5,
                )
            };
            let it = 40_000usize;
            for size in [16usize, 8] {
                for mode in 0u8..4 {
                    let st = large(size, mode, it, false);
                    let sv = large(size, mode, it, true);
                    eprintln!(
                        "WEBP_SIMD_KERNEL predict_large size={size} mode={mode} iters={it} scalar={st:.3}ms simd={sv:.3}ms ratio={:.3}",
                        sv / st
                    );
                }
            }
        }
        // D2 滤波微基准：Simple / Inner / Macroblock × 竖/横 × 16/8 lane。
        {
            let mut fe = |kind: u8, vertical: bool, length: usize, iters: usize, simd: bool| -> f64 {
                super::super::simd::set_simd_part_override("flt", Some(simd));
                for b in kplane.iter_mut() {
                    *b = inc(&mut krng);
                }
                let (simple, mbe) = match kind {
                    0 => (true, true),
                    1 => (false, false),
                    _ => (false, true),
                };
                let (interior, edge, hev) = if simple {
                    (1i32, 18i32, 2i32)
                } else if mbe {
                    (7, 30, 1)
                } else {
                    (5, 24, 1)
                };
                median(
                    || {
                        for i in 0..iters {
                            let x = 8 + (i % 16);
                            let y = 8 + (i / 16) % 32;
                            super::filter_edge(
                                &mut kplane, kpitch, x, y, length, vertical, mbe, simple, interior, edge, hev,
                            );
                        }
                    },
                    5,
                )
            };
            let it = 60_000usize;
            for kind in 0u8..3 {
                for vertical in [true, false] {
                    for length in [16usize, 8] {
                        let st = fe(kind, vertical, length, it, false);
                        let sv = fe(kind, vertical, length, it, true);
                        eprintln!(
                            "WEBP_SIMD_KERNEL filter kind={} vertical={} len={} iters={it} scalar={st:.3}ms simd={sv:.3}ms ratio={:.3}",
                            kind, vertical, length, sv / st
                        );
                    }
                }
            }
        }
        super::super::simd::set_simd_part_override("pred", None);
        super::super::simd::set_simd_part_override("flt", None);
    }
