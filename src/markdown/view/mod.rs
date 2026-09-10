//! Markdown 渲染器的视图层：行级虚拟化 + gpui_component 滚动条（双向）。
//!
//! 核心思路（参考 vellum / zorite 的行级虚拟化）：
//! - 文档被拆成**一行一条**的渲染行（见 [`crate::markdown::parser`]）；
//! - 用 gpui 的 `List` 做虚拟化：只构建 / 绘制可见行，滚动时按需重建；
//! - 横向溢出走**原生滚动容器**：`List` 的内容宽（估算的最宽行 `content_w`）由
//!   `w(content_w).min_w_full()` 在布局期一次算定（超宽行撑宽、普通文档撑满视口），
//!   外面套一层 `overflow_x_scroll` + `track_scroll(&h_handle)`；
//! - gpui_component 的滚动条都按官方叠层挂法（`absolute + inset_0` + 滚动条本体）
//!   挂在 `md-preview` 上：横向跟踪 `h_handle`、竖向跟踪 `ListState`，组件自绘
//!   （含拖拽、悬浮、自动隐藏），保证内容超宽时横条一定可见。
//!
//! 本目录按职责拆分（替代原先的单文件 `view.rs` 长文件）：
//! - `style`：排版参数快照（[`style::StyleParams`]）
//! - `dimensions`：行宽 / 表格列宽估算、图片头尺寸解析
//! - `row`：单行渲染（[`row::render_row`]）
//! - `table`：GFM 表格渲染（[`table::render_table`] / `render_table_row`）
//! - `toc`：目录悬浮按钮 + 下拉菜单（[`toc::build_toc_trigger`]）
//! - `tests`：视图层测试

mod dimensions;
mod row;
mod style;
mod table;
mod toc;

#[cfg(test)]
mod tests;

use std::rc::Rc;

use gpui::{
    div, list, px, App, Context, Hsla, InteractiveElement, IntoElement, ListAlignment, ListState,
    ParentElement, Render, ScrollDelta, ScrollHandle, ScrollWheelEvent, StatefulInteractiveElement,
    Styled, Window,
};
use gpui_component::scroll::{Scrollbar, ScrollbarMode, ScrollableElement as _};

use crate::markdown::parser::{parse_to_rows, Row, RowKind};

use dimensions::{est_row_width, image_dimensions, image_dimensions_bytes, WIDTH_BUFFER_PX};
use row::render_row;
use style::StyleParams;
use toc::build_toc_trigger;

/// 行级虚拟化在视口上下各多构建的像素量（保证快速滚动时内容连续）。
const ROW_OVERDRAW_PX: f32 = 600.0;

/// 自研 Markdown 预览器（行级虚拟化 + 双向滚动条）。
pub struct MarkdownPreview {
    rows: Rc<Vec<Rc<Row>>>,
    list_state: ListState,
    /// 背景色覆盖（由应用传入，与编辑区保持一致；None 用主题背景）。
    bg: Option<Hsla>,
    /// 估算的最宽内容（px，跨全部行），决定横向滚动范围。
    content_w: f32,
    /// 横向滚动句柄（跟踪外层横向滚动容器，供 `horizontal_scrollbar` 使用）。
    h_handle: ScrollHandle,
    /// 是否启用文本自动换行（true：段落/标题/引述/列表折行、无横向滚动；false：单行横向滚动）。
    wrap: bool,
    /// 正文字号覆盖（None = 用主题默认；EPUB 缩放字号时设置）。
    font_override: Option<f32>,
    /// 目录：文档内标题行的 (行号, 级别, 文本)。
    toc: Vec<(usize, u8, String)>,
    /// markdown 文件所在目录（用于解析相对路径图片；None = 仅绝对路径/URL 有效）。
    base_dir: Option<std::path::PathBuf>,
    /// 内存图片注册表：虚拟 src（`epubimg://...`）→ 图片字节（不落盘）。
    memory_images: std::collections::HashMap<String, std::sync::Arc<gpui::Image>>,
    /// 代码块的横向滚动句柄（行号 → 句柄）。句柄跨帧稳定，供滚动条拖动。
    code_scroll: std::collections::HashMap<usize, ScrollHandle>,
    /// 表格的横向滚动句柄（行号 → 句柄）。句柄跨帧稳定，供滚动条拖动。
    table_scroll: std::collections::HashMap<usize, ScrollHandle>,
}

impl MarkdownPreview {
    pub fn new(_cx: &mut App) -> Self {
        let list_state = ListState::new(0, ListAlignment::Top, px(ROW_OVERDRAW_PX)).measure_all();
        Self {
            rows: Rc::new(Vec::new()),
            list_state,
            bg: None,
            content_w: 0.0,
            h_handle: ScrollHandle::new(),
            wrap: false,
            font_override: None,
            toc: Vec::new(),
            base_dir: None,
            memory_images: Default::default(),
            code_scroll: Default::default(),
            table_scroll: Default::default(),
        }
    }

    /// 注册内存图片表（虚拟 src → 图片字节，不落盘），EPUB 用 `epubimg://...`。
    /// 需在 set_source 之前调用；后续每次 set_source 都会按虚拟 src 重新挂载。
    pub fn set_memory_images(
        &mut self,
        images: std::collections::HashMap<String, std::sync::Arc<gpui::Image>>,
    ) {
        self.memory_images = images;
    }

    /// 设置 markdown 文件目录（相对路径图片解析依据）。
    pub fn set_base_dir(&mut self, dir: Option<std::path::PathBuf>) {
        self.base_dir = dir;
    }

    /// 解析图片源：URL / Windows 绝对路径 / 已绝对路径原样返回，其余基于 base 目录拼成绝对路径。
    fn resolve_image_src(base: &std::path::Path, src: &str) -> String {
        let lower = src.to_lowercase();
        if lower.starts_with("http://")
            || lower.starts_with("https://")
            || lower.starts_with("file://")
        {
            return src.to_string();
        }
        if src.len() >= 2 && src.as_bytes()[1] == b':' {
            return src.to_string();
        }
        if std::path::Path::new(src).is_absolute() {
            return src.to_string();
        }
        base.join(src).to_string_lossy().to_string()
    }

    /// 设置预览背景色（打开/同步时由应用传入，使预览与编辑区同底色）。
    pub fn set_background(&mut self, bg: Hsla) {
        self.bg = Some(bg);
    }

    /// 设置正文字号覆盖（px）。None 使用主题默认；用于 EPUB 等阅读场景的缩放。
    pub fn set_font_override(&mut self, size: Option<f32>, cx: &mut App) {
        if self.font_override == size {
            return;
        }
        self.font_override = size;
        self.remeasure_dimensions(cx);
        self.list_state.remeasure();
    }

    /// 在当前字号基础上增减（EPUB 滚动缩放）；返回新的字号覆盖。
    pub fn bump_font_size(&mut self, delta: f32, cx: &mut App, current_default: f32) {
        let base = self.font_override.unwrap_or(current_default);
        self.set_font_override(Some((base + delta).clamp(9.0, 32.0)), cx);
    }

    /// 重建尺寸估算（换行/字号/内容变化后调用）。
    fn remeasure_dimensions(&mut self, cx: &mut App) {
        let p = StyleParams::from_theme_with_font(cx, self.font_override);
        self.content_w = if self.wrap {
            0.0
        } else {
            self.rows
                .iter()
                .map(|r| est_row_width(r, &p))
                .fold(0.0, f32::max)
                + WIDTH_BUFFER_PX
        };
    }

    /// 设置是否文本自动换行（EPUB 阅读等场景）。切换后无需重解析，直接触发重渲染。
    pub fn set_wrap(&mut self, wrap: bool, cx: &mut App) {
        if self.wrap == wrap {
            return;
        }
        self.wrap = wrap;
        self.remeasure_dimensions(cx);
        self.list_state.remeasure();
    }

    /// 当前是否文本自动换行。
    pub fn is_wrap(&self) -> bool {
        self.wrap
    }

    /// 目录条目：(行号, 标题级别, 标题文本)。供侧边栏目录视图使用。
    pub(crate) fn toc_entries(&self) -> &[(usize, u8, String)] {
        &self.toc
    }

    /// 滚动到指定行（目录跳转）。
    pub fn scroll_to_row(&mut self, ix: usize) {
        if ix >= self.rows.len() {
            return;
        }
        self.list_state.scroll_to_reveal_item(ix);
    }

    /// 返回覆盖**源码第 `line` 行**（0 基）的渲染行下标；空/无行返回 `None`。
    pub fn row_for_line(&self, line: usize) -> Option<usize> {
        if self.rows.is_empty() {
            return None;
        }
        // 最后一个 src_line <= line 的渲染行即覆盖该源码行的行（二分）
        let (mut lo, mut hi) = (0usize, self.rows.len());
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.rows[mid].src_line <= line {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        Some(lo.saturating_sub(1))
    }

    /// 滚动到包含**源码第 `line` 行**（0 基）的渲染行：搜索跳转（EPUB 只读预览）用。
    pub fn scroll_to_line(&mut self, line: usize) {
        if let Some(ix) = self.row_for_line(line) {
            self.list_state.scroll_to_reveal_item(ix);
        }
    }

    /// 设置预览源码（仅影响预览，原文不变）。
    pub fn set_source(&mut self, src: &str, cx: &mut App) {
        let base = self.base_dir.clone();
        let mut parsed = parse_to_rows(src);
        for r in parsed.iter_mut() {
            if r.kind != RowKind::Image {
                continue;
            }
            if let Some(s) = r.src.take() {
                // 内存图片（EPUB/PDF 虚拟 src）：直接查注册表挂载字节（不落盘、不读文件）
                if let Some(img) = self.memory_images.get(s.as_ref()) {
                    if let Some((w, h)) = image_dimensions_bytes(&img.bytes) {
                        r.img_w = Some(w);
                        r.img_h = Some(h);
                    }
                    r.img_data = Some(img.clone());
                    r.src = Some(s.into());
                    continue;
                }
                let resolved = if let Some(base) = &base {
                    Self::resolve_image_src(base, &s)
                } else {
                    s.to_string()
                };
                // 本地文件 → 读固有尺寸（保持比例显示；URL/不可读时保持 None）
                if !resolved.starts_with("http://")
                    && !resolved.starts_with("https://")
                    && !resolved.starts_with("file://")
                {
                    if let Some((w, h)) = image_dimensions(std::path::Path::new(&resolved)) {
                        r.img_w = Some(w);
                        r.img_h = Some(h);
                    }
                }
                r.src = Some(resolved.into());
            }
        }
        let rows: Vec<Rc<Row>> = parsed.into_iter().map(Rc::new).collect();
        // 提取目录：标题行（行号, 级别, 清理后的文本）
        self.toc = rows
            .iter()
            .enumerate()
            .filter_map(|(ix, r)| match r.kind {
                RowKind::Heading(level) => Some((ix, level, r.text.to_string())),
                _ => None,
            })
            .collect();
        // 代码行 → 稳定的横向滚动句柄（供块内滚动条拖动）
        self.code_scroll = rows
            .iter()
            .enumerate()
            .filter(|(_, r)| r.kind == RowKind::Code)
            .map(|(ix, _)| (ix, ScrollHandle::new()))
            .collect();
        // 表格行 → 稳定的横向滚动句柄（供块内滚动条拖动）
        self.table_scroll = rows
            .iter()
            .enumerate()
            .filter(|(_, r)| r.kind == RowKind::Table)
            .map(|(ix, _)| (ix, ScrollHandle::new()))
            .collect();
        self.rows = Rc::new(rows);
        self.remeasure_dimensions(cx);
        if self.rows.len() != self.list_state.item_count() {
            // 行数变化 → 重建并 measure_all，保证滚动条范围从一开始就正确
            self.list_state =
                ListState::new(self.rows.len(), ListAlignment::Top, px(ROW_OVERDRAW_PX))
                    .measure_all();
        } else {
            // 行数不变但内容变了 → 重新测量高度（保留滚动位置）
            self.list_state.remeasure();
        }
    }
}

impl Render for MarkdownPreview {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = self.rows.clone();
        let p = Rc::new(StyleParams::from_theme_with_font(cx, self.font_override));
        let bg = self.bg.unwrap_or(p.bg);
        let list_state = self.list_state.clone();
        let content_w = self.content_w;
        let wrap = self.wrap;
        let code_scroll = self.code_scroll.clone();
        let table_scroll = self.table_scroll.clone();
        // 目录：供目录菜单渲染
        let toc = self.toc.clone();

        if cfg!(debug_assertions) {
            let h = &self.h_handle;
            let file = std::env::temp_dir()
                .join("kilo")
                .join("mdhdebug.txt");
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&file)
            {
                use std::io::Write;
                let _ = writeln!(
                    f,
                    "render: wrap={wrap} content_w={content_w:.0} max_off=({:.0},{:.0}) bounds={:?}",
                    h.max_offset().x.as_f32(),
                    h.max_offset().y.as_f32(),
                    h.bounds(),
                );
            }
        }

        let content = {
            let p = p.clone();
            let code_scroll = code_scroll;
            let table_scroll = table_scroll;
            list(list_state.clone(), move |ix, _window, cx| {
                let ch = code_scroll.get(&ix).cloned();
                let th = table_scroll.get(&ix).cloned();
                render_row(&rows[ix], &p, wrap, ch, th, cx)
            })
        }
        .size_full();

        // 预览核心：换行（宽随视口）或非换行（横向滚动）共用外层横向滚动容器。
        // 换行模式：宽包装层保持视口宽（正文按视口折行），超宽的代码带溢出交由 harea 统一横向滚动；
        // 非换行模式：宽包装层 = max(估算最宽内容, 视口宽)，超宽产生横向溢出。
        let inner = div()
            .id("md-list-wrap")
            .debug_selector(|| "md-list-wrap".into())
            .w(px(content_w))
            .min_w_full()
            .h_full()
            .child(content);
        // 横向滚动容器：原生 `overflow_x_scroll`，由 `h_handle` 跟踪。
        let harea = div()
            .id("md-harea")
            .debug_selector(|| "md-harea".into())
            .size_full()
            .overflow_x_scroll()
            .restrict_scroll_to_axis()
            .track_scroll(&self.h_handle)
            .child(inner);
        let core: gpui::AnyElement = div()
            .id("md-preview")
            .debug_selector(|| "md-preview".into())
            .size_full()
            .relative()
            .bg(bg)
            .child(harea)
            .child(
                div()
                    .id("md-hscrollbar-overlay")
                    .debug_selector(|| "md-hscrollbar-overlay".into())
                    .absolute()
                    .inset_0()
                    .child(
                        Scrollbar::horizontal(&self.h_handle)
                            .mode(ScrollbarMode::Always)
                            .viewport_from_layout(),
                    ),
            )
            .vertical_scrollbar(&list_state)
            .into_any_element();

        // 外层容器：Ctrl+滚轮缩放字号（EPUB 阅读）；相对定位承载目录浮层/面板开关。
        let root = div()
            .id("md-preview-root")
            .debug_selector(|| "md-preview-root".into())
            .size_full()
            .relative()
            .on_scroll_wheel(cx.listener(
                |this: &mut MarkdownPreview, event: &ScrollWheelEvent, _w, cx| {
                    if !event.modifiers.control {
                        return;
                    }
                    let delta_y = match event.delta {
                        ScrollDelta::Lines(lines) => lines.y,
                        ScrollDelta::Pixels(pixels) => f32::from(pixels.y),
                    };
                    let step = delta_y.clamp(-1.0, 1.0);
                    if step != 0.0 {
                        let default =
                            StyleParams::from_theme_with_font(cx, None).para_size.as_f32();
                        this.bump_font_size(step * 1.5, cx, default);
                        cx.notify();
                    }
                },
            ));

        // 目录：右上角悬浮按钮 + 下拉菜单（见 [`toc`] 模块）
        let toc_trigger = build_toc_trigger(cx.weak_entity(), toc, p.fg);

        let toc_layer = vec![
            div()
                .id("md-toc-toggle-wrap")
                .debug_selector(|| "md-toc-toggle-wrap".into())
                .absolute()
                .top(px(8.))
                .right(px(8.))
                .child(toc_trigger)
                .into_any_element(),
        ];

        root.child(core).children(toc_layer)
    }
}
