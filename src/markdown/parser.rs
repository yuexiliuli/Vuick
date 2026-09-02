//! 手写 Markdown 行扫描器：把源码**逐行**转成渲染行（[`Row`]）。
//!
//! 设计目标：**行级渲染** —— 一个代码围栏就是 N 个等高的普通行，
//! 天然适合行级虚拟化；再大的代码块 / 长文档也不会出现“单个超大渲染块”，
//! 从根上避免滚动 / 渲染卡顿。
//!
//! 块级表格（GFM pipe table）作为**单个渲染行**消费多行源码：表头 + 分隔行 +
//! 若干数据行合并成 [`RowKind::Table`]，渲染层画成网格（表格通常不大，
//! 单个块不会破坏行级虚拟化的性能前提）。

use std::ops::Range;
use std::sync::OnceLock;

use gpui::SharedString;
use regex::Regex;

/// 渲染行类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    /// 普通段落
    Paragraph,
    /// 标题（1..=6 级）
    Heading(u8),
    /// 代码块内的行
    Code,
    /// 引述
    Quote,
    /// 列表项
    Bullet,
    /// 水平分隔线
    Rule,
    /// GFM 表格（一个行 = 整个表格，结构见 [`Row::table`]）
    Table,
    /// 空行（作为行间留白）
    Blank,
}

/// 行内样式段类型（当前 v1 渲染按行统一着色，暂不逐段消费，预留给后续增强）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunKind {
    Bold,
    Italic,
    InlineCode,
    Strikethrough,
    /// 行内链接 `[text](url)`：渲染为链接文字（去掉 URL 与括号）。
    Link,
    /// 行内图片 `![alt](url)`：渲染为 alt 文字。
    Image,
}

/// 表格列对齐（从分隔行 `:---:` / `---:` 解析）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableAlign {
    Left,
    Center,
    Right,
}

/// GFM 表格结构：表头 + 数据行 + 每列对齐。
#[derive(Debug, Clone, Default)]
pub struct TableData {
    /// 表头单元格文本（已做行内记号清理）。
    pub header: Vec<SharedString>,
    /// 数据行；每行与表头同列数（不足补空串）。
    pub rows: Vec<Vec<SharedString>>,
    /// 每列对齐（与表头列数对齐）。
    pub aligns: Vec<TableAlign>,
}

/// 一个渲染行：文本 + 类型 + 可选的行内样式段 + 列表前缀。
#[derive(Debug, Clone)]
pub struct Row {
    /// 渲染文本（已去除常见内联记号：`**` / `` ` `` 等）
    pub text: SharedString,
    pub kind: RowKind,
    /// 行内样式段（字节区间 → 类型）
    #[allow(dead_code)]
    pub runs: Vec<(Range<usize>, RunKind)>,
    /// 列表前缀，如 "•" 或有序 "1."
    pub prefix: Option<SharedString>,
    /// 表格结构（kind == [Table] 时 Some）
    pub table: Option<TableData>,
}

impl Row {
    fn new(
        text: &str,
        kind: RowKind,
        runs: Vec<(Range<usize>, RunKind)>,
        prefix: Option<SharedString>,
    ) -> Self {
        Self {
            text: text.into(),
            kind,
            runs,
            prefix,
            table: None,
        }
    }

    /// 表格行：整个表格折叠成一个 [`RowKind::Table`] 渲染行。
    fn with_table(table: TableData) -> Self {
        Self {
            text: SharedString::from(""),
            kind: RowKind::Table,
            runs: Vec::new(),
            prefix: None,
            table: Some(table),
        }
    }
}

fn inline_styles_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"`(?<code>[^`\n]+)`|\*\*(?<bold>[^*\n]+)\*\*|~~(?<strike>[^~\n]+)~~|\*(?<em>[^*\n]+)\*|!\[(?<img>[^\]]*)\]\([^)\n]*\)|\[(?<link>[^\]]+)\]\([^)\n]*\)",
        )
        .unwrap()
    })
}

/// 清理内联记号并收集行内样式段：
/// `` `代码` `` → 代码；`**粗体**` → 粗体；`*斜体*` → 斜体；
/// `~~删除~~` → 删除线；`[文字](url)` → 链接文字；`![alt](url)` → alt。
fn inline_style(src: &str) -> (SharedString, Vec<(Range<usize>, RunKind)>) {
    let mut out = String::with_capacity(src.len());
    let mut runs = Vec::new();
    let mut last = 0usize;
    for cap in inline_styles_re().captures_iter(src) {
        let m = cap.get(0).expect("整体匹配必然存在");
        let span = m.range();
        let (inner, kind) = if let Some(c) = cap.name("code") {
            (c.as_str(), RunKind::InlineCode)
        } else if let Some(c) = cap.name("bold") {
            (c.as_str(), RunKind::Bold)
        } else if let Some(c) = cap.name("strike") {
            (c.as_str(), RunKind::Strikethrough)
        } else if let Some(c) = cap.name("em") {
            (c.as_str(), RunKind::Italic)
        } else if let Some(c) = cap.name("img") {
            (c.as_str(), RunKind::Image)
        } else if let Some(c) = cap.name("link") {
            (c.as_str(), RunKind::Link)
        } else {
            unreachable!("inline_styles_re 的匹配必然落入某个命名捕获组")
        };
        out.push_str(&src[last..span.start]);
        let start = out.len();
        out.push_str(inner);
        runs.push((start..out.len(), kind));
        last = span.end;
    }
    out.push_str(&src[last..]);
    (out.into(), runs)
}

/// 围栏检测：行首（允许缩进）是否有 ≥3 个 ` 或 ~。
fn fence_len(t: &str) -> Option<(char, usize)> {
    let c = t.chars().next()?;
    if c != '`' && c != '~' {
        return None;
    }
    let n = t.chars().take_while(|&ch| ch == c).count();
    if n < 3 {
        return None;
    }
    Some((c, n))
}

fn is_rule(t: &str) -> bool {
    let s = t.trim_end();
    if s.len() < 3 {
        return false;
    }
    let c = s.chars().next().unwrap();
    if c != '-' && c != '*' && c != '_' {
        return false;
    }
    s.chars().all(|ch| ch == c || ch.is_whitespace())
}

/// 识别列表项前缀；命中返回（前缀文本，剩余内容）。
/// 任务列表 `- [ ]` / `- [x]` / `- [X]` 优先于普通 `- ` / `* ` / `+ `。
fn list_prefix(t: &str) -> Option<(SharedString, &str)> {
    for bullet in ["- ", "* ", "+ "] {
        for (mark, glyph) in [("[ ] ", "☐"), ("[x] ", "☑"), ("[X] ", "☑")] {
            let marker = format!("{bullet}{mark}");
            if let Some(rest) = t.strip_prefix(&marker) {
                return Some((glyph.into(), rest));
            }
        }
    }
    if let Some(rest) = t
        .strip_prefix("- ")
        .or_else(|| t.strip_prefix("* "))
        .or_else(|| t.strip_prefix("+ "))
    {
        return Some(("•".into(), rest));
    }
    let digits = t.chars().take_while(|c| c.is_ascii_digit()).count();
    if (1..4).contains(&digits) {
        let after = &t[digits..];
        if let Some(rest) = after.strip_prefix(". ").or_else(|| after.strip_prefix(") ")) {
            return Some((format!("{}.", &t[..digits]).into(), rest));
        }
    }
    None
}

/// 按未转义的 `|` 拆表格行，`\|` / `\\` 先被解义，再 trim 每个单元格，
/// 并去掉首尾的空 cell（`| a | b |` → `["a", "b"]`）。
fn split_table_cells(line: &str) -> Vec<SharedString> {
    let mut cells: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut it = line.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\\' => {
                if let Some(&n) = it.peek() {
                    if n == '|' || n == '\\' {
                        cur.push(n);
                        it.next();
                    } else {
                        cur.push('\\');
                    }
                } else {
                    cur.push('\\');
                }
            }
            '|' => cells.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    cells.push(cur);
    if cells.first().is_some_and(|s| s.trim().is_empty()) {
        cells.remove(0);
    }
    if cells.last().is_some_and(|s| s.trim().is_empty()) {
        cells.pop();
    }
    cells.into_iter().map(|s| s.trim().to_string().into()).collect()
}

/// 分隔行每格必须是 `:?-+:?`（冒号 + 全短横），返回每列对齐；任一格不合规返回 None。
fn parse_delimiter(cells: &[SharedString]) -> Option<Vec<TableAlign>> {
    cells
        .iter()
        .map(|c| {
            let s = c.as_ref();
            let body = s.trim_start_matches(':').trim_end_matches(':');
            if body.is_empty() || !body.chars().all(|ch| ch == '-') {
                return None;
            }
            Some(match (s.starts_with(':'), s.ends_with(':')) {
                (true, true) => TableAlign::Center,
                (false, true) => TableAlign::Right,
                _ => TableAlign::Left,
            })
        })
        .collect()
}

/// 尝试把 `lines[idx..]` 解析为一张 GFM 表格。
/// 判据：当前行是含 `|` 的表头，下一行是列数一致的分隔行；随后消费
/// 所有非空且含 `|` 的行作为数据行。返回（表格数据，共消费行数）。
fn try_parse_table(lines: &[&str], idx: usize) -> Option<(TableData, usize)> {
    let header_line = lines[idx].trim();
    if !header_line.contains('|') {
        return None;
    }
    let header = split_table_cells(header_line);
    if header.len() < 2 {
        return None;
    }
    let delim_line = lines.get(idx + 1)?.trim();
    let delim = split_table_cells(delim_line);
    if delim.len() != header.len() {
        return None;
    }
    let aligns = parse_delimiter(&delim)?;

    let ncols = header.len();
    let mut rows: Vec<Vec<SharedString>> = Vec::new();
    let mut j = idx + 2;
    while j < lines.len() {
        let l = lines[j].trim();
        if l.is_empty() || !l.contains('|') {
            break;
        }
        let mut cells = split_table_cells(l);
        cells.resize(ncols, SharedString::from(""));
        rows.push(cells);
        j += 1;
    }
    Some((
        TableData {
            header: header.iter().map(|c| inline_strip(c.as_ref())).collect(),
            rows: rows
                .into_iter()
                .map(|r| r.into_iter().map(|c| inline_strip(c.as_ref())).collect())
                .collect(),
            aligns,
        },
        j - idx,
    ))
}

/// 只取行内清理后的文本（表格单元格不需要逐段样式）。
fn inline_strip(s: &str) -> SharedString {
    inline_style(s).0
}

/// 把 Markdown 源码解析成渲染行（每行一行，含行内清理）。
pub fn parse_to_rows(src: &str) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut in_fence: Option<char> = None;
    let lines: Vec<&str> = src.split('\n').collect();
    let mut idx = 0;
    while idx < lines.len() {
        let line = lines[idx].trim_end_matches('\r');
        let t = line.trim_start();
        if let Some(fc) = in_fence {
            // 围栏内：碰到同字符闭合行则结束
            if let Some((c, _)) = fence_len(t) {
                if c == fc {
                    in_fence = None;
                    idx += 1;
                    continue;
                }
            }
            rows.push(Row::new(line, RowKind::Code, Vec::new(), None));
            idx += 1;
            continue;
        }
        if let Some((fc, _)) = fence_len(t) {
            in_fence = Some(fc);
            idx += 1;
            continue;
        }
        if t.is_empty() {
            rows.push(Row::new("", RowKind::Blank, Vec::new(), None));
            idx += 1;
            continue;
        }
        // 标题：`#{1,6} ` 后跟内容
        let hashes = t.chars().take_while(|&c| c == '#').count();
        if (1..=6).contains(&hashes) && t.len() > hashes {
            let (text, runs) = inline_style(t[hashes..].trim_start());
            rows.push(Row::new(
                text.as_ref(),
                RowKind::Heading(hashes as u8),
                runs,
                None,
            ));
            idx += 1;
            continue;
        }
        // 引述：`>` 开头
        if t.starts_with('>') {
            let rest = t.trim_start_matches('>').trim_start();
            let (text, runs) = inline_style(rest);
            rows.push(Row::new(text.as_ref(), RowKind::Quote, runs, None));
            idx += 1;
            continue;
        }
        // 列表项：`- ` / `* ` / `+ ` / `1. ` / 任务列表
        if let Some((prefix, content)) = list_prefix(t) {
            let (text, runs) = inline_style(content);
            rows.push(Row::new(
                text.as_ref(),
                RowKind::Bullet,
                runs,
                Some(prefix.clone()),
            ));
            idx += 1;
            continue;
        }
        // 水平分隔线
        if is_rule(t) {
            rows.push(Row::new("", RowKind::Rule, Vec::new(), None));
            idx += 1;
            continue;
        }
        // 表格：当前行是表头、下一行是分隔行（结构块优先于表格，故放在最后判定）
        if let Some((table, consumed)) = try_parse_table(&lines, idx) {
            rows.push(Row::with_table(table));
            idx += consumed;
            continue;
        }
        let (text, runs) = inline_style(line.trim());
        rows.push(Row::new(text.as_ref(), RowKind::Paragraph, runs, None));
        idx += 1;
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_headings_fences_lists() {
        let src = "# 一级标题\n```rs\nlet a = 1;\n```\n- item\n\n> quote\n";
        let rows = parse_to_rows(src);
        assert_eq!(rows[0].kind, RowKind::Heading(1));
        assert_eq!(rows[0].text.as_ref(), "一级标题");
        assert_eq!(rows[1].kind, RowKind::Code);
        assert_eq!(rows[1].text.as_ref(), "let a = 1;");
        assert_eq!(rows[2].kind, RowKind::Bullet);
        assert_eq!(rows[2].text.as_ref(), "item");
        assert_eq!(
            rows[2].prefix.as_ref().map(|s| s.as_ref()),
            Some("•"),
            "无序列表应带前缀"
        );
        assert_eq!(rows[4].kind, RowKind::Quote);
    }

    #[test]
    fn strips_inline_markers() {
        let rows = parse_to_rows("**加粗** 和 `code` 和 *斜体*\n");
        let r = &rows[0];
        assert_eq!(r.text.as_ref(), "加粗 和 code 和 斜体");
        assert_eq!(r.runs.len(), 3);
        assert!(matches!(
            r.runs[0].1,
            RunKind::Bold
        ));
    }

    #[test]
    fn strips_strikethrough_links_images() {
        let rows = parse_to_rows("a ~~删除~~ [链接文字](https://x.y) ![alt text](img.png)\n");
        let r = &rows[0];
        assert_eq!(r.text.as_ref(), "a 删除 链接文字 alt text");
        assert_eq!(r.runs.len(), 3);
        assert!(matches!(r.runs[0].1, RunKind::Strikethrough));
        assert!(matches!(r.runs[1].1, RunKind::Link));
        assert!(matches!(r.runs[2].1, RunKind::Image));
    }

    #[test]
    fn parses_task_lists() {
        let rows = parse_to_rows("- [x] 已完成\n- [ ] 未完成\n");
        let done = &rows[0];
        let todo = &rows[1];
        assert_eq!(done.kind, RowKind::Bullet);
        assert_eq!(
            done.prefix.as_ref().map(|s| s.as_ref()),
            Some("☑"),
            "勾选任务列表应带 ☑"
        );
        assert_eq!(done.text.as_ref(), "已完成");
        assert_eq!(
            todo.prefix.as_ref().map(|s| s.as_ref()),
            Some("☐"),
            "未勾选任务列表应带 ☐"
        );
    }

    #[test]
    fn parses_gfm_pipe_table() {
        let src = "| 型号 | 尺寸 |\n| :--- | ---: |\n| A | 10 |\n| B | 20 |\n";
        let rows = parse_to_rows(src);
        let table = rows[0].table.as_ref().expect("应为表格行");
        assert_eq!(rows.len(), 2, "表格折叠成 1 行 + 末尾空行");
        assert_eq!(table.header.len(), 2);
        assert_eq!(table.header[0].as_ref(), "型号");
        assert_eq!(table.rows.len(), 2);
        assert_eq!(table.rows[0][1].as_ref(), "10");
        assert_eq!(table.aligns, vec![TableAlign::Left, TableAlign::Right]);
    }

    #[test]
    fn table_supports_no_outer_pipes_and_escaped_pipe() {
        let src = "a | b\n--- | ---\n1 | x\\|y\n";
        let rows = parse_to_rows(src);
        let table = rows[0].table.as_ref().expect("应为表格行");
        assert_eq!(table.header[0].as_ref(), "a");
        assert_eq!(table.rows[0][1].as_ref(), "x|y", "\\| 应解义为字面竖线");
    }

    #[test]
    fn pipe_without_delimiter_stays_paragraph() {
        // 只有 `|` 而没有分隔行 → 不是表格，保持普通段落
        let rows = parse_to_rows("just | a line\nnext\n");
        assert_eq!(rows.len(), 3, "两段 + 末尾空行");
        assert_eq!(rows[0].kind, RowKind::Paragraph);
        assert_eq!(rows[0].text.as_ref(), "just | a line");
        assert_eq!(rows[1].kind, RowKind::Paragraph);
    }

    #[test]
    fn table_breaks_at_blank_or_non_pipe_line() {
        let src = "| a | b |\n|---|---|\n| 1 | 2 |\n\n后文\n| 3 | 4 |\n|---|---|\n";
        let rows = parse_to_rows(src);
        assert_eq!(rows[0].kind, RowKind::Table);
        assert_eq!(rows[1].kind, RowKind::Blank, "表格在空行处断开");
        assert_eq!(rows[2].kind, RowKind::Paragraph, "空行后普通段落");
        assert_eq!(rows[3].kind, RowKind::Table, "新表格再次识别");
    }

    #[test]
    fn delimiter_row_single_dash_is_valid() {
        // GFM 允许分隔格只有 1 个短横（`-`）
        let src = "姓名|年龄\n-|-\n张三|30\n";
        let rows = parse_to_rows(src);
        assert_eq!(rows[0].kind, RowKind::Table);
    }
}
