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

    pub(super) fn on_blur(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // 失焦前把最后未落地的拖点先落地（等价旧「逐事件同步更新」的选区结果），
        // 再走统一拖选复位（见 [`Self::cancel_mouse_drag`]）。
        self.flush_pending_drag(window, cx);
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
                    // 失焦时光标本就不渲染（`caret_shown_now` 恒 false）→ 翻转不可见，
                    // 跳过翻转与 notify（否则 500ms 一帧整树重建白耗，且拖选/悬停也不
                    // 受影响）。重获焦点时 `on_focus` 已重置 `caret_visible=true`。
                    if !this.focused {
                        return;
                    }
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
        // WY_TRACE 操作级计时（KILO_PROF::op_start，未开 WY_TRACE 时一次借锁检查即返回）：
        // 单击 → 本帧编辑器 paint 末尾结算（render.rs 的 op_finish_after_paint），产出
        // `OP kind=ed_click total=墙钟`，量「点击 → 落帧」感知延迟。
        crate::prof::op_start("ed_click");
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
        if event.modifiers.shift {
            self.selection.set_head(offset);
            self.selection.remember_column(self.buffer.char_column(offset));
        } else {
            self.selection.place(offset);
            self.selection.remember_column(self.buffer.char_column(offset));
        }
        self.dragging = true;
        self.pending_drag = None;
        self.kick_caret();
        self.reveal_caret_vertical();
        // 鼠标点击移动光标（宿主刷新状态栏光标行列等派生数据）。
        cx.emit(EditorEvent::SelectionChanged);
        cx.notify();
    }

    pub(super) fn on_mouse_move(&mut self, event: &MouseMoveEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if !self.dragging {
            return;
        }
        // WY_TRACE 操作级计时：拖选每步「移动 → 本帧 paint 结束」一次（与单击同机制，
        // 量「拖选 → 落帧」逐帧感知延迟）。
        crate::prof::op_start("ed_drag");
        let local = event.position - self.element_bounds.origin;
        // 帧级合并（B2）：只记**最新拖点**，本帧 `render` 开头 `flush_pending_drag`
        // 统一换算/落地——鼠标事件可比帧快得多（高分屏/高速鼠标），不再逐事件重复
        // offset 换算 + set_head + emit + notify。语义不变：拖选仍逐帧实时推进，且
        // `on_mouse_up/up_out/blur` 会先把最后未落地的拖点落地再收尾（快速
        // 「按下-移动-抬起」跨帧时也不会丢最后位置）。对齐 scrollbar
        // `pending_target`→prepaint 落地先例。
        self.pending_drag = Some(local);
        cx.notify();
    }

    /// 拖选帧级合并落地（B2，每帧至多一次）：把 `on_mouse_move` 记下的最新拖点换算
    /// 成选区头、刷新光标暂停/纵向滚动并广播选区变化（宿主状态栏 Ln/Col 逐帧跟随）。
    /// 本帧 `render` 开头调用——落地发生在行构建**之前**，当帧即显示新选区。
    pub(super) fn flush_pending_drag(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(local) = self.pending_drag.take() else {
            return;
        };
        #[cfg(test)]
        self.drag_flush_calls.set(self.drag_flush_calls.get() + 1);
        let offset = self.offset_at_point(local, window);
        self.selection.set_head(offset);
        self.selection.remember_column(self.buffer.char_column(offset));
        self.kick_caret();
        self.reveal_caret_vertical();
        // 拖拽扩选过程实时刷新（状态栏选区头跟随鼠标）。
        cx.emit(EditorEvent::SelectionChanged);
    }

    pub(super) fn on_mouse_up(&mut self, _event: &MouseUpEvent, window: &mut Window, cx: &mut Context<Self>) {
        // 帧级合并收尾：先把最后未渲染的拖点落地再结束拖选（否则快速
        // 「按下-移动-抬起」跨帧时该次移动会被丢弃、选区停在按下点）。
        self.flush_pending_drag(window, cx);
        self.cancel_mouse_drag();
    }

    /// 松开点落在元素 hitbox 外 / 顶层叠层 / 窗口外：元素级 `on_mouse_up`（受
    /// `hitbox.is_hovered` 门控）不会派发，本钩子在 Capture 阶段无条件收口拖选态
    /// （对齐本仓 scrollbar / video / image / wysiwyg 的 `on_mouse_up_out` 拖拽清态惯例）。
    pub(super) fn on_mouse_up_out(&mut self, _event: &MouseUpEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.flush_pending_drag(window, cx);
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
        // 未落地的拖点一并丢弃：鼠标端点（up/up_out/blur）会先 `flush_pending_drag`
        // 落地再调用本钩子；其余复位路径（整篇写回/隐藏/切离）丢弃拖点 = 不把旧文档
        // 坐标的拖点应用到新内容（与旧「逐事件同步更新」的最终选区一致）。
        self.pending_drag = None;
    }
}
