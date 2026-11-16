//! Markdown 文档的「所见即所得」行内滤镜（Phase 1）。
//!
//! 目标：让 `.md` 文件的编辑区在**纯文本不变**的前提下，把语法记号画淡、把正文
//! 应用真实富样式（加粗 / 斜体 / 删除线 / 行内代码 / 链接 / 标题 / 列表 / 引用 /
//! 围栏），左编辑区即有明显 WYSIWYG 观感，同时保留右侧真渲染预览。
//!
//! 关键设计（与[`crate::editor::view::merge_highlights`]配合）：
//! - **只改样式、绝不改文本**：底层 Rope/光标/选区/撤销/IME 一个字节都不动，
//!   因此不存在「视觉位置↔字节偏移」映射问题（字仍占位，只是被淡化）；
//! - 输出与语法高亮同构：**行内不变的 `Vec<(Range, HighlightStyle)>`**，直接喂给
//!   现有的 `merge_highlights` 做选区合并与去重叠，零新渲染机制；
//! - 跨行只有一种状态：**代码围栏**（``` / ~~~）。窗口扫描维护围栏开闭，
//!   围栏内容行整行套 `Code` 样式（不再识别行内记号），围栏标记行套 `Marker`。
//!
//! 缓存：与 [`crate::editor::highlight::Highlighter`] 同款「文档身份 × 版本 × 行窗口」
//! 缓存，大文档只扫描可见窗口（+ `lookback` 回看恢复围栏状态），与文档总大小无关；
//! 身份维度 = `Buffer::id`（O(1)）：兜住「新 Buffer 从 version=0 起撞上一份文档键」
//! 的替换场景，且命中帧零重哈希（早期用整窗文本指纹，每帧 O(窗口) 白白重算）。

use std::ops::Range;

use gpui::{FontStyle, FontWeight, HighlightStyle, Hsla, StrikethroughStyle, UnderlineStyle, px};

use crate::editor::buffer::Buffer;

/// 记号淡化强度（gpui `fade_out`：alpha × (1 − 值)）。0.75 → 保留 25% 透明度，
/// 依稀可见但明显弱于正文；想完全隐藏记号改成 1.0 即可（底稿仍占位、仍可编辑）。
const MARKER_FADE: f32 = 0.75;

/// 行内装饰类型：决定一段字形画什么富样式。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Deco {
    /// 语法记号（`#`、`**`、`` ` ``、`-`、`>`、`[`、`(` 等）：淡化。
    Marker,
    /// 加粗正文。
    Bold,
    /// 斜体正文。
    Italic,
    /// 粗斜体正文。
    BoldItalic,
    /// 删除线正文。
    Strike,
    /// 行内代码 / 围栏内容：等宽底色。
    Code,
    /// 链接/图片的显示文字（`[text]` / `![alt]`）。
    LinkText,
    /// 链接/图片的 URL 或路径（`(url)`，淡化）。
    LinkUrl,
    /// 标题正文（加粗 + 标题色）。
    Heading,
}

/// 代码围栏（开/关共用）：字符 + 最小长度（闭合需同字符且长度 ≥ 开栏）。
#[derive(Clone, Copy, PartialEq, Eq)]
struct Fence {
    ch: u8,
    n: usize,
}

/// 可见核心长度：去掉尾随 `\r`。
fn core_len(line: &str) -> usize {
    line.trim_end_matches('\r').len()
}

/// `i` 处字符占用字节数（ASCII 1，其余按 UTF-8）。
fn char_len(line: &str, i: usize) -> usize {
    line[i..].chars().next().map_or(1, |c| c.len_utf8())
}

/// 是否“词字符”（字母/数字/下划线）——用于 `_`/`*` 强调的词边界判断。
fn is_word(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
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

/// 从 `from`（含）起找第一段 ≥1 个 `ch` 的连续段，返回 (start, end)；无则 None。
fn find_run_from(line: &str, from: usize, ch: u8) -> Option<(usize, usize)> {
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
        k += char_len(line, k);
    }
    None
}

/// 行是否为代码围栏（开/关均可）：0-3 空格缩进 + ≥3 个 `` ` `` / `~`。
/// 返回 (字符, 长度)；非围栏返回 None。
fn scan_block_marker(line: &str) -> Option<Fence> {
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
    Some(Fence { ch, n })
}

/// 列表标记：返回 (start, end)（相对开头）。支持 `-`/`*`/`+` 及 `1.`/`1)`。
fn list_marker(s: &str) -> Option<(usize, usize)> {
    let b = s.as_bytes();
    if b.is_empty() {
        return None;
    }
    match b[0] {
        b'-' | b'*' | b'+' => {
            if s.len() == 1 || matches!(b[1], b' ' | b'\t') {
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

/// `[text](url)`：返回 (闭合 ']' 索引, 闭合 ')' 索引)；无 (url) 时退化为 (ci, ci)。
fn read_link(line: &str, open: usize) -> Option<(usize, usize)> {
    let b = line.as_bytes();
    let mut depth = 0usize;
    let mut ci = None;
    for (k, &c) in b.iter().enumerate().skip(open + 1) {
        match c {
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
    if after < b.len() && b[after] == b'('
        && let Some(mut d) = b[after + 1..].iter().position(|&x| x == b')') {
            d += after + 1;
            return Some((ci, d));
        }
    Some((ci, ci))
}

/// 把强调/删除线内容（`start..end`）装饰为**互不重叠**的段：整段基线为 `base`
/// 样式，内层出现的强调/代码/链接等以更小范围覆盖其上（`combine` 合并样式）。
/// gpui 渲染要求 runs 不重叠，因此这里按内层段切分、gap 填基线，不能简单叠加。
fn scan_emph_content(
    text: &str,
    start: usize,
    end: usize,
    base: Deco,
    out: &mut Vec<(Range<usize>, Deco)>,
) {
    if end <= start {
        return;
    }
    let mut tmp = Vec::new();
    scan_inline(&text[start..end], 0, &mut tmp);
    if tmp.is_empty() {
        out.push((start..end, base));
        return;
    }
    let mut cursor = start;
    for (r, d) in tmp {
        let (s, e) = (start + r.start, start + r.end);
        if cursor < s {
            out.push((cursor..s, base));
        }
        if e > s {
            // 内层记号始终淡化（不继承外层样式）
            let cd = if d == Deco::Marker { Deco::Marker } else { combine(base, d) };
            out.push((s..e, cd));
        }
        cursor = e;
    }
    if cursor < end {
        out.push((cursor..end, base));
    }
}

/// 内层样式与外层强调样式的合并：粗×斜 → 粗斜；删除线优先（视觉整段贯穿）；
/// 代码/链接自足（不继承外层）；其余继承外层。
fn combine(base: Deco, inner: Deco) -> Deco {
    match (base, inner) {
        (_, Deco::Strike) => Deco::Strike,
        (Deco::Strike, _) => Deco::Strike,
        (Deco::BoldItalic, _) => Deco::BoldItalic,
        (Deco::Bold, Deco::Italic) | (Deco::Italic, Deco::Bold) => Deco::BoldItalic,
        (Deco::Bold, Deco::Bold) | (Deco::Italic, Deco::Italic) => base,
        (_, Deco::Code) | (_, Deco::LinkText) | (_, Deco::LinkUrl) | (_, Deco::Heading) => inner,
        _ => base,
    }
}

/// 把 `[text](url)` / `![alt](url)` 分解为「记号 / 文字 / URL」装饰。
/// `start` = `[` 所在位置（图片时 `!` 在其前一字节）；`is_image` 决定是否吃下 `!`。
fn emit_link(
    start: usize,
    ci: usize,
    cp: usize,
    is_image: bool,
    out: &mut Vec<(Range<usize>, Deco)>,
) {
    let push = |out: &mut Vec<(Range<usize>, Deco)>, r: Range<usize>, d: Deco| {
        if !r.is_empty() {
            out.push((r, d));
        }
    };
    if is_image {
        push(out, start.saturating_sub(1)..start + 1, Deco::Marker); // `![`
    } else {
        push(out, start..start + 1, Deco::Marker); // `[`
    }
    if ci > start + 1 {
        push(out, start + 1..ci, Deco::LinkText); // text / alt
    }
    push(out, ci..ci + 1, Deco::Marker); // `]`
    if cp > ci {
        push(out, ci + 1..ci + 2, Deco::Marker); // `(`
        if cp > ci + 2 {
            push(out, ci + 2..cp, Deco::LinkUrl);
        }
        push(out, cp..cp + 1, Deco::Marker); // `)`
    }
}

/// 行内扫描（从 `from` 字节起）：反引号代码、强调/删除线、链接/图片、任务复选框。
/// 产出互不重叠、按序的 `(Range, Deco)`；未覆盖的区间 = 纯文本（无装饰）。
fn scan_inline(text: &str, from: usize, out: &mut Vec<(Range<usize>, Deco)>) {
    let b = text.as_bytes();
    let n = b.len();
    let mut i = from;
    while i < n {
        let ch = b[i];
        // 转义：`\X` 视为纯文本（跳过反斜杠与其后一个完整字符，多字节安全）
        if ch == b'\\' && i + 1 < n {
            i += 1 + char_len(text, i + 1);
            continue;
        }
        // 行内代码 `code`
        if ch == b'`' {
            let j = i + count_run(text, i, b'`');
            if let Some((cs, ce)) = find_run_from(text, j, b'`') {
                out.push((i..j, Deco::Marker));
                out.push((j..cs, Deco::Code));
                out.push((cs..ce, Deco::Marker));
                i = ce;
                continue;
            }
            i = j;
            continue;
        }
        // 强调：*** > ** > *（`_` 需词边界）
        if ch == b'*' || ch == b'_' {
            let run = count_run(text, i, ch);
            let t = if run >= 3 { 3 } else if run >= 2 { 2 } else { 1 };
            // `_` 开号前必须是词边界（避免 snake_case 误判为斜体）
            if ch == b'_' && i > 0 && is_word(b[i - 1]) {
                i += 1;
                continue;
            }
            if let Some((cs, ce)) = find_run_from(text, i + t, ch)
                && ce - cs >= t {
                    // 闭号的词边界要求（`_` 关号后不能紧跟词字符）
                    if ch == b'_' && ce < n && is_word(b[ce]) {
                        i += t;
                        continue;
                    }
                    let inner = match t {
                        3 => Deco::BoldItalic,
                        2 => Deco::Bold,
                        _ => Deco::Italic,
                    };
                    out.push((i..i + t, Deco::Marker));
                    scan_emph_content(text, i + t, cs, inner, out);
                    out.push((cs..cs + t, Deco::Marker));
                    i = cs + t;
                    continue;
                }
            i += t;
            continue;
        }
        // 删除线 ~~~~
        if ch == b'~' && i + 1 < n && b[i + 1] == b'~' {
            if let Some((cs, ce)) = find_run_from(text, i + 2, b'~')
                && ce - cs >= 2 {
                    out.push((i..i + 2, Deco::Marker));
                    scan_emph_content(text, i + 2, cs, Deco::Strike, out);
                    out.push((cs..cs + 2, Deco::Marker));
                    i = ce;
                    continue;
                }
            i += 2;
            continue;
        }
        // 图片 ![alt](url)
        if ch == b'!' && i + 1 < n && b[i + 1] == b'['
            && let Some((ci, cp)) = read_link(text, i + 1) {
                emit_link(i + 1, ci, cp, true, out);
                i = (cp + 1).min(n);
                continue;
            }
        // 链接 / 任务复选框
        if ch == b'[' {
            // 任务复选框 [ ] / [x] / [X]
            if n - i >= 3 && b[i + 2] == b']' && matches!(b[i + 1], b' ' | b'x' | b'X') {
                let marker_end = i + 3;
                out.push((i..marker_end, Deco::Marker));
                // 复选框后的空白也并入记号
                let mut k = marker_end;
                while k < n && b[k] == b' ' {
                    k += 1;
                }
                i = k;
                continue;
            }
            if let Some((ci, cp)) = read_link(text, i) {
                emit_link(i, ci, cp, false, out);
                i = (cp + 1).min(n);
                continue;
            }
        }
        i += char_len(text, i);
    }
}

/// 扫描一行为装饰 run（不限围栏状态；供单行测试/无状态路径使用）。
fn scan_line(line: &str) -> Vec<(Range<usize>, Deco)> {
    let core = core_len(line);
    let s = &line[..core];
    let trimmed = s.trim_start();
    let ind = s.len() - trimmed.len();
    let mut out: Vec<(Range<usize>, Deco)> = Vec::new();

    // —— ATX 标题：1-6 个 '#' + 空格/行尾 ——
    let hashes = trimmed.chars().take_while(|&c| c == '#').count();
    if (1..=6).contains(&hashes) {
        let after = ind + hashes;
        let ok = after >= core || matches!(s.as_bytes()[after], b' ' | b'\t');
        if ok {
            out.push((ind..after, Deco::Marker));
            let mut cs = after;
            if cs < core && matches!(s.as_bytes()[cs], b' ' | b'\t') {
                cs += 1;
            }
            if cs < core {
                out.push((cs..core, Deco::Heading));
            }
            return out;
        }
    }

    // —— 引用行：'>' ——
    if trimmed.starts_with('>') {
        out.push((ind..ind + 1, Deco::Marker));
        scan_inline(s, ind + 1, &mut out);
        return out;
    }

    // —— 分隔线：3+ 个相同记号 ——
    if is_hr(trimmed) {
        out.push((0..core, Deco::Marker));
        return out;
    }

    // —— 围栏开/闭行：仅记号段淡化，语言说明保持正常 ——
    if let Some(f) = scan_block_marker(trimmed) {
        let end = (ind + f.n).min(core);
        if ind < end {
            out.push((ind..end, Deco::Marker));
        }
        return out;
    }

    // —— 列表标记 ——
    if let Some((ms, me)) = list_marker(trimmed) {
        out.push((ind + ms..ind + me, Deco::Marker));
        scan_inline(s, ind + me, &mut out);
        return out;
    }

    // —— 行内 ——
    scan_inline(s, 0, &mut out);
    out
}

/// 把装饰 run 解析成最终 `HighlightStyle`（颜色/加粗/斜体/删除线/底/淡化），
/// 明暗随 `dark`。颜色单一来源 `ui::theme::ColorScheme`（第 3 波迁移：
/// 原 `CODE_BG_*/HEADING_*/LINK_*` 硬编码收编进语义色板，值逐字节不变）。
fn resolve_runs(runs: &[(Range<usize>, Deco)], dark: bool) -> Vec<(Range<usize>, HighlightStyle)> {
    let cs = if dark {
        ui::theme::ColorScheme::dark()
    } else {
        ui::theme::ColorScheme::light()
    };
    let code_bg: Hsla = cs.editor_code_bg.into();
    let link: Hsla = cs.link.into();
    let heading: Hsla = cs.heading.into();

    let mut out = Vec::with_capacity(runs.len());
    for (r, deco) in runs {
        let mut s = HighlightStyle::default();
        match deco {
            // 记号：不设色（继承主题前景），只淡化——与周围纯文本色调天然一致
            Deco::Marker => {
                s.fade_out = Some(MARKER_FADE);
            }
            Deco::Bold => {
                s.font_weight = Some(FontWeight::BOLD);
            }
            Deco::Italic => {
                s.font_style = Some(FontStyle::Italic);
            }
            Deco::BoldItalic => {
                s.font_weight = Some(FontWeight::BOLD);
                s.font_style = Some(FontStyle::Italic);
            }
            Deco::Strike => {
                s.strikethrough = Some(StrikethroughStyle { thickness: px(1.0), color: None });
            }
            // 代码：主题前景 + 浅底
            Deco::Code => {
                s.background_color = Some(code_bg);
            }
            Deco::LinkText => {
                s.color = Some(link);
                s.underline = Some(UnderlineStyle { thickness: px(1.0), color: Some(link), wavy: false });
            }
            Deco::LinkUrl => {
                s.color = Some(link);
                s.fade_out = Some(MARKER_FADE);
            }
            Deco::Heading => {
                s.color = Some(heading);
                s.font_weight = Some(FontWeight::BOLD);
            }
        }
        out.push((r.clone(), s));
    }
    out
}

/// Markdown「所见即所得」滤镜：按可见窗口扫描并缓存（文档 id × 版本 × 窗口 × 明暗）。
/// 可见性：`editor`（视图层）的 `mod.rs`/`document.rs` 跨 crate 引用 → 转 pub；
/// 字段与 `new`/`hits`/`rescans` 保持对内可见（视图只经 `Default`/`set_dark`/`scan_window` 使用）。
pub struct MarkdownDecor {
    dark: bool,
    /// 缓存键：`(buffer.id(), buffer.version, 请求 lo, 请求 hi, dark)`。
    /// 身份维度 = `Buffer::id`（O(1)）：`set_text`/`reset_document` 替换 Buffer 后
    /// version 从 0 起、id 必新，换一份文档即身份不同 → 必 miss → 自愈重扫，不再
    /// 依赖对整窗文本重哈希兜撞键（命中帧零重哈希；早期 `row_window_fingerprint` 每帧
    /// O(窗口) 重算）。version 仍保留，负责「同一文档内编辑」失效（编辑必有版本跳变）。
    key: Option<(u64, u64, usize, usize, bool)>,
    /// 扫描起始行（含回看前缀）。
    base_row: usize,
    lines: Vec<Vec<(Range<usize>, HighlightStyle)>>,
    /// 命中计数（测试探针：命中 = 静止帧直接复用缓存切片，零重扫零重哈希）。
    #[cfg(test)]
    pub(crate) hits: usize,
    /// 重扫计数（测试探针：仅版本 / 文档身份 / 明暗变化或窗口越界才重扫）。
    #[cfg(test)]
    pub(crate) rescans: usize,
}

impl MarkdownDecor {
    pub(crate) fn new() -> Self {
        Self {
            dark: false,
            key: None,
            base_row: 0,
            lines: Vec::new(),
            #[cfg(test)]
            hits: 0,
            #[cfg(test)]
            rescans: 0,
        }
    }

    /// 切换明暗色板（主题切换时调用；缓存因此失效）。
    /// 可见性：`editor`（视图层）的 `render.rs` 跨 crate 消费 → 转 pub。
    pub fn set_dark(&mut self, dark: bool) {
        if self.dark != dark {
            self.dark = dark;
            self.key = None;
        }
    }

    /// 请求可见行窗口 `[lo, hi]`（含两端）的装饰样式。返回**与请求窗口对齐**的
    /// 切片：第 0 项 = `lo` 行、长度 = `hi - lo + 1`。内部以 `lookback` 行回看起点
    /// 重建围栏状态（未闭合围栏从窗口外开始会按普通围栏处理，可接受的取舍）。
    /// 可见性：`editor`（视图层）的 `rows.rs` 跨 crate 消费 → 转 pub。
    pub fn scan_window(
        &mut self,
        buffer: &Buffer,
        lo: usize,
        hi: usize,
        lookback: usize,
    ) -> &[Vec<(Range<usize>, HighlightStyle)>] {
        let line_count = buffer.line_count();
        if line_count == 0 {
            self.lines.clear();
            self.base_row = 0;
            self.key = None;
            return &self.lines;
        }
        let lo = lo.min(line_count - 1);
        let hi = hi.min(line_count - 1);
        let version = buffer.version();

        // 命中条件：同文档（id）+ 同版本 + 同明暗 + 请求窗口落在已扫描切片内。
        // - `id` 判文档身份：换 Buffer 后 version 归零也撞不了上一份文档的键（O(1)）；
        // - `version` 判同文档内编辑：`apply_edit/undo/redo/append_chunk` 均有版本跳变，
        //   同版本 ⟹ 切片文本与原扫描完全一致 → 直接复用，零重哈希（早期实现此处对
        //   整窗缓存切片重跑指纹 O(窗口)；`line_text` 逐行拷贝也不再发生）。
        // 越界守卫：新文档更短时 `hi_orig` 可能越过 `line_count`，必须当 miss 处理。
        if let Some((id, v, _l, _h, d)) = self.key
            && id == buffer.id() && v == version && d == self.dark && lo >= self.base_row {
                let rel = lo - self.base_row;
                let hi_orig = self.base_row + self.lines.len() - 1;
                if rel + (hi - lo) < self.lines.len() && hi_orig < line_count {
                    #[cfg(test)]
                    {
                        self.hits += 1;
                    }
                    return &self.lines[rel..rel + (hi - lo) + 1];
                }
            }

        let start = lo.saturating_sub(lookback);
        let mut fence: Option<Fence> = None;
        let mut lines: Vec<Vec<(Range<usize>, HighlightStyle)>> = Vec::with_capacity(hi - start + 1);
        for row in start..=hi {
            let bullet = buffer.line_text(row);
            let text = bullet.as_ref();
            let core = core_len(text);
            let s = &text[..core];

            let runs: Vec<(Range<usize>, Deco)> = match fence {
                Some(f) if is_closing_fence(s, f) => {
                    fence = None;
                    scan_line(s)
                }
                Some(_) => {
                    // 围栏内容行：整行代码样式（不再识别行内记号）
                    if core > 0 {
                        vec![(0..core, Deco::Code)]
                    } else {
                        Vec::new()
                    }
                }
                None => match scan_block_marker(s.trim_start()) {
                    Some(f) => {
                        fence = Some(f);
                        scan_line(s)
                    }
                    None => scan_line(s),
                },
            };
            lines.push(resolve_runs(&runs, self.dark));
        }

        self.lines = lines;
        self.base_row = start;
        self.key = Some((buffer.id(), version, lo, hi, self.dark));
        #[cfg(test)]
        {
            self.rescans += 1;
        }
        &self.lines[lo - start..lo - start + (hi - lo) + 1]
    }
}

/// `line`（已去 \r）是否为 `fence` 的闭合栏（同字符且长度 ≥ 开栏）。
fn is_closing_fence(line: &str, fence: Fence) -> bool {
    scan_block_marker(line.trim_start()).is_some_and(|f| f.ch == fence.ch && f.n >= fence.n)
}

impl Default for MarkdownDecor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds_of(line: &str) -> Vec<(Range<usize>, Deco)> {
        scan_line(line)
    }

    fn names(line: &str) -> Vec<Deco> {
        kinds_of(line).into_iter().map(|(_, d)| d).collect()
    }

    #[test]
    fn heading_marker_and_content() {
        assert_eq!(names("# 标题"), vec![Deco::Marker, Deco::Heading]);
        assert_eq!(names("## 二级"), vec![Deco::Marker, Deco::Heading]);
        // 无空格的 `#x` 不算标题
        assert_eq!(names("#x"), vec![]);
    }

    #[test]
    fn quote_and_hr_and_list() {
        assert_eq!(names("> 引用"), vec![Deco::Marker]);
        assert_eq!(names("---"), vec![Deco::Marker]);
        assert_eq!(names("- 列表"), vec![Deco::Marker]);
        assert_eq!(names("1. 有序"), vec![Deco::Marker]);
        // 任务复选框 = 记号
        assert_eq!(names("- [ ] 待办"), vec![Deco::Marker, Deco::Marker]);
        assert_eq!(names("- [x] 完成"), vec![Deco::Marker, Deco::Marker]);
    }

    #[test]
    fn inline_code_does_not_break_following_bold() {
        // 回归：行内代码 `` `we` `` 出现后，同行的 `**标识接口**` 必须仍是粗体。
        let line = "我们`we`可以将它理解成一个**标识接口**。";
        let kinds = kinds_of(line);
        assert!(
            kinds
                .iter()
                .any(|(r, d)| *d == Deco::Bold && &line[r.clone()] == "标识接口"),
            "代码后粗体应生效：{kinds:?}"
        );
        // 代码段与粗体段互不重叠。
        let code = kinds.iter().find(|(_, d)| *d == Deco::Code).expect("有代码段");
        let bold = kinds
            .iter()
            .find(|(_, d)| *d == Deco::Bold)
            .expect("有粗体段");
        assert!(code.0.end <= bold.0.start, "代码段应结束于粗体段之前");
    }

    #[test]
    fn inline_emphasis() {
        assert_eq!(names("**加粗**"), vec![Deco::Marker, Deco::Bold, Deco::Marker]);
        assert_eq!(names("*斜体*"), vec![Deco::Marker, Deco::Italic, Deco::Marker]);
        assert_eq!(
            names("***粗斜***"),
            vec![Deco::Marker, Deco::BoldItalic, Deco::Marker]
        );
        assert_eq!(names("~~删~~"), vec![Deco::Marker, Deco::Strike, Deco::Marker]);
        // 嵌套：粗内含斜
        assert_eq!(
            names("**a _b_ c**"),
            vec![
                Deco::Marker,
                Deco::Bold,
                Deco::Marker,
                Deco::BoldItalic,
                Deco::Marker,
                Deco::Bold,
                Deco::Marker,
            ]
        );
        // 行内代码
        assert_eq!(names("`code`"), vec![Deco::Marker, Deco::Code, Deco::Marker]);
    }

    #[test]
    fn underscore_inside_word_is_plain() {
        assert_eq!(names("snake_case_name"), vec![]);
        assert_eq!(names("_it_"), vec![Deco::Marker, Deco::Italic, Deco::Marker]);
    }

    #[test]
    fn links_and_images() {
        assert_eq!(
            names("[链接](https://x)"),
            vec![
                Deco::Marker,     // `[`
                Deco::LinkText,   // 链接
                Deco::Marker,     // `]`
                Deco::Marker,     // `(`
                Deco::LinkUrl,    // url
                Deco::Marker,     // `)`
            ]
        );
        assert_eq!(
            names("![图](a.png)"),
            vec![
                Deco::Marker,
                Deco::LinkText,
                Deco::Marker,
                Deco::Marker,
                Deco::LinkUrl,
                Deco::Marker,
            ]
        );
        // 无 url 退化为纯链接文字
        assert_eq!(
            names("[plain]"),
            vec![Deco::Marker, Deco::LinkText, Deco::Marker]
        );
    }

    #[test]
    fn escaped_markers_are_plain() {
        assert_eq!(names(r"\*not italic\*"), vec![]);
    }

    #[test]
    fn runs_non_overlapping_and_in_line() {
        let line = "# 标题 **加粗** `code` [链接](https://x) - 项";
        let runs = kinds_of(line);
        let mut prev = 0usize;
        for (r, _) in &runs {
            assert!(r.start >= prev, "run 应非重叠有序: {r:?}");
            assert!(r.end <= line.len(), "run 应在行内: {r:?}");
            prev = r.end;
        }
        // 明暗两套都能解析
        for dark in [false, true] {
            let _ = resolve_runs(&runs, dark);
        }
    }

    #[test]
    fn window_fence_state_machine() {
        let src = "# 标题\n```rust\nlet x = **bold**;\n```\n- 列表\n";
        let b = Buffer::new(src);
        let mut dec = MarkdownDecor::new();
        let rows = dec.scan_window(&b, 0, 4, 16).to_vec();
        assert_eq!(rows.len(), 5);

        // 标题行：有 Marker + Heading
        assert!(rows[0].iter().any(|(_, s)| s.font_weight == Some(FontWeight::BOLD)));
        // 围栏内容行：整行 Code（背景）
        assert!(rows[2].iter().any(|(_, s)| s.background_color.is_some()));
        // 列表行：存在淡化记号段
        assert!(rows[4].iter().any(|(_, s)| s.fade_out.is_some()));
        // 同一窗口重查命中缓存（内容稳定）
        let again = dec.scan_window(&b, 0, 4, 16).to_vec();
        assert_eq!(again.len(), 5);
        assert_eq!(again[0], rows[0]);
    }

    #[test]
    fn window_aligns_to_request() {
        let src = (0..40)
            .map(|i| format!("line {i} **b{i}**"))
            .collect::<Vec<_>>()
            .join("\n");
        let b = Buffer::new(&src);
        let mut dec = MarkdownDecor::new();
        let lo = 10usize;
        let hi = 15usize;
        let rows = dec.scan_window(&b, lo, hi, 16).to_vec();
        assert_eq!(rows.len(), hi - lo + 1, "切片应第 0 项 = lo 行");
        for r in lo..=hi {
            let line = &rows[r - lo];
            let text = b.line_text(r);
            let bold = line
                .iter()
                .any(|(_, s)| s.font_weight == Some(FontWeight::BOLD));
            assert!(bold, "行 {r} 应有加粗段: {text:?}");
        }
    }

    #[test]
    fn doc_identity_prevents_version_zero_stale_window_after_buffer_replacement() {
        // 回归（用户确认的删除标题 `#` → 切换 panic 的色彩层根因）：`commit_live` 写回
        // 用 `Buffer::new` 替换文档后 version 归零（新 Buffer 从 version=0 起），旧实现
        // 靠 `reset_document` 显式 `clear()` 兜底防 `(version=0, lo, hi)` 命中上一份文档
        // 的样式区间——区间端越过新行长时 `StyledText::with_highlights` 崩在
        // `is_char_boundary(run.end)`。M0.3 后缓存键含文档身份（`Buffer::id`，O(1)），
        // 替换文档后 id 不同 → 必 miss → 按新文本重扫；本测试即验证**不调 clear() 也能
        // 自愈**（早期「扫描切片文本指纹」每帧对窗口 O(窗口) 重哈希，已改为 id 判别）。
        let src = Buffer::new("# 标题\n");
        let mut dec = MarkdownDecor::new();
        let before = dec.scan_window(&src, 0, 0, 16).to_vec();
        assert!(
            before[0].iter().any(|(_, s)| s.font_weight == Some(FontWeight::BOLD)),
            "标题行应有样式（否则测试无意义）"
        );
        assert!(
            before[0].iter().any(|(r, _)| r.end > " 标题".len()),
            "旧文档标题样式应落在 ` 标题`（7B）之外: {:?}",
            before[0]
        );
        let rescans_first = dec.rescans;

        // 替换文档（模拟 `commit_live::set_text`）：新旧 Buffer 都从 version=0 起（撞键前提）。
        let dst = Buffer::new(" 标题\n");
        assert_eq!(
            src.version(),
            dst.version(),
            "新旧 Buffer 都从 version=0 起（缓存键碰撞前提）"
        );

        // 不调 clear()：id 兜住身份（版本同但 id 不同 → miss → 重扫），新文档重扫出
        // 正确样式（区间全在行内、无标题）。命中帧不动，重扫计数必 +1。
        let fresh = dec.scan_window(&dst, 0, 0, 16).to_vec();
        assert_eq!(dec.rescans, rescans_first + 1, "换 Buffer 必须重扫，不得命中旧样式");
        assert!(
            fresh[0].iter().all(|(r, _)| r.end <= " 标题".len()),
            "新文档应重扫，全部区间在行内: {:?}",
            fresh
        );
        assert!(
            !fresh[0]
                .iter()
                .any(|(_, s)| s.font_weight == Some(FontWeight::BOLD)),
            "「 标题」（无 `#`）不再是标题，不应有粗体样式: {:?}",
            fresh
        );
    }

    #[test]
    fn static_frame_hits_do_not_rescan_and_version_edit_does() {
        // 优化验收：命中帧（纯滚动/悬停，同文档同版本）直接复用缓存切片，零重扫零
        // 重哈希（早期 `row_window_fingerprint` 每帧对窗口 O(窗口) 重算）；仅编辑
        // （版本跳变）才重扫。
        let src = (0..60)
            .map(|i| format!("line {i} **b{i}**"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut dec = MarkdownDecor::new();
        let mut b = Buffer::new(&src);
        dec.scan_window(&b, 4, 20, 16).to_vec();
        let rescans_first = dec.rescans;
        let hits_first = dec.hits;

        // 同文档同版本连续 200 帧（滚动命中帧的真实路数）：全部命中，不重扫不重哈希。
        for _ in 0..200 {
            let rows = dec.scan_window(&b, 4, 20, 16).to_vec();
            assert_eq!(rows.len(), 17, "命中切片长度 = hi - lo + 1");
        }
        assert_eq!(
            dec.rescans,
            rescans_first,
            "静止帧不得重扫（命中路径零重哈希）: rescans={}",
            dec.rescans
        );
        assert_eq!(dec.hits, hits_first + 200, "200 帧应全部命中");

        // 同一文档内编辑（`apply_edit` → version +1，id 不变）：必须重扫刷出最新样式。
        let range = b.line_start(4)..b.line_start(4) + b.line_len(4);
        b.apply_edit(range.clone(), "# 标题4 **b4**");
        let rows = dec.scan_window(&b, 4, 20, 16).to_vec();
        assert_eq!(
            dec.rescans,
            rescans_first + 1,
            "编辑（版本变）应重扫一次"
        );
        assert_eq!(dec.hits, hits_first + 200, "编辑那次是 miss，不再计命中");
        assert!(
            rows[0].iter().any(|(_, s)| s.font_weight == Some(FontWeight::BOLD)),
            "编辑后重扫仍出加粗（内容没动粗体）"
        );
    }

    #[test]
    fn utf8_escape_no_panic_scan_line() {
        // 红测（复现）：`\`+多字节 UTF-8（`\你` `\…` `\🍎`）是合法转义输入，禁崩。
        // `scan_inline` 转义分支（markdown.rs:287）固定 `i += 2` 把 i 落进多字节字符
        // 中间（非边界），循环尾 `i += char_len(text, i)`（markdown.rs:374）对
        // `text[i..]` 按字节切片 → `byte index ... is not a char boundary`（markdown.rs:67）。
        let cases = [r"\你", r"\…", r"\🍎"];
        for line in cases {
            let names = scan_line(line);
            assert!(
                names
                    .iter()
                    .all(|(r, _)| line.is_char_boundary(r.start) && line.is_char_boundary(r.end)),
                "转义多字节后不得产生非边界 run: {line:?} -> {names:?}"
            );
        }
    }

    #[test]
    fn utf8_escape_no_panic_then_inline() {
        // 红测（复现续）：`\`+多字节后再接词法扫描（`**` / 行内代码），i 停在多字节中间
        // 的状态无法自愈，后续 `char_len`/`find_run_from`（markdown.rs:97 亦经 char_len）
        // 仍在非边界切片 → 同样 panic。
        let cases = [r"\你**加粗**", r"- 我们\🍎 后面还有", r"a\… `code` b"];
        for line in cases {
            let _ = scan_line(line);
        }
    }

    #[test]
    fn utf8_escape_no_panic_via_window() {
        // 红测（真实用户路径）：编辑器扫 `.md` 缓冲窗口（scan_window → scan_line →
        // scan_inline），含 `\`+多字节的行即崩。
        let src = "# 标题\n\\你\n- \\🍎 列表\n";
        let b = Buffer::new(src);
        let mut dec = MarkdownDecor::new();
        let _ = dec.scan_window(&b, 0, 2, 16);
    }

    #[test]
    fn utf8_escape_multibyte_then_inline_parses() {
        // 正例：`\你` 整体视为纯文本（不产生装饰），且不吞掉后续结构——
        // `\你**加粗**` 尾部 `**加粗**` 照常解析成粗体。
        let line = r"\你**加粗**";
        let kinds = kinds_of(line);
        // `\你` 4 字节（`\` + 3 字节 `你`）必须在所有 run 之外 = 纯文本。
        assert!(
            kinds.iter().all(|(r, _)| r.start >= 4 || r.end <= 1),
            "转义段 `\\你` 不得被装饰：{kinds:?}"
        );
        assert!(
            kinds
                .iter()
                .any(|(r, d)| *d == Deco::Bold && &line[r.clone()] == "加粗"),
            "`\\你` 后的 **加粗** 应正常解析：{kinds:?}"
        );
        // 边界安全：转义段起止与各 run 均落在字符边界。
        assert!(
            kinds
                .iter()
                .all(|(r, _)| line.is_char_boundary(r.start) && line.is_char_boundary(r.end)),
            "转义多字节后不得产生非边界 run: {kinds:?}"
        );
    }
}
