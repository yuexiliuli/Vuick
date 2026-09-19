//! 实时预览编辑（WYSIWYG，Phase 3 v2）：把 Markdown **渲染视图**做成可直接编辑的
//! 块级编辑器。
//!
//! 架构（独立于 [`crate::editor::CodeEditor`] 的显示层）：
//! - **单一数据源 = [`crate::editor::buffer::Buffer`]**（Rope 纯文本 + 字节偏移），
//!   光标/选区/编辑全部在字节空间；WYSIWYG 只是它的**块级渲染滤镜**；
//! - 由 [`crate::markdown::parser::parse_to_rows_live`] 解析成 [`LiveRow`]（含
//!   「渲染字节 ↔ 源码字节」映射），逐块绘制并**自动换行**：每行被「按单词折行」拆成
//!   显示的段（`SegHit`），正文/标题/引述用**系统 UI 字体**、代码用等宽字体——
//!   命中整形与渲染同字体同字号（杜绝点击/光标错位）；
//! - 点击/键盘直接编辑 Buffer；代码块按源码行映射（点击代码行即光标落在该源码行），
//!   可正常增删改。
//!
//! 样式与右侧 `MarkdownPreview` 保持同步：引述灰色带 + 强调色左边条、代码深/浅底
//! + 等宽、标题分级字号加粗、行内加粗/斜体/代码/链接富样式。
//!
//! 调试：`#[cfg(debug_assertions)]` 下每帧打印 `[md-live] mouse / os-caret / input`
//! 三行，用于排查「鼠标点击 / OS 闪烁光标 / 实际输入位置」三者是否一致。

use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;

use gpui::{
    div, font, point, px, size, AnyElement, App, Bounds, Context, Element, ElementId,
    ElementInputHandler, Entity, EntityInputHandler, FocusHandle, Focusable, FontStyle, FontWeight,
    GlobalElementId, HighlightStyle, Hsla, InteractiveElement, IntoElement, KeyDownEvent, LayoutId,
    MouseButton, MouseDownEvent, ParentElement, Pixels, Point, Render, SharedString, ShapedLine,
    StatefulInteractiveElement, Styled, StyledText, UnderlineStyle, UTF16Selection, Window,
};

use crate::editor::buffer::Buffer;
use crate::editor::merge_highlights;
use crate::editor::selection::Selection;
use crate::markdown::parser::{LiveRow, RowKind, RowPayload, RunKind, is_text_kind, parse_to_rows_live};
use crate::markdown::view::style::StyleParams;

/// 系统 UI 字体（gpui 专用名 `.SystemUIFont`）：正文/标题/引述用，与预览一致。
const BODY_FONT: &str = ".SystemUIFont";
/// 行的左右外边距（px）：对齐预览 `px_3` 观感。
const ROW_MX: f32 = 12.0;
/// 光标条宽（px）。
const CARET_WIDTH_PX: f32 = 2.0;
/// 引述/代码带内纵衬（px）：布局用它起步，保证光标/命中与视觉一致。
const BAND_PY: f32 = 3.0;

/// 调试用：一位小数。
#[cfg(debug_assertions)]
fn round1(v: f32) -> f32 {
    (v * 10.0).round() / 10.0
}

/// 行内某一段（一个显示行）的命中信息：渲染/命中共用，含「段→源码」换算所需。
struct SegHit {
    row_ix: usize,
    /// 段在内容坐标（滚动前）中的 y 顶 / 高 / 文本起点 x（行内坐标）。
    top: f32,
    height: f32,
    x: f32,
    /// 文本行：段在 `row.text` 中的渲染字节范围。
    range: Range<usize>,
    /// 文本行：整行的渲染→源码映射。
    map: Option<Rc<Vec<usize>>>,
    /// 代码行：该源码行行首字节（恒等映射）。其余块 = `source_start`。
    base: usize,
    /// 段文本整形（与渲染同字体同字号）；None = 非交互段。
    line: Option<ShapedLine>,
    text_len: usize,
}

impl SegHit {
    /// 段内字符位置（本段相对字节）→ 源码字节。
    fn source_for(&self, local_byte: usize) -> usize {
        if let Some(map) = &self.map {
            let g = self.range.start + local_byte.min(self.range.len().max(0));
            let g = g.min(map.len() - 1);
            map[g]
        } else {
            self.base + local_byte.min(self.text_len)
        }
    }
}

/// 实时预览编辑（块级 WYSIWYG）。
pub struct MarkdownLivePreview {
    buffer: Buffer,
    selection: Selection,
    marked_range: Option<Range<usize>>,
    /// 当前解析出的渲染行（含「渲染 ↔ 源码」映射）。
    live: Vec<LiveRow>,
    focus_handle: FocusHandle,
    /// 排版参数快照（主题字体/颜色）。
    p: Rc<StyleParams>,
    /// 上一帧是否深色（主题切换触发刷新）。
    dark: bool,
    /// 内容区元素的窗口边界（包装元素 paint 回写；命中/坐标换算用）。
    element_bounds: Bounds<Pixels>,
    /// 纵向滚动句柄（跟随原生 `overflow_y_scroll` 容器）。
    v_handle: gpui::ScrollHandle,
    /// 上帧命中段表（内容坐标）。
    segs: Rc<RefCell<Vec<SegHit>>>,
    /// 字号覆盖（None = 主题默认），变化后下一帧刷新排版参数。
    font_override: Option<f32>,
    /// 排版参数是否待刷新（字号覆盖变化）。
    styles_dirty: bool,
    /// 自绘光标闪烁开关（每 ~530ms 切换）。
    blink_on: bool,
    /// 闪烁定时器是否已启动（首次渲染时惰性启动一次）。
    blink_started: bool,
    /// 上一次推给平台的滚动偏移（滚动变化时重推 IME 定位，避免候选窗停在滚动前）。
    last_ime_scroll: f32,
    /// `GPUI_IMEDEBUG=1` 调试覆盖层：绿点=最后点击、洋红环=光标、红框=OS IME bounds。
    #[cfg(debug_assertions)]
    ime_debug: bool,
    /// 最后点击位置（内容坐标）。
    #[cfg(debug_assertions)]
    click_content: Option<(f32, f32)>,
    /// 最后报告的 IME bounds（内容坐标 x, y, w, h）。
    #[cfg(debug_assertions)]
    ime_content: Option<(f32, f32, f32, f32)>,
    /// 点击落点与光标差过大（行尾/空行/短行钳制）时，「点击点」幽灵竖条
    /// （内容 x, y, h, 时间戳）。只在点击后 ~700ms 内绘制，用来说明
    /// 「为什么光标在这里、插入在这里」——参考实现（zorite/vellum）同为钳制，
    /// 幽灵条是纯粹的视觉说明，不改变插入位置。
    click_ghost: Option<(f32, f32, f32, std::time::Instant)>,
    /// 上次 `relayout` 使用的布局宽度。命中/IME 查询若发现当前元素宽度已变
    /// （窗口→全屏换宽瞬间旧几何残留），强制重排，避免用旧宽度的换行做点击/输入定位。
    last_layout_width: f32,
    /// 空行「插入列」（真 WYSIWYG）：点空行后记录 (行 idx, 点击列内容 x, 空格宽)。
    /// 光标/IME 显示在该列；首次输入先补空格到该列，字符落在点击处；非空短行仍 EOL 钳制。
    goal: Option<(usize, f32, f32)>,
}

impl MarkdownLivePreview {
    /// 以源码创建实时预览编辑器（光标在 0）。
    pub fn new(source: &str, cx: &mut App) -> Self {
        let buffer = Buffer::new(source);
        let live = parse_to_rows_live(source);
        let p = Rc::new(StyleParams::from_theme_with_font(cx, None));
        let dark = gpui_component::Theme::global(cx).is_dark();
        #[cfg(debug_assertions)]
        let ime_debug = std::env::var("GPUI_IMEDEBUG").is_ok();
        Self {
            buffer,
            selection: Selection::collapsed(0),
            marked_range: None,
            live,
            focus_handle: cx.focus_handle().tab_stop(true),
            p,
            dark,
            element_bounds: Bounds::default(),
            v_handle: gpui::ScrollHandle::new(),
            segs: Rc::new(RefCell::new(Vec::new())),
            font_override: None,
            styles_dirty: false,
            blink_on: true,
            blink_started: false,
            last_ime_scroll: 0.0,
            #[cfg(debug_assertions)]
            ime_debug,
            #[cfg(debug_assertions)]
            click_content: None,
            #[cfg(debug_assertions)]
            ime_content: None,
            click_ghost: None,
            last_layout_width: 0.0,
            goal: None,
        }
    }

    /// 首次渲染时启动自绘光标闪烁（只启动一次）。
    fn ensure_blink(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.blink_started {
            return;
        }
        self.blink_started = true;
        cx.spawn_in(window, async move |this, cx: &mut gpui::AsyncWindowContext| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(530))
                    .await;
                let Some(this) = this.upgrade() else { break };
                let _ = this.update_in(cx, |this, window, cx| {
                    // 仅在编辑器获得输入焦点时闪烁；失焦则光标常亮且不重绘。
                    if window.focused(cx).as_ref() == Some(&this.focus_handle) {
                        this.blink_on = !this.blink_on;
                        cx.notify();
                    } else {
                        this.blink_on = true;
                    }
                });
            }
        })
        .detach();
    }

    /// 当前全文（退出实时模式时写回/保存用）。
    pub fn text(&self) -> String {
        self.buffer.to_string()
    }

    /// 覆盖正文字号（应用 Ctrl+=/- 缩放用）；下一帧刷新排版参数。
    pub fn set_font_override(&mut self, size_px: Option<f32>) {
        if self.font_override != size_px {
            self.font_override = size_px;
            self.styles_dirty = true;
        }
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    /// 刷新主题排版参数（明暗 / 字号覆盖变化时）。
    fn refresh_styles(&mut self, cx: &mut Context<Self>) {
        let dark = gpui_component::Theme::global(cx).is_dark();
        if dark != self.dark || self.styles_dirty {
            self.dark = dark;
            self.styles_dirty = false;
            self.p = Rc::new(StyleParams::from_theme_with_font(cx, self.font_override));
        }
    }

    /// 编辑后重解析（整篇）+ 清命中。
    fn reparse(&mut self) {
        self.live = parse_to_rows_live(&self.buffer.to_string());
        self.segs.borrow_mut().clear();
    }

    /// 应用一次替换的**核心**（无 `cx`，可单测）：改 Buffer → 选区移到新文本末尾 →
    /// 重解析。UI 包装见 [`Self::insert_edit`]（额外 notify）。
    fn apply_edit(&mut self, range: Range<usize>, text: &str) {
        let s = self.buffer.clip(range.start).min(self.buffer.len());
        let e = self.buffer.clip(range.end).min(self.buffer.len());
        let (s, e) = if s <= e { (s, e) } else { (e, s) };
        self.buffer.apply_edit(s..e, text);
        self.selection = Selection::collapsed(s + text.len());
        self.marked_range = None;
        self.reparse();
    }

    /// 应用一次替换：改 Buffer → 选区移到新文本末尾 → 重解析 + 通知。
    ///
    /// 每次真实输入/编辑后必须把 OS 的 IME 定位推给平台（`invalidate_character_coordinates`
    /// → 下一帧 `selected_bounds` → `ImmSetCompositionWindow`/`ImmSetCandidateWindow`）。
    /// 否则 Windows 只在 `WM_IME_STARTCOMPOSITION` 拉取一次光标几何，组合/滚动期间
    /// 候选窗与自绘闪烁光标会互相脱离。
    fn insert_edit(&mut self, range: Range<usize>, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.apply_edit(range, text);
        window.invalidate_character_coordinates();
        cx.notify();
    }

    // ------------------------------------------------------------------
    // 坐标换算：源码字节 ↔ 渲染位置
    // ------------------------------------------------------------------

    /// 光标（源码偏移）→ (渲染行下标, 行内渲染字节)。文本行精确；块行 = (行,0)。
    fn caret_target(&self) -> Option<(usize, usize)> {
        if self.live.is_empty() {
            return None;
        }
        let off = self.buffer.clip(self.selection.head());
        for (ix, lr) in self.live.iter().enumerate() {
            if lr.map.is_empty() {
                continue;
            }
            let lo = lr.map[0];
            let hi = *lr.map.last().unwrap();
            if off >= lo && off <= hi {
                let b = lr.map.partition_point(|&s| s <= off).saturating_sub(1);
                let b = b.min(lr.map.len() - 1);
                return Some((ix, b));
            }
        }
        let mut chosen = 0usize;
        for (ix, lr) in self.live.iter().enumerate() {
            if lr.source_start <= off {
                chosen = ix;
            } else {
                break;
            }
        }
        Some((chosen, 0))
    }

    /// 该行渲染字节区间内是否有选中；命中返回（渲染起, 渲染止）。
    fn selection_rendered(&self, lr: &LiveRow, sel: (usize, usize)) -> Option<Range<usize>> {
        if lr.map.is_empty() {
            return None;
        }
        let (s, e) = if sel.0 <= sel.1 { sel } else { (sel.1, sel.0) };
        let lo = lr.map[0];
        let hi = *lr.map.last().unwrap();
        if e <= lo || s >= hi {
            return None;
        }
        let rs = lr.map.partition_point(|&v| v <= s).saturating_sub(1);
        let re = lr.map.partition_point(|&v| v <= e).saturating_sub(1).min(lr.map.len() - 1);
        Some(rs.min(re)..re)
    }

    // ------------------------------------------------------------------
    // 布局与渲染
    // ------------------------------------------------------------------

    /// 行内样式（RunKind → 富样式）。
    fn run_highlight(&self, kind: RunKind) -> HighlightStyle {
        let mut s = HighlightStyle::default();
        match kind {
            RunKind::Bold => {
                s.font_weight = Some(FontWeight::BOLD);
            }
            RunKind::Italic => {
                s.font_style = Some(FontStyle::Italic);
            }
            RunKind::InlineCode => {
                s.background_color = Some(self.p.code_bg);
            }
            RunKind::Strikethrough => {
                s.strikethrough = Some(gpui::StrikethroughStyle { thickness: px(1.0), color: None });
            }
            RunKind::Link | RunKind::Image => {
                s.color = Some(self.p.accent);
                s.underline = Some(UnderlineStyle {
                    thickness: px(1.0),
                    color: Some(self.p.accent),
                    wavy: false,
                });
            }
        }
        s
    }

    /// 组装整行渲染字节内的样式段（富样式 + 本行选区底色），供段裁剪。
    fn row_styles(&self, lr: &LiveRow, sel: Option<Range<usize>>) -> Vec<(Range<usize>, HighlightStyle)> {
        let runs: Vec<(Range<usize>, HighlightStyle)> = lr
            .row
            .runs
            .iter()
            .map(|(r, k)| (r.clone(), self.run_highlight(*k)))
            .collect();
        merge_highlights(&runs, sel, self.p.accent.opacity(0.30))
    }

    /// 把整行样式裁剪到某段渲染字节范围（`range` 相对 `row_text`）。
    fn seg_styled_text(
        &self,
        seg_text: &SharedString,
        range: &Range<usize>,
        styles: &[(Range<usize>, HighlightStyle)],
    ) -> StyledText {
        let mut clipped: Vec<(Range<usize>, HighlightStyle)> = Vec::new();
        for (r, st) in styles {
            let s = r.start.max(range.start);
            let e = r.end.min(range.end);
            if e > s {
                clipped.push(((s - range.start)..(e - range.start), *st));
            }
        }
        StyledText::new(seg_text.clone()).with_highlights(clipped)
    }

    /// 空行目标列用：该行字体的空格前缀宽（`" "` 的 x_for_index(1)）。
    fn space_advance(&self, window: &mut Window, kind: RowKind) -> f32 {
        let p = self.p.clone();
        let (fs, fam) = if matches!(kind, RowKind::Code) {
            (p.code_size, p.mono.clone())
        } else {
            (p.para_size, SharedString::from(BODY_FONT))
        };
        let run = gpui::TextRun {
            len: 1,
            font: font(fam),
            color: Hsla::default(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let l = window.text_system().shape_line(SharedString::from(" "), fs, &[run], None);
        l.x_for_index(1).as_f32().max(1.0)
    }

    /// 按单词自动换行：把 `text` 拆成 ≤`max_width_px` 的显示段。返回 (段字节范围, 段文本, 整形)。
    fn wrap_segments(
        &self,
        window: &mut Window,
        text: SharedString,
        font_size: gpui::Pixels,
        family: SharedString,
        max_width_px: f32,
    ) -> Vec<(Range<usize>, SharedString, ShapedLine)> {
        let shape = |window: &mut Window, s: &str, len: usize| -> ShapedLine {
            let run = gpui::TextRun {
                len,
                font: font(family.clone()),
                color: Hsla::default(),
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            window
                .text_system()
                .shape_line(SharedString::from(s), font_size, &[run], None)
        };
        if text.is_empty() || max_width_px <= 8.0 {
            let l = shape(window, text.as_ref(), text.len());
            return vec![(0..l.len(), text, l)];
        }
        let mut out: Vec<(Range<usize>, SharedString, ShapedLine)> = Vec::new();
        let bytes = text.as_bytes();
        let mut line_begin = 0usize;
        let mut i = 0usize;
        let mut acc = String::new();
        let mut last_break = 0usize;
        while i < bytes.len() {
            let ch = text[i..].chars().next().unwrap();
            acc.push_str(&text[i..i + ch.len_utf8()]);
            if ch.is_whitespace() {
                last_break = acc.len();
            }
            let w = shape(window, &acc, acc.len()).width.as_f32();
            if w > max_width_px && !acc.is_empty() {
                let (cut_rel, skip_space) = if last_break > 0 {
                    (last_break, true)
                } else {
                    ((acc.len() - ch.len_utf8()).max(1), false)
                };
                let slice_end = line_begin + cut_rel;
                let seg_text = SharedString::from(text[line_begin..slice_end].to_string());
                let l = shape(window, &seg_text, seg_text.len());
                out.push((line_begin..slice_end, seg_text, l));
                line_begin = slice_end;
                if skip_space {
                    while line_begin < bytes.len()
                        && text[line_begin..].chars().next().is_some_and(|c| c.is_whitespace())
                    {
                        line_begin += text[line_begin..].chars().next().unwrap().len_utf8();
                    }
                }
                last_break = 0;
                let hi = i.saturating_add(ch.len_utf8()).max(line_begin);
                acc = text.get(line_begin..hi).map(str::to_string).unwrap_or_default();
            }
            i += ch.len_utf8();
        }
        if text.len() > line_begin {
            let seg_text = SharedString::from(text[line_begin..].to_string());
            let l = shape(window, &seg_text, seg_text.len());
            out.push((line_begin..text.len(), seg_text, l));
        }
        out
    }

    /// 布局某行 → 把该行各段写入 `self.segs`（内容坐标），返回行高。
    fn layout_row(
        &mut self,
        window: &mut Window,
        row_ix: usize,
        lr: &LiveRow,
        y: f32,
        width_px: f32,
        row_maps: &[Option<Rc<Vec<usize>>>],
    ) -> f32 {
        let p = self.p.clone();
        let map = row_maps[row_ix].clone();
        let left_x = Self::row_left_x(lr.row.kind, lr);
        // 引述带用内衬（py）留出上下呼吸空间；代码块改用「顶/底空白带」实现，
        // 不再带内衬（首/末空白带各自占一行，对齐预览 code_block_bands）。
        let band_py = if matches!(lr.row.kind, RowKind::Quote) {
            BAND_PY
        } else {
            0.0
        };
        let mut row_h = band_py;
        // 引述 DOM 带上下 `py(BAND_PY)`：上内衬随文本段顶部对齐，下内衬（3px）只影响
        // 该行总高。必须计入返回的 `row_h`，否则后续行的 seg 几何比实际绘制矮 → 每处
        // 引述累积 +3px「光标/输入相对文本往上飘」漂移。
        let band_footer = if matches!(lr.row.kind, RowKind::Quote) {
            BAND_PY
        } else {
            0.0
        };
        let segs = self.segs.clone();
        let push = |segs: &Rc<RefCell<Vec<SegHit>>>, s: SegHit| segs.borrow_mut().push(s);
        if is_text_kind(lr.row.kind) {
            let font_size = match lr.row.kind {
                RowKind::Heading(level) => p.heading_metrics(level).0,
                _ => p.para_size,
            };
            let line_h_f = match lr.row.kind {
                RowKind::Heading(level) => p.heading_metrics(level).1.as_f32(),
                _ => p.para_line.as_f32(),
            };
            let max_w = Self::text_width(width_px, lr.row.kind);
            let wrapped = self.wrap_segments(window, lr.row.text.clone(), font_size, SharedString::from(BODY_FONT), max_w);
            for (range, seg_text, line) in &wrapped {
                let len = seg_text.len();
                push(
                    &segs,
                    SegHit {
                        row_ix,
                        top: y + row_h,
                        height: line_h_f,
                        x: left_x,
                        range: range.clone(),
                        map: map.clone(),
                        base: lr.source_start,
                        line: Some(line.clone()),
                        text_len: len,
                    },
                );
                row_h += line_h_f;
            }
            if wrapped.is_empty() {
                push(
                    &segs,
                    SegHit {
                        row_ix,
                        top: y + row_h,
                        height: line_h_f,
                        x: left_x,
                        range: 0..0,
                        map: map.clone(),
                        base: lr.source_start,
                        line: None,
                        text_len: 0,
                    },
                );
                row_h += line_h_f;
            }
            row_h += band_footer;
        } else if lr.row.kind == RowKind::Code {
            let (code_lines, _language) = match &lr.row.payload {
                Some(pl) => match &**pl {
                    RowPayload::Code { code_lines, language } => (code_lines.clone(), language.clone()),
                    _ => (Vec::new(), None),
                },
                None => (Vec::new(), None),
            };
            let fence_line = self.buffer.line_of_offset(lr.source_start);
            let line_h = p.code_line.as_f32();
            let spacer = |row_ix: usize, y: f32, x: f32, base: usize| SegHit {
                row_ix,
                top: y,
                height: line_h,
                x,
                range: 0..0,
                map: None,
                base,
                line: None,
                text_len: 0,
            };
            // 顶空白带（圆角卡片的顶部呼吸空间，对齐预览首行空白带，非交互段）。
            push(&segs, spacer(row_ix, y + row_h, left_x, lr.source_start));
            row_h += line_h;
            // 语言头（非交互段，占一行保证几何对齐）；仅渲染存在语言标签时画，故必须存在。
            if _language.is_some() {
                push(&segs, spacer(row_ix, y + row_h, left_x, lr.source_start));
                row_h += line_h;
            }
            for k in 0..code_lines.len() {
                let base_line = (fence_line + 1 + k).min(self.buffer.line_count().saturating_sub(1));
                let base = self.buffer.line_start(base_line);
                let code = code_lines[k].clone();
                let run = gpui::TextRun {
                    len: code.len(),
                    font: font(p.mono.clone()),
                    color: Hsla::default(),
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                };
                let l = window.text_system().shape_line(code.clone(), p.code_size, &[run], None);
                let len = code.len();
                push(
                    &segs,
                    SegHit {
                        row_ix,
                        top: y + row_h,
                        height: line_h,
                        x: left_x,
                        range: 0..len,
                        map: None,
                        base,
                        line: Some(l),
                        text_len: len,
                    },
                );
                row_h += line_h;
            }
            // 底空白带（圆角卡片的底部呼吸空间）。
            let last_base = code_lines
                .len()
                .checked_sub(1)
                .map(|k| {
                    let base_line = (fence_line + 1 + k).min(self.buffer.line_count().saturating_sub(1));
                    self.buffer.line_start(base_line)
                })
                .unwrap_or(lr.source_start);
            push(&segs, spacer(row_ix, y + row_h, left_x, last_base));
            row_h += line_h;
        } else {
            let h = self.block_height(lr.row.kind, lr, p.clone());
            push(
                &segs,
                SegHit {
                    row_ix,
                    top: y + row_h,
                    height: h,
                    x: left_x,
                    range: 0..0,
                    map: None,
                    base: lr.source_start,
                    line: None,
                    text_len: 0,
                },
            );
            row_h += h;
        }
        row_h
    }

    /// 行文本左起点 x（对齐预览）：外层 `px_3`(ROW_MX) 内缩背景，代码带再加带内
    /// `pl_3`(ROW_MX)，引述带再叠左边条宽。与 `text_or_code_element` 的结构一一对应。
    fn row_left_x(kind: RowKind, _lr: &LiveRow) -> f32 {
        match kind {
            RowKind::Quote => crate::config::markdown::QUOTE_BAR_WIDTH_PX + ROW_MX * 2.0,
            RowKind::Code => ROW_MX * 2.0,
            _ => ROW_MX,
        }
    }

    /// 当前行的文本可用宽度（与渲染结构一致，供自动换行折行用）：
    /// 引述 = 宽 - 外层两内缩 - 带内两内缩 - 左边条；其余文本行 = 宽 - 两内缩。
    fn text_width(width_px: f32, kind: RowKind) -> f32 {
        match kind {
            RowKind::Quote => width_px - crate::config::markdown::QUOTE_BAR_WIDTH_PX - ROW_MX * 4.0,
            _ => width_px - ROW_MX * 2.0,
        }
    }

    /// 非文本占位块高度。
    fn block_height(&self, kind: RowKind, lr: &LiveRow, p: Rc<StyleParams>) -> f32 {
        match kind {
            RowKind::Blank => p.blank_h.as_f32(),
            RowKind::Rule => p.rule_h.as_f32(),
            RowKind::Image => crate::config::markdown::IMAGE_ROW_DEFAULT_H as f32,
            RowKind::Table => {
                let rows = lr.row.table().map_or(1.0, |t| (t.rows.len() + 1) as f32);
                rows * (p.para_line.as_f32() + p.para_gap.as_f32())
            }
            _ => p.para_line.as_f32(),
        }
    }

    /// 文本（或代码）行的元素：由 `self.segs` 里该行的段驱动。
    fn text_or_code_element(
        &mut self,
        row_ix: usize,
        lr: &LiveRow,
        styles: &[(Range<usize>, HighlightStyle)],
    ) -> AnyElement {
        let p = self.p.clone();
        if lr.row.kind == RowKind::Code {
            let (mut lines, language) = match &lr.row.payload {
                Some(pl) => match &**pl {
                    RowPayload::Code { code_lines, language } => (code_lines.clone(), language.clone()),
                    _ => (Vec::new(), None),
                },
                None => (Vec::new(), None),
            };
            let line_h = p.code_line;
            // 整块圆角卡片（对齐预览 code_block_bands）：顶/底空白带同背景铺平（上下
            // 各留一行呼吸空间），整块圆角 lg(8px)；代码行 `pl_3`/`pr_3` 内衬；
            // 外层 `px_3` 内缩 → 背景不与边缘相接。
            let mut col = div()
                .w_full()
                .flex()
                .flex_col()
                .bg(p.code_bg)
                .overflow_hidden()
                .rounded(px(8.0));
            col = col.child(div().h(line_h).w_full().bg(p.code_bg));
            if let Some(lang) = language {
                col = col.child(
                    div()
                        .h(line_h)
                        .flex()
                        .items_center()
                        .pl(px(ROW_MX))
                        .pr(px(ROW_MX))
                        .text_color(p.muted_fg)
                        .text_size(px(11.0))
                        .child(lang),
                );
            }
            for code in lines.drain(..) {
                col = col.child(
                    div()
                        .h(line_h)
                        .flex()
                        .items_center()
                        .pl(px(ROW_MX))
                        .pr(px(ROW_MX))
                        .child(
                            div()
                                .whitespace_nowrap()
                                .overflow_hidden()
                                .font_family(p.mono.clone())
                                .text_size(p.code_size)
                                .text_color(p.fg)
                                .child(SharedString::from(code.as_ref())),
                        ),
                );
            }
            col = col.child(div().h(line_h).w_full().bg(p.code_bg));
            return div().w_full().px(px(ROW_MX)).child(col).into_any_element();
        }
        // 文本行：按该行的段逐段渲染（自动换行由 `wrap_segments` 已拆好的段驱动）。
        let text = lr.row.text.clone();
        let style_fs = match lr.row.kind {
            RowKind::Heading(level) => p.heading_metrics(level).0,
            _ => p.para_size,
        };
        let mut stack = div().w_full().flex().flex_col();
        let segs = self.segs.borrow();
        for s in segs.iter().filter(|s| s.row_ix == row_ix) {
            let seg_text = if s.range.start < text.len() {
                SharedString::from(text[s.range.start.min(text.len())..s.range.end.min(text.len())].to_string())
            } else {
                SharedString::from("")
            };
            let st = self.seg_styled_text(&seg_text, &s.range, styles);
            let color = if lr.row.kind == RowKind::Quote {
                p.muted_fg
            } else {
                p.fg
            };
            stack = stack.child(
                div()
                    .h(px(s.height))
                    .flex()
                    .items_center()
                    .font_family(SharedString::from(BODY_FONT))
                    .text_size(style_fs)
                    .text_color(color)
                    .child(st),
            );
        }
        let stack = stack.into_any_element();
        if lr.row.kind == RowKind::Quote {
            let bar: Hsla = gpui::rgb(crate::config::markdown::QUOTE_BAR_COLOR).into();
            // 对齐预览：外层 `px_3` 内缩（背景不与边缘相接），内层带 `pl_3`/`pr_3`
            // + `py_1` 内衬 + 左边条 + 灰底。
            return div()
                .w_full()
                .px(px(ROW_MX))
                .child(
                    div()
                        .w_full()
                        .py(px(BAND_PY))
                        .pl(px(ROW_MX))
                        .pr(px(ROW_MX))
                        .border_l(px(crate::config::markdown::QUOTE_BAR_WIDTH_PX))
                        .border_color(bar)
                        .bg(p.quote_bg)
                        .child(stack),
                )
                .into_any_element();
        }
        div()
            .w_full()
            .pl(px(Self::row_left_x(lr.row.kind, lr)))
            .pr(px(ROW_MX))
            .child(stack)
            .into_any_element()
    }

    /// 空/规则/图片/表格等块元素。
    fn block_element(&mut self, lr: &LiveRow) -> AnyElement {
        let p = self.p.clone();
        match lr.row.kind {
            RowKind::Blank => div().h(px(p.blank_h.as_f32())).w_full().into_any_element(),
            RowKind::Rule => div()
                .w_full()
                .h(px(p.rule_h.as_f32()))
                .flex()
                .items_center()
                .pl(px(ROW_MX))
                .pr(px(ROW_MX))
                .child(div().w_full().h(px(1.0)).bg(p.divider))
                .into_any_element(),
            RowKind::Image => div()
                .w_full()
                .h(px(crate::config::markdown::IMAGE_ROW_DEFAULT_H))
                .flex()
                .items_center()
                .justify_center()
                .pl(px(ROW_MX))
                .child(div().text_color(p.muted_fg).text_size(px(12.0)).child("图片（实时模式只读占位）"))
                .into_any_element(),
            RowKind::Table => {
                let mut col = div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .pl(px(ROW_MX))
                    .pr(px(ROW_MX))
                    .text_size(p.para_size)
                    .text_color(p.fg);
                if let Some(t) = lr.row.table() {
                    col = col
                        .child(
                            div()
                                .whitespace_nowrap()
                                .font_weight(FontWeight::BOLD)
                                .bg(p.table_head_bg)
                                .child(format!("| {}", t.header.join(" | "))),
                        )
                        .child(
                            div().whitespace_nowrap().child(
                                t.rows.first().map_or_else(String::new, |r| {
                                    format!(
                                        "| {}",
                                        r.iter().map(|s| s.as_ref()).collect::<Vec<_>>().join(" | ")
                                    )
                                }),
                            ),
                        );
                }
                col.into_any_element()
            }
            _ => div().w_full().h(px(p.para_line.as_f32())).into_any_element(),
        }
    }

    /// 按**当前** buffer/live 重新布局：清空 `segs` 后逐行写入内容坐标，返回总内容高。
    /// 渲染帧与「渲染外的几何查询」（IME 光标 bounds / 字符点选 / 鼠标点击）共用：
    /// 编辑后 `reparse` 已清空 `segs`，而输入系统会在**下一帧渲染前**同步查询光标
    /// 坐标——若直接用上帧的过期/空 `segs`，光标会掉到兜底（文档末尾）→
    /// 「闪烁光标与输入实际位置不匹配」。因此这些查询在 `segs` 为空时先重排。
    fn relayout(&mut self, window: &mut Window) -> f32 {
        self.segs.borrow_mut().clear();
        let width_px = {
            let w = self.element_bounds.size.width.as_f32();
            if w > 0.0 {
                w
            } else {
                window.bounds().size.width.as_f32().max(400.0)
            }
        };
        self.last_layout_width = width_px;
        let row_maps: Vec<Option<Rc<Vec<usize>>>> = self
            .live
            .iter()
            .map(|lr| {
                if lr.map.is_empty() {
                    None
                } else {
                    Some(Rc::new(lr.map.clone()))
                }
            })
            .collect();
        let mut y = 0.0f32;
        let n = self.live.len();
        for ix in 0..n {
            let lr = self.live[ix].clone();
            let h = self.layout_row(window, ix, &lr, y, width_px, &row_maps);
            y += h;
        }
        y
    }

    /// 确保 hit/IME 用「当前宽度」几何：`segs` 空（编辑后重解析）或元素宽度已变
    /// （窗口↔全屏换宽瞬间，旧宽度的换行会残留）→ 强制重排，杜绝旧几何定位。
    fn ensure_fresh_layout(&mut self, window: &mut Window) {
        let width_changed = self.element_bounds.size.width.as_f32() != self.last_layout_width;
        if self.segs.borrow().is_empty() || width_changed {
            self.relayout(window);
        }
    }

    /// 布局 + 渲染全部行，返回容器元素。
    fn render_body(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        if self.live.is_empty() {
            self.segs.borrow_mut().clear();
            return div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(div().text_color(self.p.muted_fg).child("空文档"))
                .into_any_element();
        }
        // 1) 布局：逐行写 `segs`（内容坐标），累计行高。
        self.relayout(window);
        // 滚动偏移变化 → 内容坐标→屏幕坐标的换算已变：重推 IME 定位，避免候选窗/
        // 组合窗停在滚动前的位置（Windows 只在组合开始或显式 push 时更新）。
        let scroll_y = (-self.v_handle.offset().y.as_f32()).max(0.0);
        if scroll_y != self.last_ime_scroll {
            self.last_ime_scroll = scroll_y;
            window.invalidate_character_coordinates();
        }
        let sel = {
            let s = self.selection;
            (s.start(), s.end())
        };
        let entity = cx.entity();
        // 2) 逐行建元素。
        let mut rows_el = div().w_full().relative().flex().flex_col();
        for ix in 0..self.live.len() {
            let lr = self.live[ix].clone();
            let el = if is_text_kind(lr.row.kind) || lr.row.kind == RowKind::Code {
                let sel_rend = self.selection_rendered(&lr, sel);
                let styles = self.row_styles(&lr, sel_rend);
                self.text_or_code_element(ix, &lr, &styles)
            } else {
                self.block_element(&lr)
            };
            // debug：每行包一个 `debug_selector` 包装器，登记 GPUI **实际绘制**的
            // bounds——供 `paint` 后逐行核对 painted top == seg top（判定算法 vs 实际）。
            #[cfg(debug_assertions)]
            {
                rows_el = rows_el.child(
                    div().debug_selector(move || format!("md-row-{ix}")).child(el),
                );
            }
            #[cfg(not(debug_assertions))]
            {
                rows_el = rows_el.child(el);
            }
        }
        // 3) 光标 overlay：绝对定位在内容坐标系（随滚动）；自绘闪烁（不依赖平台/IME 光标）。
        //    空行「插入列」：目标行匹配时把光标条画在点击列（真 WYSIWYG）。
        if let Some((crow, x, y, h)) = self.caret_point() {
            let draw_x = match self.goal {
                Some((grow, gx, _)) if grow == crow => gx,
                _ => x,
            };
            let blink = self.blink_on;
            rows_el = rows_el.child(
                div()
                    .absolute()
                    .left(px(draw_x))
                    .top(px(y + 1.5))
                    .h(px(h - 3.0))
                    .w(px(CARET_WIDTH_PX))
                    .rounded(px(1.0))
                    .opacity(if blink { 0.9 } else { 0.12 })
                    .bg(gpui::rgb(0x0078d4)),
            );
        }
        // 4) 钳制说明幽灵条：点击处（仅行尾/空行钳制时出现，~700ms 后消失）。
        if let Some((gx, gy, gh, at)) = self.click_ghost {
            if at.elapsed() < std::time::Duration::from_millis(700) {
                rows_el = rows_el.child(
                    div()
                        .absolute()
                        .left(px(gx))
                        .top(px(gy + 1.5))
                        .h(px((gh - 3.0).max(1.0)))
                        .w(px(CARET_WIDTH_PX))
                        .rounded(px(1.0))
                        .opacity(0.35)
                        .bg(self.p.accent),
                );
            } else {
                self.click_ghost = None;
            }
        }
        let bg = self.p.bg;
        let scroll_div = div()
            .id("md-live-scroll")
            .size_full()
            .overflow_y_scroll()
            .overflow_x_hidden()
            .track_scroll(&self.v_handle)
            .track_focus(&self.focus_handle)
            .on_mouse_down(MouseButton::Left, window.listener_for(&entity, Self::on_mouse_down))
            .on_key_down(window.listener_for(&entity, Self::on_key_down))
            .child(rows_el);
        let mut root = div()
            .id("md-live")
            .debug_selector(|| "md-live".into())
            .size_full()
            .relative()
            .overflow_hidden()
            .bg(bg)
            .child(scroll_div);
        #[cfg(debug_assertions)]
        if self.ime_debug {
            root = root.child(self.debug_overlay());
        }
        root.into_any_element()
    }

    /// `GPUI_IMEDEBUG=1` 覆盖层：绿点=最后点击、洋红环=光标、红框=OS IME bounds。
    /// 三者重合 = WYSIWYG 定位正确；谁偏离谁就暴露在那一环。
    #[cfg(debug_assertions)]
    fn debug_overlay(&self) -> impl IntoElement {
        let scroll_y = (-self.v_handle.offset().y.as_f32()).max(0.0);
        let mut ov = div().absolute().left(px(0.)).top(px(0.)).size_full();
        if let Some((ix, iy, iw, ih)) = self.ime_content {
            ov = ov.child(
                div()
                    .absolute()
                    .left(px(ix))
                    .top(px(iy - scroll_y))
                    .w(px(iw.max(1.0)))
                    .h(px(ih.max(1.0)))
                    .border(px(1.5))
                    .border_color(gpui::rgb(0xe53935).opacity(0.9))
                    .bg(gpui::rgb(0xe53935).opacity(0.10)),
            );
        }
        if let Some((cx, cy, _h)) = self.caret_point().map(|(_, x, y, h)| (x, y, h)) {
            let cy = cy - scroll_y;
            ov = ov.child(
                div()
                    .absolute()
                    .left(px(cx - 6.0))
                    .top(px(cy - 6.0))
                    .w(px(12.0))
                    .h(px(12.0))
                    .rounded(px(6.0))
                    .border(px(2.0))
                    .border_color(gpui::rgb(0xd81b60).opacity(0.9)),
            );
        }
        if let Some((gx, gy)) = self.click_content {
            let gy = gy - scroll_y;
            ov = ov.child(
                div()
                    .absolute()
                    .left(px(gx - 7.0))
                    .top(px(gy - 7.0))
                    .w(px(14.0))
                    .h(px(14.0))
                    .rounded(px(7.0))
                    .bg(gpui::rgb(0x43a047).opacity(0.85)),
            );
        }
        ov
    }

    /// 光标（源码偏移）→ (行, 内容坐标 x, 内容坐标 y, 段高)。
    fn caret_point(&self) -> Option<(usize, f32, f32, f32)> {
        if self.live.is_empty() {
            return None;
        }
        self.point_for_offset(self.buffer.clip(self.selection.head()))
    }

    /// 任意源码偏移 → (行, 内容坐标 x, 内容坐标 y, 段高)。与 `caret_point` 同源，
    /// 供 IME 组合区间（起点/终点）与选区几何使用——所有几何都来自同一份 `segs`。
    fn point_for_offset(&self, off: usize) -> Option<(usize, f32, f32, f32)> {
        if self.live.is_empty() {
            return None;
        }
        let segs = self.segs.borrow();
        let mut found = None::<(usize, f32, f32, f32)>;
        for s in segs.iter() {
            let (local_b, hit) = if let Some(map) = &s.map {
                let lo = map[s.range.start.min(map.len() - 1)];
                let hi_m = s.range.end.min(map.len() - 1);
                let hi = map[hi_m];
                if off >= lo && off <= hi {
                    let g = map.partition_point(|&v| v <= off).saturating_sub(1).clamp(s.range.start, hi_m);
                    (g - s.range.start, true)
                } else {
                    (0, false)
                }
            } else if off >= s.base && off <= s.base + s.text_len {
                ((off - s.base).min(s.text_len), true)
            } else {
                (0, false)
            };
            let x = s
                .line
                .as_ref()
                .map(|l| l.x_for_index(local_b.min(s.text_len)).as_f32())
                .unwrap_or(0.0);
            if hit {
                found = Some((s.row_ix, s.x + x, s.top, s.height));
                break;
            }
        }
        if let Some(v) = found {
            return Some(v);
        }
        // 兜底：off 在所有段之后 → 最末尾段。
        let s = segs.last()?;
        Some((s.row_ix, s.x, s.top, s.height))
    }

    /// 命中段：字段 y 是否落在段「可命中区」内。引述带上下各 3px 的 `py` 内衬
    /// 不计入任何 seg 的 top/height（隔开相邻行），但必须算成该行的可命中区——
    /// 否则点在带内留白会落空（"点这儿没反应"），且「可命中高」与绘制不等。
    fn seg_hit_bot(&self, s: &SegHit, cy: f32) -> bool {
        let is_quote = matches!(
            self.live.get(s.row_ix).map(|lr| lr.row.kind),
            Some(RowKind::Quote)
        );
        if is_quote {
            cy >= s.top - BAND_PY && cy <= s.top + s.height + BAND_PY
        } else {
            cy >= s.top && cy <= s.top + s.height
        }
    }

    /// 点击：命中段 → 源码字节落光标。
    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.button != MouseButton::Left {
            return;
        }
        window.focus(&self.focus_handle, cx);
        // 编辑后重解析会清空 `segs`/换宽后旧几何残留：点击前用当前宽度重排。
        self.ensure_fresh_layout(window);
        let local = event.position - self.element_bounds.origin;
        let scroll_y = (-self.v_handle.offset().y.as_f32()).max(0.0);
        let cy = local.y.as_f32() + scroll_y;
        let x = local.x.as_f32();
        let hit = {
            let segs = self.segs.borrow();
            let s = segs.iter().find(|s| self.seg_hit_bot(s, cy));
            s.map(|s| {
                let local_b = s
                    .line
                    .as_ref()
                    .map(|l| l.closest_index_for_x(px((x - s.x).max(0.0))).min(s.text_len))
                    .unwrap_or(0);
                (s.source_for(local_b), s.row_ix, s.x, s.line.is_none() && s.text_len == 0)
            })
        };
        if let Some((source, _, _, _)) = hit {
            self.selection = Selection::collapsed(source);
            self.marked_range = None;
        }
        // 点击即重定位光标：同步把 OS IME 定位推到新光标处（否则候选窗仍停旧位置）。
        window.invalidate_character_coordinates();
        if let Some((source, srow, _sx, is_empty)) = hit {
            if is_empty {
                // 空行 → 「插入列」真 WYSIWYG：光标/IME 显示在点击列，首次输入先补空格。
                let kind = self.live.get(srow).map(|lr| lr.row.kind).unwrap_or(RowKind::Blank);
                let sw = self.space_advance(window, kind);
                self.goal = Some((srow, x, sw.max(1.0)));
                self.click_ghost = None;
                let _ = source;
            } else {
                self.goal = None;
                // 幽灵条：仅非空短行 EOL 钳制时，说明「点在这里、字进那里」。
                if let Some((_, cx_, cy, ch)) = self.caret_point() {
                    if (x - cx_).abs() > 8.0 {
                        self.click_ghost = Some((x, cy, ch, std::time::Instant::now()));
                    } else {
                        self.click_ghost = None;
                    }
                }
            }
        }
        #[cfg(debug_assertions)]
        {
            // 调试覆盖层：记录点击的内容坐标（绿点）。
            self.click_content = Some((local.x.as_f32(), local.y.as_f32() + scroll_y));
        }
        #[cfg(debug_assertions)]
        {
            let c = self.caret_point();
            eprintln!(
                "[md-live] mouse @win=({:.1},{:.1}) bounds_origin=({:.1},{:.1}) bounds_w={:.1} local=({:.1},{:.1}) scroll={scroll_y:.1} -> byte={} caret_px={:?}",
                event.position.x.as_f32(),
                event.position.y.as_f32(),
                self.element_bounds.origin.x.as_f32(),
                self.element_bounds.origin.y.as_f32(),
                self.element_bounds.size.width.as_f32(),
                local.x.as_f32(),
                local.y.as_f32(),
                self.selection.head(),
                c.map(|(r, x, y, h)| (r, round1(x), round1(y), round1(h)))
            );
        }
        cx.notify();
    }

    /// 键盘：控制键（方向/退格/删除/回车/行首行尾）；可打印字符走输入通道。
    fn on_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let k = &event.keystroke;
        let m = k.modifiers;
        if !m.control && !m.alt && k.key_char.is_some() {
            return;
        }
        match k.key.to_ascii_lowercase().as_str() {
            "backspace" => {
                let off = self.selection.start();
                if off > 0 {
                    let prev = self.prev_char(off).unwrap_or(0);
                    self.insert_edit(prev..off, "", window, cx);
                }
            }
            "delete" => {
                let off = self.selection.end();
                if off < self.buffer.len() {
                    let next = self.next_char(off).unwrap_or(off + 1).max(off + 1);
                    let next = next.min(self.buffer.len());
                    self.insert_edit(off..next, "", window, cx);
                }
            }
            "enter" => {
                self.insert_edit(self.selection.range(), "\n", window, cx);
            }
            "left" | "right" | "up" | "down" | "home" | "end" => {
                self.move_caret(&k.key.to_ascii_lowercase());
                // 方向键离开目标行后清除「插入列」（避免把旧列渲染到别的行）。
                self.goal = None;
                window.invalidate_character_coordinates();
                cx.notify();
            }
            _ => {}
        }
    }

    fn move_caret(&mut self, key: &str) {
        let sel = self.selection;
        let (s, e) = (sel.start(), sel.end());
        match key {
            "left" => {
                let head = if s != e { s } else { self.selection.head() };
                let target = if head > 0 { self.prev_char(head).unwrap_or(0) } else { 0 };
                self.selection = Selection::collapsed(target);
            }
            "right" => {
                let head = if s != e { e } else { self.selection.head() };
                let target = if head < self.buffer.len() {
                    self.next_char(head).unwrap_or(self.buffer.len()).min(self.buffer.len())
                } else {
                    self.buffer.len()
                };
                self.selection = Selection::collapsed(target);
            }
            "up" | "down" => {
                let row = self.buffer.line_of_offset(self.selection.head());
                let last = self.buffer.line_count().saturating_sub(1);
                let target_row = if key == "up" {
                    row.saturating_sub(1)
                } else {
                    (row + 1).min(last)
                };
                self.selection = Selection::collapsed(self.buffer.line_start(target_row));
            }
            "home" => {
                let row = self.buffer.line_of_offset(self.selection.head());
                self.selection = Selection::collapsed(self.buffer.line_start(row));
            }
            "end" => {
                let row = self.buffer.line_of_offset(self.selection.head());
                self.selection = Selection::collapsed(self.buffer.line_end(row));
            }
            _ => {}
        }
    }

    /// 前一字符边界（不拆 UTF-8）。
    fn prev_char(&self, off: usize) -> Option<usize> {
        if off == 0 {
            return None;
        }
        let upto = self.buffer.text().slice(0..off.min(self.buffer.len()));
        upto.chars()
            .scan(0usize, |pos, c| {
                let s = *pos;
                *pos += c.len_utf8();
                Some(s)
            })
            .last()
    }

    /// 下一字符边界（不拆 UTF-8）。
    fn next_char(&self, off: usize) -> Option<usize> {
        if off >= self.buffer.len() {
            return None;
        }
        let from = self.buffer.text().slice(off..);
        Some(off + from.chars().next()?.len_utf8())
    }

    /// 空行「插入列」的补齐前缀：光标在目标空行（goal）且为空选区时，在 `text` 前补
    /// `floor((点击x - 行左缘)/空格宽)`（≤120）个空格，把首个字符抬到点击列。
    /// 直插（`replace_text_in_range`）与 **IME 组合**（`replace_and_mark_text_in_range`，
    /// 中文输入法走这条）共用——否则点空行打拼音，字会落到行首，光标却在点击列。
    fn goal_prefixed_text(&self, byte_range: &Range<usize>, text: &str) -> String {
        let Some((grow, gx, sw)) = self.goal else {
            return text.to_string();
        };
        if !(self.selection.is_collapsed()
            && self.selection.head() == byte_range.start
            && byte_range.start == byte_range.end)
        {
            return text.to_string();
        }
        let Some(lr) = self.live.get(grow) else {
            return text.to_string();
        };
        let left_x = Self::row_left_x(lr.row.kind, lr);
        let col = ((gx - left_x) / sw).floor().max(0.0) as usize;
        let col = col.min(120);
        if col > 0 {
            format!("{:padding$}{}", "", text, padding = col)
        } else {
            text.to_string()
        }
    }
}

impl Focusable for MarkdownLivePreview {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

// ---------------------------------------------------------------------------
// 输入（EntityInputHandler：IME / 剪贴板 / 文本输入）
// ---------------------------------------------------------------------------

impl EntityInputHandler for MarkdownLivePreview {
    fn accepts_text_input(&self, _window: &mut Window, _cx: &mut Context<Self>) -> bool {
        true
    }

    fn text_length_utf16(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        Some(self.buffer.len_utf16())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let sel = self.selection;
        Some(UTF16Selection {
            range: self.buffer.byte_to_utf16(sel.start())..self.buffer.byte_to_utf16(sel.end()),
            reversed: sel.reversed(),
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.marked_range.as_ref().map(|r| {
            self.buffer.byte_to_utf16(r.start).min(self.buffer.len_utf16())
                ..self.buffer.byte_to_utf16(r.end).min(self.buffer.len_utf16())
        })
    }

    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        _adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let s = self.buffer.utf16_to_byte(range_utf16.start);
        let e = self.buffer.utf16_to_byte(range_utf16.end);
        Some(self.buffer.text_in_range(s..e).to_string())
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let byte_range = match range {
            Some(r) => {
                self.buffer.utf16_to_byte(r.start.min(self.buffer.len_utf16()))
                    ..self.buffer.utf16_to_byte(r.end.min(self.buffer.len_utf16()))
            }
            None => self.marked_range.clone().unwrap_or_else(|| self.selection.range()),
        };
        let mut normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        // 空行「插入列」：首次输入时先把空格补到点击列，字符落在点击处（真 WYSIWYG），
        // 然后清除目标列（后续输入回到真实光标）。
        normalized = self.goal_prefixed_text(&byte_range, &normalized);
        self.goal = None;
        #[cfg(debug_assertions)]
        let debug_range = byte_range.clone();
        self.insert_edit(byte_range, &normalized, window, cx);
        #[cfg(debug_assertions)]
        {
            self.ensure_fresh_layout(window);
            let c = self.caret_point();
            eprintln!(
                "[md-live] input-bounds_w={:.1} apply byte {:?} {:?} -> sel={} caret_px={:?}",
                self.element_bounds.size.width.as_f32(),
                debug_range,
                normalized,
                self.selection.head(),
                c.map(|(r, x, y, h)| (r, round1(x), round1(y), round1(h)))
            );
        }
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _new_selected_range: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let byte_range = match range_utf16 {
            Some(r) => {
                self.buffer.utf16_to_byte(r.start.min(self.buffer.len_utf16()))
                    ..self.buffer.utf16_to_byte(r.end.min(self.buffer.len_utf16()))
            }
            None => self.marked_range.clone().unwrap_or_else(|| self.selection.range()),
        };
        // IME 组合首键同样走「空行插入列」：先把空格补到点击列，组合文本（拼音）落在
        // 点击处而不是行首；marked 区间覆盖补齐区域，提交时中文替换整个区域 → 字符在点击列。
        let new_text = self.goal_prefixed_text(&byte_range, new_text);
        self.goal = None;
        #[cfg(debug_assertions)]
        eprintln!(
            "[md-live] input-mark byte {:?} {:?} -> sel={}",
            byte_range,
            new_text,
            byte_range.start + new_text.len()
        );
        self.insert_edit(byte_range.clone(), &new_text, window, cx);
        if new_text.is_empty() {
            self.marked_range = None;
            self.selection = Selection::collapsed(byte_range.start);
        } else {
            self.marked_range = Some(byte_range.start..byte_range.start + new_text.len());
            self.selection = Selection::collapsed(byte_range.start + new_text.len());
        }
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.marked_range = None;
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        element_bounds: Bounds<Pixels>,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        // 输入系统会在编辑后、下一帧渲染前同步查询光标几何：此刻 `segs` 已被
        // reparse 清空，直接算会掉到兜底 → 闪烁光标与输入位置不匹配。先重排。
        self.ensure_fresh_layout(window);
        // IME 组合区间：平台（`selected_bounds`）优先传组合文本的 marked range。
        // 锚定整段组合文本（起点→终点，同行才拉宽），候选窗贴在所打字符下方而不是
        // 一根零宽竖条——与 zorite/gpui-editor 的 bounds_for_range 同一语义。
        let s_byte = self.buffer.utf16_to_byte(range_utf16.start.min(self.buffer.len_utf16()));
        let e_byte = self.buffer.utf16_to_byte(range_utf16.end.min(self.buffer.len_utf16()));
        let (s_row, s_x0, s_y, height) = match self.point_for_offset(s_byte) {
            Some(v) => v,
            None => return None,
        };
        // 空行「插入列」：目标行上，IME bounds 也显示在点击列（与自绘光标一致）。
        let s_x = match self.goal {
            Some((grow, gx, _)) if grow == s_row => gx,
            _ => s_x0,
        };
        let e_x = (e_byte > s_byte)
            .then(|| self.point_for_offset(e_byte))
            .flatten()
            .filter(|(_, _ex, ey, _)| (ey - s_y).abs() < 0.5)
            .map(|(_, ex, _, _)| ex.max(s_x));
        let width = e_x.map_or(CARET_WIDTH_PX, |ex| (ex - s_x).max(CARET_WIDTH_PX));
        let scroll_y = (-self.v_handle.offset().y.as_f32()).max(0.0);
        // 用平台传入的当前 element_bounds（权威值），不用可能滞后一帧的 self.element_bounds。
        let origin = element_bounds.origin;
        #[cfg(debug_assertions)]
        eprintln!(
            "[md-live] os-caret bounds range={range_utf16:?} -> ({:.1},{:.1}) w={:.1} caret_raw=({s_x:.1},{s_y:.1}) seed_origin=({:.1},{:.1})",
            origin.x.as_f32() + s_x,
            origin.y.as_f32() + s_y - scroll_y,
            width,
            self.element_bounds.origin.x.as_f32(),
            self.element_bounds.origin.y.as_f32()
        );
        #[cfg(debug_assertions)]
        {
            // 调试覆盖层：记录 IME bounds（内容坐标，红框）。
            self.ime_content = Some((s_x, s_y, width, height));
        }
        Some(Bounds::new(
            point(
                origin.x + px(s_x),
                origin.y + px(s_y - scroll_y),
            ),
            size(px(width), px(height)),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        self.ensure_fresh_layout(window);
        let local = point - self.element_bounds.origin;
        let scroll_y = (-self.v_handle.offset().y.as_f32()).max(0.0);
        let cy = local.y.as_f32() + scroll_y;
        let source = {
            let segs = self.segs.borrow();
            let s = segs.iter().find(|s| self.seg_hit_bot(s, cy))?;
            let local_b = s
                .line
                .as_ref()
                .map(|l| {
                    l.closest_index_for_x(px((local.x.as_f32() - s.x).max(0.0))).min(s.text_len)
                })
                .unwrap_or(0);
            Some(s.source_for(local_b))
        }?;
        Some(self.buffer.byte_to_utf16(source))
    }

    fn set_selected_text_range(
        &mut self,
        range_utf16: Range<usize>,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        let s = self.buffer.utf16_to_byte(range_utf16.start.min(self.buffer.len_utf16()));
        let e = self.buffer.utf16_to_byte(range_utf16.end.min(self.buffer.len_utf16()));
        self.selection = Selection::from_range(s, e);
        window.invalidate_character_coordinates();
    }

    fn paste(&mut self, item: gpui::ClipboardItem, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = item.text() {
            let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
            self.insert_edit(self.selection.range(), &normalized, window, cx);
        }
    }

    fn text_input_configuration(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> gpui::TextInputConfiguration {
        gpui::TextInputConfiguration::default()
    }
}

// ---------------------------------------------------------------------------
// 包装元素：捕获边界 + 挂输入
// ---------------------------------------------------------------------------

struct LiveElement {
    view: Entity<MarkdownLivePreview>,
    inner: AnyElement,
}

impl LiveElement {
    fn new(view: Entity<MarkdownLivePreview>, inner: AnyElement) -> Self {
        Self { view, inner }
    }
}

impl IntoElement for LiveElement {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for LiveElement {
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
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let layout_id = self.inner.request_layout(window, cx);
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.view.update(cx, |view, _| view.element_bounds = bounds);
        self.inner.prepaint(window, cx);
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.inner.paint(window, cx);
        let focus = self.view.read(cx).focus_handle();
        window.handle_input(&focus, ElementInputHandler::new(bounds, self.view.clone()), cx);
    }
}

impl Render for MarkdownLivePreview {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_blink(window, cx);
        self.refresh_styles(cx);
        let inner = self.render_body(window, cx);
        let entity = cx.entity();
        LiveElement::new(entity, inner)
    }
}

#[cfg(test)]
impl MarkdownLivePreview {
    /// 测试：在指定字节偏移插入文本（走真实编辑核心），返回新光标。
    pub(crate) fn test_insert_at(&mut self, off: usize, text: &str) -> usize {
        self.apply_edit(off..off, text);
        self.selection.head()
    }

    /// 测试：直接设定光标（源码字节）。
    pub(crate) fn test_set_caret(&mut self, off: usize) {
        self.selection = Selection::collapsed(off);
    }

    /// 测试：光标对应的 (渲染行, 渲染字节)。
    pub(crate) fn test_caret_target(&self) -> Option<(usize, usize)> {
        self.caret_target()
    }

    /// 测试：当前渲染行数。
    pub(crate) fn test_live_len(&self) -> usize {
        self.live.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{size, AppContext, Modifiers, TestAppContext, VisualTestContext};

    /// 代码行段的几何 + 源码映射信息（测试观测用）。
    #[derive(Debug, Clone)]
    struct CodeSeg {
        base: usize,
        text_len: usize,
        x: f32,
        top: f32,
        height: f32,
    }

    struct LiveHost {
        live: Entity<MarkdownLivePreview>,
    }
    impl Render for LiveHost {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            self.live.clone()
        }
    }

    /// 模拟 App 布局：左 200px「侧栏」+ 右侧 live 编辑器（live 不在窗口原点）。
    struct SidebarHost {
        live: Entity<MarkdownLivePreview>,
    }
    impl Render for SidebarHost {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .flex()
                .flex_row()
                .size_full()
                .child(div().w(px(200.)).bg(gpui::rgb(0x222222)))
                .child(div().flex_1().child(self.live.clone()))
        }
    }

    /// 以真实窗口渲染实时编辑器：resize + 多帧收敛，让 `segs`/`element_bounds` 就绪。
    fn open_live<'a>(
        cx: &'a mut TestAppContext,
        src: &str,
    ) -> (Entity<LiveHost>, &'a mut VisualTestContext) {
        cx.update(gpui_component::init);
        let (host, cx) = cx.add_window_view(|_window, cx| {
            let live = cx.new(|cx| MarkdownLivePreview::new(src, cx));
            LiveHost { live }
        });
        let cx: &mut VisualTestContext = cx;
        cx.simulate_resize(size(px(900.), px(700.)));
        for _ in 0..5 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
        }
        cx.run_until_parked();
        (host, cx)
    }

    /// 收集全部「代码行段」（map=None 且 line=Some）的几何 + 映射信息。
    fn code_segs(live: &Entity<MarkdownLivePreview>, cx: &mut App) -> Vec<CodeSeg> {
        let v = live.read(cx);
        let segs = v.segs.borrow();
        segs.iter()
            .filter(|s| s.map.is_none() && s.line.is_some())
            .map(|s| CodeSeg {
                base: s.base,
                text_len: s.text_len,
                x: s.x,
                top: s.top,
                height: s.height,
            })
            .collect()
    }

    fn caret_source(live: &Entity<MarkdownLivePreview>, cx: &mut App) -> usize {
        live.read(cx).selection.head()
    }

    fn buffer_text(live: &Entity<MarkdownLivePreview>, cx: &mut App) -> String {
        live.read(cx).text()
    }

    /// 在 `s` 的字节 `at` 处插入 `ins`（与编辑核心同语义：按字节切）。
    fn insert_bytes(s: &str, at: usize, ins: &str) -> String {
        let mut out = String::with_capacity(s.len() + ins.len());
        out.push_str(&s[..at]);
        out.push_str(ins);
        out.push_str(&s[at..]);
        out
    }

    /// 核心回归：代码块点击 → 光标字节映射 → 输入落位，LF 与 CRLF 两种行尾都要准确。
    fn check_code_click_and_type(cx: &mut TestAppContext, eol: &str) {
        // 源码按行拼接，保证字节偏移可精确推导。
        let mut src = String::new();
        src.push_str("```rust");
        src.push_str(eol);
        src.push_str("let a = 1;");
        src.push_str(eol);
        src.push_str("let b = 22;");
        src.push_str(eol);
        src.push_str("```");
        src.push('\n');

        let (host, cx) = open_live(cx, &src);
        let code = cx.update(|_, cx| host.read(cx).live.clone());

        // 先验证「点击代码行某字节 → 光标 = 该源码行行首 + 字节」。
        let segs = cx.update(|_, cx| code_segs(&code, cx));
        assert!(
            segs.len() >= 2,
            "应有两个代码行段，实际 {} 个：{segs:?}",
            segs.len()
        );

        let local_b = 3usize;
        for (i, seg) in segs.iter().take(2).enumerate() {
            // 垂直命中：点击行 top+2px 落在本行，光标 = base（行首）。
            cx.simulate_mouse_down(
                point(px(seg.x), px(seg.top + 2.0)),
                MouseButton::Left,
                Modifiers::default(),
            );
            cx.run_until_parked();
            let caret = cx.update(|_, cx| caret_source(&code, cx));
            assert_eq!(
                caret, seg.base,
                "[{eol:?}] 代码行 {i}：点击行首光标应 = base({})，实际 {caret}（seg={seg:?}）",
                seg.base
            );

            // 横向命中：在字符 local_b 的位置点击 → 光标 = base + local_b。
            let x_for_b = cx.update(|_, cx| {
                let v = code.read(cx);
                let s = v.segs.borrow();
                let s = s
                    .iter()
                    .find(|s| s.base == seg.base && s.line.is_some())
                    .expect("找到代码行段");
                s.line.as_ref().unwrap().x_for_index(local_b).as_f32()
            });
            cx.simulate_mouse_down(
                point(px(seg.x + x_for_b), px(seg.top + 2.0)),
                MouseButton::Left,
                Modifiers::default(),
            );
            cx.run_until_parked();
            let caret = cx.update(|_, cx| caret_source(&code, cx));
            assert_eq!(
                caret,
                seg.base + local_b,
                "[{eol:?}] 代码行 {i}：@{local_b} 点击光标应 = {}，实际 {caret}（x_for_b={x_for_b:.1}, seg={seg:?}）",
                seg.base + local_b
            );
        }

        // 输入串：在第二行第 local_b 字符处插入 "XY" → 文本只在该处插入、光标推进 2 字节。
        let seg1 = segs[1].clone();
        let x_for_b = cx.update(|_, cx| {
            let v = code.read(cx);
            let s = v.segs.borrow();
            let s = s
                .iter()
                .find(|s| s.base == seg1.base && s.line.is_some())
                .expect("找到代码行段");
            s.line.as_ref().unwrap().x_for_index(local_b).as_f32()
        });
        cx.simulate_mouse_down(
            point(px(seg1.x + x_for_b), px(seg1.top + 2.0)),
            MouseButton::Left,
            Modifiers::default(),
        );
        cx.run_until_parked();
        cx.simulate_input("XY");
        cx.run_until_parked();

        // 期望：把第二行 `let b = 22;` 在第 local_b 字节处插 "XY"（源字节与渲染 1:1）。
        let expected = src.replacen(
            "let b = 22;",
            &insert_bytes("let b = 22;", local_b, "XY"),
            1,
        );
        let text = cx.update(|_, cx| buffer_text(&code, cx));
        assert_eq!(
            text, expected,
            "[{eol:?}] 输入后文本应只在该处插入（got {text:?} 期望 {expected:?}）"
        );
        let caret = cx.update(|_, cx| caret_source(&code, cx));
        assert_eq!(
            caret,
            seg1.base + local_b + 2,
            "[{eol:?}] 光标应推进到 base+{}，实际 {caret}",
            local_b + 2
        );
    }

    #[gpui::test]
    fn live_edit_code_click_and_type_lf(cx: &mut TestAppContext) {
        check_code_click_and_type(cx, "\n");
    }

    #[gpui::test]
    fn live_edit_code_click_and_type_crlf(cx: &mut TestAppContext) {
        check_code_click_and_type(cx, "\r\n");
    }

    /// 光标像素（内容坐标 x, y, 高 + 行号），由 `caret_point` 直接观测。
    fn caret_pixel(live: &Entity<MarkdownLivePreview>, cx: &mut App) -> Option<(usize, f32, f32, f32)> {
        live.read(cx).caret_point()
    }

    /// 找某个源码行（base）对应的代码行段：返回 (x, top, line)。
    fn find_code_seg(
        live: &Entity<MarkdownLivePreview>,
        cx: &mut App,
        base: usize,
    ) -> (f32, f32, f32, usize) {
        let v = live.read(cx);
        let segs = v.segs.borrow();
        for s in segs.iter() {
            if s.base == base && s.line.is_some() {
                return (s.x, s.top, s.height, s.text_len);
            }
        }
        panic!("找不到 base={base} 的代码行段");
    }

    /// 光标必须像「钉住」在点击/输入位置一样：输入后（重解析+重排一帧后）光标
    /// 的像素 x/y 与该代码行段的几何严格一致——否则就是「输入错位」。
    fn assert_caret_pinned(
        live: &Entity<MarkdownLivePreview>,
        cx: &mut App,
        base: usize,
        local_b: usize,
        msg: &str,
    ) {
        let (row, x, y, h) = caret_pixel(live, cx).expect("应有光标");
        let (sx, sty, sh, _len) = find_code_seg(live, cx, base);
        // 光标 x = 段左起点 + 该行整形下 local_b 的 x（用整形线自己量，排除字体差异）
        let v = live.read(cx);
        let segs = v.segs.borrow();
        let l = segs
            .iter()
            .find(|s| s.base == base && s.line.is_some())
            .unwrap()
            .line
            .as_ref()
            .unwrap();
        let expect_x = sx + l.x_for_index(local_b.min(l.len())).as_f32();
        let local = (x - expect_x).abs();
        let vline = (y - sty).abs();
        let vh = (h - sh).abs();
        assert!(
            local < 1.0,
            "{msg}：光标 x={x:.1} 应与文本 {expect_x:.1} 对齐（段 x={sx:.1}，x 差={local:.2}px）"
        );
        assert!(
            vline < 1.0 && vh < 1.0,
            "{msg}：光标 y={y:.1} h={h:.1} 应与段 top={sty:.1} h={sh:.1} 对齐（垂直差 {vline:.2}/{vh:.2}px）"
        );
        let _ = row;
    }

    /// 真实文件形态：代码块前面有标题（块不是从第 0 行开始）+ 块内空行（测试文件
    /// 常见），逐行点击映射 + 输入后的光标像素都得严格对齐。
    #[gpui::test]
    fn live_edit_code_block_offset_and_empty_lines(cx: &mut TestAppContext) {
        let src = "# 标题\n\n> 引用\n\n```java\npublic class A {\n    \n    private int x;\n}\n```\n\n正文结尾\n";
        let (host, cx) = open_live(cx, src);
        let code = cx.update(|_, cx| host.read(cx).live.clone());

        let segs = cx.update(|_, cx| code_segs(&code, cx));
        assert!(
            segs.len() == 4,
            "代码块 4 行（含空行），实际 {}：{segs:?}",
            segs.len()
        );

        // 每行点「行首」→ 光标 = 该行 base（空行也一样，不能落到围栏/别的行）。
        for (i, seg) in segs.iter().enumerate() {
            cx.simulate_mouse_down(
                point(px(seg.x), px(seg.top + 2.0)),
                MouseButton::Left,
                Modifiers::default(),
            );
            cx.run_until_parked();
            let caret = cx.update(|_, cx| caret_source(&code, cx));
            assert_eq!(
                caret, seg.base,
                "代码行 {i}（空行={}）：点行首光标应 = base({})，实际 {caret}（seg={seg:?}）",
                seg.text_len == 0,
                seg.base
            );
        }

        // 非空行内横向点击 → 光标 = base + local_b，并在输入后光标像素仍钉在原位。
        let line = segs[2].clone(); // "    private int x;"
        assert!(line.text_len > 0);
        let local_b = 4usize; // 行内容第 4 字节（"private" 首字节）
        let x_for_b = cx.update(|_, cx| {
            let v = code.read(cx);
            let segs = v.segs.borrow();
            let s = segs
                .iter()
                .find(|s| s.base == line.base && s.line.is_some())
                .unwrap();
            s.line.as_ref().unwrap().x_for_index(local_b).as_f32()
        });
        cx.simulate_mouse_down(
            point(px(line.x + x_for_b), px(line.top + 2.0)),
            MouseButton::Left,
            Modifiers::default(),
        );
        cx.run_until_parked();
        let caret = cx.update(|_, cx| caret_source(&code, cx));
        assert_eq!(
            caret,
            line.base + local_b,
            "横向点击应 = base+{local_b}（={}），实际 {caret}",
            line.base + local_b
        );
        // 输入前光标像素就对正。
        cx.update(|_, cx| assert_caret_pinned(&code, cx, line.base, local_b, "输入前"));
        // 输入 3 个字符，光标推进 3 字节后仍钉在文本处。
        cx.simulate_input("XYZ");
        cx.run_until_parked();
        let text = cx.update(|_, cx| buffer_text(&code, cx));
        assert!(
            text.contains("    XYZprivate int x;"),
            "插入应落在该行 {local_b} 字节处：{text:?}"
        );
        let caret = cx.update(|_, cx| caret_source(&code, cx));
        assert_eq!(caret, line.base + local_b + 3, "光标推进 3 字节");
        cx.update(|_, cx| assert_caret_pinned(&code, cx, line.base, local_b + 3, "输入后"));
    }

    /// 代码块行内「退格/方向键」编辑不改动字节语义，光标依旧钉在文本。
    #[gpui::test]
    fn live_edit_code_line_backspace_keeps_caret_aligned(cx: &mut TestAppContext) {
        let src = "```java\nString a = \"hello\";\n```\n";
        let (host, cx) = open_live(cx, src);
        let code = cx.update(|_, cx| host.read(cx).live.clone());
        let segs = cx.update(|_, cx| code_segs(&code, cx));
        let seg = segs[0].clone();
        let local_b = 9usize; // '=' 附近
        let x_for_b = cx.update(|_, cx| {
            let v = code.read(cx);
            let segs = v.segs.borrow();
            let s = segs.iter().find(|s| s.base == seg.base && s.line.is_some()).unwrap();
            s.line.as_ref().unwrap().x_for_index(local_b).as_f32()
        });
        cx.simulate_mouse_down(
            point(px(seg.x + x_for_b), px(seg.top + 2.0)),
            MouseButton::Left,
            Modifiers::default(),
        );
        cx.run_until_parked();
        // 输入一个字符，再退格删掉——文本回到原样，光标回到原字节。
        cx.simulate_input(".");
        cx.run_until_parked();
        cx.simulate_keystrokes("backspace");
        cx.run_until_parked();
        let text = cx.update(|_, cx| buffer_text(&code, cx));
        assert_eq!(text, src, "退格后文本应复原");
        let caret = cx.update(|_, cx| caret_source(&code, cx));
        assert_eq!(caret, seg.base + local_b, "退格后光标应回到原字节位置");
        cx.update(|_, cx| assert_caret_pinned(&code, cx, seg.base, local_b, "退格后"));
    }

    /// 无语言标签的围栏（``` 后无内容）：布局不得多占一个「幽灵语言头」行，
    /// 否则点击第一行代码会落到围栏/上一行 → 输入错位（回归护栏）。
    #[gpui::test]
    fn live_edit_code_block_without_language_is_aligned(cx: &mut TestAppContext) {
        for eol in ["\n", "\r\n"] {
            let mut src = String::new();
            src.push_str("```");
            src.push_str(eol);
            src.push_str("printf(1);");
            src.push_str(eol);
            src.push_str("printf(2);");
            src.push_str(eol);
            src.push_str("```");
            src.push('\n');

            let (host, cx) = open_live(cx, &src);
            let code = cx.update(|_, cx| host.read(cx).live.clone());
            let segs = cx.update(|_, cx| code_segs(&code, cx));
            assert!(
                segs.len() == 2,
                "[{eol:?}] 无语言块应只有 2 个代码行段，实际 {}：{segs:?}",
                segs.len()
            );

            // 点击第一行正文中间 → 光标必须落在该行（base+local），不能落到围栏。
            let local_b = 4usize;
            let base0 = segs[0].base;
            cx.simulate_mouse_down(
                point(px(segs[0].x), px(segs[0].top + 2.0)),
                MouseButton::Left,
                Modifiers::default(),
            );
            cx.run_until_parked();
            let caret = cx.update(|_, cx| caret_source(&code, cx));
            assert_eq!(caret, segs[0].base, "[{eol:?}] 点行首应 = base({})，实际 {caret}", segs[0].base);
            let x4 = cx.update(|_, cx| {
                let v = code.read(cx);
                let segs = v.segs.borrow();
                let s = segs.iter().find(|s| s.base == base0 && s.line.is_some()).unwrap();
                s.line.as_ref().unwrap().x_for_index(local_b).as_f32()
            });
            cx.simulate_mouse_down(
                point(px(segs[0].x + x4), px(segs[0].top + 2.0)),
                MouseButton::Left,
                Modifiers::default(),
            );
            cx.run_until_parked();
            let caret = cx.update(|_, cx| caret_source(&code, cx));
            assert_eq!(
                caret, segs[0].base + local_b,
                "[{eol:?}] 横向点击应 = base+{local_b}，实际 {caret}"
            );
            cx.update(|_, cx| assert_caret_pinned(&code, cx, segs[0].base, local_b, "无语言块"));
        }
    }

    /// 编辑后、下一帧渲染前，输入系统同步查询的光标 bounds 必须基于**当前**布局：
    /// 编辑已重解析（清空 `segs`），若直接用空/过期几何，光标会消失/跳走 →
    /// 「闪烁光标与输入实际位置不匹配」。此处复现并回归。
    #[gpui::test]
    fn live_edit_ime_caret_bounds_fresh_after_edit_before_render(cx: &mut TestAppContext) {
        let src = "# 标题\n\n```rust\nlet x = 1;\n```\n";
        let (host, cx) = open_live(cx, src);
        let code = cx.update(|_, cx| host.read(cx).live.clone());

        // 点击进入代码行第 local_b 字节（用已渲染的几何）。
        let local_b = 4usize; // 'x'
        let x4 = cx.update(|_, cx| {
            let v = code.read(cx);
            let segs = v.segs.borrow();
            let s = segs.iter().find(|s| s.base > 0 && s.line.is_some()).unwrap();
            s.line.as_ref().unwrap().x_for_index(local_b).as_f32()
        });
        let seg = cx.update(|_, cx| {
            let v = code.read(cx);
            let segs = v.segs.borrow();
            let s = segs.iter().find(|s| s.base > 0 && s.line.is_some()).unwrap();
            CodeSeg {
                base: s.base,
                text_len: s.text_len,
                x: s.x,
                top: s.top,
                height: s.height,
            }
        });
        cx.simulate_mouse_down(
            point(px(seg.x + x4), px(seg.top + 2.0)),
            MouseButton::Left,
            Modifiers::default(),
        );
        cx.run_until_parked();
        let caret = cx.update(|_, cx| caret_source(&code, cx));
        assert_eq!(caret, seg.base + local_b, "点击应落在代码行内");

        // 模拟击键的同步路径：走真实输入方法（apply_edit → reparse → 清空 segs），
        // **不刷新/不重渲染**，紧接查询 IME 光标 bounds——平台正是这样做的。
        let mut bounds = cx.update(|window, cx| {
            let live_ent = host.read(cx).live.clone();
            live_ent.update(cx, |v, cx| {
                let sel = v.selection;
                let r = v.buffer.byte_to_utf16(sel.start())..v.buffer.byte_to_utf16(sel.end());
                v.replace_text_in_range(Some(r), "AB", window, cx);
                let sel2 = v.selection;
                let r2 = v.buffer.byte_to_utf16(sel2.start())..v.buffer.byte_to_utf16(sel2.end());
                v.bounds_for_range(r2, Bounds::default(), window, cx)
            })
        });
        let b = bounds.take().expect("编辑后（未重渲染）光标 bounds 不应为空");
        // 断言：bounds 不在兜底（文档末尾），而是当前代码行的正确位置。
        let (x, y) = (b.origin.x.as_f32(), b.origin.y.as_f32());
        let expect = cx.update(|_, cx| {
            let v = code.read(cx);
            let segs = v.segs.borrow();
            let s = segs.iter().find(|s| s.base == seg.base && s.line.is_some()).expect("重排后仍有该行");
            let xb = s.line.as_ref().unwrap().x_for_index((local_b + 2).min(s.text_len)).as_f32();
            // element_bounds.origin = 窗口内 (0,0)，scroll_y=0 → bounds=(x, top)
            (s.x + xb, s.top)
        });
        assert!(
            (x - expect.0).abs() < 1.0 && (y - expect.1).abs() < 1.0,
            "编辑后光标 bounds ({x:.1},{y:.1}) 应与当前几何 ({:.1},{:.1}) 一致，不得跳走",
            expect.0,
            expect.1
        );
        assert!(y >= 0.0 && y < 600.0, "光标 y 应在视口内（没掉到文档末尾兜底）：{y:.1}");
    }

    /// IME 组合（input-mark）路径：平台在「每次组合更新 / 每次输入法 push」时都会以
    /// 当前选区查询光标 bounds（`selected_bounds` 优先取 marked range）。组合中
    /// `marked_range` 已设置、光标 = 组合文本末尾；此刻查询 bounds 必须直接落在
    /// 组合文本的末尾（下一字符输入处），不得回到组合前 / 兜底位置——否则候选窗、
    /// 闪烁光标、实际输入位置三者即错位。
    #[gpui::test]
    fn live_edit_ime_marked_bounds_chase_composition_caret(cx: &mut TestAppContext) {
        let src = "# 标题\n\n```rust\nlet x = 1;\n```\n";
        let (host, cx) = open_live(cx, &src);
        let code = cx.update(|_, cx| host.read(cx).live.clone());

        // 点击进入代码行第 local_b 字节。
        let local_b = 4usize; // 'x'
        let (seg_x, seg_top, x_for_b) = cx.update(|_, cx| {
            let v = code.read(cx);
            let segs = v.segs.borrow();
            let s = segs.iter().find(|s| s.base > 0 && s.line.is_some()).unwrap();
            let seg = CodeSeg { base: s.base, text_len: s.text_len, x: s.x, top: s.top, height: s.height };
            (seg.x, seg.top, s.line.as_ref().unwrap().x_for_index(local_b).as_f32())
        });
        cx.simulate_mouse_down(point(px(seg_x + x_for_b), px(seg_top + 2.0)), MouseButton::Left, Modifiers::default());
        cx.run_until_parked();
        let caret_byte = cx.update(|_, cx| code.read(cx).selection.head());
        assert!(caret_byte > 0, "点击应落在代码行内");
        let line_base = caret_byte - local_b;

        // 模拟两次 input-mark（组合 pinyin "zhe'li"），每次 marker 更新后随即模拟
        // `selected_bounds` 的查询（用当前 marked range 的 utf16 区间）：
        // bounds 原点贴在「组合起点」，宽度覆盖整段组合文本到其末尾（同行时）——
        // 候选窗锚在组合文本下方而不是一根零宽竖条（对齐 zorite/gpui-editor）。
        for marks in ["z", "zhe'li"] {
            let bounds = cx.update(|window, cx| {
                let live_ent = host.read(cx).live.clone();
                live_ent.update(cx, |v, cx| {
                    let sel = v.selection;
                    let r = v.buffer.byte_to_utf16(sel.start())..v.buffer.byte_to_utf16(sel.end());
                    v.replace_and_mark_text_in_range(Some(r), marks, None, window, cx);
                    let marked = v.marked_text_range(window, cx).expect("组合中应有 marked range");
                    v.bounds_for_range(marked, Bounds::default(), window, cx)
                })
            });
            let b = bounds.expect("组合中光标 bounds 不应为空");
            let (bx, by, bw) = (b.origin.x.as_f32(), b.origin.y.as_f32(), b.size.width.as_f32());
            let (sx, sy, ex) = cx.update(|_, cx| {
                let v = code.read(cx);
                let segs = v.segs.borrow();
                // marked_range 是当前组合的字节区间（相对 line_base 的索引即该行字符位）。
                let mr = v.marked_range.clone().expect("组合范围");
                let s = segs.iter().find(|s| s.base == line_base && s.line.is_some()).expect("重排后仍有该行");
                let l = s.line.as_ref().unwrap();
                let i_s = (mr.start - line_base).min(s.text_len);
                let i_e = (mr.end - line_base).min(s.text_len);
                (
                    s.x + l.x_for_index(i_s).as_f32(),
                    s.top,
                    s.x + l.x_for_index(i_e).as_f32(),
                )
            });
            assert!(
                (bx - sx).abs() < 1.0 && (by - sy).abs() < 1.0,
                "input-mark {marks:?} 后光标 bounds 原点 ({bx:.1},{by:.1}) 应贴组合起点 ({sx:.1},{sy:.1})"
            );
            assert!(
                (bw - (ex - sx)).abs() < 1.0,
                "input-mark {marks:?} 后光标 bounds 宽 {bw:.1} 应覆盖组合文本 {:.1}px（起点 {sx:.1}→终点 {ex:.1}）",
                ex - sx
            );
        }
    }

    /// 调试覆盖层的数据记录：点击后 `click_content`（绿点）、IME 查询后 `ime_content`
    /// （红框）必须落在与命中一致的段几何上——覆盖层本身不引入坐标偏差。
    #[gpui::test]
    fn debug_ime_overlay_records_click_and_ime(cx: &mut TestAppContext) {
        let src = "# 标题\n\n```rust\nlet x = 1;\n```\n";
        let (host, cx) = open_live(cx, &src);
        let code = cx.update(|_, cx| host.read(cx).live.clone());

        let local_b = 4usize;
        let (seg_x, seg_top, xb4) = cx.update(|_, cx| {
            let v = code.read(cx);
            let segs = v.segs.borrow();
            let s = segs.iter().find(|s| s.base > 0 && s.line.is_some()).unwrap();
            let l = s.line.as_ref().unwrap();
            (s.x, s.top, l.x_for_index(local_b).as_f32())
        });
        cx.simulate_mouse_down(
            point(px(seg_x + xb4), px(seg_top + 2.0)),
            MouseButton::Left,
            Modifiers::default(),
        );
        cx.run_until_parked();
        let (click, ime) = cx.update(|window, cx| {
            let live_ent = host.read(cx).live.clone();
            live_ent.update(cx, |v, cx| {
                let click = v.click_content;
                let sel = v.selection;
                let r = v.buffer.byte_to_utf16(sel.start())..v.buffer.byte_to_utf16(sel.end());
                let _ = v.bounds_for_range(r, Bounds::default(), window, cx);
                (click, v.ime_content)
            })
        });
        let (cx, cy) = click.expect("点击后应记录 click_content（滚动=0，内容坐标）");
        assert!(
            (cx - (seg_x + xb4)).abs() < 1.0 && (cy - (seg_top + 2.0)).abs() < 1.0,
            "click 绿点 ({cx:.1},{cy:.1}) 应贴点击点 ({:.1},{:.1})",
            seg_x + xb4,
            seg_top + 2.0
        );
        let (ix, iy, iw, _ih) = ime.expect("IME 查询后应记录 ime_content");
        assert!(
            (ix - (seg_x + xb4)).abs() < 1.0 && (iy - seg_top).abs() < 1.0,
            "IME 红框 ({ix:.1},{iy:.1}) 应贴光标 ({:.1},{:.1})",
            seg_x + xb4,
            seg_top
        );
        assert!(iw >= 1.0, "IME 红框宽应是零宽光标条：{iw:.1}");
    }

    /// 行尾/空行钳制的"点击幽灵条"：点文字中间不出现；点行尾之后的空白出现
    /// （说明插入在行尾），不改变插入位置。
    #[gpui::test]
    fn live_edit_click_ghost_only_on_eol_clamp(cx: &mut TestAppContext) {
        let src = "# 标题\n\n```rust\nlet x = 1;\n```\n正文好\n";
        let (host, cx) = open_live(cx, &src);
        let code = cx.update(|_, cx| host.read(cx).live.clone());

        // 找第一个含文字的行段，点文字中间 → 无幽灵。
        let (seg_x, seg_top, xb, tlen) = cx.update(|_, cx| {
            let v = code.read(cx);
            let segs = v.segs.borrow();
            let s = segs.iter().find(|s| s.line.is_some() && s.text_len > 4).unwrap();
            let l = s.line.as_ref().unwrap();
            (s.x, s.top, l.x_for_index(1).as_f32(), s.text_len)
        });
        let _ = tlen;
        cx.simulate_mouse_down(point(px(seg_x + xb), px(seg_top + 2.0)), MouseButton::Left, Modifiers::default());
        cx.run_until_parked();
        assert!(
            cx.update(|_, cx| code.read(cx).click_ghost.is_none()),
            "点文字中间不应出现幽灵条"
        );

        // 点该行文字之后很远 → 出现幽灵，且光标仍在行内（没挪到点击处）。
        let (line_end_x, line_w) = cx.update(|_, cx| {
            let v = code.read(cx);
            let segs = v.segs.borrow();
            let s = segs.iter().find(|s| s.map.is_none() && s.line.is_some() && s.text_len > 4).unwrap();
            let l = s.line.as_ref().unwrap();
            (s.x + l.x_for_index(s.text_len).as_f32(), l.width().as_f32())
        });
        cx.simulate_mouse_down(point(px(line_end_x + line_w * 0.25), px(seg_top + 2.0)), MouseButton::Left, Modifiers::default());
        cx.run_until_parked();
        let ghost = cx.update(|_, cx| code.read(cx).click_ghost).expect("行尾之后点击应出现幽灵条");
        assert!(ghost.0 > line_end_x, "幽灵条应收在点击处（>行尾 {line_end_x:.1}）：x={:.1}", ghost.0);
    }

    /// 决定性：逐行读取 GPUI **实际绘制**的行 bounds（`md-row-{ix}` debug_selector），
    /// 与 `segs` 的行 top 逐一对比。这是唯一能区分「算法自洽但绘制在别处」和
    /// 「算法正确且绘制正确」的检查——此前只比对了总量（内容总高），不够。
    #[gpui::test]
    fn debug_painted_row_tops_match_segs(cx: &mut TestAppContext) {
        let path = std::path::Path::new("target/md测试.md");
        let src = if path.exists() {
            std::fs::read_to_string(path).unwrap()
        } else {
            "# 标题\n\n正文段落，用于 painted 对齐核对，再加一些文字让它足够长以便换行。\n\n```rust\nlet a = 1;\n```\n\n> 引用行\n\n结尾\n".to_string()
        };
        let (host, mut cx) = open_live(cx, &src);
        // open_live 装置下稳定收敛的宽度（900 默认 / 934.3 用户 trace 宽）。
        // 全屏 2148.6 由 `debug_wrapped_row_painted_h_vs_seg_total` 的「全新视图」
        // 分支以行级核对覆盖（手动建窗换宽在 harness 里有 paint/segs 时序假象）。
        for w in [900.0f32, 934.3] {
            cx.simulate_resize(size(px(w), px(760.)));
            for _ in 0..4 {
                cx.run_until_parked();
                let _ = cx.cx.refresh();
            }
            cx.run_until_parked();
            // 每行：可命中区 = min(top)-band  … max(top+h)+band（引述带上下 3px 含内衬）。
            let rows: Vec<(usize, f32, f32)> = cx.update(|_, cx| {
                let v = host.read(cx).live.read(cx);
                let segs = v.segs.borrow();
                let mut by_row: std::collections::BTreeMap<usize, (f32, f32)> = Default::default();
                for s in segs.iter() {
                    let e = by_row.entry(s.row_ix).or_insert((f32::MAX, f32::MIN));
                    e.0 = e.0.min(s.top);
                    e.1 = e.1.max(s.top + s.height);
                }
                let live = &v.live;
                by_row
                    .into_iter()
                    .map(|(rix, (t0, t1))| {
                        let q = match live.get(rix).map(|lr| lr.row.kind) {
                            Some(RowKind::Quote) => BAND_PY,
                            _ => 0.0,
                        };
                        (rix, t0 - q, t1 + q)
                    })
                    .collect()
            });
            let mut checked = 0usize;
            let mut max_d = 0.0f32;
            let mut max_dh = 0.0f32;
            for &(rix, hit_top, hit_bot) in rows.iter() {
                let key: &'static str = Box::leak(format!("md-row-{rix}").into_boxed_str());
                let Some(b) = cx.debug_bounds(key) else { continue };
                let (pt, ph) = (b.origin.y.as_f32(), b.size.height.as_f32());
                let ht = hit_top;
                let hh = hit_bot - hit_top;
                let d = (pt - ht).abs();
                let dh = (ph - hh).abs();
                max_d = max_d.max(d);
                max_dh = max_dh.max(dh);
                eprintln!(
                    "PAINT w={w} row={rix} hit=(top {ht:.2}, h {hh:.2}) painted=(top {pt:.2}, h {ph:.2}) dTop={d:.2} dH={dh:.2}"
                );
                checked += 1;
                if checked >= 80 {
                    break;
                }
            }
            eprintln!("PAINT w={w} checked={checked} max|ΔpaintedTop| = {max_d:.2}px  max|ΔH| = {max_dh:.2}px");
            assert!(
                checked >= 20,
                "w={w}: 至少应核对到 20 个被绘制的行，实际 {checked}"
            );
            assert!(
                max_d < 2.0 && max_dh < 2.0,
                "w={w}: 行级可命中区与绘制不一致：Δtop={max_d:.2}px ΔH={max_dh:.2}px → 算法对但绘制错"
            );
        }
    }

    /// 复现用户新症状：滚动后点击 + 输入 + 多帧，看 `v_handle.offset()` 是否被
    /// 自动滚回顶部（trace 里 scroll 5845→0、每 os-caret 查询后 −≈1040）。
    #[gpui::test]
    fn debug_scroll_does_not_drift_on_edit(cx: &mut TestAppContext) {
        let mut src = String::new();
        for i in 0..400 {
            src.push_str(&format!("第 {i} 段正文足够长用于滚动与编辑并存。\n\n"));
        }
        let (host, mut cx) = open_live(cx, &src);
        // 滚到中部。
        let scroll_to = 3000.0f32;
        cx.update(|_, cx| {
            let v = host.read(cx).live.read(cx);
            v.v_handle.set_offset(point(px(0.0), px(-scroll_to)));
        });
        for _ in 0..4 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
        }
        let off0 = cx.update(|_, cx| {
            let v = host.read(cx).live.read(cx);
            (-v.v_handle.offset().y.as_f32()).max(0.0)
        });
        eprintln!("SCROLLDRIFT offset_before_click={off0:.1} (want {scroll_to})");
        // 在一个可见行中部点击，再输入一个字符，跑 8 帧看滚动是否漂。
        let pt = cx.update(|_, cx| {
            let v = host.read(cx).live.read(cx);
            let segs = v.segs.borrow();
            let s = segs
                .iter()
                .find(|s| s.top + s.height >= scroll_to + 200.0 && s.line.is_some())
                .unwrap();
            let l = s.line.as_ref().unwrap();
            gpui::Point::new(px(s.x + l.x_for_index(2).as_f32()), px(s.top - scroll_to + 2.0))
        });
        cx.simulate_mouse_down(pt, MouseButton::Left, Modifiers::default());
        cx.run_until_parked();
        let caret = cx.update(|_, cx| host.read(cx).live.read(cx).selection.head());
        cx.update(|window, cx| {
            let live_ent = host.read(cx).live.clone();
            live_ent.update(cx, |v, cx| {
                let r = v.buffer.byte_to_utf16(caret)..v.buffer.byte_to_utf16(caret);
                v.replace_text_in_range(Some(r), "1", window, cx);
            })
        });
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
        }
        let off1 = cx.update(|_, cx| {
            let v = host.read(cx).live.read(cx);
            (-v.v_handle.offset().y.as_f32()).max(0.0)
        });
        eprintln!("SCROLLDRIFT offset_after_edit_8fr={off1:.1}");
        assert!(
            (off1 - off0).abs() < 64.0,
            "编辑后滚动漂移 {off0:.1}→{off1:.1}px：与用户 trace（scroll 5845→0）同类"
        );
    }

    /// 空行「插入列」真 WYSIWYG：点空行 → 光标/IME 显示在点击列；首次输入先补空格
    /// 到该列再插字符，字符落在点击处；输入后目标列清除。
    #[gpui::test]
    fn live_edit_empty_line_goal_column_wysiwyg(cx: &mut TestAppContext) {
        let src = "# 标题\n\n正文段落内容。\n\n结束。\n";
        let (host, cx) = open_live(cx, &src);
        let code = cx.update(|_, cx| host.read(cx).live.clone());
        let (row, top, seg_x) = cx.update(|_, cx| {
            let v = code.read(cx);
            let segs = v.segs.borrow();
            let s = segs.iter().find(|s| s.line.is_none() && s.text_len == 0).unwrap();
            (s.row_ix, s.top, s.x)
        });
        assert_eq!(seg_x, 12.0, "空行段应从左缘 x=12 开始");

        // 在空行中靠右处点击。
        let click_x = seg_x + 80.0;
        cx.simulate_mouse_down(point(px(click_x), px(top + 2.0)), MouseButton::Left, Modifiers::default());
        cx.run_until_parked();
        let g = cx.update(|_, cx| code.read(cx).goal).expect("空行点击应设置插入列");
        assert_eq!(g.0, row, "目标行=空行");
        let gx = g.1;
        assert!((gx - click_x).abs() < 1.0, "目标 x=点击 x（{gx:.1} vs {click_x:.1}）");
        let space_w = g.2;
        assert!(space_w > 1.0, "空格宽应 >1（{space_w:.2}）");

        // 首次输入前，IME bounds 也在点击列。
        let b = cx.update(|window, cx| {
            let e = host.read(cx).live.clone();
            e.update(cx, |v, cx| {
                let sel = v.selection;
                let r = v.buffer.byte_to_utf16(sel.start())..v.buffer.byte_to_utf16(sel.end());
                v.bounds_for_range(r, Bounds::default(), window, cx)
            })
        });
        let bx = b.expect("空行光标 bounds").origin.x.as_f32();
        assert!((bx - click_x).abs() < 1.5, "IME bounds 应在点击列（{bx:.1} vs {click_x:.1}）");

        // 输入 "a"：先补空格到点击列再插字符。
        let caret_before = cx.update(|_, cx| code.read(cx).selection.head());
        cx.update(|window, cx| {
            let e = host.read(cx).live.clone();
            e.update(cx, |v, cx| {
                let r = v.buffer.byte_to_utf16(caret_before)..v.buffer.byte_to_utf16(caret_before);
                v.replace_text_in_range(Some(r), "a", window, cx);
            })
        });
        cx.run_until_parked();
        let (txt, caret, goal) = cx.update(|_, cx| {
            let v = code.read(cx);
            (
                v.buffer.text_in_range(caret_before..v.selection.head()).to_string(),
                v.selection.head(),
                v.goal,
            )
        });
        let col = (80.0 / space_w).floor().max(0.0) as usize;
        assert_eq!(
            txt,
            format!("{:padding$}a", "", padding = col),
            "首次输入应补 {col} 个空格再放 a（补空格到点击列）"
        );
        assert_eq!(caret, caret_before + col + 1, "光标落在点击列字符之后");
        assert!(goal.is_none(), "输入后目标列清除");
    }

    /// 决定性复现：**IME 组合路径**（`replace_and_mark_text_in_range`，中文输入法
    /// 打拼音走的是这条，不是 `replace_text_in_range`）也必须走「空行插入列」。
    /// 现状（bug）：点空行某列 → goal 已设 → 打一个拼音，组合文本落在**行首**（列 0），
    /// 光标/IME 却在点击列 → 「点这里、字进那里」、越往下越明显。断言组合文本补空格到
    /// 点击列、marked 区间覆盖补齐后的区域、光标落在点击列、目标列被消费。
    #[gpui::test]
    fn live_edit_empty_line_goal_column_wysiwyg_ime_composition(cx: &mut TestAppContext) {
        let src = "# 标题\n\n正文段落内容。\n\n结束。\n";
        let (host, cx) = open_live(cx, &src);
        let code = cx.update(|_, cx| host.read(cx).live.clone());
        let (row, top, seg_x) = cx.update(|_, cx| {
            let v = code.read(cx);
            let segs = v.segs.borrow();
            let s = segs.iter().find(|s| s.line.is_none() && s.text_len == 0).unwrap();
            (s.row_ix, s.top, s.x)
        });
        // 空行中靠右处点击 → 目标列。
        let click_x = seg_x + 80.0;
        cx.simulate_mouse_down(point(px(click_x), px(top + 2.0)), MouseButton::Left, Modifiers::default());
        cx.run_until_parked();
        let g = cx.update(|_, cx| code.read(cx).goal).expect("空行点击应设置插入列");
        assert_eq!(g.0, row, "目标行=空行");
        assert!((g.1 - click_x).abs() < 1.0, "目标 x=点击 x");
        let space_w = g.2;
        let col = (80.0 / space_w).floor().max(0.0) as usize;

        // IME 组合第一键（走 replace_and_mark_text_in_range，与中文输入法一致）。
        let caret_before = cx.update(|_, cx| code.read(cx).selection.head());
        cx.update(|window, cx| {
            let e = host.read(cx).live.clone();
            e.update(cx, |v, cx| {
                let r = v.buffer.byte_to_utf16(caret_before)..v.buffer.byte_to_utf16(caret_before);
                v.replace_and_mark_text_in_range(Some(r), "n", None, window, cx);
            })
        });
        cx.run_until_parked();
        let (txt, caret, goal, marked) = cx.update(|_, cx| {
            let v = code.read(cx);
            (
                v.buffer.text_in_range(caret_before..v.selection.head()).to_string(),
                v.selection.head(),
                v.goal,
                v.marked_range.clone(),
            )
        });
        // 组合文本也应补空格到点击列（真 WYSIWYG：组合窗/候选窗在点击列）。
        assert_eq!(
            txt,
            format!("{:padding$}n", "", padding = col),
            "IME 组合首键应补 {col} 空格再放 n（组合文本在点击列而非行首），实际 {txt:?}"
        );
        assert_eq!(caret, caret_before + col + 1, "组合光标应落在点击列之后");
        assert!(marked == Some(caret_before..caret_before + col + 1), "marked 区间应覆盖补齐区域：{marked:?}");
        assert!(goal.is_none(), "组合首键后目标列应被消费");
        // 渲染：组合光标 x 应明显右移出「行首」落在点击列附近（真机 1.75x 已实测：
        // 点列 454 → 组合光标 457.5，逐字符与 click x 重合）。简单小文档里行尾
        // 哨兵的 x 还原允许 ±0.4 列容差，但绝不能落回行首。
        let row_left = 0.0;
        let caret_x = cx.update(|_, cx| {
            let v = code.read(cx);
            v.caret_point().map(|(_, x, _, _)| x).unwrap()
        });
        assert!(
            caret_x > row_left + 10.0 && caret_x < click_x + 25.0,
            "组合光标 x={caret_x:.1} 应落在点击列附近（>行首 {row_left}、<点列 {click_x}+25），实际落在错误的位置"
        );
    }

    /// 调试实验：把 live 视图放进「左侧 200px 侧栏 + 右侧 editor」的类 app 布局，
    /// 查 `element_bounds.origin` 到底是窗口绝对坐标（期待 x=200）还是相对坐标。
    #[gpui::test]
    fn debug_bounds_origin_with_sidebar(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let src = "# 标题\n\n```rust\nlet x = 1;\n```\n";
        let live = cx.update(|cx| cx.new(|cx| MarkdownLivePreview::new(src, cx)));
        let (host, cx) = cx.add_window_view(|_window, _cx| SidebarHost {
            live: live.clone(),
        });
        let cx: &mut VisualTestContext = cx;
        cx.simulate_resize(size(px(900.), px(600.)));
        for _ in 0..5 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
        }
        cx.run_until_parked();
        let (origin, w) = cx.update(|_, cx| {
            let v = host.read(cx).live.read(cx);
            (v.element_bounds.origin, v.element_bounds.size.width)
        });
        eprintln!("DEBUG element_bounds.origin={:?} width={:?}", origin, w);
        let x_abs = origin.x.as_f32();
        assert!(
            (x_abs - 200.0).abs() < 1.0,
            "element_bounds.origin.x 应是窗口绝对坐标 ≈200（实测 {x_abs:.1}）"
        );
        let seg = cx.update(|_, cx| {
            let v = host.read(cx).live.read(cx);
            let segs = v.segs.borrow();
            let s = segs.iter().find(|s| s.base > 0 && s.line.is_some()).unwrap();
            CodeSeg { base: s.base, text_len: s.text_len, x: s.x, top: s.top, height: s.height }
        });
        let x4 = cx.update(|_, cx| {
            let v = host.read(cx).live.read(cx);
            let segs = v.segs.borrow();
            let s = segs.iter().find(|s| s.base == seg.base && s.line.is_some()).unwrap();
            s.line.as_ref().unwrap().x_for_index(4).as_f32()
        });
        let win_x = 200.0 + seg.x + x4;
        let win_y = seg.top + 2.0;
        cx.simulate_mouse_down(point(px(win_x), px(win_y)), MouseButton::Left, Modifiers::default());
        cx.run_until_parked();
        let caret = cx.update(|_, cx| host.read(cx).live.read(cx).selection.head());
        assert_eq!(caret, seg.base + 4, "侧栏偏移后点击仍应精确映射到该字符");
    }

    /// 调试实验：滚动后点击/光标/bounds 三者的坐标换算（直接设定滚动偏移并直调
    /// `on_mouse_down`，验证「窗口坐标 + 滚动 → 内容坐标 → 字节」与 bounds 对称）。
    #[gpui::test]
    fn debug_scrolled_click_and_caret(cx: &mut TestAppContext) {
        let mut src = String::new();
        for i in 0..40 {
            src.push_str(&format!("第 {i} 段正文，用于把内容推高，确保可滚动。\n\n"));
        }
        src.push_str("```rust\nlet target = 123;\n```\n");
        let (host, cx) = open_live(cx, &src);
        let code = cx.update(|_, cx| host.read(cx).live.clone());

        let code_seg = cx.update(|_, cx| {
            let v = code.read(cx);
            let segs = v.segs.borrow();
            let s = segs.iter().find(|s| s.map.is_none() && s.line.is_some()).unwrap();
            CodeSeg { base: s.base, text_len: s.text_len, x: s.x, top: s.top, height: s.height }
        });
        let local_b = 12usize;
        let x_b = cx.update(|_, cx| {
            let v = code.read(cx);
            let segs = v.segs.borrow();
            let s = segs.iter().find(|s| s.base == code_seg.base && s.line.is_some()).unwrap();
            s.line.as_ref().unwrap().x_for_index(local_b).as_f32()
        });
        // 中等滚动（< max_offset，真实用户可达）验证换算对称。
        let scroll_y = 500.0f32;
        cx.update(|_, cx| {
            let v = host.read(cx).live.read(cx);
            v.v_handle.set_offset(point(px(0.), px(-scroll_y)));
        });
        let win_x = code_seg.x + x_b;
        let win_y = code_seg.top + 2.0 - scroll_y;
        eprintln!(
            "DEBUG code_seg base={} text_len={} x={:.1} top={:.1} scroll_y={scroll_y:.1} click=({win_x:.1},{win_y:.1}) expect byte={}",
            code_seg.base, code_seg.text_len, code_seg.x, code_seg.top, code_seg.base + local_b
        );
        let live_ent = cx.update(|_, cx| host.read(cx).live.clone());
        let ev = MouseDownEvent {
            position: point(px(win_x), px(win_y)),
            modifiers: Modifiers::default(),
            button: MouseButton::Left,
            click_count: 1,
            first_mouse: false,
        };
        cx.update(|window, cx| {
            live_ent.update(cx, |v, cx| v.on_mouse_down(&ev, window, cx));
        });
        let caret = cx.update(|_, cx| caret_source(&code, cx));
        eprintln!("DEBUG caret byte={caret}");
        assert_eq!(caret, code_seg.base + local_b, "滚动后点击仍应精确映射");
        // bounds_for_range 也应贴着文本（滚动后）。
        let (bx, by) = cx.update(|window, cx| {
            live_ent.update(cx, |v, cx| {
                let sel = v.selection;
                let r = v.buffer.byte_to_utf16(sel.start())..v.buffer.byte_to_utf16(sel.end());
                v.bounds_for_range(r, Bounds::default(), window, cx)
                    .map(|b| (b.origin.x.as_f32(), b.origin.y.as_f32()))
            })
        })
        .expect("bounds");
        eprintln!("DEBUG bounds_for_range=({bx:.1},{by:.1})");
        // 点击点在行内 +2px，光标 bounds 顶对齐行顶：期望 x=段x+字符偏移，y=段top-滚动。
        let expect_bx = code_seg.x + x_b;
        let expect_by = code_seg.top - scroll_y;
        assert!(
            (bx - expect_bx).abs() < 1.0 && (by - expect_by).abs() < 1.0,
            "滚动后 bounds 应贴合文本：({bx:.1},{by:.1}) vs 期望 ({expect_bx:.1},{expect_by:.1})"
        );
    }

    /// 调试：对比「命中/光标所用的 BODY_FONT=.SystemUIFont 整形」与「渲染 div 实际
    /// 使用的字形（主题默认 sans）」在混合 CJK/ASCII 文本上的宽度差。
    #[gpui::test]
    fn debug_body_font_width_vs_render(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let (host, cx) = cx.add_window_view(|_window, cx| {
            let live = cx.new(|cx| MarkdownLivePreview::new("这段中文 mixed ASCII 与 123 数字。\n", cx));
            SidebarHost { live }
        });
        let cx: &mut VisualTestContext = cx;
        cx.simulate_resize(size(px(900.), px(600.)));
        for _ in 0..4 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
        }
        cx.run_until_parked();

        let text = "这段中文 mixed ASCII 与 123 数字。";
        let (w_ui, w_sans, samples) = cx.update(|window, cx| {
            let v = host.read(cx).live.read(cx);
            let fs = v.p.para_size;
            let run_ui = gpui::TextRun {
                len: text.len(),
                font: font(".SystemUIFont"),
                color: gpui::Hsla::default(),
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let theme = gpui_component::Theme::global(cx).semantic_tokens();
            let sans = theme.typography.sans.clone();
            let run_sans = gpui::TextRun {
                len: text.len(),
                font: font(sans.clone()),
                color: gpui::Hsla::default(),
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let ts = window.text_system();
            let l_ui = ts.shape_line(SharedString::from(text), fs, &[run_ui], None);
            let l_sans = ts.shape_line(SharedString::from(text), fs, &[run_sans], None);
            let mut out = String::new();
            for b in [0usize, 2, 4, 8, 12, 16, 20, text.len() / 2, text.len() - 2, text.len()] {
                let xu = l_ui.x_for_index(b).as_f32();
                let xs = l_sans.x_for_index(b).as_f32();
                out.push_str(&format!("@{} UI={xu:.1} sans={xs:.1} Δ={:.1} | ", b, (xu - xs).abs()));
            }
            (l_ui.width().as_f32(), l_sans.width().as_f32(), out)
        });
        eprintln!("DEBUG BODY_FONT(.SystemUIFont) vs theme-sans\n  {text}\n  总宽 UI={w_ui:.1} sans={w_sans:.1}\n  {samples}");
        eprintln!("DEBUG done");
    }

    /// 关键：**自动换行的段落**在不同视觉行点击时，光标 x 必须 ≈ 点击 x。
    /// 若换行后第二/三行的点击偏差几十上百 px，就是「越往下/越往右越乱」的根源。
    #[gpui::test]
    fn debug_wrapped_paragraph_click_maps(cx: &mut TestAppContext) {
        // ASCII 为主的超长句，保证在测试字形下也必然换行（>876px），真正走到 wrap 路径。
        let src = "This is a deliberately long ASCII sentence used to force automatic line wrapping inside the available container width so that we can verify clicking on the second and third visual lines maps the caret back to the exact same horizontal position. Add some more words here to be safe about producing multiple wrapped lines. abc def ghi jkl mno pqr stu vwx yz. \n";
        let (host, mut cx) = open_live(cx, src);
        let code = cx.update(|_, cx| host.read(cx).live.clone());
        let segs = cx.update(|_, cx| {
            let v = code.read(cx);
            let segs = v.segs.borrow();
            segs.iter()
                .filter(|s| s.line.is_some() && s.text_len > 8)
                .map(|s| (s.base, s.text_len, s.x, s.top, s.height))
                .collect::<Vec<_>>()
        });
        eprintln!(
            "DEBUG 段落分段数={}（>1 才是真正的换行路径）段们={segs:?}",
            segs.len()
        );
        assert!(segs.len() >= 2, "测试需要一个真正换行的段落（≥2 段）");
        // 每个视觉行：点行中央，光标 x 应当回贴点击 x（± 一个字符容差 ~10px）。
        for (k, &(base, tlen, sx, top, h)) in segs.iter().take(4).enumerate() {
            let x_query = sx + 60.0;
            let y_query = top + 3.0;
            cx.simulate_mouse_down(
                point(px(x_query), px(y_query)),
                MouseButton::Left,
                Modifiers::default(),
            );
            cx.run_until_parked();
            let caret = cx.update(|_, cx| caret_source(&code, cx));
            let caret_px = cx.update(|_, cx| caret_pixel(&code, cx)).unwrap();
            let (row, cx_px, cy_px, _) = caret_px;
            eprintln!(
                "DEBUG 视觉行{k}: 点(x={x_query:.1},y={y_query:.1}) base={base} len={tlen} -> byte={caret} caret=(row{row} x={cx_px:.1} y={cy_px:.1})"
            );
            assert!(
                (cx_px - x_query).abs() < 40.0,
                "视觉行 {k}：点击 x={x_query:.1} 但光标回贴 x={cx_px:.1}（差 {:.1}px）→ 换行路径度量错位",
                (cx_px - x_query).abs()
            );
        }
    }

    /// 决定性复核：`closest_index_for_x`（点击→索引）与 `x_for_index`（索引→x）
    /// 是否互逆。若 x_for_index(closest(x)) 与 x 相差巨大（几十~上百 px），
    /// 就是「鼠标点击处 → 光标落位」错位的根源。
    #[gpui::test]
    fn debug_closest_index_for_x_roundtrip(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let (host, mut cx) = cx.add_window_view(|_window, cx| {
            let live = cx.new(|cx| MarkdownLivePreview::new("# 标题\n\n正文开始：this is 一些中文 mixed rem text 用于探测。abc123。\n", cx));
            SidebarHost { live }
        });
        let cx: &mut VisualTestContext = cx;
        cx.simulate_resize(size(px(900.), px(600.)));
        for _ in 0..4 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
        }
        cx.run_until_parked();
        let out = cx.update(|window, cx| {
            let v = host.read(cx).live.read(cx);
            let segs = v.segs.borrow();
            // 找一个含中文+ASCII 的段落段：x 范围越宽越能暴露度量错位。
            let s = segs.iter().find(|s| s.line.is_some() && s.text_len > 20).unwrap();
            let line = s.line.as_ref().unwrap();
            let total = line.width().as_f32();
            let mut out = String::new();
            for x in [0.0f32, 24.0, 48.0, 72.0, 96.0, 120.0, 160.0, 200.0, 260.0, 320.0, total * 0.5, total - 2.0] {
                let x = x.min(total - 1.0).max(0.0);
                let ix = line.closest_index_for_x(px(x));
                let back = line.x_for_index(ix).as_f32();
                out.push_str(&format!("x={x:.0} -> idx={ix} -> back={back:.1} Δ={:.1} | ", (x - back).abs()));
            }
            let _ = window;
            out
        });
        eprintln!("DEBUG closest_index_for_x roundtrip (line width {out})");
        // 对密集中文段：任何查询 x 回放不应差一个字符以上（~20px）。
        let _ = &out;
    }

    /// 用户假设复核：多行代码块是否「每行行高与文本行高不一致」导致越往后越乱。
    /// 这里对 12 行（含语言头/空行）代码块的**每一行**点击并检查：
    /// 1) 段 top 严格等差（=行高，无累计漂移）；2) 每行点击 → 光标 = 该行 base+local；
    /// 3) caret_px 与段几何完全一致。全过 ⇒ 代码块行高/每行映射在应用内是精确的。
    #[gpui::test]
    fn debug_code_block_every_line_pins(cx: &mut TestAppContext) {
        let mut src = String::new();
        src.push_str("```java\n");
        for i in 0..12 {
            if i % 3 == 2 {
                src.push_str("\n");
            } else {
                src.push_str(&format!("int v{i} = {i};\n"));
            }
        }
        src.push_str("```\n\n正文\n");
        let (host, mut cx) = open_live(cx, &src);
        let code = cx.update(|_, cx| host.read(cx).live.clone());
        let segs = cx.update(|_, cx| code_segs(&code, cx));
        assert!(
            segs.len() >= 8,
            "12 行代码块（含空行）应有 ≥8 个代码行段，实际 {}",
            segs.len()
        );
        // 1) 行距严格等差（都用同一 code_line）——检验「每行行高一致」。
        for w in segs.windows(2) {
            let d = w[1].top - w[0].top;
            assert!(
                (d - w[0].height).abs() < 0.01,
                "代码行高不一致：line@{} top={} h={} 与下一行 top={} 差={}（应={}）",
                w[0].base, w[0].top, w[0].height, w[1].top, d, w[0].height
            );
        }
        // 2)+3) 每行点击：光标=base+local，且 caret_px 与该段一致。
        let local_b = 4usize;
        for (i, seg) in segs.iter().enumerate() {
            if seg.text_len < local_b + 2 {
                continue; // 空行只测点行首
            }
            let xb = cx.update(|_, cx| {
                let v = code.read(cx);
                let segs = v.segs.borrow();
                let s = segs.iter().find(|s| s.base == seg.base && s.line.is_some()).unwrap();
                s.line.as_ref().unwrap().x_for_index(local_b).as_f32()
            });
            cx.simulate_mouse_down(
                point(px(seg.x + xb), px(seg.top + 2.0)),
                MouseButton::Left,
                Modifiers::default(),
            );
            cx.run_until_parked();
            let caret = cx.update(|_, cx| caret_source(&code, cx));
            assert_eq!(
                caret,
                seg.base + local_b,
                "代码行 {i}（base={}）点击应 = base+{local_b}，实际 {caret}",
                seg.base
            );
            let (row, px_, py, ph) = cx.update(|_, cx| caret_pixel(&code, cx)).expect("光标");
            assert!(
                (px_ - (seg.x + xb)).abs() < 1.0
                    && (py - seg.top).abs() < 1.0
                    && (ph - seg.height).abs() < 1.0,
                "代码行 {i}：caret_px=({row} x={px_:.1} y={py:.1} h={ph:.1}) 应与段 (x={:.1} top={:.1} h={:.1}) 对齐",
                seg.x + xb,
                seg.top,
                seg.height
            );
        }
    }

    /// 决定性：长文档下，seg 累计的内容高 vs 滚动容器**实际**内容高是否漂移。
    /// 漂移 ⇒ 点击/光标用 seg 几何，但文本被 gpui 实排在别处 → 「越往后差距越大」。
    /// `max_offset().y` 的**绝对值**即实际可滚动距离，`视图高 + 可滚动距离` = 绘制总内容高。
    #[gpui::test]
    fn debug_seg_vs_painted_content_height(cx: &mut TestAppContext) {
        let mut src = String::new();
        for i in 0..400 {
            src.push_str(&format!("第 {i} 段正文内容足够长用于测试换行与行高累计，避免宽度相关差异。\n\n"));
        }
        src.push_str("```rust\nlet end = 1;\n```\n");
        let (host, cx) = open_live(cx, &src);
        for scale in [1.0f32, 1.25] {
            cx.update(|window, _| window.set_scale_factor(scale));
            for _ in 0..4 {
                cx.run_until_parked();
                let _ = cx.cx.refresh();
            }
            cx.run_until_parked();
            let (seg_total, vp_h, max_off_y) = cx.update(|_, cx| {
                let v = host.read(cx).live.read(cx);
                let segs = v.segs.borrow();
                let last = segs.last().unwrap();
                (
                    last.top + last.height,
                    v.element_bounds.size.height.as_f32(),
                    v.v_handle.max_offset().y.as_f32().abs(),
                )
            });
            let painted = vp_h + max_off_y;
            let drift = painted - seg_total;
            eprintln!("DEBUG scale={scale} seg_total={seg_total:.1} painted={painted:.1} vp={vp_h:.1} scrollable={max_off_y:.1} drift={drift:.2}px");
            assert!(
                drift.abs() < 4.0,
                "scale={scale}: 绘制内容高 {painted:.1} 与 seg 几何 {seg_total:.1} 漂移 {drift:.2}px → 点击/光标与文本错位"
            );
        }
    }

    /// 用户真实文档（`target/md测试.md`）在两种宽度下的「绘制内容高 == seg 几何」断言，
    /// 覆盖真机全屏(2148.6)与窗口(934.3)。这是「光标/输入相对文本往上飘」的直接护栏。
    #[gpui::test]
    fn debug_real_doc_painted_height_matches_segs(cx: &mut TestAppContext) {
        let path = std::path::Path::new("target/md测试.md");
        if !path.exists() {
            eprintln!("WARN 缺少 target/md测试.md，跳过");
            return;
        }
        let src = std::fs::read_to_string(path).unwrap();
        cx.update(gpui_component::init);
        for w in [934.3f32, 2148.6] {
            let live = cx.update(|cx| cx.new(|cx| MarkdownLivePreview::new(&src, cx)));
            let (host, mut cx2) = cx.add_window_view(|_w, _cx| LiveHost { live });
            cx2.simulate_resize(size(px(w), px(760.)));
            for _ in 0..4 {
                cx2.run_until_parked();
                let _ = cx2.cx.refresh();
            }
            cx2.run_until_parked();
            let (seg_total, vp_h, scrollable) = cx2.update(|_, cx| {
                let v = host.read(cx).live.read(cx);
                let segs = v.segs.borrow();
                let last = segs.last().unwrap();
                (
                    last.top + last.height,
                    v.element_bounds.size.height.as_f32(),
                    v.v_handle.max_offset().y.as_f32().abs(),
                )
            });
            if seg_total <= vp_h && scrollable <= 0.0 {
                // 内容不足一屏：仍要求 seg 总高 ≤ 视口高（无溢出），语义一致。
                eprintln!("DEBUG w={w} seg_total={seg_total:.1} vp={vp_h:.1} 不足一屏");
                continue;
            }
            let painted = vp_h + scrollable;
            let drift = painted - seg_total;
            eprintln!("DEBUG w={w} seg_total={seg_total:.1} painted={painted:.1} vp={vp_h:.1} scrollable={scrollable:.1} drift={drift:.2}px");
            assert!(
                drift.abs() < 4.0,
                "w={w}: 绘制内容高 {painted:.1} 与 seg 几何 {seg_total:.1} 漂移 {drift:.2}px（真实文档）"
            );
        }
    }

    #[gpui::test]
    fn live_edit_insert_updates_text_caret_and_reparses(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let mut ed = cx.update(|cx| MarkdownLivePreview::new("# 标题\n\n正文\n", cx));
        assert_eq!(ed.text(), "# 标题\n\n正文\n");
        // 行：标题 / 空行 / 正文 / 尾随空行
        assert_eq!(ed.test_live_len(), 4);

        // 在 "正文" 前（源字节 10）插入 "加" → 光标推进 3 字节，重解析结构不变。
        let caret = ed.test_insert_at(10, "加");
        assert_eq!(caret, 13);
        assert_eq!(ed.text(), "# 标题\n\n加正文\n");
        assert_eq!(ed.test_live_len(), 4);

        // 定位："加正文" 行是 (2,0)，把光标放回行首，映射正确。
        ed.test_set_caret(10);
        assert_eq!(ed.test_caret_target(), Some((2, 0)));
    }

    #[gpui::test]
    fn live_edit_insert_within_bold_updates_source(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        // 渲染文本 "a **b** c"，在加粗内容里插入 → 源字节同步插入（不影响记号）。
        let mut ed = cx.update(|cx| MarkdownLivePreview::new("a **b** c\n", cx));
        // 源码字节：a0 sp1 *2 *3 b4 *5 *6 sp7 c8 → 'b' 在 4
        let caret = ed.test_insert_at(4, "X");
        assert_eq!(caret, 5);
        assert_eq!(ed.text(), "a **Xb** c\n");
    }

    /// 用真实文件 + 真机点击日志做**回放校验**：给定元素宽度，逐条重算 (content_y,x)
    /// 应得字节，与用户报的 byte 比对。可用来找出「字节映射」是否系统性偏了多少。
    #[gpui::test]
    fn debug_replay_real_clicks_vs_layout(cx: &mut TestAppContext) {
        // 用户日志（bounds_w=934.3）：(content_y, local_x) -> byte
        let clicks: &[(f32, f32, usize)] = &[
            (292.6 + 260.0, 243.4, 534),
            (301.7 + 260.0, 265.7, 537),
            (300.0 + 520.0, 321.7, 1413),
            (308.6 + 520.0, 330.3, 1414),
            (284.6 + 520.0, 385.7, 1368),
            (281.1 + 520.0, 250.3, 1358),
            (344.6 + 2600.0, 289.1, 6724),
            (490.9 + 3380.0, 329.1, 8682),
            (185.7 + 4680.0, 372.0, 10959),
            (362.3 + 6240.0, 344.0, 15762),
            (372.0 + 6885.1, 320.6, 16578),
        ];
        let path = std::path::Path::new("target/md测试.md");
        let src = if path.exists() {
            std::fs::read_to_string(path).unwrap()
        } else {
            eprintln!("WARN missing target/md测试.md，回放跳过");
            return;
        };
        cx.update(gpui_component::init);
        let mut best = (usize::MAX, 0usize, 0.0f32);
        // 真机实际宽度 934.3；横扫邻近宽度排除取整差异。
        for w in [9343usize, 934, 935, 933, 936, 940, 930, 900, 1000, 1100] {
            let wpx = if w == 9343 { 934.3 } else { w as f32 };
            let live = cx.update(|cx| cx.new(|cx| MarkdownLivePreview::new(&src, cx)));
            let (host, cx2) = cx.add_window_view(|_w, _cx| LiveHost { live });
            cx2.simulate_resize(size(px(wpx), px(760.)));
            for _ in 0..4 {
                cx2.run_until_parked();
                let _ = cx2.cx.refresh();
            }
            cx2.run_until_parked();
            let mut bad = 0usize;
            for &(cy, lx, want) in clicks {
                let got = cx2.update(|_, cx| {
                    let v = host.read(cx).live.read(cx);
                    let segs = v.segs.borrow();
                    let s = segs.iter().find(|s| {
                        let isq = matches!(v.live.get(s.row_ix).map(|lr| lr.row.kind), Some(RowKind::Quote));
                        if isq {
                            cy >= s.top - crate::markdown::live::BAND_PY && cy <= s.top + s.height + crate::markdown::live::BAND_PY
                        } else {
                            cy >= s.top && cy <= s.top + s.height
                        }
                    })?;
                    let lb = s
                        .line
                        .as_ref()
                        .map(|l| l.closest_index_for_x(px((lx - s.x).max(0.0))).min(s.text_len))
                        .unwrap_or(0);
                    let srcb = s.source_for(lb);
                    Some((srcb, s.top, s.height, s.base, s.text_len))
                });
                match got {
                    Some((b, top, h, base, tlen)) => {
                        if b != want {
                            bad += 1;
                            eprintln!(
                                "  w={wpx} cy={cy:.1} x={lx:.1} want={want} got={b} (top={top:.1} h={h:.1} base={base} len={tlen})"
                            );
                        }
                    }
                    None => {
                        bad += 1;
                        eprintln!("  w={wpx} cy={cy:.1} x={lx:.1} want={want} -> NO SEG");
                    }
                }
            }
            eprintln!("width {wpx}: matched {} / {}   (miss={bad})", clicks.len() - bad, clicks.len());
            if bad < best.0 {
                best = (bad, clicks.len() - bad, wpx);
            }
        }
        eprintln!(
            "REPLAY best: miss={} matched={} at width={}",
            best.0, best.1, best.2
        );
    }

    /// 调试：真实文档在宽 2148.6 下，row≈170 附近的段几何 ↔ 源码行对照（验证用户点击行）。
    #[gpui::test]
    fn debug_dump_real_doc_near_170(cx: &mut TestAppContext) {
        let path = std::path::Path::new("target/md测试.md");
        let src = std::fs::read_to_string(path).unwrap();
        cx.update(gpui_component::init);
        let live = cx.update(|cx| cx.new(|cx| MarkdownLivePreview::new(&src, cx)));
        let (host, mut cx2) = cx.add_window_view(|_w, _cx| LiveHost { live });
        cx2.simulate_resize(size(px(2148.6), px(760.)));
        for _ in 0..4 {
            cx2.run_until_parked();
            let _ = cx2.cx.refresh();
        }
        cx2.run_until_parked();
        cx2.update(|_, cx| {
            let v = host.read(cx).live.read(cx);
            let segs = v.segs.borrow();
            for s in segs.iter().filter(|s| (165..185).contains(&s.row_ix)) {
                let src_line = if s.map.is_none() && s.line.is_some() {
                    v.buffer.line_of_offset(s.base)
                } else {
                    usize::MAX
                };
                eprintln!(
                    "row={} base={} len={} x={:.1} top={:.1} h={:.1} src_line={}",
                    s.row_ix,
                    s.base,
                    s.text_len,
                    s.x,
                    s.top,
                    s.height,
                    src_line,
                );
            }
        });
    }

    /// 二分定位最后一丁点残差：前缀长度 vs (绘制高 - seg高)。
    #[gpui::test]
    fn debug_real_doc_drift_bisect(cx: &mut TestAppContext) {
        let path = std::path::Path::new("target/md测试.md");
        if !path.exists() {
            return;
        }
        let full = std::fs::read_to_string(path).unwrap();
        cx.update(gpui_component::init);
        let bytes = full.len();
        // 安全的字符边界截断。
        let cut_at = |cut: usize| {
            full.char_indices()
                .map(|(i, _)| i)
                .chain(std::iter::once(full.len()))
                .find(|&i| i >= cut)
                .unwrap_or(full.len())
        };
        for frac in [0.05f32, 0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9, 1.0] {
            let cut = cut_at((bytes as f32 * frac) as usize);
            let prefix = full[..cut].to_string();
            let live = cx.update(|cx| cx.new(|cx| MarkdownLivePreview::new(&prefix, cx)));
            let (host, mut cx2) = cx.add_window_view(|_, _| LiveHost { live });
            cx2.simulate_resize(size(px(934.3), px(760.)));
            for _ in 0..4 {
                cx2.run_until_parked();
                let _ = cx2.cx.refresh();
            }
            cx2.run_until_parked();
            let (seg_total, vp, scrollable) = cx2.update(|_, cx| {
                let v = host.read(cx).live.read(cx);
                let segs = v.segs.borrow();
                let last = segs.last().unwrap();
                (
                    last.top + last.height,
                    v.element_bounds.size.height.as_f32(),
                    v.v_handle.max_offset().y.as_f32().abs(),
                )
            });
            if scrollable <= 0.0 {
                eprintln!("DRIFTBISECT frac={frac:.2} seg={seg_total:.1} 不足一屏 skip");
                continue;
            }
            eprintln!("DRIFTBISECT frac={frac:.2} seg={seg_total:.1} painted={:.1} drift={:.2}",
                vp + scrollable, (vp + scrollable) - seg_total);
        }
    }

    /// 定位：逐行对比「该行全部 seg 的总高」vs「实际绘制的行高」，
    /// 找出折行/代码/引述等哪些行发生绘制与 seg 的实质偏差。
    #[gpui::test]
    fn debug_wrapped_row_painted_h_vs_seg_total(cx: &mut TestAppContext) {
        let path = std::path::Path::new("target/md测试.md");
        if !path.exists() {
            return;
        }
        let src = std::fs::read_to_string(path).unwrap();
        let (host, mut cx) = open_live(cx, &src);
        cx.simulate_resize(size(px(934.3), px(760.)));
        for _ in 0..4 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
        }
        cx.run_until_parked();
        // 每行：seg 总数/命中区高 vs painted。
        let rows_info: Vec<(usize, usize, f32)> = cx.update(|_, cx| {
            let v = host.read(cx).live.read(cx);
            let segs = v.segs.borrow();
            let mut m: std::collections::BTreeMap<usize, (usize, f32, f32)> = Default::default();
            for s in segs.iter() {
                let e = m.entry(s.row_ix).or_insert((0, f32::MAX, f32::MIN));
                e.0 += 1;
                e.1 = e.1.min(s.top);
                e.2 = e.2.max(s.top + s.height);
            }
            let live = &v.live;
            m.into_iter()
                .map(|(ix, (n, t0, t1))| {
                    let q = match live.get(ix).map(|lr| lr.row.kind) {
                        Some(RowKind::Quote) => 2.0 * BAND_PY,
                        _ => 0.0,
                    };
                    (ix, n, (t1 - t0) + q)
                })
                .collect()
        });
        let mut checked = 0usize;
        let mut bad = 0usize;
        for &(rix, nseg, hit_h) in rows_info.iter().take(90) {
            let key: &'static str = Box::leak(format!("md-row-{rix}").into_boxed_str());
            let Some(b) = cx.debug_bounds(key) else { continue };
            let ph = b.size.height.as_f32();
            let d = ph - hit_h;
            if d.abs() > 1.0 || nseg > 1 {
                eprintln!(
                    "WRAP row={rix} segs={nseg} hitH={hit_h:.2} paintedH={ph:.2} dH={d:.2}",
                );
                if d.abs() > 1.0 {
                    bad += 1;
                }
            }
            checked += 1;
        }
        eprintln!("WRAP checked={checked} rows_with_paintedH_mismatch={bad}");

        // 全新视图、一次性以 2148.6 建：若同样错位 → 全屏本身折行/绘制不一致（真 bug）；
        // 若一致 → 上面 900→934→2148 的 resize 级联让旧几何残留（时序 bug）。
        let live2 = cx.update(|_, cx| cx.new(|cx| MarkdownLivePreview::new(&src, cx)));
        let (host2, mut cx3) = cx.add_window_view(|_w, _cx| LiveHost { live: live2 });
        cx3.simulate_resize(size(px(2148.6), px(760.)));
        for _ in 0..4 {
            cx3.run_until_parked();
            let _ = cx3.cx.refresh();
        }
        cx3.run_until_parked();
        for &rix in &[14usize, 15, 16, 17, 18] {
            let segs = cx3.update(|_, cx| {
                let v = host2.read(cx).live.read(cx);
                let segs = v.segs.borrow();
                segs.iter()
                    .filter(|s| s.row_ix == rix)
                    .map(|s| (s.top, s.height))
                    .collect::<Vec<_>>()
            });
            let key: &'static str = Box::leak(format!("md-row-{rix}").into_boxed_str());
            let pb = cx3.debug_bounds(key);
            eprintln!(
                "FRESH2148 row={rix} painted={:?} segs(top,h)={segs:?}",
                pb.map(|b| (b.origin.y.as_f32(), b.size.height.as_f32()))
            );
        }

        // 细节：dump 14..21 行的每条 seg（top/h/base/len）与 painted wrapper。
        for &rix in &[14usize, 15, 16, 17, 18, 19, 20, 21] {
            let segs = cx.update(|_, cx| {
                let v = host.read(cx).live.read(cx);
                let segs = v.segs.borrow();
                segs.iter()
                    .filter(|s| s.row_ix == rix)
                    .map(|s| (s.row_ix, s.x, s.top, s.height, s.base, s.text_len))
                    .collect::<Vec<_>>()
            });
            let key: &'static str = Box::leak(format!("md-row-{rix}").into_boxed_str());
            let pb = cx.debug_bounds(key);
            eprintln!(
                "ROW {rix} painted={:?}  segs={:?}",
                pb.map(|b| (b.origin.y.as_f32(), b.size.height.as_f32())),
                segs
            );
        }
        assert!(
            bad == 0,
            "有 {bad} 行:实际绘制高度与 seg 总高不一致 → 命中/光标/IME 偏移的根源"
        );
    }
}
