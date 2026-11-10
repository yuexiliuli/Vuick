//! 侧边栏：可折叠/展开，顶部左侧为一竖列「目录 / 搜索」切换栏，右侧为当前视图内容。
//!
//! 折叠/展开开关放在窗口标题栏右上角（见 [`EditorApp::render_title_bar`]）；
//! 「目录」「搜索」两个按钮竖列排布，点击各自调用出对应界面。侧边栏为无缝 mica 底色
//! 直连外壳：与选项卡栏+编辑区、状态栏全部直角贴缝（无浮层卡片/圆角/间距），右缘仅以
//! 一条发丝线（`bar_border`）与正文区分隔。
//!
//! ## 模块结构（按职责拆分）
//!
//! 本模块只保留 **类型定义 / 常量 / 共享 helper**（侧栏状态、文件树状态），对全部
//! 子模块经 `use super::*` 可见；跨文件搜索的**纯扫描层**已拆独立 crate `shell_scan`
//! （文件夹递归扫描 / 行级命中 / ASCII 查找 / 扫描结果缓存与 LF 归一化），此处经
//! `pub(super) use` 重导出保持既有路径。`EditorApp` 的侧栏方法按职责拆分到下列
//! 子模块（`impl` 块跨文件合并，行为与可见性语义不变）：
//!
//! - [`chrome`]：侧栏外壳（视图切换 / 正文区布局 / 右缘拖拽调宽 / 活动栏）
//! - [`toc`]：目录视图（Markdown 标题 / PDF 书签，折叠 / 自适应宽 / 虚拟化）
//! - [`search`]：跨文件搜索（当前文件 / 整个文件夹，防抖扫描与命中跳转）
//! - [`files`]：文件树视图（渲染 / 懒加载子项 / 展开折叠 / 过滤）
//! - [`files_ops`]：文件树操作（右键菜单 / 新建 / 重命名 / 删除 / 剪贴板文件操作）

use super::file_tree_prompt::FileTreePromptKind;
use super::*;
use ui::input::Input;

use config::limits::{FOLDER_SEARCH_RESULT_CAP, SEARCH_RESULT_CAP};
use config::user::UserPrefs;
use gpui::{HighlightStyle, StyledText};
use ui::motion::{hover_bg_fade, hover_spring};

use std::rc::Rc;

mod chrome;
mod files;
mod files_ops;
mod search;
mod toc;

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

/// 文件树行缩进（px）：与目录条目层级缩进同款定值。
const FILE_TREE_INDENT: f32 = 14.0;
/// 文件树单行固定高度（px）：行强制等高，文件树虚拟化按此 divisor 精确切片
/// （与目录视图 `toc::TOC_ROW_H` 同口径；行内图标 14px + 上下 4px 留白均容纳）。
const FILE_TREE_ROW_H: f32 = 31.0;
/// 文件树「过滤文件名」输入防抖时长（ms）：停顿这么久才提交过滤词并重收集可见行
/// （大目录下避免逐键 O(可见行) 重算布局缓存，同搜索防抖姿势）。
const FILES_FILTER_DEBOUNCE_MS: u64 = 120;

/// 侧边栏搜索输入防抖时长（ms）：输入停顿这么久才提交查询并执行一次扫描。
/// 连续输入（间隔 < 本值）只更新待提交词，不触发每键全量重扫。
pub(super) const SEARCH_DEBOUNCE_MS: u64 = 150;

/// 归一化搜索查询词（与旧 `value().trim().to_lowercase()` 一致，作缓存键与渲染用词）。
fn normalize_search_query(q: &str) -> String {
    q.trim().to_lowercase()
}

/// 侧边栏视图。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SidebarView {
    /// 当前文档目录（Markdown 标题 / PDF 书签）。
    Toc,
    /// 跨打开文件的搜索。
    Search,
    /// 打开文件夹的文件树（嵌套目录浏览）。
    Files,
}

/// 文件树单条子项：显示名 + 是否目录 + 完整路径。目录在前的排序结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TreeEntry {
    /// 显示名（`file_name` 的 UTF-8 字符串，排序键）。
    pub(crate) name: String,
    /// 是否目录（目录优先于文件）。
    pub(crate) is_dir: bool,
    /// 完整路径（行点击 / 展开 / 打开文件的依据）。
    pub(crate) path: PathBuf,
}

/// 文件树状态：根 + 已展开目录集 + 每个已展开目录的直接子项缓存。
///
/// 懒加载纪律：**禁止整棵递归预扫描**——只在目录首次展开时后台读一次直接子项
/// （[`EditorApp::spawn_file_tree_scan`]），建材缓存后折叠再展开复用；渲染逐帧
/// O(可见行)，绝不每帧重扫 `read_dir` 或重建 HashMap。换根 → 全清。
#[derive(Default)]
pub(crate) struct FileTreeState {
    /// 文件树根目录（未打开 / 路径失效 = `None`）。
    pub(crate) root: Option<PathBuf>,
    /// 已展开的目录完整路径集合（折叠即移除，children 缓存保留复用）。
    pub(crate) expanded: std::collections::HashSet<PathBuf>,
    /// 每个**已扫描过**目录的直接子项缓存（漏项 = 尚未扫描，展示层按「加载中」处理）。
    pub(crate) children: std::collections::HashMap<PathBuf, Vec<TreeEntry>>,
    /// 正在后台扫描的目录（防重复读盘）。
    pub(crate) pending: std::collections::HashSet<PathBuf>,
    /// 树结构版本：换根 / 展开折叠 / 扫描子项落地时 +1，可见行缓存
    /// （[`SidebarChromeState::files_layout`]）以此失效重建。
    pub(crate) layout_gen: u64,
}

impl FileTreeState {
    /// 换根：清空已展开/子项缓存/在途扫描（残留的旧根后台读盘完成后查 `root` 丢弃）。
    pub(crate) fn set_root(&mut self, root: Option<PathBuf>) {
        self.root = root;
        self.expanded.clear();
        self.children.clear();
        self.pending.clear();
        self.layout_gen = self.layout_gen.wrapping_add(1);
    }
}

/// 文件树**可见行**描述（不含元素；根 + 已展开递归子项 + 空目录占位）。
/// 树结构没变（`files.layout_gen`）时整表复用，渲染只把可视带内的行物化为元素——
/// 万级子项下不再每帧全量构造行元素 / 键控状态 / 路径克隆。
#[derive(Clone)]
pub(crate) struct FileRow {
    /// 行对应路径（占位行 = 空目录自身；文件行 = 打开/点击依据）。
    pub(crate) path: PathBuf,
    /// 显示名（空目录占位行留空，渲染「（空）」）。
    pub(crate) label: SharedString,
    /// 是否目录（目录行可展开/折叠）。
    pub(crate) is_dir: bool,
    /// 树深（缩进）。
    pub(crate) depth: usize,
    /// 空目录占位行（无点击语义）。
    pub(crate) placeholder: bool,
}

/// 侧边栏「外壳」状态：可见性、当前视图与目录（TOC）面板的宽度/折叠/右缘拖拽。
/// 与「检索输入 + 缓存 + 订阅」分属（后者仍在 `EditorApp`，后续洋葱继续剥）。
/// 全部 `pub(crate)`：跨 `app` 子模块读写 + `app::tests` 直读。
pub(crate) struct SidebarChromeState {
    /// 侧边栏是否展开。
    pub(crate) open: bool,
    /// 侧边栏当前视图（目录 / 搜索）。
    pub(crate) view: SidebarView,
    /// 侧边栏内容面板是否展开（false = 只留窄活动栏，目录/搜索面板收起）。
    pub(crate) panel_open: bool,
    /// 侧边栏目录折叠集合（仅当前激活文档，切换文档时清空）。
    pub(crate) toc_collapsed: std::collections::HashSet<usize>,
    /// 侧边栏目录折叠集合对应的选项卡下标（切换文档即失效清空）。
    pub(crate) toc_tab: Option<usize>,
    /// 目录滚动句柄：`#sidebar-toc-scroll` 经 `track_scroll` 跟踪，虚拟化按偏移只物化可视带。
    pub(crate) toc_vscroll: gpui::ScrollHandle,
    /// 搜索结果滚动句柄：`#sidebar-search-results*` 经 `track_scroll` 跟踪，
    /// 右侧自绘纵向滚动条跟随（与目录/编辑器一致）。
    pub(crate) search_vscroll: gpui::ScrollHandle,
    /// 文件树根 / 展开集 / 子项缓存（懒加载，见 [`FileTreeState`]）。
    pub(crate) files: FileTreeState,
    /// 文件树**可见行描述缓存** `(files.layout_gen, 行表)`：树结构没变逐帧复用，
    /// 渲染只物化可视带（同目录视图 TOC 虚拟化口径）。
    pub(crate) files_layout: Option<(u64, Vec<FileRow>)>,
    /// 文件树滚动句柄：`#sidebar-files-scroll` 经 `track_scroll` 跟踪，
    /// 右侧自绘纵向滚动条跟随（与目录/搜索一致）。
    pub(crate) files_vscroll: gpui::ScrollHandle,
    /// 目录自适应宽缓存 `(选项卡下标, 编辑版本, WyEditor 内容代号, 宽)`：内容没变只算一次，
    /// 禁逐帧 O(N) 全量测宽（大文档上万标题下每帧重扫 = 卡死热源之一）。
    pub(crate) toc_width: Option<(usize, u64, u64, f32)>,
    /// 目录条目缓存 `(选项卡下标, 编辑版本, WyEditor 内容代号, 条目, 可见条目下标, 跳转目标)`：
    /// 宿主/内容/折叠没变逐帧 O(1) 复用（虚拟化只物化可视带，不再每帧全量物化万级条目）。
    /// 内容代号 = `WyEditor::content_gen`：预览关/重装（换实体）与整篇换源（同实体 in-place
    /// 喂料）都使其失效重建，避免渲染上一版条目。
    pub(crate) toc_cache:
        Option<(usize, u64, u64, markdown_wysiwyg::TocEntries, Vec<usize>, MdTocScroll)>,
    /// 侧边栏当前宽度（px）：目录视图随内容自适应，也可右缘拖拽手动调整。
    pub(crate) width: f32,
    /// 是否仍随目录内容自适应宽度（手动拖拽后关闭，切换文档重开）。
    pub(crate) width_auto: bool,
    /// 侧边栏右缘拖拽：`Some((起始 x, 起始宽))`。
    pub(crate) resize: Option<(f32, f32)>,
    /// 侧边栏右缘是否被鼠标悬停（悬停时高亮提示可拖拽）。
    pub(crate) resize_hovered: bool,
}

/// 目录条目总数超过此值才启用列表虚拟化（普通小文档仍全量渲染，行为不变）。
const TOC_VIRTUALIZE_MIN: usize = 200;
/// 可视带外上下各缓冲的条目行数（提前物化，滚动不闪烁）。
const TOC_BAND_BUFFER: usize = 8;

impl SidebarChromeState {
    /// 切换视图：未在该面板 → 展开并切到；已在该面板再点 → 只收起面板。
    /// （折叠/展开逻辑此前栖身在 `toggle_sidebar_view`，随状态一起收编。）
    pub(super) fn toggle_view(&mut self, view: SidebarView) {
        if self.panel_open && self.view == view {
            self.panel_open = false;
        } else {
            self.open = true;
            self.panel_open = true;
            self.view = view;
        }
    }
}

/// 读取目录**直接子项**：文件夹优先、组内按显示名（`to_string_lossy`）字符串排序
/// （ASCII/CJK 一视同仁）；过滤读取错误项，绝不 panic（失败返回空，展示为「（空）」）。
fn read_dir_entries(path: &std::path::Path) -> Vec<TreeEntry> {
    let mut dirs: Vec<TreeEntry> = Vec::new();
    let mut files: Vec<TreeEntry> = Vec::new();
    let Ok(rd) = std::fs::read_dir(path) else {
        return Vec::new();
    };
    for entry in rd.flatten() {
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        let e = TreeEntry {
            name: entry.file_name().to_string_lossy().into_owned(),
            is_dir: ft.is_dir(),
            path: entry.path(),
        };
        if ft.is_dir() {
            dirs.push(e);
        } else {
            files.push(e);
        }
    }
    dirs.sort_by(|a, b| a.name.cmp(&b.name));
    files.sort_by(|a, b| a.name.cmp(&b.name));
    dirs.into_iter().chain(files).collect()
}
