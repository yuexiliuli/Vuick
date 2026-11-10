//! 外壳宿主模型类型（2026-11 自 `crates/shell` 的 `app/mod.rs` 拆出）。
//!
//! 纯结构移动：`EditorApp` 的模型类型独立成 `shell_core` crate，宿主编排（`impl
//! EditorApp`）留在 shell。字段可见性按「宿主 / 测试跨 crate 要读写者 → `pub`」调整
//! （最小面，SC-2 再全面）；类型自包含的 inherent impl（[`MediaReader`] /
//! [`WinCtrlKind`] / [`Render` for [`TabDragGhost`]]）随类型迁入。
//!
//! 依赖的宿主类型（[`crate::app::EditorApp`] / `EditorAppRegistry` / `MdScrollSync` /
//! `MdTocScroll`）不在此 crate。

use std::cell::Cell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

use gpui::{
    div, px, Context, ElementId, InteractiveElement, IntoElement, ParentElement, Pixels, Point,
    Render, SharedString, Size, Styled, Subscription, Window,
};
use markdown_wysiwyg::WyEditor;
use ui::theme::ChromeColors;

use base::util::SeqAfter;
use config::user::{PdfBgPref, ThemePref};

/// 外壳宿主「模型半」状态的统一容器（SC-2/SC-3，2026-11-10 自 `EditorApp` 迁出）。
///
/// 只装**跨窗口搬迁 / 会话持久化 / 设置与搜索态 / 测试直接断言的模型态**（tabs /
/// active_tab / 侧边栏搜索态 / 最近文件 / 会话编排 / 偏好设置等）；UI 半状态（窗口句柄 /
/// 布局 / 聚焦 / 主题 glue / registry / 输入框实体 / 菜单实体）留在宿主 `EditorApp`。
/// 宿主经 `Rc<RefCell<AppModel>>` 持有，借 read/write 分 `borrow()/.borrow_mut()`。
///
/// ⚠️ 借用纪律（SC-2 同款）：RefCell 是**单元格级**互斥，同一方法内持有 `borrow_mut()`
/// 守卫时不可再 `borrow()`/`borrow_mut()`（运行期双借 panic）；跨字段读写在守卫内一次
/// 完成，方法调用前先 `drop` 守卫。
pub struct AppModel {
    /// 所有文件选项卡（原 `EditorApp::tabs`）。
    pub tabs: Vec<EditorTab>,
    /// 当前激活的选项卡下标（原 `EditorApp::active_tab`）。
    pub active_tab: usize,
    /// 顶部状态文字（状态栏；原 `EditorApp::status`）。
    pub status: SharedString,
    /// 顶部状态文字是否"正常"（原 `EditorApp::status_ok`）。
    pub status_ok: bool,
    /// 编辑器正文字号（px，用户可调；原 `EditorApp::font_size`）。
    pub font_size: f32,
    /// 是否深色（黑）模式：控制组件主题、自绘色板与原生标题栏深浅（原 `dark_mode`）。
    pub dark_mode: bool,
    /// 主题偏好三态：Follow=跟随系统 / Light / Dark（原 `EditorApp::theme_pref`）。
    pub theme_pref: ThemePref,
    /// 编辑区是否自动换行（默认换行；原 `EditorApp::wrap_enabled`）。
    pub wrap_enabled: bool,
    /// 「恢复上次会话」开关（文件 → 菜单切换，随 [`UserPrefs`] 持久化；原 `session_restore`）。
    pub session_restore: bool,
    /// 「关闭到系统托盘」开关（文件 → 菜单切换，持久化；原 `EditorApp::close_to_tray`）。
    pub close_to_tray: bool,
    /// 「媒体在新窗口打开」开关（查看 → 菜单切换，持久化；原 `media_new_window`）。
    pub media_new_window: bool,
    /// 「复用已有媒体窗口」开关（查看 → 菜单切换，持久化；原 `reuse_media_window`）。
    pub reuse_media_window: bool,
    /// 音乐窗口的已记录尺寸（逻辑像素，来自 [`UserPrefs::music_window_size`]；原 `music_window_size`）。
    pub music_window_size: Option<Size<Pixels>>,
    /// 视频窗口的已记录尺寸（与 `music_window_size` 对称；原 `video_window_size`）。
    pub video_window_size: Option<Size<Pixels>>,
    /// 媒体窗口尺寸记录的防抖落盘（原 `EditorApp::media_size_flush_debounce`）。
    pub media_size_flush_debounce: SeqAfter,
    /// 会话持久化防抖（原 `EditorApp::session_flush_debounce`）：可重入、代序作废，
    /// 内容变化停顿后才写盘一次。
    pub session_flush_debounce: SeqAfter,
    /// 会话恢复进行中：恢复期间重建的选项卡不触发持久化（原 `session_restoring`）。
    pub session_restoring: bool,
    /// 会话槽位「上次落盘内容」指纹：slot 名 → 内容 hash（原 `session_slot_hashes`）。
    pub session_slot_hashes: HashMap<String, u64>,
    /// 上次成功写盘时的清单指纹（清单序列化后 hash；原 `session_last_manifest_digest`）。
    pub session_last_manifest_digest: Option<u64>,
    /// 已推送给 PDF 阅读器的查询 `(活动标签, 查询词)`（原 `EditorApp::pdf_search`）。
    pub pdf_search: Option<(usize, String)>,
    /// 侧边栏搜索的惰性扫描缓存：按（查询词, 活动文档）复用（原 `EditorApp::search_scan`）。
    pub search_scan: Option<Rc<shell_scan::SidebarSearchScan>>,
    /// 侧边栏搜索**范围**（当前文件 / 整个文件夹；原 `EditorApp::search_scope`）。
    pub search_scope: SearchScopeTab,
    /// 用户是否**手动**切换过搜索范围（原 `EditorApp::search_scope_manual`）。
    pub search_scope_manual: bool,
    /// 文件夹级内容搜索的扫描结果（原 `EditorApp::folder_search`）。
    pub folder_search: Option<Rc<shell_scan::FolderSearchScan>>,
    /// 文件夹扫描**代**：每次发起新扫描 +1（原 `EditorApp::folder_search_gen`）。
    pub folder_search_gen: u64,
    /// quick open 后台扫描**代**（原 `EditorApp::quick_open_gen`）。
    pub quick_open_gen: u64,
    /// 最近一条 GoToLine 提示通知的 key（原 `EditorApp::goto_line_toast_key`）。
    pub goto_line_toast_key: Option<ElementId>,
    /// 最近一条 Find「当前选项卡不支持查找」提示通知的 key（原 `find_toast_key`）。
    pub find_toast_key: Option<ElementId>,
    /// 最近一条「导出/导入设置」结果提示通知的 key（原 `settings_toast_key`）。
    pub settings_toast_key: Option<ElementId>,
    /// 实际执行过的搜索扫描次数（原 `EditorApp::search_scan_count`；测试用）。
    pub search_scan_count: usize,
    /// 侧边栏搜索「防抖后提交生效」的查询词（已归一化；原 `search_committed`）。
    pub search_committed: String,
    /// 防抖窗口内最新一次输入（未提交的归一化查询词；原 `search_pending`）。
    pub search_pending: String,
    /// 搜索防抖（原 `EditorApp::search_debounce`）：可重入、代序作废。
    pub search_debounce: SeqAfter,
    /// 正在拖拽的选项卡下标（原 `EditorApp::tab_dragging`；None = 未在拖拽）。
    pub tab_dragging: Option<usize>,
    /// 本次拖拽开始时 gpui 拖拽 picture 的「抓取偏移」（原 `EditorApp::tab_drag_grab`）。
    pub tab_drag_grab: Option<Point<Pixels>>,
    /// 拖拽中的选项卡光标已越出窗口边缘（尚未松手；原 `drag_outside_armed`）。
    pub drag_outside_armed: bool,
    /// 本次 `on_drag_move` 算出的「幽灵窗应显示」决策（原 `drag_ghost_visible`）。
    pub drag_ghost_visible: bool,
    /// 拖拽选项卡排序：本次拖拽过程光标下的「插入下标」（原 `tab_reorder_target`）。
    pub tab_reorder_target: Option<usize>,
    /// PDF 阅读画布背景预设（config `pdf_bg`；原 `EditorApp::pdf_bg`）。
    pub pdf_bg: PdfBgPref,
    /// PDF 自定义画布底色（config `pdf_bg_custom`；原 `pdf_bg_custom`）。
    pub pdf_bg_custom: Option<String>,
    /// 最近一次对 PDF 页操作失败已提示过的错误文案（原 `pdf_last_error_seen`，toast 去重）。
    pub pdf_last_error_seen: Option<String>,
    /// PDF 阅读位置书签（规范化路径 → 列表；原 `EditorApp::pdf_bookmarks`）。
    pub pdf_bookmarks: HashMap<String, Vec<PdfBookmark>>,
    /// 文件树过滤词（防抖后提交、已归一化小写；原 `EditorApp::files_filter`）。
    pub files_filter: String,
    /// 防抖窗口内最新一次过滤输入（未提交；原 `files_filter_pending`）。
    pub files_filter_pending: String,
    /// 文件树过滤防抖（原 `EditorApp::files_filter_debounce`）。
    pub files_filter_debounce: SeqAfter,
    /// 最近打开的文件夹路径（新→旧；原 `EditorApp::recent_folders`）。
    pub recent_folders: Vec<String>,
    /// 最近打开的文件路径（新→旧；原 `EditorApp::recent_files`）。
    pub recent_files: Vec<String>,
    /// A：旧 live2 实体延迟回收队列（原 `EditorApp::retired_live2`；`commit_live` 暂存、
    /// 下一帧 `on_next_frame` 清空，把级联 Drop 移出点击同步路径）。
    pub retired_live2: Vec<gpui::Entity<WyEditor>>,
    /// 是否正处于音乐模式（整窗渲染 MusicView；原 `EditorApp::music_mode`）。
    pub music_mode: bool,
}

impl AppModel {
    /// 构造带初值的模型（原 `EditorApp::new_impl` 的逐初值语义迁移）：零选项卡起手，
    /// 前 16 个参数为 `new_impl` 从 `UserPrefs` / 窗口外观解析出的启动初值，其余派生
    /// 状态取全零/空默认。
    ///
    /// 命名与旧 `EditorApp::new_impl` 的 struct 字段一致，语义零变化。
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        status: SharedString,
        status_ok: bool,
        font_size: f32,
        dark_mode: bool,
        theme_pref: ThemePref,
        pdf_bg: PdfBgPref,
        pdf_bg_custom: Option<String>,
        session_restore: bool,
        close_to_tray: bool,
        media_new_window: bool,
        reuse_media_window: bool,
        music_window_size: Option<Size<Pixels>>,
        video_window_size: Option<Size<Pixels>>,
        recent_folders: Vec<String>,
        recent_files: Vec<String>,
        pdf_bookmarks: HashMap<String, Vec<PdfBookmark>>,
    ) -> Self {
        Self {
            tabs: Vec::new(),
            active_tab: 0,
            status,
            status_ok,
            font_size,
            dark_mode,
            theme_pref,
            // 编辑区软换行默认关闭；仅 Markdown 文件的编辑区默认换行（load_file_text 显式开启）。
            wrap_enabled: false,
            session_restore,
            close_to_tray,
            media_new_window,
            reuse_media_window,
            music_window_size,
            video_window_size,
            media_size_flush_debounce: SeqAfter::default(),
            session_flush_debounce: SeqAfter::default(),
            session_restoring: false,
            session_slot_hashes: HashMap::new(),
            session_last_manifest_digest: None,
            pdf_search: None,
            search_scan: None,
            search_scope: SearchScopeTab::CurrentFile,
            search_scope_manual: false,
            folder_search: None,
            folder_search_gen: 0,
            quick_open_gen: 0,
            goto_line_toast_key: None,
            find_toast_key: None,
            settings_toast_key: None,
            search_scan_count: 0,
            search_committed: String::new(),
            search_pending: String::new(),
            search_debounce: SeqAfter::default(),
            tab_dragging: None,
            tab_drag_grab: None,
            drag_outside_armed: false,
            drag_ghost_visible: false,
            tab_reorder_target: None,
            pdf_bg,
            pdf_bg_custom,
            pdf_last_error_seen: None,
            pdf_bookmarks,
            files_filter: String::new(),
            files_filter_pending: String::new(),
            files_filter_debounce: SeqAfter::default(),
            recent_folders,
            recent_files,
            retired_live2: Vec::new(),
            music_mode: false,
        }
    }
}

impl Default for AppModel {
    fn default() -> Self {
        Self {
            tabs: Vec::new(),
            active_tab: 0,
            status: SharedString::default(),
            status_ok: false,
            font_size: 0.0,
            dark_mode: false,
            theme_pref: ThemePref::Follow,
            wrap_enabled: false,
            session_restore: true,
            close_to_tray: false,
            media_new_window: true,
            reuse_media_window: true,
            music_window_size: None,
            video_window_size: None,
            media_size_flush_debounce: SeqAfter::default(),
            session_flush_debounce: SeqAfter::default(),
            session_restoring: false,
            session_slot_hashes: HashMap::new(),
            session_last_manifest_digest: None,
            pdf_search: None,
            search_scan: None,
            search_scope: SearchScopeTab::CurrentFile,
            search_scope_manual: false,
            folder_search: None,
            folder_search_gen: 0,
            quick_open_gen: 0,
            goto_line_toast_key: None,
            find_toast_key: None,
            settings_toast_key: None,
            search_scan_count: 0,
            search_committed: String::new(),
            search_pending: String::new(),
            search_debounce: SeqAfter::default(),
            tab_dragging: None,
            tab_drag_grab: None,
            drag_outside_armed: false,
            drag_ghost_visible: false,
            tab_reorder_target: None,
            pdf_bg: PdfBgPref::default(),
            pdf_bg_custom: None,
            pdf_last_error_seen: None,
            pdf_bookmarks: HashMap::new(),
            files_filter: String::new(),
            files_filter_pending: String::new(),
            files_filter_debounce: SeqAfter::default(),
            recent_folders: Vec::new(),
            recent_files: Vec::new(),
            retired_live2: Vec::new(),
            music_mode: false,
        }
    }
}

/// PDF「阅读位置书签」一条：目标页码（0 基）+ 展示标题（SC-3 随
/// `EditorApp::pdf_bookmarks` 字段迁入；原 `app::pdf_bookmarks::PdfBookmark`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PdfBookmark {
    /// 目标页码（0 基；`PdfViewer::scroll_to_page` 的直接入参）。
    pub page_index: usize,
    /// 展示标题（优先当前大纲标题；无则「第 N 页」，N 为 1 基页码）。
    pub title: String,
}

/// 侧边栏搜索**范围**（SC-3 自 `app::sidebar::SearchScopeTab` 迁入）：只搜当前文件 /
/// 递归搜已打开文件夹内文本文件。沿 `app` / `app::sidebar` 重导出维持既有路径。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum SearchScopeTab {
    /// 当前文档内容（+ 跨打开文件文件名匹配），旧语义。
    #[default]
    CurrentFile,
    /// 整个打开的文件夹（未打开文件夹时不可用）。
    Folder,
}

/// 一个文件选项卡：独立持有自研编辑器组件状态、文件路径与脏标记。
pub struct EditorTab {
    /// 本选项卡的编辑区（[`editor::CodeEditor`]）。
    pub editor: gpui::Entity<editor::CodeEditor>,
    /// 当前文件路径（None = 未命名）
    pub file_path: Option<PathBuf>,
    /// 本选项卡的**源文本编码**（[`text_open::encoding`]）：`None` = 纯 UTF-8（新建/
    /// 打开/媒体）；`Some(&'static Encoding)` = 打开时探测出的非 UTF-8 编码（含带 BOM
    /// 的 UTF-8/UTF-16）。保存（`write_to_path`）按此编码把缓冲 UTF-8 转回字节写盘。
    /// 随选项卡搬迁（EditorTab ⇄ DetachedTab 整包搬移）、文档编码随文档走。
    pub source_encoding: Option<&'static encoding_rs::Encoding>,
    /// 内容自上次打开/保存后是否被修改
    pub dirty: bool,
    /// 当前内容的镜像缓存（在 `InputEvent::Change` 时同步，避免每帧克隆 Rope）
    pub content: SharedString,
    /// 行数缓存
    pub line_count: usize,
    /// 字数缓存
    pub char_count: usize,
    /// 光标行（0 基）
    pub cursor_row: usize,
    /// 光标列（0 基）
    pub cursor_col: usize,
    /// 媒体阅读器（打开 .pdf / 图片 / 视频时创建，一次至多其一）：整页/整图/首帧只读
    /// 展示；`None` = 非媒体选项卡（文本 / Markdown / EPUB）。
    pub reader: MediaReader,
    /// 是否开启 Markdown 预览
    pub preview_on: bool,
    /// 预览是否占满整个内容区（EPUB 阅读等场景，不显示编辑区/分割线）
    pub preview_fullscreen: bool,
    /// 预览是否只读锁定（EPUB：正文只在预览区，编辑区为空，不能关闭/翻转预览）
    pub readonly_preview: bool,
    /// 大文档异步打开：预览先出、编辑器暂缓挂载（首帧显示轻量占位，
    /// 等用户第一时间看到预览内容后，下一拍再把编辑器挂上——编辑器
    /// 首次对 16 万行做 DisplayMap 布局会冻结一两秒，不让它挡在首帧前）。
    pub editor_pending: bool,
    /// 大文档 P1 后台装配在途（整篇 rope 尚在生产端）：编辑区已显示首块但**锁输入**
    /// （渲染层全区域透明拦截，见 panes.rs）——避免「打开期间编辑」与 EOF 整篇换装
    /// 竞态；`finish_file_text` 换装成功后置 false。
    pub loading_lock: bool,
    /// 是否处于「所见即所得预览编辑」模式：左面板用 `WyEditor` 代替源码编辑区；
    /// 退出时内容写回。
    pub live_edit: bool,
    /// 「预览编辑测试」：本项目 `markdown_wysiwyg::WyEditor`（以 `Doc` 为数据源）。
    /// None = 未启用。启用即 `live_edit = true`。
    pub live2: Option<gpui::Entity<WyEditor>>,
    /// 「预览编辑测试」本 live 会话里 WYSIWYG 内容是否被**编辑过**（`WyEvent::Changed`
    /// 订阅中对 live2 实体置位；进入 ON 与新建选项卡时清 false）。退出 `commit_live`
    /// 用它判断是否需要整篇写回源码编辑区：未编辑时 live2 文本恒等于打开瞬间的源码
    /// COW 快照（O(1) 同源），写回纯属 O(n) 浪费（大文档级联 `set_text` 重排卡 7-8s）
    /// → 直接销毁实体切回，不物化全文。
    pub live2_dirty: bool,
    /// A2：右栏 Markdown 预览统一组件（`WyEditor`，A1 只读门控）。MD 右栏/全屏预览
    /// 与 EPUB 只读宿主都走它（同一 `Doc` 管线的只读视图）；`None` = 未创建（文本 /
    /// PDF / 图片选项卡，或预览已关闭）。
    pub md_wy: Option<gpui::Entity<WyEditor>>,
    /// P4-1：EPUB 的**内存图片表**（虚拟 `epubimg://…` → 字节，不落盘）。打开时存
    /// 一份供 WyEditor 宿主（`ensure_epub_wy`）注入；随选项卡搬迁（与 `md_wy` 同）。
    pub epub_images: std::collections::HashMap<String, std::sync::Arc<gpui::Image>>,
    /// P4-2：EPUB 的**注记正文表**（`注记号 → 注释文本`）。打开时存一份供 WyEditor
    /// 宿主（`ensure_epub_wy` → `set_notes`）注入——悬浮卡正文；随选项卡搬迁。
    pub epub_notes: std::collections::BTreeMap<usize, String>,
    /// `md_wy` 右栏/全屏宿主的纵向滚动句柄：`#md-wy-scroll`（`overflow_y_scroll`）
    /// 经 `track_scroll` 跟踪它，右侧自绘 `Scrollbar::vertical` 跟随（与 `live_vscroll`
    /// 同型：每次渲染重建容器、句柄跨帧稳定）。随选项卡搬迁（与 `md_preview` 同）。
    pub md_wy_vscroll: gpui::ScrollHandle,
    /// 右栏 `md_wy`「在途装配」三代叠加态状态包（B3 后台整篇装配在途 / 装配代号 /
    /// Step2 提前整篇解析在途 / Step3「整篇在途·prefix 已装」收敛为单字段，见
    /// [`MdWyInstall`]）。随选项卡搬迁（EditorTab ⇄ DetachedTab）作为一个字段整包搬移，
    /// 不再逐带双份复制。
    pub md_wy_install: MdWyInstall,
    /// 编辑版本号：每次内容变更 +1。编辑区⇄预览滚动同步用它判断「画面/内容是否有新
    /// 编辑需重建同步基线」。
    pub edit_version: u64,
    /// 监听编辑事件 → 置脏 + 刷新状态栏（保持有效）
    pub _edit_sub: Subscription,
    /// 监听 PDF 阅读器 `notify()`（检索结果/加载进度到达）→ 刷新 app（侧边栏计数）。
    /// **随选项卡持有**：多 PDF 选项卡并存时各自独立订阅，避免旧单槽全局
    /// `EditorApp::_pdf_observe` 被后打开者覆盖、先打开 PDF 的 notify 丢失（真 bug）。
    /// 订阅绑定本窗口 EditorApp；随选项卡搬迁时随源 `EditorTab` 一起丢弃
    /// （`take_tab_payload` 解构 `..`），目标窗口 `adopt_tab` 重建。关选项卡 / 拔除时
    /// 随 `EditorTab` drop 自动退订（无幽灵回调 / 泄漏）。
    pub _pdf_sub: Option<gpui::Subscription>,
    /// `live2`（WyEditor）的变更订阅。
    pub _live2_sub: Option<Subscription>,
    /// 预览编辑（WYSIWYG）宿主纵向滚动句柄：`#live-scroll`（`overflow_y_scroll`）
    /// 经 `track_scroll` 跟踪它，右侧 `Scrollbar::vertical` 跟随显示（宿主原生滚动条
    /// 在此 GPUI 不绘制，需自绘）。每次渲染重建容器，句柄存这里跨帧稳定。
    pub live_vscroll: gpui::ScrollHandle,
}

/// 一个从窗口中「拔出」的选项卡快照：编辑器 / 预览等实体句柄共享（GPUI 实体
/// 与窗口无关），连同文件元数据一起跨窗口搬迁。`_edit_sub` 是源窗口持有的订阅，
/// 拔掉时随源 `EditorTab` 一并丢弃，目标窗口重新建立订阅。
pub struct DetachedTab {
    pub editor: gpui::Entity<editor::CodeEditor>,
    /// 媒体阅读器（PDF / 图片 / 视频；与 [`EditorTab::reader`] 同型，跨窗口搬迁）。
    pub reader: MediaReader,
    /// A2：右栏 WyEditor 预览实体与宿主滚动句柄随选项卡搬迁（与 `md_preview` 同；
    /// 实体与窗口无关，可直接跨窗口移动）。
    pub md_wy: Option<gpui::Entity<WyEditor>>,
    pub md_wy_vscroll: gpui::ScrollHandle,
    /// 随选项卡搬迁（整包承接在途装配状态包，防搬迁后旧完成回调被误丢 / EOF 让位判断
    /// 错位 / sync 增量错打 prefix）。
    pub md_wy_install: MdWyInstall,
    /// P4-1：EPUB 内存图片表随选项卡搬迁（与 `md_wy` 宿主共用）。
    pub epub_images: std::collections::HashMap<String, std::sync::Arc<gpui::Image>>,
    /// P4-2：EPUB 注记正文表随选项卡搬迁（`ensure_epub_wy` → `set_notes` 用）。
    pub epub_notes: std::collections::BTreeMap<usize, String>,
    pub file_path: Option<PathBuf>,
    pub dirty: bool,
    /// 源文本编码（见 [`EditorTab::source_encoding`]；随选项卡搬迁）。
    pub source_encoding: Option<&'static encoding_rs::Encoding>,
    pub content: SharedString,
    pub line_count: usize,
    pub char_count: usize,
    pub cursor_row: usize,
    pub cursor_col: usize,
    pub preview_on: bool,
    pub preview_fullscreen: bool,
    pub readonly_preview: bool,
    pub editor_pending: bool,
    /// 大文档 P1 后台装配在途（整篇 rope 尚在生产端）：编辑区已显示首块但**锁输入**
    /// （渲染层全区域透明拦截，见 panes.rs）——避免「打开期间编辑」与 EOF 整篇换装
    /// 竞态；`finish_file_text` 换装成功后置 false。
    pub loading_lock: bool,
    pub edit_version: u64,
}

/// Step2/3：大文档 md「打开即提前后台整篇解析」（与分块读盘并行、不等 EOF）在途状态。
/// 首块挂载后立即启动（`file_ops::apply_open_chunk` → `md_wy_spawn_early_install`）；
/// **Step3 两阶段**：先由 `md_wy_spawn_prefix_install` 装首块 prefix（首屏真内容、置
/// `md_wy_pending_full`），整篇就绪后本路径**带守卫换入全文**（滚动锚按 src_line 钉顶行），
/// 置 [`Self::Done`]。EOF 收尾（`finish_file_text`）据此**跳过重复 install**
/// （[`Self::Pending`]/[`Self::Done`] 时让位早期装配，不再重复
/// 整篇 `Doc::open`），[`Self::Failed`]/`None` 才回退走既有 EOF 整篇装配（兜底）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MdWyEarly {
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
pub struct MdWyInstall {
    /// 后台整篇装配在途（原 `md_wy_pending`）：`Some((装配代号, 目标内容))` = 已登记
    /// 后台 `open_from_str` 解析、尚未 `install_open`；`None` = 无在途。异步完成回调按
    /// 代号与 `seq` 匹配才装配（不装错、不泄漏）。
    pub pending: Option<(usize, SharedString)>,
    /// 装配代号单调计数器（原 `md_wy_install_seq`）：每次发起后台整篇装配 / 任何同步
    /// 改源（增量重放、同步喂源、换源 install_open、预览关闭）递增，结束在途旧装配
    /// （代号不匹配即丢弃其完成回调，防陈旧整篇覆盖刚改好的内容）。
    pub seq: usize,
    /// Step2 提前整篇解析在途状态（原 `md_wy_early`，见 [`MdWyEarly`]）。
    pub early: Option<MdWyEarly>,
    /// Step3「整篇在途」标记（原 `md_wy_pending_full`）：`Some((装配代号, prefix 字节
    /// 长))` = 预览只装了**首块 prefix**（已 `is_loaded`）、整篇后台解析在途——此期间
    /// 预览 ≠ 编辑区全文，`sync` 增量桥**禁止**把编辑区坐标增量打到 prefix（会错位），
    /// 改走「作废在途整篇 + 异步整篇重喂」；整篇就绪带守卫换入全文后置 `None`。
    pub pending_full: Option<(usize, usize)>,
}

/// 选项卡拖拽携带的数据：目标选项卡下标 + 当前显示名 + 真实 laid-out 尺寸。
/// `size` 每帧由 [`crate::app::tabs::TabBoundsRecorder`] 写入（包在选项卡元素外，把
/// 其 border-box 尺寸抄进来），拖拽预览 / 幽灵窗按它定宽定高——彻底避免「文本估算
/// 偏窄 → 窗内变小 / 窗外内容溢出被裁」。
#[derive(Clone, Debug)]
pub struct TabDragData {
    pub idx: usize,
    pub name: SharedString,
    pub size: Rc<Cell<Size<Pixels>>>,
}

/// 拖拽选项卡时跟随鼠标的「文件选项卡」半透明预览视图：窗内 gpui 拖拽 picture 与窗外
/// 幽灵窗共用同一根视图（视觉一致）——仿真实选项卡（真实 laid-out 尺寸、圆角、边框、
/// 标题 + 脏点 + 视觉关闭钮），整体半透明表达「正在拖动」。
/// 尺寸用真实 tab 的 `size`（每帧由 `tabs::TabBoundsRecorder` 写入）**显式定宽高**：
/// gpui 拖拽 picture 以 `AvailableSpace::min_size()` 布局（window.rs:3737），无固定宽
/// 会被 min-content 压窄（逆"窗内变小"）；幽灵窗口也按同一尺寸创建（逆"窗外溢出被裁"）。
pub struct TabDragGhost {
    pub name: SharedString,
    pub dirty: bool,
    pub size: Rc<Cell<Size<Pixels>>>,
    /// true = 作为窗外幽灵窗根视图（不画 1px 边框：悬于桌面时该描边会被单独呈现为
    /// 「四根线条围住选项卡」）；false = 作为窗内拖拽 picture（贴近真实激活选项卡）。
    pub ghost: bool,
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

/// 窗口控制钮（最小化/最大化/关闭）上一帧解析出的图标前景色。
/// 渲染阶段 `title_ctrl_button` 落盘，供测试断言「注入色」经真实渲染路径生效。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TitleCtrlIconFg {
    pub minimize: gpui::Hsla,
    pub maximize: gpui::Hsla,
    pub close: gpui::Hsla,
}

/// 标题栏窗口控制钮类型：平台统一按 `window_control_area` 命中区走原生语义
/// （WM_NCHITTEST → HTMINBUTTON/HTMAXBUTTON/HTCLOSE），这里只按类型分支
/// 图标 / 底色 / 注入色。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum WinCtrlKind {
    Minimize,
    Maximize,
    Close,
}

impl WinCtrlKind {
    pub fn id(self) -> &'static str {
        match self {
            WinCtrlKind::Minimize => "title-minimize",
            WinCtrlKind::Maximize => "title-maximize",
            WinCtrlKind::Close => "title-close",
        }
    }

    pub fn area(self) -> gpui::WindowControlArea {
        match self {
            WinCtrlKind::Minimize => gpui::WindowControlArea::Min,
            WinCtrlKind::Maximize => gpui::WindowControlArea::Max,
            WinCtrlKind::Close => gpui::WindowControlArea::Close,
        }
    }

    pub fn is_close(self) -> bool {
        matches!(self, WinCtrlKind::Close)
    }

    /// hover 背景淡入的填充色：关闭钮红（危险钮观感），其余灰（P2-10 统一弹簧）。
    pub fn hover_bg(self, c: ChromeColors) -> gpui::Hsla {
        if self.is_close() {
            c.close_hover.into()
        } else {
            c.hover_fill.into()
        }
    }

    /// 按压底色：关闭钮用 P1-7 `close_pressed` 深一档危险红，其余用通用按压灰。
    pub fn active_bg(self, c: ChromeColors) -> gpui::Hsla {
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
pub enum MediaReader {
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
    pub fn is_none(&self) -> bool {
        matches!(self, MediaReader::None)
    }

    /// 是否挂有「自带阅读器」的媒体（PDF / 图片 / 视频）——预览绝不以 WyEditor 渲染。
    pub fn is_reader(&self) -> bool {
        !self.is_none()
    }

    /// 是否有「锁定」语义的媒体（图片 / 视频；PDF 无右键锁定菜单）。
    pub fn is_lockable(&self) -> bool {
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
