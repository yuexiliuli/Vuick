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
use gpui_component::input::Rope;
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

/// 渲染行对齐方式（EPUB 排版指令 `%%ALIGN:X%%` 驱动）：None = 跟随默认左对齐。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowAlign {
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
    /// 代码块内所有源码行（逐行渲染成连续块）+ 围栏语言标记（用于语法着色；
    /// `None` = 无语言标记，按纯文本渲染）。
    Code {
        code_lines: Vec<SharedString>,
        language: Option<SharedString>,
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
    /// 行对齐（EPUB 排版指令 `%%ALIGN:X%%`；None = 默认左对齐）。
    pub align: Option<RowAlign>,
    /// 行内注记标记：`(字节区间, 注记号)`（EPUB 脚注 `[^n]` 的悬浮卡片/跳转用）。
    /// 区间相对 `text`；标记显示成 `[n]`（`^` 已剥掉）。
    pub notes: Vec<(Range<usize>, usize)>,
    /// 注释**定义**标记：`(字节区间, 注记号)`（EPUB「注释」章 `p.note` 的 `[!n]`，
    /// 逆跳到文内对应位置用）。显示成 `[n]`（`!` 已剥掉）。
    pub note_defs: Vec<(Range<usize>, usize)>,
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
            align: None,
            notes: Vec::new(),
            note_defs: Vec::new(),
        }
    }

    /// 记录该渲染行对应的源码起始行号（解析时填充）。
    fn with_line(mut self, line: usize) -> Self {
        self.src_line = line;
        self
    }

    /// 记录行对齐 + 行内注记标记 + 注释定义标记（解析时填充）。
    fn with_format(
        mut self,
        align: Option<RowAlign>,
        notes: Vec<(Range<usize>, usize)>,
        note_defs: Vec<(Range<usize>, usize)>,
    ) -> Self {
        self.align = align;
        self.notes = notes;
        self.note_defs = note_defs;
        self
    }

    /// 代码块行：整个围栏折叠成一个 [RowKind::Code] 渲染行（逐行渲染成连续块）。
    /// `language`：围栏首行语言标记（如 ` ```rust ` → `"rust"`），None 表示无标记。
    fn code_block(lines: Vec<SharedString>, language: Option<SharedString>) -> Self {
        let text = lines.first().cloned().unwrap_or_default();
        Self {
            text,
            kind: RowKind::Code,
            src_line: 0,
            runs: Vec::new(),
            prefix: None,
            payload: Some(Box::new(RowPayload::Code {
                code_lines: lines,
                language,
            })),
            align: None,
            notes: Vec::new(),
            note_defs: Vec::new(),
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
            align: None,
            notes: Vec::new(),
            note_defs: Vec::new(),
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
            align: None,
            notes: Vec::new(),
            note_defs: Vec::new(),
        }
    }

    /// 代码块的所有源码行（非 [`RowKind::Code`] 行返回空切片）。
    pub fn code_lines(&self) -> &[SharedString] {
        match &self.payload {
            Some(p) => match &**p {
                RowPayload::Code { code_lines, .. } => code_lines,
                _ => &[],
            },
            None => &[],
        }
    }

    /// 代码块的语言标记（围栏首行信息串，如 ` ```rust ` → "rust"；
    /// 无标记或非代码行返回 `None`）。用于预览代码块的语法着色。
    pub fn code_language(&self) -> Option<&str> {
        match &self.payload {
            Some(p) => match &**p {
                RowPayload::Code { language, .. } => language.as_deref(),
                _ => None,
            },
            None => None,
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
    let (out, runs, _map) = inline_style_mapped(src);
    (out, runs)
}

/// [`inline_style`] 的映射变体：额外返回 **渲染字节 → 源字节** 的映射表，供
/// 实时预览编辑（WYSIWYG）把「渲染位置 → 光标落点的源码字节」换算。
///
/// `map` 长度恒为 `out.len()+1`：`map[r]` = 渲染文本第 `r` 个字节对应的源字节
/// （相对 `src`），`map[out.len()]` = `src.len()`（行尾哨兵）。
fn inline_style_mapped(
    src: &str,
) -> (SharedString, Vec<(Range<usize>, RunKind)>, Vec<usize>) {
    let mut out = String::with_capacity(src.len());
    let mut runs = Vec::new();
    let mut map: Vec<usize> = Vec::with_capacity(src.len() + 1);
    let mut last = 0usize;
    for cap in inline_styles_re().captures_iter(src) {
        let m = cap.get(0).expect("整体匹配必然存在");
        let span = m.range();
        // 记号前的普通文本：直拷，map 一一对应。
        for (i, _) in src[last..span.start].bytes().enumerate() {
            map.push(last + i);
        }
        out.push_str(&src[last..span.start]);
        // 记号包裹的内容（inner 的源字节区间由命名捕获给出——正则捕获的
        // start/end 是原串字节偏移）。
        let (inner_s, inner, kind) = if let Some(c) = cap.name("code") {
            (c.start(), c.as_str(), RunKind::InlineCode)
        } else if let Some(c) = cap.name("bold") {
            (c.start(), c.as_str(), RunKind::Bold)
        } else if let Some(c) = cap.name("strike") {
            (c.start(), c.as_str(), RunKind::Strikethrough)
        } else if let Some(c) = cap.name("em") {
            (c.start(), c.as_str(), RunKind::Italic)
        } else if let Some(c) = cap.name("img") {
            (c.start(), c.as_str(), RunKind::Image)
        } else if let Some(c) = cap.name("link") {
            (c.start(), c.as_str(), RunKind::Link)
        } else {
            unreachable!("inline_styles_re 的匹配必然落入某个命名捕获组")
        };
        for i in 0..inner.len() {
            map.push(inner_s + i);
        }
        let start = out.len();
        out.push_str(inner);
        runs.push((start..out.len(), kind));
        last = span.end;
    }
    for (i, _) in src[last..].bytes().enumerate() {
        map.push(last + i);
    }
    out.push_str(&src[last..]);
    map.push(src.len());
    (out.into(), runs, map)
}

/// 解析行首的 EPUB 对齐指令 `%%ALIGN:X%%`（仅 EPUB 转换器产出；普通正文不会出现）。
/// 命中返回 `(Some(对齐), 指令剥掉后的剩余行)`，否则 `(None, 原行)`。
/// 指令与内容**同一行**（`%%ALIGN:RIGHT%%内容`）——这样该行仍是一个普通块，
/// `scan_blocks`/增量/窗口切片不需要任何额外坐标处理，块数与字节边界天然一致。
fn strip_align_directive(line: &str) -> (Option<RowAlign>, &str) {
    const PREFIX: &str = "%%ALIGN:";
    let Some(rest) = line.strip_prefix(PREFIX) else {
        return (None, line);
    };
    let Some(close) = rest.find("%%") else {
        return (None, line);
    };
    let name = &rest[..close];
    let align = match name {
        "LEFT" => Some(RowAlign::Left),
        "CENTER" => Some(RowAlign::Center),
        "RIGHT" => Some(RowAlign::Right),
        _ => return (None, line),
    };
    (align, &rest[close + 2..])
}

/// 把行内脚注标记从文本中识别出来：
/// - `[^<数字>]`：正文脚注引用（`Row.notes`，悬浮/跳到注释章）；
/// - `[!<数字>]`：注释**定义**（`Row.note_defs`，跳到文内对应位置）。
/// 显示文本都剥掉标记字符（`[^1]`/`[!1]` → `[1]`），记录 `(字节区间, 注记号)`；
/// marker 之后的既有样式段区间整体左移 1 字节（被剥字符删掉）。
fn extract_notes(
    text: SharedString,
    mut runs: Vec<(Range<usize>, RunKind)>,
) -> (
    SharedString,
    Vec<(Range<usize>, RunKind)>,
    Vec<(Range<usize>, usize)>,
    Vec<(Range<usize>, usize)>,
) {
    let src = text.as_ref();
    let mut out = String::with_capacity(src.len());
    let mut notes: Vec<(Range<usize>, usize)> = Vec::new();
    let mut note_defs: Vec<(Range<usize>, usize)> = Vec::new();
    let chars: Vec<(usize, char)> = src.char_indices().collect();
    let mut ci = 0usize;
    while ci < chars.len() {
        let (idx, ch) = chars[ci];
        if ch == '[' {
            // 形如 `[^digits]` 或 `[!digits]`
            let is_note = chars.get(ci + 1).is_some_and(|&(_, c)| c == '^');
            let is_def = chars.get(ci + 1).is_some_and(|&(_, c)| c == '!');
            if is_note || is_def {
                let mut nxt = ci + 2;
                let dstart = nxt;
                while let Some(&(_, d)) = chars.get(nxt) {
                    if d.is_ascii_digit() {
                        nxt += 1;
                    } else {
                        break;
                    }
                }
                if nxt > dstart && chars.get(nxt).is_some_and(|&(_, c)| c == ']') {
                    let digits: String =
                        src[chars[dstart].0..chars[nxt - 1].0 + 1].chars().collect();
                    let n: usize = digits.parse().unwrap_or(0);
                    let mark_start = out.len();
                    out.push('[');
                    out.push_str(&digits);
                    out.push(']');
                    let mark_end = out.len();
                    let entry = (mark_start..mark_end, n);
                    if is_def {
                        note_defs.push(entry);
                    } else {
                        notes.push(entry);
                    }
                    // 标记字符被删 → marker 之后的区间整体左移 1 字节。
                    let cut = idx;
                    for r in runs.iter_mut() {
                        if r.0.start > cut {
                            r.0.start -= 1;
                            r.0.end -= 1;
                        }
                    }
                    ci = nxt + 1;
                    continue;
                }
            }
        }
        out.push(ch);
        ci += 1;
    }
    (out.into(), runs, notes, note_defs)
}

/// [`extract_notes`] 的映射变体：剥 `[^n]`/`[!n]` 的 `^`/`!` 时同步调整渲染→源字节
/// 映射。`base_map` 是当前文本的**字节索引**渲染→源字节表（如 [`inline_style_mapped`]
/// 产出，长度 = 文本字节长 + 1）。剥掉 `^`/`!`（恰好各 1 字节）即从 `base_map` 删除
/// 对应项（删除项及其后整体左移，与渲染文本的横向移动一致）；返回的 map 覆盖剥
/// 标记后的最终文本，仍为字节索引。
#[allow(clippy::type_complexity)]
fn extract_notes_mapped(
    text: SharedString,
    mut runs: Vec<(Range<usize>, RunKind)>,
    base_map: Vec<usize>,
) -> (
    SharedString,
    Vec<(Range<usize>, RunKind)>,
    Vec<(Range<usize>, usize)>,
    Vec<(Range<usize>, usize)>,
    Vec<usize>,
) {
    let src = text.as_ref();
    let mut out = String::with_capacity(src.len());
    let mut notes: Vec<(Range<usize>, usize)> = Vec::new();
    let mut note_defs: Vec<(Range<usize>, usize)> = Vec::new();
    let mut removed: Vec<usize> = Vec::new(); // 被剥字节的 map 下标（`^`/`!`）
    let chars: Vec<(usize, char)> = src.char_indices().collect();
    let mut ci = 0usize;
    while ci < chars.len() {
        let (idx, ch) = chars[ci];
        if ch == '[' {
            // 形如 `[^digits]` 或 `[!digits]`
            let is_note = chars.get(ci + 1).is_some_and(|&(_, c)| c == '^');
            let is_def = chars.get(ci + 1).is_some_and(|&(_, c)| c == '!');
            if is_note || is_def {
                let mut nxt = ci + 2;
                let dstart = nxt;
                while let Some(&(_, d)) = chars.get(nxt) {
                    if d.is_ascii_digit() {
                        nxt += 1;
                    } else {
                        break;
                    }
                }
                if nxt > dstart && chars.get(nxt).is_some_and(|&(_, c)| c == ']') {
                    let digits: String =
                        src[chars[dstart].0..chars[nxt - 1].0 + 1].chars().collect();
                    let n: usize = digits.parse().unwrap_or(0);
                    let mark_start = out.len();
                    out.push('[');
                    out.push_str(&digits);
                    out.push(']');
                    let mark_end = out.len();
                    let entry = (mark_start..mark_end, n);
                    if is_def {
                        note_defs.push(entry);
                    } else {
                        notes.push(entry);
                    }
                    // 删掉 `^`/`!`（在 `[` 之后第 1 字节）→ map 删除该项。
                    removed.push(idx + 1);
                    // 标记字符被删 → marker 之后的区间整体左移 1 字节。
                    let cut = idx;
                    for r in runs.iter_mut() {
                        if r.0.start > cut {
                            r.0.start -= 1;
                            r.0.end -= 1;
                        }
                    }
                    ci = nxt + 1;
                    continue;
                }
            }
        }
        out.push(ch);
        ci += 1;
    }
    // 从字节索引映射删除被剥字节项（降序待删保下标有效）。
    let mut map = base_map;
    removed.sort_unstable();
    for del in removed.into_iter().rev() {
        if del < map.len() {
            map.remove(del);
        }
    }
    (out.into(), runs, notes, note_defs, map)
}

/// 段落文本的行首/行尾修整：剥掉 ASCII 空格/Tab 与尾部空白，但**保留行首全角空格**
/// （U+3000 在 Rust 的 `str::trim` 里也算空白——EPUB 正文首行缩进「　　」靠它实现，
/// 不能被这里吃掉）。普通 Markdown 段落行首没有全角空格，行为与以前一致。
fn trim_paragraph(line: &str) -> &str {
    line.trim_end().trim_start_matches([' ', '\t'])
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

/// 围栏首行的语言标记（信息串）：` ```rust ` → "rust"，` ~~~python extra ` → "python"。
/// 剥掉围栏字符后取第一个空白分隔的词；词尾的标点（如 `rust,`）一并剥掉，
/// 只留语言本体（`#` / `+` / `-` 等属合法字符保留）；无标记返回 `None`（纯文本代码）。
fn fence_language(t: &str) -> Option<&str> {
    let rest = t.trim_start_matches(['`', '~']).trim();
    let lang = rest.split_whitespace().next().unwrap_or("");
    let lang = lang
        .trim_start_matches('{')
        .trim_end_matches('}')
        .trim()
        .trim_end_matches(|c: char| matches!(c, ',' | ';' | '.' | ':' | '!' | '?'));
    if lang.is_empty() {
        None
    } else {
        Some(lang)
    }
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
        // EPUB 对齐指令（`%%ALIGN:X%%内容`）：同一行，剥掉后该行仍是普通块。
        let (align, line) = strip_align_directive(line);
        let t = line.trim_start();
        // 栅栏开头：收集到同字符闭合行之间的所有行，折叠成一个 Code 渲染行
        if let Some((fc, _)) = fence_len(t) {
            let mut code_lines = Vec::new();
            let language = fence_language(t).map(|s| s.into());
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
            rows.push(Row::code_block(code_lines, language).with_line(row_start));
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
            let (raw, runs) = inline_style(t[hashes..].trim_start());
            let (text, runs, notes, note_defs) = extract_notes(raw, runs);
            rows.push(
                Row::new(
                    text.as_ref(),
                    RowKind::Heading(hashes as u8),
                    runs,
                    None,
                )
                .with_line(row_start)
                .with_format(align, notes, note_defs),
            );
            idx += 1;
            continue;
        }
        // 引述：`>` 开头
        if t.starts_with('>') {
            let (content_align, rest) = strip_align_directive(t.trim_start_matches('>').trim_start());
            let align = content_align.or(align);
            let (raw, runs) = inline_style(rest);
            let (text, runs, notes, note_defs) = extract_notes(raw, runs);
            rows.push(
                Row::new(text.as_ref(), RowKind::Quote, runs, None)
                    .with_line(row_start)
                    .with_format(align, notes, note_defs),
            );
            idx += 1;
            continue;
        }
        // 列表项：`- ` / `* ` / `+ ` / `1. ` / 任务列表
        if let Some((prefix, content)) = list_prefix(t) {
            let (content_align, content) = strip_align_directive(content);
            let align = content_align.or(align);
            let (raw, runs) = inline_style(content);
            let (text, runs, notes, note_defs) = extract_notes(raw, runs);
            rows.push(
                Row::new(
                    text.as_ref(),
                    RowKind::Bullet,
                    runs,
                    Some(prefix.clone()),
                )
                .with_line(row_start)
                .with_format(align, notes, note_defs),
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
        let (raw, runs) = inline_style(trim_paragraph(line));
        let (text, runs, notes, note_defs) = extract_notes(raw, runs);
        rows.push(
            Row::new(text.as_ref(), RowKind::Paragraph, runs, None)
                .with_line(row_start)
                .with_format(align, notes, note_defs),
        );
        idx += 1;
    }
    rows
}

/// `seg` 相对 `src` 的字节偏移（供映射表换算成文档坐标）。
fn byte_offset(src: &str, seg: &str) -> usize {
    seg.as_ptr() as usize - src.as_ptr() as usize
}

/// 实时预览编辑（WYSIWYG）用的渲染行：在 [`Row`] 基础上追加**渲染位置 ↔ 源码字节**
/// 的映射，让「点击渲染文本 / 光标定位」能换算回 Buffer 的字节偏移。
#[derive(Clone, Debug)]
pub struct LiveRow {
    /// 渲染行本体（与 [`parse_to_rows`] 产出的 `Row` 完全一致）。
    pub row: Row,
    /// 该块首个源码字节（文档坐标；非文本行点击/光标落点用它）。
    pub source_start: usize,
    /// 渲染字节 → 文档源字节（`len = row.text.len()+1`，末尾为行尾哨兵）。
    /// 仅文本行（段落/标题/引述/列表）填充；代码/表格/图片/空行 = 空。
    pub map: Vec<usize>,
}

impl LiveRow {
    /// 渲染位置（字节，相对 `row.text`）→ 源码字节；非文本行返回 `source_start`。
    pub fn rendered_to_source(&self, rendered_byte: usize) -> usize {
        if self.map.is_empty() {
            return self.source_start;
        }
        let r = rendered_byte.min(self.map.len() - 1);
        self.map[r]
    }
}

/// [`parse_to_rows`] 的实时编辑变体：额外产出每行的 [`LiveRow`] 映射。
///
/// 与 [`parse_to_rows`] 用同一套块判定与行内清理（保证渲染结果一致），仅多算
/// 文档坐标的字节偏移与内联映射。供小/中文档的实时预览编辑使用（整篇解析）。
pub fn parse_to_rows_live(src: &str) -> Vec<LiveRow> {
    let line_starts = build_line_starts_u32(src);
    let lines: Vec<&str> = src.split('\n').collect();
    let mut out: Vec<LiveRow> = Vec::new();
    let mut idx = 0usize;
    while idx < lines.len() {
        let row_start = idx;
        let rl = lines[idx].trim_end_matches('\r');
        let ls = line_starts[row_start] as usize;
        let (align, line) = strip_align_directive(rl);
        let t = line.trim_start();
        // 围栏：整块折叠成一个 Code 渲染行（非文本行，点击落点 = 围栏首字节）
        if let Some((fc, _)) = fence_len(t) {
            let mut code_lines = Vec::new();
            let language = fence_language(t).map(|s| s.into());
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
            out.push(LiveRow {
                row: Row::code_block(code_lines, language).with_line(row_start),
                source_start: ls,
                map: Vec::new(),
            });
            continue;
        }
        if t.is_empty() {
            out.push(LiveRow {
                row: Row::new("", RowKind::Blank, Vec::new(), None).with_line(row_start),
                source_start: ls,
                map: Vec::new(),
            });
            idx += 1;
            continue;
        }
        // 文本行：段落/标题/引述/列表走同一映射装配（segment → LiveRow）
        let mk = |segment: &str, kind: RowKind, prefix: Option<SharedString>, align: Option<RowAlign>, row_start: usize, ls: usize| {
            let (raw, runs, seg_map) = inline_style_mapped(segment);
            let (text, runs, notes, defs, rel_map) = extract_notes_mapped(raw, runs, seg_map);
            let base = byte_offset(src, segment);
            let map = rel_map.into_iter().map(|b| base + b).collect();
            let row = Row::new(text.as_ref(), kind, runs, prefix)
                .with_line(row_start)
                .with_format(align, notes, defs);
            LiveRow { row, source_start: ls, map }
        };
        // 标题：`#{1,6} ` 后跟内容
        let hashes = t.chars().take_while(|&c| c == '#').count();
        if (1..=6).contains(&hashes) && t.len() > hashes {
            out.push(mk(t[hashes..].trim_start(), RowKind::Heading(hashes as u8), None, align, row_start, ls));
            idx += 1;
            continue;
        }
        // 引述：`>` 开头
        if t.starts_with('>') {
            let (content_align, rest) = strip_align_directive(t.trim_start_matches('>').trim_start());
            let align = content_align.or(align);
            out.push(mk(rest, RowKind::Quote, None, align, row_start, ls));
            idx += 1;
            continue;
        }
        // 列表项：`- ` / `* ` / `+ ` / `1. ` / 任务列表
        if let Some((prefix, content)) = list_prefix(t) {
            let (content_align, content) = strip_align_directive(content);
            let align = content_align.or(align);
            out.push(mk(content, RowKind::Bullet, Some(prefix.clone()), align, row_start, ls));
            idx += 1;
            continue;
        }
        // 水平分隔线
        if is_rule(t) {
            out.push(LiveRow {
                row: Row::new("", RowKind::Rule, Vec::new(), None).with_line(row_start),
                source_start: ls,
                map: Vec::new(),
            });
            idx += 1;
            continue;
        }
        // 独立图片行：`![alt](src)` 整行占位
        if let Some((alt, src)) = parse_image_line(t) {
            out.push(LiveRow {
                row: Row::new_image(&alt, &src).with_line(row_start),
                source_start: ls,
                map: Vec::new(),
            });
            idx += 1;
            continue;
        }
        // 表格：当前行是表头、下一行是分隔行
        if let Some((table, consumed)) = try_parse_table(&lines, idx) {
            out.push(LiveRow {
                row: Row::with_table(table).with_line(row_start),
                source_start: ls,
                map: Vec::new(),
            });
            idx += consumed;
            continue;
        }
        out.push(mk(trim_paragraph(line), RowKind::Paragraph, None, align, row_start, ls));
        idx += 1;
    }
    out
}

/// 渲染行的 Numeric 类型辅助：该行是否**文本行**（段落/标题/引述/列表），
/// 即可以做「渲染字节 ↔ 源码字节」字符级编辑映射的行。
pub fn is_text_kind(kind: RowKind) -> bool {
    matches!(
        kind,
        RowKind::Paragraph | RowKind::Heading(_) | RowKind::Bullet | RowKind::Quote
    )
}

/// 一个渲染行的**轻量元数据**（只存行号/类型/几个长度，不复制文本）。
///
/// 供大文档「窗口级懒解析」使用：行号索引（二分定位源码行→渲染行）、
/// 平均行高估算、目录扫描都只依赖它，不需要物化整篇 [`Row`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockMeta {
    /// 该渲染行覆盖的源码起始行（0 基，与 [`parse_to_rows`] 产出的 `Row.src_line` 一致）。
    pub src_line: u32,
    /// 块首行的**字节起始偏移**（原文坐标）：`parse_block_window` 直接用它切片到
    /// 块边界；增量扫描用它做尾部对齐与锚点定位——替代过去整篇 `line_starts` 数组
    /// （省内存，且增量扫描不再为建数组而全串扫一遍）。
    pub byte: u32,
    /// 块类型（与 [`parse_to_rows`] 产出的 `Row.kind` 一致）。
    pub kind: RowKind,
    /// 块首行的原始字节长度（估算用；表格/代码块按 `aux` 计数）。
    pub raw_len: u32,
    /// Code：代码行数；Table：数据行数；Image：1；其余：0。
    pub aux: u32,
}

/// 整篇文档的轻量块索引（≈20B/行，160k 行 ≈ 3.2MB，而整篇 `Vec<Row>` ≈ 25MB）。
#[derive(Debug, Clone)]
pub struct BlockIndex {
    /// 每个渲染行一个条目（按行序，`src_line`/`byte` 严格递增）。**分段持久化**：
    /// 增量扫描 O(1) 共享未变前后缀，只重建变化段（见 [`Blocks`]）。
    pub blocks: Blocks,
    /// 目录：(渲染行号, 标题级别, 标题文本)。窗口模式下照常全量可用。
    pub toc: Vec<(usize, u8, String)>,
}

/// 分段持久化的轻量块列表。
///
/// 背景：增量扫描每轮都要产出「旧前缀 + 新中段 + 旧尾部(平移)」——用扁平 `Vec` 时
/// 前后缀每轮整体 memcpy（16 万块 ≈ 3.2MB ≈ 一两 ms，是 rope 化后每轮唯一 O(n)）。
/// 这里把 `blocks` 拆成**只读段**：段内是 `Arc<[BlockMeta]>`（不可变共享），段携带
/// 子区间与常量坐标平移，读写只发生在段边界：
/// - 前缀复用 = 对前段做 O(1) 子区间裁剪，零拷贝；
/// - 尾部复用 = 子区间裁剪 + 叠加 (byte_delta, line_shift) 平移（各块统一平移，可在读取时算）；
/// - 只重建「变化中段」（通常极小）。
/// 段数超 [`BLOCKS_MAX_SEGS`] 时合并且 amortize：合并是 O(n)，但每数轮才一次。
/// 用 `Arc`（非 `Rc`）→ `BlockIndex` 仍 `Send`，可整份交给后台线程继续增量（顺带把
/// 同步循环里 `Rc<BlockIndex> → 后台` 的整份拷贝从 O(n) 降到 O(1) 引用共享）。
#[derive(Clone, Debug)]
pub struct Blocks {
    segs: std::sync::Arc<Vec<BlocksSeg>>,
    len: usize,
}

/// 一段不可变块元数据（`[start,end)` 于 `data`），读取时施加常量坐标平移。
#[derive(Clone, Debug)]
pub struct BlocksSeg {
    data: std::sync::Arc<[BlockMeta]>,
    start: usize,
    end: usize,
    byte_delta: i64,
    line_shift: i64,
}

/// 段数上限：超过即合并成一段（amortize，读取仍是 O(段数)，段数很小）。
const BLOCKS_MAX_SEGS: usize = 8;

impl BlocksSeg {
    fn new(data: std::sync::Arc<[BlockMeta]>) -> Self {
        let len = data.len();
        Self { data, start: 0, end: len, byte_delta: 0, line_shift: 0 }
    }
    #[inline]
    fn len(&self) -> usize {
        self.end - self.start
    }
    /// 段内第 `i` 个块的**当前值**（已施加平移）。
    #[inline]
    fn at(&self, i: usize) -> BlockMeta {
        let mut b = self.data[self.start + i];
        b.byte = (b.byte as i64 + self.byte_delta).max(0) as u32;
        b.src_line = (b.src_line as i64 + self.line_shift).max(0) as u32;
        b
    }
    /// 段内子区间 `[s,e)`（O(1)，保持原平移）。
    fn sub(&self, s: usize, e: usize) -> Self {
        debug_assert!(s <= e && e <= self.len());
        Self { data: self.data.clone(), start: self.start + s, end: self.start + e, byte_delta: self.byte_delta, line_shift: self.line_shift }
    }
    /// 叠加常量平移（与已有平移合成：`(data.byte + 旧δ) + 新δ = data.byte + (旧δ+新δ)`）。
    fn shifted(&self, byte_delta: i64, line_shift: i64) -> Self {
        let mut seg = self.clone();
        seg.byte_delta += byte_delta;
        seg.line_shift += line_shift;
        seg
    }
    fn iter(&self) -> impl Iterator<Item = BlockMeta> + '_ {
        (0..self.len()).map(move |i| self.at(i))
    }
}

impl Blocks {
    /// 由完整 `Vec` 建立（全量扫描 / 合并）：原地转成单个共享段，之后裁剪零拷贝。
    pub fn from_vec(v: Vec<BlockMeta>) -> Self {
        let len = v.len();
        Self { segs: std::sync::Arc::new(vec![BlocksSeg::new(v.into())]), len }
    }

    /// 段数（读取路径的遍历代价，通常 ≤ [`BLOCKS_MAX_SEGS`]）。测试/基准观测用。
    #[cfg(test)]
    pub fn seg_count(&self) -> usize {
        self.segs.len()
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// 第 `i` 个块的当前值（O(段数) ≤ [`BLOCKS_MAX_SEGS`]）。
    #[inline]
    pub fn at(&self, mut i: usize) -> BlockMeta {
        debug_assert!(i < self.len, "blocks.at({i}) 越界 len={}", self.len);
        for seg in self.segs.iter() {
            let l = seg.len();
            if i < l {
                return seg.at(i);
            }
            i -= l;
        }
        unreachable!("blocks.at({i}) 越界 len={}", self.len)
    }

    pub fn get(&self, i: usize) -> Option<BlockMeta> {
        if i >= self.len {
            return None;
        }
        Some(self.at(i))
    }

    pub fn iter(&self) -> BlocksIter<'_> {
        BlocksIter { blocks: self, seg_i: 0, seg_off: 0 }
    }

    /// 二分定位：`blocks[..n]` 中最后一个满足 `pred` 的块下标 + 1（块 `byte` 严格递增，
    /// 平移保持单调，等价于原 `slice::partition_point`）。
    pub fn partition_point<P: Fn(&BlockMeta) -> bool>(&self, pred: P) -> usize {
        let mut lo = 0usize;
        let mut hi = self.len;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let b = self.at(mid);
            if pred(&b) {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    /// 增量组装：前缀（共享段）+ 中段（新扫描的 owned）+ 后缀（共享段）。段数超限合并且。
    fn assemble(prefix: Vec<BlocksSeg>, middle: Vec<BlockMeta>, suffix: Vec<BlocksSeg>) -> Self {
        let mut segs = prefix;
        if !middle.is_empty() {
            segs.push(BlocksSeg::new(middle.into()));
        }
        segs.extend(suffix);
        segs.retain(|s| s.len() > 0);
        let len = segs.iter().map(|s| s.len()).sum();
        if segs.len() > BLOCKS_MAX_SEGS {
            // amortize：合并成单一共享段（O(n) 一次，之后数轮裁剪零拷贝）
            let mut flat = Vec::with_capacity(len);
            for s in &segs {
                flat.extend(s.iter());
            }
            return Self { segs: std::sync::Arc::new(vec![BlocksSeg::new(flat.into())]), len };
        }
        Self { segs: std::sync::Arc::new(segs), len }
    }

    /// 取出前 `k` 个块的共享段（O(段数)）——增量扫描复用未变前缀，零拷贝。
    fn shares_prefix(&self, k: usize) -> Vec<BlocksSeg> {
        let mut out = Vec::new();
        let mut rem = k;
        for seg in self.segs.iter() {
            if rem == 0 {
                break;
            }
            let l = seg.len();
            if rem >= l {
                out.push(seg.clone());
                rem -= l;
            } else {
                out.push(seg.sub(0, rem));
                rem = 0;
            }
        }
        out
    }

    /// 取出从第 `k` 块起的共享段并叠加平移 `(byte_delta, line_shift)`——复用旧尾部。
    fn shares_suffix(&self, k: usize, byte_delta: i64, line_shift: i64) -> Vec<BlocksSeg> {
        let mut out = Vec::new();
        let mut rem = k;
        for seg in self.segs.iter() {
            let l = seg.len();
            if rem >= l {
                rem -= l;
                continue;
            }
            let seg = seg.sub(rem, l).shifted(byte_delta, line_shift);
            out.push(seg);
            rem = 0;
        }
        out
    }
}

impl PartialEq for Blocks {
    fn eq(&self, other: &Self) -> bool {
        self.len == other.len && self.iter().eq(other.iter())
    }
}
impl Eq for Blocks {}

/// 按序产出每个块的当前值（值语义：平移段需即时计算，无法按引用借出）。
pub struct BlocksIter<'a> {
    blocks: &'a Blocks,
    seg_i: usize,
    seg_off: usize,
}
impl<'a> Iterator for BlocksIter<'a> {
    type Item = BlockMeta;
    fn next(&mut self) -> Option<BlockMeta> {
        while self.seg_i < self.blocks.segs.len() {
            let seg = &self.blocks.segs[self.seg_i];
            if self.seg_off < seg.len() {
                let b = seg.at(self.seg_off);
                self.seg_off += 1;
                return Some(b);
            }
            self.seg_i += 1;
            self.seg_off = 0;
        }
        None
    }
}

/// 行 → 字节起始偏移表（与 `build_line_starts(&str)` 语义相同，但存 u32 省内存）。
fn build_line_starts_u32(src: &str) -> Vec<u32> {
    let mut starts = Vec::with_capacity(src.len() / 8 + 2);
    starts.push(0u32);
    for (i, &b) in src.as_bytes().iter().enumerate() {
        if b == b'\n' {
            starts.push((i + 1) as u32);
        }
    }
    starts
}

/// 一次遍历产出整篇轻量块索引（不复制文本，不建 [`Row`]）。
///
/// 判定顺序与 [`parse_to_rows`] 完全一致（围栏 / 空行 / 标题 / 引述 / 列表 /
/// 分隔线 / 独立图片 / 表格 / 段落），并复用同一切块判定函数（`fence_len`、
/// `list_prefix`、`is_rule`、`parse_image_line`、`try_parse_table`），
/// 因此块边界与消耗行数与全量解析天然一致（有测试背书）。
pub fn scan_blocks(src: &str) -> BlockIndex {
    let line_starts = build_line_starts_u32(src);
    let lines: Vec<&str> = src.split('\n').collect();
    let mut blocks: Vec<BlockMeta> = Vec::with_capacity(lines.len());
    let mut toc: Vec<(usize, u8, String)> = Vec::new();

    let mut idx = 0usize;
    while idx < lines.len() {
        let row_start = idx;
        let raw_line = lines[idx].trim_end_matches('\r');
        let raw_len = raw_line.len() as u32;
        let byte = line_starts[row_start];
        // 与 parse_to_rows 一致的 EPUB 对齐指令剥离（kind 判定对同一份内容做）。
        let (_, line) = strip_align_directive(raw_line);
        let t = line.trim_start();

        if let Some((fc, _)) = fence_len(t) {
            let mut code_lines = 0u32;
            idx += 1;
            while idx < lines.len() {
                let l = lines[idx].trim_end_matches('\r');
                if let Some((c, _)) = fence_len(l.trim_start()) {
                    if c == fc {
                        idx += 1;
                        break;
                    }
                }
                code_lines += 1;
                idx += 1;
            }
            blocks.push(BlockMeta {
                src_line: row_start as u32,
                byte,
                kind: RowKind::Code,
                raw_len,
                aux: code_lines,
            });
            continue;
        }
        if t.is_empty() {
            blocks.push(BlockMeta {
                src_line: row_start as u32,
                byte,
                kind: RowKind::Blank,
                raw_len: 0,
                aux: 0,
            });
            idx += 1;
            continue;
        }
        let hashes = t.chars().take_while(|&c| c == '#').count();
        if (1..=6).contains(&hashes) && t.len() > hashes {
            let text = inline_strip(t[hashes..].trim_start()).to_string();
            toc.push((row_start, hashes as u8, text));
            blocks.push(BlockMeta {
                src_line: row_start as u32,
                byte,
                kind: RowKind::Heading(hashes as u8),
                raw_len,
                aux: 0,
            });
            idx += 1;
            continue;
        }
        if t.starts_with('>') {
            blocks.push(BlockMeta {
                src_line: row_start as u32,
                byte,
                kind: RowKind::Quote,
                raw_len,
                aux: 0,
            });
            idx += 1;
            continue;
        }
        if list_prefix(t).is_some() {
            blocks.push(BlockMeta {
                src_line: row_start as u32,
                byte,
                kind: RowKind::Bullet,
                raw_len,
                aux: 0,
            });
            idx += 1;
            continue;
        }
        if is_rule(t) {
            blocks.push(BlockMeta {
                src_line: row_start as u32,
                byte,
                kind: RowKind::Rule,
                raw_len,
                aux: 0,
            });
            idx += 1;
            continue;
        }
        if parse_image_line(t).is_some() {
            blocks.push(BlockMeta {
                src_line: row_start as u32,
                byte,
                kind: RowKind::Image,
                raw_len,
                aux: 1,
            });
            idx += 1;
            continue;
        }
        if let Some((table, consumed)) = try_parse_table(&lines, idx) {
            blocks.push(BlockMeta {
                src_line: row_start as u32,
                byte,
                kind: RowKind::Table,
                raw_len,
                aux: table.rows.len() as u32,
            });
            idx += consumed;
            continue;
        }
        blocks.push(BlockMeta {
            src_line: row_start as u32,
            byte,
            kind: RowKind::Paragraph,
            raw_len,
            aux: 0,
        });
        idx += 1;
    }
    BlockIndex {
        blocks: Blocks::from_vec(blocks),
        toc,
    }
}

/// 引用的**双向跳转**索引：注记号 → 文内引用 / 末尾定义所在渲染行。
///
/// 渲染行坐标 = 全局块下标（小文档 = `Vec<Row>` 下标；窗口模式 = [`BlockIndex`] 块下标），
/// 与视图层 `scroll_to_row` 的入参一致。
///
/// - [`Self::refs`]：`[^n]`（正文引用标记，显示成 `[n]`）→ (渲染行, 显示文本字节区间)
/// - [`Self::defs`]：`[!n]`（末尾定义标记，显示成 `[n]`）→ (渲染行, 显示文本字节区间)
///
/// 多个同号标记只记**第一个**（点击跳转取首处足够）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CitationIndex {
    /// 注记号 → 文内引用 `[^n]` 的 (渲染行, 显示文本字节区间)。
    pub refs: std::collections::BTreeMap<usize, (usize, Range<usize>)>,
    /// 注记号 → 末尾定义 `[!n]` 的 (渲染行, 显示文本字节区间)。
    pub defs: std::collections::BTreeMap<usize, (usize, Range<usize>)>,
    /// 注记号 → 引用详情正文（定义 `[!n]` 所在行剥掉 `[n]` 标记后的文本，去首空白）。
    /// 悬浮卡片在非 EPUB 时展示这个；EPUB 场景优先用外部注记表。
    pub details: std::collections::BTreeMap<usize, SharedString>,
}

/// 从**已解析行集**构建引用跳转索引（小文档全量路径；多位置取第一个）。
pub fn citation_index_from_rows(rows: &[Row]) -> CitationIndex {
    let mut idx = CitationIndex::default();
    for (ix, r) in rows.iter().enumerate() {
        for (range, n) in &r.notes {
            idx.refs.entry(*n).or_insert((ix, range.clone()));
        }
        for (range, n) in &r.note_defs {
            idx.defs.entry(*n).or_insert((ix, range.clone()));
            // 详情正文：定义标记 `[n]` 之后的文本（去首尾空白）。
            idx.details.entry(*n).or_insert_with(|| {
                let start = range.end.min(r.text.len());
                r.text[start..].trim().into()
            });
        }
    }
    idx
}

/// 提取一段**已剥行内样式**文本里的引用标记：返回 (清理后文本, 标记列表)。
/// 每个标记是 (显示文本字节区间, 注记号, 是否定义)；区间与 [`parse_to_rows`] 内
/// [`extract_notes`] 的产出一致。
fn extract_citation_markers(text: &str) -> (SharedString, Vec<(Range<usize>, usize, bool)>) {
    let (out, _, notes, note_defs) = extract_notes(text.into(), Vec::new());
    let markers = notes
        .into_iter()
        .map(|(r, n)| (r, n, false))
        .chain(note_defs.into_iter().map(|(r, n)| (r, n, true)))
        .collect();
    (out, markers)
}

/// 从整篇源码**逐块**扫描出引用跳转索引（窗口模式懒重建用）。
///
/// 与 [`scan_blocks`] 共用同一套切块判定（围栏 / 空行 / 标题 / 引述 / 列表 / 分隔线 /
/// 独立图片 / 表格 / 段落），块序号即渲染行下标——因此产出的渲染行坐标与窗口模式的
/// [`BlockIndex`] 完全一致，按需跳转不会错位。只对**文本块**（标题 / 引述 / 列表 /
/// 段落）剥行内样式后提取 `[^n]` / `[!n]` 标记（与 [`parse_to_rows`] 定义一致）；
/// 代码块 / 表格 / 图片不产生引用标记。
pub fn scan_citations(src: &str) -> CitationIndex {
    let mut idx = CitationIndex::default();
    let lines: Vec<&str> = src.split('\n').collect();
    let mut row_ix = 0usize;
    let mut i = 0usize;
    while i < lines.len() {
        let line = lines[i].trim_end_matches('\r');
        let (_, line) = strip_align_directive(line);
        let t = line.trim_start();
        let (cleaned, markers, consumed) = if let Some((fc, _)) = fence_len(t) {
            // 代码块：整体折叠成一渲染行（围栏行 + 到闭合行），不解析标记。
            let mut k = i + 1;
            while k < lines.len() {
                let l = lines[k].trim_end_matches('\r').trim_start();
                if fence_len(l).is_some_and(|(c, _)| c == fc) {
                    k += 1;
                    break;
                }
                k += 1;
            }
            (SharedString::default(), Vec::new(), k - i)
        } else if t.is_empty() {
            (SharedString::default(), Vec::new(), 1)
        } else {
            let hashes = t.chars().take_while(|&c| c == '#').count();
            if (1..=6).contains(&hashes) && t.len() > hashes {
                let (cleaned, m) =
                    extract_citation_markers(inline_style(t[hashes..].trim_start()).0.as_ref());
                (cleaned, m, 1)
            } else if t.starts_with('>') {
                let (_, rest) = strip_align_directive(t.trim_start_matches('>').trim_start());
                let (cleaned, m) = extract_citation_markers(inline_style(rest).0.as_ref());
                (cleaned, m, 1)
            } else if let Some((_, content)) = list_prefix(t) {
                let (_, content) = strip_align_directive(content);
                let (cleaned, m) = extract_citation_markers(inline_style(content).0.as_ref());
                (cleaned, m, 1)
            } else if is_rule(t) {
                (SharedString::default(), Vec::new(), 1)
            } else if parse_image_line(t).is_some() {
                (SharedString::default(), Vec::new(), 1)
            } else if let Some((_table, consumed)) = try_parse_table(&lines, i) {
                (SharedString::default(), Vec::new(), consumed)
            } else {
                let (cleaned, m) =
                    extract_citation_markers(inline_style(trim_paragraph(line)).0.as_ref());
                (cleaned, m, 1)
            }
        };
        for (range, n, is_def) in markers {
            if is_def {
                idx.defs.entry(n).or_insert((row_ix, range.clone()));
                // 详情正文：定义标记 `[n]` 之后的文本（去首尾空白）。
                let detail = cleaned[range.end..].trim();
                idx.details.entry(n).or_insert_with(|| detail.into());
            } else {
                idx.refs.entry(n).or_insert((row_ix, range));
            }
        }
        row_ix += 1;
        i += consumed;
    }
    idx
}

/// 两字符串从前向后相同的字节数（公共前缀长度）。
///
/// 大文档增量扫描几乎总是「只有一小段编辑，前后都是整段相等的字节」，因此按
/// 4KB 分块比较（`&[u8]` 的 `==` 落到底层 memcmp / 向量化），命中同块直接跳过，
/// 只在首个相异的块内逐字节定位——相对于逐字节迭代器（`zip().take_while()`）在
/// debug 下慢 5~10 倍、release 下数倍。量级仍是 O(n)（要读全串找差异），但把
/// 「背全文」的常数压到内存带宽级。
///
/// 生产路径已全部切到 [`scan_blocks_incremental_rope`]（按 dirty 区域切片，不再
/// 全串比对）；本函数与 [`common_suffix_bytes`]、[`scan_blocks_incremental`] 仅
/// 保留给测试/基准复用（str 路径的正确性安全网）。
#[cfg(test)]
fn common_prefix_bytes(a: &str, b: &str) -> usize {
    const BLK: usize = 4096;
    let aa = a.as_bytes();
    let bb = b.as_bytes();
    let n = aa.len().min(bb.len());
    let mut i = 0;
    while i + BLK <= n {
        if &aa[i..i + BLK] != &bb[i..i + BLK] {
            break;
        }
        i += BLK;
    }
    while i < n && aa[i] == bb[i] {
        i += 1;
    }
    i
}

/// 两字符串从后向前相同的字节数（公共后缀长度）。分块策略同上。测试/基准复用。
#[cfg(test)]
fn common_suffix_bytes(a: &str, b: &str) -> usize {
    const BLK: usize = 4096;
    let aa = a.as_bytes();
    let bb = b.as_bytes();
    let n = aa.len().min(bb.len());
    let (la, lb) = (aa.len(), bb.len());
    let mut i = 0;
    while i + BLK <= n {
        let sa = &aa[la - i - BLK..la - i];
        let sb = &bb[lb - i - BLK..lb - i];
        if sa != sb {
            break;
        }
        i += BLK;
    }
    while i < n && aa[la - 1 - i] == bb[lb - 1 - i] {
        i += 1;
    }
    i
}

/// **预览增量解析**：在上一次块索引基础上，只重扫「变化区及其所在块、直到与旧索引
/// 尾部重新对齐」的部分，跳过未变行的重复扫描。
///
/// `dirty`：编辑器侧通道给出的「当前文本中与 `prev_src` 不同的字节区间」（打字路径
/// 按 `InputEdit` 累积，O(k)），提供后**跳过 `common_prefix/_suffix` 两次 O(n) 全串
/// 比对**——打字同步的主成本从 O(n) 落到 O(k)（k=编辑字节数）。
/// `None` 时回退到全串比对定位变化区（快照 diff 无编辑信息的下界，作为兜底/测试安全网）。
///
/// 正确性保证与 `scan_blocks` 完全一致（复用同一套切块判定）；增量只在「尾部字节与
/// 旧文本相同、且当前位置恰是旧索引的块边界」时截断并复用旧尾部（块结束必为 Normal
/// 态、同字节 → 同分类，故复用安全）；对不上（如围栏边界跨变化区）就一直重扫到文末，
/// 结果与全量 `scan_blocks` 逐字节一致——这是**正确性兜底**。变化区过大直接全扫。
///
/// 生产路径已改用 rope 版 [`scan_blocks_incremental_rope`]（快照 Rope::clone O(1) +
/// 按 dirty 区域切片，不再对全串 split）；本 `&str` 版保留给测试/基准做正确性参照。
#[cfg(test)]
pub fn scan_blocks_incremental(
    prev_src: &str,
    prev: &BlockIndex,
    new_src: &str,
    dirty: Option<Range<usize>>,
) -> BlockIndex {
    let old_len = prev_src.len();
    let new_len = new_src.len();
    let new_lines: Vec<&str> = new_src.split('\n').collect();
    let (tail_start_new, changed, b_first) = match dirty {
        // 已知变化区间（new_src 坐标）→ 直接定位，跳过前后缀全串比对。
        // 首处变化之前字节未变，故 b_first 在 prev_src 中的偏移相同。
        Some(r) => {
            let dlo = r.start.min(new_len);
            let dhi = r.end.min(new_len).max(dlo);
            (dhi, dhi - dlo, dlo)
        }
        None => {
            let cp = common_prefix_bytes(prev_src, new_src);
            let cs = common_suffix_bytes(prev_src, new_src);
            let tail_start_new = new_len.saturating_sub(cs);
            let changed = new_len.saturating_sub(cp + cs);
            if new_len == old_len && changed == 0 {
                // 文本未变（同长且公共前后缀覆盖全串）
                return prev.clone();
            }
            (tail_start_new, changed, cp)
        }
    };
    if new_len == old_len && changed == 0 {
        return prev.clone();
    }
    // 变化区过大 → 全扫兜底（增量没赚头）。注意纯删除 changed 可能低估，
    // 但仍交给增量循环处理（正确性不依赖该阈值）。
    if changed >= old_len.max(new_len) / 2 {
        return scan_blocks(new_src);
    }

    let delta = new_len as i64 - old_len as i64; // 总字节增量（可负）

    // 锚点 = 旧索引中含首处变化字节的块（首处变化之前字节/行号两边一致）。
    // 用块的 `byte` 二分定位：变化前的行字节偏移在两帧相同。
    let bi = prev
        .blocks
        .partition_point(|b| b.byte as usize <= b_first)
        .saturating_sub(1);
    let anchor = prev.blocks.get(bi).map(|b| b.src_line as usize).unwrap_or(0);
    // 当前行的起始字节：锚点行起始 < b_first → 未变，直接复用旧索引的 byte。
    let anchor_byte = prev.blocks.get(bi).map(|b| b.byte as usize).unwrap_or(0);

    // 未变前缀直接沿用（分段共享，O(1) 不再整段 to_vec memcpy）；toc 同样取前缀。
    // 注意 toc 行号是**源码行**坐标，不能用块下标 `bi` 比较（围栏/表格是一块对应
    // 多行源码，两类坐标会漂移）：保留所有「行号 < 重扫起点行号 anchor」的旧标题
    // （重扫会重新产出其后的标题）。
    let prefix_segs = prev.blocks.shares_prefix(bi);
    let mut blocks: Vec<BlockMeta> = Vec::new(); // 新扫描的变化中段（通常极小）
    let mut suffix_segs: Vec<BlocksSeg> = Vec::new();
    let mut toc: Vec<(usize, u8, String)> = prev
        .toc
        .iter()
        .filter(|(r, _, _)| *r < anchor)
        .cloned()
        .collect();

    let mut idx = anchor;
    let mut cur_byte = anchor_byte as u32; // 当前扫描行的起始字节（new 坐标）
    let mut reused = false;
    while idx < new_lines.len() {
        // —— 复用旧尾部的机会：游标已进入「与旧文本字节相同」的尾部，且对应旧位置恰是块边界 ——
        //    prev_off = new_byte - delta 映射到旧坐标；旧块的 `byte` 即其行首偏移，
        //    能命中块边界（块 byte 严格递增，二分可定位）。
        let new_byte = cur_byte as usize;
        if new_byte >= tail_start_new {
            let prev_off = new_byte as i64 - delta;
            if (0..=old_len as i64).contains(&prev_off) {
                let prev_off = prev_off as usize;
                let pi = prev.blocks.partition_point(|b| (b.byte as usize) < prev_off);
                if let Some(pb) = prev.blocks.get(pi) {
                    if pb.byte as usize == prev_off {
                        // 同字节尾部 + 块首 Normal 态 → 旧尾部块逐字有效，行号与字节
                        // 同时平移（byte 平移 = 总字节增量 delta，源后缀整体平移）。
                        // 分段持久化：O(1) 共享 + 读取时施加 (delta, line_shift)。
                        let prev_line = pb.src_line as usize;
                        let line_shift = idx as i64 - prev_line as i64;
                        suffix_segs = prev.blocks.shares_suffix(pi, delta, line_shift);
                        // toc 尾部：与 `scan_blocks` 一致的「源码行」坐标 → 随行号偏移
                        for (r, lv, t) in prev.toc.iter().filter(|(r, _, _)| *r as usize >= prev_line) {
                            toc.push((( *r as i64 + line_shift).max(0) as usize, *lv, t.clone()));
                        }
                        reused = true;
                        break;
                    }
                }
            }
        }

        // —— 否则与 `scan_blocks` 完全相同的切块逻辑（逐块消费 `idx`）——
        let row_start = idx;
        let line = new_lines[idx].trim_end_matches('\r');
        let t = line.trim_start();
        let raw_len = line.len() as u32;
        let blk_byte = cur_byte;
        if let Some((fc, _)) = fence_len(t) {
            let mut code_lines = 0u32;
            idx += 1;
            cur_byte = next_line_byte(&new_lines, cur_byte, idx - 1, new_len);
            while idx < new_lines.len() {
                let l = new_lines[idx].trim_end_matches('\r');
                if let Some((c, _)) = fence_len(l.trim_start()) {
                    if c == fc {
                        idx += 1;
                        cur_byte = next_line_byte(&new_lines, cur_byte, idx - 1, new_len);
                        break;
                    }
                }
                code_lines += 1;
                idx += 1;
                cur_byte = next_line_byte(&new_lines, cur_byte, idx - 1, new_len);
            }
            blocks.push(BlockMeta {
                src_line: row_start as u32,
                byte: blk_byte,
                kind: RowKind::Code,
                raw_len,
                aux: code_lines,
            });
            continue;
        }
        if t.is_empty() {
            blocks.push(BlockMeta {
                src_line: row_start as u32,
                byte: blk_byte,
                kind: RowKind::Blank,
                raw_len: 0,
                aux: 0,
            });
            idx += 1;
            cur_byte = next_line_byte(&new_lines, cur_byte, idx - 1, new_len);
            continue;
        }
        let hashes = t.chars().take_while(|&c| c == '#').count();
        if (1..=6).contains(&hashes) && t.len() > hashes {
            let text = inline_strip(t[hashes..].trim_start()).to_string();
            toc.push((row_start, hashes as u8, text));
            blocks.push(BlockMeta {
                src_line: row_start as u32,
                byte: blk_byte,
                kind: RowKind::Heading(hashes as u8),
                raw_len,
                aux: 0,
            });
            idx += 1;
            cur_byte = next_line_byte(&new_lines, cur_byte, idx - 1, new_len);
            continue;
        }
        if t.starts_with('>') {
            blocks.push(BlockMeta {
                src_line: row_start as u32,
                byte: blk_byte,
                kind: RowKind::Quote,
                raw_len,
                aux: 0,
            });
            idx += 1;
            cur_byte = next_line_byte(&new_lines, cur_byte, idx - 1, new_len);
            continue;
        }
        if list_prefix(t).is_some() {
            blocks.push(BlockMeta {
                src_line: row_start as u32,
                byte: blk_byte,
                kind: RowKind::Bullet,
                raw_len,
                aux: 0,
            });
            idx += 1;
            cur_byte = next_line_byte(&new_lines, cur_byte, idx - 1, new_len);
            continue;
        }
        if is_rule(t) {
            blocks.push(BlockMeta {
                src_line: row_start as u32,
                byte: blk_byte,
                kind: RowKind::Rule,
                raw_len,
                aux: 0,
            });
            idx += 1;
            cur_byte = next_line_byte(&new_lines, cur_byte, idx - 1, new_len);
            continue;
        }
        if parse_image_line(t).is_some() {
            blocks.push(BlockMeta {
                src_line: row_start as u32,
                byte: blk_byte,
                kind: RowKind::Image,
                raw_len,
                aux: 1,
            });
            idx += 1;
            cur_byte = next_line_byte(&new_lines, cur_byte, idx - 1, new_len);
            continue;
        }
        if let Some((table, consumed)) = try_parse_table(&new_lines, idx) {
            for _ in 0..consumed {
                idx += 1;
                cur_byte = next_line_byte(&new_lines, cur_byte, idx - 1, new_len);
            }
            blocks.push(BlockMeta {
                src_line: row_start as u32,
                byte: blk_byte,
                kind: RowKind::Table,
                raw_len,
                aux: table.rows.len() as u32,
            });
            continue;
        }
        blocks.push(BlockMeta {
            src_line: row_start as u32,
            byte: blk_byte,
            kind: RowKind::Paragraph,
            raw_len,
            aux: 0,
        });
        idx += 1;
        cur_byte = next_line_byte(&new_lines, cur_byte, idx - 1, new_len);
    }
    debug_assert!(
        reused || idx >= new_lines.len(),
        "增量扫描要么对齐复用旧尾部、要么扫完新文本"
    );
    BlockIndex {
        blocks: Blocks::assemble(prefix_segs, blocks, suffix_segs),
        toc,
    }
}

/// 全量扫描一个 **ropey Rope** 文本（打开/首同步/大变化兜底用）。
///
/// 全量扫描本身就是 O(n)，因此内部物化一次 `String` 后复用 [`scan_blocks`]；
/// 单次 O(n) 物化只发生在「打开文档 / 预览首次同步 / 变化区过大」这类低频路径，
/// 打字同步的热路径走的是下面的 [`scan_blocks_incremental_rope`]（按 dirty 区域
/// 切片，O(k)）。传 `rope` 共享底层的快照更便宜（见 ropey COW 语义）。
pub fn scan_blocks_rope(rope: &Rope) -> BlockIndex {
    let text = rope.to_string();
    scan_blocks(&text)
}

/// 两个 rope 的公共前缀字节数。只在 `dirty=None` 的快照 diff 兜底路径调用（O(n)，
/// 与旧 `&str` 全串比对同量级），用 rope 的逐字节迭代器避免物化全文。
fn common_prefix_bytes_rope(a: &Rope, b: &Rope) -> usize {
    let n = a.len().min(b.len());
    let mut i = 0;
    let mut ia = a.bytes();
    let mut ib = b.bytes();
    while i < n {
        match (ia.next(), ib.next()) {
            (Some(x), Some(y)) if x == y => i += 1,
            _ => break,
        }
    }
    i
}

/// 两个 rope 的公共后缀字节数（方向反转的逐字节迭代）。
fn common_suffix_bytes_rope(a: &Rope, b: &Rope) -> usize {
    let n = a.len().min(b.len());
    let mut i = 0;
    let mut ia = a.bytes().reversed();
    let mut ib = b.bytes().reversed();
    while i < n {
        match (ia.next(), ib.next()) {
            (Some(x), Some(y)) if x == y => i += 1,
            _ => break,
        }
    }
    i
}

/// **预览增量解析（rope 版）**：语义与 `scan_blocks_incremental`（str 版，测试参照）
/// 完全一致，但输入为 ropey `Rope` —— 从而 UI 线程快照可 `Rope::clone`（O(1) COW，
/// 不物化全文），后台扫描也**不再对全串 split 建行表**：按 dirty 区/尾部对齐点从
/// rope `slice` 出变化区域（O(k)）再对区域切片扫描。
///
/// 定位尾部对齐点的关键不变量（与 str 版逐行对齐等价）：
/// dirty 之外新文本字节与旧文本相同，因此变化区之后的「第一条旧块边界」在旧坐标
/// `ob ≥ tail_start_new - delta` 处，对应新坐标行首 `ob + delta`——增量扫描从锚点
/// 行扫到该行首即可安全复用旧尾部 [`prev.blocks[pi..]`]（行号/字节整体平移）。
/// 对齐假设不命中（罕见，如变化区横跨围栏导致边界漂移）则整轮回退全量扫描兜底。
pub fn scan_blocks_incremental_rope(
    prev_src: &Rope,
    prev: &BlockIndex,
    new_src: &Rope,
    dirty: Option<Range<usize>>,
) -> BlockIndex {
    let old_len = prev_src.len();
    let new_len = new_src.len();
    let (tail_start_new, changed, b_first) = match dirty {
        // 已知变化区间（new_src 坐标）→ 直接定位，跳过前后缀全串比对。
        Some(r) => {
            let dlo = r.start.min(new_len);
            let dhi = r.end.min(new_len).max(dlo);
            (dhi, dhi - dlo, dlo)
        }
        None => {
            let cp = common_prefix_bytes_rope(prev_src, new_src);
            let cs = common_suffix_bytes_rope(prev_src, new_src);
            let tail_start_new = new_len.saturating_sub(cs);
            let changed = new_len.saturating_sub(cp + cs);
            if new_len == old_len && changed == 0 {
                return prev.clone();
            }
            (tail_start_new, changed, cp)
        }
    };
    if new_len == old_len && changed == 0 {
        return prev.clone();
    }
    if changed >= old_len.max(new_len) / 2 {
        return scan_blocks_rope(new_src);
    }

    let delta = new_len as i64 - old_len as i64;

    // 锚点 = 旧索引中含首处变化字节的块（变化前字节/行号两边一致）。
    let bi = prev
        .blocks
        .partition_point(|b| b.byte as usize <= b_first)
        .saturating_sub(1);
    let anchor = prev.blocks.get(bi).map(|b| b.src_line as usize).unwrap_or(0);
    let anchor_byte = prev.blocks.get(bi).map(|b| b.byte as usize).unwrap_or(0);

    // 未变前缀直接沿用；toc 同样取前缀（与 str 版相同的「源码行 vs 块下标」坐标
    // 修正：按重扫起点 `anchor` 行号过滤，而非块下标 `bi`）。
    let prefix_segs = prev.blocks.shares_prefix(bi);
    let mut blocks: Vec<BlockMeta> = Vec::new(); // 新扫描的变化中段（通常极小）
    let mut suffix_segs: Vec<BlocksSeg> = Vec::new();
    let mut toc: Vec<(usize, u8, String)> = prev
        .toc
        .iter()
        .filter(|(r, _, _)| *r < anchor)
        .cloned()
        .collect();

    // —— 只从 rope 切出「锚点到尾部对齐点」的变化区域（O(k)），不对全串 split ——
    let tail_old = tail_start_new as i64 - delta; // 尾部起点映射到旧坐标
    let pi = if tail_old <= old_len as i64 {
        prev.blocks.partition_point(|b| (b.byte as i64) < tail_old)
    } else {
        prev.blocks.len()
    };
    let region_end = if pi < prev.blocks.len() {
        // 第一条旧块边界 ≥ 尾部起点 → 新坐标行首即对齐点；夹紧到 [anchor_byte, new_len]
        ((prev.blocks.at(pi).byte as i64 + delta).max(anchor_byte as i64)).min(new_len as i64)
            as usize
    } else {
        // 尾部没有可用块边界 → 扫到文末（不复用，等价全量重扫）
        new_len
    };
    let region = if region_end > anchor_byte {
        new_src.slice(anchor_byte..region_end).to_string()
    } else {
        String::new()
    };
    let new_lines: Vec<&str> = region.split('\n').collect();
    let region_bytes = region.len();

    let mut idx = 0usize;
    let mut cur_byte = 0u32; // 区域内（相对锚点）当前行起始字节
    let mut reused = false;
    while idx < new_lines.len() {
        // —— 复用旧尾部的机会（absolute 坐标）——
        let new_byte = anchor_byte + cur_byte as usize;
        if new_byte >= tail_start_new {
            let prev_off = new_byte as i64 - delta;
            if (0..=old_len as i64).contains(&prev_off) {
                let prev_off = prev_off as usize;
                let pii = prev.blocks.partition_point(|b| (b.byte as usize) < prev_off);
                if let Some(pb) = prev.blocks.get(pii) {
                    if pb.byte as usize == prev_off {
                        let prev_line = pb.src_line as usize;
                        let line_shift = (anchor + idx) as i64 - prev_line as i64;
                        suffix_segs = prev.blocks.shares_suffix(pii, delta, line_shift);
                        for (r, lv, t) in prev.toc.iter().filter(|(r, _, _)| *r as usize >= prev_line) {
                            toc.push(((*r as i64 + line_shift).max(0) as usize, *lv, t.clone()));
                        }
                        reused = true;
                        break;
                    }
                }
            }
        }

        // —— 否则与 `scan_blocks` 完全相同的切块逻辑（逐块消费 `idx`）——
        let row_start = anchor + idx; // 文档绝对行号
        let line = new_lines[idx].trim_end_matches('\r');
        let t = line.trim_start();
        let raw_len = line.len() as u32;
        let blk_byte = (anchor_byte as u32).wrapping_add(cur_byte); // 文档绝对字节
        if let Some((fc, _)) = fence_len(t) {
            let mut code_lines = 0u32;
            idx += 1;
            cur_byte = next_line_byte(&new_lines, cur_byte, idx - 1, region_bytes);
            while idx < new_lines.len() {
                let l = new_lines[idx].trim_end_matches('\r');
                if let Some((c, _)) = fence_len(l.trim_start()) {
                    if c == fc {
                        idx += 1;
                        cur_byte = next_line_byte(&new_lines, cur_byte, idx - 1, region_bytes);
                        break;
                    }
                }
                code_lines += 1;
                idx += 1;
                cur_byte = next_line_byte(&new_lines, cur_byte, idx - 1, region_bytes);
            }
            blocks.push(BlockMeta {
                src_line: row_start as u32,
                byte: blk_byte,
                kind: RowKind::Code,
                raw_len,
                aux: code_lines,
            });
            continue;
        }
        if t.is_empty() {
            blocks.push(BlockMeta {
                src_line: row_start as u32,
                byte: blk_byte,
                kind: RowKind::Blank,
                raw_len: 0,
                aux: 0,
            });
            idx += 1;
            cur_byte = next_line_byte(&new_lines, cur_byte, idx - 1, region_bytes);
            continue;
        }
        let hashes = t.chars().take_while(|&c| c == '#').count();
        if (1..=6).contains(&hashes) && t.len() > hashes {
            let text = inline_strip(t[hashes..].trim_start()).to_string();
            toc.push((row_start, hashes as u8, text));
            blocks.push(BlockMeta {
                src_line: row_start as u32,
                byte: blk_byte,
                kind: RowKind::Heading(hashes as u8),
                raw_len,
                aux: 0,
            });
            idx += 1;
            cur_byte = next_line_byte(&new_lines, cur_byte, idx - 1, region_bytes);
            continue;
        }
        if t.starts_with('>') {
            blocks.push(BlockMeta {
                src_line: row_start as u32,
                byte: blk_byte,
                kind: RowKind::Quote,
                raw_len,
                aux: 0,
            });
            idx += 1;
            cur_byte = next_line_byte(&new_lines, cur_byte, idx - 1, region_bytes);
            continue;
        }
        if list_prefix(t).is_some() {
            blocks.push(BlockMeta {
                src_line: row_start as u32,
                byte: blk_byte,
                kind: RowKind::Bullet,
                raw_len,
                aux: 0,
            });
            idx += 1;
            cur_byte = next_line_byte(&new_lines, cur_byte, idx - 1, region_bytes);
            continue;
        }
        if is_rule(t) {
            blocks.push(BlockMeta {
                src_line: row_start as u32,
                byte: blk_byte,
                kind: RowKind::Rule,
                raw_len,
                aux: 0,
            });
            idx += 1;
            cur_byte = next_line_byte(&new_lines, cur_byte, idx - 1, region_bytes);
            continue;
        }
        if parse_image_line(t).is_some() {
            blocks.push(BlockMeta {
                src_line: row_start as u32,
                byte: blk_byte,
                kind: RowKind::Image,
                raw_len,
                aux: 1,
            });
            idx += 1;
            cur_byte = next_line_byte(&new_lines, cur_byte, idx - 1, region_bytes);
            continue;
        }
        if let Some((table, consumed)) = try_parse_table(&new_lines, idx) {
            for _ in 0..consumed {
                idx += 1;
                cur_byte = next_line_byte(&new_lines, cur_byte, idx - 1, region_bytes);
            }
            blocks.push(BlockMeta {
                src_line: row_start as u32,
                byte: blk_byte,
                kind: RowKind::Table,
                raw_len,
                aux: table.rows.len() as u32,
            });
            continue;
        }
        blocks.push(BlockMeta {
            src_line: row_start as u32,
            byte: blk_byte,
            kind: RowKind::Paragraph,
            raw_len,
            aux: 0,
        });
        idx += 1;
        cur_byte = next_line_byte(&new_lines, cur_byte, idx - 1, region_bytes);
    }
    debug_assert!(
        reused || idx >= new_lines.len(),
        "增量扫描要么对齐复用旧尾部、要么扫完新文本"
    );
    if !reused && region_end < new_len {
        // 对齐假设未命中（如变化区横跨围栏导致边界漂移）→ 全量兜底保证与
        // [`scan_blocks`] 逐字节一致（区域切片只覆盖到假设对齐点，不能丢）。
        return scan_blocks_rope(new_src);
    }
    BlockIndex {
        blocks: Blocks::assemble(prefix_segs, blocks, suffix_segs),
        toc,
    }
}

/// 从行 `i` 的起始字节推进到下一行的起始字节（+行长+换行符；末行无换行时
/// clamp 到 `new_len`，哨兵多算的 1 字节只会让尾部复用更早就触发、不会越界）。
fn next_line_byte(new_lines: &[&str], cur: u32, i: usize, new_len: usize) -> u32 {
    cur.saturating_add(new_lines[i].len() as u32)
        .saturating_add(1)
        .min(new_len as u32)
}

/// 物化块区间 `[lo, hi]`（闭区间）的渲染行。
///
/// 关键正确性前提：`parse_to_rows` 是**逐块无状态**的（每轮 while 迭代自包含），
/// 且 `lo/hi` 是块边界。因此从 `lo` 行的字节起始切片重新解析，
/// 产出的行与全量解析第 `lo..=hi` 个 [`Row`] **逐字节一致**——索引不会漂移。
/// 缓存只保留这块窗口，渲染随滚动窗口移动，整篇驻留不随文档增长。
pub fn parse_block_window(src: &str, index: &BlockIndex, lo: usize, hi: usize) -> Vec<Row> {
    if index.blocks.is_empty() {
        return Vec::new();
    }
    let n = index.blocks.len();
    let lo = lo.min(n - 1);
    let hi = hi.min(n - 1);
    if lo > hi {
        return Vec::new();
    }
    let start = index.blocks.at(lo).byte as usize;
    let mut end = if hi + 1 < n {
        index.blocks.at(hi + 1).byte as usize
    } else {
        src.len()
    };
    // 切片到"下一块首行"处会带上最后一块的换行符：`split('\n')` 会多造一个
    // 代表"下一行"的空元素 → 多出幻影空行。裁掉这 1 个 '\n' 即精确到块边界。
    if end < src.len() && src.as_bytes()[end - 1] == b'\n' {
        end -= 1;
    }
    let rows = parse_to_rows(&src[start..end]);
    debug_assert_eq!(rows.len(), hi - lo + 1, "窗口重解析行数必须与块数一致");
    // 窗口内 parse 的行号是"切片相对"的，平移到文档坐标（与块索引一致）
    let base_line = index.blocks.at(lo).src_line as usize;
    if base_line != 0 {
        let mut rows = rows;
        for r in &mut rows {
            r.src_line += base_line;
        }
        rows
    } else {
        rows
    }
}

/// rope 版 [`parse_block_window`]：块边界/行号/字节语义完全一致，只是输入为
/// ropey `Rope`——窗口只 `slice`（O(log n)）到可见区间（≤ 过扫块数），再 `to_string`
/// 物化该小块交给 [`parse_to_rows`]。窗口级懒解析路径不必再驻留整篇 6.4MB 的
/// `SharedString`（src_buf 改持 `Rope`，与编辑器 COW 共享底层，clone O(1)）。
pub fn parse_block_window_rope(rope: &Rope, index: &BlockIndex, lo: usize, hi: usize) -> Vec<Row> {
    if index.blocks.is_empty() {
        return Vec::new();
    }
    let n = index.blocks.len();
    let lo = lo.min(n - 1);
    let hi = hi.min(n - 1);
    if lo > hi {
        return Vec::new();
    }
    let start = index.blocks.at(lo).byte as usize;
    let mut end = if hi + 1 < n {
        index.blocks.at(hi + 1).byte as usize
    } else {
        rope.len()
    };
    // 切片到"下一块首行"处会带上最后一块的换行符：`split('\n')` 会多造一个
    // 代表"下一行"的空元素 → 多出幻影空行。裁掉这 1 个 '\n' 即精确到块边界。
    if end < rope.len() && rope.get_byte(end.saturating_sub(1)) == Some(b'\n') {
        end = end.saturating_sub(1);
    }
    let region = rope.slice(start..end).to_string();
    let rows = parse_to_rows(&region);
    debug_assert_eq!(rows.len(), hi - lo + 1, "窗口重解析行数必须与块数一致");
    let base_line = index.blocks.at(lo).src_line as usize;
    if base_line != 0 {
        let mut rows = rows;
        for r in &mut rows {
            r.src_line += base_line;
        }
        rows
    } else {
        rows
    }
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
        bytes += r
            .code_language()
            .map(|l| ALLOC_OVERHEAD + l.len())
            .unwrap_or(0);
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
    fn live_mapping_paragraph_bold_cjk() {
        // `rendered_to_source` 把渲染位置换算回文档字节；`**` 记号被剥（2 处各剥 2 字节）。
        let src = "前 **加粗** 后";
        let live = parse_to_rows_live(src);
        assert_eq!(live.len(), 1);
        let lr = &live[0];
        assert_eq!(lr.row.text.as_ref(), "前 加粗 后");
        let map = &lr.map;
        assert_eq!(map.len(), lr.row.text.len() + 1, "映射应覆盖渲染文本+哨兵");
        // 开头 0..3 = 前，3 = 空格；加粗内容起点 = 源 6
        assert_eq!(lr.rendered_to_source(0), 0);
        assert_eq!(lr.rendered_to_source(3), 3);
        let b_for_6 = map.iter().position(|&v| v == 6).expect("源 6 应有渲染位置");
        assert_eq!(lr.rendered_to_source(b_for_6), 6);
    }

    #[test]
    fn live_mapping_heading_bullet_quote() {
        let src = "# Head\n- item **b**\n> q";
        let live = parse_to_rows_live(src);
        assert_eq!(live.len(), 3);
        // 标题：`# ` 剥掉，渲染 "Head"，map[0]=源 2（H 的字节）
        assert_eq!(live[0].row.kind, RowKind::Heading(1));
        assert_eq!(live[0].row.text.as_ref(), "Head");
        assert_eq!(live[0].map[0], 2, "map={:?} text={:?}", live[0].map, live[0].row.text);
        assert_eq!(live[0].rendered_to_source(0), 2);
        // 列表：`- ` 剥掉，渲染 "item b"；`**b**` 里 b 的源字节 = 9
        assert_eq!(live[1].row.kind, RowKind::Bullet);
        assert_eq!(live[1].row.text.as_ref(), "item b");
        // `**b**` 里 b 的源字节 = 16（"# Head\n"=7 + "- "=2 + "item "=5 + "**"=2）
        assert_eq!(
            live[1].map.iter().position(|&v| v == 16),
            Some(5),
            "bullet map={:?} src={:?}",
            live[1].map,
            src
        );
        // 引述：`> ` 剥掉，渲染 "q"；'> q' 在源 20..23，q 字节约 22
        assert_eq!(live[2].row.kind, RowKind::Quote);
        assert_eq!(live[2].row.text.as_ref(), "q");
        assert_eq!(live[2].map[0], 22, "quote map={:?}", live[2].map);
    }

    #[test]
    fn live_mapping_cjk_roundtrip() {
        // 中文字符按字节映射（不拆 UTF-8）。"**你好**" = 2+6+2 = 10 字节。
        let src = "**你好**";
        let live = parse_to_rows_live(src);
        let lr = &live[0];
        assert_eq!(lr.row.text.as_ref(), "你好");
        // 你=源 2..5，好=5..8；哨兵 = 源 10
        assert_eq!(lr.rendered_to_source(0), 2);
        assert_eq!(lr.rendered_to_source(3), 5);
        assert_eq!(lr.rendered_to_source(6), 10, "渲染尾哨兵 = 段源尾");
    }

    #[test]
    fn live_block_rows_have_source_start_no_map() {
        let src = "~~~a\nx\n~~~\n\n![img](a.png)\n\n| a | b |\n| - | - |\n| 1 | 2 |";
        let live = parse_to_rows_live(src);
        // 行序：Code(0) Blank(11) Image(12) Blank(25) Table(27)
        assert_eq!(live.len(), 5);
        assert_eq!(live[0].row.kind, RowKind::Code);
        assert_eq!(live[0].source_start, 0);
        assert!(live[0].map.is_empty());
        assert_eq!(live[2].row.kind, RowKind::Image);
        assert_eq!(live[2].source_start, 12);
        assert!(live[2].map.is_empty());
        assert_eq!(live[4].row.kind, RowKind::Table);
        assert_eq!(live[4].source_start, 27);
        assert!(live[4].map.is_empty());
    }

    #[test]
    fn is_text_kind_covers_editable_rows() {
        assert!(is_text_kind(RowKind::Paragraph));
        assert!(is_text_kind(RowKind::Heading(2)));
        assert!(is_text_kind(RowKind::Bullet));
        assert!(is_text_kind(RowKind::Quote));
        assert!(!is_text_kind(RowKind::Code));
        assert!(!is_text_kind(RowKind::Table));
        assert!(!is_text_kind(RowKind::Image));
    }


    #[test]
    fn parses_headings_fences_lists() {
        let src = "# 一级标题\n```rs\nlet a = 1;\n```\n- item\n\n> quote\n";
        let rows = parse_to_rows(src);
        assert_eq!(rows[0].kind, RowKind::Heading(1));
        assert_eq!(rows[0].text.as_ref(), "一级标题");
        assert_eq!(rows[1].kind, RowKind::Code);
        assert_eq!(rows[1].code_language(), Some("rs"), "围栏语言应被捕获");
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
    fn code_fence_language_variants() {
        // 无语言标记 / 含信息串 / `{lang}` 旧式包裹 / 波浪号围栏
        let rows = parse_to_rows("```\nplain\n```\n\n```rust, ignore\nlet x = 1;\n```\n\n~~~{python}\nx = 1\n~~~\n");
        assert_eq!(rows[0].kind, RowKind::Code);
        assert_eq!(rows[0].code_language(), None, "纯 ``` 围栏无语言");
        assert_eq!(rows[2].kind, RowKind::Code);
        assert_eq!(rows[2].code_language(), Some("rust"), "取信息串首词");
        assert_eq!(rows[4].kind, RowKind::Code);
        assert_eq!(rows[4].code_language(), Some("python"), "`{{lang}}` 旧式包裹应剥掉花括号");
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

    /// 关键正确性：`scan_blocks` 的块边界/类型/aux 必须与全量 `parse_to_rows`
    /// 完全一致；`parse_block_window` 重解析出的行必须与全量行的同下标切片逐字节一致。
    /// 这是「窗口级懒解析」索引不漂移的根基测试。
    #[test]
    fn block_index_and_window_match_full_parse() {
        // 覆盖：围栏 / 表格 / 标题 / 列表 / 引述 / 图片 / 空行 / 普通段落混排
        let cases: Vec<String> = {
            let mut v = Vec::new();
            v.push(
                "# 一级标题\n\n正文段落，`code` 与 **bold**\n\n- 列表一\n- 列表二\n\n> 引述\n\n```rust\nlet a = 1;\nlet b = 2;\n```\n\n| 型号 | 尺寸 |\n| :--- | ---: |\n| A | 10 |\n| B | 20 |\n\n![插图](images/x.png)\n\n后文\n"
                    .to_string(),
            );
            // EPUB 排版指令 / 脚注标记（对齐指令 + 正文缩进「　　」 + `[^n]` 注记）。
            v.push(
                "%%ALIGN:RIGHT%%落款行\n　　正文[^1]段落。\n正文[^2]第二段。\n".to_string(),
            );
            // 真实（若有）+ benchdata
            for name in ["SKILL.md", "大文档测试.md", "bench_20k.md", "bench_160k.md"] {
                if let Ok(s) = std::fs::read_to_string(format!("target/{name}")) {
                    v.push(s);
                }
                if let Ok(s) = std::fs::read_to_string(format!("target/benchdata/{name}")) {
                    v.push(s);
                }
            }
            v
        };

        for src in &cases {
            let rows = parse_to_rows(src);
            let idx = scan_blocks(src);
            assert_eq!(
                idx.blocks.len(),
                rows.len(),
                "块数应等于渲染行数 (源 {} B)",
                src.len()
            );
            for (i, (m, r)) in idx.blocks.iter().zip(&rows).enumerate() {
                assert_eq!(m.src_line as usize, r.src_line, "block {i} src_line");
                assert_eq!(m.kind, r.kind, "block {i} kind");
                let aux = match r.kind {
                    RowKind::Code => r.code_lines().len() as u32,
                    RowKind::Table => r.table().map_or(0, |t| t.rows.len() as u32),
                    RowKind::Image => 1,
                    _ => 0,
                };
                assert_eq!(m.aux, aux, "block {i} aux");
            }

            // 目录：标题行号/级别/文本一致
            assert_eq!(idx.toc.len(), rows.iter().filter(|r| matches!(r.kind, RowKind::Heading(_))).count());
            for ((li, lv, tx), r) in idx.toc.iter().zip(rows.iter().filter(|r| matches!(r.kind, RowKind::Heading(_)))) {
                assert_eq!(*li, r.src_line);
                assert_eq!(*lv as u8, match r.kind { RowKind::Heading(n) => n, _ => 0 });
                assert_eq!(tx.as_str(), r.text.as_ref());
            }

            // 窗口重解析 == 全量切片（开头 / 中部 / 末尾）
            let n = idx.blocks.len();
            assert!(n > 0);
            for (lo, hi) in [
                (0, (5.min(n) + 0) as usize),
                (n / 2 - 2, n / 2 + 3),
                (n.saturating_sub(6), n - 1),
            ] {
                if lo > hi || hi >= n {
                    continue;
                }
                let win = parse_block_window(src, &idx, lo, hi);
                assert_eq!(win.len(), hi - lo + 1, "窗口 {lo}..={hi}");
                for (j, r) in win.iter().enumerate() {
                    let full = &rows[lo + j];
                    assert_eq!(r.src_line, full.src_line, "win hit {lo}+{j}");
                    assert_eq!(r.kind, full.kind);
                    assert_eq!(r.text.as_ref(), full.text.as_ref());
                }
            }
        }
    }

    /// rope 版窗口重解析（[`parse_block_window_rope`]）必须与 str 版逐行一致——
    /// 覆盖窗口模式的「从 Rope 按块字节边界切片」路径（视图 src_buf 改持 Rope 后
    /// 滚动重解析的全部走这里）。
    #[test]
    fn rope_block_window_matches_str_window() {
        let block = "# 一级标题\n\n正文段落，`code` 与 **bold**\n\n- 列表一\n- 列表二\n\n> 引述\n\n```rust\nlet a = 1;\nlet b = 2;\n```\n\n| 型号 | 尺寸 |\n| :--- | ---: |\n| A | 10 |\n| B | 20 |\n\n![插图](images/x.png)\n\n后文\n";
        let mut src = String::from(block);
        for i in 0..60 {
            src.push_str(&format!("# 章节 {i}\n\n段落与 `code` [链接](https://x)\n\n"));
        }
        let rope = Rope::from(src.as_str());
        let idx_str = scan_blocks(&src);
        let idx_rope = scan_blocks_rope(&rope);
        assert_eq!(idx_rope.blocks, idx_str.blocks, "rope 全量索引与 str 一致");
        assert_eq!(idx_rope.toc, idx_str.toc);

        let n = idx_str.blocks.len();
        for (lo, hi) in [
            (0, 4_usize),
            (n / 2 - 2, n / 2 + 3),
            (n.saturating_sub(5), n - 1),
        ] {
            if lo > hi || hi >= n {
                continue;
            }
            let ws = parse_block_window(&src, &idx_str, lo, hi);
            let wr = parse_block_window_rope(&rope, &idx_rope, lo, hi);
            assert_eq!(wr.len(), ws.len(), "rope 窗口 {lo}..={hi} 行数");
            for (r, s) in wr.iter().zip(&ws) {
                assert_eq!(r.src_line, s.src_line);
                assert_eq!(r.kind, s.kind);
                assert_eq!(r.text.as_ref(), s.text.as_ref());
                assert_eq!(
                    r.code_lines().len(),
                    s.code_lines().len(),
                    "rope 窗口 {lo}..={hi} code 行数"
                );
            }
        }
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

        // —— 大文档：窗口级懒解析 vs 全量驻留 ——
        println!("--- 窗口级懒解析对比（索引 + 原文 + 顶部 40 块窗口） ---");
        for (name, src) in &cases {
            let idx = scan_blocks(src);
            if idx.blocks.len() <= 4_000 {
                continue;
            }
            let n = idx.blocks.len();
            let index_b = n * std::mem::size_of::<BlockMeta>()
                + idx
                    .toc
                    .iter()
                    .map(|(_, _, t)| t.len() + 32)
                    .sum::<usize>();
            let src_b = src.len();
            let win = parse_block_window(src, &idx, 0, 40);
            let (_, win_b) = rows_memory_estimate(&win);
            let windowed_b = (index_b + src_b + win_b).max(1);
            let (full_n, full_b) = rows_memory_estimate(&parse_to_rows(src));
            let pct = 100.0 * (1.0 - windowed_b as f64 / full_b.max(1) as f64);
            println!(
                "{name}: 全量 {full_n} 行 ≈ {} KB ↔ 窗口模式 ≈ {} KB（省 {pct:.0}%）",
                full_b / 1024,
                windowed_b / 1024,
            );
        }
    }

    /// 增量索引必须与全量 `scan_blocks` 逐字节一致（blocks / toc）——
    /// 覆盖插入、删行、拆行、合并、围栏内改、围栏新开、表格改、标题改。
    #[test]
    fn incremental_equals_full_scan_across_edits() {
        let base = "# 标题\n> 引用\n- 列表\n\n```rust\nfn f() {}\nlet s = \"x\";\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n末段 `code` 与[链接](https://x)\n";
        let mut src = base.to_string();
        let mut prev = scan_blocks(&src);
        let apply = |src: &mut String, prev: &mut BlockIndex, start: usize, end: usize, ins: &str| {
            let (s, e) = (start.min(src.len()), end.min(src.len()).max(start));
            let mut new = String::with_capacity(src.len() + ins.len() - (e - s));
            new.push_str(&src[..s]);
            new.push_str(ins);
            new.push_str(&src[e..]);
            let inc = scan_blocks_incremental(src, prev, &new, None);
            let full = scan_blocks(&new);
            assert!(
                inc.blocks == full.blocks && inc.toc == full.toc,
                "编辑后不一致\n新文本头≈:\n{:?}\ninc_blocks={:?}\n\nfull_blocks={:?}",
                &new[..new.len().min(160)],
                inc.blocks,
                full.blocks,
            );
            *src = new;
            *prev = inc;
        };

        // 行内插入
        let at = src.find("let s").unwrap() + 3;
        apply(&mut src, &mut prev, at, at, "ux ");
        // 删除引用行
        let q = "> 引用";
        let at = src.find(q).unwrap();
        apply(&mut src, &mut prev, at, at + q.len(), "");
        // 拆行（列表后插入换行+新项）
        let at = src.find("- 列表").unwrap() + "- 列表".len();
        apply(&mut src, &mut prev, at, at, "- 新项\n");
        // 合并行（删除末段前换行）
        let at = src.find("末段").unwrap() - 1;
        apply(&mut src, &mut prev, at, at + 1, " ");
        // 围栏内新增一行（前一步把 "let s" 改成了 "letux"）
        let at = src.find("letux").unwrap();
        apply(&mut src, &mut prev, at, at, "    let t2 = 2;\n");
        // 中段新开一个围栏
        let at = src.find("末段").unwrap();
        apply(&mut src, &mut prev, at, at, "```\n中间代码\n```\n\n");
        // 编辑表头（表格行坐标变化；全 ASCII 区间避免切在 UTF-8 中间）
        let at = src.find("| a ").unwrap();
        apply(&mut src, &mut prev, at, at + 4, "| ah");
        // 编辑标题（toc 变化）："#" → "## "（0..2 全 ASCII）
        apply(&mut src, &mut prev, 0, 2, "## ");
    }

    /// 未变文本 → 增量直接返回旧索引（零重扫）。
    #[test]
    fn incremental_noop_returns_prev() {
        let src = "# 标题\n正文\n";
        let prev = scan_blocks(src);
        let inc = scan_blocks_incremental(src, &prev, src, None);
        assert_eq!(inc.blocks, prev.blocks);
        assert_eq!(inc.toc, prev.toc);
    }

    /// dirty 侧通道路径：给 `scan_blocks_incremental` 提供「变化字节区间」时，
    /// 结果必须与全量 `scan_blocks` 逐字节一致。覆盖各种编辑 + 用真实的前后缀
    /// 定位精确 dirty，以及模拟 tabs 侧 bounding box 的「膨胀」dirty。
    #[test]
    fn incremental_with_dirty_equals_full_scan() {
        let base = String::from("# 标题\n> 引用\n- 列表\n\n```rust\nfn f() {}\nlet s = \"x\";\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n末段与[链接](https://x) 结尾\n");
        // 每个场景：构造 prev_src（前一帧）→ new_src（当前帧），dirty 用前后缀定位的
        // 精确变化区 + 一个「前后膨胀 6B」的 bounding box 版各验一遍。
        let run = |name: &str, prev_src: String, new_src: String| {
            let prev = scan_blocks(&prev_src);
            let cp = common_prefix_bytes(&prev_src, &new_src);
            let cs = common_suffix_bytes(&prev_src, &new_src);
            let exact = cp..(new_src.len() - cs);
            for (tag, dirty) in [
                ("精确", exact.clone()),
                ("膨胀", (exact.start.saturating_sub(6))..(exact.end + 6).min(new_src.len())),
            ] {
                let inc = scan_blocks_incremental(&prev_src, &prev, &new_src, Some(dirty));
                let full = scan_blocks(&new_src);
                assert!(
                    inc.blocks == full.blocks && inc.toc == full.toc,
                    "{name}/{tag} 不一致\nprev_len={} new_len={} dirty={:?}\ninc_blocks={:?}\nfull_blocks={:?}",
                    prev_src.len(),
                    new_src.len(),
                    exact,
                    inc.blocks,
                    full.blocks,
                );
            }
        };

        // —— 连续两次编辑构造两帧（去抖窗口内多次编辑的真实场景）——
        let mut frame1 = base.clone();
        let mut frame2 = base.clone();
        let a = frame1.find("fn f").unwrap() + 4;
        frame1.insert_str(a, "g");
        let a = frame2.find("fn f").unwrap() + 4;
        frame2.insert_str(a, "g");
        frame2.insert_str(frame2.find("结尾").unwrap(), "XYZ");
        run("两帧累积", frame1, frame2);

        // —— 同帧多处编辑（dirty 须覆盖全部变化点）——
        run("两处编辑", base.clone(), {
            let mut v = base.clone();
            v.insert_str(v.find("| a").unwrap() + 2, "XX");
            v.insert_str(v.find("结尾").unwrap(), "YY");
            v
        });

        // —— 单编辑场景 ——
        run("文末追加", base.clone(), { let mut v = base.clone(); v.push_str("\n尾行"); v });
        run("中部删字", base.clone(), { let mut v = base.clone(); v.remove(v.find("结尾").unwrap()); v });
        run("中部插字", base.clone(), { let mut v = base.clone(); v.insert_str(v.find("结尾").unwrap(), "XYZ"); v });
        run("插整段", base.clone(), { let mut v = base.clone(); v.insert_str(v.find("结尾").unwrap(), "\n\n新段落\n"); v });
        run("拆行", base.clone(), { let mut v = base.clone(); v.insert_str(v.find("结尾").unwrap(), "\n"); v });
        run("合并行", base.clone(), { let mut v = base.clone(); let at = v.rfind('\n').unwrap(); v.remove(at); v });
        run("表头改", base.clone(), { let mut v = base.clone(); let at = v.find("| a ").unwrap(); v.replace_range(at..at + 4, "| ah"); v });
        run("围栏内改", base.clone(), { let mut v = base.clone(); v.replace_range(v.find("fn f").unwrap() + 4..v.find("fn f").unwrap() + 7, "g()"); v });
        run("去围栏", base.clone(), { let mut v = base.clone(); let at = v.find("```rust").unwrap(); v.replace_range(at..at + 3, "kok"); v });
    }

    /// ropey `Rope` 必须 `Send + Sync`（快照交后台线程扫描的前提），且 `clone` 为
    /// O(1) COW（不物化全文）。编译期 + 运行期断言。
    #[test]
    fn rope_is_send_sync_clone_cheap() {
        fn require_send_sync<T: Send + Sync>() {}
        require_send_sync::<Rope>();
        let a = Rope::from("hello 中文，多行\n第二行\n");
        let b = a.clone();
        assert_eq!(a.to_string(), b.to_string(), "COW 克隆内容一致");
        assert_eq!(a.len(), b.len());
    }

    /// rope 版增量索引必须与 rope 全量扫描逐字节一致（blocks / toc）——这是
    /// 「按 dirty 区域切片 + 尾部对齐复用」路径的正确性安全网。复现 str 版的
    /// 全部编辑场景（含跨帧累积、同帧多处编辑、精确/膨胀 dirty）。
    #[test]
    fn incremental_rope_with_dirty_equals_full_scan() {
        let base = String::from("# 标题\n> 引用\n- 列表\n\n```rust\nfn f() {}\nlet s = \"x\";\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n末段与[链接](https://x) 结尾\n");
        let run = |name: &str, prev_src: String, new_src: String| {
            let prev_rope = Rope::from(prev_src.as_str());
            let prev = scan_blocks_rope(&prev_rope);
            let new_rope = Rope::from(new_src.as_str());
            let cp = common_prefix_bytes(&prev_src, &new_src);
            let cs = common_suffix_bytes(&prev_src, &new_src);
            let exact = cp..(new_src.len() - cs);
            for (tag, dirty) in [
                ("精确", exact.clone()),
                ("膨胀", (exact.start.saturating_sub(6))..(exact.end + 6).min(new_src.len())),
            ] {
                let inc = scan_blocks_incremental_rope(&prev_rope, &prev, &new_rope, Some(dirty));
                let full = scan_blocks_rope(&new_rope);
                assert!(
                    inc.blocks == full.blocks && inc.toc == full.toc,
                    "{name}/{tag} 不一致\nprev_len={} new_len={} dirty={:?}\ninc_blocks={:?}\nfull_blocks={:?}",
                    prev_src.len(),
                    new_src.len(),
                    exact,
                    inc.blocks,
                    full.blocks,
                );
            }
        };

        // 连续两次编辑构造两帧（去抖窗口内多次编辑的真实场景）
        let mut frame1 = base.clone();
        let mut frame2 = base.clone();
        let a = frame1.find("fn f").unwrap() + 4;
        frame1.insert_str(a, "g");
        let a = frame2.find("fn f").unwrap() + 4;
        frame2.insert_str(a, "g");
        frame2.insert_str(frame2.find("结尾").unwrap(), "XYZ");
        run("两帧累积", frame1, frame2);

        // 同帧多处编辑（dirty 须覆盖全部变化点）
        run("两处编辑", base.clone(), {
            let mut v = base.clone();
            v.insert_str(v.find("| a").unwrap() + 2, "XX");
            v.insert_str(v.find("结尾").unwrap(), "YY");
            v
        });

        // 单编辑场景
        run("文末追加", base.clone(), { let mut v = base.clone(); v.push_str("\n尾行"); v });
        run("中部删字", base.clone(), { let mut v = base.clone(); v.remove(v.find("结尾").unwrap()); v });
        run("中部插字", base.clone(), { let mut v = base.clone(); v.insert_str(v.find("结尾").unwrap(), "XYZ"); v });
        run("插整段", base.clone(), { let mut v = base.clone(); v.insert_str(v.find("结尾").unwrap(), "\n\n新段落\n"); v });
        run("拆行", base.clone(), { let mut v = base.clone(); v.insert_str(v.find("结尾").unwrap(), "\n"); v });
        run("合并行", base.clone(), { let mut v = base.clone(); let at = v.rfind('\n').unwrap(); v.remove(at); v });
        run("表头改", base.clone(), { let mut v = base.clone(); let at = v.find("| a ").unwrap(); v.replace_range(at..at + 4, "| ah"); v });
        run("围栏内改", base.clone(), { let mut v = base.clone(); v.replace_range(v.find("fn f").unwrap() + 4..v.find("fn f").unwrap() + 7, "g()"); v });
        run("去围栏", base.clone(), { let mut v = base.clone(); let at = v.find("```rust").unwrap(); v.replace_range(at..at + 3, "kok"); v });
    }

    /// rope 版 `dirty=None` 的快照 diff 兜底路径 + 「未变文本零重扫」——
    /// 与 str 版行为一致。
    #[test]
    fn incremental_rope_noop_and_snapshot_diff_fallback() {
        // 未变 → 直接返回旧索引
        let src = Rope::from("# 标题\n正文\n");
        let prev = scan_blocks_rope(&src);
        let inc = scan_blocks_incremental_rope(&src, &prev, &src, None);
        assert_eq!(inc.blocks, prev.blocks);
        assert_eq!(inc.toc, prev.toc);

        // dirty=None 兜底：真实编辑，靠前后缀 diff 定位变化区
        let prev_src = Rope::from("# 标题\n> 引用\n普通段落与 `code` [链接](https://x) 结尾\n");
        let prev = scan_blocks_rope(&prev_src);
        let mut text = prev_src.to_string();
        let at = text.find("普通段落").unwrap() + 3;
        text.insert_str(at, "ABC");
        let new = Rope::from(text.as_str());
        let inc = scan_blocks_incremental_rope(&prev_src, &prev, &new, None);
        let full = scan_blocks_rope(&new);
        assert!(
            inc.blocks == full.blocks && inc.toc == full.toc,
            "dirty=None 兜底不一致\ninc_blocks={:?}\nfull_blocks={:?}",
            inc.blocks,
            full.blocks,
        );
    }

    /// 基准（--ignored）：rope 化「打字同步每轮」成本对比——旧 str 路径（`value()`
    /// 物化 6.4MB + 增量扫描对全串 split）vs 新 rope 路径（Rope::clone O(1) + 按 dirty
    /// 区域切片）。与 handoff 3.1 同量级的大文档（≈6MB/14 万行），打印不断言。
    #[test]
    #[ignore]
    fn bench_rope_incremental_vs_old_str_snapshot() {
        use std::time::Instant;
        let block = "# 章节标题\n\n正文段落，包含 `行内代码` 与 **加粗** 和 [链接](https://x.y)。\n\n- 列表项一\n- 列表项二\n\n| 列A | 列B |\n| --- | --- |\n| 值1 | 值2 |\n\n```rust\nfn main() { println!(\"hi\"); }\n```\n";
        let mut text = String::new();
        while text.len() < 6_000_000 {
            text.push_str(block);
        }
        let n_lines = text.lines().count();
        let prev_str = text.clone();
        let prev_rope = Rope::from(prev_str.as_str());
        let prev = scan_blocks(&prev_str);

        // 每次在文档中部插 6 字节（模拟打字），dirty 形如 (at..at+6)；
        // 插入点取字节边界（真实 InputEdit 的 dirty 区间恒为字符边界）。
        let at = text.find("，包含").unwrap();
        let mut new = text.clone();
        new.insert_str(at, "ABCXYZ");
        let rope = Rope::from(new.as_str());
        let dirty_range = at..(at + 6);

        // 快照成本：旧 `value()`（String 物化/克隆）vs 新 `Rope::clone`（O(1) COW）
        let t = Instant::now();
        let mut sink = 0usize;
        for _ in 0..50 {
            let s = prev_str.to_string();
            sink += s.len();
        }
        let value_ms = t.elapsed().as_secs_f64() * 1000.0 / 50.0;
        let t = Instant::now();
        for _ in 0..50 {
            let r = prev_rope.clone();
            sink += r.len();
        }
        let rope_ms = t.elapsed().as_secs_f64() * 1000.0 / 50.0;

        // 增量扫描：旧 str（全串 split 建行表）vs 新 rope（只切 dirty 区域）
        let t = Instant::now();
        let mut sum_blocks = 0usize;
        for _ in 0..50 {
            let idx = scan_blocks_incremental(&prev_str, &prev, &new, Some(dirty_range.clone()));
            sum_blocks += idx.blocks.len();
        }
        let str_inc_ms = t.elapsed().as_secs_f64() * 1000.0 / 50.0;
        let t = Instant::now();
        for _ in 0..50 {
            let idx =
                scan_blocks_incremental_rope(&prev_rope, &prev, &rope, Some(dirty_range.clone()));
            sum_blocks += idx.blocks.len();
        }
        let rope_inc_ms = t.elapsed().as_secs_f64() * 1000.0 / 50.0;

        println!(
            "大文档 {} 行 / {:.1} MB：快照 value()/String 物化 {:.3} ms vs Rope::clone {:.3} ms（{:.0}x）；增量 str(全串split) {:.3} ms vs rope(区域切片) {:.3} ms；每轮合计 str {:.3} ms vs rope {:.3} ms（{:.1}x）",
            n_lines,
            text.len() as f64 / 1e6,
            value_ms,
            rope_ms,
            value_ms / rope_ms.max(1e-9),
            str_inc_ms,
            rope_inc_ms,
            str_inc_ms + value_ms,
            rope_inc_ms + rope_ms,
            (str_inc_ms + value_ms) / (rope_inc_ms + rope_ms).max(1e-9),
        );
        assert!(sum_blocks > 0, "sink {}", sink);
    }

    /// 回归：增量 toc 前缀过滤曾用「块下标 `bi`」去比较**源码行号**。围栏（一块对
    /// 应多行源码）会让两类坐标漂移：文档中部某标题的源码行号 > 它前方的块数。
    /// 末尾追加时，如果该标题在重扫起点之前（应保留），旧过滤器却把它误丢 →
    /// 增量 toc 与全量不一致（str 与 rope 两条增量路径都曾受影响）。
    #[test]
    fn toc_prefix_uses_source_line_not_block_index() {
        // 500 行围栏（1 块）→「前置标题」（行号很大、块下标很小）→ 若干单行段落块
        let mut src = String::from("```rust\n");
        for i in 0..500 {
            src.push_str(&format!("// 第 {i} 行\n"));
        }
        src.push_str("```\n");
        src.push_str("# 前置标题\n");
        for i in 0..500 {
            src.push_str(&format!("正文行 {i}\n"));
        }
        src.push('\n');
        let base = scan_blocks(&src);
        assert!(
            base.toc.iter().any(|(_, _, t)| t == "前置标题"),
            "前置标题应在 toc"
        );

        // 末尾追加新标题：锚点 = 末尾空块，其块下标 ≈ 502 > 前置标题行号 502 附近
        let mut new = src.clone();
        new.push_str("# 追加标题\n正文追加\n");
        let dirty = src.len()..new.len();
        let full = scan_blocks(&new);

        let str_prev = base.clone();
        let inc = scan_blocks_incremental(&src, &str_prev, &new, Some(dirty.clone()));
        assert!(
            inc.blocks == full.blocks && inc.toc == full.toc,
            "str 版 toc 前缀过滤不应丢标题\ninc_toc={:?}\nfull_toc={:?}",
            inc.toc,
            full.toc,
        );

        let prev_rope = Rope::from(src.as_str());
        let prev = scan_blocks_rope(&prev_rope);
        let new_rope = Rope::from(new.as_str());
        let inc = scan_blocks_incremental_rope(&prev_rope, &prev, &new_rope, Some(dirty));
        assert!(
            inc.blocks == full.blocks && inc.toc == full.toc,
            "rope 版 toc 前缀过滤不应丢标题\ninc_toc={:?}\nfull_toc={:?}",
            inc.toc,
            full.toc,
        );
    }

    /// #5 持久段：连续编辑后 `blocks` 的段数被 amortize 压在上限内（读取保持 O(段数)），
    /// 且每轮结果与全量扫描一致（中段的 `分割/平移` 合成正确）。
    #[test]
    fn blocks_segments_stay_bounded_across_edits() {
        let block = "# 章节\n\n正文 `code` [链接](https://x)\n\n- 项一\n- 项二\n\n```rust\nlet x = 1;\n```\n\n| a | b |\n|---|--:|\n| 1 | 2 |\n\n";
        let mut text = String::new();
        for _ in 0..1500 {
            text.push_str(block);
        }
        let mut cur = Rope::from(text.as_str());
        let mut prev = scan_blocks_rope(&cur);
        // 中部 ASCII 锚点「| a |」（纯 ASCII → 其字节位置必为字符边界），供中段编辑用
        let bytes = text.as_bytes();
        let half = text.len() / 2;
        let at_mid = (half..text.len().saturating_sub(5))
            .find(|&i| bytes[i] == b'|' && &bytes[i..i + 4] == b"| a ")
            .unwrap_or(text.len() - 1);
        let mut max_segs = 0usize;
        for i in 0..120 {
            // 文末追加（段数会累积 → 触发合并），每 3 轮夹一次中部编辑（触发前缀/后缀
            // 共享与平移合成）。
            let at = if i % 3 == 2 { at_mid } else { cur.len() };
            let ins = "\n# 追加\n正文\n";
            let mut next = cur.clone();
            next.insert(at, ins);
            let dirty = at..at + ins.len();
            let inc = scan_blocks_incremental_rope(&cur, &prev, &next, Some(dirty));
            max_segs = max_segs.max(inc.blocks.seg_count());
            let full = scan_blocks_rope(&next);
            assert!(
                inc.blocks == full.blocks && inc.toc == full.toc,
                "第 {i} 轮增量应与全量一致"
            );
            cur = next;
            prev = inc;
        }
        assert!(
            max_segs <= BLOCKS_MAX_SEGS,
            "段数应被 amortize 压在上限内: max={max_segs} 上限={BLOCKS_MAX_SEGS}"
        );
    }

    /// 基准（--ignored）：复刻 handoff §3.1 的方法——对参考文件 `target/大文档测试.md`
    /// （6.4MB / ≈17 万行）连续做 **1000 次文末插入**，对比 str 路径（`value()`
    /// String 物化 + 增量扫描全串 split）与新 rope 路径（Rope::clone O(1) + 增量
    /// 扫描区域切片），每轮取 dirty，最终与全量扫描逐字节校验。打印、不断言。
    #[test]
    #[ignore]
    fn bench_1000_inserts_on_reference_doc() {
        use std::time::Instant;
        let path = std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/target/大文档测试.md"
        ));
        if !path.exists() {
            eprintln!("跳过：缺少参考文件 {path:?}");
            return;
        }
        let text = std::fs::read_to_string(path).expect("参考文件读取失败");
        let n_inserts = 1000usize;

        // —— 旧 str 路径：每轮 value()（String 物化）+ 增量扫描（全串 split）——
        let mut cur = text.clone();
        let mut prev = scan_blocks(&cur);
        let mut str_total = std::time::Duration::ZERO;
        let t0 = Instant::now();
        for i in 0..n_inserts {
            let old_len = cur.len();
            let mut next = cur.clone();
            next.push_str(&format!("\n# 追加基准段 {i}\n正文 `code` 与[链接](https://x)\n"));
            let dirty = old_len..next.len();
            let t = Instant::now();
            let inc = scan_blocks_incremental(&cur, &prev, &next, Some(dirty));
            str_total += t.elapsed();
            if i % 100 == 99 {
                let full = scan_blocks(&next);
                assert!(inc.blocks == full.blocks && inc.toc == full.toc, "str 第 {i} 轮不一致");
            }
            cur = next;
            prev = inc;
        }
        println!(
            "旧 str（value+split+增量）: {} 次文末插入共 {:.1} s，扫描累计 {:.1} ms/次",
            n_inserts,
            t0.elapsed().as_secs_f64(),
            str_total.as_secs_f64() * 1000.0 / n_inserts as f64,
        );

        // —— 新 rope 路径：每轮 Rope::clone（O(1)）+ 增量扫描（区域切片）——
        let mut cur = Rope::from(text.as_str());
        let mut prev = scan_blocks_rope(&cur);
        let mut rope_total = std::time::Duration::ZERO;
        let t0 = Instant::now();
        for i in 0..n_inserts {
            let old_len = cur.len();
            let mut next = cur.clone();
            next.insert(old_len, &format!("\n# 追加基准段 {i}\n正文 `code` 与[链接](https://x)\n"));
            let dirty = old_len..next.len();
            let t = Instant::now();
            let inc = scan_blocks_incremental_rope(&cur, &prev, &next, Some(dirty));
            rope_total += t.elapsed();
            if i % 100 == 99 {
                let full = scan_blocks_rope(&next);
                assert!(inc.blocks == full.blocks && inc.toc == full.toc, "rope 第 {i} 轮不一致");
            }
            cur = next;
            prev = inc;
        }
        println!(
            "新 rope（clone+区域切片+增量）: {} 次文末插入共 {:.1} s，扫描累计 {:.1} ms/次；末尾 blocks 段数 {}（≤ {} 才保持 O(1) 共享）",
            n_inserts,
            t0.elapsed().as_secs_f64(),
            rope_total.as_secs_f64() * 1000.0 / n_inserts as f64,
            prev.blocks.seg_count(),
            BLOCKS_MAX_SEGS,
        );

        // —— 中部编辑（前后缀都要共享/平移）：持久分段的典型场景 ——
        let mid = text.len() / 2;
        let mut cur = Rope::from(text.as_str());
        let mut prev = scan_blocks_rope(&cur);
        let mut mid_total = std::time::Duration::ZERO;
        let ins = "\n# 中段追加行\n正文\n";
        let t0 = Instant::now();
        for i in 0..200 {
            let at = mid + (i * 3) % 8000;
            let mut next = cur.clone();
            next.insert(at, ins);
            let dirty = at..at + ins.len();
            let t = Instant::now();
            let inc = scan_blocks_incremental_rope(&cur, &prev, &next, Some(dirty));
            mid_total += t.elapsed();
            if i % 100 == 99 {
                let full = scan_blocks_rope(&next);
                assert!(inc.blocks == full.blocks && inc.toc == full.toc, "rope 中段第 {i} 轮不一致");
            }
            cur = next;
            prev = inc;
        }
        println!(
            "rope 中部编辑（前后缀持久共享）: 200 次共 {:.1} s，扫描累计 {:.1} ms/次；段数 {}",
            t0.elapsed().as_secs_f64(),
            mid_total.as_secs_f64() * 1000.0 / 200.0,
            prev.blocks.seg_count(),
        );
    }

    /// 基准（--ignored）：大文档小编辑时，增量（dirty 侧通道 vs 快照 diff）vs
    /// 全量扫描耗时对比（打印、不断言）。
    #[test]
    #[ignore]
    fn bench_incremental_vs_full_scan() {
        use std::time::Instant;
        let mut src = String::new();
        for i in 0..1600 {
            src.push_str(&format!("# 标题段 {i}\n> 引用\n- 列表项\n\n```rust\nfn f() {{}}\nlet s = \"x\";\n```\n\n普通段落与 `code` [链接](https://x)\n\n"));
        }
        let prev = scan_blocks(&src);
        // 中部小编辑：给某段插几个字
        let at = src.find("普通段落").unwrap() + 6;
        let mut new = src.clone();
        new.insert_str(at, "ABC");
        let dirty = at..(at + 3);
        let t0 = Instant::now();
        let _inc_fast = scan_blocks_incremental(&src, &prev, &new, Some(dirty));
        let fast_ms = t0.elapsed();
        let t0 = Instant::now();
        let _inc = scan_blocks_incremental(&src, &prev, &new, None);
        let inc_ms = t0.elapsed();
        let t1 = Instant::now();
        let _full = scan_blocks(&new);
        let full_ms = t1.elapsed();
        println!(
            "{} 行：增量(dirty) {:.1} ms vs 增量(快照diff) {:.1} ms vs 全量 {:.1} ms",
            prev.blocks.len(),
            fast_ms.as_secs_f64() * 1000.0,
            inc_ms.as_secs_f64() * 1000.0,
            full_ms.as_secs_f64() * 1000.0,
        );
    }

    #[test]
    fn epub_align_directive_and_notes_parse() {
        let src = "%%ALIGN:RIGHT%%落款第一行\n%%ALIGN:LEFT%%浮段\n　　正文[^1]带注段。\n正文[^2]再一段。";
        let rows = parse_to_rows(src);
        assert_eq!(rows.len(), 4, "指令行不产生额外渲染行");
        // 右对齐行
        assert_eq!(rows[0].kind, RowKind::Paragraph);
        assert_eq!(rows[0].align, Some(RowAlign::Right), "RIGHT 指令应生效");
        assert_eq!(rows[0].text.as_ref(), "落款第一行", "指令前缀应被剥掉");
        // 左对齐行
        assert_eq!(rows[1].align, Some(RowAlign::Left));
        assert_eq!(rows[1].text.as_ref(), "浮段");
        // 普通正文：无对齐、`[^1]` → `[1]` + 注记区间（含前置「　　」缩进）
        assert_eq!(rows[2].align, None);
        assert_eq!(rows[2].text.as_ref(), "　　正文[1]带注段。", "注记 ^ 应剥掉");
        assert_eq!(rows[2].notes.len(), 1, "应有 1 条注记");
        let (r, n) = &rows[2].notes[0];
        assert_eq!(*n, 1);
        assert_eq!(&rows[2].text[r.clone()], "[1]", "注记区间应对应 [1]");
        // 无指令行保持默认
        assert_eq!(rows[3].align, None);
        // 窗口重解析（块边界切片）也要恢复对齐：从第 2 块起的切片应含指令行。
        let src2 = "前缀段\n%%ALIGN:RIGHT%%落款行";
        let idx = scan_blocks(src2);
        let mut rb = Vec::new();
        for i in 0..idx.blocks.len() {
            rb.extend(parse_block_window(src2, &idx, i, i));
        }
        assert_eq!(rb.len(), idx.blocks.len());
        assert_eq!(rb[1].align, Some(RowAlign::Right), "窗口重解析应恢复对齐指令");
    }

    #[test]
    fn epub_directive_inside_quote_and_bullet_is_stripped() {
        // 引述块/列表项里被包裹的对齐指令（如 `> %%ALIGN:RIGHT%%…`）会漏到行首判断
        // 之外，必须从内容里也剥掉，不能作为可见文本渲染。
        let rows = parse_to_rows("> %%ALIGN:RIGHT%%引述落款\n- %%ALIGN:LEFT%%列表项");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].kind, RowKind::Quote);
        assert_eq!(rows[0].text.as_ref(), "引述落款", "引述里的指令应剥掉，实际 {}", rows[0].text);
        assert_eq!(rows[0].align, Some(RowAlign::Right));
        assert!(!rows[0].text.contains("%%ALIGN"), "不得泄漏指令串");
        assert_eq!(rows[1].kind, RowKind::Bullet);
        assert_eq!(rows[1].text.as_ref(), "列表项", "列表项里的指令应剥掉，实际 {}", rows[1].text);
        assert_eq!(rows[1].align, Some(RowAlign::Left));
        assert!(!rows[1].text.contains("%%ALIGN"), "不得泄漏指令串");
        // 扫描器一致性不受影响（指令行仍是同一块）。
        let idx = scan_blocks("> %%ALIGN:RIGHT%%引述落款\n- %%ALIGN:LEFT%%列表项");
        assert_eq!(idx.blocks.len(), 2);
    }

    #[test]
    fn epub_note_def_marker_parses() {
        // 注释定义标记 `[!n]` → Row.note_defs（逆跳用），显示成 `[n]`。
        let rows = parse_to_rows("正文[^2]引用。\n[!2]　注释定义内容。\n[!33]　另一条。");
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].notes.len(), 1);
        assert_eq!(rows[0].note_defs.len(), 0);
        assert_eq!(rows[1].notes.len(), 0);
        assert_eq!(rows[1].note_defs.len(), 1, "定义标记应进 note_defs");
        let (r, n) = &rows[1].note_defs[0];
        assert_eq!(*n, 2);
        assert_eq!(&rows[1].text[r.clone()], "[2]");
        assert_eq!(rows[1].text.starts_with("[2]"), true);
        assert_eq!(rows[2].note_defs[0].1, 33);
    }

    #[test]
    fn citation_index_from_rows_maps_refs_and_defs() {
        // 全量路径：注记号 → (渲染行, 显示文本字节区间) + 详情正文。
        let rows = parse_to_rows("一句话[^1]引用。\n[!1]　定义1。\n[!2]　定义2（无文内对端）。");
        let idx = citation_index_from_rows(&rows);
        // 区间是**字节**坐标（CJK 每字 3 字节）：「一句话」=9B → [^1] 显示成 [1] 在 [9..12)。
        assert_eq!(idx.refs.get(&1), Some(&(0, 9..12)), "文内 [1] 应在第 0 行、区间 [9..12)");
        assert_eq!(idx.defs.get(&1), Some(&(1, 0..3)), "定义 [1] 应在第 1 行、区间 [0..3)");
        assert_eq!(idx.defs.get(&2), Some(&(2, 0..3)), "定义 [2] 应在第 2 行");
        assert_eq!(idx.refs.get(&2), None, "孤立定义不应进 refs");
        assert_eq!(idx.details.get(&1).map(|s| s.as_ref()), Some("定义1。"), "详情应剥掉 [1] 标记");
        assert_eq!(
            idx.details.get(&2).map(|s| s.as_ref()),
            Some("定义2（无文内对端）。")
        );
    }

    #[test]
    fn scan_citations_matches_row_indexes_and_skips_code() {
        // 窗口模式懒扫描：与 parse_to_rows 同块结构，行坐标（BlockIndex 下标）一致；
        // 代码块里的 `[^n]` 不是引用，不得计入。
        let src = "# 标题[^1]\n\n正文[^2]。\n\n```\n[^9] 代码里的方括号不是引用\n```\n\n[!1]　定1。\n[!2]　定2。";
        let idx = scan_citations(src);
        // 块：0=标题,1=空行,2=正文,3=空行,4=代码块,5=空行,6=定1,7=定2
        // （字节区间：「标题」=6B → [1] 在 [6..9)；「正文」=6B → [2] 在 [6..9)。）
        assert_eq!(idx.refs.get(&1), Some(&(0, 6..9)), "文内 [1] 在第 0 块");
        assert_eq!(idx.refs.get(&2), Some(&(2, 6..9)), "文内 [2] 在第 2 块");
        assert_eq!(idx.refs.contains_key(&9), false, "代码块里的 [^9] 不是引用");
        assert_eq!(idx.defs.get(&1), Some(&(6, 0..3)), "定义 [1] 在第 6 块");
        assert_eq!(idx.defs.get(&2), Some(&(7, 0..3)), "定义 [2] 在第 7 块");
        assert_eq!(idx.details.get(&1).map(|s| s.as_ref()), Some("定1。"));
        assert_eq!(idx.details.get(&2).map(|s| s.as_ref()), Some("定2。"));
        // 与全量路径产出一致（同结构 → 同名块坐标一致）。
        let rows = parse_to_rows(src);
        let from_rows = citation_index_from_rows(&rows);
        assert_eq!(idx, from_rows, "scan_citations 应与全量行推导一致");
    }
}
