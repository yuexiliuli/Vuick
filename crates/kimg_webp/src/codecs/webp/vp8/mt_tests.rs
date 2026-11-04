    use super::*;
    use super::super::container;

    /// 进程内 env 修改锁（pn gsimd parity 同款：并发测试下串行化 env 翻转，防联动交错）。
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// 按 `threads`（`KIMG_WEBP_THREADS` 值）解码静态有损整帧：返回 YUV + RGBA。
    fn decode_with_threads(data: &[u8], threads: &str) -> Option<(Vp8Yuv, Vec<u8>)> {
        unsafe {
            std::env::set_var("KIMG_WEBP_THREADS", threads);
        }
        let c = container::parse(data).ok()?;
        let mut yuv = decode_vp8(c.bitstream?, c.width, c.height).ok()?;
        let rgba = yuv_to_rgba_fancy(&mut yuv);
        Some((yuv, rgba))
    }

    /// 硬闸：threads=2（双带并行）与 threads=0（现串行）输出**逐字节一致**（YUV+RGBA 全平面）。
    /// 覆盖全部静态有损 fixture（外部语料存在性驱动；缺则 SKIP），含宽图（gen28 2400×3600）、
    /// 奇数高/矮图、单/多宏块行等边界——并行路径的每一字节都与串行硬性相同。
    #[test]
    fn threads2_matches_serial_byte_exact() {
        let _g = ENV_LOCK.lock().unwrap();
        let mut checked = 0usize;
        let mut names: Vec<String> = webp_fixtures_support::webp_fixture_names();
        names.sort();
        for name in names {
            let Some(p) = webp_fixtures_support::webp_fixture_path(&name) else {
                continue;
            };
            let data = std::fs::read(&p).expect("read fixture");
            let Ok(c) = container::parse(&data) else {
                continue;
            };
            if c.family != container::Family::StaticLossy
                && c.family != container::Family::LossyAlpha
            {
                continue;
            }
            let Some((serial_yuv, serial_rgba)) = decode_with_threads(&data, "0") else {
                eprintln!("SKIP {name} 串行解码失败（外部语料不可解）");
                continue;
            };
            let Some((para_yuv, para_rgba)) = decode_with_threads(&data, "2") else {
                panic!("{name} threads=2 解码失败（threads=0 已成功）");
            };
            assert_eq!(
                serial_yuv,
                para_yuv,
                "{name}: threads=2 与 threads=0 YUV 平面逐字节一致"
            );
            assert_eq!(
                serial_rgba,
                para_rgba,
                "{name}: threads=2 与 threads=0 RGBA 逐字节一致"
            );
            checked += 1;
        }
        unsafe {
            std::env::remove_var("KIMG_WEBP_THREADS");
        }
        assert!(
            checked >= 3,
            "应至少对拍 3 个有损 fixture（当前 {checked}；缺外部语料）"
        );
        eprintln!("WEBP_MT_PARITY_OK\t{checked}");
    }

    /// 大图双线程滤波中位墙钟（release 手动跑 `-- --ignored`）：输出
    /// `WEBP_MT <name> <w>x<h> before=..ms after=..ms ratio=..`，供「是否默认开」决策（K53；
    /// before=threads=0 串行中位，after=threads=2 中位，ratio=before/after）。
    #[test]
    #[ignore = "手动运行"]
    fn lossy_mt_probe() {
        use std::time::Instant;
        let big = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("target")
            .join("imgbench-corpus")
            .join("WEBP")
            .join("gen28-photo-lossy-q75.webp");
        let (name, data) = if big.is_file() {
            (
                "gen28-photo-lossy-q75.webp".to_string(),
                std::fs::read(&big).unwrap(),
            )
        } else {
            let p = webp_fixtures_support::webp_fixture_path("testsrc2-320x240-q75.webp")
                .unwrap_or_else(|| panic!("夹具缺失（外部语料）：testsrc2-320x240-q75.webp"));
            ("testsrc2-320x240-q75.webp".to_string(), std::fs::read(&p).unwrap())
        };
        let c = container::parse(&data).unwrap();
        let (w, h) = (c.width, c.height);
        let _g = ENV_LOCK.lock().unwrap();
        let rounds = 9usize;
        let dec = || {
            let _ = decode_vp8(c.bitstream.unwrap(), w, h).unwrap();
        };
        let median = |v: &mut Vec<f64>| {
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            v[v.len() / 2]
        };
        unsafe {
            std::env::set_var("KIMG_WEBP_THREADS", "0");
        }
        for _ in 0..2 {
            dec();
        }
        let mut t0 = Vec::new();
        for _ in 0..rounds {
            let t = Instant::now();
            dec();
            t0.push(t.elapsed().as_secs_f64() * 1e3);
        }
        unsafe {
            std::env::set_var("KIMG_WEBP_THREADS", "2");
        }
        for _ in 0..2 {
            dec();
        }
        let mut t2 = Vec::new();
        for _ in 0..rounds {
            let t = Instant::now();
            dec();
            t2.push(t.elapsed().as_secs_f64() * 1e3);
        }
        unsafe {
            std::env::remove_var("KIMG_WEBP_THREADS");
        }
        let before = median(&mut t0);
        let after = median(&mut t2);
        // 双线程与串行逐字节一致（探针内复核；若不一致抛错，WEBP_MT 前中止）。
        unsafe {
            std::env::set_var("KIMG_WEBP_THREADS", "0");
        }
        let sy = decode_vp8(c.bitstream.unwrap(), w, h).unwrap();
        unsafe {
            std::env::set_var("KIMG_WEBP_THREADS", "2");
        }
        let py = decode_vp8(c.bitstream.unwrap(), w, h).unwrap();
        unsafe {
            std::env::remove_var("KIMG_WEBP_THREADS");
        }
        assert_eq!(sy, py, "WEBP_MT 探针：双线程与串行 YUV 逐字节一致");
        eprintln!(
            "WEBP_MT {name} {w}x{h} before={before:.3}ms after={after:.3}ms ratio={:.3}",
            before / after
        );
    }

    /// RC1 硬闸「bmi ≡ baseline」：同一宏体（`parse_residuals_impl!` /
    /// `read_macroblock_modes_impl!`）展开的快道（lzcnt/bmi）与基线（bsr）逐字节一致——双路径仅
    /// 指令选择不同，字节必须恒等。对有损 + lossy-alpha fixture 双态对拍三路：串行·基线 vs
    /// 串行·BMI vs 双线程·BMI 全等。CPU 无 bmi2 → SKIP（快道不可合法执行，本闸自动退化为
    /// 基线自对，不崩）。配合既有 MT parity（默认 ent=auto→bmi）即把「双路径字节恒等」钉全
    /// （AGENTS §5：缺夹具 SKIP 是常态不是回归）。
    #[test]
    fn ent_bmi_matches_baseline_byte_exact() {
        let _g = ENV_LOCK.lock().unwrap();
        // bmi2 可用性直接问 CPU（绕过 env 部件开关；覆盖 `set_simd_part_override` 不越权）。
        let bmi2 = {
            #[cfg(target_arch = "x86_64")]
            {
                std::arch::is_x86_feature_detected!("bmi2")
            }
            #[cfg(not(target_arch = "x86_64"))]
            {
                false
            }
        };
        let mut checked = 0usize;
        let mut names: Vec<String> = webp_fixtures_support::webp_fixture_names();
        names.sort();
        for name in names {
            let Some(p) = webp_fixtures_support::webp_fixture_path(&name) else {
                continue;
            };
            let data = std::fs::read(&p).expect("read fixture");
            let Ok(c) = container::parse(&data) else {
                continue;
            };
            if c.family != container::Family::StaticLossy
                && c.family != container::Family::LossyAlpha
            {
                continue;
            }
            // 基线（串行）：强制 ent 走 `*_base`（bsr 指令选择）。
            super::super::simd::set_simd_part_override("ent", Some(false));
            let Some(base) = decode_with_threads(&data, "0") else {
                eprintln!("SKIP {name} 基线串行解码失败（外部语料不可解）");
                continue;
            };
            if !bmi2 {
                eprintln!("SKIP {name}（CPU 无 bmi2，快道不可用，仅基线自对）");
                continue;
            }
            // 快道（串行 + 双线程）：强制 ent 走 `*_bmi`（lzcnt/bmi 指令选择）。
            super::super::simd::set_simd_part_override("ent", Some(true));
            let Some(bmi_serial) = decode_with_threads(&data, "0") else {
                panic!("{name} 快道串行解码失败（基线已成功）");
            };
            let Some(bmi_para) = decode_with_threads(&data, "2") else {
                panic!("{name} 快道 threads=2 解码失败（基线已成功）");
            };
            assert_eq!(
                base.0, bmi_serial.0,
                "{name}: entropy BMI vs baseline 串行 YUV/RGBA 逐字节"
            );
            assert_eq!(
                base.1, bmi_serial.1,
                "{name}: entropy BMI vs baseline 串行 RGBA 逐字节"
            );
            assert_eq!(
                bmi_serial.0, bmi_para.0,
                "{name}: entropy BMI 串行 vs 双线程 YUV/RGBA 逐字节"
            );
            assert_eq!(
                bmi_serial.1, bmi_para.1,
                "{name}: entropy BMI 串行 vs 双线程 RGBA 逐字节"
            );
            checked += 1;
        }
        super::super::simd::set_simd_part_override("ent", None);
        unsafe {
            std::env::remove_var("KIMG_WEBP_THREADS");
        }
        assert!(
            checked >= 3 || !bmi2,
            "应至少对拍 3 个有损 fixture（当前 {checked}；缺外部语料；无 bmi2 时退化为基线自对，不判失败）"
        );
        eprintln!("WEBP_ENT_BMI_PARITY_OK\t{checked}\t(serial_bmi≡baseline ∧ MT_bmi≡serial_bmi)");
    }

    /// RC3 硬闸「branch ≡ cmov」：同宏体（`tokens`/`read_category_3_to_6`，bmi/base 共享）的
    /// 预测分支形（ZERO/G5/CAT8-10，默认开）与 cmov 全算形（`set_ent_branch_override(false)`）
    /// 输出**逐字节一致**——纯控制流重构，双形仅分支/codegen 不同。对有损 + lossy-alpha fixture
    /// 三路对拍：串行·cmov vs 串行·branch vs 双线程·branch 全等（同 `ent_bmi_matches_baseline_byte_exact`
    /// 姿势；生产默认熵路径 auto→bmi，RC1 已独立保证 bmi≡base，故本闸不重复翻 ent 双模）。
    /// 缺夹具 SKIP 是常态不是回归（AGENTS §5）。
    #[test]
    fn ent_branch_matches_cmov_byte_exact() {
        let _g = ENV_LOCK.lock().unwrap();
        let mut checked = 0usize;
        let mut names: Vec<String> = webp_fixtures_support::webp_fixture_names();
        names.sort();
        for name in names {
            let Some(p) = webp_fixtures_support::webp_fixture_path(&name) else {
                continue;
            };
            let data = std::fs::read(&p).expect("read fixture");
            let Ok(c) = container::parse(&data) else {
                continue;
            };
            if c.family != container::Family::StaticLossy
                && c.family != container::Family::LossyAlpha
            {
                continue;
            }
            // cmov 全算（对照侧）：分支形强制关。
            super::super::simd::set_ent_branch_override(Some(false));
            let Some(cmov) = decode_with_threads(&data, "0") else {
                eprintln!("SKIP {name} 基线串行解码失败（外部语料不可解）");
                continue;
            };
            // 预测分支形（默认侧）：串行 + 双线程。
            super::super::simd::set_ent_branch_override(Some(true));
            let Some(branch_serial) = decode_with_threads(&data, "0") else {
                panic!("{name} 分支形串行解码失败（基线已成功）");
            };
            let Some(branch_para) = decode_with_threads(&data, "2") else {
                panic!("{name} 分支形 threads=2 解码失败（基线已成功）");
            };
            assert_eq!(
                cmov.0, branch_serial.0,
                "{name}: 熵分支形 vs cmov 串行 YUV/RGBA 逐字节"
            );
            assert_eq!(
                cmov.1, branch_serial.1,
                "{name}: 熵分支形 vs cmov 串行 RGBA 逐字节"
            );
            assert_eq!(
                branch_serial.0, branch_para.0,
                "{name}: 熵分支形串行 vs 双线程 YUV/RGBA 逐字节"
            );
            assert_eq!(
                branch_serial.1, branch_para.1,
                "{name}: 熵分支形串行 vs 双线程 RGBA 逐字节"
            );
            checked += 1;
        }
        super::super::simd::set_ent_branch_override(None);
        unsafe {
            std::env::remove_var("KIMG_WEBP_THREADS");
        }
        assert!(
            checked >= 3,
            "应至少对拍 3 个有损 fixture（当前 {checked}；缺外部语料）"
        );
        eprintln!(
            "WEBP_ENT_BRANCH_PARITY_OK\t{checked}\t(serial_cmov≡branch ∧ MT_branch≡serial_branch)"
        );
    }

    /// RC3 A/B 同窗对拍（release 手动跑 `-- --ignored`）：进程内 override 只翻「分支形开/关」，
    /// 同二进制同布局——隔离的恰是本改动的控制流差异（比跨构建对比更公平）。
    /// 输出 tri：`entropy_stage`（stage_clock 熵段累计，含 per-MB Instant 插桩，看相对变化）、
    /// `decode`（`decode_vp8` 整帧墙钟，无插桩，主标尺）、串行与双线程两档。
    /// 裁决（K55/K60）：熵墙钟或 71 decode 净提升 ≥8%（噪声带外）→ 默认开；≤0 → 回退全部 RC3；
    /// 0~8% → 报真实数字由主 agent 定。
    #[test]
    #[ignore = "手动运行（release）"]
    fn ent_branch_ab_probe() {
        use std::time::Instant;
        let corpus = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("target")
            .join("imgbench-corpus")
            .join("WEBP");
        // 4K 高熵组（ZERO/G5 触发量大，双路径收益主战场）+ 纯 VP8 photo 各档（低熵，须无回归）。
        let files: [&str; 6] = [
            "syn-noise-3840x2160.webp",
            "syn-mandel-3840x2160.webp",
            "syn-testsrc2-3840x2160.webp",
            "strip-h6-lossy.webp",
            "img_v3_0216r_0836ba29-7fe2-4c91-b768-77565ad841eg_MIDDLE_WEBP.webp",
            "try-it-in-slintpad.webp",
        ];
        let median = |v: &mut Vec<f64>| {
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            v[v.len() / 2]
        };
        let measure =
            |data: &[u8], w: u32, h: u32, threads: &str, branch: bool, rounds: usize, clock: bool| -> (f64, f64) {
                super::super::simd::set_ent_branch_override(Some(branch));
                unsafe {
                    std::env::set_var("KIMG_WEBP_THREADS", threads);
                }
                let mut wall = Vec::new();
                let mut ent = Vec::new();
                for _ in 0..2 {
                    let _ = decode_vp8(data, w, h).unwrap();
                }
                for _ in 0..rounds {
                    if clock {
                        stage_clock::reset();
                        stage_clock::set(true);
                    }
                    let t = Instant::now();
                    let _ = decode_vp8(data, w, h).unwrap();
                    wall.push(t.elapsed().as_secs_f64() * 1e3);
                    let e = if clock {
                        let (ev, _, _, _) = stage_clock::totals();
                        stage_clock::set(false);
                        ev
                    } else {
                        0.0
                    };
                    ent.push(e);
                }
                (median(&mut wall), median(&mut ent))
            };
        let _g = ENV_LOCK.lock().unwrap();
        for name in files {
            let p = corpus.join(name);
            if !p.is_file() {
                eprintln!("SKIP {name}（外部语料缺失）");
                continue;
            }
            let data = std::fs::read(&p).unwrap();
            let Ok(c) = container::parse(&data) else {
                eprintln!("SKIP {name}（container 不可解析）");
                continue;
            };
            if c.family != container::Family::StaticLossy {
                eprintln!("SKIP {name}（family != StaticLossy，非本双路径测试对象）");
                continue;
            }
            let Some(bs) = c.bitstream else {
                eprintln!("SKIP {name}（bitstream=None）");
                continue;
            };
            let (w, h) = (c.width, c.height);
            if decode_vp8(bs, w, h).is_err() {
                eprintln!("SKIP {name}（decode_vp8 不可解，排除非本测试对象）");
                continue;
            }
            let rounds = if w * h >= 3840 * 2160 { 7 } else { 11 };
            for threads in ["0", "2"] {
                // 同窗交错 A/B（K68：单窗 off→on 顺序块会被热迁移/频率漂移污染）：
                // 每轮 off/on 交错采样，两边同窗均完成；`measure` 只做单侧采样。
                super::super::simd::set_ent_branch_override(Some(false));
                unsafe {
                    std::env::set_var("KIMG_WEBP_THREADS", threads);
                }
                let mut w_off = Vec::new();
                let mut e_off = Vec::new();
                let mut w_on = Vec::new();
                let mut e_on = Vec::new();
                let run = |branch: bool, wall: &mut Vec<f64>, ent: &mut Vec<f64>| {
                    super::super::simd::set_ent_branch_override(Some(branch));
                    for _ in 0..1 {
                        let _ = decode_vp8(bs, w, h).unwrap();
                    }
                    stage_clock::reset();
                    stage_clock::set(true);
                    let t = Instant::now();
                    let _ = decode_vp8(bs, w, h).unwrap();
                    wall.push(t.elapsed().as_secs_f64() * 1e3);
                    let (ev, _, _, _) = stage_clock::totals();
                    ent.push(ev);
                    stage_clock::set(false);
                };
                for _ in 0..2 {
                    run(false, &mut w_off, &mut e_off);
                    run(true, &mut w_on, &mut e_on);
                }
                for _ in 0..rounds {
                    run(false, &mut w_off, &mut e_off);
                    run(true, &mut w_on, &mut e_on);
                }
                super::super::simd::set_ent_branch_override(None);
                let (w_off_m, e_off_m) = (median(&mut w_off), median(&mut e_off));
                let (w_on_m, e_on_m) = (median(&mut w_on), median(&mut e_on));
                eprintln!(
                    "WEBP_ENT_BRANCH_AB {name} {w}x{h} thr={threads}: decode {}ms vs {}ms (ratio {:.3}) | \
                     ent_stage {}ms vs {}ms (ratio {:.3})  [interleaved; off=cmov on=branch]",
                    w_off_m,
                    w_on_m,
                    w_off_m / w_on_m,
                    e_off_m,
                    e_on_m,
                    e_off_m / e_on_m
                );
            }
        }
        super::super::simd::set_ent_branch_override(None);
        // 整 lossy 集汇总（74//71 有损 fixture：每只 decode_vp8 一次，off/on 各一遍，墙钟累加；
        // 覆盖率反映「整体 lossy(71)」验收口径，大图（4K noise）主导；此处 clean 墙钟（无插桩）。
        for threads in ["0", "2"] {
            let mut off_sum = 0.0f64;
            let mut on_sum = 0.0f64;
            let mut n = 0usize;
            for name in webp_fixtures_support::webp_fixture_names() {
                let Some(p) = webp_fixtures_support::webp_fixture_path(&name) else {
                    continue;
                };
                let data = std::fs::read(&p).expect("read fixture");
                let Ok(c) = container::parse(&data) else {
                    continue;
                };
                if c.family != container::Family::StaticLossy {
                    continue;
                }
                let Some(bs) = c.bitstream else {
                    continue;
                };
                let (w, h) = (c.width, c.height);
                if decode_vp8(bs, w, h).is_err() {
                    continue; // 非本解码器可解（如动画帧包），不在 lossy(71) 口径内
                }
                let (wo, _) = measure(bs, w, h, threads, false, 1, false);
                let (won, _) = measure(bs, w, h, threads, true, 1, false);
                off_sum += wo;
                on_sum += won;
                n += 1;
            }
            eprintln!(
                "WEBP_ENT_BRANCH_CORPUS_SUM thr={threads} fixtures={n}: decode {}ms vs {}ms (ratio {:.3})",
                off_sum,
                on_sum,
                off_sum / on_sum
            );
        }
        unsafe {
            std::env::remove_var("KIMG_WEBP_THREADS");
        }
    }

    /// RC1 A/B 同窗对拍（release 手动跑 `-- --ignored`）：进程内 override 只翻「熵走快道还是
    /// 基线」，同二进制同布局——隔离的恰是本改动的纯指令选择差异（比跨构建对比更公平）。
    /// 输出 tri：`entropy_stage`（stage_clock 熵段累计，含 per-MB Instant 插桩，看相对变化）、
    /// `decode`（`decode_vp8` 整帧墙钟，无插桩，主标尺）、串行与双线程两档。
    #[test]
    #[ignore = "手动运行（release）"]
    fn ent_bmi_ab_probe() {
        use std::time::Instant;
        let corpus = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("target")
            .join("imgbench-corpus")
            .join("WEBP");
        // 4K 高熵组（renorm 触发 65-85%，双路径收益主战场）+ 纯 VP8 photo 各档（低熵，须无回归）。
        let files: [&str; 6] = [
            "syn-noise-3840x2160.webp",
            "syn-mandel-3840x2160.webp",
            "syn-testsrc2-3840x2160.webp",
            "strip-h6-lossy.webp",
            "img_v3_0216r_0836ba29-7fe2-4c91-b768-77565ad841eg_MIDDLE_WEBP.webp",
            "try-it-in-slintpad.webp",
        ];
        let median = |v: &mut Vec<f64>| {
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            v[v.len() / 2]
        };
        let measure = |data: &[u8], w: u32, h: u32, threads: &str, bmi: bool, rounds: usize, clock: bool| -> (f64, f64) {
            super::super::simd::set_simd_part_override("ent", Some(bmi));
            unsafe {
                std::env::set_var("KIMG_WEBP_THREADS", threads);
            }
            let mut wall = Vec::new();
            let mut ent = Vec::new();
            for _ in 0..2 {
                let _ = decode_vp8(data, w, h).unwrap();
            }
            for _ in 0..rounds {
                if clock {
                    stage_clock::reset();
                    stage_clock::set(true);
                }
                let t = Instant::now();
                let _ = decode_vp8(data, w, h).unwrap();
                wall.push(t.elapsed().as_secs_f64() * 1e3);
                let e = if clock {
                    let (ev, _, _, _) = stage_clock::totals();
                    stage_clock::set(false);
                    ev
                } else {
                    0.0
                };
                ent.push(e);
            }
            (median(&mut wall), median(&mut ent))
        };
        let _g = ENV_LOCK.lock().unwrap();
        for name in files {
            let p = corpus.join(name);
            if !p.is_file() {
                eprintln!("SKIP {name}（外部语料缺失）");
                continue;
            }
            let data = std::fs::read(&p).unwrap();
            let Ok(c) = container::parse(&data) else {
                eprintln!("SKIP {name}（container 不可解析）");
                continue;
            };
            if c.family != container::Family::StaticLossy {
                eprintln!("SKIP {name}（family != StaticLossy，非本双路径测试对象）");
                continue;
            }
            let Some(bs) = c.bitstream else {
                eprintln!("SKIP {name}（bitstream=None）");
                continue;
            };
            let (w, h) = (c.width, c.height);
            if decode_vp8(bs, w, h).is_err() {
                eprintln!("SKIP {name}（decode_vp8 不可解，排除非本测试对象）");
                continue;
            }
            let rounds = if w * h >= 3840 * 2160 { 7 } else { 11 };
            for threads in ["0", "2"] {
                // 同窗交错 A/B（K68：单窗 off→on 顺序块会被热迁移/频率漂移污染）：
                // 每轮 off/on 交错采样，两边同窗均完成；`measure` 只做单侧采样。
                super::super::simd::set_simd_part_override("ent", Some(false));
                unsafe {
                    std::env::set_var("KIMG_WEBP_THREADS", threads);
                }
                let mut w_off = Vec::new();
                let mut e_off = Vec::new();
                let mut w_on = Vec::new();
                let mut e_on = Vec::new();
                let run = |bmi: bool, wall: &mut Vec<f64>, ent: &mut Vec<f64>| {
                    super::super::simd::set_simd_part_override("ent", Some(bmi));
                    for _ in 0..1 {
                        let _ = decode_vp8(bs, w, h).unwrap();
                    }
                    stage_clock::reset();
                    stage_clock::set(true);
                    let t = Instant::now();
                    let _ = decode_vp8(bs, w, h).unwrap();
                    wall.push(t.elapsed().as_secs_f64() * 1e3);
                    let (ev, _, _, _) = stage_clock::totals();
                    ent.push(ev);
                    stage_clock::set(false);
                };
                for _ in 0..2 {
                    run(false, &mut w_off, &mut e_off);
                    run(true, &mut w_on, &mut e_on);
                }
                for _ in 0..rounds {
                    run(false, &mut w_off, &mut e_off);
                    run(true, &mut w_on, &mut e_on);
                }
                super::super::simd::set_simd_part_override("ent", None);
                let (w_off_m, e_off_m) = (median(&mut w_off), median(&mut e_off));
                let (w_on_m, e_on_m) = (median(&mut w_on), median(&mut e_on));
                eprintln!(
                    "WEBP_ENT_AB {name} {w}x{h} thr={threads}: decode {}ms vs {}ms (ratio {:.3}) | \
                     ent_stage {}ms vs {}ms (ratio {:.3})  [interleaved]",
                    w_off_m,
                    w_on_m,
                    w_off_m / w_on_m,
                    e_off_m,
                    e_on_m,
                    e_off_m / e_on_m
                );
            }
        }
        super::super::simd::set_simd_part_override("ent", None);
        // 整 lossy 集汇总（74//71 有损 fixture：每只 decode_vp8 一次，off/on 各一遍，墙钟累加；
        // 覆盖率反映「整体 lossy(71)」验收口径，大图（4K noise）主导；此处 clean 墙钟（无插桩）。
        for threads in ["0", "2"] {
            let mut off_sum = 0.0f64;
            let mut on_sum = 0.0f64;
            let mut n = 0usize;
            for name in webp_fixtures_support::webp_fixture_names() {
                let Some(p) = webp_fixtures_support::webp_fixture_path(&name) else {
                    continue;
                };
                let data = std::fs::read(&p).expect("read fixture");
                let Ok(c) = container::parse(&data) else {
                    continue;
                };
                if c.family != container::Family::StaticLossy {
                    continue;
                }
                let Some(bs) = c.bitstream else {
                    continue;
                };
                let (w, h) = (c.width, c.height);
                if decode_vp8(bs, w, h).is_err() {
                    continue; // 非本解码器可解（如动画帧包），不在 lossy(71) 口径内
                }
                let (wo, _) = measure(bs, w, h, threads, false, 1, false);
                let (won, _) = measure(bs, w, h, threads, true, 1, false);
                off_sum += wo;
                on_sum += won;
                n += 1;
            }
            eprintln!(
                "WEBP_ENT_CORPUS_SUM thr={threads} fixtures={n}: decode {}ms vs {}ms (ratio {:.3})",
                off_sum,
                on_sum,
                off_sum / on_sum
            );
        }
        unsafe {
            std::env::remove_var("KIMG_WEBP_THREADS");
        }
    }

    /// RC2 A/B：熵快道（ent 强制 ON）上 `KIMG_WEBP_ENT_RENORM_SKIP` 开（默认，range≥128 跳过
    /// lzcnt+shl）vs 关（无条件 renorm）——lzcnt=1c 后「跳过」收益变化，本探针重新对拍。
    /// 进程内 override（`set_renorm_skip_override`）交错同窗，测 noise/testsrc2/photo。
    /// 【评审实测结论（2026-11-03）】净口径「持平偏正」：noise 反而嫌 skipped（decode
    /// ratio~0.96，无 skip 略快），testsrc2/photo 持平/略负——即 skip 分支整体不带来可测收益，
    /// 亦无系统性伤害。因净口径不显著，保留默认开（既有语义）、不做外提/移除（证据驱动不硬动）。
    #[test]
    #[ignore = "手动运行（release；RC2）"]
    fn ent_renorm_skip_ab_probe() {
        use std::time::Instant;
        let corpus = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("target")
            .join("imgbench-corpus")
            .join("WEBP");
        let files: [&str; 3] = [
            "syn-noise-3840x2160.webp",
            "syn-testsrc2-3840x2160.webp",
            "strip-h6-lossy.webp",
        ];
        let median = |v: &mut Vec<f64>| {
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            v[v.len() / 2]
        };
        let _g = ENV_LOCK.lock().unwrap();
        super::super::simd::set_simd_part_override("ent", Some(true));
        unsafe {
            std::env::set_var("KIMG_WEBP_THREADS", "0");
        }
        for name in files {
            let p = corpus.join(name);
            if !p.is_file() {
                eprintln!("SKIP {name}（外部语料缺失）");
                continue;
            }
            let data = std::fs::read(&p).unwrap();
            let Ok(c) = container::parse(&data) else {
                continue;
            };
            let Some(bs) = c.bitstream else {
                continue;
            };
            let (w, h) = (c.width, c.height);
            let rounds = if w * h >= 3840 * 2160 { 7 } else { 11 };
            let run = |skip: bool, wall: &mut Vec<f64>, ent: &mut Vec<f64>| {
                set_renorm_skip_override(Some(skip));
                for _ in 0..1 {
                    let _ = decode_vp8(bs, w, h).unwrap();
                }
                stage_clock::reset();
                stage_clock::set(true);
                let t = Instant::now();
                let _ = decode_vp8(bs, w, h).unwrap();
                wall.push(t.elapsed().as_secs_f64() * 1e3);
                let (ev, _, _, _) = stage_clock::totals();
                ent.push(ev);
                stage_clock::set(false);
            };
            let (mut w_on, mut e_on, mut w_off, mut e_off) =
                (Vec::new(), Vec::new(), Vec::new(), Vec::new());
            for _ in 0..2 {
                run(true, &mut w_on, &mut e_on);
                run(false, &mut w_off, &mut e_off);
            }
            for _ in 0..rounds {
                run(true, &mut w_on, &mut e_on);
                run(false, &mut w_off, &mut e_off);
            }
            eprintln!(
                "WEBP_ENT_RENORM_AB {name} {w}x{h} thr=0: decode(skipOn) {}ms vs {}ms (ratio {:.3}) | \
                 ent_stage {}ms vs {}ms (ratio {:.3})  [skip=ON vs OFF, 快道]",
                median(&mut w_on),
                median(&mut w_off),
                median(&mut w_on) / median(&mut w_off),
                median(&mut e_on),
                median(&mut e_off),
                median(&mut e_on) / median(&mut e_off)
            );
        }
        set_renorm_skip_override(None);
        super::super::simd::set_simd_part_override("ent", None);
        unsafe {
            std::env::remove_var("KIMG_WEBP_THREADS");
        }
    }
