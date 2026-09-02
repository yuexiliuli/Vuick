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
    div, list, rgb, px, App, Context, FontWeight, Hsla, InteractiveElement, IntoElement,
    ListAlignment, ListState, ParentElement, Pixels, Render, ScrollHandle, SharedString,
    StatefulInteractiveElement, Styled, Window,
};
use gpui_component::scroll::{Scrollbar, ScrollbarMode, ScrollableElement as _};

use crate::markdown::parser::{parse_to_rows, Row, RowKind, TableAlign, TableData};

/// 行级虚拟化在视口上下各多构建的像素量（保证快速滚动时内容连续）。
const ROW_OVERDRAW_PX: f32 = 600.0;
/// 内容宽估算的额外缓冲（px），避免估算值微偏导致最后几个字符被截断。
const WIDTH_BUFFER_PX: f32 = 24.0;

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
    fn from_theme(cx: &mut App) -> Self {
        let theme = gpui_component::Theme::global(cx);
        let tokens = theme.semantic_tokens();
        let colors = tokens.colors;
        let typo = tokens.typography;
        let sm = typo.sm;
        let mono = typo.mono_md;
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
            para_size: sm.size,
            para_line: sm.line_height,
            para_gap: px(4.),
            code_size: mono.size,
            code_line: mono.line_height,
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
        }
    }

    /// 设置预览背景色（打开/同步时由应用传入，使预览与编辑区同底色）。
    pub fn set_background(&mut self, bg: Hsla) {
        self.bg = Some(bg);
    }

    /// 设置预览源码（仅影响预览，原文不变）。
    pub fn set_source(&mut self, src: &str, cx: &mut App) {
        let rows: Vec<Rc<Row>> = parse_to_rows(src).into_iter().map(Rc::new).collect();
        // 估算最宽一行（决定横向滚动范围），加一点缓冲防截断
        let p = StyleParams::from_theme(cx);
        self.content_w = rows
            .iter()
            .map(|r| est_row_width(r, &p))
            .fold(0.0, f32::max)
            + WIDTH_BUFFER_PX;
        if rows.len() != self.list_state.item_count() {
            // 行数变化 → 重建并 measure_all，保证滚动条范围从一开始就正确
            self.list_state = ListState::new(rows.len(), ListAlignment::Top, px(ROW_OVERDRAW_PX))
                .measure_all();
        } else {
            // 行数不变但内容变了 → 重新测量高度（保留滚动位置）
            self.list_state.remeasure();
        }
        self.rows = Rc::new(rows);
    }
}

impl Render for MarkdownPreview {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = self.rows.clone();
        let p = Rc::new(StyleParams::from_theme(cx));
        let bg = self.bg.unwrap_or(p.bg);
        let list_state = self.list_state.clone();
        let content_w = self.content_w;

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
                    "render: content_w={content_w:.0} max_off=({:.0},{:.0}) bounds={:?}",
                    h.max_offset().x.as_f32(),
                    h.max_offset().y.as_f32(),
                    h.bounds(),
                );
            }
        }

        let content = list(list_state.clone(), move |ix, _window, cx| {
            render_row(&rows[ix], &p, cx)
        })
        .size_full();

        // 宽包装层：宽度 = max(估算最宽内容, 视口宽)。用 `min_w_full`（100%）兜底视口，
        // 不再靠 `view_w` 字段 + `on_prepaint` 逐帧校准（那种写法依赖绘制时序、会漂移）：
        // 普通文档撑满视口、超宽文档产生真实横向溢出。这也是 gpui-component 的
        // `Scrollable` 对横向滚动容器子节点的写法（`flex_none` + `min_w_full`）。
        let inner = div()
            .id("md-list-wrap")
            .debug_selector(|| "md-list-wrap".into())
            .w(px(content_w))
            .min_w_full()
            .h_full()
            .child(content);

        // 横向滚动容器：原生 `overflow_x_scroll`，由 `h_handle` 跟踪，
        // gpui_component 的横向滚动条读它取范围 / 偏移，并回写偏移拖拽。
        // `restrict_scroll_to_axis` 阻止 GPUI 把竖向滚轮增量折到横向，
        // 竖向滚轮只滚动 List，横向滚轮（含 Shift+滚轮）在此原生横滚。
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
            // 横向滚动条（组件自绘，跟踪横向容器句柄）。
            // 挂法与 gpui-component 的 `Scrollable`/`vertical_scrollbar` 保持一致：
            // 先套一层 absolute+inset_0 的叠层，再放 `viewport_from_layout()` 的滚动条。
            // 裸 `Scrollbar::horizontal` 直接作 `flex` 容器的绝对子元素时，布局尺寸可能
            // 塌缩（当作 flex item、flex-basis 趋近 0），导致滚动条画不出来；叠层保证它
            // 铺满预览区、拿到确定视口。默认 Scrolling 模式只在横向偏移变化时短暂出现，
            // 竖向滚轮不会触发它，内容超宽时也看不见 → 显式 Always（无横向溢出仍自动隐藏）。
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
            // 竖向滚动条（组件自绘，跟踪 ListState）
            .vertical_scrollbar(&list_state)
    }
}

/// 渲染单个行（固定高度、不换行、超出裁剪；超宽由外层横向滚动容器处理）。
fn render_row(row: &Rc<Row>, p: &Rc<StyleParams>, _cx: &mut App) -> gpui::AnyElement {
    // 表格是带 id 的有状态元素，直接整体转换，不参与下方 Div 匹配
    if let RowKind::Table = row.kind {
        let table = row.table.as_ref().expect("Table 行必须携带表格结构");
        return render_table(table, p).into_any_element();
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

        let p = cx.update(|_w, cx| StyleParams::from_theme(cx));
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
}
