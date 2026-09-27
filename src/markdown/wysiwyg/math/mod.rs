//! WYSIWYG「数学」整形族：行内 `$…$` 的 splice（spacer 占位 + reveal-on-caret）、
//! LaTeX→Unicode 展平、`LaTeX` 显示块（卡片）。与 [`super::element`] 的其余整形逻辑
//! 分离：这里只管「数学」的显示化，文本/代码/表格块整形在 `shape`，帧装配在 `frame`；
//! 绘制在 [`super::paint`]、命中在 [`super::hit`]。
//!
//! **Mermaid 图已随「专项支持」迁至 [`super::mermaid`] 独立文件夹**（parse/layout/paint
//! 三分离），此处只保留数学。

use std::ops::Range;
use std::sync::Arc;

use gpui::{Hsla, Pixels, RenderImage, SharedString, TextRun, Window};

use super::WyEditor;
use crate::markdown::parse::inline_style;

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

