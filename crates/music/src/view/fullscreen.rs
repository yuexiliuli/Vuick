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
//!
//! GPUI 无 CSS `radial-gradient`/`letter-spacing`：背景光晕用两个大圆半透明层近似、
//! 文字间距忽略；黑胶圈用 svg 圆环 + 旋转动画（暂停即停）。Esc 关全屏由根 `on_key`
//! 处理，close 钮也关。

use std::time::Instant;

use gpui::{
    AnyElement, Context, CursorStyle, DefiniteLength, Hsla, InteractiveElement as _,
    IntoElement, ParentElement as _, StatefulInteractiveElement as _, Styled as _, Window, div,
    px,
};

use crate::model::{RepeatMode, duration_str};

use super::icon::{IconView, MusicIcon};
use super::{IntoIconElement, MusicView, theme};
use ui::icon::{Icon, IconName};
use ui::{CommitMode, Slider, SliderFill, SliderThumb};

/// 全屏封面边长（px），黑胶圈按比例派生。
pub(super) const FS_COVER_PX: f32 = 320.0;

// 歌词行三态常量已随 `super::lyrics::LyricsView` 迁走（`LYRIC_ROW_H`/
// `LYRIC_FOLLOW_RESUME_MS` 在 `view::lyrics`，测试经 `super::lyrics` 引用）。

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
                    .child(self.fs_icon_btn(IconName::PlaylistAdd, "已添加到播放列表", cx))
                    .child(self.fs_icon_btn(IconName::Share, "分享", cx)),
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
                    .child(self.fs_icon_btn(IconName::Badge, "高音质", cx))
                    .child(self.fs_close_btn(cx)),
            )
            .into_any_element()
    }

    /// 工具条圆钮（40px，v2 悬停变亮）。
    fn fs_icon_btn(
        &mut self,
        icon: IconName,
        tip: &'static str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let hovered = self.fs_hover == Some(tip);
        let cl = cx.listener(move |this: &mut MusicView, _ev, _w, cx| match tip {
            // 顶栏分享钮真实化：真正写剪贴板 + toast（不再只弹假提示）。
            "分享" => this.share_current(_w, cx),
            _ => this.toast_msg(tip, cx),
        });
        div()
            .id(format!("fs-btn-{tip}"))
            .relative()
            .w(px(theme::FS_CTRL_BTN_SIZE))
            .h(px(theme::FS_CTRL_BTN_SIZE))
            .rounded(px(theme::FS_CTRL_BTN_RADIUS))
            .flex()
            .items_center()
            .justify_center()
            .bg(if hovered { theme::white_fade_12() } else { theme::white_fade_06() })
            .border_1()
            .border_color(theme::white_fade_08())
            .cursor(CursorStyle::PointingHand)
            .on_click(cl)
            .on_hover(cx.listener(move |this: &mut MusicView, hovered, _w, cx| {
                // 复用现有 toast 文案作悬停提示（id 与 tip 同名：fs-btn-{tip}）；
                // 盒按 id（=tip）记入 tip_boxes，供 set_fs_hover_and_tip 锚定按钮盒。
                this.set_fs_hover_and_tip(tip, Some(tip), *hovered, cx);
            }))
            .child(theme::bounds_canvas(self.tip_box_cell(tip)))
            .child(
                Icon::new(icon)
                    .size(px(theme::FS_CTRL_BTN_ICON))
                    .text_color(if hovered { theme::text() } else { theme::text2() }),
            )
            .into_any_element()
    }

    /// 关闭全屏钮（红 hover 语义：直接关 fs）。
    fn fs_close_btn(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let hovered = self.fs_hover == Some("fs-close");
        let cl = cx.listener(|this: &mut MusicView, _ev, _w, cx| {
            this.exit_fullscreen(cx);
        });
        div()
            .id("fs-close")
            .debug_selector(|| "fs-close".into())
            .relative()
            .w(px(theme::FS_CTRL_BTN_SIZE))
            .h(px(theme::FS_CTRL_BTN_SIZE))
            .rounded(px(theme::FS_CTRL_BTN_RADIUS))
            .flex()
            .items_center()
            .justify_center()
            .bg(if hovered {
                theme::danger_close_bg()
            } else {
                theme::white_fade_08()
            })
            .border_1()
            .border_color(if hovered {
                theme::danger_close_border()
            } else {
                theme::white_fade_12()
            })
            .cursor(CursorStyle::PointingHand)
            .on_click(cl)
            .on_hover(cx.listener(|this: &mut MusicView, hovered, _w, cx| {
                this.set_fs_hover_and_tip("fs-close", Some("退出全屏"), *hovered, cx);
            }))
            .child(theme::bounds_canvas(self.tip_box_cell("fs-close")))
            .child(
                Icon::new(IconName::FullscreenExit)
                    .size(px(theme::FS_CTRL_BTN_ICON))
                    .text_color(if hovered {
                        theme::danger_close_icon()
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
                            Icon::new(IconName::Note)
                                .size(px(80.))
                                .text_color(theme::white_fade_12()),
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
            // B 类保留：封面装饰投影（黑中性 + accent 辉光；③ 已 skin-aware，黑投影不随全局 card_shadow）。
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

    // ---- 可交互条（全屏进度/音量条）事件 → 播放器 ----

    /// seek 点按（Slider `on_change`，原 `on_strip_down(true)` seek 分支）：进拖拽态 + 立即
    /// 真实 seek；同时补回根聚焦——Slider `mouse_down` 已 `stop_propagation`，点条不再冒泡到
    /// 根 `on_root_mouse_down`（mod.rs 负责聚焦根），对齐 bottom/np `focus_music_root` 套路
    /// （全屏由 MusicView 自身渲染，root 即 `self.focus_handle`）。
    fn fs_begin_scrub(&mut self, f: f32, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_handle.focus(window, cx);
        self.state.scrubbing = true;
        self.state.last_scrub_retarget = Some(Instant::now());
        // 同步钉住播放器显示基准：拖拽中 position = 手指落点，不随时钟漂移。
        self.player.update(cx, |p, cx| p.set_scrubbing(true, cx));
        self.seek_to_fraction(f, true, cx);
    }

    /// seek 拖拽 move（Slider `on_change_during_drag`，原 `on_strip_drag(true)` seek 分支）：
    /// 节流真实 seek、节流窗内 set_seek_base 跟手；窗外钳 0/1 与松手兜底由 Slider 自有 MouseUp
    /// 接管，不再需要旧 `ev.dragging()`/`on_mouse_up_out` 探针。
    fn fs_drag_scrub(&mut self, f: f32, cx: &mut Context<Self>) {
        self.seek_to_fraction(f, false, cx);
    }

    /// seek 松手精提交（Slider `on_commit`，原 `on_strip_up(true)`）：按松手点精确分数刷新落点
    /// （窗外沿用最后锁定落点——Slider 把 f 算成精确/锁定分数），再补发末次精提交。
    fn fs_end_scrub(&mut self, f: f32, cx: &mut Context<Self>) {
        let dur = self.player.read(cx).duration();
        self.state.scrub_target = dur * f as f64;
        self.end_strip_scrub(cx);
    }

    /// 音量即时应用（Slider `on_change`/`on_change_during_drag` 共用，原 `on_strip_down/drag
    /// (false)` → `apply_strip` vol 分支）：`set_volume` + 本视图 notify；同时补回根聚焦
    /// （同 [`Self::fs_begin_scrub`] 的 NIT）。
    fn fs_apply_vol(&mut self, f: f32, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_handle.focus(window, cx);
        self.apply_strip(false, f, cx);
    }

    /// 进度条（v2 `.fs-bar*`）：走自绘 `ui::Slider`——5px/rounded(3) 细轨（bg_hover×0.8）+
    /// accent→accent_light 横向渐变填充 + 悬停白点 thumb（accent 辉光）；整条 16px 命中高。
    /// 组件只回传分数事件，宿主收口（[`Self::fs_begin_scrub`]/[`Self::fs_drag_scrub`]/
    /// [`Self::fs_end_scrub`]，保 [`Self::seek_to_fraction`] 的 SCRUB_RETARGET 节流 + 松手精提交，
    /// 对应旧 `on_strip_down(true)/on_strip_drag(true)/on_strip_up(true)` 语义）；elapsed/total
    /// 时间行与 hover 高亮（`fs_hover`）留在消费方不进组件。`fs_progress`/`fs_fill_box` 由
    /// `on_geometry` 每帧回写（后者 = Slider 填充盒的忠实重建，供「填充随进度增长」回归）。
    fn render_fs_progress(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let p = self.player.read(cx);
        let dur = p.duration();
        let pos = p.position();
        let frac = if dur > 0.0 { (pos / dur).clamp(0.0, 1.0) as f32 } else { 0.0 };
        let entity = cx.entity();
        let b_bar = self.fs_progress.clone();
        let b_fill = self.fs_fill_box.clone();
        let slider = Slider::new("fs-prog-slider")
            .value(frac)
            .commit_mode(CommitMode::OnRelease)
            .track_height(px(5.))
            .track_radius(3.)
            // 轨道底色 = 现 track（bg_hover × 0.8）；填充 = accent→accent_light 横向渐变
            //（组件内画 90° 渐变；旧 135° 对角渐变近似，见 Wire-B2 映射表）。
            .track_color(theme::bg_hover().opacity(0.8))
            .fill(SliderFill::Gradient([
                theme::accent(),
                theme::accent_light(),
            ]))
            // 白色 thumb + accent 辉光：静止透明度 0（不显现）、hover 弹簧淡入到 1——
            // 对旧「hover 才显现 12px 白点」观感（新旧圆心都贴填充末端；12px 直径=半径 6）。
            .thumb(Some(SliderThumb {
                base: 0.0,
                hover: 6.0,
                color: theme::text(),
                glow: Some(theme::accent()),
                opacity_base: 0.0,
                opacity_hover: 1.0,
            }))
            // 每帧把真实命中盒（窗口坐标）回写 fs_progress；fs_fill_box = Slider 填充盒重建
            //（轨 5px 居中：填充宽 = fill_end−左缘、高 5），替代旧 gradient_cover_tracked canvas。
            .on_geometry({
                let b_bar = b_bar.clone();
                let b_fill = b_fill.clone();
                move |geom, _w, _app| {
                    b_bar.set(geom.bounds);
                    let b = geom.bounds;
                    let w = (geom.fill_end.x.as_f32() - b.origin.x.as_f32()).max(0.0);
                    let cy = b.center().y.as_f32();
                    b_fill.set(gpui::Bounds::new(
                        gpui::point(b.origin.x, px(cy - 2.5)),
                        gpui::size(px(w), px(5.0)),
                    ));
                }
            })
            // seek 三阶段事件 → 宿主收口（进/退拖拽态 + SCRUB_RETARGET 节流 + 松手精提交）。
            .on_change({
                let entity = entity.clone();
                move |f, w, app| entity.update(app, |this, cx| this.fs_begin_scrub(f, w, cx))
            })
            .on_change_during_drag({
                let entity = entity.clone();
                move |f, _w, app| entity.update(app, |this, cx| this.fs_drag_scrub(f, cx))
            })
            .on_commit({
                let entity = entity.clone();
                move |f, _w, app| entity.update(app, |this, cx| this.fs_end_scrub(f, cx))
            });

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
                    .on_hover(cx.listener(|this: &mut MusicView, hovered, _w, cx| {
                        // 进度条只管理高亮/圆点，不弹名称提示（name=None 会清残留）。
                        this.set_fs_hover_and_tip("fs-prog", None, *hovered, cx);
                    }))
                    // debug 只读探针：Slider 用 paint_quad 自绘填充（无元素盒），回归断言靠
                    // `debug_bounds("fs-prog-fill")` 读「填充宽 = frac×条宽」——透明 div 只承载
                    // debug_selector（不绘制、无命中盒；几何与 Slider 轨同 5px 居中）。
                    .child(
                        div()
                            .debug_selector(|| "fs-prog-fill".into())
                            .absolute()
                            .left_0()
                            .top(px(5.5))
                            .h(px(5.))
                            .w(DefiniteLength::Fraction(frac)),
                    )
                    .child(slider),
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
            .child(self.fs_ctrl(IconName::Shuffle, 26., shuffle, "fs-shuffle", cx))
            .child(self.fs_ctrl(MusicIcon::Prev, 26., false, "fs-prev", cx))
            .child(self.fs_play_btn(cx))
            .child(self.fs_ctrl(MusicIcon::Next, 26., false, "fs-next", cx))
            .child(self.fs_repeat(repeat, cx))
            .into_any_element()
    }

    /// 普通控制钮（shuffle/prev/next）：active 变 accent，悬停变亮。
    fn fs_ctrl<I: IntoIconElement>(
        &mut self,
        icon: I,
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
                icon.icon_element(
                    px(size),
                    if active {
                        theme::accent()
                    } else if hovered {
                        theme::text()
                    } else {
                        theme::text2()
                    },
                ),
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
                    .text_color(theme::text()),
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
                Icon::new(IconName::Repeat)
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
                                IconName::Heart
                            } else {
                                IconName::HeartBorder
                            },
                            cur_liked,
                            "fs-like",
                            cx,
                        ),
                    )
                    .child(self.fs_action(IconName::QueueMusic, false, "fs-que", cx))
                    .child(self.fs_action(IconName::Download, false, "fs-dl", cx)),
            )
            .into_any_element()
    }

    /// 音量条（点击/拖拽调音量）：走自绘 `ui::Slider`——120px 定宽、4px/rounded(2) 细轨 +
    /// text2×0.85 实色填充（thumb=None）、`CommitMode::Immediate` 即时生效；悬停「音量」提示
    /// 留在消费方（按 fs_vol 盒锚定，同旧）。点/拖都经 [`Self::fs_apply_vol`] 即时 set_volume +
    /// 补回根聚焦。
    fn render_fs_volume(&mut self, cx: &mut Context<Self>, volume: f32) -> AnyElement {
        // 提示盒记录：复用 fs_vol 条盒 cell（无需多余 canvas；悬停「音量」按此盒锚定）。
        self.tip_boxes
            .entry("fs-vol")
            .or_insert_with(|| self.fs_vol.clone());
        let hovered = self.fs_hover == Some("fs-vol");
        let entity = cx.entity();
        let b_vol = self.fs_vol.clone();
        let slider = Slider::new("fs-vol-slider")
            .value(volume.clamp(0.0, 1.0))
            .commit_mode(CommitMode::Immediate)
            .track_height(px(4.))
            .track_radius(2.)
            // 轨道底色 = 现 track（bg_hover × 0.8）；填充 = text2 × 0.85（thumb=None）。
            .track_color(theme::bg_hover().opacity(0.8))
            .fill(SliderFill::Solid(theme::text2().opacity(0.85)))
            .thumb(None)
            // 每帧把真实命中盒（120×16，窗口坐标）回写 fs_vol（替代旧 canvas 记录）。
            .on_geometry({
                let b_vol = b_vol.clone();
                move |geom, _w, _app| b_vol.set(geom.bounds)
            })
            // vol 即时：点/拖都直接 set_volume（无 on_commit）。
            .on_change({
                let entity = entity.clone();
                move |f, w, app| entity.update(app, |this, cx| this.fs_apply_vol(f, w, cx))
            })
            .on_change_during_drag({
                let entity = entity.clone();
                move |f, w, app| entity.update(app, |this, cx| this.fs_apply_vol(f, w, cx))
            });

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
                    .cursor(CursorStyle::PointingHand)
                    .on_hover(cx.listener(|this: &mut MusicView, hovered, _w, cx| {
                        this.set_fs_hover_and_tip("fs-vol", Some("音量"), *hovered, cx);
                    }))
                    .child(slider),
            )
            .into_any_element()
    }

    /// 操作小钮（收藏/队列/下载）：liked 用 pink，否则 text3（悬停变亮）。
    fn fs_action(
        &mut self,
        icon: IconName,
        liked: bool,
        id: &'static str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let hovered = self.fs_hover == Some(id);
        let cl = cx.listener(move |this: &mut MusicView, _ev, _w, cx| match id {
            "fs-like" => {
                if let Some(i) = this.player.read(cx).current() {
                    this.player.update(cx, |p, cx| p.toggle_like(i, cx));
                    this.toast_msg(
                        if this.player.read(cx).is_liked(i) {
                            "已收藏"
                        } else {
                            "取消收藏"
                        },
                        cx,
                    );
                }
            }
            "fs-que" => this.add_to_queue_current(cx),
            "fs-dl" => this.reveal_current(cx),
            _ => {}
        });
        div()
            .id(id)
            .relative()
            .debug_selector(move || id.into())
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
                Icon::new(icon)
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
    /// 内容由 `self.lyrics_fs`（`view::lyrics::LyricsView` 实例，独立 ScrollHandle）渲染——
    /// 双实例双句柄，全屏与右栏不再争抢同一滚动状态。
    fn render_fs_lyrics(&mut self, _cx: &mut Context<Self>) -> AnyElement {
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
            .child(self.lyrics_fs.clone().into_any_element())
            .into_any_element()
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
