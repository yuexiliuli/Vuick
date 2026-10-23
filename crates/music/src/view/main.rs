//! 主内容区（flex_1）：头部（问候 + 搜索 + 圆钮）+ 按分区的内容。
//!
//! - Home：横滑「我的推荐」Mix 卡（渐变封面 + hover 播放浮层，点击播放组内第一首）
//!   + 「热门歌曲」表格；
//! - Library：按艺术家分组的歌曲列表（组头 + 行）；
//! - Liked：只列已收藏歌曲。
//! 搜索框：ASCII 可打印键直接落文本（`key_char`），中文走 IME（view 的
//! `EntityInputHandler`）：当前简化实现支持即输即滤。

use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, Context, CursorStyle, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, Window, div, px,
};

use crate::model::Track;

use super::icon::{IconView, MusicIcon};
use super::{MainSection, MusicView, theme, widgets};

/// 歌曲表行高（px，近似原型的行内距）。
const ROW_H: f32 = 52.0;

impl super::MusicView {
    /// 主内容区整块。
    pub(super) fn render_main(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let body: AnyElement = match self.state.section {
            MainSection::Home => self.render_home(window, cx),
            MainSection::Library => self.render_library(window, cx),
            MainSection::Liked => self.render_liked(window, cx),
        };
        div()
            .id("music-main")
            .debug_selector(|| "music-main".into())
            .flex_1()
            .min_w(px(0.))
            .min_h(px(0.))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .px(px(28.))
            .pt(px(24.))
            .child(self.render_main_header(window, cx))
            .child(body)
            .into_any_element()
    }

    /// 头部：问候 + 搜索框 + 通知/设置圆钮。
    fn render_main_header(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let greeting = match chrono_now_hour() {
            h if h < 6 => "凌晨好".to_string(),
            h if h < 12 => "早上好".to_string(),
            h if h < 18 => "下午好".to_string(),
            _ => "晚上好".to_string(),
        };
        div()
            .flex()
            .items_center()
            .justify_between()
            .pb(px(20.))
            .child(
                div()
                    .text_size(px(24.))
                    .font_weight(gpui::FontWeight::BOLD)
                    .text_color(theme::text())
                    .child(format!("{greeting}，Koa")),
            )
            .child(
                div().flex().items_center().gap_x(px(12.))
                    .child(self.render_search_box(window, cx))
                    .child(self.round_icon_button("music-notify", MusicIcon::Bell, "通知中心", cx))
                    .child(self.round_icon_button("music-settings", MusicIcon::Settings, "设置", cx)),
            )
            .into_any_element()
    }

    /// 搜索框：输入即过滤（`state.search` 驱动主表）。ASCII 可打印键直落
    /// （`key_char` 已带 shift）；中文 IME 由 view 的 `EntityInputHandler` 落地。
    fn render_search_box(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let focused = self.search_focus.is_focused(window);
        let border = if focused {
            theme::accent()
        } else {
            theme::border()
        };
        let text = self.state.search.clone();
        let placeholder = !focused && text.is_empty();
        let entity = cx.entity();
        let search_focus = self.search_focus.clone();
        div()
            .id("music-search")
            .debug_selector(|| "music-search".into())
            .flex()
            .items_center()
            .gap_x(px(8.))
            .px(px(14.))
            .h(px(36.))
            .w(px(240.))
            .bg(theme::bg_secondary())
            .border_1()
            .border_color(border)
            .rounded(px(18.))
            .track_focus(&self.search_focus)
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this: &mut MusicView, _ev, window, cx| {
                    this.search_focus.focus(window, cx);
                    cx.stop_propagation();
                }),
            )
            .on_key_down(cx.listener(Self::on_search_key))
            .child(IconView::new(MusicIcon::Search).size(px(16.)).text_color(theme::text3()))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .child(if placeholder {
                        div()
                            .text_size(px(13.))
                            .text_color(theme::text3())
                            .child("搜索歌曲、歌手、专辑…")
                            .into_any_element()
                    } else {
                        div()
                            .text_size(px(13.))
                            .text_color(theme::text())
                            .child(text)
                            .into_any_element()
                    }),
            )
            .child(
                // 注册 IME 输入句柄（仅搜索持焦时）：把组合文本直接落 `state.search`。
                canvas_ime_handler(entity, search_focus),
            )
            .into_any_element()
    }

    /// 搜索框按键：可打印字符（key_char）直插；backspace 退格；escape 清空。
    /// 全部吞掉，不冒泡到根的全局快捷键。
    fn on_search_key(
        &mut self,
        e: &gpui::KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let k = &e.keystroke;
        let m = k.modifiers;
        match k.key.as_str() {
            "backspace" => {
                self.state.search.pop();
                cx.notify();
            }
            "escape" => {
                self.state.search.clear();
                cx.notify();
            }
            _ => {
                if !m.control && !m.alt && !m.platform {
                    if let Some(ch) = &k.key_char {
                        self.state.search.push_str(ch);
                        cx.notify();
                    } else if k.key == "space" {
                        self.state.search.push(' ');
                        cx.notify();
                    }
                }
            }
        }
        cx.stop_propagation();
    }

    /// 圆角图标钮（通知/设置）：点击 toast。
    fn round_icon_button(
        &mut self,
        id: &'static str,
        icon: MusicIcon,
        label: &'static str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let cl = cx.listener(move |this: &mut MusicView, _ev, _w, _cx| {
            this.toast_msg(label);
        });
        div()
            .id(id)
            .w(px(36.))
            .h(px(36.))
            .rounded(px(18.))
            .flex()
            .items_center()
            .justify_center()
            .bg(theme::bg_secondary())
            .border_1()
            .border_color(theme::border())
            .cursor(CursorStyle::PointingHand)
            .on_click(cl)
            .child(IconView::new(icon).size(px(17.)).text_color(theme::text2()))
            .into_any_element()
    }

    // ---- Home：我的推荐 Mix 卡 + 热门歌曲表 ----

    fn render_home(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let p = self.player.read(cx);
        let groups: Vec<(String, String, Vec<usize>, (String, String))> = p
            .mix_groups()
            .iter()
            .take(6)
            .map(|g| {
                (
                    g.title.clone(),
                    g.subtitle.clone(),
                    g.tracks.clone(),
                    g.grad.clone(),
                )
            })
            .collect();
        let total = p.library().len();
        let hot: Vec<(usize, Track)> = super::filter_tracks_by_query(p.library(), &self.state.search)
            .iter()
            .map(|&i| (i, p.library()[i].clone()))
            .collect();
        div().flex().flex_col()
            .child(self.render_mix_scroll(groups, cx))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .pt(px(8.))
                    .pb(px(14.))
                    .child(
                        div()
                            .text_size(px(18.))
                            .font_weight(gpui::FontWeight::BOLD)
                            .text_color(theme::text())
                            .child("热门歌曲"),
                    )
                    .child(
                        div()
                            .text_size(px(13.))
                            .text_color(theme::text3())
                            .child(format!("共 {total} 首")),
                    ),
            )
            .child(self.render_song_header())
            .children(hot.iter().enumerate().map(|(rank, (i, t))| {
                self.song_row(*i, rank, t, cx)
            }))
            .into_any_element()
    }

    /// 「我的推荐」横滑 Mix 卡组（`overflow_x_scroll`）。
    fn render_mix_scroll(
        &mut self,
        groups: Vec<(String, String, Vec<usize>, (String, String))>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let cards: Vec<AnyElement> = groups
            .into_iter()
            .filter(|g| !g.2.is_empty())
            .map(|(title, sub, tracks, grad)| self.mix_card(title, sub, tracks, grad, cx))
            .collect();
        div().flex().flex_col()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .pb(px(14.))
                    .child(
                        div()
                            .text_size(px(18.))
                            .font_weight(gpui::FontWeight::BOLD)
                            .text_color(theme::text())
                            .child("我的推荐"),
                    )
                    .child(
                        div()
                            .text_size(px(13.))
                            .text_color(theme::accent_light())
                            .child("查看全部 →"),
                    ),
            )
            .child(
                div()
                    .id("music-mix-scroll")
                    .flex()
                    .gap_x(px(16.))
                    .overflow_x_scroll()
                    .pb(px(10.))
                    .children(cards),
            )
            .into_any_element()
    }

    /// 单张 Mix 卡：渐变封面（内嵌音符文本）+ hover 播放浮层 + 标题/sub。点击播组内第一首。
    fn mix_card(
        &mut self,
        title: String,
        sub: String,
        tracks: Vec<usize>,
        grad: (String, String),
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let first = tracks[0];
        let hovered = self.hover_mix == Some(first);
        let (light, dark) = (
            theme::from_hex_str(&grad.0),
            theme::from_hex_str(&grad.1),
        );
        let cover = div()
            .w(px(180.))
            .h(px(180.))
            .rounded(px(14.))
            .relative()
            .overflow_hidden()
            .mx_auto()
            .child(theme::gradient_cover((light, dark), 14.0))
            // 内嵌音乐符文本（原型 `.mix-cover-bg .material-icons`）。
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        IconView::new(MusicIcon::Note)
                            .size(px(44.))
                            .text_color(gpui::white().opacity(0.4)),
                    ),
            )
            // hover 播放浮层（右下角 accent 圆钮）。
            .child(
                div()
                    .absolute()
                    .right(px(10.))
                    .bottom(px(10.))
                    .w(px(40.))
                    .h(px(40.))
                    .rounded(px(20.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(theme::accent())
                    .opacity(if hovered { 1.0 } else { 0.0 })
                    .child(
                        IconView::new(MusicIcon::Play)
                            .size(px(20.))
                            .text_color(gpui::white())
                            .filled(true),
                    ),
            );
        let cl = cx.listener(move |this: &mut MusicView, _ev, _w, cx| {
            this.handle_play_idx(first, cx);
        });
        div()
            .id(format!("music-mix-{title}"))
            .w(px(180.))
            .flex_shrink_0()
            .cursor(CursorStyle::PointingHand)
            .on_hover(cx.listener(move |this: &mut MusicView, hovered, _w, cx| {
                this.hover_mix = if *hovered { Some(first) } else { None };
                cx.notify();
            }))
            .on_click(cl)
            .child(cover)
            .child(
                div()
                    .pt(px(8.))
                    .text_size(px(14.))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme::text())
                    .text_ellipsis()
                    .child(title),
            )
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(theme::text3())
                    .text_ellipsis()
                    .child(sub),
            )
            .into_any_element()
    }

    // ---- Library / Liked ----

    fn render_library(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let p = self.player.read(cx);
        let mut groups: Vec<(String, Vec<(usize, Track)>)> = Vec::new();
        let mut seen: Vec<String> = Vec::new();
        for (i, t) in p.library().iter().enumerate() {
            if !filter_pass(&t.title, &t.artist, &self.state.search) {
                continue;
            }
            if let Some(pos) = seen.iter().position(|a| *a == t.artist) {
                groups[pos].1.push((i, t.clone()));
            } else {
                seen.push(t.artist.clone());
                groups.push((t.artist.clone(), vec![(i, t.clone())]));
            }
        }
        let mut body: Vec<AnyElement> = Vec::new();
        body.push(
            div()
                .text_size(px(20.))
                .font_weight(gpui::FontWeight::BOLD)
                .text_color(theme::text())
                .pb(px(16.))
                .child("音乐库")
                .into_any_element(),
        );
        for (artist, rows) in &groups {
            body.push(
                div()
                    .flex()
                    .items_center()
                    .gap_x(px(8.))
                    .pt(px(18.))
                    .pb(px(10.))
                    .text_size(px(15.))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme::accent_light())
                    .child(artist.clone())
                    .child(
                        div()
                            .text_size(px(12.))
                            .font_weight(gpui::FontWeight::NORMAL)
                            .text_color(theme::text3())
                            .child(format!("{}首", rows.len())),
                    )
                    .into_any_element(),
            );
            for (rank, (i, t)) in rows.iter().enumerate() {
                body.push(self.song_row(*i, rank, t, cx));
            }
        }
        if groups.is_empty() {
            body.push(super::empty_state("暂无歌曲", "点开一个音乐文件夹加载曲库"));
        }
        div().flex().flex_col().children(body).into_any_element()
    }

    fn render_liked(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let p = self.player.read(cx);
        let rows: Vec<(usize, Track)> = p
            .library()
            .iter()
            .enumerate()
            .filter(|(i, t)| {
                p.is_liked(*i) && filter_pass(&t.title, &t.artist, &self.state.search)
            })
            .map(|(i, t)| (i, t.clone()))
            .collect();
        let mut body: Vec<AnyElement> = Vec::new();
        body.push(
            div()
                .text_size(px(20.))
                .font_weight(gpui::FontWeight::BOLD)
                .text_color(theme::text())
                .pb(px(16.))
                .child("收藏")
                .into_any_element(),
        );
        for (rank, (i, t)) in rows.iter().enumerate() {
            body.push(self.song_row(*i, rank, t, cx));
        }
        if rows.is_empty() {
            body.push(super::empty_state("还没有收藏", "点歌曲行右边的心形即可收藏"));
        }
        div().flex().flex_col().children(body).into_any_element()
    }

    /// 表头（# / 歌曲 / 专辑 / 时长 / 心形）。
    fn render_song_header(&mut self) -> AnyElement {
        let headers = ["#", "歌曲", "专辑", "时长", ""];
        div()
            .flex()
            .items_center()
            .gap_x(px(12.))
            .px(px(12.))
            .pb(px(10.))
            .border_b_1()
            .border_color(theme::border())
            .mb(px(4.))
            .children(headers.into_iter().map(|h| {
                div()
                    .h(px(20.))
                    .text_size(px(11.))
                    .text_color(theme::text3())
                    .when(h == "#", |d| d.w(px(36.)).text_align(gpui::TextAlign::Center))
                    .when(h == "歌曲", |d| d.flex_grow(2.0))
                    .when(h == "专辑", |d| d.flex_grow(1.0))
                    .when(h == "时长", |d| d.w(px(56.)).text_align(gpui::TextAlign::Right))
                    .when(h == "", |d| d.w(px(40.)).text_align(gpui::TextAlign::Center))
                    .child(h.to_string())
                    .into_any_element()
            }))
            .into_any_element()
    }

    /// 歌曲行：(序号|均衡器) + 缩略图/title/artist + 专辑 + 时长 + 心形。
    /// 点击播该行；播放中行高亮 accent 文字 / 序号换按均衡器小柱。
    fn song_row(
        &mut self,
        idx: usize,
        rank: usize,
        t: &Track,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = self.player.read(cx);
        let is_current = p.current() == Some(idx);
        let playing = p.playing() && is_current;
        let liked = p.is_liked(idx);
        let (light, dark) = theme::gradient_pair_of(t.grad);
        let accent = theme::accent();
        let row = div()
            .id(format!("song-row-{idx}"))
            .debug_selector(move || format!("song-row-{idx}"))
            .flex()
            .items_center()
            .gap_x(px(12.))
            .px(px(12.))
            .py(px(7.))
            .h(px(ROW_H))
            .rounded(px(8.))
            .cursor(CursorStyle::PointingHand)
            .bg(if is_current { theme::bg_card() } else { theme::bg_card().opacity(0.0) })
            .on_click(cx.listener(move |this: &mut MusicView, _ev, _w, cx| {
                this.handle_play_idx(idx, cx);
            }));
        // 序号列：播放中 = 均衡器动画；否则纯数字。
        let num = if playing {
            widgets::eq_bars(true, accent, format!("row-{idx}"))
        } else {
            div()
                .w(px(23.))
                .h(px(14.))
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(13.))
                .text_color(theme::text3())
                .child((rank + 1).to_string())
                .into_any_element()
        };
        row.child(
            div()
                .w(px(36.))
                .flex()
                .items_center()
                .justify_center()
                .child(num),
        )
        .child(
            div()
                .flex_grow(2.0)
                .flex()
                .items_center()
                .gap_x(px(12.))
                .min_w(px(0.))
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
                                        .size(px(18.))
                                        .text_color(gpui::white().opacity(0.5)),
                                ),
                        ),
                )
                .child(
                    div()
                        .min_w(px(0.))
                        .child(
                            div()
                                .text_size(px(14.))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(if is_current {
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
                ),
        )
        .child(
            div()
                .flex_1()
                .text_size(px(13.))
                .text_color(theme::text2())
                .text_ellipsis()
                .child(if t.album.is_empty() {
                    "—".to_string()
                } else {
                    t.album.clone()
                }),
        )
        .child(
            div()
                .w(px(56.))
                .text_size(px(13.))
                .text_color(theme::text3())
                .text_align(gpui::TextAlign::Right)
                .child(crate::model::duration_str(t.duration)),
        )
        .child(
            div()
                .id(format!("song-like-{idx}"))
                .w(px(40.))
                .flex()
                .items_center()
                .justify_center()
                .cursor(CursorStyle::PointingHand)
                .text_color(if liked { theme::pink() } else { theme::text3() })
                .on_click(cx.listener(move |this: &mut MusicView, _ev, _w, cx| {
                    this.toggle_like(idx, cx);
                    cx.stop_propagation();
                }))
                .child(
                    IconView::new(if liked { MusicIcon::Heart } else { MusicIcon::HeartBorder })
                        .filled(liked)
                        .size(px(17.))
                        .text_color(if liked { theme::pink() } else { theme::text3() }),
                ),
        )
        .into_any_element()
    }
}

/// 搜索命中判定（title/artist 大小写不敏感）。
fn filter_pass(title: &str, artist: &str, q: &str) -> bool {
    let ql = q.trim().to_lowercase();
    if ql.is_empty() {
        return true;
    }
    title.to_lowercase().contains(&ql) || artist.to_lowercase().contains(&ql)
}

/// 当前小时（本地时钟；无系统依赖的简单取时）。
fn chrono_now_hour() -> u32 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // 简化：UTC+8 小时（中国音乐场景为主）。
    (((now + 8 * 3600) % 86400) / 3600) as u32
}

/// 搜索框 canvas：注册 IME 输入句柄（仅搜索持焦时）到 view 的 `EntityInputHandler`。
/// 必须在**paint 期**调用 `window.handle_input`（gpui `debug_assert_paint` 校验）；canvas
/// 的 paint 回调刚好位于 paint 阶段，故把注册放 paint、prepaint 留空。
fn canvas_ime_handler(
    entity: gpui::Entity<MusicView>,
    search_focus: gpui::FocusHandle,
) -> AnyElement {
    use gpui::{ElementInputHandler, canvas};
    canvas(
        |_, _, _| {},
        move |bounds, _t, window, app| {
            if search_focus.is_focused(window) {
                let handler = ElementInputHandler::new(bounds, entity.clone());
                window.handle_input(&search_focus, handler, app);
            }
        },
    )
    .into_any_element()
}
