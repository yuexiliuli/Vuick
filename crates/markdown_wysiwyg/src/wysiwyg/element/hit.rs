//! 宿主 `element/hit.rs` 薄 shim：命中/坐标换算算法体已随引擎迁移到
//! `markdown_wysiwyg_engine::hit`，本模块保留宿主旧签名入口（`editor: &WyEditor`），
//! 宿主 `mouse.rs`/`input.rs`/`element/mod.rs` 与测试对 `element::hit::*` 引用原样成立。

use std::ops::Range;

use gpui::{Pixels, Point};
use markdown::parse::recognition::LinkHit;
use markdown_wysiwyg_engine::element::{Frame, ShapedRow};
use markdown_wysiwyg_engine::model::InlineImageRef;

use super::WyEditor;

/// 纯几何常量 / 类型 / 纯函数（无 editor 依赖）直接 re-export（路径面；部分名仅测试消费）。
#[allow(unused_imports)]
pub use markdown_wysiwyg_engine::hit::{
    IMAGE_EDIT_BTN_MARGIN, IMAGE_EDIT_BTN_SIZE, IMAGE_EDIT_CARD_W, IMAGE_RESIZE_HANDLE_H,
    IMAGE_RESIZE_HANDLE_MARGIN_R, IMAGE_RESIZE_HANDLE_W, INLINE_IMAGE_EDIT_BTN_MARGIN,
    INLINE_IMAGE_EDIT_BTN_SIZE, MATH_EDIT_CARD_H, MATH_EDIT_CARD_W, MATH_EDIT_EDITOR_H,
    MATH_EDIT_EDITOR_W, MATH_EDIT_TAIL_GAP_PX, NoteMarkerHit, heading_chevron_box,
    image_edit_button_local, image_edit_button_local_with, image_box_local,
    image_resize_handle_local, image_resize_handle_window, inline_image_box_local,
    inline_image_edit_button_local, inline_image_ref_of,
};

/// 源 caret → 光标条几何。
pub fn caret_geometry(editor: &WyEditor, f: &Frame) -> (bool, Pixels, Pixels) {
    markdown_wysiwyg_engine::hit::caret_geometry(&editor.rd, editor, f)
}

/// 点命中（元素局部）→ 源字节偏移。
pub fn source_offset_at_local(editor: &WyEditor, frame: &Frame, p: Point<Pixels>) -> usize {
    markdown_wysiwyg_engine::hit::source_offset_at_local(&editor.rd, editor, frame, p)
}

/// 点命中（窗口坐标）→ 源字节偏移。
pub fn source_offset_at(editor: &WyEditor, frame: &Frame, p: Point<Pixels>) -> usize {
    markdown_wysiwyg_engine::hit::source_offset_at(&editor.rd, editor, frame, p)
}

/// 元素局部坐标。
/// 预留旧签名转发（生产/测试均无消费；dormancy 保留）→ allow 压 dead_code 告警。
#[allow(dead_code)]
pub fn to_local(editor: &WyEditor, p: Point<Pixels>) -> Point<Pixels> {
    markdown_wysiwyg_engine::hit::to_local(&editor.rd, editor, p)
}

/// 高亮目标行内注记号 `n` 的绝对源字节区间。
/// 预留旧签名转发（生产/测试均无消费；dormancy 保留）→ allow 压 dead_code 告警。
#[allow(dead_code)]
pub fn note_marker_src_range(
    editor: &WyEditor,
    frame: &Frame,
    src_line: usize,
    n: usize,
    is_def: bool,
) -> Option<Range<usize>> {
    markdown_wysiwyg_engine::hit::note_marker_src_range(&editor.rd, editor, frame, src_line, n, is_def)
}

/// 图片调整条命中。
pub fn image_resize_handle_hit_at(
    editor: &WyEditor,
    frame: &Frame,
    p: Point<Pixels>,
) -> Option<(usize, f32)> {
    markdown_wysiwyg_engine::hit::image_resize_handle_hit_at(&editor.rd, editor, frame, p)
}

/// 图片盒命中。
pub fn image_box_hit_at(editor: &WyEditor, frame: &Frame, p: Point<Pixels>) -> Option<usize> {
    markdown_wysiwyg_engine::hit::image_box_hit_at(&editor.rd, editor, frame, p)
}

/// 图片编辑按钮命中。
pub fn image_edit_button_hit_at(editor: &WyEditor, frame: &Frame, p: Point<Pixels>) -> Option<usize> {
    markdown_wysiwyg_engine::hit::image_edit_button_hit_at(&editor.rd, editor, frame, p)
}

/// 行内图片本体命中。
pub fn inline_image_hit_at(
    editor: &WyEditor,
    frame: &Frame,
    p: Point<Pixels>,
) -> Option<InlineImageRef> {
    markdown_wysiwyg_engine::hit::inline_image_hit_at(&editor.rd, editor, frame, p)
}

/// 行内图片编辑按钮命中。
pub fn inline_image_edit_button_hit_at(
    editor: &WyEditor,
    frame: &Frame,
    p: Point<Pixels>,
) -> Option<InlineImageRef> {
    markdown_wysiwyg_engine::hit::inline_image_edit_button_hit_at(&editor.rd, editor, frame, p)
}

/// 任务框命中。
pub fn checkbox_hit(editor: &WyEditor, frame: &Frame, p: Point<Pixels>) -> Option<(usize, bool)> {
    markdown_wysiwyg_engine::hit::checkbox_hit(&editor.rd, editor, frame, p)
}

/// 警报折叠箭头命中。
pub fn alert_chevron_hit(editor: &WyEditor, frame: &Frame, p: Point<Pixels>) -> Option<(usize, bool)> {
    markdown_wysiwyg_engine::hit::alert_chevron_hit(&editor.rd, editor, frame, p)
}

/// 标题折叠箭头命中。
pub fn heading_chevron_hit(editor: &WyEditor, frame: &Frame, p: Point<Pixels>) -> Option<(usize, bool)> {
    markdown_wysiwyg_engine::hit::heading_chevron_hit(&editor.rd, editor, frame, p)
}

/// 行内公式命中。
pub fn inline_math_hit_at(editor: &WyEditor, frame: &Frame, p: Point<Pixels>) -> Option<Range<usize>> {
    markdown_wysiwyg_engine::hit::inline_math_hit_at(&editor.rd, editor, frame, p)
}

/// 数学块卡片命中。
pub fn math_card_hit(editor: &WyEditor, frame: &Frame, p: Point<Pixels>) -> Option<usize> {
    markdown_wysiwyg_engine::hit::math_card_hit(&editor.rd, editor, frame, p)
}

/// Mermaid 卡命中。
pub fn mermaid_card_hit(editor: &WyEditor, frame: &Frame, p: Point<Pixels>) -> Option<usize> {
    markdown_wysiwyg_engine::hit::mermaid_card_hit(&editor.rd, editor, frame, p)
}

/// 链接命中。
pub fn link_hit_at(editor: &WyEditor, frame: &Frame, p: Point<Pixels>) -> Option<LinkHit> {
    markdown_wysiwyg_engine::hit::link_hit_at(&editor.rd, editor, frame, p)
}

/// 脚注标记命中。
pub fn note_marker_hit_at(editor: &WyEditor, frame: &Frame, p: Point<Pixels>) -> Option<NoteMarkerHit> {
    markdown_wysiwyg_engine::hit::note_marker_hit_at(&editor.rd, editor, frame, p)
}

/// 代码块横向滚动条几何。
pub fn code_hscroll_geo(editor: &WyEditor, f: &Frame, row: &ShapedRow) -> Option<(f32, f32, f32, f32, f32)> {
    markdown_wysiwyg_engine::hit::code_hscroll_geo(&editor.rd, editor, f, row)
}

/// 代码块当前生效的横向滚动偏移。
/// 仅供宿主测试消费的旧签名转发（生产无调用）→ allow 压 dead_code 告警。
#[allow(dead_code)]
pub fn code_hscroll_clamped(editor: &WyEditor, f: &Frame, row: &ShapedRow) -> f32 {
    markdown_wysiwyg_engine::hit::code_hscroll_clamped(&editor.rd, editor, f, row)
}

/// 代码块横向滚动条命中。
pub fn code_hscrollbar_hit(
    editor: &WyEditor,
    frame: &Frame,
    p: Point<Pixels>,
) -> Option<(usize, bool, f32)> {
    markdown_wysiwyg_engine::hit::code_hscrollbar_hit(&editor.rd, editor, frame, p)
}

/// 表格横向滚动条几何。
pub fn table_hscroll_geo(editor: &WyEditor, f: &Frame, row: &ShapedRow) -> Option<(f32, f32, f32, f32, f32)> {
    markdown_wysiwyg_engine::hit::table_hscroll_geo(&editor.rd, editor, f, row)
}

/// 表格当前生效的横向滚动偏移。
/// 仅供宿主测试消费的旧签名转发（生产无调用）→ allow 压 dead_code 告警。
#[allow(dead_code)]
pub fn table_hscroll_clamped(editor: &WyEditor, f: &Frame, row: &ShapedRow) -> f32 {
    markdown_wysiwyg_engine::hit::table_hscroll_clamped(&editor.rd, editor, f, row)
}

/// 表格横向滚动条命中。
pub fn table_hscrollbar_hit(
    editor: &WyEditor,
    frame: &Frame,
    p: Point<Pixels>,
) -> Option<(usize, bool, f32)> {
    markdown_wysiwyg_engine::hit::table_hscrollbar_hit(&editor.rd, editor, frame, p)
}

/// 数学卡内容自然宽（px）。
/// 仅供宿主测试消费的旧签名转发（生产无调用）→ allow 压 dead_code 告警。
#[allow(dead_code)]
pub fn math_natural_w(editor: &WyEditor, row: &ShapedRow) -> f32 {
    markdown_wysiwyg_engine::hit::math_natural_w(&editor.rd, editor, row)
}

/// 数学卡横向滚动条几何。
pub fn math_hscroll_geo(editor: &WyEditor, f: &Frame, row: &ShapedRow) -> Option<(f32, f32, f32, f32, f32)> {
    markdown_wysiwyg_engine::hit::math_hscroll_geo(&editor.rd, editor, f, row)
}

/// 数学卡当前生效的横向滚动偏移。
/// 仅供宿主测试消费的旧签名转发（生产无调用）→ allow 压 dead_code 告警。
#[allow(dead_code)]
pub fn math_hscroll_clamped(editor: &WyEditor, f: &Frame, row: &ShapedRow) -> f32 {
    markdown_wysiwyg_engine::hit::math_hscroll_clamped(&editor.rd, editor, f, row)
}

/// 数学卡横向滚动条命中。
pub fn math_hscrollbar_hit(
    editor: &WyEditor,
    frame: &Frame,
    p: Point<Pixels>,
) -> Option<(usize, bool, f32)> {
    markdown_wysiwyg_engine::hit::math_hscrollbar_hit(&editor.rd, editor, frame, p)
}
