// ---------------------------------------------------------------------------
// 一致性单测：SIMD 路径与标量逐字节相同（bpp 覆盖 / 边界 / 伪随机）
// ---------------------------------------------------------------------------

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
        fn byte(&mut self) -> u8 {
            (self.next() >> 40) as u8
        }
    }

    /// 系数抽样：覆盖仅 DC（has_ac=false 语义）、全 AC、大/小量级、符号。
    fn coeff_i16(rng: &mut Lcg) -> i16 {
        match rng.next() % 12 {
            0 => 0,
            1 => 1,
            2 => -1,
            3 => 255,
            4 => -255,
            5 => 1024,
            6 => -1024,
            7 => 8192,
            8 => -8192,
            9 => 32767,
            10 => -32768,
            _ => (rng.next() as i32 as i16),
        }
    }

    #[test]
    fn inverse_dct_has_ac_shortcut_equivalence() {
        // 关键前提验证：has_ac=false（仅 DC 捷径）与 has_ac=true（全变换）对 [dc,0..0] 块
        // 逐字节一致 —— inverse_dct_add_two 标量回退恒传 has_ac=true 依赖此恒等式。
        let mut rng = Lcg(0x1111111122222222);
        let pitch = 32usize;
        let mut a = vec![0u8; 32 * 32];
        let mut b = vec![0u8; 32 * 32];
        let mut checked = 0usize;
        let mut dc_vals: Vec<i16> = vec![0, 1, -1, 15, -15, 255, -255, 2047, -2048,
            32767, -32768, 12345, -12345, 300, -300];
        for _ in 0..200 {
            dc_vals.push((rng.next() as u16) as i16);
        }
        for dc in dc_vals {
            let mut coeff = [0i16; 16];
            coeff[0] = dc;
            for px in a.iter_mut() {
                *px = rng.byte();
            }
            b.copy_from_slice(&a);
            super::super::vp8::inverse_dct_add(&coeff, &mut a, pitch, 4, 4, false);
            super::super::vp8::inverse_dct_add(&coeff, &mut b, pitch, 4, 4, true);
            assert_eq!(a, b, "has_ac 捷径不一致 dc={dc}");
            checked += 1;
        }
        eprintln!("IDCT_HAS_AC_SHORTCUT_EQUIV\t{checked}");
    }

    #[test]
    fn inverse_dct_two_simd_matches_scalar() {
        if !simd_on("sse2") {
            eprintln!("SIMD_TEST SKIP (无 sse2)");
            return;
        }
        set_simd_part_override("dct", Some(false)); // 先关，建立标量行为基线不依赖 override
        let mut rng = Lcg(0xa5a5a5a5deadbeef);
        let pitch = 64usize;
        let mut plane_simd = vec![0u8; 64 * 64];
        let mut plane_scalar = vec![0u8; 64 * 64];
        let mut checked = 0usize;
        for trial in 0..400 {
            let mut ca = [0i16; 16];
            let mut cb = [0i16; 16];
            for k in 0..16 {
                ca[k] = coeff_i16(&mut rng);
                cb[k] = coeff_i16(&mut rng);
            }
            // 偶发整块零 / 仅 DC（映射到标量 has_ac 分支差异的最大考验）。
            if trial % 37 == 0 {
                ca = [0i16; 16];
            }
            if trial % 53 == 0 {
                let mut dc_only = [0i16; 16];
                dc_only[0] = coeff_i16(&mut rng);
                cb = dc_only;
            }
            for px in plane_simd.iter_mut() {
                *px = rng.byte();
            }
            let (row0, col0) = (8 + (trial % 8), 8 + (trial % 8));
            plane_scalar.copy_from_slice(&plane_simd);
            // SIMD 双块路径。
            super::inverse_dct_add_two(&ca, &cb, &mut plane_simd, pitch, row0, col0);
            // 标量参考：逐块恒 has_ac=true（已证与既有短路语义一致）。
            super::super::vp8::inverse_dct_add(&ca, &mut plane_scalar, pitch, row0, col0, true);
            super::super::vp8::inverse_dct_add(&cb, &mut plane_scalar, pitch, row0, col0 + 4, true);
            assert_eq!(
                plane_simd, plane_scalar,
                "inverse_dct_two trial={trial} row0={row0} col0={col0}"
            );
            checked += 1;
        }
        set_simd_part_override("dct", None);
        eprintln!("INVERSE_DCT_TWO_SIMD_PARITY\t{checked}");
    }

    /// 单块 i4x4 1-block SSE2 核与标量逐字节一致：满幅 i16 系数 + 随机平面 + 随机 DC，
    /// 跨 0/±4095/±4096/±4097 边界，偶发全零 / 仅 DC 块（映射标量 has_ac=true 路径）。
    /// **K93 基线侧自检**：强制 `KIMG_WEBP_IDCT_I16` 关（i32 域 pass2）——满幅系数 i16 域会回绕
    /// 偏出，属 i32 基线的全输入域恒等域（i16 域可达性对拍见 `idct_add_one_reachable_domain_parity`）。
    #[test]
    fn inverse_dct_one_simd_matches_scalar() {
        if !simd_on("sse2") {
            eprintln!("SIMD_TEST SKIP idct_one (无 sse2)");
            return;
        }
        set_simd_part_override("dct", Some(true)); // 强制 SSE2 单块核
        set_idct_i16_override(Some(false)); // K93 基线侧：i32 pass2（全幅恒等）
        let boundaries: [i16; 16] = [
            0, 1, -1, 4095, -4095, 4096, -4096, 4097, -4097, 2047, -2048, 8192, -8192, 32767, -32768, 16384,
        ];
        let pitch = 64usize;
        let mut plane_simd = vec![0u8; 64 * 64];
        let mut plane_scalar = vec![0u8; 64 * 64];
        let mut checked = 0usize;
        for seed in [
            0x1357_9bdf_2468_ace0u64,
            0x0bad_cafe_f00d_3333,
            0xfeed_f00d_abcd_4444,
            0xdead_beef_1234_5555,
        ] {
            let mut rng = Lcg(seed);
            for trial in 0..300 {
                let mut c = [0i16; 16];
                for k in 0..16 {
                    // 满幅均匀随机（全 i16 值域），偶发注入跨 0/±4096 边界值。
                    c[k] = if rng.next() % 7 == 0 {
                        boundaries[(rng.next() as usize) % boundaries.len()]
                    } else {
                        (rng.next() as u16) as i16
                    };
                }
                // 偶发整块零 / 仅 DC。
                if trial % 41 == 0 {
                    c = [0i16; 16];
                }
                if trial % 61 == 0 {
                    let mut dc_only = [0i16; 16];
                    dc_only[0] = boundaries[(rng.next() as usize) % boundaries.len()];
                    c = dc_only;
                }
                for px in plane_simd.iter_mut() {
                    *px = rng.byte();
                }
                // 覆盖右缘/下缘：块 4x4 输出列 col0+1..col0+4、行 row0..row0+3，落 64x64
                // 平面内 col0 顶到 59（尾列 63）、row0 顶到 60（尾行 63），验证 4 字节读写不越界。
                let (row0, col0) = match trial % 23 {
                    0 => (60usize, 59usize),
                    1 => (60, 4 + (trial % 11)),
                    2 => (4 + (trial % 11), 59),
                    _ => (4 + (trial % 11), 4 + (trial % 11)),
                };
                plane_scalar.copy_from_slice(&plane_simd);
                super::inverse_dct_add_one(&c, &mut plane_simd, pitch, row0, col0);
                super::super::vp8::inverse_dct_add(&c, &mut plane_scalar, pitch, row0, col0, true);
                assert_eq!(
                    plane_simd, plane_scalar,
                    "idct_one seed=0x{seed:016x} trial={trial} row0={row0} col0={col0} c={c:?}"
                );
                checked += 1;
            }
        }
        set_simd_part_override("dct", None);
        set_idct_i16_override(None); // 恢复 i16 默认（可达域）
        eprintln!("INVERSE_DCT_ONE_SIMD_PARITY\t{checked}");
    }

    #[test]
    fn dc_add_two_simd_matches_scalar() {
        if !simd_on("sse2") {
            eprintln!("SIMD_TEST SKIP (无 sse2)");
            return;
        }
        set_simd_part_override("dct", Some(true)); // 强制 SSE2：DC 捷径内核（仅 DC 块）逐字节==标量
        let mut rng = Lcg(0x76543210fedcba98);
        let pitch = 64usize;
        let mut plane_simd = vec![0u8; 64 * 64];
        let mut plane_scalar = vec![0u8; 64 * 64];
        let mut checked = 0usize;
        for trial in 0..400 {
            let dc_a = coeff_i16(&mut rng);
            let dc_b = coeff_i16(&mut rng);
            for px in plane_simd.iter_mut() {
                *px = rng.byte();
            }
            let (row0, col0) = (8 + (trial % 8), 8 + (trial % 8));
            plane_scalar.copy_from_slice(&plane_simd);
            super::dc_add_two(dc_a, dc_b, &mut plane_simd, pitch, row0, col0);
            // 标量参考：逐块恒 has_ac=true（[dc,0..0] 完整变换残差恒 (dc+4)>>3）。
            let mut ca = [0i16; 16];
            let mut cb = [0i16; 16];
            ca[0] = dc_a;
            cb[0] = dc_b;
            super::super::vp8::inverse_dct_add(&ca, &mut plane_scalar, pitch, row0, col0, true);
            super::super::vp8::inverse_dct_add(&cb, &mut plane_scalar, pitch, row0, col0 + 4, true);
            assert_eq!(
                plane_simd, plane_scalar,
                "dc_add_two trial={trial} dc_a={dc_a} dc_b={dc_b} row0={row0} col0={col0}"
            );
            checked += 1;
        }
        set_simd_part_override("dct", None);
        eprintln!("DC_ADD_TWO_SIMD_PARITY\t{checked}");
    }

    #[test]
    fn dc_add_one_simd_matches_scalar() {
        if !simd_on("sse2") {
            eprintln!("SIMD_TEST SKIP (无 sse2)");
            return;
        }
        set_simd_part_override("dct", Some(true)); // 强制 SSE2：i4 纯 DC 捷径核（单块）逐字节==标量全核
        let mut rng = Lcg(0x0123456789abcdef);
        let pitch = 64usize;
        let mut plane_simd = vec![0u8; 64 * 64];
        let mut plane_scalar = vec![0u8; 64 * 64];
        let mut checked = 0usize;
        for trial in 0..400 {
            let dc = coeff_i16(&mut rng);
            for px in plane_simd.iter_mut() {
                *px = rng.byte();
            }
            let (row0, col0) = (8 + (trial % 8), 8 + (trial % 8));
            plane_scalar.copy_from_slice(&plane_simd);
            // SIMD 捷径核：`ac_mask` 位 0 分发的 i4 纯 DC 块（生产路径 `dc_add_one_forced`）。
            unsafe { super::dc_add_one_forced(dc, &mut plane_simd, pitch, row0, col0) };
            // 标量参考：对仅 DC 块走完整逆变换（[dc,0..0] 残差恒 (dc+4)>>3，与捷径同输出，
            // 同 `dc_add_two_simd_matches_scalar` 的既有等价性姿势）。
            let mut c = [0i16; 16];
            c[0] = dc;
            super::super::vp8::inverse_dct_add(&c, &mut plane_scalar, pitch, row0, col0, true);
            assert_eq!(
                plane_simd, plane_scalar,
                "dc_add_one trial={trial} dc={dc} row0={row0} col0={col0}"
            );
            checked += 1;
        }
        set_simd_part_override("dct", None);
        eprintln!("DC_ADD_ONE_SIMD_PARITY\t{checked}");
    }

    // -----------------------------------------------------------------------
    // VP8L 预测器 / 色变换：SIMD（块段分派）与标量逐字节一致
    // -----------------------------------------------------------------------

    /// 预测器标量参考（严格镜像 vp8l.rs `apply_predictor`：y==0 前缀 / x==0 顶行 /
    /// 末列 TR 卷绕 = 本行首像素）。
    fn predictor_reference(pixels: &mut [u32], width: usize, height: usize, bits: u32, data: &[u32]) {
        let block_w = (width + (1usize << bits) - 1) >> bits;
        use super::super::vp8l::{add_argb, BLACK};
        for y in 0..height {
            for x in 0..width {
                let index = y * width + x;
                let pred = if x == 0 && y == 0 {
                    BLACK
                } else if y == 0 {
                    pixels[index - 1]
                } else if x == 0 {
                    pixels[index - width]
                } else {
                    let left = pixels[index - 1];
                    let top = pixels[index - width];
                    let tl = pixels[index - width - 1];
                    let tr = if x == width - 1 {
                        pixels[index - x]
                    } else {
                        pixels[index - width + 1]
                    };
                    let mode = (data[(y >> bits) * block_w + (x >> bits)] >> 8) & 0xff;
                    pred_scalar_one(mode, left, top, tl, tr).expect("mode 合法")
                };
                pixels[index] = add_argb(pixels[index], pred);
            }
        }
    }

    /// 色变换标量参考（严格镜像 vp8l.rs `apply_color_transform`）。
    fn color_reference(pixels: &mut [u32], width: usize, height: usize, bits: u32, data: &[u32]) {
        let block_w = (width + (1usize << bits) - 1) >> bits;
        use super::super::vp8l::color_delta;
        for y in 0..height {
            for x in 0..width {
                let index = y * width + x;
                let cte = data[(y >> bits) * block_w + (x >> bits)];
                let green = (pixels[index] >> 8) & 0xff;
                let red = (pixels[index] >> 16) & 0xff;
                let blue = pixels[index] & 0xff;
                let red_delta = color_delta(cte & 0xff, green);
                let blue_delta = color_delta((cte >> 8) & 0xff, green);
                let red_for_blue = (red as i32 + red_delta) & 0xff;
                let new_blue = (blue as i32 + blue_delta + color_delta((cte >> 16) & 0xff, red_for_blue as u32)) & 0xff;
                pixels[index] = (pixels[index] & 0xff00_ff00) | ((red_for_blue as u32) << 16) | (new_blue as u32);
            }
        }
    }

    #[test]
    fn vp8l_predictor_inverse_simd_matches_scalar() {
        if !simd_on("sse2") {
            eprintln!("SIMD_TEST SKIP vp8l_predictor (无 sse2)");
            return;
        }
        let mut rng = Lcg(0x51ab_c0de_0fae_9001);
        let widths = [1usize, 2, 3, 4, 5, 7, 8, 16, 33, 64];
        let heights = [1usize, 2, 3, 5, 8];
        let bitsets = [2u32, 3];
        let mut checked = 0usize;
        // 全图随机模式 + 逐模式整图（确保每 mode 核与边缘/尾部都被真机上下文覆盖）。
        for &w in &widths {
            for &h in &heights {
                for &b in &bitsets {
                    let block_w = (w + (1usize << b) - 1) >> b;
                    let block_h = (h + (1usize << b) - 1) >> b;
                    let mut data = vec![0u32; block_w * block_h];
                    // 随机模式图。
                    for d in data.iter_mut() {
                        *d = ((rng.next() % 14) as u32) << 8;
                    }
                    for _ in 0..3 {
                        let mut a = vec![0u32; w * h];
                        let mut b2 = vec![0u32; w * h];
                        // 不透明像素（α≥0x81）：标量 clamp_add/clamp_half 的整像素 i32 中间量
                        // 在 debug 下不会溢出（release 回绕逐字节仍一致）；同时保留全 RGB 字节多样性。
                        for px in a.iter_mut() {
                            *px = 0xff00_0000 | ((rng.next() as u32) & 0x00ff_ffff);
                        }
                        b2.copy_from_slice(&a);
                        predictor_inverse(&mut a, w, h, b, &data).expect("SIMD 预测器");
                        predictor_reference(&mut b2, w, h, b, &data);
                        assert_eq!(a, b2, "随机模式图 w={w} h={h} bits={b}");
                        checked += 1;
                    }
                    // 逐模式整图（0..13）。
                    for m in 0..14u32 {
                        data.fill(m << 8);
                        let mut a = vec![0u32; w * h];
                        let mut b2 = vec![0u32; w * h];
                        for px in a.iter_mut() {
                            *px = 0xff00_0000 | ((rng.next() as u32) & 0x00ff_ffff);
                        }
                        b2.copy_from_slice(&a);
                        predictor_inverse(&mut a, w, h, b, &data).expect("SIMD 预测器");
                        predictor_reference(&mut b2, w, h, b, &data);
                        assert_eq!(a, b2, "单模式={m} w={w} h={h} bits={b}");
                        checked += 1;
                    }
                }
            }
        }
        eprintln!("VP8L_PREDICTOR_SIMD_PARITY\t{checked}");
    }

    #[test]
    fn vp8l_predictor_invalid_mode_defers_scalar() {
        // 非法模式（>13）：predictor_inverse 返回 None，绝不半途改动像素。
        if !simd_on("sse2") {
            return;
        }
        let mut rng = Lcg(0x0bad_cafe_f00d_beee);
        for &w in &[4usize, 8] {
            for &h in &[3usize, 5] {
                for &b in &[2u32, 3] {
                    let block_w = (w + (1usize << b) - 1) >> b;
                    let block_h = (h + (1usize << b) - 1) >> b;
                    let mut data = vec![0u32; block_w * block_h];
                    data[block_w * block_h / 2] = 14 << 8; // 一个非法 mode
                    let mut a = vec![0u32; w * h];
                    for px in a.iter_mut() {
                        *px = rng.next() as u32;
                    }
                    let before = a.clone();
                    let r = predictor_inverse(&mut a, w, h, b, &data);
                    assert!(r.is_none(), "非法模式应返回 None");
                    assert_eq!(a, before, "非法模式不得改动像素");
                }
            }
        }
    }

    #[test]
    fn vp8l_color_transform_simd_matches_scalar() {
        if !simd_on("sse4.1") && !simd_on("avx2") {
            eprintln!("SIMD_TEST SKIP vp8l_color (无 sse4.1/avx2)");
            return;
        }
        let mut rng = Lcg(0xfeeb_daad_beef_cafe);
        let widths = [1usize, 2, 3, 4, 5, 7, 8, 16, 33, 64];
        let heights = [1usize, 2, 3, 5, 8];
        let bitsets = [2u32, 3, 4];
        let edge_cte = [0x0000_0000u32, 0x0080_8080, 0x00ff_ffff, 0x007f_7f7f, 0x0080_ff00, 0x007f_0180];
        let mut checked = 0usize;
        for &w in &widths {
            for &h in &heights {
                for &b in &bitsets {
                    let block_w = (w + (1usize << b) - 1) >> b;
                    let block_h = (h + (1usize << b) - 1) >> b;
                    let mut data = vec![0u32; block_w * block_h];
                    for d in data.iter_mut() {
                        // 随机 cte + 常给符号/全值边界（0x00/0x7f/0x80/0xff）。
                        *d = if (rng.next() % 4) == 0 {
                            edge_cte[(rng.next() as usize) % edge_cte.len()]
                        } else {
                            (rng.next() as u32) & 0x00ff_ffff
                        };
                    }
                    for _ in 0..2 {
                        let mut a = vec![0u32; w * h];
                        let mut b2 = vec![0u32; w * h];
                        for px in a.iter_mut() {
                            *px = (rng.next() as u32) & 0xffff_ffff;
                        }
                        b2.copy_from_slice(&a);
                        assert!(color_transform_inverse(&mut a, w, h, b, &data), "SIMD 色变换");
                        color_reference(&mut b2, w, h, b, &data);
                        assert_eq!(a, b2, "色变换 w={w} h={h} bits={b}");
                        checked += 1;
                    }
                }
            }
        }
        eprintln!("VP8L_COLOR_SIMD_PARITY\t{checked}");
    }

    /// RGBA8↔RGB8 通道重排：SIMD 核（有 SSSE3 时走宽道）与标量逐字节一致
    /// （长度覆盖：标量尾分支 / SSSE3 整组 / 奇数右缘 / 大缓冲）。
    #[test]
    fn rgba8_rgb8_repack_matches_scalar() {
        let mut rng = Lcg(0x55aa_55aa_55aa_55aa);
        for n in [0usize, 1, 2, 3, 4, 5, 6, 7, 8, 12, 16, 17, 33, 257, 4096] {
            // 4→3 抽条。
            let rgba: Vec<u8> = (0..n * 4).map(|_| rng.byte()).collect();
            let mut rgb_simd = vec![0u8; n * 3];
            let mut rgb_ref = vec![0u8; n * 3];
            rgba8_to_rgb8(&rgba, &mut rgb_simd);
            for (px, out) in rgba.chunks_exact(4).zip(rgb_ref.chunks_exact_mut(3)) {
                out.copy_from_slice(&px[..3]);
            }
            assert_eq!(rgb_simd, rgb_ref, "4→3 n={n}");

            // 3→4 扩写（alpha 恒 255）。
            let rgb: Vec<u8> = (0..n * 3).map(|_| rng.byte()).collect();
            let mut rgba_simd = vec![0u8; n * 4];
            let mut rgba_ref = vec![0u8; n * 4];
            rgb8_to_rgba8(&rgb, &mut rgba_simd);
            for (i, px) in rgb.chunks_exact(3).enumerate() {
                let o = i * 4;
                rgba_ref[o] = px[0];
                rgba_ref[o + 1] = px[1];
                rgba_ref[o + 2] = px[2];
                rgba_ref[o + 3] = 255;
            }
            assert_eq!(rgba_simd, rgba_ref, "3→4 n={n}");
        }
        // 边界值：全 0 / 全 255 / 交替（覆盖 OR 0xFF 与 0 字节路径）。
        for val in [0u8, 1u8, 127, 128, 254, 255] {
            let rgba = vec![val; 16 * 4];
            let mut rgb_simd = vec![0u8; 16 * 3];
            rgba8_to_rgb8(&rgba, &mut rgb_simd);
            assert!(rgb_simd.iter().all(|&b| b == val), "4→3 flat {val}");
            let rgb = vec![val; 16 * 3];
            let mut rgba_simd = vec![0u8; 16 * 4];
            rgb8_to_rgba8(&rgb, &mut rgba_simd);
            assert!(
                rgba_simd
                    .chunks_exact(4)
                    .enumerate()
                    .all(|(_, px)| px[0] == val && px[1] == val && px[2] == val && px[3] == 255),
                "3→4 flat {val}"
            );
        }
        eprintln!("RGBA_RGB_REPACK_PARITY_OK");
    }

    /// 可达域双块对拍 HELPER（P1 专用）：ca/cb 双块 → 三路（i16 域 SIMD / i32 标量 / i32 SIMD
    /// K93 回退期）逐字节一致；域断言与 pre-shift 直断由调用方完成。仅测试用（模块内）。
    fn assert_reachable_pair_parity(
        ca: &[i16; 16],
        cb: &[i16; 16],
        pitch: usize,
        row0: usize,
        col0: usize,
        rng: &mut Lcg,
    ) {
        let mut plane_i16 = vec![0u8; pitch * pitch];
        let mut plane_scalar = vec![0u8; pitch * pitch];
        let mut plane_i32 = vec![0u8; pitch * pitch];
        for px in plane_i16.iter_mut() {
            *px = rng.byte();
        }
        plane_scalar.copy_from_slice(&plane_i16);
        plane_i32.copy_from_slice(&plane_i16);
        // i16 域 SIMD（默认路径，被测侧）。
        super::inverse_dct_add_two(ca, cb, &mut plane_i16, pitch, row0, col0);
        // i32 标量参考（逐块恒 has_ac=true）。
        super::super::vp8::inverse_dct_add(ca, &mut plane_scalar, pitch, row0, col0, true);
        super::super::vp8::inverse_dct_add(cb, &mut plane_scalar, pitch, row0, col0 + 4, true);
        assert_eq!(
            plane_i16, plane_scalar,
            "i16 SIMD ≠ i32 标量 (可达域) ca={ca:?} cb={cb:?}"
        );
        // K93 基线回退：i32 域 SIMD ≡ i16 域 SIMD（同序列 fixture 逐字节，进程内 A/B）。
        set_idct_i16_override(Some(false));
        super::inverse_dct_add_two(ca, cb, &mut plane_i32, pitch, row0, col0);
        set_idct_i16_override(Some(true));
        assert_eq!(
            plane_i16, plane_i32,
            "i16 SIMD ≠ i32 SIMD（K93 回退破坏字节恒等）ca={ca:?} cb={cb:?}"
        );
    }

    /// 标量两遍逆变换 max|pre-shift tmp|（对拍 HELPER）：i32 全程无回绕，值 = 第二遍
    /// `srai(x,3)` 前的最大 |x|——"pre-shift ≤ 32767"即 i16 域 pass2 免回绕充要条件（K70 直断口径）。
    fn scalar_idct_max_pre_shift(c: &[i16; 16]) -> i32 {
        const K1: i32 = 20091;
        const K2: i32 = -30068;
        let mulhi = |a: i32, b: i32| (a * b) >> 16;
        let mut t = [[0i32; 4]; 4]; // t[col][row]
        for r in 0..4 {
            let (i0, i1, i2, i3) = (
                i32::from(c[r * 4]),
                i32::from(c[r * 4 + 1]),
                i32::from(c[r * 4 + 2]),
                i32::from(c[r * 4 + 3]),
            );
            let a = i0 + i2;
            let b = i0 - i2;
            let cc = (mulhi(i1, K2) - mulhi(i3, K1)) + (i1 - i3);
            let dd = (mulhi(i1, K1) + mulhi(i3, K2)) + (i1 + i3);
            let tmp = [a + dd, b + cc, b - cc, a - dd];
            for col in 0..4 {
                t[col][r] = tmp[col];
            }
        }
        let mut max = 0i32;
        for lane in 0..4 {
            let (t0, t1, t2, t3) = (t[0][lane], t[1][lane], t[2][lane], t[3][lane]);
            let dc = t0 + 4;
            let a2 = dc + t2;
            let b2 = dc - t2;
            let cc = (mulhi(t1, K2) - mulhi(t3, K1)) + (t1 - t3);
            let dd = (mulhi(t1, K1) + mulhi(t3, K2)) + (t1 + t3);
            max = max
                .max((a2 + dd).abs())
                .max((b2 + cc).abs())
                .max((b2 - cc).abs())
                .max((a2 - dd).abs());
        }
        max
    }

    /// P1：`idct_add_two_sse2` i16 域 pass2 可达域对拍（对抗评审定案口径，K70）——随机系数
    /// ∥c∥≤2212（偶发 0/±2047/±2048/±2212 边界注入；**禁 8192/±32767/-32768**）双块、跨随机
    /// 平面位置、多种子。每块先直断：① max|c|≤2212；② scalar-pre-shift ≤ 32767（越界即告停，
    /// 勿静默改断言——若真实码流系数超 2212 会在此暴露）。验证 i16 SIMD ≡ i32 标量 ≡ i32 SIMD
    /// （K93 回退同窗字节恒等）。DC 孤立大值（±4096，真实码流 DC 可及）作独立边界块补充。
    #[test]
    fn idct_add_two_reachable_domain_parity() {
        if !simd_on("sse2") {
            eprintln!("SIMD_TEST SKIP idct_two (无 sse2)");
            return;
        }
        set_simd_part_override("dct", Some(true)); // 强制 SSE2 双指针核
        set_idct_i16_override(Some(true)); // 被测侧：i16 域 pass2
        let boundaries: [i16; 8] = [0, 1, -1, 2047, -2047, 2048, -2048, 2212];
        let pitch = 64usize;
        let mut checked = 0usize;
        // 主语料：全系数 ∥c∥≤2212。
        for seed in [
            0x1234_5678_9abc_def0u64,
            0x0bad_cafe_f00d_1111,
            0xfeed_f00d_abcd_2222,
            0xdead_beef_1234_3333,
        ] {
            let mut rng = Lcg(seed);
            for trial in 0..300 {
                let mut ca = [0i16; 16];
                let mut cb = [0i16; 16];
                for k in 0..16 {
                    // 可达域随机（[-2212,2212]），偶发边界注入。
                    ca[k] = if rng.next() % 9 == 0 {
                        boundaries[(rng.next() as usize) % boundaries.len()]
                    } else {
                        ((rng.next() % 4425) as i32 - 2212) as i16
                    };
                    cb[k] = if rng.next() % 9 == 0 {
                        boundaries[(rng.next() as usize) % boundaries.len()]
                    } else {
                        ((rng.next() % 4425) as i32 - 2212) as i16
                    };
                }
                // 偶发整块零 / 仅 DC（小值，标量 has_ac 分支差异最大考验）。
                if trial % 37 == 0 {
                    ca = [0i16; 16];
                }
                if trial % 53 == 0 {
                    let mut dc_only = [0i16; 16];
                    dc_only[0] = boundaries[(rng.next() as usize) % boundaries.len()];
                    cb = dc_only;
                }
                for c in [&ca, &cb] {
                    assert!(
                        c.iter().all(|&x| x.abs() <= 2212),
                        "语料超 2212（禁 8192，K70）seed=0x{seed:016x} trial={trial} c={c:?}"
                    );
                    let pre = scalar_idct_max_pre_shift(c);
                    assert!(
                        pre <= 32767,
                        "pre-shift={pre} 越 ±32767（i16 域不成立，勿静默改断言）seed=.. trial={trial} c={c:?}"
                    );
                }
                let (row0, col0) = (8 + (trial % 9), 8 + (trial % 9));
                assert_reachable_pair_parity(&ca, &cb, pitch, row0, col0, &mut rng);
                checked += 1;
            }
        }
        // DC 孤立大值边界块（真实码流 DC 可及）：dc ∈ {0, ±2048, ±4096}，AC 仍 ≤2212。
        for seed in [
            0x1357_9bdf_2468_ace0u64,
            0x0bad_cafe_f00d_4444,
            0xfeed_f00d_abcd_5555,
            0xdead_beef_1234_6666,
        ] {
            let mut rng = Lcg(seed);
            for trial in 0..120 {
                let dcs: [i16; 5] = [0, 2048, -2048, 4096, -4096];
                let mut ca = [0i16; 16];
                let mut cb = [0i16; 16];
                ca[0] = dcs[(rng.next() as usize) % dcs.len()];
                cb[0] = dcs[(rng.next() as usize) % dcs.len()];
                for k in 1..16 {
                    ca[k] = ((rng.next() % 4425) as i32 - 2212) as i16;
                    cb[k] = ((rng.next() % 4425) as i32 - 2212) as i16;
                }
                for c in [&ca, &cb] {
                    assert!(
                        c[0].abs() <= 4096 && c[1..].iter().all(|&x| x.abs() <= 2212),
                        "DC 边界块越界 seed=0x{seed:016x} trial={trial} c={c:?}"
                    );
                    let pre = scalar_idct_max_pre_shift(c);
                    assert!(
                        pre <= 32767,
                        "DC 边界 pre-shift={pre} 越 ±32767 seed=0x{seed:016x} trial={trial} c={c:?}"
                    );
                }
                let (row0, col0) = (8 + (trial % 9), 8 + (trial % 9));
                assert_reachable_pair_parity(&ca, &cb, pitch, row0, col0, &mut rng);
                checked += 1;
            }
        }
        set_simd_part_override("dct", None);
        set_idct_i16_override(None); // 恢复 i16 默认
        eprintln!("IDCT_TWO_REACHABLE_DOMAIN_PARITY\t{checked}");
    }

    /// P1：单块核 `idct_add_one_sse2` 的 i16 域可达域对拍（i4 路径）：随机 ∥c∥≤2212 + DC ±4096
    /// 边界块，i16 SIMD ≡ i32 标量 ≡ i32 SIMD（K93 回退）。
    #[test]
    fn idct_add_one_reachable_domain_parity() {
        if !simd_on("sse2") {
            eprintln!("SIMD_TEST SKIP idct_one_res (无 sse2)");
            return;
        }
        set_simd_part_override("dct", Some(true));
        set_idct_i16_override(Some(true));
        let pitch = 64usize;
        let mut checked = 0usize;
        for seed in [0x99aa_bbcc_ddee_ff00u64, 0x1234_5678_9012_3456, 0xa5a5_a5a5_5a5a_5a5a] {
            let mut rng = Lcg(seed);
            for trial in 0..300 {
                let mut c = [0i16; 16];
                for k in 0..16 {
                    c[k] = ((rng.next() % 4425) as i32 - 2212) as i16;
                }
                if trial % 29 == 0 {
                    c = [0i16; 16];
                }
                if trial % 43 == 0 {
                    let mut dc_only = [0i16; 16];
                    dc_only[0] = [0, 2048, -2048][(rng.next() as usize) % 3];
                    c = dc_only;
                }
                assert!(
                    c.iter().all(|&x| x.abs() <= 2212),
                    "单块语料超 2212 seed=0x{seed:016x} trial={trial} c={c:?}"
                );
                let pre = scalar_idct_max_pre_shift(&c);
                assert!(pre <= 32767, "单块 pre-shift={pre} 越界 seed=0x{seed:016x} trial={trial} c={c:?}");
                let mut plane_i16 = vec![0u8; pitch * pitch];
                let mut plane_scalar = vec![0u8; pitch * pitch];
                let mut plane_i32 = vec![0u8; pitch * pitch];
                for px in plane_i16.iter_mut() {
                    *px = rng.byte();
                }
                let (row0, col0) = match trial % 23 {
                    0 => (60usize, 59usize),
                    1 => (60, 4 + (trial % 11)),
                    2 => (4 + (trial % 11), 59),
                    _ => (4 + (trial % 11), 4 + (trial % 11)),
                };
                plane_scalar.copy_from_slice(&plane_i16);
                plane_i32.copy_from_slice(&plane_i16);
                super::inverse_dct_add_one(&c, &mut plane_i16, pitch, row0, col0);
                super::super::vp8::inverse_dct_add(&c, &mut plane_scalar, pitch, row0, col0, true);
                assert_eq!(plane_i16, plane_scalar, "单块 i16 ≠ 标量 seed=.. trial={trial} c={c:?}");
                set_idct_i16_override(Some(false));
                super::inverse_dct_add_one(&c, &mut plane_i32, pitch, row0, col0);
                set_idct_i16_override(Some(true));
                assert_eq!(plane_i16, plane_i32, "单块 i16 ≠ i32 SIMD（K93 回退）trial={trial} c={c:?}");
                checked += 1;
            }
        }
        set_simd_part_override("dct", None);
        set_idct_i16_override(None);
        eprintln!("IDCT_ONE_REACHABLE_DOMAIN_PARITY\t{checked}");
    }

    /// P3（K93 基线侧自检）：`idct_add_two_sse2` i32 域 pass2 全幅 parity（满幅 i16 系数，含
    /// 8192/±32767/-32768 等）——i32 SIMD ≡ i32 标量全输入域恒等。与 `inverse_dct_one_simd_matches_scalar`
    /// 对称，是 i16 可达域对拍的基线对照（i16 域不跑满幅，见 `idct_add_two_reachable_domain_parity`）。
    #[test]
    fn idct_add_two_full_scale_i32_baseline_parity() {
        if !simd_on("sse2") {
            eprintln!("SIMD_TEST SKIP idct_two_base (无 sse2)");
            return;
        }
        set_simd_part_override("dct", Some(true));
        set_idct_i16_override(Some(false)); // K93 基线侧：i32 域 pass2（全幅恒等）
        let boundaries: [i16; 16] = [
            0, 1, -1, 4095, -4095, 4096, -4096, 4097, -4097, 2047, -2048, 8192, -8192, 32767, -32768, 16384,
        ];
        let pitch = 64usize;
        let mut plane_simd = vec![0u8; 64 * 64];
        let mut plane_scalar = vec![0u8; 64 * 64];
        let mut checked = 0usize;
        for seed in [
            0x1234_5678_9abc_def0u64,
            0x0bad_cafe_f00d_1111,
            0xfeed_f00d_abcd_2222,
            0xdead_beef_1234_3333,
        ] {
            let mut rng = Lcg(seed);
            for trial in 0..300 {
                let mut ca = [0i16; 16];
                let mut cb = [0i16; 16];
                for k in 0..16 {
                    ca[k] = if rng.next() % 7 == 0 {
                        boundaries[(rng.next() as usize) % boundaries.len()]
                    } else {
                        (rng.next() as u16) as i16
                    };
                    cb[k] = if rng.next() % 7 == 0 {
                        boundaries[(rng.next() as usize) % boundaries.len()]
                    } else {
                        (rng.next() as u16) as i16
                    };
                }
                for px in plane_simd.iter_mut() {
                    *px = rng.byte();
                }
                let (row0, col0) = (8 + (trial % 9), 8 + (trial % 9));
                plane_scalar.copy_from_slice(&plane_simd);
                super::inverse_dct_add_two(&ca, &cb, &mut plane_simd, pitch, row0, col0);
                super::super::vp8::inverse_dct_add(&ca, &mut plane_scalar, pitch, row0, col0, true);
                super::super::vp8::inverse_dct_add(&cb, &mut plane_scalar, pitch, row0, col0 + 4, true);
                assert_eq!(
                    plane_simd, plane_scalar,
                    "i32 基线 seed=0x{seed:016x} trial={trial} row0={row0} col0={col0} ca={ca:?} cb={cb:?}"
                );
                checked += 1;
            }
        }
        set_simd_part_override("dct", None);
        set_idct_i16_override(None);
        eprintln!("IDCT_TWO_I32_BASELINE_FULLSCALE_PARITY\t{checked}");
    }

