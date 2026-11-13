//! 浮动卡片壳体 builder + 全窗居中挂载 helper（Batch 4a）。
//!
//! 从 `palette`（命令面板）/ `session_store::recent_panel`（打开最近）/
//! `shell_sidebar::prompt`（文件树新建/重命名弹层）三张居中卡里抽取的**纯壳体**：
//! Acrylic/solid 底 + 双层阴影（或无）+ 圆角档 + 内边距档 + `border_1` + `colors.border`，
//! 以及「deferred 全窗 absolute 非 flex + occlude 包裹、子 absolute 水平居中」的挂载姿势。
//!
//! # 只做壳体，禁止把卡片机制收进来
//! 本组件**不包含** focus / on_key_down（Esc·Enter）/ on_mouse_down_out / 关闭逻辑——
//! 各卡的这些机制不同（面板自身挂 `on_mouse_down_out` + 面板级 `on_key_down`；prompt 走
//! 输入组件的 Submit/Cancel 订阅 + 每帧焦点重夺），强统一必回归。调用方在原位保留各自
//! 机制，本组件只替换「造型 + 挂载」这两件纯视觉差事。

use gpui::{
    div, px, AnyElement, Div, Hsla, InteractiveElement as _, IntoElement, ParentElement as _, Styled
        as _,
};

use crate::ui::theme::{ColorScheme, FLOAT_ACRYLIC_ALPHA_DARK, FLOAT_ACRYLIC_ALPHA_LIGHT};

/// 卡底：Acrylic 半透明玻璃材质，或调用方自供的纯色。
#[derive(Clone, Copy, Debug)]
pub enum FloatCardBg {
    /// `colors.surface` 的 opacity 浮动层玻璃（亮 0.92 / 暗 0.94，theme.rs:332-333 现值）。
    Acrylic,
    /// 纯色底（WYSIWYG 等不需要分层玻璃材质的场合自供）。
    Solid(Hsla),
}

/// 阴影档。
#[derive(Clone, Copy, Debug)]
pub enum FloatCardShadow {
    /// 与菜单/通知同款双层阴影（Win11 浮动卡片；palette.rs 现值
    /// `(0,8) blur14 α0.20 + (0,2) blur4 α0.14`）。
    Double,
    /// 无阴影。
    None,
}

/// 圆角档。
#[derive(Clone, Copy, Debug)]
pub enum FloatCardRadius {
    /// 现有浮层卡圆角档（`rounded_lg`）。
    Lg,
    /// 更小一档（`rounded_md`）。
    Md,
}

/// 内边距档。
#[derive(Clone, Copy, Debug)]
pub enum FloatCardPad {
    /// 无内边距/无 flex（面板类在内部自行布局，调用方按需续挂 `.flex().flex_col()`）。
    None,
    /// 宽松档：`.flex().flex_col().p(pad).gap_y(gap)`（表单卡：四侧留白 + 纵向行距）。
    P(gpui::Pixels, gpui::Pixels),
    /// 固定档：`.p(px(16.)).gap_y(px(12.))`（＝ `P(16, 12)`，小表单卡预留档）。
    P3,
}

/// 浮动卡片壳体 builder：复刻现值「acrylic/solid 底 + 双层阴影或无 + 圆角档 + 内边距档
/// + `border_1` + `colors.border`」。返回可续挂的 gpui [`Div`]，size / focus / 事件 /
/// children 由调用方续挂；**只做壳体**（不发 enter/esc、不建房内布局、不做关闭）。
///
/// `colors` / `is_dark` 由调用方从 `Theme::global(cx)` 注入（本 builder 是纯元素构造，
/// 无 `Context` 可取全局主题）；Acrylic 透明度按 `is_dark` 取亮/暗两档现值。
pub fn float_card(
    colors: &ColorScheme,
    is_dark: bool,
    bg: FloatCardBg,
    shadow: FloatCardShadow,
    radius: FloatCardRadius,
    pad: FloatCardPad,
) -> Div {
    let surface: Hsla = colors.surface.into();
    let border: Hsla = colors.border.into();
    let mut card = div()
        .border_1()
        .border_color(border)
        .bg(match bg {
            FloatCardBg::Acrylic => surface.opacity(if is_dark {
                FLOAT_ACRYLIC_ALPHA_DARK
            } else {
                FLOAT_ACRYLIC_ALPHA_LIGHT
            }),
            FloatCardBg::Solid(color) => color,
        });
    card = match radius {
        FloatCardRadius::Lg => card.rounded_lg(),
        FloatCardRadius::Md => card.rounded_md(),
    };
    card = match shadow {
        FloatCardShadow::Double => card.shadow(vec![
            gpui::BoxShadow::new(px(0.), px(8.), gpui::hsla(0., 0., 0., 0.20))
                .blur_radius(px(14.)),
            gpui::BoxShadow::new(px(0.), px(2.), gpui::hsla(0., 0., 0., 0.14))
                .blur_radius(px(4.)),
        ]),
        FloatCardShadow::None => card,
    };
    match pad {
        FloatCardPad::None => card,
        FloatCardPad::P(pad, gap) => card.flex().flex_col().p(pad).gap_y(gap),
        FloatCardPad::P3 => card.flex().flex_col().p(px(16.)).gap_y(px(12.)),
    }
}

/// 全窗居中挂载 helper：整层 `deferred` 独立绘制 + absolute **非 flex** 全窗包裹做水平
/// 居中，子 absolute `left((win_w - w)/2).max(8)` + `top`，带 `priority`。
///
/// 逐字复刻 `palette.rs:654-667` 现值。命中测试警示（2026-10-09 最小结构复现，三条）：
/// ① `deferred` 直子必须是 absolute 的**非 flex** 容器——flex 直子会让子元素（面板/
/// 输入框/列表行）注册的 hitbox 位置失真，点击全穿透到下层编辑器；
/// ② 全窗包裹要 `occlude()`（BlockMouse）——否则点击虽命中面板元素，但仍会继续命中
///    其下方编辑器的 hitbox，编辑器 `on_mouse_down` 随后把焦点抢回去（输入框
///    `on_mouse_down` 不 stop_propagation）。`occlude` 与 exact 同链，禁止拆掉。
pub fn overlay_centered(
    card: AnyElement,
    win_w: f32,
    w: f32,
    top: f32,
    priority: usize,
) -> AnyElement {
    let x = ((win_w - w) / 2.).max(8.);
    gpui::deferred(
        div()
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .bottom_0()
            .occlude()
            .child(div().absolute().left(px(x)).top(px(top)).child(card)),
    )
    .with_priority(priority)
    .into_any_element()
}
