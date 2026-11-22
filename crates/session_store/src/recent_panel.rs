//! 「打开最近」居中大面板实体（2026-11-17 改版）：**顶部搜索框 + 全量两组分组列表**。
//! （面板内「清空最近记录」行已于 2026-11-17 移除，改由文件菜单 flyout「清空最近记录」
//! 承担；flyout 在 `crates/shell/src/app/menu.rs` 的 `recent_clear_item`。）
//! Ctrl+P 命令面板同款 Acrylic 居中大卡片（600px 宽、窗口 2/3 高、带滚动
//! 条与弹簧淡入），横居中出现于窗口中间。**默认模式即全量 [`RecentMode::All`]**：
//! 打开即显示全部历史 + 搜索框；搜索框按「名称 / 父路径」大小写不敏感子串即时过滤
//! （不重排、保持 MRU 序）；点击 / 回车打开；保留失效路径移除 + 状态栏提示、键盘 ↑↓
//! （搜索框聚焦时在候选间移动高亮，抄 `ui::palette::CommandPalette` 姿势）/
//! Enter / Esc、外点关闭。
//!
//! # Dormancy：Regions 四区域（≤10 + More / 返回 + Clear）已被「文件菜单『更多...』入口」取代
//! shell 文件菜单「打开最近…」现走 `ui::menu` 的 flyout 语义（悬停展开到右侧）；本面板
//! 是「打开最近…」flyout 新增「更多...」入口（另一批接线）打开的搜索大面板。旧四区域
//! 形态（[`RecentMode::Regions`] 分支 / More / 返回 / Clear 行逻辑）**保留源码 + 既有
//! 测试作 dormancy 回归（K24）**，不再作为可见形态；shell 的 `EditorApp::open_recent_panel`
//! 仍可直连打开面板实体。
//!
//! # 结构与隔离子 crate 的原因
//! 本组件是**独立实体**（`mode` / 选中 / 最近列表快照自持），由宿主
//! 经 `Option<Entity<Self>>` 持有：打开时宿主创建 + 聚焦 + 订阅 `DismissEvent`
//! （择项 / Esc / 点击外 → 宿主清引用、实体随之释放）。
//! 面板对宿主的调用收敛为最小接口 [`RecentPanelHost`]（弱引用 `WeakEntity<H>` 持有，
//! `H: RecentPanelHost`，shell 侧 `impl RecentPanelHost for EditorApp`），本模块不直接
//! 依赖任何 shell 类型——故可整体迁入独立 crate `session_store`。
//! 渲染外壳（Acrylic 卡 + 全窗 occlude 包裹 + 600px 宽 + 窗口 2/3 高 + 内部滚动 +
//! 键盘 ↑↓/Enter/Esc + `with_spring` 淡入）**复制**自 `ui::palette::CommandPalette::render`
//! ——不改核心组件、Ctrl+P 行为零影响；此处注释注明「壳与 CommandPalette 同源」。
//! 四区域内容（分割线分隔的区域行 / More / Clear / 返回）为 RecentPanel 自绘。
//!
//! # 模式切换
//! More / 返回 = 单实体 `mode` 字段切换 + `notify`，无 `PopupMenu::set_items` 舞蹈、
//! 不重开面板、焦点/订阅不变。
//!
//! # 反馈 1：文件菜单保持展开
//! 源菜单「打开最近…」项 `.keep_open()` → `PopupMenu` 点击后**不 dismiss**（DismissEvent
//! 不发），文件菜单保持展开；面板 `deferred` 全窗 occlude 盖在其上（不穿透）。面板
//! 关闭（择项 / Esc / 点击外）也不动文件菜单——文件菜单留到它自身下一次外部点击
//! 才收起（行为表照此写；无句柄不强行程序关菜单）。面板关闭时 `previous_focus`
//! 归还打开前的焦点（= 仍展开的文件菜单句柄）；动作已带走焦点（如打开文件聚焦
//! 新编辑器）则不归还，避免把新编辑器焦点抢回该死句柄（复用 CommandPalette 的
//! `focus_left` 语义，见 `close`）。

use std::path::PathBuf;

use gpui::{
    AnimationExt, App, AppContext, Context, DismissEvent, ElementId, Entity, EventEmitter,
    FocusHandle, Focusable, Hsla, InteractiveElement as _, IntoElement, KeyDownEvent,
    ParentElement as _, Render, ScrollHandle, SharedString, Styled as _, Subscription, WeakEntity,
    Window, div, px,
};
use ui::ListItemRow;
use ui::float_card::{FloatCardBg, FloatCardPad, FloatCardRadius, FloatCardShadow};
use ui::icon::{Icon, IconName};
use ui::input::{Input, InputEvent, InputState};
use ui::motion::hover_spring;
use ui::scroll_panel::ScrollPanelBuilder;

/// 「打开最近」面板的宿主抽象：把面板对宿主的 5 个交互收敛为最小接口，使面板实体
/// （session_store）不依赖 shell 的具体宿主类型。shell 侧以 `impl RecentPanelHost for
/// EditorApp` 提供实现（方法转发到既有 `pub(super)` 同名方法，行为完全不变）。
///
/// 方法均带 `where Self: Sized`：对 `RecentPanel<H>` 的**泛型宿主**（`H = EditorApp`，
/// 必然 Sized）用法无任何限制；此约束只因实现需传入 `Context<Self>`（gpui 的
/// `Context<T>` 要求 `T: Sized`）并保持「方法即转发、不新增逻辑」的薄接线。
pub trait RecentPanelHost: 'static {
    /// 打开文件夹（`is_dir()` 命中 → 目录/文件树换根）。
    fn open_folder_path(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>)
    where
        Self: Sized;
    /// 移除失效（路径已不存在）的最近文件夹 + 落盘 + 状态栏提示。
    fn remove_recent_folder(&mut self, path: &str, window: &mut Window, cx: &mut Context<Self>)
    where
        Self: Sized;
    /// 打开文件（建 tab / 媒体重定向等中心入口）。
    fn open_file(&mut self, path: &str, window: &mut Window, cx: &mut Context<Self>)
    where
        Self: Sized;
    /// 移除失效（路径已不存在）的最近文件 + 落盘 + 状态栏提示。
    fn remove_recent_file(&mut self, path: &str, window: &mut Window, cx: &mut Context<Self>)
    where
        Self: Sized;
    /// 清空最近文件夹 / 文件两类列表并落盘 + notify（文件菜单 flyout「清空最近记录」项）。
    fn clear_recents(&mut self, cx: &mut Context<Self>)
    where
        Self: Sized;
}

/// 面板模式（More/返回 在同一实体上切换）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RecentMode {
    /// 四区域：文件夹区（≤10 行）→ 文件区（≤10 行）→ More → Clear，区域间分割线隔开。
    Regions,
    /// 完整列表：返回 → 全部文件夹（≤[`config::limits::RECENT_MAX`]）→ 全部文件。
    All,
}

/// 面板行（每次渲染按打开时的最近列表快照重建；区域头 / 空态不可导航）。
enum Line {
    /// 区域间分割线（四区域以分割线隔开、不设文字区域头标识；不可导航）。
    Divider { debug: &'static str },
    /// 空区域「暂无」占位（不可导航）。
    Empty { text: &'static str, debug: &'static str },
    /// 文件夹 / 文件行。
    Item { is_folder: bool, path: String, name: String, debug: String },
    /// 分组头「最近文件夹」「最近文件」（含条数；不可导航）。
    Header { is_folder: bool, count: usize },
    /// 「More...」：切到完整列表模式（不关闭面板）。
    More,
    /// 「返回」：完整列表切回四区域（不关闭面板）。
    ///
    /// Dormancy：默认模式已为全量（`RecentMode::All`）且全量形态不再渲染返回行，
    /// 本变体保留源码（`activate_line` 的 More/返回 分支仍接好）作回归，不再被构造。
    #[allow(dead_code)]
    Back,
    /// 「Clear Recently Opened...」：清两类 + 落盘 + 关闭面板。
    ///
    /// Dormancy：全量（All）面板可见行已不含清空行（2026-11-17 用户要求：面板内
    /// 清空功能多余，已由文件菜单 flyout「清空最近记录」承担）；本变体仍被
    /// [`Self::build_regions_lines`]（dormancy 回归形态）引用，故保留源码与激活分支。
    Clear,
}

impl Line {
    /// 是否可导航（键盘 ↑↓ / 鼠标可选中、可执行）。
    fn is_navigable(&self) -> bool {
        matches!(self, Line::Item { .. } | Line::More | Line::Back | Line::Clear)
    }

    /// 调试定位选择器（`debug_bounds` 靠它收录；测试与新 shell 面板共用这些名）。
    fn debug(&self) -> &str {
        match self {
            Line::Divider { debug } => debug,
            Line::Empty { debug, .. } => debug,
            Line::Item { debug, .. } => debug,
            Line::Header { is_folder, .. } => {
                if *is_folder {
                    "recent-folder-header"
                } else {
                    "recent-file-header"
                }
            }
            Line::More => "recent-more",
            Line::Back => "recent-back",
            Line::Clear => "recent-clear",
        }
    }
}

/// 「打开最近」居中大面板（独立实体，宿主持有 `Option<Entity<Self>>`）。
/// 泛型参数 `H` = 宿主实体类型（`impl RecentPanelHost`），经弱引用反调（不依赖宿主类型）。
pub struct RecentPanel<H: RecentPanelHost> {
    mode: RecentMode,
    /// 打开面板时的最近列表快照（面板打开期间被别处更新 → 下次打开才反映最新）。
    folders: Vec<String>,
    files: Vec<String>,
    /// 打开面板前的焦点句柄（关闭时归还；动作已带走焦点则不归还）。
    previous_focus: Option<FocusHandle>,
    focus_handle: FocusHandle,
    /// 列表滚动句柄：`#recent-panel-scroll`（`overflow_y_scroll` + `track_scroll`）
    /// 跟踪它，右侧 `Scrollbar::vertical` 自绘跟随（宿主原生滚动条此 GPUI 不绘制）。
    scroll_handle: ScrollHandle,
    /// 宿主（`H: RecentPanelHost`）弱引用：行点击执行打开/移除/清空。
    host: WeakEntity<H>,
    /// 选中行（`lines` 内下标；区域头 / 空态不可选）。
    selected: Option<usize>,
    /// 可导航行下标（每帧渲染重建，键盘 ↑↓ 用它移动/环绕）。
    navigable: Vec<usize>,
    /// 顶部搜索框实体（`ui::input::InputState`；懒初始化于首次 render，见
    /// [`RecentPanel::ensure_input`]——`RecentPanel::new` 无 `&mut Window` 参数，
    /// 而 `InputState::new` 需 window）。
    input: Option<Entity<InputState>>,
    /// 搜索框事件订阅（Change → 重算过滤；Submit → 打开高亮 / 首个候选；Cancel → 关闭）。
    _input_sub: Option<Subscription>,
    /// 最近一次搜索词（输入框文本镜像，`build_lines` 按其过滤；Change 时更新）。
    query: String,
}

/// 面板常规行高（与 PopupMenu 项同款 26px；四区域默认视图在窗口 2/3 高内容区内
/// 容纳 More/Clear，无需滚动即可点）。
const ITEM_H: f32 = 26.;

/// 搜索命中谓词（R4）：`query` 为空 → 全命中；否则对「名称（basename）+ 父路径
/// （完整路径）」做**大小写不敏感子串**匹配（中英文均可）。命中**不重排**、保持 MRU
/// 相对顺序。与 `ui::palette::fuzzy_score` 的「名称优先、路径也能命中、大小写不敏感」
/// 语义同源，但本处按 R4 定为子串匹配（不做前缀 / 子序列打分）。
fn matches_query(query: &str, path: &str) -> bool {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return true;
    }
    let name = std::path::Path::new(path)
        .file_name()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    name.contains(q.as_str()) || path.to_lowercase().contains(q.as_str())
}

/// 按 `query` 过滤并分组（保留 MRU 相对顺序）：返回 (可见文件夹, 可见文件)。
/// 供 [`RecentPanel::build_all_lines`] 与纯函数单测共用（R8：过滤 / 分组计算）。
fn filtered_groups<'a>(
    folders: &'a [String],
    files: &'a [String],
    query: &str,
) -> (Vec<&'a str>, Vec<&'a str>) {
    let visible_folders: Vec<&str> = folders
        .iter()
        .map(String::as_str)
        .filter(|p| matches_query(query, p))
        .collect();
    let visible_files: Vec<&str> = files
        .iter()
        .map(String::as_str)
        .filter(|p| matches_query(query, p))
        .collect();
    (visible_folders, visible_files)
}

impl<H: RecentPanelHost> RecentPanel<H> {
    /// 宿主打开时创建：最近列表快照 + 打开前的焦点句柄 + 宿主弱引用。
    pub fn new(
        host: WeakEntity<H>,
        folders: Vec<String>,
        files: Vec<String>,
        previous_focus: Option<FocusHandle>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            // 默认模式 = 全量（R2）：打开即显示全部历史 + 搜索框；Regions 四区域 /
            // More 返回只保留 dormancy（不再作为可见形态）。
            mode: RecentMode::All,
            folders,
            files,
            previous_focus,
            focus_handle: cx.focus_handle(),
            scroll_handle: ScrollHandle::new(),
            host,
            selected: None,
            navigable: Vec::new(),
            input: None,
            _input_sub: None,
            query: String::new(),
        }
    }

    /// 行装配：默认（All）= 搜索过滤 + 两组分组（无底部清空，2026-11-17 移除至 flyout）；Regions
    /// （dormancy）保留旧四区域形态（见 [`Self::build_regions_lines`]）。
    fn build_lines(&self) -> Vec<Line> {
        if self.mode == RecentMode::All {
            self.build_all_lines()
        } else {
            self.build_regions_lines()
        }
    }

    /// 全量 + 搜索分组形态：按 `query` 过滤后组装「最近文件夹」「最近文件」两组
    /// （组头含条数、组内保 MRU 序不重排、0 命中组整体隐藏）；两组全 0（含搜索后
    /// 全 0 命中）→ 单一空态「暂无」。不再含清空行（2026-11-17 用户要求：面板内
    /// 「清空最近记录」行多余，交回文件菜单 flyout）。
    fn build_all_lines(&self) -> Vec<Line> {
        let mut lines = Vec::new();
        let (visible_folders, visible_files) = filtered_groups(&self.folders, &self.files, &self.query);
        if visible_folders.is_empty() && visible_files.is_empty() {
            lines.push(Line::Empty { text: "暂无", debug: "recent-file-empty" });
        } else {
            if !visible_folders.is_empty() {
                lines.push(Line::Header { is_folder: true, count: visible_folders.len() });
                for (i, p) in visible_folders.iter().enumerate() {
                    lines.push(Self::item_line(true, p, i));
                }
            }
            if !visible_files.is_empty() {
                lines.push(Line::Header { is_folder: false, count: visible_files.len() });
                for (i, p) in visible_files.iter().enumerate() {
                    lines.push(Self::item_line(false, p, i));
                }
            }
        }
        lines
    }

    /// Dormancy：旧「四区域」形态（文件夹 ≤10 → 分割线 → 文件 ≤10 → 分割线 →
    /// More → 分割线 → Clear；空区「暂无」）。已被文件菜单「更多...」入口取代，
    /// 默认模式恒为 All，本分支不再可见（保留源码 + 旧测试作 dormancy 回归，K24）。
    fn build_regions_lines(&self) -> Vec<Line> {
        let mut lines = Vec::new();
        // 区域 1：最近打开的文件夹（图标 Folder，≤10）
        let folder_count = self.folders.len().min(10);
        if folder_count == 0 {
            lines.push(Line::Empty { text: "暂无", debug: "recent-folder-empty" });
        } else {
            for i in 0..folder_count {
                lines.push(Self::item_line(true, &self.folders[i], i));
            }
        }
        lines.push(Line::Divider { debug: "recent-divider-1" });
        // 区域 2：最近打开的文件（图标 File，≤10）
        let file_count = self.files.len().min(10);
        if file_count == 0 {
            lines.push(Line::Empty { text: "暂无", debug: "recent-file-empty" });
        } else {
            for i in 0..file_count {
                lines.push(Self::item_line(false, &self.files[i], i));
            }
        }
        // 区域 3 / 4：More 与 Clear，均以分割线与上方隔开
        lines.push(Line::Divider { debug: "recent-divider-2" });
        lines.push(Line::More);
        lines.push(Line::Divider { debug: "recent-divider-3" });
        lines.push(Line::Clear);
        lines
    }

    /// 单行（文件夹/文件）：图标 + 名称（截断）+ 路径灰字（截断）。
    fn item_line(is_folder: bool, path: &str, idx: usize) -> Line {
        let name = std::path::Path::new(path)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string());
        let prefix = if is_folder { "recent-folder-row-" } else { "recent-file-row-" };
        Line::Item {
            is_folder,
            path: path.to_string(),
            name,
            debug: format!("{prefix}{idx}"),
        }
    }

    /// 懒创建顶部搜索框实体 + 订阅事件（首次 render 调用）：
    /// `RecentPanel::new` 无 `&mut Window` 参数，而 `InputState::new` 需 window → 移到
    /// 首次 render（有 window）再建。Change → 更新 `query` + 复位选中（Enter 无高亮时
    /// 走「第一个候选」）；Submit → 打开选中 / 无选中则打开第一个候选；Cancel → 关闭
    /// 面板。订阅姿势（输入框聚焦时 ↑↓ 移动高亮 / Enter 接受 / Esc 关闭）与
    /// `ui::palette::CommandPalette::new` 完全一致。
    fn ensure_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.input.is_some() {
            return;
        }
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("搜索历史…"));
        let input_entity = input.clone();
        let weak_self = cx.weak_entity();
        let sub = window.subscribe(&input, cx, move |_, ev: &InputEvent, window, cx| {
            let Some(this) = weak_self.upgrade() else {
                return;
            };
            this.update(cx, |this, cx| match ev {
                InputEvent::Change => {
                    this.query = input_entity.read(cx).value().to_string();
                    this.selected = None;
                    cx.notify();
                }
                InputEvent::Submit { .. } => {
                    if let Some(idx) = this.selected {
                        this.activate_line(idx, window, cx);
                    } else if let Some(idx) = this.first_candidate() {
                        this.activate_line(idx, window, cx);
                    }
                }
                InputEvent::Cancel => this.dismiss(window, cx),
            });
        });
        self.input = Some(input);
        self._input_sub = Some(sub);
        // 一次性把焦点拨给顶部搜索框（对齐 palette 姿势：宿主 `open_palette` 打开时直接
        // `window.focus(&input_handle)`，见 shell/app/palette.rs；本面板宿主
        // `open_recent_panel` 只聚焦面板根 `focus_handle`（menu.rs:780-781），故在首次
        // 创建搜索框后补一次 focus）。**只此一次**：之后不再逐帧拉回——旧逐帧
        // `sync_search_focus` 会在「mousedown 落行 → 面板根 `track_focus` 抢走输入框焦点
        //（gpui div.rs：可聚焦元素 mousedown bubble 自动转移焦点；本面板根挂
        // `.track_focus(&self.focus_handle)`) → 当帧 render 又把焦点拨回输入框」的路径上
        // 反复抢焦点（搜索框闪烁一瞬再打开）；palette 无此逐帧循环故不闪。经
        // `window.defer` 推迟到本帧 effect 末尾再 focus，避免渲染中途改焦点。
        let handle = self.input.as_ref().expect("搜索框已创建").read(cx).focus_handle(cx);
        window.defer(cx, move |window, cx| handle.focus(window, cx));
    }

    /// 第一个可导航候选行下标（Enter 无高亮时打开它；0 命中 → None）。
    fn first_candidate(&self) -> Option<usize> {
        self.build_lines().iter().position(|l| l.is_navigable())
    }

    /// 键盘 ↑↓ 在可导航行内移动（环绕）。
    fn select_step(&mut self, delta: isize, cx: &mut Context<Self>) {
        let n = self.navigable.len();
        if n == 0 {
            self.selected = None;
            return;
        }
        let cur = self
            .selected
            .and_then(|s| self.navigable.iter().position(|&i| i == s));
        let next = match cur {
            Some(p) => (p as isize + delta).rem_euclid(n as isize) as usize,
            None if delta > 0 => 0,
            None => n - 1,
        };
        self.selected = Some(self.navigable[next]);
        cx.notify();
    }

    /// 执行第 `line_index` 行（键盘 Enter / 鼠标点击共用）。
    /// More/返回 → 单实体切模式 + notify（不关面板）；Item/Clear → 走宿主 + 关闭面板。
    fn activate_line(&mut self, line_index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = Some(line_index);
        let lines = self.build_lines();
        let Some(line) = lines.get(line_index) else {
            return;
        };
        match line {
            Line::Item { is_folder, path, .. } => {
                self.run_item(*is_folder, path, window, cx);
                self.close(window, cx);
            }
            Line::More => {
                self.mode = RecentMode::All;
                self.selected = None;
                cx.notify();
            }
            Line::Back => {
                self.mode = RecentMode::Regions;
                self.selected = None;
                cx.notify();
            }
            Line::Clear => {
                if let Some(host) = self.host.upgrade() {
                    host.update(cx, |app, cx| {
                        app.clear_recents(cx);
                    });
                }
                self.close(window, cx);
            }
            _ => {}
        }
    }

    /// 行点击执行：文件夹存在 → `open_folder_path`；文件存在 → `open_file`；
    /// 路径失效 → 从对应列表移除 + 持久化 + 状态栏提示（`remove_recent_folder/file`）。
    fn run_item(&mut self, is_folder: bool, path: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(host) = self.host.upgrade() else {
            return;
        };
        if is_folder {
            if std::path::Path::new(path).is_dir() {
                let p = std::path::PathBuf::from(path);
                host.update(cx, |app, cx| app.open_folder_path(p, window, cx));
            } else {
                let p = path.to_string();
                host.update(cx, |app, cx| app.remove_recent_folder(&p, window, cx));
            }
        } else if std::path::Path::new(path).is_file() {
            let p = path.to_string();
            host.update(cx, |app, cx| app.open_file(&p, window, cx));
        } else {
            let p = path.to_string();
            host.update(cx, |app, cx| app.remove_recent_file(&p, window, cx));
        }
    }

    /// 焦点是否已离开面板（动作 —— 如打开文件聚焦新编辑器 —— 已带走焦点）。
    /// 面板根与顶部搜索框任一持焦都算「焦点仍在面板内」（对齐 palette 的
    /// `focus_left`：两 handle 都检查，打开时焦点落在搜索框上）。
    fn focus_left(&self, window: &Window, cx: &App) -> bool {
        let Some(f) = window.focused(cx) else {
            return false;
        };
        let input_focused = self
            .input
            .as_ref()
            .is_some_and(|i| i.read(cx).focus_handle(cx) == f);
        !(f == self.focus_handle || input_focused)
    }

    /// 关闭面板（择项 / Esc / 点击外）：发 `DismissEvent` → 宿主清引用释放实体；
    /// 焦点未被动作带走才归还打开前的焦点（保持文件菜单等来源控件的焦点）。
    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let focus_left = self.focus_left(window, cx);
        self.selected = None;
        cx.stop_propagation();
        window.prevent_default();
        cx.emit(DismissEvent);
        if !focus_left
            && let Some(handle) = self.previous_focus.take()
        {
            window.focus(&handle, cx);
        }
    }

    /// Esc / 点击面板外（宿主 toggle 关闭也走此路径）。
    pub(crate) fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close(window, cx);
    }

    fn on_key_down(&mut self, e: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        match e.keystroke.key.as_ref() {
            "up" => {
                cx.stop_propagation();
                self.select_step(-1, cx);
            }
            "down" => {
                cx.stop_propagation();
                self.select_step(1, cx);
            }
            "enter" => {
                if let Some(s) = self.selected {
                    cx.stop_propagation();
                    window.prevent_default();
                    self.activate_line(s, window, cx);
                }
            }
            "escape" => {
                cx.stop_propagation();
                self.dismiss(window, cx);
            }
            _ => {}
        }
    }

    fn on_mouse_down_out(
        &mut self,
        _e: &gpui::MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.dismiss(window, cx);
    }

    /// 渲染一行：区域头 / 空态为静态文字；可导航行带 hover 选中 + 弹簧高亮淡入。
    fn render_line(
        &self,
        index: usize,
        line: &Line,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = ui::theme::Theme::global(cx);
        let fg: Hsla = theme.colors.fg.into();
        let fg_muted: Hsla = theme.colors.fg_muted.into();
        let selected_bg: Hsla = theme.chrome.menu_highlight.into();

        let debug = line.debug().to_string();
        // 行壳复用 ui::ListItemRow + row_height 参数（本批 Pattern 10）：可导航行
        // （Item / More / 返回 / Clear）用 `ui::ListItemRow` 组装行壳（flex + items_center
        // + gap_2 + px_3 + mx_1 + min_h(26) + rounded_md + 弹簧选中背景 + 手型光标；
        // hover→`on_hover_select` 上抛、点击→`on_activate`，与 palette 模板逐语义等价）。
        // ElementId 保持 `recent-line-{index}`；debug_selector 经 override 保持
        // `line.debug()` 原行名；spring 键改走派生态 `{id}-bg`——原 `recent-item-bg-{index}`
        // 键差异无测试锁定，可接受。空态 / 分割线仍为静态绘制。

        match line {
            Line::Divider { .. } => {
                let border: Hsla = theme.colors.border.into();
                let d = line.debug().to_string();
                div()
                    .id(ElementId::from(format!("recent-line-{index}")))
                    .debug_selector(move || d.clone())
                    .mx_2()
                    .my_1()
                    .h(px(1.))
                    .bg(border.opacity(0.6))
                    .into_any_element()
            }
            Line::Empty { text, .. } => {
                let t: &'static str = text;
                div()
                    .id(ElementId::from(format!("recent-line-{index}")))
                    .debug_selector(move || debug.clone())
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .mx_1()
                    .min_h(px(ITEM_H))
                    .rounded_md()
                    .text_size(px(12.))
                    .text_color(fg_muted)
                    .child(t)
                    .into_any_element()
            }
            Line::Header { is_folder, count } => {
                let title = if *is_folder {
                    format!("最近文件夹 ({count})")
                } else {
                    format!("最近文件 ({count})")
                };
                div()
                    .id(ElementId::from(format!("recent-line-{index}")))
                    .debug_selector(move || debug.clone())
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .mx_1()
                    .min_h(px(ITEM_H))
                    .rounded_md()
                    .text_size(px(11.))
                    .text_color(fg_muted)
                    .child(title)
                    .into_any_element()
            }
            Line::Item { is_folder, path, name, .. } => {
                let is_folder = *is_folder;
                let icon = if is_folder { IconName::Folder } else { IconName::File };
                let path = path.clone();
                let name = name.clone();
                let row_debug = debug.clone();
                let weak_hover = cx.weak_entity();
                let weak_activate = cx.weak_entity();
                ListItemRow::new(ElementId::from(format!("recent-line-{index}")))
                    .row_height(ITEM_H)
                    .selected(selected)
                    .accent(selected_bg)
                    .debug_selector(move || row_debug.clone())
                    .content(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .flex_1()
                            .min_w(px(0.))
                            .child(Icon::new(icon).size(px(16.)).text_color(fg))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_overflow(gpui::TextOverflow::Truncate("…".into()))
                                    .text_color(fg)
                                    .child(SharedString::from(name)),
                            )
                            .child(
                                div()
                                    .max_w(px(180.))
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_overflow(gpui::TextOverflow::Truncate("…".into()))
                                    .text_size(px(11.))
                                    .text_color(fg_muted)
                                    .child(SharedString::from(path)),
                            ),
                    )
                    .on_hover_select(move |hovered, _window, cx| {
                        let _ = weak_hover.update(cx, |this, cx| {
                            if hovered {
                                this.selected = Some(index);
                            } else if this.selected == Some(index) {
                                this.selected = None;
                            }
                            cx.notify();
                        });
                    })
                    .on_activate(move |_event, window, cx| {
                        let _ = weak_activate.update(cx, |this, cx| {
                            this.selected = Some(index);
                            this.activate_line(index, window, cx);
                        });
                    })
                    .into_any_element()
            }
            Line::More | Line::Back | Line::Clear => {
                let label: &'static str = match line {
                    Line::More => "More...",
                    Line::Back => "返回",
                    Line::Clear => "Clear Recently Opened...",
                    _ => unreachable!(),
                };
                let row_debug = debug.clone();
                let weak_hover = cx.weak_entity();
                let weak_activate = cx.weak_entity();
                ListItemRow::new(ElementId::from(format!("recent-line-{index}")))
                    .row_height(ITEM_H)
                    .selected(selected)
                    .accent(selected_bg)
                    .debug_selector(move || row_debug.clone())
                    .content(div().text_color(fg).child(label))
                    .on_hover_select(move |hovered, _window, cx| {
                        let _ = weak_hover.update(cx, |this, cx| {
                            if hovered {
                                this.selected = Some(index);
                            } else if this.selected == Some(index) {
                                this.selected = None;
                            }
                            cx.notify();
                        });
                    })
                    .on_activate(move |_event, window, cx| {
                        let _ = weak_activate.update(cx, |this, cx| {
                            this.selected = Some(index);
                            this.activate_line(index, window, cx);
                        });
                    })
                    .into_any_element()
            }
        }
    }

    // -- 测试可见性 --
    // `pub`：shell 的 `app::tests` 跨 crate 断言用（mode 切换 / 滚动句柄 / 搜索框驱动）。

    pub fn mode(&self) -> RecentMode {
        self.mode
    }

    pub fn scroll_handle(&self) -> ScrollHandle {
        self.scroll_handle.clone()
    }

    /// 顶部搜索框实体（测试驱动过滤：`set_value` 发 `Change` → 面板重算；对齐
    /// `ui::palette::CommandPalette::input_entity`）。
    #[allow(dead_code)] // 测试驱动入口（shell app::tests 用）
    pub fn input_entity(&self) -> Option<Entity<InputState>> {
        self.input.clone()
    }

    /// 当前搜索词（测试断言过滤状态用）。
    #[allow(dead_code)] // 测试断言用（shell app::tests）
    pub fn query(&self) -> &str {
        &self.query
    }
}

impl<H: RecentPanelHost> EventEmitter<DismissEvent> for RecentPanel<H> {}

impl<H: RecentPanelHost> Focusable for RecentPanel<H> {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl<H: RecentPanelHost> Render for RecentPanel<H> {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 壳与 `ui::palette::CommandPalette::render` 同源（复制，非跨模块重构）：
        // Acrylic 卡 + 双层 shadow + 600px 宽 + 窗口 2/3 高 + 内部滚动 + 全窗 occlude
        // 包裹（非 flex absolute）居中 + `with_spring` 弹簧淡入 + 键盘/点外关闭。
        let theme = ui::theme::Theme::global(cx);
        let (colors, is_dark) = (theme.colors, theme.is_dark());

        // 懒建顶部搜索框 + 订阅（首次 render；创建后一次性把焦点拨给搜索框——palette 姿势，
        // 见 `ensure_input` 尾部注释。无逐帧焦点同步：避免 mousedown 落行被面板根抢焦点后
        // 又被拉回搜索框的闪烁路径）。
        self.ensure_input(window, cx);

        // 面板高度 = 窗口 2/3：完整列表（≤50+50 行）超高时列表区内部滚动。
        let panel_h = window.bounds().size.height.as_f32() * (2.0 / 3.0);
        // 列表滚动句柄（跨帧稳定）：`#recent-panel-scroll` 跟踪它，右侧
        // `Scrollbar::vertical` 自绘跟随。
        let scroll_handle = self.scroll_handle.clone();

        // 行装配 + 可导航序（键盘 ↑↓ 在 `navigable` 内移动）。
        let lines = self.build_lines();
        let navigable: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.is_navigable())
            .map(|(i, _)| i)
            .collect();
        self.navigable = navigable;
        if let Some(s) = self.selected
            && s >= lines.len()
        {
            self.selected = None;
        }
        let selected = self.selected;

        let rows: Vec<gpui::AnyElement> = lines
            .iter()
            .enumerate()
            .map(|(i, line)| self.render_line(i, line, selected == Some(i), cx))
            .collect();

        // 顶部搜索框：聚焦时边框点亮为主题 focus_ring（对齐 palette 的 query_box 做法，`#recent-search-input`）。
        let input_handle = self.input.as_ref().map(|i| i.read(cx).focus_handle(cx));
        let input_border: Hsla = match &input_handle {
            Some(h) => ui::focus_border_color(window, h, colors.border.into(), cx),
            None => colors.border.into(),
        };
        let query_box = div()
            .id("recent-search-input")
            .debug_selector(|| "recent-search-input".into())
            .px_3()
            .py_2()
            .mx_3()
            .mt_2()
            .mb_1()
            .rounded_md()
            .border_1()
            .border_color(input_border)
            .child(Input::new(
                self.input.as_ref().expect("搜索框已在 ensure_input 懒建"),
            ));

        let panel = ui::float_card(
            &colors,
            is_dark,
            FloatCardBg::Acrylic,
            FloatCardShadow::Double,
            FloatCardRadius::Lg,
            FloatCardPad::None,
        )
        .id("recent-panel")
        .debug_selector(|| "recent-panel".into())
        .track_focus(&self.focus_handle)
        .on_key_down(cx.listener(Self::on_key_down))
        .on_mouse_down_out(cx.listener(Self::on_mouse_down_out))
        .flex()
        .flex_col()
        .w(px(600.))
        .h(px(panel_h))
        // 打开淡入（尊重 reduce_motion：`with_spring` 自动落端点不调度帧）。
        .with_spring(
                "recent-panel-fade",
                hover_spring(1.0).from(0.0),
                |this, v| this.opacity(v),
            )
            .child(query_box)
            .child(
                ScrollPanelBuilder::new("recent-panel-scroll", scroll_handle)
                    .wrap_id("recent-panel-list")
                    .bar_id("recent-panel-vscrollbar")
                    .container_style(|d| d.size_full().p(px(4.)))
                    .build(div().flex().flex_col().gap_y(px(1.)).children(rows)),
            );

        // 合成层纪律（同 CommandPalette，见其 render 末尾注释）：整层 deferred 独立
        // 绘制 + 全窗包裹做水平居中，面板自身挂 on_mouse_down_out（点击面板外关闭）。
        // ① 全窗包裹必须是 absolute 的**非 flex** 容器（flex 直子 hitbox 失真 → 点击
        //    全穿透到下层）；② 全窗包裹要 `occlude()`（BlockMouse）——否则点击仍会
        //    继续命中下层编辑器/仍展开的文件菜单的 hitbox。
        let win_w = window.bounds().size.width.as_f32();
        ui::float_card::overlay_centered(panel.into_any_element(), win_w, 600., 96., 210)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Modifiers, MouseButton, TestAppContext};
    use std::{cell::{Cell, RefCell}, rc::Rc};

    /// 过滤谓词（R4）：大小写不敏感子串（含名称与父路径命中）、空串 / 纯空白 = 全量、
    /// 中英文均可、不命中剔除。
    #[test]
    fn matches_query_rules() {
        // 名称命中（子串、大小写不敏感）
        assert!(matches_query("read", r"C:\Docs\readme.md"));
        assert!(matches_query("README", r"C:\Docs\readme.md"));
        // 父路径命中（大小写不敏感子串）
        assert!(matches_query("docs", r"C:\Docs\readme.md"));
        assert!(matches_query("c:\\docs", r"C:\Docs\readme.md"));
        // 中文命中（名称 / 父路径）
        assert!(matches_query("说明", r"C:\Docs\项目说明.md"));
        assert!(matches_query("项目", r"C:\Docs\项目说明.md"));
        // 空串 / 纯空白 → 全命中
        assert!(matches_query("", r"C:\anything.txt"));
        assert!(matches_query("   ", r"C:\anything.txt"));
        // 不命中
        assert!(!matches_query("nope", r"C:\Docs\readme.md"));
        assert!(!matches_query("readme.md2", r"C:\Docs\readme.md"));
    }

    /// 分组计算（R4/R3）：空串 → 全量两组保 MRU 序；搜索 → 0 命中组整体隐藏；
    /// 路径子串命中保序；大小写不敏感。
    #[test]
    fn filtered_groups_preserve_order_and_hide_miss_groups() {
        let folders = vec!["C:\\a\\dirA".to_string(), "C:\\b\\dirB".to_string()];
        let files = vec![
            "C:\\x\\alpha.txt".to_string(),
            "C:\\y\\beta.md".to_string(),
        ];
        let (fs, fls) = filtered_groups(&folders, &files, "");
        assert_eq!(fs, vec!["C:\\a\\dirA", "C:\\b\\dirB"], "全量文件夹保 MRU 序");
        assert_eq!(fls, vec!["C:\\x\\alpha.txt", "C:\\y\\beta.md"], "全量文件保 MRU 序");

        // 仅文件组命中 → 文件夹组空（R4：0 命中组隐藏）
        let (fs, fls) = filtered_groups(&folders, &files, "beta");
        assert!(fs.is_empty(), "0 命中文件夹组应隐藏");
        assert_eq!(fls, vec!["C:\\y\\beta.md"]);

        // 名称子串命中、命中组内保相对序
        let (fs, fls) = filtered_groups(&folders, &files, "dir");
        assert_eq!(fs, vec!["C:\\a\\dirA", "C:\\b\\dirB"], "组内保 MRU 相对顺序");
        assert!(fls.is_empty());

        // 路径（父路径）子串命中
        let (fs, _) = filtered_groups(&folders, &files, "dirb");
        assert_eq!(fs, vec!["C:\\b\\dirB"], "父路径子串命中");

        // 大小写不敏感
        let (fs, _) = filtered_groups(&folders, &files, "DIRA");
        assert_eq!(fs, vec!["C:\\a\\dirA"]);
    }

    // ------------------------------------------------------------------
    // 行为级回归：mousedown 落行「搜索框闪烁」（宿主 open_recent_panel 只聚焦面板根，
    // 旧逐帧 sync_search_focus 在 mousedown 抢焦点路径上把焦点拨回输入框）。
    // 期望值推导（K177）：mousedown 落可聚焦元素 → gpui div.rs 的 track_focus mousedown
    // bubble handler 自动把焦点转移到该元素（面板根）→ 输入框失焦；旧实现当帧 render 的
    // sync_search_focus 又 defer 拨回输入框 = 闪烁。修复后只在打开时一次性聚焦搜索框，
    // 此后不再拉回 → mousedown 落行期间输入框应保持「未聚焦」。
    // ------------------------------------------------------------------

    /// 行为级测试宿主（mirror shell 里 `impl RecentPanelHost` 最小面）：记录打开调用 +
    /// DismissEvent 订阅计数（面板「关闭」= close 发 DismissEvent，实体由窗口根持有不释放）。
    #[derive(Default)]
    struct TestHost {
        opened: Rc<RefCell<Vec<String>>>,
        dismissed: Rc<Cell<bool>>,
    }

    impl RecentPanelHost for TestHost {
        fn open_folder_path(
            &mut self,
            path: PathBuf,
            _window: &mut Window,
            _cx: &mut Context<Self>,
        ) {
            self.opened.borrow_mut().push(format!("d:{}", path.display()));
        }

        fn remove_recent_folder(&mut self, _path: &str, _w: &mut Window, _c: &mut Context<Self>) {}

        fn open_file(&mut self, path: &str, _window: &mut Window, _cx: &mut Context<Self>) {
            self.opened.borrow_mut().push(format!("f:{path}"));
        }

        fn remove_recent_file(&mut self, _path: &str, _w: &mut Window, _c: &mut Context<Self>) {}

        fn clear_recents(&mut self, _cx: &mut Context<Self>) {}
    }

    /// 泵一帧：draw + flush 所有 effect（defer 落点 / 弹簧 / 焦点变化）。
    fn pump_frame(cx: &mut gpui::VisualTestContext) {
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.run_until_parked();
    }

    /// R1 复现路径行为测试：面板开→搜索框聚焦→mousedown 落文件行→泵帧，「输入框不再
    /// 被重新聚焦」；mouseup → 打开文件 + 关闭（DismissEvent）。
    #[gpui::test]
    fn mousedown_on_row_does_not_refocus_search_input(cx: &mut TestAppContext) {
        // 真实文件：run_item 的 `is_file()` 分支命中（打开成功才记录）。
        let f = std::env::temp_dir().join(format!("recent-panel-flicker-{}.txt", std::process::id()));
        std::fs::write(&f, "data").unwrap();
        let fs = f.to_string_lossy().into_owned();

        let host = cx.new(|_| TestHost::default());
        let fs_for_panel = fs.clone();
        let (panel, cx) = cx.add_window_view({
            let host = host.clone();
            move |_window, cx| {
                RecentPanel::new(host.downgrade(), Vec::new(), vec![fs_for_panel], None, cx)
            }
        });
        // 订阅 DismissEvent（关闭信号）；Subscription 存 `_sub` 保持存活到测试结束。
        let _sub = cx.update(|_window, cx| {
            host.update(cx, |_, cx| {
                cx.subscribe(&panel, move |h, _entity, _: &DismissEvent, _| {
                    h.dismissed.set(true)
                })
            })
        });

        // R2 保留：打开后搜索框应自动持焦（面板创建后的首次 render 懒建搜索框 + 一次性
        // 聚焦；与 palette 宿主直接聚焦 input_handle 同源）。泵一帧让 defer 落点。
        pump_frame(cx);
        let focused_on_open = cx.update(|window, cx| {
            let input = panel.read(cx).input_entity().expect("搜索框应已懒建");
            input.read(cx).focus_handle(cx).is_focused(window)
        });
        assert!(focused_on_open, "打开面板后搜索框应自动持焦（R2）");

        // R1：mousedown 落行 → 泵帧 → 输入框**不再**被重新聚焦（旧逐帧 sync_search_focus
        // 会在此 defer 拨回 = 闪烁路径；修复后一次性聚焦已消费，不再拉回）。
        let row = cx
            .debug_bounds("recent-file-row-0")
            .expect("文件行应渲染")
            .center();
        cx.simulate_mouse_down(row, MouseButton::Left, Modifiers::default());
        pump_frame(cx);
        let focused_after_mdown = cx.update(|window, cx| {
            let input = panel.read(cx).input_entity().expect("搜索框应在场");
            input.read(cx).focus_handle(cx).is_focused(window)
        });
        assert!(
            !focused_after_mdown,
            "mousedown 落行期间输入框不应被重新聚焦（闪烁路径已除）"
        );

        // mouseup → on_activate → activate_line → run_item 打开文件 + close（DismissEvent）。
        cx.simulate_mouse_up(row, MouseButton::Left, Modifiers::default());
        pump_frame(cx);
        let (opened, dismissed) = cx.update(|_w, cx| {
            (host.read(cx).opened.borrow().clone(), host.read(cx).dismissed.get())
        });
        assert_eq!(opened, vec![format!("f:{fs}")], "mouseup 应打开文件行");
        assert!(dismissed, "打开后面板应关闭（DismissEvent 发出）");

        let _ = std::fs::remove_file(&f);
    }
}
