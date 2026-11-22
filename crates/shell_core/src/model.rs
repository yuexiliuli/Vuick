//! 外壳宿主模型类型。
//!
//! `EditorApp` 的模型类型独立成 `shell_core` crate，宿主编排（`impl
//! EditorApp`）留在 shell。字段可见性按「宿主 / 测试跨 crate 要读写者 → `pub`」调整；
//! 类型自包含的 inherent impl（[`MediaReader`] / [`WinCtrlKind`] / [`Render` for
//! [`TabDragGhost`]]）随类型一并置于本 crate。
//!
//! 依赖的宿主类型（[`crate::app::EditorApp`] / `EditorAppRegistry`）不在此 crate；滚动
//! 同步 / 面板分割 / 活区内衬等纯值类型（[`MdScrollSync`] / [`MdTocScroll`] / [`PanesState`]
//! / [`LIVE_BODY_PAD_Y`]）下沉至此，宿主以重导出维持既有路径。

use std::cell::Cell;
use std::collections::HashMap;
use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;

use gpui::{
    div, px, App, Context, ElementId, InteractiveElement, IntoElement, ParentElement, Pixels,
    Point, Render, SharedString, Size, Styled, Subscription, Window,
};
use markdown_wysiwyg::WyEditor;
use ui::theme::ChromeColors;

use base::util::SeqAfter;
use config::user::{PdfBgPref, ThemePref};
use session_store::ReadingProgress;

/// WYSIWYG / MD 统一预览宿主 `#live-body` 的顶部内衬（px）：缩放锚点在做「宿主
/// ScrollHandle 偏移 ↔ WyEditor 元素局部 y」换算时使用，必须与各宿主容器
/// `.py(px(LIVE_BODY_PAD_Y))` 保持一致。
pub const LIVE_BODY_PAD_Y: f32 = 8.0;

/// md 阅读进度恢复窗口的渲染帧计数上限（≈1.5s@60fps）：恢复后预览未能在该帧数内收敛
/// 到存档块即清位归还正常双向同步——防「预览装配异常 / 目标块不可达」导致
/// [`EditorTab::md_restore_pending`] 永久锁死「预览→编辑区」驱动。取 `WyEditor` 的
/// `SCROLL_TARGET_MAX_APPLIES` 同量级、并覆盖大文档两段式装配（~200ms≈12 帧）+ 收敛。
pub const MD_RESTORE_WAIT_FRAMES: u32 = 90;

/// 标题目录的跳转目标（侧边栏 `toc_item_frame` 主点击收起/滚动两用）：右栏统一组件
/// `WyEditor`（只读预览，条目为源码行，`scroll_to_line`）。纯值类型，壳侧以重导出
/// 维持既有路径。
#[derive(Clone)]
pub enum MdTocScroll {
    Unified(gpui::Entity<WyEditor>),
}

/// 编辑区⇄预览滚动同步基线（见 shell 侧 `preview::sync_editor_preview_scroll`）。记录
/// 当前选项卡 + 编辑器内容版本 + 上次已同步的双方「块起点」键，渲染帧比对后由动了
/// 的那一侧驱动对端。切换选项卡 / 同一选项卡内换文档 / 编辑使内容版本变化 → 重建。
/// 纯值类型，壳侧以重导出维持既有路径。字段跨 crate 读写
/// （宿主 sync 每帧改 / 测试断言语义）→ 按本 crate 模型类型约定全部 `pub`。
#[derive(Clone, Copy)]
pub struct MdScrollSync {
    pub tab: usize,
    pub editor_version: Option<u64>,
    pub editor_block: Option<usize>,
    pub preview_block: Option<usize>,
    /// 我方（编辑器主导时）最近一次要求预览到达的目标块：预览重钉落位到该块是「按
    /// 我方命令跟上」，不得反被当成用户滚动预览而把编辑器拽回（来回跳动）。
    pub driven_preview_target: Option<usize>,
    /// 我方（预览主导时）最近一次要求编辑器到达的目标块：编辑器落位到该块是「按
    /// 我方命令跟上」，与 `driven_preview_target` 对称——否则编辑器跟随落位会被
    /// 误判成「编辑器先动 → 驱动预览」的反馈回声，把预览拽回（来回跳动/滚不下去）。
    pub driven_editor_target: Option<usize>,
    /// `driven_editor_target` 的收敛宽限帧数：编辑器落位通常同步、但边界块（表格等）
    /// 顶行读取可能滞后/抖动一帧到数帧，期间回显必须被抑制；超过宽限才放手让正常
    /// 双向同步接管（防编辑器永远到不了目标时同步停摆）。
    pub driven_editor_await: u32,
    /// 右栏预览「Scrub 延后」在途（I2）：预览被快速拖动 / 高频滚屏**连续跨块**时置
    /// 真，期内的编辑区 `scroll_to_line` 跟随**延后**（不逐帧嵌编辑器全窗重建）；
    /// 预览静止（松手 / 停滚）后一次收敛到最终目标，恢复双侧对齐。
    pub scrub: bool,
    /// 进入 Scrub 的连续「预览跨块」帧计数：由同步函数逐帧维护（`preview_moved`
    /// 连续即累加、中断即清零），达到 shell 侧 `preview::SCRUB_ENTER_STREAK` 才进入
    /// 延后——低频单步滚屏不触发，保持慢滚逐帧对齐。
    pub scrub_streak: u32,
    /// Scrub 延后期内「预览静止」的连续帧计数：达到 shell 侧 `preview::SCRUB_IDLE_FRAMES`
    /// （或墙钟兜底）即一次收敛。
    pub scrub_idle_frames: u32,
    /// Scrub 延后的收敛目标 = 延后期内最新一次预览顶块（松手/静止后编辑器一次
    /// 收敛到它）。
    pub scrub_target: Option<usize>,
    /// 距上次预览跨块的墙钟（静止收敛的墙钟兜底，见 shell 侧 `preview::SCRUB_IDLE`；
    /// 帧稀疏时保证仍会收敛）。
    pub scrub_last_cross: std::time::Instant,
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

/// 「编辑区 ⇄ 预览分割条」面板状态包（`EditorApp` 四个 split 字段收敛的纯值类型）。
/// 宿主 `EditorApp` 以**普通字段** `panes: PanesState` 持有（非 `Rc<RefCell>`：
/// 渲染 / 拖拽事件路径多处直读）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PanesState {
    /// 编辑区 / 预览区分屏比例（0..1，= 编辑区占宽比例）。
    /// `#[doc(hidden)] pub`：`tests/splitter.rs` 拖拽断言读（§8.6 外提清单）。
    #[doc(hidden)]
    pub split_ratio: f32,
    /// 正在拖拽分屏分割线：`Some((起始指针x, 起始比例))`。
    /// `#[doc(hidden)] pub`：`tests/splitter.rs` 拖拽开始/结束断言（§8.6 外提清单）。
    #[doc(hidden)]
    pub split_drag: Option<(f32, f32)>,
    /// 拖分割条帧级合并（B4）：`on_mouse_move` 只记**最新目标比例**，本帧 `render`
    /// 开头统一落地到 `split_ratio`（同 scrollbar `pending_target`→prepaint 的帧末统一
    /// 落地节奏）。语义不变：分割条拖动仍实时跟手（每帧一落），只合并 notify 频率。
    /// 须保持 `pub`（`PanesState` 在 shell_core、唯一消费方 layout.rs 在 shell，跨 crate
    /// 读写），doc-hidden 收敛对外文档面。
    #[doc(hidden)]
    pub pending_split_ratio: Option<f32>,
    /// 分割线是否被鼠标悬停（悬停时高亮提示可拖拽）。
    /// `#[doc(hidden)] pub`：`tests/splitter.rs` 悬停高亮断言（§8.6 外提清单）。
    #[doc(hidden)]
    pub splitter_hovered: bool,
}

impl Default for PanesState {
    fn default() -> Self {
        Self {
            split_ratio: 0.5,
            split_drag: None,
            pending_split_ratio: None,
            splitter_hovered: false,
        }
    }
}

/// 测试探针（`AppModel::commit_live` 退出路径计数）：整篇写回（有改动）vs 未改动直接
/// 销毁实体（O(1) 跳过写回）。shell 侧回归测试据此断言「未编辑不写回」。
/// 常驻编译（无 `#[cfg(test)]`）：`commit_live` 逻辑在 shell_core，而 shell 的测试构建
/// 把 shell_core 当普通依赖编译（`cfg(test)` 恒关）——探针须常驻才可被 shell 测试读取；
/// 生产进程对从未被读的原子做 fetch_add 成本可忽略，无行为差异。
#[doc(hidden)]
pub static LIVE_COMMIT_WRITEBACK: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
/// 测试探针：`commit_live`「未改动 O(1) 跳过写回」路径计数（见 [`LIVE_COMMIT_WRITEBACK`]）。
#[doc(hidden)]
pub static LIVE_COMMIT_SKIP_UNCHANGED: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
/// 测试探针：走**编辑日志重放**（方案2，`apply_live_replay` 主路径）写回的 `commit_live`
/// 次数（op 非空且护栏自检通过）。
#[doc(hidden)]
pub static LIVE_COMMIT_OPS_REPLAYED: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
/// 测试探针：编辑日志重放实际应用的 op 累计数（= 该 live 会话缴存的内容变更条数；
/// 引用计数断言「单字符恰 1 条」「大粘贴单大 op」）。
#[doc(hidden)]
pub static LIVE_COMMIT_OPS_COUNT: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// `AppModel::commit_live` 的宿主收尾指令：模型/实体侧写回完成后，把**必须留在视图层**
/// 的收尾（`on_next_frame` 清 `retired_live2`、右栏 `md_wy` 对齐 / 行级 diff 兜底）交给
/// 宿主 shell 侧执行，逐路径与旧 `EditorApp::commit_live` 行为一致。
pub enum CommitLiveAlign {
    /// 该选项卡无 live2（下标越界 / 未启用）：未暂存任何实体，宿主无需收尾。
    NoLive2,
    /// 已把 live2 暂存 `retired_live2`（宿主须调度 `on_next_frame` 清空队列），
    /// 无需右栏 `md_wy` 对齐（未改动 / 加载中退出）。
    NoAlign,
    /// 已暂存 + 编辑日志重放成功：宿主对右栏 `md_wy` 做逐 op 增量对齐
    /// （`md_wy_align_after_ops`）。
    Ops {
        ops: Vec<(Range<usize>, SharedString)>,
        src_len: usize,
        content: SharedString,
    },
    /// 已暂存 + 走到行级 diff 兜底：宿主先 `text_open::line_diff` 产 hunk 并应用到源码
    /// 编辑区，再对右栏 `md_wy` 做 diff 对齐（`md_wy_align_after_diff`）。行级 diff 计算
    /// 依赖 `text_open`（shell 既有依赖），留在宿主侧，本 crate 不新增依赖。
    Diff {
        base: String,
        content: SharedString,
    },
}

/// 外壳宿主「模型半」状态的统一容器。
///
/// 只装**跨窗口搬迁 / 会话持久化 / 设置与搜索态 / 测试直接断言的模型态**（tabs /
/// active_tab / 侧边栏搜索态 / 最近文件 / 会话编排 / 偏好设置等）；UI 半状态（窗口句柄 /
/// 布局 / 聚焦 / 主题 glue / registry / 输入框实体 / 菜单实体）留在宿主 `EditorApp`。
/// 宿主经 `Rc<RefCell<AppModel>>` 持有，借 read/write 分 `borrow()/.borrow_mut()`。
///
/// ⚠️ 借用约束：RefCell 是**单元格级**互斥，同一方法内持有 `borrow_mut()`
/// 守卫时不可再 `borrow()`/`borrow_mut()`（运行期双借 panic）；跨字段读写在守卫内一次
/// 完成，方法调用前先 `drop` 守卫。
pub struct AppModel {
    /// 所有文件选项卡。
    pub tabs: Vec<EditorTab>,
    /// 当前激活的选项卡下标。
    pub active_tab: usize,
    /// 顶部状态文字（状态栏）。
    pub status: SharedString,
    /// 顶部状态文字是否"正常"。
    pub status_ok: bool,
    /// 编辑器正文字号（px，用户可调）。
    pub font_size: f32,
    /// 是否深色（黑）模式：控制组件主题、自绘色板与原生标题栏深浅。
    pub dark_mode: bool,
    /// 主题偏好三态：Follow=跟随系统 / Light / Dark。
    pub theme_pref: ThemePref,
    /// 编辑区是否自动换行（默认换行）。
    pub wrap_enabled: bool,
    /// 「恢复上次会话」开关（文件 → 菜单切换，随 [`UserPrefs`] 持久化）。
    pub session_restore: bool,
    /// 「记住阅读进度」开关（文件 → 菜单切换，随 [`UserPrefs`] 持久化）：开 = 重开
    /// 文件回到上次阅读位置（md/txt 行、EPUB 行、PDF 页+比例+缩放），关 = 重开回顶部。
    pub restore_reading_progress: bool,
    /// 阅读进度存储（规范化路径 → 进度）。启动经 `load_cached` 注入；各保存点
    /// （关 tab / 滚动防抖）更新并落盘；重开文件时恢复读取。
    pub reading_progress: HashMap<String, ReadingProgress>,
    /// 「关闭到系统托盘」开关（文件 → 菜单切换，持久化）。
    pub close_to_tray: bool,
    /// 「媒体在新窗口打开」开关（查看 → 菜单切换，持久化）。
    pub media_new_window: bool,
    /// 「复用已有媒体窗口」开关（查看 → 菜单切换，持久化）。
    pub reuse_media_window: bool,
    /// 音乐窗口的已记录尺寸（逻辑像素，来自 [`UserPrefs::music_window_size`]）。
    pub music_window_size: Option<Size<Pixels>>,
    /// 视频窗口的已记录尺寸（与 `music_window_size` 对称）。
    pub video_window_size: Option<Size<Pixels>>,
    /// 主窗口的已记录尺寸（逻辑像素，来自 [`UserPrefs::main_window_size`]）。
    pub main_window_size: Option<Size<Pixels>>,
    /// 媒体窗口尺寸记录的防抖落盘。
    pub media_size_flush_debounce: SeqAfter,
    /// 主窗口尺寸记录的防抖落盘（镜像 `media_size_flush_debounce`；启动恢复用）。
    pub main_size_flush_debounce: SeqAfter,
    /// 会话持久化防抖：可重入、代序作废，
    /// 内容变化停顿后才写盘一次。
    pub session_flush_debounce: SeqAfter,
    /// 「阅读进度」落盘防抖：可重入、代序作废，滚动停止停顿后才写盘一次
    /// （镜像 `session_flush_debounce`；`#[cfg(not(test))]` 挂在防抖定时器里）。
    pub reading_progress_debounce: SeqAfter,
    /// 会话恢复进行中：恢复期间重建的选项卡不触发持久化。
    pub session_restoring: bool,
    /// 会话槽位「上次落盘内容」指纹：slot 名 → 内容 hash。
    pub session_slot_hashes: HashMap<String, u64>,
    /// 上次成功写盘时的清单指纹（清单序列化后 hash）。
    pub session_last_manifest_digest: Option<u64>,
    /// 已推送给 PDF 阅读器的查询 `(活动标签, 查询词)`。
    pub pdf_search: Option<(usize, String)>,
    /// 侧边栏搜索的惰性扫描缓存：按（查询词, 活动文档）复用。
    pub search_scan: Option<Rc<shell_scan::SidebarSearchScan>>,
    /// 侧边栏搜索**范围**（当前文件 / 整个文件夹）。
    pub search_scope: SearchScopeTab,
    /// 用户是否**手动**切换过搜索范围。
    pub search_scope_manual: bool,
    /// 文件夹级内容搜索的扫描结果。
    pub folder_search: Option<Rc<shell_scan::FolderSearchScan>>,
    /// 文件夹扫描**代**：每次发起新扫描 +1。
    pub folder_search_gen: u64,
    /// quick open 后台扫描**代**。
    pub quick_open_gen: u64,
    /// 最近一条 GoToLine 提示通知的 key。
    pub goto_line_toast_key: Option<ElementId>,
    /// 最近一条 Find「当前选项卡不支持查找」提示通知的 key。
    pub find_toast_key: Option<ElementId>,
    /// 最近一条「导出/导入设置」结果提示通知的 key。
    pub settings_toast_key: Option<ElementId>,
    /// 实际执行过的搜索扫描次数（测试用）。
    pub search_scan_count: usize,
    /// 侧边栏搜索「防抖后提交生效」的查询词（已归一化）。
    pub search_committed: String,
    /// 防抖窗口内最新一次输入（未提交的归一化查询词）。
    pub search_pending: String,
    /// 搜索防抖：可重入、代序作废。
    pub search_debounce: SeqAfter,
    /// 正在拖拽的选项卡下标（None = 未在拖拽）。
    pub tab_dragging: Option<usize>,
    /// 本次拖拽开始时 gpui 拖拽 picture 的「抓取偏移」。
    pub tab_drag_grab: Option<Point<Pixels>>,
    /// 拖拽中的选项卡光标已越出窗口边缘（尚未松手）。
    pub drag_outside_armed: bool,
    /// 本次 `on_drag_move` 算出的「幽灵窗应显示」决策。
    pub drag_ghost_visible: bool,
    /// 拖拽选项卡排序：本次拖拽过程光标下的「插入下标」。
    pub tab_reorder_target: Option<usize>,
    /// PDF 阅读画布背景预设（config `pdf_bg`）。
    pub pdf_bg: PdfBgPref,
    /// PDF 自定义画布底色（config `pdf_bg_custom`）。
    pub pdf_bg_custom: Option<String>,
    /// 最近一次对 PDF 页操作失败已提示过的错误文案（toast 去重）——**按 PDF 文档实体分槽**
    /// （`EntityId → 文案`）：每个文档独立去重，避免单槽跨文档共享污染（A 文档 seen 吞掉
    /// B 文档首次同文案失败提示）。实体消亡后槽位只随文档数缓慢增长（entry 极小、对其他
    /// 文档无害，EntityId 带代际号不复用），不逐关闭点清理。
    pub pdf_last_error_seen: HashMap<gpui::EntityId, String>,
    /// PDF 阅读位置书签（规范化路径 → 列表）。
    pub pdf_bookmarks: HashMap<String, Vec<PdfBookmark>>,
    /// 文件树过滤词（防抖后提交、已归一化小写）。
    pub files_filter: String,
    /// 防抖窗口内最新一次过滤输入（未提交）。
    pub files_filter_pending: String,
    /// 文件树过滤防抖。
    pub files_filter_debounce: SeqAfter,
    /// 最近打开的文件夹路径（新→旧）。
    pub recent_folders: Vec<String>,
    /// 最近打开的文件路径（新→旧）。
    pub recent_files: Vec<String>,
    /// 已退役的 live2 实体延迟回收队列（`commit_live` 暂存、下一帧 `on_next_frame`
    /// 清空，把级联 Drop 移出点击同步路径）。
    pub retired_live2: Vec<gpui::Entity<WyEditor>>,
    /// 是否正处于音乐模式（整窗渲染 MusicView）。
    pub music_mode: bool,
}

impl AppModel {
    /// 构造带初值的模型：零选项卡起手，前 16 个参数为 `new_impl` 从 `UserPrefs` /
    /// 窗口外观解析出的启动初值，其余派生状态取全零/空默认。
    ///
    /// 命名与 `EditorApp::new_impl` 的 struct 字段一致。
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
        restore_reading_progress: bool,
        music_window_size: Option<Size<Pixels>>,
        video_window_size: Option<Size<Pixels>>,
        main_window_size: Option<Size<Pixels>>,
        recent_folders: Vec<String>,
        recent_files: Vec<String>,
        pdf_bookmarks: HashMap<String, Vec<PdfBookmark>>,
        reading_progress: HashMap<String, ReadingProgress>,
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
            restore_reading_progress,
            reading_progress,
            music_window_size,
            video_window_size,
            main_window_size,
            media_size_flush_debounce: SeqAfter::default(),
            main_size_flush_debounce: SeqAfter::default(),
            session_flush_debounce: SeqAfter::default(),
            reading_progress_debounce: SeqAfter::default(),
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
            pdf_last_error_seen: HashMap::new(),
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

impl AppModel {
    /// 退出预览编辑的**纯模型 + 实体**部分：把
    /// WYSIWYG 内容写回源码编辑区 + 内容镜像；live2 实体**延迟**暂存 `retired_live2`
    /// 由宿主调度 `on_next_frame` 到下一帧渲染后销毁（A：实体级联 Drop ~36ms 移出
    /// 点击同步路径）。
    ///
    /// 视图层收尾（`on_next_frame` 清 `retired_live2`、右栏 `md_wy` 对齐 / 行级 diff
    /// 兜底）不在本方法——宿主据返回值 [`CommitLiveAlign`] 执行。
    pub fn commit_live(&mut self, idx: usize, cx: &mut App) -> CommitLiveAlign {
        // 读取 + 拆 live2：直接持 `&mut self`，无需 RefCell（宿主调用时已持有模型守卫）。
        let (l2, l2_dirty) = match self.tabs.get_mut(idx) {
            Some(tab) => {
                // 本 live 会话是否有内容编辑（读后再清，供下方「未改动跳过写回」判定）。
                let l2_dirty = tab.live2_dirty;
                tab.live2_dirty = false;
                tab.live_edit = false;
                tab._live2_sub = None;
                let Some(l2) = tab.live2.take() else {
                    return CommitLiveAlign::NoLive2;
                };
                (l2, l2_dirty)
            }
            None => return CommitLiveAlign::NoLive2,
        };
        // A2：**延迟回收**。live2 实体（WyEditor 大对象：Doc 的 rope/行索引/派生缓存 +
        // 整 `Rc` 级联）的销毁占切回 ~36ms，不能丢在点击事件路径里同步 Drop。先把实体
        // 暂存到 `retired_live2`（clone 是 O(1) 句柄；本方法内 `l2` 继续照常读写），宿主
        // 再用 `cx.on_next_frame`（gpui 保证「下一帧渲染完之后」触发）清空队列放实体
        // 出作用域 Drop。**渲染引用安全性**：此处已 `tab.live2 = None`，切回后的任何
        // render 都不再引用旧实体；`on_next_frame` 又晚于「下一帧已渲染/呈现」，故延迟
        // Drop 时既无渲染在读它、也不占用展示切回结果的那一帧的 CPU。本方法所有提前
        // return 路径共用此延迟（未改动 O(1) 跳过 / 加载中退出同样受益）。
        self.retired_live2.push(l2.clone());
        // 视图从 WYSIWYG 换回源码编辑区：先清空两端编辑器可能残留的鼠标拖选状态
        // （须在任何提前 return 之前）。点击「预览编辑测试」菜单项时 GPUI 把 MouseDown
        // 按 `hitbox.is_hovered` 漏进编辑区置 `dragging=true`（不按 z-order 裁掉被菜单
        // 遮挡的区间）；对应的 MouseUp 又被菜单项 `activate` 的 `stop_propagation`
        // 截断派发、到不了编辑器 `on_mouse_up` → `dragging` 残留。切回后仅移动鼠标
        // 就会从 anchor=0（`set_text` 收拢到行首）自动扩选到指针处。在此「切回/切离」
        // 生命周期边界触发编辑器的统一拖选复位钩子（`cancel_mouse_drag`），并覆盖提前
        // return 路径（大文档未加载完不走 `set_text`）：WYSIWYG 实体（随后即销毁）与
        // 源码编辑区（转前台显示）都不留拖选态。
        l2.update(cx, |ed, _| ed.end_selection());
        self.tabs[idx]
            .payload
            .editor
            .update(cx, |ed, _| ed.cancel_mouse_drag());
        // 大文档后台解析在途就退出：live2 尚无内容，不能写回（否则源码编辑区被清空）。
        // 源码编辑区仍是权威源，直接丢弃空壳实体即可。
        if !l2.read(cx).is_loaded() {
            self.status = SharedString::from("预览编辑测试：关（加载中退出，未写回编辑区）");
            self.status_ok = false;
            return CommitLiveAlign::NoAlign;
        }
        // 未改动（live 期间无任何内容编辑，`live2_dirty == false`）：live2 文本恒等于
        // 打开瞬间的源码 COW 快照（toggle ON 只从该快照建实体，无其它写入路径），
        // 整篇物化 + `set_text` 写回 + 喂右栏纯属 O(n) 浪费（大文档级联重排卡 7-8s）——
        // 直接销毁实体切回（O(1)），源码编辑区仍是原内容、可继续编辑。
        if !l2_dirty {
            LIVE_COMMIT_SKIP_UNCHANGED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.status = SharedString::from("预览编辑测试：关（未改动，无需写回）");
            self.status_ok = true;
            return CommitLiveAlign::NoAlign;
        }
        LIVE_COMMIT_WRITEBACK.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // —— 编辑日志重放（M：方案2，取代行级 diff 的主路径）——
        // live2 在 live 会话期把每笔内容变更（`replace_range` / 撤销 / 重做统一缴存）记成
        // `(字节区间, 新文本)` op 日志：两步起点同源（toggle ON 的源码 COW Rope 快照、
        // live 期间源码编辑区不被改写）→ op 字节偏移在源码编辑区与 live2 恒等，**按序
        // 重放即得最终态**，无需行级 LCS / 整篇物化比对。单 op = O(log n) Rope 编辑 +
        // 窗口化高亮增量。实测（debug，6.4MB）：写回本体 1 字符 ~1ms、3 行 ~3ms（旧 diff
        // 光行级扫描就 ~45ms）；`tab.content` 经 A1 走 SharedString 零拷贝分配（大文档 rope
        // 源首次 `text_shared` 恰一次 O(n) 物化 ~20ms 属固有下限）；live2 实体销毁（~36ms）
        // 已由 A2 延迟到下一帧（见上 `retired_live2`），不再计进本点击路径。
        let ops = l2.update(cx, |ed, _| ed.take_live_ops());
        if !ops.is_empty() {
            // 最终态 = live2 文本（A1：`SharedString` 句柄的 O(1) clone 赋给 `tab.content`，
            // 零拷贝共享同一缓冲；大文档 rope 源首次 `text_shared` 恰一次 O(n) 物化是
            // `tab.content` 必须为整段字符串的固有下限，已是最小化作一次）。
            let content = l2.read(cx).text();
            let src_len;
            let ok = {
                let tab = &mut self.tabs[idx];
                tab.payload.content = content.clone();
                // 行/字符计数直接取 WyEditor 增量维护的 O(1) 值（订阅已随每次编辑更新；
                // 不再对 6.4MB 全文做 lines()/chars() 线性扫——那是旧路径 ~90ms 的主来源）。
                tab.payload.line_count = l2.read(cx).line_count();
                tab.payload.char_count = l2.read(cx).char_count();
                // 重放前的源缓冲区长度（O(1)；右栏 md_wy 逐 op 重放的前提 = 起点与源同长，
                // 长度不等即空壳/失步 → 整篇喂源兜底，见宿主 `md_wy_align_after_ops`）。
                src_len = tab.payload.editor.read(cx).text().len();
                // 重放到源码编辑区（`apply_live_replay` 内置「干跑 + O(1) 长度栏」护栏）：
                // 自检失败 → 缓冲区未被改动，落宿主行级 diff 兜底（content 仍是最终态）。
                tab.payload
                    .editor
                    .update(cx, |ed, _| ed.apply_live_replay(&ops, content.as_ref()))
            };
            if ok {
                LIVE_COMMIT_OPS_REPLAYED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                LIVE_COMMIT_OPS_COUNT.fetch_add(ops.len(), std::sync::atomic::Ordering::Relaxed);
                self.status = SharedString::from("预览编辑（WYSIWYG）：关（内容已写回编辑区）");
                self.status_ok = true;
                return CommitLiveAlign::Ops { ops, src_len, content };
            }
            // 护栏失败 → 落到行级 diff 兜底（源码编辑区未被重放污染，`base` 仍有效）。
        }
        // —— 既有行级 diff 写回兜底（护栏失败 / 有脏标记但无日志；行级 hunk 计算与源码
        // 编辑区应用留在宿主 shell 侧，见 [`CommitLiveAlign::Diff`]）——
        // 模型侧在此只更新 `tab.content` / `line_count` / `char_count`（与旧实现同一位置
        // 一并以 live2 最终文本为权威），`base`（源缓冲区）与 `content`（最终态）交宿主。
        let base = self.tabs[idx].payload.editor.read(cx).text().to_string();
        let content = l2.read(cx).text();
        let tab = &mut self.tabs[idx];
        tab.payload.content = content.clone();
        let n_lines = content.as_ref().lines().count().max(1);
        let n_chars = content.as_ref().chars().count();
        tab.payload.line_count = n_lines;
        tab.payload.char_count = n_chars;
        // 与 Ops 臂同款收尾（`commit_live` 最后一条即此 status）：
        // diff 兜底写回后状态栏归位「内容已写回编辑区 / 正常」，防残留「关闭中」旧文案。
        self.status = SharedString::from("预览编辑（WYSIWYG）：关（内容已写回编辑区）");
        self.status_ok = true;
        CommitLiveAlign::Diff { base, content }
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
            restore_reading_progress: true,
            reading_progress: HashMap::new(),
            music_window_size: None,
            video_window_size: None,
            main_window_size: None,
            media_size_flush_debounce: SeqAfter::default(),
            main_size_flush_debounce: SeqAfter::default(),
            session_flush_debounce: SeqAfter::default(),
            reading_progress_debounce: SeqAfter::default(),
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
            pdf_last_error_seen: HashMap::new(),
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

/// PDF「阅读位置书签」一条：目标页码（0 基）+ 展示标题。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PdfBookmark {
    /// 目标页码（0 基；`PdfViewer::scroll_to_page` 的直接入参）。
    pub page_index: usize,
    /// 展示标题（优先当前大纲标题；无则「第 N 页」，N 为 1 基页码）。
    pub title: String,
}

/// 侧边栏搜索**范围**：只搜当前文件 / 递归搜已打开文件夹内文本文件。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum SearchScopeTab {
    /// 当前文档内容（+ 跨打开文件文件名匹配），旧语义。
    #[default]
    CurrentFile,
    /// 整个打开的文件夹（未打开文件夹时不可用）。
    Folder,
}

/// 一个文件选项卡：独立持有自研编辑器组件状态、文件路径与脏标记。
/// 23 个随选项卡搬迁的字段收敛进 [`TabPayload`]（`payload` 内嵌，单源）——跨窗口
/// 搬迁时整体 move、零复制；本结构体只保留窗口本地 live 态（`live_edit` / `live2` /
/// `live2_dirty` / `live_vscroll`）与订阅（`_edit_sub` / `_pdf_sub` / `_live2_sub`），
/// 迁入时经 [`EditorTab::from_detached`] 重建。
pub struct EditorTab {
    /// 跨窗搬迁载荷：[`TabPayload`] 本体（23 个搬迁字段；[`DetachedTab`] 即其别名）。
    /// 源窗口专属的 live 态 / 订阅不在此内，跨窗搬迁时重建。
    pub payload: TabPayload,
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
    /// 监听编辑事件 → 置脏 + 刷新状态栏（保持有效）
    pub _edit_sub: Subscription,
    /// 监听 PDF 阅读器 `notify()`（检索结果/加载进度到达）→ 刷新 app（侧边栏计数）。
    /// **随选项卡持有**：多 PDF 选项卡并存时各自独立订阅，避免旧单槽全局
    /// `EditorApp::_pdf_observe` 被后打开者覆盖、先打开 PDF 的 notify 丢失（真 bug）。
    /// 订阅绑定本窗口 EditorApp；随选项卡搬迁时随源 `EditorTab` 一起丢弃（取
    /// [`DetachedTab`] 载荷），目标窗口 `adopt_tab` 重建。关选项卡 / 拔除时随
    /// `EditorTab` drop 自动退订（无幽灵回调 / 泄漏）。
    pub _pdf_sub: Option<gpui::Subscription>,
    /// `live2`（WyEditor）的变更订阅。
    pub _live2_sub: Option<Subscription>,
    /// 预览编辑（WYSIWYG）宿主纵向滚动句柄：`#live-scroll`（`overflow_y_scroll`）
    /// 经 `track_scroll` 跟踪它，右侧 `Scrollbar::vertical` 跟随显示（宿主原生滚动条
    /// 在此 GPUI 不绘制，需自绘）。每次渲染重建容器，句柄存这里跨帧稳定。
    pub live_vscroll: gpui::ScrollHandle,
}

/// 跨窗搬迁载荷：EditorTab 与 DetachedTab 共用（[`DetachedTab`] 即本类型别名）。
/// 编辑器 / 预览等实体句柄共享（GPUI 实体与窗口无关），连同文件元数据一起跨窗口搬迁。
/// `EditorTab` 内嵌本载荷（单源）；搬迁时 `EditorTab` → 载荷走 [`From<EditorTab>`]
/// 整体搬出 `payload`，源窗口关联的 live 态与订阅（`live_edit` / `live2` /
/// `live2_dirty` / `_live2_sub` / `live_vscroll` / `_edit_sub` / `_pdf_sub`）随
/// `EditorTab` drop 一并丢弃，目标窗口经 [`EditorTab::from_detached`] 重建。
/// `_edit_sub` 是源窗口持有的订阅，拔掉时随源 `EditorTab` 一并丢弃，目标窗口
/// 重新建立订阅。
pub struct TabPayload {
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
    /// 大文档 P1 后台装配在途（整篇 rope 尚在生产端）：编辑区已显示首块但**锁输入**
    /// （渲染层全区域透明拦截，见 panes.rs）——避免「打开期间编辑」与 EOF 整篇换装
    /// 竞态；`finish_file_text` 换装成功后置 false。
    pub loading_lock: bool,
    pub edit_version: u64,
    /// 「阅读进度恢复」目标（与 [`EditorTab::reading_restore`] 同语义，随选项卡搬迁）。
    pub reading_restore: Option<ReadingProgress>,
    /// 「md 阅读进度恢复在途」守卫（与 [`EditorTab::md_restore_pending`] 同语义；
    /// 未消费完就被拖到新窗口时随选项卡搬迁）。
    pub md_restore_pending: Option<(usize, u32)>,
    /// live（WYSIWYG）可见面阅读进度恢复目标：存档源码行。默认打开即 WYSIWYG 时，
    /// `apply_reading_progress` 同时记下它，宿主逐帧（`advance_live2_restore`）等在途
    /// live2 装配完成（`is_loaded()`）后 `set_scroll_target` 钉到该行并清位——弥补
    /// 恢复只写（隐藏的）源码编辑区/右栏 md_wy、live 可见面不回位的缺口。随选项卡搬迁；
    /// 非 live 打开/退出 live 后不消费（源码态恢复走 `reading_restore`）。
    pub live2_restore: Option<usize>,
}

/// 一个从窗口中「拔出」的选项卡快照：即 [`TabPayload`] 本体（保留原名，宿主 / 测试 /
/// `tabs.rs` 占位字面量继续按其构造）。
pub type DetachedTab = TabPayload;

/// `EditorTab` → 跨窗载荷：`..` 解构丢弃源窗口专属的 live 态与订阅
/// （`live_edit` / `live2` / `live2_dirty` / `_live2_sub` / `live_vscroll` /
/// `_edit_sub` / `_pdf_sub`），与旧 `take_tab_payload` 逐字段手抄语义等价。
impl From<EditorTab> for DetachedTab {
    fn from(tab: EditorTab) -> Self {
        // 搬迁前 live 必须先提交写回（本仓所有 take_tab_payload 调用点均在
        // `commit_live` 之后）；仅调试断言，release 无行为差异。
        debug_assert!(!tab.live_edit, "live_edit 选项卡须先 commit_live 再取出载荷");
        // 内嵌单源：整体 move `payload`，源窗口 live 态 / 订阅随 `tab` drop 丢弃。
        tab.payload
    }
}

impl EditorTab {
    /// 从跨窗载荷重建完整选项卡：`payload` 承接 23 个搬迁字段；live 态 / 订阅按调用方
    /// 决策回填——`_edit_sub` 由调用方先建传入、`_pdf_sub`/`_live2_sub` 置 `None`
    /// （PDF 订阅在 adopt 的 Pdf 分支重建）。
    pub fn from_detached(
        payload: DetachedTab,
        _edit_sub: Subscription,
        live_edit: bool,
        live2: Option<gpui::Entity<WyEditor>>,
        live2_dirty: bool,
        live_vscroll: gpui::ScrollHandle,
    ) -> Self {
        Self {
            payload,
            live_edit,
            live2,
            live2_dirty,
            _edit_sub,
            live_vscroll,
            _pdf_sub: None,
            _live2_sub: None,
        }
    }
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

/// 右栏 `md_wy`「在途装配」状态的**收敛标记**单字段状态包（纯安装轴 `pending`+`seq` 已
/// 收敛进 WyEditor `install_state`，见 `markdown_wysiwyg::WyInstallState`）：Step2
/// （`early`）、Step3（`pending_full`）叠加态，EditorTab ⇄ DetachedTab 搬迁时作为
/// **一个字段**整包搬移（不再逐带双份复制进 DetachedTab）。
///
/// 两子项各自独立置位/清位（非互斥枚举）：`early` 与 `pending_full` 亦可在多阶段协作时
/// 同时置位——故收敛为**状态包**而非互斥枚举，保持「各标记独立置位/清位」的时序语义。
#[derive(Default)]
pub struct MdWyInstall {
    /// Step2 提前整篇解析在途状态（见 [`MdWyEarly`]）。
    pub early: Option<MdWyEarly>,
    /// Step3「整篇在途」标记：`Some((装配代号, prefix 字节
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
