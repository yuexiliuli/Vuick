//! `impl WyRenderHost for WyEditor`（渲染管线的宿主只读态面）。
//!
//! 引擎 `shape`/`frame`/`paint`/`hit` 迁移后一律持 `(rd, host)`；本文件把宿主原来
//! 的内联方法收缩为 trait 方法（`doc`/`caret`/`is_editable`/…）——**只读宿主态**，
//! **不触碰 `self.rd`**（`reading_col_shift` 等需读 `rd` 的口径改由引擎侧以 `rd`
//! 现算，避免 re-entrant 借用）。方法体直接转发到既有宿主天然方法，行为零变化。

use std::ops::Range;
use std::sync::Arc;

use gpui::{Bounds, Entity, Image, Pixels, RetainAllImageCache, Resource};
use markdown_wysiwyg_engine::WyRenderHost;

use super::*;

impl WyRenderHost for WyEditor {
    fn doc(&self) -> &markdown::doc::Doc {
        &self.doc
    }

    fn caret(&self) -> usize {
        self.caret
    }

    fn is_editable(&self) -> bool {
        self.is_editable()
    }

    fn is_epub(&self) -> bool {
        self.is_epub()
    }

    fn last_bounds(&self) -> Option<Bounds<Pixels>> {
        self.last_bounds()
    }

    fn memory_image(&self, src: &str) -> Option<Arc<Image>> {
        self.memory_image(src)
    }

    fn image_load_resource(&self, src: &str) -> Resource {
        self.image_load_resource(src)
    }

    fn image_cache(&self) -> Entity<RetainAllImageCache> {
        self.image_cache()
    }

    fn image_intrinsic_size(&self, src: &str) -> Option<(f32, f32)> {
        // `lazy_ensure_image_size` 的宿主只读半段（EPUB 内存图字节 / 本地文件头部）；
        // 写 `rd.image_sizes` 的工作由引擎 `frame::lazy_ensure_image_size` 承担。
        if let Some(img) = self.memory_images.get(src)
            && let Some((w, h)) =
                crate::markdown::view::dimensions::image_dimensions_bytes(&img.bytes)
        {
            return Some((w as f32, h as f32));
        }
        let resolved = self.resolve_image_src(src);
        if !resolved.starts_with("http://")
            && !resolved.starts_with("https://")
            && !resolved.starts_with("file://")
            && let Some((w, h)) =
                crate::markdown::view::dimensions::image_dimensions(std::path::Path::new(&resolved))
        {
            return Some((w as f32, h as f32));
        }
        None
    }

    fn math_slot(&self, key: &str) -> Option<markdown_wysiwyg_math::MathSlot> {
        self.math_slots().borrow().get(key).cloned()
    }

    fn code_parse_lookup(&self, lang: &str, code: &str) -> CodeParseLookup {
        self.code_parse_lookup(lang, code)
    }

    fn request_code_parse(&self, lang: &str, code: &str, src_line: usize) {
        self.request_code_parse(lang, code, src_line);
    }

    fn focused_mermaid(&self) -> Option<usize> {
        self.focused_mermaid()
    }

    fn hover_image(&self) -> Option<usize> {
        self.hover_image()
    }

    fn selected_image(&self) -> Option<usize> {
        self.selected_image()
    }

    fn hover_inline_image(&self) -> Option<InlineImageRef> {
        self.hover_inline_image()
    }

    fn selected_inline_image(&self) -> Option<InlineImageRef> {
        self.selected_inline_image()
    }

    fn selection(&self) -> Option<Range<usize>> {
        self.selection()
    }

    fn search_highlight_range(&self) -> Option<Range<usize>> {
        self.search_highlight_range()
    }
}
