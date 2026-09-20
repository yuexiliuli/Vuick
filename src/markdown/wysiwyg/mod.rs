//! 本项目管线实现的 WYSIWYG 编辑器（初步实现）——`markdown::wysiwyg`。
//!
//! 路线：以本项目自研管线（`parser`/`cells`/`ir`/`doc`）实现所见即所得编辑，
//! **逐步替换** zorite 移植的 `markdown::live`，最终全盘单一套。
//!
//! 本文件是第一步「初步实现」：
//! - **文档模型完全来自本项目** [`super::doc::Doc`]：IR 语义 + `BlockIndex` + 源；
//!   行物化 `rows_in_block_window` + 编辑映射 [`DocRow::byte_map`]（与预览逐字节
//!   一致，`parity` 钉死）。
//! - 编辑器 = auto-grow 自绘 [`element::WyEditorElement`]（宿主滚动，与 `live` 同宿主
//!   范式）；行内样式来自 `DocRow.runs`（`parser::RunKind`）。
//! - 现状声明（留待逐步补齐）：首帧/变更整篇 shape（未窗口化，大文档慢——下一步把
//!   `rows_in_line_window` 的窗口化落到 shape，`Doc` 已为此设计）；无选区/多行选择；
//!   无自动列表续行。仅作「预览编辑测试」入口，验证「本项目流程能编辑 + 正确映射回源」。

mod element;

use std::cell::{Cell, RefCell};
use std::ops::Range;

use gpui::{
    Context, EntityInputHandler, FocusHandle, Focusable, Font, Hsla, Pixels, SharedString,
    UTF16Selection, Window, font, px,
};

use super::doc::Doc;

/// 编辑器变更事件（宿主订阅，内容镜像回源码编辑区/保存流程）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WyEvent {
    Changed,
}

/// 行内样式调色（创建时注入；后续随主题切换重建）。
///
/// **关键**：`font_size`/`font` 在创建时由 [`WyEditor::new`] 从
/// `window.text_style()` **一次性**确定并存进主题——之后 `build_frame` 一律用主题值，
/// 绝不再次读 `window.text_style()`。因为 measure（栈空=默认 16）与 prepaint
/// （栈内含 `live-body` 的 `.text_size`）两阶段的 `text_style()` **不同**，若每帧现取
/// 会在两个字号间交替 = 滚动时「字体一会儿大一会儿小」。
#[derive(Clone)]
pub struct WyTheme {
    pub text: Hsla,
    pub mono: Font,
    pub font: Font,
    pub font_size: Pixels,
    pub accent: Hsla,
    pub code_bg: Hsla,
}

/// 每帧光标几何（供 IME/`EntityInputHandler::bounds_for_range` 读取）。
#[derive(Clone, Copy, Default)]
pub(crate) struct CaretVis {
    pub x: Pixels,
    pub top: Pixels,
    pub line_height: Pixels,
}

thread_local! {
    static CARET_VIS: Cell<CaretVis> = const { Cell::new(CaretVis { x: px(0.), top: px(0.), line_height: px(0.) }) };
}

pub(crate) fn set_caret_vis(v: CaretVis) {
    CARET_VIS.with(|c| c.set(v));
}
fn caret_vis() -> CaretVis {
    CARET_VIS.with(|c| c.get())
}

/// 项目管线的所见即所得编辑器：`Doc` 为唯一数据源，`caret` 是**源字节偏移**。
pub struct WyEditor {
    content: SharedString,
    doc: Doc,
    focus: FocusHandle,
    caret: usize,
    theme: WyTheme,
    /// 本帧行网格（measure→prepaint→paint 复用；**窗口化**：仅带内块被真正 shape，
    /// 带外块用 `BlockMeta` 估算高度占位，避免大文档整篇每帧 shape 卡死）。
    frame: RefCell<Option<std::rc::Rc<element::Frame>>>,
    /// 本帧元素在窗口中的布局 bounds（paint 写入；鼠标命中时全局坐标 → 元素局部）。
    last_bounds: Cell<Option<gpui::Bounds<Pixels>>>,
    /// 内容代数：任何文本变更自增；`request_layout` 用它判断帧缓存是否仍有效。
    content_gen: Cell<u64>,
    /// 可视带（窗口坐标→元素局部 y，量程化）：prepaint 用 `bounds.origin.y` +
    /// `viewport_size` 推算并发布，measure 用上一帧的带做窗口化 shape。
    shape_band: Cell<Option<(f32, f32)>>,
    /// 块「真实可视行数」缓存（关键：带宽滚动时带外块沿用**已整形过的真值**，
    /// 而非每次回到估算——否则行高在估算/真实间跳动，滚动时文字忽大忽小）。
    /// 键 = `(gen, src_line, kind_tag, width_bits)` 的 hash；值 = 可视行数。
    height_cache: RefCell<std::collections::HashMap<u64, u32>>,
}

impl WyEditor {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let ts = window.text_style();
        Self {
            content: SharedString::default(),
            doc: Doc::open(SharedString::default()),
            focus: cx.focus_handle(),
            caret: 0,
            theme: WyTheme {
                text: ts.color,
                mono: font("monospace"),
                font: ts.font(),
                font_size: ts.font_size.to_pixels(window.rem_size()),
                accent: gpui::rgb(0x3b82f6).into(),
                code_bg: gpui::hsla(0.62, 0.35, 0.62, 0.18),
            },
            frame: RefCell::new(None),
            last_bounds: Cell::new(None),
            content_gen: Cell::new(0),
            shape_band: Cell::new(None),
            height_cache: RefCell::new(std::collections::HashMap::new()),
        }
    }

    /// 覆盖主题。
    #[allow(dead_code)]
    pub fn set_theme(&mut self, theme: WyTheme) {
        self.theme = theme;
        *self.frame.borrow_mut() = None;
    }

    /// 显式设置字号（应用把全局字号同步进来；变更新帧缓存）。不读窗口 text_style，
    /// 保证 measure/prepaint 两阶段一致。
    pub fn set_font_size(&mut self, size: Pixels) {
        if self.theme.font_size == size {
            return;
        }
        self.theme.font_size = size;
        *self.frame.borrow_mut() = None;
    }

    /// 设置初始内容。
    pub fn with_text(mut self, text: &str) -> Self {
        self.set_source_text(text);
        self
    }

    /// 当前完整源文本（宿主写回用）。
    pub fn text(&self) -> SharedString {
        self.content.clone()
    }

    /// 块数（滚动量程等）。
    pub fn block_count(&self) -> usize {
        self.doc.block_count()
    }

    /// 聚焦句柄（元素命中/宿主聚焦用）。
    pub(crate) fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    /// 覆盖源并重建文档模型。
    pub fn set_source_text(&mut self, text: &str) {
        self.content = SharedString::from(text);
        self.doc = Doc::open(self.content.clone());
        self.caret = self.caret.min(self.content.len());
        *self.frame.borrow_mut() = None;
        self.content_gen.set(self.content_gen.get().wrapping_add(1));
    }

    /// 内部数据（元素读取）。
    pub(crate) fn content(&self) -> &str {
        self.content.as_ref()
    }
    pub(crate) fn doc(&self) -> &Doc {
        &self.doc
    }
    pub(crate) fn caret(&self) -> usize {
        self.caret
    }

    /// 直接放置光标（源字节偏移）。命中/测试/编程式定位共用。
    pub(crate) fn set_caret(&mut self, off: usize) {
        self.caret = off.min(self.content.len());
        *self.frame.borrow_mut() = None;
    }

    /// 记录本帧元素在窗口中的 bounds（paint 调用；鼠标命中换算用）。
    pub(crate) fn set_last_bounds(&self, bounds: Option<gpui::Bounds<Pixels>>) {
        self.last_bounds.set(bounds);
    }
    pub(crate) fn last_bounds(&self) -> Option<gpui::Bounds<Pixels>> {
        self.last_bounds.get()
    }

    pub(crate) fn content_gen(&self) -> u64 {
        self.content_gen.get()
    }
    pub(crate) fn set_shape_band(&self, band: Option<(f32, f32)>) {
        self.shape_band.set(band);
    }
    pub(crate) fn shape_band(&self) -> Option<(f32, f32)> {
        self.shape_band.get()
    }
    pub(crate) fn cached_height_lines(&self, key: u64) -> Option<u32> {
        self.height_cache.borrow().get(&key).copied()
    }
    pub(crate) fn cache_height_lines(&self, key: u64, lines: u32) {
        self.height_cache.borrow_mut().insert(key, lines);
    }

    /* ---------- 光标 ↔ 源辅助 ---------- */

    /// 源字节偏移 → (源行, 行内字节偏移)。
    pub(crate) fn row_col(&self, off: usize) -> (usize, usize) {
        let s = self.content.as_ref();
        let off = off.min(s.len());
        let mut line = 0;
        let mut last = 0;
        for (i, b) in s.bytes().enumerate() {
            if i >= off {
                break;
            }
            if b == b'\n' {
                line += 1;
                last = i + 1;
            }
        }
        (line, off - last)
    }

    /// 源行 → 该行起始字节。
    pub(crate) fn line_start(&self, line: usize) -> usize {
        let s = self.content.as_ref();
        let mut start = 0;
        let mut n = 0;
        for (i, b) in s.bytes().enumerate() {
            if n == line {
                return i;
            }
            if b == b'\n' {
                n += 1;
                start = i + 1;
            }
        }
        start
    }

    /// 源行 → 行尾字节（不含换行符）。
    pub(crate) fn line_end(&self, line: usize) -> usize {
        let start = self.line_start(line);
        match self.content.as_ref()[start..].find('\n') {
            Some(i) => start + i,
            None => self.content.len(),
        }
    }

    /// 该行的粗略 `RowKind`（行首启发，实体内部导航用；渲染以 `DocRow` 为准）。
    fn kind_for_line(&self, line: usize) -> crate::markdown::parser::RowKind {
        use crate::markdown::parser::RowKind;
        let start = self.line_start(line);
        let end = self.line_end(line);
        let raw = &self.content.as_ref()[start..end];
        let t = raw.trim_start();
        if t.is_empty() {
            RowKind::Blank
        } else if t.starts_with("```") {
            RowKind::Code
        } else if t.starts_with('|') {
            RowKind::Table
        } else if t.starts_with('#') {
            RowKind::Heading(t.chars().take_while(|c| *c == '#').count() as u8)
        } else if t.starts_with("![") {
            RowKind::Image
        } else if t.starts_with("- ") || t.starts_with("* ") || t.starts_with("+ ") {
            RowKind::Bullet
        } else if t == "---" || t == "***" || t == "___" {
            RowKind::Rule
        } else if t.starts_with('>') {
            RowKind::Quote
        } else {
            RowKind::Paragraph
        }
    }

    /// 该行「可编辑段」的源局部区间 `(lo, hi)`：`lo` = 正文源偏移，`hi` = 行尾。
    /// 非文本行 → `None`（只读/不可编辑）。
    fn editable_segment(&self, line: usize) -> Option<(usize, usize)> {
        let start = self.line_start(line);
        let end = self.line_end(line);
        if end <= start {
            return None;
        }
        let raw = &self.content.as_ref()[start..end];
        let seg = crate::markdown::parser::text_segment(raw, self.kind_for_line(line))?;
        Some((seg.0, raw.len()))
    }

    /* ---------- 编辑操作（源字节域） ---------- */

    /// 核心编辑：替换 `[start, end)` → `text`，重建模型、移光标、通知。
    pub fn replace_range(&mut self, range: Range<usize>, text: &str, cx: &mut Context<Self>) {
        let s = self.content.as_ref();
        let r = range.start.min(s.len())..range.end.min(s.len());
        if r.start == r.end && text.is_empty() {
            return;
        }
        let mut next = String::with_capacity(s.len() - r.len() + text.len());
        next.push_str(&s[..r.start]);
        next.push_str(text);
        next.push_str(&s[r.end..]);
        self.content = SharedString::from(next);
        self.doc = Doc::open(self.content.clone());
        self.caret = (r.start + text.len()).min(self.content.len());
        *self.frame.borrow_mut() = None;
        self.content_gen.set(self.content_gen.get().wrapping_add(1));
        cx.emit(WyEvent::Changed);
        cx.notify();
    }

    fn backspace(&mut self, cx: &mut Context<Self>) {
        let s = self.content.as_ref();
        let at = self.caret.min(s.len());
        if at == 0 {
            return;
        }
        let start = s[..at].char_indices().last().map(|(i, _)| i).unwrap_or(0);
        self.replace_range(start..at, "", cx);
    }

    fn delete(&mut self, cx: &mut Context<Self>) {
        let s = self.content.as_ref();
        let at = self.caret.min(s.len());
        if at >= s.len() {
            return;
        }
        let end = at + s[at..].chars().next().unwrap_or(' ').len_utf8();
        self.replace_range(at..end, "", cx);
    }

    fn newline(&mut self, cx: &mut Context<Self>) {
        self.replace_range(self.caret..self.caret, "\n", cx);
    }

    /* ---------- 光标移动 ---------- */

    fn move_left(&mut self) {
        let (line, col) = self.row_col(self.caret);
        let s = self.content.as_ref();
        if let Some((lo, hi)) = self.editable_segment(line) {
            let seg_start = self.line_start(line) + lo;
            let seg_end = self.line_start(line) + hi;
            if col > lo {
                // 段内退一字符。
                self.caret = s[..self.caret]
                    .char_indices()
                    .last()
                    .map(|(i, _)| i)
                    .unwrap_or(0)
                    .max(seg_start);
            } else if self.caret >= seg_start && self.caret < seg_end {
                // 段首（光标坐在标记后）→ 不动或回上一行段尾。
                self.caret = if line > 0 {
                    let p = line - 1;
                    let e = self.line_end(p);
                    self.editable_segment(p).map_or(self.line_start(p), |(_l, h)| self.line_start(p) + h).min(e).max(self.line_start(p))
                } else {
                    0
                };
            }
        } else if line > 0 {
            self.caret = self.line_end(line - 1).max(self.line_start(line - 1));
        } else if self.caret > 0 {
            self.caret -= 1;
        }
        self.moved();
    }

    fn move_right(&mut self) {
        let (line, _col) = self.row_col(self.caret);
        let s = self.content.as_ref();
        if let Some((lo, hi)) = self.editable_segment(line) {
            let seg_start = self.line_start(line) + lo;
            let seg_end = self.line_start(line) + hi;
            if self.caret < seg_end {
                let n = self.caret + s[self.caret..].chars().next().map_or(1, |c| c.len_utf8());
                self.caret = n.min(seg_end).max(seg_start + 1);
            } else {
                // 段尾 → 下一行段首。
                self.caret = self.next_segment_start(line);
            }
        } else {
            self.caret = self.next_segment_start(line);
        }
        self.moved();
    }

    fn next_segment_start(&self, line: usize) -> usize {
        let total = self.content.as_ref().matches('\n').count() + 1;
        if line + 1 < total {
            let n = line + 1;
            self.editable_segment(n)
                .map_or(self.line_end(n), |(lo, _)| self.line_start(n) + lo)
        } else {
            self.content.len()
        }
    }

    fn move_up(&mut self) {
        let (line, _) = self.row_col(self.caret);
        self.caret = if line > 0 {
            self.place_in_line(line - 1, self.visual_col())
        } else {
            0
        };
        self.moved();
    }

    fn move_down(&mut self) {
        let (line, _) = self.row_col(self.caret);
        let total = self.content.as_ref().matches('\n').count() + 1;
        self.caret = if line + 1 < total {
            self.place_in_line(line + 1, self.visual_col())
        } else {
            self.content.len()
        };
        self.moved();
    }

    /// 光标行内“可视列”（段内渲染列）——上下保持列。
    fn visual_col(&self) -> usize {
        let (line, col) = self.row_col(self.caret);
        if let Some((lo, _)) = self.editable_segment(line) {
            col.saturating_sub(lo)
        } else {
            0
        }
    }

    /// 目标行按“可视列”落位（限段）。
    fn place_in_line(&self, line: usize, col: usize) -> usize {
        let ls = self.line_start(line);
        let le = self.line_end(line);
        if let Some((lo, hi)) = self.editable_segment(line) {
            let seg_start = ls + lo;
            let seg_end = ls + hi;
            let cap = (seg_end.saturating_sub(seg_start + 1)).min(80);
            (seg_start + col.min(cap)).min(seg_end).min(le)
        } else {
            le.max(ls)
        }
    }

    fn moved(&mut self) {
        *self.frame.borrow_mut() = None;
    }

    /* ---------- 点击 / 鼠标 ---------- */

    /// 鼠标按下命中 → 源偏移（经 frame + byte_map）。
    pub(crate) fn on_mouse_down(&mut self, ev: &gpui::MouseDownEvent, cx: &mut Context<Self>) {
        if ev.button != gpui::MouseButton::Left {
            return;
        }
        let marker = self.frame.borrow();
        let Some(f) = marker.as_ref().map(|r| &**r) else {
            return;
        };
        let off = element::source_offset_at(self, f, ev.position);
        drop(marker);
        self.caret = off.min(self.content.len());
        *self.frame.borrow_mut() = None;
        cx.notify();
    }
}

/* ---------- 键盘 / IME 输入 ---------- */

/// 键上下文（Render 的 div 挂载；键位由 [`bind_keys`] 装配）。
pub const CONTEXT: &str = "wy-editor";

macro_rules! wy_action {
    ($name:ident, $s:literal) => {
        #[derive(Clone, Debug, PartialEq)]
        pub struct $name;
        impl gpui::Action for $name {
            fn boxed_clone(&self) -> Box<dyn gpui::Action> {
                Box::new(self.clone())
            }
            fn partial_eq(&self, other: &dyn gpui::Action) -> bool {
                (other as &dyn std::any::Any).downcast_ref::<Self>().is_some()
            }
            fn name(&self) -> &'static str {
                $s
            }
            fn name_for_type() -> &'static str {
                $s
            }
            fn build(_: serde_json::Value) -> anyhow::Result<Box<dyn gpui::Action>> {
                Ok(Box::new(Self))
            }
        }
    };
}
wy_action!(Left, "wy-left");
wy_action!(Right, "wy-right");
wy_action!(Up, "wy-up");
wy_action!(Down, "wy-down");
wy_action!(Backspace, "wy-backspace");
wy_action!(Delete, "wy-delete");
wy_action!(Enter, "wy-enter");

/// 装配键位（入口：`main.rs` 与其它键位一并安装）。
pub fn bind_keys(cx: &mut gpui::App) {
    use gpui::KeyBinding;
    cx.bind_keys([
        KeyBinding::new("left", Left, Some(CONTEXT)),
        KeyBinding::new("right", Right, Some(CONTEXT)),
        KeyBinding::new("up", Up, Some(CONTEXT)),
        KeyBinding::new("down", Down, Some(CONTEXT)),
        KeyBinding::new("backspace", Backspace, Some(CONTEXT)),
        KeyBinding::new("delete", Delete, Some(CONTEXT)),
        KeyBinding::new("enter", Enter, Some(CONTEXT)),
    ])
}

/* ---------- UTF16 ↔ 字节（IME/选择器用） ---------- */

fn offset_to_utf16(s: &str, offset: usize) -> usize {
    s[..offset.min(s.len())].encode_utf16().count()
}

fn offset_from_utf16(s: &str, utf16: usize) -> usize {
    let mut count = 0usize;
    for (i, c) in s.char_indices() {
        if count >= utf16 {
            return i;
        }
        count += c.len_utf16();
    }
    s.len()
}

fn range_to_utf16(s: &str, r: Range<usize>) -> Range<usize> {
    offset_to_utf16(s, r.start)..offset_to_utf16(s, r.end)
}

fn range_from_utf16(s: &str, r: Range<usize>) -> Range<usize> {
    offset_from_utf16(s, r.start)..offset_from_utf16(s, r.end)
}

impl EntityInputHandler for WyEditor {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let s = self.content.as_ref();
        let range = range_from_utf16(s, range_utf16);
        *actual_range = Some(range_to_utf16(s, range.clone()));
        Some(s[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let s = self.content.as_ref();
        Some(UTF16Selection {
            range: range_to_utf16(s, self.caret..self.caret),
            reversed: false,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        None
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {}

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let r = range
            .as_ref()
            .map(|r| range_from_utf16(self.content.as_ref(), r.clone()))
            .unwrap_or(self.caret..self.caret);
        self.replace_range(r, text, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        new_text: &str,
        new_selected_range: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let r_start = range
            .as_ref()
            .map(|r| range_from_utf16(self.content.as_ref(), r.clone()))
            .unwrap_or(self.caret..self.caret);
        let r_start_end = r_start.start;
        self.replace_range(r_start, new_text, cx);
        if let Some(sel) = new_selected_range {
            let start = range_from_utf16(self.content.as_ref(), sel).start;
            self.caret = (r_start_end + start).min(self.content.len());
        }
    }

    fn bounds_for_range(
        &mut self,
        _range: Range<usize>,
        element_bounds: gpui::Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<gpui::Bounds<Pixels>> {
        let v = caret_vis();
        // `caret_vis` 是元素局部坐标（`paint` 时相对元素原点记录），需叠回元素在
        // 窗口中的原点，IME/输入法候选窗才落在光标处（否则整体偏移元素原点）。
        Some(gpui::Bounds::new(
            gpui::point(element_bounds.origin.x + v.x, element_bounds.origin.y + v.top),
            gpui::size(px(2.), v.line_height),
        ))
    }

    fn character_index_for_point(
        &mut self,
        _point: gpui::Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        // 初步：返回光标（平台 IME 用）；真实命中经 element 的 on_mouse_down。
        Some(offset_to_utf16(self.content.as_ref(), self.caret))
    }

    fn accepts_text_input(&self, _: &mut Window, _: &mut Context<Self>) -> bool {
        true
    }
}

impl Focusable for WyEditor {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl gpui::EventEmitter<WyEvent> for WyEditor {}

impl gpui::Render for WyEditor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl gpui::IntoElement {
        use gpui::{CursorStyle, InteractiveElement, ParentElement, Styled};
        gpui::div()
            .relative()
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .cursor(CursorStyle::IBeam)
            .on_action(
                cx.listener(|this: &mut WyEditor, _: &Left, _: &mut Window, _: &mut Context<WyEditor>| {
                    this.move_left()
                }),
            )
            .on_action(
                cx.listener(|this: &mut WyEditor, _: &Right, _: &mut Window, _: &mut Context<WyEditor>| {
                    this.move_right()
                }),
            )
            .on_action(
                cx.listener(|this: &mut WyEditor, _: &Up, _: &mut Window, _: &mut Context<WyEditor>| {
                    this.move_up()
                }),
            )
            .on_action(
                cx.listener(|this: &mut WyEditor, _: &Down, _: &mut Window, _: &mut Context<WyEditor>| {
                    this.move_down()
                }),
            )
            .on_action(
                cx.listener(|this: &mut WyEditor, _: &Backspace, _: &mut Window, cx: &mut Context<WyEditor>| {
                    this.backspace(cx)
                }),
            )
            .on_action(
                cx.listener(|this: &mut WyEditor, _: &Delete, _: &mut Window, cx: &mut Context<WyEditor>| {
                    this.delete(cx)
                }),
            )
            .on_action(
                cx.listener(|this: &mut WyEditor, _: &Enter, _: &mut Window, cx: &mut Context<WyEditor>| {
                    this.newline(cx)
                }),
            )
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this: &mut WyEditor, ev: &gpui::MouseDownEvent, _: &mut Window, cx: &mut Context<WyEditor>| {
                    this.on_mouse_down(ev, cx)
                }),
            )
            .child(element::WyEditorElement {
                editor: cx.entity(),
            })
    }
}
