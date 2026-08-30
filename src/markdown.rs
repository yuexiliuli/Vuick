//! 轻量 Markdown 渲染（GPUI 版）。
//!
//! 移植自 Tauri 成品前端的 `src/markdown.ts`，把 Markdown 文本解析为 GPUI 的
//! `div + StyledText` 元素树，支持：段落、标题、行内 code/加粗/斜体/删除线/链接、
//! 围栏代码块、列表、表格、引用、分割线。
//!
//! 渲染结果是一个 `Div`，可直接作为 `.child(...)` 使用。

use gpui::{
    div, font, point, relative, size, px, rgb, rgba, AnyElement, App, Bounds, CursorStyle, Div,
    Element, ElementId, FontWeight, GlobalElementId, Hsla, InspectorElementId, IntoElement,
    InteractiveElement, LayoutId, MouseButton, ParentElement, Pixels, ScrollWheelEvent,
    SharedString, StatefulInteractiveElement, Styled, StyledText, Style, TextRun, UnderlineStyle,
    Window, WrappedLine,
};
use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;
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
pub(crate) struct Span {
    text: String,
    bold: bool,
    italic: bool,
    strike: bool,
    code: bool,
    link: bool,
}

/// 块级元素
#[derive(Clone, Debug)]
pub(crate) enum Block {
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

/// 与 GPUI `LineWrapper::is_word_char` 保持一致：这些字符是"单词字符"，
/// GPUI 排版时**不会**在它们中间换行（空格/标点/中文等都会产生断点）。
fn is_unbroken_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
        // Latin-1 补充 + Latin Extended A/B
        || matches!(c, '\u{00C0}'..='\u{024F}')
        // 西里尔
        || matches!(c, '\u{0400}'..='\u{04FF}')
        // 常见特殊字符：`a-b` `var_name` `3.14` `@mention` `#hash` `a=1` 等
        || matches!(c, '-' | '_' | '.' | '\'' | '$' | '%' | '@' | '#' | '^' | '~' | ',' | '=' | ':')
}

/// 连续"单词字符"达到该长度后开始考虑插入断点（约 20 个字符 ≈ 140px）
const UNBROKEN_LIMIT: usize = 20;
/// 超过限制后每 N 个"单词字符"插入一个断点
const UNBROKEN_STEP: usize = 14;

/// 在"不可断"的超长连续片段里插入零宽空格（U+200B），让长 URL / 长串也能换行。
///
/// 背景：GPUI 只在空格/标点/中文等"断点"处换行，纯单词字符的长串没有断点，
/// 会整体溢出窗口（这与"有空格的行能换行、没空格的行不能换行"的现象一致）。
/// U+200B 是零宽、不可见的合法换行机会，插入后 GPUI 即可在字符级断开。
pub fn break_long_runs(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + text.len() / 4);
    let mut run = 0usize;
    for ch in text.chars() {
        if is_unbroken_word_char(ch) {
            run += 1;
            out.push(ch);
            if run > UNBROKEN_LIMIT && run % UNBROKEN_STEP == 0 {
                out.push('\u{200b}');
            }
        } else {
            run = 0;
            out.push(ch);
        }
    }
    out
}

fn spans_to_styled(spans: &[Span], theme: &MarkdownTheme, force_bold: bool) -> SpannedText {
    let base = font(".SystemUIFont");
    let mono = font(theme.mono_family.clone());
    let mut runs: Vec<TextRun> = Vec::new();
    let mut text = String::new();
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
        // 超长不可断片段插入零宽空格，让长词也能换行
        let seg = break_long_runs(sp.text.as_str());
        let run = TextRun {
            len: seg.len(),
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
        text.push_str(&seg);
        runs.push(run);
    }
    SpannedText {
        text: SharedString::from(text),
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

// ==================== 文本选择（鼠标拖选 + Ctrl+C 复制 markdown 源码） ====================

/// 一条回答的选区共享状态（元素每帧重建，状态经 Rc 长期保留）。
///
/// 选区的坐标是"文本流"内的字符偏移——把整条回答按渲染顺序拼成一段文本，
/// 每个文本叶子占其中一段；这样拖住跨过多个标题/段落/代码行也能连续选中，
/// 而复制时再按各叶子的 markdown 标记还原成源码。
pub struct SelectCtx {
    /// 本条回答的标识（用于 Ctrl+C 定位）
    pub turn: u64,
    /// 点击回答区时把焦点移交到这里（根视图），使 Ctrl+C 走"复制选区"而非输入框
    pub focus: gpui::FocusHandle,
    /// 选择锚点（文本流字符偏移）
    pub anchor: Rc<RefCell<Option<usize>>>,
    /// 当前选区 [start, end)
    pub selection: Rc<RefCell<Option<Range<usize>>>>,
    /// 各文本叶子的排版缓存（stream_start -> 行数/行高/宽度），供 measure 复用逻辑行高
    pub cache: Rc<RefCell<HashMap<usize, LeafCache>>>,
    /// 各文本叶子最近一帧绘制时的屏上几何（供鼠标命中测试）
    pub geom: Rc<RefCell<HashMap<usize, LeafGeom>>>,
}

impl SelectCtx {
    #[allow(dead_code)]
    pub fn new(turn: u64, focus: gpui::FocusHandle) -> Self {
        Self {
            turn,
            focus,
            anchor: Rc::new(RefCell::new(None)),
            selection: Rc::new(RefCell::new(None)),
            cache: Rc::new(RefCell::new(HashMap::new())),
            geom: Rc::new(RefCell::new(HashMap::new())),
        }
    }
}

/// 一个文本叶子的排版缓存（跨帧复用：同一宽度/字号下不再重复 shape/几何计算）。
///
/// 这是拖选"跟手"的关键：拖动时布局不变，measure/prepaint/paint 全部走缓存，
/// 每帧只更新叶子在窗口中的坐标；只有宽度/字号真正变化才重新排一次。
#[derive(Clone, Default)]
pub struct LeafCache {
    /// 排版时的文本（内容变了必须重新排，防旧缓存画出旧文本）
    pub text: SharedString,
    pub font_size: Pixels,
    pub line_height: Pixels,
    pub wrap: Pixels,
    pub rows: usize,
    /// 最近一次排版的单行结果（绘制直接复用，避免每帧再次 shape）
    pub shaped: Option<WrappedLine>,
}

impl LeafCache {
    /// 宽度、字号、文本都一致才算"可用"，否则需要重新 shape。
    fn matches(&self, wrap: Pixels, font_size: Pixels, text: &str) -> bool {
        self.shaped.is_some()
            && self.wrap == wrap
            && self.font_size == font_size
            && self.text.as_ref() == text
    }
}

/// 一个文本叶子的屏上几何（命中测试用）。
///
/// 直接持有排版结果（`WrappedLine`，Arc 浅克隆），命中与高亮都走 GPUI 精确的
/// `index_for_position`/`position_for_index`：`x` 超过行尾时返回结束索引，因此
/// **行尾最后一个字也能被选中**；换行后各行坐标也按行内相对计算，天然正确。
#[derive(Clone, Default)]
pub struct LeafGeom {
    pub left: Pixels,
    pub top: Pixels,
    pub line_height: Pixels,
    /// 该叶子的文本（命中后用 byte→char 换算）
    pub text: SharedString,
    /// 最近一次排版结果（没有则退化为 0）
    pub layout: Option<WrappedLine>,
}

impl LeafGeom {
    /// 把窗口坐标 → 叶子内**字符偏移**（含行尾：落在最后一个字之后返回 len）。
    pub fn char_at(&self, point: &gpui::Point<Pixels>) -> usize {
        let Some(wl) = &self.layout else {
            return 0;
        };
        let rel = gpui::point(point.x - self.left, point.y - self.top);
        let byte = match wl.index_for_position(rel, self.line_height) {
            Ok(b) => b,
            Err(b) => b,
        };
        char_at_byte(&self.text, byte)
    }
}

/// 把一条回答的选区（文本流内偏移）还原成保留 markdown 格式的源码。
///
/// **字符级裁剪**：选中跨多个叶子时按渲染顺序以换行拼接；叶子内逐 span 裁剪——
/// 一整段 span 被完整选中才还原其标记（`**加粗**`/`` `code` ``），只选中的一部分
/// 输出纯文本子串，避免出现 `**半截**`。
#[allow(dead_code)]
pub fn selected_markdown(md: &str, selection: &Range<usize>) -> String {
    let blocks = parse_markdown(md);
    let mut out = String::new();
    let mut stream = 0usize;
    for_each_text_leaf(&blocks, |spans| {
        let len = spans.iter().map(|s| s.text.chars().count()).sum::<usize>();
        let end_at = stream + len;
        let (s, e) = (selection.start.max(stream), selection.end.min(end_at));
        if s < e {
            let (ls, le) = (s - stream, e - stream);
            if !out.is_empty() {
                out.push('\n');
            }
            // 叶子内按 span 逐个裁剪
            let mut si = 0usize;
            for sp in spans {
                let sp_len = sp.text.chars().count();
                let se = si + sp_len;
                let (os, oe) = (ls.max(si), le.min(se));
                if os < oe {
                    let sub: String = sp
                        .text
                        .chars()
                        .skip(os - si)
                        .take(oe - os)
                        .collect();
                    if os == si && oe == se {
                        // 整段 span 完整选中 → 还原标记
                        out.push_str(&span_markdown(sp, &sub));
                    } else {
                        out.push_str(&sub);
                    }
                }
                si = se;
            }
        }
        stream = end_at;
    });
    // 流里的断行零宽空格（break_long_runs 为代码/长词换行插入）不属于内容，剔除
    out.replace('\u{200b}', "")
}

/// 单个片段 → markdown 源码（`text` 为最终要写入的内容）。
#[allow(dead_code)]
fn span_markdown(sp: &Span, text: &str) -> String {
    if sp.code {
        format!("`{}`", text)
    } else if sp.link || sp.bold || sp.italic || sp.strike {
        let (left, right) = if sp.bold {
            ("**", "**")
        } else if sp.italic {
            ("*", "*")
        } else if sp.strike {
            ("~~", "~~")
        } else {
            ("", "")
        };
        format!("{left}{text}{right}")
    } else {
        text.to_owned()
    }
}

/// 按渲染顺序遍历一条回答的每个文本叶子（顺序 = 文本流顺序）。
#[allow(dead_code)]
fn for_each_text_leaf(blocks: &[Block], mut f: impl FnMut(&[Span])) {
    for block in blocks {
        match block {
            Block::Paragraph(lines) => {
                for line in lines {
                    f(line);
                }
            }
            Block::Heading { spans, .. } => f(spans),
            Block::Code(code) => {
                for line in code.split('\n') {
                    // 与渲染端一致：代码行经 break_long_runs 插零宽断行点（否则流偏移错位）
                    f(&[Span {
                        text: break_long_runs(line),
                        bold: false,
                        italic: false,
                        strike: false,
                        code: false,
                        link: false,
                    }]);
                }
            }
            Block::List { items, .. } => {
                for item in items {
                    f(item);
                }
            }
            Block::Table { headers, rows } => {
                for cell in headers {
                    f(cell);
                }
                for row in rows {
                    for cell in row {
                        f(cell);
                    }
                }
            }
            Block::Quote(_) | Block::Hr => {}
        }
    }
}

/// 可选择的单行富文本叶子。
///
/// 布局/绘制与 `StyledText` 等价：量布局时用"逻辑行高"（`text_style.line_height`，
/// 与绘制阶段一致）统一，先 shape 出可视行数再报尺寸；prepaint 排一次(更新几何供
/// 命中测试)，paint 画选区高亮 + 文本。
struct SelectText {
    data: SelectLeafData,
    ctx: Rc<SelectCtx>,
}

/// 叶子数据：文本 + 运行 + 在该条回答文本流中的起始字符偏移。
struct SelectLeafData {
    text: SharedString,
    runs: Vec<TextRun>,
    stream_start: usize,
}

impl SelectLeafData {
    fn char_len(&self) -> usize {
        self.text.chars().count()
    }
}

impl SelectText {
    fn new(data: SelectLeafData, ctx: Rc<SelectCtx>) -> Self {
        Self { data, ctx }
    }
}

impl IntoElement for SelectText {
    type Element = SelectText;
    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for SelectText {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }
    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        _cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.min_size.width = relative(0.).into();
        let start = self.data.stream_start;
        let text = self.data.text.clone();
        let runs = self.data.runs.clone();
        let ctx = self.ctx.clone();
        let layout_id = window.request_measured_layout(style, move |known, available, window, _cx| {
            let wrap_width = known.width.or(match available.width {
                gpui::AvailableSpace::Definite(x) => Some(x),
                _ => None,
            });
            let font_size = window.text_style().font_size.to_pixels(window.rem_size());
            // 命中缓存（宽度+字号一致）→ 不重新 shape，直接用上次的行数/行高
            let cached = {
                let c = ctx.cache.borrow();
                c.get(&start)
                    .filter(|lc| lc.matches(wrap_width.unwrap_or(px(0.)), font_size, &text))
                    .map(|lc| (lc.rows, lc.line_height))
            };
            let (rows, lh) = match cached {
                Some((r, l)) => (r, l),
                None => {
                    // 首帧兜底：现场 shape（行高用与绘制一致的逻辑值，避免底部空白）
                    let rows = shape_rows(&text, font_size, &runs, wrap_width, window);
                    (rows, window.line_height())
                }
            };
            let w = wrap_width.unwrap_or(px(0.));
            size(w, lh * rows as f32)
        });
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        let start = self.data.stream_start;
        let font_size = window.text_style().font_size.to_pixels(window.rem_size());
        let line_height = window.line_height();
        let wrap = bounds.size.width;
        let text = &self.data.text;
        let runs = &self.data.runs;

        // 布局没变：只更新屏上坐标（滚动时上下移动），跳过 shape 与几何计算
        {
            let c = self.ctx.cache.borrow_mut();
            let reuse = c.get(&start).filter(|lc| lc.matches(wrap, font_size, text.as_ref()));
            if let Some(lc) = reuse {
                let lh = lc.line_height;
                let mut g = self.ctx.geom.borrow_mut();
                if let Some(gm) = g.get_mut(&start) {
                    gm.left = bounds.left();
                    gm.top = bounds.top();
                } else {
                    g.insert(
                        start,
                        LeafGeom {
                            left: bounds.left(),
                            top: bounds.top(),
                            line_height: lh,
                            text: text.clone(),
                            layout: lc.shaped.clone(),
                        },
                    );
                }
                let _ = cx;
                return;
            }
        }

        // 需要重新 shape（宽度/字号变化或首帧）：排一次并缓存 WrappedLine + 几何
        let shaped = window
            .text_system()
            .shape_text(text.clone(), font_size, runs, Some(wrap), None)
            .ok()
            .and_then(|v| v.into_iter().next());
        let rows_count = shaped
            .as_ref()
            .map(|wl| wl.wrap_boundaries().len() + 1)
            .unwrap_or(1);
        self.ctx.cache.borrow_mut().insert(
            start,
            LeafCache {
                text: text.clone(),
                font_size,
                line_height,
                wrap,
                rows: rows_count,
                shaped: shaped.clone(),
            },
        );
        self.ctx.geom.borrow_mut().insert(
            start,
            LeafGeom {
                left: bounds.left(),
                top: bounds.top(),
                line_height,
                text: text.clone(),
                layout: shaped,
            },
        );
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut (),
        _prepaint: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        let start = self.data.stream_start;
        let len = self.data.char_len();
        // 选区重叠（流内 → 叶子内）
        let sel = self.ctx.selection.borrow().clone();
        let (local_start, local_end) = match sel {
            Some(r) => (
                r.start.saturating_sub(start).min(len),
                r.end.saturating_sub(start).min(len),
            ),
            None => (0, 0),
        };
        // 高亮：按可视行切片，x 用 GPUI `position_for_index`（行内相对、自动处理行尾）
        if local_start < local_end {
            if let Some(geom) = self.ctx.geom.borrow().get(&start).cloned() {
                if let Some(wl) = &geom.layout {
                    let lh = geom.line_height;
                    let text = geom.text.clone();
                    let bl = byte_for_char(&text, local_start);
                    let br = byte_for_char(&text, local_end);
                    let boundaries = wl.wrap_boundaries();
                    let rows_count = boundaries.len() + 1;
                    for r in 0..rows_count {
                        let rs = if r == 0 {
                            0
                        } else {
                            boundary_byte(wl, r - 1)
                        };
                        let re = if r == rows_count - 1 {
                            wl.len()
                        } else {
                            boundary_byte(wl, r)
                        };
                        if bl < re && br > rs {
                            let x0 = if bl <= rs {
                                px(0.)
                            } else {
                                wl.position_for_index(bl, lh).map(|p| p.x).unwrap_or_default()
                            };
                            let x1 = if br >= re {
                                wl.position_for_index(re, lh).map(|p| p.x).unwrap_or_default()
                            } else {
                                wl.position_for_index(br, lh).map(|p| p.x).unwrap_or_default()
                            };
                            if x1 > x0 {
                                window.paint_quad(
                                    gpui::fill(
                                        Bounds::new(
                                            point(geom.left + x0, geom.top + lh * r as f32),
                                            size(x1 - x0, lh),
                                        ),
                                        rgba(0x93c5fd80),
                                    )
                                    .corner_radii(px(1.)),
                                );
                            }
                        }
                    }
                }
            }
        }
        // 文本：复用缓存里的 WrappedLine，不再每帧 shape
        let (shaped, line_height) = {
            let c = self.ctx.cache.borrow();
            match c.get(&start) {
                Some(lc) if lc.shaped.is_some() => (lc.shaped.clone(), lc.line_height),
                _ => {
                    let font_size = window.text_style().font_size.to_pixels(window.rem_size());
                    (
                        window
                            .text_system()
                            .shape_text(
                                self.data.text.clone(),
                                font_size,
                                &self.data.runs,
                                Some(bounds.size.width),
                                None,
                            )
                            .ok()
                            .and_then(|v| v.into_iter().next()),
                        window.line_height(),
                    )
                }
            }
        };
        if let Some(wl) = shaped {
            let _ = wl.paint(
                bounds.origin,
                line_height,
                gpui::TextAlign::Left,
                Some(bounds),
                window,
                cx,
            );
        }
    }
}

/// 第 i 条 wrap 边界处的**字节偏移**（下一行的起始字节）。
fn boundary_byte(wl: &WrappedLine, i: usize) -> usize {
    wl.wrap_boundaries()
        .get(i)
        .map(|b| {
            wl.unwrapped_layout.runs[b.run_ix]
                .glyphs
                .get(b.glyph_ix)
                .map(|g| g.index)
                .unwrap_or(wl.len())
        })
        .unwrap_or(wl.len())
}

/// 字符偏移 → 字节偏移
fn byte_for_char(text: &str, char_idx: usize) -> usize {
    text.char_indices()
        .nth(char_idx)
        .map(|(i, _)| i)
        .unwrap_or(text.len())
}

/// 字节偏移 → 字符偏移
fn char_at_byte(text: &str, byte: usize) -> usize {
    text[..byte.min(text.len())].chars().count()
}

/// 单逻辑行 shape 返回可视行数
fn shape_rows(
    text: &str,
    font_size: Pixels,
    runs: &[TextRun],
    wrap_width: Option<Pixels>,
    window: &mut Window,
) -> usize {
    window
        .text_system()
        .shape_text(SharedString::from(text.to_owned()), font_size, runs, wrap_width, None)
        .ok()
        .and_then(|v| v.into_iter().next())
        .map(|wl| wl.wrap_boundaries().len() + 1)
        .unwrap_or(1)
}

/// 一个片段序列的字符总数
/// 生成一个可选中的文本叶子：内部是 [`SelectText`] 自绘元素，外层是一个带命中
/// 测试与拖选的状态化 div。选区写入共享的 `ctx`（文本流内偏移）。
fn render_selectable_spans(
    spans: &[Span],
    theme: &MarkdownTheme,
    force_bold: bool,
    sel: &Rc<SelectCtx>,
    stream: &mut usize,
) -> impl IntoElement {
    let data = SelectLeafData {
        text: SharedString::from(spans.iter().map(|s| s.text.as_str()).collect::<String>()),
        runs: spans_to_styled(spans, theme, force_bold).runs,
        stream_start: *stream,
    };
    let len = data.char_len();
    *stream += len;

    let ctx = sel.clone();
    let start = data.stream_start;
    let id = ElementId::Name(SharedString::from(format!("sel-{}-{}", sel.turn, start)));

    div()
        .id(id)
        .min_w(px(0.))
        .cursor(CursorStyle::IBeam)
        .on_mouse_down(MouseButton::Left, {
            let ctx = ctx.clone();
            move |ev, window, _cx| {
                // 把键盘焦点移交根视图：之后 Ctrl+C 由"复制选区"处理，而非输入框
                window.focus(&ctx.focus);
                let Some(g) = ctx.geom.borrow().get(&start).cloned() else {
                    return;
                };
                let idx = g.char_at(&ev.position);
                let global = start + idx;
                let changed = *ctx.selection.borrow() != Some(global..global);
                *ctx.anchor.borrow_mut() = Some(global);
                *ctx.selection.borrow_mut() = Some(global..global);
                if changed {
                    window.refresh();
                }
            }
        })
        .on_mouse_move(
            {
                let ctx = ctx.clone();
                move |ev, window, _cx| {
                    let Some(anchor) = *ctx.anchor.borrow() else {
                        return;
                    };
                    let Some(g) = ctx.geom.borrow().get(&start).cloned() else {
                        return;
                    };
                    let idx = g.char_at(&ev.position);
                    let cur = start + idx;
                    let (a, b) = (anchor.min(cur), anchor.max(cur));
                    let changed = *ctx.selection.borrow() != Some(a..b);
                    if changed {
                        *ctx.selection.borrow_mut() = Some(a..b);
                        // 拖动过程中立即重绘，让选区高亮实时跟随鼠标
                        window.refresh();
                    }
                }
            },
        )
        .on_mouse_up(MouseButton::Left, {
            let ctx = ctx.clone();
            move |_ev, _window, _cx| {
                *ctx.anchor.borrow_mut() = None;
            }
        })
        .child(SelectText::new(data, ctx))
}


fn render_spans_line(spans: &[Span], theme: &MarkdownTheme) -> StyledText {
    render_spans_line_weighted(spans, theme, false)
}

/// 渲染一行行内片段，可整体强制加粗（用于表头单元格）。
fn render_spans_line_weighted(spans: &[Span], theme: &MarkdownTheme, force_bold: bool) -> StyledText {
    spans_to_styled(spans, theme, force_bold).into_element()
}

fn render_block(
    block: &Block,
    theme: &MarkdownTheme,
    mode: TableMode,
    scroll: Option<&TableScrollState>,
    window: Option<&mut Window>,
    sel: Option<&Rc<SelectCtx>>,
    stream: &mut usize,
    table_idx: &mut usize,
) -> AnyElement {
    match block {
        Block::Paragraph(lines) => {
            let mut d = div().min_w(px(0.));
            for line in lines {
                if let Some(sel) = sel {
                    d = d.child(render_selectable_spans(line, theme, false, sel, stream));
                } else {
                    d = d.child(render_spans_line(line, theme));
                }
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
            let st = if let Some(sel) = sel {
                render_selectable_spans(spans, theme, false, sel, stream).into_any_element()
            } else {
                spans_to_styled(spans, theme, false).into_element().into_any_element()
            };
            div()
                .mb_2()
                .mt_1()
                .min_w(px(0.))
                .text_size(size)
                .font_weight(FontWeight::BOLD)
                .text_color(theme.heading)
                .child(st)
                .into_any_element()
        }
        Block::Code(code) => {
            let mut child = div().min_w(px(0.));
            for line in code.split('\n') {
                let broken = break_long_runs(line);
                if let Some(sel) = sel {
                    // 代码行也纳入可选（同一文本流）
                    let fake = &[Span {
                        text: broken.clone(),
                        bold: false,
                        italic: false,
                        strike: false,
                        code: false,
                        link: false,
                    }];
                    child = child.child(render_selectable_spans(fake, theme, false, sel, stream));
                } else {
                    let st = SpannedText {
                        text: SharedString::from(broken.clone()),
                        runs: vec![TextRun {
                            len: broken.len(),
                            font: font(theme.mono_family.clone()),
                            color: theme.code_text,
                            background_color: None,
                            underline: None,
                            strikethrough: None,
                        }],
                    };
                    child = child.child(st.into_element());
                }
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
            let mut d = div().mb_2().min_w(px(0.));
            for (i, item) in items.iter().enumerate() {
                let marker = if *ordered {
                    format!("{}. ", i + 1)
                } else {
                    "• ".to_string()
                };
                let mut row = div().flex().flex_row().min_w(px(0.));
                row = row.child(
                    div()
                        .min_w(px(0.))
                        .child(
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
                // 条目文本放进可收缩容器，超长不可断片段靠 break_long_runs 换行
                if let Some(sel) = sel {
                    row = row.child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .child(render_selectable_spans(item, theme, false, sel, stream)),
                    );
                } else {
                    row = row.child(div().flex_1().min_w(px(0.)).child(render_spans_line(item, theme)));
                }
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
                            let st = spans_to_styled(spans, theme, false);
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
            // （捕获 sel/stream 使其为 FnMut）
            let mut render_cell = |c: usize,
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
                    let force = header;
                    if let Some(sel) = sel {
                        cell = cell.child(render_selectable_spans(spans, theme, force, sel, stream));
                    } else if force {
                        // 表头：强行使 run 加粗（仅 div.font_weight 不会级联进带显式 font 的 run）
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

/// 把 Markdown 源文本还原成便于复制/粘贴的**纯文本**（去标记语法，保留结构）：
/// 渲染为轻量 Markdown 的入口（用亮色主题；表格默认"自动换行"模式）。
#[allow(dead_code)]
pub fn render_markdown(md: &str) -> Div {
    render_markdown_with_options(md, &MarkdownTheme::light(), TableMode::Wrap, None, None, None)
}

/// 使用自定义主题渲染 Markdown。
#[allow(dead_code)]
pub fn render_markdown_themed(md: &str, theme: &MarkdownTheme) -> Div {
    render_markdown_with_options(md, theme, TableMode::Wrap, None, None, None)
}

/// 渲染已解析的块列表为 GPUI 元素（不含解析步骤，供调用方复用缓存的 `Vec<Block>`）。
///
/// 与 [`render_markdown_with_options`] 的渲染逻辑完全一致，只是块列表由调用方提供，
/// 避免每次重渲染都重复解析（大文档实时预览时按内容缓存 `Vec<Block>` 可显著降耗）。
pub(crate) fn render_markdown_blocks(
    blocks: &[Block],
    theme: &MarkdownTheme,
    mode: TableMode,
    scroll: Option<&TableScrollState>,
    window: Option<&mut Window>,
    sel: Option<&Rc<SelectCtx>>,
) -> Div {
    let mut table_idx = 0usize;
    let mut stream = 0usize;
    let mut window = window;
    let children: Vec<AnyElement> = blocks
        .iter()
        .map(|b| {
            render_block(
                b,
                theme,
                mode,
                scroll,
                window.as_deref_mut(),
                sel,
                &mut stream,
                &mut table_idx,
            )
        })
        .collect();
    div()
        .flex()
        .flex_col()
        .min_w(px(0.))
        .text_color(theme.text)
        .children(children)
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
    sel: Option<&Rc<SelectCtx>>,
) -> Div {
    let blocks = parse_markdown(md);
    render_markdown_blocks(&blocks, theme, mode, scroll, window, sel)
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
        let bold = spans_to_styled(&spans, &theme, true);
        let normal = spans_to_styled(&spans, &theme, false);
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

    /// 超长不可断片段（长 URL/长串）插入零宽空格以便换行；普通文本不受影响。
    #[test]
    fn break_long_runs_only_splits_long_unbroken_runs() {
        // 普通句子（含空格/标点/中文，本身有断点）→ 原样保留
        assert_eq!(break_long_runs("你好，这是普通句子，带 空格。"), "你好，这是普通句子，带 空格。");
        // 短单词 → 不插入
        assert_eq!(break_long_runs("short"), "short");
        // 60 个连续字母 → 在 28/42/56 处插入 3 个零宽空格
        let long = "a".repeat(60);
        let broken = break_long_runs(&long);
        assert_eq!(
            broken.chars().filter(|&c| c == '\u{200b}').count(),
            3,
            "60 个连续字母应插入 3 个断点：{broken:?}"
        );
        // 带斜杠/点号的 URL：只在超长单段里插入
        let url = format!("https://example.com/{}", "b".repeat(60));
        let broken_url = break_long_runs(&url);
        assert!(
            broken_url.chars().filter(|&c| c == '\u{200b}').count() >= 1,
            "长地址段应插入断点：{broken_url:?}"
        );
    }

    /// 横向滚动模式下，超宽表格必须真实溢出容器（handle.max_offset.width>0），
    /// 选区复制：整篇选中时按叶还原标记（加粗/行内代码），跨块以换行拼接。
    #[test]
    fn selected_markdown_preserves_markup() {
        let md = "# 标题\n\n**加粗** 和 `code`\n\n- 甲\n- 乙";
        let blocks = parse_markdown(md);
        let mut total = 0usize;
        for_each_text_leaf(&blocks, |spans| {
            total += spans.iter().map(|s| s.text.chars().count()).sum::<usize>();
        });
        let out = selected_markdown(md, &(0..total));
        assert!(out.contains("**加粗**"), "加粗标记要保留: {out}");
        assert!(out.contains("`code`"), "行内 code 标记要保留: {out}");
        assert!(out.contains("标题"));
        assert!(out.contains("甲"));
        assert!(out.contains("乙"));
    }

    /// 选区只覆盖部分片段：按片段裁剪，未被选中的片段不进入结果。
    #[test]
    fn selected_markdown_partial_selection() {
        let md = "一二三\n\n**甲**乙";
        let out = selected_markdown(md, &(0..3));
        assert!(out.contains("一二三"));
        assert!(!out.contains("甲"), "选中只在一段时不应带出后面的片段: {out}");
    }

    /// 字符级裁剪：只选中加粗串的一部分时，不还原 `**`（避免半截标记）。
    #[test]
    fn selected_markdown_char_precision() {
        // "**加粗加粗**" -> 叶子文本流 = "加粗加粗"（4 个字符）
        let md = "**加粗加粗**";
        // 选中间"粗加"（字符 1..3）
        let out = selected_markdown(md, &(1..3));
        assert_eq!(out, "粗加", "部分选中应只复制所选子串: {out}");
        assert!(!out.contains("**"));
        assert!(!out.contains("加粗加粗"));
        // 整段全选 → 还原标记
        let whole = selected_markdown(md, &(0..4));
        assert_eq!(whole, "**加粗加粗**");
    }

    /// 跨多个 span：完整选中的 span 保留标记，部分选中的只取纯文本。
    #[test]
    fn selected_markdown_mixed_spans() {
        let md = "**粗**加";
        // 选"粗加"（字符 0..2）：粗 span 完整 → **粗**；加 span 完整 → 加
        let out = selected_markdown(md, &(0..2));
        assert_eq!(out, "**粗**加");
        // 只选第一个"粗"的后半→实际选"粗"（0..1）：完整选粗 span → **粗**
        let only_bold = selected_markdown(md, &(0..1));
        assert_eq!(only_bold, "**粗**");
    }

    /// 行尾最后一个字也要能被选中/复制：命中测试在点击位置超过行尾时返回叶子长度，
    /// 前后缀也不会丢字。用真实 GPUI 排版验证 `index_for_position` 语义。
    #[gpui::test]
    async fn leaf_hit_test_covers_line_end_char(cx: &mut gpui::TestAppContext) {
        struct Probe;
        impl gpui::Render for Probe {
            fn render(
                &mut self,
                window: &mut gpui::Window,
                _cx: &mut gpui::Context<Self>,
            ) -> impl gpui::IntoElement {
                let text = "SD-WAN组网建设";
                let runs = vec![TextRun {
                    len: text.len(),
                    font: font(".SystemUIFont"),
                    color: rgb(0x000000).into(),
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                }];
                let wl = window
                    .text_system()
                    .shape_text(text.into(), px(16.), &runs, Some(px(400.)), None)
                    .unwrap()
                    .into_iter()
                    .next()
                    .unwrap();
                let geom = LeafGeom {
                    left: px(0.),
                    top: px(0.),
                    line_height: window.line_height(),
                    text: text.into(),
                    layout: Some(wl),
                };
                let len = text.chars().count();
                // 点击远超行尾 → 结尾（len）
                assert_eq!(
                    geom.char_at(&gpui::point(px(1000.), px(1.))),
                    len,
                    "行尾最后一个字必须可选"
                );
                // 点击超出左缘 → 开头
                assert_eq!(geom.char_at(&gpui::point(px(-1.), px(1.))), 0);
                // 点击中段 → 落在范围内
                let mid = geom.char_at(&gpui::point(px(30.), px(1.)));
                assert!(mid > 0 && mid <= len);
                div().id("probe")
            }
        }
        let _view = cx.add_window_view(|_w, _c| Probe);
        for _ in 0..3 {
            cx.run_until_parked();
        }
    }

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
                        None,
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
        // 点击横向滚动条应能"跳转/拖拽"到对应位置：横向偏移从 0 变成负值。
        // 滚动条是贴在滚动容器底部的高 3px 细条，点击点必须落在 [bottom, bottom+3px] 内。
        let (bar_left, bar_bottom, viewport_w) = cx.update(|_window, cx| {
            let b = view.read(cx).state.handle.bounds();
            (b.left(), b.bottom(), b.size.width)
        });
        cx.simulate_click(
            gpui::point(bar_left + viewport_w * 0.7, bar_bottom + px(1.5)),
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
