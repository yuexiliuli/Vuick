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
    div, list, px, App, Context, FontWeight, Hsla, InteractiveElement, IntoElement, ListAlignment,
    ListState, ParentElement, Pixels, Render, ScrollHandle, SharedString, StatefulInteractiveElement,
    Styled, Window,
};
use gpui_component::scroll::{Scrollbar, ScrollbarMode, ScrollableElement as _};

use crate::markdown::parser::{parse_to_rows, Row, RowKind};

/// 行级虚拟化在视口上下各多构建的像素量（保证快速滚动时内容连续）。
const ROW_OVERDRAW_PX: f32 = 600.0;
/// 内容宽估算的额外缓冲（px），避免估算值微偏导致最后几个字符被截断。
const WIDTH_BUFFER_PX: f32 = 24.0;

/// 排版参数：渲染时从全局主题取一份“快照”，避免每行重复查主题。
struct StyleParams {
    mono: SharedString,
    fg: Hsla,
    muted_fg: Hsla,
    muted_bg: Hsla,
    divider: Hsla,
    bg: Hsla,
    para_size: Pixels,
    para_line: Pixels,
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
        Self {
            mono: typo.mono.clone(),
            fg: colors.foreground,
            muted_fg: colors.muted_foreground,
            muted_bg: colors.muted,
            divider: colors.border,
            bg: colors.background,
            para_size: sm.size,
            para_line: sm.line_height,
            code_size: mono.size,
            code_line: mono.line_height,
            blank_h: px(8.),
            rule_h: px(18.),
        }
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
            // 等宽字体按 0.65em/字符 + 左右 padding
            row.text.chars().count() as f32 * p.code_size.as_f32() * 0.65 + 16.0
        }
        RowKind::Heading(n) => {
            let scale = 1.9f32 - 0.13f32 * (n as f32 - 1.0);
            est_text_width(&row.text, p.para_size.as_f32() * scale)
        }
        RowKind::Quote => est_text_width(&row.text, p.para_size.as_f32()) + 8.0,
        RowKind::Bullet => {
            let prefix_w = row
                .prefix
                .as_ref()
                .map(|s| s.chars().count() as f32 * 8.0)
                .unwrap_or(0.0);
            est_text_width(&row.text, p.para_size.as_f32()) + prefix_w + 12.0
        }
        RowKind::Paragraph => est_text_width(&row.text, p.para_size.as_f32()),
    }
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
    let el: gpui::Div = match row.kind {
        RowKind::Blank => div().h(p.blank_h).w_full().flex_shrink_0(),
        RowKind::Rule => div()
            .h(p.rule_h)
            .w_full()
            .flex_shrink_0()
            .flex()
            .items_center()
            .px_2()
            .child(div().h(px(2.)).w_full().bg(p.divider).rounded_full()),
        RowKind::Code => {
            // 代码行：整行背景带，文本带左右 padding
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
                        .pl_2()
                        .pr_2()
                        .whitespace_nowrap()
                        .font_family(p.mono.clone())
                        .text_size(p.code_size)
                        .text_color(p.fg)
                        .child(t),
                )
        }
        RowKind::Heading(n) => {
            let scale = 1.9f32 - 0.13f32 * (n as f32 - 1.0);
            let fs = px(p.para_size.as_f32() * scale);
            let h = px(p.para_line.as_f32() * scale);
            div()
                .h(h)
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
            .h(p.para_line)
            .w_full()
            .flex_shrink_0()
            .ml_2()
            .flex()
            .items_center()
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
                .h(p.para_line)
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
            .h(p.para_line)
            .w_full()
            .flex_shrink_0()
            .flex()
            .items_center()
            .overflow_hidden()
            .whitespace_nowrap()
            .text_size(p.para_size)
            .text_color(p.fg)
            .child(row.text.clone()),
    };
    el.into_any_element()
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
}
