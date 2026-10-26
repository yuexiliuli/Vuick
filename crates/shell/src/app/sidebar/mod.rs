//! 侧边栏：可折叠/展开，顶部左侧为一竖列「目录 / 搜索」切换栏，右侧为当前视图内容。
//!
//! 折叠/展开开关放在窗口标题栏右上角（见 [`EditorApp::render_title_bar`]）；
//! 「目录」「搜索」两个按钮竖列排布，点击各自调用出对应界面。侧边栏为无缝 mica 底色
//! 直连外壳：与选项卡栏+编辑区、状态栏全部直角贴缝（无浮层卡片/圆角/间距），右缘仅以
//! 一条发丝线（`bar_border`）与正文区分隔。
//!
//! ## 模块结构（按职责拆分）
//!
//! 本模块只保留 **类型定义 / 常量 / 共享 helper**（侧栏状态、文件树状态、搜索扫描
//! 机制与其 free 函数），对全部子模块经 `use super::*` 可见。`EditorApp` 的侧栏方法
//! 按职责拆分到下列子模块（`impl` 块跨文件合并，行为与可见性语义不变）：
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

/// 文件树行缩进（px）：与目录条目层级缩进同款定值。
const FILE_TREE_INDENT: f32 = 14.0;
/// 文件树单行固定高度（px）：行强制等高，文件树虚拟化按此 divisor 精确切片
/// （与目录视图 `toc::TOC_ROW_H` 同口径；行内图标 14px + 上下 4px 留白均容纳）。
const FILE_TREE_ROW_H: f32 = 31.0;

/// 侧边栏搜索输入防抖时长（ms）：输入停顿这么久才提交查询并执行一次扫描。
/// 连续输入（间隔 < 本值）只更新待提交词，不触发每键全量重扫。
pub(super) const SEARCH_DEBOUNCE_MS: u64 = 150;

/// 归一化搜索查询词（与旧 `value().trim().to_lowercase()` 一致，作缓存键与渲染用词）。
fn normalize_search_query(q: &str) -> String {
    q.trim().to_lowercase()
}

/// 侧边栏搜索**范围**：只搜当前文件 / 递归搜已打开文件夹内文本文件。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) enum SearchScopeTab {
    /// 当前文档内容（+ 跨打开文件文件名匹配），旧语义。
    #[default]
    CurrentFile,
    /// 整个打开的文件夹（未打开文件夹时不可用）。
    Folder,
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

/// 侧边栏搜索一次扫描产出的**命中索引**（渲染复用缓存，不重扫全文）。
#[derive(Debug, Default)]
pub(super) struct SidebarSearchScan {
    /// 归一化后的查询词（trim + to_lowercase，环节点上色与缓存键）。
    pub(super) query: String,
    /// 扫描时的活动选项卡下标（缓存键的一部分：切文档自动失效重扫）。
    pub(super) tab: usize,
    /// 文件名命中的选项卡下标（按打开顺序）。
    pub(super) file_tabs: Vec<usize>,
    /// 当前活动文档中内容命中的行。
    pub(super) line_hits: Vec<SearchLineHit>,
}

/// 一行内容命中：行号 + 首个命中文本的文档字节区间 + 展示条目片段。
#[derive(Debug, Clone)]
pub(super) struct SearchLineHit {
    /// 文档行号（0 基）。
    pub(super) line: usize,
    /// 首个命中文本的文档字节区间（跳转后选中高亮）。
    pub(super) range: std::ops::Range<usize>,
    /// 去掉首尾空白后的行内容（渲染条目显示）。
    pub(super) snippet: String,
}

/// 文件夹级搜索跳过的目录名（黑名单）：VCS / 构建产物 / 依赖目录。
const FOLDER_SCAN_SKIP_DIRS: &[&str] = &[".git", "target", "node_modules"];
/// 文件树「过滤文件名」输入防抖时长（ms）：停顿这么久才提交过滤词并重收集可见行
/// （大目录下避免逐键 O(可见行) 重算布局缓存，同搜索防抖姿势）。
const FILES_FILTER_DEBOUNCE_MS: u64 = 120;
/// 文件夹级搜索跳过的**超大文本文件**阈值：> 5MB 不读盘跳过（元数据判断）。
const FOLDER_SCAN_MAX_FILE_BYTES: u64 = 5 * 1024 * 1024;
/// 二进制嗅探采样窗口（读前 N 字节含 NUL → 视为二进制跳过）。
const FOLDER_SCAN_BINARY_SNIFF_BYTES: usize = 8192;

/// 文件夹级搜索「单文件命中组」（按文件分组渲染；词频 = 组内命中条数）。
#[derive(Debug, Clone)]
pub(crate) struct FolderFileHit {
    /// 命中文件绝对路径（点击行打开的依据）。
    pub(super) path: PathBuf,
    /// 组内命中行（行号 / LF 归一化全文字节区间 / trim 片段）。
    pub(super) hits: Vec<SearchLineHit>,
}

/// 文件夹级搜索的（后台增量装配的）扫描结果缓存。
#[derive(Debug, Default, Clone)]
pub(crate) struct FolderSearchScan {
    /// 归一化查询词（渲染命中词高亮与缓存键的一部分）。
    pub(super) query: String,
    /// 扫描时的文件夹根（缓存键的一部分：换根即失效重扫）。
    pub(super) root: Option<PathBuf>,
    /// 按文件分组的结果（扫描完成顺序）。
    pub(super) files: Vec<FolderFileHit>,
    /// 是否因达到 [`FOLDER_SEARCH_RESULT_CAP`] 截断。
    pub(super) truncated: bool,
    /// 是否仍在后台扫描（UI 据此显示「搜索中…」）。
    pub(super) running: bool,
    /// 扫描代：换查询 / 换根发起新扫描即 +1，旧代异步回调按代丢弃（防晚到覆盖新结果）。
    pub(super) generation: u64,
    /// 累计命中条数（词频与截断判定依据）。
    pub(super) total_hits: usize,
}

/// 后台扫描 → 主线程的一块增量更新（有界 channel，见 [`EditorApp::start_folder_scan`]）。
#[derive(Debug)]
pub(super) enum FolderScanUpdate {
    /// 一个文件命中组（含该文件全部命中行）。
    File(FolderFileHit),
    /// 扫描完成（携带是否截断）。
    Done { truncated: bool },
}

/// 在 `text` 中收集查询词 `q` 的**全部**大小写不敏感命中的字节区间（非重叠、逐段推进），
/// 供命中词预览高亮。区间恒落在 UTF-8 字符边界（`find_ci` 语义保证），跳过重叠尾随。
fn highlight_ranges(text: &str, q: &str) -> Vec<std::ops::Range<usize>> {
    let mut out: Vec<std::ops::Range<usize>> = Vec::new();
    if q.is_empty() {
        return out;
    }
    let mut pos = 0usize;
    while pos < text.len() {
        let Some(r) = find_ci(&text[pos..], q) else {
            break;
        };
        let start = pos + r.start;
        let end = pos + r.end;
        if end <= start {
            break; // 空命中防御（理论上不会发生）
        }
        out.push(start..end);
        pos = end;
    }
    out
}

/// 后台递归扫描文件夹内容（**不在主线程执行**）：跳过黑名单目录 / 会话目录、跳过
/// >5MB 超大文件与二进制 / 非 UTF-8 文件；对每个候选文本文件 LF 归一化后逐行扫查询词
/// （`scan_line_hits`），命中文件作为一个块经有界 `tx` 外推（背压让出）。`total` 为
/// 累计命中条数，达到 `cap` 即停止扫描（返回截断标记）。单文件读失败 / 单目录读失败 /
/// 无权限 → 跳过该条目计为已扫描，**不中断**整体扫描。
pub(super) async fn folder_scan_collect(
    root: &std::path::Path,
    q: &str,
    tx: futures::channel::mpsc::Sender<FolderScanUpdate>,
    session_dir: &std::path::Path,
    total: &mut usize,
    cap: usize,
) -> bool {
    use futures::SinkExt as _;
    let mut tx = tx;
    // 显式栈 DFS（避免深目录递归爆栈；深度顺序对结果无影响，条目整体排序保证确定性）。
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if *total >= cap {
            return true;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue; // 单目录读失败 → 跳过不中断
        };
        // 条目排序保证确定性（目录/文件一律按文件名），与树视图 read_dir_entries 同款。
        let mut entries: Vec<std::fs::DirEntry> = rd.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            if *total >= cap {
                return true;
            }
            let path = entry.path();
            // 目录判定用 `entry.file_type()`（**不跟随**符号链接）：符号链接 / Windows
            // 目录联接指向的目录一律**不递归**进栈，防「目录循环引用 → 扫描无限循环」
            // （`metadata` 跟随链接，用其判目录时 `.→自身` 的联接会无限入栈白耗磁盘）。
            let Ok(ft) = entry.file_type() else {
                continue; // 单条目元数据失败 → 跳过
            };
            if ft.is_dir() {
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default();
                if FOLDER_SCAN_SKIP_DIRS.contains(&name) || path == *session_dir {
                    continue;
                }
                stack.push(path);
            } else {
                // 文件（含指向文件的符号链接）：跟随元数据取真实类型 / 大小；指向目录的
                // 符号链接 / 其它特殊文件 → 跳过（一律不递归，防循环）。
                let Ok(meta) = std::fs::metadata(&path) else {
                    continue; // 单条目元数据失败 → 跳过
                };
                if !meta.is_file() {
                    continue;
                }
                if meta.len() > FOLDER_SCAN_MAX_FILE_BYTES {
                    continue; // 超大文件：不读盘跳过
                }
                let Ok(bytes) = std::fs::read(&path) else {
                    continue; // 单文件读失败 / 无权限 → 跳过不中断
                };
                // 二进制检测：采样窗口含 NUL 字节视为二进制；再校验整篇 UTF-8
                //（编辑器以 UTF-8 读盘，非 UTF-8 文件即使无 NUL 也不可索引）。
                if bytes[..bytes.len().min(FOLDER_SCAN_BINARY_SNIFF_BYTES)].contains(&0) {
                    continue;
                }
                let Ok(content) = std::str::from_utf8(&bytes) else {
                    continue;
                };
                // LF 归一化后再扫：命中行号 / 字节区间与打开文件后的编辑缓冲一致
                //（`open_file` 同样做 `normalize_newlines`），点击跳转选中才不偏。
                // 无 `\r`（LF 已归一）直接扫原文零分配；有 CR/CRLF 才物化归一化副本。
                let remaining = cap.saturating_sub(*total);
                let mut hits: Vec<SearchLineHit> = Vec::new();
                if content.contains('\r') {
                    let normalized = crate::app::EditorApp::normalize_newlines(content);
                    scan_line_hits(&normalized, q, remaining, &mut hits);
                } else {
                    scan_line_hits(content, q, remaining, &mut hits);
                }
                if hits.is_empty() {
                    continue;
                }
                *total += hits.len();
                let f = FolderFileHit { path, hits };
                let _ = tx.send(FolderScanUpdate::File(f)).await;
            }
        }
    }
    *total >= cap
}

/// ASCII 大小写不敏感子串查找（无分配），返回首个命中的字节区间。
/// 仅当 `needle` 全 ASCII 时走 `memchr` 加速的字节快路径（用 `memchr2` 同时搜
/// 首字节大小写两种形式，命中再逐字节校验余下部分）：UTF-8 多字节序列的内部字节
/// 都 ≥ 0x80，因此纯 ASCII 命中必然落在字符边界上，返回的区间始终合法。
/// 非 ASCII 查询词回退到与原逻辑等价的字符级大小写折叠扫描（保留原语义）。
pub(super) fn find_ci(haystack: &str, needle: &str) -> Option<std::ops::Range<usize>> {
    if needle.is_empty() {
        return None;
    }
    if needle.is_ascii() {
        let h = haystack.as_bytes();
        let n = needle.as_bytes();
        if n.len() > h.len() {
            return None;
        }
        let first = n[0];
        // 同时搜首字节的 ASCII 大小写两种形式（字母大小写差 0x20）
        let first_alt = if first.is_ascii_alphabetic() {
            first ^ 0x20
        } else {
            first
        };
        let mut pos = 0usize;
        while let Some(rel) = memchr::memchr2(first, first_alt, &h[pos..]) {
            let at = pos + rel;
            if at + n.len() <= h.len() {
                let mut ok = true;
                for (j, &nb) in n.iter().enumerate().skip(1) {
                    if h[at + j].to_ascii_lowercase() != nb {
                        ok = false;
                        break;
                    }
                }
                if ok {
                    return Some(at..(at + n.len()));
                }
            }
            pos = at + 1;
        }
        return None;
    }
    // 非 ASCII needle：字符级大小写折叠比较（零分配，语义与旧「取 n 字符 collect 为
    // 整串再 to_lowercase() 与 needle 整串比较」一致：每个候选起点取 needle 同款
    // 字符数，逐字符折叠后与 needle 字符流比对；折叠展开/长度不等即不命中）。
    let needle_chars: Vec<char> = needle.chars().collect();
    let n_len = needle_chars.len();
    if n_len == 0 {
        return None;
    }
    let mut i = 0usize;
    while i < haystack.len() {
        let rest = &haystack[i..];
        let mut ni: usize = 0; // 已比对上的 needle 字符数
        let mut cand_bytes: usize = 0; // 候选在 haystack 中占用的字节数
        let mut hit = true;
        for hc in rest.chars().take(n_len) {
            cand_bytes += hc.len_utf8();
            let folded = hc.to_lowercase();
            for f in folded {
                if ni >= n_len || f != needle_chars[ni] {
                    hit = false;
                    break;
                }
                ni += 1;
            }
            if !hit {
                break;
            }
        }
        if hit && ni == n_len {
            return Some(i..(i + cand_bytes));
        }
        match rest.chars().next() {
            Some(ch) => i += ch.len_utf8(),
            None => break,
        }
    }
    None
}

/// ASCII 大小写不敏感子串是否存在（无分配；语义与旧 `to_lowercase().contains()` 一致）。
pub(super) fn contains_ci(haystack: &str, needle: &str) -> bool {
    find_ci(haystack, needle).is_some()
}

/// 对一份**已 LF 归一化**（CRLF/CR → LF，行区间以此为基）的全文逐行扫查询词，把每行的
/// 首个命中收进 `out`（行号 + 文档字节区间 + trim 片段），到达 `remaining` 上限即停。
///
/// 两条查询语义路径：
/// - **ASCII 查询**（常见路径）：`content.to_ascii_lowercase()` **一次性**建 ASCII 小写全文
///   副本（std 实现；非 ASCII 字节原样保留 → 字节布局与 `\n` 分布不变，命中字节区间可直接
///   映射回原文，UTF-8 不受破坏），然后逐行用 std `str::find`（memmem 快路径）匹配小写后的
///   查询词。语义与旧逐字节 `to_ascii_lowercase` 折叠（[`find_ci`] ASCII 分支）完全等价；
///   同一轮搜索只物化一次副本、逐行借用，**命中前零逐行分配**（唯一逐行分配是命中片段的
///   `line.trim().to_string()`，≤ remaining 次）。基准（0.8MB/万行 debug）：副本 2.5ms +
///   扫描 5.1ms ≈ 8ms，比现 memchr `find_ci`（13.6ms）快 ~1.7×，且对标大小写敏感下限。
/// - **非 ASCII 查询**：回退 [`find_ci`] 字符级大小写折叠（保留原国际化语义，零分配）。
///
/// 行字节区间 = 行首文档偏移 + 行内命中位（`doc_offset` 按 `行长+换行(1)` 累积，与命中
/// 跳转的选中高亮一致）。只读/可编辑两种来源共用此实现，保证命中语义单一。
pub(super) fn scan_line_hits(content: &str, q: &str, remaining: usize, out: &mut Vec<SearchLineHit>) {
    if q.is_empty() || remaining == 0 {
        return;
    }
    let mut doc_offset = 0usize;
    if q.is_ascii() {
        // 一次性小写全文副本（std `to_ascii_lowercase`：仅折 ASCII 字母，长度不变）。
        let lower = content.to_ascii_lowercase();
        for (li, (line, lower_line)) in content.split('\n').zip(lower.split('\n')).enumerate() {
            if out.len() >= remaining {
                break;
            }
            if let Some(pos) = lower_line.find(q) {
                out.push(SearchLineHit {
                    line: li,
                    range: (doc_offset + pos)..(doc_offset + pos + q.len()),
                    snippet: line.trim().to_string(),
                });
            }
            doc_offset += line.len() + 1;
        }
    } else {
        for (li, line) in content.split('\n').enumerate() {
            if out.len() >= remaining {
                break;
            }
            if let Some(r) = find_ci(line, q) {
                out.push(SearchLineHit {
                    line: li,
                    range: (doc_offset + r.start)..(doc_offset + r.end),
                    snippet: line.trim().to_string(),
                });
            }
            doc_offset += line.len() + 1;
        }
    }
}
