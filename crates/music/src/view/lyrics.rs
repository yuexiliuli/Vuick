//! 歌词面板**子视图**（性能优化第四步 1B）：`LyricsView` 从父 `MusicView` 拆出——歌词滚动
//! 跟随/手动态/同步歌词渲染各自独立（右栏 + 全屏**双实例**，各自 `ScrollHandle`，互不钳位）。
//!
//! 父在 `render` 按 `right_tab`/`fs_open` 给本视图注入 `visible` 门控：离树实例**心跳空转**
//! （不跑跟随、不 notify —— 离树 notify 无窗口无效化，纯浪费）；回树由父显式
//! [`LyricsView::reset_follow`] 重建跟随。心跳 = 250ms 常驻 loop：可见时执行
//! [`Self::update_lyrics_follow`]（切歌 path 自检复位 / 手动 3 秒恢复 / 可见窗钳位随迁不变），
//! 仅播放中 + 有歌词才 `cx.notify()` 自刷新（K12）——歌词高亮行推进只重绘本视图，不带动父整树。
//!
//! 布局/样式逐字平移自 `fullscreen.rs` 原实现：`#lyrics-{prefix}-scroll` 滚动容器 +
//! 同步歌词内容（可视/近邻窗口物化 + 上下定高垫片）+ 空态；行高三态与跟随量钳位不变。

use std::path::PathBuf;
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, Context, Entity, InteractiveElement as _, IntoElement, ParentElement as _,
    ScrollWheelEvent, StatefulInteractiveElement as _, Styled as _, Window, div, point, px,
};

use super::theme;
use ui::icon::{Icon, IconName};
use crate::player::MusicPlayer;

/// 歌词固定行高（px）：滚动量程反算与行窗物化都按它——三态行统一高，便于按偏移/视口
/// 精确反算行号与总高（内容总高 = 行数 × 行高）。
pub(super) const LYRIC_ROW_H: f32 = 46.0;
/// 手动滚动后自动恢复跟随的「停止滚动」时长（ms）。
pub(super) const LYRIC_FOLLOW_RESUME_MS: u64 = 3000;
/// 歌词滚动容器尚未布局前（bounds=0）兜底可见行数：估一行 46px、~11 行视口。
const LYRIC_FALLBACK_VISIBLE: usize = 11;
/// 可视行窗上下额外物化的缓冲行数（滚动时行间无缝衔接，防白缝）。
const LYRIC_ROW_BUFFER: usize = 2;
/// 歌词心跳周期（ms）：250ms，与父原 tick 同节拍——歌词跟随量钳位/手动 3 秒恢复语义不变。
const LYRIC_HEARTBEAT_MS: u64 = 250;

/// 歌词行三态（v2 `.fs-lyric`：active 白+加粗+accent 底 / past 暗 / future 更亮）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum LyricState {
    Active,
    Past,
    Future,
}

/// 歌词行三态判定：idx == active → Active；idx < active → Past；否则 Future。
fn lyric_state_for(active: Option<usize>, idx: usize) -> LyricState {
    if Some(idx) == active {
        LyricState::Active
    } else if active.is_some_and(|a| idx < a) {
        LyricState::Past
    } else {
        LyricState::Future
    }
}

/// 歌词面板子视图（右栏 + 全屏各一实例，`prefix` 隔离滚动容器 id / 元素状态）。
pub(super) struct LyricsView {
    /// 播放器实体（唯一状态源；只读）。
    pub(super) player: Entity<MusicPlayer>,
    /// 滚动容器 id 前缀（右栏 `lyrics-r` / 全屏 `lyrics-f`）：双实例树上共存，id 各异防碰撞。
    prefix: &'static str,
    /// 本实例当前是否被父挂在树上（父 render 按 right_tab/fs_open 门控 set）。离树心跳空转。
    pub(super) visible: bool,
    /// 歌词滚动容器句柄（`overflow_y_scroll` + `track_scroll`；原生滚轮与自动跟随
    /// 共用这一个 offset——跟随只写它、放手动时保持用户位置不动它）。
    pub(super) lyrics_scroll: gpui::ScrollHandle,
    /// 上次经自动跟随写入的目标偏移（句首坐标 y，offset 取其负值；None=当前非跟随态，
    /// 如已手动滚动/暂停/未布局）。用于检测「用户手动滚动」：实测 offset 偏离它即
    /// 记 `lyrics_manual_since` 暂停跟随。
    pub(super) lyrics_follow_y: Option<f32>,
    /// 手动浏览开始时刻（来自 `cx.background_executor().now()`，测试的虚拟时钟可控）。
    /// 停止滚动约 3 秒后自动恢复跟随。
    pub(super) lyrics_manual_since: Option<Instant>,
    /// 手动浏览期间上次观察到的滚动偏移（仍在滚动 → 刷新 3 秒计时）。
    lyrics_manual_last_y: f32,
    /// 当前参与歌词跟随的曲目路径（切歌/换曲时复位跟随态，新曲从自动跟随开始）。
    lyrics_track_path: Option<PathBuf>,
    /// 同步歌词心跳循环句柄（view 销毁即取消）。
    _heartbeat: Option<gpui::Task<()>>,
}

impl LyricsView {
    pub(super) fn new(
        player: Entity<MusicPlayer>,
        prefix: &'static str,
        cx: &mut Context<Self>,
    ) -> Self {
        let weak = cx.weak_entity();
        // 250ms 常驻心跳：可见（上树）时跑歌词跟随 + 播放中有歌词才自 notify（K12）。
        // 离树实例直接空转——让它跟着跑跟随只写自己的离树句柄、还白 notify，无意义。
        let heartbeat = cx.spawn(async move |_, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(LYRIC_HEARTBEAT_MS))
                    .await;
                let _ = weak.update(cx, |this, cx| {
                    if !this.visible {
                        return;
                    }
                    this.update_lyrics_follow(cx);
                    if this.player.read(cx).playing() && this.has_lyrics(cx) {
                        cx.notify();
                    }
                });
            }
        });
        Self {
            player,
            prefix,
            visible: false,
            lyrics_scroll: gpui::ScrollHandle::new(),
            lyrics_follow_y: None,
            lyrics_manual_since: None,
            lyrics_manual_last_y: 0.0,
            lyrics_track_path: None,
            _heartbeat: Some(heartbeat),
        }
    }

    /// 是否当前曲目有（非空）歌词——心跳自 notify 的门控之一（无歌词空态不随位置变化）。
    fn has_lyrics(&self, cx: &Context<Self>) -> bool {
        self.player
            .read(cx)
            .current_track()
            .and_then(|t| t.lyrics.as_ref())
            .is_some_and(|l| !l.lines.is_empty())
    }

    /// 复位歌词跟随/手动态（父在 `toggle_fs`/`set_right_tab` 时显式调——换渲染上下文从
    /// 自动跟随重新开始；保持 render 无副作用纪律，不把复位埋进 render）。
    pub(super) fn reset_follow(&mut self) {
        self.lyrics_manual_since = None;
        self.lyrics_manual_last_y = 0.0;
        self.lyrics_follow_y = None;
    }

    /// `#lyrics-{prefix}-scroll` 滚动容器：挂 `on_scroll_wheel`——**每次滚轮事件**都记
    /// 「手动滚动」并退出自动跟随。关键点：offset 已钳到界限（内容尽头）时原生滚动事件
    /// 照样触发 → 用户持续滚动时「停止滚动」计时被持续刷新，不再满 ~3s 误恢复拉回视窗
    /// （真机复报 bug1）。`overflow_y_scroll` 原生滚动手感不变，`track_scroll` 绑定句柄。
    fn lyrics_scroll_container(
        &mut self,
        cx: &mut Context<Self>,
        inner: impl IntoElement,
    ) -> AnyElement {
        let wheel = cx.listener(
            |this: &mut LyricsView, _ev: &ScrollWheelEvent, _w: &mut Window, cx: &mut Context<Self>| {
                this.lyrics_manual_since = Some(cx.background_executor().now());
                this.lyrics_follow_y = None;
            },
        );
        div()
            .id(format!("{}-scroll", self.prefix))
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .track_scroll(&self.lyrics_scroll)
            .on_scroll_wheel(wheel)
            .child(inner)
            .into_any_element()
    }

    /// 同步歌词内容（右栏「歌词」tab 与全屏右列共用一套实现，各实例独立句柄）：有歌词 →
    /// `overflow_y_scroll` 可滚动容器，内容总高 = 行数×固定行高（全歌词可经滚轮浏览）；
    /// 只物化可视/近邻窗口行（K12：播放中 250ms 心跳不全量物化几百行）；自动跟随（active
    /// 行居中）与手动滚动暂停/恢复由心跳 → [`Self::update_lyrics_follow`] 写滚动偏移。
    /// 无歌词 → 空态 + 「同名 .lrc」提示。`flex_1` 由调用方包一层保证占满可用高。
    fn render_lyrics_panel_content(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let p = self.player.read(cx);
        let pos_ms = (p.position() * 1000.0) as i64;
        let lyrics = p.current_track().and_then(|t| t.lyrics.clone());
        let Some(lyrics) = lyrics else {
            return div()
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .px(px(16.))
                .child(
                    Icon::new(IconName::Lyrics)
                        .size(px(32.))
                        .text_color(theme::text3()),
                )
                .child(
                    div()
                        .mt(px(12.))
                        .text_size(px(14.))
                        .text_color(theme::text3())
                        .child("暂无歌词"),
                )
                .child(
                    div()
                        .mt(px(6.))
                        .text_size(px(12.))
                        .text_color(theme::text3().opacity(0.7))
                        .child("同名 .lrc 放在歌曲旁即可显示同步歌词"),
                )
                .into_any_element();
        };

        let n = lyrics.lines.len();
        if n == 0 {
            return div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(14.))
                .text_color(theme::text3())
                .child("暂无歌词")
                .into_any_element();
        }
        let active = lyrics.active_index(pos_ms);
        let viewport_h = self.lyrics_scroll.bounds().size.height.as_f32();
        // 内容总高 ≤ 视口：全部物化并垂直居中（歌词少时保持「居中」观感，无可滚范围）。
        // 进入本分支时 viewport_h 必 > 0（n>0 且首帧未布局时走滚动分支兜底）。
        let fits = n as f32 * LYRIC_ROW_H <= viewport_h;
        let mut rows: Vec<AnyElement> = Vec::new();
        if fits {
            for (i, line) in lyrics.lines.iter().enumerate() {
                rows.push(self.lyric_row(&line.text, lyric_state_for(active, i)));
            }
            return self.lyrics_scroll_container(
                cx,
                div()
                    .flex()
                    .flex_col()
                    .h(px(viewport_h))
                    .justify_center()
                    .children(rows),
            );
        }
        // 超视口：只物化可视/近邻窗口行，上下补定高垫片保持内容总高 = n×行高（可滚全量）。
        let (first, last) = self.lyrics_visible_window(n);
        for (i, line) in lyrics.lines[first..last].iter().enumerate() {
            let idx = first + i;
            rows.push(self.lyric_row(&line.text, lyric_state_for(active, idx)));
        }
        let top_h = first as f32 * LYRIC_ROW_H;
        let bottom_h = (n - last) as f32 * LYRIC_ROW_H;
        self.lyrics_scroll_container(
            cx,
            div()
                .flex()
                .flex_col()
                .h(px(n as f32 * LYRIC_ROW_H))
                .child(div().h(px(top_h)))
                .children(rows)
                .child(div().h(px(bottom_h))),
        )
    }

    /// 歌词当前应物化的行窗 `(first, last_exclusive)`：由当前滚动偏移/视口反算行号
    /// （行高固定 → 精确），再外扩缓冲行。渲染物化与测试断言共用同一算法。
    pub(super) fn lyrics_visible_window(&mut self, n: usize) -> (usize, usize) {
        if n == 0 {
            return (0, 0);
        }
        let viewport_h = self.lyrics_scroll.bounds().size.height.as_f32();
        let max = self.lyrics_scroll.max_offset().y.as_f32();
        // 先钳位再反算：滚轮处理先加、布局才钳回（div.rs `scroll_offset.y += delta_y` 无
        // 界内钳制）→ render/prepaint 可能读到「max + 残量」的越界瞬时偏移；按钳位后
        // 偏移算行窗，保证与最终布局一致——内容尽头继续滚轮不再错位一行/1 帧闪（复报二）。
        let raw = -self.lyrics_scroll.offset().y.as_f32();
        let off = raw.clamp(0.0, max.max(0.0));
        let first = (off / LYRIC_ROW_H).floor().max(0.0) as usize;
        let first = first.min(n.saturating_sub(1));
        let visible = if viewport_h > 0.0 {
            (viewport_h / LYRIC_ROW_H).ceil() as usize
        } else {
            LYRIC_FALLBACK_VISIBLE
        };
        let last = (first + visible + LYRIC_ROW_BUFFER * 2).min(n);
        (first, last)
    }

    /// 自动跟随目标滚动量（句首坐标 y，px；写入 offset 时取其负值）：active 行居中、
    /// 首/末行越界夹到 [0, max]（`max_offset` 布局后才有值）。供跟随与测试共用。
    pub(super) fn lyrics_follow_scroll_top(&mut self, active: usize, n: usize) -> f32 {
        if n == 0 {
            return 0.0;
        }
        let viewport_h = self.lyrics_scroll.bounds().size.height.as_f32();
        let max_y = self.lyrics_scroll.max_offset().y.as_f32();
        let center = active as f32 * LYRIC_ROW_H + LYRIC_ROW_H * 0.5 - viewport_h * 0.5;
        center.max(0.0).min(max_y.max(0.0))
    }

    /// 歌词自动跟随（每拍由心跳调用，仅本实例可见时）：仅播放中执行——按当前播放位置得
    /// active 行，把滚动偏移写入「active 行居中」（无动画、不闪跳，维持现状跟随手感）。
    /// 跟随中若实测偏移偏离了上次写入值（用户滚轮/拖滚动条手动滚动）→ 记
    /// `lyrics_manual_since` 暂停跟随；停止滚动 ≥ [`LYRIC_FOLLOW_RESUME_MS`] 后自动恢复
    /// 跟随当前句。切歌/换曲（track path 变化）复位跟随态。暂停/空闲不写入（尊重手动）。
    pub(super) fn update_lyrics_follow(&mut self, cx: &mut Context<Self>) {
        // 非播放：不动滚动（尊重手动浏览）；清跟随基准，恢复播放时重新建立跟随。
        if !self.player.read(cx).playing() {
            self.lyrics_follow_y = None;
            return;
        }
        let Some(track) = self.player.read(cx).current_track() else {
            self.lyrics_follow_y = None;
            return;
        };
        let Some(lyrics) = &track.lyrics else {
            self.lyrics_follow_y = None;
            return;
        };
        if lyrics.lines.is_empty() {
            return;
        }
        // 切歌/换曲（path 变化）→ 复位跟随态，新曲从自动跟随当前句开始。
        if self.lyrics_track_path.as_deref() != Some(track.path.as_path()) {
            self.lyrics_track_path = Some(track.path.clone());
            self.lyrics_manual_since = None;
            self.lyrics_manual_last_y = 0.0;
            self.lyrics_follow_y = None;
        }
        let n = lyrics.lines.len();
        let pos_ms = (self.player.read(cx).position() * 1000.0) as i64;
        let active = lyrics.active_index(pos_ms).unwrap_or(0);
        let handle = self.lyrics_scroll.clone();
        let follow_y = -self.lyrics_follow_scroll_top(active, n);
        let offset_now = handle.offset().y.as_f32();
        const EPS: f32 = 2.0;
        // 跟随中：若实测偏移偏离了我们上次写入的值 → 用户手动滚过，暂停跟随。
        if self.lyrics_manual_since.is_none()
            && let Some(prev) = self.lyrics_follow_y
            && (offset_now - prev).abs() > EPS
        {
            self.lyrics_manual_since = Some(cx.background_executor().now());
            self.lyrics_manual_last_y = offset_now;
            self.lyrics_follow_y = None;
        }
        let now = cx.background_executor().now();
        match self.lyrics_manual_since {
            Some(since) => {
                if now.duration_since(since) >= Duration::from_millis(LYRIC_FOLLOW_RESUME_MS) {
                    // 停止滚动 ≥3s：恢复跟随当前句。防闪（真机复报 bug2）：目标与当前
                    // 偏移差不足一行时不写 offset（不触发重排/视窗边界抖动），直接认定
                    // 当前视口即跟随位；偏差超过一行才写一次回到居中。
                    self.lyrics_manual_since = None;
                    if (offset_now - follow_y).abs() <= LYRIC_ROW_H {
                        self.lyrics_follow_y = Some(offset_now);
                    } else {
                        self.lyrics_follow_y = Some(follow_y);
                        handle.set_offset(point(px(0.), px(follow_y)));
                    }
                } else if (offset_now - self.lyrics_manual_last_y).abs() > EPS {
                    // 仍在滚动（scrollbar 拖拽 / fling 落地未触滚轮事件时兜底）：刷新计时。
                    self.lyrics_manual_since = Some(now);
                    self.lyrics_manual_last_y = offset_now;
                }
                // 否则保持用户浏览位置，不写入偏移。
            }
            None => {
                // 自动跟随：写入居中偏移（幂等；GPUI 布局侧还会做最终夹取）。
                self.lyrics_follow_y = Some(follow_y);
                handle.set_offset(point(px(0.), px(follow_y)));
            }
        }
    }

    /// 单句歌词行（v2 `.fs-lyric` 三态样式；固定行高 [`LYRIC_ROW_H`]——滚动量程按它反算）。
    fn lyric_row(&mut self, text: &str, state: LyricState) -> AnyElement {
        let (size, color, bg, weight) = match state {
            LyricState::Active => (
                px(17.),
                gpui::white(),
                theme::accent().opacity(0.10),
                Some(gpui::FontWeight::BOLD),
            ),
            LyricState::Past => (
                px(15.),
                gpui::white().opacity(0.13),
                gpui::transparent_black(),
                None,
            ),
            LyricState::Future => (
                px(15.),
                gpui::white().opacity(0.24),
                gpui::transparent_black(),
                None,
            ),
        };
        let mut el = div()
            .px(px(14.))
            .h(px(LYRIC_ROW_H))
            .flex()
            .items_center()
            .rounded(px(6.))
            .text_size(size)
            .text_color(color)
            .bg(bg);
        if let Some(w) = weight {
            el = el.font_weight(w);
        }
        el.child(text.to_string()).into_any_element()
    }
}

impl gpui::Render for LyricsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl gpui::IntoElement {
        self.render_lyrics_panel_content(cx)
    }
}
