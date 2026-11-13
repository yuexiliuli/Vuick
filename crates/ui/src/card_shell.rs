//! 卡片壳体绘制底层：Acrylic/Solid 底 + 阴影档 + 圆角档 + 内边距档 + `border_1` +
//! `colors.border` 的**纯壳**绘制（B02b 的 `ui::glass_card` 与 `float_card` 共用底座）。
//!
//! # 只做壳
//! 本层只做「造型」这一件纯视觉差事：不发 enter/esc、不建房内布局、不做关闭、不定位挂载
//! （`overlay_centered` / `anchor_layer` 属挂载机制，留在 float_card 侧）。调用方在
//! 原位保留各自机制，本层只替换「造型」这一件纯视觉差事。
//!
//! # 与 float_card 的关系
//! [`crate::ui::float_card`] 的公开 API（`FloatCard*` 枚举 + `float_card()`）保持原样、
//! 内部转调本层的 [`card_shell`]。本层枚举覆盖面比 float_card 更宽：α 可调 Acrylic、
//! Single 单影、Sm 圆角、P 单边距——新能力先不进 float_card 公开参数，留给 B02b 的
//! `ui::glass_card` 直接使用。

use gpui::{div, px, BoxShadow, Div, Hsla, Pixels, Styled as _};

use crate::ui::theme::{
    ColorScheme, FLOAT_SHADOW_HIGH_ALPHA, FLOAT_SHADOW_PEAK_ALPHA, SINGLE_SHADOW_ALPHA,
};

/// 卡底：Acrylic 半透明玻璃材质（α 由调用方指定），或调用方自供的纯色。
#[derive(Clone, Copy, Debug)]
pub enum CardShellBg {
    /// `colors.surface` 的 opacity 玻璃底，α 由调用方给（float_card 传明暗档现值；
    /// glass_card 可传任意值调材质厚度）。
    Acrylic(f32),
    /// 纯色底（WYSIWYG 等不需要分层玻璃材质的场合自供）。
    Solid(Hsla),
}

/// 阴影档。
#[derive(Clone, Copy, Debug)]
pub enum CardShellShadow {
    /// 无阴影。
    None,
    /// 单影（参考 video hover_time 的单影语义：(0,2) blur6，α `SINGLE_SHADOW_ALPHA`）。
    Single,
    /// 与菜单/通知同款双层阴影（Win11 浮动卡片；(0,8) blur14 α0.20 + (0,2) blur4 α0.14）。
    Double,
}

/// 圆角档。
#[derive(Clone, Copy, Debug)]
pub enum CardShellRadius {
    /// 更小一档（`rounded_sm`）。
    Sm,
    /// 中档（`rounded_md`）。
    Md,
    /// 现有浮层卡圆角档（`rounded_lg`）。
    Lg,
}

/// 内边距档。
#[derive(Clone, Copy, Debug)]
pub enum CardShellPad {
    /// 无内边距/无 flex（面板类内部自行布局，调用方按需续挂 `.flex().flex_col()`）。
    None,
    /// 单侧内边距：`.p(pad)`，不设 flex（glass_card 预留档）。
    P(Pixels),
    /// 宽松档：`.flex().flex_col().p(pad).gap_y(gap)`（表单卡：四侧留白 + 纵向行距）。
    Pg(Pixels, Pixels),
    /// 固定档：`.p(px(16.)).gap_y(px(12.))`（小表单卡预留档）。
    P3,
}

/// 卡片壳体绘制底层：复刻现值「acrylic/solid 底 + 阴影档 + 圆角档 + 内边距档 +
/// `border_1` + `colors.border`」。返回可续挂的 gpui [`Div`]，size / focus / 事件 /
/// children 由调用方续挂。
///
/// 着色全部取自 `colors`：Acrylic/Solid 从 `colors.surface` 派生、阴影色 =
/// `colors.card_shadow` 按档取 alpha，本层零裸色。Acrylic alpha 由调用方直接给
/// （float_card 按明暗取 `FLOAT_ACRYLIC_ALPHA_*` 现值；glass_card 可调）。
pub fn card_shell(
    colors: &ColorScheme,
    bg: CardShellBg,
    shadow: CardShellShadow,
    radius: CardShellRadius,
    pad: CardShellPad,
) -> Div {
    let surface: Hsla = colors.surface.into();
    let border: Hsla = colors.border.into();
    let shadow_base: Hsla = colors.card_shadow.into();
    let mut card = div()
        .border_1()
        .border_color(border)
        .bg(match bg {
            CardShellBg::Acrylic(alpha) => surface.opacity(alpha),
            CardShellBg::Solid(color) => color,
        });
    card = match radius {
        CardShellRadius::Sm => card.rounded_sm(),
        CardShellRadius::Md => card.rounded_md(),
        CardShellRadius::Lg => card.rounded_lg(),
    };
    card = match shadow {
        CardShellShadow::Double => card.shadow(vec![
            BoxShadow::new(px(0.), px(8.), shadow_base.opacity(FLOAT_SHADOW_HIGH_ALPHA))
                .blur_radius(px(14.)),
            BoxShadow::new(px(0.), px(2.), shadow_base.opacity(FLOAT_SHADOW_PEAK_ALPHA))
                .blur_radius(px(4.)),
        ]),
        CardShellShadow::Single => card.shadow(vec![
            BoxShadow::new(px(0.), px(2.), shadow_base.opacity(SINGLE_SHADOW_ALPHA))
                .blur_radius(px(6.)),
        ]),
        CardShellShadow::None => card,
    };
    match pad {
        CardShellPad::None => card,
        CardShellPad::P(pad) => card.p(pad),
        CardShellPad::Pg(pad, gap) => card.flex().flex_col().p(pad).gap_y(gap),
        CardShellPad::P3 => card.flex().flex_col().p(px(16.)).gap_y(px(12.)),
    }
}
