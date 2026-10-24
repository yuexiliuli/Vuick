//! 底部播放栏（80px）：左（歌曲信息 + 收藏）/ 中（控制 + 进度）/ 右（歌词/队列/音量/全屏）。
//!
//! 复刻原型 `.bottom-bar`：`280px / 1fr / 240px` 三列；可拖拽进度条（bb_progress）与
//! 音量条（bb_vol）走 `MusicView::strip_element`（记录窗口坐标盒 → seek/set_volume）。

use gpui::{
    AnyElement, Context, CursorStyle, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, StyledImage, Window, div, img, px,
};

use crate::model::RepeatMode;

use super::icon::{IconView, MusicIcon};
use super::{MusicView, RightTab, theme};

/// 底部栏高（px）。
pub(super) const BOTTOM_H: f32 = 80.0;

impl super::MusicView {
    /// 底部播放栏整块。
    pub(super) fn render_bottom(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
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
            .into_any_element()
    }

    /// 左列（280px）：缩略图渐变 + 歌名/艺术家 + 心形。
    fn render_bb_song(
        &mut self,
        title: String,
        artist: String,
        (light, dark): (gpui::Hsla, gpui::Hsla),
        liked: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let cl = cx.listener(|this: &mut MusicView, _ev, _w, cx| {
            this.like_current(cx);
        });
        // 底栏缩略图：有真实封面 → 铺图；无 → 渐变 + 音符回退。
        let thumb_children: Vec<gpui::AnyElement> = if let Some(c) = self.cover_for_current(cx) {
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
                        IconView::new(MusicIcon::Note)
                            .size(px(22.))
                            .text_color(gpui::white().opacity(0.4)),
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
                    .relative()
                    .cursor(CursorStyle::PointingHand)
                    .on_click(cl)
                    .on_hover(self.hover_tip_listener(cx, "bb-like", "收藏"))
                    .child(theme::bounds_canvas(self.tip_box_cell("bb-like")))
                    .child(
                        IconView::new(if liked { MusicIcon::Heart } else { MusicIcon::HeartBorder })
                            .filled(liked)
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
                    .child(self.bb_icon(MusicIcon::Shuffle, 20., shuffle, "bb-shuffle", true, cx))
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
    fn bb_icon(
        &mut self,
        icon: MusicIcon,
        size: f32,
        active: bool,
        id: &'static str,
        toggle: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let tip: Option<&'static str> = match id {
            "bb-shuffle" => Some("随机播放"),
            "bb-prev" => Some("上一首"),
            "bb-next" => Some("下一首"),
            _ => None,
        };
        let cl = cx.listener(move |this: &mut MusicView, _ev, _w, cx| {
            match id {
                "bb-shuffle" => this.toggle_shuffle(cx),
                "bb-prev" => this.player.update(cx, |p, cx| p.prev(cx)),
                "bb-next" => this.player.update(cx, |p, cx| p.next(cx)),
                _ => {
                    if toggle {
                        this.toggle_shuffle(cx);
                    }
                }
            }
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
                IconView::new(icon)
                    .size(px(size))
                    .text_color(if active { theme::accent_light() } else { theme::text2() }),
            );
        if let Some(t) = tip {
            el = el.on_hover(self.hover_tip_listener(cx, id, t));
            el = el.child(theme::bounds_canvas(self.tip_box_cell(id)));
        }
        el.into_any_element()
    }

    /// 中列主播放钮（36px accent 圆）。
    fn bb_main_btn(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let p = self.player.read(cx);
        let playing = p.playing();
        let icon = if playing { MusicIcon::Pause } else { MusicIcon::Play };
        let cl = cx.listener(|this: &mut MusicView, _ev, _w, cx| this.toggle_play(cx));
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
            .on_hover(self.hover_tip_listener(cx, "bb-play", "播放/暂停"))
            .child(theme::bounds_canvas(self.tip_box_cell("bb-play")))
            .child(IconView::new(icon).size(px(20.)).text_color(gpui::white()).filled(true))
            .into_any_element()
    }

    fn bb_repeat(&mut self, repeat: RepeatMode, cx: &mut Context<Self>) -> AnyElement {
        let active = repeat != RepeatMode::Off;
        let cl = cx.listener(|this: &mut MusicView, _ev, _w, cx| this.cycle_repeat(cx));
        div()
            .id("bb-repeat")
            .relative()
            .w(px(20.))
            .h(px(20.))
            .flex()
            .items_center()
            .justify_center()
            .cursor(CursorStyle::PointingHand)
            .on_click(cl)
            .on_hover(self.hover_tip_listener(cx, "bb-repeat", "循环播放"))
            .child(theme::bounds_canvas(self.tip_box_cell("bb-repeat")))
            .child(
                IconView::new(MusicIcon::Repeat)
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
            .child(self.bb_tab_toggle(MusicIcon::Lyrics, RightTab::Lyrics, "bb-lyrics", cx))
            .child(self.bb_tab_toggle(MusicIcon::QueueMusic, RightTab::Queue, "bb-queue", cx))
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
                    .relative()
                    .cursor(CursorStyle::PointingHand)
                    .on_click(cx.listener(|this: &mut MusicView, _ev, _w, cx| this.toggle_fs(cx)))
                    .on_hover(self.hover_tip_listener(cx, "bb-fs", "全屏"))
                    .child(theme::bounds_canvas(self.tip_box_cell("bb-fs")))
                    .child(
                        IconView::new(MusicIcon::Fullscreen)
                            .size(px(18.))
                            .text_color(theme::text2()),
                    ),
            )
            .into_any_element()
    }

    /// 右列歌词/队列 tab 钮：切右侧面板 tab。
    fn bb_tab_toggle(
        &mut self,
        icon: MusicIcon,
        tab: RightTab,
        id: &'static str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let active = self.state.right_tab == tab;
        let tip: &'static str = match tab {
            RightTab::Lyrics => "歌词",
            RightTab::Queue => "播放队列",
            _ => "歌词",
        };
        let cl = cx.listener(move |this: &mut MusicView, _ev, _w, cx| {
            this.set_right_tab(tab, cx);
        });
        div()
            .id(id)
            .relative()
            .cursor(CursorStyle::PointingHand)
            .on_click(cl)
            .on_hover(self.hover_tip_listener(cx, id, tip))
            .child(theme::bounds_canvas(self.tip_box_cell(id)))
            .child(
                IconView::new(icon)
                    .size(px(18.))
                    .text_color(if active { theme::accent_light() } else { theme::text2() }),
            )
            .into_any_element()
    }
}
