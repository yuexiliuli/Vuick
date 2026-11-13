//! 编辑区内查找（Ctrl+F / F3）与跳转行（Ctrl+G）。find/goto 卡片的输入框接线、
//! 防抖、匹配/导航、按钮悬停提示状态（`FindBtn` / `FindTipState`）与测试探针。
//! 状态机本体在 `crate::editor::find` / `crate::editor::goto`，本层只做接线。

use std::{
    ops::Range,
    time::{Duration},
};

use gpui::{
    AppContext,
    Context,
    HighlightStyle,
    Window,
};

use super::{
    CodeEditor,
};

// -------------------------------------------------------------------------
// 编辑区内查找（Ctrl+F / F3 / Shift+F3）——接线层：状态机在 `crate::editor::find`，
// 本层只做输入框/焦点/防抖/滚动/绘制接线，不堆积匹配逻辑。
// -------------------------------------------------------------------------

/// 输入防抖时长（ms）：输入停止后这么多毫秒才重算匹配（打字不逐键全量扫）。
const FIND_DEBOUNCE_MS: u64 = 200;

/// 跳转行输入防抖时长（ms）：每次输入变化防抖这么多毫秒后解析 + clamp + 跳转
/// （实时逐键跳转但不做无谓重活；Enter 立即 flush 取消防抖）。
const GOTO_DEBOUNCE_MS: u64 = 300;

/// find 卡片图标按钮（上一项 / 下一项 / 关闭）：渲染层入口 + 悬停提示标识。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) enum FindBtn {
    #[default]
    Prev,
    Next,
    Close,
}

impl FindBtn {
    /// 按钮内嵌字形（基础 unicode，非 emoji/COLR；主题色下可辨）。
    pub(crate) fn glyph(&self) -> &'static str {
        match self {
            FindBtn::Prev => "↑",
            FindBtn::Next => "↓",
            FindBtn::Close => "×",
        }
    }

    /// 元素 id（既有 find-* 命名稳定性约束内新增）。
    pub(crate) fn id(&self) -> &'static str {
        match self {
            FindBtn::Prev => "find-prev-btn",
            FindBtn::Next => "find-next-btn",
            FindBtn::Close => "find-close-btn",
        }
    }

    /// 悬停提示文案（中文小浮层）。
    pub(crate) fn tip_label(&self) -> &'static str {
        match self {
            FindBtn::Prev => "上一项",
            FindBtn::Next => "下一项",
            FindBtn::Close => "关闭",
        }
    }
}

/// find 卡片按钮悬停提示状态（本地小浮层，无全局浮层机制）：当前悬停按钮 +
/// 是否已过延迟显示期。延迟排单走 `cx.spawn` + 后台 timer（到点仍悬停该钮才 shown），
/// 无需跨帧持久计时器（离开即清、关闭即复位）。
#[derive(Debug, Default)]
pub(crate) struct FindTipState {
    /// 当前悬停的按钮（None = 未悬停）。
    pub(crate) hover: Option<FindBtn>,
    /// 是否已显示提示（延迟 [`FIND_TIP_DELAY_MS`] 到期且仍悬停该钮）。
    pub(crate) shown: bool,
}

impl FindTipState {
    /// 当前应显示的提示按钮（hover 且已过延迟）；无则 `None`。
    pub(crate) fn shown_btn(&self) -> Option<FindBtn> {
        if self.shown {
            self.hover
        } else {
            None
        }
    }
}

/// find 卡片按钮悬停提示的浮现延迟（ms）：扫过按钮时不闪现，停留该时长仍悬停才显示。
const FIND_TIP_DELAY_MS: u64 = 400;

impl CodeEditor {
    /// 打开 find 条（Ctrl+F）：输入框自动聚焦 + 自动带上最近一次查询（若有）。
    pub fn open_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // 首次打开懒创建输入框实体 + 订阅「文本变化 → 置脏 + 防抖；Enter/Esc（组件
        // 统一上抛 Submit/Cancel）→ 导航 / 关闭」。
        if self.find_input.is_none() {
            let input = cx
                .new(|cx| ui::input::InputState::new(window, cx).placeholder("查找…"));
            let input_entity = input.clone();
            let weak_ed = cx.weak_entity();
            self._find_sub = Some(window.subscribe(&input, cx, move |_, ev, window, cx| {
                let Some(ed) = weak_ed.upgrade() else {
                    return;
                };
                let _ = ed.update(cx, |ed, cx| match ev {
                    ui::input::InputEvent::Change => {
                        let q = input_entity.read(cx).value().to_string();
                        ed.find_query_changed(q, cx);
                    }
                    ui::input::InputEvent::Submit { shift } => ed.find_step(!shift, cx),
                    ui::input::InputEvent::Cancel => ed.close_find(window, cx),
                });
            }));
            self.find_input = Some(input);
        }
        let was_open = self.find.open;
        self.find.open_bar();
        self.find_tip = FindTipState::default();
        // 首次打开（或上次 Esc 关闭后再次打开）：把最近一次查询带进输入框回显。
        if !was_open
            && let Some(input) = self.find_input.clone()
        {
            let q = self.find.query.clone();
            input.update(cx, |s, scx| s.replace(q, window, scx));
        }
        // 输入框自动聚焦（已打开时重复 Ctrl+F 也聚焦回输入框）。
        if let Some(input) = self.find_input.clone() {
            let fh = input.read(cx).focus_handle(cx);
            window.focus(&fh, cx);
        }
        cx.notify();
    }

    /// 关闭 find 条（Esc）：移除全部高亮、保留 last query 供 F3 复用、焦点归还编辑器。
    pub fn close_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.find.close_bar();
        self.find_tip = FindTipState::default();
        self.focus_handle.focus(window, cx);
        cx.notify();
    }

    /// F3：复用该 tab 上次查询 → 下一个；无上次查询则不动作。
    pub fn find_next(&mut self, cx: &mut Context<Self>) {
        self.find_step(true, cx);
    }

    /// Shift+F3：复用该 tab 上次查询 → 上一个；无上次查询则不动作。
    pub fn find_prev(&mut self, cx: &mut Context<Self>) {
        self.find_step(false, cx);
    }

    /// 输入框文本变化（订阅回调）：更新查询词并走防抖重算。
    fn find_query_changed(&mut self, query: String, cx: &mut Context<Self>) {
        self.find.query = query;
        self.find.invalidate();
        self.schedule_find_debounce(cx);
        cx.notify();
    }

    /// 文本编辑失效（insert/undo/redo 编辑主链调用）：匹配表按版本重算（防抖同一条路径）。
    pub(super) fn find_edit_invalidate(&mut self, cx: &mut Context<Self>) {
        if !self.find.open && !self.find.visible {
            return; // 未在查找（也未经 F3 拉高亮）：编辑无需重算。
        }
        self.find.invalidate();
        self.schedule_find_debounce(cx);
    }

    /// 防抖调度：每变化一次起一个 200ms 任务；到期用「最新查询词」重扫，仅最新代号落地。
    fn schedule_find_debounce(&mut self, cx: &mut Context<Self>) {
        let epoch = self.find.scan_epoch;
        cx.spawn(async move |this, cx| {
            let _ = cx
                .background_executor()
                .timer(Duration::from_millis(FIND_DEBOUNCE_MS))
                .await;
            let _ = this.update(cx, |ed, cx| {
                if epoch == ed.find.scan_epoch {
                    ed.find_scan_land(cx);
                }
            });
        })
        .detach();
    }

    /// 用**当前查询词**重扫并落地（同步；防抖到期 / Enter / F3 导航共用）。
    fn find_scan_land(&mut self, cx: &mut Context<Self>) {
        let query = self.find.query.clone();
        let ver = self.buffer.version();
        let (matches, capped) = crate::editor::find::compute_matches(
            &self.buffer,
            &query,
            crate::editor::find::FIND_MATCH_CAP,
        );
        self.find.dirty = false; // 本次落地即消费脏标记（否则每次导航都误判「待重算」重定位回首个）
        self.find.land(ver, query, matches, capped, true);
        self.reveal_current_find();
        cx.notify();
    }

    /// 导航前保证匹配表新鲜：编辑/换词/关条后按 `(版本, 词)` 失效重扫。
    fn ensure_find_fresh(&mut self, cx: &mut Context<Self>) {
        let ver = self.buffer.version();
        let q = self.find.query.clone();
        if !self.find.fresh_for(ver, &q) || !self.find.visible {
            self.find.scan_epoch = self.find.scan_epoch.wrapping_add(1);
            self.find_scan_land(cx);
        }
    }

    /// F3 / Shift+F3 / Enter（find 条内）统一导航：当前命中 → 下一个/上一个（回绕），
    /// 滚动使当前命中可见；无匹配不动作、不回绕。
    pub(super) fn find_step(&mut self, next: bool, cx: &mut Context<Self>) {
        // 无查询词（从未查找过）→ 不动作。
        if self.find.query.is_empty() {
            return;
        }
        self.ensure_find_fresh(cx);
        if let Some((idx, off)) = self.find.step(next) {
            self.find.current = Some(idx);
            self.set_caret_to(off);
            self.reveal_current_find();
        }
        cx.notify();
    }

    /// find 卡片按钮是否可用：上一项/下一项需「有查询词 + 有匹配」；关闭恒可用。
    pub(super) fn find_btn_enabled(&self, btn: FindBtn) -> bool {
        match btn {
            FindBtn::Prev | FindBtn::Next => {
                !self.find.query.is_empty() && !self.find.matches.is_empty()
            }
            FindBtn::Close => true,
        }
    }

    /// find 卡片按钮点击：上一项/下一项/关闭即 Shift+Enter/Enter/Esc 的鼠标等价通道。
    /// 禁态（0 匹配 / 无查询）不动作不崩；导航后回焦输入框（连续查找不丢输入），
    /// 关闭则由 `close_find` 把焦点还编辑器。
    pub(super) fn find_btn_clicked(&mut self, window: &mut Window, cx: &mut Context<Self>, btn: FindBtn) {
        match btn {
            FindBtn::Prev => {
                if self.find_btn_enabled(FindBtn::Prev) {
                    self.find_step(false, cx);
                }
            }
            FindBtn::Next => {
                if self.find_btn_enabled(FindBtn::Next) {
                    self.find_step(true, cx);
                }
            }
            FindBtn::Close => {
                self.close_find(window, cx);
                return;
            }
        }
        if self.find.open
            && let Some(input) = self.find_input.clone()
        {
            let fh = input.read(cx).focus_handle(cx);
            window.focus(&fh, cx);
        }
    }

    /// find 卡片按钮悬停：进入登记悬停钮 + 排一单「延迟显示提示」（到点仍悬停该钮
    /// 才 `shown`；换钮/移出后旧单到点自检丢弃）；移出立即清 hover + 隐藏提示。
    pub(super) fn find_btn_hover(&mut self, cx: &mut Context<Self>, btn: FindBtn, hovered: bool) {
        if hovered {
            if self.find_tip.hover != Some(btn) {
                self.find_tip.hover = Some(btn);
                self.find_tip.shown = false;
                cx.spawn(async move |this, cx| {
                    cx.background_executor()
                        .timer(Duration::from_millis(FIND_TIP_DELAY_MS))
                        .await;
                    let _ = this.update(cx, |ed, cx| {
                        if ed.find_tip.hover == Some(btn) {
                            ed.find_tip.shown = true;
                            cx.notify();
                        }
                    });
                })
                .detach();
            }
        } else if self.find_tip.hover == Some(btn) {
            self.find_tip.hover = None;
            self.find_tip.shown = false;
        }
        cx.notify();
    }

    /// 当前命中所处行滚进可视区（纵向；当前命中必在当前环境可见）。
    fn reveal_current_find(&mut self) {
        if let Some(r) = self.find.current.and_then(|i| self.find.matches.get(i)) {
            let row = self.buffer.line_of_offset(r.start);
            self.reveal_row(row);
        }
    }

    /// 某逻辑行的查找高亮样式段（行本地字节区间 + 底色：当前命中用强调色、其余普通色）。
    pub(super) fn find_row_overlays(&self, logical: usize) -> Vec<(Range<usize>, HighlightStyle)> {
        let mut out = Vec::new();
        for (r, cur) in self.find.row_highlights(&self.buffer, logical) {
            out.push((
                r,
                HighlightStyle {
                    background_color: Some(if cur {
                        self.find_current_bg
                    } else {
                        self.find_match_bg
                    }),
                    ..Default::default()
                },
            ));
        }
        out
    }

    // —— 测试探针 ——
    // cfg(test) 探针面：拆 editor crate 后被 shell 测试跨 crate 消费，转无条件 pub。

    /// 查找状态快照 `(打开, 可见, 匹配数, 当前下标, 查询词)`（测试探针）。
    pub fn test_find_state(&self) -> (bool, bool, usize, Option<usize>, String) {
        (
            self.find.open,
            self.find.visible,
            self.find.matches.len(),
            self.find.current,
            self.find.query.clone(),
        )
    }

    /// 计数文案（测试探针：行为表断言「i/N / 无匹配 / i/N+」）。
    pub fn test_find_status(&self) -> String {
        self.find.status()
    }

    /// 匹配表（全文字节区间；测试探针：「高亮全部匹配」断言用）。
    pub fn test_find_matches(&self) -> Vec<std::ops::Range<usize>> {
        self.find.matches.clone()
    }

    /// 当前命中的字节区间（`None` = 无匹配/未定位；测试探针）。
    pub fn test_find_current_range(&self) -> Option<std::ops::Range<usize>> {
        self.find.current.and_then(|i| self.find.matches.get(i)).cloned()
    }

    /// find 输入框实体（测试探针：驱动输入/断言焦点）。
    pub fn test_find_input(&self) -> Option<&gpui::Entity<ui::input::InputState>> {
        self.find_input.as_ref()
    }

    /// find 卡片按钮悬停提示快照 `(悬停按钮, 是否已显示)`（测试探针：hover 显隐断言）。
    pub(crate) fn test_find_tip_state(&self) -> Option<(FindBtn, bool)> {
        self.find_tip.hover.map(|b| (b, self.find_tip.shown))
    }

    /// 同步走一次「防抖到期重扫」落地（测试探针：fake 执行器不轮询定时 spawn，
    /// 测试需确定性驱动重算路径；与 `test_scan_sync_land` 同姿势）。
    pub fn test_find_scan_now(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let _ = window;
        self.find_scan_land(cx);
    }

    // -----------------------------------------------------------------
    // 编辑区内跳转行（Ctrl+G）
    // -----------------------------------------------------------------

    /// 打开跳转行卡片（Ctrl+G 分发到活动文本 tab）：输入框自动聚焦；再次打开回显
    /// 最近一次输入并防抖重跳（recall，同 find Esc 后 F3 复用语义）。
    pub fn open_goto(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // 首次打开懒创建输入框实体 + 订阅「文本变化 → 置脏 + 防抖；Enter/Esc（组件
        // 统一上抛 Submit/Cancel）→ 立即 flush（取消防抖）/ 关闭」。
        if self.goto_input.is_none() {
            let input = cx
                .new(|cx| ui::input::InputState::new(window, cx).placeholder("行号（可 行:列）"));
            let input_entity = input.clone();
            let weak_ed = cx.weak_entity();
            self._goto_sub = Some(window.subscribe(&input, cx, move |_, ev, window, cx| {
                let Some(ed) = weak_ed.upgrade() else {
                    return;
                };
                let _ = ed.update(cx, |ed, cx| match ev {
                    ui::input::InputEvent::Change => {
                        let q = input_entity.read(cx).value().to_string();
                        ed.goto_input_changed(q, cx);
                    }
                    // Enter（Submit）：取消防抖、当场 flush（与旧 goto_card_key_down 同
                    // 语义 —— 新代号作废旧任务后立即就地 flush，卡片不关、输入框持焦）。
                    ui::input::InputEvent::Submit { .. } => {
                        ed.goto.invalidate();
                        ed.goto_flush(cx);
                    }
                    ui::input::InputEvent::Cancel => ed.close_goto(window, cx),
                });
            }));
            self.goto_input = Some(input);
        }
        let was_open = self.goto.open;
        self.goto.open_bar();
        // 再次打开（上次 Esc/点外关闭）：把最近一次输入带进输入框回显（发 Change →
        // 订阅自动置脏 + 排防抖重跳）。
        if !was_open
            && let Some(input) = self.goto_input.clone()
        {
            let q = self.goto.cached.clone();
            input.update(cx, |s, scx| s.replace(q, window, scx));
        }
        // 输入框自动聚焦（已打开时重复 Ctrl+G 也聚焦回输入框）。
        if let Some(input) = self.goto_input.clone() {
            let fh = input.read(cx).focus_handle(cx);
            window.focus(&fh, cx);
        }
        cx.notify();
    }

    /// 关闭跳转行卡片（Esc / 点击卡片外）：保留输入缓存、焦点归还编辑器。
    pub fn close_goto(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.goto.close_bar();
        self.focus_handle.focus(window, cx);
        cx.notify();
    }

    /// 输入框文本变化（订阅回调）：更新输入缓存并立即刷新内联提示（非法/目标预览
    /// 即时反馈），随后排一次防抖跳转（实际落点等防抖）。
    fn goto_input_changed(&mut self, text: String, cx: &mut Context<Self>) {
        self.goto.cached = text;
        self.goto.invalidate();
        self.refresh_goto_hint();
        self.schedule_goto_debounce(cx);
        cx.notify();
    }

    /// 防抖调度：每变化一次起一个 [`GOTO_DEBOUNCE_MS`] 任务；到期用最新输入 flush，
    /// 仅最新代号落地（陈旧任务被作废）。
    fn schedule_goto_debounce(&mut self, cx: &mut Context<Self>) {
        let epoch = self.goto.flush_epoch;
        cx.spawn(async move |this, cx| {
            let _ = cx
                .background_executor()
                .timer(Duration::from_millis(GOTO_DEBOUNCE_MS))
                .await;
            let _ = this.update(cx, |ed, cx| {
                if epoch == ed.goto.flush_epoch {
                    ed.goto_flush(cx);
                }
            });
        })
        .detach();
    }

    /// flush（防抖到期 / Enter 共用）：用最新输入解析 + clamp + 跳转（滚动可见 +
    /// 光标落位）+ 内联提示。空输入 / 非法 / 空文档 → 不跳（提示区呈现、不崩）。
    /// 卡片不关、焦点不移（Enter 后输入框保持焦点）。
    pub(super) fn goto_flush(&mut self, cx: &mut Context<Self>) {
        // 卡片已关闭（Esc/点外）→ 丢弃残留防抖：Esc 或点外关闭后老 Task 到期不得再
        // 落地「幽灵跳」（fake 执行器测不到此竞态，真实定时器会触发；K26 同类）。
        if !self.goto.open {
            return;
        }
        self.goto.dirty = false;
        // 空文档 / 大文档后台打开尚未挂载 → 跳过定位（不崩；不发误导性目标文案，
        // 跳转实际不会发生，沿旧 shell 空文档防护）。
        if self.buffer.char_count() > 0 {
            let line_count = self.buffer.line_count();
            let parsed = crate::editor::goto::parse_and_clamp(&self.goto.cached, line_count);
            if let Some(t) = parsed {
                let row0 = t.row1 - 1;
                let col0 = t.col1 - 1;
                let offset = self.offset_for_line_col(row0, col0);
                self.scroll_to_line(row0);
                self.set_selection_range(offset..offset, cx);
                // 用**真实落点**（含「列超行宽」被 offset_for_line_col 静默钳制的最终列）
                // 刷新提示；「已钳制」指示沿用解析结论（行/列 < 1 / 超行数）。
                let (r, c) = self.cursor_position();
                self.goto.last = Some((r + 1, c + 1));
                let landed = crate::editor::goto::GotoTarget {
                    row1: r + 1,
                    col1: c + 1,
                    clamped: t.clamped,
                };
                let (h, e) = crate::editor::goto::hint_for(&self.goto.cached, Some(&landed));
                self.goto.hint = h;
                self.goto.hint_error = e;
                cx.notify();
                return;
            }
        }
        // 非法 / 空 / 空文档：不跳（提示区呈现，实时反馈文案为准）。
        self.refresh_goto_hint();
        cx.notify();
    }

    /// 按当前输入刷新内联提示（解析 + clamp + 文案；不跳转）。`goto_input_changed`
    /// 与 `goto_flush` 共用，保证「即时反馈」与「落点确认」同一条文案口径。
    fn refresh_goto_hint(&mut self) {
        // 空文档：任何输入都不跳 → 提示区留空（不发误导性目标/已钳制文案）。
        if self.buffer.char_count() == 0 {
            self.goto.hint.clear();
            self.goto.hint_error = false;
            return;
        }
        let line_count = self.buffer.line_count();
        let target = crate::editor::goto::parse_and_clamp(&self.goto.cached, line_count);
        let (h, e) = crate::editor::goto::hint_for(&self.goto.cached, target.as_ref());
        self.goto.hint = h;
        self.goto.hint_error = e;
    }

    // —— 测试探针 ——

    /// 跳转行状态快照 `(打开, 输入缓存, 最近目标, 内联提示文案, 是否错误)`（测试探针）。
    pub fn test_goto_state(&self) -> (bool, String, Option<(usize, usize)>, String, bool) {
        (
            self.goto.open,
            self.goto.cached.clone(),
            self.goto.last,
            self.goto.hint.clone(),
            self.goto.hint_error,
        )
    }

    /// 跳转行输入框实体（测试探针：驱动输入/断言焦点）。
    pub fn test_goto_input(&self) -> Option<&gpui::Entity<ui::input::InputState>> {
        self.goto_input.as_ref()
    }

    /// 同步走一次「防抖到期 flush」落地（测试探针：fake 执行器不轮询定时 spawn，
    /// 测试需确定性驱动跳转路径；与 `test_find_scan_now` 同姿势）。
    pub fn test_goto_flush_now(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let _ = window;
        self.goto_flush(cx);
    }
}
