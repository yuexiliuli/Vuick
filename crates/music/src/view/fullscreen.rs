//! 全屏播放浮层：`fs_open` 时渲染为根视图**最后子元素**（absolute inset-0，绘制/命中
//! 最上层，盖住三栏 + 底栏）。复刻原型 `.fullscreen-player`：
//! 动态背景（GPUI 无 radial-gradient，用两层大圆半透明光晕近似）、顶部工具条、
//! 主体（左 380 封面 + 黑胶圈动画 / 右 tab 控制·歌词·队列）、底部 eq 渐变装饰条、
//! 底部快捷键提示。Esc 关全屏由根 `on_key` 处理，close 钮也关。

use gpui::{
    AnyElement, Context, CursorStyle, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, Window, div, px,
};

use crate::model::RepeatMode;

use super::icon::{IconView, MusicIcon};
use super::{FsTab, MusicView, theme, widgets};

/// 全屏封面边长（px），黑胶圈按比例派生。
const FS_ART_PX: f32 = 380.0;

impl super::MusicView {
    /// 全屏浮层整块。
    pub(super) fn render_fullscreen(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = self.player.read(cx);
        let playing = p.playing();
        let title = p
            .current_track()
            .map(|t| t.title.clone())
            .unwrap_or_else(|| "未在播放".to_string());
        let artist = p
            .current_track()
            .map(|t| t.artist.clone())
            .unwrap_or_else(|| "—".to_string());
        let album = p
            .current_track()
            .map(|t| t.album.clone())
            .unwrap_or_else(|| "".to_string());
        let grad = p.current_track().map(|t| t.grad);
        let (light, dark) = grad
            .map(theme::gradient_pair_of)
            .unwrap_or((theme::accent(), theme::bg_deep()));

        div()
            .id("music-fs")
            .debug_selector(|| "music-fs".into())
            .absolute()
            .inset_0()
            .relative()
            .overflow_hidden()
            .flex()
            .flex_col()
            .bg(theme::bg_deep())
            .child(self.render_fs_glow())
            .child(self.render_fs_topbar(cx))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_center()
                    .gap_x(px(64.))
                    .flex_1()
                    .min_h(px(0.))
                    .px(px(48.))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .items_center()
                            .child(self.render_fs_art((light, dark), playing))
                            .child(self.render_fs_song_info(title, artist, album)),
                    )
                    .child(self.render_fs_control_section(window, cx)),
            )
            .child(self.render_fs_bottom_eq(playing))
            .child(self.render_fs_shortcuts())
            .into_any_element()
    }

    /// 动态背景光晕（两层大圆半透明，近似原型的 radial-gradient 光感）。
    fn render_fs_glow(&mut self) -> AnyElement {
        div()
            .absolute()
            .inset_0()
            .child(
                div()
                    .absolute()
                    .w(px(700.))
                    .h(px(700.))
                    .left(px(-120.))
                    .top(px(-180.))
                    .rounded(px(350.))
                    .bg(theme::accent().opacity(0.16)),
            )
            .child(
                div()
                    .absolute()
                    .w(px(600.))
                    .h(px(600.))
                    .right(px(-140.))
                    .bottom(px(-160.))
                    .rounded(px(300.))
                    .bg(theme::pink().opacity(0.12)),
            )
            .child(
                div()
                    .absolute()
                    .w(px(500.))
                    .h(px(500.))
                    .left(px(60.))
                    .bottom(px(-120.))
                    .rounded(px(250.))
                    .bg(theme::green().opacity(0.05)),
            )
            .into_any_element()
    }

    /// 顶部工具条：左 playlist_add/share，中「正在播放」，右 badge/more/close。
    fn render_fs_topbar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let left = [
            (MusicIcon::PlaylistAdd, "添加到播放列表"),
            (MusicIcon::Share, "分享链接已复制"),
        ];
        let right = [
            (MusicIcon::Badge, "画质：高品质"),
            (MusicIcon::More, "更多选项"),
        ];
        div()
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .flex()
            .items_center()
            .justify_between()
            .px(px(30.))
            .py(px(18.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_x(px(12.))
                    .children(left.into_iter().map(|(icon, tip)| {
                        self.fs_icon_btn(icon, tip, cx)
                    })),
            )
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(theme::text3())
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child("正在播放"),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_x(px(12.))
                    .children(right.into_iter().map(|(icon, tip)| {
                        self.fs_icon_btn(icon, tip, cx)
                    }))
                    .child(self.fs_close_btn(cx)),
            )
            .into_any_element()
    }

    /// 工具条圆钮（40px）：点击 toast 提示。
    fn fs_icon_btn(
        &mut self,
        icon: MusicIcon,
        tip: &'static str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let cl = cx.listener(move |this: &mut MusicView, _ev, _w, _cx| this.toast_msg(tip));
        div()
            .id(format!("fs-btn-{tip}"))
            .w(px(40.))
            .h(px(40.))
            .rounded(px(20.))
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui::white().opacity(0.06))
            .border_1()
            .border_color(gpui::white().opacity(0.08))
            .cursor(CursorStyle::PointingHand)
            .on_click(cl)
            .child(IconView::new(icon).size(px(19.)).text_color(theme::text2()))
            .into_any_element()
    }

    /// 关闭全屏钮（红 hover 语义：直接关 fs）。
    fn fs_close_btn(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let cl = cx.listener(|this: &mut MusicView, _ev, _w, cx| {
            this.state.fs_open = false;
            cx.notify();
        });
        div()
            .id("fs-close")
            .w(px(40.))
            .h(px(40.))
            .rounded(px(20.))
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui::white().opacity(0.08))
            .border_1()
            .border_color(gpui::white().opacity(0.12))
            .cursor(CursorStyle::PointingHand)
            .on_click(cl)
            .child(
                IconView::new(MusicIcon::FullscreenExit)
                    .size(px(19.))
                    .text_color(theme::text2()),
            )
            .into_any_element()
    }

    /// 左列：大封面（渐变 + 黑胶圈旋转动画）+ 音符占位。
    fn render_fs_art(&mut self, (light, dark): (gpui::Hsla, gpui::Hsla), playing: bool) -> AnyElement {
        div()
            .w(px(FS_ART_PX))
            .h(px(FS_ART_PX))
            .rounded(px(20.))
            .relative()
            .overflow_hidden()
            .child(theme::gradient_cover((light, dark), 20.0))
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(widgets::vinyl_ring(
                        FS_ART_PX * 0.80,
                        playing,
                        "fs-vinyl-1".into(),
                    ))
                    .child(widgets::vinyl_ring(
                        FS_ART_PX * 0.55,
                        playing,
                        "fs-vinyl-2".into(),
                    ))
                    .child(widgets::vinyl_ring(
                        FS_ART_PX * 0.30,
                        playing,
                        "fs-vinyl-3".into(),
                    )),
            )
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        IconView::new(MusicIcon::Note)
                            .size(px(88.))
                            .text_color(gpui::white().opacity(0.15)),
                    ),
            )
            .into_any_element()
    }

    /// 封面下方歌曲信息：渐变标题 + 艺术家 + 专辑。
    fn render_fs_song_info(
        &mut self,
        title: String,
        artist: String,
        album: String,
    ) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .items_center()
            .pt(px(24.))
            .child(
                div()
                    .text_size(px(28.))
                    .font_weight(gpui::FontWeight::BOLD)
                    .text_color(theme::accent_light())
                    .text_ellipsis()
                    .child(title),
            )
            .child(
                div()
                    .pt(px(6.))
                    .text_size(px(16.))
                    .text_color(theme::text2())
                    .child(artist),
            )
            .child(
                div()
                    .pt(px(4.))
                    .text_size(px(13.))
                    .text_color(theme::text3())
                    .child(if album.is_empty() { "—".to_string() } else { album }),
            )
            .into_any_element()
    }

    /// 右列：scripts tabs + 内容。
    fn render_fs_control_section(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let fs_tab = self.state.fs_tab;
        div()
            .w(px(400.))
            .flex_shrink_0()
            .flex()
            .flex_col()
            .justify_center()
            .min_h(px(0.))
            .child(self.render_fs_tabs(cx))
            .child(self.render_fs_tab_content(window, cx, fs_tab))
            .into_any_element()
    }

    /// Tab 条（控制/歌词/队列）：active accent 底。
    fn render_fs_tabs(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let tabs = [
            (FsTab::Controls, "控制"),
            (FsTab::Lyrics, "歌词"),
            (FsTab::Queue, "队列"),
        ];
        div()
            .flex()
            .gap_x(px(4.))
            .bg(gpui::white().opacity(0.05))
            .rounded(px(10.))
            .p(px(4.))
            .mb(px(24.))
            .children(tabs.into_iter().map(|(tab, label)| {
                let active = self.state.fs_tab == tab;
                let cl = cx.listener(move |this: &mut MusicView, _ev, _w, cx| {
                    this.set_fs_tab(tab, cx);
                });
                div()
                    .id(format!("fs-tab-{label}"))
                    .flex_1()
                    .py(px(8.))
                    .rounded(px(8.))
                    .cursor(CursorStyle::PointingHand)
                    .bg(if active { theme::accent() } else { gpui::transparent_black() })
                    .text_size(px(13.))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(if active { gpui::white() } else { theme::text3() })
                    .text_align(gpui::TextAlign::Center)
                    .on_click(cl)
                    .child(label.to_string())
                    .into_any_element()
            }))
            .into_any_element()
    }

    fn render_fs_tab_content(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        tab: FsTab,
    ) -> AnyElement {
        match tab {
            FsTab::Controls => self.render_fs_controls(window, cx),
            FsTab::Lyrics => self
                .div_lyrics_empty()
                .into_any_element(),
            FsTab::Queue => self.render_fs_queue(window, cx),
        }
    }

    /// 控制 tab：进度 + 控制行 + 附加行 + 音量。
    fn render_fs_controls(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = self.player.read(cx);
        let dur = p.duration();
        let pos = p.position();
        let volume = p.volume();
        let shuffle = p.shuffle();
        let repeat = p.repeat();
        let cur_liked = p.current().map(|i| p.is_liked(i)).unwrap_or(false);
        let frac = if dur > 0.0 { (pos / dur).clamp(0.0, 1.0) as f32 } else { 0.0 };
        let progress = self.strip_element(
            "fs-progress",
            self.fs_progress.clone(),
            frac,
            true,
            None,
            cx,
        );
        let vol_strip = self.strip_element(
            "fs-vol",
            self.fs_vol.clone(),
            volume,
            false,
            Some(160.),
            cx,
        );
        div().flex().flex_col()
            .child(progress)
            .child(
                div()
                    .flex()
                    .justify_between()
                    .mt(px(6.))
                    .mb(px(22.))
                    .text_size(px(12.))
                    .text_color(theme::text3())
                    .child(crate::model::duration_str(pos))
                    .child(crate::model::duration_str(dur)),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .gap_x(px(24.))
                    .pb(px(24.))
                    .child(self.fs_ctrl(MusicIcon::Shuffle, 28., shuffle, "fs-shuffle", cx))
                    .child(self.fs_ctrl(MusicIcon::Prev, 28., false, "fs-prev", cx))
                    .child(self.fs_play_btn(cx))
                    .child(self.fs_ctrl(MusicIcon::Next, 28., false, "fs-next", cx))
                    .child(self.fs_repeat(repeat, cx)),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .gap_x(px(28.))
                    .pb(px(22.))
                    .child(self.extra_btn(MusicIcon::Heart, "收藏", cur_liked, "fs-like", cx))
                    .child(self.extra_btn(MusicIcon::QueueMusic, "队列", false, "fs-que", cx))
                    .child(self.extra_btn(MusicIcon::Download, "下载", false, "fs-dl", cx)),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .gap_x(px(10.))
                    .child(
                        IconView::new(MusicIcon::Volume)
                            .size(px(20.))
                            .text_color(theme::text2()),
                    )
                    .child(vol_strip),
            )
            .into_any_element()
    }

    /// fs 普通控制钮（shuffle/prev/next）。
    fn fs_ctrl(
        &mut self,
        icon: MusicIcon,
        size: f32,
        active: bool,
        id: &'static str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let cl = cx.listener(move |this: &mut MusicView, _ev, _w, cx| {
            match id {
                "fs-shuffle" => this.toggle_shuffle(cx),
                "fs-prev" => this.player.update(cx, |p, cx| p.prev(cx)),
                "fs-next" => this.player.update(cx, |p, cx| p.next(cx)),
                _ => {}
            }
        });
        div()
            .id(id)
            .w(px(size))
            .h(px(size))
            .flex()
            .items_center()
            .justify_center()
            .cursor(CursorStyle::PointingHand)
            .on_click(cl)
            .child(
                IconView::new(icon)
                    .size(px(size))
                    .text_color(if active { theme::accent_light() } else { theme::text2() }),
            )
            .into_any_element()
    }

    /// fs 主播放钮（64px accent 渐变圆）。
    fn fs_play_btn(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let p = self.player.read(cx);
        let playing = p.playing();
        let icon = if playing { MusicIcon::Pause } else { MusicIcon::Play };
        let cl = cx.listener(|this: &mut MusicView, _ev, _w, cx| this.toggle_play(cx));
        div()
            .id("fs-play")
            .w(px(64.))
            .h(px(64.))
            .rounded(px(32.))
            .flex()
            .items_center()
            .justify_center()
            .bg(theme::accent())
            .cursor(CursorStyle::PointingHand)
            .on_click(cl)
            .child(IconView::new(icon).size(px(30.)).text_color(gpui::white()).filled(true))
            .into_any_element()
    }

    fn fs_repeat(&mut self, repeat: RepeatMode, cx: &mut Context<Self>) -> AnyElement {
        let active = repeat != RepeatMode::Off;
        let cl = cx.listener(|this: &mut MusicView, _ev, _w, cx| this.cycle_repeat(cx));
        div()
            .id("fs-repeat")
            .w(px(28.))
            .h(px(28.))
            .flex()
            .items_center()
            .justify_center()
            .cursor(CursorStyle::PointingHand)
            .on_click(cl)
            .child(
                IconView::new(MusicIcon::Repeat)
                    .size(px(26.))
                    .text_color(if active { theme::accent_light() } else { theme::text2() }),
            )
            .into_any_element()
    }

    /// 歌词 tab 空态（fs 内蒙版）。
    fn div_lyrics_empty(&mut self) -> gpui::Div {
        div()
            .flex_1()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .text_color(theme::text3())
            .child("暂无歌词")
    }

    /// 队列 tab：fs 内队列逐项。
    fn render_fs_queue(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let _ = window;
        let p = self.player.read(cx);
        let queue = p.queue().to_vec();
        let qpos = p.queue_pos();
        let playing = p.playing();
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
                    .id(format!("fs-queue-{qi}"))
                    .flex()
                    .items_center()
                    .gap_x(px(12.))
                    .px(px(10.))
                    .py(px(9.))
                    .rounded(px(8.))
                    .cursor(CursorStyle::PointingHand)
                    .bg(if is_now {
                        theme::accent().opacity(0.12)
                    } else {
                        theme::bg_card().opacity(0.0)
                    })
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
                                        IconView::new(MusicIcon::Note)
                                            .size(px(17.))
                                            .text_color(gpui::white().opacity(0.4)),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .child(
                                div()
                                    .text_size(px(14.))
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
                                    .text_size(px(12.))
                                    .text_color(theme::text3())
                                    .text_ellipsis()
                                    .child(t.artist.clone()),
                            ),
                    )
                    .child(if is_now {
                        widgets::eq_bars(playing, theme::accent(), format!("fsq-{qi}"))
                    } else {
                        div()
                            .text_size(px(12.))
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
                    .pt(px(20.))
                    .pb(px(40.))
                    .text_size(px(13.))
                    .text_color(theme::text3())
                    .child("队列为空")
                    .into_any_element(),
            );
        }
        div().flex().flex_col().children(rows).into_any_element()
    }

    /// 底部 eq 渐变装饰条（canvas 2-stop 水平渐变，播放中明亮）。
    fn render_fs_bottom_eq(&mut self, playing: bool) -> AnyElement {
        let alpha = if playing { 0.6 } else { 0.2 };
        let body = theme::gradient_cover(
            (
                theme::pink().opacity(alpha),
                theme::accent().opacity(alpha),
            ),
            1.5,
        );
        div()
            .absolute()
            .bottom_0()
            .left_0()
            .right_0()
            .h(px(3.))
            .child(body)
            .into_any_element()
    }

    /// 底部快捷键提示行。
    fn render_fs_shortcuts(&mut self) -> AnyElement {
        let keys = [
            ("Esc", "退出全屏"),
            ("Space", "播放/暂停"),
            ("←", "上一首"),
            ("→", "下一首"),
        ];
        div()
            .absolute()
            .bottom(px(16.))
            .left_0()
            .right_0()
            .flex()
            .items_center()
            .justify_center()
            .gap_x(px(20.))
            .opacity(0.5)
            .children(keys.into_iter().map(|(key, label)| {
                div()
                    .flex()
                    .items_center()
                    .gap_x(px(4.))
                    .text_size(px(11.))
                    .text_color(theme::text3())
                    .child(
                        div()
                            .px(px(5.))
                            .h(px(20.))
                            .flex()
                            .items_center()
                            .border_1()
                            .border_color(gpui::white().opacity(0.12))
                            .rounded(px(4.))
                            .text_size(px(10.))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .bg(gpui::white().opacity(0.04))
                            .text_color(theme::text3())
                            .child(key.to_string()),
                    )
                    .child(label.to_string())
                    .into_any_element()
            }))
            .into_any_element()
    }
}
