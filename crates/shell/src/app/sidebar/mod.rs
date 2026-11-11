//! 侧边栏（2026-11-11 起为 **facade 模块**）。
//!
//! 本模块原承担侧栏「类型 / 状态 / chrome 外壳渲染 / toc 目录视图」；类型、外壳渲染与
//! 目录视图已整迁 `crates/shell_sidebar`（纯结构移动，行为零变），此处保留：
//!
//! - 对 `shell_sidebar` 的类型/常量/纯函数 **`pub use` 重导出**，维持既有
//!   `sidebar::SidebarChromeState` / `SidebarView` / `FileTreeState` 等路径与沙盒
//!   （`search.rs` / `files.rs` / `files_ops.rs` 下一阶段随迁，`app::tests` 引用不变）。
//! - 侧栏搜索**纯扫描层**（`shell_scan` / `shell_core::SearchScopeTab`）重导出（不变）。
//! - 宿主侧**接线**（`impl EditorApp`）：
//!   [`EditorApp::render_body_sidebar`]（右列 + 侧栏实体组装整行）与 [`EditorApp::render_body_card`]
//!   （正文无缝方块）——两者牵扯选项卡栏 / 编辑区，宿主编排留在 shell；
//!   `toggle_sidebar_view` / `toggle_sidebar_toc_collapse` 薄转发（测试入口，逻辑在域实体）。
//!
//! `EditorApp` 的侧栏状态**仍是普通字段 `sidebar: SidebarChromeState`**（测试字段读写
//! 不变量），渲染/事件的实体侧由 `sidebar_view: Entity<shell_sidebar::Sidebar<EditorApp>>`
//! 承接（见 [`crate::app::mod`] 字段注释）。

use std::rc::Rc;

use super::file_tree_prompt::FileTreePromptKind;
use super::*;
use config::limits::{FOLDER_SEARCH_RESULT_CAP, SEARCH_RESULT_CAP};
use config::user::UserPrefs;
use gpui::{HighlightStyle, StyledText};
use ui::input::Input;
use ui::motion::{hover_bg_fade, hover_spring};

/// 侧边栏搜索**纯扫描层**（`shell_scan`）重导出：`search.rs` / `app` 字段类型 /
/// `app::tests` 沿用既有 `sidebar::…` 与 `use super::*` 路径，纯结构移动不改调用面。
/// （`find_ci` / `FolderFileHit` 仅测试或 crate 内部按名引用，不在此重导出，测试直连
/// `shell_scan`。）
pub(super) use shell_scan::{
    contains_ci, folder_scan_collect, highlight_ranges, scan_line_hits, FolderScanUpdate,
    FolderSearchScan, SearchLineHit, SidebarSearchScan,
};
// SC-3：搜索**范围**枚举随 `EditorApp::search_scope` 迁入 shell_core（`AppModel` 字段
// 类型需跨 crate 命名），此处重导出维持 `sidebar::SearchScopeTab` / 子模块（`use super::*`）
// 与测试（`crate::app::sidebar::SearchScopeTab`）的既有路径。
pub(super) use shell_core::SearchScopeTab;

// 侧栏域 crate 重导出（shell_sidebar）：状态 / 类型 / 常量 / 纯函数。`pub` 者被
// `crate::app` 二次 `pub use`（`shell::app::SidebarView` 等经 §8.6 外提链）；
// `pub(super)` 者供 shell 仍留的 `files.rs` / `search.rs` 子模块（`use super::*`）使用。
pub use shell_sidebar::{
    FileTreeState, SidebarChromeState, SidebarView, TreeEntry,
};
pub(super) use shell_sidebar::{
    read_dir_entries, normalize_search_query, FileRow, FILE_TREE_INDENT, FILE_TREE_ROW_H,
    FILES_FILTER_DEBOUNCE_MS, SEARCH_DEBOUNCE_MS, TOC_BAND_BUFFER, TOC_VIRTUALIZE_MIN,
};

mod files;
mod files_ops;
mod search;

impl EditorApp {
    /// 当前激活选项卡若为 PDF：返回该 PDF 路径的书签列表（宿主渲染期预取，供侧栏目录实体
    /// TOC 渲染——域渲染期禁再入宿主，数据以参数传入）。
    fn active_pdf_bookmarks(&self, cx: &gpui::App) -> Option<Vec<shell_core::PdfBookmark>> {
        let model = self.model.borrow();
        let tab = model.tabs.get(model.active_tab)?;
        match &tab.reader {
            shell_core::MediaReader::Pdf(pdf) => {
                let p = pdf.read(cx).path().to_path_buf();
                self.pdf_bookmarks_for(&p)
            }
            _ => None,
        }
    }

    /// 活动栏按钮点按：未在该面板 → 展开并切到该面板；
    /// 已在该面板再点一次 → **只收起面板**，侧边栏保留为窄活动栏（不留空白宽区）。
    /// 实体侧逻辑见 `shell_sidebar::Sidebar::toggle_sidebar_view`（状态纯变换）；
    /// notify 在本宿主上下文（实体经 host.update 再入本宿主会在 `app.update` 闭包内
    /// RefCell 双借 panic）。
    #[doc(hidden)]
    pub fn toggle_sidebar_view(&mut self, view: SidebarView, cx: &mut Context<Self>) {
        let sb = &mut self.sidebar;
        self.sidebar_view.update(cx, |v, _| v.toggle_sidebar_view(sb, view));
        cx.notify();
    }

    /// 折叠/展开某个目录父项（仅「有下级」的条目可折叠）。实体侧逻辑见
    /// `shell_sidebar::Sidebar::toggle_sidebar_toc_collapse`（域内算 has_child + 状态纯变换）。
    /// 生产折叠点击现走实体 `toc_collapse_click`（不进本方法）；本方法仅 `app::tests`
    /// 程序化断言用，故 `#[allow(dead_code)]`（与既有「测试可见性」入口同口径）。
    /// 语义对齐 HEAD：`toc_collapse` 变换返回 `false`（无子级提前返回）时**不 notify**
    /// （与 `host_impl.rs` 的 `SidebarHost::sidebar_toc_collapse` 同口径）；notify 在本宿主
    /// 上下文（同 `toggle_sidebar_view` 注释）。
    #[allow(dead_code)]
    pub(crate) fn toggle_sidebar_toc_collapse(&mut self, idx: usize, cx: &mut Context<Self>) {
        let sb = &mut self.sidebar;
        let changed = self
            .sidebar_view
            .update(cx, |v, cx| v.toggle_sidebar_toc_collapse(sb, idx, cx));
        if changed {
            cx.notify();
        }
    }

    /// 开始拖拽侧边栏右缘：记录 (起始 x, 起始宽)，关闭自适应由用户接管。
    /// 状态纯变换在 `shell_sidebar::state::resize_start`（迁移落点）；本方法仅 `app::tests`
    /// 入口（生产拖拽起始经调宽手柄 on_mouse_down → 宿主服务），故 `#[allow(dead_code)]`。
    #[allow(dead_code)]
    pub(crate) fn start_sidebar_resize(&mut self, start_x: f32, cx: &mut Context<Self>) {
        shell_sidebar::state::resize_start(&mut self.sidebar, start_x);
        cx.notify();
    }

    /// 拖拽中：按鼠标位移更新侧边栏宽（自变量窗口坐标 x）。宽度 clamp 在 [MIN, MAX]。
    /// 状态纯变换在 `shell_sidebar::state::resize_update`；本方法仅 `app::tests` 入口
    /// （生产拖拽中经外层 on_mouse_move），故 `#[allow(dead_code)]`。
    #[allow(dead_code)]
    pub(crate) fn update_sidebar_resize(&mut self, x: f32, cx: &mut Context<Self>) {
        if shell_sidebar::state::resize_update(&mut self.sidebar, x) {
            cx.notify();
        }
    }

    /// 结束拖拽：宽度定稿（`shell_sidebar::state::resize_end`）+ 持久化。
    /// 本方法仅 `app::tests` 入口（生产拖拽结束经外层 on_mouse_up），故 `#[allow(dead_code)]`。
    #[allow(dead_code)]
    pub(crate) fn end_sidebar_resize(&mut self, cx: &mut Context<Self>) {
        if shell_sidebar::state::resize_end(&mut self.sidebar) {
            self.persist_prefs();
            cx.notify();
        }
    }

    /// 内容区整行：侧边栏展开时在左侧停靠（紧贴标题栏下），右侧为一「右列」——
    /// 上方选项卡栏 + 下方正文 `body`（正文块承载）；收起时仅返回右列本身。
    ///
    /// 无缝直连外壳：侧边栏（mica 底色）与右列（选项卡栏 + 正文块）之间无缝隙/圆角，
    /// 仅靠侧边栏右缘一条发丝线分隔；正文右缘贴窗、编辑区直落选项卡栏之下，状态栏
    /// 贴窗底部与正文块直连。右缘调宽手柄内嵌于侧边栏右缘（实体 `render_sidebar_shell`）。
    pub(crate) fn render_body_sidebar(
        &mut self,
        body: gpui::AnyElement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        // 右列：上方选项卡栏 + 下方编辑区（正文块承载；列容器 flex_col flex_1）。
        let right_col = div()
            .id("app-right-column")
            .debug_selector(|| "app-right-column".into())
            .flex()
            .flex_col()
            .flex_1()
            .min_w(px(0.))
            .min_h(px(0.))
            .child(self.render_tab_bar(window, cx))
            .child(self.render_body_card(body, window, cx));

        if !self.sidebar.open {
            return right_col.into_any_element();
        }
        // 面板内容：目录 → 域实体（shell_sidebar::toc，书签宿主预取传入）；搜索 / 文件树
        // → 仍留 shell（search.rs / files.rs）。顺序=「先构建内容（目录会自适应更新
        // sidebar.width），再取宽」——实体外壳渲染读更新后的宽度。
        let content: Option<gpui::AnyElement> = if self.sidebar.panel_open {
            Some(match self.sidebar.view {
                SidebarView::Toc => {
                    let bookmarks = self.active_pdf_bookmarks(cx);
                    let sb = &mut self.sidebar;
                    self.sidebar_view.update(cx, |v, cx| {
                        v.render_sidebar_toc(sb, bookmarks, window, cx)
                    })
                }
                SidebarView::Search => self.render_sidebar_search(window, cx),
                SidebarView::Files => self.render_sidebar_files(window, cx),
            })
        } else {
            None
        };
        // 侧栏外壳（活动栏 + 右缘调宽手柄 + 面板槽）由域实体渲染。
        let sb = &mut self.sidebar;
        let sidebar_el = self
            .sidebar_view
            .update(cx, |v, cx| v.render_sidebar_shell(sb, content, window, cx));
        div()
            .id("app-body-with-sidebar")
            .debug_selector(|| "app-body-with-sidebar".into())
            .flex()
            .flex_row()
            .flex_1()
            .min_h(px(0.))
            // 侧边栏右缘拖拽调宽：鼠标移动/松开事件冒泡到整个内容容器统一处理。
            // （原注释保留，说明不改动；此容器现也包裹右列选项卡栏+编辑区。）
            // 拖动中/结束的状态纯变换在 `shell_sidebar::state`（迁移落点）；本 listener
            // 已持 `&mut EditorApp`，直接落 `self.sidebar` + 自身 notify（不再经实体/宿主
            // trait 反调——再入本宿主会 RefCell 双借 panic）。宽度持久化在宿主（读模型 prefs）。
            .on_mouse_move(cx.listener(
                |this: &mut EditorApp, ev: &gpui::MouseMoveEvent, _w, cx| {
                    if shell_sidebar::state::resize_update(&mut this.sidebar, ev.position.x.as_f32()) {
                        cx.notify();
                    }
                },
            ))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this: &mut EditorApp, _: &gpui::MouseUpEvent, _w, cx| {
                    if shell_sidebar::state::resize_end(&mut this.sidebar) {
                        this.persist_prefs();
                        cx.notify();
                    }
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this: &mut EditorApp, _: &gpui::MouseUpEvent, _w, cx| {
                    if shell_sidebar::state::resize_end(&mut this.sidebar) {
                        this.persist_prefs();
                        cx.notify();
                    }
                }),
            )
            .child(sidebar_el)
            .child(right_col)
            .into_any_element()
    }

    /// 正文区无缝方块：右缘贴窗直角、其它三缘与侧栏/选项卡栏/状态栏零间距直连，
    /// 无圆角、无边框（侧栏右缘发丝线、标题栏/选项卡栏底边发丝线与状态栏顶边负责分隔）。
    /// 卡面用正文底色（`panel_bg`，与编辑器/预览无缝）。
    fn render_body_card(
        &mut self,
        body: gpui::AnyElement,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        div()
            .id("app-main-card")
            .debug_selector(|| "app-main-card".into())
            // 关键：包装层必须是 flex 容器（列向），否则内部 render_body 的 .flex_1()
            // 在非 flex 的块盒里失效 → 编辑区高度坍缩为内容高，滚动条/点击命中全部错位。
            .flex()
            .flex_col()
            .flex_1()
            .min_w(px(0.))
            .min_h(px(0.))
            .ml(px(0.))
            .mr(px(0.))
            .mt(px(0.))
            .mb(px(0.))
            // 顶边 1px 发丝线 = 选项卡栏↔编辑区之分隔（由后绘制的编辑区绘制，
            // 保证不受上层选项卡栏遮挡）；其余三边由侧栏右缘/状态栏顶边承担，不重复。
            .border_t_1()
            .border_color(self.hairline())
            .bg(ui::shell::body_card_face(cx))
            .overflow_hidden()
            .child(body)
            .into_any_element()
    }
}
