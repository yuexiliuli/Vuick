use super::*;

// ---------------------------------------------------------------------------
// V2：系数 token 解码与宏块重建
// ---------------------------------------------------------------------------

/// 每位置的 (band0 行偏移)（RFC 6386 §13.3）：band(position) × 3 × 11。
pub(crate) const BAND_ROW_OFFSETS: [usize; 16] = [
    0, 33, 66, 99, 198, 132, 165, 198, 198, 198, 198, 198, 198, 198, 198, 231,
];

/// 各块在 above/left 非零掩码中的位。
pub(crate) const BLOCK_ABOVE_CONTEXT_BIT: [u32; 25] = [
    0, 1, 2, 3, 0, 1, 2, 3, 0, 1, 2, 3, 0, 1, 2, 3, 4, 5, 4, 5, 6, 7, 6, 7, 24,
];
pub(crate) const BLOCK_LEFT_CONTEXT_BIT: [u32; 25] = [
    0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 24,
];

/// Y2 块的掩码位（低 32 位掩码的位 24）。
pub(crate) const NZ_Y2: u32 = 1 << 24;

/// RFC 6386 §13.2 DCT_CAT3..6 子树（p8 之后）附额外位。熵态以调用方局部标量携带（寄存器驻留，
/// 同 `tokens` 标量化 Load/Store 姿势；`data`/`pos` 仅在 refill 触碰）。概率读用裸指针游标 `p`
/// （`tokens` 传入，指向当前 p_idx 处）：读 `p[k]`（k=8..10）。不变量与 `tokens` 同证：
/// p_idx ≤ 253、p_idx+10 ≤ 263 < 264（每型概率表长）→ 恒不越界。
///
/// **RC3**：CAT8/9/10（p[8..10]，偏斜极高但低频）随 ZERO/G5 同机制做预测分支形——位判定与
/// 调用点 `if b==0` 体合并为同一分支（`ent_branch=true`，默认）；false 走旧 cmov 形。两形输出
/// 逐字节一致（共享 `ent_split_pre`/`ent_renorm_apply` 单源算术）。
#[inline(always)]
fn read_category_3_to_6(
    data: &[u8],
    pos: &mut usize,
    value: &mut u64,
    bits: &mut i32,
    range_minus1: &mut u32,
    renorm_skip: bool,
    ent_branch: bool,
    p: *const u8,
) -> i32 {
    let (extra, base): (&[u8], i32) = {
        if ent_branch {
            // 预测分支形：CAT8 位判定直接路由（多数方 bit=0 只算本路熵态，单误判点）。
            let (vs, sp, rm1) = ent_split_pre(data, pos, value, bits, range_minus1, unsafe { *p.add(8) });
            if vs > u64::from(sp) {
                // bit=1（少数方 0.2%）：CAT10 分支（偏斜 0.94，多数方 bit=0）。
                *value -= (u64::from(sp + 1)) << (*bits as u32);
                ent_renorm_apply(bits, range_minus1, rm1 - sp, renorm_skip);
                probe_skew!(skew_clock::CAT8, 1);
                let (vs, sp, rm1) = ent_split_pre(data, pos, value, bits, range_minus1, unsafe { *p.add(10) });
                if vs > u64::from(sp) {
                    *value -= (u64::from(sp + 1)) << (*bits as u32);
                    ent_renorm_apply(bits, range_minus1, rm1 - sp, renorm_skip);
                    probe_skew!(skew_clock::CAT10, 1);
                    (&PCAT6, 67)
                } else {
                    ent_renorm_apply(bits, range_minus1, sp + 1, renorm_skip);
                    probe_skew!(skew_clock::CAT10, 0);
                    (&PCAT5, 35)
                }
            } else {
                // bit=0（多数方 99.8%，预测命中）：CAT9 分支（偏斜 0.893，多数方 bit=0）。
                ent_renorm_apply(bits, range_minus1, sp + 1, renorm_skip);
                probe_skew!(skew_clock::CAT8, 0);
                let (vs, sp, rm1) = ent_split_pre(data, pos, value, bits, range_minus1, unsafe { *p.add(9) });
                if vs > u64::from(sp) {
                    *value -= (u64::from(sp + 1)) << (*bits as u32);
                    ent_renorm_apply(bits, range_minus1, rm1 - sp, renorm_skip);
                    probe_skew!(skew_clock::CAT9, 1);
                    (&PCAT4, 19)
                } else {
                    ent_renorm_apply(bits, range_minus1, sp + 1, renorm_skip);
                    probe_skew!(skew_clock::CAT9, 0);
                    (&PCAT3, 11)
                }
            }
        } else {
            // cmov 全算（A/B 对照侧）：与分支形逐字节一致，唯指令选择不同。
            let b = ent_read_bit(data, pos, value, bits, range_minus1, renorm_skip, unsafe { *p.add(8) });
            probe_skew!(skew_clock::CAT8, b);
            if b == 0 {
                let b = ent_read_bit(data, pos, value, bits, range_minus1, renorm_skip, unsafe { *p.add(9) });
                probe_skew!(skew_clock::CAT9, b);
                if b == 0 {
                    (&PCAT3, 11)
                } else {
                    (&PCAT4, 19)
                }
            } else {
                let b = ent_read_bit(data, pos, value, bits, range_minus1, renorm_skip, unsafe { *p.add(10) });
                probe_skew!(skew_clock::CAT10, b);
                if b == 0 {
                    (&PCAT5, 35)
                } else {
                    (&PCAT6, 67)
                }
            }
        }
    };
    let mut mag = 0i32;
    let mut i = 0;
    while extra[i] != 0 {
        let b = ent_read_bit(data, pos, value, bits, range_minus1, renorm_skip, extra[i]);
        probe_skew!(skew_clock::EXTRA, b);
        mag = (mag << 1) | b as i32;
        i += 1;
    }
    base + mag
}

/// 读一个块的系数 token（RFC 6386 §13.2），去量化后按 zigzag 写回 `output`（调用前须清零）。
/// 返回最后非零 token 的 zigzag 位置 + 1（无 token 返回 0）。熵态以调用方局部标量携带 +
/// `#[inline(always)]` 使每宏块 25 次调用内联成单函数、{value,bits,range} 全程寄存器驻留
/// （C# `Tokens(ref v, ref b, ref r)` 同款 Load/Store；`data`/`pos` 仅在 refill 触碰）。
#[inline(always)]
pub(crate) fn tokens(
    data: &[u8],
    pos: &mut usize,
    value: &mut u64,
    bits: &mut i32,
    range_minus1: &mut u32,
    renorm_skip: bool,
    ent_branch: bool,
    probabilities: &[u8],
    context: usize,
    mut position: usize,
    output: &mut [i16; 16],
    dc_factor: i32,
    ac_factor: i32,
) -> usize {
    // 熵概率读不变量（证明）：`BAND_ROW_OFFSETS` 最大 231（position≤15），context≤2、
    // next_context∈{11,22} ⇒ p_idx ≤ 253；概率表按型分块 8*33=264 字节、`probabilities` 切片长
    // 恒 ≥ 264（`parse_residuals` 传 `probs[ty*264..]`，表总长 1056）⇒ p_idx+10 ≤ 263 < 264。
    // 故裸指针游标 `p = base.add(p_idx)` 读 `p[k]`（k≤10）恒不越界——用指针游标省去每次
    // `probabilities[p_idx+k]` 的边界检查，读到的概率字节与下标版逐字节相同（libwebp vp8_dec.c
    // 的 `const uint8_t* p` 滑动游标同款姿势，参考不抄）。
    //
    // 环内 `get_unchecked` 不变量（A 记账压紧·减少每系数索引算术）：`position` 两侧自增后必先
    // 判 `position == 16` break 才续用（1208-1272），故三处 `BAND_ROW_OFFSETS[position]` /
    // `ZIGZAG[position]`（1217/1258/1267）恒有 position ≤ 15 < 16——去掉必然不触发的边界检查
    // 每次索引省 1-2 uop（cmp+溢出分支），读值与下标版逐字节相同。
    let p_idx = BAND_ROW_OFFSETS[position] + context * 11;
    debug_assert!(p_idx + 10 < probabilities.len());
    let base = probabilities.as_ptr();
    let mut p = unsafe { base.add(p_idx) };
    stage_clock::acct();
    let mut last = 0usize;

    // ── RC3：ZERO/G5 两高偏斜决策点（p[1]/p[6]）在「预测分支形（默认）」与「cmov 全算
    // （KIMG_WEBP_ENT_BRANCH=0 强制）」间运行时切换。两形体共用下方局部宏（体只写一份），
    // bmi/base 两变体同调本函数 → 宏单源同步获得同一分支结构（K 约束）。分支形把位判定与
    // 调用点 `if b==0` 体合并为同一分支（避免同一位双分支双误判；多数方命中只算本路熵态）。
    /// G5 的多数方（bit=0，偏斜 89.6%）体：F67→F159 / F165+F145 链，产出 (mag, 22)。
    /// F67/F159/F165/F145 偏斜 54-64% 净负，保留 cmov 形不动（抛硬币/近硬币点禁止改）。
    macro_rules! g5_zero_body {
        () => {{
            let b = ent_read_bit(data, pos, value, bits, range_minus1, renorm_skip, unsafe { *p.add(7) });
            probe_skew!(skew_clock::F67, b);
            if b == 0 {
                let b = ent_read_bit(data, pos, value, bits, range_minus1, renorm_skip, 159);
                probe_skew!(skew_clock::F159, b);
                (5 + b as i32, 22)
            } else {
                let b = ent_read_bit(data, pos, value, bits, range_minus1, renorm_skip, 165);
                probe_skew!(skew_clock::F165, b);
                let high = b << 1;
                let b = ent_read_bit(data, pos, value, bits, range_minus1, renorm_skip, 145);
                probe_skew!(skew_clock::F145, b);
                (7 + (high | b) as i32, 22)
            }
        }};
    }
    /// 非零 token 体：幅值树（ONE/LT5/TWO/F34/G5）→ 符号 → 去量 → zigzag 写回 → 下一位置 EOB。
    /// 以块表达式产出 (mag, next_context) 后走共享尾部；G5 决策点据此切换分支/cmov 两形。
    macro_rules! nonzero_token_body {
        () => {{
            let (mag, next_context): (i32, usize) = {
                let b = ent_read_bit(data, pos, value, bits, range_minus1, renorm_skip, unsafe { *p.add(2) });
                probe_skew!(skew_clock::ONE, b);
                if b == 0 {
                    (1, 11)
                } else {
                    let b = ent_read_bit(data, pos, value, bits, range_minus1, renorm_skip, unsafe { *p.add(3) });
                    probe_skew!(skew_clock::LT5, b);
                    if b == 0 {
                        let b = ent_read_bit(data, pos, value, bits, range_minus1, renorm_skip, unsafe { *p.add(4) });
                        probe_skew!(skew_clock::TWO, b);
                        if b == 0 {
                            (2, 22)
                        } else {
                            let b = ent_read_bit(data, pos, value, bits, range_minus1, renorm_skip, unsafe { *p.add(5) });
                            probe_skew!(skew_clock::F34, b);
                            (3 + b as i32, 22)
                        }
                    } else {
                        // G5 决策（p[6]，多数方 bit=0 偏斜 89.6%）：分支形与 cmov 形运行时切换。
                        if ent_branch {
                            let (vs, sp, rm1) = ent_split_pre(data, pos, value, bits, range_minus1, unsafe { *p.add(6) });
                            if vs > u64::from(sp) {
                                // bit=1（少数方 10.4%）：DCT_CAT3..6。
                                *value -= (u64::from(sp + 1)) << (*bits as u32);
                                ent_renorm_apply(bits, range_minus1, rm1 - sp, renorm_skip);
                                probe_skew!(skew_clock::G5, 1);
                                let cat = read_category_3_to_6(data, pos, value, bits, range_minus1, renorm_skip, ent_branch, p);
                                (cat, 22)
                            } else {
                                // bit=0（多数方 89.6%，预测命中）：F67 链。
                                ent_renorm_apply(bits, range_minus1, sp + 1, renorm_skip);
                                probe_skew!(skew_clock::G5, 0);
                                g5_zero_body!()
                            }
                        } else {
                            let b = ent_read_bit(data, pos, value, bits, range_minus1, renorm_skip, unsafe { *p.add(6) });
                            probe_skew!(skew_clock::G5, b);
                            if b == 0 {
                                g5_zero_body!()
                            } else {
                                let cat = read_category_3_to_6(data, pos, value, bits, range_minus1, renorm_skip, ent_branch, p);
                                (cat, 22)
                            }
                        }
                    }
                }
            };
            let factor = if position == 0 { dc_factor } else { ac_factor };
            // 无分支符号（C# ReadSigned：符号是抛硬币，分支一半预测失败）。
            let signed = ent_read_signed(data, pos, value, bits, range_minus1, mag);
            stage_clock::dq();
            output[unsafe { *ZIGZAG.get_unchecked(position) } as usize] = (signed * factor) as i16;
            stage_clock::acct();
            last = position + 1;
            stage_clock::acct();
            position += 1;
            if position == 16 {
                break;
            }
            stage_clock::acct();
            p = unsafe { base.add(*BAND_ROW_OFFSETS.get_unchecked(position) + next_context) };
            // ── C2：下一位置 EOB 决策点（p[0]）分支形/cmov 切换（同 RC3 ZERO 样式）：分支形把位
            // 判定与「break/续读」体合并为同一分支（单误判点；预测命中方只算本路熵态）。稀疏多数方
            // bit=0 → EOB 命中 break；噪声多数方 bit=1 → 续读环顶。
            if ent_branch {
                let (vs, sp, rm1) = ent_split_pre(data, pos, value, bits, range_minus1, unsafe { *p });
                if vs > u64::from(sp) {
                    // bit=1（续读）：更新熵态，回环顶续读下一系数。
                    *value -= (u64::from(sp + 1)) << (*bits as u32);
                    ent_renorm_apply(bits, range_minus1, rm1 - sp, renorm_skip);
                    probe_skew!(skew_clock::EOB, 1);
                } else {
                    // bit=0（EOB 命中，多数方）：只算本路熵态，块结束。
                    ent_renorm_apply(bits, range_minus1, sp + 1, renorm_skip);
                    probe_skew!(skew_clock::EOB, 0);
                    break;
                }
            } else {
                let b = ent_read_bit(data, pos, value, bits, range_minus1, renorm_skip, unsafe { *p });
                probe_skew!(skew_clock::EOB, b);
                if b == 0 {
                    break; // 下一位置 EOB。
                }
            }
        }};
    }
    /// 零 token 体：前进到下一位置（零 token 不写字），位置到 16 退出。
    macro_rules! zero_token_body {
        () => {{
            stage_clock::acct();
            position += 1;
            if position == 16 {
                break;
            }
            stage_clock::acct();
            p = unsafe { base.add(*BAND_ROW_OFFSETS.get_unchecked(position)) };
            continue;
        }};
    }

    // ── C2：EOB 初始决策点（p[0]，块首）在「预测分支形（默认）」与「cmov 全算（KIMG_WEBP_ENT_BRANCH=0
    // 强制）」间运行时切换，与 RC3 ZERO/G5 同构：分支形把位判定与「进环/空块」体合并为同一分支
    // （单误判点；预测命中方只算本路熵态）。EOB 全带高偏斜（稀疏块 m≈0.88 bit=0、噪声块 m≈0.99
    // bit=1，均 ≥85%）→ 默认开、无需内容门控。bit=0 = 空块（不读系数）。
    'eob: {
        if ent_branch {
            let (vs, sp, rm1) = ent_split_pre(data, pos, value, bits, range_minus1, unsafe { *p });
            if vs > u64::from(sp) {
                // bit=1（块有内容）：更新熵态后进系数环。
                *value -= (u64::from(sp + 1)) << (*bits as u32);
                ent_renorm_apply(bits, range_minus1, rm1 - sp, renorm_skip);
                probe_skew!(skew_clock::EOB, 1);
            } else {
                // bit=0（空块，多数方命中）：只算本路熵态，整块空。
                ent_renorm_apply(bits, range_minus1, sp + 1, renorm_skip);
                probe_skew!(skew_clock::EOB, 0);
                break 'eob;
            }
        } else {
            let b = ent_read_bit(data, pos, value, bits, range_minus1, renorm_skip, unsafe { *p });
            probe_skew!(skew_clock::EOB, b);
            if b == 0 {
                break 'eob;
            }
        }
        loop {
            // ZERO 决策（p[1]，多数方 bit=0 偏斜 85.2%）：分支形与 cmov 形运行时切换。分支形把
            // 位判定与 `if b==0` 体合并为同一分支（单误判点；多数方命中只算本路熵态），两形共享
            // 单源算术（`ent_split_pre`/`ent_renorm_apply`）→ 输出逐字节一致。
            if ent_branch {
                let (vs, sp, rm1) = ent_split_pre(data, pos, value, bits, range_minus1, unsafe { *p.add(1) });
                if vs > u64::from(sp) {
                    // bit=1（少数方 14.8%）：零未命中，更新熵态后落入非零 token 体。
                    *value -= (u64::from(sp + 1)) << (*bits as u32);
                    ent_renorm_apply(bits, range_minus1, rm1 - sp, renorm_skip);
                    probe_skew!(skew_clock::ZERO, 1);
                    nonzero_token_body!();
                } else {
                    // bit=0（多数方 85.2%，预测命中）：只算本路熵态，直接落零 token 体。
                    ent_renorm_apply(bits, range_minus1, sp + 1, renorm_skip);
                    probe_skew!(skew_clock::ZERO, 0);
                    zero_token_body!();
                }
            } else {
                let b = ent_read_bit(data, pos, value, bits, range_minus1, renorm_skip, unsafe { *p.add(1) });
                probe_skew!(skew_clock::ZERO, b);
                if b == 0 {
                    zero_token_body!();
                }
                nonzero_token_body!();
            }
        }
    }
    last
}

/// 解析一个宏块全部系数（Y2 先于 luma 当 i16）。返回两块掩码：低 32 = 每块非零，高 32 = 有 AC。
/// 每宏块一次 Load/Store（对照 C# `ParseResiduals` 的 `decoder.Load(out v, out b, out r)` /
/// `decoder.Store(v, b, r)`）：熵态三标量出结构入本地 SSA，函数边界用 `&mut` 只在进出各一次
/// {value,bits,range}，`data`/`pos` 仅在 refill 触碰；内部 tokens 内联后全为寄存器。
///
/// **RC1 双路径**：`parse_residuals` 为分发器，按运行时一次判定进 `parse_residuals_bmi`
/// （`#[target_feature(enable="lzcnt,bmi1,bmi2")]` 快道）或 `parse_residuals_base`（基线
/// bsr+xor/mov→cl）。两变体函数体由下方宏展开自同一算术源——保证「bmi ≡ baseline」逐字节
/// 一致是结构性的（仅指令选择不同）：inlined `tokens`/`ent_read_bit` 的 `range.leading_zeros()`
/// 快道发 `lzcnt`（Haswell 1c vs bsr 3c）、变长移位发 `shrx/shlx`（免 `mov→cl` 依赖链）；
/// 基线路径与 RC1 前逐指令相同（bsr+xorl $31）。分发每宏块一次（`ent_bmi_enabled()` 惰性
/// OnceLock 原子读 + 恒定可预测分支），t1 主循环（MT 与串行共用 `entropy_macroblock`）都受益。
macro_rules! parse_residuals_impl {
    (
        $d:ident, $probabilities:ident, $mb:ident, $sp:ident, $i16:ident, $above_nz:ident, $left_nz:ident
    ) => ({
        // Load（每宏块一次，仿 C# `ParseResiduals` 的 `decoder.Load(out v, out b, out r)`）：
        // 熵态三标量出结构入调用方局部变量，走完 Store 回——热环内 {value,bits,range} 全程 SSA
        // 寄存器，无 40B-byval 结构的逐位回写栈（Top1 主攻点）。`renorm_skip` 也在此判定一次
        // （TOP4：OnceLock 每宏块一读，主环零 OnceLock load）。
        let renorm_skip = ent_renorm_skip();
        // RC3：分支形开关每宏块判定一次（OnceLock 一读 + 恒定可预测分支），主环零开关开销。
        let ent_branch = super::simd::ent_branch_enabled();
        let data = $d.data;
        let mut pos = $d.pos;
        let mut value = $d.value;
        let mut bits = $d.bits;
        let mut range_minus1 = $d.range_minus1;
        let mut above = *$above_nz;
        let mut left = *$left_nz;
        let mut masks = 0u64;
        let mut block = if $i16 { 24usize } else { 0usize };
        let (luma_type, luma_start, luma_dc) = if $i16 {
            (0usize, 1usize, 1i32)
        } else {
            (3usize, 0usize, $sp.y_dc)
        };
        #[cfg(test)]
        if empty_probe::on() {
            empty_probe::mb();
        }
        loop {
            let (ty, start, dc, ac) = if block < 16 {
                (luma_type, luma_start, luma_dc, $sp.y_ac)
            } else if block < 24 {
                (2usize, 0usize, $sp.uv_dc, $sp.uv_ac)
            } else {
                (1usize, 0usize, $sp.y2_dc, $sp.y2_ac)
            };
            let above_bit = BLOCK_ABOVE_CONTEXT_BIT[block];
            let left_bit = BLOCK_LEFT_CONTEXT_BIT[block];
            let context = (((above >> above_bit) & 1) + ((left >> left_bit) & 1)) as usize;
            stage_clock::acct();
            let last = tokens(
                data,
                &mut pos,
                &mut value,
                &mut bits,
                &mut range_minus1,
                renorm_skip,
                ent_branch,
                &$probabilities[ty * 8 * 33..],
                context,
                start,
                &mut $mb[block],
                dc,
                ac,
            );
            let nonzero = if last > 0 { 1u32 } else { 0u32 };
            above = (above & !(1 << above_bit)) | (nonzero << above_bit);
            left = (left & !(1 << left_bit)) | (nonzero << left_bit);
            masks |= u64::from(nonzero) << block;
            if last > 1 {
                masks |= 1u64 << (block + 32);
            }
            stage_clock::acct();
            #[cfg(test)]
            if empty_probe::on() {
                let kind: u8 = if last == 0 {
                    0
                } else if last == 1 {
                    1
                } else {
                    2
                };
                empty_probe::bump(block, kind);
            }
            if block == 24 {
                block = 0;
            } else {
                block += 1;
                if block == 24 {
                    break;
                }
            }
        }
        // Store（每宏块一次，仿 C# `decoder.Store(v, b, r)`）。
        $d.data = data;
        $d.pos = pos;
        $d.value = value;
        $d.bits = bits;
        $d.range_minus1 = range_minus1;
        *$above_nz = above;
        *$left_nz = left;
        masks
    })
}

