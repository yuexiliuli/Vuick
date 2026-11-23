//! 右侧「正在播放」面板（320px）：header（正在播放 + tab）+ 内容 + 「即将播放」。
//!
//! 复刻原型 `.right-panel`：歌曲 tab = 大封面（渐变 + 黑胶圈动画，点击进全屏）+
//! 歌名/艺术家 + 可拖拽进度条 + 控制行（shuffle/prev/play主钮/next/repeat 高亮态）+
//! 附加行（收藏/加入队列/分享）; 歌词 tab = 空态; 队列 tab = 队列逐项（当前项高亮）。

use gpui::{
    AnyElement, Context, CursorStyle, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, Window, div, px,
};

use crate::model::RepeatMode;

use super::icon::{IconView, MusicIcon};
use super::{IntoIconElement, MusicView, RightTab, theme};
use ui::icon::{Icon, IconName};

/// 右侧大封面固定边长（px），黑胶圈按比例从它派生。
pub(super) const ART_PX: f32 = 264.0;

impl super::MusicView {
    /// 右侧面板整块。
    pub(super) fn render_right(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut col: Vec<AnyElement> = vec![self.render_rp_header(cx)];
        match self.state.right_tab {
            RightTab::Song => {
                col.push(self.render_now_playing_song(window, cx));
                col.push(self.render_up_next(cx));
            }
            RightTab::Lyrics => {
                // 右栏常驻渲染 lyrics_right 实例（fs 浮层 absolute inset-0 + `.occlude()` 盖住
                // 即可）：与全屏 lyrics_fs 各持独立 ScrollHandle——双实例双几何，互不钳位。
                col.push(self.lyrics_right.clone().into_any_element());
            }
            RightTab::Queue => {
                col.push(self.render_queue_list(window, cx));
            }
        }
        div()
            .id("music-right")
            .debug_selector(|| "music-right".into())
            .w(px(320.))
            .flex_shrink_0()
            .flex()
            .flex_col()
            .bg(theme::bg_primary())
            .border_l_1()
            .border_color(theme::border())
            .px(px(20.))
            .py(px(20.))
            .overflow_y_scroll()
            .children(col)
            .into_any_element()
    }

    /// header：标题 + 三个 tab 钮（歌曲/歌词/队列）。
    fn render_rp_header(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let tabs = [
            ("rp-tab-song", RightTab::Song, "歌曲"),
            ("rp-tab-lyrics", RightTab::Lyrics, "歌词"),
            ("rp-tab-queue", RightTab::Queue, "队列"),
        ];
        div()
            .flex()
            .items_center()
            .justify_between()
            .pb(px(16.))
            .child(
                div()
                    .text_size(px(16.))
                    .font_weight(gpui::FontWeight::BOLD)
                    .text_color(theme::text())
                    .child("正在播放"),
            )
            .child(
                div()
                    .flex()
                    .gap_x(px(4.))
                    .bg(theme::bg_secondary())
                    .rounded(px(8.))
                    .p(px(3.))
                    .children(tabs.into_iter().map(|(key, tab, label)| {
                        let active = self.state.right_tab == tab;
                        let cl = cx.listener(move |this: &mut MusicView, _ev, _w, cx| {
                            this.set_right_tab(tab, cx);
                        });
                        div()
                            .id(key)
                            .relative()
                            .px(px(10.))
                            .py(px(5.))
                            .rounded(px(6.))
                            .cursor(CursorStyle::PointingHand)
                            .bg(if active { theme::accent() } else { theme::bg_secondary() })
                            .text_size(px(12.))
                            .text_color(if active { theme::text() } else { theme::text2() })
                            .on_click(cl)
                            .on_hover(self.hover_tip_listener(cx, key, label))
                            .child(theme::bounds_canvas(self.tip_box_cell(key)))
                            .child(label.to_string())
                            .into_any_element()
                    })),
            )
            .into_any_element()
    }

    /// 歌曲 tab：大封面 + 信息 + 进度 + 控制 + 附加行。
    fn render_now_playing_song(
        &mut self,
        _window: &mut Window,
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
        let shuffle = p.shuffle();
        let repeat = p.repeat();
        let cur_liked = p.current().map(|i| p.is_liked(i)).unwrap_or(false);

        // 封面渐变（无当前曲目回退 accent→deep）；有真实封面图 → 直接铺图。
        let (light, dark) = grad
            .map(theme::gradient_pair_of)
            .unwrap_or((theme::accent(), theme::bg_deep()));
        let cover = self.cover_for_current(cx);
        let art_base = match &cover {
            Some(_) => self.render_cover_or(cover, div().into_any_element()),
            None => div()
                .size_full()
                .child(theme::gradient_cover((light, dark), 16.0))
                .child(
                    // 黑胶圈 ×3（70/50/30%）：动画在独立环形子视图内、只重绘自己。
                    self.vinyl_np.clone().into_any_element(),
                )
                .child(
                    div()
                        .absolute()
                        .inset_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
                            Icon::new(IconName::Note)
                                .size(px(64.))
                                // 合法保留：压渐变/封面底上的白低透明（0.15），压底需裸白不换 token。
                                .text_color(gpui::white().opacity(0.15)),
                        ),
                )
                .into_any_element(),
        };
        let art = div()
            .id("np-art")
            .w(px(ART_PX))
            .h(px(ART_PX))
            .rounded(px(16.))
            .relative()
            .overflow_hidden()
            .mx_auto()
            .cursor(CursorStyle::PointingHand)
            .child(art_base)
            .on_click(cx.listener(|this: &mut MusicView, _ev, _w, cx| this.toggle_fs(cx)));

        // 进度条 + 当前/总时长时间（np 子视图：自带心跳刷新，拖拽 seek 自管；父稳态不重绘）。
        let progress_n_time = self.np_bar.clone().into_any_element();

        div().flex().flex_col()
            .child(art)
            .child(
                div()
                    .pt(px(16.))
                    .text_size(px(18.))
                    .font_weight(gpui::FontWeight::BOLD)
                    .text_color(theme::text())
                    .text_ellipsis()
                    .child(title),
            )
            .child(
                div()
                    .pt(px(3.))
                    .pb(px(16.))
                    .text_size(px(13.))
                    .text_color(theme::text2())
                    .text_ellipsis()
                    .child(artist),
            )
            .child(progress_n_time)
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .gap_x(px(18.))
                    .pb(px(18.))
                    .child(self.np_control(IconName::Shuffle, 22., shuffle, "随机播放", cx))
                    .child(self.np_control(MusicIcon::Prev, 24., false, "上一首", cx))
                    .child(self.np_play_main(cx))
                    .child(self.np_control(MusicIcon::Next, 24., false, "下一首", cx))
                    .child(self.np_repeat(repeat, cx)),
            )
            .child(
                div()
                    .flex()
                    .justify_between()
                    .pb(px(16.))
                    .child(
                        self.extra_btn(
                            if cur_liked { IconName::Heart } else { IconName::HeartBorder },
                            "收藏",
                            cur_liked,
                            "right-like",
                            cx,
                        ),
                    )
                    .child(self.extra_btn(IconName::QueueMusic, "加入队列", false, "right-que", cx))
                    .child(self.extra_btn(IconName::Share, "分享", false, "right-share", cx)),
            )
            .into_any_element()
    }

    /// 小控制钮（shuffle/prev/next 等）：hover 前色；active 高亮 accent。
    fn np_control<I: IntoIconElement>(
        &mut self,
        icon: I,
        size: f32,
        active: bool,
        label: &'static str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let cl = cx.listener(move |this: &mut MusicView, _ev, _w, cx| {
            match label {
                "随机播放" => this.toggle_shuffle(cx),
                "上一首" => this.player.update(cx, |p, cx| p.prev(cx)),
                "下一首" => this.player.update(cx, |p, cx| p.next(cx)),
                _ => this.toast_msg(label, cx),
            }
        });
        div()
            .id(format!("np-{label}"))
            .w(px(size))
            .h(px(size))
            .flex()
            .items_center()
            .justify_center()
            .cursor(CursorStyle::PointingHand)
            .text_color(if active { theme::accent_light() } else { theme::text2() })
            .on_click(cl)
            .child(icon.icon_element(px(size - 2.), if active {
                theme::accent_light()
            } else {
                theme::text2()
            }))
            .into_any_element()
    }

    /// 播放/暂停主钮（48px accent 圆 + 阴影）。
    fn np_play_main(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let p = self.player.read(cx);
        let playing = p.playing();
        let icon = if playing { MusicIcon::Pause } else { MusicIcon::Play };
        let cl = cx.listener(|this: &mut MusicView, _ev, _w, cx| this.toggle_play(cx));
        div()
            .id("np-play")
            .w(px(48.))
            .h(px(48.))
            .rounded(px(24.))
            .flex()
            .items_center()
            .justify_center()
            .bg(theme::accent())
            .cursor(CursorStyle::PointingHand)
            .on_click(cl)
            .child(
                IconView::new(icon)
                    .size(px(26.))
                    .text_color(theme::text()),
            )
            .into_any_element()
    }

    /// repeat 钮：Off/All/One 循环。
    fn np_repeat(&mut self, repeat: RepeatMode, cx: &mut Context<Self>) -> AnyElement {
        let active = repeat != RepeatMode::Off;
        let cl = cx.listener(|this: &mut MusicView, _ev, _w, cx| this.cycle_repeat(cx));
        div()
            .id("np-repeat")
            .w(px(22.))
            .h(px(22.))
            .flex()
            .items_center()
            .justify_center()
            .cursor(CursorStyle::PointingHand)
            .text_color(if active { theme::accent_light() } else { theme::text2() })
            .on_click(cl)
            .child(
                Icon::new(IconName::Repeat)
                    .size(px(20.))
                    .text_color(if active { theme::accent_light() } else { theme::text2() }),
            )
            .into_any_element()
    }

    /// 附加钮（收藏/加入队列/分享），供 right / fullscreen 复用。
    pub(super) fn extra_btn(
        &mut self,
        icon: IconName,
        label: &'static str,
        active: bool,
        id: &'static str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let cl = cx.listener(move |this: &mut MusicView, _ev, _w, cx| {
            match label {
                "收藏" => this.like_current(cx),
                "加入队列" => this.add_to_queue_current(cx),
                "分享" => this.share_current(_w, cx),
                _ => {}
            }
        });
        div()
            .id(id)
            .relative()
            .debug_selector(move || id.into())
            .flex()
            .items_center()
            .gap_x(px(5.))
            .cursor(CursorStyle::PointingHand)
            .text_size(px(12.))
            .text_color(if active { theme::pink() } else { theme::text3() })
            .on_click(cl)
            .on_hover(self.hover_tip_listener(cx, id, label))
            .child(theme::bounds_canvas(self.tip_box_cell(id)))
            .child(
                Icon::new(icon)
                    .size(px(15.))
                    .text_color(if active { theme::pink() } else { theme::text3() }),
            )
            .child(label.to_string())
            .into_any_element()
    }

    /// 队列 tab：当前队列逐项（当前项高亮 + 播放动画）。
    fn render_queue_list(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let p = self.player.read(cx);
        let queue = p.queue().to_vec();
        let qpos = p.queue_pos();
        let mut rows: Vec<AnyElement> = Vec::new();
        for (qi, &idx) in queue.iter().enumerate() {
            let Some(t) = p.library().get(idx) else { continue };
            let is_now = qi == qpos;
            let (light, dark) = theme::gradient_pair_of(t.grad);
            let cl = cx.listener(move |this: &mut MusicView, _ev, _w, cx| {
                this.handle_play_idx(idx, cx);
            });
            rows.push(
                div()
                    .id(format!("queue-item-{qi}"))
                    .flex()
                    .items_center()
                    .gap_x(px(10.))
                    .px(px(10.))
                    .py(px(8.))
                    .rounded(px(8.))
                    .cursor(CursorStyle::PointingHand)
                    .bg(if is_now { theme::accent().opacity(0.12) } else { theme::bg_card().opacity(0.0) })
                    .on_click(cl)
                    .child(
                        div()
                            .w(px(40.))
                            .h(px(40.))
                            .rounded(px(6.))
                            .relative()
                            .flex_shrink_0()
                            .overflow_hidden()
                            .child(theme::gradient_cover((light, dark), 6.0))
                            .child(
                                div()
                                    .absolute()
                                    .inset_0()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(
                                        Icon::new(IconName::Note)
                                            .size(px(17.))
                                            .text_color(theme::white_fade_40()),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .child(
                                div()
                                    .text_size(px(13.))
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(if is_now {
                                        theme::accent_light()
                                    } else {
                                        theme::text()
                                    })
                                    .text_ellipsis()
                                    .child(t.title.clone()),
                            )
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(theme::text3())
                                    .text_ellipsis()
                                    .child(t.artist.clone()),
                            ),
                    )
                    .child(if is_now {
                        self.eq_queue.clone().into_any_element()
                    } else {
                        div()
                            .text_size(px(11.))
                            .text_color(theme::text3())
                            .child(crate::model::duration_str(t.duration))
                            .into_any_element()
                    })
                    .into_any_element(),
            );
        }
        if rows.is_empty() {
            rows.push(
                div()
                    .pt(px(30.))
                    .text_size(px(13.))
                    .text_color(theme::text3())
                    .child("队列为空")
                    .into_any_element(),
            );
        }
        div().flex().flex_col().children(rows).into_any_element()
    }

    /// 「即将播放」：队列剩余前 3 项（歌曲 tab 底部）。
    fn render_up_next(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let p = self.player.read(cx);
        let queue = p.queue().to_vec();
        let qpos = p.queue_pos();
        let next = queue
            .iter()
            .enumerate()
            .filter(|(qi, _)| *qi > qpos)
            .take(3)
            .map(|(_, &idx)| idx)
            .collect::<Vec<usize>>();
        let mut rows: Vec<AnyElement> = Vec::new();
        for &idx in &next {
            let Some(t) = p.library().get(idx) else { continue };
            let (light, dark) = theme::gradient_pair_of(t.grad);
            let cl = cx.listener(move |this: &mut MusicView, _ev, _w, cx| {
                this.handle_play_idx(idx, cx);
            });
            rows.push(
                div()
                    .id(format!("up-next-{idx}"))
                    .flex()
                    .items_center()
                    .gap_x(px(10.))
                    .py(px(6.))
                    .cursor(CursorStyle::PointingHand)
                    .on_click(cl)
                    .child(
                        div()
                            .w(px(36.))
                            .h(px(36.))
                            .rounded(px(6.))
                            .relative()
                            .flex_shrink_0()
                            .overflow_hidden()
                            .child(theme::gradient_cover((light, dark), 6.0))
                            .child(
                                div()
                                    .absolute()
                                    .inset_0()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(
                                        Icon::new(IconName::Note)
                                            .size(px(15.))
                                            .text_color(theme::white_fade_40()),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .child(
                                div()
                                    .text_size(px(13.))
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(theme::text())
                                    .text_ellipsis()
                                    .child(t.title.clone()),
                            )
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(theme::text3())
                                    .text_ellipsis()
                                    .child(t.artist.clone()),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(theme::text3())
                            .child(crate::model::duration_str(t.duration)),
                    )
                    .into_any_element(),
            );
        }
        div().flex().flex_col().mt_auto()
            .child(
                div()
                    .pt(px(18.))
                    .pb(px(8.))
                    .text_size(px(14.))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme::text())
                    .child("即将播放"),
            )
            .children(rows)
            .into_any_element()
    }
}
