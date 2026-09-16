//! 文本缓冲：**单绳存储 + 可逆编辑记录**。
//!
//! 设计吸收：
//! - **VSCode/Monaco**：不建逐行对象（`ropey::Rope` 一棵树承载全文，行号查询 O(log n)），
//!   内存与文档大小成正比；编辑只记录「操作」而非整篇快照。
//! - **zed**：编辑事务是撤销/重做的最小单位，光标随事务一起恢复（见 [`crate::editor::undo`]）。
//!
//! 坐标约定：对外一律 **UTF-8 字节偏移**（Rope 切片天然按字节）；行/列换算由
//! [`RopeExt`]（`gpui_component::input::RopeExt`）提供，列在本模块里指「字符列」
//! （Unicode 标量数），与渲染的等宽列同向。

use std::collections::BTreeMap;
use std::ops::Range;

/// 大文档「先瞬时显示」占位横向量程的**采样步长**：均匀每隔这么多行算一次行宽。
/// 取 256 → 116MB(3.2M 行) 只算 ~12.5k 行（<1ms），远快于全量 [`compute_line_stats`]（7.3s）。
pub(crate) const LINE_SAMPLE_STRIDE: usize = 256;
/// 采样估计放大系数：采样间距可能漏掉个别极长行，放大补偿（宁可高估多留白）。
pub(crate) const LINE_SAMPLE_SAFETY: f32 = 1.2;

use gpui::SharedString;
use gpui_component::input::{Rope, RopeExt};
use ropey::LineType;

/// 一条已应用的文本编辑（可逆），撤销/重做的原子单位。
///
/// 语义：把 ``[`start`, `start+old_text.len()`)`` 替换为 `new_text`。
/// `range`（替换位置）在「编辑后」文本中依然有效，因为撤销/重做按严格 LIFO 回放，
/// 之前/之后的编辑不会改变本记录锚点之后的相对位置。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edit {
    /// 被替换区间的起点（同时也是撤销/重做替换的锚点）。
    pub start: usize,
    /// 被移除的旧文本（撤销时插回）。
    pub old_text: SharedString,
    /// 插入的新文本（重做时插回）。
    pub new_text: SharedString,
}

impl Edit {
    /// 编辑前被替换的区间。
    pub fn replaced_range(&self) -> Range<usize> {
        self.start..self.start + self.old_text.len()
    }

    /// 编辑后新文本占据的区间。
    pub fn inserted_range(&self) -> Range<usize> {
        self.start..self.start + self.new_text.len()
    }

    /// 全局字节增量（新文长度 − 旧文长度）。
    pub fn delta(&self) -> isize {
        self.new_text.len() as isize - self.old_text.len() as isize
    }

    /// `new_text` 是否包含换行（Enter/粘贴会因此成为独立撤销单元）。
    pub fn inserts_newline(&self) -> bool {
        self.new_text.contains(['\n', '\r'])
    }

    /// 逆操作：把 `new_text` 换回 `old_text`（撤销时应用）。
    pub fn reversed(&self) -> Edit {
        Edit {
            start: self.start,
            old_text: self.new_text.clone(),
            new_text: self.old_text.clone(),
        }
    }
}

/// 单字符的「渲染宽度单位」（等宽格数，供横向内容宽度估算）。
///
/// - Tab = 4 格；
/// - ASCII（0x00-0x7F）= 1 格；
/// - 其余（CJK 全角、emoji、复杂文字等）保守按 2 格。
///
/// 宁可高估（横向多留白）也绝不低估：低估会让 `StyledText` 在行内折行，行号、
/// 点击、光标全部错位并重叠（用户实测：中文长行自动"换行"、序号没对齐、点下一行
/// 文本重叠）。
pub(crate) fn char_width_units(c: char) -> usize {
    match c {
        '\t' => 4,
        c if c.is_ascii() => 1,
        _ => 2,
    }
}

/// 行宽直方图（multiset）：宽度单位 → 拥有该宽度的行数（见 [`char_width_units`]，
/// CJK/全角=2、ASCII=1、Tab=4）。最宽行 = `keys().next_back()`(O(1))。
pub(crate) type LineWidthHistogram = BTreeMap<usize, usize>;

/// 纯计函数（不依赖 `&mut Buffer`，可放后台线程）：统计全文的行宽直方图与
/// 最长行字符数。与 [`Buffer::recompute_max_line`] 等价，但可以在后台算好后
/// 经 [`Buffer::from_parts`] O(1) 装配，避免打开大文档时在 UI 线程全量重扫。
pub(crate) fn compute_line_stats(rope: &Rope) -> (usize, LineWidthHistogram) {
    let mut hist = LineWidthHistogram::new();
    let mut mx_chars = 0usize;
    for i in 0..rope.lines_len() {
        let line = rope.slice_line(i);
        let units = line.chars().map(char_width_units).sum::<usize>();
        *hist.entry(units).or_insert(0) += 1;
        mx_chars = mx_chars.max(line.chars().count());
    }
    (mx_chars, hist)
}

/// 大文档「先瞬时显示」的**占位横向量程**：均匀采样 [`LINE_SAMPLE_STRIDE`] 行算
/// 行宽，取最大值 × [`LINE_SAMPLE_SAFETY`] 作为估计放入直方图（单条目的「伪计数」——
/// 只服务于 [`Buffer::max_line_units`] 的 O(1) 最宽行查询）。真统计由后台
/// [`compute_line_stats`] 全量算出后经 [`Buffer::apply_stats`] O(1) 替换。
///
/// 效果：打开后横向滚动条量程**立刻**接近真实（无需等 116MB ≈ 7.3s 的全量统计），
/// 长行可立即左右平移；真值到位后 `apply_stats` 无缝校正。
///
/// 注意这是**采样估计**，不是硬上界：均匀间距可能漏掉个别极长行（如某条超长行整体
/// 落在采样格之外，甚至所有采样行都空/短时 est=0 → 空直方图），此类长行要等真统计
/// 到达后才可滚到尾部——×[`LINE_SAMPLE_SAFETY`] 只缓解常见情况，不保证覆盖最坏样本。
/// 若编辑发生在真统计到达前，`apply_stats` 会被「已编辑」守卫跳过，占位估计保持
/// （作为近似值，配合行宽直方图增量维护仍稳健）。
pub(crate) fn sample_line_stats(rope: &Rope) -> (usize, LineWidthHistogram) {
    let n = rope.lines_len();
    let mut mx_units = 0usize;
    let mut mx_chars = 0usize;
    let mut i = 0usize;
    while i < n {
        let line = rope.slice_line(i);
        let units = line.chars().map(char_width_units).sum::<usize>();
        mx_units = mx_units.max(units);
        mx_chars = mx_chars.max(line.chars().count());
        i += LINE_SAMPLE_STRIDE;
    }
    let est = ((mx_units as f32 * LINE_SAMPLE_SAFETY).ceil() as usize).max(mx_units);
    let hist = if est > 0 {
        LineWidthHistogram::from([(est, 1)])
    } else {
        // 全空行（est=0）：保持空直方图语义（无内容 → 无横向量程）。
        LineWidthHistogram::new()
    };
    (mx_chars, hist)
}

/// 文本缓冲：Rope 全文 + 版本号（供缓存失效判断）。
///
/// 行模型：以 LF 为行分隔符（`metric_lines_lf`，CR 视为普通字符）；
/// 尾部换行会产生一个「幻影空行」（`"a\n"` 视为 2 行）——与编辑器
/// 的行模型一致：光标可停留在末尾空行上。
#[derive(Clone)]
pub struct Buffer {
    text: Rope,
    version: u64,
    /// 全文最长（字符数，Unicode 标量）的行 —— 保留给等宽列近似/测试。
    /// 一致性：`Buffer::new/from_rope/set_text` 全量重算；编辑类操作**只增不减**
    /// （`refresh_max` 被增大的行更新），删除最长行后保持旧值（横向过度留白可接受，
    /// 换来编辑最长行时 O(编辑大小) 而非 O(n) 每次重扫）。
    max_line_chars: usize,
    /// 行宽直方图（multiset）：宽度单位 → 拥有该宽度的行数（见 [`char_width_units`]，
    /// CJK/全角=2、ASCII=1、Tab=4）。最宽行 = `keys().next_back()`(O(1))。
    ///
    /// **始终精确且增量维护**：编辑时先删旧受影响行的整行宽、再加新受影响行的整行宽
    /// （O(受影响行 × log L)，L=不同宽度数）——删除长内容后横向滚动条量程立即精确
    /// 收敛，不欠 O(1) 旧上界、也不做同步 O(n) 全量重扫。
    line_width_histogram: LineWidthHistogram,
}

impl Default for Buffer {
    fn default() -> Self {
        Self {
            text: Rope::from(""),
            version: 0,
            max_line_chars: 0,
            line_width_histogram: BTreeMap::new(),
        }
    }
}

impl Buffer {
    /// 以文本创建缓冲。
    pub fn new(source: impl AsRef<str>) -> Self {
        let mut b = Self {
            text: Rope::from(source.as_ref()),
            version: 0,
            max_line_chars: 0,
            line_width_histogram: BTreeMap::new(),
        };
        b.recompute_max_line();
        b
    }

    /// 已经存在 Rope 时直接接管（避免一次整篇拷贝）。
    pub fn from_rope(text: Rope) -> Self {
        let mut b = Self {
            text,
            version: 0,
            max_line_chars: 0,
            line_width_histogram: LineWidthHistogram::new(),
        };
        b.recompute_max_line();
        b
    }

    /// 直接用外部（后台线程）算好的统计装配，跳过全量重扫（打开大文档的 O(1)
    /// 装配路径；统计须来自 [`compute_line_stats`]，否则行宽/横向滑动条失真）。
    pub fn from_parts(
        text: Rope,
        max_line_chars: usize,
        line_width_histogram: LineWidthHistogram,
    ) -> Self {
        Self {
            text,
            version: 0,
            max_line_chars,
            line_width_histogram,
        }
    }

    /// 「先瞬时显示、后台补齐统计」路径用：全文已由 [`Self::from_parts`] 挂上（统计为
    /// 占位），后台算好真统计后 O(1) 替换。**要求文本自挂载以来未变**（调用方以版本/
    /// 编辑计数 guard），否则替换会覆盖增量维护的直方图、横向滑动条短暂失真。
    pub fn apply_stats(
        &mut self,
        max_line_chars: usize,
        line_width_histogram: LineWidthHistogram,
    ) {
        self.max_line_chars = max_line_chars;
        self.line_width_histogram = line_width_histogram;
    }

    /// 大文档「分块渐进装配」用：把后台已归一化的一块文本追到全文末尾（ropey
    /// `insert` 到字节末尾，O(块大小)），并合并本块的行宽统计（O(本块行数)）。
    ///
    /// 语义约束（调用方〔`crate::app::open_progress`〕保证）：
    /// - `text` 只含**完整行**（除文件末尾的未结束行外，均以 `\n` 结尾），绝不出现
    ///   被劈开的行——否则 `chunk_hist` 的「整行」直方图与逐块合并不成立；
    /// - `chunk_hist` / `chunk_max_chars` 是本块这些完整行的精确统计。
    ///
    /// 与 [`Self::apply_edit`] 的区别：**不产生可逆编辑记录、不算用户编辑**
    /// （撤销栈/脏标记/编辑事件都不涉及），只推进 `version` 让缓存失效。
    pub fn append_chunk(&mut self, text: &str, chunk_max_chars: usize, chunk_hist: LineWidthHistogram) {
        if !text.is_empty() {
            self.text.insert(self.text.len(), text);
            self.version += 1;
        }
        self.max_line_chars = self.max_line_chars.max(chunk_max_chars);
        for (w, c) in chunk_hist {
            *self.line_width_histogram.entry(w).or_insert(0) += c;
        }
    }

    /// 全文最长行（字符数）——O(1) 读，供等宽列近似/测试。
    pub fn max_line_chars(&self) -> usize {
        self.max_line_chars
    }

    /// 全文最宽行（宽度单位）——O(1) 读，供横向内容宽度（不折行的依据）。
    /// multiset 始终精确，编辑后立即反映真实最宽行。
    pub fn max_line_units(&self) -> usize {
        self.line_width_histogram
            .keys()
            .next_back()
            .copied()
            .unwrap_or_default()
    }

    /// 某行的宽度单位（O(行内容)）。
    fn line_units(&self, i: usize) -> usize {
        self.text.slice_line(i).chars().map(char_width_units).sum()
    }

    /// `[a, b]` 各行的宽度 collect（替换前捕获旧行宽用；注意替换后这些行可能消失）。
    fn line_widths_in(&self, a: usize, b: usize) -> Vec<usize> {
        (a..=b).map(|i| self.line_units(i)).collect()
    }

    /// 从直方图减一行宽度；计数归零则移除键。
    fn remove_line_width(&mut self, w: usize) {
        if let Some(c) = self.line_width_histogram.get_mut(&w) {
            *c -= 1;
            if *c == 0 {
                self.line_width_histogram.remove(&w);
            }
        }
    }

    /// 向直方图加一行宽度。
    fn add_line_width(&mut self, w: usize) {
        *self.line_width_histogram.entry(w).or_insert(0) += 1;
    }

    /// 编辑后的行宽直方图维护 + `max_line_chars`（只增不减）：先减被替换旧行的
    /// 整行宽（`old_widths` 须在替换前捕获），再加新行 `[old_line_a, new_line_b]`
    /// 的整行宽。行宽全用**整行**口径（边界被部分覆盖的行也要整体换新）。
    fn refresh_lines(&mut self, old_widths: Vec<usize>, old_line_a: usize, new_line_b: usize) {
        for w in old_widths {
            self.remove_line_width(w);
        }
        let mut mx_chars = self.max_line_chars;
        for i in old_line_a..=new_line_b {
            let (units, chars) = {
                let line = self.text.slice_line(i);
                (
                    line.chars().map(char_width_units).sum::<usize>(),
                    line.chars().count(),
                )
            };
            self.add_line_width(units);
            mx_chars = mx_chars.max(chars);
        }
        self.max_line_chars = mx_chars;
    }

    fn recompute_max_line(&mut self) {
        let t = std::time::Instant::now();
        let (mx, hist) = compute_line_stats(&self.text);
        self.line_width_histogram = hist;
        self.max_line_chars = mx;
        if std::env::var_os("MD_OPEN_DEBUG").is_some() {
            eprintln!(
                "[MD_OPEN_DEBUG] Buffer::recompute_max_line({} 行): {} ms",
                self.text.lines_len(),
                t.elapsed().as_millis()
            );
        }
    }

    /// 底层文本（只读）。
    pub fn text(&self) -> &Rope {
        &self.text
    }

    /// 缓存失效版本号：每次编辑 +1，高亮/布局缓存据此判定是否过期。
    pub fn version(&self) -> u64 {
        self.version
    }

    /// 总字节数。
    pub fn len(&self) -> usize {
        self.text.len()
    }

    pub fn is_empty(&self) -> bool {
        self.text.len() == 0
    }

    /// 总字符数（Unicode 标量）。
    pub fn char_count(&self) -> usize {
        self.text.len_chars()
    }

    /// UTF-16 code unit 总数（IME 用）。
    pub fn len_utf16(&self) -> usize {
        self.text.len_utf16()
    }

    /// 字节偏移 → UTF-16 偏移（O(log n)）。
    pub fn byte_to_utf16(&self, offset: usize) -> usize {
        self.text.byte_to_utf16_idx(offset.min(self.text.len()))
    }

    /// UTF-16 偏移 → 字节偏移（O(log n)）。
    pub fn utf16_to_byte(&self, offset_utf16: usize) -> usize {
        self.text.utf16_to_byte_idx(offset_utf16.min(self.text.len_utf16()))
    }

    /// 行数（LF 分隔；尾换行产生幻影空行）——编辑器光标模型语义。
    pub fn line_count(&self) -> usize {
        self.text.lines_len()
    }

    /// 显示语义行数（与 `str::lines()` 一致：`"a\nb\n"`→2，尾换行不追加一行）。
    /// 与 [`line_count`]（含幻影空行，供 ListState/光标定位用）不同——状态栏/统计用这个。
    /// O(log n)（只查末行是否为空），不物化全文。
    pub fn display_line_count(&self) -> usize {
        let n = self.text.lines_len();
        // 非空文本的末行为空 ⟺ 以换行结尾（ropey 幻影空行 = str::lines 丢弃的尾段；"\r\n" 同理）。
        let trailing_newline = self.len() > 0 && self.text.line_len(n - 1) == 0;
        (n - usize::from(trailing_newline)).max(1)
    }

    /// 行首字节偏移。
    pub fn line_start(&self, line: usize) -> usize {
        self.text.line_start_offset(line)
    }

    /// 行尾字节偏移（不含换行符）。
    pub fn line_end(&self, line: usize) -> usize {
        self.text.line_end_offset(line)
    }

    /// 行内容字节长（不含换行符及之外的尾部）。
    pub fn line_len(&self, line: usize) -> usize {
        self.text.line_len(line)
    }

    /// 字节偏移所在行。
    pub fn line_of_offset(&self, offset: usize) -> usize {
        let offset = self.clip(offset);
        self.text.byte_to_line_idx(offset, LineType::LF)
    }

    /// 字节偏移在当前行内的字节列。
    pub fn byte_column(&self, offset: usize) -> usize {
        let offset = self.clip(offset);
        offset - self.line_start(self.line_of_offset(offset))
    }

    /// 字节偏移在当前行内的字符列（Unicode 标量，O(行)）。
    pub fn char_column(&self, offset: usize) -> usize {
        let offset = self.clip(offset);
        let line = self.line_of_offset(offset);
        self.text
            .slice_line(line)
            .slice(..self.byte_column(offset))
            .chars()
            .count()
    }

    /// 字节偏移在当前行内的「宽度单位」列（O(行)，见 [`char_width_units`])——
    /// 与渲染等宽格一致：CJK 一个占 2 格。横向滚动/光标 reveal 用这个，否则
    /// 中文长行按字符列 reveal 会少滚一半，光标跑出可视区。
    pub fn unit_column(&self, offset: usize) -> usize {
        let offset = self.clip(offset);
        let line = self.line_of_offset(offset);
        self.text
            .slice_line(line)
            .slice(..self.byte_column(offset))
            .chars()
            .map(char_width_units)
            .sum()
    }

    /// 把「行内字符列」换算回字节偏移（越界夹到行尾；O(行)）。
    pub fn offset_for_char_column(&self, line: usize, column: usize) -> usize {
        let line = line.min(self.line_count().saturating_sub(1));
        let ls = self.line_start(line);
        let le = self.line_end(line);
        let mut bytes = 0usize;
        let mut n = 0usize;
        for ch in self.text.slice(ls..le).chars() {
            if n >= column {
                break;
            }
            bytes += ch.len_utf8();
            n += 1;
        }
        ls + bytes
    }

    /// 某行的文本（不含换行符；越界返回空）。
    pub fn line_text(&self, line: usize) -> SharedString {
        SharedString::from(self.text.slice_line(line).to_string())
    }

    /// 截取某行的部分文本（`from_char` ~ `from_char+max_chars`，字符列），
    /// 用于横向滚动的窗口渲染（避免每次整行整形）。
    pub fn line_text_window(&self, line: usize, from_char: usize, max_chars: usize) -> SharedString {
        let ls = self.line_start(line);
        let le = self.line_end(line);
        if ls >= le {
            return SharedString::new_static("");
        }
        let mut start_byte = ls;
        let mut n = 0usize;
        for ch in self.text.slice(ls..le).chars() {
            if n >= from_char {
                break;
            }
            start_byte += ch.len_utf8();
            n += 1;
        }
        let mut end_byte = start_byte;
        n = 0;
        for ch in self.text.slice(start_byte..le).chars() {
            if n >= max_chars {
                break;
            }
            end_byte += ch.len_utf8();
            n += 1;
        }
        SharedString::from(self.text.slice(start_byte..end_byte).to_string())
    }

    /// 字节区间文本。
    pub fn text_in_range(&self, range: Range<usize>) -> SharedString {
        let start = self.clip(range.start);
        let end = self.clip(range.end);
        if start >= end {
            return SharedString::new_static("");
        }
        SharedString::from(self.text.slice(start..end).to_string())
    }

    /// 把偏移夹到 [0, len] 的合法区间。
    pub fn clip(&self, offset: usize) -> usize {
        offset.min(self.text.len())
    }

    /// 应用一次替换（返回可逆记录）。`range` 需在字符边界上。
    pub fn apply_edit(&mut self, range: Range<usize>, new_text: &str) -> Edit {
        let range = self.clip(range.start).min(self.clip(range.end))..self.clip(range.end);
        let old_line_a = self.line_of_offset(range.start.min(self.text.len()));
        let old_line_b = self.line_of_offset(range.end.min(self.text.len()));
        // 先捕获受影响旧行的整行宽（行宽直方图需要，替换后这些行可能消失）。
        let old_widths = self.line_widths_in(old_line_a, old_line_b);
        let old = SharedString::from(self.text.slice(range.clone()).to_string());
        self.text.replace(range.clone(), new_text);
        self.version += 1;
        let new_line_b = self.line_of_offset((range.start + new_text.len()).min(self.text.len()));
        self.refresh_lines(old_widths, old_line_a, new_line_b);
        Edit {
            start: range.start,
            old_text: old,
            new_text: new_text.into(),
        }
    }

    /// 撤销：把 `inserted_range` 还原为 `old_text`。
    pub fn undo_edit(&mut self, edit: &Edit) {
        let ir = edit.inserted_range();
        let old_line_a = self.line_of_offset(ir.start.min(self.text.len()));
        let old_line_b = self.line_of_offset(ir.end.min(self.text.len()));
        let old_widths = self.line_widths_in(old_line_a, old_line_b);
        self.text.replace(ir, edit.old_text.as_ref());
        self.version += 1;
        let new_line_b = self
            .line_of_offset((edit.start + edit.old_text.len()).min(self.text.len()));
        self.refresh_lines(old_widths, old_line_a, new_line_b);
    }

    /// 重做：把 `replaced_range` 重新替换为 `new_text`。
    pub fn redo_edit(&mut self, edit: &Edit) {
        let rr = edit.replaced_range();
        let old_line_a = self.line_of_offset(rr.start.min(self.text.len()));
        let old_line_b = self.line_of_offset(rr.end.min(self.text.len()));
        let old_widths = self.line_widths_in(old_line_a, old_line_b);
        self.text.replace(rr, edit.new_text.as_ref());
        self.version += 1;
        let new_line_b = self
            .line_of_offset((edit.start + edit.new_text.len()).min(self.text.len()));
        self.refresh_lines(old_widths, old_line_a, new_line_b);
    }

    /// 整篇文本（测试/导出用；O(n) 物化）。
    pub fn to_string(&self) -> String {
        self.text.to_string()
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    pub(crate) fn assert_edit_roundtrip(source: &str, range: Range<usize>, new: &str) {
        let mut b = Buffer::new(source);
        let before = b.to_string();
        let e = b.apply_edit(range.clone(), new);
        let after = b.to_string();
        assert_eq!(
            after,
            format!("{}{}{}", &before[..range.start], new, &before[range.end..]),
            "apply 应等价于直接字符串替换"
        );
        b.undo_edit(&e);
        assert_eq!(b.to_string(), before, "undo 应还原");
        b.redo_edit(&e);
        assert_eq!(b.to_string(), after, "redo 应重放");
    }
}
