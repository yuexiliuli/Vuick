//! 侧边栏**纯状态与类型**（2026-11 自 shell 的 `app/sidebar/mod.rs` 迁入）。
//!
//! 只交付类型 / 常量 / 共享 helper / **状态纯变换**：
//!
//! - [`SidebarChromeState`]：侧栏「外壳」状态（可见性 / 视图 / 目录折叠 / 宽度与右缘
//!   拖拽 / 目录与文件树缓存 / 滚动句柄）。状态本体**仍作 `EditorApp::sidebar` 普通字段**
//!   （`app::tests::sidebar` 等跨 crate 既有字段读写不变量强制），实体的渲染/事件经
//!   `&mut SidebarChromeState` 或 [`crate::host::SidebarHost`] 服务在宿主侧落地。
//! - [`SidebarView`] / [`TreeEntry`] / [`FileTreeState`] / [`FileRow`]（文件树下一阶段随迁，
//!   本阶段保持类型/字段可见性与既有路径）。
//! - 状态纯变换：`toggle_view` / [`resize_start`] / [`resize_update`] / [`resize_end`] /
//!   [`toc_collapse`]——宿主侧与域侧共用同一份变换，杜绝逻辑双写漂移。

use std::path::{Path, PathBuf};

use gpui::SharedString;

use config::layout::{SIDEBAR_WIDTH_MAX, SIDEBAR_WIDTH_MIN};
use shell_core::{AppModel, MdTocScroll, SearchScopeTab};

/// 侧边栏视图。
/// `#[doc(hidden)] pub`：`EditorApp::sidebar.view` 与 `app::tests` 枚举比较。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[doc(hidden)]
pub enum SidebarView {
    /// 当前文档目录（Markdown 标题 / PDF 书签）。
    Toc,
    /// 跨打开文件的搜索（`search.rs` 已迁本 crate）。
    Search,
    /// 打开文件夹的文件树（`files.rs` 已迁本 crate）。
    Files,
}

/// 文件树单条子项：显示名 + 是否目录 + 完整路径。目录在前的排序结果。
/// `#[doc(hidden)] pub`：`FileTreeState.files.children` 元素类型（tests/settings_xfer.rs
/// 读 `e.path`），跨 crate 可见。
#[derive(Clone, Debug, PartialEq, Eq)]
#[doc(hidden)]
pub struct TreeEntry {
    /// 显示名（`file_name` 的 UTF-8 字符串，排序键）。
    /// `pub`：宿主/测试直写可见（下一阶段 files.rs 随迁前跨 crate 构造）。
    pub name: String,
    /// 是否目录（目录优先于文件）。
    /// `pub`：宿主/测试直写可见。
    pub is_dir: bool,
    /// 完整路径（行点击 / 展开 / 打开文件的依据）。
    /// `pub`：宿主/测试直写可见（tests/settings_xfer.rs 读 `e.path`）。
    pub path: PathBuf,
}

/// 文件树状态：根 + 已展开目录集 + 每个已展开目录的直接子项缓存。
///
/// 懒加载纪律：**禁止整棵递归预扫描**——只在目录首次展开时后台读一次直接子项
/// （宿主 `spawn_file_tree_scan` 位于 shell::app::sidebar host facade），建材缓存后折叠再展开复用；
/// 渲染逐帧 O(可见行)，绝不每帧重扫 `read_dir` 或重建 HashMap。换根 → 全清。
/// `#[doc(hidden)] pub`：`EditorApp.sidebar.files`（§8.6 外提链）元素类型。
#[derive(Default)]
#[doc(hidden)]
pub struct FileTreeState {
    /// 文件树根目录（未打开 / 路径失效 = `None`）。
    /// `pub`：宿主/测试直写可见。
    pub root: Option<PathBuf>,
    /// 已展开的目录完整路径集合（折叠即移除，children 缓存保留复用）。
    /// `pub`：宿主/测试直写可见（files.rs 已迁本 crate）。
    pub expanded: std::collections::HashSet<PathBuf>,
    /// 每个**已扫描过**目录的直接子项缓存（漏项 = 尚未扫描，展示层按「加载中」处理）。
    /// `pub`：宿主/测试直写可见（settings_xfer 读 children）。
    pub children: std::collections::HashMap<PathBuf, Vec<TreeEntry>>,
    /// 正在后台扫描的目录（防重复读盘）。
    /// `pub`：宿主/测试直写可见。
    pub pending: std::collections::HashSet<PathBuf>,
    /// 树结构版本：换根 / 展开折叠 / 扫描子项落地时 +1，可见行缓存
    /// （[`SidebarChromeState::files_layout`]）以此失效重建。
    /// `pub`：宿主/测试直写可见（files.rs 失效缓存递增）。
    pub layout_gen: u64,
}

impl FileTreeState {
    /// 换根：清空已展开/子项缓存/在途扫描（残留的旧根后台读盘完成后查 `root` 丢弃）。
    /// `#[doc(hidden)] pub`：`tests/palette.rs` 快速打开「无打开文件夹」前置断言。
    #[doc(hidden)]
    pub fn set_root(&mut self, root: Option<PathBuf>) {
        self.root = root;
        self.expanded.clear();
        self.children.clear();
        self.pending.clear();
        self.layout_gen = self.layout_gen.wrapping_add(1);
    }
}

/// 文件树后台扫描完成的**纯变换**：把某个目录的直接子项写入 `children` 缓存 + 布局代 +1；
/// 换根后旧目录的在途扫描结果不落地（路径已不在当前根下 → 丢弃，防孤儿缓存膨胀）。
/// 宿主 [`crate::host::SidebarHost::sidebar_file_tree_scan_complete`] 与域实体扫描任务共用
/// 同一份变换（单一事实源，防双写漂移）。`pub`：宿主侧（shell 的 file_ops/init 等）经
/// 宿主服务间接调用，本变换亦作测试可复用的纯函数。
pub fn file_tree_scan_complete(files: &mut FileTreeState, path: &Path, entries: Vec<TreeEntry>) {
    files.pending.remove(path);
    let stale = files
        .root
        .as_ref()
        .map(|r| !path.starts_with(r))
        .unwrap_or(true);
    if !stale {
        files.layout_gen = files.layout_gen.wrapping_add(1);
        files.children.insert(path.to_path_buf(), entries);
    }
}

/// 文件树失效的**纯变换**（新建 / 重命名 / 删除后的派生状态失效源）：`changed`（目录时
/// 含其后代）的展开集 / 子项缓存 / 在途扫描一并作废 + 布局代 +1。
/// 父目录重扫（`spawn_file_tree_scan`）由调用方负责（宿主侧文件副作用 / 域实体）。
pub fn file_tree_invalidate(files: &mut FileTreeState, changed: &Path) {
    files
        .children
        .retain(|p, _| p != changed && !p.starts_with(changed));
    files.pending.retain(|p| p != changed && !p.starts_with(changed));
    files.expanded.retain(|p| p != changed && !p.starts_with(changed));
    files.layout_gen = files.layout_gen.wrapping_add(1);
}

/// 文件树目录行展开/折叠的**纯变换**（单一事实源，宿主 `sidebar_file_tree_toggle` /
/// facade `file_tree_toggle` 共用）：折叠只移出 `expanded`；展开把目录入集合。
/// 两条分支都作废可见行布局缓存（`layout_gen` +1）。返回 `true` = **首展未扫**（`children`
/// 无该目录缓存），调用方需在**宿主侧** spawn 后台读那一层直接子项（不在域/回调内回租
/// 实体——目录行 on_click 回调期间 Sidebar 实体已租用，回租即 `double_lease_panic`）。
pub fn file_tree_toggle(files: &mut FileTreeState, path: &Path) -> bool {
    if files.expanded.remove(path) {
        files.layout_gen = files.layout_gen.wrapping_add(1);
        return false;
    }
    files.expanded.insert(path.to_path_buf());
    files.layout_gen = files.layout_gen.wrapping_add(1);
    !files.children.contains_key(path)
}

/// 文件树**可见行**描述（不含元素；根 + 已展开递归子项 + 空目录占位）。
/// 树结构没变（`files.layout_gen`）时整表复用，渲染只把可视带内的行物化为元素——
/// 万级子项下不再每帧全量构造行元素 / 键控状态 / 路径克隆。
/// `#[doc(hidden)] pub`：字段供 shell 的 files.rs（下一阶段随迁）跨 crate 构造。
#[derive(Clone)]
#[doc(hidden)]
pub struct FileRow {
    /// 行对应路径（占位行 = 空目录自身；文件行 = 打开/点击依据）。
    /// `pub`：宿主/测试直写可见（files.rs 跨 crate 构造）。
    pub path: PathBuf,
    /// 显示名（空目录占位行留空，渲染「（空）」）。
    /// `pub`：宿主/测试直写可见。
    pub label: SharedString,
    /// 是否目录（目录行可展开/折叠）。
    /// `pub`：宿主/测试直写可见。
    pub is_dir: bool,
    /// 树深（缩进）。
    /// `pub`：宿主/测试直写可见。
    pub depth: usize,
    /// 空目录占位行（无点击语义）。
    /// `pub`：宿主/测试直写可见。
    pub placeholder: bool,
}

/// 侧边栏「外壳」状态：可见性、当前视图与目录（TOC）面板的宽度/折叠/右缘拖拽。
/// 与「检索输入 + 扫描缓存 + 订阅」分属（后者 `search.rs` 已迁本 crate，渲染经共享模型）。
/// 纯结构移动（2026-11 自 shell 的 `app/sidebar/mod.rs` 迁入本 crate 后**原样保留字段**），
/// 由宿主 `EditorApp.sidebar` 普通字段持有，跨 crate（shell 编排 + 测试）字段全 `pub`。
/// `#[doc(hidden)] pub`：`EditorApp.sidebar` 字段 + `width/width_auto/files` 供
/// `tests/settings_xfer.rs` 直读/直写（§8.6 外提清单）。
#[doc(hidden)]
pub struct SidebarChromeState {
    /// 侧边栏是否展开。
    /// `pub`：宿主/测试直写可见。
    pub open: bool,
    /// 侧边栏当前视图（目录 / 搜索 / 文件树）。
    /// `pub`：宿主/测试直写可见（init/file_ops 直写）。
    pub view: SidebarView,
    /// 侧边栏内容面板是否展开（false = 只留窄活动栏，目录/搜索面板收起）。
    /// `pub`：宿主/测试直写可见。
    pub panel_open: bool,
    /// 侧边栏目录折叠集合（仅当前激活文档，切换文档时清空）。
    /// `pub`：宿主/测试直写可见（toc 缓存失效依赖）。
    pub toc_collapsed: std::collections::HashSet<usize>,
    /// 侧边栏目录折叠集合对应的选项卡下标（切换文档即失效清空）。
    /// `pub`：宿主/测试直写可见。
    pub toc_tab: Option<usize>,
    /// 目录滚动句柄：`#sidebar-toc-scroll` 经 `track_scroll` 跟踪，虚拟化按偏移只物化可视带。
    /// `pub`：宿主/测试直写可见（tests 读句柄断言）。
    pub toc_vscroll: gpui::ScrollHandle,
    /// 搜索结果滚动句柄：`#sidebar-search-results*` 经 `track_scroll` 跟踪，
    /// 右侧自绘纵向滚动条跟随（与目录/编辑器一致）。
    /// `pub`：宿主/测试直写可见（`search.rs` 已迁本 crate）。
    pub search_vscroll: gpui::ScrollHandle,
    /// 文件树根 / 展开集 / 子项缓存（懒加载，见 [`FileTreeState`]）。
    /// `pub`：宿主/测试直写可见。
    pub files: FileTreeState,
    /// 文件树**可见行描述缓存** `(files.layout_gen, 行表)`：树结构没变逐帧复用，
    /// 渲染只物化可视带（同目录视图 TOC 虚拟化口径）。
    /// `pub`：宿主/测试直写可见（tests 断言加载完成）。
    pub files_layout: Option<(u64, Vec<FileRow>)>,
    /// 文件树滚动句柄：`#sidebar-files-scroll` 经 `track_scroll` 跟踪，
    /// 右侧自绘纵向滚动条跟随（与目录/搜索一致）。
    /// `pub`：宿主/测试直写可见（files.rs 已迁本 crate）。
    pub files_vscroll: gpui::ScrollHandle,
    /// 目录自适应宽缓存 `(选项卡下标, 编辑版本, WyEditor 内容代号, 宽)`：内容没变只算一次，
    /// 禁逐帧 O(N) 全量测宽（大文档上万标题下每帧重扫 = 卡死热源之一）。
    /// `pub`：宿主/测试直写可见（toc 渲染失效）。
    pub toc_width: Option<(usize, u64, u64, f32)>,
    /// 目录条目缓存 `(选项卡下标, 编辑版本, WyEditor 内容代号, 条目, 可见条目下标, 跳转目标)`：
    /// 宿主/内容/折叠没变逐帧 O(1) 复用（虚拟化只物化可视带，不再每帧全量物化万级条目）。
    /// 内容代号 = `WyEditor::content_gen`：预览关/重装（换实体）与整篇换源（同实体 in-place
    /// 喂料）都使其失效重建，避免渲染上一版条目。
    /// `pub`：宿主/测试直写可见（toc 渲染失效重建）。
    pub toc_cache:
        Option<(usize, u64, u64, markdown_wysiwyg::TocEntries, Vec<usize>, MdTocScroll)>,
    /// 侧边栏当前宽度（px）：目录视图随内容自适应，也可右缘拖拽手动调整。
    /// `pub`：宿主/测试直写可见（settings_xfer 直读/直写）。
    pub width: f32,
    /// 是否仍随目录内容自适应宽度（手动拖拽后关闭，切换文档重开）。
    /// `pub`：宿主/测试直写可见（settings_xfer 直读/直写）。
    pub width_auto: bool,
    /// 侧边栏右缘拖拽：`Some((起始 x, 起始宽))`。
    /// `pub`：宿主/测试直写可见（tests/splitter.rs 拖拽断言）。
    pub resize: Option<(f32, f32)>,
    /// 侧边栏右缘是否被鼠标悬停（悬停时高亮提示可拖拽）。
    /// `pub`：宿主/测试直写可见（tests/splitter.rs hover 断言）。
    pub resize_hovered: bool,
}

impl SidebarChromeState {
    /// 切换视图：未在该面板 → 展开并切到；已在该面板再点 → 只收起面板。
    /// （折叠/展开逻辑此前栖身在 `toggle_sidebar_view`，随状态一起收编。）
    /// `#[doc(hidden)] pub`：`tests/pdf.rs` TOC 面板展开断言（§8.6 外提清单）。
    #[doc(hidden)]
    pub fn toggle_view(&mut self, view: SidebarView) {
        if self.panel_open && self.view == view {
            self.panel_open = false;
        } else {
            self.open = true;
            self.panel_open = true;
            self.view = view;
        }
    }
}

/// 侧边栏右缘拖拽**开始**的状态纯变换：记录 `(起始 x, 起始宽)`，关闭自适应由用户接管。
/// 返回是否真的开始了拖拽（供调用方决定是否 notify）。
pub fn resize_start(sb: &mut SidebarChromeState, start_x: f32) -> bool {
    sb.resize = Some((start_x, sb.width));
    sb.width_auto = false;
    true
}

/// 侧边栏右缘拖拽**进行中**的状态纯变换：按鼠标位移更新宽（自变量窗口坐标 x）。
/// 宽度 clamp 在 [MIN, MAX]。返回是否有拖拽在途（供调用方决定是否 notify）。
pub fn resize_update(sb: &mut SidebarChromeState, x: f32) -> bool {
    if let Some((start_x, start_w)) = sb.resize {
        sb.width = (start_w + (x - start_x))
            .clamp(SIDEBAR_WIDTH_MIN, SIDEBAR_WIDTH_MAX);
        true
    } else {
        false
    }
}

/// 侧边栏右缘拖拽**结束**的状态纯变换：宽度定稿、清拖拽标记。
/// 返回 `true` = 此前确有拖拽在途（调用方应持久化 + notify）。
pub fn resize_end(sb: &mut SidebarChromeState) -> bool {
    if sb.resize.is_some() {
        sb.resize = None;
        true
    } else {
        false
    }
}

/// 目录折叠集切换的状态纯变换：仅「有下级」条目可折叠；折叠集变更 → 可见性缓存作废
/// （下帧按新折叠重建可见条目表）。返回是否实际切换（供调用方决定是否 notify）。
pub fn toc_collapse(sb: &mut SidebarChromeState, idx: usize, has_child: bool) -> bool {
    if !has_child {
        return false;
    }
    if !sb.toc_collapsed.remove(&idx) {
        sb.toc_collapsed.insert(idx);
    }
    sb.toc_cache = None;
    true
}

/// 文件树行缩进（px）：与目录条目层级缩进同款定值。
pub const FILE_TREE_INDENT: f32 = 14.0;
/// 文件树单行固定高度（px）：行强制等高，文件树虚拟化按此 divisor 精确切片
/// （与目录视图 `toc::TOC_ROW_H` 同口径；行内图标 14px + 上下 4px 留白均容纳）。
pub const FILE_TREE_ROW_H: f32 = 31.0;
/// 文件树「过滤文件名」输入防抖时长（ms）：停顿这么久才提交过滤词并重收集可见行
/// （大目录下避免逐键 O(可见行) 重算布局缓存，同搜索防抖姿势）。
pub const FILES_FILTER_DEBOUNCE_MS: u64 = 120;

/// 侧边栏搜索输入防抖时长（ms）：输入停顿这么久才提交查询并执行一次扫描。
/// 连续输入（间隔 < 本值）只更新待提交词，不触发每键全量重扫。
pub const SEARCH_DEBOUNCE_MS: u64 = 150;

/// 当前生效的搜索范围（**纯逻辑**，侧栏搜索视图渲染 / 范围 tab 光用，单一事实源）：
/// 用户手动选过 → 手动值；否则**有打开文件夹时默认「整个文件夹」**，无文件夹回退
/// 「当前文件」（未打开文件夹时文件夹范围不可用）。
/// `model` 为宿主共享模型（`search_scope` / `search_scope_manual`），`sb` 为本状态
/// （`files.root`）；因分属两处，以参数传入保持纯函数（宿主 facade `effective_search_scope`
/// 与域实体渲染共用同一份，防逻辑双写漂移）。
pub fn effective_search_scope(model: &AppModel, sb: &SidebarChromeState) -> SearchScopeTab {
    if model.search_scope_manual {
        model.search_scope
    } else if sb.files.root.is_some() {
        SearchScopeTab::Folder
    } else {
        SearchScopeTab::CurrentFile
    }
}

/// 归一化搜索查询词（与旧 `value().trim().to_lowercase()` 一致，作缓存键与渲染用词）。
/// `pub`：本 crate 的 `search.rs` 跨模块调用。
pub fn normalize_search_query(q: &str) -> String {
    q.trim().to_lowercase()
}

/// 读取目录**直接子项**：文件夹优先、组内按显示名（`to_string_lossy`）字符串排序
/// （ASCII/CJK 一视同仁）；过滤读取错误项，绝不 panic（失败返回空，展示为「（空）」）。
/// `pub`：本 crate 的 `files.rs` 后台扫描跨模块调用。
pub fn read_dir_entries(path: &std::path::Path) -> Vec<TreeEntry> {
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
