//! status_bar：VSCode 式三区分区状态栏（P2-11）。
//!
//! 布局（自左向右，区之间用细分隔线，见 [`status_section_divider`]）：
//! - **左侧区**：语言徽标（圆角小底 + 语言类别「徽记色点」，色源 [`crate::ui::theme::language_badge_color`]）
//!   + 文件名（悬停浮层提示完整路径，P2-9 tooltip 复用）；未打开文件时显示「未命名」占位。
//! - **中间区**：光标位置（编辑区 `Ln x, Col y`）或滚动位置（预览/PDF 页码）；无编辑内容时整段省略。
//! - **右侧区**：模式/状态（预览 / 只读 / 跟随系统主题 / UTF-8）→ 统计（行/字）→ 阅读控制按钮。
//!
//! 全部数据来自既有状态派生（激活选项卡的 file_path/语言、光标、PDF/EPUB 页码、主题偏好），
//! 不新造全局状态；渲染为静态派生，无持续轮询。

use super::*;

/// 分区间的细竖线：1px × 14px，取 `bar_border`（＝`colors.border` 系）色，垂直居中。
fn status_section_divider(color: gpui::Hsla) -> gpui::Div {
    let owned = "status-bar-divider".to_string();
    div()
        .debug_selector(move || owned.clone())
        .w(px(1.))
        .h(px(14.))
        .flex_shrink_0()
        .bg(color)
}

/// 状态栏内带底色小部件（语言徽标 / 全屏 / 换行按钮）统一高度（px）：
/// 跟随 `STATUS_BAR_H` 收缩（上下各留 2px 内边距 + 12px 下限），保证底色不溢出状态栏。
fn status_pill_h() -> f32 {
    (crate::config::layout::STATUS_BAR_H - 4.0).max(12.0)
}

impl EditorApp {
    pub(super) fn render_status_bar(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let tab = self.tabs.get(self.active_tab);
        let (line_count, char_count) = tab.map(|t| (t.line_count, t.char_count)).unwrap_or((0, 0));
        let dirty = tab.map(|t| t.dirty).unwrap_or(false);
        let language = self.language_label().to_string();
        let bar_border = self.bar_border();

        // 右侧条件沿用既有判定：
        // PDF / EPUB 等只读预览自带全屏阅读，不适用「全屏/还原」「换行」切换，故隐藏。
        let preview_on = tab.map(|t| t.preview_on).unwrap_or(false);
        let readonly_preview = tab.map(|t| t.readonly_preview).unwrap_or(false);
        let fullscreen = tab.map(|t| t.preview_fullscreen).unwrap_or(false);
        let wrap_on = tab
            .map(|t| t.md_preview.read(cx).is_wrap())
            .unwrap_or(false);
        let is_markdown = tab
            .and_then(|t| t.file_path.as_ref().map(|p| Self::is_markdown_path(p.as_path())))
            .unwrap_or(false);

        // 左侧区：语言徽标 + 文件名
        let left = self.status_bar_left(dirty, language, window, cx);

        // 中间区数据源（派生自激活选项卡，无新状态）：只读预览页码 / PDF 页码 / 光标行列。
        let epub_page = tab.and_then(|t| t.md_preview.read(cx).viewport_page_bounds());
        let pdf_page = tab
            .and_then(|t| t.pdf_preview.as_ref().and_then(|pv| pv.read(cx).current_page()));
        let cursor_pos = tab.and_then(|t| {
            // 只读预览（EPUB/PDF/图片/视频）编辑区为空，无光标位置可示（EPUB 的
            // `char_count` 是书目正文统计，不能当「有编辑内容」）。
            if t.readonly_preview {
                return None;
            }
            // 「有编辑内容」以字符数为准（空文档 line_count 恒为 1 行空行，不能当内容）。
            if t.char_count > 0 || t.cursor_row != 0 || t.cursor_col != 0 {
                Some((t.cursor_row + 1, t.cursor_col + 1))
            } else {
                None
            }
        });
        let middle = self.status_bar_middle(epub_page, pdf_page, cursor_pos);

        let mut root = div()
            .id("status-bar")
            .debug_selector(|| "status-bar".into())
            .h(px(crate::config::layout::STATUS_BAR_H))
            // 状态栏无缝直连正文与窗缘：左右/底缘贴窗（mb 0），顶部贴正文（mt 0），
            // 仅保留顶部/四周发丝边框分隔。
            .mt(px(0.))
            .border_1()
            .border_color(self.hairline())
            .bg(crate::ui::shell::card_surface(cx))
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .px_3()
            .child(left)
            .child(status_section_divider(bar_border));

        // 中间区存在才挂分隔线（省略时不给「光秃秃一条竖线」的孤立分隔）。
        if let Some(middle) = middle {
            root = root
                .child(middle)
                .child(status_section_divider(bar_border));
        }

        // 弹性留白把右侧区推到底端；右侧区：状态 → 统计 → 按钮。
        root = root
            .child(div().flex_1())
            .child(self.status_bar_right(
                line_count,
                char_count,
                preview_on,
                readonly_preview,
                fullscreen,
                wrap_on,
                is_markdown,
                cx,
            ));

        root
    }

    /// 左侧区：文件名（悬停浮层显示完整路径）＋ 语言徽标 pill。
    fn status_bar_left(
        &self,
        dirty: bool,
        language: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let text_primary = self.text_primary();
        let file_name = self.file_display_name();
        // P2-9 tooltip：文件名悬停后展示完整路径（路径提示）。
        let path_hint = self
            .tabs
            .get(self.active_tab)
            .and_then(|t| t.file_path.as_ref())
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let has_path = !path_hint.is_empty();
        const TIP_ID: &str = "status-file-name";
        let tip = crate::ui::tooltip::tooltip_state(TIP_ID, window, cx);
        let tip_on = crate::ui::tooltip::tooltip_visible(&tip, cx);
        let view_id = window.current_view();

        // 路径浮层锚在文件名块**右缘**（`tooltip_layer` 偏移 = host_w + 间隙）：实测
        // 「脏标记 ● + 间距 + 文件名文本」的可见宽度，否则 host_w=0 会让浮层盖住
        // 触发它的文件名。文件名有 `max_w(320px)` 截断，测量值同样封顶（一致）。
        let base_font = window.text_style().font();
        let measure = |text: SharedString, weight: gpui::FontWeight| -> f32 {
            let len = text.len();
            let mut font = base_font.clone();
            font.weight = weight;
            f32::from(
                window
                    .text_system()
                    .shape_line(
                        text,
                        px(12.),
                        &[gpui::TextRun {
                            len,
                            font,
                            color: gpui::Hsla::default(),
                            background_color: None,
                            underline: None,
                            strikethrough: None,
                        }],
                        None,
                    )
                    .width(),
            )
        };
        let dirty_dot_w = if dirty {
            measure("●".into(), gpui::FontWeight::NORMAL) + 4.0
        } else {
            0.0
        };
        let name_w = measure(file_name.clone().into(), gpui::FontWeight::MEDIUM).min(320.0);
        let name_block_w = dirty_dot_w + name_w;

        // 文件名块：脏标记 + 文件名（过长截断）+ 路径 tooltip 浮层。
        let name_block = div()
            .id(TIP_ID)
            .debug_selector(|| TIP_ID.into())
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .min_w(px(0.))
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(if dirty {
                        self.chrome().dirty
                    } else {
                        self.chrome().text_minor
                    })
                    .child(if dirty { "●" } else { "" }),
            )
            .child(
                div()
                    .min_w(px(0.))
                    .max_w(px(320.))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_size(px(12.))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(text_primary)
                    .child(file_name),
            )
            // 路径提示浮层（锚在文件名块右缘垂直居中；无路径（未命名）恒不显示）。
            .child(crate::ui::tooltip::tooltip_layer(
                TIP_ID,
                &path_hint,
                has_path && tip_on,
                name_block_w,
                crate::config::layout::STATUS_BAR_H,
                cx,
            ))
            .on_hover({
                let state = tip.clone();
                move |hovered, _w, cx| {
                    crate::ui::tooltip::tooltip_hover(&state, *hovered, cx);
                    cx.notify(view_id);
                }
            });

        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .min_w(px(0.))
            .child(name_block)
            .child(self.status_language_badge(&language))
    }

    /// 语言徽标 pill：圆角小底（`pill_*` 令牌）+ 前端「徽记色点」
    /// （`ui::theme::language_badge_color`，VS Code 式按语言类别着色）。
    fn status_language_badge(&self, language: &str) -> gpui::Div {
        let dot = crate::ui::theme::language_badge_color(language, self.dark_mode);
        let owned = "status-language-badge".to_string();
        div()
            .debug_selector(move || owned.clone())
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .px_2()
            .h(px(status_pill_h()))
            .rounded_sm()
            .bg(self.chrome().pill_bg)
            .text_size(px(11.))
            .text_color(self.chrome().pill_fg)
            .child(div().w(px(6.)).h(px(6.)).rounded_full().bg(dot))
            .child(language.to_string())
    }

    /// 中间区：光标位置（编辑区 `Ln x, Col y`）或滚动位置（预览/PDF 页码）。
    /// 全部输入为 render 期先算好的纯数据（不持 `cx` 引用，避免 Rust2024 impl Trait
    /// 生命周期捕获造成的借用冲突）；无编辑内容时返回 `None`，整段（含分隔线）省略。
    fn status_bar_middle(
        &self,
        epub_page: Option<(usize, usize)>,
        pdf_page: Option<(usize, usize)>,
        cursor_pos: Option<(usize, usize)>,
    ) -> Option<gpui::Div> {
        // 只读预览（EPUB）：视口分页页码（滚动位置实时）。
        if let Some((page, total)) = epub_page {
            return Some(self.middle_text("status-epub-page", format!("第 {page} / {total} 页")));
        }
        // PDF：当前第一张可见页 / 总页数（滚动位置）。
        if let Some((page, total)) = pdf_page {
            return Some(self.middle_text("status-pdf-page", format!("第 {page} / {total} 页")));
        }
        // 编辑区：有内容才显示 行:列（未命名空文档省略）。
        if let Some((ln, col)) = cursor_pos {
            return Some(self.middle_text("status-cursor", format!("Ln {ln}, Col {col}")));
        }
        None
    }

    /// 中间区单条文本（muted/弱化文字；`debug_selector` 供测试定位）。
    fn middle_text(&self, sel: &'static str, text: String) -> gpui::Div {
        let owned = sel.to_string();
        div()
            .debug_selector(move || owned.clone())
            .flex_shrink_0()
            .text_size(px(12.))
            .text_color(self.chip_text())
            .child(text)
    }

    /// 右侧区：模式/状态项（预览 / 只读 / 主题 / UTF-8）→ 统计（行/字）→ 阅读控制按钮。
    /// 全部来自既有数据源派生，无轮询。
    #[allow(clippy::too_many_arguments)]
    fn status_bar_right(
        &self,
        line_count: usize,
        char_count: usize,
        preview_on: bool,
        readonly_preview: bool,
        fullscreen: bool,
        wrap_on: bool,
        is_markdown: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let text_minor = self.text_minor();
        let chip_text = self.chip_text();

        let mut right = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .flex_shrink_0();

        // 模式/状态项。
        if preview_on && !readonly_preview {
            right = right.child(self.status_item("status-mode-preview", "预览", chip_text));
        }
        if readonly_preview {
            right = right.child(self.status_item("status-mode-readonly", "只读", chip_text));
        }
        // 主题三态：跟随系统主题 / 浅色 / 深色。
        let theme_label = match self.theme_pref {
            crate::config::user::ThemePref::Follow => "跟随系统主题",
            crate::config::user::ThemePref::Light => "浅色",
            crate::config::user::ThemePref::Dark => "深色",
        };
        right = right.child(self.status_item("status-theme", theme_label, chip_text));
        // 编码：可编辑文档一律以 UTF-8 读盘（既有约定，非只读预览才有意义）。
        if !readonly_preview {
            right = right.child(self.status_item("status-encoding", "UTF-8", chip_text));
        }

        // 统计（只读预览 EPUB/PDF 编辑区为空，行/字读数无意义——已让位给页码，不显示；
        // 可编辑文档计数为 0（空文档）时省略）。
        if !readonly_preview && (line_count > 0 || char_count > 0) {
            right = right
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(text_minor)
                        .child(format!("{line_count} 行")),
                )
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(text_minor)
                        .child(format!("{char_count} 字")),
                );
        }

        // 全屏/还原按钮：仅在开启预览且非只读预览（PDF/EPUB）时显示。
        if preview_on && !readonly_preview {
            right = right.child(self.status_fullscreen_btn(fullscreen, cx));
        }

        // 换行开关按钮：仅非只读预览（非 PDF/EPUB）且非 Markdown 时显示。
        // Markdown 编辑区与预览区默认/始终自动换行，不提供切换（隐藏该按钮）。
        if !readonly_preview && !is_markdown {
            right = right.child(self.status_wrap_btn(wrap_on, cx));
        }

        right
    }

    /// 状态栏上的小状态项：muted 文字 + 恰当间距（无 hover/点击）。
    fn status_item(&self, sel: &'static str, text: &str, color: gpui::Hsla) -> gpui::Div {
        let owned = sel.to_string();
        div()
            .debug_selector(move || owned.clone())
            .flex_shrink_0()
            .text_size(px(12.))
            .text_color(color)
            .child(text.to_string())
    }

    /// 状态栏右侧「全屏/还原」切换按钮（仅开启预览且非只读预览时挂载）。
    fn status_fullscreen_btn(&self, fullscreen: bool, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("status-fullscreen-btn")
            .debug_selector(|| "status-fullscreen-btn".into())
            .flex()
            .flex_row()
            .items_center()
            .px_2()
            .h(px(status_pill_h()))
            .rounded_sm()
            .bg(self.chrome().pill_bg)
            .text_size(px(12.))
            .text_color(self.chrome().pill_fg)
            .cursor(CursorStyle::PointingHand)
            .child(if fullscreen { "还原" } else { "全屏" })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this: &mut EditorApp, _ev, window, cx| {
                    this.toggle_preview_fullscreen(window, cx);
                }),
            )
    }

    /// 状态栏右侧「换行/不换行」切换按钮（仅非只读预览且非 Markdown 时挂载）。
    fn status_wrap_btn(&self, wrap_on: bool, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("status-wrap-btn")
            .debug_selector(|| "status-wrap-btn".into())
            .flex()
            .flex_row()
            .items_center()
            .px_2()
            .h(px(status_pill_h()))
            .rounded_sm()
            .bg(self.chrome().pill_bg)
            .text_size(px(12.))
            .text_color(self.chrome().pill_fg)
            .cursor(CursorStyle::PointingHand)
            .child(if wrap_on { "换行" } else { "不换行" })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this: &mut EditorApp, _ev, window, cx| {
                    this.toggle_wrap(window, cx);
                }),
            )
    }
}
