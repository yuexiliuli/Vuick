//! WYSIWYG「数学 + Mermaid」整形族：行内 `$…$` 的 splice（spacer 占位 + reveal-on-caret）、
//! LaTeX→Unicode 展平、Mermaid 图解析与层式布局。与 [`super::element`] 的其余整形
//! 逻辑分离：这里只管「数学/图」的显示化，文本/代码/表格块整形在 `shape`，帧装配在
//! `frame`；绘制在 [`super::paint`]、命中在 [`super::hit`]。

use std::ops::Range;
use std::sync::Arc;

use gpui::{Hsla, Pixels, RenderImage, SharedString, TextRun, Window, px};

use super::{WyEditor, WyTheme};
use crate::markdown::parse::inline_style;

/// mermaid 布局结果：摆放好的节点 + 边（节点索引对 + 可选标签）。`mm_layout` 的返回；
/// `ShapedRow.mm` 携带。原在 `element.rs`，随 math+mermaid 族迁入。
pub(crate) type MmLayout = (Vec<MmPlaced>, Vec<(usize, usize, Option<String>)>);

/// 行内 `$…$` 数学：splice 后的 spacer 位置 + 要绘制的 LaTeX 文本。
/// （不 derive Debug：`RenderImage` 未实现 `Debug`。）
#[derive(Clone)]
pub(crate) struct InlineMathText {
    /// spacer 起始渲染字节。
    pub display_off: usize,
    /// spacer 字节数（= 空格数；双击命中盒宽度用它）。
    pub len: usize,
    /// 该公式在**源文档中的字节区间**（含定界符 `$…$`）——双击编辑时把光标塞进它、
    /// 并在 shape 期 reveal（reveal-on-caret 只对源区间内的共享，放智能回源）。
    pub source: Range<usize>,
    /// 去定界符后的 LaTeX（paint 以斜体排版绘制）。
    pub latex: SharedString,
    /// **排版槽键**（`inline:` + 去定界符 LaTeX）——`sync_math`（可见带懒光栅）按此
    /// claim 占槽/光栅；`latex` 是 Unicode 展示态、不可当键用。
    pub key: SharedString,
    /// 已就绪的排版位图 + 逻辑（typeset em）px 尺寸；`Some` 时 paint 画 `RenderImage`
    /// 覆盖 spacer（参考 zorite `shape_inline_math`），`None` = 排版中/失败 → 画斜体 Unicode。
    /// `w`/`h` 是 22px 排版下的原始尺寸，画图按 `inline_math_fit_h` 再缩放。
    pub ready: Option<(Arc<RenderImage>, f32, f32)>,
    /// 所在 `row.wrapped` 的 BlockLine 索引（`shape_text_row` 撑高后填，paint 定位用）。
    pub line: usize,
}


/// 行内 `$…$` 数学：把 display 文本里的公式 splice 成空格 spacer（宽度≈排版后的
/// latex 宽，`splice_inline_math` 把 spacer 映射回公式源位），paint 期再在 spacer 处
/// 画排版位图（已就绪）/ 斜体 latex（排版中或失败）。返回重写后的
/// `(display, gpui runs, byte_map)` + 每处公式位置。
///
/// `caret_disp` = 本行内光标的**渲染列**（`Some` = 光标在该行；`None` = 他行 / 无）。
/// reveal-on-caret：光标落进 `$…$` 的源区间内（含定界符 `$`）→ 该公式不 splice，
/// 显示原始 `$…$` 文本（可编辑）——用户双击公式把光标放进去即可直接改 LaTeX。
#[allow(clippy::too_many_arguments)] // splice 拆分器：输入互为正交（拆分时另行评估上下文分组）
pub(crate) fn splice_math_text(
    editor: &WyEditor,
    window: &mut Window,
    src_line: usize,
    caret_disp: Option<usize>,
    rtext: &str,
    runs: Vec<TextRun>,
    map: Vec<usize>,
    fs: Pixels,
    lh: Pixels,
    base_font: &gpui::Font,
    math_font: &gpui::Font,
    math_color: Hsla,
    code_ranges: &[Range<usize>],
) -> (String, Vec<TextRun>, Vec<usize>, Vec<InlineMathText>) {
    let spans = inline_style::inline_math_spans(rtext);
    if spans.is_empty() {
        return (rtext.to_string(), runs, map, Vec::new());
    }
    let line_start = editor.line_start(src_line);
    // 跳过完全落在行内代码里的 `$…$`（反引号内是字面量，不是数学）。
    let in_code = |r: &Range<usize>| code_ranges.iter().any(|c| c.start < r.end && r.start < c.end);
    let space_w = {
        let run = [TextRun { len: 1, font: base_font.clone(), color: math_color, ..Default::default() }];
        f32::from(window.text_system().shape_line(" ".into(), fs, &run, None).width()).max(1.)
    };
    let slots = editor.math_slots();
    let mut formulas: Vec<(Range<usize>, usize)> = Vec::new();
    let mut maths: Vec<InlineMathText> = Vec::new();
    for span in spans {
        if in_code(&span) {
            continue;
        }
        // reveal-on-caret：光标严格落在 `$…$` 内（不碰定界符不算数，与块公式一致的
        // “单击停在边上、双击进去编辑”语义）→ 不 splice，显示可编辑的原始源码。
        if let Some(c) = caret_disp
            && c > span.start && c < span.end {
                continue;
            }
        let latex = inline_style::inline_math_latex(rtext, &span);
        if latex.trim().is_empty() {
            continue;
        }
        // 排版已就绪：spacer 宽 = 位图逻辑宽（按正文行字号缩放；store 按 FONT_SIZE 排）。
        // 排版中/失败：回退 Unicode 排版宽（paint 照旧画斜体 Unicode 文本）。
        let ready: Option<(Arc<RenderImage>, f32, f32)> = match slots
            .borrow()
            .get(super::inline_math_key(latex.trim()).as_ref())
            .cloned()
        {
            Some(super::MathSlot::Ready(img, w, h)) => Some((img, w, h)),
            _ => None,
        };
        let shown = math_to_unicode(latex);
        let shown = shown.replace('\n', " ");
        let (_w, n, paint_ready) = match ready {
            Some((img, w, h)) => {
                // spacer 宽 = 显示宽（`inline_math_fit_h` 缩放，供 paint 同式画图）。
                let fit = inline_math_fit_h(fs, lh, h);
                let scale = if h > 0.0 {
                    fit / h
                } else {
                    f32::from(fs) / crate::config::math::MATH_FONT_SIZE
                };
                let lw = (w * scale).max(1.);
                (lw.max(1.), (lw / space_w).ceil().max(1.0) as usize, Some((img, w, h)))
            }
            None => {
                if shown.is_empty() {
                    continue;
                }
                let run = [TextRun { len: shown.len(), font: math_font.clone(), color: math_color, ..Default::default() }];
                let w = f32::from(window.text_system().shape_line(shown.clone().into(), fs, &run, None).width());
                let n = (w / space_w).ceil() as usize;
                if n == 0 {
                    continue;
                }
                (w, n.max(1), None)
            }
        };
        if n > 0 {
            let n = n.max(1);
            formulas.push((span.clone(), n));
            // 源区间（绝对）：span 渲染列 → 经 byte_map 反查源列 → 加行首。
            let src_lo = line_start + super::element::display_to_source(&map, span.start);
            let src_hi = line_start
                + super::element::display_to_source(&map, span.end.saturating_sub(1).max(span.start));
            maths.push(InlineMathText {
                display_off: 0,
                len: n,
                source: src_lo..src_hi + 1,
                latex: shown.into(),
                key: super::inline_math_key(latex.trim()),
                ready: paint_ready,
                line: 0,
            });
        }
    }
    if formulas.is_empty() {
        return (rtext.to_string(), runs, map, Vec::new());
    }
    let gap = TextRun { len: 0, font: base_font.clone(), color: math_color, ..Default::default() };
    let (nd, nr, nm, places) =
        inline_style::splice_inline_math(rtext, &runs, &map, &formulas, &gap);
    let maths = places
        .into_iter()
        .zip(maths)
        .map(|(pl, mut m)| {
            m.display_off = pl.display_off;
            m
        })
        .collect();
    (nd, nr, nm, maths)
}

/// 行内公式的显示高（px）：原生按正文行字号缩放（`fs/MATH_FONT_SIZE × 图高`），但 ≤
/// `INLINE_MATH_BAND × 行高`（只压超高算子公式，其余保持字号缩放、不撑行）。shape
/// （spacer/行高）与 paint（画图）都用它——宽/高永远一致。参数单一源
/// = `crate::config::math`。
pub(crate) fn inline_math_fit_h(fs: Pixels, lh: Pixels, h: f32) -> f32 {
    let base = f32::from(fs) / crate::config::math::MATH_FONT_SIZE;
    let cap = f32::from(lh) * crate::config::math::INLINE_MATH_BAND;
    (base * h).clamp(1.0, cap)
}

/// display_off 落在 `wrapped` 里的第几个 BlockLine。文本块一源码行 = 一个
/// BlockLine（行内数学/折行都在其内，spacer 属于首个）→ 恒 0。
pub(crate) fn visual_block_line_for_index(wrapped: &[gpui::WrappedLine], _off: usize) -> usize {
    let _ = wrapped;
    0
}


const LATEX_SYMBOLS: &[(&str, char)] = &[
    ("pi", 'π'), ("infty", '∞'), ("int", '∫'), ("iint", '∬'), ("oint", '∮'),
    ("sum", '∑'), ("prod", '∏'), ("cdot", '·'), ("times", '×'), ("div", '÷'),
    ("pm", '±'), ("mp", '∓'), ("neq", '≠'), ("leq", '≤'), ("geq", '≥'),
    ("le", '≤'), ("ge", '≥'), ("ne", '≠'), ("approx", '≈'), ("equiv", '≡'),
    ("sim", '∼'), ("partial", '∂'), ("nabla", '∇'),
    ("alpha", 'α'), ("beta", 'β'), ("gamma", 'γ'), ("delta", 'δ'),
    ("epsilon", 'ε'), ("zeta", 'ζ'), ("eta", 'η'), ("theta", 'θ'),
    ("lambda", 'λ'), ("mu", 'μ'), ("nu", 'ν'), ("xi", 'ξ'),
    ("rho", 'ρ'), ("sigma", 'σ'), ("tau", 'τ'), ("phi", 'φ'), ("chi", 'χ'),
    ("psi", 'ψ'), ("omega", 'ω'), ("Gamma", 'Γ'), ("Delta", 'Δ'),
    ("Theta", 'Θ'), ("Lambda", 'Λ'), ("Sigma", 'Σ'), ("Phi", 'Φ'),
    ("Omega", 'Ω'), ("rightarrow", '→'), ("leftarrow", '←'),
    ("leftrightarrow", '↔'), ("to", '→'), ("mapsto", '↦'),
    ("Rightarrow", '⇒'), ("Leftrightarrow", '⇔'),
    ("ldots", '…'), ("cdots", '⋯'), ("dots", '…'), ("in", '∈'), ("notin", '∉'),
    ("subset", '⊂'), ("subseteq", '⊆'), ("cup", '∪'), ("cap", '∩'),
    ("forall", '∀'), ("exists", '∃'), ("nexists", '∄'), ("propto", '∝'),
    ("perp", '⊥'), ("parallel", '∥'), ("angle", '∠'), ("circ", '∘'),
    ("degree", '°'), ("deg", '°'), ("oplus", '⊕'), ("otimes", '⊗'),
    ("emptyset", '∅'), ("ell", 'ℓ'), ("hbar", 'ℏ'), ("bullet", '•'),
    ("uparrow", '↑'), ("downarrow", '↓'), ("ast", '∗'), ("star", '⋆'),
    ("dag", '†'), ("ddag", '‡'),
];

/// 把一个字符上标化（Unicode 上标），不可映射返回 `None`。
fn sup_map(c: char) -> Option<char> {
    Some(match c {
        '0'..='9' => ['⁰', '¹', '²', '³', '⁴', '⁵', '⁶', '⁷', '⁸', '⁹'][c as usize - '0' as usize],
        '+' => '⁺',
        '-' => '⁻',
        '=' => '⁼',
        '(' => '⁽',
        ')' => '⁾',
        'n' => 'ⁿ',
        'i' => 'ⁱ',
        _ => return None,
    })
}

/// 把一个字符下标化（Unicode 下标），不可映射返回 `None`。
fn sub_map(c: char) -> Option<char> {
    Some(match c {
        '0'..='9' => ['₀', '₁', '₂', '₃', '₄', '₅', '₆', '₇', '₈', '₉'][c as usize - '0' as usize],
        '+' => '₊',
        '-' => '₋',
        '=' => '₌',
        'i' => 'ᵢ',
        'n' => 'ₙ',
        'x' => 'ₓ',
        'j' => 'ⱼ',
        'k' => 'ₖ',
        'm' => 'ₘ',
        _ => return None,
    })
}

/// 从 `cs[pos..]` 读一个「组」：`{…}`（含嵌套）或单个字符。返回 (内容, 新位置)。
fn read_group(cs: &[char], pos: usize) -> (String, usize) {
    if pos < cs.len() && cs[pos] == '{' {
        let mut depth = 0usize;
        let mut s = String::new();
        let mut k = pos;
        while k < cs.len() {
            match cs[k] {
                '{' => {
                    depth += 1;
                    if depth > 1 {
                        s.push('{');
                    }
                }
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return (s, k + 1);
                    }
                    s.push('}');
                }
                c => s.push(c),
            }
            k += 1;
        }
        return (s, k);
    }
    if pos < cs.len() {
        (cs[pos].to_string(), pos + 1)
    } else {
        (String::new(), pos)
    }
}

/// 只在以 `{` 开头时读组；否则 `None`（位置不动）。
fn read_group_opt(cs: &[char], pos: usize) -> (Option<String>, usize) {
    if pos < cs.len() && cs[pos] == '{' {
        let (s, ni) = read_group(cs, pos);
        (Some(s), ni)
    } else {
        (None, pos)
    }
}

/// 子/上标在**卡片**里的视觉分档：`Sup` = 上标（小一号 + 上移）、`Sub` = 下标（小一号 + 下移）。
/// 字符串版 `math_to_unicode` 用 Unicode 上/下标码位（`ᵢ₌₁ⁿ` 这类），但 `∞` 等没有码位
/// 只能正文大小平排 → `∫₋∞∞` 的极限看起来不对。卡片改用 `math_to_segments` + 字号/偏移。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum MathPos {
    Normal,
    Sub,
    Sup,
}

#[derive(Clone, Debug)]
pub(crate) struct MathSeg {
    pub text: String,
    pub pos: MathPos,
}

/// 同 `math_to_unicode` 的扫描，但把顶层 `^{…}`/`_{…}` 拆成独立段（Sup/Sub）。
/// 普通文本兜底进 Normal 段，`\frac/\sqrt/\text` 等命令整体仍按 math_to_unicode 转进
/// Normal 段（组内嵌套的 `^`/`_` 在递归里转成 Unicode 码位即可，不再细分）。
pub(crate) fn math_to_segments(src: &str) -> Vec<MathSeg> {
    let cs: Vec<char> = src.chars().collect();
    let mut segs: Vec<MathSeg> = Vec::new();
    let mut buf = String::with_capacity(src.len());
    let mut i = 0;
    let flush = |segs: &mut Vec<MathSeg>, buf: &mut String| {
        if !buf.is_empty() {
            segs.push(MathSeg { text: std::mem::take(buf), pos: MathPos::Normal });
        }
    };
    while i < cs.len() {
        match cs[i] {
            '{' | '}' => i += 1,
            '^' => {
                flush(&mut segs, &mut buf);
                let (g, ni) = read_group(&cs, i + 1);
                let g = math_to_unicode(&g);
                let mut t = String::new();
                for c in g.chars() {
                    t.push(sup_map(c).unwrap_or(c));
                }
                segs.push(MathSeg { text: t, pos: MathPos::Sup });
                i = ni;
            }
            '_' => {
                flush(&mut segs, &mut buf);
                let (g, ni) = read_group(&cs, i + 1);
                let g = math_to_unicode(&g);
                let mut t = String::new();
                for c in g.chars() {
                    t.push(sub_map(c).unwrap_or(c));
                }
                segs.push(MathSeg { text: t, pos: MathPos::Sub });
                i = ni;
            }
            '\\' => {
                if cs.get(i + 1) == Some(&'\\') {
                    buf.push('\n');
                    i += 2;
                    continue;
                }
                let mut j = i + 1;
                while j < cs.len() && cs[j].is_ascii_alphabetic() {
                    j += 1;
                }
                let name: String = cs[i + 1..j].iter().collect();
                match name.as_str() {
                    "begin" | "end" => {
                        if let (Some(_), ni) = read_group_opt(&cs, j) {
                            i = ni;
                        } else {
                            i = j;
                        }
                    }
                    "cr" | "hfill" => {
                        buf.push('\n');
                        i = j;
                    }
                    "frac" | "tfrac" | "dfrac" => {
                        let (a, ni) = read_group_opt(&cs, j);
                        let (b, nj) = match &a {
                            Some(_) => read_group_opt(&cs, ni),
                            None => (None, j),
                        };
                        if let Some(a) = &a {
                            buf.push_str(&math_to_unicode(a));
                        }
                        buf.push('/');
                        if let Some(b) = &b {
                            buf.push_str(&math_to_unicode(b));
                        }
                        i = nj;
                    }
                    "sqrt" => {
                        buf.push('√');
                        let (g, ni) = read_group_opt(&cs, j);
                        if let Some(g) = g {
                            if g.chars().count() > 1 {
                                buf.push('(');
                                buf.push_str(&math_to_unicode(&g));
                                buf.push(')');
                            } else {
                                buf.push_str(&math_to_unicode(&g));
                            }
                        }
                        i = ni;
                    }
                    "text" | "mathrm" | "mathit" | "operatorname" | "mbox" => {
                        let (g, ni) = read_group_opt(&cs, j);
                        if let Some(g) = g {
                            buf.push_str(&math_to_unicode(&g));
                        }
                        i = ni;
                    }
                    "left" | "right" | "big" | "Big" | "Bigl" | "Bigr" | "bigl" | "bigr"
                    | "small" | "limits" | "nolimits" => i = j,
                    _ => {
                        if let Some(sym) = LATEX_SYMBOLS.iter().find(|(n, _)| *n == name) {
                            buf.push(sym.1);
                        }
                        i = j;
                    }
                }
            }
            c => {
                if c == '&' {
                    buf.push(' ');
                } else {
                    buf.push(c);
                }
                i += 1;
            }
        }
    }
    flush(&mut segs, &mut buf);
    segs
}

/// 裸露 LaTeX → Unicode 排版（无光栅渲染器时的「公式感」显示）。
/// 处理上下标（`^`/`_`，含 `{…}` 组）、`\frac`/`\sqrt`/`\text` 等、希腊字母与常用符号。
/// 行内数学继续用这个扁平字符串版；**卡片**走 `math_to_segments`（细分段、可缩小+偏移）。
pub(crate) fn math_to_unicode(src: &str) -> String {
    math_to_segments(src)
        .into_iter()
        .map(|s| s.text)
        .collect()
}

/// mermaid 节点形状。
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum MmShape {
    /// 普通矩形。
    Rect,
    /// `{ … }` 判断/菱形。
    Diamond,
    /// `[ … ]` 圆角矩形（stadium）。
    Round,
    /// `(( … ))` 圆形。
    Circle,
}

/// 解析 `A[...]` / `A{...}` / `A((...))` / `A(...)` / 裸 `A` → (id, 可选标签, 形状)。
fn mm_node(s: &str) -> (String, Option<String>, MmShape) {
    let s = s.trim();
    if let Some(oi) = s.find("((") {
        let id = s[..oi].trim().to_string();
        let rest = &s[oi + 2..];
        if let Some(ci) = rest.find("))") {
            return (id, Some(rest[..ci].trim().to_string()), MmShape::Circle);
        }
        return (id, None, MmShape::Circle);
    }
    for (open, close, shp) in [
        ('[', ']', MmShape::Round),
        ('{', '}', MmShape::Diamond),
        ('(', ')', MmShape::Round),
    ] {
        if let Some(oi) = s.find(open) {
            let id = s[..oi].trim().to_string();
            let rest = &s[oi + 1..];
            if let Some(ci) = rest.find(close) {
                return (id, Some(rest[..ci].trim().to_string()), shp);
            }
            return (id, None, shp);
        }
    }
    (s.to_string(), None, MmShape::Rect)
}

/// 解析一条边 `A -->|标| B` / `A --> B` / `A --- B` → (起点文本, 终点文本, 边标)。
fn mm_edge(s: &str) -> Option<(String, String, Option<String>)> {
    let s = s.trim();
    let marker = if s.contains("-->") {
        "-->"
    } else if s.contains("---") {
        "---"
    } else {
        return None;
    };
    let idx = s.find(marker)?;
    let lhs = s[..idx].trim().to_string();
    let mut rhs = s[idx + marker.len()..].trim().to_string();
    let mut elab = None;
    if let Some(rest) = rhs.strip_prefix('|') {
        if let Some(end) = rest.find('|') {
            elab = Some(rest[..end].trim().to_string());
            rhs = rest[end + 1..].trim().to_string();
        } else {
            rhs = rest.to_string();
        }
    }
    if lhs.is_empty() || rhs.is_empty() {
        return None;
    }
    Some((lhs, rhs, elab))
}

/// 解析后的 mermaid 图（节点 + 边），供层式布局与矢量绘制用。
#[derive(Clone, Debug)]
pub(crate) struct MmGraph {
    /// (id, 显示标签, 形状)。
    pub(crate) nodes: Vec<(String, String, MmShape)>,
    /// (起点 id, 终点 id, 边标)。
    pub(crate) edges: Vec<(String, String, Option<String>)>,
}

/// 解析 `graph TD/LR` 源码 → 节点与边（缺省标签回退到 id）。
pub(crate) fn mermaid_graph(source: &str) -> MmGraph {
    let mut nodes: Vec<(String, String, MmShape)> = Vec::new();
    let mut edges: Vec<(String, String, Option<String>)> = Vec::new();
    let mut add = |id: &str, lab: Option<String>, shp: MmShape| {
        if let Some(n) = nodes.iter_mut().find(|(nid, _, _)| nid == id) {
            // 已有定义：只有本次给出**显式标签**才更新，裸 id 引用（`B -->` 里的 `B`）
            // 不得用回退的「id 本身」覆盖之前的 `B{判断}` → 判断。
            if let Some(l) = lab
                && !l.is_empty() {
                    n.1 = l;
                }
            return;
        }
        let lab = lab.filter(|l| !l.is_empty()).unwrap_or_else(|| id.to_string());
        nodes.push((id.to_string(), lab, shp));
    };
    for raw in source.lines() {
        let mut t = raw.trim();
        if let Some(s) = t.strip_suffix(';') {
            t = s.trim();
        }
        if t.is_empty()
            || t.starts_with("graph ")
            || t.starts_with("flowchart ")
            || t.starts_with("sequenceDiagram")
        {
            continue;
        }
        if let Some((a, b, elab)) = mm_edge(t) {
            let (aid, alab, ashp) = mm_node(&a);
            let (bid, blab, bshp) = mm_node(&b);
            add(&aid, alab, ashp);
            add(&bid, blab, bshp);
            edges.push((aid, bid, elab));
            continue;
        }
        let (id, lab, shp) = mm_node(t);
        add(&id, lab, shp);
    }
    MmGraph { nodes, edges }
}

/// 最长路径分层：`layer[b] ≥ layer[a] + 1`，收敛后返回各节点层号。
fn mermaid_layer_map(g: &MmGraph) -> std::collections::HashMap<String, usize> {
    use std::collections::HashMap;
    let mut layer: HashMap<String, usize> = HashMap::new();
    for (id, _, _) in &g.nodes {
        layer.insert(id.clone(), 0);
    }
    // 边界情况：节点多则多轮，收敛即止。
    for _ in 0..g.nodes.len().max(1) {
        let mut changed = false;
        for (a, b, _) in &g.edges {
            let la = layer.get(a).copied().unwrap_or(0);
            if layer.get(b).copied().unwrap_or(0) < la + 1 {
                layer.insert(b.clone(), la + 1);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    layer
}

/// 卡片布局常量（paint 与卡片高度共用，不得分开改）。
const MM_TOP_PAD: f32 = 30.;
const MM_LANE: f32 = 52.;
const MM_NODE_H: f32 = 30.;
const MM_NODE_XPAD: f32 = 14.;
const MM_SEP: f32 = 20.;
const MM_EDGE_X: f32 = 14.;

/// Mermaid 图卡片高度：由层数决定（无窗口也能算）。
pub(crate) fn mermaid_card_height(source: &str) -> Pixels {
    let g = mermaid_graph(source);
    if g.nodes.is_empty() {
        return px(140.);
    }
    let layers = mermaid_layer_map(&g);
    let n = layers.values().copied().max().unwrap_or(0) + 1;
    px(MM_TOP_PAD + n as f32 * MM_LANE + 8.)
}

/// 已摆放的节点（layout 算好几何，paint 与测试共用）。
pub(crate) struct MmPlaced {
    pub id: String,
    pub label: String,
    pub shape: MmShape,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// mermaid `graph` 的层式布局（与绘制解耦，便于单测/光栅化验证）：
/// 返回摆放好的节点与「节点索引对」边。`area` 是卡片内容区（左上 + 可用宽）。
pub(crate) fn mm_layout(
    window: &Window,
    theme: &WyTheme,
    source: &str,
    area_x: f32,
    area_y: f32,
    area_w: f32,
) -> MmLayout {
    let g = mermaid_graph(source);
    if g.nodes.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let layer = mermaid_layer_map(&g);
    let n_layers = layer.values().copied().max().unwrap_or(0) + 1;
    let mut rows: Vec<Vec<usize>> = vec![Vec::new(); n_layers];
    for (idx, (id, _, _)) in g.nodes.iter().enumerate() {
        let l = layer.get(id).copied().unwrap_or(0).min(n_layers - 1);
        rows[l].push(idx);
    }
    let fs = theme.font_size;
    let font = theme.font.clone();
    let avail = (area_w - 2. * MM_EDGE_X).max(1.);
    let xc = area_x + MM_EDGE_X;
    let mut placed: Vec<MmPlaced> = Vec::new();
    for (li, row) in rows.iter().enumerate() {
        let mut widths: Vec<f32> = Vec::with_capacity(row.len());
        for &i in row {
            let (_, label, _) = &g.nodes[i];
            let run = [TextRun { len: label.len(), font: font.clone(), color: theme.text, ..Default::default() }];
            let w = f32::from(window.text_system().shape_line(label.clone().into(), fs, &run, None).width());
            widths.push(w + 2. * MM_NODE_XPAD);
        }
        let row_total = widths.iter().sum::<f32>() + MM_SEP * (row.len() as f32 - 1.);
        let mut x = xc + ((avail - row_total) / 2.).max(0.);
        for (j, &i) in row.iter().enumerate() {
            let (id, label, shape) = &g.nodes[i];
            placed.push(MmPlaced {
                id: id.clone(),
                label: label.clone(),
                shape: *shape,
                x,
                y: area_y + MM_TOP_PAD + li as f32 * MM_LANE,
                w: widths[j],
                h: MM_NODE_H,
            });
            x += widths[j] + MM_SEP;
        }
    }
    let pos = |id: &str| placed.iter().position(|p| p.id == *id);
    let mut edges: Vec<(usize, usize, Option<String>)> = Vec::new();
    for (a, b, elab) in &g.edges {
        if let (Some(ia), Some(ib)) = (pos(a), pos(b)) {
            edges.push((ia, ib, elab.clone()));
        }
    }
    (placed, edges)
}

