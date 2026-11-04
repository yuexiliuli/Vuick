#[cfg(feature = "stage_profile")]
use super::*;

/// 阶段分账解码（`stage_profile` feature 门控；供 imgbench `vp8_stage_probe`，K59 统一基座）。
/// 与 [`decode_vp8`] 语义逐字节一致，另按【熵 / 预测 / 逆变换 / 环路滤波 / YUV 直出】统计各段
/// 墙钟（进程级原子，P-B 滤波线程路径跨线程累计）。`frame` 为 VP8 帧载荷。仅计时（返回 profile）。
#[cfg(feature = "stage_profile")]
pub fn stage_decode(frame: &[u8], width: u32, height: u32) -> Result<Vp8StageProfile> {
    use std::time::Instant;
    stage_clock::reset_profile();
    let t0 = Instant::now();
    let yuv = decode_vp8(frame, width, height)?;
    let total_ns = t0.elapsed().as_nanos() as u64;
    std::hint::black_box(&yuv.rgba);
    let (e, p, i, f, y, h, c, s, su, m, e2) = stage_clock::profile_totals();
    let sub = stage_clock::profile_sub();
    Ok(Vp8StageProfile {
        width,
        height,
        entropy_ns: e,
        predict_ns: p,
        idct_ns: i,
        filter_ns: f,
        yuv_ns: y,
        header_ns: h,
        copy_ns: c,
        scope_ns: s,
        setup_ns: su,
        mid_ns: m,
        end_ns: e2,
        total_ns,
        pipelined: filter_mt_enabled(
            (width as usize + 15) / 16,
            (height as usize + 15) / 16,
        ),
        idct_i16_ns: sub.idct_i16,
        idct_i4_ns: sub.idct_i4,
        idct_wht_ns: sub.idct_wht,
        idct_uv_ns: sub.idct_uv,
        i4_predict_ns: sub.i4_predict_ns,
        i4_idct_add_ns: sub.i4_idct_add_ns,
        i4_predict_blocks: sub.i4_predict_blocks,
        i4_idct_add_blocks: sub.i4_idct_add_blocks,
        i4_above_blocks: sub.i4_above_blocks,
        i4_fill0_blocks: sub.i4_fill0_blocks,
        i4_add_ac_blocks: sub.i4_add_ac_blocks,
        mb_i16: sub.mb_i16,
        mb_i4: sub.mb_i4,
        mb_wht: sub.mb_wht,
        ent_modes_ns: sub.ent_modes,
        ent_tokens_ns: sub.ent_tokens,
        modes_bits: sub.modes_bits,
        tokens_bits: sub.tokens_bits,
        acct_count: sub.acct,
        dq_count: sub.dq,
    })
}

/// 熵内部三类操作的单位耗时探针（`stage_profile` feature 门控；供 imgbench `vp8_stage_probe`
/// 把操作计数换算成时间占比，K53 测量口径）。返回每操作纳秒：`(bool 位读取, 记账·游标, 去量化+zigzag 写回)`。
/// 在同一（已插桩）构建里测量 → 与产线探针的分账自洽（换算分母与实测熵占比同源）。
#[cfg(feature = "stage_profile")]
pub fn probe_entropy_op_costs(reps: u32) -> (f64, f64, f64) {
    use std::time::Instant;
    // 1) bool 位读取：read_bit_val 实函数热循环（含 refill 分支，概率用 128 典型值）。
    let data = vec![0x55u8; 4096];
    let mut d = Vp8BoolDecoder::new(&data);
    let mut acc = 0u64;
    let t = Instant::now();
    for _ in 0..reps {
        let (s, b) = d.read_bit_val(128);
        d = s;
        acc = acc.wrapping_add(u64::from(b));
    }
    let c_bit = t.elapsed().as_nanos() as f64 / reps as f64;
    // 2) 记账·游标·索引：tokens 里 position/last/p_idx + BAND_ROW_OFFSETS 序列。
    let mut pos = 0usize;
    let mut last = 0usize;
    let mut p_i = 0usize;
    let t = Instant::now();
    for _ in 0..reps {
        pos += 1;
        if pos == 16 {
            pos = 0;
        }
        p_i = BAND_ROW_OFFSETS[pos] + 11;
        last = pos + 1;
    }
    std::hint::black_box((p_i, last, pos));
    let c_acct = t.elapsed().as_nanos() as f64 / reps as f64;
    // 3) 去量化+zigzag 写回：乘 factor + ZIGZAG 索引 + s16 存储。
    let mut out = [0i16; 16];
    let mut pos = 0usize;
    let mut signed = if (acc & 1) == 0 { 17i32 } else { -17i32 };
    let t = Instant::now();
    for _ in 0..reps {
        pos = (pos + 1) & 15;
        out[ZIGZAG[pos] as usize] = (signed.wrapping_mul(53)) as i16;
    }
    std::hint::black_box(&out);
    std::hint::black_box(signed);
    let c_dq = t.elapsed().as_nanos() as f64 / reps as f64;
    std::hint::black_box(acc);
    (c_bit, c_acct, c_dq)
}

/// i4 段内各操作的单位耗时（ns/op），供「原子计数 → 时间」折算。
#[cfg(feature = "stage_profile")]
#[derive(Default, Clone, Copy)]
pub struct I4OpCosts {
    /// `subblock_above` 一抽（含 block_y==0 / 中列 / 右列三分支）。
    pub c_above: f64,
    /// `predict_subblock` 本体（above 已备好；左列读取 + 10 模式写回，等权平均）。
    pub c_predict: f64,
    /// `inverse_dct_add(has_ac=false)` DC-only 标量捷径。
    pub c_add_dc: f64,
    /// `inverse_dct_add_one` has-ac 路径（SSE2 单块核，与产线同门控）。
    pub c_add_ac: f64,
    /// `[i16;16].fill(0)`。
    pub c_fill: f64,
}

/// i4 段内五类操作的单位耗时探针（`stage_profile` feature 门控；供 imgbench `vp8_stage_probe`
/// 把 predict / idct_add / fill(0) / subblock_above 原子计数折算成时间）。K83 修正方案
/// （2026-11-02）：per-block 墙钟插桩通胀（残差 ~18% 不可信）→ 外层 `idct_i4` 粗钟给总账，
/// 块内操作按本探针单位成本折算，差即循环壳净成本。与产线同构建测量（同 release / 同 feature），
/// 模拟单 i4 宏块 16 块暖缓存布局（pitch=17、块按 bx/by 顺序扫），代表真实解码 L1 缓存域。
#[cfg(feature = "stage_profile")]
pub fn probe_i4_op_costs(reps: u32) -> I4OpCosts {
    use std::time::Instant;
    let pitch = 17usize;
    let mut plane = vec![128u8; pitch * 64];
    let above_y = [42u8; 21];
    // 每块一个固定 above 行（形态同真实 `subblock_above` 输出；模式 0..10 等权循环）。
    let mut a_blk = [[0u8; 9]; 16];
    for (i, a) in a_blk.iter_mut().enumerate() {
        for (j, v) in a.iter_mut().enumerate() {
            *v = (i as u8).wrapping_mul(13).wrapping_add(j as u8);
        }
    }
    let modes = [0u8, 1, 2, 3, 4, 5, 6, 7, 8, 9, 0, 1, 2, 3, 4, 5];
    let mut acc = 0u64;

    // 1) subblock_above 本体（三分支按块位均摊）。
    let t = Instant::now();
    for k in 0..reps {
        let bx = (k & 3) as usize;
        let by = ((k >> 2) & 3) as usize;
        let a = subblock_above(&plane, pitch, by * 4, bx * 4, bx, by, &above_y);
        acc = acc.wrapping_add(u64::from(a[0] + a[8]));
    }
    std::hint::black_box(&plane);
    let c_above = t.elapsed().as_nanos() as f64 / reps as f64;

    // 2) predict_subblock 本体（above 已备好，仅左列读取 + 写回）。
    let t = Instant::now();
    for k in 0..reps {
        let bx = (k & 3) as usize;
        let by = ((k >> 2) & 3) as usize;
        let i = (k & 15) as usize;
        super::simd::predict_subblock(&mut plane, pitch, by * 4, bx * 4, modes[i], &a_blk[i]);
    }
    std::hint::black_box(&plane);
    let c_predict = t.elapsed().as_nanos() as f64 / reps as f64;

    // 3) inverse_dct_add DC-only 标量捷径（has_ac=false，非零 DC）。
    let mut coeff = [0i16; 16];
    coeff[0] = 19;
    let t = Instant::now();
    for k in 0..reps {
        let bx = (k & 3) as usize;
        let by = ((k >> 2) & 3) as usize;
        inverse_dct_add(&coeff, &mut plane, pitch, by * 4, bx * 4, false);
    }
    std::hint::black_box(&plane);
    let c_add_dc = t.elapsed().as_nanos() as f64 / reps as f64;

    // 4) inverse_dct_add_one（has-ac，AC 系数非零；SSE2 单块核，与产线同门控）。
    let mut c = [0i16; 16];
    c[0] = 17;
    c[1] = 23;
    c[2] = -9;
    c[3] = 11;
    c[4] = -7;
    let t = Instant::now();
    for k in 0..reps {
        let bx = (k & 3) as usize;
        let by = ((k >> 2) & 3) as usize;
        super::simd::inverse_dct_add_one(&c, &mut plane, pitch, by * 4, bx * 4);
    }
    std::hint::black_box(&plane);
    let c_add_ac = t.elapsed().as_nanos() as f64 / reps as f64;

    // 5) [i16;16].fill(0)。
    let mut blk = [7i16; 16];
    let t = Instant::now();
    for _ in 0..reps {
        std::hint::black_box(&mut blk);
        blk.fill(0);
        acc = acc.wrapping_add(u64::from(blk[0] as u16));
    }
    std::hint::black_box(&blk);
    let c_fill = t.elapsed().as_nanos() as f64 / reps as f64;

    std::hint::black_box(acc);
    I4OpCosts {
        c_above,
        c_predict,
        c_add_dc,
        c_add_ac,
        c_fill,
    }
}

/// 探针自身开销校准（`stage_profile` feature 门控）：i4 段外层粗钟是 per-MB `Instant::now()`、
/// 段内是 per-block Relaxed 原子计数 —— 二者本身都落在粗钟总账内，须从 shell 中扣除以得
/// 「真循环壳」。返回 ns：`(单次同线程 Relaxed fetch_add, 单次 Instant::now())`。
#[cfg(feature = "stage_profile")]
pub fn probe_clock_costs(reps: u32) -> (f64, f64) {
    use std::time::Instant;
    use std::sync::atomic::AtomicU64;
    let cnt = AtomicU64::new(0);
    let t = Instant::now();
    for _ in 0..reps {
        cnt.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    let c_atomic = t.elapsed().as_nanos() as f64 / reps as f64;
    std::hint::black_box(cnt.load(std::sync::atomic::Ordering::Relaxed));
    let t = Instant::now();
    for _ in 0..reps {
        std::hint::black_box(Instant::now());
    }
    let c_instant = t.elapsed().as_nanos() as f64 / reps as f64;
    (c_atomic, c_instant)
}
