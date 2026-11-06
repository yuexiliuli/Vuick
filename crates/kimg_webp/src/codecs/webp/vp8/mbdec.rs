use super::*;

/// 空块路径计数探针（2026-11-06 It-4，K103 探针先行）：`KIMG_WEBP_EMPTY_PROBE` env 门控
/// （仅测试构建；`parse_residuals_impl` 内 `cfg(test)` 钩子），逐块记录「块位置 × 空/仅 DC/含 AC」。
/// 默认关：关时主环仅一处 Relaxed 原子读 + 分支（~1ns/块，测试构建可接受，release 不含 cfg 全删）。
/// 开时只加原子计数（Relaxed），不改任何字节输出——供 flat/低比特率图「每 MB 空块数 + 空块机械
/// 分段成本」实测（It-4 报告；探针可留仓作证据，对应 W2「flat 每 MB ~0.05 tok、绝大多数空块」）。
#[cfg(test)]
pub(crate) mod empty_probe {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};

    static ON: AtomicBool = AtomicBool::new(false);
    static MB: AtomicU64 = AtomicU64::new(0);
    static EMPTY: [AtomicU64; 25] = [const { AtomicU64::new(0) }; 25];
    static DC_ONLY: [AtomicU64; 25] = [const { AtomicU64::new(0) }; 25];
    static AC: [AtomicU64; 25] = [const { AtomicU64::new(0) }; 25];

    /// 一次（测试进程）从 env 读门控。默认关；`set` 优先。
    pub(crate) fn init() {
        let v = std::env::var("KIMG_WEBP_EMPTY_PROBE").unwrap_or_default();
        ON.store(
            matches!(v.trim(), "1" | "true" | "on" | "y" | "yes"),
            Relaxed,
        );
    }

    pub(crate) fn set(v: bool) {
        ON.store(v, Relaxed);
    }

    pub(crate) fn on() -> bool {
        ON.load(Relaxed)
    }

    /// 每宏块一次（= 每次 `parse_residuals*` 调用）。
    pub(crate) fn mb() {
        MB.fetch_add(1, Relaxed);
    }

    /// `kind`: 0=空(last==0) / 1=仅 DC(last==1) / 2=含 AC(last>1)。`block` 0..24。
    pub(crate) fn bump(block: usize, kind: u8) {
        let d = match kind {
            0 => &EMPTY[block],
            1 => &DC_ONLY[block],
            _ => &AC[block],
        };
        d.fetch_add(1, Relaxed);
    }

    /// 返回 `(宏块数, 逐块位置空/仅DC/AC 三元计数)`。len=25，`[i]` 为块 i（Y2=24）。
    pub(crate) fn report() -> (u64, [u64; 25], [u64; 25], [u64; 25]) {
        let mut e = [0u64; 25];
        let mut d = [0u64; 25];
        let mut a = [0u64; 25];
        for i in 0..25usize {
            e[i] = EMPTY[i].load(Relaxed);
            d[i] = DC_ONLY[i].load(Relaxed);
            a[i] = AC[i].load(Relaxed);
        }
        (MB.load(Relaxed), e, d, a)
    }

    pub(crate) fn reset() {
        MB.store(0, Relaxed);
        for i in 0..25usize {
            EMPTY[i].store(0, Relaxed);
            DC_ONLY[i].store(0, Relaxed);
            AC[i].store(0, Relaxed);
        }
    }
}

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
/// 仅指令选择不同（`parse_residuals_impl!`）。
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
    if super::simd::ent_bmi_enabled() {
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
/// 否则改回 `probabilities[node >> 1]` 有界读。
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
    let base = probabilities.as_ptr();
    loop {
        debug_assert!((node >> 1) < probabilities.len());
        let bit = ent_read_bit(data, pos, value, bits, range_minus1, renorm_skip, unsafe { *base.add(node >> 1) });
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
        // 子节点索引裸指针读（并行去边界检查）：树是静态表，`node + bit` 恒 ≤ 表长-1
        // （逐树验证于 vp8_tables.rs；同概率裸读不变量，K32 贴既有机制）。
        let child = unsafe { *tree.as_ptr().add(node + bit as usize) };
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
                // 裸指针（同 read_tree 不变量）：`p` 恒长 9、可达 node≤16 ⇒ node>>1 ≤ 8 < 9；
                // `BLOCK_MODE_TREE` 静态表长 18，可达 node≤16 ⇒ node+bit ≤ 17 < 18 → 均去边界检查读。
                let pb = p.as_ptr();
                let tb = BLOCK_MODE_TREE.as_ptr();
                loop {
                    debug_assert!((node >> 1) < p.len());
                    let bit = ent_read_bit(data, &mut pos, &mut value, &mut bits, &mut range_minus1, renorm_skip, unsafe { *pb.add(node >> 1) });
                    debug_assert!((node + bit as usize) < BLOCK_MODE_TREE.len());
                    probe_skew!(skew_clock::BMODE, bit);
                    let child = unsafe { *tb.add(node + bit as usize) };
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
/// 变体内，两变体由同一宏体展开（算术单源不分裂），仅指令选择不同。
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
    if super::simd::ent_bmi_enabled() {
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

/// 构造宏块 (mbx, mby) 的 above 缓冲：`above_y[0]`=左上、`[1..17]`=上 16、`[17..21]`=右上 4；
/// chrona `above_u/v[0]`=左上、`[1..9]`=上 8。mbY==0 全 127；左列帧边缘 129。
#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare_above(
    above_y: &mut [u8; 21],
    above_u: &mut [u8; 9],
    above_v: &mut [u8; 9],
    y: &[u8],
    u: &[u8],
    v: &[u8],
    y_pitch: usize,
    uv_pitch: usize,
    mbx: usize,
    mby: usize,
    mbw: usize,
) {
    if mby == 0 {
        above_y.fill(127);
        above_u.fill(127);
        above_v.fill(127);
        return;
    }
    let y_row = mby * 16 - 1;
    let uv_row = mby * 8 - 1;
    let yl = y_row * y_pitch;
    let ul = uv_row * uv_pitch;
    above_y[0] = if mbx == 0 {
        129
    } else {
        y[yl + mbx * 16] // 列 mbx*16-1
    };
    above_u[0] = if mbx == 0 {
        129
    } else {
        u[ul + mbx * 8]
    };
    above_v[0] = if mbx == 0 {
        129
    } else {
        v[ul + mbx * 8]
    };
    for c in 0..16 {
        above_y[1 + c] = y[yl + 1 + mbx * 16 + c];
    }
    for c in 0..8 {
        above_u[1 + c] = u[ul + 1 + mbx * 8 + c];
        above_v[1 + c] = v[ul + 1 + mbx * 8 + c];
    }
    let right = mbx * 16 + 16;
    if mbx + 1 < mbw {
        // 非末列宏块：右上 4 像素取上一行真实重建值（右侧宏块上行的列 right..right+4；
        // 右缘不完整宏块情形下这些列可能落在 padded 区，但同为上一行已重建定稿的字节）。
        for i in 0..4 {
            above_y[17 + i] = y[yl + 1 + right + i];
        }
    } else {
        // 末列宏块：镜像本宏块上行第 16 列（列 mbx*16+15，padded 末位）——与参照
        // image-webp `create_border_luma` 的 `mbx == mbw-1` 分支逐字节一致。此前用
        // `(right + i).min(width - 1)` 夹到可见末列，右缘不完整宏块下与该参照偏差
        // （RFC 6386 右缘 B_PRED 子块 LD/VL 引用右上像素时错一源，宽 W%16 图右缘偏出）。
        let mirror = y[yl + 1 + mbx * 16 + 15];
        for a in above_y[17..21].iter_mut() {
            *a = mirror;
        }
    }
}

/// 分阶段计时（性能探针用）：
/// - 普通构建（无 feature、非 test）：全部为 `f()` 直通（零开销、不手写重复）。
/// - `cfg(test)`：累计各阶段耗时到进程级原子（只读不改写，字节不受影响）。探针
///   `lossy_stage_split_big` 在 release 测试二进制里跑真墙钟，输出熵/预测/变换/滤波占比。
/// - `cfg(feature="stage_profile")`：release 分账用，各阶段**恒计时**到进程级原子（跨滤波
///   线程安全），供 imgbench `vp8_stage_probe`（K59 统一基座 / K53 测量先行）。
pub(crate) mod stage_clock {
    #[cfg(test)]
    use std::sync::atomic::AtomicBool;
    #[cfg(any(test, feature = "stage_profile"))]
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
    #[cfg(any(test, feature = "stage_profile"))]
    use std::time::Instant;

    #[cfg(test)]
    static ON: AtomicBool = AtomicBool::new(false);
    #[cfg(test)]
    static ENTROPY: AtomicU64 = AtomicU64::new(0);
    #[cfg(test)]
    static PREDICT: AtomicU64 = AtomicU64::new(0);
    #[cfg(test)]
    static IDCT: AtomicU64 = AtomicU64::new(0);
    #[cfg(test)]
    static FILTER: AtomicU64 = AtomicU64::new(0);

    // release stage_profile 分账累计（进程级原子：滤波线程/主线程分属两线程，均 fetch_add）。
    #[cfg(feature = "stage_profile")]
    static P_ENTROPY: AtomicU64 = AtomicU64::new(0);
    #[cfg(feature = "stage_profile")]
    static P_PREDICT: AtomicU64 = AtomicU64::new(0);
    #[cfg(feature = "stage_profile")]
    static P_IDCT: AtomicU64 = AtomicU64::new(0);
    #[cfg(feature = "stage_profile")]
    static P_FILTER: AtomicU64 = AtomicU64::new(0);
    #[cfg(feature = "stage_profile")]
    static P_YUV: AtomicU64 = AtomicU64::new(0);
    // 主线程 scope 前后串行段（帧头解析+熵 init / 末段紧密平面拷贝）分账累计（K53 测量先行）。
    #[cfg(feature = "stage_profile")]
    static P_HEADER: AtomicU64 = AtomicU64::new(0);
    #[cfg(feature = "stage_profile")]
    static P_COPY: AtomicU64 = AtomicU64::new(0);
    // 管道 scope 墙钟（主线程，join 阻塞含 worker 全跑；K53 定位 pipeline slack）。
    #[cfg(feature = "stage_profile")]
    static P_SCOPE: AtomicU64 = AtomicU64::new(0);
    // 管道前 setup（分配+平面 memset+装饰器/熵 init）墙钟（主线程；K53 分账）。
    #[cfg(feature = "stage_profile")]
    static P_SETUP: AtomicU64 = AtomicU64::new(0);
    // 管道前 setup 与 scope 之间的剩余（token 解码器/above/row_filters 分配）墙钟（主线程）。
    #[cfg(feature = "stage_profile")]
    static P_MID: AtomicU64 = AtomicU64::new(0);
    // scope join 后、返回前的尾段（Vp8Yuv 构建 + planes 释放）墙钟（主线程）。
    #[cfg(feature = "stage_profile")]
    static P_END: AtomicU64 = AtomicU64::new(0);
    // 串行分支整段墙钟（EmitScratch 构建 + 逐宏块行「熵→重建→滤波→直出」循环）：B 线拆账
    // 「未分账其余」的串行 margin = serial_loop − Σ5 阶段（K101 统一基座）。
    #[cfg(feature = "stage_profile")]
    static P_SERIAL_LOOP: AtomicU64 = AtomicU64::new(0);

    // ---- 细分探针（2026-11-01，Task A/B：IDCT 内 i16 vs i4、熵内 bool/记账/去量化）----
    // IDCT 内部子段墙钟（t2 重建线程，跨线程原子累计，同 P_IDCT 姿势）。
    #[cfg(feature = "stage_profile")]
    static P_IDCT_I16: AtomicU64 = AtomicU64::new(0);
    #[cfg(feature = "stage_profile")]
    static P_IDCT_I4: AtomicU64 = AtomicU64::new(0);
    #[cfg(feature = "stage_profile")]
    static P_IDCT_WHT: AtomicU64 = AtomicU64::new(0);
    #[cfg(feature = "stage_profile")]
    static P_IDCT_UV: AtomicU64 = AtomicU64::new(0);
    // i4 段内子段（2026-11-02，i4 帧内预测向量化决策）：B_PRED 子块预测 vs IDCT-add 各占多少。
    #[cfg(feature = "stage_profile")]
    static P_I4_PREDICT: AtomicU64 = AtomicU64::new(0);
    #[cfg(feature = "stage_profile")]
    static P_I4_IDCT_ADD: AtomicU64 = AtomicU64::new(0);
    #[cfg(feature = "stage_profile")]
    static P_I4_PREDICT_BLOCKS: AtomicU64 = AtomicU64::new(0);
    #[cfg(feature = "stage_profile")]
    static P_I4_IDCT_ADD_BLOCKS: AtomicU64 = AtomicU64::new(0);
    // i4 段内 shell 计数（2026-11-02，K83 修正：撤 per-block Instant、改「外层粗钟 + 原子计数折算」）：
    // subblock_above 抽取次数（恒=predict 块数）/ `mb[i].fill(0)` 清零次数（恒=add 块数）/
    // has-ac `inverse_dct_add_one` 路径块数（拆 add 的 AC 与 DC-only 分账）。
    #[cfg(feature = "stage_profile")]
    static P_I4_ABOVE: AtomicU64 = AtomicU64::new(0);
    #[cfg(feature = "stage_profile")]
    static P_I4_FILL0: AtomicU64 = AtomicU64::new(0);
    #[cfg(feature = "stage_profile")]
    static P_I4_ADD_AC: AtomicU64 = AtomicU64::new(0);
    // 每文件宏块模式计数（i4/i16/WHT 执行次数）；供「i4 宏块占比 / WHT 次数」汇总。
    #[cfg(feature = "stage_profile")]
    static P_MB_I16: AtomicU64 = AtomicU64::new(0);
    #[cfg(feature = "stage_profile")]
    static P_MB_I4: AtomicU64 = AtomicU64::new(0);
    #[cfg(feature = "stage_profile")]
    static P_MB_WHT: AtomicU64 = AtomicU64::new(0);
    // 熵内部子段：模式段 / 残差段墙钟。
    #[cfg(feature = "stage_profile")]
    static P_ENT_MODES: AtomicU64 = AtomicU64::new(0);
    #[cfg(feature = "stage_profile")]
    static P_ENT_TOKENS: AtomicU64 = AtomicU64::new(0);
    // 熵内部三类操作计数（换算时间占比用，K53 口径）：
    // P_BITS = read_bit_val/read_signed_val 抛币总次数（header 段读也计入，靠段内 delta 归位）；
    // P_MODES_BITS / P_TOKENS_BITS = 模式段 / 残差段各自的 bool 读次数（段闭包内 delta 累加）；
    // P_ACCT = token 记账/游标/索引次数；P_DQ = 去量化+zigzag 写回次数。
    #[cfg(feature = "stage_profile")]
    static P_BITS: AtomicU64 = AtomicU64::new(0);
    #[cfg(feature = "stage_profile")]
    static P_MODES_BITS: AtomicU64 = AtomicU64::new(0);
    #[cfg(feature = "stage_profile")]
    static P_TOKENS_BITS: AtomicU64 = AtomicU64::new(0);
    #[cfg(feature = "stage_profile")]
    static P_ACCT: AtomicU64 = AtomicU64::new(0);
    #[cfg(feature = "stage_profile")]
    static P_DQ: AtomicU64 = AtomicU64::new(0);

    #[cfg(test)]
    pub fn set(on: bool) {
        ON.store(on, Relaxed);
    }
    #[cfg(test)]
    pub fn reset() {
        ENTROPY.store(0, Relaxed);
        PREDICT.store(0, Relaxed);
        IDCT.store(0, Relaxed);
        FILTER.store(0, Relaxed);
    }
    /// 返回四阶段累计毫秒：(entropy, predict, idct, filter)。
    #[cfg(test)]
    pub fn totals() -> (f64, f64, f64, f64) {
        (
            ENTROPY.load(Relaxed) as f64 / 1e6,
            PREDICT.load(Relaxed) as f64 / 1e6,
            IDCT.load(Relaxed) as f64 / 1e6,
            FILTER.load(Relaxed) as f64 / 1e6,
        )
    }

    /// 清零一次解码的分账累计（`stage_profile` 入口 `stage_decode` 调用）。
    #[cfg(feature = "stage_profile")]
    pub fn reset_profile() {
        P_ENTROPY.store(0, Relaxed);
        P_PREDICT.store(0, Relaxed);
        P_IDCT.store(0, Relaxed);
        P_FILTER.store(0, Relaxed);
        P_YUV.store(0, Relaxed);
        P_HEADER.store(0, Relaxed);
        P_COPY.store(0, Relaxed);
        P_SCOPE.store(0, Relaxed);
        P_SETUP.store(0, Relaxed);
        P_MID.store(0, Relaxed);
        P_END.store(0, Relaxed);
        P_SERIAL_LOOP.store(0, Relaxed);
        P_IDCT_I16.store(0, Relaxed);
        P_IDCT_I4.store(0, Relaxed);
        P_IDCT_WHT.store(0, Relaxed);
        P_IDCT_UV.store(0, Relaxed);
        P_I4_PREDICT.store(0, Relaxed);
        P_I4_IDCT_ADD.store(0, Relaxed);
        P_I4_PREDICT_BLOCKS.store(0, Relaxed);
        P_I4_IDCT_ADD_BLOCKS.store(0, Relaxed);
        P_I4_ABOVE.store(0, Relaxed);
        P_I4_FILL0.store(0, Relaxed);
        P_I4_ADD_AC.store(0, Relaxed);
        P_MB_I16.store(0, Relaxed);
        P_MB_I4.store(0, Relaxed);
        P_MB_WHT.store(0, Relaxed);
        P_ENT_MODES.store(0, Relaxed);
        P_ENT_TOKENS.store(0, Relaxed);
        P_BITS.store(0, Relaxed);
        P_MODES_BITS.store(0, Relaxed);
        P_TOKENS_BITS.store(0, Relaxed);
        P_ACCT.store(0, Relaxed);
        P_DQ.store(0, Relaxed);
    }
    /// 读取一次解码各阶段累计 ns：(entropy, predict, idct, filter, yuv, header, copy, scope, setup, mid, end, serial_loop)。
    #[cfg(feature = "stage_profile")]
    pub fn profile_totals() -> (u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) {
        (
            P_ENTROPY.load(Relaxed),
            P_PREDICT.load(Relaxed),
            P_IDCT.load(Relaxed),
            P_FILTER.load(Relaxed),
            P_YUV.load(Relaxed),
            P_HEADER.load(Relaxed),
            P_COPY.load(Relaxed),
            P_SCOPE.load(Relaxed),
            P_SETUP.load(Relaxed),
            P_MID.load(Relaxed),
            P_END.load(Relaxed),
            P_SERIAL_LOOP.load(Relaxed),
        )
    }

    /// 细分探针读取（IDCT 内部子段 / 宏块模式计数 / 熵内部子段与操作计数）。feature 外不存在。
    #[cfg(feature = "stage_profile")]
    #[derive(Default, Clone, Copy)]
    pub(crate) struct ProfileSub {
        pub idct_i16: u64,
        pub idct_i4: u64,
        pub idct_wht: u64,
        pub idct_uv: u64,
        pub i4_predict_ns: u64,
        pub i4_idct_add_ns: u64,
        pub i4_predict_blocks: u64,
        pub i4_idct_add_blocks: u64,
        /// subblock_above 抽取次数（恒 = predict 块数）笼罩子块循环壳构成分账。
        pub i4_above_blocks: u64,
        /// `mb[i].fill(0)` 清零次数（恒 = add 块数）。
        pub i4_fill0_blocks: u64,
        /// has-ac `inverse_dct_add_one` 块数（add = AC 路径 + 标量 DC-only 捷径）。
        pub i4_add_ac_blocks: u64,
        pub mb_i16: u64,
        pub mb_i4: u64,
        pub mb_wht: u64,
        pub ent_modes: u64,
        pub ent_tokens: u64,
        /// 模式段 bool 读次数（P_BITS delta）。
        pub modes_bits: u64,
        /// 残差段 bool 读次数（P_BITS delta）。
        pub tokens_bits: u64,
        /// 去量化+zigzag 写回次数。
        pub dq: u64,
        /// token 记账/游标/索引次数。
        pub acct: u64,
    }
    /// 读取细分探针累计。
    #[cfg(feature = "stage_profile")]
    pub fn profile_sub() -> ProfileSub {
        ProfileSub {
            idct_i16: P_IDCT_I16.load(Relaxed),
            idct_i4: P_IDCT_I4.load(Relaxed),
            idct_wht: P_IDCT_WHT.load(Relaxed),
            idct_uv: P_IDCT_UV.load(Relaxed),
            i4_predict_ns: P_I4_PREDICT.load(Relaxed),
            i4_idct_add_ns: P_I4_IDCT_ADD.load(Relaxed),
            i4_predict_blocks: P_I4_PREDICT_BLOCKS.load(Relaxed),
            i4_idct_add_blocks: P_I4_IDCT_ADD_BLOCKS.load(Relaxed),
            i4_above_blocks: P_I4_ABOVE.load(Relaxed),
            i4_fill0_blocks: P_I4_FILL0.load(Relaxed),
            i4_add_ac_blocks: P_I4_ADD_AC.load(Relaxed),
            mb_i16: P_MB_I16.load(Relaxed),
            mb_i4: P_MB_I4.load(Relaxed),
            mb_wht: P_MB_WHT.load(Relaxed),
            ent_modes: P_ENT_MODES.load(Relaxed),
            ent_tokens: P_ENT_TOKENS.load(Relaxed),
            modes_bits: P_MODES_BITS.load(Relaxed),
            tokens_bits: P_TOKENS_BITS.load(Relaxed),
            dq: P_DQ.load(Relaxed),
            acct: P_ACCT.load(Relaxed),
        }
    }

    /// 熵·模式段子计时：累加墙钟到 `P_ENT_MODES`，并把段内 `read_bit_val`/`read_signed_val`
    /// 调用数（`P_BITS` delta）累加进 `P_MODES_BITS`。feature 外纯透传（不改变行为）。
    #[inline(always)]
    pub fn entropy_modes<R>(f: impl FnOnce() -> R) -> R {
        #[cfg(feature = "stage_profile")]
        {
            let t = Instant::now();
            let b0 = P_BITS.load(Relaxed);
            let r = f();
            P_ENT_MODES.fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
            P_MODES_BITS.fetch_add(P_BITS.load(Relaxed) - b0, Relaxed);
            return r;
        }
        #[cfg(not(feature = "stage_profile"))]
        {
            f()
        }
    }
    /// 熵·残差段子计时：同熵·模式段，占 `P_ENT_TOKENS` / `P_TOKENS_BITS`。
    #[inline(always)]
    pub fn entropy_tokens<R>(f: impl FnOnce() -> R) -> R {
        #[cfg(feature = "stage_profile")]
        {
            let t = Instant::now();
            let b0 = P_BITS.load(Relaxed);
            let r = f();
            P_ENT_TOKENS.fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
            P_TOKENS_BITS.fetch_add(P_BITS.load(Relaxed) - b0, Relaxed);
            return r;
        }
        #[cfg(not(feature = "stage_profile"))]
        {
            f()
        }
    }
    /// IDCT·i16（16x16/8x8：2 块 SSE2 `inverse_dct_add_two`/`dc_add_two`）子计时。
    #[inline(always)]
    pub fn idct_i16<R>(f: impl FnOnce() -> R) -> R {
        #[cfg(feature = "stage_profile")]
        {
            let t = Instant::now();
            let r = f();
            P_IDCT_I16.fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
            return r;
        }
        #[cfg(not(feature = "stage_profile"))]
        {
            f()
        }
    }
    /// IDCT·i4x4 标量逐块（含 `predict_subblock` 子块预测 + `inverse_dct_add`）子计时。
    #[inline(always)]
    pub fn idct_i4<R>(f: impl FnOnce() -> R) -> R {
        #[cfg(feature = "stage_profile")]
        {
            let t = Instant::now();
            let r = f();
            P_IDCT_I4.fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
            return r;
        }
        #[cfg(not(feature = "stage_profile"))]
        {
            f()
        }
    }
    /// IDCT·WHT（`inverse_walsh_to_blocks`）子计时。
    #[inline(always)]
    pub fn idct_wht<R>(f: impl FnOnce() -> R) -> R {
        #[cfg(feature = "stage_profile")]
        {
            let t = Instant::now();
            let r = f();
            P_IDCT_WHT.fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
            return r;
        }
        #[cfg(not(feature = "stage_profile"))]
        {
            f()
        }
    }
    /// IDCT·UV 成对（i16 与 i4 共用的色度 2 块路径；单独列以便 IDCT 内分账闭包对齐）。
    #[inline(always)]
    pub fn idct_uv<R>(f: impl FnOnce() -> R) -> R {
        #[cfg(feature = "stage_profile")]
        {
            let t = Instant::now();
            let r = f();
            P_IDCT_UV.fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
            return r;
        }
        #[cfg(not(feature = "stage_profile"))]
        {
            f()
        }
    }
    /// i4 段内·B_PRED 子块预测（per-block `predict_subblock` + `subblock_above`）。
    /// K83 修正（2026-11-02）：不再逐块 `Instant::now()`（插桩通胀），只累加原子计数
    /// （predict 块 + above 抽取各一次，恒 16×mark）；时间由外层 `idct_i4` 粗钟与
    /// `probe_i4_op_costs` 折算。feature 外纯透传（不改变行为）。
    #[inline(always)]
    pub fn i4_predict<R>(f: impl FnOnce() -> R) -> R {
        #[cfg(feature = "stage_profile")]
        {
            P_I4_PREDICT_BLOCKS.fetch_add(1, Relaxed);
            P_I4_ABOVE.fetch_add(1, Relaxed);
        }
        f()
    }
    /// i4 段内·IDCT-add（`inverse_dct_add_one` / 标量 `inverse_dct_add` + `fill(0)`）。
    /// K83 修正（2026-11-02）：同 `i4_predict`，只累加原子计数（add 块 + fill(0) 各一次）。
    #[inline(always)]
    pub fn i4_idct_add<R>(f: impl FnOnce() -> R) -> R {
        #[cfg(feature = "stage_profile")]
        {
            P_I4_IDCT_ADD_BLOCKS.fetch_add(1, Relaxed);
            P_I4_FILL0.fetch_add(1, Relaxed);
        }
        f()
    }
    /// i4 段内·has-ac `inverse_dct_add_one` 路径计数（拆 add 的 AC 与 DC-only 分账）。
    /// feature 外空操作（不改变行为）。
    #[inline(always)]
    pub fn i4_add_ac() {
        #[cfg(feature = "stage_profile")]
        {
            P_I4_ADD_AC.fetch_add(1, Relaxed);
        }
    }
    /// 宏块模式计数：每宏块恰好一次（i16⇔y_mode!=4 之外即 i4）。feature 外空操作。
    #[inline(always)]
    pub fn mb_mode(is_i16: bool) {
        #[cfg(feature = "stage_profile")]
        {
            if is_i16 {
                P_MB_I16.fetch_add(1, Relaxed);
            } else {
                P_MB_I4.fetch_add(1, Relaxed);
            }
        }
        #[cfg(not(feature = "stage_profile"))]
        {}
    }
    /// WHT 执行次数（`mb[24]` WHT 实际运行的宏块计数）。
    #[inline(always)]
    pub fn mb_wht() {
        #[cfg(feature = "stage_profile")]
        {
            P_MB_WHT.fetch_add(1, Relaxed);
        }
        #[cfg(not(feature = "stage_profile"))]
        {}
    }
    /// bool 位读取计数（`read_bit_val` / `read_signed_val` 抛币各一次）。
    #[inline(always)]
    pub fn bit() {
        #[cfg(feature = "stage_profile")]
        {
            P_BITS.fetch_add(1, Relaxed);
        }
        #[cfg(not(feature = "stage_profile"))]
        {}
    }
    /// token 记账/游标/索引计数（position/p_idx/last/块序 bookkeeping）。
    #[inline(always)]
    pub fn acct() {
        #[cfg(feature = "stage_profile")]
        {
            P_ACCT.fetch_add(1, Relaxed);
        }
        #[cfg(not(feature = "stage_profile"))]
        {}
    }
    /// 去量化+zigzag 写回计数（每非零系数一次）。
    #[inline(always)]
    pub fn dq() {
        #[cfg(feature = "stage_profile")]
        {
            P_DQ.fetch_add(1, Relaxed);
        }
        #[cfg(not(feature = "stage_profile"))]
        {}
    }

    #[inline(always)]
    pub fn entropy<R>(f: impl FnOnce() -> R) -> R {
        #[cfg(feature = "stage_profile")]
        {
            let t = Instant::now();
            let r = f();
            P_ENTROPY.fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
            return r;
        }
        #[cfg(not(feature = "stage_profile"))]
        {
            #[cfg(test)]
            {
                if !ON.load(Relaxed) {
                    return f();
                }
                let t = Instant::now();
                let r = f();
                ENTROPY.fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
                return r;
            }
            #[cfg(not(test))]
            {
                f()
            }
        }
    }
    #[inline(always)]
    pub fn predict<R>(f: impl FnOnce() -> R) -> R {
        #[cfg(feature = "stage_profile")]
        {
            let t = Instant::now();
            let r = f();
            P_PREDICT.fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
            return r;
        }
        #[cfg(not(feature = "stage_profile"))]
        {
            #[cfg(test)]
            {
                if !ON.load(Relaxed) {
                    return f();
                }
                let t = Instant::now();
                let r = f();
                PREDICT.fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
                return r;
            }
            #[cfg(not(test))]
            {
                f()
            }
        }
    }
    #[inline(always)]
    pub fn idct<R>(f: impl FnOnce() -> R) -> R {
        #[cfg(feature = "stage_profile")]
        {
            let t = Instant::now();
            let r = f();
            P_IDCT.fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
            return r;
        }
        #[cfg(not(feature = "stage_profile"))]
        {
            #[cfg(test)]
            {
                if !ON.load(Relaxed) {
                    return f();
                }
                let t = Instant::now();
                let r = f();
                IDCT.fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
                return r;
            }
            #[cfg(not(test))]
            {
                f()
            }
        }
    }
    #[inline(always)]
    pub fn filter<R>(f: impl FnOnce() -> R) -> R {
        #[cfg(feature = "stage_profile")]
        {
            let t = Instant::now();
            let r = f();
            P_FILTER.fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
            return r;
        }
        #[cfg(not(feature = "stage_profile"))]
        {
            #[cfg(test)]
            {
                if !ON.load(Relaxed) {
                    return f();
                }
                let t = Instant::now();
                let r = f();
                FILTER.fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
                return r;
            }
            #[cfg(not(test))]
            {
                f()
            }
        }
    }
    /// YUV 定稿直出（fancy 上采样 + YUV→RGBA）阶段计时：普通/测试构建直通，`stage_profile`
    /// 下累计到 `P_YUV`。`emit_rows_into_rgba`（串行 + P-B 滤波线程两路径）调用。
    #[inline(always)]
    pub fn yuv<R>(f: impl FnOnce() -> R) -> R {
        #[cfg(feature = "stage_profile")]
        {
            let t = Instant::now();
            let r = f();
            P_YUV.fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
            return r;
        }
        #[cfg(not(feature = "stage_profile"))]
        {
            f()
        }
    }
    /// 帧头解析 + 熵 init（主线程，管道开启前）计时：`stage_profile` 下累计到 `P_HEADER`。
    #[inline(always)]
    pub fn header<R>(f: impl FnOnce() -> R) -> R {
        #[cfg(feature = "stage_profile")]
        {
            let t = Instant::now();
            let r = f();
            P_HEADER.fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
            return r;
        }
        #[cfg(not(feature = "stage_profile"))]
        {
            f()
        }
    }
    /// 末段紧密平面抽取（主线程，scope join 后）计时：`stage_profile` 下累计到 `P_COPY`。
    #[inline(always)]
    pub fn copy<R>(f: impl FnOnce() -> R) -> R {
        #[cfg(feature = "stage_profile")]
        {
            let t = Instant::now();
            let r = f();
            P_COPY.fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
            return r;
        }
        #[cfg(not(feature = "stage_profile"))]
        {
            f()
        }
    }
    /// 管道 scope 整段墙钟：`stage_profile` 下累计到 `P_SCOPE`（主线程 join 阻塞全景）。
    #[inline(always)]
    pub fn scope_wall<R>(f: impl FnOnce() -> R) -> R {
        #[cfg(feature = "stage_profile")]
        {
            let t = Instant::now();
            let r = f();
            P_SCOPE.fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
            return r;
        }
        #[cfg(not(feature = "stage_profile"))]
        {
            f()
        }
    }
    /// 管道前 setup（分配+平面 memset）墙钟：`stage_profile` 下累计到 `P_SETUP`。
    #[inline(always)]
    pub fn setup<R>(f: impl FnOnce() -> R) -> R {
        #[cfg(feature = "stage_profile")]
        {
            let t = Instant::now();
            let r = f();
            P_SETUP.fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
            return r;
        }
        #[cfg(not(feature = "stage_profile"))]
        {
            f()
        }
    }
    /// setup 与 scope 之间的剩余（token 解码器/above/row_filters）：累计到 `P_MID`。
    #[inline(always)]
    pub fn mid<R>(f: impl FnOnce() -> R) -> R {
        #[cfg(feature = "stage_profile")]
        {
            let t = Instant::now();
            let r = f();
            P_MID.fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
            return r;
        }
        #[cfg(not(feature = "stage_profile"))]
        {
            f()
        }
    }
    /// scope join 后、返回前尾段（Vp8Yuv 构建 + planes 释放）：累计到 `P_END`。
    #[inline(always)]
    pub fn end<R>(f: impl FnOnce() -> R) -> R {
        #[cfg(feature = "stage_profile")]
        {
            let t = Instant::now();
            let r = f();
            P_END.fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
            return r;
        }
        #[cfg(not(feature = "stage_profile"))]
        {
            f()
        }
    }
    /// 串行分支整段墙钟（EmitScratch 构建 + 逐宏块行循环）：累计到 `P_SERIAL_LOOP`。
    #[inline(always)]
    pub fn serial_loop<R>(f: impl FnOnce() -> R) -> R {
        #[cfg(feature = "stage_profile")]
        {
            let t = Instant::now();
            let r = f();
            P_SERIAL_LOOP.fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
            return r;
        }
        #[cfg(not(feature = "stage_profile"))]
        {
            f()
        }
    }
}

/// VP8 有损阶帧分账（`stage_profile` feature 下由 `stage_decode` 填充；供 imgbench
/// `vp8_stage_probe`，K59 统一基座）。本结构体无条件编译（供签名线程），计时仅 feature 下发生，
/// feature 关闭时零运行时影响。
#[cfg_attr(not(feature = "stage_profile"), allow(dead_code))]
#[derive(Default, Clone, Copy, Debug)]
pub struct Vp8StageProfile {
    pub width: u32,
    pub height: u32,
    /// 熵解码（宏块模式 + 残差 token 解析 + bool+去量化）。
    pub entropy_ns: u64,
    /// 帧内预测（prepare_above + 16x16/8x8 平面预测）。
    pub predict_ns: u64,
    /// 逆变换（WHT + 4x4 IDCT(成对/单块) + B_PRED 子块预测）。
    pub idct_ns: u64,
    /// 环路滤波（filter_macroblock_row）。
    pub filter_ns: u64,
    /// YUV 定稿直出（fancy 上采样 + YUV→RGBA）。
    pub yuv_ns: u64,
    /// 帧头解析 + 熵 init（主线程，管道开启前；K53 分账）。
    pub header_ns: u64,
    /// 末段紧密平面抽取（主线程，scope join 后；K53 分账）。
    pub copy_ns: u64,
    /// 管道 `std::thread::scope` 整段墙钟（主线程 join 阻塞，含 worker 全跑 + 调度等待）。
    pub scope_ns: u64,
    /// 管道前 setup（planes memset + rgba 分配；主线程）。
    pub setup_ns: u64,
    /// setup 与 scope 之间剩余（token 解码器/above/row_filters 分配）。
    pub mid_ns: u64,
    /// scope join 后、返回前尾段（Vp8Yuv 构建）。
    pub end_ns: u64,
    /// 串行分支整段墙钟（EmitScratch 构建 + 逐宏块行「熵→重建→滤波→直出」循环；feature 外恒 0）。
    pub serial_loop_ns: u64,
    /// 全链墙钟（含帧头解析与平面拷贝）。
    pub total_ns: u64,
    /// 是否走了 P-B 滤波线程管线（true 时 filter/yuv 在独立线程累计）。
    pub pipelined: bool,
    /// IDCT 内部子段墙钟（仅 `stage_profile` 探针填充；feature 外恒 0）。
    pub idct_i16_ns: u64,
    pub idct_i4_ns: u64,
    pub idct_wht_ns: u64,
    pub idct_uv_ns: u64,
    /// i4 段内子段墙钟：B_PRED 子块预测 vs IDCT-add（仅 `stage_profile` 探针填充）。
    pub i4_predict_ns: u64,
    pub i4_idct_add_ns: u64,
    /// i4 段内块计数：predict 子块次数（恒 16×mb_i4）/ idct-add 子块次数（非零块）。
    pub i4_predict_blocks: u64,
    pub i4_idct_add_blocks: u64,
    /// i4 段内 shell 计数（折算用）：above 抽取 / fill(0) / has-ac add 块数。
    pub i4_above_blocks: u64,
    pub i4_fill0_blocks: u64,
    pub i4_add_ac_blocks: u64,
    /// 宏块模式计数（每文件）：i16 宏块数 / i4(B_PRED) 宏块数 / WHT 执行次数。
    pub mb_i16: u64,
    pub mb_i4: u64,
    pub mb_wht: u64,
    /// 熵内部墙钟：模式段 / 残差段。
    pub ent_modes_ns: u64,
    pub ent_tokens_ns: u64,
    /// 熵内部操作计数：模式段 bool 读 / 残差段 bool 读 / 记账·游标·索引 / 去量化+zigzag 写回。
    pub modes_bits: u64,
    pub tokens_bits: u64,
    pub acct_count: u64,
    pub dq_count: u64,
}

