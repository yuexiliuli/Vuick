//! 全屏播放浮层（v2 复刻）：`fs_open` 时渲染为根视图**最后子元素**（absolute inset-0，
//! 绘制/命中最上层，盖住三栏 + 底栏），并 `.occlude()`（`HitboxBehavior::BlockMouse`）
//! ——gpui 命中栈仅对 BlockMouse break，然则盖住的下层元素（侧栏歌单/导航行等的
//! `on_click`）在点击 fs 进度条等区域时会一并触发（真机复报：点 fs 进度条弹「打开歌单」）；
//! occlude 整层拦截后，fs 子元素（条/按钮/歌词滚动）仍在栈内正常交互。原型
//! `prototype-orbitrum-player-v2.html` `.fullscreen-player`，相对 v1 的结构改动：
//! - 移除右侧「控制/歌词/队列」tab，右列改为**纯歌词面板**；
//! - 全部播放控制并入左列：封面(320)→歌曲信息→进度→控制行→音量+操作；
//! - 顶栏中心 "NOW PLAYING"；封面双层阴影；黑胶三圈不同周期/转向；进度条悬停 5→7px +
//!   亮色圆点；暂停时黑胶圈/装饰条变暗。
//! GPUI 无 CSS `radial-gradient`/`letter-spacing`：背景光晕用两个大圆半透明层近似、
//! 文字间距忽略；黑胶圈用 svg 圆环 + 旋转动画（暂停即停）。Esc 关全屏由根 `on_key`
//! 处理，close 钮也关。

use std::time::Duration;

use gpui::{
    AnyElement, Context, CursorStyle, DefiniteLength, Hsla, InteractiveElement as _,
    IntoElement, MouseButton, ParentElement as _, ScrollWheelEvent, StatefulInteractiveElement as _,
    Styled as _, Window, div, point, px,
};

use crate::model::{RepeatMode, duration_str};

use super::icon::{IconView, MusicIcon};
use super::{MusicView, theme};

/// 全屏封面边长（px），黑胶圈按比例派生。
pub(super) const FS_COVER_PX: f32 = 320.0;

/// 歌词行三态（v2 `.fs-lyric`：active 白+加粗+accent 底 / past 暗 / future 更亮）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum LyricState {
    Active,
    Past,
    Future,
}

/// 右侧「歌词」tab 是否渲染真实歌词面板（纯函数，供 `render_right` 与测试共用）：
/// `fs_open` 时全屏浮层已渲染歌词 → 右侧改静态占位，保证树上**单实例**——否则两个
/// `render_lyrics_panel_content` 会争夺同一个 `lyrics_scroll` ScrollHandle（两套几何
/// 每帧交替钳位同一共享 offset → 全屏到底后上下跳动 + 闪，真机复报三）。
pub(super) fn right_panel_live_lyrics(fs_open: bool) -> bool {
    !fs_open
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

/// 歌词固定行高（px）：滚动量程反算与行窗物化都按它——三态行统一高，便于按偏移/视口
/// 精确反算行号与总高（内容总高 = 行数 × 行高）。
pub(super) const LYRIC_ROW_H: f32 = 46.0;
/// 手动滚动后自动恢复跟随的「停止滚动」时长（ms）。
pub(super) const LYRIC_FOLLOW_RESUME_MS: u64 = 3000;
/// 歌词滚动容器尚未布局前（bounds=0）兜底可见行数：估一行 46px、~11 行视口。
const LYRIC_FALLBACK_VISIBLE: usize = 11;
/// 可视行窗上下额外物化的缓冲行数（滚动时行间无缝衔接，防白缝）。
const LYRIC_ROW_BUFFER: usize = 2;

impl super::MusicView {
    /// 全屏浮层整块（v2：左列封面+控制 / 右列歌词）。
    pub(super) fn render_fullscreen(
        &mut self,
        _window: &mut Window,
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
        let album = p.current_track().map(|t| t.album.clone()).unwrap_or_default();
        let grad = p.current_track().map(|t| t.grad);
        let (light, dark) = grad
            .map(theme::gradient_pair_of)
            .unwrap_or((theme::accent(), theme::bg_deep()));

        div()
            .id("music-fs")
            .debug_selector(|| "music-fs".into())
            .occlude()
            .absolute()
            .inset_0()
            .overflow_hidden()
            .flex()
            .flex_col()
            .bg(theme::bg_deep())
            .child(self.render_fs_bg())
            .child(self.render_fs_topbar(cx))
            .child(self.render_fs_body(cx, title, artist, album, light, dark))
            .child(self.render_fs_bottom_eq(playing))
            .into_any_element()
    }

    /// 动态背景光晕（v2 `.fs-bg`）：accent 大圆（左上 25%,30%）+ pink 大圆（右下 75%,70%），
    /// 半透明大圆近似 radial-gradient 光感；GPUI 无径向渐变故不追求精确。
    fn render_fs_bg(&mut self) -> AnyElement {
        div()
            .absolute()
            .inset_0()
            .child(
                div()
                    .absolute()
                    .w(px(760.))
                    .h(px(760.))
                    .left(px(-140.))
                    .top(px(-220.))
                    .rounded(px(380.))
                    .bg(theme::accent().opacity(0.16)),
            )
            .child(
                div()
                    .absolute()
                    .w(px(640.))
                    .h(px(640.))
                    .right(px(-160.))
                    .bottom(px(-240.))
                    .rounded(px(320.))
                    .bg(theme::pink().opacity(0.12)),
            )
            .into_any_element()
    }

    /// 顶部工具条（v2）：左 playlist_add/share，中 "NOW PLAYING"，右 high_quality/close。
    fn render_fs_topbar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .flex()
            .items_center()
            .justify_between()
            .px(px(32.))
            .py(px(20.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_x(px(12.))
                    .child(self.fs_icon_btn(MusicIcon::PlaylistAdd, "已添加到播放列表", cx))
                    .child(self.fs_icon_btn(MusicIcon::Share, "分享链接已复制", cx)),
            )
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(theme::text3())
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child("NOW PLAYING"),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_x(px(12.))
                    .child(self.fs_icon_btn(MusicIcon::Badge, "高音质", cx))
                    .child(self.fs_close_btn(cx)),
            )
            .into_any_element()
    }

    /// 工具条圆钮（40px，v2 悬停变亮）。
    fn fs_icon_btn(
        &mut self,
        icon: MusicIcon,
        tip: &'static str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let hovered = self.fs_hover == Some(tip);
        let cl = cx.listener(move |this: &mut MusicView, _ev, _w, _cx| this.toast_msg(tip));
        div()
            .id(format!("fs-btn-{tip}"))
            .relative()
            .w(px(40.))
            .h(px(40.))
            .rounded(px(20.))
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui::white().opacity(if hovered { 0.12 } else { 0.06 }))
            .border_1()
            .border_color(gpui::white().opacity(0.08))
            .cursor(CursorStyle::PointingHand)
            .on_click(cl)
            .on_hover(cx.listener(move |this: &mut MusicView, hovered, _w, cx| {
                // 复用现有 toast 文案作悬停提示（id 与 tip 同名：fs-btn-{tip}）；
                // 盒按 id（=tip）记入 tip_boxes，供 set_fs_hover_and_tip 锚定按钮盒。
                this.set_fs_hover_and_tip(tip, Some(tip), *hovered, cx);
            }))
            .child(theme::bounds_canvas(self.tip_box_cell(tip)))
            .child(
                IconView::new(icon)
                    .size(px(20.))
                    .text_color(if hovered { theme::text() } else { theme::text2() }),
            )
            .into_any_element()
    }

    /// 关闭全屏钮（红 hover 语义：直接关 fs）。
    fn fs_close_btn(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let hovered = self.fs_hover == Some("fs-close");
        let cl = cx.listener(|this: &mut MusicView, _ev, _w, cx| {
            this.state.fs_open = false;
            cx.notify();
        });
        div()
            .id("fs-close")
            .relative()
            .w(px(40.))
            .h(px(40.))
            .rounded(px(20.))
            .flex()
            .items_center()
            .justify_center()
            .bg(if hovered {
                gpui::hsla(5., 0.6, 0.55, 0.22)
            } else {
                gpui::white().opacity(0.08)
            })
            .border_1()
            .border_color(if hovered {
                gpui::hsla(5., 0.6, 0.6, 0.3)
            } else {
                gpui::white().opacity(0.12)
            })
            .cursor(CursorStyle::PointingHand)
            .on_click(cl)
            .on_hover(cx.listener(|this: &mut MusicView, hovered, _w, cx| {
                this.set_fs_hover_and_tip("fs-close", Some("退出全屏"), *hovered, cx);
            }))
            .child(theme::bounds_canvas(self.tip_box_cell("fs-close")))
            .child(
                IconView::new(MusicIcon::FullscreenExit)
                    .size(px(20.))
                    .text_color(if hovered {
                        gpui::hsla(5., 0.7, 0.7, 1.)
                    } else {
                        theme::text2()
                    }),
            )
            .into_any_element()
    }

    /// 主体两栏（v2 `.fs-body`：左列封面+控制，右列歌词）。
    fn render_fs_body(
        &mut self,
        cx: &mut Context<Self>,
        title: String,
        artist: String,
        album: String,
        light: Hsla,
        dark: Hsla,
    ) -> AnyElement {
        div()
            .relative()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .items_center()
            .justify_center()
            .gap_x(px(80.))
            .px(px(56.))
            .py(px(48.))
            .child(self.render_fs_left(cx, title, artist, album, light, dark))
            .child(self.render_fs_lyrics(cx))
            .into_any_element()
    }

    /// 左列：封面 → 歌曲信息 → 进度 → 控制 → 音量+操作。
    fn render_fs_left(
        &mut self,
        cx: &mut Context<Self>,
        title: String,
        artist: String,
        album: String,
        light: Hsla,
        dark: Hsla,
    ) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .items_center()
            .w(px(FS_COVER_PX))
            .flex_shrink_0()
            .child(self.render_fs_cover(cx, (light, dark)))
            .child(self.render_fs_song_meta(title, artist, album))
            .child(self.render_fs_progress(cx))
            .child(self.render_fs_controls(cx))
            .child(self.render_fs_bottom_row(cx))
            .into_any_element()
    }

    /// 封面 320：有真实封面图 → 直接铺图（`ObjectFit::Cover`）；无 → 渐变底 + 黑胶三圈
    /// + 音符回退（v2 骨架）。双层阴影（压暗 + accent 光晕）恒在。
    fn render_fs_cover(
        &mut self,
        cx: &mut Context<Self>,
        (light, dark): (Hsla, Hsla),
    ) -> AnyElement {
        let cover = self.cover_for_current(cx);
        let base = match &cover {
            Some(_) => self.render_cover_or(cover, div().into_any_element()),
            None => div()
                .size_full()
                .child(theme::gradient_cover((light, dark), 16.0))
                .child(
                    // 黑胶三圈（12s/9s/7s、外层反向）：动画在独立环形子视图内、只重绘自己
                    // （VinylView 自带 absolute 满布 flex 容器，视觉/相位与旧实现一致）。
                    self.vinyl_fs.clone().into_any_element(),
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
                                .size(px(80.))
                                .text_color(gpui::white().opacity(0.12)),
                        ),
                )
                .into_any_element(),
        };
        div()
            .w(px(FS_COVER_PX))
            .h(px(FS_COVER_PX))
            .rounded(px(16.))
            .relative()
            .overflow_hidden()
            .mb(px(24.))
            .shadow(vec![
                gpui::BoxShadow::new(px(0.), px(20.), gpui::hsla(0., 0., 0., 0.5))
                    .blur_radius(px(60.)),
                gpui::BoxShadow::new(px(0.), px(0.), theme::accent().opacity(0.35))
                    .blur_radius(px(60.)),
            ])
            .child(base)
            .into_any_element()
    }

    /// 封面下方歌曲信息（左对齐，v2 `.fs-song-meta`）。
    fn render_fs_song_meta(
        &mut self,
        title: String,
        artist: String,
        album: String,
    ) -> AnyElement {
        div()
            .w_full()
            .mb(px(20.))
            .child(
                div()
                    .text_size(px(22.))
                    .font_weight(gpui::FontWeight::BOLD)
                    .text_color(theme::text())
                    .text_ellipsis()
                    .child(title),
            )
            .child(
                div()
                    .mt(px(4.))
                    .text_size(px(14.))
                    .text_color(theme::text2())
                    .text_ellipsis()
                    .child(artist),
            )
            .child(
                div()
                    .mt(px(2.))
                    .text_size(px(12.))
                    .text_color(theme::text3())
                    .text_ellipsis()
                    .child(if album.is_empty() { "—".to_string() } else { album }),
            )
            .into_any_element()
    }

    /// 进度条（v2 `.fs-bar*`）：5px 轨道 + accent 渐变填充 + 悬停圆点；可拖拽 seek。
    fn render_fs_progress(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let p = self.player.read(cx);
        let dur = p.duration();
        let pos = p.position();
        let frac = if dur > 0.0 { (pos / dur).clamp(0.0, 1.0) as f32 } else { 0.0 };
        let hovered = self.fs_hover == Some("fs-prog");
        let b_down = self.fs_progress.clone();
        let b_drag = self.fs_progress.clone();
        let b_up = self.fs_progress.clone();
        let b_out = self.fs_progress.clone();

        div()
            .w_full()
            .mb(px(16.))
            .flex()
            .flex_col()
            .child(
                div()
                    .id("fs-prog")
                    .relative()
                    .w_full()
                    .h(px(16.))
                    .cursor(CursorStyle::PointingHand)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, ev, _w, cx| {
                            this.on_strip_down(true, b_down.clone(), ev, cx);
                        }),
                    )
                    .on_mouse_move(
                        cx.listener(move |this, ev, _w, cx| {
                            this.on_strip_drag(true, b_drag.clone(), ev, cx);
                        }),
                    )
                    // 松手收尾（与 strip_element/bb/np 同一套 handler，三处进度条共用一处修）：
                    // 窗口内松开 → on_mouse_up；拖出窗外/条外松开 → on_mouse_up_out 兜底。
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(move |this, ev, _w, cx| {
                            this.on_strip_up(true, b_up.clone(), ev, cx);
                        }),
                    )
                    .on_mouse_up_out(
                        MouseButton::Left,
                        cx.listener(move |this, ev, _w, cx| {
                            this.on_strip_up(true, b_out.clone(), ev, cx);
                        }),
                    )
                    .on_hover(cx.listener(|this: &mut MusicView, hovered, _w, cx| {
                        // 进度条只管理高亮/圆点，不弹名称提示（name=None 会清残留）。
                        this.set_fs_hover_and_tip("fs-prog", None, *hovered, cx);
                    }))
                    .child(theme::bounds_canvas(self.fs_progress.clone()))
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
                                    .h(px(5.))
                                    .rounded(px(3.))
                                    .bg(theme::bg_hover().opacity(0.8))
                                    .child(
                                        div()
                                            .absolute()
                                            .left_0()
                                            .top_0()
                                            .bottom_0()
                                            .w(DefiniteLength::Fraction(frac))
                                            .rounded(px(3.))
                                            .debug_selector(|| "fs-prog-fill".into())
                                            .child(theme::gradient_cover_tracked(
                                                (theme::accent(), theme::accent_light()),
                                                3.0,
                                                self.fs_fill_box.clone(),
                                            )),
                                    )
                                    .child(
                                        div()
                                            .absolute()
                                            .right(px(-6.))
                                            .top(px(-3.5))
                                            .w(px(12.))
                                            .h(px(12.))
                                            .rounded(px(6.))
                                            .bg(gpui::white())
                                            .shadow(vec![gpui::BoxShadow::new(
                                                px(0.),
                                                px(0.),
                                                theme::accent().opacity(0.6),
                                            )
                                            .blur_radius(px(10.))])
                                            .opacity(if hovered { 1.0 } else { 0.0 }),
                                    ),
                            ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .justify_between()
                    .mt(px(6.))
                    .text_size(px(11.))
                    .text_color(theme::text3())
                    .child(duration_str(pos))
                    .child(duration_str(dur)),
            )
            .into_any_element()
    }

    /// 控制行（v2 `.fs-controls`）：shuffle / prev / play(56 渐变圆) / next / repeat。
    fn render_fs_controls(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let p = self.player.read(cx);
        let shuffle = p.shuffle();
        let repeat = p.repeat();
        div()
            .w_full()
            .flex()
            .items_center()
            .justify_center()
            .gap_x(px(24.))
            .mb(px(16.))
            .child(self.fs_ctrl(MusicIcon::Shuffle, 26., shuffle, "fs-shuffle", cx))
            .child(self.fs_ctrl(MusicIcon::Prev, 26., false, "fs-prev", cx))
            .child(self.fs_play_btn(cx))
            .child(self.fs_ctrl(MusicIcon::Next, 26., false, "fs-next", cx))
            .child(self.fs_repeat(repeat, cx))
            .into_any_element()
    }

    /// 普通控制钮（shuffle/prev/next）：active 变 accent，悬停变亮。
    fn fs_ctrl(
        &mut self,
        icon: MusicIcon,
        size: f32,
        active: bool,
        id: &'static str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let hovered = self.fs_hover == Some(id);
        let cl = cx.listener(move |this: &mut MusicView, _ev, _w, cx| match id {
            "fs-shuffle" => this.toggle_shuffle(cx),
            "fs-prev" => this.player.update(cx, |p, cx| p.prev(cx)),
            "fs-next" => this.player.update(cx, |p, cx| p.next(cx)),
            _ => {}
        });
        div()
            .id(id)
            .debug_selector(move || id.to_string())
            .relative()
            .w(px(size))
            .h(px(size))
            .flex()
            .items_center()
            .justify_center()
            .cursor(CursorStyle::PointingHand)
            .on_click(cl)
            .on_hover(cx.listener(move |this: &mut MusicView, hovered, _w, cx| {
                let name: Option<&'static str> = match id {
                    "fs-shuffle" => Some("随机播放"),
                    "fs-prev" => Some("上一首"),
                    "fs-next" => Some("下一首"),
                    _ => None,
                };
                this.set_fs_hover_and_tip(id, name, *hovered, cx);
            }))
            .child(theme::bounds_canvas(self.tip_box_cell(id)))
            .child(
                IconView::new(icon)
                    .size(px(size))
                    .text_color(if active {
                        theme::accent()
                    } else if hovered {
                        theme::text()
                    } else {
                        theme::text2()
                    }),
            )
            .into_any_element()
    }

    /// 主播放钮（56px accent→light 渐变圆，白图标）。
    fn fs_play_btn(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let p = self.player.read(cx);
        let playing = p.playing();
        let icon = if playing { MusicIcon::Pause } else { MusicIcon::Play };
        let cl = cx.listener(|this: &mut MusicView, _ev, _w, cx| this.toggle_play(cx));
        div()
            .id("fs-play")
            .w(px(56.))
            .h(px(56.))
            .rounded(px(28.))
            .relative()
            .overflow_hidden()
            .flex()
            .items_center()
            .justify_center()
            .cursor(CursorStyle::PointingHand)
            .shadow(vec![gpui::BoxShadow::new(
                px(0.),
                px(4.),
                theme::accent().opacity(0.5),
            )
            .blur_radius(px(24.))])
            .on_click(cl)
            .on_hover(cx.listener(|this: &mut MusicView, hovered, _w, cx| {
                this.set_fs_hover_and_tip("fs-play", Some("播放/暂停"), *hovered, cx);
            }))
            .child(theme::bounds_canvas(self.tip_box_cell("fs-play")))
            .child(theme::gradient_cover((theme::accent(), theme::accent_light()), 28.0))
            .child(
                IconView::new(icon)
                    .size(px(28.))
                    .text_color(gpui::white())
                    .filled(true),
            )
            .into_any_element()
    }

    /// 循环钮：非 Off 高亮 accent。
    fn fs_repeat(&mut self, repeat: RepeatMode, cx: &mut Context<Self>) -> AnyElement {
        let active = repeat != RepeatMode::Off;
        let hovered = self.fs_hover == Some("fs-repeat");
        let cl = cx.listener(|this: &mut MusicView, _ev, _w, cx| this.cycle_repeat(cx));
        div()
            .id("fs-repeat")
            .relative()
            .w(px(26.))
            .h(px(26.))
            .flex()
            .items_center()
            .justify_center()
            .cursor(CursorStyle::PointingHand)
            .on_click(cl)
            .on_hover(cx.listener(|this: &mut MusicView, hovered, _w, cx| {
                this.set_fs_hover_and_tip("fs-repeat", Some("循环播放"), *hovered, cx);
            }))
            .child(theme::bounds_canvas(self.tip_box_cell("fs-repeat")))
            .child(
                IconView::new(MusicIcon::Repeat)
                    .size(px(24.))
                    .text_color(if active {
                        theme::accent()
                    } else if hovered {
                        theme::text()
                    } else {
                        theme::text2()
                    }),
            )
            .into_any_element()
    }

    /// 底部行（v2 `.fs-bottom-row`）：音量条（120）+ 操作（收藏/队列/下载）。
    fn render_fs_bottom_row(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let p = self.player.read(cx);
        let volume = p.volume();
        let cur_liked = p.current().map(|i| p.is_liked(i)).unwrap_or(false);
        div()
            .w_full()
            .flex()
            .items_center()
            .justify_between()
            .gap_x(px(16.))
            .child(self.render_fs_volume(cx, volume))
            .child(
                div()
                    .flex()
                    .gap_x(px(12.))
                    .child(
                        self.fs_action(
                            if cur_liked {
                                MusicIcon::Heart
                            } else {
                                MusicIcon::HeartBorder
                            },
                            cur_liked,
                            "fs-like",
                            cx,
                        ),
                    )
                    .child(self.fs_action(MusicIcon::QueueMusic, false, "fs-que", cx))
                    .child(self.fs_action(MusicIcon::Download, false, "fs-dl", cx)),
            )
            .into_any_element()
    }

    /// 音量条（点击/拖拽调音量）。
    fn render_fs_volume(&mut self, cx: &mut Context<Self>, volume: f32) -> AnyElement {
        let b_down = self.fs_vol.clone();
        let b_drag = self.fs_vol.clone();
        // 提示盒记录：复用 fs_vol 条盒 cell（无需多余 canvas；悬停「音量」按此盒锚定）。
        self.tip_boxes
            .entry("fs-vol")
            .or_insert_with(|| self.fs_vol.clone());
        let hovered = self.fs_hover == Some("fs-vol");
        div()
            .flex()
            .items_center()
            .gap_x(px(8.))
            .child(
                IconView::new(MusicIcon::Volume)
                    .size(px(18.))
                    .text_color(if hovered { theme::text() } else { theme::text2() }),
            )
            .child(
                div()
                    .id("fs-vol")
                    .relative()
                    .w(px(120.))
                    .h(px(16.))
                    .flex()
                    .items_center()
                    .cursor(CursorStyle::PointingHand)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, ev, _w, cx| {
                            this.on_strip_down(false, b_down.clone(), ev, cx);
                        }),
                    )
                    .on_mouse_move(
                        cx.listener(move |this, ev, _w, cx| {
                            this.on_strip_drag(false, b_drag.clone(), ev, cx);
                        }),
                    )
                    .on_hover(cx.listener(|this: &mut MusicView, hovered, _w, cx| {
                        this.set_fs_hover_and_tip("fs-vol", Some("音量"), *hovered, cx);
                    }))
                    .child(theme::bounds_canvas(self.fs_vol.clone()))
                    .child(
                        div()
                            .relative()
                            .w_full()
                            .h(px(4.))
                            .rounded(px(2.))
                            .bg(theme::bg_hover().opacity(0.8))
                            .child(
                                div()
                                    .absolute()
                                    .left_0()
                                    .top_0()
                                    .bottom_0()
                                    .w(DefiniteLength::Fraction(volume.clamp(0.0, 1.0)))
                                    .rounded(px(2.))
                                    .bg(theme::text2().opacity(0.85)),
                            ),
                    ),
            )
            .into_any_element()
    }

    /// 操作小钮（收藏/队列/下载）：liked 用 pink，否则 text3（悬停变亮）。
    fn fs_action(
        &mut self,
        icon: MusicIcon,
        liked: bool,
        id: &'static str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let hovered = self.fs_hover == Some(id);
        let cl = cx.listener(move |this: &mut MusicView, _ev, _w, cx| match id {
            "fs-like" => {
                if let Some(i) = this.player.read(cx).current() {
                    this.player.update(cx, |p, cx| p.toggle_like(i, cx));
                    this.toast_msg(if this.player.read(cx).is_liked(i) {
                        "已收藏"
                    } else {
                        "取消收藏"
                    });
                }
            }
            "fs-que" => this.toast_msg("已加入队列"),
            "fs-dl" => this.toast_msg("下载功能即将到来"),
            _ => {}
        });
        div()
            .id(id)
            .relative()
            .w(px(20.))
            .h(px(20.))
            .flex()
            .items_center()
            .justify_center()
            .cursor(CursorStyle::PointingHand)
            .on_click(cl)
            .on_hover(cx.listener(move |this: &mut MusicView, hovered, _w, cx| {
                let name: Option<&'static str> = match id {
                    "fs-like" => Some("收藏"),
                    "fs-que" => Some("加入队列"),
                    "fs-dl" => Some("下载"),
                    _ => None,
                };
                this.set_fs_hover_and_tip(id, name, *hovered, cx);
            }))
            .child(theme::bounds_canvas(self.tip_box_cell(id)))
            .child(
                IconView::new(icon)
                    .size(px(18.))
                    .text_color(if liked {
                        theme::pink()
                    } else if hovered {
                        theme::text()
                    } else {
                        theme::text3()
                    }),
            )
            .into_any_element()
    }

    /// 右列：歌词面板（v2 `.fs-right`：LYRICS 标题 + 同步歌词；无 LRC → 操作提示）。
    fn render_fs_lyrics(&mut self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .min_w(px(0.))
            .w(px(440.))
            .flex_shrink_0()
            .h(px(460.))
            .flex()
            .flex_col()
            .child(
                div()
                    .mb(px(16.))
                    .pl(px(8.))
                    .text_size(px(12.))
                    .text_color(theme::text3())
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child("LYRICS"),
            )
            .child(self.render_lyrics_panel_content(cx))
            .into_any_element()
    }

    /// `#lyrics-scroll` 滚动容器（两分支共用包装层）：挂 `on_scroll_wheel`——**每次滚轮事件**
    /// 都记「手动滚动」并退出自动跟随。关键点：offset 已钳到界限（内容尽头）时原生滚动
    /// 事件照样触发 → 用户持续滚动时「停止滚动」计时被持续刷新，不再满 ~3s 误恢复拉回视窗
    /// （真机复报 bug1）。`overflow_y_scroll` 原生滚动手感不变，`track_scroll` 绑定句柄。
    fn lyrics_scroll_container(
        &mut self,
        cx: &mut Context<Self>,
        inner: impl IntoElement,
    ) -> AnyElement {
        let wheel = cx.listener(
            |this: &mut MusicView, _ev: &ScrollWheelEvent, _w: &mut Window, cx: &mut Context<Self>| {
                this.lyrics_manual_since = Some(cx.background_executor().now());
                this.lyrics_follow_y = None;
            },
        );
        div()
            .id("lyrics-scroll")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .track_scroll(&self.lyrics_scroll)
            .on_scroll_wheel(wheel)
            .child(inner)
            .into_any_element()
    }

    /// 同步歌词内容（全屏右列与右侧面板「歌词」tab 共用）：有歌词 → `overflow_y_scroll`
    /// 可滚动容器，内容总高 = 行数×固定行高（全歌词可经滚轮浏览）；只物化可视/近邻窗口
    /// 行（K12：播放中 250ms tick 不全量物化几百行）；自动跟随（active 行居中）与手动
    /// 滚动暂停/恢复由 `on_tick` → [`Self::update_lyrics_follow`] 写滚动偏移。
    /// 无歌词 → 空态 + 「同名 .lrc」提示。`flex_1` 由调用方包一层保证占满可用高。
    pub(super) fn render_lyrics_panel_content(&mut self, cx: &mut Context<Self>) -> AnyElement {
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
                    IconView::new(MusicIcon::Lyrics)
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

    /// 歌词自动跟随（每 tick 由 `on_tick` 调用）：仅播放中执行——按当前播放位置得
    /// active 行，把滚动偏移写入「active 行居中」（无动画、不闪跳，维持现状跟随手感）。
    /// 跟随中若实测偏移偏离了上次写入值（用户滚轮/拖滚动条手动滚动）→ 记
    /// `lyrics_manual_since` 暂停跟随；停止滚动 ≥ [`LYRIC_FOLLOW_RESUME_MS`] 后自动恢复
    /// 跟随当前句。切歌/换曲（track path 变化）复位跟随态。暂停/空闲不写入（尊重手动）
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

    /// 底部 eq 渐变装饰条（v2 `.fs-eq-bar`：3px、accent→pink，播放 0.5 / 暂停 0.15）。
    fn render_fs_bottom_eq(&mut self, playing: bool) -> AnyElement {
        let alpha = if playing { 0.5 } else { 0.15 };
        div()
            .absolute()
            .bottom_0()
            .left_0()
            .right_0()
            .h(px(3.))
            .child(theme::gradient_cover(
                (theme::pink().opacity(alpha), theme::accent().opacity(alpha)),
                1.5,
            ))
            .into_any_element()
    }
}
