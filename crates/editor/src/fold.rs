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

/// 行首空白**字节数**（空格/Tab 各 1 字节；折叠候选缩进比较用）。
///
/// 注意：与 [`leading_ws_units`]（宽度单位）不同——候选扫描沿用视图层原
/// `view::leading_ws_len` 的「字节数」口径（`scan_fold_candidates` 里缩进只做
/// 大小比较，两者各自单调、但与旧实现保持逐位一致以免候选集合变化）。
fn leading_ws_bytes(s: &str) -> usize {
    s.bytes()
        .take_while(|b| *b == b' ' || *b == b'\t')
        .count()
}

/// 跨行括号配对的**段扫描**结果。
#[derive(Debug, Default)]
pub struct BracketScanReport {
    /// 段内产生的配对（`open_line ∈ [from, stop)`；`close_line ≤ stop`）。
    pub pairs: Vec<BracketPair>,
    /// 段内「栈空 + 不在块注释内」边界行（升序；含 `from`）。
    pub boundaries: Vec<usize>,
    /// 实际扫描停止行（`stop_after` 命中首个边界后的行；未命中 = `row_count`）。
    pub stop: usize,
}

/// 扫描全文/受影响段的跨行括号配对（`{ }` / `( )` / `[ ]`），O(行数) 单遍、仅记录
/// **跨行**配对（单行 `{}` 不产竖线）。`line` 按逻辑行号返回该行文本（只读）。
///
/// 词法兜底：跳过 `//` 行注释、`/* */` 块注释（跨行状态）与 `"..."` 字符串
/// （含 `\"` 转义）；**不**处理原样字符串/字符字面量（Rust 生命周期 `'a` 不宜当
/// 字符字面量跳过，否则会吞掉整行 → 可选本忽略少数偏例）。
/// 配平失败（交错/缺失）时丢弃不平衡项——竖线是纯视觉参考，偏例不影响正确性。
///
/// `scan_bracket_pairs` 是其「从 0 整篇、扫到文末」的薄包装；[`BracketScanReport`]
/// 额外给出「栈清空边界」供增量重算复用：
/// **从某个边界行以全新状态（空栈、不在块注释内）开始扫描，与从 0 整篇扫描在
/// `≥ 该行` 的处理上逐位一致**——括号不跨行（字符串在行内结束）、进入行只需
/// 栈 + 块注释位两个状态，边界后无任何 OPEN 段残留，故可断点续扫。
/// `stop_after = Some(m)` 时在**首个「行号 ≥ m 且处理后为边界」的行**停止
/// （增量重算用它圈定受影响包络尾；未命中则扫到文末）。
pub(crate) fn scan_bracket_segment<F>(
    line: F,
    row_count: usize,
    from: usize,
    stop_after: Option<usize>,
) -> BracketScanReport
where
    F: Fn(usize) -> SharedString,
{
    let mut out = BracketScanReport::default();
    if row_count == 0 {
        return out;
    }
    let from = from.min(row_count);
    // 栈：(开括号行, 开括号字节列, 开括号字符)
    let mut stack: Vec<(usize, usize, char)> = Vec::new();
    let mut in_block_comment = false;
    let mut line_ix = from;
    while line_ix < row_count {
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
                                out.pairs.push(BracketPair {
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
        if stack.is_empty() && !in_block_comment {
            // 栈清空 + 不在块注释内 = 安全断点（此后从空栈继续与整篇扫描一致）。
            out.boundaries.push(line_ix);
            if let Some(m) = stop_after {
                if line_ix >= m {
                    out.stop = line_ix;
                    return out;
                }
            }
        }
        line_ix += 1;
    }
    out.stop = row_count;
    out
}

/// 扫描全文的跨行括号配对（薄包装；语义与历史逐位一致）。
pub fn scan_bracket_pairs<F>(line: F, row_count: usize) -> Vec<BracketPair>
where
    F: Fn(usize) -> SharedString,
{
    scan_bracket_segment(line, row_count, 0, None).pairs
}

/// 折叠候选的**段扫描**结果。
#[derive(Debug, Default)]
pub struct FoldScanReport {
    /// 段内产生的折叠候选（`start ∈ [from, stop)`；`end ≤ stop`）。
    pub folds: Vec<Fold>,
    /// 段内「栈清空边界」行（升序；含 `from`——从边界以全新状态继续扫描与
    /// 从 0 整篇扫描在 `≥ from` 的行上逐位一致，见 [`scan_fold_segment`]）。
    pub boundaries: Vec<usize>,
    /// 实际扫描停止行（`stop_after` 命中首个边界后的行；未命中 = `row_count`）。
    pub stop: usize,
}

/// 扫描全文/受影响段的「可折叠候选」（缩进 + 闭记号启发式，与视图层解耦，可在
/// 后台线程/只读 Rope 快照上运行，语义等价于原 `CodeEditor::scan_fold_candidates`）。
///
/// 行 `i` 是候选起点 ⟺ 其后的首个非空行缩进更深（该行将藏起一个块）；块结束 = 首个
/// 缩进回退 `/` 同级闭记号（`}` 等）行。`line` 按逻辑行号返回该行文本（只读）。
/// 缩进口径 = 行首空白**字节数**（[`leading_ws_bytes`]，与折叠缩进启发式一致）。
///
/// `scan_fold_candidates` 是其「从 0 整篇、扫到文末」的薄包装。
/// 折叠扫描的持久状态只有 `stack`（`pending` 恒可由「上一非空行」行文本重建），
/// 故**栈空行是安全断点**：一条非空边界行 r 上，上一非空行 p 不可能被压栈
/// （压栈需 `indent_r > indent_p`，而 r 是边界意味着处理后栈空）、pending 进入
/// 下一行也仅由 r 决定 → 从 r 以全新状态开始与从 0 整篇扫描在 `≥ r` 逐位一致。
/// `stop_after = Some(m)` 时在**首个「行号 ≥ m 且处理后栈空」的行**停止
/// （增量重算用它圈定受影响包络尾；未命中则扫到文末）。
pub(crate) fn scan_fold_segment<F>(
    line: F,
    row_count: usize,
    from: usize,
    stop_after: Option<usize>,
) -> FoldScanReport
where
    F: Fn(usize) -> SharedString,
{
    let mut out = FoldScanReport::default();
    if row_count == 0 {
        return out;
    }
    let from = from.min(row_count);
    // 栈：挂起的块 `(start, indent)`。pending 记录上一非空行；下次看到更深行才入栈。
    let mut stack: Vec<(usize, usize)> = Vec::new();
    let mut pending: Option<(usize, usize)> = None;
    let mut r = from;
    while r < row_count {
        let text = line(r);
        let trimmed = text.trim_start();
        if !trimmed.is_empty() {
            let indent = leading_ws_bytes(text.as_ref());
            let closing = matches!(
                trimmed.chars().next(),
                Some('}') | Some(')') | Some(']') | Some('#') | Some('`')
            );
            if let Some((ps, pi)) = pending {
                // 上一行是块起点 ⟺ 当前行缩进更深。
                if indent > pi {
                    stack.push((ps, pi));
                }
                // 关闭缩进 ≥ 当前行的块（缩进回退或同级闭记号）。
                while let Some(&(bs, bi)) = stack.last() {
                    if indent < bi || (indent == bi && closing) {
                        if r >= bs + 2 {
                            out.folds.push(Fold { start: bs, end: r });
                        }
                        stack.pop();
                        continue;
                    }
                    break;
                }
            }
            pending = Some((r, indent));
            if stack.is_empty() {
                // 栈清空 = 安全断点（此后从空栈继续与整篇扫描逐位一致）。
                out.boundaries.push(r);
                if let Some(m) = stop_after {
                    if r >= m {
                        out.stop = r;
                        return out;
                    }
                }
            }
        }
        r += 1;
    }
    // EOF：关闭仍开着的块（延伸到最后一行）。
    let end = row_count;
    while let Some((bs, _bi)) = stack.pop() {
        if end >= bs + 2 {
            out.folds.push(Fold { start: bs, end });
        }
    }
    out.stop = row_count;
    out
}

/// 全篇扫描「可折叠候选」**纯函数**（薄包装；语义与历史逐位一致）。
pub(crate) fn scan_fold_candidates<F>(line: F, row_count: usize) -> Vec<Fold>
where
    F: Fn(usize) -> SharedString,
{
    scan_fold_segment(line, row_count, 0, None).folds
}

/// 整篇扫描折叠候选 + 括号配对 + 各自「栈清空边界」（打开/整篇替换路径用，
/// 一次性把四份结果全部落地）。
pub(crate) fn full_fold_brackets<F>(
    line: F,
    row_count: usize,
) -> (Vec<Fold>, Vec<BracketPair>, Vec<usize>, Vec<usize>)
where
    F: Fn(usize) -> SharedString + Copy,
{
    let fr = scan_fold_segment(line, row_count, 0, None);
    let br = scan_bracket_segment(line, row_count, 0, None);
    (fr.folds, br.pairs, fr.boundaries, br.boundaries)
}

/// 单个受影响行段的**增量重算**（整篇扫描的等价替代，纯函数、可在后台线程跑，
/// 对拍契约：对同一文档同一次编辑，输出与整篇扫描**逐位一致**）。
///
/// 前提（调用方保证，见本仓库 view 层 `scan_pending_edit` / `scan_multi_edit`）：
/// - `prev_folds` / `prev_pairs` / `prev_fb` / `prev_pb` 是**上次落地**的扫描结果，
///   对应「编辑前」文档；
/// - 自上落地后只累积了**一个**受影响行段 `edit = Some((line_a, min_stop, delta))`：
///   `line_a` = 编辑首行（编辑前坐标），`min_stop` = 新坐标下「首个未受影响行」的
///   边界下限，`delta` = 净行增量（新行数 − 旧行数）；
/// - 多个编辑段累积或前提缺失 → 调用方置 `multi = true` / `edit = None` →
///   本函数退化为整篇扫描（正确性兜底）。
///
/// 机制（fold 候选与括号配对各自独立）：
/// 1. `B` = 上次边界中最大的 `≤ line_a` 的栈清空边界（无则 0）；
/// 2. 在「受影响包络 `[B, stop)`」上局部重扫（`stop` = 首个 `≥ min_stop` 的边界）；
/// 3. 合并：`start < B` 的旧项保留、`start ∈ [B, stop_old)` 的旧项被重扫结果替换、
///    `start ≥ stop_old`（`stop_old = stop − delta`）的旧项按 `delta` **平移**
///    （未变段尾部行号平移）；边界列表同款合并（尾部平移）。
///
/// 返回 `(fold_candidates, bracket_pairs, fold_boundaries, bracket_boundaries)`，
/// 与整篇扫描等价（调用方可再按 `start` / `open_line` 排序）。
pub(crate) fn incremental_fold_brackets<F>(
    line: F,
    row_count: usize,
    full_limit: usize,
    prev_folds: &[Fold],
    prev_pairs: &[BracketPair],
    prev_fb: &[usize],
    prev_pb: &[usize],
    edit: Option<(usize, usize, i64)>,
    multi: bool,
) -> (Vec<Fold>, Vec<BracketPair>, Vec<usize>, Vec<usize>)
where
    F: Fn(usize) -> SharedString + Copy,
{
    // 超大文档（行数 > full_limit）保持「按需折叠」语义：候选/配对/边界一并清空，
    // 由手动折叠/点击按需提供（避免病理文档的 O(全文) 后台扫描与增量兜底退化）。
    if row_count > full_limit {
        return (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    }
    // 增量前提不满足（多编辑段累积 / 无编辑描述）→ 整篇扫描兜底。
    let Some((line_a, min_stop, delta)) = edit.filter(|_| !multi) else {
        return full_fold_brackets(line, row_count);
    };

    // —— fold 候选 ——
    // 最大边界 ≤ line_a（无则从 0 整段重扫；从边界以全新状态开始即逐位正确）。
    let bf = match prev_fb.partition_point(|&b| b <= line_a) {
        0 => 0,
        n => prev_fb[n - 1],
    };
    let fr = scan_fold_segment(line, row_count, bf, Some(min_stop));
    let stop_old_f = (fr.stop as i64 - delta).max(0) as usize;
    let mut folds: Vec<Fold> = Vec::with_capacity(prev_folds.len());
    for &f in prev_folds {
        if f.start < bf {
            folds.push(f); // 完全在编辑之前：原样保留
        } else if f.start >= stop_old_f {
            // 完全在受影响段之后：尾部行号平移
            let ns = (f.start as i64 + delta).max(0) as usize;
            let ne = ((f.end as i64 + delta) as usize).min(row_count);
            if ns < row_count && ne > ns + 1 {
                folds.push(Fold { start: ns, end: ne });
            }
        }
        // [bf, stop_old_f) 内的旧项被重扫结果替换（下面并入）。
    }
    folds.extend_from_slice(&fr.folds);
    let mut new_fb: Vec<usize> = Vec::with_capacity(prev_fb.len());
    new_fb.extend(prev_fb.iter().copied().filter(|&b| b < bf));
    new_fb.extend_from_slice(&fr.boundaries);
    // 边界与 fold 不同：段扫描在失效后又补记了 stop 行本身 → 旧边界 `== stop_old`
    // （映射到同一 stop 行）会被段扫描边界覆盖，只平移**严格大于** stop_old 的尾边界。
    new_fb.extend(
        prev_fb
            .iter()
            .copied()
            .filter(|&b| b > stop_old_f)
            .map(|b| (b as i64 + delta).max(0) as usize)
            .filter(|&b| b < row_count),
    );

    // —— 括号配对 ——
    let bb = match prev_pb.partition_point(|&b| b <= line_a) {
        0 => 0,
        n => prev_pb[n - 1],
    };
    let br = scan_bracket_segment(line, row_count, bb, Some(min_stop));
    let stop_old_b = (br.stop as i64 - delta).max(0) as usize;
    let mut pairs: Vec<BracketPair> = Vec::with_capacity(prev_pairs.len());
    for &p in prev_pairs {
        if p.open_line < bb {
            pairs.push(p); // 完全在编辑之前：原样保留
        } else if p.open_line >= stop_old_b {
            // 完全在受影响段之后：尾部行号平移（开/闭行一起移）
            let no = (p.open_line as i64 + delta).max(0) as usize;
            let nc = ((p.close_line as i64 + delta) as usize).min(row_count);
            if no < row_count && nc > no {
                pairs.push(BracketPair {
                    open_line: no,
                    close_line: nc,
                    ..p
                });
            }
        }
        // [bb, stop_old_b) 内的旧项被重扫结果替换（下面并入）。
    }
    pairs.extend_from_slice(&br.pairs);
    let mut new_pb: Vec<usize> = Vec::with_capacity(prev_pb.len());
    new_pb.extend(prev_pb.iter().copied().filter(|&b| b < bb));
    new_pb.extend_from_slice(&br.boundaries);
    // 同 new_fb：段扫描已在受影响包络内重记了边界（含 stop 行），只平移严格大于
    // stop_old 的尾边界，避免与段扫描边界重复。
    new_pb.extend(
        prev_pb
            .iter()
            .copied()
            .filter(|&b| b > stop_old_b)
            .map(|b| (b as i64 + delta).max(0) as usize)
            .filter(|&b| b < row_count),
    );

    (folds, pairs, new_fb, new_pb)
}

/// 单轮「折叠候选 + 括号配对」重算的**统一计算入口**（供后台刷新与测试同步落地共用，
/// 避免两处口径漂移）：返回 `(folds, pairs, fold_boundaries, bracket_boundaries,
/// was_incremental)`。`was_incremental` = 本次是否走了增量路径（受影响段局部重扫 +
/// 尾部平移），`false` 表示整篇扫描兜底（多编辑段累积 / 无编辑描述 / 超行数清空）。
pub(crate) fn compute_fold_brackets<F>(
    line: F,
    row_count: usize,
    full_limit: usize,
    prev_folds: &[Fold],
    prev_pairs: &[BracketPair],
    prev_fb: &[usize],
    prev_pb: &[usize],
    pending: Option<(usize, usize, i64)>,
    multi: bool,
) -> (Vec<Fold>, Vec<BracketPair>, Vec<usize>, Vec<usize>, bool)
where
    F: Fn(usize) -> SharedString + Copy,
{
    let was_incremental = pending.is_some() && !multi && row_count <= full_limit;
    let (folds, pairs, fb, pb) =
        incremental_fold_brackets(line, row_count, full_limit, prev_folds, prev_pairs, prev_fb, prev_pb, pending, multi);
    (folds, pairs, fb, pb, was_incremental)
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

    // ------------------------------------------------------------------
    // 折叠候选纯函数（`scan_fold_candidates`）
    // ------------------------------------------------------------------

    #[test]
    fn fold_candidates_find_indented_blocks() {
        let lines = [
            "fn outer() {",
            "    fn inner() {",
            "        x();",
            "    }",
            "    y();",
            "}",
            "fn next() {}",
        ];
        let cands = scan_fold_candidates(line_fn(&lines), lines.len());
        // outer 块跨 0..5（藏 1..5），inner 块跨 1..3（藏 2）——两者并存（候选允许嵌套）。
        assert!(cands.contains(&Fold { start: 0, end: 5 }), "{cands:?}");
        assert!(cands.contains(&Fold { start: 1, end: 3 }), "{cands:?}");
        // 空行不干扰、无嵌套内容的单行块不产候选。
        let blank = ["fn a() {", "", "    x", "}"];
        assert_eq!(
            scan_fold_candidates(line_fn(&blank), blank.len()),
            vec![Fold { start: 0, end: 3 }]
        );
    }

    #[test]
    fn fold_candidates_identical_to_view_metric() {
        // 用 Tab 缩进回归：行首空白按**字节**计（与原 `view::leading_ws_len` 相同），
        // 保证候选集合与折叠展开/收拢语义不因口径漂移而变化。
        let lines = ["\tfn a() {", "\t\tx;", "\t}"];
        assert_eq!(
            scan_fold_candidates(line_fn(&lines), lines.len()),
            vec![Fold { start: 0, end: 2 }]
        );
    }

    // ------------------------------------------------------------------
    // 段扫描断点 + 增量重算 × 整篇扫描对拍（Wave2-S1）
    // ------------------------------------------------------------------

    /// 从某个「栈清空边界」以全新状态续扫，必须与从 0 整篇扫描在 `≥ 该行` 的
    /// fold 候选 / 边界上逐位一致——这是增量重算正确性的根基。
    #[test]
    fn segment_from_boundary_equals_full_from_that_line() {
        let lines = [
            "fn a() {",
            "    a1();",
            "}",
            "fn b() {",
            "    fn c() {",
            "        x();",
            "    }",
            "    b1();",
            "}",
            "fn d() {}",
            "fn e() {",
            "    y();",
            "}",
        ];
        let full = scan_fold_segment(line_fn(&lines), lines.len(), 0, None);
        assert!(full.boundaries.len() >= 2, "文本应产生多个边界 {full:?}");
        for &b in &full.boundaries {
            let seg = scan_fold_segment(line_fn(&lines), lines.len(), b, None);
            let expect_folds: Vec<Fold> =
                full.folds.iter().filter(|f| f.start >= b).copied().collect();
            assert_eq!(
                seg.folds, expect_folds,
                "边界 {b} 起段扫描的 fold 应与整篇一致"
            );
            let expect_bnd: Vec<usize> =
                full.boundaries.iter().copied().filter(|&x| x >= b).collect();
            assert_eq!(seg.boundaries, expect_bnd, "边界 {b} 起的边界列表应一致");
        }
    }

    /// 括号扫描的「栈空 + 不在块注释内」边界同理：从边界续扫 = 整篇在 `≥ 该行`
    /// 逐位一致（含块注释跨行状态）。
    #[test]
    fn bracket_segment_from_boundary_equals_full_from_that_line() {
        let lines = [
            "fn x() {",
            "    a();",
            "}",
            "/* { in comment", // 打开块注释
            "} */",            // 注释内闭括号应被忽略、不配对
            "fn y() {",
            "    b();",
            "}",
        ];
        let full = scan_bracket_segment(line_fn(&lines), lines.len(), 0, None);
        assert!(full.boundaries.len() >= 2, "应有多个括号边界 {full:?}");
        for &b in &full.boundaries {
            let seg = scan_bracket_segment(line_fn(&lines), lines.len(), b, None);
            let expect_pairs: Vec<BracketPair> =
                full.pairs.iter().filter(|p| p.open_line >= b).copied().collect();
            assert_eq!(seg.pairs, expect_pairs, "边界 {b} 起段扫描的配对应一致");
            let expect_bnd: Vec<usize> =
                full.boundaries.iter().copied().filter(|&x| x >= b).collect();
            assert_eq!(seg.boundaries, expect_bnd, "边界 {b} 起的边界列表应一致");
        }
    }

    /// 把文本切成（ropey 语义）行访问闭包：`split('\n')` 保留尾换行的幻影空行，
    /// 与 `Buffer::line_count` / `Rope::lines_len` 口径一致；闭包捕获 `&str`（Copy）。
    fn text_line_fn(text: &str) -> impl Fn(usize) -> SharedString + Copy + '_ {
        move |i: usize| {
            text.split('\n')
                .nth(i)
                .map(SharedString::from)
                .unwrap_or_else(|| SharedString::new_static(""))
        }
    }

    /// 对拍核心：`prev`（上次落地结果）经一次「受影响行段描述 (line_a,min_stop,delta)」
    /// 的增量重算，四份输出必须与对该时点文本的**整篇扫描**逐位一致（fold/配对按
    /// start / open_line 排序后比较——两者本就是同一候选集合，仅内部次序约定不同）。
    fn assert_incremental_matches_full(
        prev: &(Vec<Fold>, Vec<BracketPair>, Vec<usize>, Vec<usize>),
        text: &str,
        line_a: usize,
        min_stop: usize,
        delta: i64,
        multi: bool,
        full_limit: usize,
    ) -> (Vec<Fold>, Vec<BracketPair>, Vec<usize>, Vec<usize>) {
        let line = text_line_fn(text);
        let n = text.split('\n').count();
        let (nf, np, nfb, npb) = incremental_fold_brackets(
            line,
            n,
            full_limit,
            &prev.0,
            &prev.1,
            &prev.2,
            &prev.3,
            Some((line_a, min_stop, delta)),
            multi,
        );
        let (ef, ep, efb, epb) = full_fold_brackets(line, n);
        let mut nf_sorted = nf.clone();
        nf_sorted.sort_by_key(|f| f.start);
        let mut ef_sorted = ef.clone();
        ef_sorted.sort_by_key(|f| f.start);
        assert_eq!(nf_sorted, ef_sorted, "fold 候选与整篇扫描不一致（text={text:?}）");
        let mut np_sorted = np.clone();
        np_sorted.sort_by_key(|p| p.open_line);
        let mut ep_sorted = ep.clone();
        ep_sorted.sort_by_key(|p| p.open_line);
        assert_eq!(np_sorted, ep_sorted, "括号配对与整篇扫描不一致（text={text:?}）");
        assert_eq!(nfb, efb, "fold 边界与整篇扫描不一致（text={text:?}）");
        assert_eq!(npb, epb, "括号边界与整篇扫描不一致（text={text:?}）");
        (nf, np, nfb, npb)
    }

    /// 结构编辑序列上的对拍：每一步的增量重算输出 == 该时点整篇扫描（fold 候选 /
    /// 括号配对 / 边界四份全比），覆盖行内插换行、多行删除、注释内加花括号、
    /// 整函数结构重写、删整个内层块等编辑形态。
    #[test]
    fn incremental_edits_match_full_scan() {
        let src = "\
const S: &str = \"{ not paren\";
fn outer() {
    let x = {
        inner();
        more();
    };
    /* block { comment */
    z();
    // tail }
    {
        b1();
    }
    end();
}
fn after() {
    tail();
}";
        // 上次落地结果（取整篇扫描作为对拍基）。
        let line = text_line_fn(src);
        let n = src.split('\n').count();
        let prev = full_fold_brackets(line, n);
        let mut text = src.to_string();
        let mut state = prev;

        // (被替换的旧片段, 新片段, multi)
        let edits: Vec<(&str, &str, bool)> = vec![
            ("more();", "more();\n        deep();\n        deeper();", false), // 行内插 2 换行（净 +2，嵌套变深）
            ("        inner();\n", "", false),                                // 多行删除（净 −1）
            ("/* block { comment */", "/* block { extra { comment */", false), // 注释内加花括号（括号扫描忽略）
            (
                "fn after() {\n    tail();\n}",
                "fn after() {\n    if okay {\n        tail();\n    }\n}\nfn extra() {",
                false,
            ), // 整函数结构重写（净 +3）
            ("    {\n        b1();\n    }\n", "", false),                      // 删整个内层块（净 −3）
            ("    end();", "    end(); // { },", false),                       // 同行加括号字符（行数不变）
            ("ignore-me-2", "ignored", true), // multi → 整篇兜底（预期仍与整篇一致）
            ("fn outer() {\n    let x", "fn outer() {\n    fn branch() {\n        q();\n    }\n    let x", false), // 段首插入新函数（净 +2）
        ];
        for (i, (old_piece, new_piece, multi)) in edits.iter().enumerate() {
            if *old_piece == "ignore-me-2" {
                // multi 兜底：编辑描述不参与（引擎退整篇），这里仅验证结果=整篇。
                state = assert_incremental_matches_full(
                    &state, &text, 0, text.split('\n').count(), 0, true, usize::MAX,
                );
                continue;
            }
            let pos = text.find(old_piece).unwrap_or_else(|| panic!("edit {i}: 锚点 {old_piece:?} 未找到"));
            let end = pos + old_piece.len();
            let line_a = text[..pos].matches('\n').count();
            let line_b = text[..end].matches('\n').count();
            let insert_rows = new_piece.matches('\n').count() + 1;
            let removed_rows = line_b.saturating_sub(line_a) + 1;
            let min_stop = line_a + insert_rows;
            let delta = insert_rows as i64 - removed_rows as i64;
            text.replace_range(pos..end, new_piece);
            state = assert_incremental_matches_full(
                &state, &text, line_a, min_stop, delta, *multi, usize::MAX,
            );
        }
    }

    /// 增量前提缺失的兜底路径：`multi` / 无编辑描述 → 整篇扫描；超行数阈值 →
    /// 候选/配对/边界全部清空（按需折叠语义）。
    #[test]
    fn incremental_falls_back_to_full_or_by_need() {
        let src = "fn a() {\n    x();\n}\n";
        let line = text_line_fn(src);
        let prev = full_fold_brackets(line, src.split('\n').count());
        // multi=true → 整篇兜底（输出=整篇，边界更新）。
        let r = incremental_fold_brackets(
            line, src.split('\n').count(), usize::MAX,
            &prev.0, &prev.1, &prev.2, &prev.3, Some((0, 4, 0)), true,
        );
        let full = full_fold_brackets(line, src.split('\n').count());
        assert_eq!(r.0, full.0);
        assert_eq!(r.1, full.1);
        assert_eq!(r.2, full.2);
        assert_eq!(r.3, full.3);
        // edit=None → 整篇兜底。
        let r2 = incremental_fold_brackets(
            line, src.split('\n').count(), usize::MAX,
            &prev.0, &prev.1, &prev.2, &prev.3, None, false,
        );
        assert_eq!(r2.0, full.0);
        // row_count > full_limit → 全部清空（按需折叠/配对）。
        let r3 = incremental_fold_brackets(
            line, src.split('\n').count(), 2,
            &prev.0, &prev.1, &prev.2, &prev.3, Some((0, 4, 0)), false,
        );
        assert!(r3.0.is_empty() && r3.1.is_empty() && r3.2.is_empty() && r3.3.is_empty());
    }

    // ------------------------------------------------------------------
    // 性能基准（含 release 跑法）：整篇双扫（旧，每次结构编辑 O(全文)）vs 增量
    // （新，受影响段局部重扫 + 尾部平移）。
    // ------------------------------------------------------------------

    /// 合成大文档：N 个函数体（含嵌套块），模拟 16 万行级别源码结构。
    fn large_src(funcs: usize) -> String {
        let mut s = String::with_capacity(funcs * 48);
        for i in 0..funcs {
            s.push_str(&format!("fn f{i}() {{\n    let x = {{\n        inner_{i}();\n    }};\n    z_{i}();\n}}\n"));
        }
        s
    }

    /// release 跑法：`cargo test --release --bin gpui-editor fold:: -- --ignored --nocapture`。
    /// 测量同一份 16 万行文档上：
    /// - `before` = 整篇双扫（fold 候选 + 括号配对，即旧实现的每次结构编辑代价）；
    /// - `after`  = 文档中部一次结构编辑的**增量**重算（受影响段局部重扫 + 尾部平移）。
    #[test]
    #[ignore]
    fn bench_full_vs_incremental_scan() {
        let lines_target = 160_000usize;
        let funcs = lines_target / 6 + 10; // 每个函数 6 行（含最后一个换行的幻影空行）
        let src = large_src(funcs);
        let text: &str = &src;
        let line = text_line_fn(text);
        let n = text.split('\n').count();
        assert!(n >= lines_target, "n={n}");

        // 旧：整篇双扫（做 3 次取中位，模拟 ≥3 跑）。
        let mut t_full = Vec::new();
        for _ in 0..3 {
            let t0 = std::time::Instant::now();
            let (fl, pa, ..) = full_fold_brackets(line, n);
            std::hint::black_box(&fl);
            std::hint::black_box(&pa);
            t_full.push(t0.elapsed().as_secs_f64() * 1000.0);
        }
        // 以第一次全扫结果作为增量对拍基 + 基线。
        let (prev_folds, prev_pairs, prev_fb, prev_pb) = full_fold_brackets(line, n);

        // 新：文档中部一次结构编辑（函数 k 体内插入 2 行更深缩进 → 净 +2）的增量重算。
        let k = funcs / 2; // 中段
        let line_a = k * 6 + 2;
        let min_stop = line_a + 3;
        let delta = 2i64;
        let mut t_inc = Vec::new();
        for _ in 0..7 {
            let t0 = std::time::Instant::now();
            let (f, b, fb, pb) = incremental_fold_brackets(
                line, n + delta as usize, usize::MAX,
                &prev_folds, &prev_pairs, &prev_fb, &prev_pb,
                Some((line_a, min_stop, delta)), false,
            );
            std::hint::black_box(&f);
            std::hint::black_box(&b);
            std::hint::black_box(&fb);
            std::hint::black_box(&pb);
            t_inc.push(t0.elapsed().as_secs_f64() * 1000.0);
        }
        t_full.sort_by(|a, b| a.partial_cmp(b).unwrap());
        t_inc.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let med_full = t_full[t_full.len() / 2];
        let med_inc = t_inc[t_inc.len() / 2];
        eprintln!(
            "[BENCH fold/bracket] rows={n} folds={} pairs={} boundaries={}/{}",
            prev_folds.len(),
            prev_pairs.len(),
            prev_fb.len(),
            prev_pb.len()
        );
        eprintln!(
            "[BENCH] before(整篇双扫) min/med: {:.2}/{:.2} ms | after(增量) min/med: {:.2}/{:.2} ms | 加速 {:.0}x",
            t_full[0], med_full, t_inc[0], med_inc, med_full / med_inc.max(1e-6),
        );
    }
}
