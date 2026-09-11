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

use gpui::{Image, SharedString};
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
    /// 独立图片行（`![alt](src)`，src 在 [`Row::src`]）
    Image,
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

/// 渲染行的"额外载荷"——只对少数行非 None：
/// - [`RowKind::Code`] → [`RowPayload::Code`]
/// - [`RowKind::Image`] → [`RowPayload::Image`]
/// - [`RowKind::Table`] → [`RowPayload::Table`]
///
/// 装箱保存：普通段落/标题/引述/列表行绝大多数是文本 + 行内样式，
/// 若把这些罕见字段内联进 [`Row`]，每行都要背 72B+（16 万行 ≈ 10MB+）。
/// 装箱后每行只占一个指针（8B），罕见行多一次堆分配（本来就稀少，可忽略）。
#[derive(Debug, Clone)]
pub enum RowPayload {
    /// 代码块内所有源码行（逐行渲染成连续块）。
    Code {
        code_lines: Vec<SharedString>,
    },
    /// 独立图片行：源 + 固有尺寸 + 内存图数据（EPUB 不落盘场景）。
    Image {
        src: SharedString,
        img_w: Option<u32>,
        img_h: Option<u32>,
        img_data: Option<std::sync::Arc<Image>>,
    },
    /// 表格（一个渲染行 = 整张表）。
    Table(TableData),
}

/// 图片行载荷的只读视图（非图片行返回 None）。
#[derive(Debug, Clone, Copy)]
pub struct ImageInfo<'a> {
    pub src: &'a SharedString,
    pub img_w: Option<u32>,
    pub img_h: Option<u32>,
    pub img_data: Option<&'a std::sync::Arc<Image>>,
}

/// 一个渲染行：文本 + 类型 + 可选的行内样式段 + 列表前缀。
#[derive(Debug, Clone)]
pub struct Row {
    /// 渲染文本（已去除常见内联记号：`**` / `` ` `` 等）
    pub text: SharedString,
    pub kind: RowKind,
    /// 该渲染行覆盖的源码起始行号（0 基）。用于搜索跳转等「行→渲染行」映射。
    pub src_line: usize,
    /// 行内样式段（字节区间 → 类型）
    #[allow(dead_code)]
    pub runs: Vec<(Range<usize>, RunKind)>,
    /// 列表前缀，如 "•" 或有序 "1."
    pub prefix: Option<SharedString>,
    /// 稀有字段（代码块行 / 图片行 / 表格行）装箱保存，普通行不背上这些字段。
    pub payload: Option<Box<RowPayload>>,
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
            src_line: 0,
            runs,
            prefix,
            payload: None,
        }
    }

    /// 记录该渲染行对应的源码起始行号（解析时填充）。
    fn with_line(mut self, line: usize) -> Self {
        self.src_line = line;
        self
    }

    /// 代码块行：整个围栏折叠成一个 [RowKind::Code] 渲染行（逐行渲染成连续块）。
    fn code_block(lines: Vec<SharedString>) -> Self {
        let text = lines.first().cloned().unwrap_or_default();
        Self {
            text,
            kind: RowKind::Code,
            src_line: 0,
            runs: Vec::new(),
            prefix: None,
            payload: Some(Box::new(RowPayload::Code { code_lines: lines })),
        }
    }

    /// 表格行：整个表格折叠成一个 [`RowKind::Table`] 渲染行。
    fn with_table(table: TableData) -> Self {
        Self {
            text: SharedString::from(""),
            kind: RowKind::Table,
            src_line: 0,
            runs: Vec::new(),
            prefix: None,
            payload: Some(Box::new(RowPayload::Table(table))),
        }
    }

    /// 图片行：`![alt](src)`，文本为 alt（可空），src 另存。
    fn new_image(alt: &str, src: &str) -> Self {
        Self {
            text: alt.into(),
            kind: RowKind::Image,
            src_line: 0,
            runs: Vec::new(),
            prefix: None,
            payload: Some(Box::new(RowPayload::Image {
                src: src.into(),
                img_w: None,
                img_h: None,
                img_data: None,
            })),
        }
    }

    /// 代码块的所有源码行（非 [`RowKind::Code`] 行返回空切片）。
    pub fn code_lines(&self) -> &[SharedString] {
        match &self.payload {
            Some(p) => match &**p {
                RowPayload::Code { code_lines } => code_lines,
                _ => &[],
            },
            None => &[],
        }
    }

    /// 表格结构（仅 [`RowKind::Table`] 行）。
    pub fn table(&self) -> Option<&TableData> {
        match &self.payload {
            Some(p) => match &**p {
                RowPayload::Table(t) => Some(t),
                _ => None,
            },
            None => None,
        }
    }

    /// 图片行载荷（仅 [`RowKind::Image`] 行）。
    pub fn image(&self) -> Option<ImageInfo<'_>> {
        match &self.payload {
            Some(p) => match &**p {
                RowPayload::Image {
                    src,
                    img_w,
                    img_h,
                    img_data,
                } => Some(ImageInfo {
                    src,
                    img_w: *img_w,
                    img_h: *img_h,
                    img_data: img_data.as_ref(),
                }),
                _ => None,
            },
            None => None,
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

/// 识别独立图片行 `![alt](src)`（可带尾部空白）。返回 (alt, src)。
fn parse_image_line(t: &str) -> Option<(String, String)> {
    if !t.starts_with("![") || t.find("](").is_none() {
        return None;
    }
    // 剥掉首尾空白后必须整体匹配 `![...](...)`
    let s = t.trim();
    let end = s.find(']')?;
    if !s.starts_with("![") || end < 2 {
        return None;
    }
    let alt = &s[2..end];
    let rest = s[end + 1..].trim_start();
    if !rest.starts_with('(') {
        return None;
    }
    let close = rest.find(')')?;
    // 允许 `)` 前的路径内含空格；闭合后不应再有非空内容（允许尾部空白，已被 trim）
    let src = rest[1..close].trim();
    let tail = &rest[close + 1..];
    if src.is_empty() || (tail.trim() != s && tail.trim() != "") {
        // 非纯整行图片（后面还有文字）→ 不作为独立图片行
        if !tail.trim().is_empty() {
            return None;
        }
    }
    Some((alt.to_string(), src.to_string()))
}

/// 把 Markdown 源码解析成渲染行（每行一行，含行内清理）。
pub fn parse_to_rows(src: &str) -> Vec<Row> {
    let mut rows = Vec::new();
    let lines: Vec<&str> = src.split('\n').collect();
    let mut idx = 0;
    while idx < lines.len() {
        let row_start = idx;
        let line = lines[idx].trim_end_matches('\r');
        let t = line.trim_start();
        // 栅栏开头：收集到同字符闭合行之间的所有行，折叠成一个 Code 渲染行
        if let Some((fc, _)) = fence_len(t) {
            let mut code_lines = Vec::new();
            idx += 1;
            while idx < lines.len() {
                let l = lines[idx].trim_end_matches('\r');
                if let Some((c, _)) = fence_len(l.trim_start()) {
                    if c == fc {
                        idx += 1;
                        break;
                    }
                }
                code_lines.push(SharedString::from(l));
                idx += 1;
            }
            rows.push(Row::code_block(code_lines).with_line(row_start));
            continue;
        }
        if t.is_empty() {
            rows.push(Row::new("", RowKind::Blank, Vec::new(), None).with_line(row_start));
            idx += 1;
            continue;
        }
        // 标题：`#{1,6} ` 后跟内容
        let hashes = t.chars().take_while(|&c| c == '#').count();
        if (1..=6).contains(&hashes) && t.len() > hashes {
            let (text, runs) = inline_style(t[hashes..].trim_start());
            rows.push(
                Row::new(
                    text.as_ref(),
                    RowKind::Heading(hashes as u8),
                    runs,
                    None,
                )
                .with_line(row_start),
            );
            idx += 1;
            continue;
        }
        // 引述：`>` 开头
        if t.starts_with('>') {
            let rest = t.trim_start_matches('>').trim_start();
            let (text, runs) = inline_style(rest);
            rows.push(Row::new(text.as_ref(), RowKind::Quote, runs, None).with_line(row_start));
            idx += 1;
            continue;
        }
        // 列表项：`- ` / `* ` / `+ ` / `1. ` / 任务列表
        if let Some((prefix, content)) = list_prefix(t) {
            let (text, runs) = inline_style(content);
            rows.push(
                Row::new(
                    text.as_ref(),
                    RowKind::Bullet,
                    runs,
                    Some(prefix.clone()),
                )
                .with_line(row_start),
            );
            idx += 1;
            continue;
        }
        // 水平分隔线
        if is_rule(t) {
            rows.push(Row::new("", RowKind::Rule, Vec::new(), None).with_line(row_start));
            idx += 1;
            continue;
        }
        // 独立图片行：`![alt](src)` 整行占位
        if let Some((alt, src)) = parse_image_line(t) {
            rows.push(Row::new_image(&alt, &src).with_line(row_start));
            idx += 1;
            continue;
        }
        // 表格：当前行是表头、下一行是分隔行（结构块优先于表格，故放在最后判定）
        if let Some((table, consumed)) = try_parse_table(&lines, idx) {
            rows.push(Row::with_table(table).with_line(row_start));
            idx += consumed;
            continue;
        }
        let (text, runs) = inline_style(line.trim());
        rows.push(Row::new(text.as_ref(), RowKind::Paragraph, runs, None).with_line(row_start));
        idx += 1;
    }
    rows
}

/// 估算渲染行列表的**驻留内存**（近似）：返回（行数，约多少字节）。
///
/// 口径说明：
/// - 计入 `Row` 结构体本身（`size_of::<Row>()`）；
/// - 计入每个 `SharedString` 的**内容**（`len()` 字节）外加每块独立堆分配的
///   分配器开销（这里按 `+32 B/块` 粗估，实际 16~64 B 视分配器而定）；
/// - 计入内联 `runs` / `code_lines` / 表格 cell 的 Vec 容量与内容。
///
/// 不包含：`toc` / `code_scroll` / `table_scroll` 等辅助结构、文本整形/字体缓存。
/// 用于定位“解析结果把文档放大多少倍”（15 万行 ≈ 30MB+ 的量级来自此处）。
pub(crate) fn rows_memory_estimate(rows: &[Row]) -> (usize, usize) {
    /// 每个独立堆分配的近似分配器开销（head + 对齐 + 页头分摊）。
    const ALLOC_OVERHEAD: usize = 32;
    fn string_bytes(s: &SharedString) -> usize {
        ALLOC_OVERHEAD + s.len()
    }
    fn string_vec_bytes(v: &[SharedString]) -> usize {
        v.len() * std::mem::size_of::<SharedString>()
            + v.iter().map(string_bytes).sum::<usize>()
    }

    let mut bytes = 0usize;
    for r in rows {
        bytes += std::mem::size_of::<Row>();
        bytes += string_bytes(&r.text);
        if let Some(p) = &r.prefix {
            bytes += string_bytes(p);
        }
        bytes += r.runs.capacity() * std::mem::size_of::<(Range<usize>, RunKind)>();
        bytes += string_vec_bytes(r.code_lines());
        if let Some(t) = r.table() {
            bytes += std::mem::size_of::<TableData>();
            for h in &t.header {
                bytes += string_bytes(h);
            }
            for row in &t.rows {
                bytes += string_vec_bytes(row);
            }
            bytes += t.aligns.capacity() * std::mem::size_of::<TableAlign>();
        }
        if let Some(img) = r.image() {
            bytes += string_bytes(img.src);
            if img.img_data.is_some() {
                bytes += std::mem::size_of::<std::sync::Arc<Image>>();
            }
        }
    }
    (rows.len(), bytes)
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
        let table = rows[0].table().expect("应为表格行");
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
        let table = rows[0].table().expect("应为表格行");
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

    #[test]
    fn full_line_image_becomes_image_row() {
        let src = "前文\n\n![插图](images/a.jpeg)\n\n后文\n";
        let rows = parse_to_rows(src);
        let img = rows.iter().find(|r| r.kind == RowKind::Image).expect("应有图片行");
        assert_eq!(img.text.as_ref(), "插图", "图片行文本应为 alt");
        assert_eq!(
            img.image().map(|i| i.src.as_ref()),
            Some("images/a.jpeg"),
            "图片行 src 应保留"
        );
        // 非整行的行内图片仍是段落
        let src2 = "文字 ![内嵌](x.png) 尾部\n";
        let rows2 = parse_to_rows(src2);
        assert_eq!(rows2[0].kind, RowKind::Paragraph, "行内图片不作独立图片行");
    }

    /// 基准：解析结果的驻留内存放大倍数（打印，不构成断言；`--ignored` 运行）。
    ///
    /// 运行：`cargo test --release -- --ignored bench_row_parse_memory --nocapture`
    /// 真实小文档（SKILL 风格）+ 接近 `PREVIEW_MAX_DOC_LINES` 的大文档各测一遍，
    /// 直观看出「解析成 Row 把源码放大了多少倍」。
    /// 当前进程**私有字节**（`GetProcessMemoryInfo` 的 `PrivateUsage`），
    /// 直接反映本进程堆上被该次解析真正占住的内存（含分配器锯齿/碎片）。
    /// Windows 上用 Win32 API 读取；非 Windows 无法取数返回 0（基准仅内网 Windows 用）。
    #[cfg(windows)]
    fn process_private_bytes() -> usize {
        #[repr(C)]
        struct ProcessMemoryCounters {
            cb: u32,
            page_fault_count: u32,
            peak_working_set_size: usize,
            working_set_size: usize,
            quota_peak_paged_pool_usage: usize,
            quota_paged_pool_usage: usize,
            quota_peak_nonpaged_pool_usage: usize,
            quota_nonpaged_pool_usage: usize,
            pagefile_usage: usize,
            peak_pagefile_usage: usize,
            private_usage: usize,
        }
        extern "system" {
            fn GetCurrentProcess() -> *mut std::ffi::c_void;
            fn K32GetProcessMemoryInfo(
                process: *mut std::ffi::c_void,
                counters: *mut ProcessMemoryCounters,
                size: u32,
            ) -> i32;
        }
        unsafe {
            let mut m: ProcessMemoryCounters = std::mem::zeroed();
            m.cb = std::mem::size_of::<ProcessMemoryCounters>() as u32;
            if K32GetProcessMemoryInfo(GetCurrentProcess(), &mut m, m.cb) != 0 {
                m.private_usage
            } else {
                0
            }
        }
    }

    #[cfg(not(windows))]
    fn process_private_bytes() -> usize {
        0
    }

    #[test]
    #[ignore]
    fn bench_row_parse_memory() {
        use std::time::Instant;

        // 项目 target 目录下的真实 SKILL 文档（与本讨论同款，143 行小文档）；
        // 以及 16 万行大文档（超过 PREVIEW_MAX_DOC_LINES=20000 自动预览拦截线）。
        // 跑 --ignored 且文件存在才打印，避免路径依赖。
        let mut cases: Vec<(String, String)> = Vec::new();
        for name in ["SKILL.md", "SKILL.txt", "大文档测试.md", "大文档测试.txt"] {
            if let Ok(s) = std::fs::read_to_string(format!("target/{name}")) {
                cases.push((format!("target/{name}"), s));
            }
        }
        // 自动扫描 target/benchdata/*.md（生成脚本同目录即可自动纳入对比）
        if let Ok(dir) = std::fs::read_dir("target/benchdata") {
            let mut paths: Vec<_> = dir
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().map(|e| e == "md").unwrap_or(false))
                .collect();
            paths.sort();
            for p in paths {
                if let Ok(s) = std::fs::read_to_string(&p) {
                    cases.push((format!("{}", p.display()), s));
                }
            }
        }

        // 大文档：标题/段落/列表/表格/代码围栏混合 ≈ 1.4 万行，贴近自动预览上限
        let block = "# 章节标题\n\n正文段落，包含 `行内代码` 与 **加粗** 和 [链接](https://x.y)。\n\n- 列表项一\n- 列表项二\n\n| 列A | 列B |\n| --- | --- |\n| 值1 | 值2 |\n\n```rust\nfn main() { println!(\"hi\"); }\n```\n";
        let mut big = String::with_capacity(block.len() * 700);
        for _ in 0..700 {
            big.push_str(block);
        }
        cases.push(("合成大文档（≈1.4 万行）".to_string(), big));

        use std::mem::size_of;
        println!(
            "Row={}B | RowKind={}B RunKind={}B SharedString={}B Vec<SharedString>={}B (Range,RunKind)={}B TableData={}B Option<TableData>={}B Option<Box<TableData>>={}B Option<SharedString>={}B Option<Arc<Image>>={}B Option<u32>={}B",
            size_of::<Row>(),
            size_of::<RowKind>(),
            size_of::<RunKind>(),
            size_of::<SharedString>(),
            size_of::<Vec<SharedString>>(),
            size_of::<(Range<usize>, RunKind)>(),
            size_of::<TableData>(),
            size_of::<Option<TableData>>(),
            size_of::<Option<Box<TableData>>>(),
            size_of::<Option<SharedString>>(),
            size_of::<Option<std::sync::Arc<Image>>>(),
            size_of::<Option<u32>>(),
        );

        for (name, src) in &cases {
            let t = Instant::now();
            let before = process_private_bytes();
            let rows = parse_to_rows(src);
            let after = process_private_bytes();
            let (n, bytes) = rows_memory_estimate(&rows);
            let bytes = bytes.max(1);
            let delta_kb = after.saturating_sub(before) / 1024;
            println!(
                "{name}: 源 {} B / {} 行 → {} 渲染行, 估算驻留 {} KB (= 源×{:.1}), 进程私有字节增量 {} KB, Row {} B, 解析 {} ms",
                src.len(),
                src.lines().count(),
                n,
                bytes / 1024,
                bytes as f64 / src.len().max(1) as f64,
                delta_kb,
                std::mem::size_of::<Row>(),
                t.elapsed().as_millis(),
            );
        }
    }
}
