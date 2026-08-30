//! Markdown block and inline parsing.

use super::model::{Block, Span};

fn flush_plain(spans: &mut Vec<Span>, plain: &mut String, bold: bool, italic: bool, strike: bool) {
    if !plain.is_empty() {
        spans.push(Span {
            text: std::mem::take(plain),
            bold,
            italic,
            strike,
            code: false,
            link: false,
        });
    }
}

fn parse_inline(src: &str) -> Vec<Span> {
    let mut out = Vec::new();
    parse_inline_with(src, false, false, false, &mut out);
    out
}

fn parse_inline_with(src: &str, bold: bool, italic: bool, strike: bool, out: &mut Vec<Span>) {
    let v: Vec<char> = src.chars().collect();
    let n = v.len();
    let mut plain = String::new();
    let mut pos = 0usize;

    while pos < n {
        if v[pos] == '`' {
            let mut close = None;
            for (j, ch) in v.iter().enumerate().skip(pos + 1) {
                if *ch == '`' {
                    close = Some(j);
                    break;
                }
            }
            if let Some(j) = close {
                flush_plain(out, &mut plain, bold, italic, strike);
                let text: String = v[pos + 1..j].iter().collect();
                out.push(Span {
                    text,
                    bold: false,
                    italic: false,
                    strike: false,
                    code: true,
                    link: false,
                });
                pos = j + 1;
                continue;
            }
        }

        if v[pos] == '[' {
            if let Some(end) = find_link_end(&v, pos) {
                if let Some(text_end) = (pos + 1..v.len()).find(|&j| v[j] == ']') {
                    flush_plain(out, &mut plain, bold, italic, strike);
                    let text: String = v[pos + 1..text_end].iter().collect();
                    out.push(Span {
                        text,
                        bold,
                        italic,
                        strike,
                        code: false,
                        link: true,
                    });
                    pos = end + 1;
                    continue;
                }
            }
        }

        if v[pos] == '*' && pos + 1 < n && v[pos + 1] == '*' {
            if let Some(j) = find_closing(&v, pos + 2, "**") {
                flush_plain(out, &mut plain, bold, italic, strike);
                let inner: String = v[pos + 2..j].iter().collect();
                parse_inline_with(&inner, true, italic, strike, out);
                pos = j + 2;
                continue;
            }
        }

        if v[pos] == '~' && pos + 1 < n && v[pos + 1] == '~' {
            if let Some(j) = find_closing(&v, pos + 2, "~~") {
                flush_plain(out, &mut plain, bold, italic, strike);
                let inner: String = v[pos + 2..j].iter().collect();
                parse_inline_with(&inner, bold, italic, true, out);
                pos = j + 2;
                continue;
            }
        }

        if v[pos] == '*' {
            if let Some(j) = find_closing(&v, pos + 1, "*") {
                flush_plain(out, &mut plain, bold, italic, strike);
                let inner: String = v[pos + 1..j].iter().collect();
                parse_inline_with(&inner, bold, true, strike, out);
                pos = j + 1;
                continue;
            }
        }

        plain.push(v[pos]);
        pos += 1;
    }
    flush_plain(out, &mut plain, bold, italic, strike);
}

fn find_closing(v: &[char], start: usize, marker: &str) -> Option<usize> {
    let m: Vec<char> = marker.chars().collect();
    let mut i = start;
    while i + m.len() <= v.len() {
        if v[i..i + m.len()] == m[..] {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn find_link_end(v: &[char], start: usize) -> Option<usize> {
    let mut i = start + 1;
    let mut bracket_depth = 1usize;
    while i < v.len() {
        match v[i] {
            '[' => bracket_depth += 1,
            ']' => {
                if bracket_depth == 1 {
                    if i + 1 < v.len() && v[i + 1] == '(' {
                        for (j, ch) in v.iter().enumerate().skip(i + 2) {
                            if *ch == ')' {
                                return Some(j);
                            }
                            if *ch == '\n' || *ch == ' ' {
                                break;
                            }
                        }
                    }
                    return None;
                }
                bracket_depth -= 1;
            }
            _ => {}
        }
        i += 1;
    }
    None
}

pub(crate) fn parse_markdown(src: &str) -> Vec<Block> {
    let src = src.replace("\r\n", "\n");
    let lines: Vec<&str> = src.split('\n').collect();
    let mut blocks: Vec<Block> = Vec::new();
    let mut para: Vec<String> = Vec::new();
    let mut in_code = false;
    let mut code_buf: Vec<String> = Vec::new();
    let mut table_buf: Vec<String> = Vec::new();
    let mut list: Option<(bool, Vec<String>)> = None;

    let flush_para = |blocks: &mut Vec<Block>, para: &mut Vec<String>| {
        if !para.is_empty() {
            let spans = std::mem::take(para)
                .into_iter()
                .map(|line| parse_inline(&line))
                .collect();
            blocks.push(Block::Paragraph(spans));
        }
    };

    for line in lines {
        if in_code {
            if line.trim_start().starts_with("```") {
                in_code = false;
                flush_para(&mut blocks, &mut para);
                blocks.push(Block::Code(code_buf.join("\n")));
                code_buf.clear();
            } else {
                code_buf.push(line.to_string());
            }
            continue;
        }
        if line.trim_start().starts_with("```") {
            flush_para(&mut blocks, &mut para);
            flush_table(&mut blocks, &mut table_buf);
            flush_list(&mut blocks, &mut list);
            in_code = true;
            continue;
        }
        if line.trim_start().starts_with('|') && line.trim_end().ends_with('|') {
            flush_para(&mut blocks, &mut para);
            flush_list(&mut blocks, &mut list);
            table_buf.push(line.trim().to_string());
            continue;
        }
        if !table_buf.is_empty() {
            flush_table(&mut blocks, &mut table_buf);
        }
        if line.trim().is_empty() {
            flush_para(&mut blocks, &mut para);
            flush_list(&mut blocks, &mut list);
            continue;
        }

        let trimmed = line.trim_start();
        let leading = line.len() - trimmed.len();
        if trimmed.starts_with('#') {
            let mut level = 0;
            let mut idx = 0;
            while idx < trimmed.len() && trimmed.as_bytes()[idx] == b'#' {
                level += 1;
                idx += 1;
            }
            if level <= 6 && idx < trimmed.len() && trimmed.as_bytes()[idx] == b' ' {
                flush_para(&mut blocks, &mut para);
                flush_table(&mut blocks, &mut table_buf);
                flush_list(&mut blocks, &mut list);
                blocks.push(Block::Heading {
                    level,
                    spans: parse_inline(trimmed[idx + 1..].trim()),
                });
                continue;
            }
        }

        let t = line.trim();
        let dashes = t.chars().all(|c| c == '-' || c == '*' || c == '_') && t.chars().count() >= 3;
        if dashes {
            flush_para(&mut blocks, &mut para);
            flush_table(&mut blocks, &mut table_buf);
            flush_list(&mut blocks, &mut list);
            blocks.push(Block::Hr);
            continue;
        }
        if leading == 0 && trimmed.starts_with('>') {
            flush_para(&mut blocks, &mut para);
            flush_table(&mut blocks, &mut table_buf);
            flush_list(&mut blocks, &mut list);
            blocks.push(Block::Quote(parse_inline(trimmed[1..].trim_start())));
            continue;
        }

        let ul = list_item_ul(trimmed);
        let ol = list_item_ol(trimmed);
        if ul.is_some() || ol.is_some() {
            flush_para(&mut blocks, &mut para);
            flush_table(&mut blocks, &mut table_buf);
            let ordered = ol.is_some();
            if let Some(entry) = &mut list {
                if entry.0 != ordered {
                    flush_list(&mut blocks, &mut list);
                    list = Some((ordered, Vec::new()));
                }
            } else {
                list = Some((ordered, Vec::new()));
            }
            let item = ol.or(ul).unwrap_or_default();
            if let Some(entry) = &mut list {
                entry.1.push(item.trim().to_string());
            }
            continue;
        }
        if list.is_some() {
            flush_list(&mut blocks, &mut list);
        }
        para.push(line.to_string());
    }

    if in_code {
        flush_para(&mut blocks, &mut para);
        blocks.push(Block::Code(code_buf.join("\n")));
    }
    flush_para(&mut blocks, &mut para);
    flush_table(&mut blocks, &mut table_buf);
    flush_list(&mut blocks, &mut list);
    blocks
}

fn list_item_ul(trimmed: &str) -> Option<String> {
    let bytes = trimmed.as_bytes();
    if !bytes.is_empty()
        && matches!(bytes[0], b'-' | b'+' | b'*')
        && bytes.len() > 1
        && matches!(bytes[1], b' ' | b'\t')
    {
        return Some(trimmed[1..].trim_start().to_string());
    }
    None
}

fn list_item_ol(trimmed: &str) -> Option<String> {
    let mut idx = 0;
    let bytes = trimmed.as_bytes();
    while idx < bytes.len() && bytes[idx].is_ascii_digit() {
        idx += 1;
    }
    if idx == 0 || idx >= bytes.len() {
        return None;
    }
    if matches!(bytes[idx], b'.' | b')')
        && idx + 1 < bytes.len()
        && matches!(bytes[idx + 1], b' ' | b'\t')
    {
        return Some(trimmed[idx + 1..].trim_start().to_string());
    }
    None
}

fn flush_list(blocks: &mut Vec<Block>, list: &mut Option<(bool, Vec<String>)>) {
    if let Some((ordered, items)) = list.take() {
        blocks.push(Block::List {
            ordered,
            items: items.into_iter().map(|item| parse_inline(&item)).collect(),
        });
    }
}

fn table_row_cells(line: &str) -> Vec<String> {
    let mut line = line.trim().to_string();
    if line.starts_with('|') {
        line.remove(0);
    }
    if line.ends_with('|') {
        line.pop();
    }
    line.split('|')
        .map(|cell| cell.trim().to_string())
        .collect()
}

fn flush_table(blocks: &mut Vec<Block>, table_buf: &mut Vec<String>) {
    if table_buf.is_empty() {
        return;
    }
    let rows: Vec<Vec<String>> = std::mem::take(table_buf)
        .into_iter()
        .map(|line| table_row_cells(&line))
        .collect();
    if rows.is_empty() {
        return;
    }
    let headers = rows[0].iter().map(|header| parse_inline(header)).collect();
    let rows = rows[1..]
        .iter()
        .filter(|row| {
            row.iter().any(|cell| {
                let cell = cell.trim();
                !(cell.is_empty() || (cell.len() >= 3 && cell.chars().all(|ch| ch == '-')))
            })
        })
        .map(|row| row.iter().map(|cell| parse_inline(cell)).collect())
        .collect();
    blocks.push(Block::Table { headers, rows });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_bold_code() {
        let spans = parse_inline("前 **加粗** 后 `code` 尾");
        let text: String = spans.iter().map(|span| span.text.as_str()).collect();
        assert_eq!(text, "前 加粗 后 code 尾");
        assert!(spans.iter().any(|span| span.bold));
        assert!(spans.iter().any(|span| span.code));
    }

    #[test]
    fn inline_link() {
        let spans = parse_inline("看[文档](https://example.com)吧");
        assert!(spans.iter().any(|span| span.link));
        let text: String = spans.iter().map(|span| span.text.as_str()).collect();
        assert_eq!(text, "看文档吧");
    }

    #[test]
    fn blocks_code_and_heading() {
        let blocks = parse_markdown("# 标题\n\n```\nlet x = 1;\n```\n\n正文");
        let kinds: Vec<&str> = blocks
            .iter()
            .map(|block| match block {
                Block::Heading { .. } => "h",
                Block::Code(_) => "c",
                Block::Paragraph(_) => "p",
                _ => "?",
            })
            .collect();
        assert_eq!(kinds, vec!["h", "c", "p"]);
    }

    #[test]
    fn table_parse() {
        let blocks = parse_markdown("| 项目 | 周期 |\n| --- | --- |\n| A | 3 年 |\n| B | 5 年 |");
        match blocks.first() {
            Some(Block::Table { headers, rows }) => {
                assert_eq!(headers.len(), 2);
                assert_eq!(rows.len(), 2);
            }
            other => panic!("应解析为表格: {other:?}"),
        }
    }
}
