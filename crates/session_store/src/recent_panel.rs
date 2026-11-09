//! 「打开最近」居中大面板实体（反馈 2）：把「打开最近…」的浮层改为 Ctrl+P 命令面板
//! 同款样式——更大（600px 宽、窗口 2/3 高）、Acrylic 圆角浮动卡片、**出现在窗口
//! 中间**（水平居中）、带滚动条与弹簧淡入，而不是旧的小弹层。
//!
//! # Dormancy：已被 shell 的 flyout 悬浮次级菜单取代
//! shell 文件菜单「打开最近…」现走 `ui::menu` 的 flyout 语义（悬停展开到右侧），
//! 本面板保留源码 + 既有测试作 dormancy 回归（K24）：shell 的
//! `EditorApp::open_recent_panel` 仍可直连打开面板实体、recent.rs 的相关用例保持绿色；
//! 不参与「打开最近」的生产路径。
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
    AnimationExt, App, Context, DismissEvent, ElementId, EventEmitter, FocusHandle, Focusable,
    Hsla, InteractiveElement as _, IntoElement, KeyDownEvent, ParentElement as _, Render,
    ScrollHandle, SharedString, StatefulInteractiveElement, Styled as _, WeakEntity, Window, div,
    px,
};
use ui::icon::{Icon, IconName};
use ui::motion::hover_spring;
use ui::scrollbar::{Scrollbar, ScrollbarMode};

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
    /// 清空最近文件夹 / 文件两类列表并落盘 + notify（面板「Clear Recently Opened...」行）。
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
    /// 「More...」：切到完整列表模式（不关闭面板）。
    More,
    /// 「返回」：完整列表切回四区域（不关闭面板）。
    Back,
    /// 「Clear Recently Opened...」：清两类 + 落盘 + 关闭面板。
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
}

/// 面板常规行高（与 PopupMenu 项同款 26px；四区域默认视图在窗口 2/3 高内容区内
/// 容纳 More/Clear，无需滚动即可点）。
const ITEM_H: f32 = 26.;

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
            mode: RecentMode::Regions,
            folders,
            files,
            previous_focus,
            focus_handle: cx.focus_handle(),
            scroll_handle: ScrollHandle::new(),
            host,
            selected: None,
            navigable: Vec::new(),
        }
    }

    /// 行装配：区域行（文件夹 ≤10/全量、文件 ≤10/全量）+ More/返回/Clear，区域间以
    /// 分割线隔开（不设文字区域头——四区域靠分割线 + 行图标区分）。空区显示「暂无」。
    fn build_lines(&self) -> Vec<Line> {
        let mut lines = Vec::new();
        let all = self.mode == RecentMode::All;
        if all {
            lines.push(Line::Back);
        }
        // 区域 1：最近打开的文件夹（图标 Folder）
        let folder_count = if all { self.folders.len() } else { self.folders.len().min(10) };
        if folder_count == 0 {
            lines.push(Line::Empty { text: "暂无", debug: "recent-folder-empty" });
        } else {
            for i in 0..folder_count {
                lines.push(Self::item_line(true, &self.folders[i], i));
            }
        }
        lines.push(Line::Divider { debug: "recent-divider-1" });
        // 区域 2：最近打开的文件（图标 File）
        let file_count = if all { self.files.len() } else { self.files.len().min(10) };
        if file_count == 0 {
            lines.push(Line::Empty { text: "暂无", debug: "recent-file-empty" });
        } else {
            for i in 0..file_count {
                lines.push(Self::item_line(false, &self.files[i], i));
            }
        }
        // 区域 3 / 4（仅四区域）：More 与 Clear，均以分割线与上方隔开
        if !all {
            lines.push(Line::Divider { debug: "recent-divider-2" });
            lines.push(Line::More);
            lines.push(Line::Divider { debug: "recent-divider-3" });
            lines.push(Line::Clear);
        }
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
                    let _ = host.update(cx, |app, cx| {
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
                let _ = host.update(cx, |app, cx| app.open_folder_path(p, window, cx));
            } else {
                let p = path.to_string();
                let _ = host.update(cx, |app, cx| app.remove_recent_folder(&p, window, cx));
            }
        } else if std::path::Path::new(path).is_file() {
            let p = path.to_string();
            let _ = host.update(cx, |app, cx| app.open_file(&p, window, cx));
        } else {
            let p = path.to_string();
            let _ = host.update(cx, |app, cx| app.remove_recent_file(&p, window, cx));
        }
    }

    /// 焦点是否已离开面板（动作 —— 如打开文件聚焦新编辑器 —— 已带走焦点）。
    fn focus_left(&self, window: &Window, cx: &App) -> bool {
        let Some(f) = window.focused(cx) else {
            return false;
        };
        f != self.focus_handle
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
        let chrome = ui::theme::ChromeColors::for_mode(theme.is_dark());
        let selected_bg: Hsla = chrome.menu_highlight.into();

        let debug = line.debug().to_string();
        let mut base = div()
            .id(ElementId::from(format!("recent-line-{index}")))
            .debug_selector(move || debug.clone())
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .mx_1()
            .min_h(px(ITEM_H))
            .rounded_md();

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
                base = base.text_size(px(12.)).text_color(fg_muted).child(t);
                base.into_any_element()
            }
            Line::Item { is_folder, path, name, .. } => {
                let is_folder = *is_folder;
                let icon = if is_folder { IconName::Folder } else { IconName::File };
                let path = path.clone();
                let name = name.clone();
                let row = base
                    .cursor_pointer()
                    .text_color(fg)
                    .on_hover(cx.listener(move |this, hovered, _, cx| {
                        if *hovered {
                            this.selected = Some(index);
                        } else if this.selected == Some(index) {
                            this.selected = None;
                        }
                        cx.notify();
                    }))
                    .on_click(cx.listener(move |this, _ev, window, cx| {
                        this.activate_line(index, window, cx);
                    }))
                    .with_spring(
                        format!("recent-item-bg-{index}"),
                        hover_spring(if selected { 1.0 } else { 0.0 }),
                        move |d, v| d.bg(selected_bg.opacity(v)),
                    )
                    .child(Icon::new(icon).size(px(16.)).text_color(fg))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_overflow(gpui::TextOverflow::Truncate("…".into()))
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
                    );
                row.into_any_element()
            }
            Line::More | Line::Back | Line::Clear => {
                let label: &'static str = match line {
                    Line::More => "More...",
                    Line::Back => "返回",
                    Line::Clear => "Clear Recently Opened...",
                    _ => unreachable!(),
                };
                let row = base
                    .cursor_pointer()
                    .text_color(fg)
                    .on_hover(cx.listener(move |this, hovered, _, cx| {
                        if *hovered {
                            this.selected = Some(index);
                        } else if this.selected == Some(index) {
                            this.selected = None;
                        }
                        cx.notify();
                    }))
                    .on_click(cx.listener(move |this, _ev, window, cx| {
                        this.activate_line(index, window, cx);
                    }))
                    .with_spring(
                        format!("recent-item-bg-{index}"),
                        hover_spring(if selected { 1.0 } else { 0.0 }),
                        move |d, v| d.bg(selected_bg.opacity(v)),
                    )
                    .child(label);
                row.into_any_element()
            }
        }
    }

    // -- 测试可见性 --
    // `pub`：shell 的 `app::tests` 跨 crate 断言用（mode 切换 / 滚动句柄）。

    pub fn mode(&self) -> RecentMode {
        self.mode
    }

    pub fn scroll_handle(&self) -> ScrollHandle {
        self.scroll_handle.clone()
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
        let surface: Hsla = colors.surface.into();

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

        // 顶部不设「打开最近」标题（反馈：不用文字标题标识四区域，直接列表 + 分割线）。
        let panel = div()
            .id("recent-panel")
            .debug_selector(|| "recent-panel".into())
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::on_key_down))
            .on_mouse_down_out(cx.listener(Self::on_mouse_down_out))
            .flex()
            .flex_col()
            .w(px(600.))
            .h(px(panel_h))
            .rounded_lg()
            .border_1()
            .border_color(colors.border)
            .bg(surface.opacity(if is_dark {
                ui::theme::FLOAT_ACRYLIC_ALPHA_DARK
            } else {
                ui::theme::FLOAT_ACRYLIC_ALPHA_LIGHT
            }))
            // 与菜单/通知同款双层阴影（Win11 浮动卡片）。
            .shadow(vec![
                gpui::BoxShadow::new(px(0.), px(8.), gpui::hsla(0., 0., 0., 0.20))
                    .blur_radius(px(14.)),
                gpui::BoxShadow::new(px(0.), px(2.), gpui::hsla(0., 0., 0., 0.14))
                    .blur_radius(px(4.)),
            ])
            // 打开淡入（尊重 reduce_motion：`with_spring` 自动落端点不调度帧）。
            .with_spring(
                "recent-panel-fade",
                hover_spring(1.0).from(0.0),
                |this, v| this.opacity(v),
            )
            .child(
                div()
                    .id("recent-panel-list")
                    .debug_selector(|| "recent-panel-list".into())
                    .relative()
                    .flex_1()
                    .min_h(px(0.))
                    .child(
                        div()
                            .id("recent-panel-scroll")
                            .debug_selector(|| "recent-panel-scroll".into())
                            .size_full()
                            .overflow_y_scroll()
                            .track_scroll(&scroll_handle)
                            .p(px(4.))
                            .child(div().flex().flex_col().gap_y(px(1.)).children(rows)),
                    )
                    .child(
                        div()
                            .id("recent-panel-vscrollbar")
                            .debug_selector(|| "recent-panel-vscrollbar".into())
                            .absolute()
                            .inset_0()
                            .child(
                                Scrollbar::vertical(&scroll_handle)
                                    .mode(ScrollbarMode::Always)
                                    .viewport_from_layout(),
                            ),
                    ),
            );

        // 合成层纪律（同 CommandPalette，见其 render 末尾注释）：整层 deferred 独立
        // 绘制 + 全窗包裹做水平居中，面板自身挂 on_mouse_down_out（点击面板外关闭）。
        // ① 全窗包裹必须是 absolute 的**非 flex** 容器（flex 直子 hitbox 失真 → 点击
        //    全穿透到下层）；② 全窗包裹要 `occlude()`（BlockMouse）——否则点击仍会
        //    继续命中下层编辑器/仍展开的文件菜单的 hitbox。
        let win_w = window.bounds().size.width.as_f32();
        let x = ((win_w - 600.) / 2.).max(8.);
        gpui::deferred(
            div()
                .absolute()
                .top_0()
                .left_0()
                .right_0()
                .bottom_0()
                .occlude()
                .child(div().absolute().left(px(x)).top(px(96.)).child(panel)),
        )
        .with_priority(210)
    }
}
