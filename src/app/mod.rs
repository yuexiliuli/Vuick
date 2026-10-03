//! 应用主视图 `EditorApp`：文本 / 代码 / Markdown 编辑器（多文件选项卡）。
//!
//! 布局（自上而下）：
//! - 菜单栏：文件 / 编辑 / 查看 / 帮助，点击弹出下拉菜单（[`EditorApp::render_menu_bar`]）
//! - 选项卡栏：每个文件一个选项卡，可新建（+）、切换、关闭（未保存时弹窗提示保存）
//! - 编辑区：自研 [`crate::editor::CodeEditor`]，内置语法高亮、行号、括号标尺、
//!   垂直标尺、撤销重做与折叠
//! - 预览区（开启预览时）：右侧分屏实时渲染 Markdown（复用 [`markdown`]）
//! - 状态栏：文件名 / 脏标记 / 语言徽标 / 行列 / 行数 / 字数 / 字号调节
//!
//! 打开 / 保存 / 另存为均调用**系统原生文件对话框**（`rfd`），无需手动输入路径；
//! 也支持把文件从资源管理器拖拽到窗口/编辑区打开。
//!
//! 文本编辑动作由编辑区在 `Input` 键上下文中自行处理，这里只订阅其编辑事件
//! （`EditorEvent::Edited`）刷新状态栏、脏标记与预览增量。

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
    anchored, deferred, div, px, rgb, rgba, white, Anchor, AnyWindowHandle, App, AppContext,
    Bounds, Context, CursorStyle, DismissEvent, DragMoveEvent, Edges, ExternalPaths, FocusHandle,
    Focusable, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ParentElement, Pixels,
    Point, PromptLevel, Render, SharedString, StatefulInteractiveElement, Styled, Subscription,
    VisualContext, Window, WindowBounds, WindowOptions,
};
use crate::ui::input::{InputEvent, InputState};
use crate::ui::menu::{PopupMenu, PopupMenuItem};
use ropey::Rope;
use crate::ui::icon::{Icon, IconName};

// WYSIWYG 预览编辑——本项目 `crate::markdown::wysiwyg::WyEditor`（已取代 zorite 移植的 `live`）。
use crate::markdown::{WyEditor, WyEvent};

use crate::actions::*;
use crate::config::layout::{
    DEFAULT_FONT_SIZE, FONT_MAX, FONT_MIN, MENU_BAR_H, SIDEBAR_WIDTH, SPLIT_RATIO_MAX,
    SPLIT_RATIO_MIN,
};
use crate::scrollbar::TableScrollState;

/// 把上一帧的预览 dirty 区间平移到本帧（编辑点之前不变、之后随 δ 平移）再并入
/// `[s, ne)`（新增变化区间），取 bounding box（当前文本字节坐标）；`None` = 首次编辑。
fn merge_preview_dirty(
    prev: Option<(usize, usize)>,
    s: usize,
    oe: usize,
    ne: usize,
) -> Option<(usize, usize)> {
    match prev {
        None => Some((s, ne)),
        Some((lo, hi)) => {
            let delta = ne as i64 - oe as i64;
            let hi_shifted = if hi > oe {
                hi as i64 + delta
            } else if hi >= s {
                ne as i64
            } else {
                hi as i64
            };
            Some((lo.min(s), hi_shifted.max(ne as i64).max(0) as usize))
        }
    }
}

#[cfg(test)]
mod merge_preview_dirty_tests {
    use super::merge_preview_dirty;

    #[test]
    fn first_edit_creates_interval() {
        assert_eq!(merge_preview_dirty(None, 5, 2, 8), Some((5, 8)));
    }

    #[test]
    fn edit_before_old_range_shifts_it() {
        // prev=[20,30)，插入使 [5,8)→[5,13)（δ=+5）→ 旧区间平移成 [25,35)，并入 → [5,35)。
        assert_eq!(merge_preview_dirty(Some((20, 30)), 5, 3, 8), Some((5, 35)));
    }

    #[test]
    fn edit_inside_old_range_merges_upper_bound() {
        // hi 落在 [s, oe]：上界直接折叠取新编辑终点的 ne。
        assert_eq!(merge_preview_dirty(Some((0, 5)), 5, 7, 9), Some((0, 9)));
    }

    #[test]
    fn edit_after_old_range_still_takes_bounding_box() {
        // prev=[0,4)，后续编辑 [5,9) → 两区间取并的 bounding box。
        assert_eq!(merge_preview_dirty(Some((0, 4)), 5, 7, 9), Some((0, 9)));
    }
}

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
    /// 图片查看器（打开图片时创建）：None = 非图片选项卡。
    image_viewer: Option<gpui::Entity<crate::image::ImageViewer>>,
    /// 视频查看器（打开视频时创建，M1 首帧静态阅读）：None = 非视频选项卡。
    #[cfg(feature = "video")]
    video_viewer: Option<gpui::Entity<crate::video::VideoViewer>>,
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
    /// 是否处于「所见即所得预览编辑」模式：左面板用 `WyEditor` 代替源码编辑区；
    /// 退出时内容写回。
    live_edit: bool,
    /// 「预览编辑测试」：本项目 `markdown::wysiwyg::WyEditor`（以 `Doc` 为数据源）。
    /// None = 未启用。启用即 `live_edit = true`。
    live2: Option<gpui::Entity<WyEditor>>,
    /// A2：右栏 Markdown 预览统一组件（`WyEditor`，A1 只读门控）。MD 右栏/全屏预览
    /// 从 `MarkdownPreview`（元素树）切到它（同一 `Doc` 管线的只读视图）；`None` =
    /// 未创建（文本 / PDF / EPUB 选项卡，或 MD 预览已关闭）。EPUB/PDF 仍走旧预览。
    md_wy: Option<gpui::Entity<WyEditor>>,
    /// `md_wy` 右栏/全屏宿主的纵向滚动句柄：`#md-wy-scroll`（`overflow_y_scroll`）
    /// 经 `track_scroll` 跟踪它，右侧自绘 `Scrollbar::vertical` 跟随（与 `live_vscroll`
    /// 同型：每次渲染重建容器、句柄跨帧稳定）。随选项卡搬迁（与 `md_preview` 同）。
    md_wy_vscroll: gpui::ScrollHandle,
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
    /// `live2`（WyEditor）的变更订阅。
    _live2_sub: Option<Subscription>,
    /// 预览编辑（WYSIWYG）宿主纵向滚动句柄：`#live-scroll`（`overflow_y_scroll`）
    /// 经 `track_scroll` 跟踪它，右侧 `Scrollbar::vertical` 跟随显示（宿主原生滚动条
    /// 在此 GPUI 不绘制，需自绘）。每次渲染重建容器，句柄存这里跨帧稳定。
    live_vscroll: gpui::ScrollHandle,
}

/// 一个从窗口中「拔出」的选项卡快照：编辑器 / 预览等实体句柄共享（GPUI 实体
/// 与窗口无关），连同文件元数据一起跨窗口搬迁。`_edit_sub` 是源窗口持有的订阅，
/// 拔掉时随源 `EditorTab` 一并丢弃，目标窗口重新建立订阅。
pub(crate) struct DetachedTab {
    editor: gpui::Entity<crate::editor::CodeEditor>,
    md_preview: gpui::Entity<crate::markdown::MarkdownPreview>,
    pdf_preview: Option<gpui::Entity<crate::pdf::PdfViewer>>,
    /// 图片查看器实体（随选项卡跨窗口搬迁）。
    image_viewer: Option<gpui::Entity<crate::image::ImageViewer>>,
    /// 视频查看器实体（随选项卡跨窗口搬迁）。
    #[cfg(feature = "video")]
    video_viewer: Option<gpui::Entity<crate::video::VideoViewer>>,
    /// A2：右栏 WyEditor 预览实体与宿主滚动句柄随选项卡搬迁（与 `md_preview` 同；
    /// 实体与窗口无关，可直接跨窗口移动）。
    md_wy: Option<gpui::Entity<WyEditor>>,
    md_wy_vscroll: gpui::ScrollHandle,
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

/// A4：Markdown 标题目录的跳转目标（侧边栏 `toc_item_frame` 主点击收起/滚动两用）：
/// - `Legacy` = `MarkdownPreview`（EPUB / 手动预览，条目为渲染行列，`scroll_to_row`）；
/// - `Unified` = 右栏统一组件 `WyEditor`（只读预览，条目为源码行，`scroll_to_line`）。
#[derive(Clone)]
pub(crate) enum MdTocScroll {
    Legacy(gpui::Entity<crate::markdown::view::MarkdownPreview>),
    Unified(gpui::Entity<WyEditor>),
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let accent = crate::ui::theme::Theme::global(cx).colors.accent;
        div()
            .id("tab-drag-ghost")
            .debug_selector(|| "tab-drag-ghost".into())
            .px_3()
            .py_1()
            .rounded_md()
            .bg(accent)
            .text_color(white())
            .text_size(px(12.))
            .child(self.name.clone())
    }
}

/// 所有 `EditorApp` 窗口的注册表（全局）：记录每个窗口的弱实体句柄与窗口句柄。
/// 「拖出拆分」创建的子窗口带 `is_detached=true`，把选项卡拖回主窗口时据此找到
/// 主窗口（第一个非 detached 的 EditorApp 窗口）。
#[derive(Default)]
struct EditorAppRegistry {
    windows: Vec<(gpui::WeakEntity<EditorApp>, AnyWindowHandle)>,
}


impl gpui::Global for EditorAppRegistry {}

/// 编辑区⇄预览滚动同步基线（见 [`EditorApp::sync_editor_preview_scroll`]）。记录
/// 当前选项卡 + 编辑器内容版本 + 上次已同步的双方「块起点」键，渲染帧比对后由动了
/// 的那一侧驱动对端。切换选项卡 / 同一选项卡内换文档 / 编辑使内容版本变化 → 重建。
#[derive(Clone, Copy)]
struct MdScrollSync {
    tab: usize,
    editor_version: Option<u64>,
    editor_block: Option<usize>,
    preview_block: Option<usize>,
    /// 我方（编辑器主导时）最近一次要求预览到达的目标块：预览重钉落位到该块是「按
    /// 我方命令跟上」，不得反被当成用户滚动预览而把编辑器拽回（来回跳动）。
    driven_preview_target: Option<usize>,
}

impl Default for MdScrollSync {
    fn default() -> Self {
        Self {
            tab: usize::MAX,
            editor_version: None,
            editor_block: None,
            preview_block: None,
            driven_preview_target: None,
        }
    }
}

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
    /// 通知 toasts（自研 `ui::notification`，App 自持容器与生命周期）。
    notifications: Vec<crate::ui::notification::Notification>,
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
    /// 侧边栏「外壳」状态：可见性 / 视图 / 目录折叠 / 宽度与右缘拖拽
    /// （检索输入 + 扫描缓存等仍在此处，见 [`sidebar::SidebarChromeState`]）。
    sidebar: sidebar::SidebarChromeState,
    /// 侧边栏「搜索」关键字输入控件。
    search_state: gpui::Entity<InputState>,
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
    /// 图片/视频右键「锁定/解锁」上下文菜单：弹出位置（窗口坐标）。None = 未打开。
    media_lock_menu_pos: Option<Point<Pixels>>,
    /// 已打开的媒体右键菜单实体（随位置同生同灭）。
    media_lock_menu_entity: Option<gpui::Entity<crate::ui::menu::PopupMenu>>,
    /// 媒体右键菜单的关闭订阅（选择/Esc/点击外 → 清理位置与实体）。
    media_lock_menu_sub: Option<Subscription>,
    focus_handle: FocusHandle,
    /// 编辑区⇄预览（MD 统一组件 WyEditor）滚动同步基线：当前选项卡 + 上次已同步的
    /// 双方块起点键。每帧渲染开头比对、由「动了的那一侧」驱动对端；切换选项卡/换
    /// 文档（块号失义）即重置。
    md_scroll_sync: MdScrollSync,
}

/// 当前激活选项卡的媒体查看器（图片 / 视频），供右键菜单锁定切换。
/// 一个选项卡只可能是其一（打开图片/视频时分别创建）。
#[derive(Clone)]
enum ActiveMedia {
    Image(gpui::Entity<crate::image::ImageViewer>),
    #[cfg(feature = "video")]
    Video(gpui::Entity<crate::video::VideoViewer>),
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
    /// `crate::ui::root::Root` 宿主包裹）。
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
            match ev {
                InputEvent::Change => cx.notify(),
            }
        });
        let mut app = EditorApp {
            tabs: Vec::new(),
            active_tab: 0,
            tab_bar_hscroll: TableScrollState::new(),
            tab_hscroll_seq: 0,
            status: SharedString::from("就绪"),
            status_ok: true,
            notifications: Vec::new(),
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
            sidebar: sidebar::SidebarChromeState {
                open: true,
                view: sidebar::SidebarView::Toc,
                // 目录面板默认收起（仅留 46px 活动栏，不挤占正文）；
                // 点活动栏「目录」按钮再展开。
                panel_open: false,
                toc_collapsed: Default::default(),
                toc_tab: None,
                width: SIDEBAR_WIDTH,
                width_auto: true,
                resize: None,
                resize_hovered: false,
            },
            search_state,
            _search_sub: search_sub,
            _pdf_observe: None,
            pdf_search: None,
            search_scan: None,
            search_scan_count: 0,
            open_fast_count: 0,
            open_finish_count: 0,
            tab_dragging: None,
            is_detached,
            media_lock_menu_pos: None,
            media_lock_menu_entity: None,
            media_lock_menu_sub: None,
            focus_handle,
            md_scroll_sync: MdScrollSync::default(),
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
            // 自动化测试钩子（与 MD_OPEN_DEBUG 一类调试入口同款）：打开文件后
            // 立即开启「本项目 WYSIWYG（预览编辑测试）」左面板，便于截图/识图验证。
            if std::env::var_os("WY_AUTOTEST").is_some() && app.is_markdown_tab(0) {
                app.toggle_live_test(window, cx);
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

    /// 选项卡的编辑焦点句柄：预览编辑模式返回 WYSIWYG 编辑器（否则键盘进
    /// 不可见的源码编辑区）；否则返回源码编辑区。
    fn tab_focus_handle(&self, idx: usize, cx: &App) -> FocusHandle {
        let Some(tab) = self.tabs.get(idx) else {
            return self.focus_handle.clone();
        };
        if tab.live_edit
            && let Some(l2) = &tab.live2 {
                let h = gpui::Focusable::focus_handle(l2.read(cx), cx);
                return h;
            }
        tab.editor.read(cx).focus_handle()
    }

    /// 当前激活选项卡的编辑区焦点句柄（供入口聚焦）。
    pub fn editor_focus_handle(&self, cx: &App) -> FocusHandle {
        self.tab_focus_handle(self.active_tab, cx)
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
            if ev == &crate::editor::EditorEvent::Edited
                && let Some(t) = this.tabs.iter().position(|t| t.editor == emitter)
            {
                this.on_editor_edited(t, cx);
            }
        })
    }

    /// 编辑事件：失效搜索缓存、推进版本、更新脏区间/计数，并按需触发一轮预览同步。
    /// 用 **选项卡下标** 而非长借 `&mut tab`——中途要调 `start_preview_sync`（整 `self` 可变）。
    fn on_editor_edited(&mut self, tab_ix: usize, cx: &mut Context<Self>) {
        let _t_change = crate::prof::begin();
        // 内容被编辑 → 侧边栏搜索缓存失效 + 版本号推进
        self.search_scan = None;
        // 编辑侧通道 → dirty 区间（bounding box，当前文本字节坐标）：
        // CodeEditor 记录最近一次编辑区间（字节），每轮 Change 1:1 取走。
        let dirty_range = {
            let tab = &mut self.tabs[tab_ix];
            tab.edit_version = tab.edit_version.wrapping_add(1);
            tab.editor.update(cx, |ed, _| ed.take_last_edit())
        };
        if let Some((s, oe, ne)) = dirty_range {
            let tab = &mut self.tabs[tab_ix];
            tab.preview_dirty = merge_preview_dirty(tab.preview_dirty, s, oe, ne);
            // A3 逐键增量桥：右栏 WyEditor 预览（若该选项卡有）重放**同一次**编辑
            // （src 字节区间 `[s, oe)` → 编辑后新文本 `[s, ne)`），走 WyEditor 增量管线
            //（Doc::edit + Pass1 + derived），大文档每键 ≈0.4ms——预览恒等于编辑区。
            if let Some(wy) = tab.md_wy.clone() {
                let ed = tab.editor.clone();
                let new_text = ed.read(cx).rope().slice(s..ne).to_string();
                if wy.read(cx).is_loaded() {
                    wy.update(cx, |w, cx| w.preview_apply_edit(s..oe, &new_text, cx));
                } else {
                    // 自愈：异步装配尚未完成用户已编辑 → 一次性从当前编辑区全文补齐
                    //（O(n) 仅此一次），此后退化到逐键增量（防空壳上重放错位）。
                    let full = ed.read(cx).value().to_string();
                    wy.update(cx, |w, _cx| w.set_source_text(&full));
                }
            }
        }
        // 行/字/光标走 CodeEditor 的 O(1) 计数，不再每键物化全文。
        // 行数用显示语义（`str::lines()` 口径，与文件打开/状态栏一致），
        // 不用含幻影尾行的编辑器模型 `line_count()`。
        let (line_count, char_count, cursor_row, cursor_col) = {
            let st = self.tabs[tab_ix].editor.read(cx);
            let cursor = st.cursor_position();
            (st.display_line_count(), st.char_count(), cursor.0, cursor.1)
        };
        // A4 脱离：仅在非 Markdown（EPUB / 手动预览）时才同步 MarkdownPreview——
        // MD 右栏预览已由逐键增量桥（`on_editor_edited` 上面的 bridge）实时跟随统一组件，
        // 不再为 MD 调后台 scan_blocks/喂 MarkdownPreview（省每键一次扫块 + 一份源副本）。
        if self.tabs[tab_ix].preview_on && !self.is_markdown_tab(tab_ix) {
            self.start_preview_sync(cx);
        }
        let tab = &mut self.tabs[tab_ix];
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
        // 不再 `cx.notify()`：编辑侧的 `notify` 已令窗口把**本视图及其祖先**
        // （含 app 根视图）标脏（gpui `mark_view_dirty`），下一帧必重渲染，
        // 状态栏/选项卡脏点/预览都在同一帧读到最新值——重复通知只是
        // 多走一遍 invalidation 簿记（大文档逐键该项 ~10ms+，见 prof）。
        crate::prof::end("tab_change", _t_change);
    }

    /// 开启 **一轮** Markdown 预览同步：循环「快照 → 后台扫描 → 装配 → 检查版本是否有
    /// 新编辑」，有新编辑就续跑，直到追上最新内容才停——打字期间**不会叠起多轮后台扫描**
    /// （`preview_syncing` 闸门），从「最后一次按键」起该轮扫描结束即收敛。
    fn start_preview_sync(&mut self, cx: &mut Context<Self>) {
        if self.preview_pending || self.preview_syncing {
            return;
        }
        self.preview_pending = true;
        self.preview_syncing = true;
        self.preview_seq = self.preview_seq.wrapping_add(1);
        let seq = self.preview_seq;
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
                                        crate::markdown::PreviewSrc::Rope(snapshot.clone()),
                                        index.clone(),
                                        cx,
                                    );
                                });
                                cx.notify();
                            }
                            tab.preview_prev = Some((snapshot.clone(), index.clone()));
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
            image_viewer: tab.image_viewer,
            #[cfg(feature = "video")]
            video_viewer: tab.video_viewer,
            md_wy: tab.md_wy,
            md_wy_vscroll: tab.md_wy_vscroll,
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
            image_viewer: payload.image_viewer,
            #[cfg(feature = "video")]
            video_viewer: payload.video_viewer,
            md_wy: payload.md_wy,
            md_wy_vscroll: payload.md_wy_vscroll,
            preview_on: payload.preview_on,
            preview_fullscreen: payload.preview_fullscreen,
            readonly_preview: payload.readonly_preview,
            editor_pending: payload.editor_pending,
            // 预览编辑（WYSIWYG）不随选项卡搬迁（搬迁前已提交回写，见 take_tab_payload 调用点）
            live_edit: false,
            live2: None,
            _live2_sub: None,
            live_vscroll: gpui::ScrollHandle::new(),
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
            // A4 脱离：MD（非只读）统一组件随选项卡搬家（已带内容），无需再喂预览；
            // EPUB / 手动预览仍走 MarkdownPreview。
            if !is_md || self.tabs[idx].readonly_preview {
                self.sync_md_preview(idx, cx);
            }
        }
        if let Some(pdf) = self.tabs[idx].pdf_preview.clone() {
            let pv = pdf.clone();
            pv.update(cx, |view, _| view.set_background(canvas_bg));
            self._pdf_observe = Some(cx.observe(&pdf, |_this, _e, cx| {
                cx.notify();
            }));
        }
        if let Some(img) = self.tabs[idx].image_viewer.clone() {
            img.update(cx, |view, _| view.set_background(canvas_bg));
        }
        #[cfg(feature = "video")]
        if let Some(vid) = self.tabs[idx].video_viewer.clone() {
            vid.update(cx, |view, _| view.set_background(canvas_bg));
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
        release_position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let Some(idx) = self.tab_dragging.take() else {
            return;
        };
        if idx >= self.tabs.len() {
            return;
        }
        // 实时预览编辑模式下鼠标按住拖出：先把内容写回编辑区（实时实体不随选项卡搬）。
        if self.tabs[idx].live_edit {
            self.commit_live(idx, cx);
        }
        let Some(payload) = self.take_tab_payload(idx) else {
            return;
        };
        if self.is_detached {
            // 独立文件窗口：把选项卡还给主窗口（找不到主窗口则自我转正为主窗口）
            self.restore_tab_to_primary(payload, cx);
            window.remove_window();
        } else {
            // 主窗口：把选项卡拆进新开的独立窗口（窗口出现在鼠标释放处）
            self.split_tab_to_new_window(payload, release_position, window, cx);
        }
    }

    /// 拖拽中光标**第一次越过窗口边缘** → 立即把选项卡拆进光标处的新窗口（浏览器式
    /// tear-off），让「拖出窗口」有连续过渡，而不是松开后才突然弹出独立窗口。
    /// 独立文件窗口不在此拆分：它保持「拖到选项卡栏松开 = 还原回主窗口」的语义。
    fn tear_off_drag(
        &mut self,
        idx: usize,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.is_detached {
            return;
        }
        if idx >= self.tabs.len() {
            return;
        }
        // 实时预览编辑模式下 tear-off：先写回（实时实体不随选项卡搬）。
        if self.tabs[idx].live_edit {
            self.commit_live(idx, cx);
        }
        let Some(payload) = self.take_tab_payload(idx) else {
            return;
        };
        self.tab_dragging = None;
        // 结束旧窗口的拖拽：幽灵不再画在旧窗口，也不会被新窗口的首帧绘制出来。
        let _ = cx.stop_active_drag(window);
        self.split_tab_to_new_window(payload, position, window, cx);
    }

    /// 把选项卡拆进一个新开的独立窗口：窗口左上角跟随鼠标释放位置（屏幕坐标 =
    /// 源窗口屏幕左上角 + 拖放点的窗口内坐标）；本窗口若已没有选项卡则补一个空选项卡。
    fn split_tab_to_new_window(
        &mut self,
        payload: DetachedTab,
        release_position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.tabs.is_empty() {
            self.make_tab(window, cx);
        }
        // 主窗口剩下的选项卡接管焦点（被拖走的选项卡已从 tabs 移除）
        self.activate_tab(self.active_tab, window, cx);
        let src_bounds = window.bounds();
        let bounds = Bounds {
            origin: src_bounds.origin + release_position,
            size: src_bounds.size,
        };
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
                cx.new(|cx| crate::ui::root::Root::new(view, new_window, cx))
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
                let _ = wh.update(cx, |_root, window, cx| {
                    window.focus(&focus_handle, cx);
                });
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

    /// 图片查看器切换/加载后的标签标题与状态栏同步（**渲染期调用**，见
    /// `render_fullscreen_preview`）：查看器切换图片后 `refresh_windows` 触发本窗口重绘，
    /// 全屏预览渲染前据此把选项卡 `file_path` 与状态栏跟到当前图片（标签标题跟着图走，
    /// 状态栏显示「图片 n/m：路径」）。仅在激活选项卡持有图片查看器时更新；
    /// **幂等**：路径未变返回 false（调用方不 notify，无重绘循环）。
    fn sync_active_image_meta(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(handle) = self
            .tabs
            .get(self.active_tab)
            .and_then(|t| t.image_viewer.clone())
        else {
            return false;
        };
        let (cur, idx, total) = {
            let v = handle.read(cx);
            (v.current_path(), v.current_index(), v.folder_len())
        };
        let Some(cur) = cur else {
            return false;
        };
        let Some(tab) = self.tabs.get_mut(self.active_tab) else {
            return false;
        };
        if tab.file_path.as_deref() == Some(cur.as_path()) {
            return false;
        }
        self.status = SharedString::from(format!("图片 {}/{}：{}", idx + 1, total, cur.display()));
        self.status_ok = true;
        tab.file_path = Some(cur);
        true
    }

    /// 同 [`Self::sync_active_image_meta`]，视频版：查看器同文件夹切换后把选项卡
    /// 标题/状态栏跟到新视频（渲染期幂等调用；路径未变返回 false）。
    #[cfg(feature = "video")]
    fn sync_active_video_meta(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(handle) = self
            .tabs
            .get(self.active_tab)
            .and_then(|t| t.video_viewer.clone())
        else {
            return false;
        };
        let (cur, idx, total) = {
            let v = handle.read(cx);
            (v.current_path(), v.current_index(), v.folder_len())
        };
        let Some(cur) = cur else {
            return false;
        };
        let Some(tab) = self.tabs.get_mut(self.active_tab) else {
            return false;
        };
        if tab.file_path.as_deref() == Some(cur.as_path()) {
            return false;
        }
        self.status = SharedString::from(format!("视频 {}/{}：{}", idx + 1, total, cur.display()));
        self.status_ok = true;
        tab.file_path = Some(cur);
        true
    }

    /// 当前激活选项卡是否在「锁定」干净看图模式（读图片查看器实体状态）。
    fn active_image_locked(&self, cx: &App) -> bool {
        self.tabs
            .get(self.active_tab)
            .and_then(|t| t.image_viewer.as_ref())
            .is_some_and(|v| v.read(cx).is_locked())
    }

    /// 当前激活选项卡是否在「纯视频」干净播放模式（读视频查看器实体状态）。
    #[cfg(feature = "video")]
    fn active_video_locked(&self, cx: &App) -> bool {
        self.tabs
            .get(self.active_tab)
            .and_then(|t| t.video_viewer.as_ref())
            .is_some_and(|v| v.read(cx).is_locked())
    }

    /// 当前激活选项卡的媒体查看器（图片或视频；均有则取图片）。无媒体 → None。
    fn active_media_viewer(&self) -> Option<ActiveMedia> {
        let tab = self.tabs.get(self.active_tab)?;
        if let Some(img) = &tab.image_viewer {
            return Some(ActiveMedia::Image(img.clone()));
        }
        #[cfg(feature = "video")]
        if let Some(vid) = &tab.video_viewer {
            return Some(ActiveMedia::Video(vid.clone()));
        }
        None
    }

    /// 图片/视频右键 → 在光标处弹「锁定/解锁」上下文菜单（仅活动选项卡是图片/视频时
    /// 生效）。锁定 = 隐藏全部非媒体 UI、媒体充满窗口；解锁回归。就地换掉旧的（若已打开）。
    fn on_media_right_click(
        &mut self,
        ev: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(media) = self.active_media_viewer() else {
            return;
        };
        // 先收掉上一份菜单（若有），再在光标处新建（旧订阅带 emitter 身份校验，不会误清新菜单）
        self.media_lock_menu_entity = None;
        self.media_lock_menu_pos = None;
        self.media_lock_menu_sub = None;
        let menu = PopupMenu::build(window, cx, move |menu, _window, _cx| {
            let item = match media.clone() {
                ActiveMedia::Image(img) => {
                    let viewer_render = img.clone();
                    PopupMenuItem::element(move |_window, cx| {
                        let locked = viewer_render.read(cx).is_locked();
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(div().w(px(18.)).child(if locked { "✓" } else { "" }))
                            .child("锁定图片视图")
                    })
                    .on_click(move |_click, window, cx| {
                        img.update(cx, |v, cx| v.toggle_lock(cx));
                        // 强制重绘：宿主重读媒体查看器锁定态 → 隐藏/恢复外壳 → 媒体自动填满/缩小
                        window.refresh();
                    })
                }
                #[cfg(feature = "video")]
                ActiveMedia::Video(vid) => {
                    let viewer_render = vid.clone();
                    PopupMenuItem::element(move |_window, cx| {
                        let locked = viewer_render.read(cx).is_locked();
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(div().w(px(18.)).child(if locked { "✓" } else { "" }))
                            .child("锁定视频视图")
                    })
                    .on_click(move |_click, window, cx| {
                        vid.update(cx, |v, cx| v.toggle_lock(cx));
                        window.refresh();
                    })
                }
            };
            menu.item(item)
        });
        // 聚焦并记住开菜单前的焦点（关闭后归还）
        let previous_focus = window.focused(cx);
        let focus_handle = menu.read(cx).focus_handle(cx);
        menu.update(cx, |m, _| m.set_previous_focus(previous_focus));
        window.focus(&focus_handle, cx);
        // 关闭（选择/Esc/点击外）→ 清理；emitter 身份校验避免旧菜单的关闭误清新菜单
        let menu_entity = menu.clone();
        let sub = cx.subscribe(&menu_entity, move |this, emitter, _: &DismissEvent, cx| {
            if this.media_lock_menu_entity.as_ref() == Some(&emitter) {
                this.media_lock_menu_entity = None;
                this.media_lock_menu_pos = None;
            }
            cx.notify();
        });
        self.media_lock_menu_pos = Some(ev.position);
        self.media_lock_menu_entity = Some(menu);
        self.media_lock_menu_sub = Some(sub);
        cx.notify();
    }

    /// 外壳子元素：锁定图片/视频视图时只放「媒体铺满窗口」（藏菜单栏/选项卡栏/侧边栏/
    /// 状态栏），否则齐全；始终带通知层与媒体右键菜单 Overlay。
    fn render_shell_children(
        &mut self,
        body: gpui::AnyElement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let image_locked = self.active_image_locked(cx);
        let video_locked = {
            #[cfg(feature = "video")]
            {
                self.active_video_locked(cx)
            }
            #[cfg(not(feature = "video"))]
            {
                false
            }
        };
        let mut out: Vec<gpui::AnyElement> = Vec::new();
        if image_locked || video_locked {
            // 锁定：正文（图片/视频查看器）直接铺满窗口，其余全藏 —— 媒体经重排后自动放大填满
            out.push(body);
        } else {
            out.push(self.render_title_bar(window, cx).into_any_element());
            out.push(self.render_tab_bar(cx).into_any_element());
            out.push(self.render_body_sidebar(body, cx).into_any_element());
            out.push(self.render_status_bar(cx).into_any_element());
        }
        // 通知层（右上角 toast）与媒体右键菜单 Overlay 恒在最上层
        out.push(self.render_notifications(window, cx).into_any_element());
        out.push(self.render_media_lock_menu(window, cx).into_any_element());
        out
    }

    /// 图片/视频「锁定」右键菜单 Overlay：按右键时记录的窗口坐标定位（`AnchoredPositionMode::Window`）。
    fn render_media_lock_menu(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let still_media = self.active_media_viewer().is_some();
        if !still_media {
            // 切走/关闭了媒体选项卡 → 一并收起右键菜单
            self.media_lock_menu_entity = None;
            self.media_lock_menu_pos = None;
        }
        let Some(pos) = self.media_lock_menu_pos else {
            return div().into_any_element();
        };
        let Some(menu) = self.media_lock_menu_entity.clone() else {
            return div().into_any_element();
        };
        deferred(
            anchored()
                .position_mode(gpui::AnchoredPositionMode::Window)
                .anchor(Anchor::TopLeft)
                .offset(pos)
                .snap_to_window_with_margin(Edges::all(px(8.)))
                .child(menu),
        )
        .with_priority(100)
        .into_any_element()
    }

    /// A2：当前激活选项卡的右栏 WyEditor 预览实体（仅当该选项卡**真正以 WyEditor 作
    /// 预览渲染**时才返回：Markdown 文件 → 已建 → 非 PDF → 非只读预览（EPUB 仍走
    /// `MarkdownPreview`））。`render_preview_pane` / `render_fullscreen_preview` 据此
    /// 决定走统一组件宿主。
    fn md_wy_preview(&self) -> Option<gpui::Entity<WyEditor>> {
        if !self.is_markdown_tab(self.active_tab) {
            return None;
        }
        let tab = self.tabs.get(self.active_tab)?;
        if tab.md_wy.is_none() || tab.pdf_preview.is_some() || tab.readonly_preview {
            return None;
        }
        tab.md_wy.clone()
    }

    /// 编辑区⇄预览滚动同步（每帧渲染开头调用）。只在「活动选项卡以 WyEditor 作 MD
    /// 预览且在**非 live2 实时编辑**的分屏（非全屏 / 非只读 + 预览开启）」时生效——
    /// live2 下右栏预览不在元素树，驱动隐藏实体只会残留陈旧目标。
    ///
    /// 同步单位 = 源码行所在**块起点**（两侧都经 [`WyEditor::block_start_of_line`] /
    /// [`WyEditor::top_source_line`] 归一成块）：同一块（段落/代码块/标题）内微滚动
    /// 双方键不变、不互拉，避免反馈抖动；跨块才驱动对端。
    /// 逻辑：编辑器顶行块变了 → 预览 `scroll_to_line` 到该块；预览顶块变了（且编辑器
    /// 未同时动）→ 编辑器滚动到该块起点。双侧收敛到同一块后静止。
    ///
    /// 注意：两侧键读取的是**上一帧**的滚动/视口状态（`top_source_line` 读 `panes`
    /// 上一帧渲染期推入的 `viewport_top_px`）——本帧推入发生在 sync 之后，故同步天然
    /// 滞后一帧；对逐帧收敛无影响，反而更稳。
    fn sync_editor_preview_scroll(&mut self, cx: &mut Context<Self>) {
        let idx = self.active_tab;
        if self.md_wy_preview().is_none() {
            return;
        }
        let tab = &self.tabs[idx];
        if tab.readonly_preview || tab.preview_fullscreen || !self.show_preview() || tab.live_edit {
            return;
        }
        let Some(md) = tab.md_wy.clone() else { return };
        let editor = tab.editor.clone();

        // 读两侧当前块起点键（预览帧未就绪 → None，本帧不动）。
        let preview_block = md.read(cx).top_source_line();
        let editor_block = Some(md.read(cx).block_start_of_line(editor.read(cx).viewport_top_line()));

        // 首次进入该选项卡 / 换文档（块号失义）/ 编辑器内容已编辑（块映射变）：
        // 重建基线，只记录不驱动。
        let ev = Some(editor.read(cx).version());
        if self.md_scroll_sync.tab != idx || self.md_scroll_sync.editor_version != ev {
        self.md_scroll_sync = MdScrollSync {
            tab: idx,
            editor_version: ev,
            editor_block,
            preview_block,
            driven_preview_target: None,
        };
        return;
    }
    let prev = self.md_scroll_sync;
    self.md_scroll_sync.editor_block = editor_block;
    self.md_scroll_sync.preview_block = preview_block;
    let Some(eb) = editor_block else { return };
    if preview_block == Some(eb) {
        // 两侧同一块，已同步：作废「等待预览到达」的驱动目标（已对齐）。
        self.md_scroll_sync.driven_preview_target = None;
        return;
    }
    if prev.editor_block != editor_block {
        // 编辑器先动 → 预览跟到编辑器顶行所在块。**不能在本帧 render 直接调**
        // `scroll_to_line`（宿主 `ScrollHandle::set_offset` 在渲染期会被本帧布局
        // 复位）：只把目标行写进 WY 的 `scroll_target`，由其元素 prepaint 逐帧重钉
        // 宿主偏移直到稳住（有 [`WyEditor::SCROLL_TARGET_MAX_APPLIES`] 兜底）。
        // 记录「等待预览到达」目标：预览重钉落位到它时不被误判为预览用户滚动。
        md.update(cx, |v, _| v.set_scroll_target(eb));
        self.md_scroll_sync.driven_preview_target = Some(eb);
    } else if let Some(pb) = preview_block
        && prev.preview_block != preview_block
        // 预览落位到「我方正在驱动的预览目标」= 编辑器主导下的跟随，不反向拉动。
        && preview_block != self.md_scroll_sync.driven_preview_target
    {
        // 预览先动 → 编辑器跟到预览顶块起点。
        editor.update(cx, |ed, _| ed.scroll_to_line(pb));
        // 预置编辑器基线为驱动目标：编辑器下一帧实际到位即与基线一致，不再被误判
        // 成「编辑器用户主动滚动」而把预览拽回（来回跳动）；预览现为主导，旧等待
        // 目标作废。
        self.md_scroll_sync.editor_block = Some(pb);
        self.md_scroll_sync.driven_preview_target = None;
    }
}

    /// A4：当前激活选项卡的 Markdown TOC 数据源 + 跳转目标（侧边栏目录共用）：
    /// MD 统一预览（`md_wy` 在场，非只读）→ WyEditor（条目 = 源码行，点击滚动统一组件
    /// 宿主）；EPUB / 手动预览（`md_wy` 缺席）→ `MarkdownPreview`（渲染行列，`scroll_to_row`）。
    fn md_toc_source(
        &self,
        cx: &gpui::App,
    ) -> Option<(crate::markdown::wysiwyg::TocEntries, MdTocScroll)> {
        let tab = self.tabs.get(self.active_tab)?;
        if let Some(wy) = &tab.md_wy {
            return Some((wy.read(cx).toc_entries(), MdTocScroll::Unified(wy.clone())));
        }
        Some((
            tab.md_preview.read(cx).toc_entries(),
            MdTocScroll::Legacy(tab.md_preview.clone()),
        ))
    }

    /// A2：确保某选项卡的右栏 WyEditor 预览已创建（懒创建 + A1 只读门控 + 全局字号/
    /// 基准目录）。`feed=true` 用当前编辑区文本**同步喂源**（小文档 / 切预览；
    /// 大文档异步走 `open_from_rope`(后台) + `finish_file_text` 的 `install_open`，
    /// 不在此做 O(n) UI 解析）。
    fn ensure_md_wy(
        &mut self,
        idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
        feed: bool,
    ) {
        if idx >= self.tabs.len() {
            return;
        }
        let fs = self.font_size;
        let base_dir = self.tabs[idx]
            .file_path
            .as_ref()
            .and_then(|p| p.parent())
            .map(|d| d.to_path_buf());
        if self.tabs[idx].md_wy.is_none() {
            let ed = cx.new(|cx| WyEditor::new(window, cx));
            ed.update(cx, |ed, _| {
                ed.set_editable(false); // A1 只读门控：右栏预览不可编辑
                ed.set_font_size(gpui::px(fs));
                ed.set_base_dir(base_dir.clone());
            });
            self.tabs[idx].md_wy = Some(ed.clone());
        }
        if feed {
            // 同步喂源：源 = 编辑区规范文本（与 `sync_md_preview` 同口径）。
            let text = {
                let tab = &mut self.tabs[idx];
                tab.editor.read(cx).value().to_string()
            };
            if let Some(ed) = self.tabs[idx].md_wy.clone() {
                ed.update(cx, |ed, _| {
                    ed.set_font_size(gpui::px(fs));
                    ed.set_base_dir(base_dir);
                    ed.set_source_text(&text);
                });
            }
        }
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
            // A4 脱离：MD（非只读）只走统一组件 WyEditor（ensure_md_wy 同步喂源）；
            // 仅 EPUB / 手动预览（readonly 或非 md）仍用 MarkdownPreview（sync_md_preview）。
            if is_md && !readonly {
                self.ensure_md_wy(idx, window, cx, true);
            } else {
                self.sync_md_preview(idx, cx);
            }
        } else {
            // 关闭预览：释放整篇解析出的行数据与测量缓存，内存回到基线
            // （否则隐藏的预览仍占着整篇文档，反复开关只涨不降）。
            self.tabs[idx].md_preview.update(cx, |view, cx| view.release(cx));
            // A2：右栏 WyEditor 预览随预览开关释放（同款内存回收）。
            self.tabs[idx].md_wy = None;
            self.tabs[idx].md_wy_vscroll = gpui::ScrollHandle::new();
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
            && let Some(tab) = self.tabs.get(idx) {
                tab.editor.update(cx, |ed, _cx| {
                    ed.set_soft_wrap(wrap_enabled);
                });
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

    /// 是否图片文件（按扩展名识别；清单见 [`crate::image::IMAGE_EXTENSIONS`]）。
    fn is_image_path(p: &Path) -> bool {
        crate::image::is_image_extension(p)
    }

    /// 是否视频文件（按扩展名识别；清单见 [`crate::video::VIDEO_EXTENSIONS`]）。
    #[cfg(feature = "video")]
    fn is_video_path(p: &Path) -> bool {
        crate::video::is_video_extension(p)
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
            "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "ico" | "tif" | "tiff" => "图片",
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
        // 实时预览编辑（WYSIWYG）字号：EditorState 继承外层 text_size（宿主容器渲染时
        // 读取 self.font_size），无需像旧的 MarkdownLivePreview 那样内部 set_font_override。
        cx.notify();
    }

    // -------------------------------------------------------------------
    // 所见即所得预览编辑（WYSIWYG，`markdown::wysiwyg::WyEditor`）
    // -------------------------------------------------------------------

    /// 选项卡的「有效内容」：`live2`（WyEditor，实时）开启时读它，否则读源码编辑区。
    /// 保存/关闭/写回统一走此口径——**live 期间源码编辑区不镜像**（去 O(n)），直接读
    /// WyEditor 的 rope 才不丢内容（大文档 close→保存 若读旧编辑区会整段丢失）。
    /// 大文档后台解析**未完成**（`is_loaded() == false`）时 live2 是空壳，回退源码编辑区。
    pub(super) fn tab_effective_text(&self, idx: usize, cx: &App) -> String {
        match self.tabs.get(idx) {
            Some(t) => match &t.live2 {
                Some(l2) => {
                    if l2.read(cx).is_loaded() {
                        l2.read(cx).text().to_string()
                    } else {
                        t.editor.read(cx).value().to_string()
                    }
                }
                None => t.editor.read(cx).value().to_string(),
            },
            None => String::new(),
        }
    }

    /// 退出预览编辑：把 WYSIWYG 内容写回源码编辑区 + 内容镜像 + 预览，销毁实体。
    fn commit_live(&mut self, idx: usize, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get_mut(idx) else { return };
        tab.live_edit = false;
        tab._live2_sub = None;
        let Some(l2) = tab.live2.take() else { return };
        // 大文档后台解析在途就退出：live2 尚无内容，不能写回（否则源码编辑区被清空）。
        // 源码编辑区仍是权威源，直接丢弃空壳实体即可。
        if !l2.read(cx).is_loaded() {
            self.status = SharedString::from("预览编辑测试：关（加载中退出，未写回编辑区）");
            self.status_ok = false;
            return;
        }
        let text = l2.read(cx).text().to_string();
        tab.content = SharedString::from(text);
        let content = tab.content.clone();
        let n_lines = content.as_ref().lines().count().max(1);
        let n_chars = content.as_ref().chars().count();
        tab.line_count = n_lines;
        tab.char_count = n_chars;
        tab.editor.update(cx, |ed, _| ed.set_text(&content));
        tab.md_preview.update(cx, |v, cx| v.set_source(&content, cx));
        // A2/A3：右栏 WyEditor 预览（若存在）随 WYSIWYG 写回全量刷新——
        // 写回是整篇替换（非 Change 轨迹），增量桥不会替它补，这里一次性对正源。
        if let Some(wy) = tab.md_wy.clone() {
            wy.update(cx, |w, _cx| w.set_source_text(&content));
        }
        self.status = SharedString::from("预览编辑（WYSIWYG）：关（内容已写回编辑区）");
        self.status_ok = true;
    }

    /// 「预览编辑测试」开关：用本项目 `markdown::wysiwyg::WyEditor` 渲染左面板。
    fn toggle_live_test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let idx = self.active_tab;
        if !self.is_markdown_tab(idx) {
            self.status = SharedString::from("预览编辑测试仅支持 Markdown 文件");
            self.status_ok = false;
            cx.notify();
            return;
        }
        if self.tabs[idx].live2.is_some() {
            self.commit_live(idx, cx);
        } else {
            // 源快照：O(1) COW Rope。大文档走**后台解析**（与预览的后台 scan_blocks 同构）：
            // UI 线程只做 O(1) 快照 + 空壳编辑器创建 + 完成装配；整篇 O(n) 解析
            // （`Doc::open` 6.4MB 实测 ~200ms）在 `background_executor`。小文档保持同步
            // （无「空白→内容」闪烁）。加载完成前 `is_loaded()==false`，宿主读写文本
            // （保存/退出）回退源码编辑区（`tab_effective_text` / `commit_live` 已守卫）。
            const ASYNC_LOAD_MIN_BYTES: usize = 512 * 1024;
            let rope = self.tabs[idx].editor.read(cx).text().clone();
            let async_load = rope.len() >= ASYNC_LOAD_MIN_BYTES;
            let ed = if async_load {
                cx.new(|cx| WyEditor::new(window, cx))
            } else {
                let text = SharedString::from(rope.to_string());
                cx.new(|cx| WyEditor::new(window, cx).with_text(text.as_ref()))
            };
            // 同步全局字号（新建即套用；WY 字号由编辑器自持，measure/prepaint 一致）
            let fs = self.font_size;
            ed.update(cx, |ed, _| ed.set_font_size(gpui::px(fs)));
            // 图片相对路径基于文件所在目录解析（与预览一致）
            let base_dir = self.tabs[idx]
                .file_path
                .as_ref()
                .and_then(|p| p.parent())
                .map(|d| d.to_path_buf());
            ed.update(cx, |ed, _| ed.set_base_dir(base_dir));
            let ed_sub = cx.subscribe(&ed, |this, emitter, ev: &WyEvent, cx| {
                let _t = crate::prof::begin();
                match ev {
                    WyEvent::Changed => {
                        if let Some(tab) =
                            this.tabs.iter_mut().find(|t| t.live2.as_ref() == Some(&emitter))
                        {
                            // **O(1) 增量计数**（`WyEditor::line_count/char_count` 直读
                            // `Doc` 行起始表/增量字符计数，不物化全文），源码编辑区与右侧
                            // Markdown 预览不再做每键/停手整篇镜像（大文档 set_text 级联
                            // 重解析会卡 7-8s）——它们随左面板 WYSIWYG 一起，到退出
                            // `commit_live` 时统一写回。
                            tab.line_count = emitter.read(cx).line_count();
                            tab.char_count = emitter.read(cx).char_count();
                            if !tab.dirty {
                                tab.dirty = true;
                            }
                            cx.notify();
                        }
                    }
                    WyEvent::OpenPage(p) => {
                        this.status = SharedString::from(format!("WYSIWYG 点击链接：{p}"));
                        this.status_ok = true;
                    }
                }
                crate::prof::end("wy.app.changed", _t);
            });
            let tab = &mut self.tabs[idx];
            tab.live2 = Some(ed.clone());
            tab._live2_sub = Some(ed_sub);
            tab.live_edit = true;
            if async_load {
                let tab_idx = idx;
                let ed_install = ed.clone();
                cx.spawn(async move |this, cx| {
                    let t0 = std::time::Instant::now();
                    let result =
                        cx.background_executor()
                            .spawn(async move {
                                crate::markdown::wysiwyg::WyEditor::open_from_rope(rope)
                            })
                            .await;
                    crate::prof::add_absolute(
                        "wy.app.load_async_parse",
                        t0.elapsed(),
                    );
                    let _ = this.update(cx, |this, cx| {
                        // 只装配仍挂在原 tab 的 live2（用户在解析期间切换/退出则丢弃）。
                        if this
                            .tabs
                            .get(tab_idx)
                            .is_some_and(|t| t.live2.as_ref() == Some(&ed_install))
                        {
                            ed_install.update(cx, |ed, _| ed.install_open(result));
                            let tab = &mut this.tabs[tab_idx];
                            tab.line_count = ed_install.read(cx).line_count();
                            tab.char_count = ed_install.read(cx).char_count();
                            cx.notify();
                        }
                    });
                })
                .detach();
            }
            self.status = SharedString::from("预览编辑测试：开（本项目 Doc 管线 WYSIWYG，初步实现）");
            self.status_ok = true;
            let focus = gpui::Focusable::focus_handle(ed.read(cx), cx);
            window.focus(&focus, cx);
        }
        cx.notify();
    }

    /// 切换侧边栏「展开/收起」。
    fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        self.sidebar.open = !self.sidebar.open;
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

    /// 切换黑白（深色/浅色）主题：驱动自研主题（`ui::theme`）与残留组件主题
    /// （`Theme::set` 双轨同色）。编辑器语法配色随明暗切换为 GitHub Light / Dark
    /// （见 [`crate::syntax`]）。
    fn toggle_theme(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.dark_mode = !self.dark_mode;
        crate::ui::theme::Theme::set(
            if self.dark_mode {
                crate::ui::theme::ThemeMode::Dark
            } else {
                crate::ui::theme::ThemeMode::Light
            },
            cx,
        );
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
            .child(icon_style(if self.sidebar.open {
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

    /// 右上角 toast 层：卡片可见即渲染，点击卡片按「类型 + key」精确关闭。
    fn render_notifications(&mut self, window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        if self.notifications.is_empty() {
            return div().into_any_element();
        }
        let cards = self
            .notifications
            .iter()
            .map(|n| {
                let id_type = n.id_type();
                let key = n.key().clone();
                n.render_card(window, cx)
                    .on_click(cx.listener(move |this, _, _window, cx| {
                        this.notifications
                            .retain(|it| it.id_type() == id_type && it.key() == &key);
                        cx.notify();
                    }))
            })
            .collect::<Vec<_>>();
        div()
            .id("notification-layer")
            .absolute()
            .top_0()
            .right_0()
            .mt_1()
            .mr_1()
            .flex()
            .flex_col()
            .gap_1()
            .children(cards)
            .into_any_element()
    }
}

impl Render for EditorApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // WY_TRACE 归因：操作（按键/滚动）起点 → 本帧 render 开始的等待，单独记账
        // 供「每键真实延迟」拆账——多为 60Hz vsync/帧调度，不是 CPU 段。
        if let Some(d) = crate::prof::op_elapsed() {
            crate::prof::add_absolute("app.frame_delay", d);
        }
        let _t = crate::prof::begin();
        // 编辑区⇄预览滚动同步：动了的一侧驱动另一侧到对应块（仅 MD 分屏、非 live2）。
        self.sync_editor_preview_scroll(cx);
        // 实时预览编辑（WYSIWYG）本身即「所见即所得」，右侧 Markdown 预览冗余 →
        // 编辑区占满整块（不显示分割线与预览区），退出实时模式后恢复原分屏。
        let body = self.render_body(window, cx);

        let root = self.render_root(body, window, cx);
        crate::prof::end("wy.app.render", _t);
        root
    }
}

impl EditorApp {
    /// 内容区三态：实时编辑占满 / 预览分屏（EPUB 等可全屏预览）/ 纯编辑占满。
    fn render_body(&mut self, window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let live_edit = self
            .tabs
            .get(self.active_tab)
            .map(|t| t.live_edit)
            .unwrap_or(false);
        if live_edit {
            // 实时预览编辑（WYSIWYG）本身即「所见即所得」，右侧 Markdown 预览冗余 →
            // 编辑区占满整块（不显示分割线与预览区），退出实时模式后恢复原分屏。
            div()
                .id("app-live-body")
                .debug_selector(|| "app-live-body".into())
                .flex()
                .flex_1()
                .min_h(px(0.))
                .min_w(px(0.))
                .overflow_hidden()
                .child(self.render_editor_pane(window, 1.0, cx))
                .into_any_element()
        } else if self.show_preview() {
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
                    .child(self.render_editor_pane(window, ratio, cx))
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
                .child(self.render_editor_pane(window, 1.0, cx))
                .into_any_element()
        }
    }

    /// 根视图：顶部栏 + 选项卡栏 + 内容区 + 状态栏 + 通知叠层，并挂载「拖入文件打开 /
    /// 多窗口选项卡拖拽 / 12 个快捷键动作」监听（一次组装完，render 保持薄分发）。
    fn render_root(
        &mut self,
        body: gpui::AnyElement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
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
            // on_drag_move 确认「正在拖选项卡」（携带下标）并在光标第一次越过窗口边缘时
            // 立即 tear-off 成独立窗口（窗口出现在光标处，拖出过程有过渡）；
            // on_mouse_up(_out) 作为兜底落点——窗口内松开 = 原位/普通点击（仅清状态），
            // 窗口外松开 = 独立窗口还原回主窗口。
            .on_drag_move(cx.listener(
                |this: &mut EditorApp, ev: &DragMoveEvent<TabDragData>, window, cx| {
                    let idx = ev.drag(cx).idx; // 拷贝下标，避免持有对 cx 的借用
                    // 已被 tear-off（源窗口不再有该选项卡）→ 忽略后续移动
                    if idx >= this.tabs.len() {
                        return;
                    }
                    this.tab_dragging = Some(idx);
                    let pos = ev.event.position;
                    let inside = gpui::Bounds::new(gpui::Point::default(), window.bounds().size)
                        .contains(&pos);
                    cx.notify();
                    if !inside {
                        // 拖过窗口边缘 → 立即把选项卡拆进光标处的新窗口（tear-off）
                        this.tear_off_drag(idx, pos, window, cx);
                    }
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
                cx.listener(|this: &mut EditorApp, ev: &gpui::MouseUpEvent, window, cx| {
                    this.release_tab_drag_outside(window, ev.position, cx);
                }),
            )
            // 图片/视频选项卡：右键 → 「锁定/解锁」干净查看/播放菜单（气泡到根统一处理）
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(Self::on_media_right_click),
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
            .children(self.render_shell_children(body, window, cx))
    }
}
