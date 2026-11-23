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

use super::buttons::{hover_listener, hover_state, icon_btn};
use super::icon::MusicIcon;
use super::{IntoIconElement, MusicView, RightTab, theme};
use ui::icon::{Icon, IconName};

/// 右侧大封面固定边长（px），黑胶圈按比例从它派生。
pub(super) const ART_PX: f32 = 264.0;

/// 队列行高（px）：py 8×2 + 40px 缩略图 = 56（虚拟化需确定行高反查偏移）。
pub(super) const QUEUE_ROW_H: f32 = 56.0;

/// 队列虚拟化可见窗缓冲（±行数）：多物化上下各 4 行，防快速滚动露白。
const QUEUE_VISIBLE_BUFFER: usize = 4;

/// 队列虚拟化首帧兜底物化行数（viewport 未布局时）。
const QUEUE_VISIBLE_FALLBACK: usize = 8;

/// 队列虚拟化可视窗：均匀行高算术反查（[`QUEUE_ROW_H`]）。
/// - `len==0` → `(0,0)`；
/// - `viewport<=0`（首帧未布局）→ 兜底物化前 [`QUEUE_VISIBLE_FALLBACK`] 行；
/// - 否则 `first = floor(scroll_y / 行高)`、`last = ceil((scroll_y+viewport) / 行高)`；
/// - ±`buffer` 缓冲并夹紧到 `len`，保证 `last > first`（防空窗）。
/// 纯函数可单测；判别式测试期望由公式手算。
pub(super) fn queue_visible_range(
    len: usize,
    scroll_y: f32,
    viewport: f32,
    buffer: usize,
) -> (usize, usize) {
    if len == 0 {
        return (0, 0);
    }
    if viewport <= 0.0 {
        return (0, len.min(QUEUE_VISIBLE_FALLBACK));
    }
    let first = (scroll_y / QUEUE_ROW_H).floor() as usize;
    let last = ((scroll_y + viewport) / QUEUE_ROW_H).ceil() as usize;
    let first = first.saturating_sub(buffer);
    let last = (last + buffer).min(len);
    (first, last.max(first.saturating_add(1)))
}

impl super::MusicView {
    /// 右侧面板整块。
    pub(super) fn render_right(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let is_queue = self.state.right_tab == RightTab::Queue;
        let mut col: Vec<AnyElement> = vec![self.render_rp_header(window, cx)];
        match self.state.right_tab {
            RightTab::Song => {
                col.push(self.render_now_playing_song(window, cx));
                col.push(self.render_up_next(window, cx));
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
        let mut root = div()
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
            .py(px(20.));
        // Queue tab：header 固定 + 队列列表自滚（虚拟化需确定视口/独立滚动句柄，列表
        // `music-queue-scroll` 自己 `overflow_y_scroll`）；其余 tab 整面板 `overflow_y_scroll`
        // （header 随滚，现状不变）。Queue tab 的 header 从「随面板滚」变「固定」——预期改进。
        if !is_queue {
            root = root.overflow_y_scroll();
        }
        root.children(col).into_any_element()
    }

    /// header：标题 + 三个 tab 钮（歌曲/歌词/队列）。
    fn render_rp_header(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
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
                        // tab hover 瞬时态（key 用钮 id）+ 既有 tooltip on_hover 合并为同一监听。
                        let (st, hovered) =
                            hover_state(window, cx, format!("rp-tab-hover-{key}"));
                        let tip_listener = self.hover_tip_listener(cx, key, label);
                        let view_id = window.current_view();
                        let st_for_cl = st.clone();
                        let bg = if active {
                            theme::accent()
                        } else if hovered {
                            theme::bg_hover()
                        } else {
                            theme::bg_secondary()
                        };
                        let fg = if active || hovered {
                            theme::text()
                        } else {
                            theme::text2()
                        };
                        div()
                            .id(key)
                            .debug_selector(move || key.into())
                            .relative()
                            .px(px(10.))
                            .py(px(5.))
                            .rounded(px(6.))
                            .cursor(CursorStyle::PointingHand)
                            .bg(bg)
                            .text_size(px(12.))
                            .text_color(fg)
                            .on_click(cl)
                            .on_hover(move |hovered, w, cx| {
                                st_for_cl.update(cx, |h, _| *h = *hovered);
                                cx.notify(view_id);
                                tip_listener(hovered, w, cx);
                            })
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
                    .child(self.np_control(window, IconName::Shuffle, 22., shuffle, "随机播放", cx))
                    .child(self.np_control(window, MusicIcon::Prev, 24., false, "上一首", cx))
                    .child(self.np_play_main(window, cx))
                    .child(self.np_control(window, MusicIcon::Next, 24., false, "下一首", cx))
                    .child(self.np_repeat(window, repeat, cx)),
            )
            .child(
                div()
                    .flex()
                    .justify_between()
                    .pb(px(16.))
                    .child(
                        self.extra_btn(
                            window,
                            if cur_liked { IconName::Heart } else { IconName::HeartBorder },
                            "收藏",
                            cur_liked,
                            "right-like",
                            cx,
                        ),
                    )
                    .child(self.extra_btn(window, IconName::QueueMusic, "加入队列", false, "right-que", cx))
                    .child(self.extra_btn(window, IconName::Share, "分享", false, "right-share", cx)),
            )
            .into_any_element()
    }

    /// 小控制钮（shuffle/prev/next 等）：统一 [`icon_btn`]，hover 前色 + 半透明圆底；
    /// active 保持 accent 高亮（hover 不覆盖）；新增悬停名称提示。
    fn np_control<I: IntoIconElement>(
        &mut self,
        window: &mut Window,
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
        // hover 瞬时态：key 用动作 label（随机播放/上一首/下一首…）。id 沿用既有 `np-{label}`
        // （静态串，debug 盒/测试口径不变）；tooltip 锚定盒/tip id 同 id。
        let id: &'static str = match label {
            "随机播放" => "np-随机播放",
            "上一首" => "np-上一首",
            "下一首" => "np-下一首",
            _ => "np-未知",
        };
        // tip 监听 / 点击闭包先落本地（先后借用 self+cx，各自释放后再传入 icon_btn，避免重入借用）。
        let tip_writer = self.hover_tip_listener(cx, id, label);
        icon_btn(
            window,
            cx,
            id,
            format!("np-ctl-hover-{label}"),
            icon,
            size - 2.0,
            size,
            size / 2.0,
            None,
            active,
            theme::text2(),
            theme::text(),
            Some(theme::accent_light()),
            None,
            self.tip_box_cell(id),
            tip_writer,
            cl,
        )
    }

    /// 播放/暂停主钮（48px accent 圆 + 阴影）：统一 [`icon_btn`]，fill_bg 注入 accent 实心圆底
    /// 元素、hover 叠白淡显（叠白 ×12%）；新增悬停名称提示（现无）。
    fn np_play_main(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let p = self.player.read(cx);
        let playing = p.playing();
        let icon = if playing { MusicIcon::Pause } else { MusicIcon::Play };
        let accent_fill =
            div().absolute().inset_0().rounded(px(24.)).bg(theme::accent()).into_any_element();
        let tip_writer = self.hover_tip_listener(cx, "np-play", "播放/暂停");
        let on_click = cx.listener(|this: &mut MusicView, _ev, _w, cx| this.toggle_play(cx));
        icon_btn(
            window,
            cx,
            "np-play",
            "np-play-hover".to_string(),
            icon,
            26.0,
            48.0,
            24.0,
            None,
            false,
            theme::text(),
            theme::text(),
            None,
            Some(accent_fill),
            self.tip_box_cell("np-play"),
            tip_writer,
            on_click,
        )
    }

    /// repeat 钮：Off/All/One 循环；统一 [`icon_btn`]，hover 前色 + 半透明圆底；
    /// active 保持 accent 高亮（hover 不覆盖）；新增悬停名称提示。
    fn np_repeat(
        &mut self,
        window: &mut Window,
        repeat: RepeatMode,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let active = repeat != RepeatMode::Off;
        let tip_writer = self.hover_tip_listener(cx, "np-repeat", "循环播放");
        let on_click = cx.listener(|this: &mut MusicView, _ev, _w, cx| this.cycle_repeat(cx));
        icon_btn(
            window,
            cx,
            "np-repeat",
            "np-repeat-hover".to_string(),
            IconName::Repeat,
            20.0,
            22.0,
            11.0,
            None,
            active,
            theme::text2(),
            theme::text(),
            Some(theme::accent_light()),
            None,
            self.tip_box_cell("np-repeat"),
            tip_writer,
            on_click,
        )
    }

    /// 附加钮（收藏/加入队列/分享），供 right / fullscreen 复用：统一 [`icon_btn`] 药丸形态
    /// （icon15 + gap5 + 文本），hover 淡底 + 前色变亮（active 收藏保持 pink 语义不被 hover 覆盖）。
    pub(super) fn extra_btn(
        &mut self,
        window: &mut Window,
        icon: IconName,
        label: &'static str,
        active: bool,
        id: &'static str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let tip_writer = self.hover_tip_listener(cx, id, label);
        let on_click = cx.listener(move |this: &mut MusicView, _ev, _w, cx| match label {
            "收藏" => this.like_current(cx),
            "加入队列" => this.add_to_queue_current(cx),
            // 分享需写窗口（如弹出原生分享/剪贴板助手），on_click 闭包签名自带 `&mut Window`。
            "分享" => this.share_current(_w, cx),
            _ => {}
        });
        icon_btn(
            window,
            cx,
            id,
            format!("extra-hover-{id}"),
            icon,
            15.0,
            15.0,
            6.0,
            Some(label),
            active,
            theme::text3(),
            theme::text(),
            Some(theme::pink()),
            None,
            self.tip_box_cell(id),
            tip_writer,
            on_click,
        )
    }

    /// 队列 tab：当前队列逐项（虚拟化——只物化可视窗 ±[`QUEUE_VISIBLE_BUFFER`] 行，均匀行高
    /// 算术反查见 [`queue_visible_range`]；不再全量 `to_vec()`/全量物化，整树重渲 O(可视行)）。
    /// 容器自身滚动（`music-queue-scroll`，`track_scroll(&self.queue_scroll)`）+ 顶/底定高垫片
    /// 保内容总高 = 全队列高、全范围可滚（对齐 main.rs `render_list` 垫片姿势）。
    fn render_queue_list(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let scroll_y = (-self.queue_scroll.offset().y.as_f32()).max(0.0);
        let viewport = self.queue_scroll.bounds().size.height.as_f32();
        // 借用块内借出 `&[usize]`，只克隆可见窗下标（不克隆全队）。
        let (first, len, qpos, idxs) = {
            let p = self.player.read(cx);
            let q = p.queue();
            let len = q.len();
            let (first, last) =
                queue_visible_range(len, scroll_y, viewport, QUEUE_VISIBLE_BUFFER);
            (first, len, p.queue_pos(), q[first..last].to_vec())
        };
        // 空队列：容器内居中「队列为空」。
        if len == 0 {
            return div()
                .id("music-queue-scroll")
                .debug_selector(|| "music-queue-scroll".into())
                .flex_1()
                .min_h(px(0.))
                .overflow_y_scroll()
                .track_scroll(&self.queue_scroll)
                .flex()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .text_size(px(13.))
                        .text_color(theme::text3())
                        .child("队列为空"),
                )
                .into_any_element();
        }
        let last = first + idxs.len();
        let mut rows: Vec<AnyElement> = Vec::with_capacity(idxs.len());
        for (k, &idx) in idxs.iter().enumerate() {
            let qi = first + k;
            let Some((title, artist, duration, (light, dark))) = ({
                let p = self.player.read(cx);
                p.library().get(idx).map(|t| {
                    (
                        t.title.clone(),
                        t.artist.clone(),
                        t.duration,
                        theme::gradient_pair_of(t.grad),
                    )
                })
            }) else {
                continue;
            };
            let is_now = qi == qpos;
            let cl = cx.listener(move |this: &mut MusicView, _ev, _w, cx| {
                this.handle_play_idx(idx, cx);
            });
            // 行 hover 瞬时态：key 用队列下标 qi（唯一）；当前行保持 accent 高亮不叠 hover。
            let (st, hovered) = hover_state(window, cx, format!("queue-item-hover-{qi}"));
            rows.push(
                div()
                    .id(format!("queue-item-{qi}"))
                    .debug_selector(move || format!("queue-item-{qi}"))
                    // 固定行高（虚拟化行高契约 = [`QUEUE_ROW_H`]，垂直居中内容）。
                    .h(px(QUEUE_ROW_H))
                    .flex()
                    .items_center()
                    .gap_x(px(10.))
                    .px(px(10.))
                    .rounded(px(8.))
                    .cursor(CursorStyle::PointingHand)
                    .bg(if is_now {
                        theme::accent().opacity(0.12)
                    } else if hovered {
                        theme::bg_hover().opacity(0.6)
                    } else {
                        theme::bg_card().opacity(0.0)
                    })
                    .on_click(cl)
                    .on_hover(hover_listener(window, &st))
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
                    .child(if is_now {
                        self.eq_queue.clone().into_any_element()
                    } else {
                        div()
                            .text_size(px(11.))
                            .text_color(theme::text3())
                            .child(crate::model::duration_str(duration))
                            .into_any_element()
                    })
                    .into_any_element(),
            );
        }
        let top_h = first as f32 * QUEUE_ROW_H;
        let bottom_h = (len - last) as f32 * QUEUE_ROW_H;
        div()
            .id("music-queue-scroll")
            .debug_selector(|| "music-queue-scroll".into())
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .track_scroll(&self.queue_scroll)
            .child(div().h(px(top_h)))
            .children(rows)
            .child(div().h(px(bottom_h)))
            .into_any_element()
    }

    /// 「即将播放」：队列剩余前 3 项（歌曲 tab 底部；行 hover 淡底）。
    fn render_up_next(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        // 借用块内直接迭代 `&[usize]`（不 `to_vec()` 克隆整队；O(N) 迭代零分配）。
        let next: Vec<usize> = {
            let p = self.player.read(cx);
            let queue = p.queue();
            let qpos = p.queue_pos();
            queue
                .iter()
                .enumerate()
                .filter(|(qi, _)| *qi > qpos)
                .take(3)
                .map(|(_, &idx)| idx)
                .collect()
        };
        let mut rows: Vec<AnyElement> = Vec::new();
        for &idx in &next {
            let Some((title, artist, duration, (light, dark))) = ({
                let p = self.player.read(cx);
                p.library().get(idx).map(|t| {
                    (
                        t.title.clone(),
                        t.artist.clone(),
                        t.duration,
                        theme::gradient_pair_of(t.grad),
                    )
                })
            }) else {
                continue;
            };
            let cl = cx.listener(move |this: &mut MusicView, _ev, _w, cx| {
                this.handle_play_idx(idx, cx);
            });
            // 行 hover 瞬时态：key 用库下标 idx（唯一）。
            let (st, hovered) = hover_state(window, cx, format!("up-next-hover-{idx}"));
            rows.push(
                div()
                    .id(format!("up-next-{idx}"))
                    .debug_selector(move || format!("up-next-{idx}"))
                    .flex()
                    .items_center()
                    .gap_x(px(10.))
                    .py(px(6.))
                    .rounded(px(6.))
                    .cursor(CursorStyle::PointingHand)
                    .bg(if hovered {
                        theme::bg_hover().opacity(0.6)
                    } else {
                        gpui::transparent_black()
                    })
                    .on_click(cl)
                    .on_hover(hover_listener(window, &st))
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
                            .text_size(px(11.))
                            .text_color(theme::text3())
                            .child(crate::model::duration_str(duration)),
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
