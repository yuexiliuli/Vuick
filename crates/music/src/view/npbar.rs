//! 右栏「正在播放」卡**进度/时间子视图**（性能优化第四步 2B）：`NpBarView` 从父 right.rs
//! np 卡拆出——进度条 + 当前/总时长文本自带 ~250ms 心跳（仅可见且播放中自 notify），
//! 不再搭父 250ms tick 整树 notify 便车；拖拽 seek 由本视图自管（写 player + 自己 notify
//! 跟手，对齐 bottom.rs 姿势）。
//!
//! 父在 `render` 按 `right_tab`/`fs_open` 注入 `visible` 门控：离树（非歌曲 tab / fs 覆盖）
//! 实例心跳空转。np 卡上的收藏/队列/分享等按钮仍留父（`right.rs`），本视图只承载
//! 随时间变化的进度视觉。

use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, Bounds, Context, CursorStyle, DefiniteLength, Entity, InteractiveElement as _,
    IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement as _,
    Pixels, Render, Styled as _, Window, div, px,
};

use super::theme;
use super::fraction_at;
use crate::player::MusicPlayer;

/// 进度心跳周期（ms，对齐父原 tick 250ms）：可见且播放中每拍自 notify 刷新进度/时间。
const HEARTBEAT_MS: u64 = 250;

/// 进度条拖拽真实 seek 节流（ms）：对齐父 `SCRUB_RETARGET_MS`/bottom.rs（密集 move 防刷爆
/// worker/杂音）。节流窗口内只更新显示基准（`set_seek_base`），显示仍逐 move 实时跟手。
const SCRUB_RETARGET_MS: u64 = 60;

/// 右栏「正在播放」进度条 + 时间子视图。
pub(super) struct NpBarView {
    /// 播放器实体（唯一状态源；只读）。
    pub(super) player: Entity<MusicPlayer>,
    /// 本实例当前是否被父挂在树上（父 render 按 right_tab/fs_open 门控 set）。离树心跳空转。
    pub(super) visible: bool,
    /// 进度条窗口坐标盒（canvas prepaint 记录；点/拖/松手与回归断言共用）。
    pub(super) np_progress: Rc<Cell<Bounds<Pixels>>>,
    // ---- 进度条拖拽交互态（本视图自管；fs 条拖拽态仍在父——二者不同时共存于单指针） ----
    scrubbing: bool,
    scrub_target: f64,
    scrub_seeked: Option<f64>,
    last_scrub_retarget: Option<Instant>,
    /// 进度心跳循环句柄（view 销毁即取消）。
    _heartbeat: Option<gpui::Task<()>>,
}

impl NpBarView {
    pub(super) fn new(player: Entity<MusicPlayer>, cx: &mut Context<Self>) -> Self {
        // 进度心跳：仅可见且播放中每 ~250ms 自 notify（子视图脏 → 重画拉最新 position）。
        // 离树/暂停空转——不 notify 就不重绘（K12）。
        let weak = cx.weak_entity();
        let heartbeat = cx.spawn(async move |_, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(HEARTBEAT_MS))
                    .await;
                let _ = weak.update(cx, |this, cx| {
                    if this.visible && this.player.read(cx).playing() {
                        cx.notify();
                    }
                });
            }
        });
        Self {
            player,
            visible: false,
            np_progress: Rc::new(Cell::new(Bounds::default())),
            scrubbing: false,
            scrub_target: 0.0,
            scrub_seeked: None,
            last_scrub_retarget: None,
            _heartbeat: Some(heartbeat),
        }
    }
}

impl Render for NpBarView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = self.player.read(cx);
        let dur = p.duration();
        let pos = p.position();
        let frac = if dur > 0.0 {
            (pos / dur).clamp(0.0, 1.0) as f32
        } else {
            0.0
        };
        // 进度条（np_progress）——点击/拖拽 seek 状态机本视图自管。
        let progress = self.strip_element(
            "np-progress",
            self.np_progress.clone(),
            frac,
            true,
            None,
            cx,
        );
        let time_cur = crate::model::duration_str(pos);
        let time_total = crate::model::duration_str(dur);
        div().flex().flex_col()
            .child(progress)
            .child(
                div()
                    .flex()
                    .justify_between()
                    .text_size(px(11.))
                    .text_color(theme::text3())
                    .child(time_cur)
                    .child(time_total)
                    .pb(px(14.)),
            )
            .into_any_element()
    }
}

impl NpBarView {
    // ---- 可交互条（seek 进度条）：条元素 + 事件 → player（拖拽态本视图自管） ----

    /// 构建一条可交互 seek 进度条（对齐父/bottom `strip_element` 姿势）：`bounds` 由 canvas
    /// prepaint 记录窗口坐标盒；点按即进拖拽态并真实 seek、拖动节流跟手、松手按落点收尾，
    /// 全部在本视图内处理并 `cx.notify()` 自己——拖动跟手不寄生动画/tick 帧。
    fn strip_element(
        &mut self,
        id: &'static str,
        bounds: Rc<Cell<Bounds<Pixels>>>,
        fraction: f32,
        is_seek: bool,
        width: Option<f32>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let f = fraction.clamp(0.0, 1.0);
        let fill_col = theme::accent();
        let track_col = theme::bg_hover();
        let mut el = div()
            .id(id)
            .h(px(16.))
            .relative()
            .cursor(CursorStyle::PointingHand)
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .relative()
                            .h(px(4.))
                            .rounded(px(2.))
                            .bg(track_col.opacity(0.6))
                            .child(
                                div()
                                    .absolute()
                                    .left_0()
                                    .top_0()
                                    .bottom_0()
                                    .w(DefiniteLength::Fraction(f))
                                    .debug_selector(|| "np-progress-fill".into())
                                    .rounded(px(2.))
                                    .bg(fill_col),
                            ),
                    ),
            )
            .child(theme::bounds_canvas(bounds.clone()));
        if let Some(w) = width {
            el = el.w(px(w));
        } else {
            el = el.flex_1();
        }
        let b1 = bounds.clone();
        el = el.on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, ev, _w, cx| this.on_strip_down(is_seek, b1.clone(), ev, cx)),
        );
        let b2 = bounds.clone();
        el = el.on_mouse_move(
            cx.listener(move |this, ev, _w, cx| this.on_strip_drag(is_seek, b2.clone(), ev, cx)),
        );
        let b3 = bounds.clone();
        el = el.on_mouse_up(
            MouseButton::Left,
            cx.listener(move |this, ev, _w, cx| this.on_strip_up(is_seek, b3.clone(), ev, cx)),
        );
        let b4 = bounds.clone();
        el = el.on_mouse_up_out(
            MouseButton::Left,
            cx.listener(move |this, ev, _w, cx| {
                this.on_strip_up(is_seek, b4.clone(), ev, cx)
            }),
        );
        el.into_any_element()
    }

    fn on_strip_down(
        &mut self,
        is_seek: bool,
        bounds: Rc<Cell<Bounds<Pixels>>>,
        ev: &MouseDownEvent,
        cx: &mut Context<Self>,
    ) {
        if let Some(f) = fraction_at(&bounds, ev.position) {
            if is_seek {
                self.scrubbing = true;
                self.last_scrub_retarget = Some(Instant::now());
                // 拖拽中 position() 钉在 seek_base（显示跟手指、不随时钟漂移）。
                self.player.update(cx, |p, cx| p.set_scrubbing(true, cx));
            }
            self.apply_strip(is_seek, f, cx);
        }
    }

    fn on_strip_drag(
        &mut self,
        is_seek: bool,
        bounds: Rc<Cell<Bounds<Pixels>>>,
        ev: &MouseMoveEvent,
        cx: &mut Context<Self>,
    ) {
        if !ev.dragging() {
            // 已松开但 mouse_up 未送达本条（如拖出窗外释放）→ 按最后锁定落点收尾。
            if self.scrubbing {
                self.end_strip_scrub(cx);
            }
            return;
        }
        if let Some(f) = fraction_at(&bounds, ev.position) {
            if is_seek && self.scrubbing {
                self.seek_to_fraction(f, false, cx);
            } else if !is_seek {
                self.apply_strip(false, f, cx);
            }
        }
    }

    /// 进度条松手收尾（窗口内 `on_mouse_up` / 拖出窗外 `on_mouse_up_out` 共用）：
    /// 窗口内优先用松手位置的精确分数刷新落点；窗外（fraction_at None）沿用最后锁定落点。
    fn on_strip_up(
        &mut self,
        is_seek: bool,
        bounds: Rc<Cell<Bounds<Pixels>>>,
        ev: &MouseUpEvent,
        cx: &mut Context<Self>,
    ) {
        if !is_seek || !self.scrubbing {
            return;
        }
        if let Some(f) = fraction_at(&bounds, ev.position) {
            let dur = self.player.read(cx).duration();
            self.scrub_target = dur * f as f64;
        }
        self.end_strip_scrub(cx);
    }

    /// 拖拽收尾：若最终落点尚未真实 seek（节流窗口内直接松手）→ 补发最后一次精确提交。
    fn end_strip_scrub(&mut self, cx: &mut Context<Self>) {
        if !self.scrubbing {
            return;
        }
        let target = self.scrub_target;
        self.scrubbing = false;
        // 先解锁播放器显示基准（拖拽钉扎解除），再按最终落点提交真实 seek。
        self.player.update(cx, |p, cx| p.set_scrubbing(false, cx));
        if self.scrub_seeked != Some(target) {
            self.scrub_seeked = Some(target);
            self.player.update(cx, |p, cx| p.seek(target, cx));
        }
        cx.notify();
    }

    /// 把进度条分数应用为 seek 落点：`force=true` 无条件真实 seek；`false` 走节流——
    /// 节流窗口内只更新显示基准（`set_seek_base`，显示由 seek_base 即时跟手）。
    fn seek_to_fraction(&mut self, f: f32, force: bool, cx: &mut Context<Self>) {
        let dur = self.player.read(cx).duration();
        let target = dur * f as f64;
        self.scrub_target = target;
        if force {
            self.last_scrub_retarget = Some(Instant::now());
            self.scrub_seeked = Some(target);
            self.player.update(cx, |p, cx| p.seek(target, cx));
        } else {
            let due = match self.last_scrub_retarget {
                Some(t) => t.elapsed() >= Duration::from_millis(SCRUB_RETARGET_MS),
                None => true,
            };
            if due {
                self.last_scrub_retarget = Some(Instant::now());
                self.scrub_seeked = Some(target);
                self.player.update(cx, |p, cx| p.seek(target, cx));
            } else {
                // 节流窗口内：显示仍逐 move 实时跟手（仅更新显示基准，不发 worker seek）。
                self.player.update(cx, |p, cx| p.set_seek_base(target, cx));
            }
        }
        // 拖动/点按跟手不寄生动画/tick：由本视图自己 notify 驱动重绘。
        cx.notify();
    }

    /// 应用拖拽比例（点按入口）：进度 → 立即真实 seek 并进拖拽态。
    fn apply_strip(&mut self, is_seek: bool, f: f32, cx: &mut Context<Self>) {
        if is_seek {
            self.seek_to_fraction(f, true, cx);
        }
    }
}
