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
mod music_player;
mod palette;
mod panes;
mod recent_panel;
mod sidebar;
mod status_bar;
mod tabs;

// M2.2：新增的职责子模块（`use super::*` 共享下方 prelude）。可见性语义与原 mod.rs 等价：
// 原私有条目在 `app` 内对整棵子树（含 app::tests）可见 → 搬入子模块后统一 `pub(super)`
// （可见于 `app`）；原 `pub` / `pub(super)`（= crate 可见）搬入私有子模块后升为 `pub(crate)`）。
mod chrome;
mod close_tray;
mod diff;
mod encoding;
mod file_tree_prompt;
mod init;
mod layout;
mod media;
mod media_window;
mod pdf_bookmarks;
mod pdf_menu;
mod pdf_bg;
mod preview;
mod session;
mod settings_xfer;
mod sync;
mod tab_drag;

#[cfg(test)]
mod tests;

use std::any::Any;
use std::cell::Cell;
use std::fs;
use std::path::PathBuf;
use std::rc::Rc;

use gpui::{
    anchored, deferred, div, px, white, Anchor, AnimationExt, AnyWindowHandle, App,
    AppContext, Bounds, Context, CursorStyle, DismissEvent, DragMoveEvent, Edges, ExternalPaths,
    FocusHandle, Focusable, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, Pixels, Point, PromptLevel, Render, SharedString, Size,
    StatefulInteractiveElement, Styled, Subscription, VisualContext, Window, WindowBounds,
    WindowOptions,
};
use ui::input::{InputEvent, InputState};
use ui::menu::{PopupMenu, PopupMenuItem};
use ropey::Rope;
use ui::icon::{Icon, IconName};
use ui::theme::ChromeColors;
use ui::motion::hover_spring;
use base::util::SeqAfter;

// WYSIWYG 预览编辑——本项目 `markdown_wysiwyg::WyEditor`（已取代 zorite 移植的 `live`）。
use markdown_wysiwyg::{WyEditor, WyEvent};

use crate::actions::*;
use config::layout::{
    DEFAULT_FONT_SIZE, FONT_MAX, FONT_MIN, MENU_BAR_H, SIDEBAR_WIDTH, SPLIT_RATIO_MAX,
    SPLIT_RATIO_MIN,
};
use config::user::{PdfBgPref, ThemePref, UserPrefs};
use crate::scrollbar::TableScrollState;

/// 一个文件选项卡：独立持有自研编辑器组件状态、文件路径与脏标记。
struct EditorTab {
    /// 本选项卡的编辑区（[`editor::CodeEditor`]）。
    editor: gpui::Entity<editor::CodeEditor>,
    /// 当前文件路径（None = 未命名）
    file_path: Option<PathBuf>,
    /// 本选项卡的**源文本编码**（[`crate::app::encoding`]）：`None` = 纯 UTF-8（新建/
    /// 打开/媒体）；`Some(&'static Encoding)` = 打开时探测出的非 UTF-8 编码（含带 BOM
    /// 的 UTF-8/UTF-16）。保存（`write_to_path`）按此编码把缓冲 UTF-8 转回字节写盘。
    /// 随选项卡搬迁（EditorTab ⇄ DetachedTab 整包搬移）、文档编码随文档走。
    source_encoding: Option<&'static encoding_rs::Encoding>,
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
    /// 媒体阅读器（打开 .pdf / 图片 / 视频时创建，一次至多其一）：整页/整图/首帧只读
    /// 展示；`None` = 非媒体选项卡（文本 / Markdown / EPUB）。
    reader: MediaReader,
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
    /// 大文档 P1 后台装配在途（整篇 rope 尚在生产端）：编辑区已显示首块但**锁输入**
    /// （渲染层全区域透明拦截，见 panes.rs）——避免「打开期间编辑」与 EOF 整篇换装
    /// 竞态；`finish_file_text` 换装成功后置 false。
    loading_lock: bool,
    /// 是否处于「所见即所得预览编辑」模式：左面板用 `WyEditor` 代替源码编辑区；
    /// 退出时内容写回。
    live_edit: bool,
    /// 「预览编辑测试」：本项目 `markdown_wysiwyg::WyEditor`（以 `Doc` 为数据源）。
    /// None = 未启用。启用即 `live_edit = true`。
    live2: Option<gpui::Entity<WyEditor>>,
    /// 「预览编辑测试」本 live 会话里 WYSIWYG 内容是否被**编辑过**（`WyEvent::Changed`
    /// 订阅中对 live2 实体置位；进入 ON 与新建选项卡时清 false）。退出 `commit_live`
    /// 用它判断是否需要整篇写回源码编辑区：未编辑时 live2 文本恒等于打开瞬间的源码
    /// COW 快照（O(1) 同源），写回纯属 O(n) 浪费（大文档级联 `set_text` 重排卡 7-8s）
    /// → 直接销毁实体切回，不物化全文。
    live2_dirty: bool,
    /// A2：右栏 Markdown 预览统一组件（`WyEditor`，A1 只读门控）。MD 右栏/全屏预览
    /// 与 EPUB 只读宿主都走它（同一 `Doc` 管线的只读视图）；`None` = 未创建（文本 /
    /// PDF / 图片选项卡，或预览已关闭）。
    md_wy: Option<gpui::Entity<WyEditor>>,
    /// P4-1：EPUB 的**内存图片表**（虚拟 `epubimg://…` → 字节，不落盘）。打开时存
    /// 一份供 WyEditor 宿主（`ensure_epub_wy`）注入；随选项卡搬迁（与 `md_wy` 同）。
    epub_images: std::collections::HashMap<String, std::sync::Arc<gpui::Image>>,
    /// P4-2：EPUB 的**注记正文表**（`注记号 → 注释文本`）。打开时存一份供 WyEditor
    /// 宿主（`ensure_epub_wy` → `set_notes`）注入——悬浮卡正文；随选项卡搬迁。
    epub_notes: std::collections::BTreeMap<usize, String>,
    /// `md_wy` 右栏/全屏宿主的纵向滚动句柄：`#md-wy-scroll`（`overflow_y_scroll`）
    /// 经 `track_scroll` 跟踪它，右侧自绘 `Scrollbar::vertical` 跟随（与 `live_vscroll`
    /// 同型：每次渲染重建容器、句柄跨帧稳定）。随选项卡搬迁（与 `md_preview` 同）。
    md_wy_vscroll: gpui::ScrollHandle,
    /// 右栏 `md_wy`「在途装配」三代叠加态状态包（B3 后台整篇装配在途 / 装配代号 /
    /// Step2 提前整篇解析在途 / Step3「整篇在途·prefix 已装」收敛为单字段，见
    /// [`MdWyInstall`]）。随选项卡搬迁（EditorTab ⇄ DetachedTab）作为一个字段整包搬移，
    /// 不再逐带双份复制。
    md_wy_install: MdWyInstall,
    /// 编辑版本号：每次内容变更 +1。编辑区⇄预览滚动同步用它判断「画面/内容是否有新
    /// 编辑需重建同步基线」。
    edit_version: u64,
    /// 监听编辑事件 → 置脏 + 刷新状态栏（保持有效）
    _edit_sub: Subscription,
    /// 监听 PDF 阅读器 `notify()`（检索结果/加载进度到达）→ 刷新 app（侧边栏计数）。
    /// **随选项卡持有**：多 PDF 选项卡并存时各自独立订阅，避免旧单槽全局
    /// `EditorApp::_pdf_observe` 被后打开者覆盖、先打开 PDF 的 notify 丢失（真 bug）。
    /// 订阅绑定本窗口 EditorApp；随选项卡搬迁时随源 `EditorTab` 一起丢弃
    /// （`take_tab_payload` 解构 `..`），目标窗口 `adopt_tab` 重建。关选项卡 / 拔除时
    /// 随 `EditorTab` drop 自动退订（无幽灵回调 / 泄漏）。
    _pdf_sub: Option<gpui::Subscription>,
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
    editor: gpui::Entity<editor::CodeEditor>,
    /// 媒体阅读器（PDF / 图片 / 视频；与 [`EditorTab::reader`] 同型，跨窗口搬迁）。
    reader: MediaReader,
    /// A2：右栏 WyEditor 预览实体与宿主滚动句柄随选项卡搬迁（与 `md_preview` 同；
    /// 实体与窗口无关，可直接跨窗口移动）。
    md_wy: Option<gpui::Entity<WyEditor>>,
    md_wy_vscroll: gpui::ScrollHandle,
    /// 随选项卡搬迁（整包承接在途装配状态包，防搬迁后旧完成回调被误丢 / EOF 让位判断
    /// 错位 / sync 增量错打 prefix）。
    md_wy_install: MdWyInstall,
    /// P4-1：EPUB 内存图片表随选项卡搬迁（与 `md_wy` 宿主共用）。
    epub_images: std::collections::HashMap<String, std::sync::Arc<gpui::Image>>,
    /// P4-2：EPUB 注记正文表随选项卡搬迁（`ensure_epub_wy` → `set_notes` 用）。
    epub_notes: std::collections::BTreeMap<usize, String>,
    file_path: Option<PathBuf>,
    dirty: bool,
    /// 源文本编码（见 [`EditorTab::source_encoding`]；随选项卡搬迁）。
    source_encoding: Option<&'static encoding_rs::Encoding>,
    content: SharedString,
    line_count: usize,
    char_count: usize,
    cursor_row: usize,
    cursor_col: usize,
    preview_on: bool,
    preview_fullscreen: bool,
    readonly_preview: bool,
    editor_pending: bool,
    /// 大文档 P1 后台装配在途（整篇 rope 尚在生产端）：编辑区已显示首块但**锁输入**
    /// （渲染层全区域透明拦截，见 panes.rs）——避免「打开期间编辑」与 EOF 整篇换装
    /// 竞态；`finish_file_text` 换装成功后置 false。
    loading_lock: bool,
    edit_version: u64,
}

/// 编辑器被预览驱动后的落位收敛宽限（帧）：边界块（表格等）顶行读取可能滞后/抖动
/// 一两帧，此期内「编辑器落位」不回显为「编辑器主动滚动」；超过才放手由正常双向同步接管。
const DRIVEN_EDITOR_MAX_AWAIT: u32 = 8;

/// A4：Markdown 标题目录的跳转目标（侧边栏 `toc_item_frame` 主点击收起/滚动两用）：
/// 右栏统一组件 `WyEditor`（只读预览，条目为源码行，`scroll_to_line`）。
#[derive(Clone)]
pub(crate) enum MdTocScroll {
    Unified(gpui::Entity<WyEditor>),
}

/// Step2/3：大文档 md「打开即提前后台整篇解析」（与分块读盘并行、不等 EOF）在途状态。
/// 首块挂载后立即启动（`file_ops::apply_open_chunk` → `md_wy_spawn_early_install`）；
/// **Step3 两阶段**：先由 `md_wy_spawn_prefix_install` 装首块 prefix（首屏真内容、置
/// `md_wy_pending_full`），整篇就绪后本路径**带守卫换入全文**（滚动锚按 src_line 钉顶行），
/// 置 [`Self::Done`]。EOF 收尾（`finish_file_text`）据此**跳过重复 install**
/// （[`Self::Pending`]/[`Self::Done`] 时让位早期装配，不再重复
/// 整篇 `Doc::open`），[`Self::Failed`]/`None` 才回退走既有 EOF 整篇装配（兜底）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MdWyEarly {
    /// 在途：`seq` = 启动时装配代号、`edit_version` = 启动时编辑版本（此后用户编辑 →
    /// 完成回调按代号/版本双守卫丢弃在途整篇，避免把「装旧」覆盖用户改动）。
    Pending { seq: usize, edit_version: u64 },
    /// 已由提前解析装配完成（全文就绪）。
    Done,
    /// 提前读盘/解析失败（EOF 回退走既有整篇装配兜底）。
    Failed,
}

/// 右栏 `md_wy`「在途装配」状态的单字段状态包：收敛 B3（`pending` + `seq`）、Step2
/// （`early`）、Step3（`pending_full`）三代叠加态，EditorTab ⇄ DetachedTab 搬迁时作为
/// **一个字段**整包搬移（不再逐带双份复制进 DetachedTab）。
///
/// 四子项的语义与旧字段一一对应（行为零变化）：`pending` 与 `seq` 可共存（`feed_full`
/// 同时登记后台整篇在途与代号）；`early` 与 `pending_full` 亦可在 Step2/3 两阶段协作时
/// 同时置位——故收敛为**状态包**而非互斥枚举，避免改变「各标记独立置位/清位」的既有
/// 时序语义。
#[derive(Default)]
pub(crate) struct MdWyInstall {
    /// 后台整篇装配在途（原 `md_wy_pending`）：`Some((装配代号, 目标内容))` = 已登记
    /// 后台 `open_from_str` 解析、尚未 `install_open`；`None` = 无在途。异步完成回调按
    /// 代号与 `seq` 匹配才装配（不装错、不泄漏）。
    pub(crate) pending: Option<(usize, SharedString)>,
    /// 装配代号单调计数器（原 `md_wy_install_seq`）：每次发起后台整篇装配 / 任何同步
    /// 改源（增量重放、同步喂源、换源 install_open、预览关闭）递增，结束在途旧装配
    /// （代号不匹配即丢弃其完成回调，防陈旧整篇覆盖刚改好的内容）。
    pub(crate) seq: usize,
    /// Step2 提前整篇解析在途状态（原 `md_wy_early`，见 [`MdWyEarly`]）。
    pub(crate) early: Option<MdWyEarly>,
    /// Step3「整篇在途」标记（原 `md_wy_pending_full`）：`Some((装配代号, prefix 字节
    /// 长))` = 预览只装了**首块 prefix**（已 `is_loaded`）、整篇后台解析在途——此期间
    /// 预览 ≠ 编辑区全文，`sync` 增量桥**禁止**把编辑区坐标增量打到 prefix（会错位），
    /// 改走「作废在途整篇 + 异步整篇重喂」；整篇就绪带守卫换入全文后置 `None`。
    pub(crate) pending_full: Option<(usize, usize)>,
}

/// 选项卡拖拽携带的数据：目标选项卡下标 + 当前显示名 + 真实 laid-out 尺寸。
/// `size` 每帧由 [`crate::app::tabs::TabBoundsRecorder`] 写入（包在选项卡元素外，把
/// 其 border-box 尺寸抄进来），拖拽预览 / 幽灵窗按它定宽定高——彻底避免「文本估算
/// 偏窄 → 窗内变小 / 窗外内容溢出被裁」。
#[derive(Clone, Debug)]
pub(crate) struct TabDragData {
    idx: usize,
    name: SharedString,
    size: Rc<Cell<Size<Pixels>>>,
}

/// 拖拽选项卡时跟随鼠标的「文件选项卡」半透明预览视图：窗内 gpui 拖拽 picture 与窗外
/// 幽灵窗共用同一根视图（视觉一致）——仿真实选项卡（真实 laid-out 尺寸、圆角、边框、
/// 标题 + 脏点 + 视觉关闭钮），整体半透明表达「正在拖动」。
/// 尺寸用真实 tab 的 `size`（每帧由 `tabs::TabBoundsRecorder` 写入）**显式定宽高**：
/// gpui 拖拽 picture 以 `AvailableSpace::min_size()` 布局（window.rs:3737），无固定宽
/// 会被 min-content 压窄（逆"窗内变小"）；幽灵窗口也按同一尺寸创建（逆"窗外溢出被裁"）。
struct TabDragGhost {
    name: SharedString,
    dirty: bool,
    size: Rc<Cell<Size<Pixels>>>,
    /// true = 作为窗外幽灵窗根视图（不画 1px 边框：悬于桌面时该描边会被单独呈现为
    /// 「四根线条围住选项卡」）；false = 作为窗内拖拽 picture（贴近真实激活选项卡）。
    ghost: bool,
}

impl Render for TabDragGhost {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = ui::theme::Theme::global(cx);
        let c = ui::theme::ChromeColors::for_mode(theme.is_dark());
        let s = self.size.get();
        let w = if s.width > px(0.) { s.width } else { px(120.) };
        let h = if s.height > px(0.) { s.height } else { px(26.) };
        let mut base = div()
            .id("tab-drag-ghost")
            .debug_selector(|| "tab-drag-ghost".into())
            .w(w)
            .h(h)
            .flex()
            .items_center()
            .gap_1()
            .px_2()
            .rounded_md()
            .bg(c.tab_active_bg.opacity(0.85));
        // 仅在窗内 picture 保留 1px 描边（与真实激活选项卡观感一致）；窗外幽灵窗不描边。
        if !self.ghost {
            base = base
                .border_1()
                .border_color(c.tab_active_border.opacity(0.7));
        }
        base.text_size(px(12.))
            .text_color(c.text_primary.opacity(0.92))
            .child(if self.dirty { "● " } else { "" })
            .child(self.name.clone())
            // 视觉关闭钮（不交互）：与真实 tab 同款拖尾，整体观感更接近原选项卡。
            .child(
                div()
                    .w(px(18.))
                    .h(px(18.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_md()
                    .text_size(px(13.))
                    .text_color(c.text_minor.opacity(0.8))
                    .child("×"),
            )
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
    /// 右栏预览「Scrub 延后」在途（I2）：预览被快速拖动 / 高频滚屏**连续跨块**时置
    /// 真，期内的编辑区 `scroll_to_line` 跟随**延后**（不逐帧嵌编辑器全窗重建）；
    /// 预览静止（松手 / 停滚）后一次收敛到最终目标，恢复双侧对齐。
    scrub: bool,
    /// 进入 Scrub 的连续「预览跨块」帧计数：由同步函数逐帧维护（`preview_moved`
    /// 连续即累加、中断即清零），达到 [`crate::app::preview::SCRUB_ENTER_STREAK`]
    /// 才进入延后——低频单步滚屏不触发，保持慢滚逐帧对齐。
    scrub_streak: u32,
    /// Scrub 延后期内「预览静止」的连续帧计数：达到
    /// [`crate::app::preview::SCRUB_IDLE_FRAMES`]（或墙钟兜底）即一次收敛。
    scrub_idle_frames: u32,
    /// Scrub 延后的收敛目标 = 延后期内最新一次预览顶块（松手/静止后编辑器一次
    /// 收敛到它）。
    scrub_target: Option<usize>,
    /// 距上次预览跨块的墙钟（静止收敛的墙钟兜底，见
    /// [`crate::app::preview::SCRUB_IDLE`]；帧稀疏时保证仍会收敛）。
    scrub_last_cross: std::time::Instant,
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
            scrub: false,
            scrub_streak: 0,
            scrub_idle_frames: 0,
            scrub_target: None,
            scrub_last_cross: std::time::Instant::now(),
        }
    }
}

pub struct EditorApp {
    /// 所有文件选项卡
    tabs: Vec<EditorTab>,
    /// 当前激活的选项卡下标
    active_tab: usize,
    /// 选项卡栏内各选项卡**上一帧** laid-out 的边界（与拖拽事件 `position` 同一输入坐标
    /// 空间、含横向滚动偏移）：栏内拖拽排序的**命中测试**用。`render_tab_bar` 每次重建
    /// （长度 = `tabs.len()`、按下标对应；记录器在 prepaint 阶段把真实边界写回各槽位）。
    tab_bounds: Vec<Rc<Cell<Bounds<Pixels>>>>,
    /// 选项卡栏横向滚动状态（标签过多时横向滚动 + 滚动条）
    tab_bar_hscroll: TableScrollState,
    /// 横向滚动条自动隐藏延迟（`poke_tab_hscroll` 重启）：可重入、代序作废，旧定时器
    /// 到点不误清新滚动。[`TAB_HSCROLL_HIDE_MS`] 后无新滚动再隐藏滚动条。
    tab_hscroll_delay: SeqAfter,
    /// 顶部状态文字与是否"正常"
    status: SharedString,
    status_ok: bool,
    /// 通知 toasts（自研 `ui::notification`，App 自持容器与生命周期）。
    notifications: Vec<ui::notification::Notification>,
    /// 编辑器正文字号（px，用户可调）
    font_size: f32,
    /// 是否深色（黑）模式：控制组件主题、自绘色板与原生标题栏深浅
    dark_mode: bool,
    /// 主题偏好三态：Follow=跟随系统 / Light / Dark（手动切亮/暗会覆盖「跟随」）。
    theme_pref: ThemePref,
    /// 编辑区 / 预览区分屏比例（0..1，= 编辑区占宽比例）
    split_ratio: f32,
    /// 编辑区是否自动换行（默认换行）。开关只作用编辑区；Markdown 预览
    /// 始终自动换行，不随此偏好变化（预览换行由文件类型/全屏阅读模式决定）。
    wrap_enabled: bool,
    /// 「恢复上次会话」开关（文件 → 菜单切换，随 [`UserPrefs`] 立即持久化；默认开）。
    /// 关闭时退出不写会话清单并清理遗留清单/槽位，下次启动不恢复。
    session_restore: bool,
    /// 「关闭到系统托盘」开关（文件 → 菜单切换，随 [`UserPrefs`] 立即持久化；默认关）。
    /// 开启时点标题栏 × / Alt+F4 隐藏到系统托盘图标（托盘菜单「退出」才真正退出）。
    close_to_tray: bool,
    /// 「媒体在新窗口打开」开关（查看 → 菜单切换，随 [`UserPrefs`] 立即持久化；默认开）。
    /// 开启时打开音乐/视频不在当前窗口、而在同进程新窗口（或复用已有媒体窗口，见
    /// [`Self::reuse_media_window`]）；关闭时保持现行行为（当前窗口打开）。
    media_new_window: bool,
    /// 「复用已有媒体窗口」开关（查看 → 菜单切换，随 [`UserPrefs`] 立即持久化；默认开）。
    /// 仅 `media_new_window` 开启时生效：开 = 复用最近的媒体窗口；关 = 每次新开独立窗口。
    reuse_media_window: bool,
    /// 音乐窗口的已记录尺寸（逻辑像素；来自 [`UserPrefs::music_window_size`]，随专用媒体
    /// 窗口 resize 自动写回）。`Some` = 新开音乐窗口按该尺寸居中；`None` = 跟随发起窗口。
    music_window_size: Option<Size<Pixels>>,
    /// 视频窗口的已记录尺寸（与 [`Self::music_window_size`] 对称，音乐/视频分开记录）。
    video_window_size: Option<Size<Pixels>>,
    /// 媒体窗口尺寸记录的防抖落盘（[`media_window::EditorApp::record_media_window_size`]）：
    /// 连续 resize 不逐事件写盘，停顿 [`MEDIA_SIZE_FLUSH_DEBOUNCE_MS`] 后才 persist 一次。
    /// （读取点被 `#[cfg(not(test))]` 门控 → 测试构建报未读，属性注释压掉噪声。）
    #[cfg_attr(test, allow(dead_code))]
    media_size_flush_debounce: SeqAfter,
    /// 会话持久化防抖（`schedule_session_flush`）：可重入、代序作废（仿侧边栏搜索
    /// 防抖 [`Self::search_debounce`]）。内容变化 300ms 停顿后才写盘一次，不逐键写盘。
    /// （读取点 `schedule_session_flush` 被 `#[cfg(not(test))]` 门控 → 测试构建报该字段
    /// 未读，属性注释压掉此噪声。）
    #[cfg_attr(test, allow(dead_code))]
    session_flush_debounce: SeqAfter,
    /// 会话恢复进行中：恢复期间重建的选项卡不触发持久化（避免反复写盘 / 抵抗
    /// `clear_session` 清理）。
    session_restoring: bool,
    /// 会话槽位「上次落盘内容」指纹：slot 名 → 内容 hash。`save_session_to` 据此跳过
    /// 内容未变的槽位重写——切换选项卡/结构变化不再全量重写所有未命名内容（消除主线程
    /// 同步磁盘 IO 热点），真正变化（防抖到期/编辑后）才重写该槽位。关开关/恢复完成时
    /// 清空，杜绝「旧指纹误跳过 → 盘上槽位缺失」。每次落盘后只保留仍在引用的槽位
    /// （内存随建/关选项卡不膨胀）。
    session_slot_hashes: std::collections::HashMap<String, u64>,
    /// 上次成功写盘时的清单指纹（清单序列化后 hash）。相等 → 本次保存是「无变化重入」
    /// （如 `new_tab` 紧随 `make_tab` 的二次即时落盘、内容防抖收敛后的等价重写），
    /// 清单与 prune 一并跳过——盘面已是最新，零文件 IO（本机防病毒下每次文件创建
    /// 5~25ms，无谓清单写正是切选项卡/新建残留热点的最大项）。清空路径同
    /// `session_slot_hashes`。
    session_last_manifest_digest: Option<u64>,
    /// 正在拖拽分屏分割线：`Some((起始指针x, 起始比例))`
    split_drag: Option<(f32, f32)>,
    /// 拖分割条帧级合并（B4）：`on_mouse_move` 只记**最新目标比例**，本帧 `render`
    /// 开头统一落地到 `split_ratio`（对齐 scrollbar `pending_target`→prepaint 先例）。
    /// 语义不变：分割条拖动仍实时跟手（每帧一落），只合并 notify 频率。
    pending_split_ratio: Option<f32>,
    /// 分割线是否被鼠标悬停（悬停时高亮提示可拖拽）
    splitter_hovered: bool,
    /// 侧边栏「外壳」状态：可见性 / 视图 / 目录折叠 / 宽度与右缘拖拽
    /// （检索输入 + 扫描缓存等仍在此处，见 [`sidebar::SidebarChromeState`]）。
    sidebar: sidebar::SidebarChromeState,
    /// 侧边栏「搜索」关键字输入控件。
    search_state: gpui::Entity<InputState>,
    /// 监听搜索框输入变化 → 刷新搜索结果。
    _search_sub: Subscription,
    /// 已推送给 PDF 阅读器的查询（`(活动标签, 查询词)`）：两者都未变才跳过推送，
    /// 切换标签/重开 PDF 后即使查询相同也会重新推送。
    pdf_search: Option<(usize, String)>,
    /// 侧边栏搜索的惰性扫描缓存：按（查询词, 活动文档）复用，渲染帧不重扫全文。
    /// 查询变化 / 切换文档 / 内容被编辑时清空，下次渲染重建。
    search_scan: Option<std::rc::Rc<sidebar::SidebarSearchScan>>,
    /// 侧边栏搜索**范围**（当前文件 / 整个文件夹）。用户未手动选过时按
    /// 「有打开文件夹默认选整个文件夹」自动跟随（见 [`EditorApp::effective_search_scope`]）。
    pub(crate) search_scope: sidebar::SearchScopeTab,
    /// 用户是否**手动**切换过搜索范围（手选后自动跟随规则不再覆盖）。
    pub(crate) search_scope_manual: bool,
    /// 文件夹级内容搜索的扫描结果（后台增量装配，按键 `(根, 查询词)` 复用，渲染帧不重扫）。
    /// 换根 / 关文件夹 / 查询变化 → 置 `None` 触发新扫；旧代异步回调按代丢弃。
    pub(crate) folder_search: Option<std::rc::Rc<sidebar::FolderSearchScan>>,
    /// 文件夹扫描**代**：每发起一次新扫描 +1，异步回调携带发起时代，与当前代不符即丢弃
    /// （防「换查询后旧扫描结果晚到覆盖新结果」）。
    pub(crate) folder_search_gen: u64,
    /// quick open 后台扫描**代**：每打开一次 Ctrl+P / 换工作区即 +1，异步回调按代丢弃
    /// （防「旧扫描晚到覆盖新面板结果 / 串工作区」；同 `folder_search_gen` 姿势）。
    pub(crate) quick_open_gen: u64,
    /// 最近一条 GoToLine 提示通知的 key（自托管：下次跳转行提示先移除上一条，不堆积）。
    goto_line_toast_key: Option<gpui::ElementId>,
    /// 最近一条 Find「当前选项卡不支持查找」提示通知的 key（自托管，同 goto_line 姿势）。
    find_toast_key: Option<gpui::ElementId>,
    /// 最近一条「导出/导入设置」结果提示通知的 key（自托管，下次提示先移除上一条，不堆积）。
    settings_toast_key: Option<gpui::ElementId>,
    /// 实际执行过的搜索扫描次数（每次命中缓存复用不改）。测试用：验证「查询未变
    /// 时反复渲染不重扫」这一优化真实生效。
    pub(crate) search_scan_count: usize,
    /// 侧边栏搜索「防抖后提交生效」的查询词（已归一化：trim + to_lowercase）。
    /// 渲染/扫描只看它；输入框实时文本仍在 `search_state`。连续输入在防抖窗内只
    /// 更新 `search_pending`，期满才提交本字段并触发一次扫描（不再每键全量重扫）。
    pub(crate) search_committed: String,
    /// 防抖窗口内最新一次输入（未提交的归一化查询词）；到点写入 `search_committed`。
    search_pending: String,
    /// 搜索防抖（[`Self::schedule_search_debounce`]）：可重入、代序作废——旧防抖定时器
    /// 到点被作废，只有最新一次输入期满才提交给扫描。
    search_debounce: SeqAfter,
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
    /// 本次拖拽开始时 gpui 拖拽 picture 的「抓取偏移」（`on_drag` 构造器入参 offset =
    /// 按下点在选项卡点击矩形内的偏移；拖拽中 picture 恒画在 `鼠标 - offset`）。窗外
    /// 跟手幽灵窗按同一偏移对齐定位，使「窗内 picture ↔ 窗外幽灵」在窗口边缘**无缝
    /// 衔接**（不跳变、不卡顿）。`on_drag` 构造器写入，拖拽结束清。
    tab_drag_grab: Option<Point<Pixels>>,
    /// 拖拽中的选项卡光标已越出窗口边缘（**尚未松手**）：置位 = 已进入「松开才建窗」
    /// 预位——真文件窗口只在 `on_mouse_up_out` 松开时建立（`release_tab_drag_outside`），
    /// 越界瞬间**不再开窗**（旧 tear-off 行为已移除）；回到窗内 / 窗内松开清位、
    /// 选项卡原位保留。
    drag_outside_armed: bool,
    /// 本次 `on_drag_move` 算出的「幽灵窗应显示」决策（= 光标越界 **或** 窗内拖拽 picture
    /// 将要被窗口边缘遮断，见 `tab_drag::drag_picture_about_to_clip`）：真机由
    /// `place_drag_ghost`（SetWindowPos 显/隐）执行；测试平台拿不到 HWND 无法观察该调用，
    /// 故此字段供回归锁定「贴边提前切完整幽灵窗」的显隐决策。
    drag_ghost_visible: bool,
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
    /// 拖拽选项卡排序：本次拖拽过程光标下的「插入下标」（相对当前 `tabs`；`tabs.len()`
    /// = 插到末尾；左半/右半/空隙/首尾全覆盖）。`None` = 光标不在选项卡行内（不排序，
    /// 拖出窗口仍走拆窗）。`on_drag_move` 每帧刷新；`on_mouse_up`（窗内松开）消费落地后
    /// 清；拖出窗口（拆窗）时置 None。渲染据此画白色插入指示线。
    tab_reorder_target: Option<usize>,
    /// 本窗口是否为「拖出拆分」生成的独立文件窗口。是 → 把选项卡拖出窗口 = 还原回主窗口；
    /// 否（主窗口）→ 把选项卡拖出窗口 = 拆分出新的独立窗口。
    is_detached: bool,
    /// 会话持久化责任窗口：仅**首个主窗口**（`new_impl` 的 `startup=true`）为 true。
    /// 媒体窗口 / 拆出窗口为 false——会话清单是**单份全局文件**（`session/session.json`），
    /// 非主窗口若参与 `persist_session` 会用它自己的选项卡覆盖主窗口清单（打开视频即触发
    /// `make_tab` 的即时落盘 → 下次启动主窗口会话被视频选项卡顶替丢失）。见 session.rs
    /// `persist_session` 门控与 init.rs `new_impl`。
    session_persist: bool,
    /// 图片/视频右键「锁定/解锁」上下文菜单：弹出位置（窗口坐标）。None = 未打开。
    media_lock_menu_pos: Option<Point<Pixels>>,
    /// 已打开的媒体右键菜单实体（随位置同生同灭）。
    media_lock_menu_entity: Option<gpui::Entity<ui::menu::PopupMenu>>,
    /// 媒体右键菜单的关闭订阅（选择/Esc/点击外 → 清理位置与实体）。
    media_lock_menu_sub: Option<Subscription>,
    /// PDF 右键「添加书签/插入页面/删除页面/提取页面」上下文菜单：弹出位置（窗口坐标）。
    pdf_menu_pos: Option<Point<Pixels>>,
    /// 已打开的 PDF 右键菜单实体（随位置同生同灭）。
    pdf_menu_entity: Option<gpui::Entity<PopupMenu>>,
    /// PDF 右键菜单的关闭订阅（选择/Esc/点击外 → 清理位置与实体）。
    pdf_menu_sub: Option<Subscription>,
    /// PDF 阅读画布背景预设（config `pdf_bg`；**只改画布**，绝不动 PDF 位图/文件）。
    /// `default` 跟随主题；显式预设/自定义后主题切换不再覆盖（见 chrome.rs 守卫）。
    pdf_bg: PdfBgPref,
    /// PDF 自定义画布底色（config `pdf_bg_custom`，`#rrggbb`；仅 `pdf_bg == Custom` 档
    /// 消费，非法值应用层回退默认）。
    pdf_bg_custom: Option<String>,
    /// 最近一次对 PDF 页操作失败已提示过的错误文案：`_pdf_sub` 的 observe 里
    /// 对比「变化」才 toast 一次（成功/未操作置 None），防同一错误重复弹提示。
    pdf_last_error_seen: Option<String>,
    /// Phase B3：PDF 阅读位置书签（规范化路径 → 列表）。启动经 `UserPrefs::config_path()`
    /// 同目录 `pdf_bookmarks.json` 加载；添加/删除即改内存 + 原子落盘。
    pdf_bookmarks: std::collections::HashMap<String, Vec<pdf_bookmarks::PdfBookmark>>,
    /// 选项卡栏右键「新建/打开」上下文菜单：弹出位置（窗口坐标）。None = 未打开。
    tab_bar_menu_pos: Option<Point<Pixels>>,
    /// 已打开的选项卡栏右键菜单实体（随位置同生同灭）。
    tab_bar_menu_entity: Option<gpui::Entity<PopupMenu>>,
    /// 选项卡栏右键菜单的关闭订阅（选择/Esc/点击外 → 清理位置与实体）。
    tab_bar_menu_sub: Option<Subscription>,
    /// 文件树右键上下文菜单：弹出位置（窗口坐标）。None = 未打开。
    file_tree_menu_pos: Option<Point<Pixels>>,
    /// 已打开的文件树右键菜单实体（随位置同生同灭）。
    file_tree_menu_entity: Option<gpui::Entity<PopupMenu>>,
    /// 文件树右键菜单的关闭订阅（选择/Esc/点击外 → 清理位置与实体）。
    file_tree_menu_sub: Option<Subscription>,
    /// 文件树「新建/重命名」输入弹层：打开时为 `Some`，提交/取消（`DismissEvent`）
    /// 关闭后清空。弹层是独立 [`file_tree_prompt::FileTreePrompt`] 实体。
    file_tree_prompt: Option<gpui::Entity<file_tree_prompt::FileTreePrompt>>,
    /// 文件树输入弹层的关闭订阅（提交成功/取消 → 清空引用释放实体）。
    _file_tree_prompt_sub: Option<Subscription>,
    /// 文件树「过滤文件名」输入框（typeahead 按名过滤树；防抖后提交生效）。
    files_filter_state: gpui::Entity<InputState>,
    /// 过滤输入框变化订阅（防抖调度）。
    _files_filter_sub: Subscription,
    /// 文件树过滤词（防抖后提交、已归一化小写；渲染/收集只看它）。
    pub(crate) files_filter: String,
    /// 防抖窗口内最新一次过滤输入（未提交；布局避免逐键重收）。
    files_filter_pending: String,
    /// 文件树过滤防抖（`base::SeqAfter` 可重入、代序作废，同搜索防抖姿势）。
    files_filter_debounce: SeqAfter,
    /// 最近打开的文件夹路径（新→旧，至多 [`config::limits::RECENT_MAX`] 条）。
    /// 跨重启持久化（`UserPrefs::recent_folders`）；面板四区域模式只展示前 10。
    pub(crate) recent_folders: Vec<String>,
    /// 最近打开的文件路径（新→旧，至多 [`config::limits::RECENT_MAX`] 条）。
    /// 跨重启持久化（`UserPrefs::recent_files`）；面板四区域模式只展示前 10。
    pub(crate) recent_files: Vec<String>,
    /// 「打开最近」居中大面板（反馈 2）：打开时为 `Some`，择项 / Esc / 点击外
    /// （`DismissEvent`）关闭后清空。面板是独立 [`recent_panel::RecentPanel`] 实体
    /// （mode/选中/最近列表快照自持；More/返回 单实体切模式）。
    recent_panel: Option<gpui::Entity<recent_panel::RecentPanel>>,
    /// 最近面板的关闭订阅（择项/Esc/点击外 → 清空引用并释放实体）。
    _recent_panel_sub: Option<Subscription>,
    focus_handle: FocusHandle,
    /// 编辑区⇄预览（MD 统一组件 WyEditor）滚动同步基线：当前选项卡 + 上次已同步的
    /// 双方块起点键。每帧渲染开头比对、由「动了的那一侧」驱动对端；切换选项卡/换
    /// 文档（块号失义）即重置。
    md_scroll_sync: MdScrollSync,
    /// 「跟随系统主题」时的外观变化订阅（`Window::observe_window_appearance`）；
    /// 离开跟随模式后回调只是空返，切换回跟随会立即按当前系统落地一次。
    _appearance_sub: Option<Subscription>,
    /// 专用媒体窗口（`new_media_window` 创建）的尺寸变化订阅（`observe_window_bounds`）：
    /// 用户 resize 后按当前角色自动记录音乐/视频窗口尺寸到 config.json（下次新开按记录
    /// 尺寸）。主窗口 / 拖出窗口 / 复用目标不注册（不记录），`None` 即未注册。
    _bounds_sub: Option<Subscription>,
    /// 命令面板（P2-8）：打开时为 `Some`，选择/Esc/点击外（`DismissEvent`）关闭后清空。
    /// 面板是独立 `CommandPalette` 实体（query/选中/模式/条目自持），新建即情境。
    palette: Option<gpui::Entity<ui::palette::CommandPalette>>,
    /// 面板关闭订阅（选择/Esc/点击外 → 清空 `palette` 并释放实体）。
    _palette_sub: Option<Subscription>,
    /// A：退出预览编辑（`commit_live`）时被 `take(2)` 的旧 live2 实体**延迟回收队列**：
    /// WyEditor 是大对象（`Doc` rope/行索引 + `Rc` 级联），Drop 的级联释放实测 ~36ms——
    /// 若在点击切回的事件路径内同步 Drop，会卡进「切回」延迟里。切回时把实体暂存此处、
    /// 并由 `cx.on_next_frame` 在**下一帧渲染后**清空（放实体出作用域 Drop），把级联释放
    /// 移出点击同步路径。渲染路径在 `commit_live` 之后已不再引用旧实体（`tab.live2` 置
    /// None），延迟 Drop 不影响任何渲染。
    retired_live2: Vec<gpui::Entity<WyEditor>>,
    /// 音乐播放器视图（惰性创建；不随退出音乐模式销毁——播放继续）。
    music: Option<gpui::Entity<music::MusicView>>,
    /// 是否正处于音乐模式（整窗渲染 MusicView；false = 编辑器正常界面）。
    music_mode: bool,
    /// 音乐视图事件订阅（`MusicEvent::ExitRequested` → 退出音乐模式并把焦点还给编辑区）。
    _music_sub: Option<Subscription>,
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

/// 选项卡持有的媒体查看器（Reader 收敛，取代原先 pdf/image/video 三个独立
/// `Option<Entity<_>>` 指针）：一个选项卡至多其一。enum + 每变体持有具体实体
/// （不用 dyn / 不建 trait 抽象层），消费方 `match` 收敛即可；随选项卡搬迁
/// （[`DetachedTab::reader`]）整体移动，无逐字段复制样板。
#[derive(Clone)]
enum MediaReader {
    /// 非媒体选项卡（文本 / Markdown / EPUB）。
    None,
    /// PDF 阅读器：整页原样显示（打开 .pdf 时创建，解析/栅格化自包含后台）。
    Pdf(gpui::Entity<pdf::PdfViewer>),
    /// 图片查看器：整图展示 + 同文件夹切换 + Ctrl+滚轮缩放。
    Image(gpui::Entity<image_viewer::ImageViewer>),
    /// 视频查看器：M1 首帧静态阅读 + 帧钟/控制键。
    #[cfg(feature = "video")]
    Video(gpui::Entity<video::VideoViewer>),
}

impl MediaReader {
    /// 是否未挂任何媒体阅读器（文本 / Markdown / EPUB 选项卡）。
    fn is_none(&self) -> bool {
        matches!(self, MediaReader::None)
    }

    /// 是否挂有「自带阅读器」的媒体（PDF / 图片 / 视频）——预览绝不以 WyEditor 渲染。
    fn is_reader(&self) -> bool {
        !self.is_none()
    }

    /// 是否有「锁定」语义的媒体（图片 / 视频；PDF 无右键锁定菜单）。
    fn is_lockable(&self) -> bool {
        if matches!(self, MediaReader::Image(_)) {
            return true;
        }
        #[cfg(feature = "video")]
        if matches!(self, MediaReader::Video(_)) {
            return true;
        }
        false
    }
}

impl Focusable for EditorApp {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}
