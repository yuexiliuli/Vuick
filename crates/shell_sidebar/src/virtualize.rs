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
        // end 先 clamp 到 total，start 再 clamp 到 end：折叠后 offset 可能远超 total
        // （可见带整体冲过列表末尾），两重钳制保证 start<=end<=total——调用方
        // `&rows[start..end]` / `skip(start).take(end-start)` / `total-end` 均不越界不向下溢。
        let end = (last + buffer).min(total);
        let start = first.saturating_sub(buffer).min(end);
        (start, end)
    };
    let (top_h, bottom_h) = (start as f32 * row_h, (total - end) as f32 * row_h);
    (start, end, top_h, bottom_h)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 折叠后滚动偏移远超总行数：不得返回倒置区间（start>end）——消费方
    /// `&rows[start..end]` / `skip(start).take(end-start)` 依赖该不变式（审查 toc/files）。
    #[test]
    fn collapsed_offset_beyond_total_yields_empty_band() {
        // total=40（> 测试 min 10，走虚拟化分支），行高 24、视口 200、缓冲 8。
        // offset=10_000px → 理论 first≈416、last≈425，远超 total=40。
        let (start, end, top_h, bottom_h) = virtualized_band(40, 10_000.0, 200.0, 24.0, 10, 8);
        assert!(
            start <= end,
            "不得倒置：start={start} > end={end}"
        );
        assert!(end <= 40, "end 不得越界 total=40：end={end}");
        assert_eq!(bottom_h, 0.0, "折叠后底部 spacer 应 0（total-end=0 不向下溢）");
        // start<=end 时切片安全（调用点 `visible[start..end]`）。
        let rows = (0..40).collect::<Vec<_>>();
        let _ = &rows[start..end];
    }

    /// 正常滚动带：start/end 处于视口+缓冲内、spacer 高几何正确。
    #[test]
    fn normal_band_has_expected_start_end_and_spacers() {
        // total=100（>min 10）、offset=240（第 10 行起）、行高 24、视口 240（10 行）、缓冲 2。
        let (start, end, top_h, bottom_h) =
            virtualized_band(100, 240.0, 240.0, 24.0, 10, 2);
        assert_eq!(start, 8, "start 应 = first(10)-buffer(2)");
        assert_eq!(end, 10 + 10 + 1 + 2, "end 应 = last(21)+buffer(2) 即 23");
        assert_eq!(top_h, 8.0 * 24.0, "顶部 spacer = start×行高");
        assert_eq!(bottom_h, (100 - end) as f32 * 24.0, "底部 spacer = (total-end)×行高");
        assert!(top_h >= 0.0 && bottom_h >= 0.0);
    }

    /// total ≤ min：全量物化 (0,total)，两 spacer 高 0。
    #[test]
    fn small_total_materializes_all() {
        let (start, end, top_h, bottom_h) =
            virtualized_band(5, 10_000.0, 200.0, 24.0, 10, 8);
        assert_eq!((start, end), (0, 5), "小列表应全量物化");
        assert_eq!((top_h, bottom_h), (0.0, 0.0));
    }

    /// total=0（空列表）：不 panic，空带 (0,0)。
    #[test]
    fn empty_total_returns_empty_band() {
        let (start, end, top_h, bottom_h) =
            virtualized_band(0, 0.0, 200.0, 24.0, 10, 8);
        assert_eq!((start, end), (0, 0));
        assert_eq!((top_h, bottom_h), (0.0, 0.0));
    }
}
