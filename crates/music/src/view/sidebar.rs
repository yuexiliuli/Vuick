//! 左侧导航栏（240px）：logo + 菜单组（菜单/我的/歌单）+ 底部用户卡。
//!
//! 复刻原型 `.sidebar`：bg_primary 底 + 右缘发丝线；菜单/我的组为「导航项」，
//! 首页/音乐库/收藏/最近播放/发现/电台/本地下载可切换（高亮：accent 渐变底 + 左缘 3px
//! 竖条）；歌单条目点击打开对应歌单详情（[`MainSection::Playlist`]）；条目色点 = 组渐变首色。

use gpui::{
    AnyElement, Context, CursorStyle, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, Window, div, px,
};

use super::{MainSection, MusicView, theme};
use ui::icon::{Icon, IconName};

/// 导航项行为：切换分区 / 打开歌单详情（`usize` = `mix_groups` 下标）。
/// `OpenPlaylist` 保留为「歌单型导航项」的动作枚举；当前歌单条目（C3）在
/// [`Self::render_playlists`] 内直判 `section == Playlist(i)` + 直调 `set_section`，
/// 故该变体暂未构造（枚举口径齐备、防后续新导航项误走 toast 路径）。
#[derive(Clone, Copy)]
#[allow(dead_code)]
enum NavAction {
    Section(MainSection),
    OpenPlaylist(usize),
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

    /// logo：渐变方块 + 渐变文字「Vuick Music」（原型 `.logo`）。
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
                    .relative()
                    .debug_selector(|| "music-logo".into())
                    // 背景渐变 absolute-inset-0 铺满（135deg accent→pink，对齐原型 `.logo-icon`；
                    // gradient_cover 的 (light,dark) = dark 停 0% / light 停 100%）。
                    .child(theme::gradient_cover((theme::pink(), theme::accent()), 10.0))
                    // 前景白 disc 居中叠在渐变上（不再作 flex 兄弟，避免被挤到渐变旁）。
                    .child(
                        div()
                            .absolute()
                            .inset_0()
                            .flex()
                            .items_center()
                            .justify_center()
                            .debug_selector(|| "music-logo-fg".into())
                            .child(
                                Icon::new(IconName::Disc)
                                    .size(px(20.))
                                    .text_color(theme::text()),
                            ),
                    ),
            )
            .child(
                // 渐变文字 GPUI 不支持 CSS background-clip，退化为 accent_light 单色近似。
                div()
                    .text_size(px(18.))
                    .font_weight(gpui::FontWeight::BOLD)
                    .text_color(theme::accent_light())
                    .child("Vuick Music"),
            )
            .into_any_element()
    }

    /// 菜单组：组标题 + 一组导航项（原型 `.nav-section`/`.nav-item`）。
    fn render_menu_group(
        &mut self,
        title: &str,
        cx: &mut Context<Self>,
        items: &mut Vec<(IconName, &'static str, NavAction)>,
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
        icon: IconName,
        label: &'static str,
        action: NavAction,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let active = match action {
            NavAction::Section(s) => self.state.section == s,
            NavAction::OpenPlaylist(i) => self.state.section == MainSection::Playlist(i),
        };
        let (fg, bg) = if active {
            (theme::text(), theme::accent().opacity(0.18))
        } else {
            (theme::text2(), theme::bg_hover())
        };
        let cl = cx.listener(move |this: &mut MusicView, _ev, _w, cx| match action {
            NavAction::Section(s) => this.set_section(s, cx),
            NavAction::OpenPlaylist(i) => this.set_section(MainSection::Playlist(i), cx),
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
            .child(Icon::new(icon).size(px(20.)).text_color(fg))
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

    /// 「菜单」组：首页/发现/音乐库/电台（均可切分区）。
    fn nav_items_menu(&self) -> Vec<(IconName, &'static str, NavAction)> {
        vec![
            (IconName::Home, "首页", NavAction::Section(MainSection::Home)),
            (IconName::Explore, "发现", NavAction::Section(MainSection::Discover)),
            (IconName::Library, "音乐库", NavAction::Section(MainSection::Library)),
            (IconName::Radio, "电台", NavAction::Section(MainSection::Radio)),
        ]
    }

    /// 「我的」组：收藏/最近播放/本地下载（均可切分区）。
    fn nav_items_mine(&self) -> Vec<(IconName, &'static str, NavAction)> {
        vec![
            (IconName::Heart, "收藏", NavAction::Section(MainSection::Liked)),
            (IconName::History, "最近播放", NavAction::Section(MainSection::Recent)),
            (IconName::Download, "本地下载", NavAction::Section(MainSection::Downloads)),
        ]
    }

    /// 「歌单」组：由 `mix_groups` 前 5 项生成（色点 = 组渐变首色）；点击打开对应歌单详情
    /// （当前项高亮：左缘 accent 竖条 + accent 底，观感对齐其他导航项）。
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
        for (i, (name, dot)) in groups.into_iter().enumerate() {
            let active = self.state.section == MainSection::Playlist(i);
            let (fg, bg) = if active {
                (theme::text(), theme::accent().opacity(0.18))
            } else {
                (theme::text2(), theme::bg_hover())
            };
            let id = format!("music-playlist-{name}");
            let sel = name.clone();
            let cl = cx.listener(move |this: &mut MusicView, _ev, _w, cx| {
                this.set_section(MainSection::Playlist(i), cx);
            });
            title.push(
                div()
                    .id(id)
                    .debug_selector(move || format!("music-playlist-{sel}"))
                    .flex()
                    .items_center()
                    .gap_x(px(10.))
                    .px(px(24.))
                    .py(px(8.))
                    .relative()
                    .cursor(CursorStyle::PointingHand)
                    .bg(bg.opacity(if active { 0.25 } else { 0.0 }))
                    .on_click(cl)
                    .child(
                        // 左缘 3px accent 竖条（active，对齐其他导航项）。
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
                    .child(div().w(px(8.)).h(px(8.)).rounded(px(4.)).bg(dot))
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(13.))
                            .text_ellipsis()
                            .text_color(fg)
                            .child(name.to_string()),
                    )
                    .into_any_element(),
            );
        }
        div().flex().flex_col().flex_1().children(title).into_any_element()
    }

    /// 底部用户卡（原型 `.sidebar-footer`；用户名取本机用户名，头像为其首字母）。
    fn render_user_card(&mut self) -> AnyElement {
        let name = super::local_username();
        let initial = super::username_initial(&name);
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
                    .relative()
                    .debug_selector(|| "music-user-avatar".into())
                    // 背景渐变 absolute-inset-0 铺满（135deg accent→pink，对齐原型 `.user-avatar`；
                    // gradient_cover 的 (light,dark) = dark 停 0% / light 停 100%）。
                    .child(theme::gradient_cover((theme::pink(), theme::accent()), 16.0))
                    // 前景「用户名首字母」居中叠在渐变上（不再作 flex 兄弟，避免错位）。
                    .child(
                        div()
                            .absolute()
                            .inset_0()
                            .flex()
                            .items_center()
                            .justify_center()
                            .debug_selector(|| "music-user-avatar-fg".into())
                            .child(
                                div()
                                    .text_size(px(14.))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(theme::text())
                                    .child(initial),
                            ),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .child(
                        div()
                            .text_size(px(13.))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme::text())
                            .child(name),
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
