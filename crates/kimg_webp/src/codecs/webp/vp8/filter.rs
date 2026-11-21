#![forbid(unsafe_code)]
// 环路滤波标量内核参数多、逐样本坐标传参，具名构整改动签名；row_filters 按下标循环需平移语义 → 放行。
#![allow(clippy::too_many_arguments, clippy::needless_range_loop)]
// 本文件零裸操作：SIMD 熔合路径的指针派点 + 内核调用下沉 `simd::raw::lf_filter_macroblock_safe`
// （`#[inline(always)]` 安全门面）；标量路径全部安全切片读写。

use super::*;

// ---------------------------------------------------------------------------
// V3：环路滤波（RFC 6386 §15，标量；C# `Vp8LoopFilter.cs`/标量 FilterEdge 同款算术）

/// 有符号 8bit 饱和：`p+delta` 到 [-128,127]。
#[inline]
fn clamp_signed(v: i32) -> i32 {
    v.clamp(-128, 127)
}

/// RFC 6386 §15.2：偏置后的调整钳到 int8 后右移 3（先钳再移，防止 124/125 移位到 16）。
#[inline]
fn filter_tap(adjustment: i32, bias: i32) -> i32 {
    clamp_signed(adjustment + bias) >> 3
}

/// 简单滤波（只动 p0/q0）。`center` 为存储下标，`sample_step`=相邻 p/q 步长（纵 1 / 横 stride），
/// `pixel_advance`=相邻样本步长（纵 stride / 横 1）。
fn filter_simple_scalar(
    plane: &mut [u8],
    center: usize,
    sample_step: usize,
    pixel_advance: usize,
    length: usize,
    edge: i32,
) {
    for i in 0..length {
        let sample = center + i * pixel_advance;
        let p1 = i32::from(plane[sample - 2 * sample_step]);
        let p0 = i32::from(plane[sample - sample_step]);
        let q0 = i32::from(plane[sample]);
        let q1 = i32::from(plane[sample + sample_step]);
        if (p0 - q0).abs() * 2 + (p1 - q1).abs() / 2 > edge {
            continue;
        }
        let adjustment = clamp_signed(clamp_signed(p1 - q1) + 3 * (q0 - p0));
        let q_adj = filter_tap(adjustment, 4);
        let p_adj = filter_tap(adjustment, 3);
        plane[sample - sample_step] = (p0 + p_adj).clamp(0, 255) as u8;
        plane[sample] = (q0 - q_adj).clamp(0, 255) as u8;
    }
}

/// 宏块边界 Normal 滤波（hv 高方差只动 p0/q0，否则 27/18/9 加权铺到 p2..q2）。
fn filter_normal_macroblock_scalar(
    plane: &mut [u8],
    center: usize,
    sample_step: usize,
    pixel_advance: usize,
    length: usize,
    interior: i32,
    edge: i32,
    hev: i32,
) {
    for i in 0..length {
        let sample = center + i * pixel_advance;
        let p3 = i32::from(plane[sample - 4 * sample_step]);
        let p2 = i32::from(plane[sample - 3 * sample_step]);
        let p1 = i32::from(plane[sample - 2 * sample_step]);
        let p0 = i32::from(plane[sample - sample_step]);
        let q0 = i32::from(plane[sample]);
        let q1 = i32::from(plane[sample + sample_step]);
        let q2 = i32::from(plane[sample + 2 * sample_step]);
        let q3 = i32::from(plane[sample + 3 * sample_step]);
        if (p0 - q0).abs() * 2 + (p1 - q1).abs() / 2 > edge
            || (p3 - p2).abs() > interior
            || (p2 - p1).abs() > interior
            || (p1 - p0).abs() > interior
            || (q1 - q0).abs() > interior
            || (q2 - q1).abs() > interior
            || (q3 - q2).abs() > interior
        {
            continue;
        }
        let high_variance = (p1 - p0).abs() > hev || (q1 - q0).abs() > hev;
        let adjustment = clamp_signed(clamp_signed(p1 - q1) + 3 * (q0 - p0));
        if high_variance {
            let q_adj = filter_tap(adjustment, 4);
            let p_adj = filter_tap(adjustment, 3);
            plane[sample - sample_step] = (p0 + p_adj).clamp(0, 255) as u8;
            plane[sample] = (q0 - q_adj).clamp(0, 255) as u8;
            continue;
        }
        let outer = (27 * adjustment + 63) >> 7;
        let middle = (18 * adjustment + 63) >> 7;
        let inn = (9 * adjustment + 63) >> 7;
        plane[sample - sample_step] = (p0 + outer).clamp(0, 255) as u8;
        plane[sample] = (q0 - outer).clamp(0, 255) as u8;
        plane[sample - 2 * sample_step] = (p1 + middle).clamp(0, 255) as u8;
        plane[sample + sample_step] = (q1 - middle).clamp(0, 255) as u8;
        plane[sample - 3 * sample_step] = (p2 + inn).clamp(0, 255) as u8;
        plane[sample + 2 * sample_step] = (q2 - inn).clamp(0, 255) as u8;
    }
}

/// 内部边 Normal 滤波（hv 时 adjustment 含 p1-q1，否则只有 3(q0-p0)）。
fn filter_normal_inner_scalar(
    plane: &mut [u8],
    center: usize,
    sample_step: usize,
    pixel_advance: usize,
    length: usize,
    interior: i32,
    edge: i32,
    hev: i32,
) {
    for i in 0..length {
        let sample = center + i * pixel_advance;
        let p3 = i32::from(plane[sample - 4 * sample_step]);
        let p2 = i32::from(plane[sample - 3 * sample_step]);
        let p1 = i32::from(plane[sample - 2 * sample_step]);
        let p0 = i32::from(plane[sample - sample_step]);
        let q0 = i32::from(plane[sample]);
        let q1 = i32::from(plane[sample + sample_step]);
        let q2 = i32::from(plane[sample + 2 * sample_step]);
        let q3 = i32::from(plane[sample + 3 * sample_step]);
        if (p0 - q0).abs() * 2 + (p1 - q1).abs() / 2 > edge
            || (p3 - p2).abs() > interior
            || (p2 - p1).abs() > interior
            || (p1 - p0).abs() > interior
            || (q1 - q0).abs() > interior
            || (q2 - q1).abs() > interior
            || (q3 - q2).abs() > interior
        {
            continue;
        }
        let high_variance = (p1 - p0).abs() > hev || (q1 - q0).abs() > hev;
        let adjustment = if high_variance {
            clamp_signed(clamp_signed(p1 - q1) + 3 * (q0 - p0))
        } else {
            clamp_signed(3 * (q0 - p0))
        };
        let q_adj = filter_tap(adjustment, 4);
        let p_adj = filter_tap(adjustment, 3);
        plane[sample - sample_step] = (p0 + p_adj).clamp(0, 255) as u8;
        plane[sample] = (q0 - q_adj).clamp(0, 255) as u8;
        if high_variance {
            continue;
        }
        let inn = (q_adj + 1) >> 1;
        plane[sample - 2 * sample_step] = (p1 + inn).clamp(0, 255) as u8;
        plane[sample + sample_step] = (q1 - inn).clamp(0, 255) as u8;
    }
}

/// 单条边滤波（中心为存储下标：`y*stride + 1 + x`）。
/// `pub(crate)`：`webp::simd` 的向量版以此为标量回退/对拍基准（D2）。
#[allow(clippy::too_many_arguments)]
pub(crate) fn filter_edge(
    plane: &mut [u8],
    stride: usize,
    x: usize,
    y: usize,
    length: usize,
    vertical: bool,
    macroblock_edge: bool,
    simple: bool,
    interior: i32,
    edge: i32,
    hev: i32,
) {
    let center = y * stride + 1 + x;
    let sample_step = if vertical { 1 } else { stride };
    let pixel_advance = if vertical { stride } else { 1 };
    if simple {
        filter_simple_scalar(plane, center, sample_step, pixel_advance, length, edge);
    } else if macroblock_edge {
        filter_normal_macroblock_scalar(plane, center, sample_step, pixel_advance, length, interior, edge, hev);
    } else {
        filter_normal_inner_scalar(plane, center, sample_step, pixel_advance, length, interior, edge, hev);
    }
}

/// 每宏块行滤波：按 C# `FilterMacroblockRow` 的顺序（左→内竖→上→内横），逐宏块滤波。
/// `row_filters` 为**本行** `mbw` 个 `(level, inner)`（调用方按行切窗/按行包传递，见
/// `filter_emit_serial_row` 与 `worker_loop`）。本函数是环路滤波的**串行语义源**：串行与
/// 新 MT 两路径只调度它，行内/宏块内/边顺序与算术完全不变，故任一路径的输出与串行逐字节
/// 一致（硬闸：`threads2_matches_serial_byte_exact`）。
#[allow(clippy::too_many_arguments)]
pub(crate) fn filter_macroblock_row(
    y: &mut [u8],
    y_pitch: usize,
    u: &mut [u8],
    v: &mut [u8],
    uv_pitch: usize,
    mbw: usize,
    mby: usize,
    filter: &FilterSettings,
    row_filters: &[(u8, u8)],
) {
    let y_row = mby * 16;
    let uv_row = mby * 8;
    stage_clock::filter(|| {
    for mbx in 0..mbw {
        let (level, inner) = row_filters[mbx];
        if level == 0 {
            continue;
        }
        let level_i = i32::from(level);
        let mut interior = level_i;
        if filter.sharpness != 0 {
            interior >>= if filter.sharpness > 4 { 2 } else { 1 };
            interior = interior.min(9 - i32::from(filter.sharpness));
        }
        interior = interior.max(1);
        let edge_limit = 2 * (level_i + 2) + interior;
        let sub_limit = 2 * level_i + interior;
        let hev = if level >= 40 { 2 } else if level >= 15 { 1 } else { 0 };
        let skip_inner = inner == 0;
        let simple = filter.simple;
        let x = mbx * 16;
        let uvx = mbx * 8;

        // SIMD 熔合路径（D2v3）：一次宏块全 12 核（C# FilterMacroblock 同款），边次序与下方
        // 标量完全一致 → 逐字节相同；指针派点与内核调用下沉 `simd::raw::lf_filter_macroblock_safe`
        // （`#[inline(always)]` 安全门面）。`KIMG_WEBP_NO_SIMD=1` / `KIMG_WEBP_NO_SIMD_FLT=1` /
        // `KIMG_WEBP_SIMD_FLT=0` 回退标量（门控见 simd::part_simd_on）。
        #[cfg(target_arch = "x86_64")]
        {
            if super::simd::part_simd_on("flt", "sse2") {
                super::simd::raw::lf_filter_macroblock_safe(
                    y,
                    y_pitch,
                    u,
                    v,
                    uv_pitch,
                    y_row,
                    x,
                    uv_row,
                    uvx,
                    mbx > 0,
                    mby > 0,
                    !skip_inner,
                    simple,
                    interior,
                    edge_limit,
                    sub_limit,
                    hev,
                );
                continue;
            }
        }

        if mbx > 0 {
            filter_edge(y, y_pitch, x, y_row, 16, true, true, simple, interior, edge_limit, hev);
            if !simple {
                filter_edge(u, uv_pitch, uvx, uv_row, 8, true, true, false, interior, edge_limit, hev);
                filter_edge(v, uv_pitch, uvx, uv_row, 8, true, true, false, interior, edge_limit, hev);
            }
        }
        if !skip_inner {
            for edge in [4usize, 8, 12] {
                filter_edge(y, y_pitch, x + edge, y_row, 16, true, false, simple, interior, sub_limit, hev);
            }
            if !simple {
                filter_edge(u, uv_pitch, uvx + 4, uv_row, 8, true, false, false, interior, sub_limit, hev);
                filter_edge(v, uv_pitch, uvx + 4, uv_row, 8, true, false, false, interior, sub_limit, hev);
            }
        }
        if mby > 0 {
            filter_edge(y, y_pitch, x, y_row, 16, false, true, simple, interior, edge_limit, hev);
            if !simple {
                filter_edge(u, uv_pitch, uvx, uv_row, 8, false, true, false, interior, edge_limit, hev);
                filter_edge(v, uv_pitch, uvx, uv_row, 8, false, true, false, interior, edge_limit, hev);
            }
        }
        if !skip_inner {
            for edge in [4usize, 8, 12] {
                filter_edge(y, y_pitch, x, y_row + edge, 16, false, false, simple, interior, sub_limit, hev);
            }
            if !simple {
                filter_edge(u, uv_pitch, uvx, uv_row + 4, 8, false, false, false, interior, sub_limit, hev);
                filter_edge(v, uv_pitch, uvx, uv_row + 4, 8, false, false, false, interior, sub_limit, hev);
            }
        }
    }
    });
}

/// 环路滤波双线程门控（auto 宏块总数下限）。`mbw*mbh >= MT_MIN_MB_TOTAL`（≈0.6MP，如 768×768；
/// 2026-11 微调 4096→2304：3600-MB 级（1280×720/800×1144）文件原本恒定串行、落入分账探针
/// 「最差 lw_ratio 档」（1.13-1.20x libwebp），线程开拔开销 ~20-50µs 相对其 ~15-25ms 解码可
/// 忽略，滤波+YUV 重叠收益 > 开销；字节一致由 `threads2_matches_serial_byte_exact` 全覆盖
/// （与阈值无关）。
const MT_MIN_MB_TOTAL: usize = 2304;

/// `KIMG_WEBP_THREADS=auto|0|1|2`（默认 auto）：
/// - `0` / `1` / `off` / `serial`：强制串行（重建毕→整帧滤波；输出恒与并行逐字节一致）。
/// - `2` / `on` / `two`：强制 P-B 真重叠管道（重建∥滤波，见 `decode_from_header` 下游）。
/// - 其余 / 缺省（auto）：宽图（宏块总数≥`MT_MIN_MB_TOTAL`）开管道，窄/小图关（进程开销>收益）。
///
/// 每次解码读一次 env（pn gsimd `KIMG_PNG_LEVEL` 同款，测试可翻转）。
pub(crate) fn filter_mt_enabled(mbw: usize, mbh: usize) -> bool {
    let mode = std::env::var("KIMG_WEBP_THREADS").unwrap_or_default();
    let mode = mode.trim();
    let force_off = matches!(mode, "0" | "1" | "off" | "serial");
    let force_two = matches!(mode, "2" | "on" | "two");
    if mbh == 0 || force_off {
        return false;
    }
    if force_two {
        return true;
    }
    mbw * mbh >= MT_MIN_MB_TOTAL
}

/// t2 → t3 的交接包：宏观行 r 重建定稿后的整行 padded 像素（与 planes_A 布局逐字节一致：16 行 luma +
/// 8 行 U + 8 行 V，全含左侧 129 列）+ 本行滤波参数表。随 `sync_channel(2)` 按值移动（零大块
/// 二次拷贝；容量 2 = t2 最多领先 t3 两行，自然背压限内存）。t3 借此在**自有** planes_B 上重建
/// 同布局，滤波/直出几何与 planes_A 完全同源 → 字节恒等由构造保证。
pub(crate) struct RowPixels {
    /// 16 × y_pitch 字节（luma padded 行，含左列）。
    luma: Vec<u8>,
    /// 8 × uv_pitch（U） + 8 × uv_pitch（V）。
    chroma: Vec<u8>,
    /// 本行宏块滤波参数 (level, inner)，t3 滤波输入。
    filters: Vec<(u8, u8)>,
}

/// 新 MT 管道 t2：依序消费 t1 行包，**预测+IDCT+重建**行 r 到 planes_A（本线程独占、恒未滤波），
/// 随即把本 MB 行重建定稿像素打包成 [`RowPixels`] 交给 t3。预测读上一行未滤波像素 —— planes_A
/// 从不滤波 → 与串行「predict 用未滤波值」逐字节同。`above_*` 为本线程局部（planes_A 行窗口）。
#[allow(clippy::too_many_arguments)]
pub(crate) fn recon_loop(
    rx: std::sync::mpsc::Receiver<RowPacket>,
    tx2: std::sync::mpsc::SyncSender<RowPixels>,
    rx_br: std::sync::mpsc::Receiver<RowPixels>,
    tx_ar: std::sync::mpsc::SyncSender<RowPacket>,
    y: &mut [u8],
    y_pitch: usize,
    u: &mut [u8],
    v: &mut [u8],
    uv_pitch: usize,
    mbw: usize,
) {
    let mut above_y = [127u8; 21];
    let mut above_u = [127u8; 9];
    let mut above_v = [127u8; 9];
    // 帧级 SIMD/熵门控一次判定（整帧恒定）：透传进 `reconstruct_macroblock`，消灭逐宏块门读。
    let gates = super::MbGates::detect();
    for (r, mut rowpkt) in rx.iter().enumerate() {
        for (mbx, pkt) in rowpkt.mbs.iter_mut().enumerate() {
            reconstruct_macroblock(
                y, y_pitch, u, v, uv_pitch,
                &mut above_y, &mut above_u, &mut above_v,
                mbx, r, mbw, pkt, gates,
            );
        }
        // 打包本 MB 行（行 r 已在 planes_A 定稿重建）→ t3。B：strip Vec 帧内池——优先复用
        // t3 滤波完送回（经 `rx_br`，`luma`/`chroma` 容量保活、页已暖），池空才新建；
        // 复用前 `clear()` 重写全部字节（未读残留，dirty 语义由「整行覆写」保证）。
        let RowPacket { filters, mbs } = rowpkt;
        let luma_len = 16 * y_pitch;
        let uv_len = 8 * uv_pitch;
        let mut luma = match rx_br.try_recv() {
            Ok(s) => s.luma,
            Err(_) => Vec::with_capacity(luma_len),
        };
        luma.clear();
        for i in 0..16 {
            let o = (r * 16 + i) * y_pitch;
            luma.extend_from_slice(&y[o..o + y_pitch]);
        }
        let mut chroma = match rx_br.try_recv() {
            Ok(s) => s.chroma,
            Err(_) => Vec::with_capacity(2 * uv_len),
        };
        chroma.clear();
        for i in 0..8 {
            let o = (r * 8 + i) * uv_pitch;
            chroma.extend_from_slice(&u[o..o + uv_pitch]);
        }
        for i in 0..8 {
            let o = (r * 8 + i) * uv_pitch;
            chroma.extend_from_slice(&v[o..o + uv_pitch]);
        }
        // `filters` 随像素带进 t3（滤波输入），体积仅 mbw×2B 级，克隆代价 ~0.2KB/行可忽略；
        // 原 `filters`/`mbs` Vec 整体送回 t1 复用（A 池）。
        let pkt = RowPixels { luma, chroma, filters: filters.clone() };
        if tx2.send(pkt).is_err() {
            // t3 已退出（异常收尾）：中断产出，外层 scope join 兜底。
            break;
        }
        let _ = tx_ar.try_send(RowPacket { filters, mbs });
    }
}

/// 从已定稿（滤波后）padded 平面抽紧密平面行段 `[first, end)`（luma 行 first..end、chroma 行
/// first/2 ..= (end-1)/2）。语义与旧整帧尾段拷贝逐字节一致（旧拷贝 = 本函数 0..height 单次调用）。
#[allow(clippy::too_many_arguments)]
pub(crate) fn extract_tight_rows(
    y: &[u8],
    y_pitch: usize,
    u: &[u8],
    v: &[u8],
    uv_pitch: usize,
    first: usize,
    end: usize,
    width: usize,
    out_y: &mut [u8],
    out_u: &mut [u8],
    out_v: &mut [u8],
) {
    let cwu = width.div_ceil(2);
    if end == 0 {
        return;
    }
    for r in first..end {
        out_y[r * width..(r + 1) * width].copy_from_slice(&y[r * y_pitch + 1..r * y_pitch + 1 + width]);
    }
    let cfirst = first / 2;
    let clast = (end - 1) / 2;
    for c in cfirst..=clast {
        let off = c * cwu;
        out_u[off..off + cwu].copy_from_slice(&u[c * uv_pitch + 1..c * uv_pitch + 1 + cwu]);
        out_v[off..off + cwu].copy_from_slice(&v[c * uv_pitch + 1..c * uv_pitch + 1 + cwu]);
    }
}

/// 新 MT 管道 t3：依序消费 t2 的重建行包，在**自有** planes_B（同布局 padded 缓冲，经
/// [`RowPixels`] 逐行填入）上做【环路滤波 → 行定稿直出 RGBA → 抽紧密平面】。本体恒等论证：
/// - plans_B 内容 = planes_A（未滤波重建）逐字节拷贝 → 滤波输入与串行「滤波读已重建行」同。
/// - 滤波/直出/紧密抽取完全复用 `filter_macroblock_row` / `emit_rows_into_rgba` /
///   `extract_tight_rows`，行序 m 升序、段序 `[emitted, m*16+8)` 与串行一致 → 逐字节同。
/// - planes_A 恒未滤波（t2 独占），故 t3 滤波不需要等 t2 的下一步预测 —— 无 1 行滞后约束。
///
/// 所有权：本线程独占 planes_B / rgba / out_*，零裸操作，无共享可变内存。
#[allow(clippy::too_many_arguments)]
pub(crate) fn filter_t3_loop(
    rx: std::sync::mpsc::Receiver<RowPixels>,
    tx_br: std::sync::mpsc::SyncSender<RowPixels>,
    y_pitch: usize,
    uv_pitch: usize,
    rgba: &mut [u8],
    out_y: &mut [u8],
    out_u: &mut [u8],
    out_v: &mut [u8],
    mbw: usize,
    mbh: usize,
    width: usize,
    height: usize,
    filter: &FilterSettings,
    need_tight: bool,
    channels: usize,
    alpha: Option<&[u8]>,
) {
    // t3 独占的全 padded 重建缓冲（planes_B）：与 planes_A 同布局（含左列）。L3 死写削减：
    // 每 MB 行进入时 16×y_pitch（luma）+ 16×uv_pitch（chroma）整行 `copy_from_slice` 覆写全部
    // 字节，其后滤波/直出/抽紧密只读已写入行段（上方邻行亦于上一迭代满写；行首行先写后读）
    // → 全量 memset 为死写，未初始化分配 + 整行覆写与 memset 版逐字节同，无未初始化读。
    let y_vec_len = mbh * 16 * y_pitch;
    let uv_vec_len = mbh * 8 * uv_pitch;
    let mut planes_b = super::uninit_vec_u8(y_vec_len + 2 * uv_vec_len);
    let (yb, rest) = planes_b.split_at_mut(y_vec_len);
    let (ub, vb) = rest.split_at_mut(uv_vec_len);
    let cw = (width + 1) >> 1;
    let ch = (height + 1) >> 1;
    let mut scratch = EmitScratch::new(width);
    let mut emitted = 0usize;
    for (r, pkt) in rx.iter().enumerate() {
        // 本 MB 行重建行像素写入 planes_B（布局与 planes_A 一致 → 滤波/直出几何完全同源）。
        for i in 0..16 {
            let dst = (r * 16 + i) * y_pitch;
            yb[dst..dst + y_pitch].copy_from_slice(&pkt.luma[i * y_pitch..(i + 1) * y_pitch]);
        }
        for i in 0..8 {
            let dst = (r * 8 + i) * uv_pitch;
            ub[dst..dst + uv_pitch].copy_from_slice(&pkt.chroma[i * uv_pitch..(i + 1) * uv_pitch]);
            vb[dst..dst + uv_pitch].copy_from_slice(&pkt.chroma[(8 + i) * uv_pitch..(8 + i + 1) * uv_pitch]);
        }
        // 本行滤波 + 行定稿直出 + 抽紧密平面（串行同序：先滤 m 再 emit m；末段 end=height 同语义）。
        filter_macroblock_row(yb, y_pitch, ub, vb, uv_pitch, mbw, r, filter, &pkt.filters);
        let end = if r == mbh - 1 { height } else { r * 16 + 8 };
        if end > emitted {
            if channels == 3 {
                emit_rows_into_rgb3(yb, y_pitch, ub, vb, uv_pitch, width, cw, ch, emitted, end, rgba, &mut scratch);
            } else {
                emit_rows_into_rgba(yb, y_pitch, ub, vb, uv_pitch, width, cw, ch, emitted, end, rgba, &mut scratch, alpha);
            }
            if need_tight {
                stage_clock::copy(|| {
                    extract_tight_rows(
                        yb, y_pitch, ub, vb, uv_pitch,
                        emitted, end, width, out_y, out_u, out_v,
                    );
                });
            }
            emitted = end;
        }
        // B 回收：把用过的 strip（luma/chroma/filters 全部重写后送回）交还 t2 复用；
        // 通道满则丢弃（t2 回退新建兜底），`try_send` 不阻塞 → 无死锁。
        let _ = tx_br.try_send(pkt);
    }
}

