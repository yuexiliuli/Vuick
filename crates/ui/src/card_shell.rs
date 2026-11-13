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
    ColorScheme, FLOAT_SHADOW_HIGH_ALPHA, FLOAT_SHADOW_PEAK_ALPHA, GLASS_DOUBLE_HIGH_ALPHA,
    GLASS_DOUBLE_PEAK_ALPHA, GLASS_PANEL_ALPHA_DARK, GLASS_PANEL_ALPHA_LIGHT,
    POPOVER_DOUBLE_HIGH_ALPHA, SINGLE_SHADOW_ALPHA, TOAST_DOUBLE_HIGH_ALPHA,
    TOAST_DOUBLE_PEAK_ALPHA,
};

/// 卡底：Acrylic 半透明玻璃材质（α 由调用方指定），或调用方自供的纯色。
#[derive(Clone, Copy, Debug, PartialEq)]
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
    /// 近高程双层软影：(0,4) blur12 α0.16 + (0,1) blur3 α0.12（video 占位玻璃卡现值）。
    NearDouble,
    /// 悬浮小菜单双层影：(0,6) blur12 α0.18 + (0,2) blur4 α0.12（pdf 选中弹卡现值）。
    PopoverDouble,
    /// 状态浮层双层影：(0,4) blur10 α0.18 + (0,1) blur2 α0.14（video status toast 现值；
    /// 与 PopoverDouble 同层 α 但偏移/模糊档不同，独立成档）。
    ToastDouble,
}

/// 圆角档。
#[derive(Clone, Copy, Debug)]
pub enum CardShellRadius {
    /// 更小一档（`rounded_sm`）。
    Sm,
    /// 中档（`rounded_md` = 6px）。
    Md,
    /// 现有浮层卡圆角档（`rounded_lg` = 8px）。
    Lg,
    /// 更大一档（`rounded_xl` = 12px；music 中央 toast 现值 `rounded(px(12.))`）。
    Xl,
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
    /// 不对称边距：`.px(x).py(y)`，不设 flex（video placeholder px4/py2、hover px2/py1 用）。
    PxPy(Pixels, Pixels),
}

/// 卡片壳体绘制底层：复刻现值「acrylic/solid 底 + 阴影档 + 圆角档 + 内边距档 +
/// `border_1` + `colors.border`」。返回可续挂的 gpui [`Div`]，size / focus / 事件 /
/// children 由调用方续挂。
///
/// 着色全部取自 `colors`：Acrylic/Solid 从 `colors.surface` 派生、阴影色 =
/// `colors.card_shadow` 按档取 alpha，本层零裸色。Acrylic alpha 由调用方直接给
/// （float_card 按明暗取 `FLOAT_ACRYLIC_ALPHA_*` 现值；glass_card 可调）。
/// `border` 覆盖档：`None` = 默认 `colors.border`；`Some(c)` 覆盖边框色
/// （music toast 的 accent 边框、自含主题卡等走覆盖，不落 `colors`）。
pub fn card_shell(
    colors: &ColorScheme,
    bg: CardShellBg,
    shadow: CardShellShadow,
    radius: CardShellRadius,
    pad: CardShellPad,
    border: Option<Hsla>,
) -> Div {
    let surface: Hsla = colors.surface.into();
    let border_color: Hsla = border.unwrap_or_else(|| colors.border.into());
    let shadow_base: Hsla = colors.card_shadow.into();
    let mut card = div()
        .border_1()
        .border_color(border_color)
        .bg(match bg {
            CardShellBg::Acrylic(alpha) => surface.opacity(alpha),
            CardShellBg::Solid(color) => color,
        });
    card = match radius {
        CardShellRadius::Sm => card.rounded_sm(),
        CardShellRadius::Md => card.rounded_md(),
        CardShellRadius::Lg => card.rounded_lg(),
        CardShellRadius::Xl => card.rounded_xl(),
    };
    let shadows = card_shell_shadows(shadow, shadow_base);
    card = if shadows.is_empty() {
        card
    } else {
        card.shadow(shadows)
    };
    match pad {
        CardShellPad::None => card,
        CardShellPad::P(pad) => card.p(pad),
        CardShellPad::Pg(pad, gap) => card.flex().flex_col().p(pad).gap_y(gap),
        CardShellPad::P3 => card.flex().flex_col().p(px(16.)).gap_y(px(12.)),
        CardShellPad::PxPy(horiz, vert) => card.px(horiz).py(vert),
    }
}

/// 按档构造阴影列表（无阴影 → 空 Vec）。纯函数：把「阴影档 → 具体偏移/模糊/α」的逐值映射
/// 单测守护，锁死「影偏移/模糊/α 逐值一致」的像素等价契约（`shadow_base` 由调用方传
/// `colors.card_shadow`＝黑，与旧散值 `hsla(0,0,0,α)` 逐通道同值）。
pub fn card_shell_shadows(shadow: CardShellShadow, shadow_base: Hsla) -> Vec<BoxShadow> {
    match shadow {
        CardShellShadow::None => Vec::new(),
        CardShellShadow::Single => vec![
            BoxShadow::new(px(0.), px(2.), shadow_base.opacity(SINGLE_SHADOW_ALPHA))
                .blur_radius(px(6.)),
        ],
        CardShellShadow::Double => vec![
            BoxShadow::new(px(0.), px(8.), shadow_base.opacity(FLOAT_SHADOW_HIGH_ALPHA))
                .blur_radius(px(14.)),
            BoxShadow::new(px(0.), px(2.), shadow_base.opacity(FLOAT_SHADOW_PEAK_ALPHA))
                .blur_radius(px(4.)),
        ],
        CardShellShadow::NearDouble => vec![
            BoxShadow::new(px(0.), px(4.), shadow_base.opacity(GLASS_DOUBLE_HIGH_ALPHA))
                .blur_radius(px(12.)),
            BoxShadow::new(px(0.), px(1.), shadow_base.opacity(GLASS_DOUBLE_PEAK_ALPHA))
                .blur_radius(px(3.)),
        ],
        CardShellShadow::PopoverDouble => vec![
            BoxShadow::new(px(0.), px(6.), shadow_base.opacity(POPOVER_DOUBLE_HIGH_ALPHA))
                .blur_radius(px(12.)),
            BoxShadow::new(px(0.), px(2.), shadow_base.opacity(GLASS_DOUBLE_PEAK_ALPHA))
                .blur_radius(px(4.)),
        ],
        CardShellShadow::ToastDouble => vec![
            BoxShadow::new(px(0.), px(4.), shadow_base.opacity(TOAST_DOUBLE_HIGH_ALPHA))
                .blur_radius(px(10.)),
            BoxShadow::new(px(0.), px(1.), shadow_base.opacity(TOAST_DOUBLE_PEAK_ALPHA))
                .blur_radius(px(2.)),
        ],
    }
}

/// B02b 玻璃卡底：`colors.surface` 玻璃底（默认 α 或覆盖）或调用方自供的纯色。
#[derive(Clone, Copy, Debug)]
pub enum GlassCardBg {
    /// 默认玻璃 α（亮 [`GLASS_PANEL_ALPHA_LIGHT`] / 暗 [`GLASS_PANEL_ALPHA_DARK`]）——
    /// video 玻璃面板散值的官方落点，调用方不必每处传 α。
    Acrylic,
    /// 覆盖玻璃 α：同款 `colors.surface` 玻璃底，自定义材质厚度（> 0）。
    AcrylicAlpha(f32),
    /// 纯色底（pdf 选中弹卡等自供 `colors.popover_bg`）。
    Solid(Hsla),
}

/// Acrylic 默认 α（亮 0.80 / 暗 0.72）：纯函数，测试注入明暗各一面断言默认跟随主题。
pub fn glass_acrylic_alpha(is_dark: bool) -> f32 {
    if is_dark {
        GLASS_PANEL_ALPHA_DARK
    } else {
        GLASS_PANEL_ALPHA_LIGHT
    }
}

/// 「玻璃卡底档 → 底座壳底」解析：默认 Acrylic 取 [`glass_acrylic_alpha`]，Solid 直通。
pub fn glass_card_bg(bg: GlassCardBg, is_dark: bool) -> CardShellBg {
    match bg {
        GlassCardBg::Acrylic => CardShellBg::Acrylic(glass_acrylic_alpha(is_dark)),
        GlassCardBg::AcrylicAlpha(alpha) => CardShellBg::Acrylic(alpha),
        GlassCardBg::Solid(color) => CardShellBg::Solid(color),
    }
}

/// B02b 玻璃卡便捷入口：`card_shell` 同款五参 + `is_dark` 注入 + `border` 覆盖
/// （`None` = 默认 `colors.border`，`Some(c)` 覆盖边框色）——Acrylic 默认 α 由
/// [`glass_acrylic_alpha`] 给出（亮 0.80 / 暗 0.72），调用方不必每处传 α，另有
/// [`GlassCardBg::AcrylicAlpha`] / [`GlassCardBg::Solid`] 覆盖。**只做壳**：返回可续挂的
/// gpui [`Div`]，定位/事件/children（含 min_w 等宿主补挂）由调用方在原位续挂。
pub fn glass_card(
    colors: &ColorScheme,
    is_dark: bool,
    bg: GlassCardBg,
    shadow: CardShellShadow,
    radius: CardShellRadius,
    pad: CardShellPad,
    border: Option<Hsla>,
) -> Div {
    card_shell(
        colors,
        glass_card_bg(bg, is_dark),
        shadow,
        radius,
        pad,
        border,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AbsoluteLength, DefiniteLength, Styled, hsla, red, rems};

    /// 像素长 → 已定型 DefiniteLength（读取 `style().padding.*` 的断言口径）。
    fn px_len(v: f32) -> DefiniteLength {
        DefiniteLength::Absolute(AbsoluteLength::Pixels(px(v)))
    }

    /// Acrylic 默认 α 守护：默认档 = 文档化现值（亮 0.80 / 暗 0.72），Solid 直通不受 is_dark 影响。
    #[test]
    fn glass_acrylic_default_alpha() {
        assert_eq!(GLASS_PANEL_ALPHA_LIGHT, 0.80);
        assert_eq!(GLASS_PANEL_ALPHA_DARK, 0.72);
        assert_eq!(glass_acrylic_alpha(false), GLASS_PANEL_ALPHA_LIGHT);
        assert_eq!(glass_acrylic_alpha(true), GLASS_PANEL_ALPHA_DARK);
        // 覆盖档入线：AcrylicAlpha 不被 is_dark 改写。
        assert_eq!(
            glass_card_bg(GlassCardBg::AcrylicAlpha(0.5), true),
            CardShellBg::Acrylic(0.5)
        );
        // Solid 直通（pdf 弹卡 popover_bg 路径），明暗两态一致。
        let c = hsla(0.1, 0.2, 0.3, 0.9);
        assert_eq!(glass_card_bg(GlassCardBg::Solid(c), false), CardShellBg::Solid(c));
        assert_eq!(glass_card_bg(GlassCardBg::Solid(c), true), CardShellBg::Solid(c));
    }

    /// 双层影逐值守护（Solid 双影的像素等价契约）：offsets/blur/α 逐值锁死（黑底派生）。
    #[test]
    fn double_shadows_preserve_exact_values() {
        let base = hsla(0., 0., 0., 1.); // 即 colors.card_shadow（黑），与旧散值同色
        let near = card_shell_shadows(CardShellShadow::NearDouble, base);
        assert_eq!(near.len(), 2);
        assert_eq!(near[0].offset, gpui::point(px(0.), px(4.)));
        assert_eq!(near[0].blur_radius, px(12.));
        assert_eq!(near[0].color, base.opacity(GLASS_DOUBLE_HIGH_ALPHA)); // α0.16
        assert_eq!(near[1].offset, gpui::point(px(0.), px(1.)));
        assert_eq!(near[1].blur_radius, px(3.));
        assert_eq!(near[1].color, base.opacity(GLASS_DOUBLE_PEAK_ALPHA)); // α0.12

        let pop = card_shell_shadows(CardShellShadow::PopoverDouble, base);
        assert_eq!(pop.len(), 2);
        assert_eq!(pop[0].offset, gpui::point(px(0.), px(6.)));
        assert_eq!(pop[0].blur_radius, px(12.));
        assert_eq!(pop[0].color, base.opacity(POPOVER_DOUBLE_HIGH_ALPHA)); // α0.18
        assert_eq!(pop[1].offset, gpui::point(px(0.), px(2.)));
        assert_eq!(pop[1].blur_radius, px(4.));
        assert_eq!(pop[1].color, base.opacity(GLASS_DOUBLE_PEAK_ALPHA)); // α0.12

        // B02c 新档：ToastDouble = video status toast 现值 (0,4)blur10 α0.18 + (0,1)blur2 α0.14。
        let toast = card_shell_shadows(CardShellShadow::ToastDouble, base);
        assert_eq!(toast.len(), 2);
        assert_eq!(toast[0].offset, gpui::point(px(0.), px(4.)));
        assert_eq!(toast[0].blur_radius, px(10.));
        assert_eq!(toast[0].color, base.opacity(TOAST_DOUBLE_HIGH_ALPHA)); // α0.18
        assert_eq!(toast[1].offset, gpui::point(px(0.), px(1.)));
        assert_eq!(toast[1].blur_radius, px(2.));
        assert_eq!(toast[1].color, base.opacity(TOAST_DOUBLE_PEAK_ALPHA)); // α0.14

        // 既有档不动（float_card 依赖）：Single 单影 / Double 双影 / None 空三路照旧。
        let single = card_shell_shadows(CardShellShadow::Single, base);
        assert_eq!(single.len(), 1);
        assert_eq!(single[0].offset, gpui::point(px(0.), px(2.)));
        assert_eq!(single[0].blur_radius, px(6.));
        assert_eq!(single[0].color, base.opacity(SINGLE_SHADOW_ALPHA));
        let double = card_shell_shadows(CardShellShadow::Double, base);
        assert_eq!(double.len(), 2);
        assert_eq!(double[0].offset, gpui::point(px(0.), px(8.)));
        assert_eq!(double[0].blur_radius, px(14.));
        assert_eq!(double[1].offset, gpui::point(px(0.), px(2.)));
        assert_eq!(double[1].blur_radius, px(4.));
        assert!(card_shell_shadows(CardShellShadow::None, base).is_empty());
    }

    /// pad 形态守护：PxPy = 不对称 `.px(x).py(y)`（video px4/py2 → 16/8）；
    /// Pg = flex_col + p(4) + gap_y(2)（pdf 弹卡形态）；Solid+双影同落一壳体。
    #[test]
    fn glass_card_pad_forms() {
        let mut c = glass_card(
            &ColorScheme::light(), false, GlassCardBg::Acrylic, CardShellShadow::None,
            CardShellRadius::Lg, CardShellPad::PxPy(px(16.), px(8.)), None,
        );
        let s = c.style();
        assert_eq!(s.padding.top, Some(px_len(8.)));
        assert_eq!(s.padding.bottom, Some(px_len(8.)));
        assert_eq!(s.padding.left, Some(px_len(16.)));
        assert_eq!(s.padding.right, Some(px_len(16.)));

        // pdf 弹卡形态：Solid + PopoverDouble 双影 + Pg(4,2) = flex_col · p(px(4.)) · gap_y2
        //（原 `.p(px(4.))`＝4px 四侧，勿与 px_4()=16px 混淆）；min_w 属宿主补挂，不在壳内。
        let mut c = glass_card(
            &ColorScheme::light(), false, GlassCardBg::Solid(red()),
            CardShellShadow::PopoverDouble, CardShellRadius::Lg, CardShellPad::Pg(px(4.), px(2.)),
            None,
        );
        let s = c.style();
        assert_eq!(s.display, Some(gpui::Display::Flex));
        assert_eq!(s.flex_direction, Some(gpui::FlexDirection::Column));
        assert!(s.background.is_some(), "Solid 底色落入壳体");
        assert_eq!(s.box_shadow.as_ref().map(Vec::len), Some(2), "双影落入壳体");
        assert_eq!(s.padding.top, Some(px_len(4.)));
        assert_eq!(s.padding.bottom, Some(px_len(4.)));
        assert_eq!(s.padding.left, Some(px_len(4.)));
        assert_eq!(s.padding.right, Some(px_len(4.)));
        assert_eq!(s.gap.width, None);
        assert_eq!(s.gap.height, Some(px_len(2.)));
    }

    /// B02c 新参守护：`Xl` 圆角 = rounded_xl（0.75rem = 12px，music toast 现值）与
    /// `border` 覆盖（`Some` → 边框色被覆盖 / `None` → 默认 theme border）——
    /// music toast 的 accent 边框与 12px 圆角像素等价契约。
    #[test]
    fn glass_card_xl_radius_and_border_override() {
        let accent = hsla(0.7, 0.8, 0.5, 1.);

        // Xl 圆角 + border 覆盖：Solid 底 + 无影（music toast 路径）。
        let mut c = glass_card(
            &ColorScheme::light(), false, GlassCardBg::Solid(red()), CardShellShadow::None,
            CardShellRadius::Xl, CardShellPad::PxPy(px(28.), px(12.)), Some(accent),
        );
        let s = c.style();
        // rounded_xl 落盘 = `rems(0.75)`，即 gpui 语义 12px（rem_size=16px）。
        assert_eq!(
            s.corner_radii.top_left,
            Some(AbsoluteLength::Rems(rems(0.75)))
        );
        assert_eq!(s.border_color, Some(accent), "border 覆盖生效");
        assert!(s.box_shadow.is_none(), "无影档不入壳体");
        assert_eq!(s.padding.top, Some(px_len(12.)));
        assert_eq!(s.padding.left, Some(px_len(28.)));

        // None 覆盖 → 保持默认 theme border（与旧行为一致，float_card/pdf 等零变化）。
        let mut c = glass_card(
            &ColorScheme::light(), false, GlassCardBg::Acrylic, CardShellShadow::None,
            CardShellRadius::Md, CardShellPad::None, None,
        );
        let s = c.style();
        let scheme = ColorScheme::light();
        assert_eq!(s.border_color, Some(scheme.border.into()));
    }
}
