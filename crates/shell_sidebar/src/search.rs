//! 侧边栏「跨文件搜索」实体子模块（2026-11 自 shell 的 `app/sidebar/search.rs` 迁入）。
//!
//! 关键词输入 + 范围切换（当前文件 / 整个文件夹）+ 结果渲染与防抖扫描（含文件夹后台
//! 分块扫描、PDF 全文检索接线与命中跳转）。与 chrome / toc / files / files_ops 同构：
//! 渲染期状态（`SidebarChromeState` / 共享 `AppModel`）以参数 / 共享模型入读，事件回调
//! （范围 tab 点击 / 命中行打开）经 [`crate::host::SidebarHost`] 反调宿主（打开/激活 /
//! notify），扫描与防抖均为**实体弱句柄自更新**（写共享模型 + 自 notify），不回流宿主。
//!
//! 与旧 `impl EditorApp` 的对应关系（逐行迁移，行为零变）：
//!
//! - `render_sidebar_search` / `render_search_scope_tabs` / `render_sidebar_search_results` /
//!   `render_sidebar_folder_search` / `sidebar_folder_*` / `sidebar_search_*` /
//!   `render_sidebar_pdf_search` / `sidebar_pdf_*` → 同名实体方法（`self.sidebar` →
//!   `sb` 入参，`self.search_state` → `self.search_input`）。
//! - `effective_search_scope` → [`crate::state::effective_search_scope`] 纯函数（单一事实源，
//!   宿主 facade 与域渲染共用）。
//! - `sidebar_search_scan` / `folder_search_scan` / `start_folder_scan` → 实体方法，扫描缓存
//!   与防抖状态经共享模型读写（`search_committed` / `search_pending` / `search_scan` /
//!   `folder_search` / `folder_search_gen` / `search_debounce`），宿主只保留模型失效埋点。
//! - `folder_search_open_hit` → 实体方法；「打开/激活」经既有 [`SidebarHost::sidebar_open_path_or_activate`]，
//!   「文件名命中 → 激活既有选项卡」经新增 [`SidebarHost::sidebar_activate_tab`]。

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gpui::{
    div, px, AnimationExt, App, Context, CursorStyle, HighlightStyle, InteractiveElement,
    IntoElement, ParentElement, SharedString, StatefulInteractiveElement, Styled, StyledText,
    VisualContext, Window,
};
use shell_core::{MediaReader, SearchScopeTab};
use shell_scan::{
    contains_ci, folder_scan_collect, highlight_ranges, scan_line_hits, FolderScanUpdate,
    FolderSearchScan, SearchLineHit, SidebarSearchScan,
};
use ui::icon::{Icon, IconName};
use ui::input::Input;
use ui::motion::{hover_bg_fade, hover_spring};

use crate::chrome::Sidebar;
use crate::host::SidebarHost;
use crate::prompt::FileTreePromptHost;
use crate::state::{
    effective_search_scope, normalize_search_query, SidebarChromeState, SEARCH_DEBOUNCE_MS,
};

impl<H: SidebarHost + FileTreePromptHost> Sidebar<H> {
    /// 「搜索」视图：关键词输入框 + 范围切换（当前文件 / 整个文件夹）+ 匹配结果。
    /// 「当前文件」= 当前文档内容 + 跨打开文件文件名匹配（旧语义）；
    /// 「整个文件夹」= 递归扫已打开文件夹内文本文件内容（后台分块，按文件分组展示）。
    /// 搜索输入实体为本实体自有（`self.search_input`），渲染直读；`sb` 提供
    /// `files.root`（范围判断）与 `search_vscroll`（结果滚动容器）。
    pub fn render_sidebar_search(
        &mut self,
        sb: &mut SidebarChromeState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let muted = self.text_minor();
        let query = self.model.borrow().search_committed.clone();
        let search = self.search_input.clone();
        // Fluent 文本框：整圈细边框 + 内缩边距（背景随 chrome 保持透明）；
        // 聚焦时边框点亮为强调色。
        let focused = window.focused(cx) == Some(self.search_input.read(cx).focus_handle(cx));
        let border_color: gpui::Hsla = if focused {
            ui::theme::Theme::global(cx).colors.focus_ring.into()
        } else {
            self.bar_border()
        };
        let input = div()
            .id("sidebar-search-input")
            .debug_selector(|| "sidebar-search-input".into())
            .px_2()
            .py_1()
            .mx_2()
            .my_1()
            .w_full()
            .min_h(px(34.))
            .rounded_sm()
            .border_1()
            .border_color(border_color)
            .child(Input::new(&search));

        // 扫描用「防抖后已提交」的查询词（已归一化 trim+小写）；输入框实时文本在
        // `search_input` —— 连续输入时这里沿用上次提交词的结果，防抖期满才重扫。
        let q = query;
        let scope = effective_search_scope(&self.model.borrow(), sb);
        let hint_text = match scope {
            SearchScopeTab::CurrentFile => {
                "输入关键字，搜索当前文件内容；文件名匹配所有打开文件"
            }
            SearchScopeTab::Folder => "输入关键字，搜索整个文件夹内文本文件内容",
        };
        let result_area: gpui::AnyElement = if q.is_empty() {
            div()
                .id("sidebar-search-hint")
                .debug_selector(|| "sidebar-search-hint".into())
                .px_3()
                .py_2()
                .w_full()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_overflow(gpui::TextOverflow::Truncate("…".into()))
                .text_size(px(12.))
                .text_color(muted)
                .child(hint_text)
                .into_any_element()
        } else {
            match scope {
                SearchScopeTab::CurrentFile => {
                    self.render_sidebar_search_results(sb, window, &q, cx)
                }
                SearchScopeTab::Folder => self.render_sidebar_folder_search(sb, window, &q, cx),
            }
        };

        div()
            .id("sidebar-search")
            .debug_selector(|| "sidebar-search".into())
            .flex()
            .flex_col()
            .flex_1()
            .min_w(px(0.))
            .min_h(px(0.))
            .overflow_hidden()
            .child(input)
            .child(self.render_search_scope_tabs(sb, window, cx))
            .child(result_area)
            // P2-10：面板开合轻淡入（同目录面板，见 `sidebar-toc-fade`）。
            .with_spring(
                "sidebar-search-fade",
                hover_spring(1.0).from(0.0),
                |this, v| this.opacity(v),
            )
            .into_any_element()
    }

    /// 搜索范围「当前文件 / 整个文件夹」切换 tab 条：输入框下方横排两个小 tab。
    /// 未打开文件夹时「整个文件夹」置灰不可用（点击仅提示「先打开文件夹」，
    /// 保持只搜当前文件）；活动项以活动背景 + 主前景色标示。范围状态在共享模型
    /// （`search_scope` / `search_scope_manual`），点击经 [`Self::set_search_scope`]
    /// 实体直接改写 + 自 notify；无文件夹提示写共享模型 `status`。
    fn render_search_scope_tabs(
        &mut self,
        sb: &SidebarChromeState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let (fg, muted) = (self.text_primary(), self.text_minor());
        let active_bg = self.tab_active_bg();
        let has_folder = sb.files.root.is_some();
        let scope = effective_search_scope(&self.model.borrow(), sb);
        let view_id = window.current_view();

        let current_hover =
            window.use_keyed_state("side-search-scope-cur-hover", cx, |_, _| false);
        let folder_hover =
            window.use_keyed_state("side-search-scope-folder-hover", cx, |_, _| false);
        let current_hovered = *current_hover.read(cx);
        let folder_hovered = *folder_hover.read(cx);

        let current = div()
            .id("sidebar-search-scope-current")
            .debug_selector(|| "sidebar-search-scope-current".into())
            .flex_1()
            .items_center()
            .justify_center()
            .h(px(26.))
            .mx(px(4.))
            .rounded_md()
            .text_size(px(12.))
            .text_color(if scope == SearchScopeTab::CurrentFile { fg } else { muted })
            .cursor(CursorStyle::PointingHand)
            .on_hover({
                let st = current_hover.clone();
                move |h, _w, cx| {
                    st.update(cx, |v, _| *v = *h);
                    cx.notify(view_id);
                }
            })
            .on_click(cx.listener(
                |this: &mut Self, _ev, _window, cx| {
                    this.set_search_scope(SearchScopeTab::CurrentFile, cx);
                },
            ))
            .child("当前文件");

        let folder = div()
            .id("sidebar-search-scope-folder")
            .debug_selector(|| "sidebar-search-scope-folder".into())
            .flex_1()
            .items_center()
            .justify_center()
            .h(px(26.))
            .mx(px(4.))
            .rounded_md()
            .text_size(px(12.))
            .text_color(if scope == SearchScopeTab::Folder {
                fg
            } else if has_folder {
                muted
            } else {
                muted.opacity(0.5)
            })
            .cursor(if has_folder {
                CursorStyle::PointingHand
            } else {
                CursorStyle::default()
            })
            .on_hover({
                let st = folder_hover.clone();
                move |h, _w, cx| {
                    st.update(cx, |v, _| *v = *h);
                    cx.notify(view_id);
                }
            })
            .on_click(cx.listener(
                move |this: &mut Self, _ev, _window, cx| {
                    // 未打开文件夹 → 「整个文件夹」不可用：仅提示，保持只搜当前文件。
                    // `has_folder` 渲染期捕获（文件夹有无变化必伴随宿主 notify → 本面板
                    // 重渲染更新捕获值，与旧「点击时读 sidebar.files.root」语义等价）。
                    if !has_folder {
                        this.model.borrow_mut().status =
                            SharedString::from("先打开文件夹，再搜索整个文件夹");
                        this.model.borrow_mut().status_ok = false;
                        cx.notify();
                        return;
                    }
                    this.set_search_scope(SearchScopeTab::Folder, cx);
                },
            ))
            .child("整个文件夹");

        let set_bg = |active: bool, hovered: bool| {
            if active {
                active_bg
            } else if hovered {
                active_bg
            } else {
                active_bg.opacity(0.)
            }
        };
        div()
            .id("sidebar-search-scope")
            .debug_selector(|| "sidebar-search-scope".into())
            .flex()
            .flex_row()
            .items_center()
            .mx_2()
            .mb_1()
            .child(current.bg(set_bg(scope == SearchScopeTab::CurrentFile, current_hovered)))
            .child(folder.bg(set_bg(
                scope == SearchScopeTab::Folder,
                folder_hovered && has_folder,
            )))
            .into_any_element()
    }

    /// 用户点击范围 tab：记录手动选择（此后不再自动跟随文件夹有无），并通知刷新。
    /// 范围 / 手动标记 / 当前文件扫描缓存都在共享模型（`search_scan` 亦一并失效，
    /// 下帧重建），实体直改 + 自 notify（面板开启时本实体在窗口渲染态内，notify 即
    /// 重绘窗口，行为与旧宿主 `cx.notify()` 一致）。
    pub fn set_search_scope(&mut self, scope: SearchScopeTab, cx: &mut Context<Self>) {
        self.model.borrow_mut().search_scope_manual = true;
        self.model.borrow_mut().search_scope = scope;
        self.model.borrow_mut().search_scan = None; // 范围变化 → 当前文件扫描缓存也一并失效（下帧重建）
        cx.notify();
    }

    /// 计算并渲染搜索结果（文件名命中 + 行内容命中，按打开顺序排列，封顶）。
    /// 扫描结果缓存在 [`Self::search_scan`] 对应模型字段：查询词/活动文档未变时，反复
    /// 渲染帧直接 O(1) 复用，不再每帧 O(全文) 重扫（旧逻辑在 render 里逐帧全量扫 +
    /// 每行 `to_lowercase()` 分配）。
    fn render_sidebar_search_results(
        &mut self,
        sb: &SidebarChromeState,
        window: &mut Window,
        q: &str,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let (fg, muted) = (self.text_primary(), self.text_minor());
        let active_bg = self.tab_active_bg();
        // PDF 标签：全文检索由 PdfViewer 后台执行（文本在 pdfium 里，不在 tab.content）。
        // 查询按（标签, 词）键控、变化才推送（阅读器内部 250ms 防抖）；侧边栏只展示
        // 计数/页列表并驱动跳转。结果异步到达 → 经对应 PDF 选项卡的 `_pdf_sub` 订阅
        // 重绘本面板（多 PDF 并存各自独立订阅，见 `subscribe_pdf`）。
        // 先取出 reader（owned 克隆）再 `if let`，避免 `if let` 掮获 `borrow()` 临时
        // Ref 跨过块内 `&mut self` / `borrow_mut`（E0502 / 借用纪律收窄）。
        let reader =
            self.model.borrow().tabs.get(self.model.borrow().active_tab).map(|t| t.reader.clone());
        if let Some(MediaReader::Pdf(pdf)) = reader {
            let push = self
                .model.borrow().pdf_search
                .as_ref()
                .map(|(ti, q0)| *ti != self.model.borrow().active_tab || q0 != q)
                .unwrap_or(true);
            if push {
                let active = self.model.borrow().active_tab;
                self.model.borrow_mut().pdf_search = Some((active, q.to_string()));
                let qo = q.to_string();
                pdf.update(cx, |v, cx| v.set_search(&qo, cx));
            }
            let pdf_results = self.render_sidebar_pdf_search(window, &pdf, q, cx);
            return self.wrap_search_scroll(sb, "sidebar-search-results-pdf", pdf_results);
        }
        let scan = self.sidebar_search_scan(q, cx);
        let hits = scan.file_tabs.len() + scan.line_hits.len();
        // 无命中 → 居中「灰色搜索图标 + 文案」空态占位（与目录/正文区空态同款视觉）。
        if hits == 0 {
            return self.wrap_search_scroll(
                sb,
                "sidebar-search-results",
                ui::empty_state::compact_empty_state(
                    "sidebar-search-empty",
                    IconName::Search,
                    "未找到匹配内容",
                    cx,
                ),
            );
        }
        let mut rows: Vec<gpui::AnyElement> = Vec::new();
        rows.extend(self.sidebar_search_file_rows(
            window,
            &scan,
            fg,
            muted,
            active_bg,
            cx,
        ));
        rows.extend(self.sidebar_search_line_rows(window, &scan, muted, active_bg, cx));

        let inner = div()
            .child(self.sidebar_search_summary(hits, muted))
            .child(div().flex().flex_col().py_1().children(rows));
        self.wrap_search_scroll(sb, "sidebar-search-results", inner)
    }

    /// 搜索结果滚动容器包装：`overflow_y_scroll` 滚动容器外套 `relative` 包装层，
    /// 叠一条常显（`Always`）纵向滚动条——与目录/编辑器/预览同一语义：内容超量可滚
    /// 时随 `sb.search_vscroll` 显示 thumb，轨道点击跳转、thumb 可拖。
    /// `container_id` 透传（普通搜索 `sidebar-search-results` / PDF 搜索 `-pdf`，测试沿用）。
    /// 结构/样式收敛到 `ui::scroll_panel::ScrollPanelBuilder`：滚动条覆盖层 id 显式覆盖为
    /// `sidebar-search-vscrollbar`（派生 `{id}-vscrollbar` 与之不一致）。
    fn wrap_search_scroll(
        &self,
        sb: &SidebarChromeState,
        container_id: &'static str,
        inner: impl IntoElement,
    ) -> gpui::AnyElement {
        let vhand = sb.search_vscroll.clone();
        ui::scroll_panel::ScrollPanelBuilder::new(container_id, vhand)
            .wrap_id("sidebar-search-results-wrap")
            .bar_id("sidebar-search-vscrollbar")
            .build(inner)
            .into_any_element()
    }

    /// 文件夹级搜索视图：按文件分组展示命中（文件头行 + 命中行），后台增量装配。
    /// 未打开文件夹 → 「先打开文件夹」空态；扫描中且无命中 → 「搜索中…」；
    /// 命中后按 [`FOLDER_SEARCH_RESULT_CAP`] 截断并提示。
    fn render_sidebar_folder_search(
        &mut self,
        sb: &SidebarChromeState,
        window: &mut Window,
        q: &str,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let Some(scan) = self.folder_search_scan(sb, q, window, cx) else {
            return self.wrap_search_scroll(
                sb,
                "sidebar-search-results",
                ui::empty_state::compact_empty_state(
                    "sidebar-search-empty",
                    IconName::Folder,
                    "先打开文件夹，再搜索整个文件夹",
                    cx,
                ),
            );
        };
        // 扫描进行中且尚无任何命中 → 「搜索中…」占位（有命中则走汇总头计数 + 分组行）。
        if scan.running && scan.total_hits == 0 {
            return self.folder_search_loading(sb);
        }
        if scan.total_hits == 0 {
            return self.wrap_search_scroll(
                sb,
                "sidebar-search-results",
                ui::empty_state::compact_empty_state(
                    "sidebar-search-empty",
                    IconName::Search,
                    "未找到匹配内容",
                    cx,
                ),
            );
        }
        let (fg, muted) = (self.text_primary(), self.text_minor());
        let active_bg = self.tab_active_bg();
        let rows = self.sidebar_folder_file_rows(window, &scan, fg, muted, active_bg, cx);
        let inner = div()
            .child(self.sidebar_folder_search_summary(&scan, muted))
            .child(div().flex().flex_col().py_1().children(rows));
        self.wrap_search_scroll(sb, "sidebar-search-results", inner)
    }

    /// 文件夹搜索「搜索中…」占位（扫描进行中、尚无命中时的整块提示）。
    fn folder_search_loading(&self, sb: &SidebarChromeState) -> gpui::AnyElement {
        let muted = self.text_minor();
        let container = div()
            .id("sidebar-search-loading")
            .debug_selector(|| "sidebar-search-loading".into())
            .px_3()
            .py_3()
            .flex()
            .items_center()
            .gap_2()
            .text_size(px(12.))
            .text_color(muted)
            .child(Icon::new(IconName::Search).size(px(14.)).text_color(muted))
            .child("搜索中…");
        self.wrap_search_scroll(sb, "sidebar-search-results", container)
    }

    /// 文件夹搜索汇总头：n 处匹配（m 个文件）/ 搜索中计数 / 已达上限截断提示。
    fn sidebar_folder_search_summary(
        &self,
        scan: &FolderSearchScan,
        muted: gpui::Hsla,
    ) -> gpui::AnyElement {
        let text = if scan.running {
            format!("搜索中… 已找到 {} 处匹配", scan.total_hits)
        } else if scan.truncated {
            format!("{} 处匹配（已达上限，结果截断）", scan.total_hits)
        } else {
            format!("{} 处匹配（{} 个文件）", scan.total_hits, scan.files.len())
        };
        div()
            .id("sidebar-folder-search-summary")
            .debug_selector(|| "sidebar-folder-search-summary".into())
            .px_3()
            .py_2()
            .text_size(px(11.))
            .text_color(muted)
            .child(text)
            .into_any_element()
    }

    /// 文件夹级搜索**分组渲染**：每文件一个组 = 文件头行（文件名 + 命中词频计数）+
    /// 逐命中行（`L行号  命中行预览`，命中词以强调色高亮）。点击命中行 → 打开/激活 +
    /// 光标定位 + 滚动可见 + 命中词临时高亮（见 [`Self::folder_search_open_hit`]）。
    #[allow(clippy::too_many_arguments)] // 三色 + 数据源 + 滚动句柄，与其它行渲染同构
    fn sidebar_folder_file_rows(
        &self,
        window: &mut Window,
        scan: &FolderSearchScan,
        fg: gpui::Hsla,
        muted: gpui::Hsla,
        active_bg: gpui::Hsla,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let accent: gpui::Hsla = ui::theme::Theme::global(cx).colors.accent.into();
        let mut rows: Vec<gpui::AnyElement> = Vec::new();
        for (fi, file) in scan.files.iter().enumerate() {
            let fname = file
                .path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| file.path.to_string_lossy().into_owned());
            // 文件头行（文件名 + 命中词频计数）。
            let hdr = div()
                .id(SharedString::from(format!("sidebar-folder-file-{fi}")))
                .debug_selector(move || format!("sidebar-folder-file-{fi}"))
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .px_2()
                .py_1()
                .mx(px(4.))
                .rounded_md()
                .child(Icon::new(IconName::File).size(px(13.)).text_color(fg))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_overflow(gpui::TextOverflow::Truncate("…".into()))
                        .text_size(px(12.))
                        .text_color(fg)
                        .child(fname),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .text_size(px(11.))
                        .text_color(muted)
                        .child(format!("{} 处", file.hits.len())),
                );
            rows.push(hdr.into_any_element());
            // 命中行：行号 + 预览（命中词高亮）。
            for (ki, hit) in file.hits.iter().enumerate() {
                let li = hit.line;
                let match_range = hit.range.clone();
                let snippet = hit.snippet.clone();
                let q_owned = scan.query.clone();
                let path = file.path.clone();
                // P2-10：hover 状态跨帧记录 → 行背景淡入。
                let hover = window
                    .use_keyed_state(format!("sf-hit-{fi}-{ki}-hover"), cx, |_, _| false);
                let hovered = *hover.read(cx);
                let view_id = window.current_view();
                let line_prefix = format!("L{}  ", li + 1);
                let display = format!("{line_prefix}{snippet}");
                // 命中词在预览中高亮：snippet 内全部大小写不敏感命中区间，再平移到显示串。
                let mut hl = highlight_ranges(&snippet, &q_owned);
                for r in &mut hl {
                    r.start += line_prefix.len();
                    r.end += line_prefix.len();
                }
                let styled = StyledText::new(SharedString::from(display))
                    .with_highlights(
                        hl.into_iter()
                            .map(|r| {
                                (
                                    r,
                                    HighlightStyle {
                                        color: Some(accent),
                                        ..Default::default()
                                    },
                                )
                            })
                            .collect::<Vec<_>>(),
                    );
                let r = div()
                    .id(SharedString::from(format!(
                        "sidebar-search-folder-hit-{fi}-{ki}"
                    )))
                    .debug_selector(move || format!("sidebar-search-folder-hit-{fi}-{ki}"))
                    .px_2()
                    .py_1()
                    .mx(px(4.))
                    .rounded_md()
                    .text_size(px(12.))
                    .text_color(muted)
                    .cursor(CursorStyle::PointingHand)
                    .on_hover({
                        let st = hover.clone();
                        move |h, _w, cx| {
                            st.update(cx, |v, _| *v = *h);
                            cx.notify(view_id);
                        }
                    })
                    .on_click(cx.listener(move |this: &mut Self, _ev, window, cx| {
                        this.folder_search_open_hit(&path, li, match_range.clone(), &q_owned, window, cx);
                    }))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .child(styled),
                    );
                rows.push(
                    r.bg(if hovered { active_bg } else { active_bg.opacity(0.) })
                        .into_any_element(),
                );
            }
        }
        rows
    }

    /// 文件夹搜索「取扫描或发起扫描」：按键 `(根, 查询词)` 复用缓存（含进行中/已完成），
    /// 未打开文件夹返回 `None`；查询词 / 根变化 → 发起新一轮后台扫描（新代，旧代结果丢弃）。
    fn folder_search_scan(
        &mut self,
        sb: &SidebarChromeState,
        q: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Rc<FolderSearchScan>> {
        let root = sb.files.root.clone()?;
        if let Some(s) = &self.model.borrow().folder_search
            && s.query == q
            && s.root.as_deref() == Some(root.as_path())
        {
            return Some(s.clone());
        }
        self.start_folder_scan(root, q, window, cx);
        self.model.borrow().folder_search.clone()
    }

    /// 发起一把文件夹级后台扫描：**不在主线程同步扫整棵**——生产者（后台线程）递归
    /// 读盘并逐文件外推命中块（有界 channel 背压，参考 `spawn_progressive_open` 大文档
    /// 分块读盘姿势），主线程只收块增量装配到 `model.folder_search`，扫描中 UI 显示
    /// 「搜索中…」。查询词 / 根变化 → 换代；旧代异步回调按代丢弃（不覆盖新结果）。
    /// **实体弱句柄自更新**（写共享模型 + 自 notify），不回流宿主（对照 files.rs 的
    /// 「宿主侧 spawn + update_window_entity 反调纯变换」——本扫描缓存就在共享模型，
    /// 实体直接 self-update 更贴合；绝不在行 on_click/on_input 回调经宿主回租本实体）。
    fn start_folder_scan(&mut self, root: PathBuf, q: &str, window: &mut Window, cx: &mut Context<Self>) {
        let generation = self.model.borrow().folder_search_gen.wrapping_add(1);
        self.model.borrow_mut().folder_search_gen = generation;
        self.model.borrow_mut().folder_search = Some(Rc::new(FolderSearchScan {
            query: q.to_string(),
            root: Some(root.clone()),
            files: Vec::new(),
            truncated: false,
            running: true,
            generation,
            total_hits: 0,
        }));
        let entity = cx.entity();
        let q_owned = q.to_string();
        let (tx, rx) = futures::channel::mpsc::channel::<FolderScanUpdate>(4);
        window
            .spawn(cx, async move |cx| {
                use futures::{SinkExt as _, StreamExt as _};
                // ---- 生产者（后台线程）：递归扫文件夹 → 逐文件命中按块外推 ----
                let mut prod_tx = tx.clone();
                let prod_root = root;
                let prod_q = q_owned;
                let prod_sess = config::user::UserPrefs::session_dir();
                cx.background_executor()
                    .spawn(async move {
                        let mut total = 0usize;
                        let truncated = folder_scan_collect(
                            &prod_root,
                            &prod_q,
                            prod_tx.clone(),
                            &prod_sess,
                            &mut total,
                            config::limits::FOLDER_SEARCH_RESULT_CAP,
                        )
                        .await;
                        let _ = prod_tx.send(FolderScanUpdate::Done { truncated }).await;
                    })
                    .detach();
                // 原始 sender 立即释放：生产者外推完毕后 channel 关闭 → 消费者收完退出。
                drop(tx);

                // ---- 消费者（主线程任务）：逐块增量装配，扫描中逐块刷新 ----
                let mut rx = rx;
                while let Some(update) = rx.next().await {
                    let scan_gen = generation;
                    let _ = cx.update_window_entity(&entity, move |this, _w, cx| {
                        // SC-3：先提纯 owned Rc 再 let-else，避免 `if let` 掮获的 Ref 临时
                        // 存活整个分支体（块内 `borrow_mut` 运行期双借 panic）。
                        let folder_search = this.model.borrow().folder_search.clone();
                        let Some(existing) = folder_search else {
                            return;
                        };
                        if existing.generation != scan_gen {
                            return; // 已被更新的查询/换根取代 → 丢弃旧代结果
                        }
                        // 逐块增量：基于当前状态克隆应用增量后放回（Rc 共享读、写时替换）。
                        let mut next = existing.as_ref().clone();
                        match update {
                            FolderScanUpdate::File(f) => {
                                next.total_hits += f.hits.len();
                                next.files.push(f);
                            }
                            FolderScanUpdate::Done { truncated } => {
                                next.truncated = truncated;
                                next.running = false;
                            }
                        }
                        this.model.borrow_mut().folder_search = Some(Rc::new(next));
                        cx.notify();
                    });
                }
            })
            .detach();
    }

    /// 文件夹搜索点击命中行：路径失效 → 提示不打开；有效 → 打开/激活（已打开激活既有
    /// 选项卡）+ 光标定位到命中行 + 滚动可见 + 命中词临时高亮（编辑区选区 / 预览搜索高亮）。
    /// 「打开/激活」经宿主服务 [`SidebarHost::sidebar_open_path_or_activate`]（实体经
    /// host_weak 反调，单向下行不回流实体），定位/高亮在本实体（共享模型 tab 直读）。
    pub fn folder_search_open_hit(
        &mut self,
        path: &Path,
        line: usize,
        range: Range<usize>,
        q_owned: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 错误分类：命中路径在点击时已失效（文件被删除/改名）→ 不打开并提示。
        if !path.is_file() {
            self.model.borrow_mut().status =
                SharedString::from(format!("命中文件已不存在：{}", path.display()));
            self.model.borrow_mut().status_ok = false;
            cx.notify();
            return;
        }
        if let Some(h) = self.host_weak().upgrade() {
            let _ = h.update(cx, |app, cx| app.sidebar_open_path_or_activate(path, window, cx));
        }
        let Some(ti) = self.model.borrow().tabs.iter().position(|t| t.file_path.as_deref() == Some(path))
        else {
            return;
        };
        self.jump_tab_to_hit(ti, line, range, q_owned, cx);
        cx.notify();
    }

    /// 跳转到命中行并高亮命中词：源码编辑区 = 光标落行 + 选中命中区间（选中即高亮、
    /// `reveal_caret` 滚动可见）；只读预览 / WYSIWYG / 统一组件 = `set_scroll_target` +
    /// `highlight_search`（命中词临时高亮）。大文档后台打开尚未挂载（空缓冲）→ 跳过定位。
    fn jump_tab_to_hit(
        &mut self,
        ti: usize,
        li: usize,
        range: Range<usize>,
        q_owned: &str,
        cx: &mut Context<Self>,
    ) {
        let model = self.model.borrow();
        let Some(tab) = model.tabs.get(ti) else {
            return;
        };
        if tab.readonly_preview {
            if let Some(wy) = tab.md_wy.clone() {
                wy.update(cx, |v, cx| {
                    v.set_scroll_target(li);
                    v.highlight_search(li, q_owned, cx);
                });
            }
            return;
        }
        if let Some(l2) = tab.live2.clone() {
            l2.update(cx, |v, cx| v.highlight_search(li, q_owned, cx));
        }
        if let Some(wy) = tab.md_wy.clone() {
            wy.update(cx, |v, cx| v.highlight_search(li, q_owned, cx));
        }
        let ed = tab.editor.clone();
        ed.update(cx, |ed, cx| {
            if ed.char_count() == 0 {
                return; // 大文档后台打开尚未挂载 → 跳过定位（文件回到顶部，随后内容到达）
            }
            ed.set_caret_to(ed.offset_for_line_col(li, 0));
            ed.set_selection_range(range, cx);
        });
    }

    /// 文件名命中（跨所有打开文件）：相当于「打开文件」快速选择器，点击激活选项卡。
    /// 「激活既有选项卡」经宿主服务 [`SidebarHost::sidebar_activate_tab`]（选项卡集合/
    /// 活动索引是宿主 panes 域能力）。
    fn sidebar_search_file_rows(
        &self,
        window: &mut Window,
        scan: &SidebarSearchScan,
        fg: gpui::Hsla,
        muted: gpui::Hsla,
        active_bg: gpui::Hsla,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let mut file_rows: Vec<gpui::AnyElement> = Vec::new();
        for ti in scan.file_tabs.iter().copied() {
            let name = self.model.borrow().tabs[ti]
                .file_path
                .as_ref()
                .and_then(|p| p.file_name())
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "未命名".to_string());
            let row_fg = if ti == self.model.borrow().active_tab { fg } else { muted };
            let nm = name;
            // P2-10：hover 状态跨帧记录 → 行背景淡入。
            let hover = window.use_keyed_state(format!("ss-file-{ti}-hover"), cx, |_, _| false);
            let hovered = *hover.read(cx);
            let view_id = window.current_view();
            let r = div()
                .id(SharedString::from(format!("sidebar-search-file-{ti}")))
                .debug_selector(move || format!("sidebar-search-file-{ti}"))
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .px_2()
                .py_1()
                .mx(px(4.))
                .rounded_md()
                .text_color(row_fg)
                .cursor(CursorStyle::PointingHand)
                .on_hover({
                    let st = hover.clone();
                    move |h, _w, cx| {
                        st.update(cx, |v, _| *v = *h);
                        cx.notify(view_id);
                    }
                })
                .on_click(cx.listener(move |this: &mut Self, _ev, window, cx| {
                    if let Some(h) = this.host_weak().upgrade() {
                        let _ = h.update(cx, |app, cx| app.sidebar_activate_tab(ti, window, cx));
                    }
                    cx.notify();
                }))
                .child(Icon::new(IconName::File).size(px(13.)).text_color(row_fg))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_size(px(12.))
                        .text_color(row_fg)
                        .child(nm),
                );
            file_rows.push(
                r.bg(if hovered { active_bg } else { active_bg.opacity(0.) })
                    .into_any_element(),
            );
        }
        file_rows
    }

    /// 行内容命中（只在「当前活动文档」内搜索）——若仍扫描所有打开的选项卡，
    /// 在 PDF 预览（无可搜索文本，content 为空）下会冒出别的文件（如 EPUB 完整正文）
    /// 的内容，看起来像搜索到了错误文档。点击：激活文档 + 光标/预览滚到命中行并高亮。
    fn sidebar_search_line_rows(
        &self,
        window: &mut Window,
        scan: &SidebarSearchScan,
        muted: gpui::Hsla,
        active_bg: gpui::Hsla,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let ti = self.model.borrow().active_tab;
        let mut line_rows: Vec<gpui::AnyElement> = Vec::new();
        for hit in &scan.line_hits {
            let li = hit.line;
            let match_range = hit.range.clone();
            let snippet = hit.snippet.clone();
            let q_owned = scan.query.clone();
            // P2-10：hover 状态跨帧记录 → 行背景淡入。
            let hover =
                window.use_keyed_state(format!("ss-line-{ti}-{li}-hover"), cx, |_, _| false);
            let hovered = *hover.read(cx);
            let view_id = window.current_view();
            let r = div()
                .id(SharedString::from(format!("sidebar-search-line-{ti}-{li}")))
                .debug_selector(move || format!("sidebar-search-line-{ti}-{li}"))
                .px_2()
                .py_1()
                .mx(px(4.))
                .rounded_md()
                .text_size(px(12.))
                .text_color(muted)
                .cursor(CursorStyle::PointingHand)
                .on_hover({
                    let st = hover.clone();
                    move |h, _w, cx| {
                        st.update(cx, |v, _| *v = *h);
                        cx.notify(view_id);
                    }
                })
                .on_click(cx.listener(move |this: &mut Self, _ev, window, cx| {
                    if let Some(h) = this.host_weak().upgrade() {
                        let _ = h.update(cx, |app, cx| app.sidebar_activate_tab(ti, window, cx));
                    }
                    this.jump_tab_to_hit(ti, li, match_range.clone(), &q_owned, cx);
                    cx.notify();
                }))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .child(format!("L{}  {}", li + 1, snippet)),
                );
            line_rows.push(
                r.bg(if hovered { active_bg } else { active_bg.opacity(0.) })
                    .into_any_element(),
            );
        }
        line_rows
    }

    /// 搜索结果头：0 命中提示 / n 处匹配。
    fn sidebar_search_summary(&self, total: usize, muted: gpui::Hsla) -> gpui::AnyElement {
        if total == 0 {
            div()
                .px_3()
                .py_2()
                .text_size(px(12.))
                .text_color(muted)
                .child("未找到匹配内容")
                .into_any_element()
        } else {
            div()
                .px_3()
                .py_2()
                .text_size(px(11.))
                .text_color(muted)
                .child(format!("{total} 处匹配"))
                .into_any_element()
        }
    }

    /// PDF 标签的搜索视图：顶部一行「n 处匹配」计数（右侧为 ↑/↓ 导航），
    /// 下方逐命中上下文行（`L N  前5字…目标…后5字`，单行显示超长省略，点击跳转到该命中页）。
    /// 检索本身由 [`pdf::PdfViewer::set_search`] 在阅读器后台执行，这里只读状态并驱动跳转。
    fn render_sidebar_pdf_search(
        &mut self,
        window: &mut Window,
        pdf: &gpui::Entity<pdf::PdfViewer>,
        q: &str,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let (fg, muted) = (self.text_primary(), self.text_minor());
        let active_bg = self.tab_active_bg();
        let (count, truncated, active) = pdf.read(cx).search_summary();
        let snippets: Vec<(usize, String)> = pdf.read(cx).search_snippets();

        let prev_btn = self.sidebar_pdf_nav_btn(window, "↑", -1, pdf.clone(), fg, active_bg, cx);
        let next_btn = self.sidebar_pdf_nav_btn(window, "↓", 1, pdf.clone(), fg, active_bg, cx);

        // 无匹配 → 居中空态占位（灰色搜索图标 + 提示文案），与普通搜索空态同款视觉。
        let summary: gpui::AnyElement = if count == 0 {
            let hint = if q.trim().is_empty() {
                "在 PDF 全文检索；输入关键字"
            } else {
                "PDF 中没有匹配"
            };
            ui::empty_state::compact_empty_state(
                "sidebar-search-pdf-empty",
                IconName::Search,
                hint,
                cx,
            )
        } else {
            Self::sidebar_pdf_search_summary(count, truncated, fg, prev_btn, next_btn)
        };
        let hit_rows =
            self.sidebar_pdf_hit_rows(window, pdf, &snippets, active, fg, muted, active_bg, cx);

        div()
            .id("sidebar-search-results-pdf")
            .debug_selector(|| "sidebar-search-results-pdf".into())
            .flex_1()
            .min_w(px(0.))
            .min_h(px(0.))
            .overflow_y_scroll()
            .child(summary)
            .child(div().flex().flex_col().py_1().children(hit_rows))
            .into_any_element()
    }

    /// PDF 搜索头部「上一处 / 下一处」导航按钮（↑/↓，点击在命中间移动）。
    /// 返回 owned `AnyElement`（避免 RPIT 捕获 `cx` 生命周期导致连续两按钮借用冲突）。
    /// 跳转直接驱动 PdfViewer 实体（`move_match`），无需宿主介入。
    #[allow(clippy::too_many_arguments)] // 导航按钮：命中实体 + 几何 + 配色均为正交参数，拆结构体反添噪音
    fn sidebar_pdf_nav_btn(
        &self,
        window: &mut Window,
        symbol: &'static str,
        delta: i64,
        target: gpui::Entity<pdf::PdfViewer>,
        fg: gpui::Hsla,
        active_bg: gpui::Hsla,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let id = if delta < 0 {
            "sidebar-search-pdf-prev"
        } else {
            "sidebar-search-pdf-next"
        };
        // P2-9：tooltip 文案（上一处 / 下一处匹配）。
        let label = if delta < 0 { "上一处匹配" } else { "下一处匹配" };
        // P2-10：hover 状态跨帧记录 → 背景淡入。
        let hover = window.use_keyed_state(format!("{id}-hover"), cx, |_, _| false);
        let hovered = *hover.read(cx);
        // P2-9：tooltip 状态（同按钮 id 键控）。
        let tip = ui::tooltip::tooltip_state(id, window, cx);
        let tip_on = ui::tooltip::tooltip_visible(&tip, cx);
        let view_id = window.current_view();
        let btn = div()
            .id(SharedString::from(id))
            .debug_selector(move || SharedString::from(id).into())
            .flex()
            .items_center()
            .justify_center()
            .w(px(28.))
            .h(px(24.))
            .mx(px(2.))
            .rounded_md()
            .text_size(px(16.))
            .text_color(fg)
            .cursor(CursorStyle::PointingHand)
            .on_hover({
                let st = hover.clone();
                let tip = tip.clone();
                move |h, _w, cx| {
                    st.update(cx, |v, _| *v = *h);
                    ui::tooltip::tooltip_hover(&tip, *h, cx);
                    cx.notify(view_id);
                }
            })
            .on_click(cx.listener(move |_this: &mut Self, _ev, _w, cx| {
                target.update(cx, |v, cx| v.move_match(delta, cx));
                cx.notify();
            }))
            .child(symbol)
            // P2-9：tooltip 浮层（贴导航按钮右缘垂直居中；按钮 28×24）。
            .child(ui::tooltip::tooltip_layer(
                id, label, tip_on, 28.0, 24.0, cx,
            ));
        hover_bg_fade(btn, format!("{id}-bg"), active_bg, hovered).into_any_element()
    }

    /// PDF 搜索头部「n 处匹配」计数（计数在左、导航靠右；计数过长尾部省略）。
    /// 调用方已处理 `count == 0` 的空态分支，这里只渲染命中计数头。
    #[allow(clippy::too_many_arguments)] // 双色 + 计数来源 + 导航按钮，与 render_* 同构
    fn sidebar_pdf_search_summary(
        count: usize,
        truncated: bool,
        fg: gpui::Hsla,
        prev: impl IntoElement,
        next: impl IntoElement,
    ) -> gpui::AnyElement {
        let text = if truncated {
            format!("{count} 处匹配（已截断）")
        } else {
            format!("{count} 处匹配")
        };
        div()
            .id("sidebar-search-pdf-head")
            .px_3()
            .py_2()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_overflow(gpui::TextOverflow::Truncate("…".into()))
                    .text_size(px(12.))
                    .text_color(fg)
                    .child(text),
            )
            .child(
                div()
                    .id("sidebar-search-pdf-nav")
                    .flex_shrink_0()
                    .flex()
                    .flex_row()
                    .items_center()
                    .child(prev)
                    .child(next),
            )
            .into_any_element()
    }

    /// PDF 搜索命中行（每个命中一行）：`L N  前5字…目标…后5字`，单行显示、超出省略；
    /// 当前激活命中高亮，点击跳转到该命中页（直接驱动 PdfViewer `focus_match_on_page`）。
    #[allow(clippy::too_many_arguments)] // 三色 + 数据源 + 命中列表，与 render_* 同构
    fn sidebar_pdf_hit_rows(
        &self,
        window: &mut Window,
        pdf: &gpui::Entity<pdf::PdfViewer>,
        snippets: &[(usize, String)],
        active: usize,
        fg: gpui::Hsla,
        muted: gpui::Hsla,
        active_bg: gpui::Hsla,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let mut hit_rows: Vec<gpui::AnyElement> = Vec::new();
        for (idx, (page, snippet)) in snippets.iter().enumerate() {
            let page = *page;
            let color = if idx == active { fg } else { muted };
            let row_pdf = pdf.clone();
            // P2-10：hover 状态跨帧记录 → 行背景淡入。
            let hover = window.use_keyed_state(format!("ss-pdf-hit-{idx}-hover"), cx, |_, _| false);
            let hovered = *hover.read(cx);
            let view_id = window.current_view();
            let r = div()
                .id(SharedString::from(format!("sidebar-search-pdf-hit-{idx}")))
                .debug_selector(move || format!("sidebar-search-pdf-hit-{idx}"))
                .px_2()
                .py_1()
                .mx(px(4.))
                .rounded_md()
                .flex()
                .flex_row()
                .items_center()
                .text_size(px(12.))
                .text_color(color)
                .cursor(CursorStyle::PointingHand)
                .on_hover({
                    let st = hover.clone();
                    move |h, _w, cx| {
                        st.update(cx, |v, _| *v = *h);
                        cx.notify(view_id);
                    }
                })
                .on_click(cx.listener(move |_this: &mut Self, _ev, _w, cx| {
                    row_pdf.update(cx, |v, cx| v.focus_match_on_page(page, cx));
                    cx.notify();
                }))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_overflow(gpui::TextOverflow::Truncate("…".into()))
                        .child(format!("L{}  {}", page + 1, snippet)),
                );
            hit_rows.push(
                r.bg(if hovered { active_bg } else { active_bg.opacity(0.) })
                    .into_any_element(),
            );
        }
        hit_rows
    }

    /// 输入变化 → 登记防抖待提交词并重启防抖计时器（`SeqAfter` 代序作废旧定时器）。
    /// 每键只做 O(1) 更新 + 宿主 forwarder 即时通知刷新输入框渲染；[`SEARCH_DEBOUNCE_MS`]
    /// 期满且无新输入才把 `search_pending` 提交为 `search_committed` 并 notify（触发一次
    /// 扫描）。连续输入（间隔 < 防抖窗）时扫描零触发——大文档不再每键全量重扫正文。
    /// 宿主的输入框订阅（`init.rs`）转发到本方法；**提交由实体弱句柄自更新 + 自 notify**
    /// （写共享模型，不回流宿主）。
    pub fn schedule_search_debounce(&mut self, value: String, cx: &mut Context<Self>) {
        self.model.borrow_mut().search_pending = normalize_search_query(&value);
        let weak = cx.weak_entity();
        self.model.borrow().search_debounce.after(
            cx,
            std::time::Duration::from_millis(SEARCH_DEBOUNCE_MS),
            move |app| {
                let _ = weak.update(app, |this, cx| {
                    // 期间有新输入 → 代序已前进，本定时器作废（防抖可重入）。
                    // SC-3：先提纯 owned 再比较（if 条件里的 borrow 临时 Ref 存活整分支）。
                    let pending = std::mem::take(&mut this.model.borrow_mut().search_pending);
                    let search_committed = this.model.borrow().search_committed.clone();
                    if search_committed != pending {
                        this.model.borrow_mut().search_committed = pending;
                        cx.notify();
                    }
                });
            },
        );
    }

    /// 侧边栏搜索「扫描并缓存」：查询词 + 活动文档未变时命中缓存（O(1) Rc 克隆，
    /// 渲染帧不重扫）；否则对文件名（跨全部打开文件）与活动文档行内容扫一次。
    /// 行内容两条来源：只读预览直接借 `content` 镜像（零物化）；可编辑正文 `value()`
    /// 一次性物化后借 `&str` 逐行扫（`scan_line_hits`）——同一轮搜索只物化一次、逐行借用，
    /// 不再每行新分配小写临时串。匹配：ASCII 查询一次性小写全文副本 + std `str::find`
    /// （memmem 快路径，零逐行分配）；非 ASCII 查询词回退 [`find_ci`] 字符级折叠扫描。
    /// 结果缓存在共享模型 `search_scan`，编辑/加载/切文档时宿主清空重扫（交互零变化）。
    fn sidebar_search_scan(&mut self, q: &str, cx: &App) -> Rc<SidebarSearchScan> {
        // SC-3：先提纯 owned Rc 再 if-let（`search_scan` 在共享模型，if-let 掮获的 Ref
        // 临时会跨过 `borrow_mut` 判定，运行期双借 panic）。
        let scan = self.model.borrow().search_scan.clone();
        if let Some(s) = scan
            && s.tab == self.model.borrow().active_tab && s.query == q {
                return s;
            }
        self.model.borrow_mut().search_scan_count += 1;

        let mut file_tabs: Vec<usize> = Vec::new();
        let mut line_hits: Vec<SearchLineHit> = Vec::new();
        // 文件名匹配（跨所有打开文件）
        for (ti, tab) in self.model.borrow().tabs.iter().enumerate() {
            if file_tabs.len() + line_hits.len() >= config::limits::SEARCH_RESULT_CAP {
                break;
            }
            let name = tab
                .file_path
                .as_ref()
                .and_then(|p| p.file_name())
                .map(|s| s.to_string_lossy())
                .unwrap_or_default();
            if contains_ci(&name, q) {
                file_tabs.push(ti);
            }
        }
        // 行内容命中（仅当前活动文档；readonly 走 content 镜像，可编辑走编辑器 Rope
        // 一次物化）。文件名已占的配额扣掉后，剩余额度交给行内容扫描（共用 CAP）。
        let remaining = config::limits::SEARCH_RESULT_CAP.saturating_sub(file_tabs.len());
        if remaining > 0
            && let Some(tab) = self.model.borrow().tabs.get(self.model.borrow().active_tab) {
                if tab.readonly_preview {
                    // 只读预览：直接借内容镜像（零物化；打开时已 LF 归一化）。
                    scan_line_hits(tab.content.as_ref(), q, remaining, &mut line_hits);
                } else {
                    // 可编辑正文：`value()` 一次性物化（LF 归一化，与预测/命中一致）。
                    // 相比逐行迭代 Rope（`iter_lines` 每行 `slice_line` O(log N)/行 →
                    // 139k 行 debug 实测超 1.7s；`lines(LF)` 虽 O(N) 但 debug 全遍历
                    // 仍 ~0.7s），物化 6.4MB ≈ 5ms、一次换取 O(N) 纯 `&str` 扫描——
                    // 防抖后每次只付一次，远快于逐行 Rope 迭代。整篇只物化一次后
                    // 借 `&str` 逐行扫（见 `scan_line_hits`：命中前零分配，唯一每行
                    // 分配是命中片段的 `trim` 副本，≤ remaining 次）。
                    let content = tab.editor.read(cx).value();
                    scan_line_hits(&content, q, remaining, &mut line_hits);
                }
            }

        let scan = std::rc::Rc::new(SidebarSearchScan {
            query: q.to_string(),
            tab: self.model.borrow().active_tab,
            file_tabs,
            line_hits,
        });
        self.model.borrow_mut().search_scan = Some(scan.clone());
        scan
    }
}
