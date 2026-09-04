//! 自研 Markdown 渲染器的视图层：行级虚拟化 + gpui_component 滚动条（双向）。
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

use std::rc::Rc;

use gpui::{
    div, img, list, rgb, px, App, Context, FontWeight, Hsla, InteractiveElement, IntoElement,
    ListAlignment, ListState, ObjectFit, ParentElement, Pixels, Render, ScrollDelta, ScrollHandle,
    ScrollWheelEvent, SharedString, StatefulInteractiveElement, Styled, StyledImage, Window,
};
use gpui_component::scroll::{Scrollbar, ScrollbarMode, ScrollableElement as _};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::menu::{DropdownMenu, PopupMenu, PopupMenuItem};

use crate::markdown::parser::{parse_to_rows, Row, RowKind, TableAlign, TableData};

/// 行级虚拟化在视口上下各多构建的像素量（保证快速滚动时内容连续）。
const ROW_OVERDRAW_PX: f32 = 600.0;
/// 内容宽估算的额外缓冲（px），避免估算值微偏导致最后几个字符被截断。
const WIDTH_BUFFER_PX: f32 = 24.0;
/// 未知尺寸图片行的默认显示高度（px）。
const IMAGE_ROW_DEFAULT_H: f32 = 280.0;

/// 图片固有宽度显示上限（px）：更宽的图按 contain 缩放，避免小图被拉大。
const IMAGE_MAX_W: f32 = 600.0;

/// 读取 4 字节大端无符号整数（辅助解析 PNG/JPEG 头）。
fn be_u32(d: &[u8], at: usize) -> Option<u32> {
    d.get(at..at + 4)
        .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

/// 排版参数：渲染时从全局主题取一份“快照”，避免每行重复查主题。
/// 所有取值都对齐 gpui_component `node.rs` 的 Markdown 渲染风格：
/// 段落用 `sm`，标题用语义字号 + 加粗，代码块用 `muted` 底 + 等宽字体，
/// 引述用浅灰底 + 强调色左边框。
struct StyleParams {
    mono: SharedString,
    fg: Hsla,
    muted_fg: Hsla,
    muted_bg: Hsla,
    divider: Hsla,
    accent: Hsla,
    bg: Hsla,
    /// 引述块背景（浅灰卡片感）。
    quote_bg: Hsla,
    /// 表头底色（浅灰）。
    table_head_bg: Hsla,
    para_size: Pixels,
    para_line: Pixels,
    /// 普通文本行额外垂直间距（行高之外，用于拉开行与行之间的距离）。
    para_gap: Pixels,
    code_size: Pixels,
    code_line: Pixels,
    blank_h: Pixels,
    rule_h: Pixels,
}

impl StyleParams {
    /// 从主题构建排版参数；`font_override` 可覆盖正文/代码字号（EPUB 缩放字号用）。
    fn from_theme_with_font(cx: &mut App, font_override: Option<f32>) -> Self {
        let theme = gpui_component::Theme::global(cx);
        let tokens = theme.semantic_tokens();
        let colors = tokens.colors;
        let typo = tokens.typography;
        let sm = typo.sm;
        let mono = typo.mono_md;
        // 正文基准字号：默认用主题 sm；有覆盖时按其缩放等宽字体（等宽按 0.85 比例）
        let (para, line, code, code_line) = match font_override {
            Some(fs) => {
                let fs = fs.max(9.0);
                (
                    px(fs),
                    px(fs * 1.45),
                    px(fs * 0.88),
                    px(fs * 1.3),
                )
            }
            None => (
                sm.size,
                sm.line_height,
                mono.size,
                mono.line_height,
            ),
        };
        let is_dark = theme.is_dark();
        // 引用旧 render.rs：引述 = 浅灰底 + 粗左边框；表头 = 浅灰底。暗色模式配深色变体。
        let (quote_bg, table_head_bg) = if is_dark {
            (rgb(0x1b2431).into(), rgb(0x1f2937).into())
        } else {
            (rgb(0xf9fafb).into(), rgb(0xf3f4f6).into())
        };
        Self {
            mono: typo.mono.clone(),
            fg: colors.foreground,
            muted_fg: colors.muted_foreground,
            muted_bg: colors.muted,
            divider: colors.border,
            accent: colors.accent,
            bg: colors.background,
            quote_bg,
            table_head_bg,
            para_size: para,
            para_line: line,
            para_gap: px(4.),
            code_size: code,
            code_line: code_line,
            blank_h: px(12.),
            rule_h: px(18.),
        }
    }

    /// 第 n 级标题的字号与行高（借鉴旧 render.rs：h1=20 h2=18 h3=16 其余 15）。
    fn heading_metrics(&self, level: u8) -> (Pixels, Pixels) {
        let fs = match level {
            1 => px(20.),
            2 => px(18.),
            3 | 4 => px(16.),
            _ => px(15.),
        };
        (fs, px(fs.as_f32() * 1.45))
    }
}

/// CJK 等全角字符：宽度按 1 个 em 估算，英文数字按 0.6 em。
fn is_wide_char(c: char) -> bool {
    let cp = c as u32;
    (0x2E80..=0x9FFF).contains(&cp) // CJK 统一表意 + 部首 + 假名
        || (0xAC00..=0xD7FF).contains(&cp) // 谚文
        || (0xF900..=0xFAFF).contains(&cp) // CJK 兼容表意
        || (0x3000..=0x303F).contains(&cp) // CJK 标点
        || (0xFF00..=0xFF60).contains(&cp) // 全角符号
}

/// 估算一段文本的渲染宽度（px）。行元素的真实宽度由元素布局决定，这里仅用来给
/// 横向滚动容器定一个“足够放下超宽行”的内容宽。
fn est_text_width(text: &str, font_size: f32) -> f32 {
    text.chars().fold(0.0, |acc, c| {
        acc + if is_wide_char(c) {
            font_size
        } else {
            font_size * 0.6
        }
    })
}

/// 估算一行的渲染宽度（px）。
fn est_row_width(row: &Row, p: &StyleParams) -> f32 {
    match row.kind {
        RowKind::Blank | RowKind::Rule => 0.0,
        RowKind::Image => {
            // 图片行 by contain 撑满可用宽度 → 宽度由视口决定，不计入最宽内容
            0.0
        }
        RowKind::Code => {
            // 等宽字体按 0.65em/字符 + 左右 padding（pl_3 + pr_3 = 24px）
            row.text.chars().count() as f32 * p.code_size.as_f32() * 0.65 + 24.0
        }
        RowKind::Heading(n) => {
            let (fs, _) = p.heading_metrics(n);
            est_text_width(&row.text, fs.as_f32())
        }
        // 引述：左边框 3px + pl_3 12px 缩进
        RowKind::Quote => est_text_width(&row.text, p.para_size.as_f32()) + 16.0,
        RowKind::Bullet => {
            let prefix_w = row
                .prefix
                .as_ref()
                .map(|s| s.chars().count() as f32 * 8.0)
                .unwrap_or(0.0);
            est_text_width(&row.text, p.para_size.as_f32()) + prefix_w + 12.0
        }
        RowKind::Paragraph => est_text_width(&row.text, p.para_size.as_f32()),
        RowKind::Table => row
            .table
            .as_ref()
            .map(|t| table_column_widths(t, p).iter().sum())
            .unwrap_or(0.0),
    }
}

/// 表格每列估算宽度（px）：逐格 `est_text_width` + 左右各 10px 内距，
/// 下限 48px，与 [`render_table`] 的实际布局一致（超宽表格撑出横向滚动范围）。
fn table_column_widths(table: &TableData, p: &StyleParams) -> Vec<f32> {
    let ncols = table.header.len().max(1);
    let mut widths = vec![0.0f32; ncols];
    let font = p.para_size.as_f32();
    for (ci, cell) in table.header.iter().enumerate() {
        widths[ci] = widths[ci].max(est_text_width(cell.as_ref(), font) + 20.0);
    }
    for row in &table.rows {
        for (ci, cell) in row.iter().enumerate().take(ncols) {
            widths[ci] = widths[ci].max(est_text_width(cell.as_ref(), font) + 20.0);
        }
    }
    for w in &mut widths {
        *w = (*w).max(48.0);
    }
    widths
}

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
        }
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

    /// 从本地图片文件头读取固有尺寸 (w, h)（不整图解码，快）。
    /// 仅解析常见格式；失败返回 None，渲染回退到固定高度。
    fn image_dimensions(path: &std::path::Path) -> Option<(u32, u32)> {
        let data = std::fs::read(path).ok()?;
        let d = data.as_slice();
        if d.len() < 8 {
            return None;
        }
        // PNG: 8 字节签名 + IHDR(4 长度 + 4 类型) + 宽高(各 4 大端)
        if d.starts_with(&[0x89, b'P', b'N', b'G']) {
            return Some((be_u32(d, 16)?, be_u32(d, 20)?));
        }
        // JPEG: 扫描段找 SOF0/SOF1/SOF2 (0xFFC0..0xFFC3)，高度在前
        if d.starts_with(&[0xFF, 0xD8]) {
            let mut i = 2usize;
            while i + 9 < d.len() {
                if d[i] != 0xFF {
                    i += 1;
                    continue;
                }
                let marker = d[i + 1];
                if marker == 0xFF {
                    i += 1;
                    continue;
                }
                // standalone markers
                if matches!(marker, 0xD8 | 0xD9 | 0x01 | 0xD0..=0xD7) {
                    i += 2;
                    continue;
                }
                if i + 4 > d.len() {
                    break;
                }
                let seg_len = u16::from_be_bytes([d[i + 2], d[i + 3]]) as usize;
                if matches!(marker, 0xC0..=0xC3) {
                    return Some((
                        u16::from_be_bytes([d[i + 7], d[i + 8]]) as u32,
                        u16::from_be_bytes([d[i + 5], d[i + 6]]) as u32,
                    ));
                }
                i += 2 + seg_len;
            }
            return None;
        }
        // GIF: "GIF87a"/"GIF89a" + 逻辑屏幕宽高（小端）
        if d.starts_with(b"GIF8") {
            return Some((
                u16::from_le_bytes([d[6], d[7]]) as u32,
                u16::from_le_bytes([d[8], d[9]]) as u32,
            ));
        }
        // BMP: "BM" + 14 字节头后有 40 字节 DIB，宽高（有符号小端）
        if d.starts_with(b"BM") && d.len() >= 26 {
            let w = i32::from_le_bytes([d[18], d[19], d[20], d[21]]);
            let h = i32::from_le_bytes([d[22], d[23], d[24], d[25]]);
            if w > 0 {
                return Some((w as u32, h.unsigned_abs()));
            }
            return None;
        }
        None
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

    /// 滚动到指定行（目录跳转）。
    pub fn scroll_to_row(&mut self, ix: usize) {
        if ix >= self.rows.len() {
            return;
        }
        self.list_state.scroll_to_reveal_item(ix);
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
                    if let Some((w, h)) = Self::image_dimensions(std::path::Path::new(&resolved)) {
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
            list(list_state.clone(), move |ix, _window, cx| {
                render_row(&rows[ix], &p, wrap, cx)
            })
        }
        .size_full();

        // 预览核心：换行模式（宽随视口）或非换行（横向滚动）。各分支产出任意元素。
        let core: gpui::AnyElement = if wrap {
            let inner = div()
                .id("md-list-wrap")
                .debug_selector(|| "md-list-wrap".into())
                .w_full()
                .h_full()
                .child(content);
            div()
                .id("md-preview")
                .debug_selector(|| "md-preview".into())
                .size_full()
                .relative()
                .bg(bg)
                .child(inner)
                // 竖向滚动条（组件自绘，跟踪 ListState）
                .vertical_scrollbar(&list_state)
                .into_any_element()
        } else {
            // 非换行：宽包装层 = max(估算最宽内容, 视口宽)，超宽产生横向溢出
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
            div()
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
                .into_any_element()
        };

        // 目录：右上角悬浮按钮（gpui-component Button + DropdownMenu），
        // 点击弹出下拉菜单列出标题；条目 hover 背景/圆角由组件主题自动处理（含从下往上移动）。
        // 条目内部样式读取当前主题色，保证黑白切换后依旧清晰。
        let weak = cx.weak_entity();
        let toc_entries = toc.clone();
        let toc_trigger = Button::new("md-toc-toggle")
            .label("☰")
            .ghost()
            .w(px(30.))
            .h(px(30.))
            .rounded(px(6.))
            .text_size(px(15.))
            .text_color(p.fg)
            .dropdown_menu(move |menu: PopupMenu, _window: &mut Window, cx: &mut Context<PopupMenu>| {
                let mut m = menu;
                // 读取当前主题语义色（作用于自定义目录条目内容）
                let theme = gpui_component::Theme::global(cx);
                let colors = theme.semantic_tokens().colors;
                let color = colors.foreground;
                let muted = colors.muted_foreground;
                let accent = colors.accent;
                if toc_entries.is_empty() {
                    m = m.item(PopupMenuItem::element(move |_, _| {
                        div()
                            .w_full()
                            .px_3()
                            .py_1()
                            .text_size(px(13.))
                            .text_color(muted)
                            .child("（空目录）")
                    }));
                } else {
                    for &(row_ix, level, ref text) in &toc_entries {
                        let label = text.clone();
                        let weak = weak.clone();
                        let color = color.clone();
                        let muted = muted.clone();
                        let accent = accent.clone();
                        m = m.item(
                            PopupMenuItem::element(move |_, _| {
                                let indent = (level.saturating_sub(1) as f32) * 16.0;
                                div()
                                    .w_full()
                                    .pl(px(indent))
                                    .pr_2()
                                    .py_0()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div()
                                            .w(px(5.))
                                            .h(px(5.))
                                            .rounded_full()
                                            .bg(if level == 1 { accent } else { muted }),
                                    )
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w(px(0.))
                                            .overflow_hidden()
                                            .whitespace_nowrap()
                                            .text_size(if level == 1 { px(13.5) } else { px(13.) })
                                            .text_color(color)
                                            .font_weight(if level == 1 {
                                                FontWeight::SEMIBOLD
                                            } else {
                                                FontWeight::NORMAL
                                            })
                                            .child(label.clone()),
                                    )
                            })
                            .on_click(move |_, _window, cx| {
                                // 点击后组件菜单自动 dismiss（PopupMenuItem::ElementItem）
                                let _ = weak.update(cx, |this, cx| {
                                    this.scroll_to_row(row_ix);
                                    cx.notify();
                                });
                            }),
                        );
                    }
                }
                m
            });

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

        // 外层容器：Ctrl+滚轮缩放字号（EPUB 阅读）；相对定位承载目录浮层。
        div()
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
            ))
            .child(core)
            .children(toc_layer)
    }
}

/// 渲染单个行（固定高度、不换行、超出裁剪；超宽由外层横向滚动容器处理）。
///
/// `wrap = true` 时为**自动换行模式**：文本行高度自适应、自然折行（用于 EPUB 阅读）；
/// `wrap = false`（默认）为单行 + 横向滚动模式。
fn render_row(row: &Rc<Row>, p: &Rc<StyleParams>, wrap: bool, _cx: &mut App) -> gpui::AnyElement {
    // 表格是带 id 的有状态元素，直接整体转换，不参与下方 Div 匹配
    if let RowKind::Table = row.kind {
        let table = row.table.as_ref().expect("Table 行必须携带表格结构");
        return render_table(table, p).into_any_element();
    }
    // 独立图片行：按图片固有比例显示，高度随可用宽度自适应（缩小窗口不出现大片留白）。
    // 宽不超过 IMAGE_MAX_W；未知尺寸（URL/不可读）回退固定 IMAGE_ROW_DEFAULT_H。
    // src 为空/加载失败时显示 alt 作为 fallback。
    if let RowKind::Image = row.kind {
        let src = row.src.clone().unwrap_or_default().to_string();
        let alt = row.text.clone();
        let fallback_color = p.muted_fg;

        let img_el = if src.is_empty() {
            div()
                .text_size(px(13.))
                .text_color(fallback_color)
                .child(alt)
                .into_any_element()
        } else {
            // 本地绝对路径 → PathBuf（ImageAssetLoader 走 Resource::Path = fs::read）；
            // URL → &str（走向 Resource::Uri，经 HTTP 加载）。
            let src_is_url =
                src.starts_with("http://") || src.starts_with("https://") || src.starts_with("file://");
            let source: gpui::ImageSource = if src_is_url {
                src.as_str().into()
            } else {
                std::path::PathBuf::from(&src).into()
            };
            img(source)
                .object_fit(ObjectFit::Contain)
                .w_full()
                .h_full()
                .with_fallback(move || {
                    div()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(px(13.))
                        .text_color(fallback_color)
                        .child(alt.clone())
                        .into_any_element()
                })
                .into_any_element()
        };

        // 已知固尺寸：图片盒宽 = min(视口宽, IMAGE_MAX_W)，高由宽 * h/w 得出
        //（aspect_ratio 使高度随可用宽度缩放 → 缩小窗口不会留白）；
        // 外层铺满宽度并 justify_center，使图片盒在窄于 600px 的视口里也水平居中。
        let img_w = row.img_w;
        let img_h = row.img_h;
        let box_ = match (img_w, img_h) {
            (Some(w), Some(h)) if w > 0 && h > 0 => div()
                .w_full()
                .max_w(px(IMAGE_MAX_W))
                .aspect_ratio(w as f32 / h as f32)
                .debug_selector(|| "md-img-box".into()),
            _ => div()
                .w(px(IMAGE_MAX_W))
                .max_w_full()
                .h(px(IMAGE_ROW_DEFAULT_H))
                .min_h(px(IMAGE_ROW_DEFAULT_H))
                .debug_selector(|| "md-img-box".into()),
        };
        return div()
            .w_full()
            .flex_shrink_0()
            .px_3()
            .py_1()
            .flex()
            .justify_center()
            .items_center()
            .child(box_.child(img_el))
            .into_any_element();
    }

    // ---------- 自动换行模式：仅对文本行生效，代码/规则/空行保持原样 ----------
    if wrap {
        match row.kind {
            RowKind::Paragraph => {
                let t = row.text.clone();
                return div()
                    .w_full()
                    .min_h(px(p.para_line.as_f32() + p.para_gap.as_f32() + 6.0))
                    .flex_shrink_0()
                    .px_3()
                    .py_1()
                    .whitespace_normal()
                    .text_size(p.para_size)
                    .text_color(p.fg)
                    .child(t)
                    .into_any_element();
            }
            RowKind::Heading(n) => {
                let (fs, lh) = p.heading_metrics(n);
                let t = row.text.clone();
                return div()
                    .w_full()
                    .min_h(lh)
                    .flex_shrink_0()
                    .px_3()
                    .py_1()
                    .whitespace_normal()
                    .text_size(fs)
                    .text_color(p.fg)
                    .font_weight(FontWeight::BOLD)
                    .child(t)
                    .into_any_element();
            }
            RowKind::Quote => {
                let t = row.text.clone();
                return div()
                    .w_full()
                    .min_h(px(p.para_line.as_f32() + p.para_gap.as_f32() + 6.0))
                    .flex_shrink_0()
                    .pl_3()
                    .pr_3()
                    .py_1()
                    .border_l_2()
                    .border_color(p.accent)
                    .bg(p.quote_bg)
                    .whitespace_normal()
                    .text_size(p.para_size)
                    .text_color(p.muted_fg)
                    .child(t)
                    .into_any_element();
            }
            RowKind::Bullet => {
                let mut text = row.text.to_string();
                if let Some(prefix) = &row.prefix {
                    text = format!("{prefix} {text}");
                }
                return div()
                    .w_full()
                    .min_h(px(p.para_line.as_f32() + p.para_gap.as_f32() + 6.0))
                    .flex_shrink_0()
                    .pl_5()
                    .pr_3()
                    .py_1()
                    .whitespace_normal()
                    .text_size(p.para_size)
                    .text_color(p.fg)
                    .child(text)
                    .into_any_element();
            }
            _ => {}
        }
    }

    let el: gpui::Div = match row.kind {
        RowKind::Blank => div().h(p.blank_h).w_full().flex_shrink_0(),
        RowKind::Rule => div()
            .h(p.rule_h)
            .w_full()
            .flex_shrink_0()
            .flex()
            .items_center()
            .px_3()
            .child(div().h(px(1.)).w_full().bg(p.divider).rounded_full()),
        RowKind::Code => {
            // 代码行：整行浅灰背景带（与栅栏内其它行连成一块），等宽字体
            let t = row.text.clone();
            div()
                .h(p.code_line)
                .w_full()
                .flex_shrink_0()
                .bg(p.muted_bg)
                .flex()
                .items_center()
                .overflow_hidden()
                .child(
                    div()
                        .pl_3()
                        .pr_3()
                        .whitespace_nowrap()
                        .font_family(p.mono.clone())
                        .text_size(p.code_size)
                        .text_color(p.fg)
                        .child(t),
                )
        }
        RowKind::Heading(n) => {
            let (fs, lh) = p.heading_metrics(n);
            div()
                .h(lh)
                .w_full()
                .flex_shrink_0()
                .flex()
                .items_center()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_size(fs)
                .text_color(p.fg)
                .font_weight(FontWeight::BOLD)
                .child(row.text.clone())
        }
        RowKind::Quote => div()
            .h(px(p.para_line.as_f32() + p.para_gap.as_f32()))
            .w_full()
            .flex_shrink_0()
            .flex()
            .items_center()
            .pl_3()
            .py_1()
            .border_l_2()
            .border_color(p.accent)
            .bg(p.quote_bg)
            .overflow_hidden()
            .whitespace_nowrap()
            .text_size(p.para_size)
            .text_color(p.muted_fg)
            .child(row.text.clone()),
        RowKind::Bullet => {
            let mut text = row.text.to_string();
            if let Some(prefix) = &row.prefix {
                text = format!("{prefix} {text}");
            }
            div()
                .h(px(p.para_line.as_f32() + p.para_gap.as_f32()))
                .w_full()
                .flex_shrink_0()
                .ml_3()
                .flex()
                .items_center()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_size(p.para_size)
                .text_color(p.fg)
                .child(text)
        }
        RowKind::Paragraph => div()
            .h(px(p.para_line.as_f32() + p.para_gap.as_f32()))
            .w_full()
            .flex_shrink_0()
            .flex()
            .items_center()
            .overflow_hidden()
            .whitespace_nowrap()
            .text_size(p.para_size)
            .text_color(p.fg)
            .child(row.text.clone()),
        RowKind::Table => unreachable!("表格已在上方提前返回"),
        RowKind::Image => unreachable!("图片行已在上方提前返回"),
    };
    el.into_any_element()
}

/// 渲染一张 GFM 表格为带边框网格（借鉴旧 render.rs 的表格视觉）：
/// 外边 1px 圆角边框 + 表头浅灰底/加粗 + 表头下分隔线 + 列左竖线 + 数据行上横线，
/// 列宽按 [`table_column_widths`] 估算（内容自适应，超宽走外层横向滚动）。
fn render_table(table: &TableData, p: &Rc<StyleParams>) -> gpui::Stateful<gpui::Div> {
    let border = p.divider;
    let widths = table_column_widths(table, p);
    let total: f32 = widths.iter().sum();
    let row_h = px(p.para_size.as_f32() * 1.45 + 12.0);

    let mut grid = div()
        .id("md-table")
        .debug_selector(|| "md-table".into())
        .flex()
        .flex_col()
        .w(px(total))
        .flex_shrink_0()
        .border_1()
        .border_color(border)
        .rounded(px(6.))
        .overflow_hidden();

    grid = grid.child(render_table_row(
        &table.header,
        &widths,
        &table.aligns,
        true,
        border,
        row_h,
        p,
    ));
    // 表头下分隔线
    grid = grid.child(div().h(px(1.)).w_full().flex_shrink_0().bg(border));
    for (ri, row) in table.rows.iter().enumerate() {
        let mut cells = row.clone();
        cells.resize(table.header.len(), SharedString::from(""));
        let mut row_el = render_table_row(
            &cells,
            &widths,
            &table.aligns,
            false,
            border,
            row_h,
            p,
        );
        // 数据行之间加一条上横线（旧 render.rs 用 border_t_1）
        if ri > 0 {
            row_el = row_el.border_t_1().border_color(border);
        }
        grid = grid.child(row_el);
    }
    grid
}

/// 表格的一行：定宽单元格 + 列左竖线；表头加粗/着色，按对齐方式排版。
fn render_table_row(
    cells: &[SharedString],
    widths: &[f32],
    aligns: &[TableAlign],
    is_header: bool,
    border: Hsla,
    row_h: Pixels,
    p: &StyleParams,
) -> gpui::Div {
    let mut row_el = div().flex().flex_row().w_full().flex_shrink_0();
    if is_header {
        row_el = row_el.bg(p.table_head_bg);
    }
    for (ci, cell) in cells.iter().enumerate() {
        let mut cell_el = div()
            .w(px(widths.get(ci).copied().unwrap_or(48.0)))
            .h(row_h)
            .flex_shrink_0()
            .px(px(10.))
            .flex()
            .items_center()
            .overflow_hidden()
            .whitespace_nowrap()
            .text_size(p.para_size)
            .text_color(p.fg);
        // 借鉴旧 render.rs：c>0 的列在左边加分割线（网格线齐平）
        if ci > 0 {
            cell_el = cell_el.border_l_1().border_color(border);
        }
        match aligns.get(ci) {
            Some(TableAlign::Center) => cell_el = cell_el.text_center(),
            Some(TableAlign::Right) => cell_el = cell_el.text_right(),
            _ => {}
        }
        if is_header {
            cell_el = cell_el.font_weight(FontWeight::BOLD);
        }
        row_el = row_el.child(cell_el.child(cell.clone()));
    }
    row_el
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext, Context, Entity, Render, TestAppContext, VisualTestContext, size};

    struct PreviewHost {
        md: Entity<MarkdownPreview>,
    }
    impl Render for PreviewHost {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            self.md.clone()
        }
    }

    /// 以 300x400 视口打开空预览，再把源码设为 500 个字符的超宽行。
    /// 顺序：先 `set_source` 再 `resize` —— resize 触发一帧完整重绘，让布局/滚动条就绪。
    fn setup_wide(cx: &mut TestAppContext) -> (Entity<PreviewHost>, &mut VisualTestContext) {
        cx.update(gpui_component::init);
        let (host, cx) = cx.add_window_view(|_window, cx| {
            let md = cx.new(|cx| MarkdownPreview::new(cx));
            PreviewHost { md }
        });
        let cx: &mut VisualTestContext = cx;
        let wide = "x".repeat(500);
        cx.update(|_, cx| {
            let md = host.read(cx).md.clone();
            md.update(cx, |md, cx| md.set_source(&wide, cx));
        });
        cx.simulate_resize(size(px(300.), px(400.)));
        settle(cx);
        (host, cx)
    }

    /// 让渲染收敛：`max_offset` / `bounds` 只在每帧布局时刷新，多送几帧确保
    /// 滚动容器以最终视口尺寸完成布局。
    fn settle(cx: &mut VisualTestContext) {
        for _ in 0..5 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
        }
        cx.run_until_parked();
    }

    /// 超宽内容应让横向滚动句柄产生正向 max_offset（可滚动范围正确）。
    #[gpui::test]
    fn wide_source_makes_harea_overflow(cx: &mut TestAppContext) {
        let (host, cx) = setup_wide(cx);

        let (max_x, view_w, content_w) = cx.update(|_, cx| {
            let md = host.read(cx).md.clone();
            let md = md.read(cx);
            (md.h_handle.max_offset().x, md.h_handle.bounds().size.width, md.content_w)
        });
        assert!(content_w > 300.0, "估算内容宽应超过视口，content_w={content_w:?}");
        assert!(
            max_x > px(0.),
            "横向应可滚动（max_offset.x={max_x:?}，视口宽={view_w:?}，内容宽={content_w:?}）"
        );

        // 设置横向偏移应生效（内容向左移，滚动条随之移动）
        cx.update(|_, cx| {
            let md = host.read(cx).md.clone();
            md.update(cx, |md, cx| {
                md.h_handle.set_offset(gpui::point(px(-120.), px(0.)));
                cx.notify();
            });
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let off_x = cx.update(|_, cx| host.read(cx).md.read(cx).h_handle.offset().x);
        assert!(off_x < px(-100.), "横向偏移应生效，实际={off_x:?}");
    }

    /// 内容超宽时，横向滚动条叠层应铺满预览区（得到确定的布局视口），
    /// 且 `content_w` 超过该视口 → gpui-component 的横条一定画出滑块
    /// （组件内判据：content_size = max_offset + viewport > container_size）。
    #[gpui::test]
    fn hscrollbar_overlay_covers_preview_when_overflow(cx: &mut TestAppContext) {
        let (host, cx) = setup_wide(cx);

        let preview = cx
            .debug_bounds("md-preview")
            .expect("预览根 md-preview 应存在");
        let overlay = cx
            .debug_bounds("md-hscrollbar-overlay")
            .expect("横向滚动条叠层 md-hscrollbar-overlay 应存在");
        let harea = cx
            .debug_bounds("md-harea")
            .expect("横向滚动容器 md-harea 应存在");

        assert!(
            (overlay.size.width.as_f32() - preview.size.width.as_f32()).abs() < 1.0,
            "横向滚动条叠层应铺满预览区宽度，overlay={overlay:?} preview={preview:?}"
        );
        assert!(
            overlay.size.width > px(0.) && overlay.size.height > px(0.),
            "叠层不应塌缩成 0，overlay={overlay:?}"
        );
        assert_eq!(harea.size.width, overlay.size.width);

        let (content_w, max_x) = cx.update(|_, cx| {
            let md = host.read(cx).md.clone();
            let md = md.read(cx);
            (md.content_w, md.h_handle.max_offset().x)
        });
        assert!(
            content_w > overlay.size.width.as_f32(),
            "内容宽应超过滚动条视口宽，content_w={content_w:.0} overlay_w={}",
            overlay.size.width.as_f32()
        );
        assert!(max_x > px(0.), "横向应存在可滚动范围，max_offset.x={max_x:?}");
    }

    /// 内容不超宽时横向不可滚动（不产出多余横向滚动范围）。
    #[gpui::test]
    fn no_hscroll_when_content_fits(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let (host, cx) = cx.add_window_view(|_window, cx| {
            let md = cx.new(|cx| MarkdownPreview::new(cx));
            PreviewHost { md }
        });
        let cx: &mut VisualTestContext = cx;
        // 几行短内容（估算宽远小于 300 视口）→ 不发生横向溢出
        let short = "短文本\n第二行\n结束";
        cx.update(|_, cx| {
            let md = host.read(cx).md.clone();
            md.update(cx, |md, cx| md.set_source(&short, cx));
        });
        cx.simulate_resize(size(px(300.), px(400.)));
        settle(cx);

        let (max_x, content_w) = cx.update(|_, cx| {
            let md = host.read(cx).md.clone();
            let md = md.read(cx);
            (md.h_handle.max_offset().x, md.content_w)
        });
        assert!(
            content_w < 300.0,
            "短内容估算宽应小于视口，content_w={content_w:.0}"
        );
        assert_eq!(max_x, px(0.), "短内容不应产生横向滚动范围，实际={max_x:?}");
    }

    /// GFM 表格应渲染成有实际宽高的网格，且其宽度计入横向滚动内容宽估算。
    #[gpui::test]
    fn renders_markdown_table_as_grid(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let (host, cx) = cx.add_window_view(|_window, cx| {
            let md = cx.new(|cx| MarkdownPreview::new(cx));
            PreviewHost { md }
        });
        let cx: &mut VisualTestContext = cx;
        let md_src = "| 型号 | 尺寸 |\n|:---|---:|\n| A | 10 |\n| B | 20 |\n";
        cx.update(|_, cx| {
            let md = host.read(cx).md.clone();
            md.update(cx, |md, cx| md.set_source(md_src, cx));
        });
        cx.simulate_resize(size(px(300.), px(400.)));
        settle(cx);

        let table = cx
            .debug_bounds("md-table")
            .expect("表格应渲染出 md-table 网格");
        assert!(table.size.width > px(0.), "表格应有宽度，实际={table:?}");
        assert!(table.size.height > px(0.), "表格应有高度（表头+数据行），实际={table:?}");

        let content_w = cx.update(|_, cx| host.read(cx).md.read(cx).content_w);
        assert!(
            content_w + 40.0 >= table.size.width.as_f32(),
            "表格宽度应计入横向滚动内容宽（content_w={content_w:.0}，表宽={:?}）",
            table.size.width
        );
    }

    /// 每个标题下的文本行（段落/列表/引述）都应获得相同的加高行距（不只是第一个标题下）。
    /// 复现：多个标题各带若干段，逐个用 `bounds_for_item` 检查文本 item 高度。
    #[gpui::test]
    fn paragraph_gap_applies_after_every_heading(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let (host, cx) = cx.add_window_view(|_window, cx| {
            let md = cx.new(|cx| MarkdownPreview::new(cx));
            PreviewHost { md }
        });
        let cx: &mut VisualTestContext = cx;
        // 两个标题各带文本：开头段落 → 标题一 → 段落 → 标题二 → 列表项 → 引述
        let md_src = "# 标题一\n开头段落\n\n## 标题二\n第二个段落\n- 列表项\n> 引述\n";
        cx.update(|_, cx| {
            let md = host.read(cx).md.clone();
            md.update(cx, |md, cx| md.set_source(md_src, cx));
        });
        cx.simulate_resize(size(px(400.), px(600.)));
        settle(cx);

        let p = cx.update(|_w, cx| StyleParams::from_theme_with_font(cx, None));
        let rows = parse_to_rows(md_src);
        let expected = p.para_line.as_f32() + p.para_gap.as_f32();

        // 找出所有文本行（段落/列表/引述）在 rows 中的下标
        let mut checked = 0usize;
        for (ix, r) in rows.iter().enumerate() {
            if !matches!(
                r.kind,
                RowKind::Paragraph | RowKind::Bullet | RowKind::Quote
            ) {
                continue;
            }
            let b = cx
                .update(|_, cx| host.read(cx).md.read(cx).list_state.bounds_for_item(ix))
                .expect("文本行 item 应有 bounds");
            assert!(
                (b.size.height.as_f32() - expected).abs() < 1.0,
                "第 {ix} 行（{:?}）高度应为 {expected:.1}，实际={:?}",
                r.kind,
                b.size.height
            );
            checked += 1;
        }
        assert_eq!(checked, 4, "应检查到 4 个文本行（2 段+1 列表+1 引述）");
    }

    /// EPUB 阅读：Ctrl+滚轮缩放应更新正文字号覆盖（基数 = 主题默认）。
    #[gpui::test]
    fn font_zoom_updates_override(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let (host, cx) = cx.add_window_view(|_window, cx| {
            let md = cx.new(|cx| MarkdownPreview::new(cx));
            PreviewHost { md }
        });
        let cx: &mut VisualTestContext = cx;
        let md_src = "# 标题\n正文段落内容，用于字号缩放验证。\n";
        cx.update(|_, cx| {
            let md = host.read(cx).md.clone();
            md.update(cx, |md, cx| md.set_source(md_src, cx));
        });
        cx.simulate_resize(size(px(400.), px(600.)));
        settle(cx);

        let default = cx.update(|_, cx| {
            StyleParams::from_theme_with_font(cx, None).para_size.as_f32()
        });
        // 初始无覆盖
        let none = cx.update(|_, cx| host.read(cx).md.read(cx).font_override);
        assert!(none.is_none(), "初始应无字号覆盖");

        // 模拟 Ctrl+滚轮向上（放大 1 档）
        cx.update(|window, cx| {
            let md = host.read(cx).md.clone();
            md.update(cx, |md, cx| md.bump_font_size(1.5, cx, default));
            window.refresh();
        });
        settle(cx);
        let after = cx.update(|_, cx| host.read(cx).md.read(cx).font_override);
        assert!(after.is_some(), "滚动后应产生字号覆盖");
        assert!(
            (after.unwrap() - (default + 1.5)).abs() < 0.001,
            "字号应增大 1.5，实际={after:?}"
        );
    }

    /// EPUB 目录：标题行应进入 toc；toggle_toc 可展开/收起；scroll_to_row 定位不 panic。
    #[gpui::test]
    fn toc_builds_and_scrolls(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let (host, cx) = cx.add_window_view(|_window, cx| {
            let md = cx.new(|cx| MarkdownPreview::new(cx));
            PreviewHost { md }
        });
        let cx: &mut VisualTestContext = cx;
        let md_src = "# 第一章\n开头\n\n## 第一节\n内容\n\n# 第二章\n结尾\n";
        cx.update(|_, cx| {
            let md = host.read(cx).md.clone();
            md.update(cx, |md, cx| md.set_source(md_src, cx));
        });
        cx.simulate_resize(size(px(400.), px(600.)));
        settle(cx);

        let toc = cx.update(|_, cx| host.read(cx).md.read(cx).toc.clone());
        assert_eq!(toc.len(), 3, "应有 3 个标题进入目录，实际={toc:?}");
        let (row0, lvl0, _) = &toc[0];
        assert_eq!(*row0, 0usize, "首个目录项应是第 0 行（# 第一章）");
        assert_eq!(*lvl0, 1u8, "第一章是 h1");
        assert!(toc[0].2.contains("第一章"));

        // 目录组件渲染冒烟：右上角触发按钮应存在
        let btn = cx.debug_bounds("md-toc-toggle-wrap");
        assert!(btn.is_some(), "目录引导按钮应渲染（md-toc-toggle-wrap）");

        // 跳转到一个中段标题（不应 panic），并验证目标行被滚动到视口顶部附近
        let target = toc[1].0;
        cx.update(|_, cx| {
            let md = host.read(cx).md.clone();
            md.update(cx, |md, _cx| md.scroll_to_row(target));
        });
        settle(cx);
        let target_bounds = cx.update(|_, cx| {
            host.read(cx).md.read(cx).list_state.bounds_for_item(target)
        });
        let tb = target_bounds.expect("跳转后目标行应有 bounds");
        assert!(
            tb.origin.y >= px(0.) && tb.origin.y < px(200.),
            "跳转到中段标题后目标行应滚动到视口顶部附近（origin.y={:?}）",
            tb.origin.y
        );
    }

    /// 设置了 `set_base_dir` 后，相对路径图片 src 应解析为绝对路径；
    /// URL / Windows 盘符绝对路径保持原样；未设置时保持原样。
    #[gpui::test]
    fn base_dir_resolves_relative_image_srcs(cx: &mut TestAppContext) {
        use std::path::PathBuf;
        cx.update(gpui_component::init);
        let (host, cx) = cx.add_window_view(|_window, cx| {
            let md = cx.new(|cx| MarkdownPreview::new(cx));
            PreviewHost { md }
        });
        let cx: &mut VisualTestContext = cx;

        let md_src = "![1](wuling.png)\n\n![1](C:\\Users\\c22500540\\Pictures\\dify.jpg)\n\n![1](https://example.com/a.png)\n";
        cx.update(|_, cx| {
            let md = host.read(cx).md.clone();
            md.update(cx, |md, cx| {
                md.set_base_dir(Some(PathBuf::from(r"C:\Users\c22500540\Pictures")));
                md.set_source(md_src, cx);
            });
        });
        cx.simulate_resize(size(px(400.), px(600.)));
        settle(cx);

        let rows = cx.update(|_, cx| {
            host.read(cx)
                .md
                .read(cx)
                .rows
                .iter()
                .map(|r| r.kind.clone() == RowKind::Image)
                .collect::<Vec<_>>()
        });
        drop(rows); // 仅确保读取不 panic；src 从 parsed 断言

        // 直接从 parse 通路验证：resolve 逻辑在 set_source 内完成了 base 拼接
        let srcs = cx.update(|_, cx| {
            host.read(cx)
                .md
                .read(cx)
                .rows
                .iter()
                .filter(|r| r.kind == RowKind::Image)
                .map(|r| r.src.clone().map(|s| s.to_string()))
                .collect::<Vec<_>>()
        });
        assert_eq!(srcs.len(), 3, "应解析出 3 张图片行，实际={srcs:?}");
        assert_eq!(
            srcs[0].as_deref(),
            Some(r"C:\Users\c22500540\Pictures\wuling.png"),
            "相对路径应基于 base_dir 解析为绝对路径，实际={:?}",
            srcs[0]
        );
        assert_eq!(
            srcs[1].as_deref(),
            Some(r"C:\Users\c22500540\Pictures\dify.jpg"),
            "Windows 盘符绝对路径应保持原样，实际={:?}",
            srcs[1]
        );
        assert_eq!(
            srcs[2].as_deref(),
            Some("https://example.com/a.png"),
            "URL 应保持原样，实际={:?}",
            srcs[2]
        );

        // 本地文件应读取固有尺寸（wuling.png=565x724，dify.jpg=1632x492）
        let dims = cx.update(|_, cx| {
            host.read(cx)
                .md
                .read(cx)
                .rows
                .iter()
                .filter(|r| r.kind == RowKind::Image)
                .map(|r| (r.img_w, r.img_h))
                .collect::<Vec<_>>()
        });
        assert_eq!(dims[0], (Some(565), Some(724)), "wuling.png 尺寸期望 565x724，实际={:?}", dims[0]);
        assert_eq!(dims[1], (Some(1632), Some(492)), "dify.jpg 尺寸期望 1632x492，实际={:?}", dims[1]);
        assert_eq!(dims[2], (None, None), "URL 无本地尺寸，实际={:?}", dims[2]);
    }

    /// 实际渲染的图片行高度应按固有比例随可用宽度变化（aspect-ratio 驱动），
    /// 缩小视口高度也随之缩小——不出现固定高度的大片留白。
    #[gpui::test]
    fn image_rows_height_follows_width(cx: &mut TestAppContext) {
        use std::path::PathBuf;
        cx.update(gpui_component::init);
        let (host, cx) = cx.add_window_view(|_window, cx| {
            let md = cx.new(|cx| MarkdownPreview::new(cx));
            PreviewHost { md }
        });
        let cx: &mut VisualTestContext = cx;

        let wabsp = r"C:\Users\c22500540\Pictures\wuling.png";
        let md_src = format!("# 标题\n\n![w]({wabsp})\n\n![s](C:\\Users\\c22500540\\Pictures\\dify.jpg)\n");
        cx.update(|_, cx| {
            let md = host.read(cx).md.clone();
            md.update(cx, |md, cx| {
                md.set_base_dir(Some(PathBuf::from(r"C:\Users\c22500540\Pictures")));
                md.set_source(&md_src, cx);
            });
        });

        let measure = |cx: &mut VisualTestContext| -> (f32, f32) {
            let (wh, dh) = cx.update(|_, cx| {
                let md = host.read(cx).md.read(cx);
                let wi = md
                    .rows
                    .iter()
                    .position(|r| r.kind == RowKind::Image && r.text == "w")
                    .expect("应有 wuling 图片行");
                let di = md
                    .rows
                    .iter()
                    .position(|r| r.kind == RowKind::Image && r.text == "s")
                    .expect("应有 dify 图片行");
                let wb = md.list_state.bounds_for_item(wi).expect("wuling 行应有 bounds");
                let db = md.list_state.bounds_for_item(di).expect("dify 行应有 bounds");
                (wb.size.height.as_f32(), db.size.height.as_f32())
            });
            (wh, dh)
        };

        // 宽视口 500px：wuling 宽=min(500-24, 600)=476 → 高=476*724/565≈610，限高后按比例
        cx.simulate_resize(size(px(500.), px(800.)));
        settle(cx);
        let (w_wide, d_wide) = measure(cx);

        // 窄视口 260px：wuling 宽=260-24=236 → 高=236*724/565≈302（成比例缩小）
        cx.simulate_resize(size(px(260.), px(800.)));
        settle(cx);
        let (w_small, d_small) = measure(cx);

        assert!(
            w_small < w_wide - 50.0,
            "缩小视口后图片行高度应明显变小：wide={w_wide:.0} small={w_small:.0}"
        );
        assert!(
            d_small < d_wide - 50.0,
            "dify 行高同样应随宽度缩小：wide={d_wide:.0} small={d_small:.0}"
        );

        // 图片盒水平居中：md-img-box 左缘 ≈ (视口宽 - 盒宽)/2
        cx.simulate_resize(size(px(500.), px(800.)));
        settle(cx);
        let bo = cx
            .debug_bounds("md-img-box")
            .expect("图片盒 md-img-box 应存在");
        let prev = cx
            .debug_bounds("md-preview")
            .expect("预览根 md-preview 应存在");
        let view_w = prev.size.width.as_f32();
        let expect_x = (view_w - bo.size.width.as_f32()) / 2.0;
        assert!(
            (bo.origin.x.as_f32() - expect_x).abs() < 2.0,
            "图片盒应水平居中：盒宽={:.0}，期望左缘={expect_x:.0}，实际={:?}",
            bo.size.width.as_f32(),
            bo.origin
        );

        // 宽视口（预览 > 600）：盒宽应被 IMAGE_MAX_W 封顶并仍然居中
        cx.simulate_resize(size(px(900.), px(800.)));
        settle(cx);
        let bo2 = cx
            .debug_bounds("md-img-box")
            .expect("图片盒 md-img-box 应存在(宽视口)");
        let prev2 = cx
            .debug_bounds("md-preview")
            .expect("预览根 md-preview 应存在(宽视口)");
        let view_w2 = prev2.size.width.as_f32();
        assert!(
            (bo2.size.width.as_f32() - IMAGE_MAX_W).abs() < 2.0,
            "宽视口下图片盒宽应封顶 {IMAGE_MAX_W}，实际={:.0}",
            bo2.size.width.as_f32()
        );
        let expect_x2 = (view_w2 - IMAGE_MAX_W) / 2.0;
        assert!(
            (bo2.origin.x.as_f32() - expect_x2).abs() < 2.0,
            "宽视口下图片盒仍应水平居中：期望左缘={expect_x2:.0}，实际={:?}",
            bo2.origin
        );
    }
}
