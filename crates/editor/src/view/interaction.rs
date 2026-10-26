//! 焦点 / 闪烁 / 鼠标事件。`CodeEditor` 的 focus/blur 钩子、光标闪烁
//! （`start_blink` / `kick_caret`）与鼠标按下/移动/抬起/松开离场的事件分发
//! （含拖选态生命周期 `cancel_mouse_drag`）。

use std::{
    time::{Duration, Instant},
};

use gpui::{
    px,
    Context,
    MouseButton,
    MouseDownEvent,
    MouseMoveEvent,
    MouseUpEvent,
    Window,
};

use super::{
    CodeEditor,
    EditorEvent,
    BLINK_MS,
    BLINK_PAUSE_MS,
};

impl CodeEditor {
    pub(super) fn on_focus(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.focused = true;
        self.caret_visible = true;
        self.caret_pause_until = Some(Instant::now());
        cx.emit(EditorEvent::Focus);
        if !self.blink_running {
            self.start_blink(cx);
        }
    }

    pub(super) fn on_blur(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.focused = false;
        // 失焦 = 编辑器离开活动态：进行中的鼠标拖选失去意义，随生命周期一并复位。
        // 拖选只依赖「按下 → 移动」；松开/失焦/切走任一中断拖拽闭环都不能留
        // `dragging` 残留（收敛到统一生命周期钩子，见 [`Self::cancel_mouse_drag`]）。
        self.cancel_mouse_drag();
        self.undo.set_cooperative(false);
        self.marked_range = None;
        cx.emit(EditorEvent::Blur);
    }

    pub(super) fn start_blink(&mut self, cx: &mut Context<Self>) {
        if self.blink_running {
            return;
        }
        self.blink_running = true;
        self.blink_epoch = self.blink_epoch.wrapping_add(1);
        let epoch = self.blink_epoch;
        cx.spawn(async move |this, cx| loop {
            let elapsed = this
                .update(cx, |this, _| {
                    this.caret_pause_until
                        .map_or(u128::MAX, |p| p.elapsed().as_millis())
                })
                .unwrap_or(u128::MAX);
            if elapsed >= BLINK_PAUSE_MS as u128 {
                let _ = this.update(cx, |this, cx| {
                    this.caret_visible = !this.caret_visible;
                    cx.notify();
                });
            }
            cx.background_executor()
                .timer(Duration::from_millis(BLINK_MS))
                .await;
            let keep = this
                .update(cx, |this, _| this.focused && epoch == this.blink_epoch)
                .unwrap_or(false);
            if !keep {
                this.update(cx, |this, _| this.blink_running = false).ok();
                break;
            }
        })
        .detach();
    }

    pub(super) fn kick_caret(&mut self) {
        self.caret_visible = true;
        self.caret_pause_until = Some(Instant::now());
    }

    pub(super) fn on_mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if event.button != MouseButton::Left {
            return;
        }
        self.focus_handle.focus(window, cx);
        self.undo.set_cooperative(false);
        let local = event.position - self.element_bounds.origin;
        // gutter 内点击：命中**折叠首行或可折叠候选** → 切换折叠（不移动光标）；
        // 否则回落普通点击（x 在行号列 → 列 0，与既有行为一致）。
        if local.x >= px(0.) && local.x < self.gutter_width_px {
            let lh = self.line_height.as_f32().max(1.);
            let content_y = self.scroll_top_px() + local.y.as_f32();
            let vis = (content_y / lh).floor().max(0.) as usize;
            let vis = vis.min(self.folds.visual_line_count().saturating_sub(1));
            let logical = self.folds.logical_row(vis);
            if self.has_fold_or_candidate(logical) {
                self.toggle_fold_at(logical);
                self.cancel_mouse_drag();
                return;
            }
        }
        let offset = self.offset_at_point(local, window);
        // 标尺点亮：代码区点击 → 该行所属最内层括号块的标尺竖线点亮；非块内行 → 清除。
        // （此处已越过 gutter 分支，标尺本身不可点选，只响应代码区。）
        let caret_row = self.buffer.line_of_offset(offset);
        self.active_guide_open = self.guide_pair_for_logical_row(caret_row);
        if event.modifiers.shift {
            self.selection.set_head(offset);
            self.selection.remember_column(self.buffer.char_column(offset));
        } else {
            self.selection.place(offset);
            self.selection.remember_column(self.buffer.char_column(offset));
        }
        self.dragging = true;
        self.kick_caret();
        self.reveal_caret_vertical();
        // 鼠标点击移动光标（宿主刷新状态栏光标行列等派生数据）。
        cx.emit(EditorEvent::SelectionChanged);
        cx.notify();
    }

    pub(super) fn on_mouse_move(&mut self, event: &MouseMoveEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !self.dragging {
            return;
        }
        let local = event.position - self.element_bounds.origin;
        let offset = self.offset_at_point(local, window);
        self.selection.set_head(offset);
        self.selection.remember_column(self.buffer.char_column(offset));
        self.kick_caret();
        self.reveal_caret_vertical();
        // 拖拽扩选过程也实时刷新（状态栏选区头跟随鼠标）。
        cx.emit(EditorEvent::SelectionChanged);
        cx.notify();
    }

    pub(super) fn on_mouse_up(&mut self, _event: &MouseUpEvent, _window: &mut Window, _cx: &mut Context<Self>) {
        self.cancel_mouse_drag();
    }

    /// 松开点落在元素 hitbox 外 / 顶层叠层 / 窗口外：元素级 `on_mouse_up`（受
    /// `hitbox.is_hovered` 门控）不会派发，本钩子在 Capture 阶段无条件收口拖选态
    /// （对齐本仓 scrollbar / video / image / wysiwyg 的 `on_mouse_up_out` 拖拽清态惯例）。
    pub(super) fn on_mouse_up_out(&mut self, _event: &MouseUpEvent, _window: &mut Window, _cx: &mut Context<Self>) {
        self.cancel_mouse_drag();
    }

    /// 生命周期钩子：编辑器即将「失焦 / 隐藏 / 宿主切换」时统一复位鼠标拖选态
    /// （仅清 `dragging`，不动光标 / 选区，不改变交互模型、不改键位）。
    ///
    /// 拖选态生命周期：`on_mouse_down` 置位 → `on_mouse_up` / `on_mouse_up_out` /
    /// `on_blur` / 整篇写回（`reset_document`）/ 宿主切离 任一终点复位，散落清位全部
    /// 收敛到本钩子（宿主在编辑区被切离 / 隐藏的正确时机触发），取代历史 P4 的
    /// 「调用点散落清位」补丁。
    ///
    /// 背景（P4）：点菜单项时 GPUI 把鼠标按下按 `hitbox.is_hovered` 漏进编辑区（不按
    /// z-order 裁掉被菜单遮挡的区间）置 `dragging=true`；对应的 MouseUp 又被菜单项
    /// `activate` 的 `stop_propagation` 截断派发、到不了 `on_mouse_up` → `dragging`
    /// 残留，切回后仅移动鼠标就从 anchor 自动扩选。
    pub fn cancel_mouse_drag(&mut self) {
        self.dragging = false;
    }
}
