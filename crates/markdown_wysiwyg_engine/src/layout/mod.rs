//! 共享 run 构造/装饰 helper：供 WYSIWYG 行整形层 [`crate::shape`] 消费，
//! 为**单一几何真值 `shape_docrow`** 提供统一的 `DocRow.runs → gpui TextRun`
//! 构造与行内代码背景区间扩展。
//!
//! 本模块**不承载任何整形/分片几何**：历史上的分片整形并行面（`shape_docrow_shard`
//! 及 `shape_*_shards` 系，原第二消费方预览引擎已删 b4e614d）已真删，
//! 几何改动一律落在 `shape.rs` 一侧，勿在此重建平行整形路径。
//!
//! 不引入新依赖；依赖方向 `layout → (markdown::parse / base::util)`。

use std::ops::Range;

use gpui::{Hsla, TextRun};

use markdown::parse::RunKind;

/// 把背景区间向左右扩展吞掉紧邻空白：留给圆角背景的左右视觉间距。
/// （高亮区间即"代码文本 + 两侧各一个空格"，等宽 overrides 只覆盖代码本体，
/// 空格保持正文字宽 → 圆角灰底略宽于字形，左右有呼吸空间。）
/// 供 `shape.rs` 的行内代码 pill 背景区间使用（M5-B5 P4-2 由
/// `markdown::view::row` 迁入本层——旧预览引擎已删）。
pub fn expand_bg_over_whitespace(text: &str, range: &Range<usize>) -> Range<usize> {
    let mut start = range.start;
    let mut end = range.end;
    if start > 0 {
        let prev = text[..start].chars().next_back();
        if let Some(c) = prev
            && c.is_whitespace()
        {
            start -= c.len_utf8();
        }
    }
    if end < text.len() {
        let next = text[end..].chars().next();
        if let Some(c) = next
            && c.is_whitespace()
        {
            end += c.len_utf8();
        }
    }
    start..end
}

/// 单一 run 构造核心（`crate::shape::runs_to_text_runs` 的公共基，且是它唯一
/// 生产消费者）：
/// - 逐 run 守卫（字符边界吸收 / 空区间丢弃 / 未覆盖区间补默认 / 尾段补齐）与
///   `crate::shape::runs_to_text_runs` 逐行一致（`base::util::char_align_byte` 吸收
///   中切字符的区间）；
/// - `base`：run 之外的默认字体（当前 run 字体由 `deco` 派生）；未覆盖区间（两个 run
///   之间的空隙 / 尾段）一律 `base`、无装饰。
/// - `base_color`：间隙/尾段默认 run 的文字色。**调用方必须传正文实色**——gpui 对
///   `color: None` 的 run 以黑色绘制（`text_system::line.rs` 累积器初值 `black()`），
///   否则暗色主题下间隙文字不可见（回归：重构早期漏设致 WYSIWYG 文字消失）。
/// - `deco`：`RunKind → TextRunDeco`（字体 + 可选色/划线/下划线）。几何只由字形维度
///   决定（色影响绘制不影响宽度/折行）。
#[allow(clippy::too_many_arguments)] // run 构造公共基：字体 + 基色 + 装饰三路正交
pub fn text_runs(
    text: &str,
    runs: &[(Range<usize>, RunKind)],
    base: &gpui::Font,
    base_color: gpui::Hsla,
    deco: impl Fn(RunKind) -> TextRunDeco,
) -> Vec<TextRun> {
    let mut out = Vec::with_capacity(runs.len().saturating_add(1).max(1));
    let mut i = 0usize;
    for (range, kind) in runs {
        let s = base::util::char_align_byte(text, range.start).max(i);
        let e = base::util::char_align_byte(text, range.end);
        if e <= s {
            continue;
        }
        if s > i {
            out.push(TextRun {
                len: s - i,
                font: base.clone(),
                color: base_color,
                ..Default::default()
            });
        }
        let d = deco(*kind);
        let mut tr = TextRun {
            len: e - s,
            font: d.font,
            ..Default::default()
        };
        if let Some(c) = d.color {
            tr.color = c;
        }
        if let Some(st) = d.strikethrough {
            tr.strikethrough = Some(st);
        }
        if let Some(ul) = d.underline {
            tr.underline = Some(ul);
        }
        out.push(tr);
        i = e;
    }
    if i < text.len() {
        out.push(TextRun {
            len: text.len() - i,
            font: base.clone(),
            color: base_color,
            ..Default::default()
        });
    }
    out
}

/// [`text_runs`] 的逐 run 装饰：字体 + 颜色/划线/下划线（全 `Option`，可只取字形）。
#[derive(Clone)]
pub struct TextRunDeco {
    pub font: gpui::Font,
    pub color: Option<Hsla>,
    pub strikethrough: Option<gpui::StrikethroughStyle>,
    pub underline: Option<gpui::UnderlineStyle>,
}
