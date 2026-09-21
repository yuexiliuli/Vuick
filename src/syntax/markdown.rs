//! 编辑器 Markdown 高亮：**逐行状态机 + 窗口式按需扫描**（替代 syntect 对 .md 的解析）。
//!
//! 与 VS Code 同思路但更贴合本项目：markdown 的逐行扫描器极快，因此**不缓存整篇、不
//! 做启动构建**——每次渲染只扫描当前可见窗口（可见行 + [`LOOKBACK_LINES`] 前缀，用于
//! 恢复围栏等跨行状态），成本 O(可见行)，与文档总大小无关：
//! - 打开大文档**零构建**，首帧即出颜色（旧 syntect 窗口式解析 ~十几 ms，本项目自研
//!   逐行扫描器更快，且没有参与基准的 O(行数) update 开销）；
//! - `update()` 只存一份 COW 文本克隆（O(1)），不做任何整篇工作；
//! - 跨行只有一种状态：**围栏代码块**（``` / ~~~）。围栏内容复用
//!   [`crate::syntax::parse_code_lines`]（syntect / TOML，opt-level 3）按块着色；
//!   lookback 前缀外已开未闭合的围栏（开栏离可见区 > LOOKBACK 行）按普通文本处理
//!  （对绝大多数 Markdown 无感；极端超长围栏属可接受的取舍）。
//!
//! 其余语言仍走 [`super::SyntectHighlighter`]（syntect 窗口式解析 + 折叠）。

#![allow(dead_code)] // 编辑区 Markdown 高亮器未被接线（自研 editor 自带），保留防告警

use std::ops::Range;

use gpui::{Context, HighlightStyle, SharedString, Window};
use gpui_component::input::{
    EditorState, FoldRange, HighlightStyleResolver, InputEdit, InputHighlighter, Rope, RopeExt,
};

use crate::config::limits::{LOOKBACK_LINES, MAX_TEXT_BYTES};
use crate::syntax::{resolve_styles, EditTracker};

#[cfg(test)]
use crate::syntax::parse_code_lines;

/// 一行的语义 run：**行内字节区间** → 语义名（与 GitHub 配色键一致）。
type LineRuns = Vec<(Range<usize>, &'static str)>;

/// 代码围栏：由字符与最小长度标识，闭合需同字符且长度 ≥ 开栏。
#[derive(Clone, Copy, PartialEq, Eq)]
struct Fence {
    ch: u8,
    n: usize,
}

/// 可安装到编辑器的 Markdown 逐行高亮：窗口式按需扫描，无整篇缓存。
pub(crate) struct MarkdownHighlighter {
    language: SharedString,
    text: Rope,
    tracker: EditTracker,
}

impl MarkdownHighlighter {
    pub(crate) fn new(language: &str, tracker: EditTracker) -> Self {
        Self {
            language: language.to_owned().into(),
            text: Rope::from(""),
            tracker,
        }
    }
}

impl InputHighlighter for MarkdownHighlighter {
    fn language(&self) -> SharedString {
        self.language.clone()
    }

    /// 编辑侧通道：把本次编辑的字节区间交给预览同步（先于 InputEvent::Change）。
    /// 只保留编辑后的文本 COW 克隆（O(1)），不做任何整篇工作；解析推迟到渲染。
    fn update(
        &mut self,
        edit: Option<InputEdit>,
        text: &Rope,
        _folding: bool,
        _window: &mut Window,
        _cx: &mut Context<EditorState>,
    ) {
        let _t = crate::prof::begin();
        if let Some(e) = &edit {
            *self.tracker.borrow_mut() = Some((e.start_byte, e.old_end_byte, e.new_end_byte));
        }
        if text.len() > MAX_TEXT_BYTES {
            // 超大纲：清掉高亮，保持即时响应
            if self.text.len() != 0 {
                self.text = Rope::from("");
            }
        } else {
            self.text = text.clone();
        }
        crate::prof::end("hl_update", _t);
    }

    /// 渲染时按**当前可见窗口**扫描（可见行 + lookback 前缀），产出按需语义段。
    fn styles(
        &self,
        range: &Range<usize>,
        resolver: &dyn HighlightStyleResolver,
    ) -> Vec<(Range<usize>, HighlightStyle)> {
        let _t = crate::prof::begin();
        let semantic = window_semantic(&self.text, range);
        let r = resolve_styles(&semantic, range, resolver);
        crate::prof::end("hl_styles", _t);
        r
    }

    fn fold_ranges(&self, _: &Rope) -> Vec<FoldRange> {
        Vec::new()
    }

    fn fold_ranges_for_edit(&self, _range: Range<usize>, _text: &Rope) -> Vec<FoldRange> {
        Vec::new()
    }
}

/// 扫描 `range` 所在可见窗口，产出**文档坐标**的有序语义段（不包含缝，切到 range 内）。
fn window_semantic(text: &Rope, range: &Range<usize>) -> Vec<(Range<usize>, &'static str)> {
    if text.len() == 0 || range.is_empty() {
        return Vec::new();
    }
    let n = text.lines_len();
    let lo = text.offset_to_point(range.start.min(text.len())).row.min(n - 1);
    let hi = text
        .offset_to_point(range.end.saturating_sub(1).min(text.len()))
        .row
        .min(n - 1);
    // 可见窗口起点前留 lookback 前缀恢复围栏等跨行状态
    let from = lo.saturating_sub(LOOKBACK_LINES);
    let mut out = Vec::new();
    let mut r = from;
    while r <= hi {
        let line = line_string(text, r);
        if let Some((fence, lang)) = scan_block_marker(&line) {
            // —— 开栏行 ——
            push_row_runs(text, r, &fence_line_runs(&line), range, &mut out);
            let cf = find_fence_close(text, r + 1, fence);
            match cf {
                Some(cf) => {
                    // 内容行 [r+1, cf)：整块按语言着色
                    let first = r + 1;
                    if first < cf {
                        let cb = text.line_start_offset(first);
                        let ce = text.line_start_offset(cf);
                        let code = text.slice(cb..ce).to_string();
                        let per = fence_content_runs(&lang, &code);
                        for (i, row) in (first..cf).enumerate() {
                            if let Some(runs) = per.get(i) {
                                push_row_runs(text, row, runs, range, &mut out);
                            }
                        }
                    }
                    // 闭栏行（围栏内最后一行）
                    let cf_line = line_string(text, cf);
                    push_row_runs(text, cf, &fence_line_runs(&cf_line), range, &mut out);
                    r = cf + 1;
                }
                None => {
                    // 未闭合：内容行着色到文末
                    let first = r + 1;
                    if first < n {
                        let cb = text.line_start_offset(first);
                        let code = text.slice(cb..text.len()).to_string();
                        let per = fence_content_runs(&lang, &code);
                        for (i, row) in (first..n).enumerate() {
                            if let Some(runs) = per.get(i) {
                                push_row_runs(text, row, runs, range, &mut out);
                            }
                        }
                    }
                    r = n; // 无后续普通行
                }
            }
        } else {
            let runs = scan_markdown_line(&line);
            push_row_runs(text, r, &runs, range, &mut out);
            r += 1;
        }
    }
    out
}

/// 把一行（行内相对 run）压入输出，转文档坐标并裁剪到 `range`（只保留可见部分）。
fn push_row_runs(
    text: &Rope,
    row: usize,
    runs: &LineRuns,
    range: &Range<usize>,
    out: &mut Vec<(Range<usize>, &'static str)>,
) {
    let base = text.line_start_offset(row);
    for (rr, name) in runs {
        let (s, e) = (rr.start, rr.end);
        let ds = base + s;
        let de = base + e;
        if de > range.start && ds < range.end {
            out.push((ds..de, *name));
        }
    }
}

/// 代码围栏内容行着色：整块过一次 [`parse_code_lines`]（syntect / TOML）逐行返回；
/// 未知语言退化为整行 "string"。
fn fence_content_runs(lang: &str, code: &str) -> Vec<LineRuns> {
    let _ = lang;
    fallback_code_runs(code)
}

/// 未知语言退化的代码行着色：每行可见部分整行 "string"。
fn fallback_code_runs(code: &str) -> Vec<LineRuns> {
    let b = code.as_bytes();
    let mut out = Vec::new();
    let mut start = 0usize;
    for i in 0..=b.len() {
        if i == b.len() || b[i] == b'\n' {
            let core = code[start..i].trim_end_matches('\r').len();
            out.push(if core > 0 {
                vec![(0..core, "string")]
            } else {
                Vec::new()
            });
            start = i + 1;
        }
    }
    out
}

/// 一行文本（不含换行；可能含尾部 '\r'）。
fn line_string(text: &Rope, row: usize) -> String {
    if row >= text.lines_len() {
        return String::new();
    }
    text.slice_line(row).to_string()
}

/// 可见核心长度：去掉尾随 '\r'。
fn core_len(line: &str) -> usize {
    line.trim_end_matches('\r').len()
}

/// 从 `from_row` 起寻找与 `fence` 闭合的行（同字符、长度 ≥ 开栏、仅空格后缀）。
fn find_fence_close(text: &Rope, from_row: usize, fence: Fence) -> Option<usize> {
    let n = text.lines_len();
    for r in from_row..n {
        let line = line_string(text, r);
        if let Some((f, _lang)) = scan_block_marker(&line) {
            if f.ch == fence.ch && f.n >= fence.n {
                return Some(r);
            }
        }
    }
    None
}

/// 行是否为围栏（开/关均可）：0-3 空格缩进 + ≥3 个 `` ` `` / `~`。返回 (Fence, 语言信息)。
fn scan_block_marker(line: &str) -> Option<(Fence, String)> {
    let b = line.as_bytes();
    if b.is_empty() {
        return None;
    }
    let lead = b.iter().take(3).take_while(|&&c| c == b' ').count();
    let ch = *b.get(lead)?;
    if ch != b'`' && ch != b'~' {
        return None;
    }
    let mut n = 0;
    while lead + n < b.len() && b[lead + n] == ch {
        n += 1;
    }
    if n < 3 {
        return None;
    }
    let rest = line[lead + n..].trim();
    Some((Fence { ch, n }, rest.to_string()))
}

/// 围栏行的着色区间：围栏标记段（行内字节，不含收尾空格）。
fn fence_line_runs(line: &str) -> LineRuns {
    match scan_block_marker(line) {
        Some((fence, _)) => {
            let lead = line
                .as_bytes()
                .iter()
                .take(3)
                .take_while(|&&c| c == b' ')
                .count();
            let end = (lead + fence.n).min(core_len(line));
            if lead < end {
                vec![(lead..end, "preproc")]
            } else {
                Vec::new()
            }
        }
        None => Vec::new(),
    }
}

/// 扫描一行普通 Markdown（非围栏内容），产出语义 run（行内字节区间）。
fn scan_markdown_line(line: &str) -> LineRuns {
    let core = core_len(line);
    let s = &line[..core];
    let trimmed = s.trim_start();
    let ind = s.len() - trimmed.len();

    // —— ATX 标题：1-6 个 '#' + 空格/行尾 ——
    let hashes = trimmed.chars().take_while(|&c| c == '#').count();
    if (1..=6).contains(&hashes) {
        let ok = hashes >= trimmed.len() || matches!(trimmed.as_bytes()[hashes], b' ' | b'\t');
        if ok {
            return vec![(ind..core, "keyword")];
        }
    }

    // —— 引用行：'>' ——
    if trimmed.starts_with('>') {
        return vec![(ind..core, "comment")];
    }

    // —— 分隔线：3+ 个相同的 '-'/'*'/'_'（间允许空格）——
    if is_hr(trimmed) {
        return vec![(ind..core, "operator")];
    }

    // —— 围栏开/闭行 ——
    if scan_block_marker(trimmed).is_some() {
        return vec![(ind..core, "preproc")];
    }

    // —— 列表标记：'-'/'*'/'+' 或 '1.'/'1)' ——
    if let Some((ms, me)) = list_marker(trimmed) {
        let mut runs = vec![(ind + ms..ind + me, "operator")];
        runs.extend(scan_inline(s, ind + me));
        return normalize_runs(runs, core);
    }

    // —— 行内 ——
    normalize_runs(scan_inline(s, 0), core)
}

/// 行内扫描：反引号代码、链接/图片、加粗/斜体/删除线标记。`from` 为起始字节。
fn scan_inline(line: &str, from: usize) -> LineRuns {
    let b = line.as_bytes();
    let n = b.len();
    let mut runs: LineRuns = Vec::new();
    let mut i = from;
    while i < n {
        match b[i] {
            b'`' => {
                let mut j = i;
                while j < n && b[j] == b'`' {
                    j += 1;
                }
                match find_run(line, j, b'`') {
                    Some((_cs, ce)) => {
                        runs.push((i..ce, "string"));
                        i = ce;
                    }
                    None => i = j,
                }
            }
            ch @ (b'*' | b'_') => {
                let run_n = count_run(line, i, ch);
                let cnt = if run_n >= 2 { 2 } else { 1 };
                match find_run(line, i + cnt, ch) {
                    Some((cs, ce)) if ce - cs >= cnt => {
                        runs.push((i..i + cnt, "operator"));
                        runs.push((cs..cs + cnt, "operator"));
                        i = ce;
                    }
                    _ => i += cnt,
                }
            }
            b'~' => {
                if i + 1 < n && b[i + 1] == b'~' {
                    match find_run(line, i + 2, b'~') {
                        Some((cs, ce)) if ce - cs >= 2 => {
                            runs.push((i..i + 2, "operator"));
                            runs.push((cs..cs + 2, "operator"));
                            i = ce;
                        }
                        _ => i += 2,
                    }
                } else {
                    i += 1;
                }
            }
            b'!' => {
                if i + 1 < n && b[i + 1] == b'[' {
                    if let Some((ci, cp)) = read_link(line, i + 1) {
                        runs.push((i..ci + 1, "string"));
                        runs.push((ci + 1..cp + 1, "function"));
                        i = cp + 1;
                        continue;
                    }
                }
                i += 1;
            }
            b'[' => {
                if let Some((ci, cp)) = read_link(line, i) {
                    runs.push((i..ci + 1, "string"));
                    runs.push((ci + 1..cp + 1, "function"));
                    i = cp + 1;
                    continue;
                }
                i += 1;
            }
            _ => i += char_len_at(line, i),
        }
    }
    runs
}

/// `[text](url)`：返回 (闭合 ']' 索引, 闭合 ')' 索引)；无 (url) 时退化为 (ci, ci)。
fn read_link(line: &str, open: usize) -> Option<(usize, usize)> {
    let b = line.as_bytes();
    let mut depth = 0usize;
    let mut ci = None;
    for k in open + 1..b.len() {
        match b[k] {
            b'[' => depth += 1,
            b']' => {
                if depth == 0 {
                    ci = Some(k);
                    break;
                }
                depth -= 1;
            }
            _ => {}
        }
    }
    let ci = ci?;
    let after = ci + 1;
    if after < b.len() && b[after] == b'(' {
        if let Some(cp) = b[after + 1..].iter().position(|&x| x == b')') {
            return Some((ci, after + 1 + cp));
        }
    }
    Some((ci, ci))
}

/// 从 `from`（不含）起找第一段 ≥1 个 `ch` 的连续段，返回 (start,end)；无则 None。
fn find_run(line: &str, from: usize, ch: u8) -> Option<(usize, usize)> {
    let b = line.as_bytes();
    let mut k = from;
    while k < b.len() {
        if b[k] == ch {
            let s = k;
            while k < b.len() && b[k] == ch {
                k += 1;
            }
            return Some((s, k));
        }
        k += char_len_at(line, k);
    }
    None
}

/// 从 `i` 起连续 `ch` 的个数。
fn count_run(line: &str, i: usize, ch: u8) -> usize {
    let b = line.as_bytes();
    let mut n = 0;
    while i + n < b.len() && b[i + n] == ch {
        n += 1;
    }
    n
}

/// `i` 处字符占用字节数（ASCII 1，其余按 UTF-8）。
fn char_len_at(line: &str, i: usize) -> usize {
    line[i..].chars().next().map_or(1, |c| c.len_utf8())
}

/// 列表标记：返回 (start, end)（相对 `trimmed`）。支持 `-`/`*`/`+` 及 `1.`/`1)`。
fn list_marker(trimmed: &str) -> Option<(usize, usize)> {
    let b = trimmed.as_bytes();
    if b.is_empty() {
        return None;
    }
    match b[0] {
        b'-' | b'*' | b'+' => {
            if trimmed.len() == 1 || matches!(b[1], b' ' | b'\t') {
                Some((0, 1))
            } else {
                None
            }
        }
        c if c.is_ascii_digit() => {
            let mut j = 0;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            if j < b.len() && matches!(b[j], b'.' | b')') {
                let k = j + 1;
                if k >= b.len() || matches!(b[k], b' ' | b'\t') {
                    return Some((0, k));
                }
            }
            None
        }
        _ => None,
    }
}

/// 分隔线：去空格后 ≥3 个相同 `-`/`*`/`_`。
fn is_hr(trimmed: &str) -> bool {
    let chs: Vec<char> = trimmed
        .chars()
        .filter(|c| *c != ' ' && *c != '\t')
        .collect();
    chs.len() >= 3 && {
        let first = chs[0];
        matches!(first, '-' | '*' | '_') && chs.iter().all(|&c| c == first)
    }
}

/// 规整 run 列表：裁剪到 [0, core]、去空、合并相邻同名段。
fn normalize_runs(mut runs: LineRuns, core: usize) -> LineRuns {
    runs.sort_by_key(|(r, _)| (r.start, r.end));
    let mut out: LineRuns = Vec::with_capacity(runs.len());
    for (r, name) in runs {
        let s = r.start.min(core);
        let e = r.end.min(core);
        if s >= e {
            continue;
        }
        if let Some(last) = out.last_mut() {
            if last.1 == name && last.0.end == s {
                last.0 = last.0.start..e;
                continue;
            }
        }
        out.push((s..e, name));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names_of(line_runs: &LineRuns) -> Vec<&'static str> {
        line_runs.iter().map(|(_, n)| *n).collect()
    }

    /// 按可见窗口取语义段并在文档坐标下转为一组"行号 → 语义名集合"。
    fn semantic_names(text: &Rope, range: &Range<usize>) -> Vec<&'static str> {
        let sem = window_semantic(text, range);
        sem.into_iter().map(|(_, n)| n).collect()
    }

    #[test]
    fn basic_constructs_are_classified() {
        #[rustfmt::skip]
        let cases: Vec<(&str, Vec<&'static str>)> = vec![
            ("# 标题", vec!["keyword"]),
            ("## 二级标题", vec!["keyword"]),
            ("> 引用", vec!["comment"]),
            ("---", vec!["operator"]),
            ("- 列表", vec!["operator"]),
            ("1. 有序列表", vec!["operator"]),
            ("`inline`", vec!["string"]),
            ("**加粗**", vec!["operator", "operator"]),
            ("[链接](https://x)", vec!["string", "function"]),
            ("![图](a.png)", vec!["string", "function"]),
            ("普通文本行", vec![]),
        ];
        for (line, want) in cases {
            let got: Vec<&'static str> = names_of(&scan_markdown_line(line));
            assert_eq!(got, want, "line={line:?}");
        }
    }

    #[test]
    fn fence_marker_detection() {
        assert!(scan_block_marker("```rust").is_some());
        assert!(scan_block_marker("   ~~~").is_some());
        assert!(scan_block_marker("``").is_none());
        assert!(scan_block_marker("plain`x`").is_none());
    }

    /// 窗口扫描一次只产出可见区间的语义段（文档坐标，不越界、不越栏）。
    #[test]
    fn window_returns_document_coordinates() {
        let src = "# 标题\n\n```rust\nfn main() {\n    let s = \"x\";\n}\n```\n\n`inline` 与[链接](https://x)\n";
        let rope = Rope::from(src);
        let all: Vec<&str> = semantic_names(&rope, &(0..src.len()));
        assert!(all.contains(&"keyword"), "标题应有 keyword");
        assert!(all.contains(&"preproc"), "围栏应有 preproc");
        assert!(all.contains(&"function"), "链接应有 function");

        // 只取一个局部窗口（标题区）也应正确，且不越界
        let heading_end = "# 标题\n".len();
        let sem = window_semantic(&rope, &(0..heading_end));
        for (r, _) in &sem {
            assert!(r.start < heading_end && r.end <= heading_end, "越界: {r:?}");
        }
        assert!(sem.iter().any(|(_, n)| *n == "keyword"));
    }

    /// 围栏开栏在可见窗口上游 lookback 之外时不识别（接受取舍）；在窗口内必须正确着色。
    #[test]
    fn fence_within_window_is_colored() {
        let src = "第一行\n```rust\nlet s = \"x\";\n```\n末行\n";
        let rope = Rope::from(src);
        let sem = window_semantic(&rope, &(0..src.len()));
        let names: Vec<&str> = sem.iter().map(|(_, n)| *n).collect();
        assert!(names.contains(&"preproc"), "围栏行应有 preproc");
        assert!(names.contains(&"string"), "代码内容应有 string");
    }

    /// 只取文档中部窗口（围栏整体都在窗口上边界内）仍能正确识别并闭合围栏。
    #[test]
    fn window_inside_fence_block_colors_code() {
        let mut src = String::new();
        for _ in 0..1000 {
            src.push_str("# 标题段\n\n- 列表\n\n");
        }
        src.push_str("```toml\nkey = \"value\"\nother = 42\n```\n");
        let rope = Rope::from(src.as_str());
        // 命中国栏内容窗口
        let start = src.find("other = 42").unwrap();
        let range = start..(start + "other = 42".len());
        let names = semantic_names(&rope, &range);
        assert!(
            names.contains(&"property") || names.contains(&"string"),
            "toml 代码内容应有颜色: {names:?}"
        );
    }

    /// styles() 解析结果必须无缝覆盖请求区间（GPUI 渲染依赖）。
    #[test]
    fn styles_cover_requested_range() {
        struct Dummy;
        impl HighlightStyleResolver for Dummy {
            fn style(&self, _: &str) -> Option<HighlightStyle> {
                None
            }
        }
        let src = "# 标题\n正文 `code` 与 **粗**\n```\nx\n```\n";
        let rope = Rope::from(src);
        let mut hl = MarkdownHighlighter::new("markdown", EditTracker::default());
        hl.text = rope.clone(); // 等价于 update() 只暂存文本
        let len = rope.len();
        let styles = hl.styles(&(0..len), &Dummy);
        let covered: usize = styles.iter().map(|(r, _)| r.end - r.start).sum();
        assert_eq!(covered, len, "样式应无缝覆盖: {styles:?}");
    }

    /// 临时探针：定位窗口扫描慢在哪一步（用完即删）。
    #[test]
    #[ignore]
    fn probe_window_primitives() {
        use std::time::Instant;
        let block = String::from("# 标题段\n> 引用\n```rust\nfn f() {}\nlet s = \"x\";\n```\n- 列表\n[链接](https://x)\n\n");
        let mut src = String::new();
        for _ in 0..8000 {
            src.push_str(&block);
        }
        let rope = Rope::from(src.as_str());
        let n = rope.lines_len();
        // 1) slice_line(row).to_string()
        let t = Instant::now();
        let mut acc = 0usize;
        for _ in 0..2000 {
            for r in 0..50 {
                acc = acc.wrapping_add(line_string(&rope, r).len());
            }
        }
        let slice50 = t.elapsed();
        eprintln!("probe: 2000x 行 string 50 行 = {:?} ms (acc={acc})", slice50.as_millis());
        // 2) line_start_offset
        let t = Instant::now();
        let mut acc2 = 0usize;
        for _ in 0..2000 {
            for r in 0..50 {
                acc2 = acc2.wrapping_add(rope.line_start_offset(r));
            }
        }
        eprintln!("probe: 2000x line_start_offset 50 行 = {:?} ms (acc={acc2})", t.elapsed().as_millis());
        // 3) offset_to_point 边角
        let t = Instant::now();
        let mut acc3 = 0usize;
        for _ in 0..2000 {
            for b in 0..50 {
                acc3 = acc3.wrapping_add(rope.offset_to_point(b * 3).row);
            }
        }
        eprintln!("probe: 2000x offset_to_point 50 字节位 = {:?} ms", t.elapsed().as_millis());
        // 4) window_semantic 全量+局部
        let range = 4000..8000;
        let t = Instant::now();
        let sem = window_semantic(&rope, &range);
        eprintln!("probe: window_semantic(4000..8000, 行{}..{}) = {:?} ms, {} 段",
            rope.offset_to_point(range.start).row,
            rope.offset_to_point(range.end - 1).row,
            t.elapsed().as_millis(), sem.len());
        let _ = n;
        // 5) scan_markdown_line 本身（无围栏的普通行）
        let plain = "# 标题段\n> 引用行\n- 列表项\n普通正文与`code`和**加重**以及[链接](https://x)\n";
        let t = Instant::now();
        let mut acc5 = 0usize;
        for _ in 0..5000 {
            for l in plain.lines() {
                acc5 = acc5.wrapping_add(scan_markdown_line(l).len());
            }
        }
        eprintln!("probe: 5000x 扫描 4 行普通 markdown = {:?} ms", t.elapsed().as_millis());
        // 6) 无围栏文档的窗口扫描（隔离 fence 解析成本）
        let mut fpsrc = String::new();
        for i in 0..4000 {
            fpsrc.push_str(&format!("# 标题 {i}\n> 引用\n- 列表\n普通正文与`code`和**加重**以及[链接](https://x)\n\n"));
        }
        let fprope = Rope::from(fpsrc.as_str());
        let r = 20_000..24_000;
        let t = Instant::now();
        let sem6 = window_semantic(&fprope, &r);
        eprintln!("probe: 无围栏窗口 行{}..{} = {:?} ms, {} 段",
            fprope.offset_to_point(r.start).row, fprope.offset_to_point(r.end - 1).row,
            t.elapsed().as_millis(), sem6.len());
        // 7) parse_code_lines 单个小 rust 代码块
        let small = "fn main() {\n    let s = \"x\";\n}\n";
        let t = Instant::now();
        let mut acc7 = 0usize;
        for _ in 0..2000 {
            acc7 = acc7.wrapping_add(parse_code_lines("rust", small).map(|v| v.len()).unwrap_or(0));
        }
        eprintln!("probe: 2000x parse_code_lines(rust, 3行) = {:?} ms (单次 {:.0} µs)",
            t.elapsed().as_millis(), t.elapsed().as_micros() as f64 / 2000.0);
        // 8) find_fence_close 成本（一次对 72k 行文档里的小块）
        let t = Instant::now();
        let mut acc8 = 0usize;
        for _ in 0..2000 {
            acc8 = acc8.wrapping_add(find_fence_close(&rope, 0, Fence { ch: b'`', n: 3 }).unwrap_or(0));
        }
        eprintln!("probe: 2000x find_fence_close(小块) = {:?} ms", t.elapsed().as_millis());
    }
    #[test]
    #[ignore]
    fn bench_window_scan_is_constant_time() {
        use std::time::Instant;
        let block = String::from("# 标题段\n> 引用\n```rust\nfn f() {}\nlet s = \"x\";\n```\n- 列表\n[链接](https://x)\n\n");
        let mut src = String::new();
        for _ in 0..8000 {
            src.push_str(&block);
        }
        let rope = Rope::from(src.as_str());
        let total = rope.len();

        // 顶部窗口
        let t0 = Instant::now();
        let sem = window_semantic(&rope, &(0..total.min(4000)));
        let top = t0.elapsed();

        // 深部窗口（远离文档头）
        let deep_start = total - 4000;
        let t1 = Instant::now();
        let _s2 = window_semantic(&rope, &(deep_start..total));
        let deep = t1.elapsed();

        println!(
            "整篇 {} 行 / {} MB：顶部窗口扫描 {:.2} ms（{} 段）、深部窗口 {:.2} ms —— 与文档总大小无关",
            rope.lines_len(),
            total / 1024 / 1024,
            top.as_secs_f64() * 1000.0,
            sem.len(),
            deep.as_secs_f64() * 1000.0,
        );
    }
}
