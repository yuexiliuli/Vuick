//! VP8L Huffman 符号热读的裸 unsafe 叶子子模块（`codecs/webp/vp8l.rs` 薄委托依赖本模块）：
//! `read_symbol` 裸指针表区解码 + 探针计数（`stage_profile` feature）。
//! 上层 `vp8l.rs` 以 `#![forbid(unsafe_code)]` + 薄委托显式依赖本模块。
//! 算法 / 常量 / 调用顺序与隔离前逐字节一致（字节恒等是硬约束，禁改算术）。

#![allow(unsafe_code)]

use super::super::super::vp8l::{ROOT_BITS, SUBTABLE_FLAG};

// 探针计数（仅 `stage_profile` feature；read_symbol 命中子表频率 → 量化「更大 root 主表」
// 收益上限）。`read_symbol` 与 `vp8l::stage_decode` 共用，故 pub(crate)。
#[cfg(feature = "stage_profile")]
thread_local! {
    pub(crate) static SYMBOL_READS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    pub(crate) static SUBTABLE_HITS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// 解码一个符号（入口 ≥15 位缓冲）。根条目 = `(symbol<<16)|length` 或
/// 二级表指针 `(start<<16)|SUBTABLE_FLAG|subBits`。
/// 热路径用裸指针：`table` 指向 `tables.data` 基址，`offset` 为通道偏移（`tables.data + offset`
/// 该区域长度在建表期已 `ensure`，≥ 1<<ROOT_BITS + 二级表），索引恒在区域内（K42 上限在建表
/// 校验），parity 全量覆盖标量/向量一致性。非 unsafe fn（承接原签名）：裸指针构造与
/// `.add`/解引用均在本模块 unsafe 内完成，调用方仅传基址与偏移。
#[inline(always)]
pub(crate) fn read_symbol(
    table: *const u32,
    offset: usize,
    root_mask: u32,
    buffer: &mut u64,
    bits: &mut u32,
) -> u32 {
    // SAFETY: 调用方保证 `table.add(offset)` 指向建表层（`tables.data` ensure 区），`&buffer`/
    // `&bits` 为活引用；root 掩码下标与二级表偏移（entry>>16 + 子表内位段）恒在建表层长度内
    // （K42 建表校验）。
    unsafe {
        let table = table.add(offset);
        #[cfg(feature = "stage_profile")]
        SYMBOL_READS.with(|c| c.set(c.get() + 1));
        let entry = *table.add((*buffer as u32 & root_mask) as usize);
        let entry = if entry & SUBTABLE_FLAG != 0 {
            #[cfg(feature = "stage_profile")]
            SUBTABLE_HITS.with(|c| c.set(c.get() + 1));
            let index =
                (entry >> 16) + ((*buffer as u32 >> ROOT_BITS) & ((1 << (entry & 0xff)) - 1));
            *table.add(index as usize)
        } else {
            entry
        };
        let length = entry & 0xff;
        *buffer >>= length;
        *bits -= length;
        entry >> 16
    }
}
