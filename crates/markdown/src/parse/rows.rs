//! 行级 Markdown 解析：把源码**逐行**转成渲染行（[`Row`]）。
//!
//! 从 `parser`（行扫描器 + 块索引 + 引用索引 + 增量分析）拆出的「行解析」部分
//! （2026-09-24 重构）：只负责「一行 / 少数连续行（代码围栏 / GFM 表格）→ [`Row`]」
//! 的纯解析与行内清理，无状态、无增量；块索引 / 增量 / 引用索引见 `parser`
//! （`blocks*` / `citations` 经门面重导出）。

use std::ops::Range;
use std::sync::OnceLock;

use gpui::{Image, SharedString};
use regex::Regex;

use super::cells;
use super::inline_style::blockquote_prefix;

/// 样式段列表：`(字节区间, 样式类别)`（行内解析的通用产物）。
pub(crate) type StyledRuns = Vec<(Range<usize>, RunKind)>;
/// [`inline_style_mapped`] 的返回：渲染文本 + 样式段 + 渲染→源字节映射表。
pub(crate) type MappedStyleResult = (SharedString, StyledRuns, Vec<usize>);
/// 脚注标记：`(字节区间, 注记号)`。
pub(crate) type NoteMarkers = Vec<(Range<usize>, usize)>;
/// [`extract_notes`] 的返回：清理文本 + 样式段 + 正文引用 + 定义标记。
pub(crate) type ExtractNotesResult = (SharedString, StyledRuns, NoteMarkers, NoteMarkers);

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
    /// 脚注标记 `[n]`（由 [`extract_notes`] 从 `[^n]`/`[!n]` 剥记号时登记）：
    /// 静态渲染成 accent 高亮色（对齐 A 侧预览 `link_color = p.accent`）。与
    /// `Row.notes`/`Row.note_defs` 同区间同位置——两处共用同一「显示区间」语义。
    NoteMarker,
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
#[derive(Debug, Clone, Default, PartialEq)]
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
#[allow(dead_code)] // 固有尺寸字段：预览引擎已删后仅 parse 测试消费
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
    /// 每个内容行的 `>` 前缀源字节偏移（暂无用途，后续阶段用）。
    pub quoted: Option<Vec<usize>>,
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
            quoted: None,
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
            quoted: None,
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
            quoted: None,
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
            quoted: None,
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
pub(crate) fn inline_style(src: &str) -> (SharedString, Vec<(Range<usize>, RunKind)>) {
    let (out, runs, _map) = inline_style_mapped(src);
    (out, runs)
}

/// [`inline_style`] 的映射变体：额外返回 **渲染字节 → 源字节** 的映射表，供
/// 实时预览编辑（WYSIWYG）把「渲染位置 → 光标落点的源码字节」换算。
///
/// `map` 长度恒为 `out.len()+1`：`map[r]` = 渲染文本第 `r` 个字节对应的源字节
/// （相对 `src`），`map[out.len()]` = `src.len()`（行尾哨兵）。
///
/// `pub(crate)`：底座 `markdown::doc` 复用它给统一行 [`DocRow`] 装配编辑映射。
pub(crate) fn inline_style_mapped(
    src: &str,
) -> MappedStyleResult {
    // 快路径：`inline_styles_re` 的每个匹配都以 `` ` `` / `*` / `~` / `[` 起始。
    // 一行不含这些字节 → 正则恒无匹配 → 直拷原文 + 恒等映射（与慢路径零匹配
    // 产物逐字节一致）。省掉 debug 下每行 `regex::captures_iter` 的常数开销——这是
    // 大文档滚动每带新进块、输入每键重建可视行时 `shape_docrow` 单价的主成分。
    if !src.bytes().any(|b| matches!(b, b'`' | b'*' | b'~' | b'[')) {
        let map: Vec<usize> = (0..=src.len()).collect();
        return (src.into(), Vec::new(), map);
    }
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
pub(crate) fn strip_align_directive(line: &str) -> (Option<RowAlign>, &str) {
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
///
/// 显示文本都剥掉标记字符（`[^1]`/`[!1]` → `[1]`），记录 `(字节区间, 注记号)`；
/// marker 之后的既有样式段区间整体左移 1 字节（被剥字符删掉）。
pub(crate) fn extract_notes(
    text: SharedString,
    mut runs: Vec<(Range<usize>, RunKind)>,
) -> ExtractNotesResult {
    let src = text.as_ref();
    let mut out = String::with_capacity(src.len());
    let mut notes: Vec<(Range<usize>, usize)> = Vec::new();
    let mut note_defs: Vec<(Range<usize>, usize)> = Vec::new();
    // 注记标记 `[n]` 静态 accent 高亮（回归：P4-2 注记迁入 WYSIWYG 后标号变回正文色）。
    // 标记的显示区间（`out` 的当前字节位）就地收集成 `RunKind::NoteMarker`，循环后与
    // 既有 runs 做无重叠 break-merge（同 WYSIWYG `shape::style_links_in_runs` 的切分），
    // 防止「标记嵌在加粗/链接里」时 `runs_to_text_runs` 的顺延 `i` 把后加的标记 run
    // 吞掉 → 高亮丢失。
    let mut marker_runs: Vec<(Range<usize>, RunKind)> = Vec::new();
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
                    marker_runs.push((mark_start..mark_end, RunKind::NoteMarker));
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
    let runs = merge_marker_runs(out.len(), runs, &marker_runs);
    (out.into(), runs, notes, note_defs)
}

/// 把注记标记区间以 [`RunKind::NoteMarker`] 并入既有 runs，产出**互不重叠**、完全
/// 铺满 `[0, text_len)` 的样式段：段内被标记区间**整段**覆盖 → NoteMarker（标记优先），
/// 否则取覆盖它的既有 run（加粗/链接等）；两不沾 → 普通文本。边界取「既有 run 边界 ∪
/// 标记区间边界」，旧 run 越界（剥记号后区间未收敛）段直接丢弃。
fn merge_marker_runs(
    text_len: usize,
    old: Vec<(Range<usize>, RunKind)>,
    extra: &[(Range<usize>, RunKind)],
) -> Vec<(Range<usize>, RunKind)> {
    if extra.is_empty() {
        return old;
    }
    let mut breaks: Vec<usize> = vec![0, text_len];
    for (rg, _) in &old {
        breaks.push(rg.start);
        breaks.push(rg.end);
    }
    for (rg, _) in extra {
        breaks.push(rg.start);
        breaks.push(rg.end);
    }
    breaks.sort_unstable();
    breaks.dedup();
    let mut out: Vec<(Range<usize>, RunKind)> = Vec::new();
    for w in breaks.windows(2) {
        let a = w[0].min(text_len);
        let b = w[1].min(text_len);
        if a >= b {
            continue;
        }
        let kind = extra
            .iter()
            .find(|(rg, _)| rg.start <= a && b <= rg.end)
            .map(|(_, k)| *k)
            .or_else(|| old.iter().find(|(rg, _)| rg.start <= a && b <= rg.end).map(|(_, k)| *k));
        if let Some(k) = kind {
            out.push((a..b, k));
        }
    }
    out
}

/// 段落文本的行首/行尾修整：剥掉**尾部**空白，但**保留行首** ASCII 空格/Tab 与全角
/// 空格（U+3000）。
/// - 行首留白 = WY 行首按 Tab/空格插入的**可见缩进**（用户 P1：行首空白输入「没用」
///   的根因——此前这里把行首 ASCII 空白剥掉，源文本有空格、渲染却不回显 → 看着没插）。
/// - 本引擎不做 CommonMark 的「4 空格缩进代码块」：块分类只看行首符号
///   （`#`/`-`/`>`/围栏/表格，`parse_to_rows` 先 `line.trim_start()` 再分类），
///   行首空白不改变块语义，故放行可见也不会产生「意外代码块」。
/// - 全角空格（U+3000）保留 += EPUB 正文首行缩进「　　」（`str::trim` 视 U+3000 为
///   空白，此前特殊照顾，现在放行全部行首空白后语义自然覆盖）。
pub(crate) fn trim_paragraph(line: &str) -> &str {
    line.trim_end()
}

/// 一行源码 → 该行传给 [`inline_style_mapped`] 的**文本段**及其在行内的字节偏移。
///
/// 逐类型镜像 [`parse_to_rows`] 的段派生（标题剥 `#`、列表剥前缀、引述剥 `>`、
/// 段落剥行首空格/Tab），供底座 [`crate::markdown::doc`] 为统一行装配
/// 「渲染字节 → 源字节」编辑映射。非文本行（代码/表格/图片/分隔线/空行）返回 `None`。
pub fn text_segment(line: &str, kind: RowKind) -> Option<(usize, &str)> {
    let lead = line.len() - line.trim_start().len();
    let t = &line[lead..];
    match kind {
        RowKind::Paragraph => {
            let seg = trim_paragraph(line);
            if seg.is_empty() {
                return None;
            }
            // 行首 blank 不再剥 → 渲染文本从源字节 0 起（off=0），byte_map 恒等；
            // byte_map 直接把渲染位置换算回源 = 所见即所得（P1）。
            Some((0, seg))
        }
        RowKind::Heading(_) => {
            let hashes = t.chars().take_while(|&c| c == '#').count();
            let body = &t[hashes..];
            let seg = body.trim_start();
            if seg.is_empty() {
                return None;
            }
            Some((lead + hashes + (body.len() - seg.len()), seg))
        }
        RowKind::Bullet => {
            let (_, content) = list_prefix(t)?;
            if content.is_empty() {
                return None;
            }
            let off = lead + (content.as_ptr() as usize - t.as_ptr() as usize);
            Some((off, content))
        }
        RowKind::Quote => {
            let rest = t.trim_start_matches('>').trim_start();
            if rest.is_empty() {
                return None;
            }
            let off = lead + (rest.as_ptr() as usize - t.as_ptr() as usize);
            Some((off, rest))
        }
        _ => None,
    }
}

/// 围栏检测：行首（允许缩进）是否有 ≥3 个 ` 或 ~。
pub(crate) fn fence_len(t: &str) -> Option<(char, usize)> {
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
        .trim_end_matches([',', ';', '.', ':', '!', '?']);
    if lang.is_empty() {
        None
    } else {
        Some(lang)
    }
}

pub(crate) fn is_rule(t: &str) -> bool {
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
pub(crate) fn list_prefix(t: &str) -> Option<(SharedString, &str)> {
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
///
/// 单元格**边界**判定复用 `cells::split_unescaped_pipes`（与 live `table_cells`
/// 共用同一字节机制，见 `markdown::parity`）；本函数另做显示层清理（解义转义竖线）。
/// `pub(crate)` 供奇偶测试（`markdown::parity`）对照 live 侧的 `table_cells`。
pub(crate) fn split_table_cells(line: &str) -> Vec<SharedString> {
    let mut cells: Vec<String> = cells::split_unescaped_pipes(line)
        .map(unescape_table_cell)
        .collect();
    if cells.first().is_some_and(|s| s.trim().is_empty()) {
        cells.remove(0);
    }
    if cells.last().is_some_and(|s| s.trim().is_empty()) {
        cells.pop();
    }
    cells.into_iter().map(|s| s.trim().to_string().into()).collect()
}

/// 预览用单元格显示清理：`\|`→`|`、`\\`→`\`（其余 `\x` 原样保留）。
/// live 侧保留转义原样（源码逐字节、光标/IME 映射），差异见 `parity` #7。
fn unescape_table_cell(s: &str) -> String {
    let mut out = String::new();
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\\' => match it.peek() {
                Some(&n @ ('|' | '\\')) => {
                    out.push(n);
                    it.next();
                }
                _ => out.push('\\'),
            },
            _ => out.push(c),
        }
    }
    out
}

/// [`unescape_table_cell`] 的映射变体：`map[k]` = 渲染字节 `out[k]` 对应的**源字节
/// （相对 `s` 起始）**；长度 = `out.len()+1`，末项哨兵 = `s.len()`。
/// `\|`→`|`、`\\`→`\`（2→1 字节）时按字面字符所在源位映射。
fn unescape_table_cell_mapped(s: &str) -> (String, Vec<usize>) {
    let mut out = String::with_capacity(s.len());
    let mut map = Vec::with_capacity(s.len() + 1);
    let mut cs = s.char_indices().peekable();
    while let Some((idx, c)) = cs.next() {
        if c == '\\' && matches!(cs.peek(), Some(&(_, '|' | '\\'))) {
            let (nidx, n) = *cs.peek().unwrap();
            for b in 0..n.len_utf8() {
                map.push(nidx + b);
            }
            out.push(n);
            cs.next();
            continue;
        }
        for b in 0..c.len_utf8() {
            map.push(idx + b);
        }
        out.push(c);
    }
    map.push(s.len());
    (out, map)
}

/// 按 [`split_table_cells`] **同一 pipeline**（拆未转义 `|` → 解义 → trim → 行内清理）
/// 拆分表格行，并附加每单元格的「渲染字节 → 源字节」映射与源区间，供 WYSIWYG 表格
/// 编辑换算（单元格内容可原地改，表格保持结构）。
///
/// 返回逐单元格 `(源字节区间, 渲染文本, 渲染字节→源字节映射)`：
/// - `源字节区间`相对**该行行首**（trim 掉空白后的内容，`src.end` 为哨兵能指到的尾）；
/// - 映射长度 = `渲染文本.len()+1`，末项哨兵 = `src.end`（相对行首）。
pub fn split_table_cells_mapped(
    line: &str,
) -> Vec<(Range<usize>, SharedString, Vec<usize>)> {
    let base = line.as_ptr() as usize;
    let mut segs: Vec<&str> = cells::split_unescaped_pipes(line).collect();
    if segs.first().is_some_and(|s| s.trim().is_empty()) {
        segs.remove(0);
    }
    if segs.last().is_some_and(|s| s.trim().is_empty()) {
        segs.pop();
    }
    let mut out = Vec::with_capacity(segs.len());
    for seg in segs {
        let raw_off = seg.as_ptr() as usize - base;
        let lead = seg.len() - seg.trim_start().len();
        let content = seg.trim();
        let (u_text, u_map) = unescape_table_cell_mapped(content);
        let t = u_text.trim_start();
        let lead_t = u_text.len() - t.len();
        let (render, _runs, i_map) = inline_style_mapped(t);
        let src_base = raw_off + lead;
        let src_end = raw_off + lead + content.len();
        // 末项哨兵固定 = 内容源尾（`\|`/行内记号不改变源的物理边界）。
        let map: Vec<usize> = i_map
            .iter()
            .map(|&j| src_base + u_map[lead_t + j])
            .collect();
        let mut map = map;
        if let Some(last) = map.last_mut() {
            *last = src_end;
        }
        out.push((src_base..src_end, render, map));
    }
    out
}

/// 分隔行每格必须是 `:?-+:?`（冒号 + 全短横），返回每列对齐；任一格不合规返回 None。
/// 复用共享唯一实现 `cells::parse_aligns`（与 live、IR 同源，见 parity）。
fn parse_delimiter(cells: &[SharedString]) -> Option<Vec<TableAlign>> {
    let strs: Vec<&str> = cells.iter().map(|s| s.as_ref()).collect();
    cells::parse_aligns(&strs).map(|a| {
        a.into_iter()
            .map(|al| match al {
                cells::CellAlign::Left => TableAlign::Left,
                cells::CellAlign::Center => TableAlign::Center,
                cells::CellAlign::Right => TableAlign::Right,
            })
            .collect()
    })
}

/// 尝试把 `lines[idx..]` 解析为一张 GFM 表格。
/// 判据：当前行是含 `|` 的表头，下一行是列数一致的分隔行；随后消费
/// 所有非空且含 `|` 的行作为数据行。返回（表格数据，共消费行数）。
pub(crate) fn try_parse_table(lines: &[&str], idx: usize) -> Option<(TableData, usize)> {
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
pub(crate) fn inline_strip(s: &str) -> SharedString {
    inline_style(s).0
}

/// `![alt](src)` 前缀识别核心（原 `markdown::image` 模块，并入 `parser` 减少零散文件；
/// 与 preview/wysiwyg 的识别语义由 `parity::image_construct_edges_are_documented` 钉死）。
pub struct ImgRef<'a> {
    /// alt（原样，未 trim）。
    pub alt: &'a str,
    /// src（已 trim）。
    pub src: &'a str,
    /// `)` 之后的尾部（未 trim）。
    pub tail: &'a str,
    /// `](` 中 `]` 相对 `![` 之后的偏移（供解析器精确复刻其「首个 `]`」历史规则）。
    pub close_alt_rel: usize,
}

/// 从行首解析 `![alt](src)`：要求 `![` 后第一个 `](` 成对、`(` 后有 `)`、src 非空。
/// 只做前缀匹配；整行 / `{width=N}` / 列表前缀等策略由各调用方决定。
pub fn parse_ref(line: &str) -> Option<ImgRef<'_>> {
    let rest = line.strip_prefix("![")?;
    let close_alt = rest.find("](")?;
    let alt = &rest[..close_alt];
    let after = &rest[close_alt + 2..];
    let close_src = after.find(')')?;
    let src = after[..close_src].trim();
    let tail = &after[close_src + 1..];
    (!src.is_empty()).then_some(ImgRef {
        alt,
        src,
        tail,
        close_alt_rel: close_alt,
    })
}

/// 识别独立图片行 `![alt](src)`（可带尾部空白）。返回 (alt, src)。
/// 前缀识别复用共享核心 [`parse_ref`]，这里叠加「整行」策略：
/// `]` 后必须紧跟 `(`，且 `)` 后只能剩空白。
pub fn parse_image_line(t: &str) -> Option<(String, String)> {
    let r = parse_ref(t)?;
    // 历史语义：alt 由「第一个 `]`」界定且必须紧跟 `(`。`parse_ref` 取的是第一处
    // `](`；仅当 `]` 出现在 `](` 之前时二者不同（如 `![a]b](c)`）→ 既有实现返回 None。
    let close_pos = 2 + r.close_alt_rel; // `](` 的 `]` 在 `t` 中的字节偏移
    if t.as_bytes()[..close_pos].contains(&b']') {
        return None;
    }
    if !r.tail.trim().is_empty() {
        return None;
    }
    Some((r.alt.to_string(), r.src.to_string()))
}

/// 引号区：从 `region`（首个元素是 `>` 开头行）收集连续 `>` 行成引号区，
/// 逐行用 [`blockquote_prefix`] 剥掉 `>` 前缀（含其后至多一个空格 + 前导空白），
/// 记录每行「正文起点」相对真实源行的字节偏移，然后交给 [`parse_rows_inside`]
/// 对区内内容递归子分类（表格/围栏/列表/标题/分隔线/图片；其余降级 [`RowKind::Quote`]）。
/// 返回（引号区内全部渲染行，消费的源码行数）。
fn parse_quote_region(region: &[&str], base: usize) -> (Vec<Row>, usize) {
    let mut j = 0;
    while j < region.len() && region[j].trim_start().starts_with('>') {
        j += 1;
    }
    let mut body: Vec<&str> = Vec::with_capacity(j);
    let mut offs: Vec<usize> = Vec::with_capacity(j);
    for raw in &region[..j] {
        let raw = raw.trim_end_matches('\r');
        match blockquote_prefix(raw) {
            Some(p) => {
                let body0 = &raw[p..];
                let b = body0.trim_start();
                offs.push(p + (body0.len() - b.len()));
                body.push(b);
            }
            None => {
                offs.push(0);
                body.push(raw);
            }
        }
    }
    parse_rows_inside(&body, &offs, base)
}

/// 解析引号区内容（剥过 `>` 前缀的 body 行），复用与顶层 [`parse_to_rows`]
/// **完全一致**的判定顺序（围栏/空行/标题/残留引述/列表/分隔线/图片/表格/段落），
/// 保证块边界与 [`super::blocks::classify_block`] 严格一致（窗口物化不变式）。
///
/// - 结构与顶层同名（`Table`/`Code`/`Bullet`/`Heading(n)`/`Rule`/`Image`），并携带
///   `quoted`：每个被覆盖内容行的 `>` 前缀源字节偏移（单行块 = 1 元素）。
/// - 非结构内容行（含空行）降级为 [`RowKind::Quote`]，保持引号灰带连续。
fn parse_rows_inside(body: &[&str], offs: &[usize], base: usize) -> (Vec<Row>, usize) {
    let mut rows = Vec::new();
    let mut idx = 0usize;
    while idx < body.len() {
        let row_abs = base + idx;
        let (align, line) = strip_align_directive(body[idx]);
        let t = line.trim_start();
        // 栅栏开头：收集到同字符闭合行之间的所有行，折叠成一个 Code 渲染行
        if let Some((fc, _)) = fence_len(t) {
            let language = fence_language(t).map(|s| s.into());
            let mut code_lines = Vec::new();
            let mut content_offs = Vec::new();
            idx += 1;
            while idx < body.len() {
                let l = body[idx];
                if let Some((c, _)) = fence_len(l.trim_start())
                    && c == fc {
                    idx += 1;
                    break;
                }
                code_lines.push(SharedString::from(l));
                content_offs.push(offs[idx]);
                idx += 1;
            }
            let mut row = Row::code_block(code_lines, language).with_line(row_abs);
            row.quoted = Some(content_offs);
            rows.push(row);
            continue;
        }
        if t.is_empty() {
            rows.push(Row::new("", RowKind::Quote, Vec::new(), None).with_line(row_abs));
            idx += 1;
            continue;
        }
        // 标题：`#{1,6} ` 后跟内容
        let hashes = t.chars().take_while(|&c| c == '#').count();
        if (1..=6).contains(&hashes) && t.len() > hashes {
            let (raw, runs) = inline_style(t[hashes..].trim_start());
            let (text, runs, notes, note_defs) = extract_notes(raw, runs);
            let mut row = Row::new(
                text.as_ref(),
                RowKind::Heading(hashes as u8),
                runs,
                None,
            )
            .with_line(row_abs)
            .with_format(align, notes, note_defs);
            row.quoted = Some(vec![offs[idx]]);
            rows.push(row);
            idx += 1;
            continue;
        }
        // 残留 `>`（如 `>  >` 双空格剥后仍以 `>` 开头）：按单行引述处理，
        // 剥前缀口径与 `text_segment(Quote)` 一致（byte_map 对齐）。
        if t.starts_with('>') {
            let (content_align, rest) = strip_align_directive(t.trim_start_matches('>').trim_start());
            let align = content_align.or(align);
            let (raw, runs) = inline_style(rest);
            let (text, runs, notes, note_defs) = extract_notes(raw, runs);
            rows.push(
                Row::new(text.as_ref(), RowKind::Quote, runs, None)
                    .with_line(row_abs)
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
            let mut row = Row::new(
                text.as_ref(),
                RowKind::Bullet,
                runs,
                Some(prefix.clone()),
            )
            .with_line(row_abs)
            .with_format(align, notes, note_defs);
            row.quoted = Some(vec![offs[idx]]);
            rows.push(row);
            idx += 1;
            continue;
        }
        // 水平分隔线
        if is_rule(t) {
            let mut row = Row::new("", RowKind::Rule, Vec::new(), None).with_line(row_abs);
            row.quoted = Some(vec![offs[idx]]);
            rows.push(row);
            idx += 1;
            continue;
        }
        // 独立图片行：`![alt](src)` 整行占位
        if let Some((alt, src)) = parse_image_line(t) {
            let mut row = Row::new_image(&alt, &src).with_line(row_abs);
            row.quoted = Some(vec![offs[idx]]);
            rows.push(row);
            idx += 1;
            continue;
        }
        // 表格：当前行是表头、下一行是分隔行（结构块优先于表格，故放在最后判定）
        if let Some((table, consumed)) = try_parse_table(body, idx) {
            let content_offs: Vec<usize> = (idx..idx + consumed)
                .filter(|&k| k != idx + 1) // 跳过分隔行
                .map(|k| offs[k])
                .collect();
            let mut row = Row::with_table(table).with_line(row_abs);
            row.quoted = Some(content_offs);
            rows.push(row);
            idx += consumed;
            continue;
        }
        // 段落：引号区内降级为 Quote
        let (raw, runs) = inline_style(trim_paragraph(line));
        let (text, runs, notes, note_defs) = extract_notes(raw, runs);
        rows.push(
            Row::new(text.as_ref(), RowKind::Quote, runs, None)
                .with_line(row_abs)
                .with_format(align, notes, note_defs),
        );
        idx += 1;
    }
    (rows, idx)
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
                if let Some((c, _)) = fence_len(l.trim_start())
                    && c == fc {
                        idx += 1;
                        break;
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
        // 引述：`>` 开头——收集连续 `>` 行成引号区，剥前缀后对区内内容
        // 递归子分类（表格/围栏/列表/标题/分隔线/图片/段落），让块级结构在
        // 引用内也被解析（2026-11-13）。
        if t.starts_with('>') {
            let (sub_rows, consumed) = parse_quote_region(&lines[idx..], row_start);
            rows.extend(sub_rows);
            idx += consumed;
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