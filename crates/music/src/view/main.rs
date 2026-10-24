//! 主内容区（flex_1）：头部（问候 + 搜索 + 圆钮）+ 按分区的内容。
//!
//! - Home：横滑「我的推荐」Mix 卡（渐变封面 + hover 播放浮层，点击播放组内第一首）
//!   + 「热门歌曲」表格；
//! - Library：按艺术家分组的歌曲列表（组头 + 行）；
//! - Liked：只列已收藏歌曲。
//! 搜索框：ASCII 可打印键直接落文本（`key_char`），中文走 IME（view 的
//! `EntityInputHandler`）：当前简化实现支持即输即滤。
//!
//! **列表虚拟化 + 零克隆**（性能优化第二步）：Library/Liked/Home 三个 O(N) 列表不再每帧全量
//! 物化 + 深克隆 `Vec<Track>`——改为「过滤/分组显示模型」（[`MainList`]，只存库下标/序号，
//! 渲染期按索引借用 `player.library()`）缓存（搜索词 + `player.mutation()` 不变则复用），
//! 滚动容器 `track_scroll(&main_scroll)` 按偏移二分反查可视行窗（±缓冲行）只物化可见项，
//! 上下定高垫片保持内容总高 = 总高（滚轮全范围可滚）。组头/行高固定（[`GROUP_H`]/[`ROW_H`]）
//! 是前缀反查的前提。曲库越大收益越大（首行窗正确、点击行下标映射不漂移）。

use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, Context, CursorStyle, InteractiveElement as _, IntoElement, ObjectFit,
    ParentElement as _, RenderImage, StatefulInteractiveElement as _, Styled as _, StyledImage,
    Window, div, img, px,
};

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use crate::model::Track;

use super::icon::{IconView, MusicIcon};
use super::{MainSection, MusicView, theme, widgets};

/// 歌曲表行高（px，近似原型的行内距）。
const ROW_H: f32 = 52.0;

/// 艺术家分组头高（px）：虚拟化需确定性行高以做前缀反查行窗，原「pt18+pb10+15px 文本」≈49
/// 归一为整 48、垂直居中（外观近似；`GROUP_H`/`ROW_H` 固定是「仅物化可视窗」的前提）。
const GROUP_H: f32 = 48.0;

/// 可视窗上下各多物化 N 行（缓冲）：快速滚动不至于露白/闪。
const VISIBLE_BUFFER: usize = 6;

/// 首帧未布局（scroll 容器 viewport=0）时的兜底物化行数（防空窗）。
const FALLBACK_FIRST_ROWS: usize = 8;

/// 列表显示项（一行 = 分组头或歌行）。只存**库下标 + 组内序号**，不克隆 `Track`。
#[derive(Clone, Debug)]
pub(super) enum MainItem {
    /// Library 艺术家分组头（固定高 [`GROUP_H`]）。
    GroupHeader { artist: Arc<str>, count: usize },
    /// 歌行：`idx` = 真实库下标（点击 `handle_play_idx(idx)` 映射），`rank` = 组内/过滤序。
    Row { idx: usize, rank: usize },
}

/// 主区「过滤+分组」后的扁平显示模型（一次构建、缓存复用）：
/// - `items`：显示行序列（Library 含组头+歌行；Home/Liked 纯歌行）；
/// - `prefix`：前缀像素高（`prefix[i]` = 前 i 项总高 px·i32，严格递增）——`visible_range`
///   二分反查「滚动偏移/视口 → [first,last) 可视窗」，O(log N)；
/// - `total`：歌行总数（表头「共 N 首」计数 / 空态判定）。
pub(super) struct MainList {
    pub items: Vec<MainItem>,
    prefix: Vec<i32>,
    pub total: usize,
}

impl MainList {
    fn new(items: Vec<MainItem>, total: usize) -> Self {
        let mut prefix = Vec::with_capacity(items.len() + 1);
        let mut acc = 0i32;
        prefix.push(0);
        for it in &items {
            acc += match it {
                MainItem::Row { .. } => ROW_H as i32,
                MainItem::GroupHeader { .. } => GROUP_H as i32,
            };
            prefix.push(acc);
        }
        Self { items, prefix, total }
    }

    /// 可视行窗 `[first,last)`（item 下标，含缓冲）：`scroll_y` = 容器向下滚动 px（≥0），
    /// `viewport` = 容器可视高。按 prefix 二分反查；首帧未布局（viewport=0）用
    /// [`FALLBACK_FIRST_ROWS`] 兜底；钳位 + 至少 1 行、空态 (0,0)。
    fn visible_range(&self, scroll_y: f32, viewport: f32) -> (usize, usize) {
        let n = self.items.len();
        if n == 0 {
            return (0, 0);
        }
        let eff_viewport = if viewport <= 0.0 {
            FALLBACK_FIRST_ROWS as f32 * ROW_H
        } else {
            viewport
        };
        let s = scroll_y.max(0.0);
        let e = s + eff_viewport;
        // 最高项满足 prefix[i] <= s 的个数 → 首个「底 > s」的项 = 首个可见项（钳到底部时收尾）。
        let first = self
            .prefix
            .partition_point(|&p| (p as f32) <= s)
            .min(n)
            .saturating_sub(1);
        // 从 first 向后走到「顶 >= e」→ last（开区间）。
        let mut last = first;
        while last < n && (self.prefix[last + 1] as f32) < e {
            last += 1;
        }
        last = (last + 1).min(n);
        // 缓冲扩展。
        let first_buf = first.saturating_sub(VISIBLE_BUFFER);
        let last_buf = (last + VISIBLE_BUFFER).min(n);
        (first_buf, last_buf.max(first_buf + 1))
    }

    /// 总内容高（px，滚动量程）：顶部/底部垫片之和保底此值。
    fn total_h(&self) -> f32 {
        self.prefix[self.items.len()] as f32
    }
}

/// 40px 列表缩略图（歌曲表行）：有封面 → 铺图（`ObjectFit::Cover`）；无 → 渐变 + 音符回退。
fn song_thumb_div(cover: Option<Arc<RenderImage>>, (light, dark): (gpui::Hsla, gpui::Hsla)) -> AnyElement {
    let base: AnyElement = match cover {
        Some(c) => div()
            .size_full()
            .child(
                img(gpui::ImageSource::Render(c))
                    .object_fit(ObjectFit::Cover)
                    .w_full()
                    .h_full(),
            )
            .into_any_element(),
        None => div()
            .size_full()
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
            )
            .into_any_element(),
    };
    div()
        .w(px(40.))
        .h(px(40.))
        .rounded(px(6.))
        .relative()
        .flex_shrink_0()
        .overflow_hidden()
        .child(base)
        .into_any_element()
}

impl super::MusicView {
    /// 主内容区整块：头部固定（问候+搜索+圆钮），其下按分区渲染（分区标题/表头固定，
    /// 只有列表区滚动虚拟化——`main_scroll` 是单一活跃滚动容器句柄）。
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
        let list = self.main_list_for(MainSection::Home, cx);
        div()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
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
            .child(if list.total == 0 {
                super::empty_state("没有匹配的歌曲", "清空搜索或换个关键词").into_any_element()
            } else {
                self.render_list(&list, cx)
            })
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
        // Mix 卡封面：组内第一首有真实封面 → 铺图；无 → 渐变 + 音符回退。
        let first_cover = self
            .player
            .read(cx)
            .library()
            .get(first)
            .and_then(|t| t.cover_path.as_deref())
            .and_then(|p| self.cover_for(Some(p)));
        let cover_base: AnyElement = match first_cover {
            Some(c) => div()
                .size_full()
                .child(
                    img(gpui::ImageSource::Render(c))
                        .object_fit(ObjectFit::Cover)
                        .w_full()
                        .h_full(),
                )
                .into_any_element(),
            None => div()
                .size_full()
                .child(theme::gradient_cover((light, dark), 14.0))
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
                .into_any_element(),
        };
        let cover = div()
            .w(px(180.))
            .h(px(180.))
            .rounded(px(14.))
            .relative()
            .overflow_hidden()
            .mx_auto()
            .child(cover_base)
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
        let list = self.main_list_for(MainSection::Library, cx);
        div()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .child(self.section_title("音乐库"))
            .child(if list.total == 0 {
                super::empty_state("暂无歌曲", "点开一个音乐文件夹加载曲库").into_any_element()
            } else {
                self.render_list(&list, cx)
            })
            .into_any_element()
    }

    fn render_liked(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let list = self.main_list_for(MainSection::Liked, cx);
        div()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .child(self.section_title("收藏"))
            .child(if list.total == 0 {
                super::empty_state("还没有收藏", "点歌曲行右边的心形即可收藏").into_any_element()
            } else {
                self.render_list(&list, cx)
            })
            .into_any_element()
    }

    /// 分区标题（音乐库/收藏）：固定于列表滚动区之上（同表头/搜索——只列表滚动）。
    fn section_title(&self, title: &str) -> AnyElement {
        div()
            .text_size(px(20.))
            .font_weight(gpui::FontWeight::BOLD)
            .text_color(theme::text())
            .pb(px(16.))
            .child(title.to_string())
            .into_any_element()
    }

    /// 虚拟化列表（[`MainList`] 可视窗 → 物化 ± 缓冲行）：顶部/底部定高垫片保内容总高
    /// （= [`MainList::total_h`]，全范围可滚）；屏外行不物化、不画渐变、不建 debug_selector。
    fn render_list(&mut self, list: &Rc<MainList>, cx: &mut Context<Self>) -> AnyElement {
        let scroll_y = (-self.main_scroll.offset().y.as_f32()).max(0.0);
        let viewport = self.main_scroll.bounds().size.height.as_f32();
        let (first, last) = list.visible_range(scroll_y, viewport);
        let top_h = list.prefix[first] as f32;
        let bottom_h = (list.total_h() - list.prefix[last] as f32).max(0.0);
        let items = &list.items;
        div()
            .id("music-list")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .track_scroll(&self.main_scroll)
            .child(div().h(px(top_h)))
            .children((first..last).map(|i| match &items[i] {
                MainItem::Row { idx, rank } => self.song_row(*idx, *rank, cx),
                MainItem::GroupHeader { artist, count } => self.group_header(artist, *count),
            }))
            .child(div().h(px(bottom_h)))
            .into_any_element()
    }

    /// 艺术家分组头（固定高 [`GROUP_H`]，竖向居中）：组标题 + 首数；样式色/字号同旧。
    fn group_header(&self, artist: &str, count: usize) -> AnyElement {
        let sel = format!("song-group-{artist}");
        div()
            .debug_selector(move || sel.clone())
            .h(px(GROUP_H))
            .flex()
            .items_center()
            .gap_x(px(8.))
            .text_size(px(15.))
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .text_color(theme::accent_light())
            .child(artist.to_string())
            .child(
                div()
                    .text_size(px(12.))
                    .font_weight(gpui::FontWeight::NORMAL)
                    .text_color(theme::text3())
                    .child(format!("{count}首")),
            )
            .into_any_element()
    }

    /// 取分区「过滤/分组显示模型」缓存：搜索词 + `player.mutation()` 不变则复用（免每帧
    /// 重过滤 + `Vec<Track>` 深克隆）；变化才重建（纯索引模型，构建也不克隆 Track）。
    fn main_list_for(&mut self, section: MainSection, cx: &mut Context<Self>) -> Rc<MainList> {
        let q = self.state.search.clone();
        let key = (section, q);
        let mutation = self.player.read(cx).mutation();
        if let Some((rev, cached)) = self.main_list_cache.get(&key) {
            if *rev == mutation {
                return cached.clone();
            }
        }
        let list = {
            let p = self.player.read(cx);
            let liked = if section == MainSection::Liked {
                Some(p.liked_paths())
            } else {
                None
            };
            build_main_list(section, p.library(), &key.1, liked)
        };
        self.main_list_cache
            .insert(key, (mutation, list.clone()));
        list
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
    /// 虚拟化侧只对**可视窗行**调用；内部按 `idx` 借库引用读显示字段（只克隆文本字段，
    /// 不克隆整个 `Track`——性能优化第二步）。库越界（缓存未刷新拍）兜底空行。
    fn song_row(&mut self, idx: usize, rank: usize, cx: &mut Context<Self>) -> AnyElement {
        let p = self.player.read(cx);
        let is_current = p.current() == Some(idx);
        let playing = p.playing() && is_current;
        let liked = p.is_liked(idx);
        // 短暂借用读显示字段后即释放 guard（防与下方 `cx.listener` 双借冲突）。
        let (title, artist, album, duration, grad, cover_path) = {
            let Some(t) = p.library().get(idx) else {
                return div().h(px(ROW_H)).into_any_element();
            };
            (
                t.title.clone(),
                t.artist.clone(),
                t.album.clone(),
                t.duration,
                t.grad,
                t.cover_path.clone(),
            )
        };
        let (light, dark) = theme::gradient_pair_of(grad);
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
                .child(song_thumb_div(self.cover_for(cover_path.as_deref()), (light, dark)))
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
                                .child(title),
                        )
                        .child(
                            div()
                                .text_size(px(12.))
                                .text_color(theme::text3())
                                .text_ellipsis()
                                .child(artist),
                        ),
                ),
        )
        .child(
            div()
                .flex_1()
                .text_size(px(13.))
                .text_color(theme::text2())
                .text_ellipsis()
                .child(if album.is_empty() {
                    "—".to_string()
                } else {
                    album
                }),
        )
        .child(
            div()
                .w(px(56.))
                .text_size(px(13.))
                .text_color(theme::text3())
                .text_align(gpui::TextAlign::Right)
                .child(crate::model::duration_str(duration)),
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

/// 构建主区某分区的过滤/分组显示模型（纯索引，零 `Track` 克隆；`main_list_for` 在
/// 搜索词 × 库/收藏变更加速键变化时调用）。Liked 需 `liked`（已收藏路径集）过滤。
fn build_main_list(
    section: MainSection,
    lib: &[Track],
    q: &str,
    liked: Option<&std::collections::HashSet<String>>,
) -> Rc<MainList> {
    match section {
        MainSection::Library => {
            // 按艺术家分组：HashMap 消 `seen.position` 线性扫；组内 rank 从 0 起。
            let mut artist_to_group: HashMap<&str, usize> = HashMap::new();
            let mut groups: Vec<(Arc<str>, Vec<(usize, usize)>)> = Vec::new();
            for (i, t) in lib.iter().enumerate() {
                if !filter_pass(&t.title, &t.artist, q) {
                    continue;
                }
                if let Some(&gi) = artist_to_group.get(t.artist.as_str()) {
                    let rows = &mut groups[gi].1;
                    rows.push((i, rows.len()));
                } else {
                    artist_to_group.insert(t.artist.as_str(), groups.len());
                    groups.push((t.artist.clone().into(), vec![(i, 0)]));
                }
            }
            let mut items = Vec::new();
            let mut total = 0usize;
            for (artist, rows) in &groups {
                items.push(MainItem::GroupHeader {
                    artist: artist.clone(),
                    count: rows.len(),
                });
                items.extend(rows.iter().map(|&(idx, rank)| MainItem::Row { idx, rank }));
                total += rows.len();
            }
            Rc::new(MainList::new(items, total))
        }
        MainSection::Liked => {
            let mut items = Vec::new();
            let mut rank = 0usize;
            for (i, t) in lib.iter().enumerate() {
                let is_liked = liked.is_some_and(|l| {
                    l.contains(&t.path.to_string_lossy().to_string())
                });
                if is_liked && filter_pass(&t.title, &t.artist, q) {
                    items.push(MainItem::Row { idx: i, rank });
                    rank += 1;
                }
            }
            Rc::new(MainList::new(items, rank))
        }
        MainSection::Home => {
            let mut items = Vec::new();
            let mut rank = 0usize;
            for (i, t) in lib.iter().enumerate() {
                if filter_pass(&t.title, &t.artist, q) {
                    items.push(MainItem::Row { idx: i, rank });
                    rank += 1;
                }
            }
            Rc::new(MainList::new(items, rank))
        }
    }
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
