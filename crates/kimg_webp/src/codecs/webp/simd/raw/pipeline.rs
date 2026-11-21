#![allow(unsafe_code)]
// 宏块重建内核：参数多 + above_row 逐位下标循环（字节恒等硬约束，改写迭代器会动 codegen）→ 放行。
#![allow(clippy::too_many_arguments, clippy::needless_range_loop)]
//! 裸 unsafe 叶子子模块 `raw::pipeline`：pipeline/mbdec 域的宏块重建整块迁入
//! （`reconstruct_macroblock` / `reconstruct_i4_mb_ssse3`，原 `vp8/pipeline.rs`）。
//! 上层 `vp8/pipeline.rs` 以 `#![forbid(unsafe_code)]` + 薄委托显式依赖本模块。
//! 算法 / 常量 / 调用顺序与隔离前逐字节一致（字节恒等是硬约束，禁改算术）。
//! `#[target_feature]` SSSE3 汇聚函数 `reconstruct_i4_mb_ssse3` 整块原样搬入（私有依赖
//! `predict_subblock_forced`/`idct_add_one_forced`/`dc_add_one_forced` 经 simd 同级再导出透传，
//! 可在本函数内内联，效果与隔离前逐字节不变）；`reconstruct_macroblock` 的安全 fn 内预测行
//! 裸读下沉 `super::unchecked_index` 门面（内联后同指令，零新增边界检查）。

use super::super::super::vp8::{
    inverse_dct_add, inverse_walsh_to_blocks, predict_subblock, prepare_above, subblock_above,
    subblock_above_row, stage_clock, MbGates, MbPacket, NZ_Y2,
};
use super::super::{
    dc_add_one_forced, dc_add_two_forced, idct_add_one_forced, inverse_dct_add_two_forced,
    predict_large, predict_subblock_forced,
};

/// 单宏块帧内预测 + 逆变换 + 残差写回（原 `decode_from_header`「阶段二/三」，逐字节不变）。
/// t2 依序重建时 `prepare_above` 读到的上一行是**已重建未滤波**像素，与串行（整帧重建毕再滤波）
/// 语义一致 → 字节恒等不依赖行顺序，只依赖「预测读未滤、滤波读已建」。`pkt.mb` 可变：
/// WHT 把 DC 写入 luma 块、用后清零，与串行共享 `mb` 数组的写序完全同构。
/// `gates`：整帧恒定门控（[`MbGates::detect`] 每帧取一次），替换逐宏块/逐块门读。
/// `#[inline(always)]`：串行经 `decode_macroblock` 与熵段同体内联，消灭逐宏块第二次
/// out-of-line 调用+分发；MT t2 `recon_loop` 单调用点同样受益。
/// 本函数自 `vp8/pipeline.rs` 整块迁入（含 unsafe SIMD 分发与 `reconstruct_i4_mb_ssse3`）。
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
            predict_large(y, y_pitch, dy, dx, 16, y_mode, mby > 0, mbx > 0, &above_y[..17]);
        }
        predict_large(u, uv_pitch, dyu, dxu, 8, uv_mode, mby > 0, mbx > 0, &above_u[..9]);
        predict_large(v, uv_pitch, dyu, dxu, 8, uv_mode, mby > 0, mbx > 0, &above_v[..9]);
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
            // DCT SIMD 判定提升出 i16 对循环（补齐与 i4 段 pipeline.rs `dct_simd` 的不对称）：
            // `part_simd_on` 含 CPU 特性探测与多个 OnceLock，逐对调用在汇编上是真函数 call
            // （热路径每宏块 8 对各付一次 ≈15-25ns/调）；判定在整帧解码内恒定（env/特性探测
            // 只跑一次），本宏块开头取一次，循环内改调 `_forced` 变体。`KIMG_WEBP_NO_SIMD_DCT=1`
            // / `KIMG_WEBP_NO_SIMD=1` / 非 x86_64 被判为假时走标量回退，关闭途径保留。
            let dct_simd = gates.dct_simd;
            // IDCT 第二遍 i16 域开关同样提升出对循环（同 `dct_simd` 姿势，K93 双路径）：
            // `idct_i16_enabled()` 内部是 OnceLock 读取 + 分支（asm 每块 `movl ON(%rip);testl;jne`），
            // 判定在整帧解码内恒定，本宏块取一次、循环内经 `_forced` 核透传局部 bool。
            let i16_en = gates.i16_en;
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
                            if dct_simd {
                                if (((ac_mask >> a) | (ac_mask >> b)) & 1) != 0 {
                                    // Safety：`dct_simd` 已在循环外判定 `part_simd_on("dct","sse2")`
                                    // 为真且本对块内恒定，SSE2 核指令可用。
                                    unsafe {
                                        inverse_dct_add_two_forced(
                                            &mb[a], &mb[b], y, y_pitch, row0, dx + pair_base * 4, i16_en,
                                        )
                                    }
                                } else {
                                    // Safety：同 AC 分支——SSE2 核可用；纯 DC 分发由 `ac_mask`
                                    // 双块位 0 保证（非 DC 系数全 0），捷径输出与全 IDCT 恒等。
                                    unsafe {
                                        dc_add_two_forced(
                                            mb[a][0], mb[b][0], y, y_pitch, row0, dx + pair_base * 4,
                                        )
                                    }
                                }
                            } else {
                                // 标量：直调 vp8 标量核（恒 has_ac=true，对纯 DC 块与 DC 捷径逐字节
                                // 一致，既有 `inverse_dct_has_ac_shortcut_equivalence`）。不可再走
                                // `simd::inverse_dct_add_two`/`dc_add_two`——其内部会重判门控，本分支
                                // 已被 `dct_simd` 判为假（如 `KIMG_WEBP_NO_SIMD_DCT=1`），须保留关闭途径
                                // 的标量语义。
                                if (((ac_mask >> a) | (ac_mask >> b)) & 1) != 0 {
                                    inverse_dct_add(
                                        &mb[a], y, y_pitch, row0, dx + pair_base * 4, true,
                                    );
                                    inverse_dct_add(
                                        &mb[b], y, y_pitch, row0, dx + pair_base * 4 + 4, true,
                                    );
                                } else {
                                    let mut ca = [0i16; 16];
                                    let mut cb = [0i16; 16];
                                    ca[0] = mb[a][0];
                                    cb[0] = mb[b][0];
                                    inverse_dct_add(&ca, y, y_pitch, row0, dx + pair_base * 4, true);
                                    inverse_dct_add(&cb, y, y_pitch, row0, dx + pair_base * 4 + 4, true);
                                }
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
            // i4 段门控：`pred_simd`/`dct_simd` 取自整帧恒定的 `gates`（`dct_simd` 与 i16/UV 段
            // 不同：i4 段须与 `i4_simd` 合取，保持既有分支语义）。
            let pred_simd = gates.pred_simd;
            let dct_simd = gates.i4_simd && gates.dct_simd;
            // `idct_i16_enabled()`（K93 第二遍 i16 域开关）同提升：逐块调用是同一 OnceLock 读取
            // + 分支（asm 每块 `movl ON(%rip);testl;jne`），判定在整帧解码内恒定，本宏块取一次
            // 透传 `idct_add_one_forced`，循环内用局部 bool。
            let i16_en = gates.i16_en;
            stage_clock::idct_i4(|| {
                if pred_simd {
                    // Coder Iteration 4：i4 子块壳批量化——把整段 16 块 predict→add 提升进单个
                    // `#[target_feature(enable="ssse3")]` 汇聚函数 `reconstruct_i4_mb_ssse3`：
                    // SSSE3/SIMD 核在函数内可内联（调用方特征 ⊇ 被调方），above/系数/常数寄存器
                    // 驻留，消除旧路径每块真 `callq predict_subblock_ssse3` 与栈往返（壳≈30%/AC 块）。
                    // Safety：`pred_simd` = `KIMG_WEBP_I4_SIMD`（含测试覆盖）开 && `part_simd_on(
                    // "pred","ssse3")` 为真（含 `is_x86_feature_detected!("ssse3")` 运行时探测）→
                    // 调用 SSSE3 汇聚函数安全；其余指针/越界语义与逐块路径一致（`subblock_above`
                    // 不变量）。`dct_simd`/`i16_en` 判定同在外层（本宏块内恒定）透传。
                    #[cfg(target_arch = "x86_64")]
                    {
                        unsafe {
                            reconstruct_i4_mb_ssse3(
                                y, y_pitch, above_y, &mut *mb, b_modes, nonzero_mask, ac_mask, dct_simd,
                                i16_en, dy, dx,
                            )
                        }
                        return;
                    }
                    #[cfg(not(target_arch = "x86_64"))]
                    {
                        // 非 x86_64：`pred_simd` 恒为假（`simd_on` 恒 false），本段不可达；占位避免
                        // 对仅 x86_64 编译的 `reconstruct_i4_mb_ssse3` 的引用。
                        unreachable!("pred_simd 在非 x86_64 恒为 false")
                    }
                }
                // 标量逐块路径原样保留（字节恒等基线）：`pred_simd` 为假 = `KIMG_WEBP_I4_SIMD=0` /
                // pred SSSE3 关 / 非 x86_64 → 每块直调 vp8 标量 predict，add 分发不变（上方
                // SSSE3 分支 `return` 后走到本段）。
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
                        let row_base = row_above * y_pitch + dx;
                        debug_assert!(row_base + 16 < y.len());
                        for k in 0..17usize {
                            above_row[k] = super::unchecked_index(&*y, row_base + k);
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
                                    predict_subblock_forced(y, y_pitch, row0, col0, b_modes[block], &above)
                                }
                            } else {
                                // 标量：直调 vp8 标量核。注意不可再走 `simd::predict_subblock`——
                                // 其内部按部件门控 `part_simd_on("pred","ssse3")` 判定，`KIMG_WEBP_I4_SIMD=0`
                                // 只关 i4 时该门控仍为真会误入 SSSE3，破坏「i4 标量回退」语义。
                                predict_subblock(y, y_pitch, row0, col0, b_modes[block], &above);
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
                                            idct_add_one_forced(&mb[block], y, y_pitch, row0, col0, i16_en)
                                        }
                                    } else {
                                        // 标量：直调 vp8 标量核（恒 has_ac=true）。不可走
                                        // `simd::inverse_dct_add_one`——其内部按部件门控判定，
                                        // `KIMG_WEBP_I4_SIMD=0` 只关 i4 时会误入 SSE2。
                                        inverse_dct_add(&mb[block], y, y_pitch, row0, col0, true);
                                    }
                                } else if dct_simd {
                                    // Safety：同 `has_ac` 分支——SSE2 核指令可用；纯 DC 分发由
                                    // `has_ac==false` 保证（非 DC 系数全 0），捷径输出与全 IDCT 恒等。
                                    unsafe {
                                        dc_add_one_forced(mb[block][0], y, y_pitch, row0, col0)
                                    }
                                } else {
                                    // 标量回退：`inverse_dct_add` 恒 has_ac=true，对纯 DC 块与
                                    // DC 捷径逐字节一致（既有 `inverse_dct_has_ac_shortcut_equivalence`）。
                                    inverse_dct_add(&mb[block], y, y_pitch, row0, col0, true);
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
                // DCT SIMD 判定提升出 UV 对循环（同 i16 段 `dct_simd` 姿势）：每宏块 4 对，
                // 循环外判定一次，循环内改调 `_forced` 变体；关闭途径（`KIMG_WEBP_NO_SIMD_DCT=1`
                // / `KIMG_WEBP_NO_SIMD=1` / 非 x86_64）走标量回退保留。
                let dct_simd = gates.dct_simd;
                // `idct_i16_enabled()`（K93 第二遍 i16 域开关）同提升出 UV 对循环：判定在整帧
                // 解码内恒定，本宏块取一次、经 `inverse_dct_add_two_forced` 透传，省去逐对
                // OnceLock 读取 + 分支（asm 每对 `movl ON(%rip);testl;jne`）。
                let i16_en = gates.i16_en;
                for a in [16usize, 18, 20, 22] {
                    let b = a + 1;
                    let row0 = dyu + ((a >> 1) & 1) * 4;
                    let nz_a = ((nonzero_mask >> a) & 1) != 0 || mb[a][0] != 0;
                    let nz_b = ((nonzero_mask >> b) & 1) != 0 || mb[b][0] != 0;
                    if !(nz_a || nz_b) {
                        continue;
                    }
                    let has_ac = ((ac_mask >> a) | (ac_mask >> b)) & 1 != 0;
                    let (plane, col0) = if a < 20 {
                        (&mut *u, dxu)
                    } else {
                        (&mut *v, dxu)
                    };
                    if dct_simd {
                        // Safety：`dct_simd` 已在循环外判定 `part_simd_on("dct","sse2")` 为真且
                        // 本对块内恒定，SSE2 核可用；纯 DC 分发由 `ac_mask` 双块位 0 保证。
                        if has_ac {
                            unsafe {
                                inverse_dct_add_two_forced(
                                    &mb[a], &mb[b], plane, uv_pitch, row0, col0, i16_en,
                                )
                            }
                        } else {
                            unsafe {
                                dc_add_two_forced(
                                    mb[a][0], mb[b][0], plane, uv_pitch, row0, col0,
                                )
                            }
                        }
                    } else if has_ac {
                        // 标量：直调 vp8 标量核（同 i16 段 `dct_simd==false` 分支，保留关闭途径）。
                        inverse_dct_add(&mb[a], plane, uv_pitch, row0, col0, true);
                        inverse_dct_add(&mb[b], plane, uv_pitch, row0, col0 + 4, true);
                    } else {
                        let mut ca = [0i16; 16];
                        let mut cb = [0i16; 16];
                        ca[0] = mb[a][0];
                        cb[0] = mb[b][0];
                        inverse_dct_add(&ca, plane, uv_pitch, row0, col0, true);
                        inverse_dct_add(&cb, plane, uv_pitch, row0, col0 + 4, true);
                    }
                    mb[a].fill(0);
                    mb[b].fill(0);
                }
            }
        });
    });
}

/// Coder Iteration 4：i4 子块壳批量化汇聚函数——把「Y 平面一个 i4 宏块的 16 个 4×4 块」的
/// predict→idct-add 完整循环整体提升进单个 `#[target_feature(enable="ssse3")]` 函数（SSSE3
/// 可用路径；非 x86_64 经 `#[cfg]` 剔除）。效果：`predict_subblock_forced`/`idct_add_one_forced`/
/// `dc_add_one_forced` 在本函数内可内联（调用方特征 ssse3 ⊇ 被调方 sse2/ssse3），消灭旧路径
/// 「非 ssse3 的 `reconstruct_macroblock` 调 SSSE3/SIMD 核」的逐块真 `callq` 与 above/系数栈
/// 往返，above/常数/指针寄存器驻留（壳≈30%/AC 块）。**字节恒等硬闸**：块序、predict→add
/// 逐块次序、行缓存（66fcbc9）、AC/DC 分发、`dct_simd`/`i16_en` 关闭途径与
/// `reconstruct_macroblock` 的 i4 标量逐块路径完全同构；predict 本函数内恒走 SSSE3 核（调用方
/// 仅在 `pred_simd` 为真时进入，无需标量回退分支）。
/// **Safety**：调用方须保证 CPU 具 ssse3（`pred_simd` = `i4_simd_enabled()` &&
/// `part_simd_on("pred","ssse3")` 为真，经 `is_x86_feature_detected!("ssse3")` 运行时探测）；
/// `y`/`above_y` 越界语义同 i4 逐块路径（`subblock_above` 不变量注释）。
/// 自 `vp8/pipeline.rs` 整块迁入（内核内容未重打字；含其私有安全依赖 `subblock_above`/
/// `subblock_above_row`/`stage_clock` 与 SIMD `_forced` 再导出透传）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "ssse3")]
unsafe fn reconstruct_i4_mb_ssse3(
    y: &mut [u8],
    y_pitch: usize,
    above_y: &[u8; 21],
    mb: &mut [[i16; 16]; 25],
    b_modes: &[u8; 16],
    nonzero_mask: u32,
    ac_mask: u32,
    dct_simd: bool,
    i16_en: bool,
    dy: usize,
    dx: usize,
) {
    // 逐 4×4 子块重建。子块间串行依赖：块 (bx,by) 预测读左/上已重建像素、add 随后覆写，
    // predict→add 逐块次序不可重排（S7 同版约束）。外层按子块行 / 内层按列扫，`row0`/`col0`
    // 用循环增量直算，字节输出与标量逐块路径逐块次序完全一致。
    let mut block = 0usize;
    for by in 0..4usize {
        let row0 = dy + by * 4;
        // 行缓存：by>0 时本子块行上方的 17 字节平面行（raw 偏移 `(row0-1)*pitch+dx..dx+16`）
        // 一次读入，本行 4 个子块复用（by==0 顶行走既有 `subblock_above` 的 above_y 缓冲，
        // 无需平面读）。语义：本宏块只写行 dy+4by..dy+4by+3，by 迭代启动时上方行已由上一子块
        // 行写毕 → 与逐块读原平面逐字节同源，可安全提升到循环外。
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
                // Safety：本函数仅在 `pred_simd`（内含 ssse3 运行时探测）为真时被调用，SSSE3
                // 核指令可用；predict 恒走向量核（调用方已保证 pred_simd，无标量回退分支）。
                unsafe {
                    predict_subblock_forced(y, y_pitch, row0, col0, b_modes[block], &above)
                }
            });
            if ((nonzero_mask >> block) & 1) != 0 || mb[block][0] != 0 {
                // 块级 AC 分发（镜像 i16/UV 的 `ac_mask` 双块位判定）：`nonzero 位 1 且
                // ac 位 0` = 纯 DC 块（`tokens` 返回 last==1，非 DC 系数恒 0）——走单块
                // DC 捷径 `dc_add_one_forced`（(dc+4)>>3 广播，字节恒等）；ac 位 1 = 有 AC，
                // 仍走全 IDCT 核。
                let has_ac = ((ac_mask >> block) & 1) != 0;
                stage_clock::i4_idct_add(|| {
                    // 计数器语义：`i4_add_ac` 只计 AC 块；DC-only 块不计（探针折算
                    // `add_dc_n = i4_idct_add_blocks − i4_add_ac_blocks` 即纯 DC 块数）。
                    if has_ac {
                        stage_clock::i4_add_ac();
                        if dct_simd {
                            // Safety：`dct_simd` 由调用方提升判定 `part_simd_on("dct","sse2")`
                            // 为真且本块内恒定（sse2 ⊆ ssse3，本函数内可内联）。
                            unsafe {
                                idct_add_one_forced(&mb[block], y, y_pitch, row0, col0, i16_en)
                            }
                        } else {
                            // 标量：直调 vp8 标量核（恒 has_ac=true）。`KIMG_WEBP_I4_SIMD=0` /
                            // `KIMG_WEBP_NO_SIMD_DCT=1` 关闭途径保留（dct_simd 为假分支）。
                            inverse_dct_add(&mb[block], y, y_pitch, row0, col0, true);
                        }
                    } else if dct_simd {
                        // Safety：同 `has_ac` 分支——SSE2 核指令可用；纯 DC 分发由
                        // `has_ac==false` 保证（非 DC 系数全 0），捷径输出与全 IDCT 恒等。
                        unsafe {
                            dc_add_one_forced(mb[block][0], y, y_pitch, row0, col0)
                        }
                    } else {
                        // 标量回退：`super::inverse_dct_add` 恒 has_ac=true，对纯 DC 块与
                        // DC 捷径逐字节一致（既有 `inverse_dct_has_ac_shortcut_equivalence`）。
                        inverse_dct_add(&mb[block], y, y_pitch, row0, col0, true);
                    }
                    mb[block].fill(0);
                });
            }
            block += 1;
        }
    }
}
