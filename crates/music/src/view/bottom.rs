//! 底部播放栏（80px）**子视图**（性能优化第三步 B 段）：整块底栏从父 `MusicView` 拆成
//! 本视图独占的 [`BottomBarView`]——自带 ~120ms 进度心跳（仅播放中自 notify），不再搭
//! eq 动画帧 / 父 250ms tick 的「整树 notify」便车（进度平滑从 4Hz 提到 ~8.3Hz 且不带动父）；
//! 拖拽 seek/音量由本视图自管（写 player + 自己 notify 跟手）；按钮交互经
//! `parent: WeakEntity<MusicView>` 回父（父状态真变）；悬停提示走父全局 tooltip 层——
//! 本视图把按钮盒写入父 `tip_boxes` 同名 cell、悬停状态写父 `tip`（锚定/延迟/换钮语义与
//! 既有 T 系列回归一致，不退回光标定位）。
//!
//! 布局照旧：`280px / 1fr / 240px` 三列；可拖拽进度条（bb_progress）与音量条（bb_vol）。

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use gpui::{
    AnyElement, App, Bounds, Context, CursorStyle, DefiniteLength, Entity, InteractiveElement as _,
    IntoElement, ParentElement as _, Pixels, Render, StatefulInteractiveElement as _, Styled as _,
    StyledImage, WeakEntity, Window, div, img, px,
};

use crate::model::RepeatMode;
use crate::player::MusicPlayer;
use crate::scrub::ScrubState;

use super::icon::{IconView, MusicIcon};
use super::{IntoIconElement, MusicView, RightTab, theme};
use ui::icon::{Icon, IconName};
use ui::{CommitMode, Slider, SliderFill};

/// 底部栏高（px）。
pub(super) const BOTTOM_H: f32 = 80.0;

/// 进度心跳周期（ms，目标 ~8Hz）：播放中每拍自 notify 刷新进度/时间文本。
/// 暂停/空闲不 notify——进度心跳不与父 250ms tick 重复刷（父 tick 仍是结构/歌词的 4Hz 基准）。
const HEARTBEAT_MS: u64 = 120;

/// 底栏全部可悬停提示钮的 id：其窗口盒写入父 `tip_boxes` 同名 cell（tooltip 锚定按钮盒，
/// 全局浮层由父 `render_tooltip` 绘制——保证「父对全局 tooltip 层保持」且 T 系列回归不破）。
const BB_TIP_IDS: &[&'static str] = &[
    "bb-like",
    "bb-shuffle",
    "bb-prev",
    "bb-next",
    "bb-play",
    "bb-repeat",
    "bb-lyrics",
    "bb-queue",
    "bb-fs",
    "bb-vol",
];

/// 拉取父 `tip_boxes` 的各底栏钮窗口盒 cell——已并入 [`BottomBarView::seed_tip_cells`]
/// （`new` 期间父已 lease 无法 update，须延迟到首次 render 才拉，见其注释）。

pub(super) struct BottomBarView {
    player: Entity<MusicPlayer>,
    /// 父视图弱柄：按钮交互/收藏/hover 提示回父（WeakEntity 防循环引用）。
    parent: WeakEntity<MusicView>,
    /// 进度条窗口坐标盒（canvas prepaint 记录；可交互条点/拖/松手与回归断言共用）。
    pub(super) bb_progress: Rc<Cell<Bounds<Pixels>>>,
    /// 音量条窗口坐标盒（与父 `tip_boxes["bb-vol"]` 同一 cell——条盒即悬停锚点盒）。
    bb_vol: Rc<Cell<Bounds<Pixels>>>,
    /// 各悬停钮的窗口盒（写入父 tip_boxes 同名 cell；tooltip 锚定经父全局层渲染）。
    tip_cells: HashMap<&'static str, Rc<Cell<Bounds<Pixels>>>>,
    // 进度条拖拽交互态（本视图自管；np/fs 条的拖拽态仍在父——二者不同时共存于单指针；
    // 收敛自 scrub 节流状态机，见 scrub.rs 本批 Pattern 2）。
    scrub: ScrubState,
    /// 进度心跳循环句柄（view 销毁即取消）。
    _heartbeat: Option<gpui::Task<()>>,
}

impl BottomBarView {
    pub(super) fn new(
        player: Entity<MusicPlayer>,
        parent: WeakEntity<MusicView>,
        cx: &mut Context<Self>,
    ) -> Self {
        // 拉取父 tip_boxes 的各底栏钮 cell 推迟到首次 render（见 [`Self::seed_tip_cells`]）：
        // `new` 期间父 MusicView 正被构造（已 lease），此时 `parent.update` 会 double-lease panic。
        let tip_cells = HashMap::new();
        // 进度心跳：仅播放中每 ~120ms 自 notify（子视图脏 → 重画拉最新 position 刷新进度/时间）。
        let weak = cx.weak_entity();
        let heartbeat = cx.spawn(async move |_, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(HEARTBEAT_MS))
                    .await;
                let _ = weak.update(cx, |this, cx| {
                    if this.player.read(cx).playing() {
                        cx.notify();
                    }
                });
            }
        });
        Self {
            player,
            parent,
            bb_progress: Rc::new(Cell::new(Bounds::default())),
            bb_vol: Rc::new(Cell::new(Bounds::default())),
            tip_cells,
            scrub: ScrubState::new(),
            _heartbeat: Some(heartbeat),
        }
    }

    /// 首次 render 时拉取父 `tip_boxes` 的各底栏钮 cell（懒建到父）——延迟到此刻是因
    /// `new` 期间父 MusicView 还在构造（已 lease），此时无法 `parent.update`。首帧后
    /// 幂等跳过；各钮的窗口盒写入父同名 cell，父全局 tooltip 按它锚定。
    fn seed_tip_cells(&mut self, cx: &mut Context<Self>) {
        if !self.tip_cells.is_empty() {
            return;
        }
        let mut m = HashMap::new();
        if let Some(p) = self.parent.upgrade() {
            for &id in BB_TIP_IDS {
                let cell = p.update(&mut *cx, |this, _cx| this.tip_box_cell(id));
                m.insert(id, cell);
            }
        }
        self.bb_vol = m.get("bb-vol").cloned().unwrap_or_default();
        self.tip_cells = m;
    }

    /// 只读父当前右侧面板 tab（active 高亮显示用；克隆 Copy 值不持借用）。
    fn parent_right_tab(&self, cx: &Context<Self>) -> RightTab {
        self.parent
            .upgrade()
            .map(|p| p.read(cx).state.right_tab)
            .unwrap_or_default()
    }

    /// 悬停提示监听（回父全局 tooltip 层，语义与父 `hover_tip_listener` 一致）：
    /// 进入/换钮 → 把本按钮盒拷入父 `tip_box` + 设父 `tip`（父 `schedule_tooltip_flush`
    /// 到点 notify 让父浮层绘出）；离开若文案正是本钮则清空（换钮同拍顺序无关）。
    /// 时刻用 `background_executor` 时钟（测试 `advance_clock` 可控延迟判定）。
    fn hover_listener(
        &self,
        label: &'static str,
        cell: Rc<Cell<Bounds<Pixels>>>,
    ) -> impl Fn(&bool, &mut Window, &mut App) + 'static {
        let parent = self.parent.clone();
        move |hovered, _w, cx| {
            let _ = parent.update(cx, |this, cx| {
                if *hovered {
                    this.tip = Some((label, cx.background_executor().now()));
                    this.tip_box = Some(cell.get());
                    this.schedule_tooltip_flush(cx);
                } else if this.tip.is_some_and(|(l, _)| l == label) {
                    this.tip = None;
                    this.tip_box = None;
                }
                cx.notify();
            });
        }
    }
}

impl Render for BottomBarView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.seed_tip_cells(cx);
        let p = self.player.read(cx);
        let title = p
            .current_track()
            .map(|t| t.title.clone())
            .unwrap_or_else(|| "未在播放".to_string());
        let artist = p
            .current_track()
            .map(|t| t.artist.clone())
            .unwrap_or_else(|| "—".to_string());
        let grad = p.current_track().map(|t| t.grad);
        let cur_liked = p.current().map(|i| p.is_liked(i)).unwrap_or(false);
        let (light, dark) = grad
            .map(theme::gradient_pair_of)
            .unwrap_or((theme::accent(), theme::bg_deep()));

        div()
            .id("music-bottom")
            .debug_selector(|| "music-bottom".into())
            .h(px(BOTTOM_H))
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_x(px(16.))
            .px(px(20.))
            .bg(theme::bg_primary())
            .border_t_1()
            .border_color(theme::border())
            .child(self.render_bb_song(title, artist, (light, dark), cur_liked, cx))
            .child(self.render_bb_center(cx))
            .child(self.render_bb_right(window, cx))
    }
}

impl BottomBarView {
    /// 左列（280px）：缩略图渐变 + 歌名/艺术家 + 心形。
    fn render_bb_song(
        &mut self,
        title: String,
        artist: String,
        (light, dark): (gpui::Hsla, gpui::Hsla),
        liked: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let parent = self.parent.clone();
        let cl = cx.listener(move |_this: &mut BottomBarView, _ev, _w, cx| {
            // 收藏回父：父 `like_current` 负责 toggle + toast（语义等同现状）。
            let _ = parent.upgrade().map(|p| p.update(cx, |mv, cx| mv.like_current(cx)));
        });
        // 底栏缩略图：有真实封面 → 铺图（父 covers 缓存）；无 → 渐变 + 音符回退。
        let cover = self
            .parent
            .upgrade()
            .map(|p| p.update(cx, |mv, cx| mv.cover_for_current(cx)))
            .flatten();
        let thumb_children: Vec<gpui::AnyElement> = if let Some(c) = cover {
            vec![div()
                .size_full()
                .child(
                    img(gpui::ImageSource::Render(c))
                        .object_fit(gpui::ObjectFit::Cover)
                        .w_full()
                        .h_full(),
                )
                .into_any_element()]
        } else {
            vec![
                theme::gradient_cover((light, dark), 8.0).into_any_element(),
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        Icon::new(IconName::Note)
                            .size(px(22.))
                            .text_color(theme::white_fade_40()),
                    )
                    .into_any_element(),
            ]
        };
        div()
            .w(px(280.))
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_x(px(12.))
            .child(
                div()
                    .w(px(48.))
                    .h(px(48.))
                    .rounded(px(8.))
                    .relative()
                    .flex_shrink_0()
                    .overflow_hidden()
                    .children(thumb_children),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .child(
                        div()
                            .text_size(px(13.))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme::text())
                            .text_ellipsis()
                            .child(title),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(theme::text3())
                            .text_ellipsis()
                            .child(artist),
                    ),
            )
            .child(
                div()
                    .id("bb-like")
                    .debug_selector(|| "bb-like".into())
                    .relative()
                    .cursor(CursorStyle::PointingHand)
                    .on_click(cl)
                    .on_hover(self.hover_listener(
                        "收藏",
                        self.tip_cells.get("bb-like").cloned().unwrap_or_default(),
                    ))
                    .child(
                        theme::bounds_canvas(
                            self.tip_cells.get("bb-like").cloned().unwrap_or_default(),
                        ),
                    )
                    .child(
                        Icon::new(if liked { IconName::Heart } else { IconName::HeartBorder })
                            .size(px(18.))
                            .text_color(if liked { theme::pink() } else { theme::text3() }),
                    ),
            )
            .into_any_element()
    }

    /// 中列（flex_1）：控制行 + 进度条 + 时间。
    fn render_bb_center(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let p = self.player.read(cx);
        let dur = p.duration();
        let pos = p.position();
        let shuffle = p.shuffle();
        let repeat = p.repeat();
        let frac = if dur > 0.0 { (pos / dur).clamp(0.0, 1.0) as f32 } else { 0.0 };
        let progress = self.strip_element(
            "bb-progress",
            self.bb_progress.clone(),
            frac,
            true,
            None,
            None,
            cx,
        );
        div()
            .flex_1()
            .min_w(px(0.))
            .flex()
            .flex_col()
            .items_center()
            .gap_y(px(5.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_x(px(14.))
                    .child(self.bb_icon(IconName::Shuffle, 20., shuffle, "bb-shuffle", true, cx))
                    .child(self.bb_icon(MusicIcon::Prev, 20., false, "bb-prev", false, cx))
                    .child(self.bb_main_btn(cx))
                    .child(self.bb_icon(MusicIcon::Next, 20., false, "bb-next", false, cx))
                    .child(self.bb_repeat(repeat, cx)),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_x(px(8.))
                    .w_full()
                    .max_w(px(560.))
                    .child(
                        div()
                            .w(px(36.))
                            .text_size(px(11.))
                            .text_color(theme::text3())
                            .text_align(gpui::TextAlign::Center)
                            .child(crate::model::duration_str(pos)),
                    )
                    .child(progress)
                    .child(
                        div()
                            .w(px(36.))
                            .text_size(px(11.))
                            .text_color(theme::text3())
                            .text_align(gpui::TextAlign::Center)
                            .child(crate::model::duration_str(dur)),
                    ),
            )
            .into_any_element()
    }

    /// 中列小型图标钮（shuffle/prev/next）。
    fn bb_icon<I: IntoIconElement>(
        &mut self,
        icon: I,
        size: f32,
        active: bool,
        id: &'static str,
        _toggle: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let tip: Option<&'static str> = match id {
            "bb-shuffle" => Some("随机播放"),
            "bb-prev" => Some("上一首"),
            "bb-next" => Some("下一首"),
            _ => None,
        };
        let parent = self.parent.clone();
        let cl = cx.listener(move |_this: &mut BottomBarView, _ev, _w, cx| {
            let _ = match id {
                "bb-shuffle" => parent.clone().update(cx, |mv, cx| mv.toggle_shuffle(cx)),
                "bb-prev" => parent
                    .clone()
                    .update(cx, |mv, cx| mv.player.update(cx, |p, cx| p.prev(cx))),
                "bb-next" => parent
                    .clone()
                    .update(cx, |mv, cx| mv.player.update(cx, |p, cx| p.next(cx))),
                _ => parent.clone().update(cx, |mv, cx| mv.toggle_shuffle(cx)),
            };
        });
        let mut el = div()
            .id(id)
            .debug_selector(move || id.to_string())
            .w(px(size))
            .h(px(size))
            .relative()
            .flex()
            .items_center()
            .justify_center()
            .cursor(CursorStyle::PointingHand)
            .on_click(cl)
            .child(
                icon.icon_element(
                    px(size),
                    if active { theme::accent_light() } else { theme::text2() },
                ),
            );
        if let Some(t) = tip {
            let cell = self.tip_cells.get(id).cloned().unwrap_or_default();
            el = el.on_hover(self.hover_listener(t, cell.clone()));
            el = el.child(theme::bounds_canvas(cell));
        }
        el.into_any_element()
    }

    /// 中列主播放钮（36px accent 圆）。
    fn bb_main_btn(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let p = self.player.read(cx);
        let playing = p.playing();
        let icon = if playing { MusicIcon::Pause } else { MusicIcon::Play };
        let parent = self.parent.clone();
        let cl = cx.listener(move |_this: &mut BottomBarView, _ev, _w, cx| {
            let _ = parent.clone().update(cx, |mv, cx| mv.toggle_play(cx));
        });
        div()
            .id("bb-play")
            .debug_selector(|| "bb-play".into())
            .w(px(36.))
            .h(px(36.))
            .relative()
            .rounded(px(18.))
            .flex()
            .items_center()
            .justify_center()
            .bg(theme::accent())
            .cursor(CursorStyle::PointingHand)
            .on_click(cl)
            .on_hover(self.hover_listener(
                "播放/暂停",
                self.tip_cells.get("bb-play").cloned().unwrap_or_default(),
            ))
            .child(
                theme::bounds_canvas(
                    self.tip_cells.get("bb-play").cloned().unwrap_or_default(),
                ),
            )
            .child(IconView::new(icon).size(px(20.)).text_color(theme::text()))
            .into_any_element()
    }

    fn bb_repeat(&mut self, repeat: RepeatMode, cx: &mut Context<Self>) -> AnyElement {
        let active = repeat != RepeatMode::Off;
        let parent = self.parent.clone();
        let cl = cx.listener(move |_this: &mut BottomBarView, _ev, _w, cx| {
            let _ = parent.clone().update(cx, |mv, cx| mv.cycle_repeat(cx));
        });
        div()
            .id("bb-repeat")
            .debug_selector(|| "bb-repeat".into())
            .relative()
            .w(px(20.))
            .h(px(20.))
            .flex()
            .items_center()
            .justify_center()
            .cursor(CursorStyle::PointingHand)
            .on_click(cl)
            .on_hover(self.hover_listener(
                "循环播放",
                self.tip_cells.get("bb-repeat").cloned().unwrap_or_default(),
            ))
            .child(
                theme::bounds_canvas(
                    self.tip_cells.get("bb-repeat").cloned().unwrap_or_default(),
                ),
            )
            .child(
                Icon::new(IconName::Repeat)
                    .size(px(18.))
                    .text_color(if active { theme::accent_light() } else { theme::text2() }),
            )
            .into_any_element()
    }

    /// 右列（240px）：歌词/队列 tab 钮 + 音量（图标 + 条）+ 全屏钮。
    fn render_bb_right(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let p = self.player.read(cx);
        let volume = p.volume();
        let vol_strip = self.strip_element(
            "bb-vol",
            self.bb_vol.clone(),
            volume,
            false,
            Some(80.),
            Some("音量"),
            cx,
        );
        div()
            .w(px(240.))
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_end()
            .gap_x(px(12.))
            .child(self.bb_tab_toggle(IconName::Lyrics, RightTab::Lyrics, "bb-lyrics", cx))
            .child(self.bb_tab_toggle(IconName::QueueMusic, RightTab::Queue, "bb-queue", cx))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_x(px(8.))
                    .child(
                        IconView::new(MusicIcon::Volume).size(px(18.)).text_color(theme::text2()),
                    )
                    .child(vol_strip),
            )
            .child(
                div()
                    .id("bb-fs")
                    .debug_selector(|| "bb-fs".into())
                    .relative()
                    .cursor(CursorStyle::PointingHand)
                    .on_click({
                        let parent = self.parent.clone();
                        cx.listener(move |_this: &mut BottomBarView, _ev, _w, cx| {
                            let _ = parent.clone().update(cx, |mv, cx| mv.toggle_fs(cx));
                        })
                    })
                    .on_hover(self.hover_listener(
                        "全屏",
                        self.tip_cells.get("bb-fs").cloned().unwrap_or_default(),
                    ))
                    .child(
                        theme::bounds_canvas(
                            self.tip_cells.get("bb-fs").cloned().unwrap_or_default(),
                        ),
                    )
                    .child(
                        Icon::new(IconName::Fullscreen)
                            .size(px(18.))
                            .text_color(theme::text2()),
                    ),
            )
            .into_any_element()
    }

    /// 右列歌词/队列 tab 钮：切右侧面板 tab（回父 `set_right_tab`，父状态真变）。
    fn bb_tab_toggle(
        &mut self,
        icon: IconName,
        tab: RightTab,
        id: &'static str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let active = self.parent_right_tab(cx) == tab;
        let tip: &'static str = match tab {
            RightTab::Lyrics => "歌词",
            RightTab::Queue => "播放队列",
            _ => "歌词",
        };
        let parent = self.parent.clone();
        let cl = cx.listener(move |_this: &mut BottomBarView, _ev, _w, cx| {
            let _ = parent.clone().update(cx, |mv, cx| mv.set_right_tab(tab, cx));
        });
        let cell = self.tip_cells.get(id).cloned().unwrap_or_default();
        div()
            .id(id)
            .debug_selector(move || id.to_string())
            .relative()
            .cursor(CursorStyle::PointingHand)
            .on_click(cl)
            .on_hover(self.hover_listener(tip, cell.clone()))
            .child(theme::bounds_canvas(cell))
            .child(
                Icon::new(icon)
                    .size(px(18.))
                    .text_color(if active { theme::accent_light() } else { theme::text2() }),
            )
            .into_any_element()
    }

    // ---- 可交互条（进度/音量 share）：走自绘 `ui::Slider`（拖拽态仍本视图自管） ----

    /// 构建一条可交互条（seek 进度条 / 音量条共用，替换原自绘条）：整条 16px 命中高、4px
    /// 圆角细轨（`bg_hover×0.6`）+ accent 实色填充（thumb=None），观感/命中与旧自绘条一致；
    /// `bounds` 由 Slider `on_geometry` 每帧回写窗口坐标盒（替代旧 canvas 记录，点/拖/松手与
    /// 回归断言读法不变；`bb_vol` 同时是悬停锚点盒）。组件只回传分数事件，宿主收口（见
    /// [`Self::begin_scrub`]/[`Self::drag_scrub`]/[`Self::end_scrub`]/[`Self::apply_vol`]）：
    /// seek 走 `OnRelease`（on_change=点按进拖拽态+force seek / on_change_during_drag=节流跟手
    /// / on_commit=松手精提交），vol 走 `Immediate`（on_change+on_change_during_drag 即时
    /// set_volume、无 on_commit）。`tip=Some(名称)` 时挂悬停提示（音量条用，提示留在消费方
    /// 不进组件）。
    fn strip_element(
        &mut self,
        id: &'static str,
        bounds: Rc<Cell<Bounds<Pixels>>>,
        fraction: f32,
        is_seek: bool,
        width: Option<f32>,
        tip: Option<&'static str>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entity = cx.entity();
        let f = fraction.clamp(0.0, 1.0);
        let mut slider = Slider::new(id)
            .value(f)
            .commit_mode(if is_seek {
                CommitMode::OnRelease
            } else {
                CommitMode::Immediate
            })
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
            });
        if is_seek {
            // seek 三阶段事件 → 宿主收口（scrubbing 进/退 + SCRUB_RETARGET 节流 + 松手补发）。
            slider = slider
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
        } else {
            // vol 即时：点/拖都直接 set_volume（无 on_commit）。
            slider = slider
                .on_change({
                    let entity = entity.clone();
                    move |f, w, app| entity.update(app, |this, cx| this.apply_vol(f, w, cx))
                })
                .on_change_during_drag({
                    let entity = entity.clone();
                    move |f, w, app| entity.update(app, |this, cx| this.apply_vol(f, w, cx))
                });
        }
        let strip = div()
            .h(px(16.))
            .relative()
            // debug 只读探针：Slider 用 paint_quad 自绘填充（无元素盒），回归断言靠
            // `debug_bounds("bb-progress-fill")` 读「填充宽 = f×条宽」——此处透明 div 只承载
            // debug_selector（不绘制、无命中盒），几何与 Slider 轨同（4px 居中）。
            .child(
                div()
                    .debug_selector(|| "bb-progress-fill".into())
                    .absolute()
                    .left_0()
                    .top(px(6.))
                    .h(px(4.))
                    .w(DefiniteLength::Fraction(f)),
            )
            .child(slider);
        let strip = match width {
            Some(w) => strip.w(px(w)),
            None => strip.flex_1(),
        };
        if let Some(t) = tip {
            // 提示钮（音量）：条盒即悬停锚点盒（同一 Cell）——提示留在消费方不进组件；
            // 悬停包装层独立 id（`-wrap`），滑块 keyed id 保持 `id` 不变。
            div()
                .id(format!("{id}-wrap"))
                .h(px(16.))
                .on_hover(self.hover_listener(t, bounds))
                .child(strip)
                .into_any_element()
        } else {
            strip.into_any_element()
        }
    }

    /// 点按把焦点交回音乐根：Slider `mouse_down` 已 `stop_propagation`，点条不再冒泡到根
    /// `on_root_mouse_down`（mod.rs 负责聚焦根）——仅当根未持焦（如搜索框持焦后点条）时
    /// space/方向键全局快捷键会失效。此处补回与旧冒泡等价的聚焦路径（只加回焦点一拍，
    /// 不改 Slider 的 stop_propagation）。
    fn focus_music_root(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(root) = self.parent.upgrade() {
            cx.focus_view(&root, window);
        }
    }

    /// seek 点按进拖拽态 + 立即真实 seek（Slider `on_change`；原 `on_strip_down` seek 分支；
    /// 委托 `ScrubState::begin`，scrub.rs 本批 Pattern 2）。
    fn begin_scrub(&mut self, f: f32, window: &mut Window, cx: &mut Context<Self>) {
        // 根聚焦闭包不捕获整个 self（`&mut self.scrub` 与 `&self.player` 需共存）：克隆父弱柄取走。
        let parent = self.parent.clone();
        self.scrub.begin(f, window, cx, &self.player, move |w, cx| {
            // 补回根聚焦（Slider 拦了冒泡；点按即把焦点交回根）。
            if let Some(root) = parent.upgrade() {
                cx.focus_view(&root, w);
            }
        });
    }

    /// seek 拖拽 move：节流真实 seek（Slider `on_change_during_drag`；委托 `ScrubState::drag`）。
    fn drag_scrub(&mut self, f: f32, cx: &mut Context<Self>) {
        self.scrub.drag(f, cx, &self.player);
    }

    /// seek 松手精提交（Slider `on_commit`；委托 `ScrubState::end`）。
    fn end_scrub(&mut self, f: f32, cx: &mut Context<Self>) {
        self.scrub.end(f, cx, &self.player);
    }

    /// vol 即时应用：`set_volume` + 本视图 notify（Slider `on_change`/`on_change_during_drag`
    /// 共用；原 `apply_strip` vol 分支）。
    fn apply_vol(&mut self, f: f32, window: &mut Window, cx: &mut Context<Self>) {
        // 补回根聚焦（Slider 拦了冒泡；点按/拖拽即时把焦点交回根）。
        self.focus_music_root(window, cx);
        self.player.update(cx, |p, cx| p.set_volume(f, cx));
        // 音量视觉同进度：本视图自身 notify，点/拖即时刷新、不寄生动画/tick 帧。
        cx.notify();
    }
}
