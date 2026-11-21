//! status_bar：VSCode 式三区分区状态栏（P2-11）。
//!
//! 布局（自左向右，区之间用细分隔线，见 [`status_section_divider`]）：
//! - **左侧区**：语言徽标（圆角小底 + 语言类别「徽记色点」，色源 [`ui::theme::language_badge_color`]）。
//!   文件名与「悬停显示完整路径」提示在选项卡栏：每个有名文件选项卡
//!   悬停 200ms 后在其下方浮出自身完整路径 tooltip（见 [`crate::app::tabs`]）；未打开文件时
//!   徽标显示「纯文本」占位。
//! - **中间区**：光标位置（编辑区 `Ln x, Col y`）或滚动位置（预览/PDF 页码与缩放百分比）；
//!   PDF 模式下缩放滑条紧随「缩放百分比」右侧（用户 2026-11-20 放置）；无编辑内容时整段省略。
//! - **右侧区**：模式/状态（预览 / 网格 / 只读 / 跟随系统主题 / UTF-8）→ 统计（行/字）→ 阅读控制按钮。
//!
//! 全部数据来自既有状态派生（激活选项卡的 file_path/语言、光标、PDF/EPUB 页码、主题偏好），
//! 不新造全局状态；渲染为静态派生，无持续轮询。

// 状态栏片段构造逐参数传参 → 放行
#![allow(clippy::too_many_arguments)]

use super::*;

use pdf::view::PageMode;

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

/// 状态栏右侧带底 pill（全屏/换行/音乐按钮）目标高度（px）：`ui::Pill` Md 档默认
/// 22px 会在 23px 状态栏里顶满，这里按 `STATUS_BAR_H - 4 = 19` 覆写（上下各留
/// 2px 内边距 + 12px 下限），保证底色不溢出状态栏。
fn status_pill_h() -> f32 {
    (config::layout::STATUS_BAR_H - 4.0).max(12.0)
}

impl EditorApp {
    pub(super) fn render_status_bar(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        // 借用约束：一次 borrow 取出激活选项卡的全部廉价字段（Scalar/String/实体引用
        // 读数）即释放 guard——杜绝「长借用 model.borrow() 构造子元素」后子元素继续借用的
        // 隐患；后续 `status_bar_left/right` 等自身再 borrow，互不嵌套。
        let (
            line_count,
            char_count,
            language,
            preview_on,
            readonly_preview,
            fullscreen,
            wrap_on,
            is_markdown,
            epub_page,
            pdf_page,
            cursor_pos,
            pdf_active,
            pdf_grid_on,
            pdf_page_mode,
            pdf_zoom,
        ) = {
            let model = self.model.borrow();
            let tab = model.tabs.get(model.active_tab);
            let (line_count, char_count) = tab
                .map(|t| (t.payload.line_count, t.payload.char_count))
                .unwrap_or((0, 0));
            let language = self.language_label().to_string();
            // 右侧条件沿用既有判定：
            // PDF / EPUB 等只读预览自带全屏阅读，不适用「全屏/还原」「换行」切换，故隐藏。
            let preview_on = tab.map(|t| t.payload.preview_on).unwrap_or(false);
            let readonly_preview = tab.map(|t| t.payload.readonly_preview).unwrap_or(false);
            let fullscreen = tab.map(|t| t.payload.preview_fullscreen).unwrap_or(false);
            // P4-2：预览统一 WyEditor（页面恒换行，`MarkdownPreview::is_wrap` 已删）；
            // 「换行」按钮显示 = 有统一 WyEditor 预览在场。
            let wrap_on = tab.map(|t| t.payload.md_wy.is_some()).unwrap_or(false);
            let is_markdown = tab
                .and_then(|t| {
                    t.payload
                        .file_path
                        .as_ref()
                        .map(|p| Self::is_markdown_path(p.as_path()))
                })
                .unwrap_or(false);
            // 中间区数据源（派生自激活选项卡，无新状态）：只读预览页码 / PDF 页码 / 光标行列。
            let epub_page = tab
                .map(|_| self.epub_page_bounds(window, cx))
                .unwrap_or(None);
            let pdf_page = tab.and_then(|t| match &t.payload.reader {
                MediaReader::Pdf(pv) => {
                    let v = pv.read(cx);
                    v.current_page()
                        .map(|(page, total)| (page, total, v.zoom_factor()))
                }
                _ => None,
            });
            let cursor_pos = tab.and_then(|t| {
                // 只读预览（EPUB/PDF/图片/视频）编辑区为空，无光标位置可示（EPUB 的
                // `char_count` 是书目正文统计，不能当「有编辑内容」）。
                if t.payload.readonly_preview {
                    return None;
                }
                // 「有编辑内容」以字符数为准（空文档 line_count 恒为 1 行空行，不能当内容）。
                if t.payload.char_count > 0
                    || t.payload.cursor_row != 0
                    || t.payload.cursor_col != 0
                {
                    Some((t.payload.cursor_row + 1, t.payload.cursor_col + 1))
                } else {
                    None
                }
            });
            // wave4：页面网格视图开关——显隐只依「激活选项卡是否为 PDF 阅读器」（PDF 恒
            // 只读全屏预览，勿加「非预览」门）；高亮态随 PdfViewer 的 grid_mode 实体状态。
            let pdf_active = tab
                .map(|t| matches!(&t.payload.reader, MediaReader::Pdf(_)))
                .unwrap_or(false);
            let pdf_grid_on = tab
                .and_then(|t| match &t.payload.reader {
                    MediaReader::Pdf(pv) => Some(pv.read(cx).grid_mode()),
                    _ => None,
                })
                .unwrap_or(false);
            // 阅读页排模式（连续/单页/双页；与网格总览正交，非 PDF 缺省连续）与阅读缩放
            // （Ctrl+滚轮 / 显示比例滑条共享同一 zoom 源）。供右区页排钮选中态与左区滑条 render 读。
            let pdf_page_mode = tab
                .and_then(|t| match &t.payload.reader {
                    MediaReader::Pdf(pv) => Some(pv.read(cx).page_mode()),
                    _ => None,
                })
                .unwrap_or(PageMode::Continuous);
            let pdf_zoom = tab
                .and_then(|t| match &t.payload.reader {
                    MediaReader::Pdf(pv) => Some(pv.read(cx).zoom_factor()),
                    _ => None,
                })
                .unwrap_or(1.0);
            (
                line_count,
                char_count,
                language,
                preview_on,
                readonly_preview,
                fullscreen,
                wrap_on,
                is_markdown,
                epub_page,
                pdf_page,
                cursor_pos,
                pdf_active,
                pdf_grid_on,
                pdf_page_mode,
                pdf_zoom,
            )
        };
        let bar_border = self.bar_border();

        // 左侧区：仅语言徽标；缩放滑条已移入中间区（紧随「第 N/M 页 100%」百分比右侧，
        // 见 `status_bar_middle`）。
        let left = self.status_bar_left(language);
        let middle = self.status_bar_middle(epub_page, pdf_page, cursor_pos, pdf_grid_on, pdf_zoom, cx);

        let mut root = div()
            .id("status-bar")
            .debug_selector(|| "status-bar".into())
            .h(px(config::layout::STATUS_BAR_H))
            // 状态栏无缝直连正文与窗缘：左右/底缘贴窗（mb 0），顶部贴正文（mt 0），
            // 仅保留顶部/四周发丝边框分隔。
            .mt(px(0.))
            .border_1()
            .border_color(self.hairline())
            .bg(ui::shell::card_surface(cx))
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .px_3()
            .child(left)
            .child(status_section_divider(bar_border));

        // 中间区存在才挂分隔线（省略时不给「光秃秃一条竖线」的孤立分隔）。
        if let Some(middle) = middle {
            root = root.child(middle).child(status_section_divider(bar_border));
        }

        // 弹性留白把右侧区推到底端；右侧区：状态 → 统计 → 按钮。
        root = root.child(div().flex_1()).child(self.status_bar_right(
            line_count,
            char_count,
            preview_on,
            readonly_preview,
            fullscreen,
            wrap_on,
            is_markdown,
            pdf_active,
            pdf_grid_on,
            pdf_page_mode,
            cx,
        ));

        root
    }

    /// 左侧区：语言徽标 pill（文件名与「悬停显示完整路径」提示在选项卡栏，
    /// 见 `shell_tabs::TabBar` 的选项卡元素）。缩放滑条已移至中间区（紧随「第 N/M 页
    /// 100%」百分比右侧，见 [`Self::status_bar_middle`]）。
    fn status_bar_left(&self, language: String) -> gpui::Div {
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .min_w(px(0.))
            .child(self.status_language_badge(&language))
    }

    /// 语言徽标 pill：`ui::Pill`（Sm/Chip 壳）＋前端「徽记色点」
    /// （`ui::theme::language_badge_color`，VS Code 式按语言类别着色）；纯展示无点击。
    fn status_language_badge(&self, language: &str) -> impl IntoElement {
        let dot = ui::theme::language_badge_color(language, self.model.borrow().dark_mode);
        ui::Pill::new("status-language-badge")
            .size(ui::PillSize::Sm)
            .tone(ui::PillTone::Chip)
            .leading_dot(dot)
            .label(language.to_string())
    }

    /// 中间区：光标位置（编辑区 `Ln x, Col y`）或滚动位置（预览/PDF 页码）。
    /// 输入为 render 期先算好的纯数据；PDF 分支把缩放滑条（[`Self::status_pdf_zoom_slider`]）
    /// 紧随「缩放百分比」右侧一并挂载（用户 2026-11-20 放置）。无编辑内容时返回 `None`，
    /// 整段（含分隔线）省略。
    fn status_bar_middle(
        &self,
        epub_page: Option<(usize, usize)>,
        pdf_page: Option<(usize, usize, f32)>,
        cursor_pos: Option<(usize, usize)>,
        pdf_grid_on: bool,
        pdf_zoom: f32,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Div> {
        // 只读预览（EPUB）：视口分页页码（滚动位置实时）。
        if let Some((page, total)) = epub_page {
            return Some(self.middle_text("status-epub-page", format!("第 {page} / {total} 页")));
        }
        // PDF：当前第一张可见页 / 总页数（滚动位置）+ 当前缩放百分比（真机取证 zoom%），
        // 缩放滑条紧随百分比右侧（2026-11-20 用户要求「滑条放到缩放百分比右边」）；
        // 网格模式隐藏滑条（网格有独立 grid_zoom）。
        if let Some((page, total, _)) = pdf_page {
            let mut m = div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(self.middle_text(
                    "status-pdf-page",
                    format!("第 {page} / {total} 页  {:.0}%", pdf_zoom * 100.0),
                ));
            if !pdf_grid_on {
                m = m.child(self.status_pdf_zoom_slider(pdf_zoom, cx));
            }
            return Some(m);
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

    /// 右侧区：模式/状态项（预览 / 网格 / 只读 / 主题 / UTF-8）→ 统计（行/字）→ 阅读控制按钮。
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
        pdf_active: bool,
        pdf_grid_on: bool,
        pdf_page_mode: PageMode,
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
        // PDF 页面视图控制组：紧跟「预览」、置于「只读」左侧（用户验收项：不放最右边）；
        // 仅激活选项卡为 PDF 阅读器时显示（PDF 恒只读全屏预览、无「全屏/还原」冲突）。
        if pdf_active {
            // 页排钮：双页（选中=高亮 pill）/ 网格（独立于 page_mode、保留既有
            // `toggle_grid` 语义）；连续阅读 = 双页钮不选中。
            // 显示比例滑条仅阅读模式（非网格）挂载——网格有独立 grid_zoom，滑条只控
            // 单列阅读缩放（与 Ctrl+滚轮共享同一 zoom 源 → 滑块与中间区 zoom% 双向同步）。
            right = right
                .child(self.status_pdf_mode_btn(
                    "status-pdf-facing-btn",
                    "双页",
                    pdf_page_mode == PageMode::Facing,
                    pdf_grid_on,
                    pdf_page_mode,
                    PageMode::Facing,
                    cx,
                ))
                .child(self.status_pdf_grid_btn(pdf_grid_on, cx));
        }
        if readonly_preview {
            right = right.child(self.status_item("status-mode-readonly", "只读", chip_text));
        }
        // 主题三态：跟随系统主题 / 浅色 / 深色。
        let theme_label = match self.model.borrow().theme_pref {
            config::user::ThemePref::Follow => "跟随系统主题",
            config::user::ThemePref::Light => "浅色",
            config::user::ThemePref::Dark => "深色",
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

        // 音乐指示（编辑器模式且 MusicView 存在、当前有曲目时）：♪ 歌名 + 播放/暂停符号，
        // 点击切 播放/暂停（不切音乐模式）。
        if !self.model.borrow().music_mode {
            let music_hint = self.music.as_ref().map(|mv| {
                let pl = mv.read(cx).player.read(cx);
                (pl.current_track().map(|t| t.title.clone()), pl.playing())
            });
            if let Some((Some(title), playing)) = music_hint
                && !title.is_empty()
            {
                right = right.child(self.status_music_btn(title, playing, cx));
            }
        }

        right
    }

    /// 状态栏右侧「音乐」指示钮：`♪ 歌名` + 播放/暂停符号；按下即切 播放/暂停
    ///（不切音乐模式，也不进入音乐界面）。
    fn status_music_btn(
        &self,
        title: String,
        playing: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        ui::Pill::new("status-music-btn")
            .size(ui::PillSize::Md)
            .tone(ui::PillTone::Chip)
            // Md 默认 22px 高/圆角 md 与状态栏壳不匹配，覆写回 19px + 圆角 sm（1:1 保留原壳）。
            .h(px(status_pill_h()))
            .rounded_sm()
            .label(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .child(if playing { "⏸ " } else { "▶ " })
                    .child(format!("♪ {title}")),
            )
            .on_mouse_down(cx.listener(|this: &mut EditorApp, _ev, _w, cx| {
                if let Some(mv) = &this.music {
                    mv.update(cx, |v, cx| v.toggle_play(cx));
                }
            }))
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

    /// 状态栏右侧「全屏/还原」切换按钮（仅开启预览且非只读预览时挂载）；按下即切换。
    fn status_fullscreen_btn(&self, fullscreen: bool, cx: &mut Context<Self>) -> impl IntoElement {
        ui::Pill::new("status-fullscreen-btn")
            .size(ui::PillSize::Md)
            .tone(ui::PillTone::Chip)
            // Md 默认 22px 高/圆角 md 与状态栏壳不匹配，覆写回 19px + 圆角 sm（1:1 保留原壳）。
            .h(px(status_pill_h()))
            .rounded_sm()
            .label(if fullscreen { "还原" } else { "全屏" })
            .on_mouse_down(cx.listener(|this: &mut EditorApp, _ev, window, cx| {
                this.toggle_preview_fullscreen(window, cx);
            }))
    }

    /// 状态栏右侧「换行/不换行」切换按钮（仅非只读预览且非 Markdown 时挂载）；按下即切换。
    fn status_wrap_btn(&self, wrap_on: bool, cx: &mut Context<Self>) -> impl IntoElement {
        ui::Pill::new("status-wrap-btn")
            .size(ui::PillSize::Md)
            .tone(ui::PillTone::Chip)
            // Md 默认 22px 高/圆角 md 与状态栏壳不匹配，覆写回 19px + 圆角 sm（1:1 保留原壳）。
            .h(px(status_pill_h()))
            .rounded_sm()
            .label(if wrap_on { "换行" } else { "不换行" })
            .on_mouse_down(cx.listener(|this: &mut EditorApp, _ev, window, cx| {
                this.toggle_wrap(window, cx);
            }))
    }

    /// 状态栏右侧「页面网格视图」切换按钮（wave4，仅激活选项卡为 PDF 阅读器时挂载，
    /// 置于「只读」状态项左侧）：在「单列阅读 ↔ 页面网格总览」间切换。两态视觉：
    /// - 关态（grid_on=false）：纯文字可点（对齐 `status_item` 样式，**无底色高亮**）；
    /// - 开态（grid_on=true）：高亮 pill（`ui::Pill` Chip 壳 + `selected_bg` 选中底色）。
///
    /// 按下即从激活选项卡取出 PdfViewer 实体（随选项卡搬迁即随动，无需 shell 额外状态）
    /// 调 `toggle_grid`——内部已 `notify`，订阅（`subscribe_pdf`）驱动外壳同帧重绘刷新按钮态。
    /// 颜色一律走 `ui::theme` 禁裸色：开态底色 `selected_bg` 随亮暗主题，关态文字走 `chip_text`。
    fn status_pdf_grid_btn(&self, grid_on: bool, cx: &mut Context<Self>) -> gpui::AnyElement {
        let active_bg: gpui::Hsla = ui::theme::Theme::global(cx).colors.selected_bg.into();
        let toggle = self.status_pdf_grid_toggle(cx);

        // 两态共用**同一 Pill 壳**（同 19px 高 / 同内边距 / 同圆角）→ 几何恒定，仅底色与
        // 文字色翻转——与「双页」钮同姿势（消除 纯文字 div ↔ Pill 切换的抖动）。
        let (bg, fg): (gpui::Hsla, gpui::Hsla) = if grid_on {
            (active_bg, ui::theme::ChromeColors::for_mode(ui::theme::Theme::global(cx).is_dark()).pill_fg.into())
        } else {
            (gpui::transparent_black(), self.chip_text())
        };
        ui::Pill::new("status-pdf-grid-btn")
            .size(ui::PillSize::Md)
            .tone(ui::PillTone::Chip)
            // Md 默认 22px 高/圆角 md 与状态栏壳不匹配，覆写回 19px + 圆角 sm（1:1 保留原壳）。
            .h(px(status_pill_h()))
            .rounded_sm()
            .bg(bg)
            .text_color(fg)
            .label("▦ 网格")
            .on_mouse_down(toggle)
            .into_any_element()
    }

    /// 「页面网格」按钮的点击闭包：从激活选项卡取 PdfViewer 实体并翻转网格
    /// （闭包内 short borrow 即释放；选项卡已被关闭 → 取不到 → 直接返回）。
    fn status_pdf_grid_toggle(
        &self,
        cx: &mut Context<Self>,
    ) -> impl Fn(&gpui::MouseDownEvent, &mut gpui::Window, &mut gpui::App) + 'static {
        cx.listener(|this: &mut EditorApp, _ev: &gpui::MouseDownEvent, _w, cx| {
            let Some(viewer) = (|| {
                let model = this.model.borrow();
                let tab = model.tabs.get(model.active_tab)?;
                match &tab.payload.reader {
                    MediaReader::Pdf(pv) => Some(pv.clone()),
                    _ => None,
                }
            })() else {
                return;
            };
            viewer.update(cx, |v, cx| v.toggle_grid(cx));
        })
    }

    /// 状态栏右侧「双页」页排模式切换按钮（仅激活选项卡为 PDF 阅读器时挂载，
    /// 置于「网格」钮左侧）：两态视觉复刻 `status_pdf_grid_btn`——未选中 = 纯文字可点
    /// （`chip_text`、无底色），选中 = 高亮 pill（`selected_bg` 选中底色）。选中态 =
    /// 当前阅读页排模式 `page_mode` 与目标一致（页排与网格总览正交：网格进出保留
    /// `page_mode`，故网格里选中态依然成立）。按下取激活选项卡 PdfViewer 实体调
    /// `set_page_mode`（内部已处理「网格中先退出网格再落新模式」）。
    fn status_pdf_mode_btn(
        &self,
        sel: &'static str,
        label: &'static str,
        selected: bool,
        grid_on: bool,
        current: PageMode,
        target: PageMode,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let active_bg: gpui::Hsla = ui::theme::Theme::global(cx).colors.selected_bg.into();
        // 图标颜色跟随文字色（未选中 muted chip_text / 选中 pill_fg），两态图标文字同色不跳。
        let theme = ui::theme::Theme::global(cx);
        let chrome = ui::theme::ChromeColors::for_mode(theme.is_dark());
        let pill_fg: gpui::Hsla = chrome.pill_fg.into();
        let on_click = self.status_pdf_mode_toggle(grid_on, current, target, cx);

        // 两态共用**同一 Pill 壳**（同 19px 高 / 同 px_2 内边距 / 同 rounded_sm）→ 几何恒定，
        // 仅底色与文字色翻转（未选中 = 透明底，选中 = selected_bg 高亮）——消除「纯文字 div
        // ↔ Pill」切换的高度/内边距差导致的左右抖动（2026-11-20 用户反馈「点击双页会左移」）。
        // 图标 = 自绘 SVG（`BookOpen` 对开双页，Fluent 线性描边，无字体缺字风险）。
        let (bg, fg): (gpui::Hsla, gpui::Hsla) = if selected {
            (active_bg, pill_fg)
        } else {
            (gpui::transparent_black(), self.chip_text())
        };
        ui::Pill::new(sel)
            .size(ui::PillSize::Md)
            .tone(ui::PillTone::Chip)
            // Md 默认 22px 高/圆角 md 与状态栏壳不匹配，覆写回 19px + 圆角 sm（1:1 保留原壳）。
            .h(px(status_pill_h()))
            .rounded_sm()
            .bg(bg)
            .text_color(fg)
            .label(
                div()
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .child(
                        ui::icon::Icon::new(ui::icon::IconName::BookOpen)
                            .size(px(13.))
                            .text_color(fg),
                    )
                    .child(label),
            )
            .on_mouse_down(on_click)
            .into_any_element()
    }

    /// 「双页」按钮点击闭包：从激活选项卡取 PdfViewer 实体，落页排模式或弹起回连续。
    /// （闭包内 short borrow 即释放；选项卡已被关闭 → 取不到 → 直接返回。）
    fn status_pdf_mode_toggle(
        &self,
        grid_on: bool,
        current: PageMode,
        target: PageMode,
        cx: &mut Context<Self>,
    ) -> impl Fn(&gpui::MouseDownEvent, &mut gpui::Window, &mut gpui::App) + 'static {
        cx.listener(
            move |this: &mut EditorApp, _ev: &gpui::MouseDownEvent, _w, cx| {
                let Some(viewer) = (|| {
                    let model = this.model.borrow();
                    let tab = model.tabs.get(model.active_tab)?;
                    match &tab.payload.reader {
                        MediaReader::Pdf(pv) => Some(pv.clone()),
                        _ => None,
                    }
                })() else {
                    return;
                };
                // 当前已是目标模式且非网格 → 弹起回连续；否则落新页排模式（`set_page_mode`
                // 内部已处理「网格中先退出网格再落新模式」）。
                let next = if !grid_on && current == target {
                    PageMode::Continuous
                } else {
                    target
                };
                viewer.update(cx, |v, cx| v.set_page_mode(next, cx));
            },
        )
    }

    /// 状态栏左侧「显示比例」滑条（PDF 阅读模式，`ui::Slider`，2026-11-20 从右区移入左区）：以
    /// `[config::pdf::ZOOM_MIN, ZOOM_MAX]` 把当前缩放 `zoom` 归一化为显示值（与
    /// Ctrl+滚轮共享同一 zoom 源 → 天然双向同步、中间区 zoom% 同源），`Immediate`
    /// 每档 `set_zoom_abs`（viewer worker 防抖自担）。仅阅读模式挂载（网格总览隐藏：
    /// 网格有独立 grid_zoom，滑条只控单列阅读缩放）。颜色一律走 `ui::theme`（Slider
    /// 默认轨道 border / 填充 accent，禁硬编码）。
    fn status_pdf_zoom_slider(&self, zoom: f32, cx: &mut Context<Self>) -> gpui::Div {
        let frac = ((zoom - config::pdf::ZOOM_MIN)
            / (config::pdf::ZOOM_MAX - config::pdf::ZOOM_MIN))
            .clamp(0.0, 1.0);
        let owned = "status-pdf-zoom-slider".to_string();
        div()
            .debug_selector(move || owned.clone())
            .w(px(72.))
            .h(px(16.))
            .child(
                ui::Slider::new("status-pdf-zoom-slider")
                    .value(frac)
                    .commit_mode(ui::CommitMode::Immediate)
                    .track_height(px(4.))
                    .on_change(self.status_pdf_zoom_setter(cx))
                    .on_change_during_drag(self.status_pdf_zoom_setter(cx)),
            )
    }

    /// 「显示比例」滑条值变更闭包：滑条分数 → `[ZOOM_MIN, ZOOM_MAX]` 绝对缩放并
    /// `set_zoom_abs`（同目标早退由 viewer 内部兜底）。经 `cx.entity()` 捕获宿主实体，
    /// 每次取值从激活选项卡现取 PdfViewer（随选项卡搬迁即随动）。
    fn status_pdf_zoom_setter(
        &self,
        cx: &mut Context<Self>,
    ) -> impl Fn(f32, &mut gpui::Window, &mut gpui::App) + 'static {
        let entity = cx.entity();
        move |f, _w, app| {
            let Some(viewer) = (|| {
                let model = entity.read(app).model.borrow();
                let tab = model.tabs.get(model.active_tab)?;
                match &tab.payload.reader {
                    MediaReader::Pdf(pv) => Some(pv.clone()),
                    _ => None,
                }
            })() else {
                return;
            };
            let zoom = config::pdf::ZOOM_MIN + f * (config::pdf::ZOOM_MAX - config::pdf::ZOOM_MIN);
            viewer.update(app, |v, cx| v.set_zoom_abs(zoom, cx));
        }
    }
}
