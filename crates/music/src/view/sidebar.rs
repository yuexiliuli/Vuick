//! 左侧导航栏（240px）：logo + 菜单组（菜单/我的/歌单）+ 底部用户卡。
//!
//! 复刻原型 `.sidebar`：bg_primary 底 + 右缘发丝线；菜单/我的组为「导航项」，
//! 首页/音乐库/收藏可切换（高亮：accent 渐变底 + 左缘 3px 竖条）；电台/最近播放/
//! 本地下载/歌单只弹 toast「即将到来」；歌单条目由 `mix_groups` 前几项生成
//! （色点 = 组渐变首色）。

use gpui::{
    AnyElement, Context, CursorStyle, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, Window, div, px,
};

use super::icon::{IconView, MusicIcon};
use super::{MainSection, MusicView, theme};

/// 导航项行为：切换分区 / 只弹 toast（未实现功能）。
#[derive(Clone, Copy)]
enum NavAction {
    Section(MainSection),
    ComingSoon(&'static str),
}

impl super::MusicView {
    /// 左侧导航栏整块。
    pub(super) fn render_sidebar(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let menu_border = theme::border();
        div()
            .id("music-sidebar")
            .debug_selector(|| "music-sidebar".into())
            .w(px(240.))
            .flex_shrink_0()
            .flex()
            .flex_col()
            .bg(theme::bg_primary())
            .border_r_1()
            .border_color(menu_border)
            .overflow_y_scroll()
            .child(self.render_logo())
            .child(self.render_menu_group("菜单", cx, &mut self.nav_items_menu()))
            .child(self.render_menu_group("我的", cx, &mut self.nav_items_mine()))
            .child(self.render_playlists(cx))
            .child(self.render_user_card())
            .into_any_element()
    }

    /// logo：渐变方块 + 渐变文字「Orbitrum Player」（原型 `.logo`）。
    fn render_logo(&mut self) -> AnyElement {
        div()
            .flex()
            .items_center()
            .gap_x(px(10.))
            .px(px(24.))
            .pb(px(28.))
            .child(
                div()
                    .w(px(36.))
                    .h(px(36.))
                    .rounded(px(10.))
                    .flex()
                    .items_center()
                    .justify_center()
                    // 渐变方块：canvas 画 accent→pink 135deg（原型 `.logo-icon`）。
                    .child(
                        div()
                            .size_full()
                            .child(theme::gradient_cover(
                                (theme::pink(), theme::accent()),
                                10.0,
                            )),
                    )
                    .child(
                        IconView::new(MusicIcon::Disc)
                            .size(px(20.))
                            .text_color(gpui::white())
                            .filled(true),
                    ),
            )
            .child(
                // 渐变文字 GPUI 不支持 CSS background-clip，退化为 accent_light 单色近似。
                div()
                    .text_size(px(18.))
                    .font_weight(gpui::FontWeight::BOLD)
                    .text_color(theme::accent_light())
                    .child("Orbitrum Player"),
            )
            .into_any_element()
    }

    /// 菜单组：组标题 + 一组导航项（原型 `.nav-section`/`.nav-item`）。
    fn render_menu_group(
        &mut self,
        title: &str,
        cx: &mut Context<Self>,
        items: &mut Vec<(MusicIcon, &'static str, NavAction)>,
    ) -> AnyElement {
        let mut children: Vec<AnyElement> = Vec::new();
        children.push(
            div()
                .px(px(24.))
                .pb(px(12.))
                .text_size(px(11.))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(theme::text3())
                .child(title.to_string())
                .into_any_element(),
        );
        for (icon, label, action) in items {
            children.push(self.render_nav_item(*icon, label, *action, cx));
        }
        div()
            .flex()
            .flex_col()
            .pb(px(24.))
            .children(children)
            .into_any_element()
    }

    /// 单个导航项（含 active 高亮 + 左缘竖条）。
    fn render_nav_item(
        &mut self,
        icon: MusicIcon,
        label: &'static str,
        action: NavAction,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let active = match action {
            NavAction::Section(s) => self.state.section == s,
            _ => false,
        };
        let (fg, bg) = if active {
            (theme::text(), theme::accent().opacity(0.18))
        } else {
            (theme::text2(), theme::bg_hover())
        };
        let cl = cx.listener(move |this: &mut MusicView, _ev, _w, cx| match action {
            NavAction::Section(s) => this.set_section(s, cx),
            NavAction::ComingSoon(t) => this.toast_msg(t, cx),
        });
        div()
            .id(label)
            .debug_selector(move || format!("music-nav-{label}"))
            .flex()
            .items_center()
            .gap_x(px(14.))
            .px(px(24.))
            .py(px(10.))
            .relative()
            .cursor(CursorStyle::PointingHand)
            .bg(bg.opacity(if active { 0.25 } else { 0.0 }))
            .on_click(cl)
            .child(
                // 左缘 3px accent 竖条（active）。
                div()
                    .absolute()
                    .left_0()
                    .top_0()
                    .bottom_0()
                    .w(px(3.))
                    .rounded_l(px(3.))
                    .bg(theme::accent())
                    .opacity(if active { 1.0 } else { 0.0 }),
            )
            .child(IconView::new(icon).size(px(20.)).text_color(fg))
            .child(
                div()
                    .flex_1()
                    .text_size(px(14.))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(fg)
                    .child(label.to_string()),
            )
            .into_any_element()
    }

    /// 「菜单」组：首页（可切）/ 发现（toast）/ 音乐库（可切）/ 电台（toast，NEW 徽标省）。
    fn nav_items_menu(&self) -> Vec<(MusicIcon, &'static str, NavAction)> {
        vec![
            (MusicIcon::Home, "首页", NavAction::Section(MainSection::Home)),
            (MusicIcon::Explore, "发现", NavAction::ComingSoon("发现即将到来")),
            (MusicIcon::Library, "音乐库", NavAction::Section(MainSection::Library)),
            (MusicIcon::Radio, "电台", NavAction::ComingSoon("电台即将到来")),
        ]
    }

    /// 「我的」组：收藏（可切）/ 最近播放 / 本地下载（toast）。
    fn nav_items_mine(&self) -> Vec<(MusicIcon, &'static str, NavAction)> {
        vec![
            (MusicIcon::Heart, "收藏", NavAction::Section(MainSection::Liked)),
            (MusicIcon::History, "最近播放", NavAction::ComingSoon("最近播放即将到来")),
            (MusicIcon::Download, "本地下载", NavAction::ComingSoon("本地下载即将到来")),
        ]
    }

    /// 「歌单」组：由 `mix_groups` 前 5 项生成（色点 = 组渐变首色），点击 toast。
    fn render_playlists(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let mut title = vec![div()
            .px(px(24.))
            .pb(px(12.))
            .text_size(px(11.))
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .text_color(theme::text3())
            .child("歌单")
            .into_any_element()];
        let groups: Vec<(String, gpui::Hsla)> = self
            .player
            .read(cx)
            .mix_groups()
            .iter()
            .take(5)
            .map(|g| (g.title.clone(), theme::from_hex_str(&g.grad.0)))
            .collect();
        for (name, dot) in groups {
            let id = format!("music-playlist-{name}");
            let tip = name.clone();
            let cl = cx.listener(move |this: &mut MusicView, _ev, _w, cx| {
                this.toast_msg(format!("打开歌单：{tip}"), cx);
            });
            let sel = name.clone();
            title.push(
                div()
                    .id(id)
                    .debug_selector(move || format!("music-playlist-{sel}"))
                    .flex()
                    .items_center()
                    .gap_x(px(10.))
                    .px(px(24.))
                    .py(px(8.))
                    .cursor(CursorStyle::PointingHand)
                    .text_color(theme::text2())
                    .on_click(cl)
                    .child(div().w(px(8.)).h(px(8.)).rounded(px(4.)).bg(dot))
                    .child(
                        div()
                            .text_size(px(13.))
                            .text_ellipsis()
                            .child(name.to_string()),
                    )
                    .into_any_element(),
            );
        }
        div().flex().flex_col().flex_1().children(title).into_any_element()
    }

    /// 底部用户卡（原型 `.sidebar-footer`）。
    fn render_user_card(&mut self) -> AnyElement {
        div()
            .flex()
            .items_center()
            .gap_x(px(12.))
            .px(px(24.))
            .py(px(16.))
            .border_t_1()
            .border_color(theme::border())
            .child(
                div()
                    .w(px(32.))
                    .h(px(32.))
                    .rounded(px(16.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        div()
                            .size_full()
                            .child(theme::gradient_cover(
                                (theme::pink(), theme::accent()),
                                16.0,
                            )),
                    )
                    .child(div().text_size(px(14.)).font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(gpui::white()).child("K")),
            )
            .child(
                div()
                    .flex_1()
                    .child(
                        div()
                            .text_size(px(13.))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme::text())
                            .child("Koa"),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(theme::green())
                            .child("● 在线"),
                    ),
            )
            .into_any_element()
    }
}
