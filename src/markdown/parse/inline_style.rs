//! 自 `markdown::live`（已移除）迁至 `markdown` 顶层，与 `wysiwyg`/`view`/`ir`/`parity`
//! 共用识别层。本文件 = 行内样式/块级区域识别：标题/警报/数学/表格/折叠/图片行等，
//! 供当前各消费方；曾是 W1 行内扫描引擎主体（`styled_runs`/`hidden_runs` 等）已随
//! 死代码清理删除（无生产引用、仅其自身单测）。

use std::ops::Range;

use gpui::TextRun;

use super::cells::{is_backslash_escaped, split_unescaped_pipes};

/// ATX heading depth (1–6) if `line` is a heading: 1–6 leading `#` followed by
/// a space or end-of-line. `None` otherwise.
pub(crate) fn heading_level(line: &str) -> Option<u8> {
    let b = line.as_bytes();
    let mut n = 0;
    while n < b.len() && b[n] == b'#' {
        n += 1;
    }
    ((1..=6).contains(&n) && (n == b.len() || b[n] == b' ')).then_some(n as u8)
}

/// [`heading_level`], also matching a heading behind a list marker
/// (`- ### Notes`, the outliner shape). Returns the depth plus the line's
/// indent when it's a LIST heading (`None` = a top-level heading) — list
/// headings fold by indent, not by heading boundaries.
pub(crate) fn line_heading_level(line: &str) -> Option<(u8, Option<usize>)> {
    if let Some(l) = heading_level(line) {
        return Some((l, None));
    }
    let p = list_prefix(line).map(|(p, ..)| p)?;
    let l = heading_level(&line[p..])?;
    Some((l, Some(line.len() - line.trim_start().len())))
}

/// Line ranges (heading line through section end) of collapsed heading
/// sections: `folded` holds the trimmed source lines of folded headings
/// (`## Goals`). A top-level section runs from its heading to the next
/// heading of the same or a higher level, fence-aware — mirrors
/// [`super::recognition::extract_section`]. A LIST heading's section
/// (`- ### Notes`) is its indented children: it runs while lines sit deeper
/// than the heading's own indent (blank lines included). Every line matching
/// a folded key folds (duplicate headings fold together; the key is the line
/// text).
pub(crate) fn heading_fold_regions(
    content: &str,
    folded: &std::collections::HashSet<String>,
) -> Vec<Range<usize>> {
    if folded.is_empty() {
        return Vec::new();
    }
    let lines: Vec<&str> = content.split('\n').collect();
    let indent = |l: &str| l.len() - l.trim_start().len();
    let mut out = Vec::new();
    let mut in_fence = false;
    let mut i = 0;
    while i < lines.len() {
        if lines[i].trim_start().starts_with("```") {
            in_fence = !in_fence;
            i += 1;
            continue;
        }
        let level = if in_fence {
            None
        } else {
            line_heading_level(lines[i])
        };
        match level {
            Some((l, list_indent)) if folded.contains(lines[i].trim()) => {
                let start = i;
                i += 1;
                while i < lines.len() {
                    if lines[i].trim_start().starts_with("```") {
                        in_fence = !in_fence;
                    } else if !in_fence {
                        let stop = match list_indent {
                            None => heading_level(lines[i]).is_some_and(|n| n <= l),
                            Some(ind) => !lines[i].trim().is_empty() && indent(lines[i]) <= ind,
                        };
                        if stop {
                            break;
                        }
                    }
                    i += 1;
                }
                out.push(start..i);
            }
            _ => i += 1,
        }
    }
    out
}

/// True if `line` is a thematic break (horizontal rule): three or more of the
/// same `-`, `*`, or `_`, separated only by optional spaces — e.g. `---`, `***`,
/// `- - -`. The editor paints a divider in its place (reveal-on-caret). Matches
/// CommonMark, ignoring the rare setext-underline ambiguity this app doesn't use.
#[allow(dead_code)]
pub(crate) fn thematic_break(line: &str) -> bool {
    let mut chars = line.bytes().filter(|b| !b.is_ascii_whitespace());
    let Some(first) = chars.next() else {
        return false;
    };
    if !matches!(first, b'-' | b'*' | b'_') {
        return false;
    }
    let mut count = 1;
    for b in chars {
        if b != first {
            return false;
        }
        count += 1;
    }
    count >= 3
}
pub(crate) use super::recognition::{AlertKind, alert_prefix};

/// Each alert (callout) region in `content`: an alert marker line (with or
/// without a fold char) plus its `>` continuation lines, ending at a blank /
/// non-quote line or the next alert marker. Yields the source-line range, the
/// kind, and the fold state (`Some` only when the marker carries `-`/`+`).
/// Marker recognition delegates to [`super::recognition::alert_prefix`] (single
/// source); the two public queries below derive from here.
pub(crate) fn alert_regions(content: &str) -> Vec<(Range<usize>, AlertKind, Option<bool>)> {
    fn marker(line: &str) -> Option<(AlertKind, Option<bool>)> {
        let p = blockquote_prefix(line)?;
        let (kind, _, fold) = alert_prefix(&line[p..])?;
        Some((kind, fold))
    }
    let lines: Vec<&str> = content.split('\n').collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if let Some((kind, fold)) = marker(lines[i]) {
            let start = i;
            i += 1;
            while i < lines.len()
                && blockquote_prefix(lines[i]).is_some()
                && marker(lines[i]).is_none()
            {
                i += 1;
            }
            out.push((start..i, kind, fold));
        } else {
            i += 1;
        }
    }
    out
}

/// Foldable-callout regions (`> [!NOTE]-` / `+`): the line range + fold state,
/// derived from [`alert_regions`]. Body lines of a folded region collapse in
/// the WYSIWYG view unless the caret is inside (reveal-on-caret).
/// 生产路径经 `WyEditor::derived()` 直接消费 [`alert_regions`]；此薄壳仅测试用。
#[cfg(test)]
pub(crate) fn alert_fold_regions(content: &str) -> Vec<(Range<usize>, bool)> {
    alert_regions(content)
        .into_iter()
        .filter_map(|(r, _, fold)| fold.map(|f| (r, f)))
        .collect()
}

/// 每个警报（callout）区域的 `AlertKind`（marker 行 + 连续 `>` body 行）——
/// 供 WYSIWYG 把整段 callout 的左条统一画成警报色。键 = 源码行号；含/不含
/// 折叠符的警报都覆盖（由 [`alert_regions`] 派生）。生产路径经
/// `WyEditor::derived()` 消费；此薄壳仅测试用。
#[cfg(test)]
pub(crate) fn alert_region_kinds(content: &str) -> std::collections::HashMap<usize, AlertKind> {
    let mut out = std::collections::HashMap::new();
    for (r, kind, _) in alert_regions(content) {
        for l in r {
            out.insert(l, kind);
        }
    }
    out
}

pub(crate) fn blockquote_prefix(line: &str) -> Option<usize> {
    let b = line.as_bytes();
    if b.first() != Some(&b'>') {
        return None;
    }
    let mut p = 0;
    while p < b.len() && b[p] == b'>' {
        p += 1;
        if p < b.len() && b[p] == b' ' {
            p += 1;
        }
    }
    Some(p)
}

/// If `line` is a list item, return `(prefix_len, indent, ordered, number)`.
/// `prefix_len` is the byte length of the leading whitespace plus the marker
/// (a bullet `-`/`*`/`+`, or digits then `.`/`)`) plus one space; `indent` is the
/// leading-whitespace length (nesting depth); `ordered`/`number` describe an
/// ordered item. The editor hides this prefix and paints a bullet/number,
/// revealing the raw prefix on caret.
pub(crate) fn list_prefix(line: &str) -> Option<(usize, usize, bool, u32)> {
    let b = line.as_bytes();
    let mut i = 0;
    while i < b.len() && (b[i] == b' ' || b[i] == b'\t') {
        i += 1;
    }
    let indent = i;
    // Unordered: `-`/`*`/`+` then a space.
    if i < b.len() && matches!(b[i], b'-' | b'*' | b'+') && b.get(i + 1) == Some(&b' ') {
        return Some((i + 2, indent, false, 0));
    }
    // Ordered: one or more digits, then `.` or `)`, then a space.
    let ds = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    if i > ds && matches!(b.get(i), Some(b'.') | Some(b')')) && b.get(i + 1) == Some(&b' ') {
        let num = line[ds..i].parse::<u32>().unwrap_or(1);
        return Some((i + 2, indent, true, num));
    }
    None
}

/// `(position, nesting level)` of every ordered list item, both 0-based-
/// level / 1-based-position. Word-style, NOT CommonMark: every list counts

/// A copied slice with its ordered markers rewritten to their DISPLAYED

/// If `line` is a GFM task item — a list item whose body starts with `[ ]`,
/// `[x]`, or `[X]` then a space — return `(prefix_len, indent, checked)`, where
/// `prefix_len` covers the list marker plus the checkbox. The editor hides this
/// prefix and paints a ☐/☑ box (parity with the reading view).
pub(crate) fn task_prefix(line: &str) -> Option<(usize, usize, bool)> {
    let (list_len, indent, ..) = list_prefix(line)?;
    let rest = &line.as_bytes()[list_len..];
    if rest.len() >= 4
        && rest[0] == b'['
        && rest[2] == b']'
        && rest[3] == b' '
        && matches!(rest[1], b' ' | b'x' | b'X')
    {
        return Some((list_len + 4, indent, matches!(rest[1], b'x' | b'X')));
    }
    None
}

/// Toggle a GFM task item's checkbox in `line` — flip the char between the
/// brackets (`[ ]`↔`[x]`). Returns the rewritten line, or `None` if `line` isn't
/// a task item. The length is unchanged (one ASCII byte swapped).
pub(crate) fn toggle_task_checkbox(line: &str) -> Option<String> {
    let (prefix_len, _indent, checked) = task_prefix(line)?;
    let box_byte = prefix_len - 3; // the char between `[` and `]`
    let mut out = line.to_string();
    out.replace_range(box_byte..box_byte + 1, if checked { " " } else { "x" });
    Some(out)
}

/// If `line` is a standalone image — `![alt](src)`, optionally followed by a
/// `{width=N}` / `{width=Npx}` attribute, with only whitespace around it —
/// return `(src, explicit_width)`. The editor renders such a line as the image
/// (W4) when the caret is elsewhere; a line with any other trailing text stays
/// plain source. Prefix recognition is shared with the preview parser
/// (`crate::markdown::parser::parse_ref`, parity-pinned).
#[allow(dead_code)]
pub(crate) fn image_line(line: &str) -> Option<(&str, Option<f32>)> {
    let r = crate::markdown::parser::parse_ref(line.trim())?;
    let tail = r.tail.trim();
    let width = if tail.is_empty() {
        None
    } else {
        let w = tail.strip_prefix("{width=")?.strip_suffix('}')?;
        Some(
            w.strip_suffix("px")
                .unwrap_or(w)
                .trim()
                .parse::<f32>()
                .ok()?,
        )
    };
    Some((r.src, width))
}

/// Like [`image_line`], but also matches an image that is the sole body of a
/// list item — `- ![](src)` / `1. ![](src){width=N}`. Returns `(src, width,
/// marker_len)`, where `marker_len` is the byte length of the leading list
/// marker (0 for a plain standalone image). The editor renders the image inset
/// past the marker so a bulleted image keeps its bullet, instead of the row
/// collapsing to the image or falling back to raw source.
#[allow(dead_code)]
pub(crate) fn image_row(line: &str) -> Option<(&str, Option<f32>, usize)> {
    if let Some((src, width)) = image_line(line) {
        return Some((src, width, 0));
    }
    let (plen, ..) = list_prefix(line)?;
    let (src, width) = image_line(&line[plen..])?;
    Some((src, width, plen))
}

/// Horizontal alignment of a display `$$…$$` block, chosen per-block via a
/// `<!-- math:left -->` / `<!-- math:right -->` marker comment on the line directly above it.
/// `Center` is the default (no marker), matching LaTeX display math; standard Markdown viewers
/// ignore the comment.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum MathAlign {
    Left,
    #[default]
    Center,
    Right,
}

impl MathAlign {
    fn from_name(name: &str) -> Option<Self> {
        match name {
            "left" => Some(Self::Left),
            "center" => Some(Self::Center),
            "right" => Some(Self::Right),
            _ => None,
        }
    }
}

/// Parse a `<!-- math:ALIGN -->` marker line. `None` if it isn't one.
pub(crate) fn math_align_marker(line: &str) -> Option<MathAlign> {
    let inner = line
        .trim()
        .strip_prefix("<!--")?
        .strip_suffix("-->")?
        .trim();
    MathAlign::from_name(inner.strip_prefix("math:")?.trim())
}

/// A detected `$$…$$` block: its line range (both fences), the LaTeX between them, its
/// alignment, and the optional `<!-- math:ALIGN -->` marker line directly above it.
#[derive(Clone)]
pub(crate) struct MathRegion {
    pub range: Range<usize>,
    pub source: String,
    pub align: MathAlign,
}

pub(crate) fn math_regions(content: &str) -> Vec<MathRegion> {
    let lines: Vec<&str> = content.split('\n').collect();
    let mut out = Vec::new();
    let mut in_code = false;
    let mut i = 0;
    while i < lines.len() {
        if lines[i].trim_start().starts_with("```") {
            // `$$` inside a fenced code block (shell PIDs!) is never math.
            in_code = !in_code;
            i += 1;
            continue;
        }
        if in_code {
            i += 1;
            continue;
        }
        if lines[i].trim() == "$$" {
            let start = i;
            let mut j = i + 1;
            while j < lines.len() && lines[j].trim() != "$$" {
                j += 1;
            }
            let source = lines[start + 1..j].join("\n");
            let end = (j + 1).min(lines.len()); // include the closing fence
            // An alignment marker on the line directly above the opening fence.
            let align = match start
                .checked_sub(1)
                .and_then(|m| math_align_marker(lines[m]))
            {
                Some(a) => a,
                None => MathAlign::default(),
            };
            out.push(MathRegion {
                range: start..end,
                source,
                align,
            });
            i = end;
        } else if let Some(inner) = one_line_math(lines[i]) {
            // `$$x^2$$` ALONE on a line is display math (issue #54): block
            // treatment (size, centering, alignment marker). Lines mixing
            // words with a pair are normalized apart at edit time (see
            // `apply_auto_replace`) — never hidden.
            let align = match i.checked_sub(1).and_then(|m| math_align_marker(lines[m])) {
                Some(a) => a,
                None => MathAlign::default(),
            };
            out.push(MathRegion {
                range: i..i + 1,
                source: inner.to_string(),
                align,
            });
            i += 1;
        } else {
            i += 1;
        }
    }
    out
}

/// The inner LaTeX when the whole (trimmed) line is one `$$…$$` pair with
/// non-empty content — the one-line display-math form.
pub(crate) fn one_line_math(line: &str) -> Option<&str> {
    let t = line.trim();
    let inner = t.strip_prefix("$$")?.strip_suffix("$$")?;
    let inner = inner.trim();
    // `$$$$`, `$$ $$`, and interior `$$`s (two pairs jammed together) are
    // not the one-line form.
    (!inner.is_empty() && !inner.contains("$$")).then_some(inner)
}

/// Inline `$…$` math spans within a single text line (NOT block `$$` fences) — byte ranges
/// covering the whole span, both `$` delimiters included. Follows the common (pandoc) rule so
/// prose like "it cost $5 and $10" isn't mistaken for math: the opening `$` is followed by a
/// non-space, the closing `$` is preceded by a non-space and not followed by a digit, the
/// content is non-empty, `$$` is skipped (block-fence-ish / empty), and `\$` is a literal.
///
/// `$` and `\` are ASCII bytes that can't occur inside a multi-byte UTF-8 sequence, so the
/// byte scan is char-safe and the returned ranges fall on char boundaries.
/// Every inline `![alt](src)` image on `line`, as `(full span, src range)`.
/// A whole-line image is handled as a block widget (`image_row`) before a line
/// reaches inline shaping, so these are the mixed-line (text + image) ones.
/// Images inside inline code aren't matched.
pub(crate) fn inline_image_spans(line: &str) -> Vec<(Range<usize>, Range<usize>)> {
    let b = line.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 1 < b.len() {
        if b[i] == b'!'
            && b[i + 1] == b'['
            && let Some(rb) = line[i + 2..].find(']')
            && line[i + 2 + rb + 1..].starts_with('(')
            && let Some(rp) = line[i + 2 + rb + 2..].find(')')
        {
            let src = (i + 2 + rb + 2)..(i + 2 + rb + 2 + rp);
            out.push((i..(src.end + 1), src.clone()));
            i = src.end + 1;
        } else {
            i += 1;
        }
    }
    out
}

/// The LaTeX inside an inline-math `span` of `line` — strips one `$` per
/// side, or two for a same-line `$$…$$` display pair.
pub(crate) fn inline_math_latex<'a>(line: &'a str, span: &Range<usize>) -> &'a str {
    let d = if line[span.clone()].starts_with("$$") {
        2
    } else {
        1
    };
    &line[span.start + d..span.end - d]
}

pub(crate) fn inline_math_spans(line: &str) -> Vec<Range<usize>> {
    let bytes = line.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'$' || is_backslash_escaped(bytes, i) {
            i += 1;
            continue;
        }
        // `$$…$$` on ONE line is display math (issue #54; block `$$` fences
        // live on their own lines and never form a same-line pair). Unlike
        // `$…$` there are no space rules — pandoc display math allows them —
        // but the pair must close on this line around non-space content.
        if bytes.get(i + 1) == Some(&b'$') {
            let mut j = i + 2;
            let mut close = None;
            while j + 1 < bytes.len() {
                if bytes[j] == b'$' && bytes[j + 1] == b'$' && !is_backslash_escaped(bytes, j) {
                    close = Some(j);
                    break;
                }
                j += 1;
            }
            match close {
                Some(c) if line[i + 2..c].trim().is_empty() => {}
                Some(c) => {
                    out.push(i..c + 2);
                    i = c + 2;
                    continue;
                }
                None => {}
            }
            i += 2;
            continue;
        }
        // Opening `$`: reject a space right after.
        match bytes.get(i + 1) {
            None => {
                i += 1;
                continue;
            }
            Some(c) if c.is_ascii_whitespace() => {
                i += 1;
                continue;
            }
            _ => {}
        }
        // Scan for a valid closing `$`: unescaped, non-space before, not a digit after. A `$`
        // that can't close (space before, e.g. the second `$` in "$5 and $10") is skipped and
        // the scan continues for a later valid one, as pandoc does.
        let mut j = i + 1;
        let mut close = None;
        while j < bytes.len() {
            if bytes[j] == b'$' && !is_backslash_escaped(bytes, j) {
                let space_before = bytes[j - 1].is_ascii_whitespace();
                let digit_after = bytes.get(j + 1).is_some_and(|c| c.is_ascii_digit());
                if !space_before && !digit_after {
                    close = Some(j);
                    break;
                }
            }
            j += 1;
        }
        match close {
            Some(c) => {
                out.push(i..c + 1);
                i = c + 1;
            }
            None => i += 1,
        }
    }
    out
}

/// Where one inline formula sits in a shaped line: the byte offset of its spacer in the DISPLAY
/// string (to position the painted image) and the SOURCE byte range of the `$…$` span (to
/// hit-test a click back to the formula and to target its edit/commit).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct InlineMathPlace {
    pub display_off: usize,
    /// Byte length of the spacer. Needed to place the raster on an RTL row,
    /// where `display_off` is the spacer's RIGHT edge, not its left.
    pub len: usize,
    pub source: Range<usize>,
}

/// The sub-slice of `runs` covering display byte range `[lo, hi)`, clipping the runs that
/// straddle an edge. `runs` must tile the display in order (as `hidden_runs` emits them).
fn clip_runs(runs: &[TextRun], lo: usize, hi: usize) -> Vec<TextRun> {
    let mut out = Vec::new();
    let mut off = 0;
    for r in runs {
        let (s, e) = (off, off + r.len);
        off = e;
        let (cs, ce) = (s.max(lo), e.min(hi));
        if cs < ce {
            out.push(TextRun {
                len: ce - cs,
                ..r.clone()
            });
        }
    }
    out
}

/// Replace each inline formula's `$…$` glyphs with an invisible spacer the caller paints the
/// typeset image over. Takes `hidden_runs` output (`display`, `runs`, `map` — the display→source
/// byte map of length `display.len() + 1`) and the line's `formulas` as `(source range, spacer
/// width in spaces)`, sorted by source start and non-overlapping. Returns the rewritten
/// `display` / `runs` / `map` plus each formula's placement. The spacer borrows `gap`'s font/
/// color (its glyphs are spaces, so only the advance matters) and maps back to the span start,
/// so a click on it lands the caret at the formula (which then reveals raw source / opens the
/// editor).
pub(crate) fn splice_inline_math(
    display: &str,
    runs: &[TextRun],
    map: &[usize],
    formulas: &[(Range<usize>, usize)],
    gap: &TextRun,
) -> (String, Vec<TextRun>, Vec<usize>, Vec<InlineMathPlace>) {
    let mut nd = String::with_capacity(display.len());
    let mut nm: Vec<usize> = Vec::with_capacity(map.len());
    let mut nr: Vec<TextRun> = Vec::new();
    let mut places = Vec::new();
    let mut cursor = 0; // display byte offset consumed so far
    for (src, n) in formulas {
        // The display range covering this span: first display offset reaching the span's
        // start, up to the first reaching its end (`map` is monotonic non-decreasing).
        let d0 = map
            .iter()
            .position(|&s| s >= src.start)
            .unwrap_or(display.len());
        let d1 = map
            .iter()
            .position(|&s| s >= src.end)
            .unwrap_or(display.len());
        if d0 < cursor {
            continue; // overlapping / out-of-order formula — skip defensively
        }
        // Verbatim text before the formula.
        nd.push_str(&display[cursor..d0]);
        nm.extend_from_slice(&map[cursor..d0]);
        nr.extend(clip_runs(runs, cursor, d0));
        // The spacer, mapped back to the span start.
        places.push(InlineMathPlace {
            display_off: nd.len(),
            len: *n,
            source: src.clone(),
        });
        let src_at = map.get(d0).copied().unwrap_or(src.start);
        for _ in 0..*n {
            nd.push(' ');
        }
        nm.extend(std::iter::repeat_n(src_at, *n));
        nr.push(TextRun {
            len: *n,
            ..gap.clone()
        });
        cursor = d1;
    }
    // Tail after the last formula.
    nd.push_str(&display[cursor..]);
    nm.extend_from_slice(&map[cursor..display.len()]);
    nr.extend(clip_runs(runs, cursor, display.len()));
    nm.push(*map.last().unwrap_or(&0)); // final source-end sentinel
    (nd, nr, nm, places)
}

/// A detected GFM table region: the half-open range of logical line indices it
/// spans (header, separator, then body rows). The editor no longer renders
/// tables (they show as source); this stays because `markdown::ir` and
/// `markdown::parity` read `lines` to model/verify tables in the reader.
#[derive(Clone)]
pub(crate) struct TableRegion {
    pub lines: Range<usize>,
}

/// Detect GFM table regions in `content` (W4c). A region is a row line (trimmed
/// text starts with `|`) immediately followed by a separator row
/// (`| --- | :--: |`), then any further row lines. Returns regions in order.
pub(crate) fn table_regions(content: &str) -> Vec<TableRegion> {
    let lines: Vec<&str> = content.split('\n').collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 1 < lines.len() {
        if is_table_row(lines[i]) && is_separator_row(lines[i + 1]) {
            let start = i;
            let mut end = i + 2; // header + separator
            // Stop before a new table jammed directly below: a row that is itself
            // immediately followed by a `|---|` separator is the next table's
            // header, not a body row of this one — otherwise adjacent tables (no
            // blank line between) merge into one grid and the second separator
            // shows up as `---` cells.
            while end < lines.len()
                && is_table_row(lines[end])
                && !is_separator_row(lines.get(end + 1).copied().unwrap_or(""))
            {
                end += 1;
            }
            out.push(TableRegion {
                lines: start..end,
            });
            i = end;
        } else {
            i += 1;
        }
    }
    out
}

/// A table row is a line whose trimmed text starts with `|`.
pub(crate) fn is_table_row(line: &str) -> bool {
    line.trim_start().starts_with('|')
}

/// Split a `| a | b |` row into trimmed cell strings (the bounding pipes drop the
/// empty leading/trailing cells they'd otherwise create).
pub(crate) fn table_cells(line: &str) -> Vec<&str> {
    let t = line.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = t.strip_suffix('|').unwrap_or(t);
    split_unescaped_pipes(t).map(str::trim).collect()
}

/// The unescaped-pipe splitter and backslash-escape check live in
/// [`super::cells`] — shared byte machinery with the preview parser
/// (edge semantics pinned by `markdown::parity`).

/// Is `line` a table separator row (every cell is dashes with optional
/// alignment colons)? Alignment semantics are the single implementation
/// `super::cells::parse_aligns` (shared with the preview parser and
/// the semantic IR — parity-pinned).
fn is_separator_row(line: &str) -> bool {
    if !is_table_row(line) {
        return false;
    }
    let cells = table_cells(line);
    !cells.is_empty() && super::cells::parse_aligns(&cells).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spans(line: &str) -> Vec<&str> {
        inline_math_spans(line)
            .into_iter()
            .map(|r| &line[r])
            .collect()
    }

    #[test]
    fn inline_math_basic() {
        assert_eq!(spans("the area $\\pi r^2$ of a circle"), vec!["$\\pi r^2$"]);
        assert_eq!(spans("$x$"), vec!["$x$"]);
        assert_eq!(spans("a $x$ and $y$ b"), vec!["$x$", "$y$"]);
        // Spaces are fine *inside* a span; only the immediate inner edges must be non-space.
        assert_eq!(spans("$x = 5$ done"), vec!["$x = 5$"]);
    }

    #[test]
    fn inline_math_rejects_money_and_empties() {
        assert!(spans("it cost $5 and $10 total").is_empty());
        assert!(spans("$ x $").is_empty()); // space right after opener / before closer
        assert!(spans("a $$ b").is_empty()); // `$$` is not an inline span
        assert!(spans("lone $ dollar").is_empty());
        // Closer immediately followed by a digit isn't a closer (pandoc rule).
        assert!(spans("$x$5").is_empty());
    }

    #[test]
    fn inline_math_escapes_and_later_close() {
        // `\$` is a literal dollar, not a delimiter.
        assert!(spans("price is 5\\$ even").is_empty());
        assert_eq!(spans("\\$ and $x$"), vec!["$x$"]);
        // A `$` that can't close is skipped; a later valid one closes the span.
        assert_eq!(spans("$a $5 b$ end"), vec!["$a $5 b$"]);
    }

    fn run(len: usize) -> TextRun {
        TextRun {
            len,
            font: gpui::font("Helvetica"),
            color: gpui::Hsla::default(),
            background_color: None,
            underline: None,
            strikethrough: None,
        }
    }

    /// A plain text run of `len` bytes (style is irrelevant to the splice; only `len` matters).
    fn runs_len(runs: &[TextRun]) -> usize {
        runs.iter().map(|r| r.len).sum()
    }

    #[test]
    fn splice_one_formula() {
        // "a $x$ b" with the `$x$` span (bytes 2..5) → a 3-space spacer. display == source here,
        // so the map is the identity (length 8).
        let display = "a $x$ b";
        let map: Vec<usize> = (0..=display.len()).collect();
        let (nd, nr, nm, places) =
            splice_inline_math(display, &[run(display.len())], &map, &[(2..5, 3)], &run(0));
        assert_eq!(nd, "a     b"); // "a " + 3 spaces + " b" = a + 5 spaces + b
        assert_eq!(
            places,
            vec![InlineMathPlace {
                display_off: 2,
                len: 3,
                source: 2..5
            }]
        );
        assert_eq!(nm, vec![0, 1, 2, 2, 2, 5, 6, 7]); // spacer bytes map to the span start (2)
        assert_eq!(nm.len(), nd.len() + 1);
        assert_eq!(runs_len(&nr), nd.len()); // runs still tile the display
    }

    #[test]
    fn splice_two_formulas_and_leading() {
        // Two spans, the first at the very start: "$a$ and $b$".
        let display = "$a$ and $b$";
        let map: Vec<usize> = (0..=display.len()).collect();
        let (nd, nr, nm, places) = splice_inline_math(
            display,
            &[run(display.len())],
            &map,
            &[(0..3, 2), (8..11, 4)],
            &run(0),
        );
        assert_eq!(nd, "   and     "); // 2 spaces + " and " + 4 spaces
        assert_eq!(
            places,
            vec![
                InlineMathPlace {
                    display_off: 0,
                    len: 2,
                    source: 0..3
                },
                InlineMathPlace {
                    display_off: 7,
                    len: 4,
                    source: 8..11
                },
            ]
        );
        assert_eq!(nm.len(), nd.len() + 1);
        assert_eq!(runs_len(&nr), nd.len());
        assert_eq!(*nm.last().unwrap(), display.len()); // sentinel preserved
    }

    #[test]
    fn splice_no_formulas_is_identity() {
        let display = "plain text";
        let map: Vec<usize> = (0..=display.len()).collect();
        let (nd, nr, nm, places) =
            splice_inline_math(display, &[run(display.len())], &map, &[], &run(0));
        assert_eq!(nd, display);
        assert_eq!(nm, map);
        assert!(places.is_empty());
        assert_eq!(runs_len(&nr), nd.len());
    }

    #[test]
    fn inline_math_multibyte_safe() {
        // A multi-byte char (é, —) adjacent to a delimiter counts as non-space; ranges stay
        // on char boundaries.
        let line = "café $x$ — π";
        assert_eq!(spans(line), vec!["$x$"]);
        for r in inline_math_spans(line) {
            assert!(line.is_char_boundary(r.start) && line.is_char_boundary(r.end));
        }
    }

    #[test]
    fn forgiving_math_forms() {
        // Words mixed with math are NORMALIZED apart (edit-time), never
        // hidden: mixed lines are not regions themselves.
        assert!(math_regions("words $$E=mc^2$$ more").is_empty());
        assert!(math_regions("fees are $$5 and $$10 total").is_empty());
        assert!(math_regions("wer $$\nu(m)=v(n,x)\n$$ wer").is_empty());
        // `$$` inside a fenced code block is never math.
        assert!(math_regions("```sh\necho $$\n$$\n```").is_empty());
    }

    #[test]
    fn inline_display_math_double_dollar() {
        // Same-line `$$…$$` is display math (issue #54): one span covering
        // both delimiter pairs, spaces inside allowed.
        assert_eq!(spans("$$E=mc^2$$"), vec!["$$E=mc^2$$"]);
        assert_eq!(spans("a $$ x + y $$ b"), vec!["$$ x + y $$"]);
        assert_eq!(inline_math_latex("$$E=mc^2$$", &(0..10)), "E=mc^2");
        assert_eq!(inline_math_latex("$x$", &(0..3)), "x");
        // Unclosed / empty pairs match nothing; a lone `$$` fence line is
        // the BLOCK scanner's business.
        assert_eq!(spans("$$"), Vec::<&str>::new());
        assert_eq!(spans("$$$$"), Vec::<&str>::new());
        assert_eq!(spans("$$x"), Vec::<&str>::new());
        // A `$$` pair doesn't swallow a later single-dollar span.
        assert_eq!(spans("$$a$$ then $b$"), vec!["$$a$$", "$b$"]);
    }

    #[test]
    fn detects_a_gfm_table() {
        let md = "intro\n\n| A | B |\n| --- | :-: |\n| 1 | 2 |\n| 3 | 4 |\n\nafter";
        let regions = table_regions(md);
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].lines, 2..6); // header, separator, 2 body rows
    }

    #[test]
    fn cells_split_and_trim() {
        assert_eq!(table_cells("| a | b |"), vec!["a", "b"]);
        assert_eq!(table_cells("|  |  |"), vec!["", ""]);
    }

    #[test]
    fn escaped_pipes_are_cell_content() {
        // GFM: `\|` is a literal pipe in the cell, not a separator — the
        // reader (GFM parser) agrees; a blind split miscounted cells and
        // column rewrites then corrupted the row.
        assert_eq!(
            table_cells(r"| left \| right | ok |"),
            vec![r"left \| right", "ok"]
        );
        // A double backslash escapes the backslash — that pipe IS a separator.
        assert_eq!(table_cells(r"| a \\| b |"), vec![r"a \\", "b"]);
    }

    #[test]
    fn separator_required() {
        // Pipes in prose without a separator row are not a table.
        assert!(table_regions("a | b\nc | d").is_empty());
        assert!(table_regions("| not a table\njust text").is_empty());
        // A right-aligned separator (`---:`) still starts a region.
        assert_eq!(table_regions("| h |\n| ---: |\n| x |")[0].lines, 0..3);
    }

    #[test]
    fn a_marker_comment_above_does_not_enter_the_region() {
        // The style-marker comment line above stays out of the region's `lines`.
        let r = table_regions("<!-- table:header -->\n| h |\n| --- |\n| x |");
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].lines, 1..4);

        // No marker line → the region starts at the header row.
        let g = table_regions("| h |\n| --- |\n| x |");
        assert_eq!(g[0].lines, 0..3);
    }

    #[test]
    fn adjacent_tables_split_into_two_regions() {
        // Two tables jammed together with no blank line between must not merge: the
        // second table's header + `|---|` start a new region (otherwise the second
        // separator rendered as `---` cells in one merged grid).
        let r = table_regions(
            "| a | b |\n| --- | --- |\n| 1 | 2 |\n| c | d |\n| --- | --- |\n| 3 | 4 |",
        );
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].lines, 0..3);
        assert_eq!(r[1].lines, 3..6);
    }

    #[test]
    fn heading_and_image_line() {
        assert_eq!(heading_level("## Hi"), Some(2));
        assert_eq!(heading_level("#notaheading"), None);
        assert_eq!(image_line("![a](b.png)"), Some(("b.png", None)));
        assert_eq!(
            image_line("![a](b.png){width=320}"),
            Some(("b.png", Some(320.0)))
        );
        assert_eq!(image_line("text ![a](b.png)"), None);
        // A list item whose sole body is an image renders too (its bullet stays).
        assert_eq!(
            image_row("- ![a](b.png){width=452}"),
            Some(("b.png", Some(452.0), 2))
        );
        assert_eq!(image_row("1. ![a](b.png)"), Some(("b.png", None, 3)));
        assert_eq!(image_row("![a](b.png)"), Some(("b.png", None, 0)));
        assert_eq!(image_row("- text ![a](b.png)"), None);
    }

    #[test]
    fn thematic_break_detection() {
        for s in ["---", "***", "___", "- - -", "  ---  ", "----"] {
            assert!(thematic_break(s), "{s:?} should be a rule");
        }
        for s in ["--", "**", "", "text", "---x", "- -- text", "===", "> ---"] {
            assert!(!thematic_break(s), "{s:?} should not be a rule");
        }
    }

    #[test]
    fn alert_kinds_detected() {
        // 通过 recognition::alert_prefix 取 kind（构造层单一源）。
        let kind = |s: &str| alert_prefix(s).map(|(k, ..)| k);
        assert_eq!(kind("[!NOTE]"), Some(AlertKind::Note));
        assert_eq!(kind(" [!CAUTION] "), Some(AlertKind::Caution));
        // Lenient form: body text on the marker line.
        assert_eq!(kind("[!NOTE] text after"), Some(AlertKind::Note));
        for s in ["[!note]", "[!NOTEXT]", "note", ""] {
            assert_eq!(kind(s), None, "{s:?}");
        }
    }

    #[test]
    fn toggle_task_checkbox_flips() {
        assert_eq!(
            toggle_task_checkbox("- [ ] todo").as_deref(),
            Some("- [x] todo")
        );
        assert_eq!(
            toggle_task_checkbox("- [x] done").as_deref(),
            Some("- [ ] done")
        );
        assert_eq!(
            toggle_task_checkbox("- [X] done").as_deref(),
            Some("- [ ] done")
        );
        assert_eq!(
            toggle_task_checkbox("  - [ ] nested").as_deref(),
            Some("  - [x] nested")
        );
        assert_eq!(toggle_task_checkbox("- plain"), None);
    }

    #[test]
    fn alert_fold_regions_span_the_quote_block() {
        let src = "> [!NOTE]- hidden\n> body\n> more\nprose\n> [!TIP]+ open\n> b\n> [!NOTE] plain";
        let r = alert_fold_regions(src);
        // Folded NOTE spans its quote lines; open TIP's region ends at the
        // plain (non-foldable) alert marker, which starts no region itself.
        assert_eq!(r, vec![(0..3, true), (4..6, false)]);
    }

    #[test]
    fn alert_region_kinds_cover_marker_and_body() {
        let src = "> [!NOTE]- hidden\n> body一\n> body二\nprose\n> [!TIP]+ open\n> b\n> [!NOTE] plain\n> p2";
        // 每个 callout region（marker + 连续 `>` body 行）的每行都记上对应 AlertKind；
        // 有/无折叠符的警报都覆盖。普通引述/非引述行不在此表。
        let m = alert_region_kinds(src);
        assert_eq!(m.get(&0), Some(&AlertKind::Note));
        assert_eq!(m.get(&1), Some(&AlertKind::Note), "body 行 1 也应变色");
        assert_eq!(m.get(&2), Some(&AlertKind::Note), "body 行 2 也应变色");
        assert_eq!(m.get(&4), Some(&AlertKind::Tip));
        assert_eq!(m.get(&5), Some(&AlertKind::Tip), "TIP body 也变色");
        assert_eq!(m.get(&6), Some(&AlertKind::Note), "无折叠符的警报也覆盖");
        assert_eq!(m.get(&7), Some(&AlertKind::Note), "无折叠符警报的 body 也变色");
        assert!(!m.contains_key(&3), "普通 prose 行不应变色");
    }

    #[test]
    fn heading_fold_regions_span_their_section() {
        let src = "# Top\nbody\n## Sub\nsub body\n```\n# not a heading\n```\n## Next\nx";
        let folded: std::collections::HashSet<String> = ["## Sub".to_string()].into();
        // `## Sub` folds through the fence (the `#` inside is code) up to
        // `## Next`; nothing else folds.
        assert_eq!(heading_fold_regions(src, &folded), vec![2..7]);
        // Folding `# Top` swallows everything (no later h1).
        let folded: std::collections::HashSet<String> = ["# Top".to_string()].into();
        assert_eq!(heading_fold_regions(src, &folded), vec![0..9]);
        assert!(heading_fold_regions(src, &Default::default()).is_empty());
        // A LIST heading folds its indented children (blanks included) and
        // stops at the next sibling — not at unrelated later headings.
        let src = "* ### Notes: ^abc\n  - one\n\n  - two\n* sibling\n# After";
        let folded: std::collections::HashSet<String> = ["* ### Notes: ^abc".to_string()].into();
        assert_eq!(heading_fold_regions(src, &folded), vec![0..4]);
    }
}
