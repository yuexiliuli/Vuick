//! 宿主 `frame` 薄 shim：渲染管线函数体已随引擎迁移到 `markdown_wysiwyg_engine::frame`，
//! 本模块保留**宿主侧的旧签名入口**（`editor: &WyEditor` / `doc`）——宿主各调用点
//! （`element` 胶水 / shell / 测试）与既有路径（`wysiwyg::frame::*`）无需改动，经
//! 本 shim 把 `(&editor.rd, editor)` / `doc` 转交给引擎。行为/失效时序逐字保持。

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{Pixels, Window};
use markdown::doc::Doc;
use markdown_wysiwyg_engine::element::{BlockLayout, Frame};
use markdown_wysiwyg_engine::model::{PrebuildBlock, PrebuildPass1};
use markdown_wysiwyg_engine::WyTheme;

use super::WyEditor;

/// 帧构建（分带窗口化）——算法体见 `markdown_wysiwyg_engine::frame::build_frame`。
pub fn build_frame(
    editor: &WyEditor,
    window: &mut Window,
    width: Pixels,
    band: (f32, f32),
) -> Frame {
    markdown_wysiwyg_engine::frame::build_frame(&editor.rd, editor, window, width, band)
}

/// 首帧 Pass1 预建（输入 → 引擎纯计算）。
pub fn build_prebuild_pass1(doc: &Doc) -> PrebuildPass1 {
    markdown_wysiwyg_engine::frame::build_prebuild_pass1(doc)
}

/// 单个预建块的估算高。
/// 预留旧签名转发（生产/测试均无消费；dormancy 保留）→ allow 压 dead_code 告警。
#[allow(dead_code)]
pub(crate) fn prebuild_block_height(
    editor: &WyEditor,
    pb: &PrebuildBlock,
    theme: &WyTheme,
    avail: f32,
    line_h: Pixels,
) -> f32 {
    markdown_wysiwyg_engine::frame::prebuild_block_height(&editor.rd, pb, theme, avail, line_h)
}

/// 用预建输入在首帧物化整篇基础布局。
/// 仅供宿主测试消费的旧签名转发（生产无调用）→ allow 压 dead_code 告警。
#[allow(dead_code)]
pub(crate) fn pass1_from_prebuild(
    editor: &WyEditor,
    base: &Rc<RefCell<BlockLayout>>,
    theme: &WyTheme,
    avail: f32,
    line_h: Pixels,
) -> bool {
    markdown_wysiwyg_engine::frame::pass1_from_prebuild(
        &editor.rd,
        &editor.doc,
        base,
        theme,
        avail,
        line_h,
    )
}

/// **Pass 1**（仅结构性变更时）全块基础高 + 起始源码行（三段式增量）——算法体见引擎。
/// 仅供宿主测试消费的旧签名转发（生产无调用）→ allow 压 dead_code 告警。
#[allow(dead_code)]
pub(crate) fn pass1_base_layout(
    doc: &Doc,
    editor: &WyEditor,
    n: usize,
    base: &Rc<RefCell<BlockLayout>>,
    old_layout: Option<&Rc<RefCell<BlockLayout>>>,
    theme: &WyTheme,
    avail: f32,
    line_h: Pixels,
) {
    markdown_wysiwyg_engine::frame::pass1_base_layout(
        doc, &editor.rd, n, base, old_layout, theme, avail, line_h,
    );
}

/// 块内容指纹（shape 缓存键）——直接转发（同签名）。
pub(crate) fn block_content_id(doc: &Doc, block: usize) -> u64 {
    markdown_wysiwyg_engine::frame::block_content_id(doc, block)
}

/// D1 滚动预热。
pub(crate) fn prewarm_band(
    e: &WyEditor,
    frame: &Frame,
    window: &mut Window,
    scroll_top: f32,
) {
    markdown_wysiwyg_engine::frame::prewarm_band(&e.rd, e, frame, window, scroll_top);
}

/// I4 巨型代码块「静止权威整形」。
/// 预留旧签名转发（生产/测试均无消费；dormancy 保留）→ allow 压 dead_code 告警。
#[allow(dead_code)]
pub(crate) fn settle_giant_blocks(
    e: &WyEditor,
    frame: &Frame,
    window: &mut Window,
    scroll_top: f32,
) {
    markdown_wysiwyg_engine::frame::settle_giant_blocks(&e.rd, e, window, frame, scroll_top);
}

/// WYSIWYG 首帧 paint 阶段的 emoji 彩字管线预热（共享 `super::warm`，先画的窗格先预热）。
pub(crate) fn warm_emoji_rasterizer(window: &mut Window) {
    if !super::warm_emoji_is_done() {
        super::warm_emoji_rasterizer(window, "wysiwyg");
    }
}
