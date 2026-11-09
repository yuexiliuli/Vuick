//! WYSIWYG 写回增量：把「toggle ON 的源快照」与 live2 最终文本做**行级 diff**，产出
//! 少量互不重叠、按 base 字节偏移升序的 `(Range<usize>, String)` 替换（**须从后往前
//! 应用**，早偏移区间在晚偏移区间位移后仍有效）。
//!
//! 正确性 = 完整性：任意 diff 只要应用后把 `base` 精确变成 `target` 即合法；最小性是
//! 效率优化而非硬约束（宁可多换也不丢字）。因此对于超出有界 LCS 的大段编辑，直接退化为
//! 一段大替换——它仍走 CodeEditor/WyEditor 的**增量编辑管线**（O(log n) Rope 编辑 +
//! 窗口化高亮重解析），不是 `set_text` 的整篇重建/重排级联。

use std::ops::Range;

/// 有界 LCS 的 DP 单元上限（2M 单元 ≈ 16MB 临时表，毫秒级）。超出 → 整段替换兜底。
const LCS_CELL_LIMIT: usize = 2_000_000;

/// 行切分：`(行起始字节偏移, 行文本)`，每行保留尾部 `\n`（末行可不带）。
/// `""` → `[(0, "")]`（单空行，换行语义上与 `"a"` 的最后一行为同一形状）。
fn line_spans(s: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::with_capacity(8);
    let mut off = 0;
    for seg in s.split_inclusive('\n') {
        out.push((off, seg));
        off += seg.len();
    }
    if out.is_empty() {
        out.push((0, ""));
    }
    out
}

/// 整行替换 hunk（base 行索引表示）：`[a_start, a_end)` 的 base 行换成 `repl` 文本。
struct LineHunk {
    a_start: usize,
    a_end: usize,
    repl: String,
}

/// 对行序列 `bm`/`tm`（相对全局 base 起始行偏移 `off`）做公共行前后缀裁剪 + 有界
/// LCS 拆分，产出整行 hunk（升序）。过大时退化为一段整体替换。
///
/// **刻意不做「字节级前后缀」预裁剪**：字节后缀会贪婪吞掉「改行与未改行共享尾字符」
/// 的行（实测三个替换行 `b\nc\nd\n` ↔ `B\nC\nDd\n` 时，base 的 `d\n` 会与 target 的
/// `Dd\n` 共享 `d`，被误当公共后缀裁掉，产生重复行）。行级 trim（整行相等才算公共）
/// 无此歧义。
fn diff_lines(bm: &[&str], tm: &[&str], off: usize, out: &mut Vec<LineHunk>) {
    debug_assert!(!(bm.is_empty() && tm.is_empty()));
    if bm.is_empty() {
        // 纯插入整段
        out.push(LineHunk {
            a_start: off,
            a_end: off,
            repl: tm.concat(),
        });
        return;
    }
    if tm.is_empty() {
        // 纯删除整段
        out.push(LineHunk {
            a_start: off,
            a_end: off + bm.len(),
            repl: String::new(),
        });
        return;
    }
    // 公共行前缀 / 后缀
    let mut lp = 0;
    while lp < bm.len() && lp < tm.len() && bm[lp] == tm[lp] {
        lp += 1;
    }
    let mut ls = 0;
    while ls < bm.len() - lp
        && ls < tm.len() - lp
        && bm[bm.len() - 1 - ls] == tm[tm.len() - 1 - ls]
    {
        ls += 1;
    }
    let b_inner = &bm[lp..bm.len() - ls];
    let t_inner = &tm[lp..tm.len() - ls];
    let (n, m) = (b_inner.len(), t_inner.len());
    if b_inner.is_empty() && t_inner.is_empty() {
        return;
    }
    if n * m > LCS_CELL_LIMIT {
        // 大段/分散多编辑兜底：整段替换（仍然增量，非 set_text 级联）。
        out.push(LineHunk {
            a_start: off + lp,
            a_end: off + lp + n,
            repl: t_inner.concat(),
        });
        return;
    }
    // 有界 LCS：反推差异 hunk（matched 行之间夹着的未匹配段 = 一处替换）。
    let mut dp = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if b_inner[i] == t_inner[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }
    let (mut i, mut j) = (0usize, 0usize);
    let mut open: Option<usize> = None;
    let mut repl: Vec<&str> = Vec::new();
    while i < n || j < m {
        if i < n && j < m && b_inner[i] == t_inner[j] {
            if let Some(a) = open.take() {
                out.push(LineHunk {
                    a_start: off + lp + a,
                    a_end: off + lp + i,
                    repl: repl.concat(),
                });
                repl.clear();
            }
            i += 1;
            j += 1;
        } else {
            if open.is_none() {
                open = Some(i);
            }
            let down = if i < n && j < m {
                dp[i + 1][j] >= dp[i][j + 1]
            } else {
                i < n
            };
            if down {
                i += 1;
            } else {
                repl.push(t_inner[j]);
                j += 1;
            }
        }
    }
    if let Some(a) = open {
        out.push(LineHunk {
            a_start: off + lp + a,
            a_end: off + lp + i,
            repl: repl.concat(),
        });
    }
}

/// 单行 ↔ 单行 hunk 的字符级精修：把「整行替换」缩到实际变更的字符区间
/// （长行里改 1 字符 → 只替换那几个字符，不冲刷整行）。
/// `bo..eo` = base 中该行的字节范围（含尾部 `\n`），`repl` = 目标行文本。
fn refine_single(base: &str, bo: usize, eo: usize, repl: &str) -> (Range<usize>, String) {
    let line = &base[bo..eo];
    if line == repl {
        return (bo..eo, repl.to_string());
    }
    // 字符级公共前缀
    let mut pre_b = 0usize;
    let mut pre_t = 0usize;
    let mut it_b = line.chars();
    let mut it_t = repl.chars();
    while let (Some(cb), Some(ct)) = (it_b.next(), it_t.next()) {
        if cb != ct {
            break;
        }
        pre_b += cb.len_utf8();
        pre_t += ct.len_utf8();
    }
    // 字符级公共后缀（从尾部回扫；边界均为字符起点，安全切片）
    let mut end_b = line.len();
    let mut end_t = repl.len();
    loop {
        let pb = line[..end_b].char_indices().next_back();
        let pt = repl[..end_t].char_indices().next_back();
        let (Some((ib, cb)), Some((it, ct))) = (pb, pt) else { break };
        if cb != ct {
            break;
        }
        end_b = ib;
        end_t = it;
    }
    if pre_b >= end_b || pre_t >= end_t {
        // 无共同骨架（整体重写该行）：退回整行替换，保证合法。
        return (bo..eo, repl.to_string());
    }
    let new_lo = bo + pre_b;
    let new_eo = bo + end_b;
    let new_repl = &repl[pre_t..end_t];
    debug_assert!(new_lo <= new_eo);
    (new_lo..new_eo, new_repl.to_string())
}

/// 主入口：把 `base` → `target` 的**行级差异**转为少量替换 hunk（按 base 字节偏移
/// 升序、互不重叠；外部边界整行、1:1 行 hunk 精修到字符级）。
/// 返回空 = 无差异或 `base == target`。
pub fn line_diff(base: &str, target: &str) -> Vec<(Range<usize>, String)> {
    if base == target {
        return Vec::new();
    }
    let bl = line_spans(base);
    let tl = line_spans(target);
    let bm: Vec<&str> = bl.iter().map(|(_, s)| *s).collect();
    let tm: Vec<&str> = tl.iter().map(|(_, s)| *s).collect();
    let mut hunks: Vec<LineHunk> = Vec::new();
    diff_lines(&bm, &tm, 0, &mut hunks);
    // 行 hunk → base 字节范围（hunk 覆盖整行：`行起始 .. 下一行起始/全文末`；
    // 纯插入可锚到末行之后 → 用全文末字节偏移）。
    let bb_len = base.len();
    let b_off = |li: usize| if li < bl.len() { bl[li].0 } else { bb_len };
    let mut out: Vec<(Range<usize>, String)> = Vec::with_capacity(hunks.len());
    for h in hunks {
        let a_start = b_off(h.a_start);
        let a_end = b_off(h.a_end);
        if h.a_end - h.a_start == 1 {
            // 单行 hunk：字符级精修（长行小改动不必冲刷整行）
            let (rng, txt) = refine_single(base, a_start, a_end, &h.repl);
            out.push((rng, txt));
        } else {
            out.push((a_start..a_end, h.repl));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(base: &str, hunks: &[(Range<usize>, String)]) -> String {
        let mut out = base.to_string();
        for (r, t) in hunks.iter().rev() {
            out.replace_range(r.clone(), t);
        }
        out
    }

    fn roundtrip(base: &str, target: &str) {
        let hunks = line_diff(base, target);
        let got = apply(base, &hunks);
        assert_eq!(got, target, "diff(base→target) 应用后必须精确还原 target\nbase={base:?}\ntarget={target:?}\nhunks={hunks:?}");
    }

    #[test]
    fn diff_identical_is_empty() {
        assert!(line_diff("a\nb\nc\n", "a\nb\nc\n").is_empty());
    }

    #[test]
    fn diff_one_char_mid_line() {
        // 长行中改 1 字符 → 只替换那一小段（单行精修）
        let base = format!("{}{}", "x".repeat(100), "hello\nrest\n");
        let mut target = base.clone();
        let at = 100 + 1;
        target.replace_range(at..at + 1, "H");
        let hunks = line_diff(&base, &target);
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].1, "H");
        assert!((hunks[0].0.end - hunks[0].0.start) <= 2);
        roundtrip(&base, &target);
    }

    #[test]
    fn diff_insert_at_doc_start() {
        let base = "# 标题\n正文。\n";
        let target = "# 新\n# 标题\n正文。\n";
        let hunks = line_diff(base, target);
        roundtrip(base, target);
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].0, 0..0);
        assert_eq!(hunks[0].1, "# 新\n");
    }

    #[test]
    fn diff_change_three_lines() {
        // 改 3 行：base 的 b\nc\nd\n ↔ target 的 B\nC\nDd\n（target 改行共享尾字符 'd'，
        // 字节后缀贪心必踩坑 → 必须行级 trim，见 diff_lines 注释）。
        let base = "a\nb\nc\nd\ne\nf\n";
        let target = "a\nB\nC\nDd\ne\nf\n";
        roundtrip(base, target);
        let hunks = line_diff(base, target);
        assert!(
            hunks.len() <= 2,
            "相邻三行替换 hunk 应 ≤2，实际 {hunks:?}"
        );
        let got = apply(base, &hunks);
        assert_eq!(got, target);
    }

    #[test]
    fn diff_edit_middle_keep_surroundings() {
        let base = "line0\nline1\nBB\nCC\nline4\nline5\n";
        let target = "line0\nline1\nxx\nline4\nline5\n";
        roundtrip(base, target);
        let hunks = line_diff(base, target);
        // 只应在 BB/CC 区产生 1 个 hunk（或 2 个紧邻 hunk，判宽松）
        assert!(hunks.len() <= 2, "局部改动 hunk 过多：{hunks:?}");
        for (r, _) in &hunks {
            assert!(r.start <= r.end && r.end <= base.len());
        }
    }

    #[test]
    fn diff_delete_middle() {
        let base = "a\nb\nc\nd\ne\n";
        let target = "a\ne\n";
        roundtrip(base, target);
        let hunks = line_diff(base, target);
        assert!(hunks.len() <= 2);
        assert_eq!(apply(base, &hunks), target);
    }

    #[test]
    fn diff_cjk_mid_line() {
        // 中文：CJK 字符替换不得切破 UTF-8 边界
        let prefix = "开头内容很长的前缀";
        let base = format!("{prefix}正文内容{prefix}结尾内容\n另一行\n");
        let mut target = base.clone();
        let at = prefix.len() + "正文".len();
        target.replace_range(at..at + 3, "换");
        let hunks = line_diff(&base, &target);
        roundtrip(&base, &target);
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].1, "换");
    }

    #[test]
    fn diff_trailing_newline_growth() {
        let base = "a\nb";
        let target = "a\nb\nc\n";
        roundtrip(base, target);
        let hunks = line_diff(base, target);
        assert!(!hunks.is_empty());
    }

    #[test]
    fn diff_whole_doc_replaced() {
        // 整体替换 → 兜底为一段大替换，但仍需精确还原
        let base = "old1\nold2\nold3\nold4\nold5\nold6\nold7\nold8\n";
        let target = "new1\nnew2\nnew3\n";
        roundtrip(base, target);
        let hunks = line_diff(base, target);
        assert_eq!(apply(base, &hunks), target);
    }

    #[test]
    fn diff_large_region_fallback() {
        // 超过 LCS 上限 → 整段替换兜底，仍精确还原
        let base: String = (0..3000).map(|i| format!("base_line_{i}\n")).collect();
        let mut target = base.clone();
        for i in 100..2900 {
            target = target.replacen(&format!("base_line_{i}"), &format!("changed_{i}"), 1);
        }
        roundtrip(&base, &target);
        let hunks = line_diff(&base, &target);
        assert_eq!(apply(&base, &hunks), target);
    }

    #[test]
    fn diff_append_at_end() {
        let base = "a\nb";
        let target = "a\nb + 追加";
        roundtrip(base, target);
        let hunks = line_diff(base, target);
        let got = apply(base, &hunks);
        assert_eq!(got, target);
    }

    #[test]
    fn diff_prepend_and_append() {
        let base = "middle\n";
        let target = "head\nmiddle\ntail";
        roundtrip(base, target);
        let hunks = line_diff(base, target);
        assert_eq!(apply(base, &hunks), target);
        assert!(hunks.len() <= 2);
    }
}
