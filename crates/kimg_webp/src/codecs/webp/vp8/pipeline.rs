use super::*;

// ---------------------------------------------------------------------------
// 新 MT 管道（t1=串行熵 / t2=预测+IDCT+重建+滤波+YUV）的宏块行包与共享阶段函数。
// 串行路径（`filter_mt_enabled`==false）与 MT 两路径共用 `entropy_macroblock` /
// `reconstruct_macroblock` 同一函数体 → 逐字节一致是结构性成立（硬闸
// `threads2_matches_serial_byte_exact` / `static_lossy_threads2_matches_serial_byte_exact`）。
// ---------------------------------------------------------------------------

/// 单宏块熵解码产物（t1 → t2 的唯一交接对象）。含去量化后的 i16 系数（DC 捷径/双指针核在
/// `reconstruct_macroblock` 按串行同款复用，字节不变）。`Default` 全零 = 未写系数位恒 0。
pub(crate) struct MbPacket {
    y_mode: u8,
    uv_mode: u8,
    /// y_mode==4（16x16）时为真；调用方沿用本仓 `i16` 命名习惯（值命名空间可遮蔽原语类型名）。
    i16: bool,
    nonzero_mask: u32,
    ac_mask: u32,
    b_modes: [u8; 16],
    mb: [[i16; 16]; 25],
}

impl Default for MbPacket {
    fn default() -> Self {
        MbPacket {
            y_mode: 0,
            uv_mode: 0,
            i16: false,
            nonzero_mask: 0,
            ac_mask: 0,
            b_modes: [0; 16],
            mb: [[0i16; 16]; 25],
        }
    }
}

/// 单一宏块行的完成包：`mbs[mbx]`（t2 重建输入）+ `filters[mbx]`（t2 滤波输入，原行滤波参数表
/// `(level, inner)`）。随通道按序交给 t2，t2 依序重建行 r 后滤行 r-1（串行同序）。整包按值
/// 移动（零大块拷贝）；t1 只持有通道发送端。
pub(crate) struct RowPacket {
    pub(crate) filters: Vec<(u8, u8)>,
    pub(crate) mbs: Vec<MbPacket>,
}

/// 单宏块熵解码（模式 + 残差 token + 去量化），产出 [`MbPacket`] 与行滤波参数 `(level, inner)`。
/// 逐条对应原 `decode_from_header::reconstruct_row` 的「阶段一」，只是把 `mb`/`b_modes` 收进
/// 包（每宏块零初始化，与串行 mb 复用数组在「写入前全零」语义上等价——未写位恒 0）。
/// 熵态（`header`/`td` bool 解码器与 `above_nz`/`left_nz`/`above_b`/`left_b`）全程留在 t1。
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
/// t2 依序重建时 `prepare_above` 读到的上一行是**已重建未滤波**像素，与串行（整帧重建毕再滤波）
/// 语义一致 → 字节恒等不依赖行顺序，只依赖「预测读未滤、滤波读已建」。`pkt.mb` 可变：
/// WHT 把 DC 写入 luma 块、用后清零，与串行共享 `mb` 数组的写序完全同构。
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
) {
    let nonzero_mask = pkt.nonzero_mask;
    let ac_mask = pkt.ac_mask;
    let is_i16 = pkt.i16;
    let y_mode = pkt.y_mode;
    let uv_mode = pkt.uv_mode;
    let dy = mby * 16;
    let dx = mbx * 16;
    let dyu = mby * 8;
    let dxu = mbx * 8;

    stage_clock::predict(|| {
        prepare_above(
            above_y, above_u, above_v,
            y, u, v, y_pitch, uv_pitch, mbx, mby, mbw,
        );
        if is_i16 {
            super::simd::predict_large(y, y_pitch, dy, dx, 16, y_mode, mby > 0, mbx > 0, &above_y[..17]);
        }
        super::simd::predict_large(u, uv_pitch, dyu, dxu, 8, uv_mode, mby > 0, mbx > 0, &above_u[..9]);
        super::simd::predict_large(v, uv_pitch, dyu, dxu, 8, uv_mode, mby > 0, mbx > 0, &above_v[..9]);
    });

    stage_clock::idct(|| {
        let mb = &mut pkt.mb;
        let b_modes = &pkt.b_modes;
        stage_clock::mb_mode(is_i16);
        if is_i16 && (nonzero_mask & NZ_Y2) != 0 {
            stage_clock::idct_wht(|| {
                let y2 = mb[24];
                inverse_walsh_to_blocks(&y2, mb, (ac_mask & NZ_Y2) != 0);
                mb[24].fill(0);
            });
            stage_clock::mb_wht();
        }

        if is_i16 {
            stage_clock::idct_i16(|| {
                for by in 0..4 {
                    let row0 = dy + by * 4;
                    for pair_base in [0usize, 2] {
                        let a = by * 4 + pair_base;
                        let b = a + 1;
                        if ((nonzero_mask >> a) & 1) != 0
                            || mb[a][0] != 0
                            || ((nonzero_mask >> b) & 1) != 0
                            || mb[b][0] != 0
                        {
                            if (((ac_mask >> a) | (ac_mask >> b)) & 1) != 0 {
                                super::simd::inverse_dct_add_two(
                                    &mb[a], &mb[b], y, y_pitch, row0, dx + pair_base * 4,
                                );
                            } else {
                                super::simd::dc_add_two(
                                    mb[a][0], mb[b][0], y, y_pitch, row0, dx + pair_base * 4,
                                );
                            }
                            mb[a].fill(0);
                            mb[b].fill(0);
                        }
                    }
                }
            });
        } else {
            // I4：把 predict/add 的 SIMD 特性判定提升出子块循环——`part_simd_on` 含 CPU 特性
            // 探测与多个 OnceLock，逐块调用在汇编上是真函数 call（热路径每块付一次）；判定结果
            // 在整帧解码内恒定（env/特性探测只跑一次），本宏块开头取一次。对比旧路径二处省写：
            // ① add 分支去 `ac_mask` 判断并统一走单块核——仅 DC 块原走标量 `inverse_dct_add
            //   (has_ac=false)`（15.95ns/块，反比 `inverse_dct_add_one` 的 SSE2 单块核 10.56ns/块
            //   慢），且 `idct_add_one_sse2` 对仅 DC 块与标量捷径逐字节一致
            //   （`inverse_dct_has_ac_shortcut_equivalence`）；② 去 `let coeff = mb[block]` 全量
            //   16 系数拷贝（原实现 32B 栈拷 + 回读每块都付，零块也付），核直接借用 `&mb[block]`。
            let pred_simd = super::simd::part_simd_on("pred", "ssse3");
            let dct_simd = super::simd::part_simd_on("dct", "sse2");
            stage_clock::idct_i4(|| {
                // 逐 4×4 子块重建。子块间串行依赖：块 (bx,by) 预测读左/上已重建像素、add 随后
                // 覆写，predict→add 逐块次序不可重排（S7 同版约束）。外层按子块行 / 内层按列扫，
                // `row0`/`col0` 用循环增量直算（删 block→bx/by 反解 &3 / >>2），字节输出与旧
                // 路径逐块次序完全一致。
                let mut block = 0usize;
                for by in 0..4usize {
                    let row0 = dy + by * 4;
                    // 行缓存：by>0 时本子块行上方的 17 字节平面行（raw 偏移 `(row0-1)*pitch+dx..dx+16`）
                    // 一次读入，本行 4 个子块复用（by==0 顶行走既有 `subblock_above` 的 above_y 缓冲，
                    // 无需平面读）。语义：`row1..dy+4by-1` 行在本轮 by 迭代内不被任何子块写（本宏块
                    // 只写行 dy+4by..dy+4by+3），且 by 迭代启动时该行已由上一子块行写毕 → 与逐块
                    // 读原平面逐字节同源，可安全提升到循环外。
                    let mut above_row = [0u8; 17];
                    if by > 0 {
                        let row_above = row0 - 1;
                        debug_assert!(row_above * y_pitch + dx + 16 < y.len());
                        let base = unsafe { y.as_ptr().add(row_above * y_pitch).add(dx) };
                        for k in 0..17usize {
                            above_row[k] = unsafe { *base.add(k) };
                        }
                    }
                    for bx in 0..4usize {
                        let col0 = dx + bx * 4;
                        stage_clock::i4_predict(|| {
                            let above = if by == 0 {
                                subblock_above(y, y_pitch, row0, col0, bx, by, above_y)
                            } else {
                                subblock_above_row(&above_row, bx, above_y)
                            };
                            if pred_simd {
                                // Safety：`pred_simd` 已在循环外判定 `part_simd_on("pred","ssse3")`
                                // 为真且本块内恒定，SSSE3 核指令可用。
                                unsafe {
                                    super::simd::predict_subblock_forced(y, y_pitch, row0, col0, b_modes[block], &above)
                                }
                            } else {
                                super::simd::predict_subblock(y, y_pitch, row0, col0, b_modes[block], &above);
                            }
                        });
                        if ((nonzero_mask >> block) & 1) != 0 || mb[block][0] != 0 {
                            // 块级 AC 分发（镜像 i16/UV 的 `ac_mask` 双块位判定）：`nonzero 位 1 且
                            // ac 位 0` = 纯 DC 块（`tokens` 返回 last==1，非 DC 系数恒 0）——走单块
                            // DC 捷径 `dc_add_one_forced`（(dc+4)>>3 广播，~20 uops，替代全 4×4
                            // IDCT ~118 uops，字节恒等）；ac 位 1 = 有 AC，仍走全 IDCT 核。
                            let has_ac = ((ac_mask >> block) & 1) != 0;
                            stage_clock::i4_idct_add(|| {
                                // 计数器语义：`i4_add_ac` 只计 AC 块；DC-only 块不计（探针折算
                                // `add_dc_n = i4_idct_add_blocks − i4_add_ac_blocks` 即纯 DC 块数）。
                                if has_ac {
                                    stage_clock::i4_add_ac();
                                    if dct_simd {
                                        // Safety：`dct_simd` 已在循环外判定 `part_simd_on("dct","sse2")`
                                        // 为真且本块内恒定，SSE2 核指令可用。
                                        unsafe {
                                            super::simd::idct_add_one_forced(&mb[block], y, y_pitch, row0, col0)
                                        }
                                    } else {
                                        super::simd::inverse_dct_add_one(&mb[block], y, y_pitch, row0, col0);
                                    }
                                } else if dct_simd {
                                    // Safety：同 `has_ac` 分支——SSE2 核指令可用；纯 DC 分发由
                                    // `has_ac==false` 保证（非 DC 系数全 0），捷径输出与全 IDCT 恒等。
                                    unsafe {
                                        super::simd::dc_add_one_forced(mb[block][0], y, y_pitch, row0, col0)
                                    }
                                } else {
                                    // 标量回退：`inverse_dct_add_one` 恒 has_ac=true，对纯 DC 块与
                                    // DC 捷径逐字节一致（既有 `inverse_dct_has_ac_shortcut_equivalence`）。
                                    super::simd::inverse_dct_add_one(&mb[block], y, y_pitch, row0, col0);
                                }
                                mb[block].fill(0);
                            });
                        }
                        block += 1;
                    }
                }
            });
        }

        stage_clock::idct_uv(|| {
            if (nonzero_mask & 0xff0000) != 0 {
                for a in [16usize, 18, 20, 22] {
                    let b = a + 1;
                    let row0 = dyu + ((a >> 1) & 1) * 4;
                    let nz_a = ((nonzero_mask >> a) & 1) != 0 || mb[a][0] != 0;
                    let nz_b = ((nonzero_mask >> b) & 1) != 0 || mb[b][0] != 0;
                    if !(nz_a || nz_b) {
                        continue;
                    }
                    let has_ac = ((ac_mask >> a) | (ac_mask >> b)) & 1 != 0;
                    if a < 20 {
                        if has_ac {
                            super::simd::inverse_dct_add_two(&mb[a], &mb[b], u, uv_pitch, row0, dxu);
                        } else {
                            super::simd::dc_add_two(mb[a][0], mb[b][0], u, uv_pitch, row0, dxu);
                        }
                    } else if has_ac {
                        super::simd::inverse_dct_add_two(&mb[a], &mb[b], v, uv_pitch, row0, dxu);
                    } else {
                        super::simd::dc_add_two(mb[a][0], mb[b][0], v, uv_pitch, row0, dxu);
                    }
                    mb[a].fill(0);
                    mb[b].fill(0);
                }
            }
        });
    });
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
    // 各段内容、写入次序与既有三 Vec 完全一致（全部字节仍在 129 左列初始化 + 重建覆盖内）。
    let y_vec_len = ch * y_pitch;
    let uv_vec_len = uh * uv_pitch;
    let (mut planes, mut rgba, mut out_y, mut out_u, mut out_v) = stage_clock::setup(|| {
        let mut planes = vec![0u8; y_vec_len + 2 * uv_vec_len];
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
            None => unsafe { uninit_vec_u8(width * height * channels) },
        };
        // 紧密平面输出缓冲（真实宽高；仅供测试/兼容 API 消费，生产 rgba 路径弃用 = 纯浪费，
        // 由 `need_tight` 门控跳过以省 1-2 遍整帧写）。L3 未初始化：MT 由 t3 直出、串行由尾段
        // `extract_tight_rows` 写满，返回前不被读，省略 memset 逐字节不变。
        let cwu = (width + 1) / 2;
        let chu = (height + 1) / 2;
        let (out_y, out_u, out_v) = if need_tight {
            (
                unsafe { uninit_vec_u8(width * height) },
                unsafe { uninit_vec_u8(cwu * chu) },
                unsafe { uninit_vec_u8(cwu * chu) },
            )
        } else {
            (Vec::new(), Vec::new(), Vec::new())
        };
        (planes, rgba, out_y, out_u, out_v)
    });
    let (mut y, uv_rest) = planes.split_at_mut(y_vec_len);
    let (mut u, mut v) = uv_rest.split_at_mut(uv_vec_len);

    // 行定稿直出目标：`out=Some`（调用方缓冲，`decode_vp8_into`/`decode_vp8_with_alpha_into`
    // 直写入口）或内部整帧 `rgba`（既有收缓冲路径）。长度校验先于任何像素写入，直接 Err
    // 而非静默截断——与无损侧 `decode_into_rgba8` 的「输出缓冲长度须为 w*h*4」同口径。
    let dst: &mut [u8] = match out {
        Some(o) => {
            let expect = (width as usize)
                .checked_mul(height as usize)
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
        let mut above_b = vec![0u8; mbw * 4];
        let mut above_nz = vec![0u32; mbw];
        let above_y = [127u8; 21];
        let above_u = [127u8; 9];
        let above_v = [127u8; 9];
        // 每行宏块的 (滤波等级, 是否滤内部边)，仅串行路径消费（MT 行包自带滤波参数）。
        let row_filters = vec![(0u8, 0u8); mbw * mbh];
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
        // 领先 2 行，自然背压限内存。y/u/v(planes_A)/planes_B/rgba/out_* 各线程独占，零 unsafe。
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
        let mut pkt = MbPacket::default();
        let mut scratch = EmitScratch::new(width);
        let mut emitted = 0usize;
        for mby in 0..mbh {
            // 每宏块行重置 4 个左邻 b 模式（C# `leftB.Clear()` 同款）。
            let mut left_b = [0u8; 4];
            let mut left_nz = 0u32;
            // 行内复用该分区的 token 解码器（&mut 边界，熵态在宏块级函数内部寄存器驻留）。
            let td = &mut token_decoders[mby & (partition_count - 1)];
            for mbx in 0..mbw {
                let above_b_slot: &mut [u8; 4] =
                    (&mut above_b[mbx * 4..mbx * 4 + 4]).try_into().unwrap();
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
                    &mut pkt,
                );
                row_filters[mby * mbw + mbx] = (level, inner);
                reconstruct_macroblock(
                    &mut y, y_pitch, &mut u, &mut v, uv_pitch,
                    &mut above_y, &mut above_u, &mut above_v,
                    mbx, mby, mbw, &mut pkt,
                );
            }
            // 行窗：重建行 mby 后，上一行 mby-1 的滤波输入已全部就绪（bottom 邻行 mby 已做），
            // 立即滤波上一行并发射其已定稿行 —— 滤波/YUV 读的都是刚写出的热行（消整帧重建毕
            // 再滤波的冷平面重读）。字节恒等：与旧「整帧重建毕 → filter_and_emit_serial」滤波
            // 行序完全一致（filter 0,1,…,mbh-1，逐宏块行同序），且本方案滤波 mby-1 恰在重建
            // mby 之后（bottom 邻行已重建、非滤波污染），预测读未滤波行的不变式保持。
            if mby >= 1 {
                emitted = filter_emit_serial_row(
                    &mut y, y_pitch, &mut u, &mut v, uv_pitch, mbw,
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
            let emitted = filter_emit_serial_row(
                &mut y, y_pitch, &mut u, &mut v, uv_pitch, mbw,
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
                    &y, y_pitch, &u, &v, uv_pitch,
                    0, height, width, &mut out_y, &mut out_u, &mut out_v,
                );
            });
        }
    }

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

