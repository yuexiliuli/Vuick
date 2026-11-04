// ---------------------------------------------------------------------------
// 一致性单测：SIMD 路径与标量逐字节相同（bpp 覆盖 / 边界 / 伪随机）
// ---------------------------------------------------------------------------

#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::*;

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

    #[test]
    fn yuv_row_simd_matches_scalar() {
        let mut rng = Lcg(0x123456789abcdef0);
        if !simd_on("sse4.1") {
            eprintln!("SIMD_TEST SKIP (无 sse4.1)");
            return;
        }
        for n in [0usize, 1, 2, 4, 7, 8, 9, 15, 16, 33, 128, 257] {
            let y: Vec<u8> = (0..n).map(|_| rng.byte()).collect();
            let u: Vec<u8> = (0..n).map(|_| rng.byte()).collect();
            let v: Vec<u8> = (0..n).map(|_| rng.byte()).collect();
            let mut simd_out = vec![0u8; n * 4];
            let mut scalar_out = vec![0u8; n * 4];
            yuv_row_to_rgba(&y, &u, &v, &mut simd_out);
            for i in 0..n {
                let (r, g, b) = yuv_to_rgb(y[i], u[i], v[i]);
                scalar_out[i * 4] = r;
                scalar_out[i * 4 + 1] = g;
                scalar_out[i * 4 + 2] = b;
                scalar_out[i * 4 + 3] = 255;
            }
            assert_eq!(simd_out, scalar_out, "n={n}: SIMD vs 标量逐字节");
        }
        // 边界值：全 0 / 全 255 / 常见灰阶（验证负数与 clamp 路径）。
        for val in [0u8, 1, 16, 64, 128, 129, 254, 255] {
            let y = vec![val; 16];
            let u = vec![val; 16];
            let v = vec![val; 16];
            let mut simd_out = vec![0u8; 64];
            let mut scalar_out = vec![0u8; 64];
            yuv_row_to_rgba(&y, &u, &v, &mut simd_out);
            for i in 0..16 {
                let (r, g, b) = yuv_to_rgb(y[i], u[i], v[i]);
                scalar_out[i * 4] = r;
                scalar_out[i * 4 + 1] = g;
                scalar_out[i * 4 + 2] = b;
                scalar_out[i * 4 + 3] = 255;
            }
            assert_eq!(simd_out, scalar_out, "flat {val}: SIMD vs 标量");
        }
        eprintln!("YUV_ROW_SIMD_PARITY_OK");
    }

    /// RGB3 直出核：`yuv_row_to_rgb3` 每像素前 3 字节恒等于 `yuv_row_to_rgba` 同输入
    /// RGB3 直出核：`yuv_row_to_rgb3` 每像素前 3 字节恒等于 `yuv_row_to_rgba` 同输入
    /// （opaque 有损直出 RGB3 的字节契约硬闸）。覆盖全档（avx2/u16、sse4.1、标量）与边界长
    /// （0/1/2/7/8/9/15/16/17/32/33/128/257 及全 0/全 255 灰阶）。
    #[test]
    fn yuv_row_rgb3_matches_rgba() {
        let mut rng = Lcg(0x9e3779b97f4a7c15);
        // 强制/直走各档长度，验证任一 SIMD 或标量路径 RGB3 与 RGBA 前 3 字节恒等。
        for n in [0usize, 1, 2, 7, 8, 9, 15, 16, 17, 31, 32, 33, 128, 257] {
            let y: Vec<u8> = (0..n).map(|_| rng.byte()).collect();
            let u: Vec<u8> = (0..n).map(|_| rng.byte()).collect();
            let v: Vec<u8> = (0..n).map(|_| rng.byte()).collect();
            let mut rgba = vec![0u8; n * 4];
            yuv_row_to_rgba(&y, &u, &v, &mut rgba);
            let mut rgb3 = vec![0u8; n * 3];
            yuv_row_to_rgb3(&y, &u, &v, &mut rgb3);
            for i in 0..n {
                assert_eq!(
                    rgb3[i * 3..i * 3 + 3],
                    rgba[i * 4..i * 4 + 3],
                    "n={n} px={i}: RGB3 前 3 字节应等于 RGBA 前 3 字节"
                );
            }
        }
        // 边界值灰阶（负数/超限 clamp 路径）。
        for val in [0u8, 1, 16, 64, 128, 129, 254, 255] {
            let y = vec![val; 16];
            let u = vec![val; 16];
            let v = vec![val; 16];
            let mut rgba = vec![0u8; 64];
            let mut rgb3 = vec![0u8; 48];
            yuv_row_to_rgba(&y, &u, &v, &mut rgba);
            yuv_row_to_rgb3(&y, &u, &v, &mut rgb3);
            for i in 0..16 {
                assert_eq!(rgb3[i * 3..i * 3 + 3], rgba[i * 4..i * 4 + 3], "flat {val} px={i}");
            }
        }
        eprintln!("YUV_ROW_RGB3_PARITY_OK");
    }

    /// lossy-alpha 发射融合核：`yuv_row_to_rgba_alpha(Some(&alpha))` 逐字节恒等于
    /// 「旧二趟 `yuv_row_to_rgba` + 事后标量 `rgba[i*4+3]=alpha[i]`」合成（单遍≡两遍字节契约），
    /// 且 `None` 时与 `yuv_row_to_rgba`（α=255）完全一致；直接对标量 RGB 参考（覆盖全档
    /// avx2/i32、avx2/u16、sse4.1、标量尾列与边界长）。
    #[test]
    fn yuv_row_rgba_alpha_matches_two_pass_merge() {
        let mut rng = Lcg(0x51ed270b8c2d5f01);
        if !simd_on("sse4.1") {
            eprintln!("SIMD_TEST SKIP (无 sse4.1)");
            return;
        }
        for n in [0usize, 1, 2, 7, 8, 9, 15, 16, 17, 31, 32, 33, 128, 257] {
            let y: Vec<u8> = (0..n).map(|_| rng.byte()).collect();
            let u: Vec<u8> = (0..n).map(|_| rng.byte()).collect();
            let v: Vec<u8> = (0..n).map(|_| rng.byte()).collect();
            let a: Vec<u8> = (0..n).map(|_| rng.byte()).collect();
            // 两遍合成参考：`yuv_row_to_rgba`（α=255）→ 事后标量写 alpha。
            let mut two_pass = vec![0u8; n * 4];
            yuv_row_to_rgba(&y, &u, &v, &mut two_pass);
            for (i, &av) in a.iter().enumerate() {
                two_pass[i * 4 + 3] = av;
            }
            // 单遍融合核（alpha 并入发射内层）。
            let mut fused = vec![0u8; n * 4];
            yuv_row_to_rgba_alpha(&y, &u, &v, Some(&a), &mut fused);
            assert_eq!(fused, two_pass, "n={n}: 单遍融合应逐字节等于两遍合成");
            // 直接对标量 RGB 参考（A 通道=alpha[i]）。
            for i in 0..n {
                let (r, g, b) = yuv_to_rgb(y[i], u[i], v[i]);
                assert_eq!(&fused[i * 4..i * 4 + 3], &[r, g, b], "n={n} px={i} RGB");
                assert_eq!(fused[i * 4 + 3], a[i], "n={n} px={i} A");
            }
            // None（无 ALPH）≡ 旧 `yuv_row_to_rgba`（α=255）。
            let mut none_out = vec![0u8; n * 4];
            yuv_row_to_rgba_alpha(&y, &u, &v, None, &mut none_out);
            let mut plain = vec![0u8; n * 4];
            yuv_row_to_rgba(&y, &u, &v, &mut plain);
            assert_eq!(none_out, plain, "n={n}: None 恒 255 应与 yuv_row_to_rgba 一致");
        }
        // 边界值灰阶 + 全 0/全 255 alpha。
        for val in [0u8, 1, 64, 128, 254, 255] {
            let y = vec![val; 16];
            let u = vec![val; 16];
            let v = vec![val; 16];
            for av in [0u8, 128, 255] {
                let a = vec![av; 16];
                let mut fused = vec![0u8; 64];
                yuv_row_to_rgba_alpha(&y, &u, &v, Some(&a), &mut fused);
                for i in 0..16 {
                    let (r, g, b) = yuv_to_rgb(val, val, val);
                    assert_eq!(&fused[i * 4..i * 4 + 3], &[r, g, b], "flat {val} av={av} px={i}");
                    assert_eq!(fused[i * 4 + 3], av, "flat {val} av={av} A");
                }
            }
        }
        eprintln!("YUV_ROW_RGBA_ALPHA_MERGE_PARITY_OK");
    }

    /// ARGB→RGBA 输出与 Subtract-Green：SIMD vs 标量逐字节（含奇偶边界与全通道路径）。
    #[test]
    fn argb_to_rgba_and_subtract_green_match_scalar() {
        let mut rng = Lcg(0x0123456789abcdef);
        for n in [0usize, 1, 2, 3, 4, 5, 8, 9, 12, 16, 33, 200] {
            let mut px: Vec<u32> = (0..n).map(|_| rng.next() as u32).collect();
            let mut out_simd = vec![0u8; n * 4];
            argb_to_rgba(&px, &mut out_simd);
            let mut out_scalar = vec![0u8; n * 4];
            for (i, &p) in px.iter().enumerate() {
                out_scalar[i * 4] = (p >> 16) as u8;
                out_scalar[i * 4 + 1] = (p >> 8) as u8;
                out_scalar[i * 4 + 2] = p as u8;
                out_scalar[i * 4 + 3] = (p >> 24) as u8;
            }
            assert_eq!(out_simd, out_scalar, "argb_to_rgba n={n}");

            // 原地版：SIMD 收尾 vs 标量逐字节（B/R 对调；覆盖 AVX2 8px / SSSE3 4px / 尾列长）。
            let mut ip_simd = px.clone();
            argb_to_rgba_inplace(&mut ip_simd);
            let ip_ref: Vec<u32> = px
                .iter()
                .map(|&p| (p & 0xff00_ff00) | ((p & 0xff) << 16) | ((p >> 16) & 0xff))
                .collect();
            assert_eq!(ip_simd, ip_ref, "argb_to_rgba_inplace n={n}");
            // 原地版逐字节 = 非原地版输出（同一输入）。
            let ip_bytes: Vec<u8> = ip_simd
                .iter()
                .flat_map(|&p| p.to_le_bytes())
                .collect();
            assert_eq!(ip_bytes, out_simd, "argb_to_rgba inplace vs out-of-place n={n}");

            let simd_px = px.clone();
            subtract_green(&mut px);
            // 真实标量语义（v8pl `apply_subtract_green`：`+G & 0xff` 掩 8 位）逐字节对拍。
            let true_ref = simd_px
                .iter()
                .map(|&p| {
                    let g = (p >> 8) & 0xff;
                    let r = (((p >> 16) & 0xff) + g) & 0xff;
                    let b = ((p & 0xff) + g) & 0xff;
                    (p & 0xff00_ff00) | (r << 16) | b
                })
                .collect::<Vec<u32>>();
            assert_eq!(px, true_ref, "subtract_green TRUE-scalar n={n}");
        }
        eprintln!("ARGB_TO_RGBA_SUBTRACT_GREEN_PARITY_OK");
    }

    /// fancy 上采样：SIMD 整行 vs vp8 标量 `upsample_fancy` 逐字节（单一算术源对拍）。
    #[test]
    fn upsample_fancy_row_matches_scalar() {
        let mut rng = Lcg(0xdeadbeefcafef00d);
        // 各宽度（含奇数/偶数与超宽）覆盖中间/首末列路径。
        for cw in [0usize, 1, 2, 3, 4, 5, 6, 7, 8, 9, 15, 16, 17, 64, 129] {
            for wmul in [0usize, 1] {
                if cw == 0 {
                    continue;
                }
                // 输出宽 = 2*cw 或 2*cw-1（奇数原宽时末列仅左像素）。
                let width = 2 * cw - wmul;
                if width == 0 {
                    continue;
                }
                let cur: Vec<u8> = (0..cw).map(|_| rng.byte()).collect();
                let adj: Vec<u8> = (0..cw).map(|_| rng.byte()).collect();
                let simd = upsample_fancy_row(&cur, &adj, width);
                let scalar = super::super::vp8::upsample_fancy(&cur, &adj, width);
                assert_eq!(simd, scalar, "cw={cw} width={width}: SIMD vs 标量");
            }
        }
        eprintln!("UPSAMPLE_FANCY_SIMD_PARITY_OK");
    }

    /// fancy 上采样 U/V 合并路径 vs 单平面路径（单平面已与 vp8 标量逐字节对拍）逐字节一致。
    /// 覆盖 cw∈{1,2,…} 奇偶输出宽与 cw≤1 边界（cw=0 无输出宽，跳过与单平面测试一致）。
    #[test]
    fn upsample_fancy_uv_matches_single() {
        let mut rng = Lcg(0x9e3779b97f4a7c15);
        for cw in [0usize, 1, 2, 3, 4, 5, 6, 7, 8, 9, 15, 16, 17, 64, 129] {
            for wmul in [0usize, 1] {
                if cw == 0 {
                    continue;
                }
                // 输出宽 = 2*cw 或 2*cw-1（奇数原宽时末列仅左像素）。
                let width = 2 * cw - wmul;
                let u_cur: Vec<u8> = (0..cw).map(|_| rng.byte()).collect();
                let u_adj: Vec<u8> = (0..cw).map(|_| rng.byte()).collect();
                let v_cur: Vec<u8> = (0..cw).map(|_| rng.byte()).collect();
                let v_adj: Vec<u8> = (0..cw).map(|_| rng.byte()).collect();
                // 单平面（对照）路径。
                let u_single = upsample_fancy_row(&u_cur, &u_adj, width);
                let v_single = upsample_fancy_row(&v_cur, &v_adj, width);
                // 合并路径。
                let mut u_out = vec![0u8; width];
                let mut v_out = vec![0u8; width];
                let mut t_ws = vec![0u16; 2 * (cw + 8)];
                upsample_fancy_into_uv(
                    &u_cur, &u_adj, &v_cur, &v_adj, &mut u_out, &mut v_out, &mut t_ws,
                );
                assert_eq!(u_out, u_single, "U cw={cw} width={width}: 合并 vs 单平面");
                assert_eq!(v_out, v_single, "V cw={cw} width={width}: 合并 vs 单平面");
            }
        }
        eprintln!("UPSAMPLE_FANCY_UV_PARITY_OK");
    }

    /// P2 行对熔合字节恒等 oracle：`upsample_fancy_into_uv_pair`（奇行 t=3*top+cur、偶行
    /// t=3*cur+top 一次成）≡ 逐行两次 `upsample_fancy_into_uv`（后者已分别对拍 vp8 标量）。
    /// 覆盖输出宽奇偶（2cw / 2cw-1）、cw 边界小值/批量阈值、像素饱和值。
    #[test]
    fn upsample_pair_matches_twice_into_uv() {
        let mut rng = Lcg(0x6a09e667f3bcc909);
        let mut checked = 0usize;
        for cw in [1usize, 2, 3, 4, 5, 6, 7, 8, 9, 15, 16, 17, 18, 63, 64, 65, 128, 129, 256] {
            for wmul in [0usize, 1] {
                let width = 2 * cw - wmul;
                for _ in 0..8 {
                    let mut u_top = vec![0u8; cw];
                    let mut u_cur = vec![0u8; cw];
                    let mut v_top = vec![0u8; cw];
                    let mut v_cur = vec![0u8; cw];
                    for dst in [&mut u_top, &mut u_cur, &mut v_top, &mut v_cur] {
                        for px in dst.iter_mut() {
                            *px = match rng.next() % 10 {
                                0 => 0,
                                1 => 1,
                                2 => 254,
                                3 => 255,
                                _ => rng.byte(),
                            };
                        }
                    }
                    // 逐行对照（各自 `upsample_fancy_into_uv`，语义=2 次单行）。
                    let mut u_odd_ref = vec![0u8; width];
                    let mut v_odd_ref = vec![0u8; width];
                    let mut u_even_ref = vec![0u8; width];
                    let mut v_even_ref = vec![0u8; width];
                    let mut t2 = vec![0u16; 2 * (cw + 8)];
                    upsample_fancy_into_uv(&u_top, &u_cur, &v_top, &v_cur, &mut u_odd_ref, &mut v_odd_ref, &mut t2);
                    upsample_fancy_into_uv(&u_cur, &u_top, &v_cur, &v_top, &mut u_even_ref, &mut v_even_ref, &mut t2);
                    // 熔合路径。
                    let mut u_odd = vec![0u8; width];
                    let mut v_odd = vec![0u8; width];
                    let mut u_even = vec![0u8; width];
                    let mut v_even = vec![0u8; width];
                    let mut t4 = vec![0u16; 4 * (cw + 8)];
                    upsample_fancy_into_uv_pair(
                        &u_top, &u_cur, &v_top, &v_cur,
                        &mut u_odd, &mut v_odd, &mut u_even, &mut v_even, &mut t4,
                    );
                    assert_eq!(u_odd, u_odd_ref, "U 奇行 cw={cw} width={width}");
                    assert_eq!(v_odd, v_odd_ref, "V 奇行 cw={cw} width={width}");
                    assert_eq!(u_even, u_even_ref, "U 偶行 cw={cw} width={width}");
                    assert_eq!(v_even, v_even_ref, "V 偶行 cw={cw} width={width}");
                    checked += 1;
                }
            }
        }
        eprintln!("UPSAMPLE_PAIR_EQ_TWICE_INTO_UV\t{checked}");
    }

    // -----------------------------------------------------------------------
    // D1：4×4 十模式 × 邻居组合 + 16×16/8×8 四模式，SIMD 与标量逐字节一致
    // -----------------------------------------------------------------------

    /// 邻居值抽样（含饱和/边界）。
    fn neighbor_byte(rng: &mut Lcg) -> u8 {
        match rng.next() % 8 {
            0 => 0,
            1 => 1,
            2 => 127,
            3 => 128,
            4 => 254,
            5 => 255,
            _ => rng.byte(),
        }
    }

    #[test]
    fn predict_subblock_simd_matches_scalar() {
        if !simd_on("ssse3") {
            eprintln!("SIMD_TEST SKIP (无 ssse3)");
            return;
        }
        let mut rng = Lcg(0x51d2e3f4a5b6c7d8);
        let (pitch, rows) = (40usize, 40usize);
        let mut plane = vec![0u8; rows * pitch];
        let mut a = vec![0u8; rows * pitch];
        let mut b = vec![0u8; rows * pitch];
        let mut checked = 0usize;
        for trial in 0..400 {
            let mode = (trial % 10) as u8;
            let r0 = 1 + (trial % 3);
            let c0 = 1 + (trial % 3);
            for px in plane.iter_mut() {
                *px = rng.byte();
            }
            // 左列（列 col0-1）与 above 参数分别采样；偶发左列=129 帧边界。
            let mut above = [0u8; 9];
            for i in 0..9 {
                above[i] = neighbor_byte(&mut rng);
            }
            for i in 0..4 {
                let v = if trial % 7 == 0 { 129 } else { neighbor_byte(&mut rng) };
                plane[(r0 + i) * pitch + c0 - 1] = v;
            }
            a.copy_from_slice(&plane);
            b.copy_from_slice(&plane);
            super::predict_subblock(&mut a, pitch, r0, c0, mode, &above);
            super::super::vp8::predict_subblock(&mut b, pitch, r0, c0, mode, &above);
            assert_eq!(a, b, "4×4 mode={mode} r0={r0} c0={c0} trial={trial}");
            checked += 1;
        }
        eprintln!("PREDICT_SUBBLOCK_SIMD_PARITY\t{checked}");
    }

    /// 16×16 / 8×8 四大模式 SIMD vs 标量逐字节。
    #[test]
    fn predict_large_simd_matches_scalar() {
        if !simd_on("ssse3") {
            eprintln!("SIMD_TEST SKIP (无 ssse3)");
            return;
        }
        let mut rng = Lcg(0x0123456789abcdef);
        let pitch = 64usize;
        let mut plane = vec![0u8; 64 * 64];
        let mut a = vec![0u8; 64 * 64];
        let mut b = vec![0u8; 64 * 64];
        let mut checked = 0usize;
        for trial in 0..120 {
            let size = if trial % 2 == 0 { 16 } else { 8 };
            let mode = (trial / 3) % 4;
            let has_top = (trial >> 2) & 1 == 0;
            let has_left = (trial >> 3) & 1 == 0;
            let (r0, c0) = (1, 1);
            for px in plane.iter_mut() {
                *px = rng.byte();
            }
            let above: Vec<u8> = (0..=size).map(|_| neighbor_byte(&mut rng)).collect();
            for i in 0..size {
                plane[(r0 + i) * pitch + c0 - 1] = neighbor_byte(&mut rng);
            }
            a.copy_from_slice(&plane);
            b.copy_from_slice(&plane);
            super::predict_large(&mut a, pitch, r0, c0, size, mode, has_top, has_left, &above);
            super::super::vp8::predict_large(&mut b, pitch, r0, c0, size, mode, has_top, has_left, &above);
            assert_eq!(
                a, b,
                "predict_large mode={mode} size={size} has_top={has_top} has_left={has_left} trial={trial}"
            );
            checked += 1;
        }
        eprintln!("PREDICT_LARGE_SIMD_PARITY\t{checked}");
    }

    // -----------------------------------------------------------------------
    // D2：环路滤波 Simple/Normal（Inner/Macroblock）× 竖/横 × 16/8 lane × 参数带
    // -----------------------------------------------------------------------

    #[test]
    fn filter_edge_simd_matches_scalar() {
        if !simd_on("sse2") {
            eprintln!("SIMD_TEST SKIP (无 sse2)");
            return;
        }
        set_simd_part_override("flt", Some(true));
        let mut rng = Lcg(0xfeedfacecafebeef);
        let pitch = 80usize;
        let n = 80usize;
        let mut plane_simd = vec![0u8; n * pitch];
        let mut plane_scalar = vec![0u8; n * pitch];
        let mut checked = 0usize;
        let simple_opts = [true, false];
        let mb_opt = [true, false];
        let vertical_opt = [true, false];
        let len_opt = [16usize, 8];
        // sharpness/level 常见量级：interior/edge/hev 全组合抽样。
        for trial in 0..160 {
            let simple = simple_opts[trial % 2];
            let macroblock_edge = mb_opt[(trial / 2) % 2];
            let vertical = vertical_opt[(trial / 4) % 2];
            let length = len_opt[(trial / 8) % 2];
            let (x, y) = if length == 16 { (16, 16) } else { (8, 8) };
            let interior = match (trial / 16) % 8 { 0 => 1, 1 => 2, 2 => 3, 3 => 5, 4 => 7, 5 => 9, 6 => 15, _ => 63 };
            let edge = match (trial / 16) % 8 { 0 => 2, 1 => 8, 2 => 16, 3 => 24, 4 => 40, 5 => 60, 6 => 130, _ => 190 };
            let hev = ((trial / 17) % 3) as i32;
            for b in plane_simd.iter_mut() {
                *b = rng.byte();
            }
            // 逼真邻域：边界常数值偶发插入（大 flat / 强边）。
            for i in 0..(n * pitch / 8) {
                let off = rng.next() as usize % (n * pitch);
                plane_simd[off] = if i % 3 == 0 { 0 } else { 255 };
            }
            plane_scalar.copy_from_slice(&plane_simd);
            super::filter_edge(&mut plane_simd, pitch, x, y, length, vertical, macroblock_edge, simple, interior, edge, hev);
            super::super::vp8::filter_edge(&mut plane_scalar, pitch, x, y, length, vertical, macroblock_edge, simple, interior, edge, hev);
            assert_eq!(
                plane_simd, plane_scalar,
                "filter simple={simple} mb={macroblock_edge} vertical={vertical} len={length} interior={interior} edge={edge} hev={hev} trial={trial}"
            );
            checked += 1;
        }
        set_simd_part_override("flt", None);
        eprintln!("FILTER_EDGE_SIMD_PARITY\t{checked}");
    }

    /// 8×8 转置梯往返（矩阵转置自身逆）与 16×8 梯（load16x8 ↔ store16x8）自检。
    #[test]
    fn transpose_ladders_roundtrip() {
        if !simd_on("sse2") {
            eprintln!("SIMD_TEST SKIP (无 sse2)");
            return;
        }
        let mut rng = Lcg(0x8421c8421c842184);
        // 8×8：各行低 8 字节为矩阵行，转两次应还原。
        let mut plane = [0u8; 64];
        for b in plane.iter_mut() {
            *b = rng.byte();
        }
        unsafe {
            let rows: [__m128i; 8] = std::array::from_fn(|i| {
                _mm_loadl_epi64(plane.as_ptr().add(i * 8) as *const __m128i)
            });
            let cols = transpose8x8(rows);
            // 断言低 8 字节 = 期望转置（cols[k] 低 8 字节 = 列 k 的 8 行）。
            for k in 0..8 {
                let mut got = [0u8; 8];
                _mm_storel_epi64(got.as_mut_ptr() as *mut __m128i, cols[k]);
                for r in 0..8 {
                    assert_eq!(got[r], plane[r * 8 + k], "8×8 转置 col{k} row{r}");
                }
            }
            let back = transpose8x8(cols);
            for i in 0..8 {
                let mut got = [0u8; 8];
                _mm_storel_epi64(got.as_mut_ptr() as *mut __m128i, back[i]);
                assert_eq!(&got[..], &plane[i * 8..i * 8 + 8], "8×8 往返 row{i}");
            }
        }
        // 16×8：内存 16 行 × 8 列，load16x8 拆 a/b 两半 → store16x8 应还原。
        let mut plane = vec![0u8; 16 * 8];
        for b in plane.iter_mut() {
            *b = rng.byte();
        }
        let mut copy = plane.clone();
        unsafe {
            let a = copy.as_mut_ptr();
            let b = a.add(8 * 8); // 行 8..15（stride=8）
            let c = load16x8(a, 8, b, 8);
            store16x8(a, 8, b, 8, c);
        }
        assert_eq!(copy, plane, "16×8 转置梯往返");
        eprintln!("TRANSPOSE_LADDERS_ROUNDTRIP_OK");
    }

    // -----------------------------------------------------------------------
    // D3：IDCT 双块并行（SSE2）与标量逐字节一致（系数/量级/DC-only/全零随机对拍）
    // -----------------------------------------------------------------------

