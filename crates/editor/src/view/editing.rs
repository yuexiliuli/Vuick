//! 键盘分发 / 光标移动 / 折叠 / 编辑命令 / 撤销重做。`on_key_down` 原样键分发、
//! 方向/词/行/文档移动与选区扩展、代码折叠切换与括号标尺竖线（`bracket_guide_segments*`）、
//! 插入/退格/删除/换行/Tab、复制剪切粘贴、撤销与重做。

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
    BracketGuidesMode,
    BRACKET_GUIDE_CLOSE_TAIL_LINES,
    BRACKET_GUIDE_OPEN_HEAD_LINES,
    INDENT_GUIDE_INDENT_SIZE,
};
use config::limits::{
    FOLD_CANDIDATE_FULL_MAX_LINES,
};
use crate::editor::fold::{
    BracketPair,
    BracketPairLite,
    Fold,
    blank_guide_level,
    indent_guide_level,
    leading_indent,
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

/// 括对竖线取色**单源**（E3 封口，2026-11-14）：`palette[depth % 3]` 循环池 + active →
/// alpha 1.0（否则 0.3）。render 竖线 / 钩线 / active 引导高亮 / 测试 getter 四处共用；
/// 改取色公式只此一处，禁止各消费点再散写 `palette[d%3].opacity(…)` 式取色（防分叉漂移）。
pub(crate) fn bracket_guide_color(palette: &[gpui::Hsla; 3], depth: usize, active: bool) -> gpui::Hsla {
    let c = palette[depth % 3];
    if active {
        c.opacity(1.0)
    } else {
        c.opacity(0.3)
    }
}

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
        self.caret.kick();
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
        let base_indent = leading_indent(self.buffer.line_text(start).as_ref()).bytes();
        let mut end = None;
        for r in (start + 1)..self.row_count {
            let line = self.buffer.line_text(r);
            let trimmed = line.trim_start();
            if trimmed.is_empty() {
                continue;
            }
            let indent = leading_indent(line.as_ref()).bytes();
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
            self.bracket_char_depths.clear();
            self.fold_boundaries.clear();
            self.bracket_boundaries.clear();
            self.fold_snapshots.clear();
            self.bracket_snapshots.clear();
        } else {
            let rope = self.buffer.text();
            let (folds, pairs, chars, fb, pb, fsnaps, bsnaps) =
                crate::editor::fold::full_fold_brackets(
                    |r| crate::editor::buffer::line_shared(rope, r),
                    self.row_count,
                );
            self.fold_candidates = folds;
            self.fold_candidates.sort_by_key(|f| f.start);
            self.bracket_pairs = pairs;
            self.bracket_pairs.sort_by_key(|p| p.open_line);
            self.bracket_char_depths = chars;
            self.bracket_char_depths.sort_by_key(|c| (c.line, c.col));
            self.fold_boundaries = fb;
            self.bracket_boundaries = pb;
            self.fold_snapshots = fsnaps;
            self.bracket_snapshots = bsnaps;
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
                        this.bracket_char_depths.clone(),
                        this.fold_boundaries.clone(),
                        this.bracket_boundaries.clone(),
                        this.fold_snapshots.clone(),
                        this.bracket_snapshots.clone(),
                        this.scan_pending_edit,
                        this.scan_multi_edit,
                    )
                });
                let Ok((
                    epoch,
                    row_count,
                    rope,
                    folds,
                    pairs,
                    chars,
                    fb,
                    pb,
                    fsnaps,
                    bsnaps,
                    pending,
                    multi,
                )) = snap
                else {
                    return;
                };
                let (folds, brackets, bracket_chars, nfb, npb, nfsnaps, nbsnaps, was_incremental) =
                    cx.background_executor()
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
                                &chars,
                                &fb,
                                &pb,
                                &fsnaps,
                                &bsnaps,
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
                            // 落地即排序：折叠候选按 `start`、括号配对按 `open_line`、
                            // 括号字符按 `(line,col)` 升序（gutter/标尺/行着色查询走二分）。
                            // 仅内部次序，语义不变。
                            this.fold_candidates = folds;
                            this.fold_candidates.sort_by_key(|f| f.start);
                            this.bracket_pairs = brackets;
                            this.bracket_pairs.sort_by_key(|p| p.open_line);
                            this.bracket_char_depths = bracket_chars;
                            this.bracket_char_depths.sort_by_key(|c| (c.line, c.col));
                            // 增量边界更新为本次全景（供下一次增量续用），并消费
                            // 增量描述（若期间又有编辑，epoch 已变、不会走到这里）。
                            this.fold_boundaries = nfb;
                            this.bracket_boundaries = npb;
                            this.fold_snapshots = nfsnaps;
                            this.bracket_snapshots = nbsnaps;
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

    /// 行内 `[0..byte_col)` 的**宽度单位**（空格=1、Tab=4、ASCII=1、其余=2，与渲染等宽格
    /// 一致）——开/闭括号所在列。`byte_col` 越界夹到行尾。
    /// `pub(super)`：render 的横向钩线（`bracket_hook`）也按同口径取端点列。
    pub(super) fn guide_col_prefix_units(&self, line: usize, byte_col: usize) -> usize {
        let s = crate::editor::buffer::line_shared(self.buffer.text(), line);
        let s = s.as_ref();
        let upto = byte_col.min(s.len());
        s[..upto]
            .chars()
            .map(crate::editor::buffer::char_width_units)
            .sum::<usize>()
    }

    /// 闭括号所在 `close_line`、字节列 `close_col` **之前**是否有非空白字符（VSCode
    /// `hasTextBeforeClosingBracket`）：false = 闭行独占 `}`（行首闭合）→ 竖线段不含闭行、
    /// 钩线画顶边；true = 闭行前尚有代码（`} else` 等）→ 竖线段含闭行、钩线画底边。
    /// 与 `bracket_guide_segments_flagged`（跨度）和 render 钩线共用同一谓词（K153 封口）。
    pub(super) fn has_text_before_close(&self, close_line: usize, close_col: usize) -> bool {
        let s = crate::editor::buffer::line_shared(self.buffer.text(), close_line);
        let mut byte = 0usize;
        for ch in s.chars() {
            if byte >= close_col {
                break;
            }
            if !ch.is_whitespace() {
                return true;
            }
            byte += ch.len_utf8();
        }
        false
    }

    /// C4 竖线列：`c = min(开括号列 units, 闭括号列 units, 对内最小非闭缩进 + 1)`。
    /// 良构下退化为开行缩进（逐位 ≡ 旧 guide_units）；坏代码下由内层缩进就近接管。
    /// **col0 保留可画**（VS Code 禁 0，但本仓良构测试锁定外层 fn/impl 块；设计默认保留）。
    /// 「对内最小非闭缩进」为**落地期预存字段** [`BracketPair::guide_body_min_units`]
    /// （扫描/增量期算好；帧内 O(1) 只读，不再逐内行 `line_shared` 堆扫——K153/K160
    /// 单源 + parity 测试保证与旧运行时算法逐字节一致），唯一保留的运行时行读是开/闭
    /// 两行的 `guide_col_prefix_units`（每对恒定 2 次，与括对尺寸无关）。
    fn guide_pair_column(&self, p: &BracketPair) -> usize {
        self.guide_pair_column_fields(
            p.open_line,
            p.open_col,
            p.close_line,
            p.close_col,
            p.guide_body_min_units,
        )
    }

    /// 同 [`Self::guide_pair_column`]，但取更轻量的 `BracketPairLite`——供 render 的横向
    /// 钩线（active 括对）与缩进抑制表（C3）复用同一列口径（K153：渲染与几何 getter 同源）。
    pub(super) fn guide_pair_column_lite(&self, p: &BracketPairLite) -> usize {
        self.guide_pair_column_fields(
            p.open_line,
            p.open_col,
            p.close_line,
            p.close_col,
            p.guide_body_min_units,
        )
    }

    fn guide_pair_column_fields(
        &self,
        open_line: usize,
        open_col: usize,
        close_line: usize,
        close_col: usize,
        body_min_units: Option<u32>,
    ) -> usize {
        let open_units = self.guide_col_prefix_units(open_line, open_col);
        let close_units = self.guide_col_prefix_units(close_line, close_col);
        match body_min_units {
            Some(m) => open_units.min(close_units).min(m as usize + 1),
            None => open_units.min(close_units),
        }
    }

    /// 同 [`Self::bracket_guide_segments`]，但每个合并段额外带 `(depth, active)`：
    /// - `depth` = 该段所属括对的嵌套层级（`BracketPair.depth`），合并同列样本段时
    ///   **仅同 depth 合并**（异 depth 各成段）——对齐 VS Code bracket guides 按
    ///   `depth % 3` 循环取色，深度不同色不同，不能糊成一色；
    /// - `active` = 该段所属括对是否为**活动括对**（[`Self::active_pair`]，由主光标
    ///   严格内含派生、随光标移动跟随）。
    /// `mode` = [`BracketGuidesMode`]：**Off = 不产竖线**（只留钩线层，见 render
    /// `bracket_guide_hooks_layer`）；**Active = 仅活动括对产段**（其余括对跳过、不产线，
    /// 其列灰缩进线正常显示/共存，对齐 VSCode `bracketPairs:'active'`）；All = 全部括对产段
    /// （VSCode `bracketPairs:'always'`，现行为）。
    /// `sf` = 窗口 scale_factor：段几何行距用 pitch（吸附行距），与渲染吸附同源
    /// （见 `snap_guide_extent`），确保竖线底端贴真实渲染行网格、不逐行漂移。
    pub(super) fn bracket_guide_segments_flagged(
        &self,
        top_vis: usize,
        off: f32,
        lh: f32,
        sf: f32,
        harea_w: f32,
        view_h: f32,
        active: Option<BracketPairLite>,
        mode: BracketGuidesMode,
    ) -> Vec<(f32, f32, f32, usize, bool)> {
        if self.bracket_pairs.is_empty() {
            return Vec::new();
        }
        if mode == BracketGuidesMode::Off {
            return Vec::new(); // Off：不产竖线（渲染只挂钩线层，见 `bracket_guide_hooks_layer`）。
        }
        // 竖线端点微调（× 行高，见 config/layout.rs）：底部伸入闭行贴 `}` 字形，
        // 顶部从首覆盖行向下缩进控制留白。均为可配置行高系数。
        let head = BRACKET_GUIDE_OPEN_HEAD_LINES * lh;
        let tail = BRACKET_GUIDE_CLOSE_TAIL_LINES * lh;
        // 段几何以 pitch（吸附行距）为行距，与 snap_guide_extent 同源：行高经 gpui
        // round_half_toward_zero 预吸附成整数设备 px（正值 = ceil(lh·sf − 0.5)，vendored
        // util.rs:130），竖线累计必须用吸附后行距，否则 lh·sf 小数部分 >0.5 时逐行短
        // 1px、底端累计不贴「下一行顶」（须与 taffy 同式，勿用 round()）。
        let pitch = super::layout::pitch_for_line_height(lh * sf) / sf;
        let bottom_vis = top_vis.saturating_add(((view_h / lh).ceil() as usize) + 1);
        // 窗口化：见 `bracket_pair_window`（lb/ub 二分口径与 suppress 同源）。竖线只可能
        // 来自 `open_line ∈ [top_vis − 回看, bottom_vis]` 的配对（回看常量
        // `BRACKET_GUIDE_WINDOW_LOOKBACK_LINES`）；原实现对**全部**配对逐条算相交判定
        // （大文档 1k+ 对 × 每帧 ms 级白烧），实际视口内只有个位数可见段。
        let window = self.bracket_pair_window(top_vis, view_h, lh);
        // (x, 视觉顶行, 视觉底行**开区间**, depth, 是否 active) —— 先整列收集，再合并
        // 同列**同 depth** 重叠/相邻段。
        let mut intervals: Vec<(f32, usize, usize, usize, bool)> = Vec::new();
        for p in &self.bracket_pairs[window] {
            if p.open_line >= p.close_line {
                continue; // 过期（编辑后未及时重扫）的配对；重扫会清理。
            }
            let is_active = active == Some(BracketPairLite::from(p));
            // 模式过滤：Off 已早返回；Active 仅活动括对产段（`includeInactive=false`，VSCode
            // active 口径）；All 全部产段（现行为）。
            if mode == BracketGuidesMode::Active && !is_active {
                continue;
            }
            let vs = self.folds.visual_row(p.open_line);
            let ve = self.folds.visual_row(p.close_line);
            // 竖线段跨度对齐 VSCode（2026-11-14 第 4 片）：`[open .. close]` **含开行**；末行
            // 按「闭括号前有无非空白文本」决定含/不含——闭行独占 `}`（如 `}`/`    }` 行首闭合，
            // `hasTextBeforeClosingBracket`=false）→ 段到闭行前止（开区间不含闭行）；闭行前
            // 尚有代码（`} else {` / `x }`）→ 含闭行（VSCode 同口径）。
            let seg_top = vs;
            let seg_bot_excl = if self.has_text_before_close(p.close_line, p.close_col) {
                ve.saturating_add(1)
            } else {
                ve
            };
            if seg_bot_excl <= seg_top {
                continue; // 无体（开行即闭行/折叠压缩）→ 无竖线。
            }
            if seg_bot_excl <= top_vis || seg_top >= bottom_vis {
                continue; // 与视口不相交。
            }
            // C4 竖线列：min(开括号列, 闭括号列, 对内最小非闭缩进+1)——良构退化为开行缩进。
            let x = self.col_to_px(self.guide_pair_column(p));
            if x < 0.0 || x > harea_w {
                continue; // 出文本区裁掉。
            }
            intervals.push((
                x.round().max(0.0),
                seg_top.max(top_vis),
                seg_bot_excl.min(bottom_vis),
                p.depth,
                is_active,
            ));
        }
        // 按列排序，合并同列**同 depth** 重叠/相邻区间。
        intervals.sort_by(|a, b| {
            a.0.partial_cmp(&b.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.1.cmp(&b.1))
        });
        let mut out: Vec<(f32, f32, f32, usize, bool)> = Vec::new();
        let mut cur: Option<(f32, usize, usize, usize, bool)> = None;
        for (x, t, b, d, hl) in intervals {
            match &mut cur {
                Some((cx, ct, cb, cd, ca)) if *cx == x && t <= *cb && *cd == d => {
                    *cb = (*cb).max(b);
                    *ca |= hl;
                }
                _ => {
                    if let Some((cx, ct, cb, cd, ca)) = cur.take() {
                        out.push((
                            cx,
                            (ct as f32 - top_vis as f32) * pitch - off + head,
                            (cb - ct) as f32 * pitch - head + tail,
                            cd,
                            ca,
                        ));
                    }
                    cur = Some((x, t, b, d, hl));
                }
            }
        }
        if let Some((cx, ct, cb, cd, ca)) = cur {
            out.push((
                cx,
                (ct as f32 - top_vis as f32) * pitch - off + head,
                (cb - ct) as f32 * pitch - head + tail,
                cd,
                ca,
            ));
        }
        out
    }

    /// 括号配对的**视口二分窗口**下标 `bracket_pairs[lb..ub]`（竖线段 / 缩进让位两消费方
    /// 同口径共用，防二分再漂移）：配对按 `open_line` 升序，`lb` = 首个
    /// `open_line ≥ top_vis − 回看`（[`config::limits::BRACKET_GUIDE_WINDOW_LOOKBACK_LINES`]）、
    /// `ub` = 首个 `open_line > bottom_vis`（`bottom_vis = top_vis + ceil(view_h/lh) + 1`）。
    /// 窗口语义见 `bracket_guide_segments_flagged`：开括号在视口之下（`> bottom_vis`）
    /// 的配对必不相交；开在「回看」行以上的大跨度配对（竖线远超一屏）视为无意义跳过。
    fn bracket_pair_window(&self, top_vis: usize, view_h: f32, line_height: f32) -> Range<usize> {
        let bottom_vis =
            top_vis.saturating_add(((view_h / line_height).ceil() as usize) + 1);
        let lb = self.bracket_pairs.partition_point(|p| {
            p.open_line
                < top_vis.saturating_sub(config::limits::BRACKET_GUIDE_WINDOW_LOOKBACK_LINES)
        });
        let ub = self.bracket_pairs.partition_point(|p| p.open_line <= bottom_vis);
        lb..ub
    }

    pub(super) fn bracket_guide_segments(
        &self,
        top_vis: usize,
        off: f32,
        lh: f32,
        sf: f32,
        harea_w: f32,
        view_h: f32,
        mode: BracketGuidesMode,
    ) -> Vec<(f32, f32, f32)> {
        // 未点亮（active=None）→ 与旧实现语义一致（3 元组，剥掉 depth/active）；active 由
        // 编辑器真实活动括对派生（与渲染同源）。
        self.bracket_guide_segments_flagged(
            top_vis, off, lh, sf, harea_w, view_h, self.active_pair(), mode,
        )
        .into_iter()
        .map(|(x, y, h, _, _)| (x, y, h))
        .collect()
    }

    /// C3 缩进让位抑制表（2026-11-14 第 4 片）：可视窗口内按模式取 bracket pairs → `(可视行,
    /// 引导列)`（每对覆盖 `visual_row(open) ..= visual_row(close)`，含开行、按 A 同款「闭行
    /// 前有文本则含闭行」尽量一致、可保守含闭行）。供 `indentation_guide_segments_flagged`
    /// 让位：括对行不点亮缩进 active、括对引导列不画灰线（该列已由彩色 bracket 竖线接管）。
    /// `mode` = [`BracketGuidesMode`]：Active = **仅填活动括对**的（行,引导列）（其余括对的灰
    /// 缩进线/active 高亮正常回显、与其共存，VSCode `bracketPairs:'active'` 态）；All = 全部
    /// 括对（现行为）；Off = 空表（无 bracket 线 → 无让位）。
    /// 窗口口径与 `bracket_guide_segments_flagged` 的二分窗口**完全一致**；空表（无括对）→
    /// 缩进行为与旧实现逐位一致。
    pub(super) fn bracket_guide_suppress(
        &self,
        top_vis: usize,
        view_h: f32,
        lh: f32,
        mode: BracketGuidesMode,
    ) -> Vec<(usize, usize)> {
        if mode == BracketGuidesMode::Off || self.bracket_pairs.is_empty() {
            return Vec::new();
        }
        let bottom_vis = top_vis.saturating_add(((view_h / lh).ceil() as usize) + 1);
        let window = self.bracket_pair_window(top_vis, view_h, lh);
        // Active 模式只让位活动括对（VSCode `bracketPairs:'active'`：仅其列的灰线被彩色竖线
        // 接管）。All 模式 active 字段无用。
        let active_lite = if mode == BracketGuidesMode::Active {
            self.active_pair()
        } else {
            None
        };
        let mut out = Vec::new();
        for p in &self.bracket_pairs[window] {
            if p.open_line >= p.close_line {
                continue; // 过期配对（编辑后未及时重扫）。
            }
            if let Some(al) = active_lite {
                if al != BracketPairLite::from(p) {
                    continue; // Active：跳过非活动括对，其灰缩进线保留。
                }
            }
            let col = self.guide_pair_column(p);
            let vs = self.folds.visual_row(p.open_line);
            let ve = self.folds.visual_row(p.close_line);
            // 视口夹逼（c2）：消费方 `indentation_guide_segments_flagged` 只查
            // `top_vis..bottom_vis` 内行，跨屏配对的 `vs..=ve` 越出视口外行全是死条目
            // （大块可数千行），夹到视口内再逐一填（逐位保真：表内行子集不变）。
            let rs = vs.max(top_vis);
            let re = ve.min(bottom_vis.saturating_sub(1));
            if rs <= re {
                for r in rs..=re {
                    out.push((r, col));
                }
            }
        }
        out
    }

    /// **块体模型关联括对**（2026-11-14 用户 3 夹具实测钉死，唯一真值；替代旧「严格内含」
    /// 缩进级口径）：对点击行 r（逻辑行号 = `selection.head()` 所在行），关联括对 P 按
    /// 优先级（任一命中即止）：
    ///   1. 存在 `open_line == r` 的括对 → P = 其中 depth 最大者（点 `{` 开行 → 该块）；
    ///   2. 否则存在 `open_line < r < close_line`（**严格含 r**）→ P = 其中 depth 最大者
    ///      （点块内任意深行 → 最内块）；
    ///   3. 否则存在 `close_line == r` 的括对 → P = 其中 depth 最大者（点 `}`：步骤 2
    ///      无外层严格含 r 才到此处——if 的 `}` 在 fn 内 → fn；fn 顶层 `}` → fn）；
    ///   4. 否则 None（无关联块 → 无高亮）。
    /// 纯行号派生（不再看光标字符列）。`bracket_pairs` 已按 `open_line` 升序：步骤 1 用
    /// 二分前缀定界（mega 同开行多配对取 depth 最大）；步骤 2/3 只扫 `open_line ≥ r − 回看`
    /// 的窗口（跨 `BRACKET_GUIDE_WINDOW_LOOKBACK_LINES` 长活括对省略 = 竖线窗口同一次已
    /// 批准语义）。等 depth 保留序号最前者（确定性）。
    pub(super) fn active_pair(&self) -> Option<BracketPairLite> {
        let caret = self.selection.head().min(self.buffer.len());
        let r = self.buffer.line_of_offset(caret);
        // 1) 开行：open_line == r 中取 depth 最大。
        let lo = self.bracket_pairs.partition_point(|p| p.open_line < r);
        let hi = self.bracket_pairs.partition_point(|p| p.open_line <= r);
        if let Some(p) = self.pick_max_depth(self.bracket_pairs[lo..hi].iter()) {
            return Some(BracketPairLite::from(p));
        }
        // 2) 严格含 r：open_line < r < close_line 中取 depth 最大（最内块）。窗口化（c1）：
        //   候选的 open_line 必 < r（严格含），故只扫 `open_line ≥ r − 回看` 的下界起——
        //   跨 > `BRACKET_GUIDE_WINDOW_LOOKBACK_LINES` 行的长活括对在光标深处视为无意义
        //   跳过（与竖线段的窗口省略同一次已批准语义），避免每键 O(#括对≤光标行) 全前缀扫。
        let win_lo = self
            .bracket_pairs
            .partition_point(|p| p.open_line < r.saturating_sub(
                config::limits::BRACKET_GUIDE_WINDOW_LOOKBACK_LINES,
            ));
        let cands2 = self.bracket_pairs[win_lo..hi]
            .iter()
            .filter(|p| p.open_line < r && r < p.close_line);
        if let Some(p) = self.pick_max_depth(cands2) {
            return Some(BracketPairLite::from(p));
        }
        // 3) 闭行：close_line == r 中取 depth 最大（点 `}`）。非过期对
        //   open_line < close == r ⇒ open_line ≤ r−1 < r ≤ hi 段内，与步骤 2 同取下界
        //   且收住上界 `hi`（只扫窗口内对、不越前缀）。
        let cands3 = self.bracket_pairs[win_lo..hi]
            .iter()
            .filter(|p| p.close_line == r);
        self.pick_max_depth(cands3).map(BracketPairLite::from)
    }

    /// 候选 pair 迭代器取 depth 最大者（跳过过期对 `open >= close`；等 depth 保留首个，
    /// 确定性）。`active_pair` 三个步骤共用。
    fn pick_max_depth<'a>(
        &self,
        cands: impl Iterator<Item = &'a BracketPair>,
    ) -> Option<&'a BracketPair> {
        let mut best: Option<&BracketPair> = None;
        for p in cands {
            if p.open_line >= p.close_line {
                continue; // 过期配对（编辑后未及时重扫）。
            }
            match best {
                None => best = Some(p),
                Some(b) if b.depth < p.depth => best = Some(p),
                _ => {}
            }
        }
        best
    }

    /// active 缩进高亮色（**块体模型**，2026-11-14 用户实测口径）：光标行关联括对
    /// （[`Self::active_pair`]，块体模型 1/2/3）→ 该括对色（`bracket_guide_palette[depth % 3]`）；
    /// 无关联括对 → VSCode 默认灰 [`Self::active_guide_color`]。render 的缩进 active 高亮
    /// 与测试 getter 共用同一个决定路径（K153 封口，防颜色路径再分叉）。
    pub(super) fn active_guide_highlight_color(&self) -> gpui::Hsla {
        self.active_pair()
            .map(|ap| bracket_guide_color(&self.bracket_guide_palette, ap.depth, true))
            .unwrap_or(self.active_guide_color)
    }

    /// （测试）`active_guide_highlight_color` 只读暴露（缩进 active 高亮色断言用）。
    #[cfg(test)]
    pub fn test_active_guide_highlight_color(&self) -> gpui::Hsla {
        self.active_guide_highlight_color()
    }

    /// （测试）`test_bracket_guide_segments_flagged` 用的 active pair（活动括对）。
    #[cfg(test)]
    pub fn test_active_pair(&self) -> Option<BracketPairLite> {
        self.active_pair()
    }

    /// （测试）bracket 竖线段 `(x, y_rel, h, depth, active)`（含 depth/active，供
    /// 按深度取色 / 活动高亮断言；active 由编辑器真实活动括对派生——与渲染同源）。
    /// `mode` = [`BracketGuidesMode`]：All = 全部括对（active 标记随真实活动括对）；
    /// Active = 仅活动括对产段。
    #[cfg(test)]
    pub fn test_bracket_guide_segments_flagged(
        &self,
        top_vis: usize,
        off: f32,
        lh: f32,
        sf: f32,
        harea_w: f32,
        view_h: f32,
        mode: BracketGuidesMode,
    ) -> Vec<(f32, f32, f32, usize, bool)> {
        self.bracket_guide_segments_flagged(
            top_vis, off, lh, sf, harea_w, view_h, self.active_pair(), mode,
        )
    }

    /// 缩进引导线（indentation guides，§8 视觉主源）带 active 高亮位的段集。逐**可视行**
    /// （折叠感知）计算：非空行 level = `indent_guide_level(leading_indent.units(), is)` →
    /// 画列 `{ (L−1)·is | L=1..=level }`（0-based g=0,is,2is,…）；空/纯空白行按可视序
    /// 上一/下一**非空**可视行的缩进取 `blank_guide_level` 续线（折叠体内行不出现在可视序
    /// → 不产线；折叠头行照常画）。
    /// 列段化：同列连续可视行（高亮位相同）合并成一条竖线段；像素 `x=g·cw−hscroll`、
    /// 出 `[0, harea_w]` 裁剪（与 bracket 段同式）；段几何以 pitch（吸附行距）为行距、
    /// 顶部 `(ct−top_vis)·pitch−off`、高 `(cb−ct)·pitch`——同 `snap_guide_extent` 行距。
    /// `active` = Option<`(active_col, group 首可视行, group 末可视行 + 1, 光标可视行)`>：
    /// §8.3 active 视觉（**块体模型**，2026-11-14 用户实测唯一真值）=
    /// 在 `active_col` 上、**块体区间 `[体首, 体末+1)`（= 关联括对 `[visual(open)+1,
    /// visual(close))`，开含闭不含）内**的**全部连续段**高亮——整块体点亮（不只光标行）。
    /// group 由 [`Self::active_guide_group`] 从 `selection.head()` 光标的块体模型关联括对
    /// P 派生：块体外该列段不亮、该列未绘出的行（缩进不足 / `}` 行）自然断开不散射。列与
    /// 体区间由调用方从光标派生 → 方向键/点击/折叠均跟随。
    pub(super) fn indentation_guide_segments_flagged(
        &self,
        top_vis: usize,
        off: f32,
        lh: f32,
        sf: f32,
        harea_w: f32,
        view_h: f32,
        active: Option<(usize, usize, usize, usize)>,
        suppress: &[(usize, usize)],
    ) -> Vec<(f32, f32, f32, bool)> {
        let is = INDENT_GUIDE_INDENT_SIZE;
        // C3 让位（2026-11-14 第 4 片）：`suppress` = `(可视行, 引导列)` 表（见
        // `bracket_guide_suppress`）。列查用 HashSet 摊 O(1)；`supp_rows`（表内出现的行）
        // 用于 active 高亮让位——括对行保留灰线但不点亮（VSCode：bracket guide 竖线已在该
        // 列/行接管画面）。空表 → 与旧实现逐位一致。
        let supp: std::collections::HashSet<(usize, usize)> =
            suppress.iter().copied().collect();
        let supp_rows: std::collections::HashSet<usize> =
            suppress.iter().map(|&(r, _)| r).collect();
        // 段几何行距 = 吸附行距 pitch（与 bracket 段 / snap_guide_extent 同源，见
        // `bracket_guide_segments_flagged` 注释）。
        let pitch = super::layout::pitch_for_line_height(lh * sf) / sf;
        let vcount = self.folds.visual_line_count();
        let total = self.folds.total();
        let bottom_vis = top_vis
            .saturating_add(((view_h / lh).ceil() as usize) + 1)
            .min(vcount.max(top_vis));

        // active 高亮：列 + 块区间（可视行，含开行）+ 光标可视行（§8.3）。
        let (active_col, active_block, _caret_vis) = match active {
            Some((c, v_open, v_close, cv)) => (Some(c), Some((v_open, v_close)), cv),
            None => (None, None, 0),
        };

        // 可视行 → 缩进 units（`None` = 空/纯空白行或越文档尾——空行与尾行都不直接产级，
        // 空行改由可视邻居续线）。
        let indent_of = |visual: usize| -> Option<i32> {
            if visual >= vcount {
                return None;
            }
            let logical = self.folds.logical_row(visual);
            if logical >= total {
                return None;
            }
            let s = crate::editor::buffer::line_shared(self.buffer.text(), logical);
            let s = s.as_ref();
            if s.trim_start().is_empty() {
                return None;
            }
            Some(leading_indent(s).units() as i32)
        };

        // 每可视行画列集 → 按列收集可视行（active 高亮在段化时按「带区间内全部连续段」定）。
        let mut by_col: std::collections::BTreeMap<usize, Vec<usize>> =
            std::collections::BTreeMap::new();
        // 空行续线取值（仅复杂度优化，行为逐位一致）：上一非空用**游标**（随行移动 O(1)），
        // 首值 = 视口上方最近非空行（一次定位，与无界扫 `(0..v).rev()` 一致——视口首行即
        // 空行时仍取到上方代码）；下一非空用**惰性预取**（`next_cand` 缓存 + `next_scan_from`
        // 短路），只前进不回头 → 长纯空白段/近全空 buffer 上每空行摊销 O(1)，不逐空行无界扫。
        let mut prev_indent: Option<i32> = (0..top_vis).rev().find_map(|w| indent_of(w));
        let mut next_cand: Option<(usize, i32)> = None;
        let mut next_scan_from: usize = top_vis;
        for v in top_vis..bottom_vis {
            // 缓存的下一非空行已被当前行到达/越过 → 作废。
            if let Some((w, _)) = next_cand {
                if w <= v {
                    next_cand = None;
                }
            }
            let level = match indent_of(v) {
                Some(i) => {
                    prev_indent = Some(i);
                    indent_guide_level(i as usize, is) as usize
                }
                None => {
                    // 空行：A=上一非空（游标）、B=下一非空（预取）。
                    let next = match next_cand {
                        Some((w, ind)) if w > v => Some(ind),
                        _ => {
                            let from = (v + 1).max(next_scan_from);
                            let mut found: Option<(usize, i32)> = None;
                            for w in from..vcount {
                                if let Some(wi) = indent_of(w) {
                                    found = Some((w, wi));
                                    break;
                                }
                            }
                            match found {
                                Some((w, ind)) => {
                                    next_cand = Some((w, ind));
                                    next_scan_from = w + 1;
                                    Some(ind)
                                }
                                None => {
                                    next_scan_from = vcount; // 已确认其后无非空行 → 短路。
                                    None
                                }
                            }
                        }
                    };
                    blank_guide_level(prev_indent, next, is as i32) as usize
                }
            };
            if level == 0 {
                continue;
            }
            for l in 1..=level {
                let g = (l - 1) * is;
                if supp.contains(&(v, g)) {
                    continue; // 括对引导列压掉缩进灰线（该列已由彩色 bracket 竖线接管）。
                }
                by_col.entry(g).or_default().push(v);
            }
        }

        // 列段化：同列连续可视行合并成一条竖线段（x 平移/裁剪同 bracket）。active 列上
        // 「带区间 `[v_open, v_close)` 内的全部连续段」点亮（整带；其余列/段不亮）。
        let mut out: Vec<(f32, f32, f32, bool)> = Vec::new();
        for (g, samples) in by_col {
            let raw_x = self.col_to_px(g);
            if raw_x < 0.0 || raw_x > harea_w {
                continue; // 出 `[0, harea_w]` 裁掉（x<0 的整列不画，同 bracket 段）。
            }
            let x = raw_x.round().max(0.0);
            // 逐「自然连续组」输出：active 列上，组与带区间 `[v_open, v_close)` 的交叠段
            // 点亮（整带，跨空行续线的贯通段同亮），拆「前不亮 / 亮 / 后不亮」；带外/其余
            // 列整段不亮。C3：`supp_rows` 中行不点亮（保留灰线）——按逐行 lit 位合并 run，
            // 空表时与「前/亮/后」三分段逐位一致。
            let mut i = 0;
            while i < samples.len() {
                let mut j = i + 1;
                while j < samples.len() && samples[j] == samples[j - 1] + 1 {
                    j += 1;
                }
                let gs = samples[i];
                let ge = samples[j - 1];
                let mut emit = |ct: usize, cb: usize, hl: bool| {
                    out.push((
                        x,
                        (ct as f32 - top_vis as f32) * pitch - off,
                        (cb - ct) as f32 * pitch,
                        hl,
                    ));
                };
                let row_lit = |r: usize| {
                    active_col == Some(g)
                        && active_block.map_or(false, |(vo, vc)| r >= vo && r < vc)
                        && !supp_rows.contains(&r)
                };
                let mut r = gs;
                while r <= ge {
                    let lit_here = row_lit(r);
                    let mut r2 = r + 1;
                    while r2 <= ge && row_lit(r2) == lit_here {
                        r2 += 1;
                    }
                    emit(r, r2, lit_here);
                    r = r2;
                }
                i = j;
            }
        }
        out
    }

    /// 无 active 高亮（active=None）的缩进引导线段（3 元组，与 bracket 段同形）。
    pub(super) fn indentation_guide_segments(
        &self,
        top_vis: usize,
        off: f32,
        lh: f32,
        sf: f32,
        harea_w: f32,
        view_h: f32,
    ) -> Vec<(f32, f32, f32)> {
        self.indentation_guide_segments_flagged(top_vis, off, lh, sf, harea_w, view_h, None, &[])
            .into_iter()
            .map(|(x, y, h, _)| (x, y, h))
            .collect()
    }

    /// active 视觉「**块体模型**」终稿（2026-11-14 用户 3 夹具实测钉死，唯一真值；历史
    /// 规范见 docs/bracket-guide-redesign.md §8.3，替代旧纯缩进组 getActiveIndentGuide
    /// 缩进级游走）：返回 `(列, 体首可视, 体末可视+1)`，仅供
    /// 缩进 active 高亮（整块体点亮）。
    /// - **关联括对** P = [`Self::active_pair`]（块体模型 1/2/3：开行 / 严格含 / 闭行，各
    ///   取 depth 最大）；None → 无高亮。
    /// - **体区间** = `[visual(open)+1, visual(close))`（半开，**不含括号两行**，折叠感知
    ///   经 `folds.visual_row`）；单行配对 / 折叠压缩（体空 `first >= vc`）→ None。
    /// - **列** = [`Self::guide_pair_column_lite`]（`guide_pair_column` 同式，render 竖线 /
    ///   钩线同源，K153 封口）；**色** = `palette[P.depth % 3]`（见 `active_guide_highlight_color`）。
    /// 由 `selection.head()` 所在逻辑行派生 → 方向键/点击/横滚/折叠均跟随。
    pub(super) fn active_guide_group(&self) -> Option<(usize, usize, usize)> {
        let p = self.active_pair()?;
        let vo = self.folds.visual_row(p.open_line);
        let vc = self.folds.visual_row(p.close_line);
        let first = vo.saturating_add(1);
        if first >= vc {
            return None; // 体空（单行配对 / 折叠压缩）→ 无高亮。
        }
        Some((self.guide_pair_column_lite(&p), first, vc))
    }

    /// （测试）块体模型 active 组：`(active_col, 体首可视行, 体末可视行 + 1)`（半开，不含
    /// 括号两行；`None` = 无关联块或体空）。
    #[cfg(test)]
    pub fn test_active_guide_group(&self) -> Option<(usize, usize, usize)> {
        self.active_guide_group()
    }

    /// （测试）缩进引导线两色 = `(guide_color, active_guide_color)`（VSCode 默认灰，
    /// 走 ui::theme `guide_inactive` / `guide_active`；测试锚 `indent_guides_colors_match_vscode_default`）。
    #[cfg(test)]
    pub fn test_guide_colors(&self) -> (gpui::Hsla, gpui::Hsla) {
        (self.guide_color, self.active_guide_color)
    }

    /// （测试）bracket 竖线按 `depth % 3` 循环池取色（VSCode bracket guides）：active
    /// 段 alpha 1.0、inactive 段 alpha 0.3；色 = `theme colors.bracket_guide_palette`。
    #[cfg(test)]
    pub fn test_bracket_guide_color(&self, depth: usize, active: bool) -> gpui::Hsla {
        bracket_guide_color(&self.bracket_guide_palette, depth, active)
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
        self.caret.kick();
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
        // H2 修复（真机字形）：直接插 `\t` 在 Windows 生产 DirectWrite 整形下零宽
        // （见 base::tab_diag 根因审计）→ 改插 `tab_size` 个空格（可见缩进；与
        // `outdent` 吃掉 ≤tab_size 前导空格对称）。
        let indent = " ".repeat(self.tab_size);
        if self.selection.is_collapsed() {
            // E1：无选区 → 在光标处插入一小段可见缩进（空格）。
            base::tab_diag::diag("editor-insert", || {
                format!("off={}", self.selection.head())
            });
            self.insert_edit(self.selection.range(), &indent, EditIntent::Atomic, cx);
            return;
        }
        // E2：有选区 → 只缩进「选区覆盖的源行」（每行行首各加一小段空格缩进），
        // **绝不替换/删除选中行**——旧实现 `insert_edit(selection.range(), "\t")`
        // 会把整段选区替换成单字符，单选一整行即「行消失」的数据破坏。
        base::tab_diag::diag("editor-indent", || "lines".into());
        self.indent_selected_lines(&indent, cx);
    }

    /// E2：把选区覆盖的每一源行行首插入 `indent`（默认 `tab_size` 个空格缩进），选中
    /// 文字原样保留。整段作为**一笔原子替换**（单 undo 事务、单次缓冲编辑、单 dirty
    /// 段），而非逐行多次 push_edit——`UndoManager::push_edit` 对不邻接的 Atomic 编辑
    /// 会各自成事务，逐行插入会把一次 Tab 拆成多笔撤销（见 undo.rs 的 commit-on-push
    /// 语义）。选区两端按「其前已插缩进数」平移（每行一次缩进 = `indent.len()` 字节），
    /// 仍覆盖原几行。
    fn indent_selected_lines(&mut self, indent: &str, cx: &mut Context<Self>) {
        let indent_len = indent.len().max(1);
        let range = self.selection.range();
        let line_a = self.buffer.line_of_offset(range.start);
        let line_b = self
            .buffer
            .line_of_offset(range.end.saturating_sub(1).max(range.start));
        // 被替换段 = 首选中行的行首 → 选区终点（末行含未选中前缀也会被行首缩进，
        // 这是「选中即缩进该行」的标准语义；替换仅添加缩进，不删任何原字符）。
        let span_start = self.buffer.line_start(line_a);
        let span_end = range.end.max(span_start);
        if span_end <= span_start {
            return;
        }
        let old_span = self.buffer.text_in_range(span_start..span_end);
        // 行首逐个前缀（尾换行后的幻影空行不插）。
        let mut new_span = String::with_capacity(old_span.len() + indent_len * (line_b - line_a + 1));
        let mut at_start = true;
        for ch in old_span.chars() {
            if at_start {
                new_span.push_str(indent);
                at_start = false;
            }
            if ch == '\n' {
                at_start = true;
            }
            new_span.push(ch);
        }
        // 选区两端平移：`[span_start, e]` 内每处行首前缀了一次缩进（含 e 恰为行首）。
        let shift_endpoint = |e: usize| {
            e + indent_len * (line_a..=line_b).filter(|&r| self.buffer.line_start(r) <= e).count()
        };
        let new_anchor = shift_endpoint(self.selection.anchor);
        let new_head = shift_endpoint(self.selection.head);
        // 一笔原子替换（内部完成撤销事务/高亮增量/折叠同步/预览 dirty/通知）。
        self.insert_edit(span_start..span_end, &new_span, EditIntent::Atomic, cx);
        // 还原选区覆盖范围（insert_edit 会把光标收敛到插入终点）。
        self.selection.anchor = new_anchor.min(self.buffer.len());
        self.selection.head = new_head.min(self.buffer.len());
        self.selection.preferred_column = None;
        self.caret.kick();
        cx.notify();
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
        self.caret.kick();
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
        self.caret.kick();
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
        self.caret.kick();
        self.reveal_caret();
        // 重做 = 文本变更：查找匹配表失效重算。
        self.find_edit_invalidate(cx);
        cx.emit(EditorEvent::Edited);
        cx.notify();
    }
}
