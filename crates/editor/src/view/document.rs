//! 文档装载 / 文本与选择 / 编辑提交。`CodeEditor` 的文档生命周期
//! （`new` / `set_text` / `set_rope_with_stats*` / `apply_replaces` / `apply_live_replay`）、
//! 文本/选择/光标访问器，以及 `apply_edit_synced` 编辑主链与 `mark_scan_dirty_incremental`
//! 折痕扫描登记。

use std::{
    cell::Cell,
    ops::Range,
};

use gpui::{
    px,
    App,
    Bounds,
    Context,
    FocusHandle,
    ListAlignment,
    ListState,
    ScrollHandle,
    SharedString,
};
use ropey::Rope;

use crate::editor::fold::{
    FoldMap,
};
use crate::editor::highlight::Highlighter;
use crate::editor::markdown::MarkdownDecor;
use crate::editor::rope_ext::RopeExt;
use crate::editor::undo::{
    EditIntent,
    UndoManager,
};
use crate::editor::{
    Buffer,
    Selection,
};
use super::{
    CodeEditor,
    EditorEvent,
    FindTipState,
    HlFill,
    RowCache,
    LIST_WINDOW_ROWS,
};
#[cfg(test)]
use super::CODE_EDITOR_FULL_RESET_CALLS;

impl CodeEditor {
    /// 创建编辑器（空文档）。
    pub fn new(cx: &mut App) -> Self {
        let focus_handle = cx.focus_handle().tab_stop(true);
        let buffer = Buffer::default();
        let row_count = buffer.line_count().max(1);
        // 固定窗口虚拟化：ListState **恒持 `LIST_WINDOW_ROWS` 项**（与 `resync_list`
        // 的 `reset(LIST_WINDOW_ROWS)` 同一不变量）。绝不能用 `row_count` 初始化——
        // 空文档 row_count=1 → 列表只有 1 项，且 `apply_desired_to_window` 的窗口覆盖
        // 判定不会补建窗口 → 新建空标签后粘贴多行会「只有第一行能渲染」（回归测试
        // `empty_new_tab_paste_not_collapsed_to_first_row`）。
        let list_state = ListState::new(LIST_WINDOW_ROWS, ListAlignment::Top, px(16.));
        Self {
            buffer,
            selection: Selection::collapsed(0),
            undo: UndoManager::new(),
            focus_handle,
            list_state,
            h_handle: ScrollHandle::new(),
            highlighter: Highlighter::new(),
            hl_dirty: None,
            hl_fill: HlFill::default(),
            row_cache: RowCache::default(),
            md_decor: MarkdownDecor::default(),
            md_mode: false,
            language: None,
            mono: SharedString::new_static(""),
            font_size: px(0.),
            line_height: px(0.),
            char_width: px(0.),
            metrics_ready: false,
            metric_fp: None,
            metric_compute_count: 0,
            row_count,
            hscroll_cols: 0,
            visible_chars: 80,
            gutter_chars: 3,
            gutter_width_px: px(0.),
            content_w_px: 0.,
            bg: ui::theme::Theme::light().colors.bg.into(),
            fg: ui::theme::Theme::light().colors.fg.into(),
            gutter_fg: ui::theme::Theme::light().colors.fg_muted.into(),
            sel_bg: ui::theme::Theme::light().colors.editor_selection_bg.into(),
            fold_bg: ui::theme::Theme::light().colors.editor_fold_bg.into(),
            active_line_bg: ui::theme::Theme::light().colors.editor_active_line_bg.into(),
            caret_color: ui::theme::Theme::light().colors.fg.into(),
            marked_range: None,
            last_edit: None,
            font_override: None,
            dragging: false,
            focused: false,
            caret_visible: false,
            caret_pause_until: None,
            blink_epoch: 0,
            blink_running: false,
            focus_registered: false,
            element_bounds: Bounds::default(),
            pending_drag: None,
            #[cfg(test)]
            drag_flush_calls: Cell::new(0),
            hscroll_tail_gap: 0.0,
            // 固定窗口虚拟化：窗口首行绝对行号（初始 0）。
            window_base: 0,
            desired_scroll: std::rc::Rc::new(Cell::new(0.)),
            // 指标未就绪前收到的「滚动目标逻辑行」（见 [`Self::scroll_to_line`]）。
            pending_scroll_line: None,
            tab_size: 4,
            folds: FoldMap::new(),
            fold_candidates: Vec::new(),
            bracket_pairs: Vec::new(),
            fold_boundaries: Vec::new(),
            bracket_boundaries: Vec::new(),
            scan_epoch: 0,
            scan_inflight: false,
            scan_dirty_edit: false,
            scan_pending_edit: None,
            scan_multi_edit: false,
            ruler_color: ui::theme::Theme::light().colors.fg_muted.opacity(0.55).into(),
            guide_color: ui::theme::Theme::light().colors.fg_muted.opacity(0.4).into(),
            active_guide_open: None,
            active_guide_color: ui::theme::Theme::light().colors.accent.opacity(0.9).into(),
            live_replay_calls: Cell::new(0),
            apply_replaces_calls: Cell::new(0),
            #[cfg(test)]
            guide_scan_steps: Cell::new(0),
            #[cfg(test)]
            scan_inc_landings: Cell::new(0),
            #[cfg(test)]
            scan_full_landings: Cell::new(0),
            find: crate::editor::find::FindOverlay::default(),
            _find_sub: None,
            find_match_bg: ui::theme::Theme::light().colors.editor_find_match_bg.into(),
            find_current_bg: ui::theme::Theme::light().colors.editor_find_current_bg.into(),
            find_input: None,
            find_tip: FindTipState::default(),
            goto: crate::editor::goto::GoToOverlay::default(),
            _goto_sub: None,
            goto_input: None,
        }
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    /// 设置语法高亮语言（`Some("rust")` / `None` 关闭）。
    pub fn set_language(&mut self, language: Option<&str>) {
        self.language = language.map(|s| s.to_owned());
        // Markdown 文档行渲染切到「所见即所得」滤镜（syntect 的 md 高亮不再使用，
        // 但仍保留 syntect 用于其余语言）。`.md` 由 app 统一以 "markdown" 接入。
        self.md_mode = matches!(language, Some("markdown"));
        self.highlighter.set_language(language);
        // 语言切换：高亮缓存将整段重解析，滞留的增量登记一并清除（防跨语言误用）。
        self.hl_dirty = None;
    }

    /// 设置文档文本（清空撤销/重做与选区）。
    pub fn set_text(&mut self, text: &str) {
        #[cfg(test)]
        CODE_EDITOR_FULL_RESET_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.buffer = Buffer::new(text);
        self.reset_document(false);
    }

    /// 与 [`Self::set_text`] 相同，但**连 Rope 与行宽统计都复用调用方（后台）算好的
    /// 结果**（O(1) 装配，不再全量重扫）。统计须与 `text` 严格对应（见
    /// `Buffer::from_parts`），否则横向滑动条量程/最长行列估算失真。打开大文档的主路径。
    pub fn set_rope_with_stats(
        &mut self,
        text: Rope,
        max_line_chars: usize,
        line_width_histogram: crate::editor::buffer::LineWidthHistogram,
    ) {
        #[cfg(test)]
        CODE_EDITOR_FULL_RESET_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.buffer = Buffer::from_parts(text, max_line_chars, line_width_histogram);
        self.reset_document(false);
    }

    /// 同 [`Self::set_rope_with_stats`]，但**延后折叠候选/括号配对扫描**：`defer_scans=true`
    /// 时首屏零同步重扫（`resync_list(true)` 置 `scan_dirty_edit`，折叠数据交给下一帧后台
    /// `refresh_fold_brackets` 补齐）——大文档渐进打开**首块挂载**即用此路径，把
    /// [*首屏即就绪*] 的同步全行扫描（首块 6561 行也 ~146ms）挪出首帧临界路径；文档
    /// 后续块持续追加、折叠扫描本就该整篇收尾后一次做。
    pub fn set_rope_with_stats_defer(
        &mut self,
        text: Rope,
        max_line_chars: usize,
        line_width_histogram: crate::editor::buffer::LineWidthHistogram,
    ) {
        #[cfg(test)]
        CODE_EDITOR_FULL_RESET_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.buffer = Buffer::from_parts(text, max_line_chars, line_width_histogram);
        self.reset_document(true);
    }

    /// 大文档打开「整篇换装」专用（P3 之后）：替换全文 Rope 但**保留 Buffer id + 高亮
    /// 缓存**。换装前（挂载首块）已解析的可见窗口在新全文里逐位相同，尾交给 highlighter
    /// `tail_append` A0 门控跨版本复用→不再整窗零收益重解析（大 `.rs` 首窗 syntect
    /// ~620ms debug = 用户「打开 .rs 会卡一下」根因）。折叠/括号扫描仍走延后（同
    /// `set_rope_with_stats_defer`）。`tail_append_row` = 挂载首块行数（新内容起点）。
    /// **仅 open 流水线 EOF 收尾且未编辑时可调**；普通整篇替换仍走新 id 全量作废。
    pub fn set_rope_with_stats_keep_highlight(
        &mut self,
        text: Rope,
        max_line_chars: usize,
        line_width_histogram: crate::editor::buffer::LineWidthHistogram,
        tail_append_row: usize,
    ) {
        #[cfg(test)]
        CODE_EDITOR_FULL_RESET_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.buffer
            .replace_text_keep_id(text, max_line_chars, line_width_histogram, tail_append_row);
        self.reset_document_impl(true, true);
    }

    /// 大文档「先瞬时显示、后台补齐统计」用：全文已经由 [`Self::set_rope_with_stats`]
    /// 挂上（统计为占位），后台算好真统计后 O(1) 修正横向滑动条量程/最长行。
    /// **要求调用方保证统计与当前文本一致**（打开后未编辑）。
    pub fn apply_stats(
        &mut self,
        max_line_chars: usize,
        line_width_histogram: crate::editor::buffer::LineWidthHistogram,
    ) {
        self.buffer.apply_stats(max_line_chars, line_width_histogram);
    }

    /// 大文档「分块渐进装配」追加一块已归一化文本（整行语义）：只把该块追到 Buffer
    /// 末尾并更新行数，**不重建列表 / 不回滚光标 / 不产生编辑事件**——打开期间的
    /// 滚动位置与撤销栈保留。首块仍走 [`Self::set_rope_with_stats`]（建立文档）。
    ///
    /// **不在此做折叠候选/括号配对扫描**（旧实现每块整篇重扫 → 打开 160k 行文档
    /// UI 线程卡 ~2s）。打开收尾（`file_ops` 末块落定）由 [`Self::refresh_fold_brackets`]
    /// 在后台统一重扫一次。
    pub fn append_mounted_chunk(
        &mut self,
        chunk: &str,
        chunk_max_chars: usize,
        chunk_hist: crate::editor::buffer::LineWidthHistogram,
    ) {
        self.buffer.append_chunk(chunk, chunk_max_chars, chunk_hist);
        self.row_count = self.buffer.line_count().max(1);
        self.folds.set_total(self.row_count);
    }

    /// 整篇替换文档后的通用复位（撤销/选区/横向滚动/标记清空 + 列表行数同步）。
    /// `defer_scans` 透传给 [`Self::resync_list`]：`false` = 同步重扫折叠候选/括号配对
    /// （内容全新、首屏即就绪，测试依赖）；`true` = 延到下一帧后台刷新（打开大文档
    /// 首块挂载走 [`Self::set_rope_with_stats_defer`]，首屏零同步重扫）。
    fn reset_document(&mut self, defer_scans: bool) {
        self.reset_document_impl(defer_scans, false);
    }

    /// [`Self::reset_document`] 的实现；`keep_highlight=true` 时**跳过高亮缓存作废**——
    /// 仅供大文档打开整篇换装（[`Self::set_rope_with_stats_keep_highlight`]）使用：换装后
    /// 顶部窗口与挂载首块逐位相同，由 `tail_append` A0 门控跨版本复用缓存，避免整窗
    /// 零收益重解析（大 `.rs` ~620ms debug）。
    fn reset_document_impl(&mut self, defer_scans: bool, keep_highlight: bool) {
        self.undo.clear();
        self.selection = Selection::collapsed(0);
        // 整篇替换 = 文档被换掉：查找匹配表全部失效，关条清高亮（保留 last_query 不重置）。
        self.find.close_bar();
        // 整篇替换 = 文档在光标下被换掉：任何进行中的鼠标拖选都失去意义，一并取消。
        // （否则 `set_text` 只收拢选区、却残留 `dragging=true`，宿主切回视图后仅移动
        // 鼠标就从 anchor=0 自动扩选——见 app::tests::*wysiwyg_toggle_off* 回归。
        // 走统一生命周期钩子，与 on_mouse_up/on_blur/on_mouse_up_out 同一复位语义。）
        self.cancel_mouse_drag();
        self.hscroll_cols = 0;
        self.marked_range = None;
        self.last_edit = None;
        // 整篇替换：高亮增量登记 + 高亮缓存一并作废（下一帧整段重解析；
        // 防「新旧文档同 version=0」命中上一份文档的颜色）。
        self.hl_dirty = None;
        if !keep_highlight {
            self.highlighter.invalidate();
        }
        // 整篇替换：行级缓存同样显式清空（`Buffer::new` 后 version 归零，同防
        // 「新旧文档同 version=0」命中上一份文档的行静态内容）。
        self.row_cache.clear();
        // Markdown「所见即所得」滤镜缓存无需作废：缓存键含文档身份（`Buffer::id`，
        // O(1)）+ version，`Buffer::new` 后 version 归零、id 必新，也不会命中上一份
        // 文档的样式区间，`scan_window` 首扫即身份 miss → 按新文档自愈（M0.3；勿再手加
        // `md_decor.clear()` 兜底；命中帧零重哈希）。
        // 整篇替换：点亮标尺清除（内容已变，旧“所属块”提示无意义）。
        self.active_guide_open = None;
        // 滚动目标回零（新文档从头开始；分帧增长中的深跳占位也随之复位）。
        self.desired_scroll.set(0.);
        // 整篇替换 = 新文档：就绪前暂存的旧滚动目标逻辑行失效，一并清位。
        self.pending_scroll_line = None;
        // 诊断：`desired_scroll` 直写零的调用点（真机「47→2」若此处命中即编辑区被打回顶）。
        crate::diag::ed_scroll(
            "reset_document_impl.desired_zero",
            &format!("row_count={} keep_highlight={keep_highlight}", self.row_count),
        );
        // 整篇替换：折叠视图投影一并清空（文本结构已变）。
        self.folds.clear();
        self.resync_list(defer_scans);
    }

    /// 在选区处输入文本（等同连续打字：合并撤销、走同一编辑主链）。
    pub fn insert_text(&mut self, text: &str, cx: &mut Context<Self>) {
        self.insert_edit(self.selection.range(), text, EditIntent::Typing, cx);
    }

    /// 批量增量替换（切回「预览编辑测试」整篇写回用）：按**从后往前**应用一组互不
    /// 重叠、已整行对齐的 `(字节区间, 文本)`，把文档改写为目标内容。每处走
    /// [`Self::apply_edit_synced`] 的 O(log n) Rope 编辑 + 窗口化高亮增量登记，**不整篇
    /// 重建**（区别于 `set_text` 的 `reset_document`：保留滚动/折叠/行静态缓存，只在高亮
    /// 器按编辑登记/版本跳变局部重解析）——按 diff 改写大文档只在受影响区间重解析，
    /// 典型小改动落到数百 ms 内（旧整篇 `set_text` 级联重排 debug ~11s）。
    ///
    /// 语义对齐 `set_text`：清空撤销栈（写回 = 原子替换，undo/redo 不逐个回滚本次
    /// 写回事务）、光标收到行首、无编辑事件（宿主写回后自行同步行/字符计数与右栏）。
    /// 空列表 = no-op。
    pub fn apply_replaces(&mut self, replaces: &[(Range<usize>, String)]) {
        if replaces.is_empty() {
            return;
        }
        self.apply_replaces_calls
            .set(self.apply_replaces_calls.get() + 1);
        // 与 `set_text` 同语义：整篇写回 = 原子替换，撤销栈清空（旧内容快照作废）。
        self.undo.clear();
        let mut min_line_a = usize::MAX;
        let mut total_rows_delta: i64 = 0;
        for (range, text) in replaces.iter().rev() {
            // 编辑前行跨度（后→前应用：较晚偏移 hunk 不位移较早偏移 hunk 的行号）。
            let line_a = self.buffer.line_of_offset(range.start);
            let line_b = self.buffer.line_of_offset(range.end);
            total_rows_delta += text.matches('\n').count() as i64 + 1;
            total_rows_delta -= line_b.saturating_sub(line_a) as i64 + 1;
            min_line_a = min_line_a.min(line_a);
            // 缓冲写入 + 折叠同步 + 高亮增量登记（单 hunk 场景即精确值）。
            self.apply_edit_synced(range.clone(), text);
        }
        // 多 hunk：`apply_edit_synced` 的单槽登记彼此覆盖 → 归并为粗略一份；高亮器按
        // 编辑版本跳变（>1）回落有界整段重解析（O(窗口)，正确且与旧实现代价相当）。
        if replaces.len() > 1 {
            let tail = min_line_a.saturating_add(total_rows_delta.max(0) as usize);
            self.hl_dirty = Some((min_line_a, tail, total_rows_delta));
        }
        // 与 `set_text` 一致：整篇写回后光标行首、无待重放的单笔编辑轨迹。
        self.selection = Selection::collapsed(0);
        self.last_edit = None;
        self.marked_range = None;
        // 批量写回 = 文档在光标下被整篇改写：查找匹配表作废、关条清高亮（保留 last_query）。
        self.find.close_bar();
    }

    /// WYSIWYG 编辑日志**按序重放**（切回「预览编辑测试」有改动路径，方案2）：把 live2
    /// 缴存的 `(字节区间, 新文本)` op（含撤/重做的净逆操作）按发生顺序逐条应用到当前
    /// 缓冲区。两者从同一「toggle ON 的源码 COW Rope 快照」起步、live 会话期间本缓冲区
    /// 不被改写 → op 字节坐标恒等，按序重放即得最终态。语义对齐 [`Self::set_text`] /
    /// [`Self::apply_replaces`]：清空撤销栈（整篇写回 = 原子替换）、光标收行首、无编辑事件；
    /// 每处走 [`Self::apply_edit_synced`] 的 O(log n) Rope 编辑 + 窗口化高亮增量登记，
    /// **不整篇重建**（区别于 `set_text` 的 reset_document 级联）。
    ///
    /// **护栏**（护栏成本必须 O(编辑量)+O(1)，绝不做全篇 O(n) 比对——那会把 6.4MB 大文档
    /// 的「1 字符切回」顶到几十 ms）：先在临时 COW Rope 上按序干跑一次，逐 op 校验越界 /
    /// 切中多字节字符（这些会污染缓冲区，必须先拦下），最后做 **O(1) 总长度比对**——方案2
    /// 的现实漂移模式（CRLF / BOM / 漏 op）**全部改变总字节长**，长度不等即拦截返回
    /// `false`；同长异字在「同源 Rope + 确定性 op」下不可能发生（且由 app 层全文等价测试
    /// 兜底）。干跑 + 长度栏都不通过 → **不改动缓冲区**，由调用方回退既有行级 diff 管线
    /// （`app::diff`）。
    pub fn apply_live_replay(&mut self, ops: &[(Range<usize>, gpui::SharedString)], expected: &str) -> bool {
        if ops.is_empty() {
            return expected == self.text_string();
        }
        let exp_len = expected.len();
        // —— 干跑自检（护栏）：逐 op 边界/字符边界校验 + 终态 O(1) 长度栏 ——
        let mut probe = self.buffer_text().clone();
        for (r, t) in ops {
            if r.start > r.end || r.end > probe.len() {
                return false;
            }
            if !probe.is_char_boundary(r.start) || !probe.is_char_boundary(r.end) {
                return false;
            }
            probe.replace(r.clone(), t.as_ref());
        }
        if probe.len() != exp_len {
            return false;
        }
        // —— 自检通过：落盘（与 apply_replaces 同语义的原子整篇写回）——
        self.undo.clear();
        for (range, text) in ops {
            self.apply_edit_synced(range.clone(), text.as_ref());
        }
        self.selection = Selection::collapsed(0);
        self.last_edit = None;
        self.marked_range = None;
        // 批量写回 = 文档被整篇改写：查找匹配表作废、关条清高亮（保留 last_query）。
        self.find.close_bar();
        self.live_replay_calls
            .set(self.live_replay_calls.get() + 1);
        true
    }
    /// （仅测试）本实例成功走 `apply_live_replay`（方案2 op 重放）的次数。
    // cfg(test) 探针面：拆 crate 后被 markdown 测试跨 crate 消费，转无条件 pub。
    pub fn live_replay_calls(&self) -> usize {
        self.live_replay_calls.get()
    }

    /// （仅测试）本实例成功走 `apply_replaces`（行级 diff 兜底）的次数（非空替换）。
    // cfg(test) 探针面：拆 crate 后被 markdown 测试跨 crate 消费，转无条件 pub。
    pub fn apply_replaces_calls(&self) -> usize {
        self.apply_replaces_calls.get()
    }

    /// （仅测试）后台折叠候选/括号配对落地次数 `(增量, 整篇)`——断言结构编辑后
    /// 走增量路径而非 O(全文) 整篇重扫。
    #[cfg(test)]
    pub(crate) fn test_scan_landings(&self) -> (usize, usize) {
        (self.scan_inc_landings.get(), self.scan_full_landings.get())
    }

    /// （仅测试）**同步**执行一轮折叠候选/括号配对刷新并落地——跳过 gpui 后台
    /// executor 调度（测试执行器对第二个及以后的 render 内 `cx.spawn` 不再轮询，
    /// 属观测器材性，生产由真实事件循环驱动），与后台刷新共用同一计算入口
    /// （[`crate::editor::fold::compute_fold_brackets`]）。返回是否走了增量路径。
    #[cfg(test)]
    pub(crate) fn test_scan_sync_land(&mut self) -> bool {
        use config::limits::FOLD_CANDIDATE_FULL_MAX_LINES as SCAN_LIMIT;
        if !self.scan_dirty_edit {
            return false; // 无待刷新：不落地、不计数
        }
        self.scan_dirty_edit = false;
        let rope = self.buffer.text();
        let (folds, brackets, nfb, npb, was_incremental) =
            crate::editor::fold::compute_fold_brackets(
                |r| crate::editor::buffer::line_shared(rope, r),
                self.row_count,
                SCAN_LIMIT,
                &self.fold_candidates,
                &self.bracket_pairs,
                &self.fold_boundaries,
                &self.bracket_boundaries,
                self.scan_pending_edit,
                self.scan_multi_edit,
            );
        self.fold_candidates = folds;
        self.fold_candidates.sort_by_key(|f| f.start);
        self.bracket_pairs = brackets;
        self.bracket_pairs.sort_by_key(|p| p.open_line);
        self.fold_boundaries = nfb;
        self.bracket_boundaries = npb;
        self.scan_pending_edit = None;
        self.scan_multi_edit = false;
        if was_incremental {
            self.scan_inc_landings
                .set(self.scan_inc_landings.get() + 1);
        } else {
            self.scan_full_landings
                .set(self.scan_full_landings.get() + 1);
        }
        was_incremental
    }

    /// 底层 Rope（只读；O(1) COW 快照给后台线程）。
    pub fn text(&self) -> &Rope {
        self.buffer.text()
    }

    /// Rope 别称（语义化：给预览同步取快照）。
    pub fn rope(&self) -> &Rope {
        self.buffer.text()
    }

    /// 整篇文本（O(n) 物化；导出/保存用）。
    pub fn text_string(&self) -> String {
        self.buffer.to_string()
    }

    /// 兼容 `EditorState::value()`：返回全文 String（保存/测试用）。
    pub fn value(&self) -> String {
        self.buffer.to_string()
    }

    /// 编辑版本号：每次编辑 +1（预览同步判断“扫描期间是否有新编辑”）。
    pub fn version(&self) -> u64 {
        self.buffer.version()
    }

    /// 文本行数（LF 计行，尾换行含幻影空行）——编辑器行模型语义。
    pub fn line_count(&self) -> usize {
        self.buffer.line_count()
    }

    /// 状态栏显示用行数（`str::lines()` 语义：`"a\nb\n"`→2，尾换行不追加一行）。
    pub fn display_line_count(&self) -> usize {
        self.buffer.display_line_count()
    }

    /// 字符总数。
    pub fn char_count(&self) -> usize {
        self.buffer.char_count()
    }

    /// 行内字符列 → 字节偏移（越界夹到行尾；搜索跳转/程序化定位用）。
    pub fn offset_for_line_col(&self, line: usize, column_char: usize) -> usize {
        self.buffer.offset_for_char_column(line, column_char)
    }

    /// 选中字节区间（搜索跳转高亮/程序化选区）：滚动到可见并通知。
    pub fn set_selection_range(&mut self, range: Range<usize>, cx: &mut Context<Self>) {
        let s = self.buffer.clip(range.start);
        let e = self.buffer.clip(range.end);
        self.selection = Selection::from_range(s, e);
        self.undo.set_cooperative(false);
        self.kick_caret();
        self.reveal_caret();
        // 程序化定位（搜索跳转/侧边栏目录）也发光标变化事件，状态栏跟随。
        cx.emit(EditorEvent::SelectionChanged);
        cx.notify();
    }

    /// 光标位置 `(行 0 基, 字符列 0 基)`。
    pub fn cursor_position(&self) -> (usize, usize) {
        let h = self.selection.head();
        (self.buffer.line_of_offset(h), self.buffer.char_column(h))
    }

    /// 取走最近一次编辑的字节区间 `(start, old_end, new_end)`（当前文本坐标）。
    /// 供预览增量扫描累积 dirty；取走后清空。
    pub fn take_last_edit(&mut self) -> Option<(usize, usize, usize)> {
        self.last_edit.take()
    }

    /// 软换行开关（暂未实现：首版 display 行 ≡ buffer 行，占位保持 API 一致）。
    pub fn set_soft_wrap(&mut self, _enable: bool) {}

    /// 字号覆盖（px；`None` 走主题默认）。由应用 Ctrl+=/- 调节。
    pub fn set_font_override(&mut self, size_px: Option<f32>) {
        self.font_override = size_px;
        self.metrics_ready = false;
    }

    /// 当前生效字号（px）。
    pub fn font_size_px(&self) -> f32 {
        self.font_size.as_f32()
    }

    pub fn buffer_text(&self) -> &ropey::Rope {
        self.buffer.text()
    }

    pub fn selection(&self) -> Selection {
        self.selection
    }

    /// 光标移动到字节偏移（折叠选区）。
    pub fn set_caret_to(&mut self, offset: usize) {
        self.selection.place(self.buffer.clip(offset));
        self.undo.set_cooperative(false);
    }

    pub fn can_undo(&self) -> bool {
        self.undo.can_undo()
    }

    pub fn can_redo(&self) -> bool {
        self.undo.can_redo()
    }

    /// 就地编辑并同步行数（O(log n) 缓冲编辑）。固定窗口虚拟化下列表条目数与文档
    /// 无关，无需 splice——只更新 `row_count`；后续 `reveal_caret` 会按新光标行
    /// 重新定位窗口。
    pub(super) fn apply_edit_synced(&mut self, range: Range<usize>, text: &str) -> crate::editor::buffer::Edit {
        // WY_TRACE breakdown：编辑主链「缓冲应用」段（O(log n) Rope 编辑 + 高亮增量登记）。
        let _t_apply = crate::prof::begin();
        // 编辑会改变块结构/跨度：清除熄灭点亮标尺（语义见 guide click highlight）。
        self.active_guide_open = None;
        // 先取被替换区间的**编辑前**行跨度（折叠区间同步需要，编辑后行号已变）。
        let prev_rows = self.row_count;
        let line_a = self.buffer.line_of_offset(range.start);
        let line_b = self.buffer.line_of_offset(range.end);
        let edit = self.buffer.apply_edit(range, text);
        self.row_count = self.buffer.line_count().max(1);
        // 高亮增量登记：编辑前最小受影响行 + 编辑后首个未变尾部行 + 净行增量。
        // （一次性消费；`Highlighter` 据此从 `line_a - 回看` 局部重解析并收敛早退。）
        let insert_rows = text.matches('\n').count() + 1;
        let removed_rows = line_b.saturating_sub(line_a) + 1;
        self.hl_dirty = Some((
            line_a,
            line_a.saturating_add(insert_rows),
            insert_rows as i64 - removed_rows as i64,
        ));
        // 折叠同步：编辑进折叠体 → 自动展开；折叠后整体平移；锚被删 → 丢弃。
        self.folds.sync_edit(line_a, line_b, text, self.row_count);
        // 折叠候选 / 括号配对：**不再同步整篇重扫**（大文档逐键卡顿）。行数变了或
        // 文本含括号字符 → 登记**单个**受影响行段描述，由下一帧 render 挂起一轮
        // 后台**增量**刷新（受影响包络局部重扫 + 尾部行号平移，见 `fold.rs`）。
        let has_bracket =
            text.bytes().any(|b| matches!(b, b'{' | b'}' | b'(' | b')' | b'[' | b']'))
                || edit
                    .old_text
                    .bytes()
                    .any(|b| matches!(b, b'{' | b'}' | b'(' | b')' | b'[' | b']'));
        if self.row_count != prev_rows || has_bracket {
            // min_stop = 新坐标下首个未受影响行（替换文本占 `insert_rows` 行）。
            self.mark_scan_dirty_incremental(
                line_a,
                line_a.saturating_add(insert_rows),
                insert_rows as i64 - removed_rows as i64,
            );
        }
        crate::prof::end("ed.apply_sync", _t_apply);
        edit
    }

    /// 登记一次「折叠候选 / 括号配对后台增量刷新」的受影响行段：
    /// - 自上落地后仍只有**一个**未落地编辑段 → 记入 [`Self::scan_pending_edit`]；
    /// - 已有未落地段（本次是第二个或更多）→ 置 [`Self::scan_multi_edit`]（增量
    ///   前提不成立，下一轮退整篇重扫兜底），并始终打 [`Self::scan_dirty_edit`]。
    ///
    /// `line_a` = 编辑首行（编辑前坐标），`min_stop` = 新坐标下首个未受影响行
    /// （受影响包络闭区间上界），`delta` = 净行增量（新行数 − 旧行数）。
    pub(super) fn mark_scan_dirty_incremental(&mut self, line_a: usize, min_stop: usize, delta: i64) {
        self.scan_dirty_edit = true;
        if self.scan_multi_edit || self.scan_pending_edit.is_some() {
            self.scan_multi_edit = true;
        } else {
            self.scan_pending_edit = Some((line_a, min_stop, delta));
        }
    }
}
