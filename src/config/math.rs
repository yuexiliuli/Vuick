//! 数学排版（RaTeX）配置常量：字号 / 设备像素比 / 排版边距 / 行内撑高上限 / 公式色。
//!
//! 单一来源：`wysiwyg::math_typeset`（光栅）与 `wysiwyg::element`（行内撑高/缩放）、
//! `wysiwyg::mod`（离屏排版/公式色）全部从这里取值，调一个参数无需满项目找。

/// 数学排版字号（px/em）——比正文略大，块级公式可读。
pub const MATH_FONT_SIZE: f32 = 22.0;
/// 数学排版设备像素比（2× 保证清晰，显示为逻辑尺寸不变）。
pub const MATH_DPR: f32 = 2.0;
/// 块级 `$$…$$` 卡片公式的排版边距（px，RaTeX padding）。
pub const MATH_PAD: f32 = 8.0;
/// 行内 `$…$` 公式的排版边距（px）：正文行高只有 ~19px，必须收紧——否则 RaTeX 的
/// 大边距会把小公式（`x^2` 等）也顶出 1 行、撑高行。
pub const INLINE_MATH_PAD: f32 = 1.5;
/// 行内**超高算子**公式（`\int`/`\sum`/`\prod` 等带上下限）允许的撑高上限
/// （× 正文行高）：只压这类公式，其余公式贴着行高不被改、不撑行。调小 = 公式更紧凑。
pub const INLINE_MATH_BAND: f32 = 1.35;
/// 数学公式文字色（RGB 十六进制，不含 alpha）；暗色主题在 `mod::math_color` 提亮。
pub const MATH_COLOR: u32 = 0x6d5bd0;
/// 暗色主题下数学公式色的提亮量（HSL 亮度 channel 增加量）
pub const MATH_COLOR_DARK_LIFT: f32 = 0.18;
