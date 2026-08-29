//! 文本输入层的**公共共享部分**：动作类型、事件、历史栈、字素/词边界、
//! UTF-16 换算、换行选区矩形等纯逻辑。被 [`crate::input::text_area`]（多行）与
//! [`crate::input::text_box`]（单行）复用，避免两处各自维护一份。

use std::ops::Range;
use std::path::PathBuf;

use gpui::{actions, Bounds, Pixels};
use unicode_segmentation::UnicodeSegmentation;

// `actions!` 宏一次性声明一组"动作类型"，每个动作都是零大小的结构体；
// 之后通过 `KeyBinding` + `on_action` 把键盘输入路由到对应的处理函数。
// 例如按"←"触发 `Left` 动作，进而调用输入控件的 `left`。
actions!(
    input,
    [
        Backspace,
        Delete,
        Left,
        Right,
        Up,
        Down,
        SelectLeft,
        SelectRight,
        SelectUp,
        SelectDown,
        SelectAll,
        Home,
        End,
        MoveToPreviousWordBoundary,
        MoveToNextWordBoundary,
        SelectToPreviousWordBoundary,
        SelectToNextWordBoundary,
        DeleteWordBackward,
        DeleteWordForward,
        MoveToBeginningOfDocument,
        MoveToEndOfDocument,
        SelectToBeginningOfDocument,
        SelectToEndOfDocument,
        ShowCharacterPalette,
        Paste,
        Cut,
        Copy,
        Enter,
        ShiftEnter,
        Undo,
        Redo,
        Quit,
        Save,
        NewFile,
        OpenFile,
        TogglePreview,
        IncreaseFontSize,
        DecreaseFontSize,
        ResetFontSize,
    ]
);

/// 文本输入控件对外发出的"提交"事件（例如聊天输入框的"回车发送"）。
pub struct TextInputEvent;

/// 文本输入控件内容发生**编辑**的事件（打字/粘贴/删除/撤销/重做/IME 上屏等）。
///
/// 供外层持有者（如编辑器应用）监听，以便实时刷新"未保存/脏标记"等依赖内容变更的状态。
#[derive(Clone, Copy, Debug)]
pub struct TextEditEvent;

/// 用户把外部文件**拖放到**文本输入控件上时发出的事件（携带被拖入的文件路径）。
///
/// 由控件在 `on_drop` 回调里发出，外层持有者订阅后据此打开文件。
#[derive(Clone, Debug)]
pub struct FileDropped(pub PathBuf);

/// 撤销/重做的历史记录单元。
///
/// 一次编辑 = 在 `range`（修改前内容中的位置）处把 `old_text` 替换为 `new_text`：
/// - 撤销：把 `range.start..range.start+new_text.len()`（修改后内容中的区域）替换回 `old_text`；
/// - 重做：把 `range` 替换为 `new_text`。
#[derive(Clone)]
pub struct HistoryItem {
    /// 修改在"修改前"内容中的位置
    pub range: Range<usize>,
    /// 被替换掉的原文（撤销时恢复）
    pub old_text: String,
    /// 写入的新文本（重做时恢复）
    pub new_text: String,
}

/// 撤销/重做栈。
#[derive(Clone)]
pub struct History {
    items: Vec<HistoryItem>,
    /// 当前状态在栈中的位置；`-1` 表示尚未记录任何快照。
    index: i64,
}

impl Default for History {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            index: -1,
        }
    }
}

impl History {
    pub fn new() -> Self {
        Self::default()
    }

    #[allow(dead_code)]
    pub fn get_index(&self) -> i64 {
        self.index
    }

    /// 记录一次快照，覆盖被撤销掉（但在同一分支）的旧历史。
    pub fn push(&mut self, item: HistoryItem) {
        self.items.truncate((self.index + 1).max(0) as usize);
        self.items.push(item);
        self.index += 1;
    }

    pub fn undo(&mut self) -> Option<&HistoryItem> {
        if self.index >= 0 {
            let item = self.items.get(self.index as usize);
            self.index -= 1;
            return item;
        }
        None
    }

    pub fn redo(&mut self) -> Option<&HistoryItem> {
        if self.index + 1 < self.items.len() as i64 {
            self.index += 1;
            return self.items.get(self.index as usize);
        }
        None
    }
}

// ==================== 字素/词边界（自由函数，便于单测） ====================

/// 把可能落在字符中间的字节偏移修正到 ≤ 它的最近字符边界。
/// （真实调用方传来的都是字素边界，这里只为防御非边界输入。）
pub fn floor_char_boundary_safe(text: &str, offset: usize) -> usize {
    let mut i = offset.min(text.len());
    while i > 0 && !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// 上一个字素边界（UTF-8 字节偏移）。
pub fn previous_grapheme_boundary(text: &str, offset: usize) -> usize {
    if text.is_empty() {
        return 0;
    }
    let offset = floor_char_boundary_safe(text, offset);
    if offset == 0 {
        return 0;
    }
    text[..offset]
        .grapheme_indices(true)
        .next_back()
        .map(|(i, _)| i)
        .unwrap_or(0)
}

/// 下一个字素边界（UTF-8 字节偏移）。
pub fn next_grapheme_boundary(text: &str, offset: usize) -> usize {
    let offset = floor_char_boundary_safe(text, offset);
    if offset >= text.len() {
        return text.len();
    }
    text[offset..]
        .grapheme_indices(true)
        .nth(1)
        .map(|(i, _)| offset + i)
        .unwrap_or(text.len())
}

/// `offset` 处（必须落在字素边界上）的字素是否被视为"词字符"。
///
/// 词字符 = Unicode 字母/数字（含中文等 CJK）+ 下划线；空白与标点都不是，
/// 所以"中文词语"也会被当作一个词（参考 Zed/gpui-todos 的词边界语义）。
pub fn is_word_char(text: &str, offset: usize) -> bool {
    let offset = floor_char_boundary_safe(text, offset);
    if offset >= text.len() {
        return false;
    }
    let end = next_grapheme_boundary(text, offset);
    text[offset..end]
        .chars()
        .next()
        .map(|c| c.is_alphanumeric() || c == '_')
        .unwrap_or(false)
}

/// 上一个"词起点"（Ctrl+← / 双击选词左边界）。
///
/// - 光标在词中时 → 回到当前词首；
/// - 光标在空白/标点处时 → 跨过空白找到上一个词并回到其词首。
pub fn previous_word_boundary_in(text: &str, offset: usize) -> usize {
    let mut i = floor_char_boundary_safe(text, offset.min(text.len()));
    if i == 0 {
        return 0;
    }
    // 若当前位置不是词字符（空白/标点），先向左走到词内（或 0）
    if !is_word_char(text, i) {
        while i > 0 && !is_word_char(text, i) {
            i = previous_grapheme_boundary(text, i);
        }
    }
    // 现在 `i` 落在某词字符上（或 0）。回退到词首：仅当"前一个字符也是词字符"
    // 时才左移——这样能停在词的首字符上，而不是越过它（避免 off-by-one）。
    while i > 0 {
        let prev = previous_grapheme_boundary(text, i);
        if prev == i || !is_word_char(text, prev) {
            break;
        }
        i = prev;
    }
    i
}

/// 下一个"词起点/词尾"（Ctrl+→ / 双击选词右边界）。
///
/// - 光标在词中时 → 跳到当前词尾；
/// - 光标在空白/标点处时 → 跨过空白跳到下一个词首。
pub fn next_word_boundary_in(text: &str, offset: usize) -> usize {
    let mut i = floor_char_boundary_safe(text, offset.min(text.len()));
    let len = text.len();
    if i >= len {
        return len;
    }
    if is_word_char(text, i) {
        while i < len && is_word_char(text, i) {
            i = next_grapheme_boundary(text, i);
        }
        return i;
    }
    while i < len && !is_word_char(text, i) {
        i = next_grapheme_boundary(text, i);
    }
    i
}

// ==================== UTF-16 换算（IME 回调用） ====================

/// 把 UTF-8 字节偏移换算成 UTF-16 单元偏移。
pub fn offset_to_utf16(text: &str, offset: usize) -> usize {
    let mut cu_count = 0;
    for (i, c) in text.char_indices() {
        if i >= offset {
            break;
        }
        cu_count += c.len_utf16();
    }
    cu_count
}

/// 把 UTF-16 单元偏移换算成 UTF-8 字节偏移。
pub fn offset_to_utf8(text: &str, offset_utf16: usize) -> usize {
    let mut cu_count = 0;
    for (i, c) in text.char_indices() {
        if cu_count >= offset_utf16 {
            return i;
        }
        cu_count += c.len_utf16();
    }
    text.len()
}

/// 把 UTF-8 区间换算成 UTF-16 区间（IME 回传用）。
pub fn range_to_utf16(text: &str, range: &Range<usize>) -> Range<usize> {
    offset_to_utf16(text, range.start)..offset_to_utf16(text, range.end)
}

/// 把 UTF-16 区间换算成 UTF-8 区间（IME 定位/命中测试用）。
pub fn range_from_utf16(text: &str, range: &Range<usize>) -> Range<usize> {
    offset_to_utf8(text, range.start)..offset_to_utf8(text, range.end)
}

// ==================== 文本排版相关纯逻辑 ====================

/// 工具函数：给定行区间列表与偏移量，判断该偏移位于第几行。
pub fn line_index_for_offset(ranges: &[Range<usize>], offset: usize) -> usize {
    let mut idx = 0;
    for (i, r) in ranges.iter().enumerate() {
        if r.start <= offset && offset <= r.end {
            idx = i;
        }
    }
    idx
}

/// 换行边界 -> 该边界对应的 UTF-8 字节偏移（行内）。
pub fn boundary_byte(wl: &gpui::WrappedLine, b: &gpui::WrapBoundary) -> usize {
    let run = &wl.unwrapped_layout.runs[b.run_ix];
    run.glyphs
        .get(b.glyph_ix)
        .map(|g| g.index)
        .unwrap_or(wl.len())
}

/// 计算某逻辑行内、行内字节区间 `[sel_start, sel_end)` 的选区（或下划线）矩形。
///
/// 返回的矩形**相对该逻辑行的可视原点 (0,0)**：换行后第 k 个片段画在第 k 个可视行，
/// 因此 `y = line_height * k`，`x` 为该片段内的偏移。绘制时只需再加行原点与 `bounds.left()`。
///
/// ⚠️ 必须逐片段按行号下移 y：GPUI 的 `paint_line` 在遇到换行边界时执行
/// `glyph_origin.y += line_height`（见 gpui `text_system/line.rs`）来堆叠片段，
/// 若高亮矩形不跟着下移，就会全部叠在第一可视行上（历史显示 bug：
/// Ctrl+A / 鼠标拖选跨多行时只有第一行显示高亮，实际选区是正确的）。
pub fn selection_rects_in_line(
    wl: &gpui::WrappedLine,
    line_height: Pixels,
    sel_start: usize,
    sel_end: usize,
) -> Vec<Bounds<Pixels>> {
    let uw = &wl.unwrapped_layout;
    // 依次枚举各换行片段（行内字节区间）
    let mut segs: Vec<(usize, usize)> = Vec::new();
    let mut seg_start = 0usize;
    for b in wl.wrap_boundaries() {
        let seg_end = boundary_byte(wl, b);
        segs.push((seg_start, seg_end));
        seg_start = seg_end;
    }
    segs.push((seg_start, wl.len()));

    let mut rects = Vec::new();
    for (row, (seg_s, seg_e)) in segs.iter().enumerate() {
        let a = sel_start.max(*seg_s);
        let b = sel_end.min(*seg_e);
        if b > a {
            let seg_x0 = uw.x_for_index(*seg_s);
            let x0 = uw.x_for_index(a) - seg_x0;
            let x1 = uw.x_for_index(b) - seg_x0;
            let y0 = line_height * row as f32;
            rects.push(Bounds::from_corners(gpui::point(x0, y0), gpui::point(x1, y0 + line_height)));
        }
    }
    rects
}
