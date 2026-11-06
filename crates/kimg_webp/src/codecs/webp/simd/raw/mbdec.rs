#![allow(unsafe_code)]
//! 裸 unsafe 叶子子模块 `raw::mbdec`：pipeline/mbdec 域的宏块熵解码整块迁入
//! （`parse_residuals*` / `read_macroblock_modes*` 分发器与 BMI 快道内核、`read_tree`/树路由宏，
//! 原 `vp8/mbdec.rs`）。上层 `vp8/mbdec.rs` 以 `#![forbid(unsafe_code)]` + 薄委托显式依赖
//! 本模块。算法 / 常量 / 调用顺序与隔离前逐字节一致（字节恒等是硬约束，禁改算术）。
//! `#[target_feature(enable="lzcnt,bmi1,bmi2")]` 内核 `parse_residuals_bmi` /
//! `read_macroblock_modes_bmi` 与基线变体由同一宏体展开（算术单源不分裂，仅指令选择不同）；
//! tree/prob 裸读（~4 处）下沉 `super::unchecked_index` 门面（内联后同指令，零新增边界检查）。

use super::super::super::vp8::{
    ent_read_bit, ent_renorm_skip, parse_residuals_impl, probe_skew, stage_clock, tokens,
    BLOCK_ABOVE_CONTEXT_BIT, BLOCK_LEFT_CONTEXT_BIT, BLOCK_MODE_TREE, KEYFRAME_BLOCK_MODE_PROBS,
    KEYFRAME_UV_MODE_PROBS, KEYFRAME_Y_MODE_PROBS, KEYFRAME_Y_MODE_TREE, SEGMENT_ID_TREE,
    UV_MODE_TREE, SegmentParameters, Vp8BoolDecoder,
};
use super::super::ent_bmi_enabled;
#[cfg(test)]
use super::super::super::vp8::empty_probe;
#[cfg(feature = "stage_profile")]
use super::super::super::vp8::skew_clock;

/// 基线变体：与 RC1 前完全相同的指令选择（bsr+xorl $31 / mov→cl），旧 CPU 与
/// `KIMG_WEBP_ENT_NO_BMI=1` 强制侧走本条。字节恒等由「bmi≡baseline」硬闸对拍。
#[inline(never)]
fn parse_residuals_base(
    d: &mut Vp8BoolDecoder<'_>,
    probabilities: &[u8],
    mb: &mut [[i16; 16]; 25],
    sp: &SegmentParameters,
    i16: bool,
    above_nz: &mut u32,
    left_nz: &mut u32,
) -> u64 {
    parse_residuals_impl!(d, probabilities, mb, sp, i16, above_nz, left_nz)
}

/// BMI1/2+lzcnt 快道变体：`leading_zeros`→`lzcnt`、变长移位→`shrx/shlx`（免 cl 依赖）；
/// 语义与基线逐字节一致（同宏体），仅指令选择不同。**Safety**：仅当 `ent_bmi_enabled()`
/// 为真（`bmi2` 可用 ⇒ `bmi1`/`lzcnt` 亦可用）时被调用——`lzcnt` 在无 BMI1 CPU 上会被解码成
/// `bsr`（语义反转→错字节），故绝不裸发；非 x86_64 本函数不存在（cfg 剔除）。
#[cfg(target_arch = "x86_64")]
#[inline(never)]
#[target_feature(enable = "lzcnt,bmi1,bmi2")]
unsafe fn parse_residuals_bmi(
    d: &mut Vp8BoolDecoder<'_>,
    probabilities: &[u8],
    mb: &mut [[i16; 16]; 25],
    sp: &SegmentParameters,
    i16: bool,
    above_nz: &mut u32,
    left_nz: &mut u32,
) -> u64 {
    parse_residuals_impl!(d, probabilities, mb, sp, i16, above_nz, left_nz)
}

/// 熵系数解码分发器：见上方「RC1 双路径」说明。快道与基线由同一宏体展开（算术单源不分裂），
/// 仅指令选择不同（`parse_residuals_impl!`）。自 `vp8/mbdec.rs` 迁入（薄委托透传）。
#[inline(never)]
pub(crate) fn parse_residuals(
    d: &mut Vp8BoolDecoder<'_>,
    probabilities: &[u8],
    mb: &mut [[i16; 16]; 25],
    sp: &SegmentParameters,
    i16: bool,
    above_nz: &mut u32,
    left_nz: &mut u32,
) -> u64 {
    #[cfg(target_arch = "x86_64")]
    if ent_bmi_enabled() {
        // Safety：ent_bmi_enabled() 保证 bmi2（⇒bmi1/lzcnt）可用；快道与基线逐字节一致
        // （「bmi≡baseline」硬闸覆盖），仅指令选择不同。
        return unsafe { parse_residuals_bmi(d, probabilities, mb, sp, i16, above_nz, left_nz) };
    }
    parse_residuals_base(d, probabilities, mb, sp, i16, above_nz, left_nz)
}

/// 树遍历：node + read_bit_val(prob[node>>1]) 沿树下行，叶子（≤0）返回 -child。熵态以调用方
/// 局部标量携带（同 `read_macroblock_modes` Load/Store；`data`/`pos` 仅在 refill 触碰）。
/// 概率读用裸指针（去边界检查）：全部三处调用（SEGMENT_ID / KEYFRAME_Y_MODE / UV_MODE）的可达
/// 节点 node∈{0,2,4,6}（逐树验证于 vp8_tables.rs）⇒ node>>1 ≤ 3 且 < 表长；
/// 三处 probs 表长 3/4/3，len-3 表实际最大下标 2。新增调用点须复查本不变量，
/// 否则改回 `probabilities[node >> 1]` 有界读。（裸读下沉 `super::unchecked_index` 门面，
/// 内联后与裸指针同指令，零新增边界检查；不变量同上。）
#[inline(always)]
fn read_tree(
    data: &[u8],
    pos: &mut usize,
    value: &mut u64,
    bits: &mut i32,
    range_minus1: &mut u32,
    renorm_skip: bool,
    tree: &[i8],
    probabilities: &[u8],
) -> u32 {
    let mut node = 0usize;
    loop {
        debug_assert!((node >> 1) < probabilities.len());
        let bit = ent_read_bit(data, pos, value, bits, range_minus1, renorm_skip, super::unchecked_index(probabilities, node >> 1));
        debug_assert!((node + bit as usize) < tree.len());
        #[cfg(feature = "stage_profile")]
        {
            // 树身份区分决策点：三棵调用树各用一处 site 计数（段/亮度模式/色度模式）。
            let site = if std::ptr::eq(tree, &SEGMENT_ID_TREE) {
                skew_clock::SEGMENT
            } else if std::ptr::eq(tree, &KEYFRAME_Y_MODE_TREE) {
                skew_clock::YMODE
            } else {
                skew_clock::UVMODE
            };
            skew_clock::bump(site, bit);
        }
        // 子节点索引读（并行去边界检查）：树是静态表，`node + bit` 恒 ≤ 表长-1
        // （逐树验证于 vp8_tables.rs；同概率裸读不变量，K32 贴既有机制）。
        let child = super::unchecked_index(tree, node + bit as usize);
        if child <= 0 {
            return -child as u32;
        }
        node = child as usize;
    }
}

/// RFC 6386 §11.5 宏块模式（segment / skip / ymode / 16 个 bmode（仅 B_PRED） / uvmode）。
/// 熵态三标量 {value,bits,range} 每宏块一次 Load/Store（对照 C# `ReadMacroBlockModes` 的
/// `header.Load(out v, out b, out r)` / `header.Store(v, b, r)`）；`data`/`pos` 仅在 refill 触碰。
/// 函数边界 `&mut` + 熵态局部标量拷贝（byval 会逐位回写内存，本形态保寄存器驻留）。
///
/// **RC1 双路径**（同 `parse_residuals`）：分发器按运行时一次判定进 `read_macroblock_modes_bmi`
/// （lzcnt/bmi 快道）或 `read_macroblock_modes_base`（基线 bsr）。两变体函数体由宏
/// `read_macroblock_modes_impl!` 展开自同一算术源——树路由（`read_tree`/内联 `ent_read_bit`）的
/// 指令选择不同不影响字节。`ent_read_signed`（符号位）恒等 sh=1-(range>>7) 免 lzcnt，保持不动。
macro_rules! read_macroblock_modes_impl {
    (
        $header:ident, $segment_probabilities:ident, $skip_probability:ident,
        $above_modes:ident, $left_modes:ident, $block_modes:ident
    ) => ({
        let renorm_skip = ent_renorm_skip();
        let data = $header.data;
        let mut pos = $header.pos;
        let mut value = $header.value;
        let mut bits = $header.bits;
        let mut range_minus1 = $header.range_minus1;
        let segment = match $segment_probabilities {
            Some(probs) => read_tree(
                data,
                &mut pos,
                &mut value,
                &mut bits,
                &mut range_minus1,
                renorm_skip,
                &SEGMENT_ID_TREE,
                probs,
            ),
            None => 0,
        };
        let skip = if $skip_probability < 0 {
            0
        } else {
            let b = ent_read_bit(
                data,
                &mut pos,
                &mut value,
                &mut bits,
                &mut range_minus1,
                renorm_skip,
                $skip_probability as u8,
            );
            probe_skew!(skew_clock::SKIP, b);
            b
        };
        let y_mode = read_tree(
            data,
            &mut pos,
            &mut value,
            &mut bits,
            &mut range_minus1,
            renorm_skip,
            &KEYFRAME_Y_MODE_TREE,
            &KEYFRAME_Y_MODE_PROBS,
        );
        if y_mode == 4 {
            for i in 0..16 {
                let above = if i < 4 { $above_modes[i] } else { $block_modes[i - 4] };
                let leftv = if (i & 3) == 0 {
                    $left_modes[i >> 2]
                } else {
                    $block_modes[i - 1]
                };
                let p = &KEYFRAME_BLOCK_MODE_PROBS[(usize::from(above) * 10 + usize::from(leftv)) * 9..]
                    [..9];
                let mut node = 0usize;
                // 概率/树读（同 read_tree 不变量，下沉 `super::unchecked_index`）：`p` 恒长 9、
                // 可达 node≤16 ⇒ node>>1 ≤ 8 < 9；`BLOCK_MODE_TREE` 静态表长 18，可达 node≤16 ⇒
                // node+bit ≤ 17 < 18 → 均去边界检查读（内联后同裸指针指令）。
                loop {
                    debug_assert!((node >> 1) < p.len());
                    let bit = ent_read_bit(data, &mut pos, &mut value, &mut bits, &mut range_minus1, renorm_skip, super::unchecked_index(p, node >> 1));
                    debug_assert!((node + bit as usize) < BLOCK_MODE_TREE.len());
                    probe_skew!(skew_clock::BMODE, bit);
                    let child = super::unchecked_index(&BLOCK_MODE_TREE, node + bit as usize);
                    if child <= 0 {
                        $block_modes[i] = -child as u8;
                        break;
                    }
                    node = child as usize;
                }
            }
            for i in 0..4 {
                $above_modes[i] = $block_modes[12 + i];
                $left_modes[i] = $block_modes[i * 4 + 3];
            }
        } else {
            let derived = match y_mode {
                0 => 0u8,
                1 => 2,
                2 => 3,
                _ => 1,
            };
            for m in $above_modes.iter_mut() {
                *m = derived;
            }
            for m in $left_modes.iter_mut() {
                *m = derived;
            }
        }
        let uv_mode = read_tree(
            data,
            &mut pos,
            &mut value,
            &mut bits,
            &mut range_minus1,
            renorm_skip,
            &UV_MODE_TREE,
            &KEYFRAME_UV_MODE_PROBS,
        );
        $header.data = data;
        $header.pos = pos;
        $header.value = value;
        $header.bits = bits;
        $header.range_minus1 = range_minus1;
        segment | (skip << 2) | (y_mode << 3) | (uv_mode << 6)
    })
}

/// 基线变体：与 RC1 前完全相同的指令选择（bsr+xorl $31 / mov→cl）；旧 CPU 与
/// `KIMG_WEBP_ENT_NO_BMI=1` 强制侧走本条。字节恒等由「bmi≡baseline」硬闸对拍。
#[inline(never)]
fn read_macroblock_modes_base(
    header: &mut Vp8BoolDecoder<'_>,
    segment_probabilities: Option<&[u8; 3]>,
    skip_probability: i32,
    above_modes: &mut [u8; 4],
    left_modes: &mut [u8; 4],
    block_modes: &mut [u8; 16],
) -> u32 {
    read_macroblock_modes_impl!(header, segment_probabilities, skip_probability, above_modes, left_modes, block_modes)
}

/// BMI1/2+lzcnt 快道变体：`leading_zeros`→`lzcnt`、变长移位→`shrx/shlx`（免 cl 依赖）；
/// 语义与基线逐字节一致（同宏体），仅指令选择不同。**Safety**：仅当 `ent_bmi_enabled()`
/// 为真（`bmi2` 可用 ⇒ `bmi1`/`lzcnt` 亦可用）时被调用——`lzcnt` 在无 BMI1 CPU 上会被解码成
/// `bsr`（语义反转→错字节），故绝不裸发；非 x86_64 本函数不存在（cfg 剔除）。
#[cfg(target_arch = "x86_64")]
#[inline(never)]
#[target_feature(enable = "lzcnt,bmi1,bmi2")]
unsafe fn read_macroblock_modes_bmi(
    header: &mut Vp8BoolDecoder<'_>,
    segment_probabilities: Option<&[u8; 3]>,
    skip_probability: i32,
    above_modes: &mut [u8; 4],
    left_modes: &mut [u8; 4],
    block_modes: &mut [u8; 16],
) -> u32 {
    read_macroblock_modes_impl!(header, segment_probabilities, skip_probability, above_modes, left_modes, block_modes)
}

/// 宏块模式解码分发器：见「RC1 双路径」说明。树路由/符号位/renorm 全部熵位读落在快道或基线
/// 变体内，两变体由同一宏体展开（算术单源不分裂），仅指令选择不同。自 `vp8/mbdec.rs` 迁入
/// （薄委托透传）。
#[inline(never)]
pub(crate) fn read_macroblock_modes(
    header: &mut Vp8BoolDecoder<'_>,
    segment_probabilities: Option<&[u8; 3]>,
    skip_probability: i32,
    above_modes: &mut [u8; 4],
    left_modes: &mut [u8; 4],
    block_modes: &mut [u8; 16],
) -> u32 {
    #[cfg(target_arch = "x86_64")]
    if ent_bmi_enabled() {
        // Safety：ent_bmi_enabled() 保证 bmi2（⇒bmi1/lzcnt）可用；快道与基线逐字节一致
        // （「bmi≡baseline」硬闸覆盖），仅指令选择不同。
        return unsafe {
            read_macroblock_modes_bmi(
                header,
                segment_probabilities,
                skip_probability,
                above_modes,
                left_modes,
                block_modes,
            )
        };
    }
    read_macroblock_modes_base(
        header,
        segment_probabilities,
        skip_probability,
        above_modes,
        left_modes,
        block_modes,
    )
}
