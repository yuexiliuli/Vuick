//! 自绘 pill（胶囊 chip）：`Pill` + [`PillSize`] + [`PillTone`]。
//!
//! 两个轴各走语义档、内部映射：
//! - 尺寸（[`PillSize`]）：Sm 紧凑（≈11px 字 + `rounded_sm`）/ Md 常规（≈12px 字 +
//!   `rounded_md`），高度/字号全部为命名常量，消费方不散落裸数字。
//! - 壳配色（[`PillTone`]）：Chip 取 `chrome.pill_bg/pill_fg`（芯片令牌）；Surface 取
//!   `colors.surface/fg`（内容表面**实底**）——颜色一律走 `ui::theme` 语义 token，
//!   组件内部零裸 rgba/hex。
//!
//! 可选前导色点（[`Pill::leading_dot`]，6×6 圆点，如状态栏语言徽记色点）与可选交互：
//! [`Pill::on_click`]（点击上抛）/ [`Pill::on_mouse_down`]（**按下即响应**，左键，如
//! 状态栏全屏/换行/音乐按钮）——挂其一即手型 cursor + 对应监听；不挂则纯展示、
//! 无 cursor、无事件监听。

use std::rc::Rc;

use gpui::{
    AnyElement, App, ClickEvent, CursorStyle, ElementId, Hsla, InteractiveElement, IntoElement,
    MouseButton, MouseDownEvent, ParentElement, Refineable, RenderOnce, Stateful,
    StatefulInteractiveElement, StyleRefinement, Styled, ViewElement, Window, div,
    prelude::FluentBuilder as _, px, relative,
};

use crate::ui::theme::{ChromeColors, Theme};

/// Sm 档字号（px）：紧凑 chip（状态栏语言徽标等）。
const PILL_SM_TEXT_SIZE: f32 = 11.0;
/// Md 档字号（px）：常规 chip。
const PILL_MD_TEXT_SIZE: f32 = 12.0;
/// Sm 档高度（px）：对齐状态栏 pill 真实高 `STATUS_BAR_H - 4 = 19`（落点「h≈12+」）。
const PILL_SM_HEIGHT: f32 = 19.0;
/// Md 档高度（px）：12px 字 + 上下 ~5px 内边距的常规 chip 外廓（≈「px2/py1」观感）。
const PILL_MD_HEIGHT: f32 = 22.0;
/// 前导色点直径（px）。
const PILL_DOT_SIZE: f32 = 6.0;
/// 色点与 label 的间距（px）。
const PILL_LABEL_GAP: f32 = 4.0;

/// 点击回调别名：与 `ui::Button` 同款签名。
type ClickHandler = Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>;

/// 鼠标按下回调别名：与 gpui `on_mouse_down` 同款签名（左键，按下即响应）。
type MouseDownHandler = Rc<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>;

/// 元素 id → `debug_selector` 名：`ElementId::Name` 直接用；其余回退到 Debug 串
/// （`debug_bounds` 只按 debug_selector 收录元素，见 `crates/shell` 既有备注；方法
/// 签名要求返回 `String`，见 gpui `debug_selector`）。
fn id_selector(id: &ElementId) -> String {
    match id {
        ElementId::Name(name) => name.to_string(),
        _ => format!("{id:?}"),
    }
}

/// Pill 尺寸语义档（映射到命名常量；不在消费方散落裸数字高度）。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum PillSize {
    /// 紧凑档：11px 字 + `rounded_sm`（状态栏语言徽标等）。
    Sm,
    /// 常规档：12px 字 + `rounded_md`（下档 chip 等）。
    #[default]
    Md,
}

impl PillSize {
    /// 语义档 → （字号 px, 高度 px）。值全为命名常量；圆角直接取 gpui 语义方法
    /// （`rounded_sm`/`rounded_md`，无数值漂移）。
    fn profile(&self) -> (f32, f32) {
        match self {
            PillSize::Sm => (PILL_SM_TEXT_SIZE, PILL_SM_HEIGHT),
            PillSize::Md => (PILL_MD_TEXT_SIZE, PILL_MD_HEIGHT),
        }
    }
}

/// Pill 壳配色档（Solid 实心：底色 + 前景文字）。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum PillTone {
    /// chip 壳：`pill_bg` 底 → `pill_fg` 字（状态栏语言徽标全屏/换行钮等）。
    #[default]
    Chip,
    /// surface 壳：`surface` **实色**底 → `fg` 主文字（下档 chip 等）。
    Surface,
}

impl PillTone {
    /// 壳配色：底色 + 前景文字（theme 语义 token，禁裸 rgba）。
    fn colors(&self, theme: &Theme, chrome: &ChromeColors) -> (Hsla, Hsla) {
        match self {
            PillTone::Chip => (chrome.pill_bg.into(), chrome.pill_fg.into()),
            PillTone::Surface => (theme.colors.surface.into(), theme.colors.fg.into()),
        }
    }
}

/// 自绘 pill（胶囊 chip）：语义档尺寸 + 语义壳配色 + 可选前导色点 + 可选点击。
///
/// 用法（builder 链，风格对齐 `ui::Button`）：`Pill::new("lang").size(PillSize::Sm)
/// .tone(PillTone::Chip).leading_dot(dot).label("Markdown").on_click(cb)`。
/// 交互性**由是否挂 `on_click` 决定**：挂 → 手型 cursor + click 上抛；不挂 → 纯展示。
pub struct Pill {
    id: ElementId,
    base: Stateful<gpui::Div>,
    /// 调用方经 [`Styled`] 链累计的样式（render 时叠加到默认样式之上，可覆盖）。
    style: StyleRefinement,
    size: PillSize,
    tone: PillTone,
    leading_dot: Option<Hsla>,
    label: Option<AnyElement>,
    on_click: Option<ClickHandler>,
    on_mouse_down: Option<MouseDownHandler>,
}

impl Pill {
    /// 以调用方指定的 id 构建（debug_selector 随 id 派生，供 `debug_bounds` 定位）。
    pub fn new(id: impl Into<ElementId>) -> Self {
        let id = id.into();
        let selector = id_selector(&id);
        Self {
            id: id.clone(),
            base: div().id(id).debug_selector(move || selector.clone()),
            style: StyleRefinement::default(),
            size: PillSize::Md,
            tone: PillTone::Chip,
            leading_dot: None,
            label: None,
            on_click: None,
            on_mouse_down: None,
        }
    }

    /// 尺寸语义档（Sm 紧凑 / Md 常规）。
    pub fn size(mut self, size: PillSize) -> Self {
        self.size = size;
        self
    }

    /// 壳配色档（Chip 芯片 / Surface 内容表面实底）。
    pub fn tone(mut self, tone: PillTone) -> Self {
        self.tone = tone;
        self
    }

    /// 前导色点（6×6 圆点，如状态栏语言徽记色点）；不设则不渲染。
    pub fn leading_dot(mut self, dot: Hsla) -> Self {
        self.leading_dot = Some(dot);
        self
    }

    /// 文本/inner 内容（任意 `IntoElement`）。
    pub fn label(mut self, label: impl IntoElement) -> Self {
        self.label = Some(label.into_any_element());
        self
    }

    /// 注册点击回调（可交互：手型 cursor + click 上抛）；不调则纯展示。
    pub fn on_click(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_click = Some(Rc::new(handler));
        self
    }

    /// 注册鼠标**按下**回调（左键，按下即响应，不等点击抬起——状态栏全屏/换行/音乐
    /// 等「按下即切换」按钮的时序；K2：时序变了=行为回归）。可交互 → 手型 cursor +
    /// mouse_down 上抛；不调则无按下响应。
    pub fn on_mouse_down(
        mut self,
        handler: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_mouse_down = Some(Rc::new(handler));
        self
    }
}

impl Styled for Pill {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl RenderOnce for Pill {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = Theme::global(cx);
        // P1-2：取**合并后** Theme 的 chrome（不再从 `for_mode` base 现取）。
        let (bg, fg) = self.tone.colors(theme, &theme.chrome);
        let (text_size, height) = self.size.profile();
        let on_click = self.on_click;
        let mouse_down = self.on_mouse_down;
        let caller_style = self.style.clone();
        let leading_dot = self.leading_dot;
        let label = self.label;
        let dot_selector = format!("{}::dot", id_selector(&self.id));

        let mut root = self
            .base
            .flex()
            .flex_shrink_0()
            .items_center()
            .justify_center()
            .line_height(relative(1.))
            .gap(px(PILL_LABEL_GAP))
            .px_2()
            .h(px(height))
            .text_size(px(text_size))
            .bg(bg)
            .text_color(fg);
        // 圆角按语义档：Sm → rounded_sm / Md → rounded_md（gpui 语义方法，无数值漂移）。
        root = match self.size {
            PillSize::Sm => root.rounded_sm(),
            PillSize::Md => root.rounded_md(),
        };
        // 可交互（挂 on_click 或 on_mouse_down 任一）才给手型 cursor + 对应监听；
        // 纯展示无 cursor 无监听（cursor 与监听同一门控，headless 无法读光标样式，
        // 行为断言见 tests）。
        let interactive = on_click.is_some() || mouse_down.is_some();
        root = root.when(interactive, |this| this.cursor(CursorStyle::PointingHand));
        root = root.when_some(mouse_down, |this, cb| {
            this.on_mouse_down(MouseButton::Left, move |event, window, cx| {
                cb(event, window, cx)
            })
        });
        root = root.when_some(on_click, |this, cb| {
            this.on_click(move |event, window, cx| cb(event, window, cx))
        });
        // 前导色点（6×6 圆点，debug_selector = "<pill id>::dot" 供定位）。
        if let Some(dot) = leading_dot {
            root = root.child(
                div()
                    .debug_selector(move || dot_selector.clone())
                    .flex_shrink_0()
                    .size(px(PILL_DOT_SIZE))
                    .rounded_full()
                    .bg(dot),
            );
        }
        if let Some(label) = label {
            root = root.child(label);
        }
        // 调用方样式最后叠加（w/h/px/rounded/text_size/text_color… 由调用方决定覆盖）。
        root.style().refine(&caller_style);
        root.into_any_element()
    }
}

impl IntoElement for Pill {
    type Element = ViewElement<Self>;

    #[track_caller]
    fn into_element(self) -> Self::Element {
        ViewElement::new(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{
        Context, Modifiers, MouseButton, Render, TestAppContext, VisualTestContext,
    };
    use std::{cell::Cell, rc::Rc};

    struct Harness {
        /// pill-sm 是否挂 on_click（可交互）。
        interactive: bool,
        /// pill-sm 是否挂 on_mouse_down（按下即响应；优先于 on_click 分支）。
        press: bool,
        /// pill-sm 是否带前导色点（带点同时渲染无点对照 pill-plain）。
        with_dot: bool,
        clicks: Rc<Cell<usize>>,
    }

    impl Render for Harness {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let clicks = self.clicks.clone();
            let mut sm = Pill::new("pill-sm")
                .size(PillSize::Sm)
                .tone(PillTone::Chip)
                .label("Markdown");
            if self.with_dot {
                sm = sm.leading_dot(gpui::hsla(0.60, 0.60, 0.90, 1.0));
            }
            let sm = if self.press {
                sm.on_mouse_down(move |_, _, _| clicks.set(clicks.get() + 1))
            } else if self.interactive {
                sm.on_click(move |_, _, _| clicks.set(clicks.get() + 1))
            } else {
                sm
            };
            div()
                .id("pill-wrap")
                .w(px(400.))
                .h(px(200.))
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .px_2()
                .py_4()
                .child(sm)
                .child(
                    Pill::new("pill-md")
                        .size(PillSize::Md)
                        .tone(PillTone::Surface)
                        .label("UTF-8"),
                )
                // 带点对照：与 pill-sm 同 label 同尺寸、仅差色点 → 宽度差 = 点+间距。
                .when(self.with_dot, |this| {
                    this.child(
                        Pill::new("pill-plain")
                            .size(PillSize::Sm)
                            .tone(PillTone::Chip)
                            .label("Markdown"),
                    )
                })
        }
    }

    fn harness(
        cx: &mut TestAppContext,
        interactive: bool,
        with_dot: bool,
    ) -> (&mut VisualTestContext, Rc<Cell<usize>>) {
        let clicks = Rc::new(Cell::new(0));
        let (_, cx) = cx.add_window_view({
            let clicks = clicks.clone();
            move |_, _| Harness {
                interactive,
                press: false,
                with_dot,
                clicks,
            }
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        (cx, clicks)
    }

    /// 按下即响应（on_mouse_down）夹具。
    fn press_harness(
        cx: &mut TestAppContext,
    ) -> (&mut VisualTestContext, Rc<Cell<usize>>) {
        let clicks = Rc::new(Cell::new(0));
        let (_, cx) = cx.add_window_view({
            let clicks = clicks.clone();
            move |_, _| Harness {
                interactive: false,
                press: true,
                with_dot: false,
                clicks,
            }
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        (cx, clicks)
    }

    // ---- 纯函数：语义档映射 ----

    #[test]
    fn size_profile_maps_to_semantic_tiers() {
        // 语义档真值：Sm 紧凑 = 11px 字 / 19px 高（对齐状态栏 pill 真实高 STATUS_BAR_H-4=19，
        // 落点「h≈12+」兑现）；Md 常规 = 12px 字 / 22px 高（≈「px2/py1 + 12px 字」外廓）。
        assert_eq!(PillSize::Sm.profile(), (PILL_SM_TEXT_SIZE, PILL_SM_HEIGHT));
        assert_eq!(PillSize::Md.profile(), (PILL_MD_TEXT_SIZE, PILL_MD_HEIGHT));
        // 字号/高度两档必须可辨（紧凑 vs 常规）。
        assert_ne!(PILL_SM_TEXT_SIZE, PILL_MD_TEXT_SIZE);
        assert_ne!(PILL_SM_HEIGHT, PILL_MD_HEIGHT);
    }

    #[test]
    fn tone_colors_map_to_themed_pairs() {
        // 语义映射：Chip → pill_bg/pill_fg（chrome 芯片令牌）；Surface → surface/fg
        //（内容表面实底 + 主文字）。亮暗各验一眼（值随明暗分流，映射关系恒成立）。
        for (theme, chrome) in [
            (Theme::light(), ChromeColors::light()),
            (Theme::dark(), ChromeColors::dark()),
        ] {
            let (bg, fg) = PillTone::Chip.colors(&theme, &chrome);
            assert_eq!(bg, chrome.pill_bg.into(), "Chip 底色 = pill_bg");
            assert_eq!(fg, chrome.pill_fg.into(), "Chip 前景 = pill_fg");
            let (bg, fg) = PillTone::Surface.colors(&theme, &chrome);
            assert_eq!(bg, theme.colors.surface.into(), "Surface 底色 = surface");
            assert_eq!(fg, theme.colors.fg.into(), "Surface 前景 = fg");
            // 两壳底色必须可辨（下档 chip 用 surface 实底、状态栏 pill 用浅灰 chip 底）。
            let chip_bg = PillTone::Chip.colors(&theme, &chrome).0;
            let surface_bg = PillTone::Surface.colors(&theme, &chrome).0;
            assert_ne!(chip_bg, surface_bg, "Chip 与 Surface 底色应不同（壳可辨）");
        }
    }

    // ---- 渲染：尺寸 / 色点 ----

    /// 演算：Sm/Md 高度被钉成命名常量（19/22px），label 文本参与横向布局（宽>0）。
    #[gpui::test]
    fn sm_and_md_render_with_distinct_sizes(cx: &mut TestAppContext) {
        let (cx, _) = harness(cx, false, false);
        let sm = cx.debug_bounds("pill-sm").expect("Sm pill 应渲染");
        let md = cx.debug_bounds("pill-md").expect("Md pill 应渲染");
        assert_eq!(sm.size.height, px(PILL_SM_HEIGHT), "Sm 高 = 语义常量");
        assert_eq!(md.size.height, px(PILL_MD_HEIGHT), "Md 高 = 语义常量");
        assert!(sm.size.width.as_f32() > 0., "Sm label 文本应参与布局（宽>0）");
        assert!(md.size.width.as_f32() > 0., "Md label 文本应参与布局（宽>0）");
        assert_ne!(sm.size.height, md.size.height, "Sm/Md 尺寸语义档应不同");
    }

    /// 前导色点渲出独立 6×6 圆点子元素（debug_selector = `<pill id>::dot`），且
    /// 点 + gap 使带点 pill 比同 label（同尺寸同壳）的 pill 更宽——证明点在布局树中。
    #[gpui::test]
    fn leading_dot_renders_and_adds_width(cx: &mut TestAppContext) {
        let (cx, _) = harness(cx, false, true);
        let dot = cx
            .debug_bounds("pill-sm::dot")
            .expect("前导色点应渲染为独立子元素");
        assert_eq!(dot.size.width, px(PILL_DOT_SIZE), "色点宽 = 6px");
        assert_eq!(dot.size.height, px(PILL_DOT_SIZE), "色点高 = 6px");
        let with_dot = cx.debug_bounds("pill-sm").expect("带点 pill 应在场");
        let plain = cx.debug_bounds("pill-plain").expect("无点对照 pill 应在场");
        assert!(
            with_dot.size.width.as_f32() > plain.size.width.as_f32(),
            "带点 pill 应比同 label 无点 pill 更宽（点+间距参与布局）"
        );
    }

    // ---- 交互：on_click 上抛 / 纯展示惰性 ----

    /// 可交互（挂 on_click）时，点击 pill 命中区触发回调一次。
    #[gpui::test]
    fn interactive_pill_fires_on_click(cx: &mut TestAppContext) {
        cx.update(crate::test_support::bind_local_keymaps);
        let (cx, clicks) = harness(cx, true, false);
        let sm = cx.debug_bounds("pill-sm").expect("pill-sm 应在场");
        cx.simulate_click(sm.center(), Modifiers::default());
        assert_eq!(clicks.get(), 1, "可交互 pill 点击应触发 on_click");
    }

    /// 纯展示（不挂 on_click）：无监听 → 点击无动作（cursor 与监听同一门控：
    /// `when_some(on_click, …)` 才给 pointer——headless 读不到光标样式，故以
    /// 「不响应点击」作为可观测等价断言，见 K49 headless 边界）。
    #[gpui::test]
    fn non_interactive_pill_is_inert(cx: &mut TestAppContext) {
        cx.update(crate::test_support::bind_local_keymaps);
        let (cx, clicks) = harness(cx, false, false);
        let sm = cx.debug_bounds("pill-sm").expect("pill-sm 应在场");
        cx.simulate_click(sm.center(), Modifiers::default());
        assert_eq!(clicks.get(), 0, "纯展示 pill 应无 cursor 相关监听、点击无动作");
    }

    /// on_mouse_down：鼠标**按下**（down）即触发回调，无需等 mouse_up/click 抬起——
    /// 与状态栏全屏/换行/音乐钮「按下即响应」时序一致（K2：时序变了=行为回归）。
    #[gpui::test]
    fn mouse_down_fires_on_press(cx: &mut TestAppContext) {
        cx.update(crate::test_support::bind_local_keymaps);
        let (cx, presses) = press_harness(cx);
        let sm = cx.debug_bounds("pill-sm").expect("pill-sm 应在场");
        cx.simulate_mouse_down(sm.center(), MouseButton::Left, Default::default());
        assert_eq!(presses.get(), 1, "按下即应触发 on_mouse_down（不等点击抬起）");
        // 抬起不二次触发：仅按下计一次（on_click 未挂，无点击重复计数）。
        cx.simulate_mouse_up(sm.center(), MouseButton::Left, Default::default());
        assert_eq!(presses.get(), 1, "抬起不应重复触发回调");
    }
}
