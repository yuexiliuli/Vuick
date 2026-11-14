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
    ParentElement as _, RenderImage, ScrollWheelEvent, StatefulInteractiveElement as _, Styled as _,
    StyledImage, Window, div, img, px,
};

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use crate::model::Track;

use super::icon::{IconView, MusicIcon};
use super::{MainSection, MusicView, theme};
use ui::icon::{Icon, IconName};
use ui::scrollbar::{Scrollbar as UiScrollbar, ScrollbarMode};

/// 主区横向内边距（px，左右同值）：`#music-main` 内容区留白；overlay 滚动条用
/// `right(-MAIN_PAD_X)` 贴主区右缘（跨过右 padding），两端共用同一常量防漂移。
const MAIN_PAD_X: f32 = 28.0;

/// 歌曲表行高（px，近似原型的行内距）。
const ROW_H: f32 = 52.0;

/// 艺术家分组头高（px）：虚拟化需确定性行高以做前缀反查行窗，原「pt18+pb10+15px 文本」≈49
/// 归一为整 48、垂直居中（外观近似；`GROUP_H`/`ROW_H` 固定是「仅物化可视窗」的前提）。
const GROUP_H: f32 = 48.0;

/// Home「我的推荐」标题行高（px，虚拟行固定高；「我的推荐」+「查看全部 →」）。
pub(super) const REC_TITLE_H: f32 = 40.0;

/// Home Mix 卡横滑行高（px，虚拟行固定高）：180 封面 + 标题/副标题 + 底留白，
/// 内嵌横向滚动容器（`overflow_x_scroll` + `track_scroll(&main_mix_scroll)`）。行高固定是
/// 前缀反查的前提——内容溢出由行内横滚裁剪，不求全卡高度。
pub(super) const REC_CARDS_H: f32 = 240.0;

/// Home「热门歌曲」标题（+共 N 首）+ 歌曲表头行高（px，虚拟行固定高）：标题行
/// 与表头随整页滚动；表头只一行、滚走合理（`render_song_header` 并入本行）。
pub(super) const HOT_TITLE_H: f32 = 90.0;

/// Home 空态占位行高（px，虚拟行固定高）：空库/扫描中/搜索 0 命中时仍在列表内占位
/// （稳定竖向布局，随整页可滚、不 panic）。
pub(super) const HOME_EMPTY_H: f32 = 120.0;

/// 可视窗上下各多物化 N 行（缓冲）：快速滚动不至于露白/闪。
const VISIBLE_BUFFER: usize = 6;

/// 主区列表 overlay 滚动条（[`ui::Scrollbar`] Auto）：16px 轨道条右缘贴「内容右缘 +
/// [`MAIN_PAD_X`] + 4px」——`ui::Scrollbar` 的 thumb 常态 6px 但右内缩 4px（THUMB_INSET
/// 语义），条右缘再 +4 才让 thumb 右缘精确落回旧自绘 6px 条位置（原观感位移契约，勿改）。
const SB_TRACK_W: f32 = 16.0;

/// 主区列表滚动条 Auto 空闲隐藏延时（ms）：最后一次滚动活动（滚轮/拖拽 thumb）后 3s 无新
/// 活动 → 隐藏（「鼠标移动不算动作」——列表内移动不刷新计时，由 `ui::Scrollbar` 空闲定时器
/// 驱动）。
const SCROLLBAR_HIDE_MS: u64 = 3000;

/// 滚动条 thumb 最小高（**原始滑块长**，px）：`ui::Scrollbar` 的 `thumb_min` 作用于原始
/// 滑块长、两端各缩 4px inset 后的可视高才是合约口径 → 设为 32（painted = 32 − 2×4 = 24，
/// 满足「可见最小高 ≥24px」契约；旧自绘即 24 可视）。列表极长时 thumb 不致过小。
const SCROLLBAR_THUMB_MIN_H: f32 = 32.0;

/// 首帧未布局（scroll 容器 viewport=0）时的兜底物化行数（防空窗）。
const FALLBACK_FIRST_ROWS: usize = 8;

/// 列表显示项（一行 = 分组头或歌行）。只存**库下标 + 组内序号**，不克隆 `Track`。
#[derive(Clone, Debug)]
pub(super) enum MainItem {
    /// Library 艺术家分组头（固定高 [`GROUP_H`]）。
    GroupHeader { artist: Arc<str>, count: usize },
    /// 歌行：`idx` = 真实库下标（点击 `handle_play_idx(idx)` 映射），`rank` = 组内/过滤序。
    Row { idx: usize, rank: usize },
    // ---- Home 固定高头部虚拟项（只由 `build_main_list(Home)` 插在歌曲行前；Library/Liked 不插）----
    /// Home「我的推荐」标题行（固定高 [`REC_TITLE_H`]）。
    HomeRecTitle,
    /// Home Mix 卡横滑行（固定高 [`REC_CARDS_H`]；内部横向滚动，竖向滚轮在此行被拦截转横向）。
    HomeMixRow,
    /// Home「热门歌曲」标题（共 N 首）+ 歌曲表头行（固定高 [`HOT_TITLE_H`]）。
    HomeHotTitle { total: usize },
    /// Home 空态占位行（固定高 [`HOME_EMPTY_H`]；空库/扫描中/搜索 0 命中时列表仍可滚）。
    HomeEmpty,
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
                MainItem::HomeRecTitle => REC_TITLE_H as i32,
                MainItem::HomeMixRow => REC_CARDS_H as i32,
                MainItem::HomeHotTitle { .. } => HOT_TITLE_H as i32,
                MainItem::HomeEmpty => HOME_EMPTY_H as i32,
            };
            prefix.push(acc);
        }
        Self { items, prefix, total }
    }

    /// 可视行窗 `[first,last)`（item 下标，含缓冲）：`scroll_y` = 容器向下滚动 px（≥0），
    /// `viewport` = 容器可视高。按 prefix 二分反查；首帧未布局（viewport=0）用
    /// [`FALLBACK_FIRST_ROWS`] 兜底；钳位 + 至少 1 行、空态 (0,0)。
    pub(super) fn visible_range(&self, scroll_y: f32, viewport: f32) -> (usize, usize) {
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
    pub(super) fn total_h(&self) -> f32 {
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
                        Icon::new(IconName::Note)
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
            MainSection::Recent => self.render_recent(window, cx),
        };
        div()
            .id("music-main")
            .debug_selector(|| "music-main".into())
            .flex_1()
            .min_w(px(0.))
            .min_h(px(0.))
            .flex()
            .flex_col()
            .px(px(MAIN_PAD_X))
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
                    .child(self.round_icon_button("music-notify", IconName::Bell, "通知中心", cx))
                    .child(self.round_icon_button("music-settings", IconName::Settings, "设置", cx)),
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
            .child(Icon::new(IconName::Search).size(px(16.)).text_color(theme::text3()))
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
        icon: IconName,
        label: &'static str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let cl = cx.listener(move |this: &mut MusicView, _ev, _w, cx| {
            this.toast_msg(label, cx);
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
            .child(Icon::new(icon).size(px(17.)).text_color(theme::text2()))
            .into_any_element()
    }

    // ---- Home：我的推荐 Mix 卡 + 热门歌曲表 ----

    fn render_home(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        // Home 整页并入 `#music-list` 单一竖向虚拟滚动：「我的推荐」（标题+Mix 卡横滑行）+
        // 「热门歌曲」（标题+表头）都在滚动区内的固定高虚拟项里，随歌曲行一起滚；
        // 问候/搜索头部保持固定（用户点名范围外）。空库/空命中头部仍占位稳定。
        let list = self.main_list_for(MainSection::Home, cx);
        div()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .child(self.render_list(&list, cx))
            .into_any_element()
    }

    /// Home「我的推荐」标题行（虚拟项固定高 [`REC_TITLE_H`]；随整页滚）。
    fn home_rec_title(&mut self) -> AnyElement {
        div()
            .id("home-rec-title")
            .debug_selector(|| "home-rec-title".into())
            .h(px(REC_TITLE_H))
            .flex()
            .items_center()
            .justify_between()
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
            )
            .into_any_element()
    }

    /// Home Mix 卡横滑行（虚拟项固定高 [`REC_CARDS_H`]）：固定高壳 + 内部 `overflow_x_scroll`
    /// 横滑容器（`track_scroll(&main_mix_scroll)` **持久化横滚位置**——虚拟化滚出再滚回不丢）。
    /// **滚轮拦截（需求 2）**：竖向滚轮经 `overflow_x_scroll`（x 可滚、y 不可滚）原生自动转
    /// 横向消费，本壳 `on_scroll_wheel` 只 `stop_propagation()` 阻止冒泡到外层 `#music-list`
    /// 竖向滚——「滚 Mix 卡只横向、不带动整页」；横向滚轮同理放行。无 mix 组时渲染空行占位。
    fn home_mix_row(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let groups = self
            .player
            .read(cx)
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
            .collect::<Vec<_>>();
        let cards: Vec<AnyElement> = groups
            .into_iter()
            .filter(|g| !g.2.is_empty())
            .map(|(title, sub, tracks, grad)| self.mix_card(title, sub, tracks, grad, cx))
            .collect();
        let stop = cx.listener(
            |_this: &mut MusicView, _ev: &ScrollWheelEvent, _w: &mut Window, cx: &mut Context<Self>| {
                // 竖向滚轮已被行内 `overflow_x_scroll` 转横向消费；这里只拦冒泡，防外层整页竖向滚。
                cx.stop_propagation();
            },
        );
        div()
            .id("home-mix-row")
            .debug_selector(|| "home-mix-row".into())
            .h(px(REC_CARDS_H))
            .overflow_hidden()
            .on_scroll_wheel(stop)
            .child(
                div()
                    .id("music-mix-scroll")
                    .size_full()
                    .flex()
                    .gap_x(px(16.))
                    .overflow_x_scroll()
                    .track_scroll(&self.main_mix_scroll)
                    .pb(px(10.))
                    .children(cards),
            )
            .into_any_element()
    }

    /// Home「热门歌曲」标题（共 N 首）+ 歌曲表头行（虚拟项固定高 [`HOT_TITLE_H`]；随整页滚）。
    fn home_hot_title(&mut self, total: usize) -> AnyElement {
        div()
            .id("home-hot-title")
            .debug_selector(|| "home-hot-title".into())
            .h(px(HOT_TITLE_H))
            .flex()
            .flex_col()
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
            .into_any_element()
    }

    /// Home 空态占位行（虚拟项固定高 [`HOME_EMPTY_H`]）：空库/扫描中/搜索 0 命中时列表仍可滚、
    /// 头部占位稳定。文案渲染期读 player（拆「正在扫描」与「无匹配」两况）。
    fn home_empty(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let p = self.player.read(cx);
        let (title, sub) = if p.scanning() && p.library().is_empty() {
            ("正在扫描音乐库…", "大目录首次装载需要片刻")
        } else {
            ("没有匹配的歌曲", "清空搜索或换个关键词")
        };
        div()
            .id("home-empty")
            .debug_selector(|| "home-empty".into())
            .h(px(HOME_EMPTY_H))
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .text_color(theme::text2())
            .child(
                div()
                    .text_size(px(15.))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme::text2())
                    .child(title.to_string()),
            )
            .child(
                div()
                    .pt_1()
                    .text_size(px(12.))
                    .text_color(theme::text3())
                    .child(sub.to_string()),
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
                            Icon::new(IconName::Note)
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
                            .text_color(gpui::white()),
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
        let scanning = {
            let p = self.player.read(cx);
            p.scanning() && p.library().is_empty()
        };
        div()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .child(self.section_title("音乐库"))
            .child(if scanning {
                // 异步扫库装配态：加载中给弱提示，扫完一次性提交（commit_scan → 一次 notify）。
                super::empty_state("正在扫描音乐库…", "大目录首次装载需要片刻")
                    .into_any_element()
            } else if list.total == 0 {
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

    /// 最近播放：本次会话播放历史（最近先、同曲去重置顶；与 Liked 同款「路径集过滤 +
    /// 搜索词过滤」记入列表显示模型）。历史为空 → 空态。
    fn render_recent(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let list = self.main_list_for(MainSection::Recent, cx);
        div()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .child(self.section_title("最近播放"))
            .child(if list.total == 0 {
                super::empty_state("暂无播放记录", "本会话播放过的歌曲会出现在这里").into_any_element()
            } else {
                self.render_list(&list, cx)
            })
            .into_any_element()
    }

    /// 分区标题（音乐库/收藏/最近播放）：固定于列表滚动区之上（同表头/搜索——只列表滚动）。
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
    /// 外层包 `relative` 供右侧 overlay 滚动条（[`Self::main_scrollbar`]）叠放（absolute 不随
    /// 内容滚）；滚动容器本身仍 flex_1 撑满。
    fn render_list(&mut self, list: &Rc<MainList>, cx: &mut Context<Self>) -> AnyElement {
        let scroll_y = (-self.main_scroll.offset().y.as_f32()).max(0.0);
        let viewport = self.main_scroll.bounds().size.height.as_f32();
        let (first, last) = list.visible_range(scroll_y, viewport);
        let top_h = list.prefix[first] as f32;
        let bottom_h = (list.total_h() - list.prefix[last] as f32).max(0.0);
        let items = &list.items;
        let list_el = div()
            .id("music-list")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .track_scroll(&self.main_scroll)
            // 滚轮活动由 overlay `ui::Scrollbar`（Auto）的「滚动区滚轮」监听统一驱动显示/计时
            // （其活动区域 = 本滚动容器边界），此处不再逐字刷新滚动条状态。
            .child(div().h(px(top_h)))
            .children((first..last).map(|i| match &items[i] {
                MainItem::Row { idx, rank } => self.song_row(*idx, *rank, cx),
                MainItem::GroupHeader { artist, count } => self.group_header(artist, *count),
                MainItem::HomeRecTitle => self.home_rec_title(),
                MainItem::HomeMixRow => self.home_mix_row(cx),
                MainItem::HomeHotTitle { total } => self.home_hot_title(*total),
                MainItem::HomeEmpty => self.home_empty(cx),
            }))
            .child(div().h(px(bottom_h)));
        let scrollbar = self.main_scrollbar(list, cx);
        div()
            .relative()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .child(list_el)
            .child(scrollbar)
            .into_any_element()
    }

    /// 主区列表右侧 overlay 滚动条：迁移到 `ui::Scrollbar`（Auto 模式）——内置滚动区滚轮
    /// 活动（活动区域 = 本滚动容器边界）、空闲自动隐藏、点击跳页、thumb 拖拽（含**拖出
    /// track 持续跟手 clamp**，经 gpui 窗口级 `on_mouse_event` 监听实现——原 root mousemove
    /// 冒泡兜底退役）。参数按契约冻结：autohide 3s、thumb_min 24、hover 只换色不保显
    /// （hide_on_hover=false）、拖拽常显、激活不增宽（auto_widen=false）。
    ///
    /// 位置契约：旧自绘 6px 条右缘 = 内容右缘 + [`MAIN_PAD_X`]；`ui::Scrollbar` 常态 6px
    /// thumb 右内缩 4px → 16px 轨道条右缘 = 内容右缘 + `MAIN_PAD_X` + 4，thumb 视觉精确落回
    /// 原处（禁观感位移）。内容 ≤ 视口 → 不渲染。
    fn main_scrollbar(&mut self, list: &Rc<MainList>, _cx: &mut Context<Self>) -> AnyElement {
        let viewport = self.main_scroll.bounds().size.height.as_f32();
        if list.total_h() <= viewport {
            self.main_sb_shown.set(false);
            return div().into_any_element();
        }
        let sb = UiScrollbar::vertical(&self.main_scroll)
            .mode(ScrollbarMode::Auto)
            .autohide(Duration::from_secs(SCROLLBAR_HIDE_MS / 1000))
            .thumb_min(SCROLLBAR_THUMB_MIN_H)
            .hide_on_hover(false)
            .drag_keep_visible(true)
            .auto_widen(false)
            // P4：注入深紫子主题 token（全走 theme，零裸色）——thumb/track 用 bg_hover
            // （暗紫表面，对照 B07 迁移前旧自绘条 rest=bg_hover、无可见轨道）、accent 用
            // text3（旧 hover/拖拽变亮色同义值），三槽语义逐一对齐旧条。
            .thumb_color(Some(theme::bg_hover()))
            .track_color(Some(theme::bg_hover()))
            .accent_color(Some(theme::text3()))
            .viewport_from_layout()
            // 可见性只读快照：组件 prepaint 提交 `!hidden`，测试/调试据此断言显隐（组件
            // 内部 Auto 判定是唯一事实源，此处不回写）。
            .report_visible_to(self.main_sb_shown.clone())
            .into_any_element();
        div()
            .id("music-main-scrollbar")
            .debug_selector(|| "music-main-scrollbar".into())
            .absolute()
            .top_0()
            .bottom_0()
            .right(px(-(MAIN_PAD_X + 4.0)))
            .w(px(SB_TRACK_W))
            .child(sb)
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
            let recent = if section == MainSection::Recent {
                Some(p.recent())
            } else {
                None
            };
            build_main_list(section, p.library(), &key.1, liked, recent)
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
                    .when(h == "歌曲", |d| d.flex_grow(2.0).flex_basis(px(0.)))
                    .when(h == "专辑", |d| d.flex_grow(1.0).flex_basis(px(0.)))
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
        // 序号列：播放中 = 均衡器动画（独立子视图，动画只重绘它、不带动整树）；否则纯数字。
        let num = if playing {
            self.eq_main.clone().into_any_element()
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
                .flex_basis(px(0.))
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
                .id(format!("song-album-{idx}"))
                .debug_selector(move || format!("song-album-{idx}"))
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
                    Icon::new(if liked { IconName::Heart } else { IconName::HeartBorder })
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
/// 搜索词 × 库/收藏/历史变更（mutation）加速键变化时调用）。Liked 需 `liked`（已收藏
/// 路径集）、Recent 需 `recent`（会话播放历史序，最近先）过滤。
fn build_main_list(
    section: MainSection,
    lib: &[Track],
    q: &str,
    liked: Option<&std::collections::HashSet<String>>,
    recent: Option<&[String]>,
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
        MainSection::Recent => {
            // 最近播放：按会话历史序（最近先）逐 path 从库里取行，搜索词过滤同 Liked；
            // 历史为空 → 空列表（`render_recent` 空态接管）。
            let Some(recent) = recent else {
                return Rc::new(MainList::new(Vec::new(), 0));
            };
            // path → 库下标映射（免逐 path 线性扫库；换库/清空时缓存已整体失效重建）。
            let mut idx_by_path: HashMap<String, usize> = HashMap::new();
            for (i, t) in lib.iter().enumerate() {
                idx_by_path
                    .entry(t.path.to_string_lossy().to_string())
                    .or_insert(i);
            }
            let mut items = Vec::new();
            let mut rank = 0usize;
            for path in recent {
                let Some(&i) = idx_by_path.get(path) else {
                    // 历史路径已不在当前库（库换代/文件被删）→ 跳过不显示。
                    continue;
                };
                let t = &lib[i];
                if filter_pass(&t.title, &t.artist, q) {
                    items.push(MainItem::Row { idx: i, rank });
                    rank += 1;
                }
            }
            Rc::new(MainList::new(items, rank))
        }
        MainSection::Home => {
            // Home 头部三个固定高虚拟项（「我的推荐」标题 / Mix 卡横滑行 / 「热门歌曲」标题+表头）
            // 插在歌曲行前，使整页并入 `#music-list` 竖向虚拟滚动一起滚；Library/Liked 不插。
            // 空库/搜索 0 命中时追加固定高空态占位项（头部仍占位稳定、列表可滚不 panic）。
            let mut items = Vec::with_capacity(lib.len() + 3);
            items.push(MainItem::HomeRecTitle);
            items.push(MainItem::HomeMixRow);
            let total = lib
                .iter()
                .filter(|t| filter_pass(&t.title, &t.artist, q))
                .count();
            items.push(MainItem::HomeHotTitle { total });
            if total == 0 {
                items.push(MainItem::HomeEmpty);
            }
            let mut rank = 0usize;
            for (i, t) in lib.iter().enumerate() {
                if filter_pass(&t.title, &t.artist, q) {
                    items.push(MainItem::Row { idx: i, rank });
                    rank += 1;
                }
            }
            Rc::new(MainList::new(items, total))
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
