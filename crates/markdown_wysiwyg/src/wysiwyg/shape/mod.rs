//! 宿主 `shape` 薄 shim：行整形算法体已随引擎迁移到 `markdown_wysiwyg_engine::shape`，
//! 本模块保留宿主的旧签名入口（`editor: &WyEditor` 型）与纯函数 re-export，宿主
//! `element`（`pub(crate) use super::shape::*`）与测试对 `wysiwyg::shape::*` /
//! `element::shape::*` 的引用原样成立。

use gpui::{Font, Pixels, Window};
use markdown::doc::DocRow;
use markdown::parse::inline_style::MathRegion;
use markdown_wysiwyg_engine::{element::ShapedRow, WyTheme};

use super::WyEditor;

// —— 纯函数（签名与引擎一致）直接 re-export（路径面；部分名仅测试消费时 lib 构建报
// unused——re-export 是 `element::shape::*` 既有路径的承载，允许压）——
#[allow(unused_imports)]
pub use markdown_wysiwyg_engine::shape::{
    RowTextStyle, align_offsets_for, build_math_card_row, bullet_gutter, estimate_height,
    inline_image_hits, link_hits, math_display_lines, math_line_height_h, note_marker_hits,
    runs_to_text_runs, shape_table_row, strip_prefix_bytes, style_links_in_runs,
    table_block_height, table_row_h, take_data_line_or_placeholder, text_style_for,
};

/// 文本行整形（`WrapWyRenderHost` 只读态经 editor 转发）。算法体见引擎 `shape` 模块。
/// 预留旧签名转发（生产/测试均无消费；dormancy 保留）→ allow 压 dead_code 告警。
#[allow(dead_code)]
pub fn shape_text_row(
    editor: &WyEditor,
    r: DocRow,
    window: &mut Window,
    width: Pixels,
    theme: &WyTheme,
    base_font: &Font,
    wrap_w: f32,
) -> ShapedRow {
    markdown_wysiwyg_engine::shape::shape_text_row(
        &editor.rd,
        editor,
        r,
        window,
        width,
        theme,
        base_font,
        wrap_w,
    )
}

/// 文本行装饰化整形（警报/列表/链接/行内图）。
/// 预留旧签名转发（生产/测试均无消费；dormancy 保留）→ allow 压 dead_code 告警。
#[allow(dead_code)]
pub fn shape_text_row_decorated(
    r: DocRow,
    editor: &WyEditor,
    window: &mut Window,
    width: Pixels,
    theme: &WyTheme,
    base_font: &Font,
) -> ShapedRow {
    markdown_wysiwyg_engine::shape::shape_text_row_decorated(
        r, &editor.rd, editor, window, width, theme, base_font,
    )
}

/// 代码围栏块整形。
/// 预留旧签名转发（生产/测试均无消费；dormancy 保留）→ allow 压 dead_code 告警。
#[allow(dead_code)]
pub fn shape_code_row(
    r: DocRow,
    editor: &WyEditor,
    window: &mut Window,
    theme: &WyTheme,
    width: Pixels,
) -> ShapedRow {
    markdown_wysiwyg_engine::shape::shape_code_row(r, &editor.rd, editor, window, theme, width)
}

/// 独立图片行整形。
/// 预留旧签名转发（生产/测试均无消费；dormancy 保留）→ allow 压 dead_code 告警。
#[allow(dead_code)]
pub fn shape_image_row(
    r: DocRow,
    editor: &WyEditor,
    window: &mut Window,
    theme: &WyTheme,
    base_font: &Font,
    avail: f32,
) -> ShapedRow {
    markdown_wysiwyg_engine::shape::shape_image_row(
        r, &editor.rd, editor, window, theme, base_font, avail,
    )
}

/// 按块类型分派整形。
/// 预留旧签名转发（生产/测试均无消费；dormancy 保留）→ allow 压 dead_code 告警。
#[allow(dead_code)]
pub fn shape_docrow(
    r: DocRow,
    editor: &WyEditor,
    window: &mut Window,
    width: Pixels,
    avail: f32,
    theme: &WyTheme,
    base_font: &Font,
) -> ShapedRow {
    markdown_wysiwyg_engine::shape::shape_docrow(
        r, &editor.rd, editor, window, width, avail, theme, base_font,
    )
}

/// 独立图片行显示宽（用户覆写 / 自然宽）。
/// 预留旧签名转发（生产/测试均无消费；dormancy 保留）→ allow 压 dead_code 告警。
#[allow(dead_code)]
pub fn effective_image_w(editor: &WyEditor, src: &str, avail: f32) -> f32 {
    markdown_wysiwyg_engine::shape::effective_image_w(&editor.rd, src, avail)
}

/// 独立图片行显示高（真实宽高比 / 占位高）。
/// 预留旧签名转发（生产/测试均无消费；dormancy 保留）→ allow 压 dead_code 告警。
#[allow(dead_code)]
pub fn image_row_height(editor: &WyEditor, src: &str, avail: f32) -> Pixels {
    markdown_wysiwyg_engine::shape::image_row_height(&editor.rd, src, avail)
}

/// 独立图片块基础布局高（图片盒 + 题注）。
/// 预留旧签名转发（生产/测试均无消费；dormancy 保留）→ allow 压 dead_code 告警。
#[allow(dead_code)]
pub fn image_block_base_height(
    editor: &WyEditor,
    src: &str,
    avail: f32,
    line_h: Pixels,
) -> f32 {
    markdown_wysiwyg_engine::shape::image_block_base_height(&editor.rd, src, avail, line_h)
}

/// 数学块卡片高（与槽位无关的单值估计）。
/// 仅供宿主测试消费的旧签名转发（生产无调用）→ allow 压 dead_code 告警。
#[allow(dead_code)]
pub fn math_card_height(editor: &WyEditor, reg: &MathRegion) -> Pixels {
    markdown_wysiwyg_engine::shape::math_card_height(&editor.rd, reg)
}
