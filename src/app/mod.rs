//! 应用主视图 `EditorApp`：文本 / 代码 / Markdown 编辑器（多文件选项卡）。
//!
//! 布局（自上而下）：
//! - 菜单栏：文件 / 编辑 / 查看 / 帮助，点击弹出下拉菜单（[`EditorApp::render_menu_bar`]）
//! - 选项卡栏：每个文件一个选项卡，可新建（+）、切换、关闭（未保存时弹窗提示保存）
//! - 编辑区：**gpui-component 的 `Input`（CodeEditor 模式）**，内置语法高亮、
//!   行号、缩进参考线、搜索面板、撤销重做与滚动（替换原先自研 `TextArea`）
//! - 预览区（开启预览时）：右侧分屏实时渲染 Markdown（复用 [`markdown`]）
//! - 状态栏：文件名 / 脏标记 / 语言徽标 / 行列 / 行数 / 字数 / 字号调节
//!
//! 打开 / 保存 / 另存为均调用**系统原生文件对话框**（`rfd`），无需手动输入路径；
//! 也支持把文件从资源管理器拖拽到窗口/编辑区打开。
//!
//! 文本编辑动作（backspace/ctrl-v/undo…）由 gpui-component 的 `Input` 在 `Input`
//! 键上下文中自行处理，这里只订阅其 `InputEvent::Change` 刷新状态栏与脏标记。

mod file_ops;
mod open_progress;
mod menu;
mod panes;
mod sidebar;
mod status_bar;
mod tabs;

#[cfg(test)]
mod tests;

use std::any::Any;
use std::fs;
use std::path::{Path, PathBuf};

use gpui::{
    div, px, rgb, rgba, white, AnyWindowHandle, App, AppContext, Context, CursorStyle,
    DragMoveEvent, ExternalPaths, FocusHandle, Focusable, InteractiveElement, IntoElement,
    MouseButton, ParentElement, PromptLevel, Render, SharedString, StatefulInteractiveElement,
    Styled, Subscription, VisualContext, Window, WindowBounds, WindowOptions,
};
use gpui_component::input::{InputEvent, InputState, Rope};
use gpui_component::{Icon, IconName};

use crate::actions::*;
use crate::config::layout::{
    DEFAULT_FONT_SIZE, FONT_MAX, FONT_MIN, MENU_BAR_H, SIDEBAR_WIDTH, SPLIT_RATIO_MAX,
    SPLIT_RATIO_MIN,
};
use crate::scrollbar::TableScrollState;

/// 一个文件选项卡：独立持有自研编辑器组件状态、文件路径与脏标记。
struct EditorTab {
    /// 本选项卡的编辑区（[`crate::editor::CodeEditor`]）。
    editor: gpui::Entity<crate::editor::CodeEditor>,
    /// 当前文件路径（None = 未命名）
    file_path: Option<PathBuf>,
    /// 内容自上次打开/保存后是否被修改
    dirty: bool,
    /// 当前内容的镜像缓存（在 `InputEvent::Change` 时同步，避免每帧克隆 Rope）
    content: SharedString,
    /// 行数缓存
    line_count: usize,
    /// 字数缓存
    char_count: usize,
    /// 光标行（0 基）
    cursor_row: usize,
    /// 光标列（0 基）
    cursor_col: usize,
    /// 自研 Markdown 渲染器状态（行级虚拟化）。
    md_preview: gpui::Entity<crate::markdown::MarkdownPreview>,
    /// PDF 阅读器（打开 .pdf 时创建）：整页原样显示；None = 非 PDF 选项卡。
    pdf_preview: Option<gpui::Entity<crate::pdf::PdfViewer>>,
    /// 是否开启 Markdown 预览
    preview_on: bool,
    /// 预览是否占满整个内容区（EPUB 阅读等场景，不显示编辑区/分割线）
    preview_fullscreen: bool,
    /// 预览是否只读锁定（EPUB：正文只在预览区，编辑区为空，不能关闭/翻转预览）
    readonly_preview: bool,
    /// 大文档异步打开：预览先出、编辑器暂缓挂载（首帧显示轻量占位，
    /// 等用户第一时间看到预览内容后，下一拍再把编辑器挂上——编辑器
    /// 首次对 16 万行做 DisplayMap 布局会冻结一两秒，不让它挡在首帧前）。
    editor_pending: bool,
    /// 预览**增量解析**的基底：上次同步的整篇快照（编辑器正文 Rope 的 O(1) COW 克隆，
    /// 不物化 6.4MB 全文）与其块索引。下一次去抖后用
    /// [`crate::markdown::scan_blocks_incremental_rope`] 复用（只重扫变化区）。
    preview_prev: Option<(Rope, std::rc::Rc<crate::markdown::BlockIndex>)>,
    /// 编辑版本号：每次内容变更 +1。预览同步循环用它判断「扫描期间是否又有新编辑」，
    /// 从而在扫描结束后自动续跑合并，而不是丢给下一次按键才触发。
    edit_version: u64,
    /// 去抖窗口内累积的「dirty 字节区间」（bounding box，当前文本字节坐标）：供
    /// [`crate::markdown::scan_blocks_incremental_rope`] 跳过 O(n) 快照 diff；同步取走时清空。
    /// 数据来自 [`crate::editor::CodeEditor::take_last_edit`]（每轮 Change 取走）。
    preview_dirty: Option<(usize, usize)>,
    /// 监听编辑事件 → 置脏 + 刷新状态栏（保持有效）
    _edit_sub: Subscription,
}

/// 一个从窗口中「拔出」的选项卡快照：编辑器 / 预览等实体句柄共享（GPUI 实体
/// 与窗口无关），连同文件元数据一起跨窗口搬迁。`_edit_sub` 是源窗口持有的订阅，
/// 拔掉时随源 `EditorTab` 一并丢弃，目标窗口重新建立订阅。
pub(crate) struct DetachedTab {
    editor: gpui::Entity<crate::editor::CodeEditor>,
    md_preview: gpui::Entity<crate::markdown::MarkdownPreview>,
    pdf_preview: Option<gpui::Entity<crate::pdf::PdfViewer>>,
    file_path: Option<PathBuf>,
    dirty: bool,
    content: SharedString,
    line_count: usize,
    char_count: usize,
    cursor_row: usize,
    cursor_col: usize,
    preview_on: bool,
    preview_fullscreen: bool,
    readonly_preview: bool,
    editor_pending: bool,
    preview_prev: Option<(Rope, std::rc::Rc<crate::markdown::BlockIndex>)>,
    edit_version: u64,
    preview_dirty: Option<(usize, usize)>,
}

/// 选项卡拖拽携带的数据：目标选项卡下标 + 当前显示名（拖拽幽灵视图用它画“小胶囊”）。
#[derive(Clone, Debug)]
pub(crate) struct TabDragData {
    idx: usize,
    name: SharedString,
}

/// 拖拽选项卡时跟随鼠标的“幽灵”小胶囊视图。
struct TabDragGhost {
    name: SharedString,
}

impl Render for TabDragGhost {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("tab-drag-ghost")
            .debug_selector(|| "tab-drag-ghost".into())
            .px_3()
            .py_1()
            .rounded_md()
            .bg(rgb(crate::config::colors::RESIZE_ACCENT))
            .text_color(white())
            .text_size(px(12.))
            .child(self.name.clone())
    }
}

/// 所有 `EditorApp` 窗口的注册表（全局）：记录每个窗口的弱实体句柄与窗口句柄。
/// 「拖出拆分」创建的子窗口带 `is_detached=true`，把选项卡拖回主窗口时据此找到
/// 主窗口（第一个非 detached 的 EditorApp 窗口）。
struct EditorAppRegistry {
    windows: Vec<(gpui::WeakEntity<EditorApp>, AnyWindowHandle)>,
}

impl Default for EditorAppRegistry {
    fn default() -> Self {
        Self {
            windows: Vec::new(),
        }
    }
}

impl gpui::Global for EditorAppRegistry {}

pub struct EditorApp {
    /// 所有文件选项卡
    tabs: Vec<EditorTab>,
    /// 当前激活的选项卡下标
    active_tab: usize,
    /// 选项卡栏横向滚动状态（标签过多时横向滚动 + 滚动条）
    tab_bar_hscroll: TableScrollState,
    /// 自动隐藏定时器的代序号：只清除同代定时器，避免旧定时器误清新滚动。
    tab_hscroll_seq: u64,
    /// 顶部状态文字与是否"正常"
    status: SharedString,
    status_ok: bool,
    /// 编辑器正文字号（px，用户可调）
    font_size: f32,
    /// Markdown 预览重渲染去抖是否已挂起（避免打字时逐键整篇重解析）。
    preview_pending: bool,
    /// 预览同步是否正在扫描/装配中：为 true 时不再起新一轮（防多轮 500ms 后台扫描
    /// 叠在一起；同步循环结束后复位，其间若有新编辑会自动续跑合并）。
    preview_syncing: bool,
    /// 预览同步的代序号：每轮去抖发起时 +1；后台 `scan_blocks` 结果只允许最新代
    /// 落盘，旧代迟到结果直接丢弃（防旧内容覆盖新编辑）。
    preview_seq: u64,
    /// 是否深色（黑）模式：控制组件主题、自绘色板与原生标题栏深浅
    dark_mode: bool,
    /// 编辑区 / 预览区分屏比例（0..1，= 编辑区占宽比例）
    split_ratio: f32,
    /// 编辑区是否自动换行（默认换行）。开关只作用编辑区；Markdown 预览
    /// 始终自动换行，不随此偏好变化（预览换行由文件类型/全屏阅读模式决定）。
    wrap_enabled: bool,
    /// 正在拖拽分屏分割线：`Some((起始指针x, 起始比例))`
    split_drag: Option<(f32, f32)>,
    /// 分割线是否被鼠标悬停（悬停时高亮提示可拖拽）
    splitter_hovered: bool,
    /// 侧边栏是否展开。
    sidebar_open: bool,
    /// 侧边栏当前视图（目录 / 搜索）。
    sidebar_view: sidebar::SidebarView,
    /// 侧边栏内容面板是否展开（false = 只留窄活动栏，目录/搜索面板收起）。
    sidebar_panel_open: bool,
    /// 侧边栏「搜索」关键字输入控件。
    search_state: gpui::Entity<InputState>,
    /// 侧边栏目录折叠集合（仅当前激活文档，切换文档时清空）。
    sidebar_toc_collapsed: std::collections::HashSet<usize>,
    /// 侧边栏目录折叠集合对应的选项卡下标（切换文档即失效清空）。
    sidebar_toc_tab: Option<usize>,
    /// 侧边栏当前宽度（px）：目录视图随内容自适应，也可右缘拖拽手动调整。
    sidebar_width: f32,
    /// 是否仍随目录内容自适应宽度（手动拖拽后关闭，切换文档重开）。
    sidebar_width_auto: bool,
    /// 侧边栏右缘拖拽：`Some((起始 x, 起始宽))`。
    sidebar_resize: Option<(f32, f32)>,
    /// 侧边栏右缘是否被鼠标悬停（悬停时高亮提示可拖拽）。
    sidebar_resize_hovered: bool,
    /// 监听搜索框输入变化 → 刷新搜索结果。
    _search_sub: Subscription,
    /// 监听 PDF 阅读器 `notify()`（检索结果/加载进度到达）→ 刷新 app（侧边栏计数）。
    _pdf_observe: Option<gpui::Subscription>,
    /// 已推送给 PDF 阅读器的查询（`(活动标签, 查询词)`）：两者都未变才跳过推送，
    /// 切换标签/重开 PDF 后即使查询相同也会重新推送。
    pdf_search: Option<(usize, String)>,
    /// 侧边栏搜索的惰性扫描缓存：按（查询词, 活动文档）复用，渲染帧不重扫全文。
    /// 查询变化 / 切换文档 / 内容被编辑时清空，下次渲染重建。
    search_scan: Option<std::rc::Rc<sidebar::SidebarSearchScan>>,
    /// 实际执行过的搜索扫描次数（每次命中缓存复用不改）。测试用：验证「查询未变
    /// 时反复渲染不重扫」这一优化真实生效。
    pub(crate) search_scan_count: usize,
    /// 大文档「两段式打开」快段（`load_file_text` 延迟挂载编辑器）执行次数。
    /// 测试用：验证大文件先瞬时挂编辑器、再后台补齐统计/预览的完整流程两段都跑了。
    pub(crate) open_fast_count: usize,
    /// 大文档「两段式打开」慢段（`finish_file_text` 应用统计 + 预览索引）执行次数。
    pub(crate) open_finish_count: usize,
    /// 正在拖拽的选项卡下标（None = 未在拖拽选项卡）：`on_drag_move<TabDragData>`
    /// 在拖拽确认后写入；鼠标在窗口内/外松开时据此决定还原原位还是拆分/还原窗口。
    tab_dragging: Option<usize>,
    /// 本窗口是否为「拖出拆分」生成的独立文件窗口。是 → 把选项卡拖出窗口 = 还原回主窗口；
    /// 否（主窗口）→ 把选项卡拖出窗口 = 拆分出新的独立窗口。
    is_detached: bool,
    focus_handle: FocusHandle,
}

impl Focusable for EditorApp {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EditorApp {
    /// 组件编辑器内部统一使用 LF，避免 Windows 剪贴板 CRLF 进入文本布局。
    fn normalize_newlines(text: &str) -> String {
        text.replace("\r\n", "\n").replace('\r', "\n")
    }

    /// 创建并注册 `EditorApp` 实体（作为窗口的第一层视图，外层再由
    /// `gpui_component::Root` 包裹）。
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> EditorApp {
        Self::new_impl(window, cx, None, false)
    }

    /// 「拖出拆分」创建独立文件窗口：首个选项卡直接采用拖出的选项卡快照。
    /// 返回的 `EditorApp` 标记为 `is_detached`，它的选项卡再被拖出 = 还原回主窗口。
    pub fn new_detached(
        window: &mut Window,
        cx: &mut Context<Self>,
        first_tab: DetachedTab,
    ) -> EditorApp {
        Self::new_impl(window, cx, Some(first_tab), true)
    }

    fn new_impl(
        window: &mut Window,
        cx: &mut Context<Self>,
        first_tab: Option<DetachedTab>,
        is_detached: bool,
    ) -> EditorApp {
        let focus_handle = cx.focus_handle();
        // 侧边栏搜索输入框：内容变化 → 重刷 app（从而重算并展示搜索结果）
        let search = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("搜索文件与内容…")
                .default_value("")
        });
        let search_state = search.clone();
        let search_sub = cx.subscribe(&search, |_this, _emitter, ev: &InputEvent, cx| {
            if let InputEvent::Change = ev {
                cx.notify();
            }
        });
        let mut app = EditorApp {
            tabs: Vec::new(),
            active_tab: 0,
            tab_bar_hscroll: TableScrollState::new(),
            tab_hscroll_seq: 0,
            status: SharedString::from("就绪"),
            status_ok: true,
            font_size: DEFAULT_FONT_SIZE,
            preview_pending: false,
            preview_syncing: false,
            preview_seq: 0,
            dark_mode: false,
            split_ratio: 0.5,
            // 编辑区软换行默认关闭；仅 Markdown 文件的编辑区默认换行（load_file_text 显式开启）。
            wrap_enabled: false,
            split_drag: None,
            splitter_hovered: false,
            sidebar_open: true,
            sidebar_view: sidebar::SidebarView::Toc,
            // 目录面板默认收起（仅留 46px 活动栏，不挤占正文）；
            // 点活动栏「目录」按钮再展开。
            sidebar_panel_open: false,
            search_state,
            sidebar_toc_collapsed: Default::default(),
            sidebar_toc_tab: None,
            sidebar_width: SIDEBAR_WIDTH,
            sidebar_width_auto: true,
            sidebar_resize: None,
            sidebar_resize_hovered: false,
            _search_sub: search_sub,
            _pdf_observe: None,
            pdf_search: None,
            search_scan: None,
            search_scan_count: 0,
            open_fast_count: 0,
            open_finish_count: 0,
            tab_dragging: None,
            is_detached,
            focus_handle,
        };
        app.register_window(window, cx);
        if let Some(first_tab) = first_tab {
            app.adopt_tab(first_tab, Some(window), cx);
        } else {
            app.make_tab(window, cx);
        }

        // 命令行参数：第一个参数作为初始打开文件（复用首个空选项卡，不额外新建）
        if !is_detached {
            let mut args = std::env::args_os();
            args.next(); // 跳过程序名
            if let Some(first) = args.next() {
                let p = PathBuf::from(&first);
                if p.is_file() {
                    app.open_file_in_tab(0, &p.to_string_lossy(), window, cx);
                }
            }
        }
        app
    }

    /// 把本窗口登记进全局 `EditorAppRegistry`（惰性清理已关闭窗口的失效项）。
    fn register_window(&self, window: &mut Window, cx: &mut Context<Self>) {
        if !cx.has_global::<EditorAppRegistry>() {
            cx.set_global(EditorAppRegistry::default());
        }
        let weak = cx.weak_entity();
        let handle = window.window_handle();
        let registry = cx.global_mut::<EditorAppRegistry>();
        registry.windows.retain(|(weak, _)| weak.upgrade().is_some());
        registry.windows.push((weak, handle));
    }

    /// 当前激活选项卡的编辑区焦点句柄（供入口聚焦）。
    pub fn editor_focus_handle(&self, cx: &App) -> FocusHandle {
        self.tabs
            .get(self.active_tab)
            .map(|t| t.editor.read(cx).focus_handle())
            .unwrap_or_else(|| self.focus_handle.clone())
    }

    /// 订阅编辑器变更（置脏 + 刷新状态栏 + 预览增量入口）。
    /// `make_tab` 新建与 `adopt_tab` 跨窗口迁移共用：订阅绑定的是 *本窗口* 的 `EditorApp`，
    /// 因此选项卡迁到哪个窗口，编辑事件就只刷新哪个窗口。
    fn subscribe_editor(
        &self,
        editor: &gpui::Entity<crate::editor::CodeEditor>,
        cx: &mut Context<Self>,
    ) -> Subscription {
        let ed = editor.clone();
        cx.subscribe(&ed, |this, emitter, ev: &crate::editor::EditorEvent, cx| {
            if let Some(tab) = this.tabs.iter_mut().find(|t| t.editor == emitter) {
                if ev == &crate::editor::EditorEvent::Edited {
                    let _t_change = crate::prof::begin();
                    // 内容被编辑 → 侧边栏搜索缓存失效 + 版本号推进
                    this.search_scan = None;
                    tab.edit_version = tab.edit_version.wrapping_add(1);
                    // 编辑侧通道 → dirty 区间（bounding box，当前文本字节坐标）：
                    // CodeEditor 记录最近一次编辑区间（字节），每轮 Change 1:1 取走。
                    if let Some((s, oe, ne)) =
                        tab.editor.update(cx, |ed, _| ed.take_last_edit())
                    {
                        let delta = ne as i64 - oe as i64;
                        tab.preview_dirty = Some(match tab.preview_dirty {
                            None => (s, ne),
                            Some((lo, hi)) => {
                                // 把上一帧的 dirty 平移到本帧（编辑点之前不变、之后随 δ 平移）
                                // 再并入 [s, ne)（新增变化区间），取 bounding box。
                                let hi_shifted = if hi > oe {
                                    hi as i64 + delta
                                } else if hi >= s {
                                    ne as i64
                                } else {
                                    hi as i64
                                };
                                (lo.min(s), hi_shifted.max(ne as i64).max(0) as usize)
                            }
                        });
                    }
                    // 行/字/光标走 CodeEditor 的 O(1) 计数，不再每键物化全文。
                    // 行数用显示语义（`str::lines()` 口径，与文件打开/状态栏一致），
                    // 不用含幻影尾行的编辑器模型 `line_count()`。
                    let (line_count, char_count, cursor_row, cursor_col) = {
                        let st = tab.editor.read(cx);
                        let cursor = st.cursor_position();
                        (st.display_line_count(), st.char_count(), cursor.0, cursor.1)
                    };
                    // 仅在开启预览时才同步 Markdown 预览，且**合成一轮**：
                    // 一次同步任务里循环「快照 → 后台扫描 → 装配 → 检查版本是否有新编辑」，
                    // 有新编辑就续跑，直到追上最新内容才停——打字期间**不会叠起多轮后台扫描**
                    // （`preview_syncing` 闸门），从「最后一次按键」起该轮扫描结束即收敛。
                    if tab.preview_on && !this.preview_pending && !this.preview_syncing {
                        this.preview_pending = true;
                        this.preview_syncing = true;
                        this.preview_seq = this.preview_seq.wrapping_add(1);
                        let seq = this.preview_seq;
                        cx.spawn(async move |this, cx| {
                            loop {
                                let Some((snapshot, panel_bg, base_dir, prev, version, dirty)) =
                                    this
                                        .update(cx, |this, cx| {
                                            let panel_bg = this.panel_bg();
                                            let base_dir = this
                                                .tabs
                                                .get(this.active_tab)
                                                .and_then(|t| t.file_path.as_ref())
                                                .and_then(|p| p.parent())
                                                .map(|d| d.to_path_buf());
                                            let snapshot = this
                                                .tabs
                                                .get(this.active_tab)
                                                .filter(|t| t.preview_on)
                                                .map(|t| t.editor.read(cx).text().clone());
                                            let prev = this
                                                .tabs
                                                .get(this.active_tab)
                                                .and_then(|t| t.preview_prev.clone());
                                            let version = this
                                                .tabs
                                                .get(this.active_tab)
                                                .map(|t| t.edit_version)
                                                .unwrap_or(0);
                                            let dirty = this
                                                .tabs
                                                .get_mut(this.active_tab)
                                                .and_then(|t| t.preview_dirty.take());
                                            snapshot.map(|snap| {
                                                (snap, panel_bg, base_dir, prev, version, dirty)
                                            })
                                        })
                                        .ok()
                                        .flatten()
                                else {
                                    break;
                                };
                                let index = match &prev {
                                    Some((p_src, p_idx)) => {
                                        let p_src = p_src.clone();
                                        let p_idx = (**p_idx).clone();
                                        let snap = snapshot.clone();
                                        let dirty = dirty.map(|(a, b)| a..b);
                                        cx.background_executor()
                                            .spawn(async move {
                                                crate::markdown::scan_blocks_incremental_rope(
                                                    &p_src,
                                                    &p_idx,
                                                    &snap,
                                                    dirty,
                                                )
                                            })
                                            .await
                                    }
                                    None => {
                                        let snap = snapshot.clone();
                                        cx.background_executor()
                                            .spawn(async move {
                                                crate::markdown::scan_blocks_rope(&snap)
                                            })
                                            .await
                                    }
                                };
                                let index = std::rc::Rc::new(index);
                                let keep = this
                                    .update(cx, |this, cx| {
                                        if this.preview_seq != seq {
                                            return false;
                                        }
                                        let mut keep = false;
                                        if let Some(tab) = this.tabs.get_mut(this.active_tab) {
                                            if tab.preview_on {
                                                tab.md_preview.update(cx, |view, cx| {
                                                    view.set_background(panel_bg);
                                                    view.set_base_dir(base_dir);
                                                    view.set_source_with_index(
                                                        crate::markdown::PreviewSrc::Rope(
                                                            snapshot.clone(),
                                                        ),
                                                        index.clone(),
                                                        cx,
                                                    );
                                                });
                                                cx.notify();
                                            }
                                            tab.preview_prev =
                                                Some((snapshot.clone(), index.clone()));
                                            keep = tab.preview_on && tab.edit_version != version;
                                        }
                                        keep
                                    })
                                    .unwrap_or(false);
                                if !keep {
                                    break;
                                }
                            }
                            let _ = this.update(cx, |this, _| {
                                this.preview_pending = false;
                                this.preview_syncing = false;
                            });
                        })
                        .detach();
                    }
                    // 只对 readonly_preview（EPUB/PDF，编辑区为空）镜像全文到 content；
                    // 普通文本/Markdown 以编辑器 Rope 为准，不做整份副本（省内存）。
                    if tab.readonly_preview {
                        let content = tab.editor.read(cx).text().to_string();
                        tab.content = SharedString::from(content);
                    }
                    tab.line_count = line_count;
                    tab.char_count = char_count;
                    tab.cursor_row = cursor_row;
                    tab.cursor_col = cursor_col;
                    if !tab.dirty {
                        tab.dirty = true;
                    }
                    cx.notify();
                    crate::prof::end("tab_change", _t_change);
                }
            }
        })
    }

    /// 从本窗口「拔出」一个选项卡：移除选项卡（**不**释放 PDF 纹理、不重置编辑器——
    /// 实体与元数据整体移到目标窗口），下标与激活态自动校正。
    fn take_tab_payload(&mut self, idx: usize) -> Option<DetachedTab> {
        if idx >= self.tabs.len() {
            return None;
        }
        let tab = self.tabs.remove(idx);
        if idx < self.active_tab {
            self.active_tab -= 1;
        } else if self.active_tab >= self.tabs.len() {
            self.active_tab = self.tabs.len().saturating_sub(1);
        }
        self.search_scan = None; // 选项卡集合变化 → 搜索缓存失效
        Some(DetachedTab {
            editor: tab.editor,
            md_preview: tab.md_preview,
            pdf_preview: tab.pdf_preview,
            file_path: tab.file_path,
            dirty: tab.dirty,
            content: tab.content,
            line_count: tab.line_count,
            char_count: tab.char_count,
            cursor_row: tab.cursor_row,
            cursor_col: tab.cursor_col,
            preview_on: tab.preview_on,
            preview_fullscreen: tab.preview_fullscreen,
            readonly_preview: tab.readonly_preview,
            editor_pending: tab.editor_pending,
            preview_prev: tab.preview_prev,
            edit_version: tab.edit_version,
            preview_dirty: tab.preview_dirty,
        })
    }

    /// 把一个「拔出」的选项卡收纳进本窗口：共享编辑器/预览实体、重建编辑订阅、
    /// 置为激活并同步预览/PDF 主题与软换行偏好。
    /// `window=None` 用于跨窗口还原（此时不抢焦点，由调用方在目标窗口聚焦）；
    /// `Some(window)` 用于拆出新窗口（直接聚焦新窗口的编辑区）。
    pub(super) fn adopt_tab(
        &mut self,
        payload: DetachedTab,
        mut window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) {
        let edit_sub = self.subscribe_editor(&payload.editor, cx);
        let is_md = payload
            .file_path
            .as_ref()
            .map(|p| Self::is_markdown_path(p))
            .unwrap_or(false);
        let idx = self.tabs.len();
        self.tabs.push(EditorTab {
            editor: payload.editor,
            file_path: payload.file_path,
            dirty: payload.dirty,
            content: payload.content,
            line_count: payload.line_count,
            char_count: payload.char_count,
            cursor_row: payload.cursor_row,
            cursor_col: payload.cursor_col,
            md_preview: payload.md_preview,
            pdf_preview: payload.pdf_preview,
            preview_on: payload.preview_on,
            preview_fullscreen: payload.preview_fullscreen,
            readonly_preview: payload.readonly_preview,
            editor_pending: payload.editor_pending,
            preview_prev: payload.preview_prev,
            edit_version: payload.edit_version,
            preview_dirty: payload.preview_dirty,
            _edit_sub: edit_sub,
        });
        self.active_tab = idx;
        self.search_scan = None;
        // 编辑区软换行与目标窗口偏好对齐（md 默认换行、预览开着强制换行）
        let wrap = is_md || self.wrap_enabled || self.tabs[idx].preview_on;
        self.tabs[idx].editor.update(cx, |ed, _cx| {
            ed.set_soft_wrap(wrap);
        });
        // 预览/PDF 主题背景刷新 + PDF 订阅重建
        let canvas_bg = self.window_bg();
        if self.tabs[idx].preview_on {
            self.sync_md_preview(idx, cx);
        }
        if let Some(pdf) = self.tabs[idx].pdf_preview.clone() {
            let pv = pdf.clone();
            pv.update(cx, |view, _| view.set_background(canvas_bg));
            self._pdf_observe = Some(cx.observe(&pdf, |_this, _e, cx| {
                cx.notify();
            }));
        }
        self.pdf_search = None; // 重新推送检索查询
        if let Some(window) = window.as_mut() {
            self.activate_tab(idx, window, cx);
            self.status = SharedString::from("已拆分为独立窗口");
            // 窗口标题跟随当前文件（拆出的新窗口直接以此命名）
            if let Some(p) = self.tabs[idx].file_path.as_ref() {
                window.set_window_title(&p.to_string_lossy());
            } else {
                window.set_window_title("未命名");
            }
        } else {
            self.status = SharedString::from("已还原到主窗口");
        }
        self.status_ok = true;
        cx.notify();
    }

    /// 拖拽中的选项卡在**窗口外**松开：主窗口 → 拆成独立窗口；独立窗口 → 还原回主窗口。
    /// `window.remove_window()`（独立窗口还原后），标志窗口本帧结束后销毁。
    pub(crate) fn release_tab_drag_outside(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(idx) = self.tab_dragging.take() else {
            return;
        };
        if idx >= self.tabs.len() {
            return;
        }
        let Some(payload) = self.take_tab_payload(idx) else {
            return;
        };
        if self.is_detached {
            // 独立文件窗口：把选项卡还给主窗口（找不到主窗口则自我转正为主窗口）
            self.restore_tab_to_primary(payload, cx);
            window.remove_window();
        } else {
            // 主窗口：把选项卡拆进新开的独立窗口
            self.split_tab_to_new_window(payload, window, cx);
        }
    }

    /// 把选项卡拆进一个新开的独立窗口；本窗口若已没有选项卡则补一个空选项卡。
    fn split_tab_to_new_window(
        &mut self,
        payload: DetachedTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.tabs.is_empty() {
            self.make_tab(window, cx);
        }
        // 主窗口剩下的选项卡接管焦点（被拖走的选项卡已从 tabs 移除）
        self.activate_tab(self.active_tab, window, cx);
        let bounds = window.bounds().clone();
        let titlebar_options = gpui::TitlebarOptions {
            title: Some("GPUI 编辑器".into()),
            appears_transparent: true,
            ..Default::default()
        };
        let _ = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(titlebar_options),
                ..Default::default()
            },
            |new_window, cx| {
                let view = cx.new(|cx| EditorApp::new_detached(new_window, cx, payload));
                cx.new(|cx| gpui_component::Root::new(view, new_window, cx))
            },
        );
        cx.notify();
    }

    /// 把选项卡还原到主（非 detached）窗口；找不到主窗口时把本窗口转正为主窗口并原地收纳。
    fn restore_tab_to_primary(&mut self, payload: DetachedTab, cx: &mut Context<Self>) {
        let registry = cx.global::<EditorAppRegistry>();
        let primary: Option<(gpui::Entity<EditorApp>, AnyWindowHandle)> = registry
            .windows
            .iter()
            .filter_map(|(weak, wh)| {
                let entity = weak.upgrade()?;
                let detached = entity.read_with(cx, |app, _| app.is_detached);
                (!detached).then_some((entity, *wh))
            })
            .next();
        let focus_handle = payload.editor.read(cx).focus_handle();
        match primary {
            Some((entity, wh)) => {
                // 把选项卡交给主窗口（数据收纳 + 不抢焦点），随后在主窗口里聚焦编辑区
                entity.update(cx, |app, cx| app.adopt_tab(payload, None, cx));
                if let Some(root_handle) = wh.downcast::<gpui_component::Root>() {
                    let _ = root_handle.update(cx, |_root, window, cx| {
                        window.focus(&focus_handle, cx);
                    });
                }
            }
            None => {
                // 主窗口已不存在（例如被关闭）→ 本窗口转正为主窗口，选项卡原地保留
                self.is_detached = false;
                self.adopt_tab(payload, None, cx);
            }
        }
    }

    /// 选项卡默认文件名（用于保存对话框）。
    fn tab_file_name(&self, idx: usize) -> String {
        self.tabs
            .get(idx)
            .and_then(|t| t.file_path.as_ref())
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "未命名.txt".into())
    }

    /// 当前激活选项卡是否开启预览。
    fn show_preview(&self) -> bool {
        self.tabs
            .get(self.active_tab)
            .map(|t| t.preview_on)
            .unwrap_or(false)
    }

    /// 把指定选项卡的当前内容同步到 Markdown 预览（开启预览时刷新一次）。
    /// 源用 `tab.content`（编辑区内容的镜像，EPUB 打开时正文只存在于 content/预览，
    /// 编辑区为空，不能用 editor.value() 否则会拿空串覆盖预览导致内容消失）。
    fn sync_md_preview(&mut self, idx: usize, cx: &mut Context<Self>) {
        let panel_bg = self.panel_bg();
        // 预览区始终自动换行（Markdown/EPUB，不随编辑区换行偏好）
        let Some(tab) = self.tabs.get_mut(idx) else {
            return;
        };
        let value = if !tab.content.is_empty() {
            tab.content.clone().to_string()
        } else {
            tab.editor.read(cx).value().to_string()
        };
        tab.md_preview.update(cx, |view, cx| {
            view.set_background(panel_bg);
            view.set_base_dir(tab.file_path.as_ref().and_then(|p| p.parent()).map(|d| d.to_path_buf()));
            view.set_wrap(true, cx);
            view.set_source(&value, cx);
        });
    }

    /// 切换指定选项卡的 Markdown 预览（眼睛图标 / 菜单共用）。
    /// 会先激活该选项卡，再翻转其 `preview_on`。
    /// EPUB（`readonly_preview`）：正文只在预览区、编辑区为空，翻转预览无意义，
    /// 点击仅重新同步内容（持久保持全屏预览，不会落入空白编辑区）。
    fn toggle_preview_for_tab(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.activate_tab(idx, window, cx);
        let readonly = self.tabs.get(idx).map(|t| t.readonly_preview).unwrap_or(false);
        let preview_on = {
            let tab = &mut self.tabs[idx];
            if readonly {
                tab.preview_on = true;
                tab.preview_fullscreen = true;
            } else {
                tab.preview_on = !tab.preview_on;
                // 关闭预览时同步退出全屏
                if !tab.preview_on {
                    tab.preview_fullscreen = false;
                    // 增量基底随预览关闭即失效（下次开启重新全量建立）
                    tab.preview_prev = None;
                }
            }
            tab.preview_on
        };
        // 开启预览 → 编辑区强制换行，方便在分屏里编辑 Markdown；
        // 关闭预览 → 恢复默认：Markdown 文件编辑区仍换行（md 默认），其余按全局偏好。
        let is_md = self.is_markdown_tab(idx);
        let wrap = preview_on || is_md || self.wrap_enabled;
        self.tabs[idx].editor.update(cx, |ed, _cx| {
            ed.set_soft_wrap(wrap);
        });
        if preview_on {
            self.sync_md_preview(idx, cx);
        } else {
            // 关闭预览：释放整篇解析出的行数据与测量缓存，内存回到基线
            // （否则隐藏的预览仍占着整篇文档，反复开关只涨不降）。
            self.tabs[idx].md_preview.update(cx, |view, cx| view.release(cx));
        }
        cx.notify();
    }

    /// 切换当前选项卡预览是否全屏占满（EPUB 阅读模式）。
    fn toggle_preview_fullscreen(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let idx = self.active_tab;
        let (fullscreen, preview, editor) = {
            let Some(tab) = self.tabs.get_mut(idx) else {
                return;
            };
            tab.preview_fullscreen = !tab.preview_fullscreen;
            let fs = tab.preview_fullscreen;
            (fs, tab.md_preview.clone(), tab.editor.clone())
        };
        // 进入全屏阅读模式 → 预览强制换行、编辑区强制换行；
        // 退出全屏 → 预览始终换行（不随偏好）；编辑区恢复默认（md 换行 / 其余按偏好）。
        preview.update(cx, |view, cx| view.set_wrap(true, cx));
        let is_md = self.is_markdown_tab(idx);
        let editor_wrap = if fullscreen {
            true
        } else {
            is_md || self.wrap_enabled
        };
        editor.update(cx, |ed, _cx| {
            ed.set_soft_wrap(editor_wrap);
        });
        cx.notify();
    }

    /// 切换当前选项卡编辑区的自动换行/不换行，并更新全局编辑器偏好。
    /// 只作用于编辑区；Markdown/EPUB 预览始终自动换行，不随此开关变化。
    fn toggle_wrap(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let idx = self.active_tab;
        let wrap_enabled = !self.wrap_enabled;
        self.wrap_enabled = wrap_enabled;
        // 全屏阅读模式强制换行，跳过（由全屏切换控制）；否则按编辑器偏好
        if self
            .tabs
            .get(idx)
            .map(|t| !t.preview_fullscreen)
            .unwrap_or(true)
        {
            if let Some(tab) = self.tabs.get(idx) {
                tab.editor.update(cx, |ed, _cx| {
                    ed.set_soft_wrap(wrap_enabled);
                });
            }
        }
        cx.notify();
    }

    fn is_markdown_path(p: &Path) -> bool {
        p.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("md") || e.eq_ignore_ascii_case("markdown"))
            .unwrap_or(false)
    }

    /// 选项卡是否 Markdown 文件（编辑区/预览默认换行的依据）。
    fn is_markdown_tab(&self, idx: usize) -> bool {
        self.tabs
            .get(idx)
            .and_then(|t| t.file_path.as_ref())
            .map(|p| Self::is_markdown_path(p))
            .unwrap_or(false)
    }

    /// 是否 EPUB 文件（按扩展名识别）。
    fn is_epub_path(p: &Path) -> bool {
        p.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("epub"))
            .unwrap_or(false)
    }

    /// 是否 PDF 文件（按扩展名识别）。
    fn is_pdf_path(p: &Path) -> bool {
        p.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("pdf"))
            .unwrap_or(false)
    }

    /// 按扩展名推断 gpui-component 语法高亮的语言名（未知扩展名回退 "text"）。
    fn language_id(p: &Path) -> &'static str {
        let ext = p
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .unwrap_or_default();
        match ext.as_str() {
            "md" | "markdown" => "markdown",
            "rs" => "rust",
            "toml" => "toml",
            "json" => "json",
            "py" => "python",
            "js" | "jsx" => "javascript",
            "ts" | "tsx" => "typescript",
            "html" | "htm" => "html",
            "css" | "scss" => "css",
            "c" | "h" => "c",
            "cpp" | "cc" | "hpp" => "cpp",
            "go" => "go",
            "java" => "java",
            "sh" | "bash" => "bash",
            "yaml" | "yml" => "yaml",
            "sql" => "sql",
            _ => "text",
        }
    }

    /// 当前文件显示名（无文件时为"未命名"）。
    fn file_display_name(&self) -> String {
        self.tabs
            .get(self.active_tab)
            .and_then(|t| t.file_path.as_ref())
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "未命名".to_string())
    }

    /// 按扩展名推断语言标签（仅用于状态栏展示）。
    fn language_label(&self) -> &'static str {
        let Some(p) = self
            .tabs
            .get(self.active_tab)
            .and_then(|t| t.file_path.as_ref())
        else {
            return "纯文本";
        };
        let ext = p
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .unwrap_or_default();
        match ext.as_str() {
            "md" | "markdown" => "Markdown",
            "epub" => "EPUB",
            "pdf" => "PDF",
            "rs" => "Rust",
            "toml" => "TOML",
            "json" => "JSON",
            "py" => "Python",
            "js" | "ts" | "jsx" | "tsx" => "JavaScript",
            "html" | "htm" => "HTML",
            "css" | "scss" => "CSS",
            "c" | "h" => "C",
            "cpp" | "cc" | "hpp" => "C++",
            "go" => "Go",
            "java" => "Java",
            "sh" | "bash" => "Shell",
            "yaml" | "yml" => "YAML",
            "xml" => "XML",
            "sql" => "SQL",
            "txt" => "纯文本",
            _ => "文本",
        }
    }

    /// 调节编辑器字号并刷新预览（范围约束见 FONT_MIN/FONT_MAX）。
    fn change_font_size(&mut self, delta: f32, cx: &mut Context<Self>) {
        self.font_size = (self.font_size + delta).clamp(FONT_MIN, FONT_MAX);
        cx.notify();
    }

    /// 切换侧边栏「展开/收起」。
    fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        self.sidebar_open = !self.sidebar_open;
        cx.notify();
    }

    /// 窗口整体背景（编辑区以外的画布）。
    fn window_bg(&self) -> gpui::Hsla {
        if self.dark_mode {
            rgb(0x111827).into()
        } else {
            rgb(0xf3f4f6).into()
        }
    }

    /// 菜单栏 / 选项卡栏 / 状态栏等工具条背景。
    fn bar_bg(&self) -> gpui::Hsla {
        if self.dark_mode {
            rgb(0x1f2937).into()
        } else {
            rgb(0xf3f4f6).into()
        }
    }

    /// 工具条分隔边框。
    fn bar_border(&self) -> gpui::Hsla {
        if self.dark_mode {
            rgb(0x374151).into()
        } else {
            rgb(0xe5e7eb).into()
        }
    }

    /// 编辑器/预览面板背景。
    fn panel_bg(&self) -> gpui::Hsla {
        if self.dark_mode {
            rgb(0x111827).into()
        } else {
            white()
        }
    }

    /// 主要文字颜色。
    fn text_primary(&self) -> gpui::Hsla {
        if self.dark_mode {
            rgb(0xe5e7eb).into()
        } else {
            rgb(0x111827).into()
        }
    }

    /// 次要/弱化文字颜色。
    fn text_minor(&self) -> gpui::Hsla {
        if self.dark_mode {
            rgb(0x9ca3af).into()
        } else {
            rgb(0x6b7280).into()
        }
    }

    /// 胶囊/小按钮等"芯片"背景。
    fn chip_bg(&self) -> gpui::Hsla {
        if self.dark_mode {
            rgb(0x374151).into()
        } else {
            rgb(0xe5e7eb).into()
        }
    }

    /// 芯片上的文字颜色。
    fn chip_text(&self) -> gpui::Hsla {
        if self.dark_mode {
            rgb(0xe5e7eb).into()
        } else {
            rgb(0x374151).into()
        }
    }

    /// 激活选项卡背景。
    fn tab_active_bg(&self) -> gpui::Hsla {
        if self.dark_mode {
            rgb(0x374151).into()
        } else {
            white()
        }
    }

    /// 激活选项卡边框。
    fn tab_active_border(&self) -> gpui::Hsla {
        if self.dark_mode {
            rgb(0x4b5563).into()
        } else {
            rgb(0xd1d5db).into()
        }
    }

    /// 状态栏/其他整体面板背景。
    fn surface_bg(&self) -> gpui::Hsla {
        if self.dark_mode {
            rgb(0x111b2a).into()
        } else {
            white()
        }
    }

    /// 切换黑白（深色/浅色）主题：驱动 gpui_component 组件主题与自绘色板。
    /// 自绘标题栏也走同一套色板，因此整窗（含标题栏）随之切换。
    /// 编辑器语法配色随明暗切换为 GitHub Light / GitHub Dark（见 [`crate::syntax`]）。
    fn toggle_theme(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.dark_mode = !self.dark_mode;
        gpui_component::Theme::change(
            if self.dark_mode {
                gpui_component::ThemeMode::Dark
            } else {
                gpui_component::ThemeMode::Light
            },
            None,
            cx,
        );
        // Theme::change 会重置 highlight_theme，这里重装 GitHub 语法配色
        // （暗色编辑区背景也由该配色承载：0x111827，与旧 apply_dark_editor_colors 一致；行号栏不设独立背景色）。
        crate::syntax::install_github_theme(cx);
        // 刷新所有 Markdown 预览背景：背景是自绘色板（panel_bg）快照，
        // 若不刷新，切换主题后预览文字色随组件主题变化而背景仍是旧色，导致内容看不清。
        let panel_bg = self.panel_bg();
        // PDF 阅读器画布用灰色系（window_bg），与页面“纸面”卡片形成分隔
        let canvas_bg = self.window_bg();
        for tab in &self.tabs {
            let md = tab.md_preview.clone();
            md.update(cx, |view, cx| {
                view.set_background(panel_bg);
                cx.notify();
            });
            // PDF 阅读器同样刷新背景（主题色变化后与画布保持一致）
            if let Some(pdf) = &tab.pdf_preview {
                let pv = pdf.clone();
                pv.update(cx, |view, cx| {
                    view.set_background(canvas_bg);
                    cx.notify();
                });
            }
        }
        cx.notify();
    }
    /// 自绘标题栏：无边框窗口下代替原生标题栏，颜色跟随黑白主题。
    /// 左侧为 文件/编辑/查看/帮助 菜单，中间空位为窗口拖拽区，
    /// 右侧为最小化 / 最大化 / 关闭按钮。
    ///
    /// 拖拽与窗口控制全部交给平台原生处理：各元素标上
    /// [`gpui::WindowControlArea`]，GPUI 在 WM_NCHITTEST 里返回对应
    /// HTCAPTION / HTMINMAXBUTTON，由系统完成移动（贴边吸附）、
    /// 最小化、最大化（含双击标题栏切换）与关闭。
    pub(super) fn render_title_bar(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let (bar_bg, bar_border, text_minor) =
            (self.bar_bg(), self.bar_border(), self.text_minor());
        let icon_style = |icon: IconName| {
            Icon::new(icon).size(px(14.)).text_color(text_minor)
        };

        // 侧边栏「展开/收起」开关：收拢时显示打开图标（点击展开），展开时显示收起图标。
        // 用自绘图标按钮（非 window_control_area，普通点击交互）。
        let sidebar_toggle = div()
            .id("title-sidebar-toggle")
            .debug_selector(|| "title-sidebar-toggle".into())
            .w(px(30.))
            .h(px(24.))
            .flex()
            .items_center()
            .justify_center()
            .rounded_md()
            .text_color(text_minor)
            .cursor(CursorStyle::PointingHand)
            .hover(|style| style.bg(self.bar_border()))
            .child(icon_style(if self.sidebar_open {
                IconName::PanelLeftClose
            } else {
                IconName::PanelLeftOpen
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this: &mut EditorApp, _ev, _window, cx| {
                    this.toggle_sidebar(cx);
                }),
            );

        let maximize_icon = if window.is_maximized() {
            IconName::WindowRestore
        } else {
            IconName::WindowMaximize
        };

        let minimize = div()
            .id("title-minimize")
            .w(px(30.))
            .h(px(24.))
            .flex()
            .items_center()
            .justify_center()
            .rounded_md()
            .text_color(text_minor)
            .cursor(CursorStyle::PointingHand)
            .child(icon_style(IconName::WindowMinimize))
            .window_control_area(gpui::WindowControlArea::Min);

        let maximize = div()
            .id("title-maximize")
            .w(px(30.))
            .h(px(24.))
            .flex()
            .items_center()
            .justify_center()
            .rounded_md()
            .text_color(text_minor)
            .cursor(CursorStyle::PointingHand)
            .child(icon_style(maximize_icon))
            .window_control_area(gpui::WindowControlArea::Max);

        let close = div()
            .id("title-close")
            .w(px(30.))
            .h(px(24.))
            .flex()
            .items_center()
            .justify_center()
            .rounded_md()
            .text_color(text_minor)
            .cursor(CursorStyle::PointingHand)
            .child(icon_style(IconName::WindowClose))
            .window_control_area(gpui::WindowControlArea::Close);

        div()
            .id("title-bar")
            .h(px(30.))
            .w_full()
            .flex()
            .items_center()
            .gap_1()
            .px_2()
            .bg(bar_bg)
            .border_b_1()
            .border_color(bar_border)
            .child(self.render_menu_buttons(cx))
            .child(
                div()
                    .id("title-drag")
                    .flex_1()
                    .h_full()
                    .window_control_area(gpui::WindowControlArea::Drag),
            )
            .children([
                sidebar_toggle.into_any_element(),
                minimize.into_any_element(),
                maximize.into_any_element(),
                close.into_any_element(),
            ])
    }
}

impl Render for EditorApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = if self.show_preview() {
            let ratio = self.split_ratio;
            // EPUB 等全屏预览：预览区占满整个内容区，不显示编辑区与分割线。
            let fullscreen = self
                .tabs
                .get(self.active_tab)
                .map(|t| t.preview_fullscreen)
                .unwrap_or(false);
            if fullscreen {
                div()
                    .id("app-fullscreen-preview")
                    .debug_selector(|| "app-fullscreen-preview".into())
                    .flex()
                    .flex_1()
                    .min_h(px(0.))
                    .child(self.render_fullscreen_preview(window, cx))
                    .into_any_element()
            } else {
                div()
                    .id("app-split-body")
                    .debug_selector(|| "app-split-body".into())
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_h(px(0.))
                    // 分割线拖拽：鼠标移动/松开事件冒泡到容器统一处理
                    .on_mouse_move(cx.listener(
                        |this: &mut EditorApp, ev: &gpui::MouseMoveEvent, window, cx| {
                            if let Some((start_x, start_ratio)) = this.split_drag {
                                let total = window.bounds().size.width.as_f32();
                                if total > 0. {
                                    let x = ev.position.x.as_f32();
                                    this.split_ratio = (start_ratio + (x - start_x) / total)
                                        .clamp(SPLIT_RATIO_MIN, SPLIT_RATIO_MAX);
                                    cx.notify();
                                }
                            }
                        },
                    ))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(
                            |this: &mut EditorApp, _: &gpui::MouseUpEvent, _w, cx| {
                                this.split_drag = None;
                                cx.notify();
                            },
                        ),
                    )
                    .on_mouse_up_out(
                        MouseButton::Left,
                        cx.listener(
                            |this: &mut EditorApp, _: &gpui::MouseUpEvent, _w, cx| {
                                this.split_drag = None;
                                cx.notify();
                            },
                        ),
                    )
                    .child(self.render_editor_pane(ratio, cx))
                    .child(self.render_splitter(cx))
                    .child(self.render_preview_pane(window, 1.0 - ratio, cx))
                    .into_any_element()
            }
        } else {
            // 无预览：编辑区占满剩余。必须 min_w(0)+overflow_hidden——
            // 否则该容器 min-width:auto 被编辑器 980px 内容撑住，右侧溢出窗口
            // 46px，编辑器元素右缘落在屏幕外 → 滚动条画在窗口外看不见。
            div()
                .flex()
                .flex_1()
                .min_h(px(0.))
                .min_w(px(0.))
                .overflow_hidden()
                .child(self.render_editor_pane(1.0, cx))
                .into_any_element()
        };

        div()
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .bg(self.window_bg())
            .track_focus(&self.focus_handle(cx))
            // 从资源管理器拖入文件到窗口任意处 → 打开每个文件（目录跳过）
            .can_drop(|drag: &dyn Any, _w, _c| drag.downcast_ref::<ExternalPaths>().is_some())
            .on_drop(cx.listener(
                |this: &mut EditorApp, paths: &ExternalPaths, window, cx| {
                    this.open_dropped_paths(paths, window, cx);
                },
            ))
            // 多窗口：选项卡拖拽。
            // on_drag_move 确认「正在拖选项卡」（携带下标），on_mouse_up(_out) 决定落点——
            // 在窗口内松开 = 原位/普通点击（仅清状态）；在窗口外松开 = 拆分独立窗口 / 还原回主窗口。
            .on_drag_move(cx.listener(
                |this: &mut EditorApp, ev: &DragMoveEvent<TabDragData>, _window, cx| {
                    let data = ev.drag(cx);
                    this.tab_dragging = Some(data.idx);
                    cx.notify();
                },
            ))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this: &mut EditorApp, _ev, _window, cx| {
                    this.tab_dragging = None;
                    cx.notify();
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this: &mut EditorApp, _ev, window, cx| {
                    this.release_tab_drag_outside(window, cx);
                }),
            )
            .on_action(cx.listener(|this: &mut EditorApp, _: &Save, window, cx| {
                this.save(window, cx)
            }))
            .on_action(
                cx.listener(|this: &mut EditorApp, _: &PasteNormalized, window, cx| {
                    this.paste_normalized(window, cx)
                }),
            )
            .on_action(cx.listener(|this: &mut EditorApp, _: &SaveAs, window, cx| {
                this.save_as(window, cx)
            }))
            .on_action(
                cx.listener(|this: &mut EditorApp, _: &NewFile, window, cx| {
                    this.new_tab(window, cx)
                }),
            )
            .on_action(
                cx.listener(|this: &mut EditorApp, _: &OpenFile, window, cx| {
                    this.open_dialog(window, cx)
                }),
            )
            .on_action(
                cx.listener(|this: &mut EditorApp, _: &CloseTab, window, cx| {
                    this.close_tab(this.active_tab, window, cx);
                }),
            )
            .on_action(
                cx.listener(|this: &mut EditorApp, _: &TogglePreview, window, cx| {
                    let idx = this.active_tab;
                    this.toggle_preview_for_tab(idx, window, cx);
                }),
            )
            .on_action(
                cx.listener(|this: &mut EditorApp, _: &IncreaseFontSize, _w, cx| {
                    this.change_font_size(1.0, cx)
                }),
            )
            .on_action(
                cx.listener(|this: &mut EditorApp, _: &DecreaseFontSize, _w, cx| {
                    this.change_font_size(-1.0, cx)
                }),
            )
            .on_action(
                cx.listener(|this: &mut EditorApp, _: &ResetFontSize, _w, cx| {
                    this.font_size = DEFAULT_FONT_SIZE;
                    cx.notify();
                }),
            )
            .on_action(
                cx.listener(|this: &mut EditorApp, _: &ToggleWordWrap, window, cx| {
                    this.toggle_wrap(window, cx);
                }),
            )
            .child(self.render_title_bar(window, cx))
            .child(self.render_tab_bar(cx))
            .child(self.render_body_sidebar(body, cx))
            .child(self.render_status_bar(cx))
            // 通知层（右上角 toast）：叠加在最上层，用于"无法读取文件"等提示
            .children(gpui_component::Root::render_notification_layer(window, cx))
    }
}
