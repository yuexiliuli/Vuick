//! 全 crate 共享的纯函数式小助手（无 GPUI / 外部依赖）。
//!
//! 目前只承载「字节区间 ↔ UTF-8 字符边界」的防御性钳制基元：Markdown 预览
//! `markdown::view::row`、WYSIWYG `markdown::wysiwyg::shape::runs_to_text_runs`、
//! 源码编辑器 `editor::view::render::char_align_highlights` 三处此前各自内联的
//! 「向前进位吸收到字符边界」逻辑统一收敛到这里（M1.1-P5 三处合一）。

use std::ops::Range;

/// 把字节位置**向前进位**钳到最近的 **UTF-8 字符边界**：先钳入 `text` 长度，再从 `p`
/// 起向后扫描到 `is_char_boundary` 为真的位置（`p` 已在边界上则原样返回）。
///
/// 这是「run/区间不能切在多字节字符里」的**共享基元**：CJK/emoji/组合字符下，
/// 编辑→重析后按字节搬运的区间会把边界切进字符内部，GPUI `Text`/`StyledText`
/// 在非字符边界处切 run 会 panic（`text.rs` 的 `is_char_boundary` assert / shaping 崩）。
/// 预览 `row::char_align_range`、WYSIWYG `shape::runs_to_text_runs`、源码编辑器
/// `render::char_align_highlights` 三处统一收敛到它。
pub(crate) fn char_align_byte(text: &str, mut p: usize) -> usize {
    let len = text.len();
    p = p.min(len);
    while p < len && !text.is_char_boundary(p) {
        p += 1;
    }
    p
}

/// 把字节区间两端分别吸收到最近的字符边界（向前进位、钳入 `text` 长度）。
/// 空/越界/反向安全；区间可能缩小为 0 长（两端同边界）——由调用方自行丢弃空段。
pub(crate) fn char_align_range(text: &str, range: Range<usize>) -> Range<usize> {
    char_align_byte(text, range.start)..char_align_byte(text, range.end)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// M1.1-P5 上游坏区间诊断：任何切进多字节字符（CJK / emoji / ZWJ 序列 / 组合字符 /
    /// 越界）内部的字节偏移，喂给统一后的共享基元都必须**不 panic** 且被正确钳制到
    /// 最近的字符边界。这是预览 `row` / WYSIWYG `shape` / 源码编辑器 `render` 三处共用
    /// 的同一路径，链路端到端的字形测试分别在 `row::tests::char_align_range_absorbs_cjk_boundaries`、
    /// `element::tests::runs_to_text_runs_clamps_mid_char_ranges_to_char_boundaries`、
    /// `editor::tests::char_align_highlights_survives_stale_ranges`。
    #[test]
    fn char_align_byte_absorbs_mid_char_offsets() {
        // "测🍎试❗"：测=3B，🍎=4B，试=3B，❗=3B → 13 字节。
        let text = "测🍎试❗";
        assert_eq!(text.len(), 13);
        for (p, expected) in [
            (1, 3),   // 切进『测』(0..3) 内部 → 前向吸收到 3
            (2, 3),   // 同上
            (4, 7),   // 切进 🍎(3..7) 内部 → 吸收到 7
            (6, 7),   // 同上
            (8, 10),  // 切进『试』(7..10) 内部 → 吸收到 10
            (11, 13), // 切进 ❗(10..13) 内部 → 吸收到 13
        ] {
            assert_eq!(char_align_byte(text, p), expected, "p={p}");
        }
        // 已对齐 / 恰好 len / 越界 → 原样或钳到 len，一律不 panic。
        assert_eq!(char_align_byte(text, 3), 3);
        assert_eq!(char_align_byte(text, 13), 13);
        assert_eq!(char_align_byte(text, 999), 13);
    }

    #[test]
    fn char_align_byte_survives_emoji_zwj_and_combining() {
        // ZWJ 家庭分组 emoji：任何非边界内部偏移都被吸收到真字符边界。
        let zwj = "👨\u{200d}👩\u{200d}👧";
        for off in 1..zwj.len() {
            let aligned = char_align_byte(zwj, off);
            assert!(zwj.is_char_boundary(aligned), "ZWJ off={off} → {aligned}");
            assert!(aligned >= off, "只能向前进位");
        }
        // 组合字符：cafè = c/a/f/e + U+0301 组合重音（字节 4..6），end=5 切进组合内 → 吸收到行尾。
        let comb = "cafe\u{301}";
        let aligned = char_align_range(comb, 3..(4 + 1));
        assert_eq!(aligned, 3..comb.len(), "end 应吸收到组合字符序列末尾: {aligned:?}");
        assert!(comb.is_char_boundary(aligned.start) && comb.is_char_boundary(aligned.end));
    }

    #[test]
    fn char_align_range_absorbs_mixed_and_out_of_bounds() {
        let text = "你说得对，好。行不行。";
        // 终点 19 切在「。」(18..21) 中间 → 前向吸收到 21；起点 18 已在边界上。
        assert_eq!(char_align_range(text, 0..19), 0..21);
        // 已对齐的不变。
        assert_eq!(char_align_range(text, 3..6), 3..6);
        // end 越界 → 钳入行尾。
        assert_eq!(char_align_range(text, 18..999), 18..text.len());
        // 整体越界 → 不 panic、坍缩为同边界（调用方丢弃空段）。
        assert_eq!(char_align_range(text, 500..999), text.len()..text.len());
    }
}
