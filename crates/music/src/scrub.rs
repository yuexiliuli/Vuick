//! 进度条 seek/scrub 节流状态机（bottom/npbar 共用；本批收敛自两份逐行拷贝）。
//!
//! 本批 Pattern 2：纯机械、零行为变化——把 `BottomBarView` 与 `NpBarView` 各自的
//! `begin_scrub / drag_scrub / end_scrub / end_strip_scrub / seek_to_fraction` 五个逐行
//! 同构方法 + `scrubbing / scrub_target / scrub_seeked / last_scrub_retarget` 四个字段
//! 收敛为一份共享实现。心跳（`HEARTBEAT_MS`）两侧常量与判定不同（120+无 visible vs
//! 250+visible），保持各自原样，不回收到本模块。

use std::time::{Duration, Instant};

use gpui::{Context, Entity, Window};

use crate::player::MusicPlayer;

/// 进度条拖拽真实 seek 节流（ms）：与父 `SCRUB_RETARGET_MS` 同量级（密集 move 防刷爆 worker）。
pub(crate) const SCRUB_RETARGET_MS: u64 = 60;

/// 进度条拖拽交互态（本视图自管；np/fs 条的拖拽态仍在父——二者不同时共存于单指针）。
/// 收敛自 bottom.rs / npbar.rs 两份逐行拷贝的四个字段 + 五个方法（本批 Pattern 2）。
pub(crate) struct ScrubState {
    /// 进度条是否正在拖拽（左键按下未松开）。
    pub(crate) scrubbing: bool,
    /// 拖拽当前锁定的落点（秒）：逐 move 更新，松手按此收尾提交（「松手停在当前位置」）。
    pub(crate) scrub_target: f64,
    /// 本次拖拽已向 worker 真实 seek 过的落点：松手同落点不重复 seek（无杂音）。
    pub(crate) scrub_seeked: Option<f64>,
    /// 拖拽节流：上次真实 seek 的时刻（密集 move 防刷爆 worker/杂音，对齐视频
    /// `SCRUB_RETARGET_MS`）。节流窗口内每 move 只碰显示基准，不影响跟手。
    pub(crate) last_scrub_retarget: Option<Instant>,
}

impl ScrubState {
    /// 全默认初始态（等价于原两个 View 的字段初始化）。
    pub(crate) fn new() -> Self {
        Self {
            scrubbing: false,
            scrub_target: 0.0,
            scrub_seeked: None,
            last_scrub_retarget: None,
        }
    }

    /// seek 点按进拖拽态 + 立即真实 seek（Slider `on_change`；原 `on_strip_down` seek 分支）。
    pub(crate) fn begin<C: 'static>(
        &mut self,
        f: f32,
        window: &mut Window,
        cx: &mut Context<C>,
        player: &Entity<MusicPlayer>,
        focus_root: impl FnOnce(&mut Window, &mut Context<C>),
    ) {
        // 补回根聚焦（Slider 拦了冒泡；点按即把焦点交回根）。
        focus_root(window, cx);
        self.scrubbing = true;
        self.last_scrub_retarget = Some(Instant::now());
        // 拖拽中 position() 钉在 seek_base（显示跟手指、不随时钟漂移）。
        player.update(cx, |p, cx| p.set_scrubbing(true, cx));
        self.seek_to_fraction(f, true, cx, player);
    }

    /// seek 拖拽 move：节流真实 seek、节流窗内 set_seek_base 跟手（Slider
    /// `on_change_during_drag`；原 `on_strip_drag` seek 分支——「松手但 move 未达」由 Slider
    /// 自有 MouseUp 兜底，不再需要旧 `ev.dragging()` 探针）。
    pub(crate) fn drag<C: 'static>(
        &mut self,
        f: f32,
        cx: &mut Context<C>,
        player: &Entity<MusicPlayer>,
    ) {
        self.seek_to_fraction(f, false, cx, player);
    }

    /// seek 松手精提交：按松手点精确分数更新落点（窗外沿用最后锁定落点——Slider 把 f 算成
    /// 精确/锁定分数），再补发末次精提交（Slider `on_commit`；原 `on_strip_up` +
    /// `end_strip_scrub`）。
    pub(crate) fn end<C: 'static>(
        &mut self,
        f: f32,
        cx: &mut Context<C>,
        player: &Entity<MusicPlayer>,
    ) {
        let dur = player.read(cx).duration();
        self.scrub_target = dur * f as f64;
        self.end_strip_scrub(cx, player);
    }

    /// 拖拽收尾：若最终落点尚未真实 seek（节流窗口内直接松手）→ 补发最后一次精确提交。
    pub(crate) fn end_strip_scrub<C: 'static>(
        &mut self,
        cx: &mut Context<C>,
        player: &Entity<MusicPlayer>,
    ) {
        if !self.scrubbing {
            return;
        }
        let target = self.scrub_target;
        self.scrubbing = false;
        // 先解锁播放器显示基准（拖拽钉扎解除），再按最终落点提交真实 seek。
        player.update(cx, |p, cx| p.set_scrubbing(false, cx));
        if self.scrub_seeked != Some(target) {
            self.scrub_seeked = Some(target);
            player.update(cx, |p, cx| p.seek(target, cx));
        }
        cx.notify();
    }

    /// 把进度条分数应用为 seek 落点：`force=true` 无条件真实 seek；`false` 走节流——
    /// 节流窗口内只更新显示基准（`set_seek_base`，显示由 seek_base 即时跟手）。
    pub(crate) fn seek_to_fraction<C: 'static>(
        &mut self,
        f: f32,
        force: bool,
        cx: &mut Context<C>,
        player: &Entity<MusicPlayer>,
    ) {
        let dur = player.read(cx).duration();
        let target = dur * f as f64;
        self.scrub_target = target;
        if force {
            self.last_scrub_retarget = Some(Instant::now());
            self.scrub_seeked = Some(target);
            player.update(cx, |p, cx| p.seek(target, cx));
        } else {
            let due = match self.last_scrub_retarget {
                Some(t) => t.elapsed() >= Duration::from_millis(SCRUB_RETARGET_MS),
                None => true,
            };
            if due {
                self.last_scrub_retarget = Some(Instant::now());
                self.scrub_seeked = Some(target);
                player.update(cx, |p, cx| p.seek(target, cx));
            } else {
                // 节流窗口内：显示仍逐 move 实时跟手（仅更新显示基准，不发 worker seek）。
                player.update(cx, |p, cx| p.set_seek_base(target, cx));
            }
        }
        // 拖动/点按跟手不寄生动画/tick：由本视图自己 notify 驱动重绘。
        cx.notify();
    }
}
