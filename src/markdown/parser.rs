//! 手写 Markdown 行扫描器：把源码**逐行**转成渲染行（[`Row`]）。
//!
//! 设计目标：**行级渲染** —— 一个代码围栏就是 N 个等高的普通行，
//! 天然适合行级虚拟化；再大的代码块 / 长文档也不会出现“单个超大渲染块”，
//! 从根上避免滚动 / 渲染卡顿。

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
    /// 空行（作为行间留白）
    Blank,
}

/// 行内样式段类型（当前 v1 渲染按行统一着色，暂不逐段消费，预留给后续增强）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunKind {
    Bold,
    Italic,
    InlineCode,
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
        }
    }
}

fn inline_styles_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new("`([^`\\n]+)`|\\*\\*([^*\\n]+)\\*\\*|\\*([^*\\n]+)\\*").unwrap()
    })
}

/// 清理内联记号并收集行内样式段：
/// `` `代码` `` → 代码；`**粗体**` → 粗体；`*斜体*` → 斜体。
fn inline_style(src: &str) -> (SharedString, Vec<(Range<usize>, RunKind)>) {
    let mut out = String::with_capacity(src.len());
    let mut runs = Vec::new();
    let mut last = 0usize;
    for cap in inline_styles_re().find_iter(src) {
        let span = cap.range();
        let (inner, kind) = match &src[span.start..span.end] {
            s if s.starts_with('`') => (&s[1..s.len() - 1], RunKind::InlineCode),
            s if s.starts_with("**") => (&s[2..s.len() - 2], RunKind::Bold),
            s => (&s[1..s.len() - 1], RunKind::Italic),
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
fn list_prefix(t: &str) -> Option<(SharedString, &str)> {
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

/// 把 Markdown 源码解析成渲染行（每行一行，含行内清理）。
pub fn parse_to_rows(src: &str) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut in_fence: Option<char> = None;
    for raw in src.split('\n') {
        let line = raw.trim_end_matches('\r');
        let t = line.trim_start();
        if let Some(fc) = in_fence {
            // 围栏内：碰到同字符闭合行则结束
            if let Some((c, _)) = fence_len(t) {
                if c == fc {
                    in_fence = None;
                    continue;
                }
            }
            rows.push(Row::new(line, RowKind::Code, Vec::new(), None));
            continue;
        }
        if let Some((fc, _)) = fence_len(t) {
            in_fence = Some(fc);
            continue;
        }
        if t.is_empty() {
            rows.push(Row::new("", RowKind::Blank, Vec::new(), None));
            continue;
        }
        // 标题：`#{1,6} ` 后跟内容
        let hashes = t.chars().take_while(|&c| c == '#').count();
        if (1..=6).contains(&hashes) && t.as_bytes().len() > hashes {
            let (text, runs) = inline_style(t[hashes..].trim_start());
            rows.push(Row::new(
                text.as_ref(),
                RowKind::Heading(hashes as u8),
                runs,
                None,
            ));
            continue;
        }
        // 引述：`>` 开头
        if t.starts_with('>') {
            let rest = t.trim_start_matches('>').trim_start();
            let (text, runs) = inline_style(rest);
            rows.push(Row::new(text.as_ref(), RowKind::Quote, runs, None));
            continue;
        }
        // 列表项：`- ` / `* ` / `+ ` / `1. `
        if let Some((prefix, content)) = list_prefix(t) {
            let (text, runs) = inline_style(content);
            rows.push(Row::new(
                text.as_ref(),
                RowKind::Bullet,
                runs,
                Some(prefix.clone()),
            ));
            continue;
        }
        // 水平分隔线
        if is_rule(t) {
            rows.push(Row::new("", RowKind::Rule, Vec::new(), None));
            continue;
        }
        // 表格（含 | ）：v1 按普通行处理，避免单行大宽产生额外问题
        let (text, runs) = inline_style(line.trim());
        rows.push(Row::new(text.as_ref(), RowKind::Paragraph, runs, None));
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
    }
}
