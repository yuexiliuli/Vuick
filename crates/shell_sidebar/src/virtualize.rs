//! 列表虚拟化「可视带切分」纯计算（Batch A / P6 收敛）。
//!
//! 收敛自 toc.rs / files.rs 两份逐行同构块——滚动量程 / 命中稳定所依赖的唯一几何源；
//! 改此一处两视图同步。spacer / 行物化仍由调用方各自构造。

/// 首帧容器尚未布局（高度 0）时的固定视口估计（px）：只多物化几行。
pub(crate) const VIRTUALIZE_FALLBACK_VIEWPORT_H: f32 = 700.0;

/// 虚拟化可视带切分：返回 `(start, end, top_spacer_h, bottom_spacer_h)`。纯计算。
///
/// `total` = 总行数、`offset` = 滚动偏移（px）、`viewport_h` = 可视区高（px）、
/// `row_h` = 固定行高、`min` = 超过才启用虚拟化的最小总行数、`buffer` = 可视带外
/// 上下各缓冲行数。总行数 ≤ `min` 时全量物化 `(0, total)`，两 spacer 高 0。
/// 折叠 / 条目大减导致滚动偏移越界时钳制空带（`start <= end`）。
pub(crate) fn virtualized_band(
    total: usize,
    offset: f32,
    viewport_h: f32,
    row_h: f32,
    min: usize,
    buffer: usize,
) -> (usize, usize, f32, f32) {
    let (start, end) = if total <= min {
        (0usize, total)
    } else {
        // 首帧容器尚未布局（高度 0）→ 退化为固定视口估计（只多物化几行）。
        let vh = if viewport_h > 0.0 {
            viewport_h
        } else {
            VIRTUALIZE_FALLBACK_VIEWPORT_H
        };
        let first = (offset / row_h) as usize;
        let last = ((offset + vh) / row_h) as usize + 1;
        let start = first.saturating_sub(buffer);
        let end = (last + buffer).min(total);
        // 折叠后条目大减时偏移可能越界：钳制空带。
        (start.min(end), end.max(start))
    };
    let (top_h, bottom_h) = (start as f32 * row_h, (total - end) as f32 * row_h);
    (start, end, top_h, bottom_h)
}
