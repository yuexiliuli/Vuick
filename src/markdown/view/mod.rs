//! Markdown 渲染器的视图层：行级虚拟化 + gpui_component 滚动条（双向）。
//!
//! 核心思路（参考 vellum / zorite 的行级虚拟化）：
//! - 文档被拆成**一行一条**的渲染行（见 [`crate::markdown::parser`]）；
//! - 用 gpui 的 `List` 做虚拟化：只构建 / 绘制可见行，滚动时按需重建；
//! - **竖滚动条量程**：`measure_all()` 会首帧排版整篇（十万行一次 1GB+），所以改用
//!   `with_uniform_item_height` 以「宽度感知的平均行高」铺设量程（见 `update_height_hint`）
//!   ——行数 × 平均高 ≈ 真实总高，滚动条从一开始就能拖动到全文，内存只随可见窗口走；
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
//! - `tests`：视图层测试

mod dimensions;
mod row;
mod style;
mod table;

#[cfg(test)]
mod tests;

use std::rc::Rc;

use gpui::{
    div, list, px, rgba, App, Context, Hsla, InteractiveElement, IntoElement, ListAlignment,
    ListState, ParentElement, Render, ScrollDelta, ScrollHandle, ScrollWheelEvent,
    StatefulInteractiveElement, Styled, Window,
};
use gpui_component::scroll::{Scrollbar, ScrollbarMode, ScrollableElement as _};

use crate::markdown::parser::{parse_to_rows, Row, RowKind};

use dimensions::{est_row_width, image_dimensions, image_dimensions_bytes, WIDTH_BUFFER_PX, IMAGE_ROW_DEFAULT_H};
use row::render_row;
use style::StyleParams;

/// 行级虚拟化在视口上下各多构建的像素量（保证快速滚动时内容连续）。
const ROW_OVERDRAW_PX: f32 = 600.0;

/// 搜索跳转高亮停留时长（ms）：侧边栏搜索跳到命中行后高亮一段时间，随后自动消失。
const HIGHLIGHT_MS: u64 = 2200;

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
    /// 搜索跳转高亮：命中的源码行（0 基）与小写查询词；None = 无高亮。
    highlight_search: Option<(usize, String)>,
    /// 高亮代序列号：每次发起/清除高亮 +1，定时器只清除同代高亮，避免旧定时器误清新高亮。
    highlight_seq: u64,
    /// 统一行高提示（px）：宽度感知的平均行高估算，用于 `ListState::with_uniform_item_height`，
    /// 让竖滚动条量程从头一开始就接近真实总高（不用一次性排版整篇）。
    est_hint: f32,
    /// 生成 `est_hint` 时的视口宽度（px）；<0 表示尚未按宽度估算（需要重算）。
    est_hint_width: f32,
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
            highlight_search: None,
            highlight_seq: 0,
            est_hint: 0.0,
            est_hint_width: -1.0,
        }
    }

    /// 搜索跳转后高亮「源码第 `line` 行」（0 基）内**第一个命中 `query` 的文本**，
    /// 只给搜索词本身着色（不吃整行），[`HIGHLIGHT_MS`] 后自动消失。
    pub fn highlight_search(&mut self, line: usize, query: &str, cx: &mut Context<Self>) {
        self.highlight_search = Some((line, query.to_lowercase()));
        self.highlight_seq = self.highlight_seq.wrapping_add(1);
        let seq = self.highlight_seq;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(HIGHLIGHT_MS))
                .await;
            let _ = this.update(cx, |view, cx| {
                if view.highlight_seq == seq {
                    view.highlight_search = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// 清除搜索跳转高亮。
    pub fn clear_highlight(&mut self) {
        self.highlight_search = None;
        self.highlight_seq = self.highlight_seq.wrapping_add(1);
    }

    /// 当前搜索跳转高亮映射到的 **(渲染行, 行内字节区间)**；无高亮或命中文本不在
    /// 渲染文本中时返回 `None`。测试用，确认高亮只覆盖搜索词而非整行。
    #[cfg(test)]
    pub fn search_highlight_range(&self) -> Option<(usize, std::ops::Range<usize>)> {
        let (line, query) = self.highlight_search.as_ref()?;
        let row_ix = self.row_for_line(*line)?;
        let text = self.rows.get(row_ix)?.text.as_ref();
        let range = find_text_range(text, query)?;
        Some((row_ix, range))
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
        self.invalidate_height_hint();
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
        self.invalidate_height_hint();
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
        // 内容变了，旧的跳转高亮随之失效
        self.clear_highlight();
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
        // 行高估算依赖视口宽度，换源后强制按新宽度重算（render 里执行）。
        self.invalidate_height_hint();
        if self.rows.len() != self.list_state.item_count() {
            // 行数变化 → 重建 ListState。注意**不能**用 `measure_all()`：它会首帧
            // 就把整篇所有行全部渲染并排版（十万行文档一次吃进 1GB+ 的字体测量缓存），
            // 而且每次开关预览重解析都会再生成一份、内存只涨不降。
            // 这里**故意不设**静态高度提示：若先设一个占位提示，首帧 summary 会被它
            // 垫到「看似有效」而跳过统一高度提示的重填，随后列表又在首次布局时把
            // size_hint 清成 None（见 `update_height_hint`），滚动条量程就再没人管了。
            // 直接全部留空，交给 `update_height_hint` 在首次渲染按视口宽度填平均高：
            // 滚动条量程从一开始就接近真实总高，但只对可见行测量/排版（行级虚拟化）。
            self.list_state =
                ListState::new(self.rows.len(), ListAlignment::Top, px(ROW_OVERDRAW_PX));
        } else {
            // 行数不变但内容变了 → 重新测量高度（保留滚动位置；未测行继续用统一提示）
            self.list_state.remeasure();
        }
    }

    /// 释放预览重资源（解析出的行数据 + 列表测量缓存），关闭预览时调用。
    /// 否则隐藏的预览仍占着整篇文档的解析结果；反复开关预览会积累内存。
    /// 下次开启预览时 [`Self::set_source`] 会重新解析，行为不变。
    pub fn release(&mut self, cx: &mut App) {
        self.rows = Rc::new(Vec::new());
        self.list_state = ListState::new(0, ListAlignment::Top, px(ROW_OVERDRAW_PX));
        self.content_w = 0.0;
        self.toc.clear();
        self.code_scroll.clear();
        self.table_scroll.clear();
        self.highlight_search = None;
        self.highlight_seq = self.highlight_seq.wrapping_add(1);
        self.est_hint = 0.0;
        self.est_hint_width = -1.0;
        self.remeasure_dimensions(cx);
    }

    /// 标记「平均行高的宽度感知估算」已失效：换源 / 换行 / 字号变化后调用，
    /// 下一次 render（视口宽度已知）会重算并刷新统一高度提示。
    fn invalidate_height_hint(&mut self) {
        self.est_hint_width = -1.0;
    }

    /// 按当前视口宽度估算「平均渲染行高」作为列表的统一高度提示。
    /// 这样竖滚动条量程 = 行数 × 平均行高 ≈ 真实总高，从头一开始就可用滚动条
    /// 跳到任意位置（含文末），而无需像 `measure_all()` 那样一次性排版整篇。
    /// 估算规则尽量贴合 `render_row` 的几何：非文本行按公式精确，文本行按字符宽
    /// 折算行数（CJK ≈ 字号，其余 ≈ 0.55 × 字号），微差只会让滚动条映射略微
    /// 非线性，随行真正渲染后逐步收敛。
    fn estimated_avg_row_height(&self, p: &StyleParams, width: f32) -> f32 {
        if self.rows.is_empty() {
            return p.body_row_hint().as_f32();
        }
        let avail = (width - 24.0).max(40.0); // px_3 左右内缩
        let wrap = self.wrap;
        let mut total = 0.0f32;
        for r in self.rows.iter() {
            let h = match r.kind {
                RowKind::Blank => p.blank_h.as_f32(),
                RowKind::Rule => p.rule_h.as_f32(),
                RowKind::Code => {
                    (r.code_lines.len().max(1) as f32 + 2.0) * p.code_line.as_f32()
                }
                RowKind::Table => {
                    let row_h = p.para_size.as_f32() * 1.45 + 12.0;
                    (1.0 + r.table.as_ref().map_or(0, |t| t.rows.len()) as f32) * row_h
                        + 2.0
                        + 8.0
                }
                RowKind::Image => IMAGE_ROW_DEFAULT_H,
                RowKind::Heading(n) => {
                    let (fs, lh) = p.heading_metrics(n);
                    if wrap {
                        est_wrap_lines(&r.text, avail, fs.as_f32()).max(1) as f32
                            * lh.as_f32()
                            + 8.0
                    } else {
                        lh.as_f32()
                    }
                }
                RowKind::Paragraph | RowKind::Quote | RowKind::Bullet => {
                    if wrap {
                        let lines =
                            est_wrap_lines(&r.text, avail, p.para_size.as_f32()).max(1) as f32;
                        (lines * p.para_line.as_f32()).max(p.para_line.as_f32())
                            + p.para_gap.as_f32()
                            + 8.0
                    } else {
                        p.para_line.as_f32() + p.para_gap.as_f32()
                    }
                }
            };
            total += h;
        }
        total / self.rows.len() as f32
    }

    /// 视口宽度已知（render 里）后刷新统一高度提示。
    ///
    /// 背景：gpui 的 `list` 元素在**首次布局/视口宽度变化**时的 prepaint 里会把所有
    /// 未测量项的 size_hint 清为 `None`（`list.rs`，比较 `last_layout_bounds` 宽度），
    /// 之后滚动条总高就只算可见行、无法一开始拖到全文。这里每次渲染用 O(1) 的
    /// summary 总高对账：发现总高远小于「行数×平均高」（提示已被清掉）就重新套用；
    /// 套用**不重建** ListState（保 `last_layout_bounds`，下一帧 prepaint 不再清），
    /// 并返回 `true` 让调用方 `cx.notify()` 多画一帧闭环。收敛后不再触发，不无限重画。
    fn update_height_hint(&mut self, cx: &mut App, width: f32) -> bool {
        if self.rows.is_empty() {
            return false;
        }
        let width_changed = (self.est_hint_width - width).abs() >= 2.0;
        if width_changed {
            let p = StyleParams::from_theme_with_font(cx, self.font_override);
            self.est_hint = self.estimated_avg_row_height(&p, width);
            self.est_hint_width = width;
        }
        let hint = self.est_hint;
        if hint <= 0.0 {
            return false;
        }
        let n = self.rows.len();
        if self.list_state.item_count() != n {
            return false;
        }
        // 列表当前总高 = 可滚动上限 + 视口高（尚未布局时二者皆 0）
        let summary_h = self.list_state.max_offset_for_scrollbar().y.as_f32()
            + self.list_state.viewport_bounds().size.height.as_f32();
        let expected = n as f32 * hint;
        if summary_h >= expected * 0.5 {
            // 提示仍有效（或已被测量行撑到足够高）→ 无需处理
            return false;
        }
        // 提示被清掉 → 不重建、原地重新填满（保留 last_layout_bounds，避免下次 prepaint 再清）
        self.list_state = self.list_state.clone().with_uniform_item_height(px(hint));
        true
    }
}

/// 估算法：按字符宽（CJK/全角 ≈ font_size，其余 ≈ 0.55 × font_size）累计文本宽度，
/// 折出在可用宽度 `avail` 下的行数。用于「平均行高」估算，非精确排版。
fn est_wrap_lines(text: &str, avail: f32, font_size: f32) -> usize {
    if text.is_empty() {
        return 1;
    }
    let mut w = 0.0f32;
    for ch in text.chars() {
        let c = ch as u32;
        let cw = if (0x2E80..=0x9FFF).contains(&c) || (0xFF00..=0xFFEF).contains(&c) {
            font_size
        } else {
            font_size * 0.55
        };
        w += cw;
    }
    (w / avail).ceil() as usize
}

impl Render for MarkdownPreview {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = self.rows.clone();
        let p = Rc::new(StyleParams::from_theme_with_font(cx, self.font_override));
        let bg = self.bg.unwrap_or(p.bg);
        // 视口宽度已知 → 刷新统一行高提示，让竖滚动条量程从一开始就接近真实总高。
        // （首次布局/宽度变化后 gpui 会清空 size_hint，这里检测到就重建重填，并
        // 请求多画一帧让量程真正生效——下一帧量程即满、不会再通知，不会无限重画。）
        let pane_w = {
            let hw = self.h_handle.bounds().size.width.as_f32();
            if hw > 0.0 {
                hw
            } else {
                window.bounds().size.width.as_f32() * 0.5
            }
        };
        if self.update_height_hint(cx, pane_w) {
            cx.notify();
        }
        let list_state = self.list_state.clone();
        let content_w = self.content_w;
        let wrap = self.wrap;
        let code_scroll = self.code_scroll.clone();
        let table_scroll = self.table_scroll.clone();

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

        // 搜索跳转高亮：只给命中**文本**着色（不吃整行）。把命中的源码行映射到渲染行，
        // 在该行渲染文本里定位查询词的字节区间，连同主题感知背景色交给 render_row。
        let search_highlight = {
            let mut hit: Option<(usize, std::ops::Range<usize>)> = None;
            if let Some((line, query)) = self.highlight_search.as_ref() {
                if let Some(row_ix) = self.row_for_line(*line) {
                    if let Some(range) = find_text_range(&self.rows[row_ix].text, query) {
                        hit = Some((row_ix, range));
                    }
                }
            }
            let color = if hit.is_some() {
                Some(if gpui_component::Theme::global(cx).is_dark() {
                    rgba(0xf59e0b47).into()
                } else {
                    rgba(0xfef08a8c).into()
                })
            } else {
                None
            };
            hit.map(|(row_ix, range)| (row_ix, range, color.expect("命中则有背景色")))
        };

        let content = {
            let p = p.clone();
            let code_scroll = code_scroll;
            let table_scroll = table_scroll;
            let search_highlight = search_highlight;
            list(list_state.clone(), move |ix, _window, cx| {
                let ch = code_scroll.get(&ix).cloned();
                let th = table_scroll.get(&ix).cloned();
                let search = search_highlight
                    .as_ref()
                    .filter(|(row_ix, _, _)| *row_ix == ix)
                    .map(|(_, range, color)| (range.clone(), *color));
                render_row(&rows[ix], &p, wrap, ch, th, search, cx)
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

        root.child(core)
    }
}

/// 在 `text` 内查找第一个忽略大小写命中小写 `needle` 的字节区间；未命中返回 `None`。
/// 用于搜索跳转为命中**文本**着色（非整行）。
fn find_text_range(text: &str, needle: &str) -> Option<std::ops::Range<usize>> {
    let needle_chars = needle.chars().count();
    if needle_chars == 0 {
        return None;
    }
    let mut i = 0usize;
    while i < text.len() {
        let candidate: String = text[i..].chars().take(needle_chars).collect();
        if !candidate.is_empty() && candidate.to_lowercase() == needle {
            return Some(i..(i + candidate.len()));
        }
        let ch = text[i..].chars().next()?;
        i += ch.len_utf8();
    }
    None
}
