#![forbid(unsafe_code)]
// 环路滤波标量/SIMD 内核参数多、逐寄存器传参，具名构参可读性反而变差 → 按既定内核风格放行。
#![allow(clippy::too_many_arguments)]
// 下游 re-export 面按「零符号丢失」保留（`simd/mod.rs` 的 `use self::loop_filter::*` 仍消费），
// 仅供 loopy/对照/测试按名引用；生产调用已在 raw 内部直连，非 test 构建无需按名引用 → 抑制
// unused_imports（内核本身带休眠标注，另有 `filter_edge` 的 allow(dead_code) 兜底）。
#![allow(unused_imports)]
// ---------------------------------------------------------------------------
// D2 环路滤波安全薄委托（unsafe 叶子隔离）：全部 `#[target_feature]` SSE2 SIMD 内核与
// 分派门面已移入同层 `raw::loop_filter`（含原 `simd/lf_edge.rs` 的宏块熔合内核，见
// `simd/lf_edge.rs`）。本文件顶部属性由编译期保证全文件零裸操作：安全的 `filter_edge` 退化为
// 一行薄委托；unsafe 内核（转置梯 / `lf_edge_*` / `lf_do_filter*` 等核心）经 `pub(crate) use`
// re-export 原样透传（同 `predict_subblock_forced` 姿势），签名 / 可见性 / 行为逐字节不变。
// 休眠标注（FilterKind 集群 / filter_edge_sse2 等）随内核进 raw，属性原样带过去。
// ---------------------------------------------------------------------------

/// 单边滤波分派：`length` 16（luma）/ 8（chroma）且 flt-SIMD 开 → SSE2 向量核；否则 vp8.rs 标量。
/// **默认未接入生产**（D2 实测持平/微负，vp8.rs 走标量；本入口仅测试/探针/复测用），
/// 非 test 构建允许 dead_code。薄委托 `raw::loop_filter::filter_edge`。
#[cfg_attr(not(test), allow(dead_code))]
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
    super::raw::loop_filter::filter_edge(plane, stride, x, y, length, vertical, macroblock_edge, simple, interior, edge, hev)
}

// ---------------------------------------------------------------------------
// 内核 re-export 透传（unsafe fn 原样透传；raw::loop_filter 为唯一 unsafe 免灾区）。
// 16×8 / 8×8 转置梯与 D2v2 `lf_edge_*` 内核（x86_64 SSE2 专用，保持原 cfg 门控）。
// ---------------------------------------------------------------------------
#[cfg(target_arch = "x86_64")]
pub(crate) use super::raw::loop_filter::lf_filter_edge_sse2;
#[cfg(target_arch = "x86_64")]
pub(crate) use super::raw::loop_filter::load16x8;
#[cfg(target_arch = "x86_64")]
pub(crate) use super::raw::loop_filter::store16x8;
#[cfg(target_arch = "x86_64")]
pub(crate) use super::raw::loop_filter::transpose8x8;
#[cfg(target_arch = "x86_64")]
pub(crate) use super::raw::loop_filter::lf_needs_filter;
#[cfg(target_arch = "x86_64")]
pub(crate) use super::raw::loop_filter::lf_do_filter2;
#[cfg(target_arch = "x86_64")]
pub(crate) use super::raw::loop_filter::lf_do_filter4;
#[cfg(target_arch = "x86_64")]
pub(crate) use super::raw::loop_filter::lf_do_filter6;
#[cfg(target_arch = "x86_64")]
pub(crate) use super::raw::loop_filter::lf_mask_composite;
#[cfg(target_arch = "x86_64")]
pub(crate) use super::raw::loop_filter::lf_max_diff;
#[cfg(target_arch = "x86_64")]
pub(crate) use super::raw::loop_filter::lf_edge_v16;
#[cfg(target_arch = "x86_64")]
pub(crate) use super::raw::loop_filter::lf_edge_h;
