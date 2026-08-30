//! 轻量 Markdown 渲染（GPUI 版）。
//!
//! 移植自 Tauri 成品前端的 `src/markdown.ts`，把 Markdown 文本解析为 GPUI 的
//! `div + StyledText` 元素树，支持：段落、标题、行内 code/加粗/斜体/删除线/链接、
//! 围栏代码块、列表、表格、引用、分割线。
//!
//! 渲染结果是一个 `Div`，可直接作为 `.child(...)` 使用。

use super::model::{
    Block, MarkdownTheme, Span, TableMode, CELL_BORDER, CELL_HEADER_BOLD_BUFFER, CELL_H_PAD,
};
use super::parser::parse_markdown;
use super::selection::{render_selectable_spans, SelectCtx};
use super::text::{break_long_runs, spans_to_styled, SpannedText};
use crate::scrollbar::{horizontal_scrollbar, TableScrollState};
use gpui::{
    div, font, px, rgb, AnyElement, Div, ElementId, FontWeight, InteractiveElement, IntoElement,
    ParentElement, Pixels, ScrollWheelEvent, SharedString, StatefulInteractiveElement, Styled,
    StyledText, TextRun, Window,
};
use std::rc::Rc;

// ==================== 渲染为 GPUI 元素 ====================

fn render_spans_line(spans: &[Span], theme: &MarkdownTheme) -> StyledText {
    render_spans_line_weighted(spans, theme, false)
}

/// 渲染一行行内片段，可整体强制加粗（用于表头单元格）。
fn render_spans_line_weighted(
    spans: &[Span],
    theme: &MarkdownTheme,
    force_bold: bool,
) -> StyledText {
    spans_to_styled(spans, theme, force_bold).into_element()
}

#[allow(clippy::too_many_arguments)]
fn render_block(
    block: &Block,
    theme: &MarkdownTheme,
    mode: TableMode,
    scroll: Option<&TableScrollState>,
    window: Option<&mut Window>,
    sel: Option<&Rc<SelectCtx>>,
    stream: &mut usize,
    table_idx: &mut usize,
) -> AnyElement {
    match block {
        Block::Paragraph(lines) => {
            let mut d = div().min_w(px(0.));
            for line in lines {
                if let Some(sel) = sel {
                    d = d.child(render_selectable_spans(line, theme, false, sel, stream));
                } else {
                    d = d.child(render_spans_line(line, theme));
                }
            }
            d.mb_2().into_any_element()
        }
        Block::Heading { level, spans } => {
            let size = match level {
                1 => px(20.),
                2 => px(18.),
                3 => px(16.),
                _ => px(15.),
            };
            let st = if let Some(sel) = sel {
                render_selectable_spans(spans, theme, false, sel, stream).into_any_element()
            } else {
                spans_to_styled(spans, theme, false)
                    .into_element()
                    .into_any_element()
            };
            div()
                .mb_2()
                .mt_1()
                .min_w(px(0.))
                .text_size(size)
                .font_weight(FontWeight::BOLD)
                .text_color(theme.heading)
                .child(st)
                .into_any_element()
        }
        Block::Code(code) => {
            let mut child = div().min_w(px(0.));
            for line in code.split('\n') {
                let broken = break_long_runs(line);
                if let Some(sel) = sel {
                    // 代码行也纳入可选（同一文本流）
                    let fake = &[Span {
                        text: broken.clone(),
                        bold: false,
                        italic: false,
                        strike: false,
                        code: false,
                        link: false,
                    }];
                    child = child.child(render_selectable_spans(fake, theme, false, sel, stream));
                } else {
                    let st = SpannedText {
                        text: SharedString::from(broken.clone()),
                        runs: vec![TextRun {
                            len: broken.len(),
                            font: font(theme.mono_family.clone()),
                            color: theme.code_text,
                            background_color: None,
                            underline: None,
                            strikethrough: None,
                        }],
                    };
                    child = child.child(st.into_element());
                }
            }
            child
                .bg(rgb(0x0f172a))
                .rounded_lg()
                .p_3()
                .mb_2()
                .text_size(px(13.))
                .text_color(gpui::rgb(0xe2e8f0))
                .into_any_element()
        }
        Block::List { ordered, items } => {
            let mut d = div().mb_2().min_w(px(0.));
            for (i, item) in items.iter().enumerate() {
                let marker = if *ordered {
                    format!("{}. ", i + 1)
                } else {
                    "• ".to_string()
                };
                let mut row = div().flex().flex_row().min_w(px(0.));
                row = row.child(
                    div().min_w(px(0.)).child(
                        SpannedText {
                            text: SharedString::from(marker.to_owned()),
                            runs: vec![TextRun {
                                len: marker.len(),
                                font: font(".SystemUIFont"),
                                color: theme.text,
                                background_color: None,
                                underline: None,
                                strikethrough: None,
                            }],
                        }
                        .into_element(),
                    ),
                );
                // 条目文本放进可收缩容器，超长不可断片段靠 break_long_runs 换行
                if let Some(sel) = sel {
                    row = row.child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .child(render_selectable_spans(item, theme, false, sel, stream)),
                    );
                } else {
                    row = row.child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .child(render_spans_line(item, theme)),
                    );
                }
                d = d.child(row);
            }
            d.into_any_element()
        }
        Block::Table { headers, rows } => {
            let ncols = headers
                .len()
                .max(rows.iter().map(|r| r.len()).max().unwrap_or(0));
            if ncols == 0 {
                return div().into_any_element();
            }

            // 横向滚动模式：先量出每一列的最大内容宽度（含 padding + 边框），
            // 全列单元格统一用该宽度 → 各列分割线严格对齐（Scroll 模式列宽不再按行各异）。
            let mut col_widths: Vec<Pixels> = vec![px(0.); ncols];
            if mode == TableMode::Scroll {
                if let Some(wnd) = window {
                    let font_size = px(13.);
                    for (c, width) in col_widths.iter_mut().enumerate().take(ncols) {
                        let mut m = px(0.);
                        for spans in std::iter::once(headers)
                            .chain(rows.iter())
                            .filter_map(|r| r.get(c))
                        {
                            let st = spans_to_styled(spans, theme, false);
                            let lw = wnd
                                .text_system()
                                .shape_line(st.text, font_size, &st.runs, None)
                                .width;
                            let w = lw + CELL_H_PAD + CELL_BORDER + CELL_HEADER_BOLD_BUFFER;
                            if w > m {
                                m = w;
                            }
                        }
                        *width = m;
                    }
                }
            }

            // 单元格：统一 padding/字号/颜色；c>0 的列加左侧分割线（网格线）；表头加浅色底
            // （捕获 sel/stream 使其为 FnMut）
            let mut render_cell =
                |c: usize, row: &Vec<Vec<Span>>, header: bool, theme: &MarkdownTheme| -> Div {
                    let mut cell = div()
                        .px_2()
                        .py_1()
                        .text_size(px(13.))
                        .text_color(theme.table_text);
                    if header {
                        cell = cell.font_weight(FontWeight::BOLD).bg(theme.table_bg);
                    }
                    // 每一列之间画一条竖直分割线，让各列文字有齐平的参考线
                    if c > 0 {
                        cell = cell.border_l_1().border_color(theme.blockquote_border);
                    }
                    match mode {
                        TableMode::Wrap => {
                            // 等宽列：flex_1 平分可用宽度，min_w(0) 允许收缩，文本自动换行
                            cell = cell.flex_1().min_w(px(0.));
                        }
                        TableMode::Scroll => {
                            // 列宽取自测量值（全列一致 → 对齐），nowrap 保证单行、表宽=内容宽
                            cell = cell.w(col_widths[c]).flex_shrink_0().whitespace_nowrap();
                        }
                    }
                    if let Some(spans) = row.get(c) {
                        let force = header;
                        if let Some(sel) = sel {
                            cell = cell
                                .child(render_selectable_spans(spans, theme, force, sel, stream));
                        } else if force {
                            // 表头：强行使 run 加粗（仅 div.font_weight 不会级联进带显式 font 的 run）
                            cell = cell.child(render_spans_line_weighted(spans, theme, true));
                        } else {
                            cell = cell.child(render_spans_line(spans, theme));
                        }
                    }
                    cell
                };

            let header_row = div()
                .flex()
                .flex_row()
                .children((0..ncols).map(|c| render_cell(c, headers, true, theme)));
            let data_rows: Vec<Div> = rows
                .iter()
                .map(|row| {
                    div()
                        .flex()
                        .flex_row()
                        .border_t_1()
                        .border_color(theme.blockquote_border)
                        .children((0..ncols).map(|c| render_cell(c, row, false, theme)))
                })
                .collect();
            // 表格本体：外框 + 表头 + 数据行（网格线齐平）
            let table_body = div()
                .mt_1()
                .mb_2()
                .rounded_md()
                .border_1()
                .border_color(theme.blockquote_border)
                .overflow_hidden()
                .child(header_row)
                .children(data_rows);

            match mode {
                TableMode::Wrap => table_body.into_any_element(),
                TableMode::Scroll => {
                    // 横向滚动：滚动容器用 flex 行（块级会把子元素钳制到容器宽，导致
                    // 无法横向溢出），表格本体 flex_shrink_0 保持内容自然宽度，超宽时滚动。
                    let state = scroll.cloned().unwrap_or_default();
                    let handle = state.handle.clone();
                    // stateful 需要一个全局（按父级作用域隔离）唯一的 id，按本消息内的
                    // 表格序号生成，保证跨帧状态（横向位置）稳定
                    let idx = *table_idx;
                    *table_idx += 1;
                    let scroller = div()
                        .id(ElementId::Name(SharedString::from(format!(
                            "md-tbl-scroll-{idx}"
                        ))))
                        .mt_1()
                        .flex()
                        .flex_row()
                        .overflow_x_scroll()
                        .track_scroll(&handle)
                        // 滚轮消费：表格可横向滚动时，把滚轮定死在本表格（GPUI 内置横向
                        // 滚动会先把它映射成横向滚动），并阻止向父级（消息区）冒泡，
                        // 否则页面会跟着上下滚动；表格不可滚动时放行让页面正常滚动。
                        .on_scroll_wheel({
                            let handle = handle.clone();
                            move |_event: &ScrollWheelEvent, _window, cx| {
                                if handle.max_offset().x > px(0.) {
                                    cx.stop_propagation();
                                }
                            }
                        })
                        .child(table_body.flex_shrink_0());
                    div()
                        .mt_0()
                        .mb_2()
                        .child(scroller)
                        .child(horizontal_scrollbar(
                            &state,
                            ElementId::Name(SharedString::from(format!("md-tbl-hsb-{idx}"))),
                        ))
                        .into_any_element()
                }
            }
        }
        Block::Quote(spans) => div()
            .mt_1()
            .mb_2()
            .pl_3()
            .py_1()
            .border_l_2()
            .border_color(theme.blockquote_border)
            .bg(theme.blockquote_bg)
            .child(render_spans_line(spans, theme))
            .into_any_element(),
        Block::Hr => div()
            .mt_2()
            .mb_2()
            .h(px(1.))
            .bg(rgb(0xe5e7eb))
            .into_any_element(),
    }
}

/// 把 Markdown 源文本还原成便于复制/粘贴的**纯文本**（去标记语法，保留结构）：
/// 渲染为轻量 Markdown 的入口（用亮色主题；表格默认"自动换行"模式）。
#[allow(dead_code)]
pub fn render_markdown(md: &str) -> Div {
    render_markdown_with_options(
        md,
        &MarkdownTheme::light(),
        TableMode::Wrap,
        None,
        None,
        None,
    )
}

/// 使用自定义主题渲染 Markdown。
#[allow(dead_code)]
pub fn render_markdown_themed(md: &str, theme: &MarkdownTheme) -> Div {
    render_markdown_with_options(md, theme, TableMode::Wrap, None, None, None)
}

/// 渲染已解析的块列表为 GPUI 元素（不含解析步骤，供调用方复用缓存的 `Vec<Block>`）。
///
/// 与 [`render_markdown_with_options`] 的渲染逻辑完全一致，只是块列表由调用方提供，
/// 避免每次重渲染都重复解析（大文档实时预览时按内容缓存 `Vec<Block>` 可显著降耗）。
pub(crate) fn render_markdown_blocks(
    blocks: &[Block],
    theme: &MarkdownTheme,
    mode: TableMode,
    scroll: Option<&TableScrollState>,
    window: Option<&mut Window>,
    sel: Option<&Rc<SelectCtx>>,
) -> Div {
    let mut table_idx = 0usize;
    let mut stream = 0usize;
    let mut window = window;
    let children: Vec<AnyElement> = blocks
        .iter()
        .map(|b| {
            render_block(
                b,
                theme,
                mode,
                scroll,
                window.as_deref_mut(),
                sel,
                &mut stream,
                &mut table_idx,
            )
        })
        .collect();
    div()
        .flex()
        .flex_col()
        .min_w(px(0.))
        .text_color(theme.text)
        .children(children)
}

/// 渲染 Markdown，并指定表格超宽时的处理方式。
///
/// - `scroll`：`TableMode::Scroll` 时提供表格的横向滚动（句柄 + 拖拽状态，调用方
///   需为同一篇内容保存一个 [`TableScrollState`]，跨帧保持横向位置与拖拽）；
/// - `window`：Scroll 模式下用量文本实际宽度来统一列宽（保证各列分割线对齐）；
///   其它模式可传 `None`。
pub fn render_markdown_with_options(
    md: &str,
    theme: &MarkdownTheme,
    mode: TableMode,
    scroll: Option<&TableScrollState>,
    window: Option<&mut Window>,
    sel: Option<&Rc<SelectCtx>>,
) -> Div {
    let blocks = parse_markdown(md);
    render_markdown_blocks(&blocks, theme, mode, scroll, window, sel)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 表头整体强制加粗要作用在 TextRun 的字体上（祖先 div 的 font_weight
    /// 不会级联进带显式 font 的 run——修复"表头加粗没效果"）。
    #[test]
    fn header_force_bold_uses_run_font() {
        let blocks = parse_markdown("列 A");
        let Block::Paragraph(lines) = &blocks[0] else {
            panic!("应解析为段落");
        };
        let spans = &lines[0];
        let theme = MarkdownTheme::light();
        let bold = spans_to_styled(spans, &theme, true);
        let normal = spans_to_styled(spans, &theme, false);
        assert!(
            bold.runs[0].font.weight == FontWeight::BOLD,
            "表头 run 应为加粗字重"
        );
        assert!(
            normal.runs[0].font.weight == FontWeight::NORMAL,
            "普通单元格 run 不应加粗"
        );
    }

    /// 超长不可断片段（长 URL/长串）插入零宽空格以便换行；普通文本不受影响。
    #[test]
    fn break_long_runs_only_splits_long_unbroken_runs() {
        // 普通句子（含空格/标点/中文，本身有断点）→ 原样保留
        assert_eq!(
            break_long_runs("你好，这是普通句子，带 空格。"),
            "你好，这是普通句子，带 空格。"
        );
        // 短单词 → 不插入
        assert_eq!(break_long_runs("short"), "short");
        // 60 个连续字母 → 在 28/42/56 处插入 3 个零宽空格
        let long = "a".repeat(60);
        let broken = break_long_runs(&long);
        assert_eq!(
            broken.chars().filter(|&c| c == '\u{200b}').count(),
            3,
            "60 个连续字母应插入 3 个断点：{broken:?}"
        );
        // 带斜杠/点号的 URL：只在超长单段里插入
        let url = format!("https://example.com/{}", "b".repeat(60));
        let broken_url = break_long_runs(&url);
        assert!(
            broken_url.chars().filter(|&c| c == '\u{200b}').count() >= 1,
            "长地址段应插入断点：{broken_url:?}"
        );
    }

    /// 底部横向滚动条才会出现；否则说明表格被容器宽度钳制、没有横向滚动。
    #[gpui::test]
    async fn scroll_table_overflows_wide_container(cx: &mut gpui::TestAppContext) {
        struct WideHost {
            state: TableScrollState,
        }
        impl gpui::Render for WideHost {
            fn render(
                &mut self,
                window: &mut gpui::Window,
                _cx: &mut gpui::Context<Self>,
            ) -> impl gpui::IntoElement {
                let long: String = "abcdefghijklmnopqrstuvwxyz0123456789".repeat(12);
                let md = format!("| 列A | 很长的列B |\n| --- | --- |\n| 值 | {long} |");
                div().h(px(200.)).w(px(400.)).bg(gpui::rgb(0xffffff)).child(
                    render_markdown_with_options(
                        &md,
                        &MarkdownTheme::light(),
                        TableMode::Scroll,
                        Some(&self.state),
                        Some(window),
                        None,
                    ),
                )
            }
        }

        let (view, cx) = cx.add_window_view(|_window, _cx| WideHost {
            state: TableScrollState::new(),
        });
        cx.simulate_resize(gpui::size(px(500.), px(400.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let max_w = cx.update(|_window, cx| view.read(cx).state.handle.max_offset().x);
        assert!(
            max_w > px(0.),
            "横向滚动模式下超宽表格应可横向滚动：max_offset.width={max_w:?}"
        );
        // 点击横向滚动条应能"跳转/拖拽"到对应位置：横向偏移从 0 变成负值。
        // 滚动条是贴在滚动容器底部的高 3px 细条，点击点必须落在 [bottom, bottom+3px] 内。
        let (bar_left, bar_bottom, viewport_w) = cx.update(|_window, cx| {
            let b = view.read(cx).state.handle.bounds();
            (b.left(), b.bottom(), b.size.width)
        });
        cx.simulate_click(
            gpui::point(bar_left + viewport_w * 0.7, bar_bottom + px(1.5)),
            gpui::Modifiers::default(),
        );
        cx.run_until_parked();
        let offset_x = cx.update(|_window, cx| view.read(cx).state.handle.offset().x);
        assert!(
            offset_x < px(-1.),
            "点击横向滚动条应能把表格向右滚动：offset.x={offset_x:?}"
        );
    }
}
