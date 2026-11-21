#![forbid(unsafe_code)]
use super::*;

// ---------------------------------------------------------------------------
// 新 MT 管道（t1=串行熵 / t2=预测+IDCT+重建+滤波+YUV）的宏块行包与共享阶段函数。
// 串行路径（`filter_mt_enabled`==false）与 MT 两路径共用 `entropy_macroblock` /
// `reconstruct_macroblock` 同一函数体 → 逐字节一致是结构性成立（硬闸
// `threads2_matches_serial_byte_exact` / `static_lossy_threads2_matches_serial_byte_exact`）。
// 本文件已转 forbid 属性薄委托：全部裸操作（`reconstruct_macroblock` /
// `reconstruct_i4_mb_ssse3` 内核与 SIMD 分发）迁入 `simd::raw::pipeline`，此处仅剩一行委托
// （签名/可见性/字节不变）；`uninit_vec_u8`/`unchecked_index` 等经 `simd::raw` 安全门面。
// ---------------------------------------------------------------------------

/// 单宏块熵解码产物（t1 → t2 的唯一交接对象）。含去量化后的 i16 系数（DC 捷径/双指针核在
/// `reconstruct_macroblock` 按串行同款复用，字节不变）。`Default` 全零 = 未写系数位恒 0。
/// 字段 `pub(crate)`：`simd::raw::pipeline` 迁入的 `reconstruct_macroblock` 按字段读写本包
/// （非本模块子孙，须显式打开字段可见性；包级 crate 内可见，不改字节/布局）。
#[derive(Default)]
pub(crate) struct MbPacket {
    pub(crate) y_mode: u8,
    pub(crate) uv_mode: u8,
    /// y_mode==4（16x16）时为真；调用方沿用本仓 `i16` 命名习惯（值命名空间可遮蔽原语类型名）。
    pub(crate) i16: bool,
    pub(crate) nonzero_mask: u32,
    pub(crate) ac_mask: u32,
    pub(crate) b_modes: [u8; 16],
    pub(crate) mb: [[i16; 16]; 25],
}


/// 单一宏块行的完成包：`mbs[mbx]`（t2 重建输入）+ `filters[mbx]`（t2 滤波输入，原行滤波参数表
/// `(level, inner)`）。随通道按序交给 t2，t2 依序重建行 r 后滤行 r-1（串行同序）。整包按值
/// 移动（零大块拷贝）；t1 只持有通道发送端。
pub(crate) struct RowPacket {
    pub(crate) filters: Vec<(u8, u8)>,
    pub(crate) mbs: Vec<MbPacket>,
}

/// 单宏块解码所需 SIMD/熵门控（整帧恒定：CPU 特性探测 + env/override 只在首次访问触发一次，
/// 之后全 Relaxed 原子读）。在 `decode_from_header` / `recon_loop` 每帧取一次，经 per-MB 函数
/// 透传——消除逐宏块/逐块的 `part_simd_on`/`idct_i16_enabled`/`i4_simd_enabled` 多次读取
/// （asm 每块 `movl ON(%rip);testl;jne` 分支，4–6ns/MB；`_forced` 核已在循环外判定过）。
#[derive(Clone, Copy)]
pub(crate) struct MbGates {
    /// `part_simd_on("dct","sse2")` 原始判定。i16/UV 段直用；i4 段须再与 `i4_simd` 合取
    /// （`i4_simd && dct_simd`，保持既有 i4 分支语义）。
    pub(crate) dct_simd: bool,
    /// `idct_i16_enabled()`（K93 第二遍 i16 域开关）。
    pub(crate) i16_en: bool,
    /// `i4_simd_enabled()`（i4 段专用开关）。
    pub(crate) i4_simd: bool,
    /// `i4_simd && part_simd_on("pred","ssse3")`（i4 段 B_PRED 子块预测 SIMD；已含 i4 开关）。
    pub(crate) pred_simd: bool,
}

impl MbGates {
    /// 每帧取一次。与 `reconstruct_macroblock` 旧「逐宏块读取」取值逐位一致（门控函数在整帧
    /// 解码内恒定，测试 override 亦在整帧内不变）→ 透传不改变任何分发语义。
    pub(crate) fn detect() -> MbGates {
        let dct_simd = super::simd::part_simd_on("dct", "sse2");
        let i16_en = super::simd::idct_i16_enabled();
        let i4_simd = super::simd::i4_simd_enabled();
        let pred_simd = i4_simd && super::simd::part_simd_on("pred", "ssse3");
        MbGates { dct_simd, i16_en, i4_simd, pred_simd }
    }
}

/// 单宏块熵解码（模式 + 残差 token + 去量化），产出 [`MbPacket`] 与行滤波参数 `(level, inner)`。
/// 逐条对应原 `decode_from_header::reconstruct_row` 的「阶段一」，只是把 `mb`/`b_modes` 收进
/// 包（每宏块零初始化，与串行 mb 复用数组在「写入前全零」语义上等价——未写位恒 0）。
/// 熵态（`header`/`td` bool 解码器与 `above_nz`/`left_nz`/`above_b`/`left_b`）全程留在 t1。
/// `#[inline(always)]`：串行路径经 `decode_macroblock` 合并时与 `reconstruct_macroblock` 同体
/// 内联（消灭逐宏块两次 out-of-line 调用+分发）；MT t1 单调用点同样受益，字节恒等由同源函数体保证。
#[inline(always)]
#[allow(clippy::too_many_arguments)]
fn entropy_macroblock(
    header: &mut Vp8BoolDecoder<'_>,
    td: &mut Vp8BoolDecoder<'_>,
    segment_probs: Option<&[u8; 3]>,
    skip_probability: i32,
    probs: &[u8],
    seg_params: &[SegmentParameters; 4],
    above_b: &mut [u8; 4],
    left_b: &mut [u8; 4],
    above_nz: &mut u32,
    left_nz: &mut u32,
    pkt: &mut MbPacket,
) -> (u8, u8) {
    // 阶段①（熵计时）：模式/段/skip 解码。
    let modes = stage_clock::entropy(|| {
        stage_clock::entropy_modes(|| {
            read_macroblock_modes(header, segment_probs, skip_probability, above_b, left_b, &mut pkt.b_modes)
        })
    });
    let segment = (modes & 3) as usize;
    let skip = (modes & 4) != 0;
    let y_mode = ((modes >> 3) & 7) as u8;
    let uv_mode = (modes >> 6) as u8;
    let is_i16 = y_mode != 4;
    let sp = seg_params[segment];
    // 惰性零化（A：MbPacket 帧内池 + 仅 touched-宏块零化；不在任何熵计时闭合内，避免把「内容
    // 无关写」算进熵账，也避免探针 Instant/原子与 memset 相互干扰）。
    // - 非跳过：`parse_residuals` 的 `tokens` 只写非零系数位、未写位必须为 0（`MbPacket::default`
    //   等价语义），因此整包零化——复用槽位即使残留脏块也被清掉，逐字节不变由结构保证。
    // - 跳过：重建侧只把 `mb[0..16][0]` 当 DC 门控读（i16 的 WHT/UV 段与 i4 均不读其余位），
    //   仅零化 16 个 DC 槽（32B），免去整包 830B 内容无关写。
    let masks = if skip {
        for b in pkt.mb.iter_mut().take(16) {
            b[0] = 0;
        }
        let keep = if is_i16 { 0 } else { NZ_Y2 };
        *above_nz &= keep;
        *left_nz &= keep;
        0u64
    } else {
        pkt.mb = [[0i16; 16]; 25];
        // 阶段②（熵计时）：残差 token + 去量化。
        stage_clock::entropy(|| {
            stage_clock::entropy_tokens(|| {
                parse_residuals(td, probs, &mut pkt.mb, &sp, is_i16, above_nz, left_nz)
            })
        })
    };
    pkt.y_mode = y_mode;
    pkt.uv_mode = uv_mode;
    pkt.i16 = is_i16;
    pkt.nonzero_mask = masks as u32;
    pkt.ac_mask = (masks >> 32) as u32;
    let level = if is_i16 { sp.filter_level_i16 } else { sp.filter_level_i4 };
    let inner = if pkt.nonzero_mask == 0 && is_i16 { 0u8 } else { 1u8 };
    (level, inner)
}

/// 单宏块帧内预测 + 逆变换 + 残差写回（原 `decode_from_header`「阶段二/三」，逐字节不变）。
/// 薄委托 `simd::raw::pipeline::reconstruct_macroblock`：全部裸操作（阶段内 SIMD `_forced`
/// 分发、`reconstruct_i4_mb_ssse3` 汇聚核、预测行裸读）已随该函数整块迁入 raw 叶子；本入口
/// 一行转发，签名 / 可见性 / `#[inline(always)]` / 字节不变（两端内联后与隔离前逐指令一致）。
#[inline(always)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn reconstruct_macroblock(
    y: &mut [u8],
    y_pitch: usize,
    u: &mut [u8],
    v: &mut [u8],
    uv_pitch: usize,
    above_y: &mut [u8; 21],
    above_u: &mut [u8; 9],
    above_v: &mut [u8; 9],
    mbx: usize,
    mby: usize,
    mbw: usize,
    pkt: &mut MbPacket,
    gates: MbGates,
) {
    crate::codecs::webp::simd::raw::pipeline::reconstruct_macroblock(
        y, y_pitch, u, v, uv_pitch,
        above_y, above_u, above_v,
        mbx, mby, mbw, pkt, gates,
    )
}

// `reconstruct_i4_mb_ssse3`（SSSE3 汇聚内核）已整块迁入 `simd::raw::pipeline`
// （连同其私有安全依赖 `subblock_above`/`subblock_above_row`/`stage_clock` 与 SIMD `_forced`
// 再导出透传；`reconstruct_macroblock` 在本文件仅剩薄委托调用它）。本处无残留定义。


/// 串行路径单 per-MB 解码：熵（`entropy_macroblock`）→ 重建（`reconstruct_macroblock`）合并为
/// 一次调用——消灭逐宏块两次 out-of-line 调用+分发（c1 ~14–25ns/MB 的可解部分 ≈5–10ns/MB）。
/// 两段子函数 `#[inline(always)]` 在本函数内全内联（同 `reconstruct_i4_mb_ssse3` 先例的
/// 「整段提升进单函数」姿势）：熵态（header/td/above_nz/left_nz）与重建态（y/u/v/above_*）
/// 在同一函数体内流转，寄存器驻留、无 ABI 往返。字节恒等：与 MT 两路径共用完全相同的
/// `entropy_macroblock`/`reconstruct_macroblock` 函数体（`threads2_matches_serial_byte_exact`
/// 结构性成立），`gates` 整帧恒定一次判定，语义不变。
#[allow(clippy::too_many_arguments)]
fn decode_macroblock(
    header: &mut Vp8BoolDecoder<'_>,
    td: &mut Vp8BoolDecoder<'_>,
    segment_probs: Option<&[u8; 3]>,
    skip_probability: i32,
    probs: &[u8],
    seg_params: &[SegmentParameters; 4],
    above_b: &mut [u8; 4],
    left_b: &mut [u8; 4],
    above_nz: &mut u32,
    left_nz: &mut u32,
    pkt: &mut MbPacket,
    y: &mut [u8],
    y_pitch: usize,
    u: &mut [u8],
    v: &mut [u8],
    uv_pitch: usize,
    above_y: &mut [u8; 21],
    above_u: &mut [u8; 9],
    above_v: &mut [u8; 9],
    mbx: usize,
    mby: usize,
    mbw: usize,
    gates: MbGates,
) -> (u8, u8) {
    let (level, inner) = entropy_macroblock(
        header,
        td,
        segment_probs,
        skip_probability,
        probs,
        seg_params,
        above_b,
        left_b,
        above_nz,
        left_nz,
        pkt,
    );
    reconstruct_macroblock(
        y, y_pitch, u, v, uv_pitch,
        above_y, above_u, above_v,
        mbx, mby, mbw, pkt, gates,
    );
    (level, inner)
}

pub(crate) fn decode_from_header(
    fh: FrameHeader<'_>,
    need_tight: bool,
    channels: usize,
    alpha: Option<&[u8]>,
    out: Option<&mut [u8]>,
) -> Result<Vp8Yuv> {
    debug_assert!(channels == 3 || channels == 4);
    let width = fh.width as usize;
    let height = fh.height as usize;
    let mbw = fh.mb_width;
    let mbh = fh.mb_height;
    let cw = mbw * 16;
    let ch = mbh * 16;
    let uw = mbw * 8;
    let uh = mbh * 8;
    let y_pitch = cw + 1;
    let uv_pitch = uw + 1;
    let seg_params = compute_segment_parameters(&fh.segments, &fh.quantizers, &fh.filter);

    // L3 分配收敛：y/u/v 三平面合一缓冲（按首尾分段），堆分配 3 → 1。解码全程不 realloc，
    // 依赖其长度固定即指针稳定（与下方裸指针同前提）。字节恒等：合并仅改变分配粒度，
    // 各段内容、写入次序与既有三 Vec 完全一致。
    // L3 死写削减：全量零化是死写——预测/IDCT 逐宏块整块写满数据区，滤波/发射只读已重建
    // 行段；重建前仅「左 129 列」（`r*pitch+0`，供 `predict_large` H/TM 与 `predict_subblock`
    // 左缘读）被预测读取，故只对该列确定性初始化，省全量 memset。无未初始化读（见下）。
    let y_vec_len = ch * y_pitch;
    let uv_vec_len = uh * uv_pitch;
    // 固定缓冲池（结构线唯一实靶）：planes / above_b / above_nz / row_filters / EmitScratch
    // 每调用一次分配收敛为每线程复用池——第二次起的 decode 不再分配、不首触缺页（见
    // `subbuffers.rs`）。线程归属：所有取/还都在调用线程完成（MT 路径 planes_A 虽被 t2 重建
    // 线程写入，其生命周期由本 guard 全程持有、`std::thread::scope` join 后才归还）→ 无跨线程
    // 归还（K28 每线程独立）。各缓冲「take 时重初始化 or 全写先于读」判定见 `subbuffers.rs`。
    let mut sub = Vp8SubBuffersGuard::take(y_vec_len, uv_vec_len, mbw, mbh);
    let (mut planes, mut rgba, mut out_y, mut out_u, mut out_v) = stage_clock::setup(|| {
        // planes 池化：take 时已按本帧 `len` 就绪（grow 用 reserve 不触碰新页，与
        // `uninit_vec_u8` 同未初始化语义——首页缺页仍发生在重建写，而非 here）。
        // 129 左列确定性初始化每帧照旧；内部像素由预测/IDCT 逐宏块整块写满先于读（既有
        // 微批不变式，复用上一帧残值不影响任何读）。`rgba`/`out_*` 例外：随 `Vp8Yuv` 逃逸给
        // 调用方，不能池化（会被下一帧 take 复用覆盖仍在使用的输出）→ 每帧新建。
        let mut planes = sub.take_planes();
        {
            let (y, uv_rest) = planes.split_at_mut(y_vec_len);
            let (u, v) = uv_rest.split_at_mut(uv_vec_len);
            for r in 0..ch {
                y[r * y_pitch] = 129;
            }
            for r in 0..uh {
                u[r * uv_pitch] = 129;
                v[r * uv_pitch] = 129;
            }
        }
        // 行定稿直出缓冲：`out=Some`（直写调用方缓冲）时跳过整帧 w*h*channels 自分配——由
        // 调用方缓冲承担，结构性省一次整帧分配；两分支（Pipelined/串行）随后都以 `dst` 为
        // 唯一写入目标。`channels`=3（RGB3 opaque 有损直出）/4（RGBA）。
        // L3：未初始化分配——两条直出路径（pipelined `filter_t3_loop` / 串行
        // `filter_emit_serial_row`）都逐行写满 `[0,height)×width×channels`，返回前无任何字节读，
        // 错误路径直接 Err（不构造 Vp8Yuv），故省略全量 memset 逐字节不变（仅省零化墙钟）。
        let rgba = match &out {
            Some(_) => Vec::new(),
            None => uninit_vec_u8(width * height * channels),
        };
        // 紧密平面输出缓冲（真实宽高；仅供测试/兼容 API 消费，生产 rgba 路径弃用 = 纯浪费，
        // 由 `need_tight` 门控跳过以省 1-2 遍整帧写）。L3 未初始化：MT 由 t3 直出、串行由尾段
        // `extract_tight_rows` 写满，返回前不被读，省略 memset 逐字节不变。
        let cwu = width.div_ceil(2);
        let chu = height.div_ceil(2);
        let (out_y, out_u, out_v) = if need_tight {
            (
                uninit_vec_u8(width * height),
                uninit_vec_u8(cwu * chu),
                uninit_vec_u8(cwu * chu),
            )
        } else {
            (Vec::new(), Vec::new(), Vec::new())
        };
        (planes, rgba, out_y, out_u, out_v)
    });
    let (y, uv_rest) = planes.split_at_mut(y_vec_len);
    let (u, v) = uv_rest.split_at_mut(uv_vec_len);

    // 行定稿直出目标：`out=Some`（调用方缓冲，`decode_vp8_into`/`decode_vp8_with_alpha_into`
    // 直写入口）或内部整帧 `rgba`（既有收缓冲路径）。长度校验先于任何像素写入，直接 Err
    // 而非静默截断——与无损侧 `decode_into_rgba8` 的「输出缓冲长度须为 w*h*4」同口径。
    let dst: &mut [u8] = match out {
        Some(o) => {
            let expect = width
                .checked_mul(height)
                .and_then(|n| n.checked_mul(channels))
                .ok_or_else(|| Vp8Error::new("VP8 直写缓冲尺寸溢出"))?;
            if o.len() != expect {
                return Err(Vp8Error::new("VP8 直写缓冲长度须为 w*h*channels"));
            }
            o
        }
        None => &mut rgba,
    };

    let mut header = fh.header;
    let (
        partition_count,
        mut token_decoders,
        probs,
        skip_probability,
        segment_probs,
        mut above_b,
        mut above_nz,
        mut above_y,
        mut above_u,
        mut above_v,
        mut row_filters,
        pipelined,
    ) = stage_clock::mid(|| {
        let partition_count = fh.partitions.len();
        let token_decoders: Vec<Vp8BoolDecoder<'_>> =
            fh.partitions.iter().map(|p| Vp8BoolDecoder::new(p)).collect();
        let probs = fh.coefficient_probabilities;
        let skip_probability = if fh.skip_flag_present {
            i32::from(fh.skip_probability)
        } else {
            -1
        };
        let segment_probs: Option<&[u8; 3]> = if fh.segments.enabled && fh.segments.update_map {
            Some(&fh.segments.map_probabilities)
        } else {
            None
        };
        // above_b / above_nz：take 时已确定性重初始化（fill 0，`vec![0; _]` 原语义——首宏块
        // 行写前读取依赖 0 初值）。row_filters：逐元素写满先于读（串行每宏块写、经受滤波行才
        // 读；MT 不消费本表）→ take 时未重零。
        let above_b = sub.take_above_b();
        let above_nz = sub.take_above_nz();
        let above_y = [127u8; 21];
        let above_u = [127u8; 9];
        let above_v = [127u8; 9];
        // 每行宏块的 (滤波等级, 是否滤内部边)，仅串行路径消费（MT 行包自带滤波参数）。
        let row_filters = sub.take_row_filters();
        let pipelined = filter_mt_enabled(mbw, mbh);
        (
            partition_count,
            token_decoders,
            probs,
            skip_probability,
            segment_probs,
            above_b,
            above_nz,
            above_y,
            above_u,
            above_v,
            row_filters,
            pipelined,
        )
    });

    if pipelined {
        // 新 3 线程管道：t1(主) = 串行熵/去量化 → 依序产**行包**；t2 = 预测+IDCT+重建（planes_A，
        // 恒未滤波）→ 依序把重建定稿行打包 `RowPixels` 交 t3；t3 = 在自有 planes_B 上滤波 +
        // 行定稿直出 RGBA + 抽紧密平面。通道均 `sync_channel(2)` 按值移动，容量 2 = 上一级最多
        // 领先 2 行，自然背压限内存。y/u/v(planes_A)/planes_B/rgba/out_* 各线程独占，零裸操作。
        // 字节恒等论证见 `recon_loop` / `filter_t3_loop`：planes_A 恒未滤波、planes_B 同源拷贝、
        // 滤波/直出/抽取行序与串行完全一致 → 逐字节同（硬闸 threads2/static_lossy 覆盖）。
        // 熵态（bool 解码器 / above_nz / above_b / left_*）全留 thread1。
    std::thread::scope(|s| {
        // 通道在 scope 闭包内创建：闭包返回时 `tx`/`tx2` 随其 drop（先于 scope 回收子线程），
        // t2/t3 的 `rx.iter()` 随即收到断开（Err）→ 收尾退出，无死锁。
        stage_clock::scope_wall(|| {
        let (tx, rx) = std::sync::mpsc::sync_channel::<RowPacket>(2);
        let (tx2, rx2) = std::sync::mpsc::sync_channel::<RowPixels>(2);
            // A：t1→t2 行包回收通道（t2 重建完把 `mbs`/`filters` Vec 空包送回 t1 复用——
            // 消灭每行 `with_capacity(mbw)` ×2 的分配/首触缺页，≥2304 MB 大图帧内 ~540 次 alloc
            // 摊销到 ~4 个槽位）。回收通道一律 `try_send`/`try_recv`：满/空时双方不阻塞、
            // 各自退回新建兜底，杜绝跨通道死锁。
            let (tx_ar, rx_ar) = std::sync::mpsc::sync_channel::<RowPacket>(2);
            // B：t2→t3 行像素带回收（t3 滤波完把 strip Vec 送回 t2 复用）。
            let (tx_br, rx_br) = std::sync::mpsc::sync_channel::<RowPixels>(2);
            let yb = &mut *y;
            let ub = &mut *u;
            let vb = &mut *v;
            let rb = &mut *dst;
            let oy = &mut *out_y;
            let ou = &mut *out_u;
            let ov = &mut *out_v;
            s.spawn(move || {
                recon_loop(rx, tx2, rx_br, tx_ar, yb, y_pitch, ub, vb, uv_pitch, mbw)
            });
            s.spawn(move || {
                filter_t3_loop(rx2, tx_br, y_pitch, uv_pitch, rb, oy, ou, ov, mbw, mbh, width, height, &fh.filter, need_tight, channels, alpha)
            });
            for mby in 0..mbh {
                // 每宏块行重置 4 个左邻 b 模式（C# `leftB.Clear()` 同款）。
                let mut left_b = [0u8; 4];
                let mut left_nz = 0u32;
                // 行内复用该分区的 token 解码器（&mut 边界，熵态在宏块级函数内部寄存器驻留）。
                let td = &mut token_decoders[mby & (partition_count - 1)];
                // 行包帧内池：优先复用 t2 送回的空包（`mbs`/`filters` Vec 容量保活、页已暖），
                // 池空（首几行或回收慢）才新建兜底。`resize(mbw, default)`：回收包 len 恒为
                // mbw → no-op；新建包一次性全量零化打底（后续槽位由 `entropy_macroblock` 惰性零化接管）。
                let mut row = match rx_ar.try_recv() {
                    Ok(rp) => rp,
                    Err(_) => RowPacket {
                        filters: Vec::with_capacity(mbw),
                        mbs: Vec::with_capacity(mbw),
                    },
                };
                row.filters.clear();
                // 不用 `resize`（那要求 MbPacket: Clone，使综述里 `#[derive(Clone)]` 变成"被依赖"伪死代码）：
                // 回收包 len 恒为 mbw → no-op；新建包按 `Default` 逐槽构造（仅 Default，无 Clone 需求）。
                if row.mbs.len() < mbw {
                    row.mbs.extend((0..mbw - row.mbs.len()).map(|_| MbPacket::default()));
                }
                for mbx in 0..mbw {
                    let above_b_slot: &mut [u8; 4] =
                        (&mut above_b[mbx * 4..mbx * 4 + 4]).try_into().unwrap();
                    // 就地填充复用槽位：`entropy_macroblock` 内按 skip 惰性零化（`mb`）并覆写全部
                    // 头字段（y/uv/i16/masks/b_modes），未写位恒 0 语义由该函数建立 → 免去每宏块
                    // `MbPacket::default()` 整包 830B 栈零化 + 拷贝入 Vec。
                    let (level, inner) = entropy_macroblock(
                        &mut header,
                        td,
                        segment_probs,
                        skip_probability,
                        &probs,
                        &seg_params,
                        above_b_slot,
                        &mut left_b,
                        &mut above_nz[mbx],
                        &mut left_nz,
                        &mut row.mbs[mbx],
                    );
                    row.filters.push((level, inner));
                }
                if tx.send(row).is_err() {
                    // worker 已退出（异常收尾）：中断产出，scope join 兜底。
                    break;
                }
            }
        });
        });
        // scope join 完成：t3 已滤波+直出+抽紧密平面全部行。

    } else {
        // 串行：逐宏块「熵 → 预测+IDCT+重建」（与 MT 共用 `entropy_macroblock` /
        // `reconstruct_macroblock`，字节恒等结构性成立），整帧重建毕再滤波直出。
        // 串行逐宏块「熵 → 预测+IDCT+重建」：单 `MbPacket` 槽位整帧复用——`entropy_macroblock`
        // 内按 skip 惰性零化建立「未写位恒 0」语义，重建用后按块 `fill(0)`，逐字节与旧
        // 「每宏块 `MbPacket::default()`」等价（硬闸 threads2/static_lossy 覆盖），且省去整帧
        // ~27MB 的 830B 内容无关零化（跳过宏块仅零化 16 个 DC 槽 32B）。
        // 串行分支整段墙钟（B 线拆账：margin = serial_loop − Σ5 阶段；feature 外零开销）。
        stage_clock::serial_loop(|| {
        let mut pkt = MbPacket::default();
        // EmitScratch 池化：池中复用则 resize 到当前 width（各字段 emit 前全写先于读，见
        // `EmitScratch::resize_to`），池空才新建；serial 分支专属，用完归还。
        let mut scratch = sub.take_scratch(width);
        let mut emitted = 0usize;
        // 帧级 SIMD/熵门控一次判定（`part_simd_on`/`idct_i16_enabled`/`i4_simd_enabled` 在整帧
        // 解码内恒定）：透传进 `decode_macroblock`，消灭逐宏块/逐块门读（c2 ~4–6ns/MB）。
        let gates = MbGates::detect();
        for mby in 0..mbh {
            // 每宏块行重置 4 个左邻 b 模式（C# `leftB.Clear()` 同款）。
            let mut left_b = [0u8; 4];
            let mut left_nz = 0u32;
            // 行内复用该分区的 token 解码器（&mut 边界，熵态在宏块级函数内部寄存器驻留）。
            let td = &mut token_decoders[mby & (partition_count - 1)];
            for mbx in 0..mbw {
                let above_b_slot: &mut [u8; 4] =
                    (&mut above_b[mbx * 4..mbx * 4 + 4]).try_into().unwrap();
                // 单 per-MB 调用（熵+重建合并）：过去两处 out-of-line 调用+分发收敛为一次。
                let (level, inner) = decode_macroblock(
                    &mut header,
                    td,
                    segment_probs,
                    skip_probability,
                    &probs,
                    &seg_params,
                    above_b_slot,
                    &mut left_b,
                    &mut above_nz[mbx],
                    &mut left_nz,
                    &mut pkt,
                    y, y_pitch, u, v, uv_pitch,
                    &mut above_y, &mut above_u, &mut above_v,
                    mbx, mby, mbw,
                    gates,
                );
                row_filters[mby * mbw + mbx] = (level, inner);
            }
            // 行窗：重建行 mby 后，上一行 mby-1 的滤波输入已全部就绪（bottom 邻行 mby 已做），
            // 立即滤波上一行并发射其已定稿行 —— 滤波/YUV 读的都是刚写出的热行（消整帧重建毕
            // 再滤波的冷平面重读）。字节恒等：与旧「整帧重建毕 → filter_and_emit_serial」滤波
            // 行序完全一致（filter 0,1,…,mbh-1，逐宏块行同序），且本方案滤波 mby-1 恰在重建
            // mby 之后（bottom 邻行已重建、非滤波污染），预测读未滤波行的不变式保持。
            if mby >= 1 {
                emitted = filter_emit_serial_row(
                    y, y_pitch, u, v, uv_pitch, mbw,
                    mby - 1, mbh, &fh.filter, &row_filters[(mby - 1) * mbw..mby * mbw],
                    width, height, dst, &mut scratch, channels, alpha, emitted,
                );
            }
        }
        // 收尾：最后一行 mbh-1（其 bottom 邻行为 padded 末行，整帧重建毕），滤波并发射剩余行。
        // mbh==0（0 高帧，未见实际语料）时旧整帧路径 filter_and_emit_serial 直接空跑，此处
        // 同样跳过（`mbh-1` 会下溢）。
        if mbh > 0 {
            #[allow(unused_assignments)]
            let _emitted = filter_emit_serial_row(
                y, y_pitch, u, v, uv_pitch, mbw,
                mbh - 1, mbh, &fh.filter, &row_filters[(mbh - 1) * mbw..mbh * mbw],
                width, height, dst, &mut scratch, channels, alpha, emitted,
            );
        }

        // 抽紧密平面（仅测试/兼容 API 路径 need_tight；生产 rgba 直出路径跳过 = 省整帧写）。
        // L3：out_* 已未初始化分配，下方逐行写满（读已整帧滤波的 y/u/v padded 平面），
        // 返回前不被读，省略 memset 逐字节不变。
        if need_tight {
            stage_clock::copy(|| {
                extract_tight_rows(
                    y, y_pitch, u, v, uv_pitch,
                    0, height, width, &mut out_y, &mut out_u, &mut out_v,
                );
            });
        }
        // 归还上采样工作区给池（serial 分支专属；MT 分支未 take，随 guard Drop 归还）。
        sub.return_scratch(scratch);
        });
    }

    // 归还各固定缓冲给池（下帧 take 即再就绪；`rgba`/`out_*` 例外：随 `Vp8Yuv` 逃逸给调用方，
    // 不能池化——见 setup 注释）。本函数仅有的提前 `return Err`（直写缓冲长度不符）发生在
    // mid() 之前，planes 此时已 take 出、随 drop 释放（池退回空 → 下帧重建），语义不变。
    sub.return_planes(planes);
    sub.return_above_b(above_b);
    sub.return_above_nz(above_nz);
    sub.return_row_filters(row_filters);

    // scope join 后、返回前尾段（Vp8Yuv 构建）：主线程墙钟全部归账，无隐藏盲区（K53）。
    // `rgba` 缓冲按 `channels` 已写满（3=RGB3 / 4=RGBA4）；路由到对应字段，另一个恒空。
    stage_clock::end(|| {
        let (rgba_field, rgb_field) = if channels == 3 {
            (Vec::new(), rgba)
        } else {
            (rgba, Vec::new())
        };
        Ok(Vp8Yuv {
            width: fh.width,
            height: fh.height,
            y: out_y,
            u: out_u,
            v: out_v,
            rgba: rgba_field,
            rgb: rgb_field,
        })
    })
}

