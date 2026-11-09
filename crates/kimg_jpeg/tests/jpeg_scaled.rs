//! JPEG 展示档 DCT 缩放的验收测试（B 契约 / 验收口径；den∈{2,4,8}）。
//!
//! - **B1 网格同构**：`scaled_dimensions == box_dims`（顶左对齐、`ceil(w/den)×ceil(h/den)`）
//!   对全 `w,h∈[1..20]` + 语料各档尺寸成立；`read_image_scaled` 输出尺寸与声明一致。
//! - **B2 抗混叠半核**：内核级 `cs::tests::idct4x4/idct2x2_eq_box_of_bandlimited` +
//!   `dc_scaled_kernels_eq_dc_only_value`（lib 单元）；本文件核对端到端 PSNR + 高频能量不降。
//! - **验收口径**：den=2/4（420+444 q100 3mp/12mp + moe 50MP）+ den=8 + 渐进（SOF2）各档；
//!   全部 PSNR≥30（宽松档 ≥28）。
//! - **env 回退**：`KIMG_JPEG_SCALED=0` → `read_image_scaled` 返回 Unsupported。
//!   注意 OnceLock 进程内一次判定 → 该用例独立成文件在独立进程跑（`jpeg_scaled_gate.rs`）。
//! - 语料缺失 → SKIP（常态）；长命令前台阻塞到结束，禁全量。
//!
//! 运行：`cargo test -p kimg_jpeg --test jpeg_scaled -- --nocapture`

mod fixtures;

use kimg_core::ImageDecoder as _;
use kimg_jpeg::codecs::jpeg::JpegDecoder;

use fixtures::*;

/// `den×den` 盒式降采样（单线程参考）：`den×den` 源块 → 1 输出像素，每通道平均；
/// 右/下不满块按实际像素数归一。输出 `ceil(w/den)×ceil(h/den)`。
fn box_downsample_n(px: &[u8], iw: u32, ih: u32, ch: usize, den: u32) -> Vec<u8> {
    let ow = iw.div_ceil(den);
    let oh = ih.div_ceil(den);
    let mut out = vec![0u8; ow as usize * oh as usize * ch];
    for oy in 0..oh {
        for ox in 0..ow {
            let mut sum = vec![0u32; ch];
            let mut n = 0u32;
            for dy in 0..den {
                for dx in 0..den {
                    let (sx, sy) = (ox * den + dx, oy * den + dy);
                    if sx < iw && sy < ih {
                        for c in 0..ch {
                            sum[c] += u32::from(px[(sy * iw + sx) as usize * ch + c]);
                        }
                        n += 1;
                    }
                }
            }
            for c in 0..ch {
                out[(oy * ow + ox) as usize * ch + c] = (sum[c] / n.max(1)) as u8;
            }
        }
    }
    out
}

/// PSNR（所有通道合并 MSE；dB）。
fn psnr(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len(), "PSNR 输入长度不一致");
    let mut mse_sum = 0.0f64;
    let n = a.len();
    for (&x, &y) in a.iter().zip(b.iter()) {
        let d = i32::from(x) - i32::from(y);
        mse_sum += (d * d) as f64;
    }
    let mse = mse_sum / n.max(1) as f64;
    if mse <= 1e-6 {
        return f64::INFINITY;
    }
    10.0 * (255.0 * 255.0 / mse).log10()
}

/// 高频能量：2D 高通（像素与其 4 邻均值差）范数²，归一化到像素数（判别「过度模糊」）。
fn highfreq_energy(px: &[u8], w: u32, h: u32, ch: usize) -> f64 {
    let mut e = 0.0f64;
    let mut n = 0u32;
    for y in 1..h.saturating_sub(1) {
        for x in 1..w.saturating_sub(1) {
            for c in 0..ch {
                let v = px[(y * w + x) as usize * ch + c];
                let mut neigh = 0i32;
                for (dx, dy) in [(-1i32, 0), (1, 0), (0, -1i32), (0, 1)] {
                    let xx = (x as i32 + dx) as u32;
                    let yy = (y as i32 + dy) as u32;
                    neigh += i32::from(px[(yy * w + xx) as usize * ch + c]);
                }
                let lp = (neigh + 2) / 4;
                let hp = i32::from(v) - lp;
                e += (hp * hp) as f64;
                n += 1;
            }
        }
    }
    e / n.max(1) as f64
}

/// 全解一张语料 JPEG：自然 color_type 字节（JpegDecoder 同口径）。
fn full_decode(bytes: &[u8]) -> Result<(u32, u32, kimg_core::ColorType, Vec<u8>), String> {
    let dec = JpegDecoder::from_bytes(bytes).map_err(|e| format!("构造失败 {e}"))?;
    let (w, h) = dec.dimensions();
    let ct = dec.color_type();
    let mut buf = vec![0u8; dec.total_bytes() as usize];
    dec.read_image(&mut buf).map_err(|e| format!("全解失败 {e}"))?;
    Ok((w, h, ct, buf))
}

/// den 缩放解码：自然 color_type 字节（与全解同 color_type）。
fn scaled_decode(
    bytes: &[u8],
    den: u8,
) -> Result<(u32, u32, kimg_core::ColorType, Vec<u8>), String> {
    let dec = JpegDecoder::from_bytes(bytes).map_err(|e| format!("构造失败 {e}"))?;
    let ct = dec.color_type();
    let (sw, sh) = dec
        .scaled_dimensions(den)
        .ok_or(format!("scaled_dimensions({den}) 意外为 None"))?;
    let ch = usize::from(ct.bytes_per_pixel());
    let mut buf = vec![0u8; sw as usize * sh as usize * ch];
    dec.read_image_scaled(den, &mut buf)
        .map_err(|e| format!("缩放失败 {e}"))?;
    Ok((sw, sh, ct, buf))
}

/// 全解 + 盒式 den（现展示产物参照）：与 `scaled_decode` 同 color_type。
fn box_ref(bytes: &[u8], den: u8) -> Result<(u32, u32, kimg_core::ColorType, Vec<u8>), String> {
    let (w, h, ct, px) = full_decode(bytes)?;
    let ch = usize::from(ct.bytes_per_pixel());
    let b = box_downsample_n(&px, w, h, ch, u32::from(den));
    let (bw, bh) = (w.div_ceil(u32::from(den)), h.div_ceil(u32::from(den)));
    Ok((bw, bh, ct, b))
}

/// 单文件验收：scaled vs box（同 color_type）PSNR + 高频能量比。
/// 返回 Ok(Some((psnr_db, energy_ratio))) 已测；Ok(None) = SKIP（语料缺失/格式不支持）。
fn check_one_den(
    name: &str,
    bytes: &[u8],
    den: u8,
    min_psnr: f64,
) -> Result<Option<(f64, f64)>, String> {
    let (sw, sh, ct, s) = match scaled_decode(bytes, den) {
        Ok(v) => v,
        Err(e) if e.contains("does not support") || e.contains("Unsupported") => {
            println!("{name}: den={den} SKIP（缩放不支持：{e}）");
            return Ok(None);
        }
        Err(e) => return Err(format!("{name}: den={den} {e}")),
    };
    let (bw, bh, bct, b) = box_ref(bytes, den)?;
    if bct != ct {
        return Err(format!("{name}: den={den} color_type 不一致 scaled={ct:?} box={bct:?}"));
    }
    if (sw, sh) != (bw, bh) {
        return Err(format!(
            "{name}: den={den} 网格不同构 scaled={sw}×{sh} box={bw}×{bh}（B1 契约）"
        ));
    }
    let p = psnr(&s, &b);
    let ch = usize::from(ct.bytes_per_pixel());
    let es = highfreq_energy(&s, sw, sh, ch);
    let eb = highfreq_energy(&b, sw, sh, ch);
    let ratio = if eb > 1e-9 { es / eb } else { 1.0 };
    println!(
        "{name}: den={den} {sw}×{sh} {:?} PSNR={p:.2}dB 高频能量比={ratio:.2} (scaled {es:.2} / box {eb:.2})",
        ct
    );
    if p < min_psnr {
        return Err(format!("{name}: den={den} PSNR {p:.2}dB < {min_psnr}dB"));
    }
    // 高频能量不降（相对 box 展示产物；抗混叠允许适量衰减，但不得过度模糊 < 0.5×）
    if ratio < 0.5 {
        return Err(format!("{name}: den={den} 缩放过度模糊 高频能量比 {ratio:.2} < 0.5"));
    }
    Ok(Some((p, ratio)))
}

/// 对单个文件逐一跑一组 (den, min_psnr) 验收；任一失败记一失败项。
fn check_dens(
    name: &str,
    bytes: &[u8],
    dens: &[(u8, f64)],
    failures: &mut Vec<String>,
    checked: &mut usize,
) {
    for &(den, min_psnr) in dens {
        match check_one_den(name, bytes, den, min_psnr) {
            Ok(Some(_)) => *checked += 1,
            Ok(None) => {}
            Err(e) => failures.push(e),
        }
    }
}

/// B1/验收聚合：对口语料（420/444×q100×3mp/12mp、moe 50MP、渐进 3MP）逐一多档验收。
/// den=2/4（420+444，q100 细纹理/振铃应力）+ den=8（420 12mp）+ 渐进（420 prog 系）。
#[test]
fn scaled_psnr_vs_box_acceptance() {
    let dir = corpus_dir();
    if !dir.is_dir() {
        println!("SKIP: missing {}", dir.display());
        return;
    }
    let files = collect_jpegs(&dir);
    // 焦点清单：名称 → 各档验收 (den, min_psnr)。缺失即跳过；den=2/4/8 全覆盖。
    let focus: Vec<(&str, Vec<(u8, f64)>)> = vec![
        (
            "420-q100-3mp__landscape-almsee.jpg",
            vec![(2, 30.0), (4, 30.0)],
        ),
        (
            "444-q100-3mp__landscape-almsee.jpg",
            vec![(2, 30.0), (4, 30.0)],
        ),
        (
            "420-q100-12mp__texture-ground.jpg",
            vec![(2, 30.0), (4, 30.0), (8, 30.0)],
        ),
        (
            "444-q100-12mp__architecture-teatro.jpg",
            vec![(2, 30.0), (4, 30.0), (8, 30.0)],
        ),
        // moe 50MP baseline（4:2:0 + RST）：den=4 减轻 50MP 全解+box 参照成本，仍覆盖真实大图
        ("moe 46577 duel_dolls tinkle.jpg", vec![(4, 30.0)]),
        // 渐进（SOF2）scaled：3mp 语料快速验收 den=2/4
        (
            "420-q95-3mp-prog__panorama-lakeview.jpg",
            vec![(2, 30.0), (4, 30.0)],
        ),
        (
            "420-q75-3mp-prog__night-alexanderplatz.jpg",
            vec![(2, 30.0), (4, 30.0)],
        ),
    ];
    let mut checked = 0usize;
    let mut failures: Vec<String> = Vec::new();
    for (want, dens) in &focus {
        let p = dir.join(want);
        if !p.is_file() {
            println!("SKIP: missing {}（语料子集未含）", p.display());
            continue;
        }
        let bytes = std::fs::read(&p).unwrap();
        check_dens(want, &bytes, dens, &mut failures, &mut checked);
    }
    // 通用样本（q75/90 等，den=2 宽松 ≥28dB）加粗样本：任一失败即报
    for p in &files {
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        if focus.iter().any(|(w, _)| *w == name.as_str()) {
            continue;
        }
        let bytes = std::fs::read(p).unwrap();
        match check_one_den(&name, &bytes, 2, 28.0) {
            Ok(Some(_)) => checked += 1,
            Ok(None) => {}
            Err(e) => failures.push(e),
        }
    }
    println!("JPEG_SCALED SUMMARY checked={checked} fail={}", failures.len());
    for f in &failures {
        println!("SCALED FAIL: {f}");
    }
    assert!(
        failures.is_empty(),
        "{} 个缩放验收失败：\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// B1 网格同构（property）：`scaled_dimensions(den) == (w.div_ceil(den), h.div_ceil(den))`
/// 对 **den∈{2,4,8}** × **全 `w,h∈[1..20]`** 逐项断言（零解码：仅头解析后取
/// `scaled_dimensions`，不跑像素解码）；den∉{2,4,8} → None。再加 12MP 语料的
/// 「声明 == 实际解码 == box」端到端对拍（含非 8 整倍宽高）。
///
/// 做法：读一张最小语料 JPEG，把它的 SOF 宽高逐项 patch 成 (w,h)，`from_bytes` 只做头
/// 解析（µs 级），对每个尺寸核对 `scaled_dimensions(den)` 与 box 网格公式同构。
#[test]
fn scaled_dims_eq_box_dims() {
    let dir = corpus_dir();
    if !dir.is_dir() {
        println!("SKIP: missing {}", dir.display());
        return;
    }
    // 最小语料（0.5mp q50 48KB）作 patch 底本；缺失则退 3mp 样本。
    let base = {
        let cand = dir.join("420-q50-0.5mp__texture-ground.jpg");
        if cand.is_file() {
            cand
        } else {
            dir.join("420-q100-3mp__landscape-almsee.jpg")
        }
    };
    if !base.is_file() {
        println!("SKIP: 无语料可作 SOF patch 底本");
        return;
    }
    let base_bytes = std::fs::read(&base).unwrap();
    for w in 1..=20u32 {
        for h in 1..=20u32 {
            let patched = patch_sof_dims(&base_bytes, w, h)
                .unwrap_or_else(|| panic!("patch SOF 失败 ({w}×{h})"));
            let dec = JpegDecoder::from_bytes(&patched)
                .unwrap_or_else(|e| panic!("patch 后头解析失败 ({w}×{h}): {e}"));
            assert_eq!(
                dec.dimensions(),
                (w, h),
                "patch SOF 后 dimensions 应为 {w}×{h}"
            );
            // B1：scaled == box 网格公式（顶左对齐 ceil/den）；den∉{2,4,8} → None
            for den in [2u8, 4, 8] {
                assert_eq!(
                    dec.scaled_dimensions(den),
                    Some((w.div_ceil(u32::from(den)), h.div_ceil(u32::from(den)))),
                    "B1 网格同构 ({w}×{h}) den={den}"
                );
            }
            for den in [1u8, 3, 5] {
                assert_eq!(dec.scaled_dimensions(den), None, "den={den} 应不支持 ({w}×{h})");
            }
        }
    }
    println!("B1 grid property: 全 w,h∈[1..20] × den∈{{2,4,8}} scaled==box 网格同构 ✓");

    // 端到端：以 12MP 420 语料（若存在）核对「声明（api）== 实际解码 == box 尺寸」。
    let target = dir.join("420-q100-12mp__texture-ground.jpg");
    if !target.is_file() {
        println!("SKIP: missing 12mp 语料（网格端到端用一般 420 样本替代）");
        return;
    }
    let bytes = std::fs::read(&target).unwrap();
    for den in [2u8, 4] {
        let (sw, sh, _ct, _s) = scaled_decode(&bytes, den).unwrap();
        let (bw, bh, _bct, _b) = box_ref(&bytes, den).unwrap();
        assert_eq!(sw, bw, "B1 网格同构：宽 den={den}");
        assert_eq!(sh, bh, "B1 网格同构：高 den={den}");
        println!("B1 grid 420-q100-12mp den={den} scaled={sw}×{sh} == box（顶左对齐）");
    }
}

/// 把 JPEG 的 SOF（SOF0 优先，落 `FF Cx` 段）宽高 patch 成 (w, h)：返回新字节。
/// 段布局 `FF C0 <len:2> <prec:1> <height:2> <width:2>`；只改字节，不动结构。
fn patch_sof_dims(bytes: &[u8], w: u32, h: u32) -> Option<Vec<u8>> {
    let mut i = 0usize;
    while i + 4 < bytes.len() {
        if bytes[i] == 0xFF {
            let m = bytes[i + 1];
            let is_sof = matches!(
                m,
                0xC0 | 0xC1 | 0xC2 | 0xC3 | 0xC5 | 0xC6 | 0xC7 | 0xC9 | 0xCA | 0xCB | 0xCD
                    | 0xCE | 0xCF
            );
            if is_sof {
                if i + 9 > bytes.len() || bytes[i + 2..i + 4].len() != 2 {
                    return None;
                }
                let mut out = bytes.to_vec();
                out[i + 5..i + 7].copy_from_slice(&(h as u16).to_be_bytes());
                out[i + 7..i + 9].copy_from_slice(&(w as u16).to_be_bytes());
                return Some(out);
            }
            // 有长度的段：跳过；无长度独立标记（TEM 01 / RST D0-D7 / SOI D8 / EOI D9）跳 2 字节
            if m == 0x01 || (0xD0..=0xD7).contains(&m) || m == 0xD8 || m == 0xD9 {
                i += 2;
            } else if i + 4 <= bytes.len() {
                let len = u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]);
                if len < 2 {
                    return None;
                }
                i += 2 + usize::from(len);
            } else {
                return None;
            }
        } else {
            i += 1;
        }
    }
    None
}

/// 全解字节稳定（B3 子集）：den=1 全解路径输出与 C# 参照一致——已有
/// `jpeg_csharp_parity` 硬闸全量兜底，本测试只防「scaled 参数污染 den=1 派生」的
/// 定向回归：对 420-q100-3mp 断言全解尺寸/颜色类型/缩放尺寸关系健在。
#[test]
fn den1_path_unpolluted_by_scaled() {
    let dir = corpus_dir();
    if !dir.is_dir() {
        println!("SKIP: missing {}", dir.display());
        return;
    }
    let target = dir.join("420-q100-3mp__landscape-almsee.jpg");
    if !target.is_file() {
        println!("SKIP: missing");
        return;
    }
    let bytes = std::fs::read(&target).unwrap();
    // 先走一次 scaled（触发 scaled_den 分支），再全解 → 断言尺寸/输出健在（共享
    // init_cs_tail/参数未被污染；B3 全量由 jpeg_csharp_parity 兜底）。
    for den in [2u8, 4, 8] {
        if let Ok((sw, sh, ct, px)) = scaled_decode(&bytes, den) {
            let (w, h, ct2, _px2) = full_decode(&bytes).unwrap();
            assert_eq!(ct, ct2, "scaled/全解 color_type 应一致");
            assert_eq!(
                (sw, sh),
                (w.div_ceil(u32::from(den)), h.div_ceil(u32::from(den))),
                "B1 缩放尺寸 den={den}"
            );
            assert_eq!(px.len(), sw as usize * sh as usize * usize::from(ct.bytes_per_pixel()));
        }
    }
    println!("B3 den=1 与 scaled 共享路径未污染（全解尺寸/色型健在）");
}

/// 探针：语料时钟（release `#[ignore]`，同窗 A/B 用）：scaled 解码 vs box 参照全链毫秒。
///
/// **口径**：`box_ref` = 全解 + **单线程** `box_downsample_n`（本测量器自有实现），
/// 不含生产的 `box_downsample_bgra_or_rgba_rayon` 并行降采样与 `to_rgba8` 展开——
/// 故此处「scaled vs 单线程 box」是**解码侧理想化上限**；生产端到端含 rayon 并行 box（全解
/// RGBA→BGRA 中间态 + 并行降档），box 侧会更快、比值会**缩水**。上报口径按
/// 「解码侧 scaled vs 单线程 box」标注，勿与其他端到端数字混用。
#[test]
#[ignore]
fn scaled_vs_box_wallclock() {
    use std::time::Instant;
    let dir = corpus_dir();
    if !dir.is_dir() {
        println!("SKIP: missing {}", dir.display());
        return;
    }
    let target = dir.join("420-q100-12mp__texture-ground.jpg");
    if !target.is_file() {
        println!("SKIP: missing 12mp");
        return;
    }
    let bytes = std::fs::read(&target).unwrap();
    let mut best_scaled = f64::MAX;
    let mut best_box = f64::MAX;
    let iters = 3;
    for i in 0..iters {
        let t = Instant::now();
        let _ = scaled_decode(&bytes, 2).unwrap();
        let ts = t.elapsed().as_secs_f64() * 1000.0;
        let t = Instant::now();
        let _ = box_ref(&bytes, 2).unwrap();
        let tb = t.elapsed().as_secs_f64() * 1000.0;
        if i > 0 {
            best_scaled = best_scaled.min(ts);
            best_box = best_box.min(tb);
        } else {
            best_scaled = ts;
            best_box = tb;
        }
        println!("SCALED_WALL iter{i}: scaled={ts:.1}ms box={tb:.1}ms");
    }
    println!(
        "SCALED_WALL best (warm, {iters} iters): scaled={best_scaled:.1}ms box={best_box:.1}ms"
    );
    assert!(best_scaled > 0.0, "不可达");
}

/// 探针（release `#[ignore]`）：大图（moe 50MP baseline / yande.re 48MP progressive）
/// den=4/8 scaled 解码时钟 + 与 box 参照的 PSNR。本任务验收焦点：release den=4
/// 50MP 应 <250ms（对照现全解 ~421ms）、debug <6.5s（对照现 ~10.4s）。
///
/// **口径**：box 参照 = 全解 + 单线程盒式（同 `scaled_vs_box_wallclock` 注释）。
/// 仅在点名运行（`--ignored --nocapture`），跑的是各自语料普通档（非 kimg 专项）。
#[test]
#[ignore]
fn big_image_scaled_wallclock_psnr() {
    use std::time::Instant;
    let dir = corpus_dir();
    if !dir.is_dir() {
        println!("SKIP: missing {}", dir.display());
        return;
    }
    // (文件名, den)：moe 50MP baseline 420 的 den=4；yande.re 48MP progressive 的 den=4。
    let targets: Vec<(&str, u8)> = vec![
        ("moe 46577 duel_dolls tinkle.jpg", 4),
        ("yande.re 226355 crease fixme loli lolita_fashion raw_scan tinkerbell tinkle.jpg", 4),
        ("moe 46577 duel_dolls tinkle.jpg", 8),
    ];
    for (name, den) in &targets {
        let p = dir.join(name);
        if !p.is_file() {
            println!("SKIP: missing {}", p.display());
            continue;
        }
        let bytes = std::fs::read(&p).unwrap();
        let t = Instant::now();
        let (sw, sh, ct, s) = match scaled_decode(&bytes, *den) {
            Ok(v) => v,
            Err(e) => {
                println!("{name} den={den}: scaled 失败 {e}");
                continue;
            }
        };
        let ts = t.elapsed().as_secs_f64() * 1000.0;
        let t = Instant::now();
        let (bw, bh, _bct, b) = match box_ref(&bytes, *den) {
            Ok(v) => v,
            Err(e) => {
                println!("{name} den={den}: box 参照失败 {e}");
                continue;
            }
        };
        let tb = t.elapsed().as_secs_f64() * 1000.0;
        let p = psnr(&s, &b);
        let ch = usize::from(ct.bytes_per_pixel());
        let es = highfreq_energy(&s, sw, sh, ch);
        let eb = highfreq_energy(&b, bw, bh, ch);
        let ratio = if eb > 1e-9 { es / eb } else { 1.0 };
        println!(
            "BIG_SCALED {name}: den={den} {sw}×{sh} scaled={ts:.1}ms box={tb:.1}ms \
             PSNR={p:.2}dB 高频能量比={ratio:.2}"
        );
    }
}
