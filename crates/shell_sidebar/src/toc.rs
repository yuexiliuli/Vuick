//! 侧边栏「目录（TOC）」视图。
//!
//! 当前文档标题大纲（Markdown 标题 / PDF 书签）的折叠 / 自适应宽 / 虚拟化渲染与点击
//! 跳转。全部数据（模型 / WyEditor / PdfViewer 实体）经共享 `Rc<RefCell<AppModel>>` 域内
//! 直取；仅宿主域外的数据（PDF 书签——`normalize_key` 归属在 shell 的 `pdf_bookmarks`）
//! 由宿主渲染期预取、以 `bookmarks` 参数传入（域渲染期禁再入宿主，见 [`crate::host`]）。
//! 事件回调（折叠 / 书签删除）经 [`crate::host::SidebarHost`] 反调宿主办状态迁移。

use gpui::{
    div, px, AnimationExt, App, Context, InteractiveElement, IntoElement, ParentElement,
    SharedString, StatefulInteractiveElement, Styled, Window,
};
use shell_core::{MdTocScroll, MediaReader, PdfBookmark};
use ui::motion::hover_spring;

use crate::chrome::Sidebar;
use crate::host::SidebarHost;
use crate::prompt::FileTreePromptHost;
use crate::state::{SidebarChromeState, toc_collapse, FILE_TREE_INDENT};
use ::toc; // 显式导入外部 crate `toc`（本子模块名与之同名，需显式绑定避免 glob 遮蔽）

/// 目录条目总数超过此值才启用列表虚拟化（普通小文档仍全量渲染，行为不变）。
/// `pub`：shell 仍留的 `files.rs`（文件树虚拟化同口径）跨 crate 引用。
pub const TOC_VIRTUALIZE_MIN: usize = 200;
/// 可视带外上下各缓冲的条目行数（提前物化，滚动不闪烁）。
/// `pub`：shell 仍留的 `files.rs`（文件树虚拟化同口径）跨 crate 引用。
pub const TOC_BAND_BUFFER: usize = 8;

impl<H: SidebarHost + FileTreePromptHost> Sidebar<H> {
    /// 当前激活选项卡的 Markdown TOC 数据源 + 跳转目标（A4，经模型直取，与宿主
    /// `EditorApp::md_toc_source` 同口径）：MD / EPUB 统一预览都走 WyEditor 只读宿主
    /// （条目 = 源码行，点击滚动统一组件）；无 WyEditor → `None`。
    fn md_toc_source(&self, cx: &gpui::App) -> Option<(markdown_wysiwyg::TocEntries, MdTocScroll)> {
        let model = self.model.borrow();
        let active = model.active_tab;
        let tab = model.tabs.get(active)?;
        let wy = tab.payload.md_wy.as_ref()?;
        Some((wy.read(cx).toc_entries(), MdTocScroll::Unified(wy.clone())))
    }

    /// 目录条目 `idx` 是否有下级（可折叠；P4-2：MD/EPUB 目录统一来自 WyEditor 只读宿主）。
    fn toc_has_child(&self, idx: usize, cx: &gpui::App) -> bool {
        match self.model.borrow().tabs.get(self.model.borrow().active_tab) {
            Some(tab) => match &tab.payload.reader {
                MediaReader::Pdf(pv) => {
                    let outlines = pv.read(cx).outlines().to_vec();
                    let levels: Vec<usize> = outlines.iter().map(|e| e.level).collect();
                    toc::toc_node_has_child(&levels, idx)
                }
                _ => {
                    let levels: Vec<usize> = self
                        .md_toc_source(cx)
                        .map(|(entries, _)| entries.iter().map(|&(_, l, _)| l as usize).collect())
                        .unwrap_or_default();
                    toc::toc_node_has_child(&levels, idx)
                }
            },
            None => false,
        }
    }

    /// 折叠/展开某个目录父项（仅「有下级」的条目可折叠）。宿主 `app.toggle_sidebar_toc_collapse`
    /// 测试入口的实体侧实现：域内算 `has_child` → 状态纯变换。返回变换是否实际生效
    /// （`false` = 无子级提前返回，与 HEAD 语义一致）；**返回权交给宿主 forwarder 决定是否
    /// notify**（实体经 host.update 再入宿主会在宿主已持 `&mut` 的 `app.update` 闭包内
    /// RefCell 双借 panic，同 chrome 注释）。
    pub fn toggle_sidebar_toc_collapse(
        &mut self,
        sb: &mut SidebarChromeState,
        idx: usize,
        cx: &mut Context<Self>,
    ) -> bool {
        let has_child = self.toc_has_child(idx, cx);
        toc_collapse(sb, idx, has_child)
    }

    /// 目录条目标折叠箭头的点击回调（实体 listener → 无 `&mut SidebarChromeState`）：
    /// 域内算 `has_child` → 经宿主 `sidebar_toc_collapse` 落地状态迁移。
    fn toc_collapse_click(&mut self, idx: usize, cx: &mut App) {
        let has_child = self.toc_has_child(idx, cx);
        self.host_call(cx, |app, cx| app.sidebar_toc_collapse(idx, has_child, cx));
    }

    /// 估算目录文字宽度（px）：CJK 取全宽、ASCII 取 0.6 倍字号。
    fn toc_text_width(text: &str, font_px: f32) -> f32 {
        text.chars()
            .map(|c| font_px * if c.is_ascii() { 0.6 } else { 1.0 })
            .sum()
    }

    /// 目录条目行所需宽度（px）：起始缩进 + 层级缩进 + 箭头列 + 圆点 + 标题 + 右侧留白。
    fn toc_row_width(level: usize, title: &str, font_px: f32) -> f32 {
        Self::toc_text_width(title, font_px)
            + 6.0              // pl 起始缩进
            + level as f32 * FILE_TREE_INDENT
            + 16.0             // 箭头列（无箭头也占位对齐）
            + 6.0 + 5.0 + 6.0  // gap + 圆点 + gap
            + 8.0              // pr
    }

    /// 计算当前激活文档目录所需的最大行宽（px）；无文档/无标题返回 `None`。
    /// `bookmarks` = 宿主预取的当前 PDF 路径书签（仅 PDF 分支用；MD 传 `&None`）。
    fn compute_sidebar_toc_width(
        &self,
        bookmarks: &Option<Vec<PdfBookmark>>,
        cx: &gpui::App,
    ) -> Option<f32> {
        let model = self.model.borrow();
        let tab = model.tabs.get(model.active_tab)?;
        let font_px = 13.0f32;
        // 右侧页码列宽（PDF 书签有页码，Markdown 无）：预留以免标题被截断
        let page_col_w = 34.0f32;
        let max_row = if let MediaReader::Pdf(pdf) = &tab.payload.reader {
            let outlines = pdf.read(cx).outlines();
            let mut w = outlines
                .iter()
                .map(|e| Self::toc_row_width(e.level, &e.title, font_px) + page_col_w)
                .fold(0.0f32, f32::max);
            // Phase B3：书签区段行宽（标题 + 页码 + 删除 × 的额外列）。
            if let Some(bms) = bookmarks.as_ref() {
                w = bms
                    .iter()
                    .map(|b| Self::toc_row_width(0, &b.title, font_px) + page_col_w + 22.0)
                    .fold(w, f32::max);
            }
            w
        } else {
            // P4-2：MD / EPUB 目录都来自统一 WyEditor 只读宿主。
            let entries = self
                .md_toc_source(cx)
                .map(|(e, _)| e)
                .unwrap_or_default();
            entries
                .iter()
                .map(|&(_, l, ref t)| Self::toc_row_width(l as usize, t, font_px))
                .fold(0.0f32, f32::max)
        };
        (max_row > 0.0).then_some(max_row)
    }

    /// 目录视图顶栏：「目录」标题。开关交给左侧竖列「目录」按钮（再点一次收拢侧边栏）。
    fn render_sidebar_toc_header(&mut self) -> impl IntoElement {
        let fg = self.text_primary();
        let border = self.bar_border();
        crate::header::sidebar_panel_header(
            "sidebar-toc-header",
            "目录",
            fg,
            border,
            None::<gpui::Div>,
        )
    }

    /// 「目录」视图：当前文档的标题大纲（Markdown 标题 / PDF 书签），带面板头
    /// （「目录」+ 关闭）与页码，条目样式统一来自公共 `toc` crate（可折叠树）。
    /// `bookmarks` = 宿主渲染期预取的当前 PDF 路径书签（域渲染期禁再入宿主）。
    pub fn render_sidebar_toc(
        &mut self,
        sb: &mut SidebarChromeState,
        bookmarks: Option<Vec<PdfBookmark>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let (fg, muted) = (self.text_primary(), self.text_minor());
        let active_bg = self.tab_active_bg();
        let accent = ui::theme::Theme::global(cx).colors.accent.into();

        // 折叠状态按文档缓存：切换文档即清空，并重新按新文档内容自适应宽度
        let active = self.model.borrow().active_tab;
        if sb.toc_tab != Some(active) {
            sb.toc_tab = Some(active);
            sb.toc_collapsed.clear();
            sb.width_auto = true;
            sb.toc_width = None;
            sb.toc_cache = None;
        }

        // 目录视图自适应宽：按当前文档最宽目录条目估算（未手动拖拽时）。
        // 按 (选项卡, 编辑版本, WyEditor 内容代号) 缓存：内容没变只算一次，禁逐帧 O(N) 全量测宽。
        if sb.width_auto {
            let model = self.model.borrow();
            let tab = model.tabs.get(active);
            let ver = tab.map(|t| t.payload.edit_version).unwrap_or(0);
            let cg = tab
                .and_then(|t| t.payload.md_wy.as_ref())
                .map(|wy| wy.read(cx).content_gen())
                .unwrap_or(0);
            let cached = sb
                .toc_width
                .as_ref()
                .is_some_and(|(ti, v, g, _)| *ti == active && *v == ver && *g == cg);
            if !cached
                && let Some(w) = self.compute_sidebar_toc_width(&bookmarks, cx)
            {
                sb.toc_width = Some((active, ver, cg, w));
                sb.width = (config::layout::SIDEBAR_ACTIVITY_W + w + 12.0)
                    .clamp(config::layout::SIDEBAR_WIDTH_MIN, config::layout::SIDEBAR_WIDTH_MAX);
            }
        }

        let inner: gpui::AnyElement =
            match self.model.borrow().tabs.get(self.model.borrow().active_tab) {
                Some(tab) => {
                    let collapsed = sb.toc_collapsed.clone();
                    // PDF 书签目录 + Phase B3 「书签」区段（阅读位置书签：点击跳页、× 删除）
                    if let MediaReader::Pdf(pdf) = &tab.payload.reader {
                        let outlines = pdf.read(cx).outlines().to_vec();
                        let mut blocks: Vec<gpui::AnyElement> = Vec::new();
                        if outlines.is_empty() {
                            blocks.push(
                                ui::empty_state::compact_empty_state(
                                    "sidebar-toc-empty",
                                    ui::icon::IconName::File,
                                    "当前 PDF 无书签目录",
                                    cx,
                                )
                                .into_any_element(),
                            );
                        } else {
                            let rows = self.sidebar_toc_pdf_rows(
                                window, &outlines, &collapsed, fg, muted, active_bg, accent,
                                pdf.clone(), cx,
                            );
                            blocks.push(
                                div()
                                    .flex()
                                    .flex_col()
                                    .py_1()
                                    .children(rows)
                                    .into_any_element(),
                            );
                        }
                        // 书签区段：路径关联持久化（宿主）；无书签不占版面。
                        let bm_path = pdf.read(cx).path().to_path_buf();
                        if let Some(bms) = bookmarks.as_ref()
                            && !bms.is_empty()
                        {
                            blocks.push(
                                div()
                                    .id("sidebar-toc-bookmarks")
                                    .debug_selector(|| "sidebar-toc-bookmarks".into())
                                    .flex()
                                    .items_center()
                                    .px(px(8.))
                                    .h(px(26.))
                                    .text_size(px(11.))
                                    .text_color(muted)
                                    .child("书签")
                                    .into_any_element(),
                            );
                            let rows = self.sidebar_toc_bookmark_rows(
                                window, &bm_path, bms, fg, muted, active_bg, accent, pdf.clone(),
                                cx,
                            );
                            blocks.push(
                                div()
                                    .flex()
                                    .flex_col()
                                    .pb_1()
                                    .children(rows)
                                    .into_any_element(),
                            );
                        }
                        div().flex().flex_col().children(blocks).into_any_element()
                    } else {
                        // Markdown 标题（P4-2：MD / EPUB 目录都来自统一 WyEditor 宿主）。
                        let model = self.model.borrow();
                        let tab = model.tabs.get(active);
                        let ver = tab.map(|t| t.payload.edit_version).unwrap_or(0);
                        let wy = tab.and_then(|t| t.payload.md_wy.as_ref());
                        let cg = wy.map(|wy| wy.read(cx).content_gen()).unwrap_or(0);
                        // 条目 + 可见性 + 跳转目标按 (选项卡, 编辑版本, WyEditor 宿主 + 内容代号)
                        // 缓存：内容/折叠没变逐帧 O(1) 复用，大文档不再每帧全量克隆条目/重扫。
                        // 命中需宿主同一 + 内容代号同一：预览关/重装（md_wy 换实体）与整篇换源
                        //（同实体 in-place 喂料，edit_version 不变）都作废重建，不渲染上一版条目。
                        let cache_ok = match &sb.toc_cache {
                            Some((ti, v, g, _, _, scroll))
                                if *ti == active && *v == ver && *g == cg =>
                            {
                                match scroll {
                                    MdTocScroll::Unified(cached_wy) => wy == Some(cached_wy),
                                }
                            }
                            _ => false,
                        };
                        if !cache_ok {
                            // 内容变更/切换文档 → 重建条目 + 可见性表（一次性，逐帧 O(1) 复用）；
                            // 无 wy / 空目录 → 显式清缓存落空态分支，避免渲染到上一版条目。
                            match self.md_toc_source(cx) {
                                Some((entries, scroll)) if !entries.is_empty() => {
                                    let levels: Vec<usize> =
                                        entries.iter().map(|&(_, l, _)| l as usize).collect();
                                    let vis = toc::toc_visibility(&levels, &collapsed);
                                    let visible: Vec<usize> = vis
                                        .iter()
                                        .enumerate()
                                        .filter_map(|(i, &v)| v.then_some(i))
                                        .collect();
                                    sb.toc_cache =
                                        Some((active, ver, cg, entries, visible, scroll));
                                }
                                _ => sb.toc_cache = None,
                            }
                        }
                        match &sb.toc_cache {
                            Some((_, _, _, entries, visible, scroll)) if !entries.is_empty() => {
                                let total = visible.len();
                                // 虚拟化：只物化可视带 + 上下 M 行缓冲；带外以等高 spacer 占位
                                //（总高恒为 visible × 行高，滚动量程/命中稳定）。普通小文档全量。
                                let vhand = &sb.toc_vscroll;
                                let offset = (-vhand.offset().y).max(px(0.)).as_f32();
                                let viewport_h = vhand.bounds().size.height.as_f32();
                                let (start, end, top_h, bottom_h) =
                                    crate::virtualize::virtualized_band(
                                        total, offset, viewport_h, toc::TOC_ROW_H,
                                        TOC_VIRTUALIZE_MIN, TOC_BAND_BUFFER,
                                    );
                                let rows = self.sidebar_toc_md_rows(
                                    window, entries, &visible[start..end], &collapsed, fg, muted,
                                    active_bg, accent, scroll.clone(), cx,
                                );
                                div()
                                    .flex()
                                    .flex_col()
                                    .py_1()
                                    .child(if top_h > 0.0 {
                                        div().h(px(top_h)).into_any_element()
                                    } else {
                                        div().into_any_element()
                                    })
                                    .children(rows)
                                    .child(if bottom_h > 0.0 {
                                        div().h(px(bottom_h)).into_any_element()
                                    } else {
                                        div().into_any_element()
                                    })
                                    .into_any_element()
                            }
                            _ => ui::empty_state::compact_empty_state(
                                "sidebar-toc-empty",
                                ui::icon::IconName::BookOpen,
                                "当前文件无目录（无标题）",
                                cx,
                            ),
                        }
                    }
                }
                None => ui::empty_state::compact_empty_state(
                    "sidebar-toc-empty",
                    ui::icon::IconName::File,
                    "当前无打开文档",
                    cx,
                ),
            };

        div()
            .id("sidebar-toc")
            .debug_selector(|| "sidebar-toc".into())
            .flex_1()
            // 允许面板缩到比最宽条目还窄（否则 min-width:auto 拒绝缩窄，拖窄后
            // 面板溢出侧边栏右缘，条目 hover/点击仍响应编辑器区）
            .min_w(px(0.))
            .min_h(px(0.))
            .overflow_hidden()
            .flex()
            .flex_col()
            .child(self.render_sidebar_toc_header())
            // 滚动条只覆盖面板内容区（不含头部），故把滚动容器 + 滚动条叠层包进
            // `relative` 包装层（对齐 panes/editor：叠层为滚动容器的兄弟、`absolute inset_0`）。
            // 结构/样式收敛到 ui::scroll_panel::ScrollPanelBuilder：三层 id 显式覆盖为
            // sidebar 既有实测 id（派生 `{id}-vscrollbar` 与之不一致），目录容器沿用 `py_1`。
            .child(
                ui::scroll_panel::ScrollPanelBuilder::new(
                    "sidebar-toc-scroll",
                    sb.toc_vscroll.clone(),
                )
                .wrap_id("sidebar-toc-scroll-wrap")
                .bar_id("sidebar-toc-vscrollbar")
                .container_style(|d: gpui::Div| d.py_1())
                .build(inner),
            )
            // P2-10：面板开合轻淡入（宽度动画会让「自适应宽 + 布局断言」测试抖动，
            // 改为只动绘制、不碰布局的透明度淡入）。
            .with_spring(
                "sidebar-toc-fade",
                hover_spring(1.0).from(0.0),
                |this, v| this.opacity(v),
            )
            .into_any_element()
    }

    /// PDF 书签目录条目（可折叠树；可见性按当前折叠集合过滤，点击跳转页码）。
    #[allow(clippy::too_many_arguments)] // 四色 + 数据源 + 折叠集，与 toc_item_frame 同构
    fn sidebar_toc_pdf_rows(
        &self,
        window: &mut Window,
        outlines: &[pdf::PdfOutlineEntry],
        collapsed: &std::collections::HashSet<usize>,
        fg: gpui::Hsla,
        muted: gpui::Hsla,
        active_bg: gpui::Hsla,
        accent: gpui::Hsla,
        pdf: gpui::Entity<pdf::PdfViewer>,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let levels: Vec<usize> = outlines.iter().map(|e| e.level).collect();
        let visible = toc::toc_visibility(&levels, collapsed);
        let mut rows: Vec<gpui::AnyElement> = Vec::new();
        for (i, e) in outlines.iter().enumerate() {
            if !visible[i] {
                continue;
            }
            let page = e.page_index;
            let wk = pdf.clone();
            let has_child = toc::toc_node_has_child(&levels, i);
            let is_collapsed = has_child && collapsed.contains(&i);
            let idx = i;
            // 折叠回调先物化（与下方 `window, cx` 参数不可同表达式借用 cx）。
            let toggle_collapse = cx.listener(move |this: &mut Self, _ev, _w, cx| {
                cx.stop_propagation();
                this.toc_collapse_click(idx, cx);
            });
            rows.push(toc::toc_item_frame(
                format!("sidebar-toc-pdf-{i}"),
                format!("sidebar-toc-caret-{i}"),
                e.level,
                e.title.clone(),
                has_child,
                is_collapsed,
                e.level == 0,
                (page != usize::MAX).then(|| (page + 1).to_string()),
                fg,
                muted,
                active_bg,
                accent,
                window,
                cx,
                move |_, _w, cx| {
                    if page != usize::MAX {
                        wk.update(cx, |v, cx| {
                            v.scroll_to_page(page);
                            cx.notify();
                        });
                    }
                },
                toggle_collapse,
            ));
        }
        rows
    }

    /// Phase B3：PDF「书签」区段条目（阅读位置书签：页码 + 标题）。点击 → 跳页
    /// （`scroll_to_page`）；右侧小「×」→ 删除该条（`stop_propagation` 防误触跳转，
    /// 对照 toc caret 折叠防误点同款姿势）。
    #[allow(clippy::too_many_arguments)] // 四色 + 数据源 + 折叠集，与 toc_item_frame 同构
    fn sidebar_toc_bookmark_rows(
        &self,
        window: &mut Window,
        path: &std::path::Path,
        bms: &[PdfBookmark],
        fg: gpui::Hsla,
        muted: gpui::Hsla,
        hover_bg: gpui::Hsla,
        accent: gpui::Hsla,
        pdf: gpui::Entity<pdf::PdfViewer>,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let mut rows: Vec<gpui::AnyElement> = Vec::with_capacity(bms.len());
        for (i, bm) in bms.iter().enumerate() {
            let page_index = bm.page_index;
            let title = bm.title.clone();
            let wk = pdf.clone();
            let del_path = path.to_path_buf();
            let del_id = format!("sidebar-toc-bookmark-del-{i}");
            // 删除钮（小 ×）：stop_propagation 防触发整行跳页。
            let del = div()
                .id(SharedString::from(del_id.clone()))
                .debug_selector(move || del_id.clone())
                .w(px(18.))
                .h(px(18.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_sm()
                .text_size(px(12.))
                .text_color(muted)
                .hover(|style| style.bg(hover_bg))
                .active(|style| style.bg(hover_bg.opacity(0.8)))
                .cursor(gpui::CursorStyle::PointingHand)
                .on_click(cx.listener(
                    move |this: &mut Self, _ev: &gpui::ClickEvent, _w, cx| {
                        cx.stop_propagation();
                        this.host_call(cx, |app, cx| {
                            app.sidebar_remove_pdf_bookmark(&del_path, i, cx)
                        });
                    },
                ))
                .child("×");
            let row_id = format!("sidebar-toc-bookmark-{i}");
            let row_id2 = row_id.clone();
            let row = div()
                .id(SharedString::from(row_id))
                .debug_selector(move || row_id2.clone())
                .h(px(28.))
                .w_full()
                .flex()
                .items_center()
                .gap(px(6.))
                .pl(px(10.))
                .pr(px(6.))
                .rounded(px(4.))
                .hover(|style| style.bg(hover_bg))
                .cursor(gpui::CursorStyle::PointingHand)
                .on_click(cx.listener(
                    move |_: &mut Self, _ev: &gpui::ClickEvent, _w, cx| {
                        wk.update(cx, |v, cx| {
                            v.scroll_to_page(page_index);
                            cx.notify();
                        });
                    },
                ))
                .child(
                    div()
                        .w(px(5.))
                        .h(px(5.))
                        .rounded_full()
                        .flex_shrink_0()
                        .bg(accent),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_overflow(gpui::TextOverflow::Truncate("…".into()))
                        .text_size(px(12.5))
                        .text_color(fg)
                        .child(title),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .pl_2()
                        .text_size(px(10.5))
                        .text_color(muted)
                        .child((page_index + 1).to_string()),
                )
                .child(del);
            rows.push(row.into_any_element());
        }
        let _ = window;
        rows
    }

    /// Markdown 标题目录条目（可折叠树；可见性按当前折叠集合过滤）。
    /// **虚拟化**：`visible` = 调用方算好的「待物化条目下标」（可视带切片，非全量），
    /// 只物化带内条目，大文档不再每帧全量实例化万级元素。
    /// P4-2：`scroll` 跳转目标 = 右栏 WyEditor 只读预览（源码行）`scroll_to_line`。
    #[allow(clippy::too_many_arguments)] // 四色 + 数据源 + 折叠集，与 toc_item_frame 同构
    fn sidebar_toc_md_rows(
        &self,
        window: &mut Window,
        entries: &[(usize, u8, SharedString)],
        visible: &[usize],
        collapsed: &std::collections::HashSet<usize>,
        fg: gpui::Hsla,
        muted: gpui::Hsla,
        active_bg: gpui::Hsla,
        accent: gpui::Hsla,
        scroll: MdTocScroll,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let levels: Vec<usize> = entries.iter().map(|&(_, l, _)| l as usize).collect();
        let mut rows: Vec<gpui::AnyElement> = Vec::with_capacity(visible.len());
        for &i in visible {
            let (tgt, lvl, ref text) = entries[i];
            let sc = scroll.clone();
            let has_child = toc::toc_node_has_child(&levels, i);
            let is_collapsed = has_child && collapsed.contains(&i);
            let idx = i;
            // 折叠回调先物化（与下方 `window, cx` 参数不可同表达式借用 cx）。
            let toggle_collapse = cx.listener(move |this: &mut Self, _ev, _w, cx| {
                cx.stop_propagation();
                this.toc_collapse_click(idx, cx);
            });
            rows.push(toc::toc_item_frame(
                format!("sidebar-toc-md-{i}"),
                format!("sidebar-toc-caret-{i}"),
                lvl as usize,
                text.to_string(),
                has_child,
                is_collapsed,
                lvl == 1,
                None,
                fg,
                muted,
                active_bg,
                accent,
                window,
                cx,
                move |_, window, cx| match &sc {
                    MdTocScroll::Unified(wy) => {
                        wy.update(cx, |v, cx| {
                            v.scroll_to_line(tgt, window);
                            cx.notify();
                        });
                    }
                },
                toggle_collapse,
            ));
        }
        rows
    }
}
