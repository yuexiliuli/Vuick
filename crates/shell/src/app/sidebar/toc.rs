//! 侧边栏「目录（TOC）」子模块：当前文档标题大纲（Markdown 标题 / PDF 书签）的
//! 折叠 / 自适应宽 / 虚拟化渲染与点击跳转。

use super::*;
use ::toc; // 显式导入外部 crate `toc`（本子模块名与之同名，需显式绑定避免 glob 遮蔽）

impl EditorApp {
    /// 折叠/展开某个目录父项（仅「有下级」的条目可折叠）。
    pub(crate) fn toggle_sidebar_toc_collapse(&mut self, idx: usize, cx: &mut Context<Self>) {
        let has_child = match self.tabs.get(self.active_tab) {
            Some(tab) => match &tab.reader {
                MediaReader::Pdf(pv) => {
                    let outlines = pv.read(cx).outlines().to_vec();
                    let levels: Vec<usize> = outlines.iter().map(|e| e.level).collect();
                    toc::toc_node_has_child(&levels, idx)
                }
                _ => {
                    // P4-2：MD / EPUB 目录都来自统一 WyEditor 只读宿主。
                    let levels: Vec<usize> = self
                        .md_toc_source(cx)
                        .map(|(entries, _)| entries.iter().map(|&(_, l, _)| l as usize).collect())
                        .unwrap_or_default();
                    toc::toc_node_has_child(&levels, idx)
                }
            },
            None => false,
        };
        if !has_child {
            return;
        }
        if !self.sidebar.toc_collapsed.remove(&idx) {
            self.sidebar.toc_collapsed.insert(idx);
        }
        // 折叠集变更 → 可见性缓存作废（下帧按新折叠重建可见条目表）。
        self.sidebar.toc_cache = None;
        cx.notify();
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
            + level as f32 * 14.0
            + 16.0             // 箭头列（无箭头也占位对齐）
            + 6.0 + 5.0 + 6.0  // gap + 圆点 + gap
            + 8.0              // pr
    }

    /// 计算当前激活文档目录所需的最大行宽（px）；无文档/无标题返回 `None`。
    fn compute_sidebar_toc_width(&self, cx: &gpui::App) -> Option<f32> {
        let tab = self.tabs.get(self.active_tab)?;
        let font_px = 13.0f32;
        // 右侧页码列宽（PDF 书签有页码，Markdown 无）：预留以免标题被截断
        let page_col_w = 34.0f32;
        let max_row = if let MediaReader::Pdf(pdf) = &tab.reader {
            let outlines = pdf.read(cx).outlines();
            outlines
                .iter()
                .map(|e| Self::toc_row_width(e.level, &e.title, font_px) + page_col_w)
                .fold(0.0f32, f32::max)
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
        let (fg, _muted) = (self.text_primary(), self.text_minor());
        let border = self.bar_border();
        div()
            .id("sidebar-toc-header")
            .debug_selector(|| "sidebar-toc-header".into())
            .flex()
            .items_center()
            .px(px(10.))
            .h(px(36.))
            .border_b_1()
            .border_color(border)
            .child(
                div()
                    .text_size(px(13.5))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(fg)
                    .child("目录"),
            )
    }

    /// 「目录」视图：当前文档的标题大纲（Markdown 标题 / PDF 书签），
    /// 带面板头（「目录」+ 关闭）与页码，条目样式统一来自公共 [`crate::toc`]（可折叠树）。
    pub(super) fn render_sidebar_toc(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let (fg, muted) = (self.text_primary(), self.text_minor());
        let active_bg = self.tab_active_bg();
        let accent = ui::theme::Theme::global(cx).colors.accent.into();

        // 折叠状态按文档缓存：切换文档即清空，并重新按新文档内容自适应宽度
        let active = self.active_tab;
        if self.sidebar.toc_tab != Some(active) {
            self.sidebar.toc_tab = Some(active);
            self.sidebar.toc_collapsed.clear();
            self.sidebar.width_auto = true;
            self.sidebar.toc_width = None;
            self.sidebar.toc_cache = None;
        }

        // 目录视图自适应宽：按当前文档最宽目录条目估算（未手动拖拽时）。
        // 按 (选项卡, 编辑版本, WyEditor 内容代号) 缓存：内容没变只算一次，禁逐帧 O(N) 全量测宽。
        if self.sidebar.width_auto {
            let tab = self.tabs.get(active);
            let ver = tab.map(|t| t.edit_version).unwrap_or(0);
            let cg = tab
                .and_then(|t| t.md_wy.as_ref())
                .map(|wy| wy.read(cx).content_gen())
                .unwrap_or(0);
            let cached = self
                .sidebar
                .toc_width
                .as_ref()
                .is_some_and(|(ti, v, g, _)| *ti == active && *v == ver && *g == cg);
            if !cached
                && let Some(w) = self.compute_sidebar_toc_width(cx)
            {
                self.sidebar.toc_width = Some((active, ver, cg, w));
                self.sidebar.width = (config::layout::SIDEBAR_ACTIVITY_W + w + 12.0)
                    .clamp(config::layout::SIDEBAR_WIDTH_MIN, config::layout::SIDEBAR_WIDTH_MAX);
            }
        }

        let inner: gpui::AnyElement = match self.tabs.get(self.active_tab) {
            Some(tab) => {
                let collapsed = self.sidebar.toc_collapsed.clone();
                // PDF 书签目录
                if let MediaReader::Pdf(pdf) = &tab.reader {
                    let outlines = pdf.read(cx).outlines().to_vec();
                    if outlines.is_empty() {
                        ui::empty_state::compact_empty_state(
                            "sidebar-toc-empty",
                            IconName::File,
                            "当前 PDF 无书签目录",
                            cx,
                        )
                    } else {
                        let rows = self.sidebar_toc_pdf_rows(
                            window, &outlines, &collapsed, fg, muted, active_bg, accent,
                            pdf.clone(), cx,
                        );
                        div().flex().flex_col().py_1().children(rows).into_any_element()
                    }
                } else {
                    // Markdown 标题（P4-2：MD / EPUB 目录都来自统一 WyEditor 宿主）。
                    let tab = self.tabs.get(active);
                    let ver = tab.map(|t| t.edit_version).unwrap_or(0);
                    let wy = tab.and_then(|t| t.md_wy.as_ref());
                    let cg = wy.map(|wy| wy.read(cx).content_gen()).unwrap_or(0);
                    // 条目 + 可见性 + 跳转目标按 (选项卡, 编辑版本, WyEditor 宿主 + 内容代号)
                    // 缓存：内容/折叠没变逐帧 O(1) 复用，大文档不再每帧全量克隆条目/重扫。
                    // 命中需宿主同一 + 内容代号同一：预览关/重装（md_wy 换实体）与整篇换源
                    //（同实体 in-place 喂料，edit_version 不变）都作废重建，不渲染上一版条目。
                    let cache_ok = match &self.sidebar.toc_cache {
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
                                self.sidebar.toc_cache =
                                    Some((active, ver, cg, entries, visible, scroll));
                            }
                            _ => self.sidebar.toc_cache = None,
                        }
                    }
                    match &self.sidebar.toc_cache {
                        Some((_, _, _, entries, visible, scroll)) if !entries.is_empty() => {
                            let total = visible.len();
                            // 虚拟化：只物化可视带 + 上下 M 行缓冲；带外以等高 spacer 占位
                            //（总高恒为 visible × 行高，滚动量程/命中稳定）。普通小文档全量。
                            let (start, end) = if total <= TOC_VIRTUALIZE_MIN {
                                (0usize, total)
                            } else {
                                let vhand = &self.sidebar.toc_vscroll;
                                let offset = (-vhand.offset().y).max(px(0.)).as_f32();
                                let viewport_h = vhand.bounds().size.height.as_f32();
                                // 首帧容器尚未布局（高度 0）→ 退化为固定视口估计（只多物化几行）。
                                let vh = if viewport_h > 0.0 {
                                    viewport_h
                                } else {
                                    700.0
                                };
                                let first = (offset / toc::TOC_ROW_H) as usize;
                                let last = ((offset + vh) / toc::TOC_ROW_H) as usize + 1;
                                let start = first.saturating_sub(TOC_BAND_BUFFER);
                                let end = (last + TOC_BAND_BUFFER).min(total);
                                // 折叠后条目大减时偏移可能越界：钳制空带。
                                (start.min(end), end.max(start))
                            };
                            let rows = self.sidebar_toc_md_rows(
                                window, entries, &visible[start..end], &collapsed, fg, muted,
                                active_bg, accent, scroll.clone(), cx,
                            );
                            let (top_h, bottom_h) = (
                                start as f32 * toc::TOC_ROW_H,
                                (total - end) as f32 * toc::TOC_ROW_H,
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
                            IconName::BookOpen,
                            "当前文件无目录（无标题）",
                            cx,
                        ),
                    }
                }
            }
            None => ui::empty_state::compact_empty_state(
                "sidebar-toc-empty",
                IconName::File,
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
                    self.sidebar.toc_vscroll.clone(),
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
            let toggle_collapse = cx.listener(move |this: &mut EditorApp, _ev, _w, cx| {
                cx.stop_propagation();
                this.toggle_sidebar_toc_collapse(idx, cx);
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

    /// Markdown 标题目录条目（可折叠树；可见性按当前折叠集合过滤）。
    /// **虚拟化**：`visible` = 调用方算好的「待物化条目下标」（可视带切片，非全量），
    /// 只物化带内条目，大文档不再每帧全量实例化万级元素。
    /// P4-2：`scroll` 跳转目标 = 右栏 WyEditor 只读预览（源码行）`scroll_to_line`；
    ///（`MarkdownPreview` 的 `Legacy` 渲染行目标已随引擎删除。）
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
            let toggle_collapse = cx.listener(move |this: &mut EditorApp, _ev, _w, cx| {
                cx.stop_propagation();
                this.toggle_sidebar_toc_collapse(idx, cx);
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
