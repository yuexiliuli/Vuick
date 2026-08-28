//! 轻量 Markdown 渲染（GPUI 版）。
//!
//! 移植自 Tauri 成品前端的 `src/markdown.ts`，把 Markdown 文本解析为 GPUI 的
//! `div + StyledText` 元素树，支持：段落、标题、行内 code/加粗/斜体/删除线/链接、
//! 围栏代码块、列表、表格、引用、分割线。
//!
//! 渲染结果是一个 `Div`，可直接作为 `.child(...)` 使用。

use gpui::{
    div, font, px, rgb, rgba, AnyElement, Div, ElementId, FontWeight, Hsla, IntoElement,
    InteractiveElement, ParentElement, Pixels, ScrollWheelEvent, SharedString,
    StatefulInteractiveElement, Styled, StyledText, TextRun, UnderlineStyle, Window,
};
use super::scrollbar::{horizontal_scrollbar, TableScrollState};

/// 表格超宽时的处理方式。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TableMode {
    /// 列按可用宽度等分，单元格内文本自动换行（不出现横向滚动）。
    Wrap,
    /// 列取内容自然宽度（各列等宽对齐），超出可用宽度时出现**可拖拽的横向滚动条**。
    Scroll,
}

/// 单元格左右内边距（px_2 = 8px*2）——列宽测量时补上，避免文字顶到边框
const CELL_H_PAD: Pixels = px(16.);
/// 单元格左边框宽度（列宽测量时补上）
const CELL_BORDER: Pixels = px(1.);
/// 表头加粗比普通字稍宽的测量缓冲（Scroll 模式列宽按普通字重测量）
const CELL_HEADER_BOLD_BUFFER: Pixels = px(2.);

/// 渲染配色（可整体定制明/暗主题）。
pub struct MarkdownTheme {
    pub text: Hsla,
    pub code_bg: Hsla,
    pub code_text: Hsla,
    pub mono_family: SharedString,
    pub link: Hsla,
    pub heading: Hsla,
    pub blockquote_bg: Hsla,
    pub blockquote_border: Hsla,
    pub table_bg: Hsla,
    pub table_text: Hsla,
}

impl MarkdownTheme {
    /// 亮色主题（对应原前端的浅色风格）
    pub fn light() -> Self {
        Self {
            text: rgb(0x1f2328).into(),
            code_bg: rgba(0x00000014).into(),
            code_text: rgb(0x1f6feb).into(), // 行内 code 用蓝色
            mono_family: "Consolas".into(),
            link: rgb(0x2563eb).into(),
            heading: rgb(0x111827).into(),
            blockquote_bg: rgb(0xf9fafb).into(),
            blockquote_border: rgb(0xd1d5db).into(),
            table_bg: rgb(0xf3f4f6).into(),
            table_text: rgb(0x1f2328).into(),
        }
    }
}

/// 行内样式片段
#[derive(Clone, Debug)]
struct Span {
    text: String,
    bold: bool,
    italic: bool,
    strike: bool,
    code: bool,
    link: bool,
}

/// 块级元素
#[derive(Debug)]
enum Block {
    Paragraph(Vec<Vec<Span>>), // 每行一个 Vec<Span>
    Heading { level: usize, spans: Vec<Span> },
    Code(String),
    List { ordered: bool, items: Vec<Vec<Span>> },
    Table {
        headers: Vec<Vec<Span>>,
        rows: Vec<Vec<Vec<Span>>>,
    },
    Quote(Vec<Span>),
    Hr,
}

// ==================== 行内解析（对齐原 markdown.ts 的 inline()） ====================

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
        // 行内代码 `...`
        if v[pos] == '`' {
            let mut close = None;
            for j in pos + 1..n {
                if v[j] == '`' {
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

        // 链接 [text](url)
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

        // 加粗 **text**
        if v[pos] == '*' && pos + 1 < n && v[pos + 1] == '*' {
            if let Some(j) = find_closing(&v, pos + 2, "**") {
                flush_plain(out, &mut plain, bold, italic, strike);
                let inner: String = v[pos + 2..j].iter().collect();
                parse_inline_with(&inner, true, italic, strike, out);
                pos = j + 2;
                continue;
            }
        }

        // 删除线 ~~text~~
        if v[pos] == '~' && pos + 1 < n && v[pos + 1] == '~' {
            if let Some(j) = find_closing(&v, pos + 2, "~~") {
                flush_plain(out, &mut plain, bold, italic, strike);
                let inner: String = v[pos + 2..j].iter().collect();
                parse_inline_with(&inner, bold, italic, true, out);
                pos = j + 2;
                continue;
            }
        }

        // 斜体 *text*
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

/// 查找以 `start`（字符索引）为起点、结束标记 `marker` 的第一次出现位置（字符索引）。
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

/// 解析 `[text](url)`：返回闭合 `)` 的字符索引。
fn find_link_end(v: &[char], start: usize) -> Option<usize> {
    let mut i = start + 1;
    let mut bracket_depth = 1usize;
    while i < v.len() {
        match v[i] {
            '[' => bracket_depth += 1,
            ']' => {
                if bracket_depth == 1 {
                    // 期望后面紧跟 '('
                    if i + 1 < v.len() && v[i + 1] == '(' {
                        // 找到 ')'
                        for j in i + 2..v.len() {
                            if v[j] == ')' {
                                return Some(j);
                            }
                            if v[j] == '\n' || v[j] == ' ' {
                                break; // url 不含空白
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

// ==================== 块级解析（对齐原 markdown.ts 的 renderMarkdown） ====================

fn parse_markdown(src: &str) -> Vec<Block> {
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
            let spans: Vec<Vec<Span>> = std::mem::take(para)
                .into_iter()
                .map(|l| parse_inline(&l))
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

        // 标题
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
                let rest = trimmed[idx + 1..].trim();
                blocks.push(Block::Heading {
                    level,
                    spans: parse_inline(rest),
                });
                continue;
            }
        }

        // 分割线
        let t = line.trim();
        let dashes = t.chars().all(|c| c == '-' || c == '*' || c == '_') && t.chars().count() >= 3;
        if dashes {
            flush_para(&mut blocks, &mut para);
            flush_table(&mut blocks, &mut table_buf);
            flush_list(&mut blocks, &mut list);
            blocks.push(Block::Hr);
            continue;
        }

        // 引用
        if leading == 0 && trimmed.starts_with('>') {
            flush_para(&mut blocks, &mut para);
            flush_table(&mut blocks, &mut table_buf);
            flush_list(&mut blocks, &mut list);
            let rest = trimmed[1..].trim_start().to_string();
            blocks.push(Block::Quote(parse_inline(&rest)));
            continue;
        }

        // 列表
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
            let item = if let Some(x) = ol {
                x
            } else {
                ul.unwrap_or_default()
            };
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
    let b = trimmed.as_bytes();
    if !b.is_empty() && (b[0] == b'-' || b[0] == b'+' || b[0] == b'*') {
        if b.len() > 1 && (b[1] == b' ' || b[1] == b'\t') {
            return Some(trimmed[1..].trim_start().to_string());
        }
    }
    None
}

fn list_item_ol(trimmed: &str) -> Option<String> {
    let mut idx = 0;
    let b = trimmed.as_bytes();
    while idx < b.len() && b[idx].is_ascii_digit() {
        idx += 1;
    }
    if idx == 0 || idx >= b.len() {
        return None;
    }
    if b[idx] == b'.' || b[idx] == b')' {
        if idx + 1 < b.len() && (b[idx + 1] == b' ' || b[idx + 1] == b'\t') {
            return Some(trimmed[idx + 1..].trim_start().to_string());
        }
    }
    None
}

fn flush_list(blocks: &mut Vec<Block>, list: &mut Option<(bool, Vec<String>)>) {
    if let Some((ordered, items)) = list.take() {
        let items = items
            .into_iter()
            .map(|i| parse_inline(&i))
            .collect::<Vec<_>>();
        blocks.push(Block::List { ordered, items });
    }
}

fn table_row_cells(line: &str) -> Vec<String> {
    let mut s = line.trim().to_string();
    if s.starts_with('|') {
        s.remove(0);
    }
    if s.ends_with('|') {
        s.pop();
    }
    s.split('|').map(|c| c.trim().to_string()).collect()
}

fn flush_table(blocks: &mut Vec<Block>, table_buf: &mut Vec<String>) {
    if table_buf.is_empty() {
        return;
    }
    let buf = std::mem::take(table_buf);
    let mut rows: Vec<Vec<String>> = Vec::new();
    for line in buf {
        rows.push(table_row_cells(&line));
    }
    if rows.is_empty() {
        return;
    }
    let headers = rows[0]
        .iter()
        .map(|h| parse_inline(h))
        .collect::<Vec<_>>();
    let data: Vec<Vec<Vec<Span>>> = rows[1..]
        .iter()
        .filter(|row| {
            // 跳过 `---` 分隔行
            row.iter().any(|c| {
                let t = c.trim();
                !(t.is_empty() || (t.len() >= 3 && t.chars().all(|ch| ch == '-')))
            })
        })
        .map(|row| row.iter().map(|c| parse_inline(c)).collect())
        .collect();
    blocks.push(Block::Table {
        headers,
        rows: data,
    });
}

// ==================== 渲染为 GPUI 元素 ====================

fn spans_to_styled(
    text: &str,
    spans: &[Span],
    theme: &MarkdownTheme,
    force_bold: bool,
) -> SpannedText {
    let base = font(".SystemUIFont");
    let mono = font(theme.mono_family.clone());
    let mut runs: Vec<TextRun> = Vec::new();
    let mut total = 0usize;
    for sp in spans {
        let mut f = base.clone();
        if sp.code {
            f = mono.clone();
        }
        // 注意：必须设置在 run 的字体上才生效——设在外层 div 的 font_weight
        // 不会级联进带显式 font 的 TextRun（这就是此前"表头加粗没效果"的原因）。
        if sp.bold || force_bold {
            f = f.bold();
        }
        if sp.italic {
            f = f.italic();
        }
        let run = TextRun {
            len: sp.text.len(),
            font: f,
            color: if sp.code {
                theme.code_text
            } else if sp.link {
                theme.link
            } else {
                theme.text
            },
            background_color: if sp.code {
                Some(theme.code_bg)
            } else {
                None
            },
            underline: if sp.link {
                Some(UnderlineStyle {
                    thickness: px(1.),
                    color: Some(theme.link),
                    wavy: false,
                })
            } else {
                None
            },
            strikethrough: if sp.strike {
                Some(gpui::StrikethroughStyle {
                    thickness: px(1.),
                    color: Some(theme.text),
                })
            } else {
                None
            },
        };
        total += sp.text.len();
        runs.push(run);
    }
    debug_assert_eq!(total, text.len());
    SpannedText {
        text: SharedString::from(text.to_owned()),
        runs,
    }
}

struct SpannedText {
    text: SharedString,
    runs: Vec<TextRun>,
}

impl SpannedText {
    fn into_element(self) -> StyledText {
        StyledText::new(self.text).with_runs(self.runs)
    }
}

fn render_spans_line(spans: &[Span], theme: &MarkdownTheme) -> StyledText {
    render_spans_line_weighted(spans, theme, false)
}

/// 渲染一行行内片段，可整体强制加粗（用于表头单元格）。
fn render_spans_line_weighted(spans: &[Span], theme: &MarkdownTheme, force_bold: bool) -> StyledText {
    let text: String = spans.iter().map(|s| s.text.as_str()).collect();
    spans_to_styled(&text, spans, theme, force_bold).into_element()
}

fn render_block(
    block: &Block,
    theme: &MarkdownTheme,
    mode: TableMode,
    scroll: Option<&TableScrollState>,
    window: Option<&mut Window>,
    table_idx: &mut usize,
) -> AnyElement {
    match block {
        Block::Paragraph(lines) => {
            let mut d = div();
            for line in lines {
                d = d.child(render_spans_line(line, theme));
            }
            d.mb_2().into_any_element()
        }
        Block::Heading { level, spans } => {
            let size = match level {
                1 => px(20.),
                2 => px(18.),
                3 => px(16.),
                _ => px(15.),
            };
            let text: String = spans.iter().map(|s| s.text.as_str()).collect();
            let st = spans_to_styled(&text, spans, theme, false).into_element();
            div()
                .mb_2()
                .mt_1()
                .text_size(size)
                .font_weight(FontWeight::BOLD)
                .text_color(theme.heading)
                .child(st)
                .into_any_element()
        }
        Block::Code(code) => {
            let mut child = div();
            for line in code.split('\n') {
                let st = SpannedText {
                    text: SharedString::from(line.to_owned()),
                    runs: vec![TextRun {
                        len: line.len(),
                        font: font(theme.mono_family.clone()),
                        color: theme.code_text,
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    }],
                };
                child = child.child(st.into_element());
            }
            child
                .bg(rgb(0x0f172a))
                .rounded_lg()
                .p_3()
                .mb_2()
                .text_size(px(13.))
                .text_color(gpui::rgb(0xe2e8f0))
                .into_any_element()
        }
        Block::List { ordered, items } => {
            let mut d = div().mb_2();
            for (i, item) in items.iter().enumerate() {
                let marker = if *ordered {
                    format!("{}. ", i + 1)
                } else {
                    "• ".to_string()
                };
                let mut row = div().flex().flex_row();
                row = row.child(
                    div().child(
                        SpannedText {
                            text: SharedString::from(marker.to_owned()),
                            runs: vec![TextRun {
                                len: marker.len(),
                                font: font(".SystemUIFont"),
                                color: theme.text,
                                background_color: None,
                                underline: None,
                                strikethrough: None,
                            }],
                        }
                        .into_element(),
                    ),
                );
                row = row.child(render_spans_line(item, theme));
                d = d.child(row);
            }
            d.into_any_element()
        }
        Block::Table { headers, rows } => {
            let ncols = headers.len().max(rows.iter().map(|r| r.len()).max().unwrap_or(0));
            if ncols == 0 {
                return div().into_any_element();
            }

            // 横向滚动模式：先量出每一列的最大内容宽度（含 padding + 边框），
            // 全列单元格统一用该宽度 → 各列分割线严格对齐（Scroll 模式列宽不再按行各异）。
            let mut col_widths: Vec<Pixels> = vec![px(0.); ncols];
            if mode == TableMode::Scroll {
                let mut window = window;
                if let Some(wnd) = window.as_deref_mut() {
                    let font_size = px(13.);
                    for c in 0..ncols {
                        let mut m = px(0.);
                        for spans in std::iter::once(headers)
                            .chain(rows.iter())
                            .filter_map(|r| r.get(c))
                        {
                            let text: String = spans.iter().map(|s| s.text.as_str()).collect();
                            let st = spans_to_styled(&text, spans, theme, false);
                            let lw = wnd
                                .text_system()
                                .shape_line(st.text, font_size, &st.runs, None)
                                .width;
                            let w = lw
                                + CELL_H_PAD
                                + CELL_BORDER
                                + CELL_HEADER_BOLD_BUFFER;
                            if w > m {
                                m = w;
                            }
                        }
                        col_widths[c] = m;
                    }
                }
            }

            // 单元格：统一 padding/字号/颜色；c>0 的列加左侧分割线（网格线）；表头加浅色底
            let render_cell = |c: usize,
                               row: &Vec<Vec<Span>>,
                               header: bool,
                               theme: &MarkdownTheme|
             -> Div {
                let mut cell = div()
                    .px_2()
                    .py_1()
                    .text_size(px(13.))
                    .text_color(theme.table_text);
                if header {
                    cell = cell.font_weight(FontWeight::BOLD).bg(theme.table_bg);
                }
                // 每一列之间画一条竖直分割线，让各列文字有齐平的参考线
                if c > 0 {
                    cell = cell.border_l_1().border_color(theme.blockquote_border);
                }
                match mode {
                    TableMode::Wrap => {
                        // 等宽列：flex_1 平分可用宽度，min_w(0) 允许收缩，文本自动换行
                        cell = cell.flex_1().min_w(px(0.));
                    }
                    TableMode::Scroll => {
                        // 列宽取自测量值（全列一致 → 对齐），nowrap 保证单行、表宽=内容宽
                        cell = cell
                            .w(col_widths[c])
                            .flex_shrink_0()
                            .whitespace_nowrap();
                    }
                }
                if let Some(spans) = row.get(c) {
                    // 表头：强行使 run 加粗（仅 div.font_weight 不会级联进带显式 font 的 run）
                    if header {
                        cell = cell.child(render_spans_line_weighted(spans, theme, true));
                    } else {
                        cell = cell.child(render_spans_line(spans, theme));
                    }
                }
                cell
            };

            let header_row = div()
                .flex()
                .flex_row()
                .children((0..ncols).map(|c| render_cell(c, headers, true, theme)));
            let data_rows: Vec<Div> = rows
                .iter()
                .map(|row| {
                    div()
                        .flex()
                        .flex_row()
                        .border_t_1()
                        .border_color(theme.blockquote_border)
                        .children((0..ncols).map(|c| render_cell(c, row, false, theme)))
                })
                .collect();
            // 表格本体：外框 + 表头 + 数据行（网格线齐平）
            let table_body = div()
                .mt_1()
                .mb_2()
                .rounded_md()
                .border_1()
                .border_color(theme.blockquote_border)
                .overflow_hidden()
                .child(header_row)
                .children(data_rows);

            match mode {
                TableMode::Wrap => table_body.into_any_element(),
                TableMode::Scroll => {
                    // 横向滚动：滚动容器用 flex 行（块级会把子元素钳制到容器宽，导致
                    // 无法横向溢出），表格本体 flex_shrink_0 保持内容自然宽度，超宽时滚动。
                    let state = scroll.cloned().unwrap_or_default();
                    let handle = state.handle.clone();
                    // stateful 需要一个全局（按父级作用域隔离）唯一的 id，按本消息内的
                    // 表格序号生成，保证跨帧状态（横向位置）稳定
                    let idx = *table_idx;
                    *table_idx += 1;
                    let scroller = div()
                        .id(ElementId::Name(SharedString::from(format!("md-tbl-scroll-{idx}"))))
                        .mt_1()
                        .flex()
                        .flex_row()
                        .overflow_x_scroll()
                        .track_scroll(&handle)
                        // 滚轮消费：表格可横向滚动时，把滚轮定死在本表格（GPUI 内置横向
                        // 滚动会先把它映射成横向滚动），并阻止向父级（消息区）冒泡，
                        // 否则页面会跟着上下滚动；表格不可滚动时放行让页面正常滚动。
                        .on_scroll_wheel({
                            let handle = handle.clone();
                            move |_event: &ScrollWheelEvent, _window, cx| {
                                if handle.max_offset().width > px(0.) {
                                    cx.stop_propagation();
                                }
                            }
                        })
                        .child(table_body.flex_shrink_0());
                    div()
                        .mt_0()
                        .mb_2()
                        .child(scroller)
                        .child(horizontal_scrollbar(
                            &state,
                            ElementId::Name(SharedString::from(format!("md-tbl-hsb-{idx}"))),
                        ))
                        .into_any_element()
                }
            }
        }
        Block::Quote(spans) => div()
            .mt_1()
            .mb_2()
            .pl_3()
            .py_1()
            .border_l_2()
            .border_color(theme.blockquote_border)
            .bg(theme.blockquote_bg)
            .child(render_spans_line(spans, theme))
            .into_any_element(),
        Block::Hr => div()
            .mt_2()
            .mb_2()
            .h(px(1.))
            .bg(rgb(0xe5e7eb))
            .into_any_element(),
    }
}

/// 渲染为轻量 Markdown 的入口（用亮色主题；表格默认"自动换行"模式）。
#[allow(dead_code)]
pub fn render_markdown(md: &str) -> Div {
    render_markdown_with_options(md, &MarkdownTheme::light(), TableMode::Wrap, None, None)
}

/// 使用自定义主题渲染 Markdown。
#[allow(dead_code)]
pub fn render_markdown_themed(md: &str, theme: &MarkdownTheme) -> Div {
    render_markdown_with_options(md, theme, TableMode::Wrap, None, None)
}

/// 渲染 Markdown，并指定表格超宽时的处理方式。
///
/// - `scroll`：`TableMode::Scroll` 时提供表格的横向滚动（句柄 + 拖拽状态，调用方
///   需为同一篇内容保存一个 [`TableScrollState`]，跨帧保持横向位置与拖拽）；
/// - `window`：Scroll 模式下用量文本实际宽度来统一列宽（保证各列分割线对齐）；
///   其它模式可传 `None`。
pub fn render_markdown_with_options(
    md: &str,
    theme: &MarkdownTheme,
    mode: TableMode,
    scroll: Option<&TableScrollState>,
    window: Option<&mut Window>,
) -> Div {
    let blocks = parse_markdown(md);
    let mut table_idx = 0usize;
    let mut window = window;
    let children: Vec<AnyElement> = blocks
        .iter()
        .map(|b| render_block(b, theme, mode, scroll, window.as_deref_mut(), &mut table_idx))
        .collect();
    div()
        .flex()
        .flex_col()
        .text_color(theme.text)
        .children(children)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_bold_code() {
        let s = parse_inline("前 **加粗** 后 `code` 尾");
        let text: String = s.iter().map(|x| x.text.as_str()).collect();
        assert_eq!(text, "前 加粗 后 code 尾");
        assert!(s.iter().any(|x| x.bold));
        assert!(s.iter().any(|x| x.code));
    }

    /// 表头整体强制加粗要作用在 TextRun 的字体上（祖先 div 的 font_weight
    /// 不会级联进带显式 font 的 run——修复"表头加粗没效果"）。
    #[test]
    fn header_force_bold_uses_run_font() {
        let spans = parse_inline("列 A");
        let theme = MarkdownTheme::light();
        let bold = spans_to_styled("列 A", &spans, &theme, true);
        let normal = spans_to_styled("列 A", &spans, &theme, false);
        assert!(
            bold.runs[0].font.weight == FontWeight::BOLD,
            "表头 run 应为加粗字重"
        );
        assert!(
            normal.runs[0].font.weight == FontWeight::NORMAL,
            "普通单元格 run 不应加粗"
        );
    }

    #[test]
    fn inline_link() {
        let s = parse_inline("看[文档](https://example.com)吧");
        assert!(s.iter().any(|x| x.link));
        let text: String = s.iter().map(|x| x.text.as_str()).collect();
        assert_eq!(text, "看文档吧");
    }

    #[test]
    fn blocks_code_and_heading() {
        let b = parse_markdown("# 标题\n\n```\nlet x = 1;\n```\n\n正文");
        let kinds: Vec<&str> = b
            .iter()
            .map(|x| match x {
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
        let b = parse_markdown("| 项目 | 周期 |\n| --- | --- |\n| A | 3 年 |\n| B | 5 年 |");
        match b.get(0) {
            Some(Block::Table { headers, rows }) => {
                assert_eq!(headers.len(), 2);
                assert_eq!(rows.len(), 2);
            }
            other => panic!("应解析为表格: {other:?}"),
        }
    }

    /// 横向滚动模式下，超宽表格必须真实溢出容器（handle.max_offset.width>0），
    /// 底部横向滚动条才会出现；否则说明表格被容器宽度钳制、没有横向滚动。
    #[gpui::test]
    async fn scroll_table_overflows_wide_container(cx: &mut gpui::TestAppContext) {
        struct WideHost {
            state: TableScrollState,
        }
        impl gpui::Render for WideHost {
            fn render(
                &mut self,
                window: &mut gpui::Window,
                _cx: &mut gpui::Context<Self>,
            ) -> impl gpui::IntoElement {
                let long: String = "abcdefghijklmnopqrstuvwxyz0123456789".repeat(12);
                let md = format!("| 列A | 很长的列B |\n| --- | --- |\n| 值 | {long} |");
                div()
                    .h(px(200.))
                    .w(px(400.))
                    .bg(gpui::rgb(0xffffff))
                    .child(render_markdown_with_options(
                        &md,
                        &MarkdownTheme::light(),
                        TableMode::Scroll,
                        Some(&self.state),
                        Some(window),
                    ))
            }
        }

        let (view, cx) = cx.add_window_view(|_window, _cx| WideHost {
            state: TableScrollState::new(),
        });
        cx.simulate_resize(gpui::size(px(500.), px(400.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let max_w = cx.update(|_window, cx| view.read(cx).state.handle.max_offset().width);
        assert!(
            max_w > px(0.),
            "横向滚动模式下超宽表格应可横向滚动：max_offset.width={max_w:?}"
        );
        // 点击横向滚动条应能"跳转/拖拽"到对应位置：横向偏移从 0 变成负值
        let (bar_left, bar_bottom, viewport_w) = cx.update(|_window, cx| {
            let b = view.read(cx).state.handle.bounds();
            (b.left(), b.bottom(), b.size.width)
        });
        cx.simulate_click(
            gpui::point(bar_left + viewport_w * 0.7, bar_bottom + px(6.)),
            gpui::Modifiers::default(),
        );
        cx.run_until_parked();
        let offset_x = cx.update(|_window, cx| view.read(cx).state.handle.offset().x);
        assert!(
            offset_x < px(-1.),
            "点击横向滚动条应能把表格向右滚动：offset.x={offset_x:?}"
        );
    }
}
