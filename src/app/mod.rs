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
//!
//! ## 模块结构（M2.2 拆分）
//!
//! 本模块只保留 **类型定义 / 状态与共享 prelude**（`EditorApp` 结构、`EditorTab`、
//! 拖拽/滚动同步的辅助类型与常量，及对全部子模块可见的 `use` 预导入）。`EditorApp`
//! 的方法按职责拆分到下列子模块（`impl` 块跨文件合并，行为与可见性语义不变）：
//!
//! - [`init`]：构造 / 窗口注册 / 焦点句柄（`EditorApp::new`）
//! - [`sync`]：编辑器事件订阅、脏追踪与后台预览增量同步循环
//! - [`tab_drag`]：跨窗口选项卡搬迁 / 拖出拆分 / 还原
//! - [`preview`]：编辑 ⇄ 预览切换、统一 WyEditor 预览与滚动同步、WYSIWYG 活区、路径/语言推断
//! - [`media`]：图片 / 视频查看器接线与右键「锁定/解锁」菜单
//! - [`chrome`]：主题 / 外壳配色、标题栏与通知层
//! - [`layout`]：主体布局与根视图渲染（`Render` 实现、欢迎页、快捷键接线）
//!
//! 其余职责已在更早拆分：`file_ops`（打开/保存/粘贴）、`open_progress`（大文档分块
//! 异步就绪）、`menu`（菜单栏）、`palette`（命令面板）、`panes`（编辑/预览宿主与
//! 分割线）、`sidebar`（侧边栏）、`status_bar`（状态栏）、`tabs`（选项卡栏）。

mod file_ops;
mod open_progress;
mod menu;
mod palette;
mod panes;
mod sidebar;
mod status_bar;
mod tabs;

// M2.2：新增的职责子模块（`use super::*` 共享下方 prelude）。可见性语义与原 mod.rs 等价：
// 原私有条目在 `app` 内对整棵子树（含 app::tests）可见 → 搬入子模块后统一 `pub(super)`
// （可见于 `app`）；原 `pub` / `pub(super)`（= crate 可见）搬入私有子模块后升为 `pub(crate)`）。
mod chrome;
mod init;
mod layout;
mod media;
mod preview;
mod sync;
mod tab_drag;

#[cfg(test)]
mod tests;

use std::any::Any;
use std::fs;
use std::path::PathBuf;

use gpui::{
    anchored, deferred, div, px, white, Anchor, AnimationExt, AnyWindowHandle, App,
    AppContext, Bounds, Context, CursorStyle, DismissEvent, DragMoveEvent, Edges, ExternalPaths,
    FocusHandle, Focusable, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, Pixels, Point, PromptLevel, Render, SharedString, StatefulInteractiveElement,
    Styled, Subscription, VisualContext, Window, WindowBounds, WindowOptions,
};
use crate::ui::input::{InputEvent, InputState};
use crate::ui::menu::{PopupMenu, PopupMenuItem};
use ropey::Rope;
use crate::ui::icon::{Icon, IconName};
use crate::ui::theme::ChromeColors;
use crate::ui::motion::hover_spring;

// WYSIWYG 预览编辑——本项目 `crate::markdown::wysiwyg::WyEditor`（已取代 zorite 移植的 `live`）。
use crate::markdown::{WyEditor, WyEvent};

use crate::actions::*;
use crate::config::layout::{
    DEFAULT_FONT_SIZE, FONT_MAX, FONT_MIN, MENU_BAR_H, SIDEBAR_WIDTH, SPLIT_RATIO_MAX,
    SPLIT_RATIO_MIN,
};
use crate::config::user::{ThemePref, UserPrefs};
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

/// EPUB 预览后端开关（P4-1「EPUB 迁 WyEditor 只读宿主」的可回滚开关）：
/// `true`（默认）= EPUB 预览走 **WyEditor 只读宿主**（`tab.md_wy`）；`false` = 回退
/// 现有 **MarkdownPreview** 路径（注记/引文/页码等 A 侧能力，P4-2 再收口）。
/// 运行时读环境变量 `EPUB_PREVIEW_BACKEND`（`"0"`/`"off"` → 回退；其余 → 新宿主），
/// 便于线上/测试按进程回滚。（仅测试）实例字段 `epub_backend_force_legacy` 优先。
impl EditorApp {
    pub(crate) fn epub_preview_backend_wy(&self) -> bool {
        #[cfg(test)]
        if self.epub_backend_force_legacy {
            return false;
        }
        std::env::var("EPUB_PREVIEW_BACKEND").map_or(true, |v| {
            !v.eq_ignore_ascii_case("0") && !v.eq_ignore_ascii_case("off")
        })
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
    /// P4-1：EPUB 的**内存图片表**（虚拟 `epubimg://…` → 字节，不落盘）。打开时存
    /// 一份供 WyEditor 宿主（`ensure_epub_wy`）注入；随选项卡搬迁（与 `md_wy` 同）。
    epub_images: std::collections::HashMap<String, std::sync::Arc<gpui::Image>>,
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
    /// P4-1：EPUB 内存图片表随选项卡搬迁（与 `md_wy` 宿主共用）。
    epub_images: std::collections::HashMap<String, std::sync::Arc<gpui::Image>>,
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

/// 编辑器被预览驱动后的落位收敛宽限（帧）：边界块（表格等）顶行读取可能滞后/抖动
/// 一两帧，此期内「编辑器落位」不回显为「编辑器主动滚动」；超过才放手由正常双向同步接管。
const DRIVEN_EDITOR_MAX_AWAIT: u32 = 8;

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
        let theme = crate::ui::theme::Theme::global(cx);
        let ghost_bg =
            crate::ui::theme::ChromeColors::for_mode(theme.is_dark()).drag_ghost_bg;
        div()
            .id("tab-drag-ghost")
            .debug_selector(|| "tab-drag-ghost".into())
            .px_3()
            .py_1()
            .rounded_lg()
            .bg(ghost_bg)
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
    /// 我方（预览主导时）最近一次要求编辑器到达的目标块：编辑器落位到该块是「按
    /// 我方命令跟上」，与 `driven_preview_target` 对称——否则编辑器跟随落位会被
    /// 误判成「编辑器先动 → 驱动预览」的反馈回声，把预览拽回（来回跳动/滚不下去）。
    driven_editor_target: Option<usize>,
    /// `driven_editor_target` 的收敛宽限帧数：编辑器落位通常同步、但边界块（表格等）
    /// 顶行读取可能滞后/抖动一帧到数帧，期间回显必须被抑制；超过宽限才放手让正常
    /// 双向同步接管（防编辑器永远到不了目标时同步停摆）。
    driven_editor_await: u32,
}

impl Default for MdScrollSync {
    fn default() -> Self {
        Self {
            tab: usize::MAX,
            editor_version: None,
            editor_block: None,
            preview_block: None,
            driven_preview_target: None,
            driven_editor_target: None,
            driven_editor_await: 0,
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
    /// （仅测试）强制本实例 EPUB 预览走 MarkdownPreview 回退后端（免 env 跨并行测试竞态）。
    #[cfg(test)]
    epub_backend_force_legacy: bool,
    /// 预览同步的代序号：每轮去抖发起时 +1；后台 `scan_blocks` 结果只允许最新代
    /// 落盘，旧代迟到结果直接丢弃（防旧内容覆盖新编辑）。
    preview_seq: u64,
    /// 是否深色（黑）模式：控制组件主题、自绘色板与原生标题栏深浅
    dark_mode: bool,
    /// 主题偏好三态：Follow=跟随系统 / Light / Dark（手动切亮/暗会覆盖「跟随」）。
    theme_pref: ThemePref,
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
    /// 「打开文件对话框」的调用次数（空态「打开文件…」按钮点击 / 文件菜单 / Ctrl+O
    /// 共用 `open_dialog` 入口）。测试用：断言空态按钮触发 open（测试为 stub 不真弹窗）。
    pub(crate) open_dialog_calls: usize,
    /// 窗口控制钮最后渲染帧解析出的图标前景色（渲染阶段落盘，仅测试/调试可读）：
    /// 断言「hover/按压注入色」确实经过真实渲染路径（由 hover/press keyed 态驱动）。
    /// `None` = 标题栏尚未渲染过。
    pub(crate) last_title_ctrl_fg: Option<TitleCtrlIconFg>,
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
    /// 选项卡栏右键「新建/打开」上下文菜单：弹出位置（窗口坐标）。None = 未打开。
    tab_bar_menu_pos: Option<Point<Pixels>>,
    /// 已打开的选项卡栏右键菜单实体（随位置同生同灭）。
    tab_bar_menu_entity: Option<gpui::Entity<PopupMenu>>,
    /// 选项卡栏右键菜单的关闭订阅（选择/Esc/点击外 → 清理位置与实体）。
    tab_bar_menu_sub: Option<Subscription>,
    focus_handle: FocusHandle,
    /// 编辑区⇄预览（MD 统一组件 WyEditor）滚动同步基线：当前选项卡 + 上次已同步的
    /// 双方块起点键。每帧渲染开头比对、由「动了的那一侧」驱动对端；切换选项卡/换
    /// 文档（块号失义）即重置。
    md_scroll_sync: MdScrollSync,
    /// 「跟随系统主题」时的外观变化订阅（`Window::observe_window_appearance`）；
    /// 离开跟随模式后回调只是空返，切换回跟随会立即按当前系统落地一次。
    _appearance_sub: Option<Subscription>,
    /// 命令面板（P2-8）：打开时为 `Some`，选择/Esc/点击外（`DismissEvent`）关闭后清空。
    /// 面板是独立 `CommandPalette` 实体（query/选中/模式/条目自持），新建即情境。
    palette: Option<gpui::Entity<crate::ui::palette::CommandPalette>>,
    /// 面板关闭订阅（选择/Esc/点击外 → 清空 `palette` 并释放实体）。
    _palette_sub: Option<Subscription>,
}

/// 窗口控制钮（最小化/最大化/关闭）上一帧解析出的图标前景色。
/// 渲染阶段 `title_ctrl_button` 落盘，供测试断言「注入色」经真实渲染路径生效。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct TitleCtrlIconFg {
    pub minimize: gpui::Hsla,
    pub maximize: gpui::Hsla,
    pub close: gpui::Hsla,
}

/// 标题栏窗口控制钮类型：平台统一按 `window_control_area` 命中区走原生语义
/// （WM_NCHITTEST → HTMINBUTTON/HTMAXBUTTON/HTCLOSE），这里只按类型分支
/// 图标 / 底色 / 注入色。
#[derive(Clone, Copy, PartialEq, Eq)]
enum WinCtrlKind {
    Minimize,
    Maximize,
    Close,
}

impl WinCtrlKind {
    fn id(self) -> &'static str {
        match self {
            WinCtrlKind::Minimize => "title-minimize",
            WinCtrlKind::Maximize => "title-maximize",
            WinCtrlKind::Close => "title-close",
        }
    }

    fn area(self) -> gpui::WindowControlArea {
        match self {
            WinCtrlKind::Minimize => gpui::WindowControlArea::Min,
            WinCtrlKind::Maximize => gpui::WindowControlArea::Max,
            WinCtrlKind::Close => gpui::WindowControlArea::Close,
        }
    }

    fn is_close(self) -> bool {
        matches!(self, WinCtrlKind::Close)
    }

    /// hover 背景淡入的填充色：关闭钮红（危险钮观感），其余灰（P2-10 统一弹簧）。
    fn hover_bg(self, c: ChromeColors) -> gpui::Hsla {
        if self.is_close() {
            c.close_hover.into()
        } else {
            c.hover_fill.into()
        }
    }

    /// 按压底色：关闭钮用 P1-7 `close_pressed` 深一档危险红，其余用通用按压灰。
    fn active_bg(self, c: ChromeColors) -> gpui::Hsla {
        if self.is_close() {
            c.close_pressed.into()
        } else {
            c.pressed_fill.into()
        }
    }
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
