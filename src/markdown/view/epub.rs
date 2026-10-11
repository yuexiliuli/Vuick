//! EPUB 阅读增强（M5-B5 P3-1c-1 拆分自原 `view/mod.rs`）：注记悬浮 / 引文导航 /
//! 页码化等**只读阅读体验**，作为 `MarkdownPreview` 宿主的实现块。
//! 通过 `pub(super)`（可见于 `view` 子树）访问宿主私有字段与方法（`total_h` 等）；
//! 宿主/渲染/滚动/源装配在 `super::gateway`。
//!
//! - `NoteHit` / `NoteCtx`：注记命中矩形共享表（`row::ShardStrip` paint 写、宿主读）；
//! - `set_epub`：EPUB 排版开关 + 脚注表注入；
//! - `viewport_page_bounds`：轻量视口分页读数（状态栏页码）；
//! - 引文导航：`ensure_citations` / `navigate_marker` / `set_marker_highlight` /
//!   `note_hit_at` / `citation_index_from_preview_rows`；
//! - 注记悬浮卡片：`with_hover_note_card` / `note_card` / `note_card_point`。

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{
    anchored, deferred, div, point, px, App, Bounds, Context, Div, Hsla, InteractiveElement,
    IntoElement, Pixels, Point, ParentElement, SharedString, Stateful, Styled,
};
use crate::config::timing::HIGHLIGHT_MS;
use crate::markdown::parse::{scan_citations, CitationIndex};

use super::gateway::{MarkdownPreview, PreviewSrc};
use super::row::PreviewRow;

/// EPUB 脚注悬浮命中：一行内某个 `[n]` 标记在**上一帧**的画面矩形 + 对应注释正文。
/// 由 [`row::NoteMarkers`] 在 paint 时写入共享表（窗口坐标）；鼠标移动时视图据此判 hover，
/// 点击时据此做「文内 ↔ 末尾定义」双向跳转。
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct NoteHit {
    /// 全局块下标（渲染行标识；同一行每次重绘覆盖旧条目）。
    pub(crate) row_ix: usize,
    /// 注记号。
    pub(crate) n: usize,
    /// 是否末尾定义标记（`[!n]`）；false = 文内引用 `[^n]`。
    pub(crate) is_def: bool,
    /// 行内**子条带**序号：legacy `TextTrack` 恒 0（整行一个布局）；unified `ShardStrip`
    /// 按 shard 分片记录（同行的多片各自独立，互不清除；P3-1b-2 删 TextTrack 后保留）。
    pub(crate) hit_sub: usize,
    /// 标记矩形（窗口坐标，上一帧）。
    pub(crate) bounds: Bounds<Pixels>,
}

/// EPUB 注记渲染的共享上下文：注释文本表 + 上帧命中矩形表。
#[derive(Clone, Default)]
pub(crate) struct NoteCtx {
    /// 注记号 → 注释正文（`set_epub` 注入；空 = 非 EPUB）。
    pub(crate) notes: std::collections::BTreeMap<usize, SharedString>,
    /// 命中矩形共享表（row.rs 写、view 读）。
    pub(crate) hits: Rc<RefCell<Vec<NoteHit>>>,
}

impl MarkdownPreview {
    /// 开启/关闭 EPUB 阅读排版并注入脚注表（`load_epub` 调用）。
    /// 开启后：标题居中、正文首行缩进 2 字符、`[n]` 注记悬停显示卡片。
    pub fn set_epub(&mut self, on: bool, notes: std::collections::BTreeMap<usize, String>, _cx: &mut App) {
        self.epub_mode = on;
        // 换新注记上下文（命中表也随之清空重来）。
        self.note_ctx = Rc::new(NoteCtx {
            notes: notes
                .into_iter()
                .map(|(k, v)| (k, SharedString::from(v)))
                .collect(),
            hits: Rc::new(RefCell::new(Vec::new())),
        });
        self.hover_hit = None;
        self.list_state.remeasure();
        self.est_hint_width = -1.0;
    }

    /// EPUB 轻量视口分页读数：`(当前页, 总页数)`（1 基）——按「内容总高 / 视口高」
    /// 切页，当前页以视口**底部到达**的页计（clamp 到 `[1, 总页数]`）。口径与纵向
    /// 滚动条量程同源（窗口模式 = `desired_scroll`＋抽样块高模型总高；小文档 = 实测
    /// ListState px 滚动＋实测总高，见 `total_h`），故**拖到底必然「第 N/N 页」**、
    /// 字号调整/换宽后页码自动跟随。
    /// 非 EPUB / 无源返回 `None`（不显示页码）。纯展示读数，无翻页交互。
    pub(crate) fn viewport_page_bounds(&self) -> Option<(usize, usize)> {
        // 有源判定：小文档 `rows` 物化全量；窗口模式懒物化 `rows` 恒空，以 `index`
        // 为准（否则大 EPUB 会误判为无源而不显示页码）。
        if !self.epub_mode || (self.index.is_none() && self.rows.is_empty()) {
            return None;
        }
        let view_h = self
            .list_state
            .viewport_bounds()
            .size
            .height
            .as_f32()
            .max(1.);
        // **就绪门控**：视口尚未布局 / 小文档平均行高未估（render 节流后才估）/
        // 窗口高度模型未建（render 后才有）时返回 `None`——宁可不显示，也不让首帧
        // 乱值（视口 0 / est_hint 0 时页数会被算成假值）造成「1/45 跳 1/55」跳动。
        if view_h < 16.0 {
            return None;
        }
        if self.index.is_none() && self.est_hint <= 0.0 {
            return None;
        }
        if self.index.is_some() && self.height_pref.is_empty() {
            return None;
        }
        let content_h = self.total_h().max(view_h);
        let total = ((content_h / view_h).ceil()).max(1.) as usize;
        // 滚动 top（px，与量程同源）：
        // - 窗口模式：像素模型用户目标 `desired_scroll`；
        // - 小文档：绝对逻辑行 × 统一行高 + 行内偏移（`list_state.scroll_px_offset_*`
        //   在 summary 未就绪时常恒 0，不能用于页码）。
        let top_px = if self.index.is_some() {
            self.desired_scroll.get().max(0.)
        } else {
            // 小文档：与量程同源的**实测**滚动 px（`scroll_px_offset_for_scrollbar`，
            // 已由 `update_height_hint` 铺 uniform 高度 → summary 完备、取值为真实 px）。
            // 用估算（行×平均高）会停在估算末尾——真机实测>估算时拖到底页码永远
            // < 总页数；实测 px 保证「拖到底 = 第 N/N 页」。
            -self.list_state.scroll_px_offset_for_scrollbar().y.as_f32()
        };
        // 当前页 = 视口**底部到达**的页（底部到达页语义）：某页顶部滚进视口底边才算
        // 「翻到那页」。这与「拖到底 = 第 N/N 页」自洽——视口底 = 内容底时当前页必然
        // = 总页数；若用「顶部到达页」语义，最后不满一屏时顶部永远停在 `floor(C/v)`
        // 页、到不了总页数（真机实测「44/47」的来源之一），也无法滚过末尾补上那页。
        // `-0.5` 为亚像素护栏：恰好落在页边界时钳回前一页，不把末页顶进下一页。
        let bottom = top_px + view_h;
        let page = ((bottom - 0.5) / view_h).floor().max(0.) as usize;
        Some((page.min(total - 1) + 1, total))
    }

    /// 命中引用标记（上一帧注记矩形共享表），返回该标记的 [`NoteHit`]。
    pub(super) fn note_hit_at(&self, pos: Point<Pixels>) -> Option<NoteHit> {
        self.note_ctx
            .hits
            .borrow()
            .iter()
            .find(|h| h.bounds.contains(&pos))
            .copied()
    }

    /// 确保引用跳转索引就绪（懒建：只在源码代变化后重建；小文档走行集，大文档全源逐块扫描）。
    pub(super) fn ensure_citations(&mut self) {
        if self.cit_built_gen == self.cit_gen {
            return;
        }
        self.cit = match &self.index {
            // 窗口模式：整篇源码逐块扫描（懒，仅交互到引用时；不占用打字同步热路径）。
            Some(_) => {
                let src = match &self.src_buf {
                    Some(PreviewSrc::Str(s)) => s.to_string(),
                    Some(PreviewSrc::Rope(r)) => r.to_string(),
                    None => String::new(),
                };
                scan_citations(&src)
            }
            // 小文档：从已解析渲染行（`PreviewRow` 含引注标记）直接取（几乎零成本）。
            None => citation_index_from_preview_rows(&self.rows),
        };
        self.cit_built_gen = self.cit_gen;
    }

    /// 点击引用标记 `hit` 后跳转对端：
    /// - 文内引用 `[^n]` → 末尾定义 `[!n]`（引用详情）；
    /// - 末尾定义 `[!n]` → 文内引用 `[^n]`。
    ///
    /// 跳到对端所在渲染行并给该行的 `[n]` 标记加亮；无对端（孤立标记）仅清点选、不跳转。
    pub(super) fn navigate_marker(&mut self, hit: NoteHit, cx: &mut Context<Self>) {
        self.ensure_citations();
        let target = if hit.is_def {
            self.cit.refs.get(&hit.n).cloned()
        } else {
            self.cit.defs.get(&hit.n).cloned()
        };
        let Some((row, _range)) = target else {
            self.select_anchor = None;
            self.selection = None;
            cx.notify();
            return;
        };
        // scroll_to_row 内部已清悬浮/命中（滚动会改变注记矩形）；随后加亮对端标记。
        self.scroll_to_row(row);
        self.set_marker_highlight(row, hit.n, !hit.is_def, cx);
        cx.notify();
    }

    /// 跳转后给目标行的 `[n]` 标记加亮（`is_def` 指明是定义还是文内引用），
    /// [`HIGHLIGHT_MS`] 后自动消失（代序号防旧定时器误清）。
    fn set_marker_highlight(&mut self, row: usize, n: usize, is_def: bool, cx: &mut Context<Self>) {
        self.marker_hl = Some((row, n, is_def));
        self.marker_hl_seq = self.marker_hl_seq.wrapping_add(1);
        let seq = self.marker_hl_seq;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(HIGHLIGHT_MS))
                .await;
            let _ = this.update(cx, |view, cx| {
                if view.marker_hl_seq == seq {
                    view.marker_hl = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// 悬浮卡片的显示位置（窗口坐标，位于注记矩形右下方，带边距）。
    fn note_card_point(&self, hit: &NoteHit) -> Point<Pixels> {
        point(
            px(hit.bounds.right().as_f32() + 8.0),
            px(hit.bounds.top().as_f32() + 4.0),
        )
    }
}

/// 从已解析渲染行（[`PreviewRow`]）构建引用跳转索引（小文档全量路径；多位置取第一个）。
/// 语义与 `parse::citation_index_from_rows` 完全一致，只是行源换成 `PreviewRow`
/// （`notes`/`note_defs`/`text` 由预览行携带；M5-B5 P3-1a 行型切统一 `DocRow` 后
/// 不再喂 `&[Row]`）。
fn citation_index_from_preview_rows(rows: &[PreviewRow]) -> CitationIndex {
    let mut idx = CitationIndex::default();
    for (ix, r) in rows.iter().enumerate() {
        for (range, n) in &r.notes {
            idx.refs.entry(*n).or_insert((ix, range.clone()));
        }
        for (range, n) in &r.note_defs {
            idx.defs.entry(*n).or_insert((ix, range.clone()));
            // 详情正文：定义标记 `[n]` 之后的文本（去首尾空白）。
            idx.details.entry(*n).or_insert_with(|| {
                let start = range.end.min(r.docrow.text.len());
                r.docrow.text[start..].trim().into()
            });
        }
    }
    idx
}

impl MarkdownPreview {
    /// 悬浮详情卡片：鼠标停在 `[n]` 标记（文内引用 / 末尾定义皆可）上时，浮出该引用
    /// 的详情正文。正文优先用 EPUB 注记表（`set_epub` 注入）；非 EPUB 的 markdown 用
    /// `cit.details`（定义 `[!n]` 行剥掉 `[n]` 标记后的文本）。
    pub(super) fn with_hover_note_card(&self, core: Stateful<Div>, cx: &mut Context<Self>) -> Stateful<Div> {
        let hit = self.hover_hit;
        match hit {
            Some(hit) => {
                let text = self
                    .note_ctx
                    .notes
                    .get(&hit.n)
                    .cloned()
                    .or_else(|| self.cit.details.get(&hit.n).cloned());
                match text {
                    Some(text) => {
                        let at = self.note_card_point(&hit);
                        let card = note_card(text, at, cx);
                        core.child(
                            deferred(
                                anchored()
                                    .snap_to_window_with_margin(px(8.))
                                    .position(at)
                                    .child(card),
                            )
                            .with_priority(1),
                        )
                    }
                    None => core,
                }
            }
            None => core,
        }
    }
}

/// EPUB 脚注悬浮卡片：主题配色的 popover 样小卡片，展示一条注释正文。
fn note_card(text: SharedString, _at: Point<Pixels>, cx: &App) -> gpui::AnyElement {
    let theme = crate::ui::theme::Theme::global(cx);
    let colors = theme.colors;
    let card_bg: Hsla = colors.popover_bg.into();
    let border: Hsla = colors.border.into();
    let fg: Hsla = colors.fg.into();
    div()
        .debug_selector(|| "md-note-card".into())
        .bg(card_bg)
        .border_1()
        .border_color(border)
        .rounded_md()
        .p_3()
        .max_w(px(460.))
        .child(
            div()
                .whitespace_normal()
                .text_size(px(12.5))
                .text_color(fg)
                .child(text),
        )
        .into_any_element()
}
