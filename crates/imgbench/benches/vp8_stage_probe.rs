//! VP8 有损解码阶段分账探针（K59 统一基座 / K53 测量先行 / K60 决策闸）。
//!
//! 在有损 corpus（imgbench-corpus/WEBP 的 `gen28-photo-lossy-q75.webp` 2400×3600 照片类 + 各
//! 中小图）上量【熵(token 解析+bool+去量化) / 帧内预测 / 逆变换(IDCT) / 环路滤波 / YUV 直出
//! (fancy 上采样+YUV→RGB)】各自墙钟占比 + 全链，release 多轮中位。
//! 前置数字用于 K60 决策闸：先钉量级再定优化对象（top 1-2 阶段）。
//!
//! 运行：
//! ```text
//! cargo bench -p imgbench --bench vp8_stage_probe --features stage-profile
//! ```
//! 未带 `stage-profile` feature 时输出 `N/A` 提示（kimg 探针入口以该 feature 门控）。
//! 输出 `VP8_STAGE_PROBE <file> wxh entropy=..ms(..%) predict=..ms(..%) idct=..ms(..%)
//! filter=..ms(..%) yuv=..ms(..%) inner_sum=..ms raw_wall=..ms alpha=false pipeline=true`
//! （加 `--features libwebp-ref` 时附 `lw_total=..ms lw_ratio=..`：同窗真机 libwebp 1.6.0 SIMD
//! 单文件全链实测，直接对照「ours 各段 vs C 库全链」，K68 同窗 A/B 语义）。
//!
//! 口径说明：`filter`/`yuv` 在宽图自动走 P-B 滤波线程管线（独立线程累计，进程级原子），
//! `raw_wall` 为主线程墙钟（含帧头解析与平面拷贝）；熵/预测/IDCT 恒在主线程。
//! 2026-11-01 新 MT 架构落地后：熵仍恒在 t1（主线程），预测/IDCT/滤波/YUV 恒在 t2（重建线程）；
//! 分账原子跨线程累计故各段占比不变，`total_ns` 为真实墙钟（before/after 对比主看此列）。

use std::time::Instant;

const ROUNDS: usize = 9;

/// 语料定位（镜像 tests/fixtures/webp/mod.rs 的目录优先级；bench 非 test 构建不能复用其
/// `#[cfg(test)]` 模块）：`KIMG_WEBP_FIXTURES`（`;` 分隔多目录）→ `target/imgbench-corpus/WEBP`
/// → `E:\Rust\SimdCodecs\测试图片\WEBP` → SimdCodecs 上游 Fixtures\webp。
fn corpus_dirs() -> Vec<std::path::PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(val) = std::env::var("KIMG_WEBP_FIXTURES") {
        for p in val.split(';') {
            let p = p.trim();
            if !p.is_empty() {
                dirs.push(std::path::PathBuf::from(p));
            }
        }
    }
    dirs.push(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("target")
            .join("imgbench-corpus")
            .join("WEBP"),
    );
    dirs.push(std::path::PathBuf::from(r"E:\Rust\SimdCodecs\测试图片\WEBP"));
    dirs.push(std::path::PathBuf::from(
        r"E:\Rust\SimdCodecs\tests\Sdcb.SimdCodecs.Tests\Fixtures\webp",
    ));
    dirs
}

/// 收集静态有损（VP8 帧）文件：镜像 aggregate bench 的语料（三目录全收，不去重）＋
/// `KIMG_WEBP_FIXTURES`。按像素数降序取 top-N（≤N 时全收）。
#[cfg(feature = "stage-profile")]
fn collect_lossy() -> Vec<(String, Vec<u8>, u32, u32, bool)> {
    let mut out: Vec<(String, Vec<u8>, u32, u32, bool)> = Vec::new();
    for dir in corpus_dirs() {
        if !dir.is_dir() {
            continue;
        }
        for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let p = e.path();
            if p.extension().map(|x| x == "webp").unwrap_or(false) {
                let Ok(b) = std::fs::read(&p) else { continue };
                if let Some((frame, w, h, has_alpha)) = extract(&b) {
                    if frame.len() >= 1 << 10 {
                        out.push((
                            p.file_name().unwrap().to_string_lossy().into_owned(),
                            frame,
                            w,
                            h,
                            has_alpha,
                        ));
                    }
                }
            }
        }
    }
    out.sort_by(|a, b| (b.2 as u64 * b.3 as u64).cmp(&(a.2 as u64 * a.3 as u64)));
    out.truncate(130);
    // 子集控制（K53 定向/复现）：`KIMG_WEBP_STAGE_SUBSET` = 数量 N（取像素最大前 N）或
    // `;` 分隔文件名（精确匹配）。缺省全收。
    if let Ok(sel) = std::env::var("KIMG_WEBP_STAGE_SUBSET") {
        let sel = sel.trim();
        if let Ok(n) = sel.parse::<usize>() {
            out.truncate(n);
        } else {
            let names: Vec<String> = sel.split(';').map(|s| s.trim().to_string()).collect();
            out.retain(|(n, _, _, _, _)| names.iter().any(|x| x == n));
        }
    }
    out
}

#[cfg(feature = "stage-profile")]
fn extract(b: &[u8]) -> Option<(Vec<u8>, u32, u32, bool)> {
    kimg::codecs::webp::vp8_extract_frame(b)
}

/// 逐字段取多轮中位（每轮一次完整解码）。
#[cfg(feature = "stage-profile")]
fn median_profile(frame: &[u8], w: u32, h: u32, rounds: usize) -> kimg::codecs::webp::Vp8StageProfile {
    let _ = kimg::codecs::webp::vp8_stage_decode(frame, w, h);
    let mut acc: Vec<_> = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        let t = Instant::now();
        let mut p = kimg::codecs::webp::vp8_stage_decode(frame, w, h)
            .unwrap_or_else(|e| panic!("vp8_stage_decode 失败: {e}"));
        let wall = t.elapsed().as_nanos() as u64;
        // 以本轮外部墙钟为准（内部段之和 ≈ 外部墙钟；原始墙钟供占比分母另列）。
        p.total_ns = wall;
        acc.push(p);
    }
    let mut out = kimg::codecs::webp::Vp8StageProfile::default();
    for f in [
        "entropy_ns",
        "predict_ns",
        "idct_ns",
        "filter_ns",
        "yuv_ns",
        "header_ns",
        "copy_ns",
        "scope_ns",
        "setup_ns",
        "mid_ns",
        "end_ns",
        "total_ns",
        "idct_i16_ns",
        "idct_i4_ns",
        "idct_wht_ns",
        "idct_uv_ns",
        "mb_i16",
        "mb_i4",
        "mb_wht",
        "ent_modes_ns",
        "ent_tokens_ns",
        "modes_bits",
        "tokens_bits",
        "acct_count",
        "dq_count",
    ] {
        let mut v: Vec<u64> = acc.iter().map(|p| field(p, f)).collect();
        v.sort_unstable();
        set_field(&mut out, f, v[v.len() / 2]);
    }
    out.width = w;
    out.height = h;
    out.pipelined = acc[0].pipelined;
    out
}

#[cfg(feature = "stage-profile")]
fn field(p: &kimg::codecs::webp::Vp8StageProfile, f: &str) -> u64 {
    match f {
        "entropy_ns" => p.entropy_ns,
        "predict_ns" => p.predict_ns,
        "idct_ns" => p.idct_ns,
        "filter_ns" => p.filter_ns,
        "yuv_ns" => p.yuv_ns,
        "header_ns" => p.header_ns,
        "copy_ns" => p.copy_ns,
        "scope_ns" => p.scope_ns,
        "setup_ns" => p.setup_ns,
        "mid_ns" => p.mid_ns,
        "end_ns" => p.end_ns,
        "total_ns" => p.total_ns,
        "idct_i16_ns" => p.idct_i16_ns,
        "idct_i4_ns" => p.idct_i4_ns,
        "idct_wht_ns" => p.idct_wht_ns,
        "idct_uv_ns" => p.idct_uv_ns,
        "mb_i16" => p.mb_i16,
        "mb_i4" => p.mb_i4,
        "mb_wht" => p.mb_wht,
        "ent_modes_ns" => p.ent_modes_ns,
        "ent_tokens_ns" => p.ent_tokens_ns,
        "modes_bits" => p.modes_bits,
        "tokens_bits" => p.tokens_bits,
        "acct_count" => p.acct_count,
        "dq_count" => p.dq_count,
        _ => unreachable!(),
    }
}

#[cfg(feature = "stage-profile")]
fn set_field(p: &mut kimg::codecs::webp::Vp8StageProfile, f: &str, v: u64) {
    match f {
        "entropy_ns" => p.entropy_ns = v,
        "predict_ns" => p.predict_ns = v,
        "idct_ns" => p.idct_ns = v,
        "filter_ns" => p.filter_ns = v,
        "yuv_ns" => p.yuv_ns = v,
        "header_ns" => p.header_ns = v,
        "copy_ns" => p.copy_ns = v,
        "scope_ns" => p.scope_ns = v,
        "setup_ns" => p.setup_ns = v,
        "mid_ns" => p.mid_ns = v,
        "end_ns" => p.end_ns = v,
        "total_ns" => p.total_ns = v,
        "idct_i16_ns" => p.idct_i16_ns = v,
        "idct_i4_ns" => p.idct_i4_ns = v,
        "idct_wht_ns" => p.idct_wht_ns = v,
        "idct_uv_ns" => p.idct_uv_ns = v,
        "mb_i16" => p.mb_i16 = v,
        "mb_i4" => p.mb_i4 = v,
        "mb_wht" => p.mb_wht = v,
        "ent_modes_ns" => p.ent_modes_ns = v,
        "ent_tokens_ns" => p.ent_tokens_ns = v,
        "modes_bits" => p.modes_bits = v,
        "tokens_bits" => p.tokens_bits = v,
        "acct_count" => p.acct_count = v,
        "dq_count" => p.dq_count = v,
        _ => unreachable!(),
    }
}

fn main() {
    let profile = if cfg!(debug_assertions) { "debug" } else { "release" };
    let have_probe = cfg!(feature = "stage-profile");
    println!(
        "VP8_STAGE_PROBE profile={profile} rounds={ROUNDS} stage_profile={have_probe} \
         libwebp_ref={}",
        cfg!(feature = "libwebp-ref")
    );
    #[cfg(feature = "stage-profile")]
    {
        let files = collect_lossy();
        if files.is_empty() {
            println!(
                "VP8_STAGE_PROBE corpus SKIP (webp 语料目录无 ≥16KB VP8 有损帧；\
                 设 KIMG_WEBP_FIXTURES=... )"
            );
            return;
        }
        run_probe(&files);
    }
    #[cfg(not(feature = "stage-profile"))]
    {
        println!("VP8_STAGE_PROBE N/A (需 --features stage-profile 启用 kimg 阶段计时)");
    }
    eprintln!("[vp8_stage_probe] done");
}

/// libwebp 全链耗时中位（`libwebp-ref` feature 门控；缺则 None）。每轮完整 `WebPDecodeRGBA`
/// （与 aggregate bench 同通道），多轮中位便于同窗对比。
#[cfg(feature = "libwebp-ref")]
fn libwebp_median_ms(data: &[u8], rounds: usize) -> Option<f64> {
    use libwebp_sys as lw;
    let mut w = 0i32;
    let mut h = 0i32;
    unsafe {
        let ptr = lw::WebPDecodeRGBA(data.as_ptr(), data.len(), &mut w, &mut h);
        if ptr.is_null() {
            return None;
        }
        // 暖机一轮。
        let size = (w as usize) * (h as usize) * 4;
        let out = std::slice::from_raw_parts(ptr, size).to_vec();
        std::hint::black_box(&out);
        lw::WebPFree(ptr as *mut std::ffi::c_void);
        let mut acc: Vec<f64> = Vec::with_capacity(rounds);
        for _ in 0..rounds {
            let t = Instant::now();
            let ptr = lw::WebPDecodeRGBA(data.as_ptr(), data.len(), &mut w, &mut h);
            let size = (w as usize) * (h as usize) * 4;
            let out = std::slice::from_raw_parts(ptr, size).to_vec();
            std::hint::black_box(&out);
            acc.push(t.elapsed().as_secs_f64() * 1000.0);
            lw::WebPFree(ptr as *mut std::ffi::c_void);
        }
        acc.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap());
        Some(acc[acc.len() / 2])
    }
}
#[cfg(not(feature = "libwebp-ref"))]
fn libwebp_median_ms(_data: &[u8], _rounds: usize) -> Option<f64> {
    None
}

#[cfg(feature = "stage-profile")]
fn run_probe(files: &[(String, Vec<u8>, u32, u32, bool)]) {
    // Task A/B 细分：熵内部三类操作的单位耗时校准（与插桩构建同源，K53 换算口径）。
    let (c_bit, c_acct, c_dq) = kimg::codecs::webp::vp8_probe_entropy_op_costs(2_000_000);
    println!(
        "VP8_ENT_OPCOST c_bit={:.3}ns c_acct={:.3}ns c_dq={:.3}ns",
        c_bit, c_acct, c_dq
    );
    let mut tot_entropy = 0.0f64;
    let mut tot_idct = 0.0f64;
    let mut tot_total = 0.0f64;
    let mut tot_cut = 0.0f64; // Task B：ceiling 墙钟相对现墙钟的切割%（按文件（名义全链）等权平均）
    let mut tot_cut_new = 0.0f64; // 新 MT 架构模型（t1=熵、t2=pred+idct+filter+yuv）的等权平均切割%
    let mut tot_idct_i16 = 0.0f64;
    let mut tot_idct_i4 = 0.0f64;
    let mut tot_i4_ratio = 0.0f64;
    let mut tot_bool = 0.0f64;
    let mut tot_acct = 0.0f64;
    let mut tot_dq = 0.0f64;
    for (name, frame, w, h, has_alpha) in files {
        let p = median_profile(frame, *w, *h, ROUNDS);
        let ms = |n: u64| n as f64 / 1e6;
        // 干净全链 = 各段之和；原始墙钟另列。
        let sum = p.entropy_ns + p.predict_ns + p.idct_ns + p.filter_ns + p.yuv_ns;
        let pct = |n: u64| n as f64 / sum as f64 * 100.0;
        // Task A 细分（IDCT 内部）：i16「2 块 SSE2 成对」 vs i4x4 标量逐块（含子块预测） vs WHT vs UV。
        {
            let idc = p.idct_i16_ns + p.idct_i4_ns + p.idct_wht_ns + p.idct_uv_ns;
            let ipc = |n: u64| n as f64 / idc.max(1) as f64 * 100.0;
            let mb_total = (p.mb_i16 + p.mb_i4).max(1);
            println!(
                "VP8_IDCT_SUB {name} {}x{} idct={:.3}ms i16_pairs={:.3}ms({:.1}%) i4_scalar={:.3}ms({:.1}%) wht={:.3}ms({:.1}%) uv={:.3}ms({:.1}%) mb_i16={} mb_i4={} mb_wht={} i4_ratio={:.1}%",
                w, h, ms(p.idct_ns), ms(p.idct_i16_ns), ipc(p.idct_i16_ns),
                ms(p.idct_i4_ns), ipc(p.idct_i4_ns), ms(p.idct_wht_ns), ipc(p.idct_wht_ns),
                ms(p.idct_uv_ns), ipc(p.idct_uv_ns), p.mb_i16, p.mb_i4, p.mb_wht,
                p.mb_i4 as f64 / mb_total as f64 * 100.0
            );
            tot_idct_i16 += p.idct_i16_ns as f64 / idc.max(1) as f64;
            tot_idct_i4 += p.idct_i4_ns as f64 / idc.max(1) as f64;
            tot_i4_ratio += p.mb_i4 as f64 / mb_total as f64;
        }
        // Task B 细分（熵内部）：bool 读 / token 记账·游标·索引 / 去量化+zigzag 写回。
        // 换算 = 操作计数 × 校准单位耗时（模式+残差 bool 读同源计数）。
        {
            let bt = (p.modes_bits + p.tokens_bits) as f64 * c_bit;
            let at = p.acct_count as f64 * c_acct;
            let dt = p.dq_count as f64 * c_dq;
            let et = (bt + at + dt).max(1e-9);
            println!(
                "VP8_ENT_SUB {name} {}x{} ent={:.3}ms modes={:.3}ms({:.1}%) tokens={:.3}ms({:.1}%) bits_mode={} bits_token={} acct={} dq={} bool_read={:.1}% token_acct={:.1}% dequant_zigzag={:.1}%",
                w, h, ms(p.entropy_ns), ms(p.ent_modes_ns),
                p.ent_modes_ns as f64 / p.entropy_ns.max(1) as f64 * 100.0,
                ms(p.ent_tokens_ns),
                p.ent_tokens_ns as f64 / p.entropy_ns.max(1) as f64 * 100.0,
                p.modes_bits, p.tokens_bits, p.acct_count, p.dq_count,
                bt / et * 100.0, at / et * 100.0, dt / et * 100.0
            );
            tot_bool += bt / et;
            tot_acct += at / et;
            tot_dq += dt / et;
        }
        // B：每线程忙时（Task B，线程拆分天花板）。pipelined 下 filter/yuv 在滤波线程累计
        // （重粒子 yuv≈19% + filter≈13% 均在 thread2），熵/预测/IDCT 恒在主线程。
        // 拆分上限：把 IDCT+重建劈到 thread2 后，墙钟 ≈ max(主线程剩熵+预测, 滤波线程
        // 扩容后 filter+yuv+idct)。
        {
            let t1_busy = p.entropy_ns + p.predict_ns + p.idct_ns;
            let t2_busy = if p.pipelined { p.filter_ns + p.yuv_ns } else { 0 };
            let t1_split = p.entropy_ns + p.predict_ns; // 拆 IDCT 后主线程剩余
            let t2_split = p.filter_ns + p.yuv_ns + p.idct_ns; // 滤波线程扩容后忙时
            let wall_split = t1_split.max(t2_split);
            println!(
                "THREAD_SPLIT {name} {}x{} t1_busy={:.3}ms t2_busy={:.3}ms wall={:.3}ms \
                 split_entropy+t2={:.3}ms ceiling_wall'={:.3}ms ceiling_cut={:.1}% pipelined={}",
                w,
                h,
                ms(t1_busy),
                ms(t2_busy),
                ms(p.total_ns),
                ms(t1_split),
                ms(wall_split),
                (1.0 - wall_split as f64 / p.total_ns as f64) * 100.0,
                p.pipelined,
            );
            // 新 MT 架构模型（2026-11-01 落地后语义）：t1=串行熵（熵解码无法并行，bool 态串行），
            // t2=预测+IDCT+重建+滤波+YUV。ceiling' = max(entropy, t2_all)；actual_wall=total_ns 真实墙钟，
            // actual_cut=真实墙钟相对「各段之和」的理论上界的切割（多轮中位，K53 测量先行）。
            let t1_ent = p.entropy_ns;
            let t2_all = p.predict_ns + p.idct_ns + p.filter_ns + p.yuv_ns;
            let wall_new = t1_ent.max(t2_all);
            println!(
                "THREAD_SPLIT2 {name} {}x{} t1_entropy={:.3}ms t2_payload={:.3}ms \
                 ceiling_wall'={:.3}ms ceiling_cut={:.1}% actual_wall={:.3}ms actual_vs_ceiling={:.3}",
                w,
                h,
                ms(t1_ent),
                ms(t2_all),
                ms(wall_new),
                (1.0 - wall_new as f64 / p.total_ns as f64) * 100.0,
                ms(p.total_ns),
                ms(p.total_ns) / ms(wall_new).max(1e-9),
            );
            tot_cut += (1.0 - wall_split as f64 / sum as f64) * 100.0;
            tot_cut_new += (1.0 - wall_new as f64 / sum as f64) * 100.0;
        }
        // libwebp 需要完整 .webp 容器而非抽出的 VP8 帧：用 corpus 原始路径重新读（仅统计用，
        // 与 `frame` 同文件；代价是一次磁盘读，可忽略）。
        let lw_total = {
            let mut raw = None;
            for dir in corpus_dirs() {
                let p = dir.join(name);
                if let Ok(b) = std::fs::read(&p) {
                    raw = Some(b);
                    break;
                }
            }
            raw.and_then(|b| libwebp_median_ms(&b, ROUNDS))
        };
        match lw_total {
            Some(lwt) => println!(
                "VP8_STAGE_PROBE {name} {}x{} entropy={:.3}ms({:.1}%) predict={:.3}ms({:.1}%) \
                 idct={:.3}ms({:.1}%) filter={:.3}ms({:.1}%) yuv={:.3}ms({:.1}%) \
                 inner_sum={:.3}ms raw_wall={:.3}ms head={:.3}ms scope_w={:.3}ms copy={:.3}ms setup={:.3}ms \
                 lw_total={:.3}ms lw_ratio={:.2} alpha={} pipeline={}",
                w,
                h,
                ms(p.entropy_ns),
                pct(p.entropy_ns),
                ms(p.predict_ns),
                pct(p.predict_ns),
                ms(p.idct_ns),
                pct(p.idct_ns),
                ms(p.filter_ns),
                pct(p.filter_ns),
                ms(p.yuv_ns),
                pct(p.yuv_ns),
                ms(sum),
                ms(p.total_ns),
                ms(p.header_ns),
                ms(p.scope_ns),
                ms(p.copy_ns),
                ms(p.setup_ns),
                lwt,
                ms(p.total_ns) / lwt,
                has_alpha,
                p.pipelined,
            ),
            None => println!(
                "VP8_STAGE_PROBE {name} {}x{} entropy={:.3}ms({:.1}%) predict={:.3}ms({:.1}%) \
                 idct={:.3}ms({:.1}%) filter={:.3}ms({:.1}%) yuv={:.3}ms({:.1}%) \
                 inner_sum={:.3}ms raw_wall={:.3}ms head={:.3}ms scope_w={:.3}ms copy={:.3}ms setup={:.3}ms alpha={} pipeline={}",
                w,
                h,
                ms(p.entropy_ns),
                pct(p.entropy_ns),
                ms(p.predict_ns),
                pct(p.predict_ns),
                ms(p.idct_ns),
                pct(p.idct_ns),
                ms(p.filter_ns),
                pct(p.filter_ns),
                ms(p.yuv_ns),
                pct(p.yuv_ns),
                ms(sum),
                ms(p.total_ns),
                ms(p.header_ns),
                ms(p.scope_ns),
                ms(p.copy_ns),
                ms(p.setup_ns),
                has_alpha,
                p.pipelined,
            ),
        }
        tot_entropy += p.entropy_ns as f64 / sum as f64;
        tot_idct += p.idct_ns as f64 / sum as f64;
        tot_total += ms(sum);
    }
    let n = files.len() as f64;
    println!(
        "VP8_STAGE_PROBE SUMMARY files={} mean_entropy_share={:.1}% mean_idct_share={:.1}% \
         mean_fullchain={:.3}ms",
        files.len(),
        tot_entropy / n * 100.0,
        tot_idct / n * 100.0,
        tot_total / n
    );
    println!(
        "THREAD_SPLIT SUMMARY files={} mean_ceiling_cut={:.1}% (split IDCT+recon to thread2, \
         wall'≈max(entropy+predict, filter+yuv+idct))",
        files.len(),
        tot_cut / n
    );
    println!(
        "THREAD_SPLIT2 SUMMARY files={} mean_ceiling_cut2={:.1}% (new MT: t1=entropy, \
         wall'≈max(entropy, predict+idct+filter+yuv))",
        files.len(),
        tot_cut_new / n
    );
    println!(
        "VP8_IDCT_SUB SUMMARY files={} mean_i16_pairs_idctshare={:.1}% mean_i4_scalar_idctshare={:.1}% mean_i4_mb_ratio={:.1}%",
        files.len(),
        tot_idct_i16 / n * 100.0,
        tot_idct_i4 / n * 100.0,
        tot_i4_ratio / n * 100.0
    );
    println!(
        "VP8_ENT_SUB SUMMARY files={} mean_bool_read_share={:.1}% mean_token_acct_share={:.1}% mean_dequant_zigzag_share={:.1}%",
        files.len(),
        tot_bool / n * 100.0,
        tot_acct / n * 100.0,
        tot_dq / n * 100.0
    );
    eprintln!("[vp8_stage_probe] done");
}