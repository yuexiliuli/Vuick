//! 音乐播放器 UI 深紫子主题：调色板 + 常用样式构造助手（自含于本 crate）。
//!
//! 调色板十六进制值**逐字取自** UI 原型 `prototype-orbitrum-player-v1.html` 的
//! `:root` 变量（`--bg-deep` / `--accent` / `--border` …）。GPUI 没有 CSS：
//! - 颜色统一由 [`c()`]/[`c_a()`] 转成运行期 [`Hsla`] 供 `.bg()`/`.text_color()` 消费；
//! - 渐变封面不存在 CSS 渐变，用 `canvas` + `window.paint_quad` + [`linear_gradient`]
//!   自绘（见 [`gradient_cover`]），[`grad_css`] 仅产出原型同款 CSS 字符串供测试/日志。
//! 所有视图只从本模块取色，禁止散落裸 hex。

use std::cell::Cell;
use std::rc::Rc;

use gpui::{
    AnyElement, Bounds, Hsla, IntoElement, Pixels, Styled as _, canvas, fill, linear_color_stop,
    linear_gradient, px,
};

// ---- 原型 `:root` 调色板常量（逐字，十六进制） ----
/// 主背景（极深紫黑）：`--bg-deep: #0d0b1a`
pub const BG_DEEP: u32 = 0x0d0b1a;
/// 侧栏/右栏/底栏底色：`--bg-primary: #13112a`
pub const BG_PRIMARY: u32 = 0x13112a;
/// 次级表面（搜索框/tab 条/按钮）：`--bg-secondary: #1a1738`
pub const BG_SECONDARY: u32 = 0x1a1738;
/// 卡片/行 hover 底：`--bg-card: #201d40`
pub const BG_CARD: u32 = 0x201d40;
/// 更深一档 hover：`--bg-hover: #282452`
pub const BG_HOVER: u32 = 0x282452;
/// 主题强调色（主按钮/进度条）：`--accent: #7c5cfc`
pub const ACCENT: u32 = 0x7c5cfc;
/// 强调亮色（active 文字/徽标）：`--accent-light: #a78bfa`
pub const ACCENT_LIGHT: u32 = 0xa78bfa;
/// 主文字：`--text-primary: #f0edff`
pub const TEXT: u32 = 0xf0edff;
/// 次要文字：`--text-secondary: #9892b8`
pub const TEXT2: u32 = 0x9892b8;
/// 三级/弱化文字：`--text-muted: #5e5880`
pub const TEXT3: u32 = 0x5e5880;
/// 分隔线/边框：`--border: rgba(255,255,255,0.06)`（0.06×255≈15=0x0f，RRGGBBAA 编码）
pub const BORDER: u32 = 0xffffff0f;
/// 粉色（收藏/渐变/强调）：`--pink: #f472b6`
pub const PINK: u32 = 0xf472b6;
/// 绿色（在线点/状态）：`--green: #34d399`
pub const GREEN: u32 = 0x34d399;
/// 橙色（歌单点等）：`--orange: #fb923c`
pub const ORANGE: u32 = 0xfb923c;
/// 蓝色（歌单点等）：`--blue: #60a5fa`
pub const BLUE: u32 = 0x60a5fa;

/// 十六进制 RGB → 运行期 [`Hsla`]（GPUI 无编译期 hex→HSL，运行时转换即可）。
pub fn c(hex: u32) -> Hsla {
    gpui::rgb(hex).into()
}

/// 十六进制 RGBA（RRGGBBAA）→ 运行期 [`Hsla`]。
pub fn c_a(hex: u32) -> Hsla {
    gpui::rgba(hex).into()
}

/// 主背景色。
pub fn bg_deep() -> Hsla {
    c(BG_DEEP)
}
/// 侧栏/右栏/底栏底色。
pub fn bg_primary() -> Hsla {
    c(BG_PRIMARY)
}
/// 次级表面（搜索框/tab/按钮）。
pub fn bg_secondary() -> Hsla {
    c(BG_SECONDARY)
}
/// 卡片/行 hover 底。
pub fn bg_card() -> Hsla {
    c(BG_CARD)
}
/// 更深一档 hover。
pub fn bg_hover() -> Hsla {
    c(BG_HOVER)
}
/// 主题强调色。
pub fn accent() -> Hsla {
    c(ACCENT)
}
/// 强调亮色。
pub fn accent_light() -> Hsla {
    c(ACCENT_LIGHT)
}
/// 主文字。
pub fn text() -> Hsla {
    c(TEXT)
}
/// 次要文字。
pub fn text2() -> Hsla {
    c(TEXT2)
}
/// 三级/弱化文字。
pub fn text3() -> Hsla {
    c(TEXT3)
}
/// 分隔线/边框。
pub fn border() -> Hsla {
    c_a(BORDER)
}
/// 粉色。
pub fn pink() -> Hsla {
    c(PINK)
}
/// 绿色。
pub fn green() -> Hsla {
    c(GREEN)
}
/// 橙色。
pub fn orange() -> Hsla {
    c(ORANGE)
}
/// 蓝色。
pub fn blue() -> Hsla {
    c(BLUE)
}

/// 深紫玻璃遮罩（toast 底）：原型 `.toast { background: rgba(20,17,50,0.92) }`
/// → 0.92×255≈235=0xeb，编码 `0x141132eb`。
pub fn glass() -> Hsla {
    c_a(0x141132eb)
}

/// 由 model `Track.grad`（主色 `(r,g,b)`）派生渐变双色：主色 + 加深 55% 变体
/// （与 `model::gradient_pair` 的加深系数一致，保证封面观感同源）。
pub fn gradient_pair_of((r, g, b): (u32, u32, u32)) -> (Hsla, Hsla) {
    let dark = (
        ((r as f32 * 0.55) as u32).min(255),
        ((g as f32 * 0.55) as u32).min(255),
        ((b as f32 * 0.55) as u32).min(255),
    );
    (c((r << 16) | (g << 8) | b), c((dark.0 << 16) | (dark.1 << 8) | dark.2))
}

/// 解析 `#rrggbb` 渐变字符串（`MixGroup.grad` 两色）→ [`Hsla`]；解析失败回退强调色。
pub fn from_hex_str(s: &str) -> Hsla {
    let hex = u32::from_str_radix(s.trim_start_matches('#'), 16).unwrap_or(ACCENT);
    c(hex)
}

/// Hsla → `#rrggbb`（供 [`grad_css`] 复刻原型 CSS 写法）。
fn hsla_hex(color: Hsla) -> String {
    let rgba: gpui::Rgba = color.into();
    let to8 = |v: f32| ((v * 255.0).round() as u32).min(255);
    format!(
        "#{:02x}{:02x}{:02x}",
        to8(rgba.r),
        to8(rgba.g),
        to8(rgba.b)
    )
}

/// 由 `Track.grad` 生成原型 `linear-gradient(135deg,#..,#..)` 字符串（任务契约要求；
/// GPUI 绘制不走本函数，见 [`gradient_cover`]）。
pub fn grad_css((r, g, b): (u32, u32, u32)) -> String {
    let (light, dark) = gradient_pair_of((r, g, b));
    format!("linear-gradient(135deg,{}, {})", hsla_hex(light), hsla_hex(dark))
}

/// 渐变封面（GPUI 无 CSS 渐变）：`canvas` 绘制 `135deg` 线性渐变圆角方块，
/// 通常 `absolute inset-0` 铺满父容器。`(light, dark)` 为 `[0%, 100%]` 两停靠色。
pub fn gradient_cover((light, dark): (Hsla, Hsla), radius: f32) -> AnyElement {
    gradient_cover_impl((light, dark), radius, None)
}

/// 同 [`gradient_cover`]，但把 canvas **实际绘制盒**写入 `record`：供 fs 进度填充的
/// 「填充随位置增长」回归断言——canvas 布局高度曾塌缩为 0（真机复报五：填充隐形只留轨道
/// 一色），此盒可直接断言绘制面积非零且随 frac 变化（`bounds_canvas` 同款 Cell 姿势）。
pub fn gradient_cover_tracked(
    (light, dark): (Hsla, Hsla),
    radius: f32,
    record: Rc<Cell<Bounds<Pixels>>>,
) -> AnyElement {
    gradient_cover_impl((light, dark), radius, Some(record))
}

/// 渐变绘制公共实现：canvas 自身 `absolute().inset_0()` 填最近定位祖先（gpui 所有 div 默认
/// `position: relative` = 直接父 div）——否则 block 子盒高度塌缩 0（布局只横向拉伸、纵向为
/// content 高 0）→ 渐变面积 0 不可见（真机复报五根因）。
fn gradient_cover_impl(
    (light, dark): (Hsla, Hsla),
    radius: f32,
    record: Option<Rc<Cell<Bounds<Pixels>>>>,
) -> AnyElement {
    canvas(
        |_, _, _| {},
        move |bounds, _, window, _cx| {
            if let Some(r) = &record {
                r.set(bounds);
            }
            window.paint_quad(
                fill(
                    bounds,
                    linear_gradient(
                        135.,
                        linear_color_stop(dark, 0.),
                        linear_color_stop(light, 1.),
                    ),
                )
                .corner_radii(px(radius)),
            );
        },
    )
    .absolute()
    .inset_0()
    .into_any_element()
}

/// 供可交互条（进度/音量）记录元素窗口坐标盒的 canvas 辅助：prepaint 期把自身布局盒
/// 写入 `bounds`，事件回调据此反算比例（对齐 video 查看器 `strip_element` 姿势）。
/// `absolute().inset_0()`：canvas 脱离布局流、在任何容器上下文（含 flex-row）都记录
/// **父条完整盒**（宽>0）——旧实现无尺寸 canvas 在 flex-row（如 `#fs-vol`）里是 flex
/// item → 宽塌缩 0 → `fraction_at` 拒算 → 全屏音量/进度点拖失效（真机复报）。
pub fn bounds_canvas(bounds: Rc<Cell<Bounds<Pixels>>>) -> AnyElement {
    canvas(
        move |b, _w, _app| bounds.set(b),
        |_, _, _, _| {},
    )
    .absolute()
    .inset_0()
    .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_constants_match_proto_colors() {
        // 逐字钉死原型 `:root` 十六进制，防抄写漂移。
        assert_eq!(BG_DEEP, 0x0d0b1a);
        assert_eq!(BG_PRIMARY, 0x13112a);
        assert_eq!(BG_SECONDARY, 0x1a1738);
        assert_eq!(BG_CARD, 0x201d40);
        assert_eq!(BG_HOVER, 0x282452);
        assert_eq!(ACCENT, 0x7c5cfc);
        assert_eq!(ACCENT_LIGHT, 0xa78bfa);
        assert_eq!(TEXT, 0xf0edff);
        assert_eq!(TEXT2, 0x9892b8);
        assert_eq!(TEXT3, 0x5e5880);
        assert_eq!(PINK, 0xf472b6);
        assert_eq!(GREEN, 0x34d399);
        assert_eq!(ORANGE, 0xfb923c);
        assert_eq!(BLUE, 0x60a5fa);
    }

    #[test]
    fn grad_css_produces_proto_css_gradient() {
        let s = grad_css((0x7c, 0x5c, 0xfc));
        assert!(s.starts_with("linear-gradient(135deg,"), "s={s}");
        assert!(s.contains('#'), "应变出 #rrggbb 两色：{s}");
        assert_eq!(grad_css((0x7c, 0x5c, 0xfc)), grad_css((0x7c, 0x5c, 0xfc)));
    }

    #[test]
    fn gradient_pair_darkens_single_color() {
        let (light, dark) = gradient_pair_of((0x7c, 0x5c, 0xfc));
        assert_ne!(light, dark, "两色应不同（主色 vs 加深变体）");
        let d: gpui::Rgba = dark.into();
        let l: gpui::Rgba = light.into();
        assert!(d.r < l.r && d.g < l.g && d.b < l.b, "加深变体各分量更暗");
    }
}
