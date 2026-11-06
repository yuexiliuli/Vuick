#![forbid(unsafe_code)]
#![allow(unused_imports)]
// ---------------------------------------------------------------------------
// D2v3 宏块级熔合环路滤波安全薄委托（unsafe 叶子隔离）：全部 `#[target_feature]` SSE2
// 内核已随 `simd/loop_filter.rs` 一并移入同层 `raw::loop_filter`。本文件顶部属性由编译期
// 保证全文件零裸操作；生产入口 `lf_filter_macroblock` 为 unsafe fn，按
// `predict_subblock_forced` 姿势以 `pub(crate) use` re-export 原样透传，签名 / 可见性 /
// 行为逐字节不变。
// 生产调用已改经 `raw::lf_filter_macroblock_safe`（安全门面）直连；本 re-export 按「零符号
// 丢失」保留，仅供对照/测试按名引用（同 `simd/loop_filter.rs` 抑制姿态）。
// ---------------------------------------------------------------------------

/// 宏块级熔合环路滤波入口（生产；C# `Vp8Decoder.FilterMacroblock` 同款）。一次宏块完成
/// 全 12 个 16-lane 核调用，边次序与标量 `vp8::filter_macroblock_row` 完全一致，逐字节相同。
/// unsafe fn 原样透传 `raw::loop_filter::lf_filter_macroblock`（生产口见 `raw::lf_filter_macroblock_safe`）。
#[cfg(target_arch = "x86_64")]
pub(crate) use super::raw::loop_filter::lf_filter_macroblock;
