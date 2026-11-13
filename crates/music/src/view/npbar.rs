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
    AnyElement, Bounds, Context, DefiniteLength, Entity, InteractiveElement as _,
    IntoElement, ParentElement as _, Pixels, Render, Styled as _, WeakEntity, Window, div, px,
};

use super::{MusicView, theme};
use crate::player::MusicPlayer;
use ui::{CommitMode, Slider, SliderFill};

/// 进度心跳周期（ms，对齐父原 tick 250ms）：可见且播放中每拍自 notify 刷新进度/时间。
const HEARTBEAT_MS: u64 = 250;

/// 进度条拖拽真实 seek 节流（ms）：对齐父 `SCRUB_RETARGET_MS`/bottom.rs（密集 move 防刷爆
/// worker/杂音）。节流窗口内只更新显示基准（`set_seek_base`），显示仍逐 move 实时跟手。
const SCRUB_RETARGET_MS: u64 = 60;

/// 右栏「正在播放」进度条 + 时间子视图。
pub(super) struct NpBarView {
    /// 播放器实体（唯一状态源；只读）。
    pub(super) player: Entity<MusicPlayer>,
    /// 父视图弱柄：seek 点按把焦点交回根（Slider stop_propagation 拦了冒泡，需显式补）。
    parent: WeakEntity<MusicView>,
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
    pub(super) fn new(
        player: Entity<MusicPlayer>,
        parent: WeakEntity<MusicView>,
        cx: &mut Context<Self>,
    ) -> Self {
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
            parent,
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
        let progress = self.strip_element("np-progress", self.np_progress.clone(), frac, cx);
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
    // ---- 可交互条（seek 进度条）：走自绘 `ui::Slider`（拖拽态仍本视图自管） ----

    /// 构建 seek 进度条（替换原自绘条，与 bottom `strip_element` 同构）：整条 16px 命中高、
    /// 4px 圆角细轨（`bg_hover×0.6`）+ accent 实色填充（thumb=None），观感/命中与旧自绘条
    /// 一致；`bounds` 由 Slider `on_geometry` 每帧回写窗口坐标盒（替代旧 canvas 记录，点/拖/
    /// 松手与回归断言读法不变）。组件只回传分数事件，宿主收口（`begin_scrub`/`drag_scrub`/
    /// `end_scrub` 保 SCRUB_RETARGET 节流 + 松手精提交）。
    fn strip_element(
        &mut self,
        id: &'static str,
        bounds: Rc<Cell<Bounds<Pixels>>>,
        fraction: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entity = cx.entity();
        let f = fraction.clamp(0.0, 1.0);
        let slider = Slider::new(id)
            .value(f)
            .commit_mode(CommitMode::OnRelease)
            .track_height(px(4.))
            .track_radius(2.)
            // 轨道底色 = 现 track（bg_hover × 0.6）；填充 = music theme::accent()（thumb=None）。
            .track_color(theme::bg_hover().opacity(0.6))
            .fill(SliderFill::Solid(theme::accent()))
            .thumb(None)
            // 每帧把真实命中盒（窗口坐标）回写 `bounds`（替代旧 canvas 记录）。
            .on_geometry({
                let bounds = bounds.clone();
                move |geom, _w, _app| bounds.set(geom.bounds)
            })
            // seek 三阶段事件 → 宿主收口（scrubbing 进/退 + SCRUB_RETARGET 节流 + 松手补发）。
            .on_change({
                let entity = entity.clone();
                move |f, w, app| entity.update(app, |this, cx| this.begin_scrub(f, w, cx))
            })
            .on_change_during_drag({
                let entity = entity.clone();
                move |f, _w, app| entity.update(app, |this, cx| this.drag_scrub(f, cx))
            })
            .on_commit({
                let entity = entity.clone();
                move |f, _w, app| entity.update(app, |this, cx| this.end_scrub(f, cx))
            });
        div()
            .h(px(16.))
            .relative()
            // debug 只读探针：Slider 用 paint_quad 自绘填充（无元素盒），回归断言靠
            // `debug_bounds("np-progress-fill")` 读「填充宽 = f×条宽」——此处透明 div 只承载
            // debug_selector（不绘制、无命中盒），几何与 Slider 轨同（4px 居中）。
            .child(
                div()
                    .debug_selector(|| "np-progress-fill".into())
                    .absolute()
                    .left_0()
                    .top(px(6.))
                    .h(px(4.))
                    .w(DefiniteLength::Fraction(f)),
            )
            .child(slider)
            .into_any_element()
    }

    /// 点按把焦点交回音乐根：Slider `mouse_down` 已 `stop_propagation`，点条不再冒泡到根
    /// `on_root_mouse_down`（mod.rs 负责聚焦根）——仅当根未持焦时 space/方向键全局快捷键
    /// 会失效。此处补回与旧冒泡等价的聚焦路径（对齐 bottom 条；只加回焦点一拍）。
    fn focus_music_root(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(root) = self.parent.upgrade() {
            cx.focus_view(&root, window);
        }
    }

    /// seek 点按进拖拽态 + 立即真实 seek（Slider `on_change`；原 `on_strip_down` seek 分支）。
    fn begin_scrub(&mut self, f: f32, window: &mut Window, cx: &mut Context<Self>) {
        // 补回根聚焦（Slider 拦了冒泡；点按即把焦点交回根）。
        self.focus_music_root(window, cx);
        self.scrubbing = true;
        self.last_scrub_retarget = Some(Instant::now());
        // 拖拽中 position() 钉在 seek_base（显示跟手指、不随时钟漂移）。
        self.player.update(cx, |p, cx| p.set_scrubbing(true, cx));
        self.seek_to_fraction(f, true, cx);
    }

    /// seek 拖拽 move：节流真实 seek、节流窗内 set_seek_base 跟手（Slider
    /// `on_change_during_drag`；原 `on_strip_drag` seek 分支）。
    fn drag_scrub(&mut self, f: f32, cx: &mut Context<Self>) {
        self.seek_to_fraction(f, false, cx);
    }

    /// seek 松手精提交：按松手点精确分数更新落点（窗外沿用最后锁定落点——Slider 把 f 算成
    /// 精确/锁定分数），再补发末次精提交（Slider `on_commit`；原 `on_strip_up` +
    /// `end_strip_scrub`）。
    fn end_scrub(&mut self, f: f32, cx: &mut Context<Self>) {
        let dur = self.player.read(cx).duration();
        self.scrub_target = dur * f as f64;
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
}
