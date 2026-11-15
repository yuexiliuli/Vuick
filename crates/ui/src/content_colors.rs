//! 内容渲染层颜色令牌（Pattern 4.1 / 4.2 / 4.6 收编）：把散落在 PDF 渲染 /
//! WYSIWYG 绘制 / shell_pdf 画布预设里的**硬编码内容色字面量**集中为命名常量。
//!
//! 定位：这里收的是「内容渲染层」颜色——叠加高亮 / 警报条 / 玻璃控件 / 纸面表面色 /
//! 画布预设——与 [`theme::ColorScheme`] 的主题语义字段互补（**不动** theme/motion/palette）。
//! 值 = 既有字面量逐字节一致（`#[cfg(test)]` 钉值测试守护，防未来漂移改像素）；
//! 明/暗模式有别者成对 `…_DARK` / `…_LIGHT`、调用方按 `Theme::global(cx).is_dark()` 取（同旧行为）；
//! `[u8;3]` 画布预设字节供 [`RecolorParams`] 消费（Hsla 由 `gpui::rgb(…).into()` 同转换派生）。

use gpui::Hsla;

/// RGBA 四浮点通道（0..=1）→ Hsla。逐字复刻 gpui `From<Rgba> for Hsla` 的 HSL 换算
/// （h/s/l/a 不做二次 clamp，与运行时 `rgba(hex).into()` 逐 bit 一致）。
const fn float_to_hsla(r: f32, g: f32, b: f32, a: f32) -> Hsla {
    let max = r.max(g.max(b));
    let min = r.min(g.min(b));
    let delta = max - min;
    let l = (max + min) / 2.0;
    let s = if l == 0.0 || l == 1.0 {
        0.0
    } else if l < 0.5 {
        delta / (2.0 * l)
    } else {
        delta / (2.0 - 2.0 * l)
    };
    // gpui 用 `((g - b) / delta).rem_euclid(6.0) / 6.0`；本模块钉值色相 red-max 情形
    // `(g-b)/delta` 恒 ∈ [0,6)（rem_euclid 恒等，`#[cfg(test)]` 逐值钉死守护）——
    // const 环境无 `f32::rem_euclid`，用恒等形式；未来新增红主导且蓝>绿的颜色需复核。
    let h = if delta == 0.0 {
        0.0
    } else if max == r {
        ((g - b) / delta) / 6.0
    } else if max == g {
        ((b - r) / delta + 2.0) / 6.0
    } else {
        ((r - g) / delta + 4.0) / 6.0
    };
    Hsla { h, s, l, a }
}

/// RGBA 十六进制（4 字节 r/g/b/a，同 `gpui::rgba(hex)`）→ Hsla。
const fn rgba_to_hsla(hex: u32) -> Hsla {
    let bytes = hex.to_be_bytes();
    let a = (bytes[3] as f32) / 255.0;
    float_to_hsla(bytes[0] as f32 / 255.0, bytes[1] as f32 / 255.0, bytes[2] as f32 / 255.0, a)
}

/// RGB 十六进制（跳最高字节、alpha=1，同 `gpui::rgb(hex)`）→ Hsla。
const fn rgb_to_hsla(hex: u32) -> Hsla {
    let bytes = hex.to_be_bytes();
    float_to_hsla(bytes[1] as f32 / 255.0, bytes[2] as f32 / 255.0, bytes[3] as f32 / 255.0, 1.0)
}

// ============================ PDF 阅读器（crates/pdf） ============================

/// PDF 检索命中（非激活）高亮蓝：暗档 `rgba(0x60a5fab0)`。
pub const PDF_SEARCH_HIT_BLUE_DARK: Hsla = rgba_to_hsla(0x60a5fab0);
/// PDF 检索命中（非激活）高亮蓝：亮档 `rgba(0x60a5fa70)`。
pub const PDF_SEARCH_HIT_BLUE_LIGHT: Hsla = rgba_to_hsla(0x60a5fa70);
/// PDF 检索**激活**命中高亮橙 `rgba(0xf59e0bcc)`：与 WYSIWYG 告警橙同色相 `0xf59e0b`，但
/// alpha 档不同（cc vs ff）→ 非同一 Hsla、不合并；都集中在本模块，同文件改一处即可。
pub const PDF_SEARCH_ACTIVE_AMBER: Hsla = rgba_to_hsla(0xf59e0bcc);
/// PDF 待存注释高亮底（半透黄）`rgba(0xffe67866)`。
pub const PDF_ANNOT_HIGHLIGHT_YELLOW: Hsla = rgba_to_hsla(0xffe67866);
/// PDF 待存注释删除线红 `rgba(0xe63c3cff)`。
pub const PDF_ANNOT_STRIKE_RED: Hsla = rgba_to_hsla(0xe63c3cff);
/// PDF 待存注释下划线蓝 `rgba(0x3c78e6ff)`。
pub const PDF_ANNOT_UNDERLINE_BLUE: Hsla = rgba_to_hsla(0x3c78e6ff);
/// PDF 待存注释「灰框」（聚焦包围盒边框）`rgba(0x6b6b6bdd)`：与 `PDF_ANNOT_FOCUS_FILL_GRAY`
/// 同色相 `6b6b6b`、alpha 档不同（dd vs 14），为两个 const。
pub const PDF_ANNOT_FRAME_GRAY: Hsla = rgba_to_hsla(0x6b6b6bdd);
/// PDF 待存注释聚焦填充底（灰框同色相低透明）`rgba(0x6b6b6b14)`。
pub const PDF_ANNOT_FOCUS_FILL_GRAY: Hsla = rgba_to_hsla(0x6b6b6b14);
/// PDF 图片选中描框青绿：暗档 `rgba(0x2ecc8fff)`。
pub const PDF_IMAGE_SELECT_GREEN_DARK: Hsla = rgba_to_hsla(0x2ecc8fff);
/// PDF 图片选中描框青绿：亮档 `rgba(0x12a866ff)`。
pub const PDF_IMAGE_SELECT_GREEN_LIGHT: Hsla = rgba_to_hsla(0x12a866ff);
/// PDF 页卡纸面底：暗档 `rgb(0x1f2937)`（读纸偏好评级，属 config 非 chrome）。
pub const PDF_PAPER_BG_DARK: Hsla = rgb_to_hsla(0x1f2937);
/// PDF 页卡纸边框：暗档 `rgb(0x484848)`。
pub const PDF_PAPER_BORDER_DARK: Hsla = rgb_to_hsla(0x484848);
/// PDF 页卡纸面底：亮档 `gpui::white()`（h=0 s=0 l=1 a=1）。
pub const PDF_PAPER_BG_LIGHT: Hsla = gpui::white();
/// PDF 页卡纸边框：亮档 `rgb(0xe0e0e0)`。
pub const PDF_PAPER_BORDER_LIGHT: Hsla = rgb_to_hsla(0xe0e0e0);

// ===================== WYSIWYG 引擎（crates/markdown_wysiwyg_engine） =====================

/// 警报条底（明/暗共用 base；`rgb(0x3b82f6)`，亮/暗提亮在 `alert_color` 内做）。
pub const WY_ALERT_NOTE_BASE: Hsla = rgb_to_hsla(0x3b82f6);
/// 警报条底 `rgb(0x10b981)`。
pub const WY_ALERT_TIP_BASE: Hsla = rgb_to_hsla(0x10b981);
/// 警报条底 `rgb(0x8b5cf6)`。
pub const WY_ALERT_IMPORTANT_BASE: Hsla = rgb_to_hsla(0x8b5cf6);
/// 警报条底 `rgb(0xf59e0b)`（alpha=1）：与 PDF 激活橙 `PDF_SEARCH_ACTIVE_AMBER`（α=cc）色相
/// 相同但 alpha 不同，独立成 const，同文件改色一处即可。
pub const WY_ALERT_WARNING_BASE: Hsla = rgb_to_hsla(0xf59e0b);
/// 警报条底 `rgb(0xef4444)`。
pub const WY_ALERT_CAUTION_BASE: Hsla = rgb_to_hsla(0xef4444);
/// 注记跳转加亮：暗档 `rgba(0x58a6ff59)`。
pub const WY_NOTE_JUMP_HL_DARK: Hsla = rgba_to_hsla(0x58a6ff59);
/// 注记跳转加亮：亮档 `rgba(0x9ec9ff88)`。
pub const WY_NOTE_JUMP_HL_LIGHT: Hsla = rgba_to_hsla(0x9ec9ff88);
/// 搜索命中加亮：暗档 `rgba(0xf59e0b47)`。
pub const WY_SEARCH_HIT_HL_DARK: Hsla = rgba_to_hsla(0xf59e0b47);
/// 搜索命中加亮：亮档 `rgba(0xfef08a8c)`。
pub const WY_SEARCH_HIT_HL_LIGHT: Hsla = rgba_to_hsla(0xfef08a8c);
/// 图片缩放柄黑玻璃底 `hsla(0,0,0.9,0.65)`（与编辑钮 0.6 档有别）。
pub const WY_GLASS_HANDLE_BG: Hsla = gpui::hsla(0., 0., 0.9, 0.65);
/// 图片编辑钮黑玻璃底 `hsla(0,0,0.9,0.6)`。
pub const WY_GLASS_BTN_BG: Hsla = gpui::hsla(0., 0., 0.9, 0.6);
/// 玻璃控件白细边（缩放柄 / 编辑钮共用）`hsla(0,0,1,0.55)`。
pub const WY_GLASS_BORDER: Hsla = gpui::hsla(0., 0., 1., 0.55);
/// 编辑钮三点黑点 `hsla(0,0,0.22,1)`。
pub const WY_GLASS_DOT: Hsla = gpui::hsla(0., 0., 0.22, 1.);

// ====================== PDF 画布预设（crates/shell_pdf） ======================

/// 画布预设「夜间」底 `[0x20,0x20,0x20]`（字节供 `RecolorParams.bg`；Hsla 由
/// `gpui::rgb(0x202020).into()` 派生）。
pub const CANVAS_PRESET_NIGHT: [u8; 3] = [0x20, 0x20, 0x20];
/// 画布预设「护眼」底 `[0xC7,0xED,0xCC]`。
pub const CANVAS_PRESET_EYE: [u8; 3] = [0xC7, 0xED, 0xCC];
/// 画布预设「羊皮纸」底 `[0xFB,0xF0,0xD9]`。
pub const CANVAS_PRESET_PARCHMENT: [u8; 3] = [0xFB, 0xF0, 0xD9];
/// 深色档墨迹色 `[0xD8,0xD8,0xD8]`（浅灰，`RecolorParams.ink`；如需改白字只动此 const）。
pub const CANVAS_DARK_INK: [u8; 3] = [0xD8, 0xD8, 0xD8];

#[cfg(test)]
mod tests {
    use super::*;

    /// 运行时参照：`rgba(hex).into()`（与 const 侧同转换，校验 const 复刻逐 bit 一致）。
    fn rgba_expect(hex: u32) -> Hsla {
        gpui::rgba(hex).into()
    }
    /// 运行时参照：`rgb(hex).into()`。
    fn rgb_expect(hex: u32) -> Hsla {
        gpui::rgb(hex).into()
    }

    #[test]
    fn pdf_overlay_tokens_pin_original_literals() {
        assert_eq!(PDF_SEARCH_HIT_BLUE_DARK, rgba_expect(0x60a5fab0));
        assert_eq!(PDF_SEARCH_HIT_BLUE_LIGHT, rgba_expect(0x60a5fa70));
        assert_eq!(PDF_SEARCH_ACTIVE_AMBER, rgba_expect(0xf59e0bcc));
        assert_eq!(PDF_ANNOT_HIGHLIGHT_YELLOW, rgba_expect(0xffe67866));
        assert_eq!(PDF_ANNOT_STRIKE_RED, rgba_expect(0xe63c3cff));
        assert_eq!(PDF_ANNOT_UNDERLINE_BLUE, rgba_expect(0x3c78e6ff));
        assert_eq!(PDF_ANNOT_FRAME_GRAY, rgba_expect(0x6b6b6bdd));
        assert_eq!(PDF_ANNOT_FOCUS_FILL_GRAY, rgba_expect(0x6b6b6b14));
        assert_eq!(PDF_IMAGE_SELECT_GREEN_DARK, rgba_expect(0x2ecc8fff));
        assert_eq!(PDF_IMAGE_SELECT_GREEN_LIGHT, rgba_expect(0x12a866ff));
        assert_eq!(PDF_PAPER_BG_DARK, rgb_expect(0x1f2937));
        assert_eq!(PDF_PAPER_BORDER_DARK, rgb_expect(0x484848));
        assert_eq!(PDF_PAPER_BG_LIGHT, gpui::white());
        assert_eq!(PDF_PAPER_BORDER_LIGHT, rgb_expect(0xe0e0e0));
    }

    #[test]
    fn wy_tokens_pin_original_literals() {
        assert_eq!(WY_ALERT_NOTE_BASE, rgb_expect(0x3b82f6));
        assert_eq!(WY_ALERT_TIP_BASE, rgb_expect(0x10b981));
        assert_eq!(WY_ALERT_IMPORTANT_BASE, rgb_expect(0x8b5cf6));
        assert_eq!(WY_ALERT_WARNING_BASE, rgb_expect(0xf59e0b));
        assert_eq!(WY_ALERT_CAUTION_BASE, rgb_expect(0xef4444));
        assert_eq!(WY_NOTE_JUMP_HL_DARK, rgba_expect(0x58a6ff59));
        assert_eq!(WY_NOTE_JUMP_HL_LIGHT, rgba_expect(0x9ec9ff88));
        assert_eq!(WY_SEARCH_HIT_HL_DARK, rgba_expect(0xf59e0b47));
        assert_eq!(WY_SEARCH_HIT_HL_LIGHT, rgba_expect(0xfef08a8c));
        assert_eq!(WY_GLASS_HANDLE_BG, gpui::hsla(0., 0., 0.9, 0.65));
        assert_eq!(WY_GLASS_BTN_BG, gpui::hsla(0., 0., 0.9, 0.6));
        assert_eq!(WY_GLASS_BORDER, gpui::hsla(0., 0., 1., 0.55));
        assert_eq!(WY_GLASS_DOT, gpui::hsla(0., 0., 0.22, 1.));
    }

    #[test]
    fn canvas_presets_pin_original_literals() {
        assert_eq!(CANVAS_PRESET_NIGHT, [0x20, 0x20, 0x20]);
        assert_eq!(CANVAS_PRESET_EYE, [0xC7, 0xED, 0xCC]);
        assert_eq!(CANVAS_PRESET_PARCHMENT, [0xFB, 0xF0, 0xD9]);
        assert_eq!(CANVAS_DARK_INK, [0xD8, 0xD8, 0xD8]);
    }
}
