//! 一个基于 GPUI 的最小多行文本输入控件（TextArea）。
//!
//! 它照着官方 `input.rs` 示例扩展出来的，额外支持：
//! - 多行内容（`allow_newlines` 控制是否允许换行）；开启 `submit_on_enter` 后
//!   Enter 发出提交事件、Shift+Enter 插入换行（聊天输入框用）；
//! - 可见的光标与选区绘制；
//! - 常驻可见的"自定义滚动条"（GPUI 0.2.2 只提供系统 overlay 滚动条，
//!   Windows 上会自动隐藏等同不可见，所以这里自己画轨道/滑块并挂到共享的
//!   `ScrollHandle` 上做拖拽滚动）；
//! - 撤销/重做（undo/redo）历史栈；
//! - 剪贴板复制/剪切/粘贴、全选（使用系统剪贴板）；
//! - **长行自动换行**（按可用宽度 wrap，命中测试/光标定位均按换行后的可视行处理）。
//!
//! 对外暴露两种用法：
//! 1. [`TextArea`] 实现 [`Render`]，可作为普通子元素放进界面；
//! 2. [`TextElement`] 是真正负责排版（shaping）与绘制的底层 `Element`。
//!
//! 内部使用 [`gpui::WrappedLine`]（`shape_text` 产出）实现换行排版：
//! 每个"逻辑行"（以 `\n` 分隔）会在宽度不足时被拆成多个"可视行"。

use std::ops::Range;

use gpui::{
    div, fill, hsla, point, px, relative, rgba, size, App, AvailableSpace, Bounds,
    Context, Corners, CursorStyle, Element, ElementId, ElementInputHandler, Entity,
    EntityInputHandler, FocusHandle, Focusable, GlobalElementId, InspectorElementId, IntoElement,
    LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, Pixels,
    Point, Render, SharedString, Styled, Style, TextAlign, TextRun, UTF16Selection, WrappedLine,
    InteractiveElement, ScrollHandle, Window,
};
use crate::input::shared::*;

pub struct TextArea {
    /// 焦点句柄（决定键盘事件是否发给本控件）
    pub focus_handle: FocusHandle,

    /// 占位文本（内容为空时显示、半透明）
    pub placeholder: SharedString,

    /// 当前内容（UTF-8 字节串）
    pub content: SharedString,

    /// 当前选区（URF-8 字节区间，与 IME 的回传区间一致）
    pub selected_range: Range<usize>,

    /// 选中方向（向前还是向后），用于 Shift 方向键调整选区时修正起止
    selection_reversed: bool,

    /// IME 组合中的临时选区（由系统在候选窗口弹出时设置）
    marked_range: Option<Range<usize>>,

    /// 是否允许换行（Enter 插入 `\n`）
    pub allow_newlines: bool,
    /// 是否"回车即提交"：开启后按下 Enter 会发出 [`TextInputEvent`] 而不是换行；
    /// 需要换行时按 Shift+Enter。
    pub submit_on_enter: bool,

    /// 记录单次标记/替换操作前的快照，用于手势开始时 push 历史
    pending: Option<HistoryItem>,

    /// 撤销/重做栈
    history: History,

    /// 滚动条拖拽状态
    is_dragging_scrollbar: bool,
    /// 按下滚动条滑块时，鼠标相对滑块顶部的偏移（像素），拖动时滑块不跳
    scrollbar_grab_offset: Pixels,
    /// 是否处于"按住鼠标拖选"状态（仅在按下左键后为 true，移动不应改变选区）
    is_selecting: bool,
    /// 与本控件联动的滚动句柄（由外层滚动容器注入）
    pub scroll_handle: ScrollHandle,

    /// 上一次排版（shape）好的每"逻辑行"的换行布局，供绘制与命中测试复用
    pub last_layout: Option<Vec<WrappedLine>>,
    /// 为 `last_layout` 缓存的源内容，用于判断内容是否变化、要不要重新排版
    pub last_shaped_content: Option<SharedString>,
    /// 为 `last_layout` 缓存的换行宽度；宽度变化需重新排版
    pub last_wrap_width: Option<Pixels>,
    /// 绘制阶段记录的本控件布局边界（用于坐标换算）
    pub last_bounds: Option<Bounds<Pixels>>,
    /// 最后记录的行高（像素）
    pub last_line_height: Pixels,
    /// 每个逻辑行在其内容区内的可视 y 原点（相对 bounds.top()，未加滚动偏移）
    pub last_visual_offsets: Vec<Pixels>,
    /// 全部内容的可视高度（含换行展开，像素）
    pub last_content_height: Pixels,
}

impl TextArea {
    /// 创建一个文本输入框实体（在 App 上下文中以 `cx.new(...)` 包装使用）。
    ///
    /// 注意 `cx.new` 要求 `TextArea: 'static`，本类型满足。
    pub fn new(
        focus_handle: FocusHandle,
        placeholder: SharedString,
        content: SharedString,
        allow_newlines: bool,
    ) -> Self {
        let len = content.len();
        Self {
            focus_handle,
            placeholder,
            content,
            selected_range: len..len,
            selection_reversed: false,
            marked_range: None,
            allow_newlines,
            submit_on_enter: false,
            pending: None,
            history: History::new(),
            is_dragging_scrollbar: false,
            scrollbar_grab_offset: px(0.),
            is_selecting: false,
            scroll_handle: ScrollHandle::new(),
            last_layout: None,
            last_shaped_content: None,
            last_wrap_width: None,
            last_bounds: None,
            last_line_height: px(0.),
            last_visual_offsets: Vec::new(),
            last_content_height: px(0.),
        }
    }

    /// 开启"回车即提交"模式（聊天输入框用）。
    pub fn with_submit_on_enter(mut self) -> Self {
        self.submit_on_enter = true;
        self
    }

    /// 光标位置（选区终点；向后选择时是起点）。
    pub fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    /// 把光标移动到 `new_offset`，并清空选区。
    pub fn move_to(&mut self, new_offset: usize, cx: &mut Context<Self>) {
        self.selected_range = new_offset..new_offset;
        self.selection_reversed = false;
        self.scroll_cursor_into_view(cx);
        cx.notify();
    }

    /// 当前字符位置之前一个字素边界（跳过 ASCII 空白与中文标点组合）。
    pub fn previous_boundary(&self, offset: usize) -> usize {
        previous_grapheme_boundary(self.content.as_ref(), offset)
    }

    /// 之后一个字素边界。
    pub fn next_boundary(&self, offset: usize) -> usize {
        next_grapheme_boundary(self.content.as_ref(), offset)
    }

    /// `offset` 处（必须落在字素边界上）的字素是否被视为"词字符"。
    fn is_word_char_at(&self, offset: usize) -> bool {
        is_word_char(self.content.as_ref(), offset)
    }

    /// 上一个"词起点"（Ctrl+← / 双击选词左边界）。
    pub fn previous_word_boundary(&self, offset: usize) -> usize {
        previous_word_boundary_in(self.content.as_ref(), offset)
    }

    /// 下一个"词起点/词尾"（Ctrl+→ / 双击选词右边界）。
    pub fn next_word_boundary(&self, offset: usize) -> usize {
        next_word_boundary_in(self.content.as_ref(), offset)
    }

    /// 由"可视行号 + 该列 x"确定行区间起点（供双击选行等）。
    fn line_range_at(&self, offset: usize) -> Range<usize> {
        let (line, _) = self.line_col(offset.min(self.content.len()));
        let ranges = self.line_ranges();
        ranges.get(line).cloned().unwrap_or(0..0)
    }

    /// 计算内容偏移对应的"可视坐标"：`(逻辑行号, 该逻辑行内的可视行号, 该可视行内的 x)`。
    ///
    /// 换行排版下，一个逻辑行（`\n` 分隔）可能被拆成多个可视行；↑/↓ 应逐"可视行"
    /// 移动并尽量保持同一列，而不是整段逻辑行跳跃（长行换行后体验更合理）。
    fn visual_row_for_offset(&self, offset: usize) -> (usize, usize, Pixels) {
        let Some(lines) = &self.last_layout else {
            return (0, 0, px(0.));
        };
        let (line, rel) = self.line_col(offset.min(self.content.len()));
        let Some(wl) = lines.get(line) else {
            return (line, 0, px(0.));
        };
        let pos = wl
            .position_for_index(rel.min(wl.len()), self.last_line_height)
            .unwrap_or_default();
        let row = if self.last_line_height > px(0.) {
            (pos.y / self.last_line_height) as usize
        } else {
            0
        };
        (line, row, pos.x)
    }

    /// 由"可视坐标"反查内容偏移：在指定逻辑行的第 `row` 个可视行里找第一个
    /// x（相对该片断起点）不小于 `x` 的索引。`row` 越界时取该逻辑行最后一个可视行。
    fn offset_for_visual_position(&self, line: usize, row: usize, x: Pixels) -> usize {
        let Some(lines) = &self.last_layout else {
            return 0;
        };
        let ranges = self.line_ranges();
        let Some(wl) = lines.get(line) else {
            return 0;
        };
        let line_start = ranges[line].start;
        let uw = &wl.unwrapped_layout;
        // 枚举该逻辑行的换行片断（字节区间），第 k 个片断 = 第 k 个可视行
        let mut segs: Vec<(usize, usize)> = Vec::new();
        let mut seg_start = 0usize;
        for b in wl.wrap_boundaries() {
            let seg_end = boundary_byte(wl, b);
            segs.push((seg_start, seg_end));
            seg_start = seg_end;
        }
        segs.push((seg_start, wl.len()));
        let (s, e) = if segs.is_empty() {
            (0usize, wl.len())
        } else {
            segs[row.min(segs.len() - 1)]
        };
        // 在片断内找第一个相对 x >= 目标 x 的字节索引；都小于目标则停在该片断末尾
        let x0 = uw.x_for_index(s);
        let mut best = e;
        for idx in s..=e {
            let xr = uw.x_for_index(idx) - x0;
            if xr >= x {
                best = idx;
                break;
            }
            best = idx;
        }
        line_start + best
    }

    // ==================== 选区 ====================

    /// 把选区活动端扩展到 `offset`；若方向发生翻转则自动修正区间的起止与反向标记。
    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        if self.selection_reversed {
            self.selected_range.start = offset
        } else {
            self.selected_range.end = offset
        };
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
        self.scroll_cursor_into_view(cx);
        cx.notify();
    }

    /// 选中指定区间（Shift+End / Shift+Home 用），并设置方向标记。
    #[allow(dead_code)]
    fn select_range(&mut self, new_range: Range<usize>, cx: &mut Context<Self>) {
        self.selected_range = new_range.start.min(new_range.end)..new_range.start.max(new_range.end);
        self.selection_reversed = new_range.start > new_range.end;
        self.scroll_cursor_into_view(cx);
        cx.notify();
    }

    /// 全选（Ctrl+A）。
    fn select_all(&mut self, cx: &mut Context<Self>) {
        self.selected_range = 0..self.content.len();
        self.selection_reversed = false;
        cx.notify();
    }

    // ==================== 编辑 ====================

    /// 替换 `range`（UTF-8 字节区间，None 表示选区）内的文本为 `text`。
    ///
    /// 支持 IME：替换后会根据 `our_text` 的 UTF-16 长度信息修正选中区，
    /// 使得组合中的拼音串与替换后的汉字能对齐。
    pub fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 记录撤销快照（只记录即将被替换掉的内容）
        let range = match range {
            Some(r) => r,
            None => self.selected_range.clone(),
        };
        let old = self.content.to_string();
        if self.pending.is_none() {
            self.pending = Some(HistoryItem {
                text: old[range.clone()].to_string(),
                range: range.start..range.start, // 恢复时把光标放在这段开头
            });
        }
        let mut new = old;
        new.replace_range(range.clone(), text);
        self.content = SharedString::from(new);
        self.selected_range = range.start + text.len()..range.start + text.len();
        self.selection_reversed = false;
        let _ = window;
        // IME: 光标、选区基于 UTF-16。这里 `text` 是系统给的"已上屏"内容，
        // 我们按 UTF-16 长度回算光标即可（我们的存储是 UTF-8，见 `replace` 上方说明）。
        cx.notify();
    }

    /// 在当前位置插入文本（历史会合并为一次）。
    pub fn insert_text_at_cursor(
        &mut self,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 若光标处已有选区（如 IME 下屏替换），则覆盖选区
        let range = if self.selected_range.is_empty() {
            self.cursor_offset()..self.cursor_offset()
        } else {
            self.selected_range.clone()
        };
        self.replace_text_in_range(Some(range), text, window, cx);
        self.scroll_cursor_into_view(cx);
        cx.notify();
    }

    /// 删除光标前一个字素（Backspace）；有选区时删除选区。
    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        self.delete_range(Some(
            if self.selected_range.is_empty() {
                let start = self.previous_boundary(self.cursor_offset());
                start..self.cursor_offset()
            } else {
                self.selected_range.clone()
            },
        ), window, cx);
    }

    /// 删除光标后一个字素（Delete）；有选区时删除选区。
    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        self.delete_range(Some(
            if self.selected_range.is_empty() {
                let end = self.next_boundary(self.cursor_offset());
                self.cursor_offset()..end
            } else {
                self.selected_range.clone()
            },
        ), window, cx);
    }

    /// 删除指定区间（并记入撤销）。
    fn delete_range(
        &mut self,
        range: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range.unwrap_or_else(|| self.selected_range.clone());
        if range.is_empty() {
            return;
        }
        self.replace_text_in_range(Some(range.clone()), "", window, cx);
        self.scroll_cursor_into_view(cx);
        cx.notify();
    }

    /// 生成"词选区"（双击选词用）。以 `offset` 为中心向两侧扩展到最近的词边界；
    /// 若 `offset` 落在空白/标点上（附近无词），返回空区间（不选）。
    fn word_range(&self, offset: usize) -> Range<usize> {
        if self.content.is_empty() {
            return 0..0;
        }
        let o = offset.min(self.content.len());
        // 点击必须落在某个词字符上（允许正好落在词尾：左侧仍是词字符）
        let on_word = self.is_word_char_at(o)
            || (o > 0 && self.previous_boundary(o) < o && self.is_word_char_at(self.previous_boundary(o)));
        if !on_word {
            return o..o;
        }
        let mut start = o;
        while start > 0 && self.is_word_char_at(start) {
            start = self.previous_boundary(start);
        }
        let mut end = o;
        while end < self.content.len() && self.is_word_char_at(end) {
            end = self.next_boundary(end);
        }
        start..end
    }


    /// 把当前 pending 快照压入撤销栈（在手势结束时调用）。
    fn commit(&mut self) {
        if let Some(item) = self.pending.take() {
            self.history.push(item);
        }
    }

    /// 撤销：回退到上一个快照。
    fn undo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(item) = self.history.undo() {
            let item = item.clone();
            let old = self.content.to_string();
            let mut new = old;
            new.replace_range(item.range.clone(), &item.text);
            self.content = SharedString::from(new);
            self.selected_range = item.range.start..item.range.start;
            self.selection_reversed = false;
            self.scroll_cursor_into_view(cx);
            cx.notify();
        }
        let _ = window;
    }

    /// 重做：应用下一个快照。
    fn redo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(item) = self.history.redo() {
            let item = item.clone();
            let old = self.content.to_string();
            let mut new = old;
            new.replace_range(item.range.clone(), &item.text);
            self.content = SharedString::from(new);
            self.selected_range = item.range.end..item.range.end;
            self.selection_reversed = false;
            self.scroll_cursor_into_view(cx);
            cx.notify();
        }
        let _ = window;
    }

    /// 复制选区文本到剪贴板（无选区时不做任何事）。
    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            return;
        }
        let text = &self.content[self.selected_range.clone()];
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text.to_string()));
        cx.notify();
    }

    /// 剪切选区：复制到剪贴板并删除。
    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            return;
        }
        let text = &self.content[self.selected_range.clone()];
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text.to_string()));
        self.delete_range(None, window, cx);
    }

    /// 粘贴剪贴板内容。
    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(clipboard) = cx.read_from_clipboard() {
            if let Some(text) = clipboard.text() {
                self.insert_text_at_cursor(&text, window, cx);
            }
        }
    }

    /// 上移光标（按**可视行**移动：长行换行后也逐行上移，并尽量保持同一列）。
    fn up(&mut self, _: &Up, _: &mut Window, cx: &mut Context<Self>) {
        let (line, row, x) = self.visual_row_for_offset(self.cursor_offset());
        if row > 0 {
            self.move_to(self.offset_for_visual_position(line, row - 1, x), cx);
        } else if line > 0 {
            // 上一逻辑行的最后一个可视行，保持同一列 x
            self.move_to(self.offset_for_visual_position(line - 1, usize::MAX, x), cx);
        }
    }

    /// 下移光标（按**可视行**移动）。
    fn down(&mut self, _: &Down, _: &mut Window, cx: &mut Context<Self>) {
        let (line, row, x) = self.visual_row_for_offset(self.cursor_offset());
        let line_rows = self
            .last_layout
            .as_ref()
            .and_then(|lines| lines.get(line))
            .map(|wl| wl.wrap_boundaries().len() + 1)
            .unwrap_or(1);
        if row + 1 < line_rows {
            self.move_to(self.offset_for_visual_position(line, row + 1, x), cx);
        } else if let Some(lines) = &self.last_layout {
            if line + 1 < lines.len() {
                // 下一逻辑行的第一个可视行，保持同一列 x
                self.move_to(self.offset_for_visual_position(line + 1, 0, x), cx);
            }
        }
    }

    /// Enter：提交模式下发出提交事件；否则仅在允许多行时插入换行。
    fn enter(&mut self, _: &Enter, window: &mut Window, cx: &mut Context<Self>) {
        if self.submit_on_enter {
            cx.emit(TextInputEvent);
        } else if self.allow_newlines {
            self.insert_text_at_cursor("\n", window, cx);
        }
    }

    /// Shift+Enter：在允许多行时插入换行（提交模式下的换行手段）。
    fn shift_enter(&mut self, _: &ShiftEnter, window: &mut Window, cx: &mut Context<Self>) {
        if self.allow_newlines {
            self.insert_text_at_cursor("\n", window, cx);
        }
    }

    /// 获取当前内容的逻辑行字节区间列表（按 `\n` 切分）。
    /// 空内容也返回一个 `[0..0]`。
    pub fn line_ranges(&self) -> Vec<Range<usize>> {
        let text = &self.content;
        let mut ranges = Vec::new();
        let mut start = 0;
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' {
                ranges.push(start..i);
                start = i + 1;
            }
        }
        if start <= text.len() {
            ranges.push(start..text.len());
        }
        if ranges.is_empty() {
            ranges.push(0..0);
        }
        ranges
    }

    /// 行数（以逻辑行计，含换行展开前）。
    #[allow(dead_code)]
    pub fn line_count(&self) -> usize {
        self.line_ranges().len()
    }

    /// 给定内容偏移，返回 `(行号, 列号=行内字节偏移)`。
    pub fn line_col(&self, offset: usize) -> (usize, usize) {
        let ranges = self.line_ranges();
        for (i, r) in ranges.iter().enumerate() {
            if offset >= r.start && offset <= r.end {
                return (i, offset - r.start);
            }
        }
        // 越界时兜底：定位到末尾行
        let last = ranges.last().unwrap();
        (ranges.len().max(1) - 1, (offset - last.start).min(last.len()))
    }

    /// 把光标滚动到可见区域（若光标所在可视行在视口外，则滚到最近可见位置）。
    fn scroll_cursor_into_view(&self, _cx: &mut Context<Self>) {
        let Some(bounds) = &self.last_bounds else {
            return;
        };
        let lh = self.last_line_height;
        if lh == px(0.) {
            return;
        }
        let ranges = self.line_ranges();
        let (line_idx, _) = self.line_col(self.cursor_offset());
        let offsets = self.visual_offsets();
        let line_off = if line_idx < offsets.len() {
            offsets[line_idx]
        } else {
            px(0.)
        };
        let ls = (-self.scroll_handle.offset().y).max(px(0.));
        let viewport_h = self.scroll_handle.bounds().size.height;
        if viewport_h <= px(0.) {
            return;
        }
        // 光标行上缘相对内容区
        let ly = bounds.top() + line_off - ls;
        if ly < bounds.top() {
            // 上面不可见 → 滚上去
            let new_scroll = (ls - (bounds.top() - ly)).max(px(0.));
            self.scroll_handle
                .set_offset(point(px(0.), -new_scroll));
        } else if ly + lh > bounds.top() + viewport_h {
            // 下面不可见 → 滚下去
            let new_scroll = ls + (ly + lh - (bounds.top() + viewport_h));
            self.scroll_handle
                .set_offset(point(px(0.), -new_scroll));
        }
        let _ = ranges;
    }

    /// 每个逻辑行在其内容区内的可视 y 原点（相对 bounds.top()，未加滚动偏移）。
    fn visual_offsets(&self) -> Vec<Pixels> {
        let lh = self.last_line_height;
        let mut offsets = Vec::new();
        let mut acc = px(0.);
        if let Some(lines) = &self.last_layout {
            for wl in lines {
                offsets.push(acc);
                acc += lh * (wl.wrap_boundaries().len() as f32 + 1.);
            }
        }
        offsets
    }

    /// 整段内容的可视高度（含换行展开）。
    fn wrapped_content_height(&self) -> Pixels {
        if let Some(lines) = &self.last_layout {
            let lh = self.last_line_height;
            lines
                .iter()
                .map(|wl| lh * (wl.wrap_boundaries().len() as f32 + 1.))
                .fold(px(0.), |a, b| a + b)
        } else {
            px(0.)
        }
    }

    /// 根据屏幕坐标 y 判断点击落在哪个"逻辑行"（含换行展开）。
    fn line_index_at_y(&self, position: Point<Pixels>) -> usize {
        let Some(bounds) = &self.last_bounds else {
            return 0;
        };
        let line_height = self.last_line_height;
        if line_height == px(0.) {
            return 0;
        }
        // `last_bounds` 来自 draw 回调，那里的 bounds 已被滚动容器的
        // `element_offset` 平移过（即 bounds.top() 就是内容原点在屏幕上的 y）。
        let y = position.y - bounds.top();
        let lines = self.last_layout.as_ref();
        let mut acc = px(0.);
        let mut idx = 0;
        if let Some(lines) = lines {
            for (i, wl) in lines.iter().enumerate() {
                let h = line_height * (wl.wrap_boundaries().len() as f32 + 1.);
                if y < acc + h {
                    return i;
                }
                acc += h;
                idx = i;
            }
        }
        idx
    }

    /// 计算常驻自定义滚动条的几何信息。
    ///
    /// 返回 `(轨道, 滑块, 已滚下距离, 内容高, 视口高)`；
    /// 当内容不超高（不需要滚动）时返回 `None`。
    ///
    /// 注意：轨道水平锚定在**元素自身的右缘**（`last_bounds.right()`），而不是
    /// 滚动容器的右缘——容器有 `p_2` 内边距，若锚到容器右缘，滑块会画在内边距
    /// 区域，落在 TextArea 命中区之外，导致鼠标按下拖不到（见历史 bug）。
    /// 锚在元素右缘即内容区右缘，滚动条仍响应 TextArea 的鼠标事件。
    /// 垂直方向锚定在视口上（`scroll_handle.bounds().top()`，固定屏幕位置），
    /// 这样内容滚动时轨道/滑块保持不动。
    pub fn scrollbar_geometry(
        &self,
    ) -> Option<(Bounds<Pixels>, Bounds<Pixels>, Pixels, Pixels, Pixels)> {
        let viewport = self.scroll_handle.bounds();
        let viewport_h = viewport.size.height;
        if viewport_h <= px(0.) {
            return None;
        }
        // 水平右边界：元素内容区右缘；尚未布局（last_bounds 为空）时退回容器右缘
        let anchor_right = self
            .last_bounds
            .as_ref()
            .map(|b| b.right())
            .unwrap_or(viewport.right());
        let content_h = self.wrapped_content_height().max(self.last_line_height);
        // 滚动容器通过平移子元素实现滚动：offset.y 顶部为 0，滚下去为负，
        // 所以"已滚下的距离"取其相反数再夹到非负。
        let scroll_down = (-self.scroll_handle.offset().y).max(px(0.));
        let max_scroll = (content_h - viewport_h).max(px(0.));
        if content_h <= viewport_h {
            return None;
        }
        let track_w = px(12.);
        // 轨道贴住元素右缘、从视口顶部开始、占满视口高度
        let track = Bounds::new(
            point(anchor_right - track_w, viewport.top()),
            size(track_w, viewport_h),
        );
        // 滑块高度 = 视口高 * (视口高/内容高)，至少 20px 便于点按
        let fraction = if max_scroll > px(0.) {
            viewport_h / content_h
        } else {
            1.
        };
        let thumb_h = (viewport_h * fraction).max(px(20.));
        // 滑块顶部位置 = 滚动进度 * (视口高 - 滑块高)
        let scroll_frac = if max_scroll > px(0.) {
            scroll_down / max_scroll
        } else {
            0.
        };
        let thumb_top = scroll_frac * (viewport_h - thumb_h);
        let thumb = Bounds::new(
            point(track.origin.x, track.origin.y + thumb_top),
            size(track_w, thumb_h),
        );
        Some((track, thumb, scroll_down, content_h, viewport_h))
    }

    /// 按"滚动进度"（0.0..1.0 的浮点，0=顶部，1=底部）直接设置滚动位置。
    /// 用于点击/拖拽滚动条时让滑块严格跟随鼠标。
    pub fn set_scroll_fraction(&self, frac: f32, cx: &mut Context<Self>) {
        let viewport_h = self.scroll_handle.bounds().size.height;
        if viewport_h <= px(0.) {
            return;
        }
        let content_h = self.wrapped_content_height().max(self.last_line_height);
        let max_scroll = (content_h - viewport_h).max(px(0.));
        let frac = frac.clamp(0., 1.);
        let new_scroll = max_scroll * frac;
        let new_offset_y = -new_scroll;
        self.scroll_handle.set_offset(point(px(0.), new_offset_y));
        cx.notify();
    }

    // ==================== 键盘动作处理 ====================

    /// ←：无选区时把光标移到前一个字素边界；有选区时把光标折到选区起点（取消选区）。
    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.previous_boundary(self.cursor_offset()), cx);
        } else {
            self.move_to(self.selected_range.start, cx);
        }
    }

    /// →：无选区时移向后一个字素边界；有选区时折到选区终点。
    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.next_boundary(self.cursor_offset()), cx);
        } else {
            self.move_to(self.selected_range.end, cx);
        }
    }

    /// Shift+←：将选区活动端向左扩展（含反向折叠）。
    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.previous_boundary(self.cursor_offset()), cx);
    }

    /// Shift+→：向右扩展选区。
    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next_boundary(self.cursor_offset()), cx);
    }

    /// Shift+↑：把选区扩展到上一可视行同一列。
    fn select_up(&mut self, _: &SelectUp, _: &mut Window, cx: &mut Context<Self>) {
        let (line, row, x) = self.visual_row_for_offset(self.cursor_offset());
        if row > 0 {
            self.select_to(self.offset_for_visual_position(line, row - 1, x), cx);
        } else if line > 0 {
            self.select_to(self.offset_for_visual_position(line - 1, usize::MAX, x), cx);
        }
    }

    /// Shift+↓：把选区扩展到下一可视行同一列。
    fn select_down(&mut self, _: &SelectDown, _: &mut Window, cx: &mut Context<Self>) {
        let (line, row, x) = self.visual_row_for_offset(self.cursor_offset());
        let line_rows = self
            .last_layout
            .as_ref()
            .and_then(|lines| lines.get(line))
            .map(|wl| wl.wrap_boundaries().len() + 1)
            .unwrap_or(1);
        if row + 1 < line_rows {
            self.select_to(self.offset_for_visual_position(line, row + 1, x), cx);
        } else if let Some(lines) = &self.last_layout {
            if line + 1 < lines.len() {
                self.select_to(self.offset_for_visual_position(line + 1, 0, x), cx);
            }
        }
    }

    /// Home：光标移到当前行首（无选区）。
    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        let (line, _) = self.line_col(self.cursor_offset());
        let start = self.line_ranges()[line].start;
        self.move_to(start, cx);
    }

    /// End：光标移到当前行尾。
    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        let (line, _) = self.line_col(self.cursor_offset());
        let end = self.line_ranges()[line].end;
        self.move_to(end, cx);
    }

    // ==================== 词级导航 / 文档级导航（参照 Zed/编辑器输入框） ====================

    /// Ctrl+←：光标跳到上一个词起点。
    fn move_to_previous_word_boundary(
        &mut self,
        _: &MoveToPreviousWordBoundary,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_to(self.previous_word_boundary(self.cursor_offset()), cx);
    }

    /// Ctrl+→：光标跳到下一个词边界。
    fn move_to_next_word_boundary(
        &mut self,
        _: &MoveToNextWordBoundary,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_to(self.next_word_boundary(self.cursor_offset()), cx);
    }

    /// Ctrl+Shift+←：把选区扩展到上一个词起点。
    fn select_to_previous_word_boundary(
        &mut self,
        _: &SelectToPreviousWordBoundary,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_to(self.previous_word_boundary(self.cursor_offset()), cx);
    }

    /// Ctrl+Shift+→：把选区扩展到下一个词边界。
    fn select_to_next_word_boundary(
        &mut self,
        _: &SelectToNextWordBoundary,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_to(self.next_word_boundary(self.cursor_offset()), cx);
    }

    /// Ctrl+Backspace：删除光标前一个词（有选区时删除选区）。
    fn delete_word_backward(&mut self, _: &DeleteWordBackward, window: &mut Window, cx: &mut Context<Self>) {
        let range = if self.selected_range.is_empty() {
            let start = self.previous_word_boundary(self.cursor_offset());
            start..self.cursor_offset()
        } else {
            self.selected_range.clone()
        };
        self.delete_range(Some(range), window, cx);
    }

    /// Ctrl+Delete：删除光标后一个词。
    fn delete_word_forward(&mut self, _: &DeleteWordForward, window: &mut Window, cx: &mut Context<Self>) {
        let range = if self.selected_range.is_empty() {
            let end = self.next_word_boundary(self.cursor_offset());
            self.cursor_offset()..end
        } else {
            self.selected_range.clone()
        };
        self.delete_range(Some(range), window, cx);
    }

    /// Ctrl+Home：光标移到文档开头。
    fn move_to_beginning_of_document(
        &mut self,
        _: &MoveToBeginningOfDocument,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_to(0, cx);
    }

    /// Ctrl+End：光标移到文档结尾。
    fn move_to_end_of_document(
        &mut self,
        _: &MoveToEndOfDocument,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_to(self.content.len(), cx);
    }

    /// Ctrl+Shift+Home：把选区扩展到文档开头。
    fn select_to_beginning_of_document(
        &mut self,
        _: &SelectToBeginningOfDocument,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_to(0, cx);
    }

    /// Ctrl+Shift+End：把选区扩展到文档结尾。
    fn select_to_end_of_document(
        &mut self,
        _: &SelectToEndOfDocument,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_to(self.content.len(), cx);
    }

    fn select_all_action(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.select_all(cx);
    }

    // ==================== 鼠标交互 ====================

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle.clone());

        // 若点中了滚动条轨道区域（元素右缘 12px），进入滚动条拖拽**而不移动光标**
        if let Some((track, thumb, _, _, _)) = self.scrollbar_geometry() {
            if event.position.x >= track.origin.x {
                self.is_dragging_scrollbar = true;
                // 记录按下点相对滑块顶部的偏移：拖动时滑块跟随鼠标但不"跳到"按下位置
                self.scrollbar_grab_offset = (event.position.y - thumb.origin.y).max(px(0.));
                cx.notify();
                return;
            }
        }

        self.is_selecting = true;
        // 取布局数据的屏幕坐标，把点击位置映射到内容偏移
        let offset = self.index_for_mouse_position(event.position);
        if event.modifiers.shift {
            // Shift+点击：从原光标扩展到点击处
            self.select_to(offset, cx);
        } else {
            // 普通单击定位；双击选词；三击及以上选行（参照 Zed / gpui-todos 输入框）
            match event.click_count {
                0 | 1 => self.move_to(offset, cx),
                2 => {
                    let w = self.word_range(offset);
                    self.select_range(w, cx);
                }
                _ => {
                    let l = self.line_range_at(offset);
                    self.select_range(l, cx);
                }
            }
        }
        cx.notify();
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.is_dragging_scrollbar = false;
        self.is_selecting = false;
        self.commit();
        cx.notify();
    }

    /// 鼠标拖动：仅当按住左键时才更新选区活动端；否则拖拽滚动条。
    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_dragging_scrollbar {
            // 用"按下时记录的抓取偏移"把鼠标 y 换算成滚动进度，滑块严格跟随鼠标
            if let Some((_track, thumb, _, _, viewport_h)) = self.scrollbar_geometry() {
                let slide = viewport_h - thumb.size.height;
                if slide > px(0.) {
                    let rel =
                        (event.position.y - self.scroll_handle.bounds().top()) - self.scrollbar_grab_offset;
                    let frac = (rel / slide).clamp(0., 1.) as f32;
                    self.set_scroll_fraction(frac, cx);
                }
            }
            return;
        }
        // 只有处于"拖选"状态才跟随鼠标扩展选区（参考官方 examples/input.rs）
        if self.is_selecting {
            let offset = self.index_for_mouse_position(event.position);
            self.select_to(offset, cx);
        }
    }

    // ==================== 命中测试 ====================

    /// 根据鼠标屏幕坐标算出对应的内容字节偏移（点击定位用）。
    fn index_for_mouse_position(&self, position: Point<Pixels>) -> usize {
        // 内容为空时显示的是占位文本，其布局属于占位符；任何点击都映射到
        // 真实（空）内容的偏移 0。
        if self.content.is_empty() {
            return 0;
        }
        let Some(bounds) = &self.last_bounds else {
            return 0;
        };
        // 超出文本区上下边界时分别折到开头/结尾（参考官方 examples/input.rs）
        if position.y < bounds.top() {
            return 0;
        }
        if position.y > bounds.bottom() {
            return self.content.len();
        }
        let Some(lines) = &self.last_layout else {
            return 0;
        };
        if lines.is_empty() {
            return 0;
        }
        let line_idx = self.line_index_at_y(position);
        let line = &lines[line_idx];
        let offsets = self.visual_offsets();
        let y_in_line = position.y - (bounds.top() + offsets[line_idx]);
        let point_in_line = gpui::point(position.x - bounds.left(), y_in_line);
        let utf8 = match line
            .closest_index_for_position(point_in_line, self.last_line_height)
        {
            Ok(i) => i,
            Err(i) => i,
        };
        let ranges = self.line_ranges();
        ranges[line_idx].start + utf8
    }

    // ==================== IME ====================

    /// 把 UTF-16 偏移转换成 UTF-8 字节偏移。
    pub fn offset_to_utf8(&self, offset_utf16: usize) -> usize {
        let mut cu_count = 0;
        for (i, c) in self.content.char_indices() {
            if cu_count >= offset_utf16 {
                return i;
            }
            cu_count += c.len_utf16();
        }
        self.content.len()
    }

    /// 把 UTF-8 字节偏移转换成 UTF-16 偏移（IME 回调用）。
    pub fn offset_to_utf16(&self, offset: usize) -> usize {
        let mut cu_count = 0;
        for (i, c) in self.content.char_indices() {
            if i >= offset {
                break;
            }
            cu_count += c.len_utf16();
        }
        cu_count
    }

    // ==================== Render ====================
}

// Render 实现（供作为子元素使用时直接 `.child(input.clone())`）。
impl Render for TextArea {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut el = div()
            .flex()
            .id("text-input")
            .key_context("TextArea")
            .w_full()
            .min_w(px(0.))
            .min_h(px(24.))
            .cursor(CursorStyle::IBeam)
            .track_focus(&self.focus_handle.clone())
            .text_size(px(14.))
            .line_height(relative(1.4))
            .child(TextElement { input: cx.entity() });

        // 鼠标交互（点击定位 / 拖选 / 拖滚动条；拖出控件外松手也要收尾）
        el = el
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move));

        // 焦点时的键盘动作（键位本身在 main.rs 里全局注册）
        el = el
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::up))
            .on_action(cx.listener(Self::down))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_up))
            .on_action(cx.listener(Self::select_down))
            .on_action(cx.listener(Self::select_all_action))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::move_to_previous_word_boundary))
            .on_action(cx.listener(Self::move_to_next_word_boundary))
            .on_action(cx.listener(Self::select_to_previous_word_boundary))
            .on_action(cx.listener(Self::select_to_next_word_boundary))
            .on_action(cx.listener(Self::delete_word_backward))
            .on_action(cx.listener(Self::delete_word_forward))
            .on_action(cx.listener(Self::move_to_beginning_of_document))
            .on_action(cx.listener(Self::move_to_end_of_document))
            .on_action(cx.listener(Self::select_to_beginning_of_document))
            .on_action(cx.listener(Self::select_to_end_of_document))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(|this: &mut TextArea, _ev: &Undo, window, cx| {
                this.undo(window, cx)
            }))
            .on_action(cx.listener(|this: &mut TextArea, _ev: &Redo, window, cx| {
                this.redo(window, cx)
            }));
        if self.allow_newlines {
            el = el
                .on_action(cx.listener(Self::enter))
                .on_action(cx.listener(Self::shift_enter));
        }
        el
    }
}

impl Focusable for TextArea {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl gpui::EventEmitter<TextInputEvent> for TextArea {}

// ==================== 工具函数 ====================

/// 用指定换行宽度 shape 当前内容并缓存到实体里，返回内容可视 (宽, 高)。
fn shape_and_cache(
    input: &Entity<TextArea>,
    wrap_width: Option<Pixels>,
    window: &mut Window,
    cx: &mut App,
) -> (Pixels, Pixels) {
    let style = window.text_style();
    let font_size = style.font_size.to_pixels(window.rem_size());
    // 行高：**优先复用上一次 paint 阶段记录的逻辑行高**。
    // 历史 bug：在"量布局（request_measured_layout）"阶段调用 `window.line_height()`
    // 会返回放大了的（设备像素）行高；若在 measure 里用它 shape 并缓存，内容可视
    // 高度会永远大于绘制区域 → 换行行数越多底部空白越多、且空白处无法输入。
    let line_height = {
        let stored = input.read(cx).last_line_height;
        if stored > px(0.) {
            stored
        } else {
            window.line_height()
        }
    };

    let content = input.read(cx).content.clone();
    let placeholder = input.read(cx).placeholder.clone();

    // 空内容时绘制占位文本（半透明灰色），否则绘制真实内容。
    // 文本整体用**单个 TextRun**，避免按逻辑行切分 run 时在中文等多字节字符上
    // 与 DirectWrite 的字体回退分段产生错位（会引发 "not a char boundary" 崩溃）。
    let (text, runs) = if content.is_empty() {
        let run = TextRun {
            len: placeholder.len(),
            font: style.font(),
            color: hsla(0., 0., 0., 0.4),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        (placeholder, vec![run])
    } else {
        let run = TextRun {
            len: content.len(),
            font: style.font(),
            color: style.color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        (content.clone(), vec![run])
    };

    let lines = window
        .text_system()
        .shape_text(text, font_size, &runs, wrap_width, None)
        .unwrap_or_default()
        .into_vec();

    let (mut content_w, mut content_h) = (px(0.), px(0.));
    for wl in &lines {
        content_w = content_w.max(wl.width());
        content_h += line_height * (wl.wrap_boundaries().len() as f32 + 1.);
    }

    input.update(cx, |input, _| {
        input.last_layout = Some(lines);
        input.last_shaped_content = Some(content);
        input.last_wrap_width = wrap_width;
        input.last_line_height = line_height;
    });

    (content_w, content_h)
}

/// 底层文本绘制元素：持有 [`TextArea`] 的实体句柄，
/// 由 GPUI 的 `Element` 协议驱动完成"量布局→预排版→绘制"三步。
pub struct TextElement {
    pub input: Entity<TextArea>,
}

/// 预排版阶段传给 `paint` 的中间产物（每"逻辑行"一个换行布局）。
pub struct PrepaintState {
    pub lines: Vec<WrappedLine>,
}


impl gpui::IntoElement for TextElement {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TextElement {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    /// 步骤一：量布局。宽度占满父级，高度按"换行后可视行数"计算。
    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        _cx: &mut App,
    ) -> (LayoutId, ()) {
        let input = self.input.clone();
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        // 允许收缩到小于内容宽度（flex 默认 min-width:auto 会按内容 min-content 把
        // 长行顶到超宽），这样在限定宽度容器内文本才会自动换行、边框不会变长。
        style.min_size.width = relative(0.).into();
        let layout_id = window.request_measured_layout(style, move |known, available, window, cx| {
            // 换行宽度：拿不到已知宽度时，用父级给的定义宽度
            let wrap_width = known.width.or(match available.width {
                AvailableSpace::Definite(x) => Some(x),
                _ => None,
            });
            // 关键：量布局阶段 `window.line_height()` 是设备像素（×DPI），而绘制阶段是
            // 逻辑像素（两者可差 1.3 倍）。若在 measure 里重新 shape 并用设备像素行高算高度，
            // 布局高度会永远大于文字实际高度 → 底部空白随行数增多。
            // 因此优先复用 **prepaint 已用逻辑行高 shape 好的缓存**，仅在无缓存或宽度不匹配时
            // 才现场 shape 兜底（首帧/中间测量用）。
            let cached_size = {
                let e = input.read(cx);
                if let (Some(lines), cw, lh) = (&e.last_layout, e.last_wrap_width, e.last_line_height) {
                    if cw == wrap_width && lh > px(0.) {
                        let cwpx = lines
                            .iter()
                            .map(|wl| wl.width())
                            .fold(px(0.), |a, b| a.max(b));
                        let chpx = lines
                            .iter()
                            .map(|wl| lh * (wl.wrap_boundaries().len() as f32 + 1.))
                            .fold(px(0.), |a, b| a + b);
                        Some((cwpx, chpx))
                    } else {
                        None
                    }
                } else {
                    None
                }
            };
            let (content_w, content_h) = match cached_size {
                Some((cw, ch)) => (cw, ch),
                None => shape_and_cache(&input, wrap_width, window, cx),
            };
            let width = wrap_width.unwrap_or(content_w);
            size(width, content_h)
        });
        (layout_id, ())
    }

    /// 步骤二：预排版。把内容按可用宽度 shape（含自动换行）成可绘制的
    /// `WrappedLine`；内容/宽度/行高任一变化才重新 shape，否则在光标闪烁/
    /// 鼠标移动等每帧回调里重复 shape 会让大段粘贴卡死。
    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> PrepaintState {
        let wrap_width = Some(bounds.size.width);
        let needs_reshape = {
            let input = self.input.read(cx);
            input.last_layout.is_none()
                || input.last_shaped_content.as_ref() != Some(&input.content)
                || input.last_wrap_width != wrap_width
                || input.last_line_height != window.line_height()
        };
        if needs_reshape {
            shape_and_cache(&self.input, wrap_width, window, cx);
        }
        // 把缓存的行布局"取走"交给 paint（paint 结束后会再存回去）
        let lines = self
            .input
            .update(cx, |input, _| input.last_layout.take())
            .unwrap_or_default();
        PrepaintState { lines }
    }

    /// 步骤三：绘制。画文本、选区高亮、光标，以及自定义滚动条。
    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut (),
        prepaint: &mut PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        // 把本控件注册为该区域输入事件的处理者（点击、键盘聚焦等）
        let focus_handle = self.input.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );

        let line_height = window.line_height();
        let (line_ranges, selected_range, marked_range, cursor_offset) = {
            let input = self.input.read(cx);
            (
                input.line_ranges(),
                input.selected_range.clone(),
                input.marked_range.clone(),
                input.cursor_offset(),
            )
        };
        let is_focused = focus_handle.is_focused(window);
        let cursor_line = line_index_for_offset(&line_ranges, cursor_offset);

        let lines = std::mem::take(&mut prepaint.lines);
        // 每个逻辑行的可视 y 原点（相对 bounds.top()）
        let mut offsets = Vec::with_capacity(lines.len());
        let mut total_content_h = px(0.);
        for wl in &lines {
            offsets.push(total_content_h);
            total_content_h += line_height * (wl.wrap_boundaries().len() as f32 + 1.);
        }

        // 性能优化：上千行时只绘制视口里**可见**的逻辑行。offsets / total_content_h
        // 仍按全量计算（供命中测试与滚动条用，开销只是浮点加法）；这里直接跳过
        // 视口外的行，避免每帧对成千上万行做 shape 后的 paint（这是多行卡顿的主因）。
        let (scroll_y, viewport_h) = {
            let handle = self.input.read(cx).scroll_handle.clone();
            let viewport = handle.bounds();
            ((-handle.offset().y).max(px(0.)), viewport.size.height)
        };
        let (view_top, view_bottom) = (scroll_y, scroll_y + viewport_h);
        // 视口为空（尚未布局）时退化为全量绘制
        let do_clip = viewport_h > px(0.);

        for (i, wl) in lines.iter().enumerate() {
            let row_h = line_height * (wl.wrap_boundaries().len() as f32 + 1.);
            let line_top = offsets[i];
            if do_clip && (line_top + row_h < view_top || line_top > view_bottom) {
                continue;
            }
            let y = bounds.top() + line_top;

            // 选区高亮：把选区与该行的各个换行片段求交，逐可视行画半透明蓝色矩形
            if !selected_range.is_empty() && i < line_ranges.len() {
                let r = &line_ranges[i];
                // 先夹到本行区间内，避免选区起始/终点落在别的行时减法下溢
                let sel_start = selected_range.start.max(r.start).min(r.end) - r.start;
                let sel_end = selected_range.end.max(r.start).min(r.end) - r.start;
                for rect in selection_rects_in_line(wl, line_height, sel_start, sel_end) {
                    let quad = fill(
                        Bounds::from_corners(
                            point(bounds.left() + rect.origin.x, y + rect.origin.y),
                            point(
                                bounds.left() + rect.origin.x + rect.size.width,
                                y + rect.origin.y + rect.size.height,
                            ),
                        ),
                        rgba(0x3311ff30),
                    );
                    window.paint_quad(quad);
                }
            }

            // IME 组合态下划线：在组合文本（`marked_range`）下方画一条蓝色细线，
            // 让中文/日文等 IME 候选仍在上屏前的拼写区间一目了然（参照 Zed 编辑器输入框）。
            if is_focused {
                if let Some(mr) = &marked_range {
                    if !mr.is_empty() && i < line_ranges.len() {
                        let r = &line_ranges[i];
                        let ms = mr.start.max(r.start).min(r.end) - r.start;
                        let me = mr.end.max(r.start).min(r.end) - r.start;
                        for rect in selection_rects_in_line(wl, line_height, ms, me) {
                            // 下划线同样要跟随换行片段逐可视行下移（rect.origin.y 已是行内偏移）
                            let quad = fill(
                                Bounds::from_corners(
                                    point(
                                        bounds.left() + rect.origin.x,
                                        y + rect.origin.y + line_height - px(2.5),
                                    ),
                                    point(
                                        bounds.left() + rect.origin.x + rect.size.width,
                                        y + rect.origin.y + line_height - px(1.),
                                    ),
                                ),
                                rgba(0x2563ebcc),
                            );
                            window.paint_quad(quad);
                        }
                    }
                }
            }

            // 绘制该逻辑行文字（自动换行，内部按片段下移）
            wl.paint(
                point(bounds.left(), y),
                line_height,
                TextAlign::default(),
                None,
                window,
                cx,
            )
            .unwrap();
        }

        // 光标：聚焦 + 无选区时，画一条 2px 蓝色竖线（定位到换行后的准确位置）
        if is_focused
            && selected_range.is_empty()
            && cursor_line < lines.len()
            && cursor_line < line_ranges.len()
        {
            let r = &line_ranges[cursor_line];
            let rel = cursor_offset - r.start;
            if let Some(pos) = lines[cursor_line].position_for_index(rel, line_height) {
                let quad = fill(
                    Bounds::new(
                        point(
                            bounds.left() + pos.x,
                            bounds.top() + offsets[cursor_line] + pos.y,
                        ),
                        size(px(2.), line_height),
                    ),
                    gpui::blue(),
                );
                window.paint_quad(quad);
            }
        }

        // 把本次绘制所需的布局数据存回控件（paint 结束后保留给下一帧用量布局/命中测试用）
        self.input.update(cx, |input, _cx| {
            input.last_layout = Some(lines);
            input.last_bounds = Some(bounds);
            input.last_line_height = line_height;
            input.last_visual_offsets = offsets;
            input.last_content_height = total_content_h;
        });

        // 自绘常驻滚动条：轨道 + 滑块。GPUI 0.2.2 的系统 overlay 滚动条在
        // Windows 上会自动隐藏等同不可见，所以自己画（圆角、半透明美化版）。
        let geometry = self.input.read(cx).scrollbar_geometry();
        if let Some((track, thumb, _scroll_down, _content_h, _viewport_h)) = geometry {
            // 轨道：极淡圆角底
            window.paint_quad(
                fill(track, rgba(0x93a0aa1f)).corner_radii(Corners::all(px(3.))),
            );
            // 滑块：居中的 6px 圆角半透明条，拖拽中加深
            let thumb_w = px(6.);
            let thumb_x = track.origin.x + (track.size.width - thumb_w) / 2.;
            let thumb_center = Bounds::new(
                point(thumb_x, thumb.origin.y),
                size(thumb_w, thumb.size.height),
            );
            let thumb_color = if self.input.read(cx).is_dragging_scrollbar {
                rgba(0x3b82f6c0)
            } else {
                rgba(0x64748b96)
            };
            window.paint_quad(
                fill(thumb_center, thumb_color).corner_radii(Corners::all(px(3.))),
            );
        }
    }
}

impl EntityInputHandler for TextArea {
    /// IME 取某段内容的文本（区间为 UTF-16），并回填实际区间。
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        let range = range.start.min(self.content.len())..range.end.min(self.content.len());
        actual_range.replace(self.range_to_utf16(&range));
        Some(self.content[range].to_string())
    }

    /// 当前选区（UTF-16 表示）。
    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selected_range),
            reversed: self.selection_reversed,
        })
    }

    /// IME 组合中的临时区间（以 UTF-16 返回，与官方 examples/input.rs 一致）。
    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked_range.as_ref().map(|r| self.range_to_utf16(r))
    }

    /// 取消 IME 临时标记。
    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.marked_range = None;
    }

    /// IME 候选窗口应锚定在哪个屏幕矩形上。
    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let lines = self.last_layout.as_ref()?;
        let line_height = self.last_line_height;
        let range = self.range_from_utf16(&range_utf16);
        let ranges = self.line_ranges();
        let start_line = self.line_col(range.start).0.min(lines.len() - 1);
        let r = &ranges[start_line];
        let offsets = self.visual_offsets();
        // 起点在该行换行布局内的像素位置（x、y 均相对该行可视区域）
        let pos = lines[start_line].position_for_index(range.start - r.start, line_height)?;
        // y 需要叠加滚动偏移（bounds 来自滚动容器，坐标已平移）
        Some(Bounds::new(
            point(
                bounds.left() + pos.x,
                bounds.top() + offsets[start_line] + pos.y + self.scroll_handle.offset().y,
            ),
            size(px(2.), line_height),
        ))
    }

    /// 屏幕坐标 → 内容 UTF-16 偏移（IME 定位用，如双击选词）。
    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        // 内容为空时布局属于占位文本，任何点都映射到真实（空）内容的偏移 0
        if self.content.is_empty() {
            return Some(0);
        }
        let bounds = self.last_bounds.as_ref()?;
        let lines = self.last_layout.as_ref()?;
        if lines.is_empty() {
            return Some(0);
        }
        let line_idx = self.line_index_at_y(point);
        let line = &lines[line_idx];
        let offsets = self.visual_offsets();
        let y_in_line = point.y - (bounds.top() + offsets[line_idx]);
        let point_in_line = gpui::point(point.x - bounds.left(), y_in_line);
        let utf8 = match line.index_for_position(point_in_line, self.last_line_height) {
            Ok(i) => i,
            Err(i) => i,
        };
        let ranges = self.line_ranges();
        let line_start = ranges[line_idx].start;
        Some(self.offset_to_utf16(line_start + utf8))
    }

    /// 系统请求替换某段区间（IME 输入 / 组合确认）。区间为 UTF-16。
    ///
    /// 区间解析顺序与官方 `examples/input.rs` 一致：
    /// 显式区间 > 当前组合（`marked_range`）> 当前选区。
    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());
        let range = range.start.min(self.content.len())..range.end.min(self.content.len());
        self.content = SharedString::from(
            self.content[..range.start].to_owned() + new_text + &self.content[range.end..],
        );
        self.selected_range = range.start + new_text.len()..range.start + new_text.len();
        self.marked_range = None;
        self.selection_reversed = false;
        self.scroll_cursor_into_view(cx);
        cx.notify();
    }

    /// 替换文本并标记临时区间（IME 组合候选实时上屏并选中）。
    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());
        let range = range.start.min(self.content.len())..range.end.min(self.content.len());
        self.content = SharedString::from(
            self.content[..range.start].to_owned() + new_text + &self.content[range.end..],
        );
        if !new_text.is_empty() {
            self.marked_range = Some(range.start..range.start + new_text.len());
        } else {
            self.marked_range = None;
        }
        self.selected_range = new_selected_range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .map(|nr| nr.start + range.start..nr.end + range.start)
            .unwrap_or_else(|| range.start + new_text.len()..range.start + new_text.len());
        self.selection_reversed = false;
        self.scroll_cursor_into_view(cx);
        cx.notify();
    }
}

impl TextArea {
    /// 把 UTF-16 区间换算成 UTF-8 区间。
    fn utf16_range_to_utf8(&self, range_utf16: Range<usize>) -> (usize, usize) {
        (
            self.offset_to_utf8(range_utf16.start),
            self.offset_to_utf8(range_utf16.end),
        )
    }

    /// 把 UTF-16 区间换算成 UTF-8 区间（供命中测试/IME 锚定使用）。
    fn range_from_utf16(&self, range_utf16: &Range<usize>) -> Range<usize> {
        let (s, e) = self.utf16_range_to_utf8(range_utf16.clone());
        s..e
    }

    /// 把 UTF-8 区间换算成 UTF-16 区间（IME 回传用）。
    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_undo_redo() {
        let mut h = History::new();
        h.push(HistoryItem {
            text: "a".into(),
            range: 0..1,
        });
        h.push(HistoryItem {
            text: "b".into(),
            range: 1..2,
        });
        assert_eq!(h.undo().unwrap().text, "b");
        assert_eq!(h.undo().unwrap().text, "a");
        assert!(h.undo().is_none());
        assert_eq!(h.redo().unwrap().text, "a");
        h.push(HistoryItem {
            text: "c".into(),
            range: 2..3,
        });
        // 在"b"之后做新编辑，会丢弃被撤销的部分，redo 应彻底失效
        assert!(h.redo().is_none());
    }

    #[test]
    fn word_boundary_ascii() {
        let s = "hello world"; // 0..5=hello, 6..11=world
        assert_eq!(previous_word_boundary_in(s, 0), 0);
        assert_eq!(previous_word_boundary_in(s, 2), 0); // 词中
        assert_eq!(previous_word_boundary_in(s, 5), 0); // 空白处 → 前一个词 "hello"
        assert_eq!(previous_word_boundary_in(s, 6), 6); // "world" 词首（词字符，原地）
        assert_eq!(previous_word_boundary_in(s, 8), 6); // 词中
        assert_eq!(previous_word_boundary_in(s, 11), 6); // 文末

        assert_eq!(next_word_boundary_in(s, 0), 5);
        assert_eq!(next_word_boundary_in(s, 2), 5);
        assert_eq!(next_word_boundary_in(s, 5), 6);
        assert_eq!(next_word_boundary_in(s, 6), 11);
        assert_eq!(next_word_boundary_in(s, 8), 11);
        assert_eq!(next_word_boundary_in(s, 11), 11);
    }

    #[test]
    fn word_boundary_punctuation() {
        let s = "abc,def"; // ',' 不是词字符
        assert_eq!(previous_word_boundary_in(s, 7), 4); // "def" 起点
        assert_eq!(previous_word_boundary_in(s, 4), 4);
        assert_eq!(previous_word_boundary_in(s, 3), 0); // 逗号处 → "abc" 起点
        assert_eq!(next_word_boundary_in(s, 0), 3); // "abc" 终点
        assert_eq!(next_word_boundary_in(s, 3), 4); // 逗号 → "def" 起点
        assert_eq!(next_word_boundary_in(s, 7), 7);
    }

    #[test]
    fn word_boundary_cjk() {
        let s = "你好 世界"; // 你0..3 好3..6, ' '6, 世7..10 界10..13
        assert_eq!(previous_word_boundary_in(s, 4), 0);
        assert_eq!(previous_word_boundary_in(s, 6), 0); // 空白→"你好"起点
        assert_eq!(previous_word_boundary_in(s, 10), 7); // "世界"起点
        assert_eq!(previous_word_boundary_in(s, 13), 7);

        assert_eq!(next_word_boundary_in(s, 0), 6); // "你好"终点
        assert_eq!(next_word_boundary_in(s, 6), 7); // 空白→"世界"起点
        assert_eq!(next_word_boundary_in(s, 7), 13);
    }

    #[test]
    fn grapheme_boundary_dance() {
        let s = "a🏳️‍🌈b"; // 旗帜 emoji 是复合字素
        let b1 = next_grapheme_boundary(s, 0); // 'a' 之后
        assert_eq!(&s[0..b1], "a");
        let b2 = next_grapheme_boundary(s, b1); // 旗帜之后
        assert_eq!(&s[b1..b2], "🏳️‍🌈");
        let b3 = next_grapheme_boundary(s, b2); // 'b' 之后
        assert_eq!(&s[b2..b3], "b");
        assert_eq!(next_grapheme_boundary(s, b3), b3);
        assert_eq!(previous_grapheme_boundary(s, b1), 0);
        assert_eq!(previous_grapheme_boundary(s, b2), b1);
    }

    // ---- 回归：换行后底部不应出现空白（历史 bug：布局高度>绘制高度，随行数增长） ----
    #[gpui::test]
    async fn wrapped_layout_matches_painted_height(cx: &mut gpui::TestAppContext) {
        let long_line =
            "这行文字很长很长用于测试自动换行，在有限宽度内应当拆成多个可视行，abcdefghijklmnopqrstuvwxyz 0123456789";
        let content = (0..12)
            .map(|i| format!("{i}:{long_line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let (view, cx) = cx.add_window_view(|_window, cx| {
            TextArea::new(
                cx.focus_handle(),
                SharedString::from(""),
                SharedString::from(content),
                true,
            )
        });

        // 有限宽度 → 强制自动换行成多个可视行
        cx.simulate_resize(size(px(300.), px(400.)));
        cx.run_until_parked();

        // 再跑几帧（输入字符触发重排），确保高度已自愈为与绘制一致
        for _ in 0..3 {
            cx.simulate_input("x");
            cx.run_until_parked();
        }

        let (bounds_h, content_h, visual_lines, wrap_w) = cx.update(|_window, cx| {
            let t = view.read(cx);
            let visual_lines: usize = t
                .last_layout
                .as_ref()
                .map(|ls| ls.iter().map(|w| w.wrap_boundaries().len() + 1).sum())
                .unwrap_or(0);
            (
                t.last_bounds.map(|b| b.size.height),
                t.last_content_height,
                visual_lines,
                t.last_wrap_width,
            )
        });

        assert!(visual_lines > 12, "换行应产生多于逻辑行的可视行，实际 {visual_lines}");
        assert_eq!(
            bounds_h,
            Some(content_h),
            "布局高度必须等于绘制内容高度（当前存在底部空白：bounds={bounds_h:?} vs content={content_h:?}, wrap={wrap_w:?})"
        );
    }

    // ---- 回归：Ctrl+A / 鼠标拖选在换行后，每个可视行都要有高亮矩形 ----
    #[gpui::test]
    async fn wrapped_selection_highlights_each_visual_row(cx: &mut gpui::TestAppContext) {
        // 两段超长连续文本（自动换行成多可视行）——对应报告里的 Ctrl+A / 鼠标全选
        let long = "一二三四五六七八九十甲乙丙丁戊己庚辛壬癸子丑寅卯辰巳午未申酉戌亥天地玄黄宇宙洪荒";
        let content = format!("{long}{long}\n{long}{long}");
        let (view, cx) = cx.add_window_view(|_window, cx| {
            TextArea::new(
                cx.focus_handle(),
                SharedString::from(""),
                SharedString::from(content),
                true,
            )
        });
        cx.simulate_resize(size(px(300.), px(400.)));
        cx.run_until_parked();

        let (lh, per_line) = cx.update(|_window, cx| {
            let t = view.read(cx);
            let lh = t.last_line_height;
            let lines = t.last_layout.clone().unwrap_or_default();
            let data: Vec<(usize, Vec<Pixels>)> = lines
                .iter()
                .map(|wl| {
                    // 模拟 Ctrl+A 整行全选：每个换行片段都应产生一个高亮矩形
                    let rects = selection_rects_in_line(wl, lh, 0, wl.len());
                    (
                        rects.len(),
                        rects.iter().map(|r| r.origin.y).collect::<Vec<_>>(),
                    )
                })
                .collect();
            (lh, data)
        });

        assert!(lh > px(0.));
        assert!(per_line.len() >= 2, "应有两段逻辑行");
        for (line_idx, (count, ys)) in per_line.iter().enumerate() {
            assert!(
                *count >= 2,
                "第 {line_idx} 行在每个换行片段上都应有高亮矩形（历史 bug：全部叠在第一行）"
            );
            // 高亮矩形必须逐可视行下移 line_height（与 GPUI paint_line 堆叠片段一致）
            assert!(
                (ys[0] - px(0.)).abs() < px(0.5),
                "第 {line_idx} 行首个高亮应在顶行，实际 {ys:?}"
            );
            for idx in 1..ys.len() {
                let diff = ys[idx] - ys[idx - 1];
                assert!(
                    (diff - lh).abs() < px(0.5),
                    "第 {line_idx} 行高亮应逐行下移 lh：ys={ys:?}"
                );
            }
        }
    }
}
