//! 应用主视图 `EditorApp`：文本 / 代码 / Markdown 编辑器（多文件选项卡）。
//!
//! 布局（自上而下）：
//! - 菜单栏：文件 / 编辑 / 查看 / 帮助，点击弹出下拉菜单（[`EditorApp::render_menu_bar`]）
//! - 选项卡栏：每个文件一个选项卡，可新建（+）、切换、关闭（未保存时弹窗提示保存）
//! - 编辑区：自研 [`editor::CodeEditor`]，内置语法高亮、行号、括号标尺、
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
//! ## 模块结构
//!
//! 本模块只保留 **类型定义 / 状态与共享 prelude**（`EditorApp` 结构、`MdScrollSync`、
//! `MdTocScroll` / `EditorAppRegistry`，及对全部子模块可见的 `use` 预导入）。`EditorTab`
//! / `DetachedTab` / `MediaReader` / `MdWyInstall` 等**纯模型类型**在独立 crate
//! `shell_core`，此处 `pub use shell_core::{…}` 重导出维持既有路径。`EditorApp`
//! 的方法按职责拆分到下列子模块（`impl` 块跨文件合并）：
//!
//! - [`init`]：构造 / 窗口注册 / 焦点句柄（`EditorApp::new`）
//! - [`sync`]：编辑器事件订阅、脏追踪与后台预览增量同步循环
//! - [`tab_drag`]：跨窗口选项卡搬迁 / 拖出拆分 / 还原
//! - [`preview`]：编辑 ⇄ 预览切换、统一 WyEditor 预览与滚动同步、WYSIWYG 活区、路径/语言推断
//! - [`media`]：图片 / 视频查看器接线与右键「锁定/解锁」菜单
//! - [`chrome`]：主题 / 外壳配色、标题栏与通知层
//! - [`layout`]：主体布局与根视图渲染（`Render` 实现、欢迎页、快捷键接线）
//!
//! 其余职责分布：`file_ops`（打开/保存/粘贴）、`text_open`（大文档分块
//! 异步就绪 / 编码 / 差异，独立叶子 crate）、`menu`（菜单栏）、`palette`（命令面板）、
//! `panes`（编辑/预览宿主与分割线）、`sidebar`（侧边栏）、`status_bar`（状态栏）、
//! `tabs`（选项卡栏）。

mod file_ops;
mod menu;
mod music_player;
mod palette;
mod panes;
mod sidebar;
mod status_bar;
mod tabs;

// 职责子模块（`use super::*` 共享下方 prelude）的可见性约定：
// 私有条目在 `app` 内对整棵子树（含 app::tests）可见 → 统一 `pub(super)`（可见于 `app`）；
// 原 `pub` / `pub(super)`（= crate 可见）在私有子模块内升为 `pub(crate)`）。
mod chrome;
mod close_tray;
// 文件树「新建/重命名」输入弹层：类型/渲染在域 crate（shell_sidebar），宿主经
// FileTreePromptHost 反调提交动作；此处为重导出 facade（路径保持既有）。
pub mod file_tree_prompt {
    pub use shell_sidebar::{FileTreePrompt, FileTreePromptHost, FileTreePromptKind};
}
mod host_impl;
mod init;
mod layout;
mod media;
mod media_window;
mod file_tree_ops;
// PDF 域宿主薄适配：域逻辑在 shell_pdf；本模块为宿主 EditorApp 的
// 书签/背景/菜单动作薄适配 + 右键菜单视图/挂载。命名避开 `pdf`（作 `pdf` 会遮蔽
// 同 subtree 的 `pdf::PdfViewer` 等 extern crate 路径）。
mod pdf_host;
mod preview;
mod session;
mod settings_xfer;
mod sync;
mod tab_drag;

#[cfg(test)]
mod tests;

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

use gpui::{
    div, px, white, AnyWindowHandle, App,
    AppContext, Bounds, Context, CursorStyle, DismissEvent, DragMoveEvent, EntityId,
    ExternalPaths, FocusHandle, Focusable, InteractiveElement, IntoElement, MouseButton,
    MouseDownEvent, ParentElement, Pixels, Point, PromptLevel, Render, SharedString, Size,
    StatefulInteractiveElement, Styled, Subscription, VisualContext, WeakEntity, Window,
    WindowBounds, WindowOptions,
};
use ui::input::{InputEvent, InputState};
use ui::menu::{PopupMenu, PopupMenuItem};
use ropey::Rope;
use ui::icon::IconName;
use ui::theme::ChromeColors;
use base::util::SeqAfter;

// WYSIWYG 预览编辑——本项目 `markdown_wysiwyg::WyEditor`（已取代 zorite 移植的 `live`）。
// （`WyEvent` 随订阅实现在 `shell_preview`，无需再在宿主 prelude。）
use markdown_wysiwyg::WyEditor;

use crate::actions::*;
use config::layout::{
    DEFAULT_FONT_SIZE, FONT_MAX, FONT_MIN, MENU_BAR_H, SIDEBAR_WIDTH, SPLIT_RATIO_MAX,
    SPLIT_RATIO_MIN,
};
use config::user::{PdfBgPref, ThemePref, UserPrefs};
use shell_widgets::scrollbar::TableScrollState;

// `EditorApp` 的纯模型类型在独立 crate `shell_core`。
// 此处经 `pub use` 重导出，维持既有 `app::EditorTab` / `app::DetachedTab` 等路径
// 与子模块 `use super::*` 的可见性语义不变。
pub use shell_core::{
    AppHost, AppModel, CommitLiveAlign, DetachedTab, EditorTab, MdWyEarly, MdWyInstall, MediaReader,
    PanesState, PdfBookmark, SearchScopeTab, TabDragData, TabDragGhost, TitleCtrlIconFg,
    WinCtrlKind,
};
/// 「阅读进度恢复」值类型（属 session_store 叶子 crate；经 `shell::app` 重导出供
/// 集成测试（`tests/reading_progress.rs`）按其 variant 断言与宿主共同引用同一类型。
pub use session_store::ReadingProgress;

/// 测试可见重导出：
/// - `app::session` 私有模块的纯逻辑入口（`save_session_to` / `restore_session_from`）：
///   integration 目标（`tests/*.rs` 独立二进制）只能见 lib pub 面，私有 `mod session` 的
///   路径不可达，经此 `pub use` 暴露。`restore_session_from` 恒编译
///   （测试/基准唯一驱动，生产进程不调用，pub 故无 dead_code 告警）。
/// - `app::sidebar` 私有模块的宿主状态类型与视图枚举：它们的 `width`/`files` 等字段随
///   `EditorApp::sidebar` 字段一并 pub，供 `tests/settings_xfer.rs` 直读/直写
///   （`import_opened_folder_pops_children` / 宽度即时生效断言）。
#[doc(hidden)]
pub use session::{restore_session_from, save_session_to};
#[doc(hidden)]
pub use sidebar::{FileTreeState, SidebarChromeState, SidebarView, TreeEntry};
// `palette`/`media_window`/`pdf_bookmarks`
// 私有模块的纯逻辑面供 `tests/palette.rs`（quick open 检索）、`tests/media.rs`
// （媒体窗口打开决策/尺寸取值）、`tests/pdf.rs`（书签盘上读写）访问。
#[doc(hidden)]
// `media_window` 纯决策在 `shell_media::media_window`；
// 此处经 shell_media 重导出维持既有 `app::{resolve_media_open_target, ...}` 面
// （`tests/media.rs` 行为表 / 尺寸取值断言）。
pub use shell_media::media_window::{
    resolve_media_open_target, resolve_recorded_size, MediaOpenTarget, MediaWindowKind,
};
#[doc(hidden)]
pub use palette::{quick_open_collect, QUICK_OPEN_MAX_FILE_BYTES, QUICK_OPEN_MAX_FILES};
#[doc(hidden)]
// `pdf_bookmarks` 纯逻辑在 `shell_pdf::bookmarks`；
// 此处经 shell_pdf 重导出维持既有 `app::{load, normalize_key}` 面（书签盘上断言）。
pub use shell_pdf::bookmarks::{load, normalize_key};
#[doc(hidden)]
pub use file_ops::MediaKind;

/// 所有 `EditorApp` 窗口的注册表（全局）：记录每个窗口的弱实体句柄与窗口句柄。
/// 「拖出拆分」创建的子窗口带 `is_detached=true`，把选项卡拖回主窗口时据此找到
/// 主窗口（第一个非 detached 的 EditorApp 窗口）。
/// `#[doc(hidden)] pub`：`tests/bench.rs`（`bench_new_media_window_first_frame` 断
/// 言「窗口数 +1」）与 `tests/` 媒体夹具经 `cx.global::<EditorAppRegistry>()` 读
/// `windows` 长度/枚举（§8.6 外提清单；生产仍仅供 shell 内部使用）。
#[derive(Default)]
#[doc(hidden)]
pub struct EditorAppRegistry {
    #[doc(hidden)]
    pub windows: Vec<(gpui::WeakEntity<EditorApp>, AnyWindowHandle)>,
}


impl gpui::Global for EditorAppRegistry {}

/// 编辑区⇄预览滚动同步基线（见 [`EditorApp::sync_editor_preview_scroll`]）。记录
/// 当前选项卡 + 编辑器内容版本 + 上次已同步的双方「块起点」键，渲染帧比对后由动了
/// 的那一侧驱动对端。切换选项卡 / 同一选项卡内换文档 / 编辑使内容版本变化 → 重建。
/// 原定义（`struct` + 手写 `Default`）在 shell_core；仅 `mod app` 子树消费，
/// 故用模块私有 `use` 引入。
use shell_core::MdScrollSync;

/// Mica 材质层单段（`render_material_layer` 构建期一次算好、`material_layer_cache` 缓存
/// 的段参数）：像素顶边 / 像素高 / 颜色；`last=true` 最后一段时 `height` 忽略（paint 期
/// 锚到实际窗底、随窗口高度拉伸，桶内 resize 无需重算）。`pub(super)`：EditorApp 字段
/// 与 chrome.rs（`use super::*`）共同消费。
pub(super) struct MaterialBand {
    /// 段顶边距窗口顶的像素。
    pub(super) top: f32,
    /// 段像素高（`last` 时忽略）。
    pub(super) height: f32,
    /// 段颜色（lightness 插值）。
    pub(super) color: gpui::Hsla,
    /// 是否最后一段（锚到底，随实际窗高拉伸）。
    pub(super) last: bool,
}

pub struct EditorApp {
    /// 模型半状态统一容器：`tabs` / `active_tab` 等文件/会话/搜索/偏好模型态在
    /// `shell_core::AppModel`；UI 半状态（窗口 / 布局 / 聚焦 / 主题 glue / registry）
    /// 仍留宿主字段。宿主读写模型态一律走
    /// `self.model.borrow()/.borrow_mut()`。`Rc<RefCell>` 供宿主与 `AppHost` 共享。
    /// 注意：RefCell 是**单元格级**互斥（非字段级 split 借用），同一方法内持有
    /// `borrow_mut()` 守卫时不可再 `borrow()`/`borrow_mut()`（运行期双借 panic）。
    pub model: Rc<RefCell<AppModel>>,
    /// 选项卡栏内各选项卡**上一帧** laid-out 的边界（与拖拽事件 `position` 同一输入坐标
    /// 空间、含横向滚动偏移）：栏内拖拽排序的**命中测试**用（命中计算在
    /// `shell_tabs::drag::{tab_insert_index, tab_drag_over_row}` 纯函数，此快照以参数传入）。
    /// `TabBar` 实体渲染期每帧重建（长度 = `tabs.len()`、按下标对应；经 `render_tab_bar`
    /// 的 `&mut` 入参写回本宿主字段——记录器在 prepaint 阶段把真实边界写回各槽位）。
    tab_bounds: Vec<Rc<Cell<Bounds<Pixels>>>>,
    /// 选项卡栏横向滚动状态（标签过多时横向滚动 + 滚动条；`TabBar` 实体渲染期以 `&`
    /// 入参接线）。
    tab_bar_hscroll: TableScrollState,
    /// 横向滚动条自动隐藏延迟（`poke_tab_hscroll` 重启）：可重入、代序作废，旧定时器
    /// 到点不误清新滚动。[`TAB_HSCROLL_HIDE_MS`] 后无新滚动再隐藏滚动条。
    tab_hscroll_delay: SeqAfter,
    /// 通知 toasts（自研 `ui::notification`，App 自持容器与生命周期）。
    /// `#[doc(hidden)] pub`：`tests/settings_xfer.rs` 断言导入/导出结果 toast
    /// （`app.notifications.last().message_str()`，§8.6 外提清单）。
    #[doc(hidden)]
    pub notifications: Vec<ui::notification::Notification>,
    /// 「编辑区 ⇄ 预览分割条」面板状态（[`PanesState`]，纯值类型，在 shell_core）：
    /// `split_ratio` / `split_drag` / `pending_split_ratio` / `splitter_hovered` 四字段
    /// 收敛于此。以**普通字段**持有（非 `Rc<RefCell>`）：render / 拖拽事件路径多处直读。
    /// `#[doc(hidden)] pub`：`tests/splitter.rs` 拖拽/悬停断言读 `panes.*`（§8.6 外提清单）。
    #[doc(hidden)]
    pub panes: PanesState,
    /// 侧边栏「外壳」状态：可见性 / 视图 / 目录折叠 / 宽度与右缘拖拽
    /// （类型 `SidebarChromeState` 在 shell_sidebar；本字段仍是**普通值**——
    /// `app::tests` 与 `tests/*` 的既有字段读写不变量强制，见 §8.6 外提链）。
    /// `#[doc(hidden)] pub`：`tests/settings_xfer.rs` 直读/直写 `width`/`width_auto`/
    /// `files.root`/`files.children`（§8.6 外提清单）。
    #[doc(hidden)]
    pub sidebar: shell_sidebar::SidebarChromeState,
    /// 侧边栏**渲染/事件实体**（承接自 shell_sidebar 域 crate）：
    /// 外壳（活动栏/调宽手柄）+ 目录（TOC）视图 + 搜索（`crate::search`)
    /// + 文件树（files/files_ops）在 `Entity<Sidebar<EditorApp>>` 上构建
    /// （`Context<Sidebar>` 内 `cx.listener` 绑定域实体）；宿主渲染期以 `&mut self.sidebar`
    /// 传入状态、事件回调经 `SidebarHost` 反调本宿主。搜索输入实体亦收进域
    /// （测试经 `sidebar::EditorApp::search_state(cx)` 兼容读取器取句柄）。
    sidebar_view: gpui::Entity<shell_sidebar::Sidebar<EditorApp>>,
    /// 监听搜索框输入变化 → 刷新搜索结果。
    _search_sub: Subscription,
    /// 选项卡栏**渲染/事件实体**（承接自 shell_tabs 域 crate）：选项卡栏
    /// 渲染 + 选项卡元素 + 眼睛预览钮 + 横向滚动条接线 + 右键菜单项构建在
    /// `Entity<TabBar<EditorApp>>` 上（`Context<TabBar>` 内 `cx.listener` 绑定域实体）；
    /// 宿主渲染期把 `&mut self.tab_bounds` / `&self.tab_bar_hscroll` 与预取的
    /// `active_is_md`/`preview_on` 传入，事件回调经 `TabsHost` 反调本宿主。
    tab_bar_view: gpui::Entity<shell_tabs::TabBar<EditorApp>>,
    /// 大文档「两段式打开」快段（`load_file_text` 延迟挂载编辑器）执行次数。
    /// 测试用：验证大文件先瞬时挂编辑器、再后台补齐统计/预览的完整流程两段都跑了。
    pub(crate) open_fast_count: usize,
    /// 大文档「两段式打开」慢段（`finish_file_text` 应用统计 + 预览索引）执行次数。
    /// `#[doc(hidden)] pub`：`tests/bench.rs` 流水线就绪判据（§8.6 外提清单）。
    #[doc(hidden)]
    pub open_finish_count: usize,
    /// 「打开文件对话框」的调用次数（空态「打开文件…」按钮点击 / 文件菜单 / Ctrl+O
    /// 共用 `open_dialog` 入口）。测试用：断言空态按钮触发 open（测试为 stub 不真弹窗）。
    pub(crate) open_dialog_calls: usize,
    /// 窗口控制钮最后渲染帧解析出的图标前景色（渲染阶段落盘，仅测试/调试可读）：
    /// 断言「hover/按压注入色」确实经过真实渲染路径（由 hover/press keyed 态驱动）。
    /// `None` = 标题栏尚未渲染过。
    pub(crate) last_title_ctrl_fg: Option<TitleCtrlIconFg>,
    /// 窗口控制钮图标前景色探针（`icon_color_for` 闭包同步写入的共享 `Rc`）：构建期
    /// **一次创建、宿主长期持有**，最小化/最大化/关闭三钮每帧复用同一份——避免每钮
    /// 每帧新建 `Rc<RefCell>` 的分配噪音（标题栏每帧重建 ×3）。渲染路径与原先相同：
    /// 闭包写入 → 构建后按当前 kind 合并进 `last_title_ctrl_fg`。
    pub(crate) title_ctrl_fg_probe: std::rc::Rc<std::cell::RefCell<TitleCtrlIconFg>>,
    /// Mica 材质层缓存：`(高度桶, dark_mode)` → 已算好的 64 段材质带（`Rc` 共享）。
    /// 根渲染每帧调 `render_material_layer`，桶/主题不变时只 clone `Rc` 复用段数组、
    /// 由 `MaterialLayer` 元素直接画 64 个 quad——不再每帧重建 64 个 band Div；
    /// 仅尺寸桶 / 明暗主题变化才重算段数组。`None` = 尚未渲染过。
    material_layer_cache: Option<(f32, bool, Rc<Vec<MaterialBand>>)>,
    /// 拖出窗外时跟随鼠标的「幽灵胶囊」置顶透明小窗句柄（见 [`Self::update_drag_ghost`]）：
    /// 让被拖选项卡在窗外**仍跟手可见**（gpui 拖拽幽灵只画在窗内，窗外被裁剪）；
    /// 回到窗内 / 松开 / 取消即销毁。None = 未建 / 已销毁。
    drag_ghost: Option<AnyWindowHandle>,
    /// 拆出窗口**构造期**（`new_detached` → `new_impl` 里首次 `adopt_tab` 收纳）抑制多余
    /// `cx.notify()`：构造紧随 `open_window` 的**同步首帧**，构造期入队的 Notify/RefreshWindows
    /// 效果要在首帧**之后**的 effect flush 才应用 → 新窗口白画一整帧冗余重绘（大档首帧
    /// ~27-40ms，松手→新窗肉眼延时的大头之一）。仅构造期置位，其余常态路径不受影响。
    constructing: bool,
    /// 幽灵窗创建已 `cx.defer` 延迟在建（防同周期内第 2 次 `on_drag_move` 重复排程建第 2 个
    /// 幽灵窗）；defer 回调落地（建成或放弃）后清 false。
    drag_ghost_pending: bool,
    /// 本窗口是否为「拖出拆分」生成的独立文件窗口。是 → 把选项卡拖出窗口 = 还原回主窗口；
    /// 否（主窗口）→ 把选项卡拖出窗口 = 拆分出新的独立窗口。
    is_detached: bool,
    /// 会话持久化责任窗口：仅**首个主窗口**（`new_impl` 的 `startup=true`）为 true。
    /// 媒体窗口 / 拆出窗口为 false——会话清单是**单份全局文件**（`session/session.json`），
    /// 非主窗口若参与 `persist_session` 会用它自己的选项卡覆盖主窗口清单（打开视频即触发
    /// `make_tab` 的即时落盘 → 下次启动主窗口会话被视频选项卡顶替丢失）。见 session.rs
    /// `persist_session` 门控与 init.rs `new_impl`。
    session_persist: bool,
    /// 图片/视频右键「锁定/解锁」上下文菜单槽位（位置/实体/订阅三件套，
    /// [`ui::menu::MenuSlot`]）。None = 未打开。
    media_lock_menu: ui::menu::MenuSlot,
    /// PDF 右键「添加书签/插入页面/删除页面/提取页面」上下文菜单槽位。
    /// `#[doc(hidden)] pub`：`tests/pdf.rs` 菜单项禁用判定经 `.pdf_menu.entity`
    /// （§8.6 外提清单）。
    #[doc(hidden)]
    pub pdf_menu: ui::menu::MenuSlot,
    /// 选项卡栏右键「新建/打开」上下文菜单槽位。
    /// （菜单**构建与交互**在 shell_tabs `TabBar` 实体，槽位仍留宿主——
    /// `app/tests/{tabs_actions,tabs_context}.rs` 直读、media/pdf 前置弹窗互关直清；实体
    /// 经 `TabsHost` 落地。）
    tab_bar_menu: ui::menu::MenuSlot,
    /// 文件树右键上下文菜单**测试兼容镜像**：实体（`shell_sidebar::files_ops`）自持
    /// pos/菜单实体/订阅，宿主只镜像菜单实体供 `app::tests` 直接字段断言
    /// （实体在 shell_sidebar，宿主留薄兼容镜像）。
    #[doc(hidden)]
    pub file_tree_menu_entity: Option<gpui::Entity<PopupMenu>>,
    /// 文件树「新建/重命名」输入弹层**测试兼容镜像**：实体（`shell_sidebar::files_ops`）
    /// 自持弹层实体/订阅，宿主只镜像弹层实体供 `app::tests` 直接字段断言/驱动输入
    /// （实体在 shell_sidebar，宿主留薄兼容镜像）。
    #[doc(hidden)]
    pub file_tree_prompt: Option<gpui::Entity<shell_sidebar::FileTreePrompt<EditorApp>>>,
    /// 文件树「过滤文件名」输入框（typeahead 按名过滤树；防抖后提交生效）。
    files_filter_state: gpui::Entity<InputState>,
    /// 过滤输入框变化订阅（防抖调度）。
    _files_filter_sub: Subscription,
    /// 「打开最近」居中大面板（反馈 2）：打开时为 `Some`，择项 / Esc / 点击外
    /// （`DismissEvent`）关闭后清空。面板是独立 [`session_store::RecentPanel`] 实体
    /// （mode/选中/最近列表快照自持；More/返回 单实体切模式），`<EditorApp>` 为
    /// [`session_store::RecentPanelHost`] 宿主。
    /// `#[doc(hidden)] pub`：`tests/recent.rs` 大量面板断言（§8.6 外提清单）。
    #[doc(hidden)]
    pub recent_panel: Option<gpui::Entity<session_store::RecentPanel<EditorApp>>>,
    /// 最近面板的关闭订阅（择项/Esc/点击外 → 清空引用并释放实体）。
    _recent_panel_sub: Option<Subscription>,
    focus_handle: FocusHandle,
    /// 编辑区⇄预览（MD 统一组件 WyEditor）滚动同步基线：当前选项卡 + 上次已同步的
    /// 双方块起点键。每帧渲染开头比对、由「动了的那一侧」驱动对端；切换选项卡/换
    /// 文档（块号失义）即重置。
    md_scroll_sync: MdScrollSync,
    /// 阅读进度脏哨兵：`(活动选项卡下标, 上次观测到的阅读位置)`。每帧 `render` 由
    /// [`Self::check_reading_progress`] 比对——位置变化才重启防抖落盘（防抖只该在
    /// 滚动停顿后触发一次，避免滚停后无限周期重写）。会话恢复完成处由
    /// `seed_reading_progress_sentinel` 播种为真实位置（R2：防恢复后首帧把存档覆写 0）。
    rp_last: Option<(usize, Option<session_store::ReadingProgress>)>,
    /// 「跟随系统主题」时的外观变化订阅（`Window::observe_window_appearance`）；
    /// 离开跟随模式后回调只是空返，切换回跟随会立即按当前系统落地一次。
    _appearance_sub: Option<Subscription>,
    /// 专用媒体窗口（`new_media_window` 创建）的尺寸变化订阅（`observe_window_bounds`）：
    /// 用户 resize 后按当前角色自动记录音乐/视频窗口尺寸到 config.json（下次新开按记录
    /// 尺寸）。主窗口 / 拖出窗口 / 复用目标不注册（不记录），`None` 即未注册。
    _bounds_sub: Option<Subscription>,
    /// 命令面板（P2-8）：打开时为 `Some`，选择/Esc/点击外（`DismissEvent`）关闭后清空。
    /// 面板是独立 `CommandPalette` 实体（query/选中/模式/条目自持），新建即情境。
    /// `#[doc(hidden)] pub`：`tests/palette.rs` quick open 扫描就绪轮询（§8.6 外提清单）。
    #[doc(hidden)]
    pub palette: Option<gpui::Entity<ui::palette::CommandPalette>>,
    /// 面板关闭订阅（选择/Esc/点击外 → 清空 `palette` 并释放实体）。
    _palette_sub: Option<Subscription>,
    /// 音乐播放器视图（惰性创建；不随退出音乐模式销毁——播放继续）。
    /// `#[doc(hidden)] pub`：`tests/music.rs`/`tests/media.rs` 断言音乐模式已建视图
    /// （`app.music.is_some()`）与 `ExitRequested` 事件驱动（§8.6 外提清单）。
    #[doc(hidden)]
    pub music: Option<gpui::Entity<music::MusicView>>,
    /// 音乐视图事件订阅（`MusicEvent::ExitRequested` → 退出音乐模式并把焦点还给编辑区）。
    _music_sub: Option<Subscription>,
    /// 宿主实体自身弱句柄/实体 id——`AppHost` 的 `&self` 服务方法经它再入宿主实体
    /// （订阅回调 / 延迟回调 / 异步任务里读模型需在回调体进行，见 host.rs 借用约束）。
    own_weak: WeakEntity<EditorApp>,
    own_entity_id: EntityId,
}

impl Focusable for EditorApp {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

