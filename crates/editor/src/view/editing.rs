//! 键盘分发 / 光标移动 / 折叠 / 编辑命令 / 撤销重做。`on_key_down` 原样键分发、
//! 方向/词/行/文档移动与选区扩展、代码折叠切换与括号标尺竖线（`bracket_guide_segments*`）、
//! 插入/退格/删除/换行/Tab、复制剪切粘贴、撤销与重做。

use super::leading_ws_len;

use std::{
    ops::Range,
};

use gpui::{
    ClipboardItem,
    Context,
    KeyDownEvent,
    Window,
};

use config::layout::{
    BRACKET_GUIDE_CLOSE_TAIL_LINES,
    BRACKET_GUIDE_OPEN_HEAD_LINES,
};
use config::limits::{
    FOLD_CANDIDATE_FULL_MAX_LINES,
};
use crate::editor::fold::{
    Fold,
};
use crate::editor::movement;
use crate::editor::undo::{
    EditIntent,
};
use crate::editor::{
    Selection,
};
use super::{
    CodeEditor,
    EditorEvent,
};

impl CodeEditor {

    /// 统一按键：仅处理 keymap 未覆盖的 **Shift+方向**（gpui-component 的
    /// Select{Left,Right,Up,Down} 是组件内部 action、公共 keymap 用了它但我们无法
    /// 注册 handler，故在原样 key_down 层处理）。其余键全部走 `on_action`。
    ///
    /// **Enter/Shift+Enter 也在此处理**（换成行）：不能挂在 `on_action(&Enter)` 上——
    /// 动作层在泡泡阶段命中任意 `Enter` 监听即 `stop_propagation`（gpui 动作默认
    /// 停传播，window.rs `dispatch_action_on_node_inner` 6597 行），KeyDownEvent 会被
    /// 提前吞掉、不再派发；而 find 条（编辑区内查找）需要在**原样 key_down** 层区分
    /// Enter（下一个）与 Shift+Enter（上一个）——只有本层（或 find 条）不依赖动作层
    /// 拦截时，find 条聚焦后回车才不会被编辑区换行抢先。空 Enter key 在 IME/字体
    /// 路径之前优先处理。
    pub(super) fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let k = &event.keystroke;
        let m = k.modifiers;
        if k.key.eq_ignore_ascii_case("enter") {
            // find 条输入框持焦时其 on_key_down 已 stop（导航），走不到这里；
            // 编辑器持焦（含 find 条打开但焦点在编辑器）：回车=换行（含 Shift+Enter）。
            self.insert_newline(cx);
            self.undo.set_cooperative(false);
            cx.stop_propagation();
            return;
        }
        // 无修饰可打印字符 → IME/InputHandler
        if !m.control && !m.alt && !m.platform && k.key_char.is_some() {
            return;
        }
        if m.shift && !m.control && !m.alt && !m.platform {
            match k.key.to_ascii_lowercase().as_str() {
                "left" => {
                    self.arrow(true, -1, 0, cx);
                    return;
                }
                "right" => {
                    self.arrow(true, 1, 0, cx);
                    return;
                }
                "up" => {
                    self.arrow(true, 0, -1, cx);
                    return;
                }
                "down" => {
                    self.arrow(true, 0, 1, cx);
                    return;
                }
                _ => {}
            }
        }
        // 代码折叠：Ctrl+Shift+[ 折叠光标所在块；Ctrl+Shift+] 展开光标处折叠。
        if m.control && m.shift && !m.alt && !m.platform {
            match k.key.to_ascii_lowercase().as_str() {
                "[" => {
                    self.fold_under_caret(cx);
                }
                "]" => {
                    self.unfold_under_caret(cx);
                }
                _ => {}
            }
        }
    }

    /// 统一的移动+选区应用点：新光标若落进折叠隐藏体 → 跳到该折叠之后的首个可见行
    /// （VS Code 式「跳过折叠体」，不自动展开）。
    fn apply_move(&mut self, select: bool, new_head: usize, preferred: Option<u32>, cx: &mut Context<Self>) {
        let new_head = self.snap_out_of_hidden(new_head);
        if select {
            let col = self.buffer.char_column(new_head);
            self.selection.set_head(new_head);
            if preferred.is_some() {
                self.selection.remember_column(col);
            }
        } else {
            self.selection.move_cursor(new_head, preferred);
        }
        self.kick_caret();
        self.reveal_caret();
        // 方向键/Home/End/翻页等移动光标（宿主刷新状态栏光标行列等派生数据）。
        cx.emit(EditorEvent::SelectionChanged);
        cx.notify();
    }

    /// 若 `off` 处于某折叠的隐藏体内部 → 返回该折叠**结束行**（首个可见行）的行首；
    /// 否则原样返回。
    fn snap_out_of_hidden(&self, off: usize) -> usize {
        let row = self.buffer.line_of_offset(off);
        match self.folds.hidden_containing(row) {
            Some(f) => self.buffer.line_start(f.end.min(self.row_count)),
            None => off,
        }
    }

    /// 折叠操作后保持「视口顶行逻辑行」稳定：把**编辑前**捕获的顶行逻辑行重新映射
    /// 到新视觉坐标并滚动到位——折叠/展开上方的折叠会改变视觉行数与像素位置，否则
    /// 视口会瞬间跳走。`top_logical` = 折叠前 `abs_scroll_top().0` 经映射得到的逻辑行。
    fn anchor_top_logical_after_fold(&mut self, top_logical: usize, off_px: f32) {
        let vis2 = self.folds.visual_row(top_logical);
        self.scroll_to_px(vis2 as f32 * self.line_height.as_f32().max(1.) + off_px);
    }

    /// 视口顶行的逻辑行（折叠操作前捕获用）。
    fn capture_top_logical(&self) -> (usize, f32) {
        let (top_vis, off) = self.abs_scroll_top();
        (self.folds.logical_row(top_vis), off)
    }

    /// gutter 点击切换：该行已有激活折叠 → 展开；否则有候选 → 按候选区间折叠；
    /// 否则尝试按缩进启发式创建。
    pub(super) fn toggle_fold_at(&mut self, line: usize) {
        let line = line.min(self.row_count.saturating_sub(1));
        let top = self.capture_top_logical();
        let did = if let Some(f) = self.folds.fold_starting_at(line).copied() {
            self.folds.expand_at(f.start);
            true
        } else if let Some(c) = self.candidate_starting_at(line) {
            self.folds.toggle(c.start, c.end);
            true
        } else if let Some(end) = self.fold_block_end(line) {
            self.folds.toggle(line, end);
            true
        } else {
            false
        };
        if did {
            self.anchor_top_logical_after_fold(top.0, top.1);
        }
    }

    /// Ctrl+Shift+[：创建光标所在块（优先用折叠候选；否则缩进启发式）的折叠；
    /// 已折叠则保持。
    fn fold_under_caret(&mut self, cx: &mut Context<Self>) {
        let line = self.buffer.line_of_offset(self.selection.head());
        if self.folds.fold_starting_at(line).is_some() {
            return;
        }
        let fold_range = self
            .candidate_starting_at(line)
            .map(|c| (c.start, c.end))
            .or_else(|| self.fold_block_end(line).map(|end| (line, end)));
        if let Some((start, end)) = fold_range {
            let top = self.capture_top_logical();
            self.folds.toggle(start, end);
            self.anchor_top_logical_after_fold(top.0, top.1);
            cx.notify();
        }
    }

    /// Ctrl+Shift+]：展开光标所在/包含的折叠。
    fn unfold_under_caret(&mut self, cx: &mut Context<Self>) {
        let line = self.buffer.line_of_offset(self.selection.head());
        if self.folds.fold_at(line).is_some() {
            let top = self.capture_top_logical();
            self.folds.expand_at(line);
            self.anchor_top_logical_after_fold(top.0, top.1);
            cx.notify();
        }
    }

    /// 缩进启发式折叠块：从 `start` 行起向后找到块结束行（**开区间** end）。
    /// - 首个缩进 `< start 行缩进` 的行 → 块结束（dedent）；
    /// - 同缩进且以闭/尾记号（`}` `)` `]` `#` `` ` ``）开头 → 块结束；
/// - 空行跳过；
/// - 到文件尾 → 结束。
///
/// 至少需隐藏 1 行（`end ≥ start+2`）才视为可折叠。
    pub(super) fn fold_block_end(&self, start: usize) -> Option<usize> {
        if self.row_count == 0 || start >= self.row_count {
            return None;
        }
        let base_indent = leading_ws_len(self.buffer.line_text(start).as_ref());
        let mut end = None;
        for r in (start + 1)..self.row_count {
            let line = self.buffer.line_text(r);
            let trimmed = line.trim_start();
            if trimmed.is_empty() {
                continue;
            }
            let indent = leading_ws_len(line.as_ref());
            if indent < base_indent {
                end = Some(r);
                break;
            }
            if indent == base_indent
                && matches!(
                    trimmed.chars().next(),
                    Some('}') | Some(')') | Some(']') | Some('#') | Some('`')
                )
            {
                end = Some(r);
                break;
            }
        }
        let end = end.unwrap_or(self.row_count);
        (end >= start + 2).then_some(end)
    }

    /// 以 `line` 为起点的折叠候选（gutter 指示器/点击折叠的区间来源）。
    ///
    /// `fold_candidates` 已按 `start` 升序（见 `recompute_fold_brackets_full` / 后台
    /// `refresh_fold_brackets` 落地处）→ 二分 O(log n)，避免 gutter 每帧对每行做
    /// O(候选数) 线性扫描（大文档数列 ~1k+ 候选 × 每帧 40 行 = 每键几 ms 白烧）。
    fn candidate_starting_at(&self, line: usize) -> Option<Fold> {
        let i = self.fold_candidates.partition_point(|f| f.start < line);
        self.fold_candidates.get(i).filter(|f| f.start == line).copied()
    }

    /// `line` 行的折叠指示箭头状态（渲染与测试共用的**单一权威**）：
    /// - `None` = 不画箭头；
    /// - `Some(true)` = 已激活折叠首行（隐藏体收起）→ 右向「▸」；
    /// - `Some(false)` = 仅折叠候选起点 → 下向「▾」。
    pub(super) fn fold_arrow_state(&self, line: usize) -> Option<bool> {
        if self.folds.fold_starting_at(line).is_some() {
            Some(true)
        } else if self.candidate_starting_at(line).is_some() {
            Some(false)
        } else {
            None
        }
    }

    /// `line` 是否有「可折叠」标记：已激活折叠首行，或折叠候选起点。
    pub(super) fn has_fold_or_candidate(&self, line: usize) -> bool {
        self.fold_arrow_state(line).is_some()
    }

    /// 打开/整篇替换路径的**同步**整篇重扫（内容全新，首屏即就绪，测试依赖）：
    /// 折叠候选 + 括号标尺配对一次算齐，并落地各自的「栈清空边界」
    /// （`fold_boundaries` / `bracket_boundaries`），供后续编辑的后台**增量**重算复用。
    /// ≤ 阈值的整篇扫，超大文档清空（改为按需折叠）。
    ///
    /// 落地后按 `start` / `open_line` 升序排序——gutter/标尺查询走二分
    /// （`candidate_starting_at` / `bracket_guide_segments_flagged`），
    /// 原始扫描是 LIFO 收栈序非单调，排序只改内部存储次序、集合与语义不变。
    pub(super) fn recompute_fold_brackets_full(&mut self) {
        if self.row_count > FOLD_CANDIDATE_FULL_MAX_LINES {
            self.fold_candidates.clear();
            self.bracket_pairs.clear();
            self.fold_boundaries.clear();
            self.bracket_boundaries.clear();
        } else {
            let rope = self.buffer.text();
            let (folds, pairs, fb, pb) = crate::editor::fold::full_fold_brackets(
                |r| crate::editor::buffer::line_shared(rope, r),
                self.row_count,
            );
            self.fold_candidates = folds;
            self.fold_candidates.sort_by_key(|f| f.start);
            self.bracket_pairs = pairs;
            self.bracket_pairs.sort_by_key(|p| p.open_line);
            self.fold_boundaries = fb;
            self.bracket_boundaries = pb;
        }
        self.scan_pending_edit = None;
        self.scan_multi_edit = false;
    }

    /// 在**后台线程**重算折叠候选 + 括号标尺配对并落地（单飞 + 版本号取消）。
    ///
    /// - 打开收尾（file_ops 末块落定）与编辑后的下一帧 render 都会调用；
    /// - 编辑走**增量**：在受影响行段包络 `[B, stop)` 上局部重扫 + 未变段尾部行号
    ///   平移（见 [`crate::editor::fold::incremental_fold_brackets`]），
    ///   只读 Rope O(1) COW 快照在 `background_executor` 上跑纯函数；多编辑段累积
    ///   （`scan_multi_edit`）或增量前提缺失时退化为整篇扫描兜底；
    /// - 结果回来时若期间又有请求（`scan_epoch` 已变）则丢弃旧结果、用最新快照
    ///   重跑一轮；
    /// - 不阻塞 UI 线程：打开 160k 行文档的 ~280ms 全量扫描不再卡界面，输入含括号
    ///   字符也不再逐键整篇重扫（结构编辑事件的 O(全文) 双扫改 O(受影响段)）。
    pub fn refresh_fold_brackets(&mut self, cx: &mut Context<Self>) {
        use config::limits::FOLD_CANDIDATE_FULL_MAX_LINES as SCAN_LIMIT;
        self.scan_epoch = self.scan_epoch.wrapping_add(1);
        if self.scan_inflight {
            return; // 已有任务在跑：其末尾会因 epoch 已变而用最新快照重跑一轮。
        }
        self.scan_inflight = true;
        cx.spawn(async move |this, cx| {            loop {
                // 快照：最新文档 Rope（O(1) COW）+ 上次落地结果（增量对拍基）+ 增量描述。
                let snap = this.update(cx, |this, _| {
                    (
                        this.scan_epoch,
                        this.row_count,
                        this.buffer.text().clone(),
                        this.fold_candidates.clone(),
                        this.bracket_pairs.clone(),
                        this.fold_boundaries.clone(),
                        this.bracket_boundaries.clone(),
                        this.scan_pending_edit,
                        this.scan_multi_edit,
                    )
                });
                let Ok((epoch, row_count, rope, folds, pairs, fb, pb, pending, multi)) = snap
                else {
                    return;
                };
                let (folds, brackets, nfb, npb, was_incremental) = cx
                    .background_executor()
                    .spawn(async move {
                        let line = |r: usize| crate::editor::buffer::line_shared(&rope, r);
                        // 增量：受影响包络局部重扫 + 尾部平移；前提缺失 → 整篇重扫兜底
                        // （`was_incremental` 同时是测试探针，见 `compute_fold_brackets`）。
                        crate::editor::fold::compute_fold_brackets(
                            line,
                            row_count,
                            SCAN_LIMIT,
                            &folds,
                            &pairs,
                            &fb,
                            &pb,
                            pending,
                            multi,
                        )
                    })
                    .await;
                // 非测试构建：`was_incremental` 仅供测试探针统计落地路径，落为 `_`。
                #[cfg(not(test))]
                let _ = was_incremental;
                let done = this
                    .update(cx, |this, cx| {
                        if epoch == this.scan_epoch {
                            #[cfg(test)]
                            {
                                if was_incremental {
                                    this.scan_inc_landings.set(this.scan_inc_landings.get() + 1);
                                } else {
                                    this.scan_full_landings
                                        .set(this.scan_full_landings.get() + 1);
                                }
                            }
                            // 落地即排序：折叠候选按 `start`、括号配对按 `open_line`
                            // 升序（gutter/标尺查询走二分）。仅内部次序，语义不变。
                            this.fold_candidates = folds;
                            this.fold_candidates.sort_by_key(|f| f.start);
                            this.bracket_pairs = brackets;
                            this.bracket_pairs.sort_by_key(|p| p.open_line);
                            // 增量边界更新为本次全景（供下一次增量续用），并消费
                            // 增量描述（若期间又有编辑，epoch 已变、不会走到这里）。
                            this.fold_boundaries = nfb;
                            this.bracket_boundaries = npb;
                            this.scan_pending_edit = None;
                            this.scan_multi_edit = false;
                            this.scan_inflight = false;
                            cx.notify();
                            true
                        } else {
                            false // 期间又有编辑/请求 → 用最新快照重跑
                        }
                    })
                    .unwrap_or(true);
                if done {
                    return;
                }
            }
        })
        .detach();
    }

    /// 括号标尺在当前视口内的竖线段：`(x_px, y_rel, h_px)`（文本区本地坐标）。
    /// 每个跨行括号配对一根竖线，位于**块首行缩进列**（`guide_units` × 字宽，随横向
    /// 滚动），从开行**之后**画到闭行**内约 0.25 行深**——两端「抵行不抵字」：顶部
    /// 恰接触首覆盖行上缘（开行底缘、不伸入开行、不与 `{` 字形相抵），底部伸入闭行
    /// 内约 0.25 行，留小空隙（可配，不画满整行，避免穿过字形）。端点微调常量见
    /// `config::layout`。
    ///
    /// **同列区间合并**：同一列上重叠/相邻的竖线合并成一根连续线——否则两条 1px
    /// 线在同像素叠加会显得更暗/更粗（同列嵌套块会命中此情况）。
    /// `top_vis`/`off` = `abs_scroll_top()`；`lh` 行高；`harea_w` 文本区宽；`view_h` 视口高。
    /// 光标逻辑行 L 落在配对跨度 `[open_line, close_line]` 内即命中（点击点亮用）；
    /// 开行**含本身**（点击 `Self {` 那行的 `{` → 点亮 Self 自己打开的块，而非外层 fn）。
    /// 多对命中时取**最深**（`open_line` 最大的那个）——即光标所在的最内层块。
    pub(super) fn guide_pair_for_logical_row(&self, logical: usize) -> Option<usize> {
        // 配对表按 `open_line` 升序（`recompute_fold_brackets_full` / `refresh_fold_brackets`
        // 两处落地排序）→ 先二分裁出候选前缀 `open_line <= logical`；最内层包含 logical
        // 的配对 = 前缀中 `close_line >= logical` 且 `open_line` 最大者。配对为规范嵌套
        // （跨行括号可嵌套/并列，不交叉）→ 从右往左首个 `close_line >= logical` 即最深
        // 包含（更左配对的 open_line 更小，不可能更深）。典型点击位于块体内 → 一次命中
        // （O(log N) + O(1)）；仅当光标落在全部配对之后（本行无命中）才线性回扫兜底。
        #[cfg(test)]
        self.guide_scan_steps.set(0);
        let i = self.bracket_pairs.partition_point(|p| p.open_line <= logical);
        for p in self.bracket_pairs[..i].iter().rev() {
            #[cfg(test)]
            self.guide_scan_steps.set(self.guide_scan_steps.get() + 1);
            if p.close_line >= logical {
                return Some(p.open_line);
            }
        }
        None
    }

    /// 同 [`Self::bracket_guide_segments`]，但每个合并段额外带 `bool` = 该段是否命中
    /// `active`（点亮标尺的 `open_line`）。合并同列样本段时，任一样本命中则整段点亮。
    pub(super) fn bracket_guide_segments_flagged(
        &self,
        top_vis: usize,
        off: f32,
        lh: f32,
        harea_w: f32,
        view_h: f32,
        active: Option<usize>,
    ) -> Vec<(f32, f32, f32, bool)> {
        if self.bracket_pairs.is_empty() {
            return Vec::new();
        }
        // 竖线端点微调（× 行高，见 config/layout.rs）：底部伸入闭行贴 `}` 字形，
        // 顶部从首覆盖行向下缩进控制留白。均为可配置行高系数。
        let head = BRACKET_GUIDE_OPEN_HEAD_LINES * lh;
        let tail = BRACKET_GUIDE_CLOSE_TAIL_LINES * lh;
        let cw = self.char_width.as_f32().max(1.);
        let hscroll = self.hscroll_px();
        let bottom_vis = top_vis.saturating_add(((view_h / lh).ceil() as usize) + 1);
        // 窗口化：配对已按 `open_line` 升序（两个落地处排序，见 `recompute_fold_brackets_full` /
        // `refresh_fold_brackets`）。竖线只可能来自 `open_line ∈ [top_vis − 回看, bottom_vis]`
        // 的配对——开括号在视口之下（`> bottom_vis`）的配对必不相交；开在「回看」行以上的
        // 大跨度配对（竖线远超一屏）视为无意义跳过。原实现对**全部**配对逐条算 `visual_row`
        // /相交判定（大文档 1k+ 对 × 每帧 = 每键几 ms 白烧），实际视口内只有个位数可见段。
        // 回看再放宽也不影响正确性（仍逐条跑原过滤），只决定「多长跨度的竖线还画」。
        let lb = self
            .bracket_pairs
            .partition_point(|p| p.open_line < top_vis.saturating_sub(
                config::limits::BRACKET_GUIDE_WINDOW_LOOKBACK_LINES,
            ));
        let ub = self.bracket_pairs.partition_point(|p| p.open_line <= bottom_vis);
        // (x, 视觉顶行, 视觉底行**开区间**, 是否点亮) —— 先整列收集，再合并同列重叠/相邻段。
        let mut intervals: Vec<(f32, usize, usize, bool)> = Vec::new();
        for p in &self.bracket_pairs[lb..ub] {
            if p.open_line >= p.close_line {
                continue; // 过期（编辑后未及时重扫）的配对；重扫会清理。
            }
            let vs = self.folds.visual_row(p.open_line);
            let ve = self.folds.visual_row(p.close_line);
            // 区间 [open 行之后, close 行之前)。
            let seg_top = vs.saturating_add(1);
            let seg_bot = ve.saturating_sub(1);
            if seg_bot < seg_top {
                continue; // 相邻行块（无体）或折叠压缩 → 无竖线。
            }
            if seg_bot < top_vis || seg_top > bottom_vis {
                continue; // 与视口不相交。
            }
            let x = p.guide_units as f32 * cw - hscroll;
            if x < 0.0 || x > harea_w {
                continue; // 出文本区裁掉。
            }
            intervals.push((
                x.round().max(0.0),
                seg_top.max(top_vis),
                seg_bot.min(bottom_vis).saturating_add(1),
                active == Some(p.open_line),
            ));
        }
        // 按列排序，合并同列重叠/相邻区间。
        intervals.sort_by(|a, b| {
            a.0.partial_cmp(&b.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.1.cmp(&b.1))
        });
        let mut out: Vec<(f32, f32, f32, bool)> = Vec::new();
        let mut cur: Option<(f32, usize, usize, bool)> = None;
        for (x, t, b, hl) in intervals {
            match &mut cur {
                Some((cx, ct, cb, ca)) if *cx == x && t <= *cb => {
                    *cb = (*cb).max(b);
                    *ca |= hl;
                }
                _ => {
                    if let Some((cx, ct, cb, ca)) = cur.take() {
                        out.push((
                            cx,
                            (ct as f32 - top_vis as f32) * lh - off + head,
                            (cb - ct) as f32 * lh - head + tail,
                            ca,
                        ));
                    }
                    cur = Some((x, t, b, hl));
                }
            }
        }
        if let Some((cx, ct, cb, ca)) = cur {
            out.push((
                cx,
                (ct as f32 - top_vis as f32) * lh - off + head,
                (cb - ct) as f32 * lh - head + tail,
                ca,
            ));
        }
        out
    }

    pub(super) fn bracket_guide_segments(
        &self,
        top_vis: usize,
        off: f32,
        lh: f32,
        harea_w: f32,
        view_h: f32,
    ) -> Vec<(f32, f32, f32)> {
        // 未点亮（active=None）→ 与旧实现语义一致（3 元组）。
        self.bracket_guide_segments_flagged(top_vis, off, lh, harea_w, view_h, None)
            .into_iter()
            .map(|(x, y, h, _)| (x, y, h))
            .collect()
    }

    pub(super) fn arrow(&mut self, select: bool, dc: isize, dr: isize, cx: &mut Context<Self>) {
        self.undo.set_cooperative(false);
        if dr != 0 {
            let (off, col) = movement::move_vertical(
                &self.buffer,
                self.selection.head(),
                dr,
                self.selection.preferred_column,
            );
            self.apply_move(select, off, Some(col), cx);
        } else {
            // 水平：有选区且不按住 Shift → 先收敛到选区起点/终点
            let head = self.selection.head();
            let mut new_head = if dc < 0 {
                movement::char_left(self.buffer.text(), head)
            } else {
                movement::char_right(self.buffer.text(), head)
            };
            if !select && !self.selection.is_collapsed() {
                new_head = if dc < 0 {
                    self.selection.start()
                } else {
                    self.selection.end()
                };
            }
            self.apply_move(select, new_head, None, cx);
        }
    }

    pub(super) fn move_word_left(&mut self, cx: &mut Context<Self>) {
        self.undo.set_cooperative(false);
        let new = movement::word_left(self.buffer.text(), self.selection.head());
        self.apply_move(false, new, None, cx);
    }

    pub(super) fn move_word_right(&mut self, cx: &mut Context<Self>) {
        self.undo.set_cooperative(false);
        let new = movement::word_right(self.buffer.text(), self.selection.head());
        self.apply_move(false, new, None, cx);
    }

    pub(super) fn select_word_left(&mut self, cx: &mut Context<Self>) {
        self.undo.set_cooperative(false);
        let new = movement::word_left(self.buffer.text(), self.selection.head());
        self.apply_move(true, new, None, cx);
    }

    pub(super) fn select_word_right(&mut self, cx: &mut Context<Self>) {
        self.undo.set_cooperative(false);
        let new = movement::word_right(self.buffer.text(), self.selection.head());
        self.apply_move(true, new, None, cx);
    }

    pub(super) fn move_line_start(&mut self, select: bool, cx: &mut Context<Self>) {
        self.undo.set_cooperative(false);
        let new = movement::to_line_start(&self.buffer, self.selection.head());
        self.apply_move(select, new, None, cx);
    }

    pub(super) fn move_line_end(&mut self, select: bool, cx: &mut Context<Self>) {
        self.undo.set_cooperative(false);
        let new = movement::to_line_end(&self.buffer, self.selection.head());
        self.apply_move(select, new, None, cx);
    }

    pub(super) fn move_doc_start(&mut self, select: bool, cx: &mut Context<Self>) {
        self.undo.set_cooperative(false);
        self.apply_move(select, 0, None, cx);
    }

    pub(super) fn move_doc_end(&mut self, select: bool, cx: &mut Context<Self>) {
        self.undo.set_cooperative(false);
        self.apply_move(select, self.buffer.len(), None, cx);
    }

    fn page_row_count(&self) -> usize {
        let vh = self.list_state.viewport_bounds().size.height.as_f32();
        let lh = self.line_height.as_f32().max(1.);
        ((vh / lh).floor() as usize).saturating_sub(1).max(1)
    }

    pub(super) fn page(&mut self, select: bool, dir: isize, cx: &mut Context<Self>) {
        self.undo.set_cooperative(false);
        let rows = self.page_row_count() as isize;
        let (off, col) = movement::move_vertical(
            &self.buffer,
            self.selection.head(),
            dir * rows,
            self.selection.preferred_column,
        );
        self.apply_move(select, off, Some(col), cx);
    }

    // -----------------------------------------------------------------
    // 编辑主链
    // -----------------------------------------------------------------

    pub(super) fn insert_edit(&mut self, range: Range<usize>, text: &str, intent: EditIntent, cx: &mut Context<Self>) {
        // 操作级实时追踪（WY_TRACE）：一切编辑主链的单一漏斗——打字/IME/退格/删除/
        // 换行/Tab/粘贴/剪切/程序化 insert_text 都经此，在此登记 per-key 事件链起点，
        // 到 EditorElement::paint 结算「按键→绘制完成」的真实感知延迟与阶段 breakdown。
        if crate::prof::trace_active() {
            crate::prof::op_start_detail("ed_edit", &format!("intent={intent:?}"));
        }
        self.undo
            .start_tx(intent, (self.selection.anchor, self.selection.head));
        let edit = self.apply_edit_synced(range, text);
        let head = edit.inserted_range().end;
        let old_end = edit.replaced_range().end;
        self.last_edit = Some((edit.start, old_end, head));
        self.undo.push_edit(edit);
        self.selection.place(head);
        // 打字/退格/删除保留合作（连续合并）；换行/原子编辑断开
        self.undo.set_cooperative(match intent {
            EditIntent::Atomic => false,
            _ => !text.contains(['\n', '\r']),
        });
        self.kick_caret();
        self.reveal_caret();
        // 文本变更：查找匹配表按 buffer 版本失效重算（防抖同一条路径，见 `find_edit_invalidate`）。
        self.find_edit_invalidate(cx);
        cx.emit(EditorEvent::Edited);
        cx.notify();
    }

    pub(super) fn backspace(&mut self, cx: &mut Context<Self>) {
        if !self.selection.is_collapsed() {
            return self.delete_selection_atomic(cx);
        }
        let head = self.selection.head();
        if head == 0 {
            return;
        }
        let start = movement::char_left(self.buffer.text(), head);
        self.insert_edit(start..head, "", EditIntent::Backspace, cx);
    }

    pub(super) fn backspace_word(&mut self, cx: &mut Context<Self>) {
        if !self.selection.is_collapsed() {
            return self.delete_selection_atomic(cx);
        }
        let head = self.selection.head();
        if head == 0 {
            return;
        }
        let start = movement::word_left(self.buffer.text(), head);
        self.insert_edit(start..head, "", EditIntent::Backspace, cx);
    }

    pub(super) fn delete_forward(&mut self, cx: &mut Context<Self>) {
        if !self.selection.is_collapsed() {
            return self.delete_selection_atomic(cx);
        }
        let head = self.selection.head();
        if head >= self.buffer.len() {
            return;
        }
        let end = movement::char_right(self.buffer.text(), head);
        self.insert_edit(head..end, "", EditIntent::Delete, cx);
    }

    pub(super) fn delete_word_forward(&mut self, cx: &mut Context<Self>) {
        if !self.selection.is_collapsed() {
            return self.delete_selection_atomic(cx);
        }
        let head = self.selection.head();
        if head >= self.buffer.len() {
            return;
        }
        let end = movement::word_right(self.buffer.text(), head);
        self.insert_edit(head..end, "", EditIntent::Delete, cx);
    }

    fn delete_selection_atomic(&mut self, cx: &mut Context<Self>) {
        let range = self.selection.range();
        if range.is_empty() {
            return;
        }
        self.insert_edit(range, "", EditIntent::Atomic, cx);
    }

    fn insert_newline(&mut self, cx: &mut Context<Self>) {
        let at = self.selection.head();
        let row = self.buffer.line_of_offset(at);
        let indent: String = self
            .buffer
            .line_text(row)
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect();
        let text = format!("\n{indent}");
        let range = if self.selection.is_collapsed() {
            at..at
        } else {
            self.selection.range()
        };
        self.insert_edit(range, &text, EditIntent::Atomic, cx);
    }

    pub(super) fn tab(&mut self, outdent: bool, cx: &mut Context<Self>) {
        if outdent {
            return self.outdent(cx);
        }
        self.insert_edit(self.selection.range(), "\t", EditIntent::Atomic, cx);
    }

    fn outdent(&mut self, cx: &mut Context<Self>) {
        let at = self.selection.head();
        let row = self.buffer.line_of_offset(at);
        let ls = self.buffer.line_start(row);
        let line = self.buffer.line_text(row);
        let prefix: String = line
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect();
        if prefix.is_empty() {
            return;
        }
        let remove_bytes = if prefix.starts_with('\t') {
            '\t'.len_utf8()
        } else {
            let spaces = prefix
                .chars()
                .take(self.tab_size)
                .map(|c| c.len_utf8())
                .sum::<usize>();
            spaces.min(ls + line.len() - ls).max(1)
        };
        let end = (ls + remove_bytes).min(self.buffer.line_end(row) - ls + ls);
        let range = ls..end.max(ls).min(self.buffer.line_end(row));
        if range.end <= range.start {
            return;
        }
        self.insert_edit(range, "", EditIntent::Atomic, cx);
    }

    pub(super) fn select_all(&mut self, cx: &mut Context<Self>) {
        self.undo.set_cooperative(false);
        self.selection = Selection::from_range(0, self.buffer.len());
        self.kick_caret();
        cx.notify();
    }

    pub(super) fn copy(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if !self.selection.is_collapsed() {
            let text = self.buffer.text_in_range(self.selection.range());
            cx.write_to_clipboard(ClipboardItem::new_string(text.to_string()));
        }
    }

    pub(super) fn cut(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if !self.selection.is_collapsed() {
            let text = self.buffer.text_in_range(self.selection.range());
            cx.write_to_clipboard(ClipboardItem::new_string(text.to_string()));
            self.delete_selection_atomic(cx);
        }
    }

    pub(super) fn paste(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(item) = cx.read_from_clipboard()
            && let Some(text) = item.text() {
                self.paste_text(&text, cx);
            }
    }

    pub(super) fn undo(&mut self, cx: &mut Context<Self>) {
        let Some(tx) = self.undo.undo() else {
            return;
        };
        crate::prof::op_start("ed_undo");
        // 整组撤销的受替换区**bounding box**（撤销前坐标，见下方 last_edit 注）：
        // 编辑前先取首行/末行，供折叠候选 / 括号配对后台**增量**刷新登记行段。
        let a_off = tx.edits.iter().map(|e| e.start).min().unwrap_or(0);
        let oe_off = tx
            .edits
            .iter()
            .map(|e| e.start + e.new_text.len())
            .max()
            .unwrap_or(0)
            .min(self.buffer.len());
        let pre_rows = self.row_count;
        let line_a = self.buffer.line_of_offset(a_off);
        let line_b = self.buffer.line_of_offset(oe_off);
        for edit in tx.edits.iter().rev() {
            // 撤销前该编辑处于「新文本已插入」状态：被替换区 = inserted_range。
            let la = self.buffer.line_of_offset(edit.start);
            let lb = self.buffer.line_of_offset(
                edit.inserted_range().end.min(self.buffer.len()),
            );
            self.buffer.undo_edit(edit);
            self.row_count = self.buffer.line_count().max(1);
            self.folds.sync_edit(la, lb, &edit.old_text, self.row_count);
        }
        // 撤销整组：dirty 用覆盖所有编辑的最小−最大区间（bounding box，保守）。
        // 撤销把「已插入的新文本」替换回「旧文本」：被替换区（撤销前坐标）
        // = min start .. max(start + new_text.len)，恢复文本末 = start + 恢复字节和
        // （旧实现误用 old_text.len 当被替换区长 → 纯插入的撤销得到零宽 (a,a,a)，
        // App 增量桥/预览同步在撤销时收不到正确区间）。
        if let (Some(a), Some(oe)) = (
            tx.edits.iter().map(|e| e.start).min(),
            tx.edits.iter().map(|e| e.start + e.new_text.len()).max(),
        ) {
            let restored = tx.edits.iter().map(|e| e.old_text.len()).sum::<usize>();
            self.last_edit = Some((a, oe, a + restored));
        }
        // 折叠候选 / 括号配对增量：行段 = [line_a, min_stop)（min_stop 取新坐标
        // 首个未受影响行的**上界**，conservative 偏大 → 增量包络只会更大、仍正确）。
        let delta = self.row_count as i64 - pre_rows as i64;
        let min_stop = line_b
            .saturating_add(delta.max(0) as usize)
            .saturating_add(1);
        self.mark_scan_dirty_incremental(line_a, min_stop, delta);
        self.selection = Selection::from_range(tx.before.0, tx.before.1);
        self.resync_list(true);
        self.kick_caret();
        self.reveal_caret();
        // 撤销 = 文本变更：查找匹配表失效重算。
        self.find_edit_invalidate(cx);
        cx.emit(EditorEvent::Edited);
        cx.notify();
    }

    pub(super) fn redo(&mut self, cx: &mut Context<Self>) {
        let Some(tx) = self.undo.redo() else {
            return;
        };
        crate::prof::op_start("ed_redo");
        // 整组重做的受替换区 bounding box（重做前坐标，见下方 last_edit 注）：先取
        // 首行/末行供折叠候选 / 括号配对后台**增量**刷新登记行段。
        let a_off = tx.edits.iter().map(|e| e.start).min().unwrap_or(0);
        let oe_off = tx
            .edits
            .iter()
            .map(|e| e.start + e.old_text.len())
            .max()
            .unwrap_or(0)
            .min(self.buffer.len());
        let pre_rows = self.row_count;
        let line_a = self.buffer.line_of_offset(a_off);
        let line_b = self.buffer.line_of_offset(oe_off);
        for edit in tx.edits.iter() {
            // 重做前该编辑处于「旧文本被移除」状态：被替换区 = replaced_range。
            let la = self.buffer.line_of_offset(edit.start);
            let lb = self.buffer.line_of_offset(
                edit.replaced_range().end.min(self.buffer.len()),
            );
            self.buffer.redo_edit(edit);
            self.row_count = self.buffer.line_count().max(1);
            self.folds.sync_edit(la, lb, &edit.new_text, self.row_count);
        }
        // 重做整组：把「旧文本」重新替换回「新文本」。被替换区（重做前坐标）
        // = min start .. max(start + old_text.len)，新文本末 = start + 新文本字节和。
        // （与撤销同款修正：旧实现误用 new_text.len 当被替换区，交叉删除/粘贴的重做
        // 同样给不出正确区间。）
        if let (Some(a), Some(oe)) = (
            tx.edits.iter().map(|e| e.start).min(),
            tx.edits
                .iter()
                .map(|e| e.start + e.old_text.len())
                .max(),
        ) {
            let inserted = tx.edits.iter().map(|e| e.new_text.len()).sum::<usize>();
            self.last_edit = Some((a, oe, a + inserted));
        }
        // 折叠候选 / 括号配对增量：行段 = [line_a, min_stop)（同撤销，conservative）。
        let delta = self.row_count as i64 - pre_rows as i64;
        let min_stop = line_b
            .saturating_add(delta.max(0) as usize)
            .saturating_add(1);
        self.mark_scan_dirty_incremental(line_a, min_stop, delta);
        self.selection = Selection::from_range(tx.after.0, tx.after.1);
        self.resync_list(true);
        self.kick_caret();
        self.reveal_caret();
        // 重做 = 文本变更：查找匹配表失效重算。
        self.find_edit_invalidate(cx);
        cx.emit(EditorEvent::Edited);
        cx.notify();
    }
}
