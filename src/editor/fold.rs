//! 代码折叠（视图层投影）：**绝不修改 Rope**，折叠状态是一份独立于文本的
//! 「视觉行 ↔ 逻辑行」映射视图。
//!
//! 设计约束（与编辑器固定窗口虚拟化对齐）：
//! - 折叠隐藏的是**整逻辑行**，行高保持统一 → 渲染/滚动仍跑「行数 × 行高」的
//!   像素模型，只是把「逻辑行数」换成「视觉行数」（`visual_line_count`）；
//! - 折叠区间**平坦互不相交**（v1 取舍）：映射是单调的、可二分，查询 O(log F)；
//!   嵌套折叠需要区间代数，留给后续（届时才值得上 SumTree）。
//! - `Fold { start, end }` 半开区间：`start` 行本身仍可见（折叠首行），
//!   **隐藏区间为 `(start, end)`**（`end` 行自身可见）。
//!
//! # 映射公式（F = 折叠数，全部 O(log F)）
//!
//! `hidden_prefix[i]` = 前 `i` 个折叠累计隐藏行数（`body_len = end-start-1`）。
//!
//! - `visual_row(logical)`：phys 行折叠体内部 → 塌缩到该折叠首行的视觉行；
//!   否则 `logical − 在其之前已结束的折叠隐藏行数`。
//! - `logical_row(visual)`：二分「折叠首行视觉位置」，视觉落在折叠首行 ⇒ 其
//!   逻辑行，否则 `visual + hidden_prefix`（折叠体在视觉上是「塌缩的一格」）。

use gpui::SharedString;

/// 一个折叠区间：隐藏 `(start, end)`，`start` 行可见、`end` 行可见。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fold {
    /// 折叠起始逻辑行（可见，渲染折叠指示器）。
    pub start: usize,
    /// 折叠结束逻辑行（**开区间**：隐藏 `[start+1, end)`，`end` 自身可见）。
    pub end: usize,
}

impl Fold {
    /// 本折叠隐藏的逻辑行数（start 之后、end 之前的行）。
    pub fn body_len(&self) -> usize {
        self.end.saturating_sub(self.start).saturating_sub(1)
    }
}

/// 一个**跨行**括号配对：括号标尺（bracket guides）的一条竖线区间，
/// 从开括号所在行**之后**一直画到闭括号所在行**之前**。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BracketPair {
    /// 开括号所在逻辑行。
    pub open_line: usize,
    /// 开括号在行内的**字节列**（保留：诊断用）。
    pub open_col: usize,
    /// 闭括号所在逻辑行（恒 > `open_line`）。
    pub close_line: usize,
    /// 竖线所在**宽度单位列**（空格=1、Tab=4，见 [`super::buffer::char_width_units`]）：
    /// **开括号行的缩进宽**（块首行缩进，如 impl 行缩进 0、fn 行缩进 4、Self 行缩进 8）。
    /// 竖线画在这里并与块体左对齐；不画在开/闭行上（避免压到 `{`/`}` 字形）。
    pub guide_units: usize,
}

/// 行首空白（空格/Tab）的宽度单位数：空格=1、Tab=4（与 `char_width_units` 一致）。
fn leading_ws_units(s: &str) -> usize {
    s.bytes()
        .take_while(|b| *b == b' ' || *b == b'\t')
        .map(|b| if b == b'\t' { 4 } else { 1 })
        .sum()
}

/// 扫描全文的跨行括号配对（`{ }` / `( )` / `[ ]`），O(行数) 单遍、仅记录**跨行**配对
/// （单行 `{}` 不产竖线）。`line` 按逻辑行号返回该行文本（只读）。
///
/// 词法兜底：跳过 `//` 行注释、`/* */` 块注释（跨行状态）与 `"..."` 字符串
/// （含 `\"` 转义）；**不**处理原样字符串/字符字面量（Rust 生命周期 `'a` 不宜当
/// 字符字面量跳过，否则会吞掉整行 → 可选本忽略少数偏例）。
/// 配平失败（交错/缺失）时丢弃不平衡项——竖线是纯视觉参考，偏例不影响正确性。
pub fn scan_bracket_pairs<F>(line: F, row_count: usize) -> Vec<BracketPair>
where
    F: Fn(usize) -> SharedString,
{
    let mut out: Vec<BracketPair> = Vec::new();
    // 栈：(开括号行, 开括号字节列, 开括号字符)
    let mut stack: Vec<(usize, usize, char)> = Vec::new();
    let mut in_block_comment = false;
    for line_ix in 0..row_count {
        let text: SharedString = line(line_ix);
        let s = text.as_ref();
        let mut col = 0usize;
        while col < s.len() {
            let ch = s[col..].chars().next().unwrap_or('\0');
            if in_block_comment {
                if s[col..].starts_with("*/") {
                    in_block_comment = false;
                    col += 2;
                } else {
                    col += ch.len_utf8();
                }
                continue;
            }
            match ch {
                '/' if s[col..].starts_with("//") => break,
                '/' if s[col..].starts_with("/*") => {
                    in_block_comment = true;
                    col += 2;
                }
                '"' => {
                    // 跳过字符串字面量（`\"` 转义）。
                    col += 1;
                    while col < s.len() {
                        let c = s[col..].chars().next().unwrap_or('\0');
                        col += c.len_utf8();
                        if c == '\\' && col < s.len() {
                            let e = s[col..].chars().next().unwrap_or('\0');
                            col += e.len_utf8();
                        } else if c == '"' {
                            break;
                        }
                    }
                }
                '{' | '(' | '[' => {
                    stack.push((line_ix, col, ch));
                    col += ch.len_utf8();
                }
                '}' | ')' | ']' => {
                    let want = match ch {
                        '}' => '{',
                        ')' => '(',
                        _ => '[',
                    };
                    while let Some((ol, oc, och)) = stack.pop() {
                        if och == want {
                            if line_ix > ol {
                                // 竖线列 = 开括号行的缩进宽（块首缩进）。
                                let guide_units = leading_ws_units(line(ol).as_ref());
                                out.push(BracketPair {
                                    open_line: ol,
                                    open_col: oc,
                                    close_line: line_ix,
                                    guide_units,
                                });
                            }
                            break;
                        }
                    }
                    col += ch.len_utf8();
                }
                _ => {
                    col += ch.len_utf8();
                }
            }
        }
    }
    out
}

/// 平坦（互不相交、按 `start` 升序）折叠集合 + 视觉↔逻辑映射。
#[derive(Clone, Debug, Default)]
pub struct FoldMap {
    ranges: Vec<Fold>,
    /// `hidden_prefix[i]` = 前 `i+1` 个折叠累计隐藏行数。
    hidden_prefix: Vec<usize>,
    /// 逻辑总行数（编辑器行数，`set_total`/`sync_edit` 维护）。
    total: usize,
}

impl FoldMap {
    pub fn new() -> Self {
        Self::default()
    }

    /// 清空全部折叠（文档整篇替换/打开前调用）。
    pub fn clear(&mut self) {
        self.ranges.clear();
        self.hidden_prefix.clear();
        self.total = 0;
    }

    /// 同步逻辑总行数（编辑器行数变化后），同时把越界的折叠裁掉。
    pub fn set_total(&mut self, total: usize) {
        self.total = total.max(1);
        self.rebuild();
    }

    /// 当前逻辑总行数。
    pub fn total(&self) -> usize {
        self.total
    }

    /// 视觉总行数（O(1)）。
    pub fn visual_line_count(&self) -> usize {
        self.total
            .saturating_sub(self.hidden_prefix.last().copied().unwrap_or(0))
    }

    /// 逻辑行 ∈ [start, end) 的折叠（含首行自身）；无则 `None`。
    pub fn fold_at(&self, line: usize) -> Option<&Fold> {
        let i = self.ranges.partition_point(|f| f.start <= line);
        let i = i.checked_sub(1)?;
        let f = &self.ranges[i];
        (line < f.end).then_some(f)
    }

    /// 以 `line` 为折叠首行的折叠（= 该行渲染「已折叠」指示器）。
    pub fn fold_starting_at(&self, line: usize) -> Option<&Fold> {
        match self.fold_at(line) {
            Some(f) if f.start == line => Some(f),
            _ => None,
        }
    }

    /// `line` 是否处于某折叠的**隐藏体内部**（严格 `start < line < end`）。
    pub fn hidden_containing(&self, line: usize) -> Option<&Fold> {
        match self.fold_at(line) {
            Some(f) if line > f.start => Some(f),
            _ => None,
        }
    }

    /// 逻辑行 → 视觉行（隐藏体内部塌缩到折叠首行的视觉行）。
    pub fn visual_row(&self, logical: usize) -> usize {
        // 最后一个 start <= logical 的折叠。
        if let Some(f) = self.fold_at(logical) {
            // logical ∈ [f.start, f.end)：首行直接等于自身，隐藏体塌缩到首行。
            let i = self.ranges.partition_point(|x| x.start <= logical) - 1;
            let before = self.hidden_prefix.get(i.wrapping_sub(1)).copied().unwrap_or(0);
            return f.start - before;
        }
        // 折叠体之前/之后：会计入所有「已完整结束」的折叠（end <= logical）。
        let j = self.ranges.partition_point(|f| f.end <= logical);
        let hidden = self.hidden_prefix.get(j.wrapping_sub(1)).copied().unwrap_or(0);
        logical.saturating_sub(hidden)
    }

    /// 视觉行 → 逻辑行（单调递增；映射到可见行）。
    pub fn logical_row(&self, visual: usize) -> usize {
        let vcount = self.visual_line_count();
        if vcount == 0 {
            return 0;
        }
        let visual = visual.min(vcount.saturating_sub(1));
        // 最后一个「折叠首行视觉位置 <= visual」的折叠。
        let n = self.ranges.len();
        let mut lo = 0usize;
        let mut hi = n;
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.visual_start(mid) <= visual {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        if lo == 0 {
            return visual;
        }
        let idx = lo - 1;
        if visual == self.visual_start(idx) {
            // 视觉恰落在折叠首行 → 该首行的逻辑行。
            self.ranges[idx].start
        } else {
            // 折叠体在视觉上是「塌缩的一格」：其后的视觉行跳过整个隐藏体。
            visual + self.hidden_prefix[idx]
        }
    }

    /// 折叠 `i` 的首行在视觉坐标系上的位置。
    fn visual_start(&self, i: usize) -> usize {
        let before = if i == 0 { 0 } else { self.hidden_prefix[i - 1] };
        self.ranges[i].start.saturating_sub(before)
    }

    /// 切换 `[start, end)`（半开逻辑行）的折叠：
    /// - 已有以 `start` 为折叠首行的折叠 → 展开（删除）；
    /// - 否则删除所有与之相交的折叠后新建（保持平坦）。
    pub fn toggle(&mut self, start: usize, end: usize) {
        if self.total == 0 {
            self.total = 1;
        }
        // 展开：start 行已是折叠首行。
        if let Some(pos) = self.ranges.iter().position(|f| f.start == start) {
            self.ranges.remove(pos);
            self.rebuild();
            return;
        }
        // 无可隐藏行（end <= start+1）→ 无事发生（也不能创建空折叠）。
        if end <= start + 1 {
            return;
        }
        let start = start.min(self.total.saturating_sub(1));
        let end = end.min(self.total);
        if end <= start + 1 {
            return;
        }
        // 平坦化：去掉与 [start, end) 相交的既有折叠。
        self.ranges.retain(|f| !(f.end > start && f.start < end));
        self.ranges.push(Fold { start, end });
        self.rebuild();
    }

    /// 展开包含 `line` 的折叠（含折叠首行自身）。
    pub fn expand_at(&mut self, line: usize) {
        if let Some(f) = self.fold_at(line).copied() {
            self.ranges.retain(|x| x.start != f.start);
            self.rebuild();
        }
    }

    /// 文本编辑后同步折叠区间（**必须在 Buffer 已应用编辑后、且给出编辑前受影响
    /// 的行跨度时调用**）：
    /// - 被替换的行跨度 `[line_a, line_b]`（含两端，编辑前坐标）；
    /// - `new_text`（替换后的新文本，用于算净行增量）；
    /// - `new_total`（编辑后的逻辑总行数）。
    ///
    /// 规则：
    /// 1. 折叠**隐藏体**与编辑跨度相交 → 自动展开（丢弃该折叠）；
    /// 2. 折叠**首行**落在编辑跨度内 → 锚消失，丢弃；
    /// 3. 折叠整体在编辑之前 → 原样保留；
    /// 4. 折叠整体在编辑之后 → 按净行增量平移（越界截断/丢弃）。
    pub fn sync_edit(&mut self, line_a: usize, line_b: usize, new_text: &str, new_total: usize) {
        self.total = new_total.max(1);
        let insert_lines = new_text.matches('\n').count();
        let remove_lines = line_b.saturating_sub(line_a);
        let delta = insert_lines as isize - remove_lines as isize;
        let mut keep: Vec<Fold> = Vec::with_capacity(self.ranges.len());
        for f in self.ranges.drain(..) {
            // 隐藏体 (f.start, f.end) 与 [line_a, line_b] 相交？
            // 相交 ⟺ 存在 L：f.start < L < f.end 且 line_a <= L <= line_b。
            if f.start < line_b && f.end > line_a {
                continue; // 编辑进了折叠体 → 展开
            }
            // 折叠首行被替换。
            if f.start >= line_a && f.start <= line_b {
                continue;
            }
            // 折叠整体在编辑之前。
            if f.end <= line_a {
                keep.push(f);
                continue;
            }
            // 折叠整体在编辑之后（f.start > line_b 由上面排除）→ 平移。
            let ns = (f.start as isize + delta).max(0) as usize;
            let ne = (f.end as isize + delta).max(0) as usize;
            let ne = ne.min(self.total);
            if ne > ns + 1 && ns < self.total {
                keep.push(Fold { start: ns, end: ne });
            }
        }
        self.ranges = keep;
        self.rebuild();
    }

    /// 重建前缀并裁剪越界/退化区间（保持平坦有序）。文档收缩（`set_total`）时把
    /// 越界折叠的 `end` **截断**到新总行数（而非整条丢弃），保留仍有效的折叠。
    fn rebuild(&mut self) {
        self.ranges.retain(|f| f.start < self.total);
        for f in &mut self.ranges {
            if f.end > self.total {
                f.end = self.total;
            }
        }
        self.ranges.retain(|f| f.end > f.start + 1);
        self.ranges.sort_by_key(|f| f.start);
        let mut prefix = Vec::with_capacity(self.ranges.len());
        let mut acc = 0usize;
        for f in &self.ranges {
            acc += f.body_len();
            prefix.push(acc);
        }
        self.hidden_prefix = prefix;
    }

    /// 全部折叠的只读视图（测试/诊断）。
    pub fn folds(&self) -> &[Fold] {
        &self.ranges
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造：total 行 + 若干折叠 `(start, end)`。
    fn fm(total: usize, folds: &[(usize, usize)]) -> FoldMap {
        let mut m = FoldMap::new();
        m.set_total(total);
        for &(s, e) in folds {
            m.toggle(s, e);
        }
        m
    }

    #[test]
    fn toggle_and_visual_count() {
        let mut m = FoldMap::new();
        m.set_total(12);
        // 折叠 [5,10)：隐藏 6..9 共 4 行。
        m.toggle(5, 10);
        assert_eq!(m.visual_line_count(), 12 - 4);
        assert_eq!(m.folds(), &[Fold { start: 5, end: 10 }]);
        // 再 toggle 同 start → 展开。
        m.toggle(5, 10);
        assert_eq!(m.visual_line_count(), 12);
        assert!(m.folds().is_empty());
        // 空折叠不可创建。
        m.toggle(6, 7);
        assert!(m.folds().is_empty());
    }

    #[test]
    fn toggle_removes_overlaps() {
        let mut m = fm(20, &[(2, 8), (10, 15)]);
        // 新建 [3,12) 与两者相交 → 全删后只留新建。
        m.toggle(3, 12);
        assert_eq!(m.folds(), &[Fold { start: 3, end: 12 }]);
    }

    #[test]
    fn visual_logical_roundtrip_flat() {
        let m = fm(12, &[(5, 10)]);
        // 逻辑 → 视觉
        for l in 0..=4 {
            assert_eq!(m.visual_row(l), l);
        }
        assert_eq!(m.visual_row(5), 5); // 首行可见
        for l in 6..=9 {
            assert_eq!(m.visual_row(l), 5, "隐藏体塌缩到首行 l={l}");
        }
        assert_eq!(m.visual_row(10), 6, "end 行可见、跳过隐藏体");
        assert_eq!(m.visual_row(11), 7);
        // 视觉 → 逻辑
        for v in 0..=5 {
            assert_eq!(m.logical_row(v), v);
        }
        assert_eq!(m.logical_row(6), 10);
        assert_eq!(m.logical_row(7), 11);
    }

    #[test]
    fn multiple_folds_mapping() {
        let m = fm(30, &[(3, 6), (10, 20)]);
        // 折叠1 隐藏 4,5（2 行）；折叠2 隐藏 11..19（9 行）。
        assert_eq!(m.visual_line_count(), 30 - 11);
        assert_eq!(m.visual_row(3), 3);
        assert_eq!(m.visual_row(5), 3);
        assert_eq!(m.visual_row(6), 4);
        assert_eq!(m.visual_row(10), 8); // 10 - hidden(折叠1=2)
        assert_eq!(m.visual_row(30 - 1), 30 - 11 - 1);
        assert_eq!(m.logical_row(0), 0);
        assert_eq!(m.logical_row(3), 3);
        assert_eq!(m.logical_row(4), 6);
        assert_eq!(m.logical_row(8), 10);
        // 视觉 9 = 折叠2 首行（10）之后的首个可见行 = 跳过折叠体 11..19 → 逻辑 20。
        assert_eq!(m.logical_row(9), 20);
        // 单调映射往返：对每个视觉行，visual_row(logical_row(v)) == v。
        for v in 0..m.visual_line_count() {
            let l = m.logical_row(v);
            assert_eq!(m.visual_row(l), v, "roundtrip v={v} l={l}");
        }
    }

    #[test]
    fn sync_edit_outside_keeps_and_shifts() {
        let mut m = fm(12, &[(5, 10)]);
        // 在第 2 行插入一行（line_a=line_b=2，new_text 含换行，delta=+1）。
        m.sync_edit(2, 2, "\nX", 13);
        assert_eq!(m.folds(), &[Fold { start: 6, end: 11 }]);
        // 撤销回插：删除第 2 起的整行文本（编辑前行跨度 [2, 3)，delta=-1）。
        m.sync_edit(2, 3, "", 12);
        assert_eq!(m.folds(), &[Fold { start: 5, end: 10 }]);
        // 在折叠之后删除一行（delta=-1，折叠整体在其前 → 原样保留）。
        m.sync_edit(11, 12, "", 11);
        assert_eq!(m.folds(), &[Fold { start: 5, end: 10 }]);
    }

    #[test]
    fn sync_edit_inside_body_expands() {
        let mut m = fm(12, &[(5, 10)]);
        // 编辑隐藏在 7 行的内容 → 折叠展开。
        m.sync_edit(7, 7, "a", 12);
        assert!(m.folds().is_empty());
    }

    #[test]
    fn sync_edit_on_start_line_drops_fold() {
        let mut m = fm(12, &[(5, 10)]);
        // 编辑折叠首行（整行被替换）→ 锚消失，丢弃。
        m.sync_edit(5, 5, "zzz", 12);
        assert!(m.folds().is_empty());
    }

    #[test]
    fn sync_edit_deleting_body_removes_and_shifts_after() {
        let mut m = fm(20, &[(5, 10), (15, 18)]);
        // 删除整行跨度 [4, 10)（覆盖折叠1 的 5..9）：折叠1 消失；
        // 净 delta = 0 − (10−4) = −6 → 折叠2（start15>10）平移为 [9,12]。
        m.sync_edit(4, 10, "", 14);
        assert_eq!(m.folds(), &[Fold { start: 9, end: 12 }]);
    }

    #[test]
    fn set_total_clamps_ranges() {
        let mut m = fm(20, &[(5, 10), (15, 18)]);
        m.set_total(9); // 文档变短：折叠2 越界被裁，折叠1 end 截到 9。
        assert_eq!(m.folds(), &[Fold { start: 5, end: 9 }]);
    }

    #[test]
    fn hidden_containing_queries() {
        let m = fm(12, &[(5, 10)]);
        assert!(m.fold_starting_at(5).is_some());
        assert!(m.fold_starting_at(6).is_none());
        assert!(m.hidden_containing(5).is_none()); // 首行不是隐藏体
        assert!(m.hidden_containing(6).is_some());
        assert!(m.hidden_containing(9).is_some());
        assert!(m.hidden_containing(10).is_none()); // end 行可见
        assert_eq!(m.fold_at(7).map(|f| f.end), Some(10));
        assert_eq!(m.fold_at(11), None);
    }

    // ------------------------------------------------------------------
    // 括号配对扫描（括号标尺的数据源）
    // ------------------------------------------------------------------

    /// 按行号返回行文本的闭包（越界给空行）。
    fn line_fn<'a>(lines: &'a [&'a str]) -> impl Fn(usize) -> SharedString + 'a {
        move |i: usize| {
            lines
                .get(i)
                .map(|s| SharedString::from(*s))
                .unwrap_or_else(|| SharedString::new_static(""))
        }
    }

    #[test]
    fn bracket_scan_nested_and_multi_line_only() {
        let lines = [
            "impl RowData {",
            "    fn empty() -> Self {",
            "        Self {",
            "            a,",
            "        }",
            "    }",
            "}",
        ];
        let pairs = scan_bracket_pairs(line_fn(&lines), lines.len());
        // 只记跨行：内层 Self{, 中 fn{, 外层 impl{ 各一条；
        // 竖线列 = 块首行缩进宽（0/4/8 单位）。
        assert_eq!(
            pairs,
            vec![
                BracketPair { open_line: 2, open_col: 13, close_line: 4, guide_units: 8 },
                BracketPair { open_line: 1, open_col: 23, close_line: 5, guide_units: 4 },
                BracketPair { open_line: 0, open_col: 13, close_line: 6, guide_units: 0 },
            ]
        );
        // 单行配对不产竖线。
        let one = ["fn a() {}"];
        assert!(scan_bracket_pairs(line_fn(&one), 1).is_empty());
    }

    #[test]
    fn bracket_scan_skips_strings_and_comments() {
        let lines = [
            "let s = \"{ not(open }\";",       // 字符串里的括号忽略
            "// { comment bracket }",          // 行注释忽略
            "/* block { ( */",                 // 块注释开始
            "fn ok() {",                       // 真正的开括号
            "/* } still comment */",           // 闭括号在注释里 → 忽略
            "}",
        ];
        let pairs = scan_bracket_pairs(line_fn(&lines), lines.len());
        // 开行 `fn ok() {` 缩进 0 → 竖线列 0（块注释内的 `}` 已被跳过）。
        assert_eq!(
            pairs,
            vec![BracketPair { open_line: 3, open_col: 8, close_line: 5, guide_units: 0 }]
        );
        // 转义字符串 `"a\"{b"` 内部花括号忽略；块体 `    x();` → 竖线列 4。
        let esc = ["let s = \"a\\\"{b\";\nfn f() {\n    x();\n}"];
        let el: Vec<&str> = esc[0].split('\n').collect();
        let pairs = scan_bracket_pairs(line_fn(&el), el.len());
        assert_eq!(
            pairs,
            vec![BracketPair { open_line: 1, open_col: 7, close_line: 3, guide_units: 0 }],
            "字符串内转义花括号应被跳过"
        );
    }

    #[test]
    fn bracket_scan_lifetime_does_not_break() {
        let lines = ["fn max<'a>(x: &'a str, y: &'a str) &'a str {", "    x", "}"];
        let pairs = scan_bracket_pairs(line_fn(&lines), lines.len());
        // 生命周期撇号不当作字符字面量：`{` 在 ( ) 之后仍配对；开行缩进 0。
        assert_eq!(
            pairs,
            vec![BracketPair { open_line: 0, open_col: 43, close_line: 2, guide_units: 0 }]
        );
    }
}
