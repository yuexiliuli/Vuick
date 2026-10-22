//! 侧边栏：可折叠/展开，顶部左侧为一竖列「目录 / 搜索」切换栏，右侧为当前视图内容。
//!
//! 折叠/展开开关放在窗口标题栏右上角（见 [`EditorApp::render_title_bar`]）；
//! 「目录」「搜索」两个按钮竖列排布，点击各自调用出对应界面。侧边栏为无缝 mica 底色
//! 直连外壳：与选项卡栏+编辑区、状态栏全部直角贴缝（无浮层卡片/圆角/间距），右缘仅以
//! 一条发丝线（`bar_border`）与正文区分隔。

use super::file_tree_prompt::FileTreePromptKind;
use super::*;
use ui::input::Input;

use config::limits::{FOLDER_SEARCH_RESULT_CAP, SEARCH_RESULT_CAP};
use config::user::UserPrefs;
use gpui::{HighlightStyle, StyledText};
use ui::motion::{hover_bg_fade, hover_spring};

use std::rc::Rc;

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
pub(super) enum SidebarView {
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

impl EditorApp {
    /// 活动栏按钮点按：未在该面板 → 展开并切到该面板；
    /// 已在该面板再点一次 → **只收起面板**，侧边栏保留为窄活动栏（不留空白宽区）。
    pub(super) fn toggle_sidebar_view(&mut self, view: SidebarView, cx: &mut Context<Self>) {
        self.sidebar.toggle_view(view);
        cx.notify();
    }

    /// 内容区整行：侧边栏展开时在左侧停靠（紧贴标题栏下），右侧为一「右列」——
    /// 上方选项卡栏 + 下方正文 `body`（正文块承载）；收起时仅返回右列本身。
    ///
    /// 无缝直连外壳：侧边栏（mica 底色）与右列（选项卡栏 + 正文块）之间无缝隙/圆角，
    /// 仅靠侧边栏右缘一条发丝线分隔；正文右缘贴窗、编辑区直落选项卡栏之下，状态栏
    /// 贴窗底部与正文块直连。右缘调宽手柄内嵌于侧边栏右缘（见 [`Self::render_sidebar`]）。
    pub(super) fn render_body_sidebar(
        &mut self,
        body: gpui::AnyElement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        // 右列：上方选项卡栏 + 下方编辑区（正文块承载；列容器 flex_col flex_1）。
        let right_col = div()
            .id("app-right-column")
            .debug_selector(|| "app-right-column".into())
            .flex()
            .flex_col()
            .flex_1()
            .min_w(px(0.))
            .min_h(px(0.))
            .child(self.render_tab_bar(window, cx))
            .child(self.render_body_card(body, window, cx));

        if !self.sidebar.open {
            return right_col.into_any_element();
        }
        div()
            .id("app-body-with-sidebar")
            .debug_selector(|| "app-body-with-sidebar".into())
            .flex()
            .flex_row()
            .flex_1()
            .min_h(px(0.))
            // 侧边栏右缘拖拽调宽：鼠标移动/松开事件冒泡到整个内容容器统一处理。
            // （原注释保留，说明不改动；此容器现也包裹右列选项卡栏+编辑区。）
            .on_mouse_move(cx.listener(
                |this: &mut EditorApp, ev: &gpui::MouseMoveEvent, _w, cx| {
                    this.update_sidebar_resize(ev.position.x.as_f32(), cx);
                },
            ))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this: &mut EditorApp, _: &gpui::MouseUpEvent, _w, cx| {
                    this.end_sidebar_resize(cx);
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this: &mut EditorApp, _: &gpui::MouseUpEvent, _w, cx| {
                    this.end_sidebar_resize(cx);
                }),
            )
            .child(self.render_sidebar(window, cx))
            .child(right_col)
            .into_any_element()
    }

    /// 正文区无缝方块：右缘贴窗直角、其它三缘与侧栏/选项卡栏/状态栏零间距直连，
    /// 无圆角、无边框（侧栏右缘发丝线、标题栏/选项卡栏底边发丝线与状态栏顶边负责分隔）。
    /// 卡面用正文底色
    /// （`panel_bg`，与编辑器/预览无缝）。
    fn render_body_card(
        &mut self,
        body: gpui::AnyElement,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        div()
            .id("app-main-card")
            .debug_selector(|| "app-main-card".into())
            // 关键：包装层必须是 flex 容器（列向），否则内部 render_body 的 .flex_1()
            // 在非 flex 的块盒里失效 → 编辑区高度坍缩为内容高，滚动条/点击命中全部错位。
            .flex()
            .flex_col()
            .flex_1()
            .min_w(px(0.))
            .min_h(px(0.))
            .ml(px(0.))
            .mr(px(0.))
            .mt(px(0.))
            .mb(px(0.))
            // 顶边 1px 发丝线 = 选项卡栏↔编辑区之分隔（由后绘制的编辑区绘制，
            // 保证不受上层选项卡栏遮挡）；其余三边由侧栏右缘/状态栏顶边承担，不重复。
            .border_t_1()
            .border_color(self.hairline())
            .bg(ui::shell::body_card_face(cx))
            .overflow_hidden()
            .child(body)
            .into_any_element()
    }

    /// 侧边栏整体：左侧为常驻竖列切换栏（活动栏），右侧为可展开的内容面板
    /// （目录/搜索）。面板收起时仅剩 46px 活动栏，正文区随之占满，不留空白宽区。
    ///
    /// 无缝 mica 直连外壳：整块 = 左侧活动栏 + 右侧面板（面板收起时仅活动栏），
    /// 底色用半透明 mica（`mica_bar_bg`）、无圆角、无四周边框，右缘一条发丝线
    /// （`bar_border`）与正文区分隔；右缘内嵌拖拽调宽手柄（见
    /// [`Self::render_sidebar_resize_handle`]，外层 `app-body-with-sidebar` 统一收事件）。
    pub(super) fn render_sidebar(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let (side_bg, border) = (self.mica_bar_bg(), self.bar_border());
        // 先构建内容（目录视图会自适应更新 sidebar_width），再取宽，避免本帧用旧宽
        let content: Option<gpui::AnyElement> = if self.sidebar.panel_open {
            Some(match self.sidebar.view {
                SidebarView::Toc => self.render_sidebar_toc(window, cx),
                SidebarView::Search => self.render_sidebar_search(window, cx),
                SidebarView::Files => self.render_sidebar_files(window, cx),
            })
        } else {
            None
        };
        let width_px = if self.sidebar.panel_open {
            self.sidebar.width
        } else {
            config::layout::SIDEBAR_ACTIVITY_W
        };
        div()
            .id("sidebar")
            .debug_selector(|| "sidebar".into())
            .w(px(width_px))
            .flex_shrink_0()
            .relative()
            .flex()
            .flex_row()
            // 贴窗左上角：左缘直角、顶缘贴标题栏（mt 0，与选项卡栏对齐）；下缘贴
            // 状态栏（mb 0）。右缘发丝线即与正文的分隔，无间距。
            .ml(px(0.))
            .mt(px(0.))
            .mb(px(0.))
            .bg(side_bg)
            .border_r_1()
            .border_color(self.hairline())
            .overflow_hidden()
            // 拖拽调宽事件由外层 app-body-with-sidebar 容器统一处理（见 render_body_sidebar）
            .child(self.render_sidebar_activity_bar(window, border, cx))
            .children(content)
            .children(self.render_sidebar_resize_handle(cx))
    }

    /// 开始拖拽侧边栏右缘：记录 (起始 x, 起始宽)，关闭自适应由用户接管。
    pub(super) fn start_sidebar_resize(&mut self, start_x: f32, cx: &mut Context<Self>) {
        self.sidebar.resize = Some((start_x, self.sidebar.width));
        self.sidebar.width_auto = false;
        cx.notify();
    }

    /// 拖拽中：按鼠标位移更新侧边栏宽（自变量窗口坐标 x）。宽度 clamp 在 [MIN, MAX]。
    pub(super) fn update_sidebar_resize(&mut self, x: f32, cx: &mut Context<Self>) {
        if let Some((start_x, start_w)) = self.sidebar.resize {
            self.sidebar.width = (start_w + (x - start_x))
                .clamp(config::layout::SIDEBAR_WIDTH_MIN, config::layout::SIDEBAR_WIDTH_MAX);
            cx.notify();
        }
    }

    /// 结束拖拽：宽度定稿后持久化（`width_auto` 已被关掉，存为手动宽度）。
    pub(super) fn end_sidebar_resize(&mut self, cx: &mut Context<Self>) {
        if self.sidebar.resize.is_some() {
            self.sidebar.resize = None;
            self.persist_prefs();
            cx.notify();
        }
    }

    /// 侧边栏右缘拖拽手柄：宽 6px 命中区，视觉 2px 强调细线（悬停/拖拽时经
    /// `hover_spring` 淡入，贴右缘绝对定位——左侧留 4px 余量、右侧 2px 视觉线）。
    /// 面板收起时返回空（无拖拽意义；不留任何 `<sidebar-gap>` 空间隔元素）。
    ///
    /// 命中/拖拽逻辑与现状完全一致：`start_sidebar_resize` 记 (起始窗口 x, 起始宽)、
    /// 外层 `app-body-with-sidebar` 的 `on_mouse_move/up(_out)` 全程跟踪（窗口坐标 δ，
    /// 与手柄位置无关）。
    fn render_sidebar_resize_handle(&mut self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        if !self.sidebar.panel_open {
            return Vec::new();
        }
        let drag_line = ui::theme::Theme::global(cx).colors.drag_hover_border;
        vec![
            div()
                .id("sidebar-resize")
                .debug_selector(|| "sidebar-resize".into())
                .absolute()
                .right(px(0.))
                .top(px(0.))
                .bottom(px(0.))
                .w(px(6.))
                .cursor(CursorStyle::ResizeColumn)
                .on_hover(cx.listener(
                    |this: &mut EditorApp, hovered: &bool, _w, cx| {
                        this.sidebar.resize_hovered = *hovered;
                        cx.notify();
                    },
                ))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this: &mut EditorApp, ev: &gpui::MouseDownEvent, _w, cx| {
                        this.start_sidebar_resize(ev.position.x.as_f32(), cx);
                    }),
                )
                .child(
                    div()
                        // 贴到右缘，不用 flex 居中（否则蓝色线离边缘差 ~2px 看着有缝）
                        .absolute()
                        .right(px(0.))
                        .top(px(0.))
                        .bottom(px(0.))
                        .w(px(2.))
                        .rounded_full()
                        .bg(drag_line)
                        // P2-10：悬停/拖拽时细线淡入（滚动条滑块同款指数平滑可接受，
                        // 这里用统一 hover 弹簧）。
                        .with_spring(
                            "sidebar-resize-accent",
                            hover_spring(if self.sidebar.resize.is_some()
                                || self.sidebar.resize_hovered
                            {
                                1.0
                            } else {
                                0.0
                            }),
                            |this, v| this.opacity(v),
                        ),
                )
                .into_any_element(),
        ]
    }

    /// 侧边栏左侧竖列切换栏：目录 / 搜索 两个纯图标按钮上下排布。
    /// 活动项**仅图标高亮**（强调蓝、背景不变），并在最左侧边缘出现蓝色竖条
    /// （与目录条目左侧指示条同款），便于快速识别当前面板。
    fn render_sidebar_activity_bar(
        &mut self,
        window: &mut Window,
        border: gpui::Hsla,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let (fg, muted) = (self.text_primary(), self.text_minor());
        let accent: gpui::Hsla = ui::theme::Theme::global(cx).colors.accent.into();
        let c = self.chrome();
        let hover_fill: gpui::Hsla = c.hover_fill.into();
        let pressed_fill: gpui::Hsla = c.pressed_fill.into();
        let specs = [
            (SidebarView::Toc, IconName::BookOpen, "sidebar-btn-toc", "目录"),
            (
                SidebarView::Search,
                IconName::Search,
                "sidebar-btn-search",
                "搜索",
            ),
            (
                SidebarView::Files,
                IconName::Folder,
                "sidebar-btn-files",
                "文件",
            ),
        ];
        let mut btns: Vec<gpui::AnyElement> = Vec::with_capacity(3);
        for (view, icon, id, label) in specs {
            let is_active = self.sidebar.panel_open && self.sidebar.view == view;
            // 活动项图标用主前景色（深色主题下近白），不变蓝
            let col = if is_active { fg } else { muted };
            let id_owned = id.to_string();
            // P2-10：hover 状态跨帧记录 → 背景淡入。
            let btn_hover = window.use_keyed_state(format!("{id}-hover"), cx, |_, _| false);
            let btn_hovered = *btn_hover.read(cx);
            // P2-9：tooltip 状态（同按钮 id 键控：延迟出现 300ms / 移出消失）。
            let tip = ui::tooltip::tooltip_state(id, window, cx);
            let tip_on = ui::tooltip::tooltip_visible(&tip, cx);
            let view_id = window.current_view();
            let btn = div()
                .id(SharedString::from(id))
                .debug_selector(move || id_owned.clone())
                .relative()
                .w(px(40.))
                .h(px(38.))
                .flex()
                .items_center()
                .justify_center()
                .mx(px(3.))
                .my(px(1.))
                .rounded_sm()
                // 背景不随活动状态变化，仅图标着色区分
                .text_color(col)
                .cursor(CursorStyle::PointingHand)
                .active(|style| style.bg(pressed_fill))
                // 活动项：最左侧蓝色竖条（同目录条目左侧指示条）
                .child(
                    div()
                        .absolute()
                        .top(px(5.))
                        .bottom(px(5.))
                        .left(px(2.))
                        .w(px(2.))
                        .rounded_full()
                        .bg(accent)
                        .opacity(if is_active { 1.0 } else { 0.0 }),
                )
                .on_click(cx.listener(move |this: &mut EditorApp, _ev, _w, cx| {
                    this.toggle_sidebar_view(view, cx);
                }))
                .on_hover({
                    let st = btn_hover.clone();
                    let tip = tip.clone();
                    move |hovered, _w, cx| {
                        st.update(cx, |v, _| *v = *hovered);
                        ui::tooltip::tooltip_hover(&tip, *hovered, cx);
                        cx.notify(view_id);
                    }
                })
                .child(Icon::new(icon).size(px(22.)).text_color(col))
                // P2-9：tooltip 浮层（贴活动栏按钮右缘垂直居中；按钮 40×38）。
                .child(ui::tooltip::tooltip_layer(
                    id, label, tip_on, 40.0, 38.0, cx,
                ));
            btns.push(
                hover_bg_fade(btn, format!("{id}-bg"), hover_fill, btn_hovered).into_any_element(),
            );
        }
        div()
            .id("sidebar-activity-bar")
            .debug_selector(|| "sidebar-activity-bar".into())
            .w(px(config::layout::SIDEBAR_ACTIVITY_W))
            .flex_shrink_0()
            .flex()
            .flex_col()
            .items_center()
            .py_1()
            .border_r_1()
            .border_color(border)
            .children(btns)
    }

    /// 折叠/展开某个目录父项（仅「有下级」的条目可折叠）。
    pub(super) fn toggle_sidebar_toc_collapse(&mut self, idx: usize, cx: &mut Context<Self>) {
        let has_child = match self.tabs.get(self.active_tab) {
            Some(tab) => match &tab.reader {
                MediaReader::Pdf(pv) => {
                    let outlines = pv.read(cx).outlines().to_vec();
                    let levels: Vec<usize> = outlines.iter().map(|e| e.level).collect();
                    toc::toc_node_has_child(&levels, idx)
                }
                _ => {
                    // P4-2：MD / EPUB 目录都来自统一 WyEditor 只读宿主。
                    let levels: Vec<usize> = self
                        .md_toc_source(cx)
                        .map(|(entries, _)| entries.iter().map(|&(_, l, _)| l as usize).collect())
                        .unwrap_or_default();
                    toc::toc_node_has_child(&levels, idx)
                }
            },
            None => false,
        };
        if !has_child {
            return;
        }
        if !self.sidebar.toc_collapsed.remove(&idx) {
            self.sidebar.toc_collapsed.insert(idx);
        }
        // 折叠集变更 → 可见性缓存作废（下帧按新折叠重建可见条目表）。
        self.sidebar.toc_cache = None;
        cx.notify();
    }

    /// 估算目录文字宽度（px）：CJK 取全宽、ASCII 取 0.6 倍字号。
    fn toc_text_width(text: &str, font_px: f32) -> f32 {
        text.chars()
            .map(|c| font_px * if c.is_ascii() { 0.6 } else { 1.0 })
            .sum()
    }

    /// 目录条目行所需宽度（px）：起始缩进 + 层级缩进 + 箭头列 + 圆点 + 标题 + 右侧留白。
    fn toc_row_width(level: usize, title: &str, font_px: f32) -> f32 {
        Self::toc_text_width(title, font_px)
            + 6.0              // pl 起始缩进
            + level as f32 * 14.0
            + 16.0             // 箭头列（无箭头也占位对齐）
            + 6.0 + 5.0 + 6.0  // gap + 圆点 + gap
            + 8.0              // pr
    }

    /// 计算当前激活文档目录所需的最大行宽（px）；无文档/无标题返回 `None`。
    fn compute_sidebar_toc_width(&self, cx: &gpui::App) -> Option<f32> {
        let tab = self.tabs.get(self.active_tab)?;
        let font_px = 13.0f32;
        // 右侧页码列宽（PDF 书签有页码，Markdown 无）：预留以免标题被截断
        let page_col_w = 34.0f32;
        let max_row = if let MediaReader::Pdf(pdf) = &tab.reader {
            let outlines = pdf.read(cx).outlines();
            outlines
                .iter()
                .map(|e| Self::toc_row_width(e.level, &e.title, font_px) + page_col_w)
                .fold(0.0f32, f32::max)
        } else {
            // P4-2：MD / EPUB 目录都来自统一 WyEditor 只读宿主。
            let entries = self
                .md_toc_source(cx)
                .map(|(e, _)| e)
                .unwrap_or_default();
            entries
                .iter()
                .map(|&(_, l, ref t)| Self::toc_row_width(l as usize, t, font_px))
                .fold(0.0f32, f32::max)
        };
        (max_row > 0.0).then_some(max_row)
    }

    /// 目录视图顶栏：「目录」标题。开关交给左侧竖列「目录」按钮（再点一次收拢侧边栏）。
    fn render_sidebar_toc_header(&mut self) -> impl IntoElement {
        let (fg, _muted) = (self.text_primary(), self.text_minor());
        let border = self.bar_border();
        div()
            .id("sidebar-toc-header")
            .debug_selector(|| "sidebar-toc-header".into())
            .flex()
            .items_center()
            .px(px(10.))
            .h(px(36.))
            .border_b_1()
            .border_color(border)
            .child(
                div()
                    .text_size(px(13.5))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(fg)
                    .child("目录"),
            )
    }

    /// 「目录」视图：当前文档的标题大纲（Markdown 标题 / PDF 书签），
    /// 带面板头（「目录」+ 关闭）与页码，条目样式统一来自公共 [`crate::toc`]（可折叠树）。
    fn render_sidebar_toc(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let (fg, muted) = (self.text_primary(), self.text_minor());
        let active_bg = self.tab_active_bg();
        let accent = ui::theme::Theme::global(cx).colors.accent.into();

        // 折叠状态按文档缓存：切换文档即清空，并重新按新文档内容自适应宽度
        let active = self.active_tab;
        if self.sidebar.toc_tab != Some(active) {
            self.sidebar.toc_tab = Some(active);
            self.sidebar.toc_collapsed.clear();
            self.sidebar.width_auto = true;
            self.sidebar.toc_width = None;
            self.sidebar.toc_cache = None;
        }

        // 目录视图自适应宽：按当前文档最宽目录条目估算（未手动拖拽时）。
        // 按 (选项卡, 编辑版本, WyEditor 内容代号) 缓存：内容没变只算一次，禁逐帧 O(N) 全量测宽。
        if self.sidebar.width_auto {
            let tab = self.tabs.get(active);
            let ver = tab.map(|t| t.edit_version).unwrap_or(0);
            let cg = tab
                .and_then(|t| t.md_wy.as_ref())
                .map(|wy| wy.read(cx).content_gen())
                .unwrap_or(0);
            let cached = self
                .sidebar
                .toc_width
                .as_ref()
                .is_some_and(|(ti, v, g, _)| *ti == active && *v == ver && *g == cg);
            if !cached
                && let Some(w) = self.compute_sidebar_toc_width(cx)
            {
                self.sidebar.toc_width = Some((active, ver, cg, w));
                self.sidebar.width = (config::layout::SIDEBAR_ACTIVITY_W + w + 12.0)
                    .clamp(config::layout::SIDEBAR_WIDTH_MIN, config::layout::SIDEBAR_WIDTH_MAX);
            }
        }

        let inner: gpui::AnyElement = match self.tabs.get(self.active_tab) {
            Some(tab) => {
                let collapsed = self.sidebar.toc_collapsed.clone();
                // PDF 书签目录
                if let MediaReader::Pdf(pdf) = &tab.reader {
                    let outlines = pdf.read(cx).outlines().to_vec();
                    if outlines.is_empty() {
                        ui::empty_state::compact_empty_state(
                            "sidebar-toc-empty",
                            IconName::File,
                            "当前 PDF 无书签目录",
                            cx,
                        )
                    } else {
                        let rows = self.sidebar_toc_pdf_rows(
                            window, &outlines, &collapsed, fg, muted, active_bg, accent,
                            pdf.clone(), cx,
                        );
                        div().flex().flex_col().py_1().children(rows).into_any_element()
                    }
                } else {
                    // Markdown 标题（P4-2：MD / EPUB 目录都来自统一 WyEditor 宿主）。
                    let tab = self.tabs.get(active);
                    let ver = tab.map(|t| t.edit_version).unwrap_or(0);
                    let wy = tab.and_then(|t| t.md_wy.as_ref());
                    let cg = wy.map(|wy| wy.read(cx).content_gen()).unwrap_or(0);
                    // 条目 + 可见性 + 跳转目标按 (选项卡, 编辑版本, WyEditor 宿主 + 内容代号)
                    // 缓存：内容/折叠没变逐帧 O(1) 复用，大文档不再每帧全量克隆条目/重扫。
                    // 命中需宿主同一 + 内容代号同一：预览关/重装（md_wy 换实体）与整篇换源
                    //（同实体 in-place 喂料，edit_version 不变）都作废重建，不渲染上一版条目。
                    let cache_ok = match &self.sidebar.toc_cache {
                        Some((ti, v, g, _, _, scroll))
                            if *ti == active && *v == ver && *g == cg =>
                        {
                            match scroll {
                                MdTocScroll::Unified(cached_wy) => wy == Some(cached_wy),
                            }
                        }
                        _ => false,
                    };
                    if !cache_ok {
                        // 内容变更/切换文档 → 重建条目 + 可见性表（一次性，逐帧 O(1) 复用）；
                        // 无 wy / 空目录 → 显式清缓存落空态分支，避免渲染到上一版条目。
                        match self.md_toc_source(cx) {
                            Some((entries, scroll)) if !entries.is_empty() => {
                                let levels: Vec<usize> =
                                    entries.iter().map(|&(_, l, _)| l as usize).collect();
                                let vis = toc::toc_visibility(&levels, &collapsed);
                                let visible: Vec<usize> = vis
                                    .iter()
                                    .enumerate()
                                    .filter_map(|(i, &v)| v.then_some(i))
                                    .collect();
                                self.sidebar.toc_cache =
                                    Some((active, ver, cg, entries, visible, scroll));
                            }
                            _ => self.sidebar.toc_cache = None,
                        }
                    }
                    match &self.sidebar.toc_cache {
                        Some((_, _, _, entries, visible, scroll)) if !entries.is_empty() => {
                            let total = visible.len();
                            // 虚拟化：只物化可视带 + 上下 M 行缓冲；带外以等高 spacer 占位
                            //（总高恒为 visible × 行高，滚动量程/命中稳定）。普通小文档全量。
                            let (start, end) = if total <= TOC_VIRTUALIZE_MIN {
                                (0usize, total)
                            } else {
                                let vhand = &self.sidebar.toc_vscroll;
                                let offset = (-vhand.offset().y).max(px(0.)).as_f32();
                                let viewport_h = vhand.bounds().size.height.as_f32();
                                // 首帧容器尚未布局（高度 0）→ 退化为固定视口估计（只多物化几行）。
                                let vh = if viewport_h > 0.0 {
                                    viewport_h
                                } else {
                                    700.0
                                };
                                let first = (offset / toc::TOC_ROW_H) as usize;
                                let last = ((offset + vh) / toc::TOC_ROW_H) as usize + 1;
                                let start = first.saturating_sub(TOC_BAND_BUFFER);
                                let end = (last + TOC_BAND_BUFFER).min(total);
                                // 折叠后条目大减时偏移可能越界：钳制空带。
                                (start.min(end), end.max(start))
                            };
                            let rows = self.sidebar_toc_md_rows(
                                window, entries, &visible[start..end], &collapsed, fg, muted,
                                active_bg, accent, scroll.clone(), cx,
                            );
                            let (top_h, bottom_h) = (
                                start as f32 * toc::TOC_ROW_H,
                                (total - end) as f32 * toc::TOC_ROW_H,
                            );
                            div()
                                .flex()
                                .flex_col()
                                .py_1()
                                .child(if top_h > 0.0 {
                                    div().h(px(top_h)).into_any_element()
                                } else {
                                    div().into_any_element()
                                })
                                .children(rows)
                                .child(if bottom_h > 0.0 {
                                    div().h(px(bottom_h)).into_any_element()
                                } else {
                                    div().into_any_element()
                                })
                                .into_any_element()
                        }
                        _ => ui::empty_state::compact_empty_state(
                            "sidebar-toc-empty",
                            IconName::BookOpen,
                            "当前文件无目录（无标题）",
                            cx,
                        ),
                    }
                }
            }
            None => ui::empty_state::compact_empty_state(
                "sidebar-toc-empty",
                IconName::File,
                "当前无打开文档",
                cx,
            ),
        };

        div()
            .id("sidebar-toc")
            .debug_selector(|| "sidebar-toc".into())
            .flex_1()
            // 允许面板缩到比最宽条目还窄（否则 min-width:auto 拒绝缩窄，拖窄后
            // 面板溢出侧边栏右缘，条目 hover/点击仍响应编辑器区）
            .min_w(px(0.))
            .min_h(px(0.))
            .overflow_hidden()
            .flex()
            .flex_col()
            .child(self.render_sidebar_toc_header())
            // 滚动条只覆盖面板内容区（不含头部），故把滚动容器 + 滚动条叠层包进
            // `relative` 包装层（对齐 panes/editor：叠层为滚动容器的兄弟、`absolute inset_0`）。
            // 结构/样式收敛到 ui::scroll_panel::ScrollPanelBuilder：三层 id 显式覆盖为
            // sidebar 既有实测 id（派生 `{id}-vscrollbar` 与之不一致），目录容器沿用 `py_1`。
            .child(
                ui::scroll_panel::ScrollPanelBuilder::new(
                    "sidebar-toc-scroll",
                    self.sidebar.toc_vscroll.clone(),
                )
                .wrap_id("sidebar-toc-scroll-wrap")
                .bar_id("sidebar-toc-vscrollbar")
                .container_style(|d: gpui::Div| d.py_1())
                .build(inner),
            )
            // P2-10：面板开合轻淡入（宽度动画会让「自适应宽 + 布局断言」测试抖动，
            // 改为只动绘制、不碰布局的透明度淡入）。
            .with_spring(
                "sidebar-toc-fade",
                hover_spring(1.0).from(0.0),
                |this, v| this.opacity(v),
            )
            .into_any_element()
    }

    /// PDF 书签目录条目（可折叠树；可见性按当前折叠集合过滤，点击跳转页码）。
    #[allow(clippy::too_many_arguments)] // 四色 + 数据源 + 折叠集，与 toc_item_frame 同构
    fn sidebar_toc_pdf_rows(
        &self,
        window: &mut Window,
        outlines: &[pdf::PdfOutlineEntry],
        collapsed: &std::collections::HashSet<usize>,
        fg: gpui::Hsla,
        muted: gpui::Hsla,
        active_bg: gpui::Hsla,
        accent: gpui::Hsla,
        pdf: gpui::Entity<pdf::PdfViewer>,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let levels: Vec<usize> = outlines.iter().map(|e| e.level).collect();
        let visible = toc::toc_visibility(&levels, collapsed);
        let mut rows: Vec<gpui::AnyElement> = Vec::new();
        for (i, e) in outlines.iter().enumerate() {
            if !visible[i] {
                continue;
            }
            let page = e.page_index;
            let wk = pdf.clone();
            let has_child = toc::toc_node_has_child(&levels, i);
            let is_collapsed = has_child && collapsed.contains(&i);
            let idx = i;
            // 折叠回调先物化（与下方 `window, cx` 参数不可同表达式借用 cx）。
            let toggle_collapse = cx.listener(move |this: &mut EditorApp, _ev, _w, cx| {
                cx.stop_propagation();
                this.toggle_sidebar_toc_collapse(idx, cx);
            });
            rows.push(toc::toc_item_frame(
                format!("sidebar-toc-pdf-{i}"),
                format!("sidebar-toc-caret-{i}"),
                e.level,
                e.title.clone(),
                has_child,
                is_collapsed,
                e.level == 0,
                (page != usize::MAX).then(|| (page + 1).to_string()),
                fg,
                muted,
                active_bg,
                accent,
                window,
                cx,
                move |_, _w, cx| {
                    if page != usize::MAX {
                        wk.update(cx, |v, cx| {
                            v.scroll_to_page(page);
                            cx.notify();
                        });
                    }
                },
                toggle_collapse,
            ));
        }
        rows
    }

    /// Markdown 标题目录条目（可折叠树；可见性按当前折叠集合过滤）。
    /// **虚拟化**：`visible` = 调用方算好的「待物化条目下标」（可视带切片，非全量），
    /// 只物化带内条目，大文档不再每帧全量实例化万级元素。
    /// P4-2：`scroll` 跳转目标 = 右栏 WyEditor 只读预览（源码行）`scroll_to_line`；
    ///（`MarkdownPreview` 的 `Legacy` 渲染行目标已随引擎删除。）
    #[allow(clippy::too_many_arguments)] // 四色 + 数据源 + 折叠集，与 toc_item_frame 同构
    fn sidebar_toc_md_rows(
        &self,
        window: &mut Window,
        entries: &[(usize, u8, SharedString)],
        visible: &[usize],
        collapsed: &std::collections::HashSet<usize>,
        fg: gpui::Hsla,
        muted: gpui::Hsla,
        active_bg: gpui::Hsla,
        accent: gpui::Hsla,
        scroll: MdTocScroll,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let levels: Vec<usize> = entries.iter().map(|&(_, l, _)| l as usize).collect();
        let mut rows: Vec<gpui::AnyElement> = Vec::with_capacity(visible.len());
        for &i in visible {
            let (tgt, lvl, ref text) = entries[i];
            let sc = scroll.clone();
            let has_child = toc::toc_node_has_child(&levels, i);
            let is_collapsed = has_child && collapsed.contains(&i);
            let idx = i;
            // 折叠回调先物化（与下方 `window, cx` 参数不可同表达式借用 cx）。
            let toggle_collapse = cx.listener(move |this: &mut EditorApp, _ev, _w, cx| {
                cx.stop_propagation();
                this.toggle_sidebar_toc_collapse(idx, cx);
            });
            rows.push(toc::toc_item_frame(
                format!("sidebar-toc-md-{i}"),
                format!("sidebar-toc-caret-{i}"),
                lvl as usize,
                text.to_string(),
                has_child,
                is_collapsed,
                lvl == 1,
                None,
                fg,
                muted,
                active_bg,
                accent,
                window,
                cx,
                move |_, window, cx| match &sc {
                    MdTocScroll::Unified(wy) => {
                        wy.update(cx, |v, cx| {
                            v.scroll_to_line(tgt, window);
                            cx.notify();
                        });
                    }
                },
                toggle_collapse,
            ));
        }
        rows
    }

    /// 「搜索」视图：关键词输入框 + 范围切换（当前文件 / 整个文件夹）+ 匹配结果。
    /// 「当前文件」= 当前文档内容 + 跨打开文件文件名匹配（旧语义）；
    /// 「整个文件夹」= 递归扫已打开文件夹内文本文件内容（后台分块，按文件分组展示）。
    fn render_sidebar_search(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let muted = self.text_minor();
        let query = self.search_committed.clone();
        let search = self.search_state.clone();
        // Fluent 文本框：整圈细边框 + 内缩边距（背景随 chrome 保持透明）；
        // 聚焦时边框点亮为强调色。
        let focused = window.focused(cx) == Some(self.search_state.read(cx).focus_handle(cx));
        let border_color: gpui::Hsla = if focused {
            ui::theme::Theme::global(cx).colors.focus_ring.into()
        } else {
            self.bar_border()
        };
        let input = div()
            .id("sidebar-search-input")
            .debug_selector(|| "sidebar-search-input".into())
            .px_2()
            .py_1()
            .mx_2()
            .my_1()
            .w_full()
            .min_h(px(34.))
            .rounded_sm()
            .border_1()
            .border_color(border_color)
            .child(Input::new(&search));

        // 扫描用「防抖后已提交」的查询词（已归一化 trim+小写）；输入框实时文本在
        // `search_state` —— 连续输入时这里沿用上次提交词的结果，防抖期满才重扫。
        let q = query;
        let scope = self.effective_search_scope();
        let hint_text = match scope {
            SearchScopeTab::CurrentFile => {
                "输入关键字，搜索当前文件内容；文件名匹配所有打开文件"
            }
            SearchScopeTab::Folder => "输入关键字，搜索整个文件夹内文本文件内容",
        };
        let result_area: gpui::AnyElement = if q.is_empty() {
            div()
                .id("sidebar-search-hint")
                .debug_selector(|| "sidebar-search-hint".into())
                .px_3()
                .py_2()
                .w_full()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_overflow(gpui::TextOverflow::Truncate("…".into()))
                .text_size(px(12.))
                .text_color(muted)
                .child(hint_text)
                .into_any_element()
        } else {
            match scope {
                SearchScopeTab::CurrentFile => {
                    self.render_sidebar_search_results(window, &q, cx)
                }
                SearchScopeTab::Folder => self.render_sidebar_folder_search(window, &q, cx),
            }
        };

        div()
            .id("sidebar-search")
            .debug_selector(|| "sidebar-search".into())
            .flex()
            .flex_col()
            .flex_1()
            .min_w(px(0.))
            .min_h(px(0.))
            .overflow_hidden()
            .child(input)
            .child(self.render_search_scope_tabs(window, cx))
            .child(result_area)
            // P2-10：面板开合轻淡入（同目录面板，见 `sidebar-toc-fade`）。
            .with_spring(
                "sidebar-search-fade",
                hover_spring(1.0).from(0.0),
                |this, v| this.opacity(v),
            )
            .into_any_element()
    }

    /// 搜索范围「当前文件 / 整个文件夹」切换 tab 条：输入框下方横排两个小 tab。
    /// 未打开文件夹时「整个文件夹」置灰不可用（点击仅提示「先打开文件夹」，
    /// 保持只搜当前文件）；活动项以活动背景 + 主前景色标示。
    fn render_search_scope_tabs(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let (fg, muted) = (self.text_primary(), self.text_minor());
        let active_bg = self.tab_active_bg();
        let has_folder = self.sidebar.files.root.is_some();
        let scope = self.effective_search_scope();
        let view_id = window.current_view();

        let current_hover =
            window.use_keyed_state("side-search-scope-cur-hover", cx, |_, _| false);
        let folder_hover =
            window.use_keyed_state("side-search-scope-folder-hover", cx, |_, _| false);
        let current_hovered = *current_hover.read(cx);
        let folder_hovered = *folder_hover.read(cx);

        let current = div()
            .id("sidebar-search-scope-current")
            .debug_selector(|| "sidebar-search-scope-current".into())
            .flex_1()
            .items_center()
            .justify_center()
            .h(px(26.))
            .mx(px(4.))
            .rounded_md()
            .text_size(px(12.))
            .text_color(if scope == SearchScopeTab::CurrentFile { fg } else { muted })
            .cursor(CursorStyle::PointingHand)
            .on_hover({
                let st = current_hover.clone();
                move |h, _w, cx| {
                    st.update(cx, |v, _| *v = *h);
                    cx.notify(view_id);
                }
            })
            .on_click(cx.listener(
                |this: &mut EditorApp, _ev, window, cx| {
                    this.set_search_scope(SearchScopeTab::CurrentFile, window, cx);
                },
            ))
            .child("当前文件");

        let folder = div()
            .id("sidebar-search-scope-folder")
            .debug_selector(|| "sidebar-search-scope-folder".into())
            .flex_1()
            .items_center()
            .justify_center()
            .h(px(26.))
            .mx(px(4.))
            .rounded_md()
            .text_size(px(12.))
            .text_color(if scope == SearchScopeTab::Folder {
                fg
            } else if has_folder {
                muted
            } else {
                muted.opacity(0.5)
            })
            .cursor(if has_folder {
                CursorStyle::PointingHand
            } else {
                CursorStyle::default()
            })
            .on_hover({
                let st = folder_hover.clone();
                move |h, _w, cx| {
                    st.update(cx, |v, _| *v = *h);
                    cx.notify(view_id);
                }
            })
            .on_click(cx.listener(
                |this: &mut EditorApp, _ev, window, cx| {
                    // 未打开文件夹 → 「整个文件夹」不可用：仅提示，保持只搜当前文件。
                    if this.sidebar.files.root.is_none() {
                        this.status = SharedString::from("先打开文件夹，再搜索整个文件夹");
                        this.status_ok = false;
                        cx.notify();
                        return;
                    }
                    this.set_search_scope(SearchScopeTab::Folder, window, cx);
                },
            ))
            .child("整个文件夹");

        let set_bg = |active: bool, hovered: bool| {
            if active {
                active_bg
            } else if hovered {
                active_bg
            } else {
                active_bg.opacity(0.)
            }
        };
        div()
            .id("sidebar-search-scope")
            .debug_selector(|| "sidebar-search-scope".into())
            .flex()
            .flex_row()
            .items_center()
            .mx_2()
            .mb_1()
            .child(current.bg(set_bg(scope == SearchScopeTab::CurrentFile, current_hovered)))
            .child(folder.bg(set_bg(
                scope == SearchScopeTab::Folder,
                folder_hovered && has_folder,
            )))
            .into_any_element()
    }

    /// 当前生效的搜索范围：用户手动选过 → 手动值；否则**有打开文件夹时默认
    /// 「整个文件夹」**，无文件夹回退「当前文件」（未打开文件夹时文件夹范围不可用）。
    pub(super) fn effective_search_scope(&self) -> SearchScopeTab {
        if self.search_scope_manual {
            self.search_scope
        } else if self.sidebar.files.root.is_some() {
            SearchScopeTab::Folder
        } else {
            SearchScopeTab::CurrentFile
        }
    }

    /// 用户点击范围 tab：记录手动选择（此后不再自动跟随文件夹有无），并通知刷新。
    pub(super) fn set_search_scope(
        &mut self,
        scope: SearchScopeTab,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.search_scope_manual = true;
        self.search_scope = scope;
        self.search_scan = None; // 范围变化 → 当前文件扫描缓存也一并失效（下帧重建）
        cx.notify();
    }

    /// 计算并渲染搜索结果（文件名命中 + 行内容命中，按打开顺序排列，封顶）。
    /// 扫描结果缓存在 [`Self::search_scan`]：查询词/活动文档未变时，反复渲染帧
    /// 直接 O(1) 复用，不再每帧 O(全文) 重扫（旧逻辑在 render 里逐帧全量扫 + 
    /// 每行 `to_lowercase()` 分配）。
    fn render_sidebar_search_results(
        &mut self,
        window: &mut Window,
        q: &str,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let (fg, muted) = (self.text_primary(), self.text_minor());
        let active_bg = self.tab_active_bg();
        // PDF 标签：全文检索由 PdfViewer 后台执行（文本在 pdfium 里，不在 tab.content）。
        // 查询按（标签, 词）键控、变化才推送（阅读器内部 250ms 防抖）；侧边栏只展示
        // 计数/页列表并驱动跳转。结果异步到达 → 经对应 PDF 选项卡的 `_pdf_sub` 订阅
        // 重绘本面板（多 PDF 并存各自独立订阅，见 `subscribe_pdf`）。
        if let Some(MediaReader::Pdf(pdf)) =
            self.tabs.get(self.active_tab).map(|t| t.reader.clone())
        {
            let push = self
                .pdf_search
                .as_ref()
                .map(|(ti, q0)| *ti != self.active_tab || q0 != q)
                .unwrap_or(true);
            if push {
                self.pdf_search = Some((self.active_tab, q.to_string()));
                let qo = q.to_string();
                pdf.update(cx, |v, cx| v.set_search(&qo, cx));
            }
            let pdf_results = self.render_sidebar_pdf_search(window, &pdf, q, cx);
            return self.wrap_search_scroll("sidebar-search-results-pdf", pdf_results);
        }
        let scan = self.sidebar_search_scan(q, cx);
        let hits = scan.file_tabs.len() + scan.line_hits.len();
        // 无命中 → 居中「灰色搜索图标 + 文案」空态占位（与目录/正文区空态同款视觉）。
        if hits == 0 {
            return self.wrap_search_scroll(
                "sidebar-search-results",
                ui::empty_state::compact_empty_state(
                    "sidebar-search-empty",
                    IconName::Search,
                    "未找到匹配内容",
                    cx,
                ),
            );
        }
        let mut rows: Vec<gpui::AnyElement> = Vec::new();
        rows.extend(self.sidebar_search_file_rows(
            window,
            &scan,
            fg,
            muted,
            active_bg,
            cx,
        ));
        rows.extend(self.sidebar_search_line_rows(window, &scan, muted, active_bg, cx));

        let inner = div()
            .child(self.sidebar_search_summary(hits, muted))
            .child(div().flex().flex_col().py_1().children(rows));
        self.wrap_search_scroll("sidebar-search-results", inner)
    }

    /// 搜索结果滚动容器包装：`overflow_y_scroll` 滚动容器外套 `relative` 包装层，
    /// 叠一条常显（`Always`）纵向滚动条——与目录/编辑器/预览同一语义：内容超量可滚
    /// 时随 [`Self::search_vscroll`] 显示 thumb，轨道点击跳转、thumb 可拖。
    /// `container_id` 透传（普通搜索 `sidebar-search-results` / PDF 搜索 `-pdf`，测试沿用）。
    /// 结构/样式收敛到 `ui::scroll_panel::ScrollPanelBuilder`：滚动条覆盖层 id 显式覆盖为
    /// `sidebar-search-vscrollbar`（派生 `{id}-vscrollbar` 与之不一致）。
    fn wrap_search_scroll(
        &self,
        container_id: &'static str,
        inner: impl IntoElement,
    ) -> gpui::AnyElement {
        let vhand = self.sidebar.search_vscroll.clone();
        ui::scroll_panel::ScrollPanelBuilder::new(container_id, vhand)
            .wrap_id("sidebar-search-results-wrap")
            .bar_id("sidebar-search-vscrollbar")
            .build(inner)
            .into_any_element()
    }

    /// 文件夹级搜索视图：按文件分组展示命中（文件头行 + 命中行），后台增量装配。
    /// 未打开文件夹 → 「先打开文件夹」空态；扫描中且无命中 → 「搜索中…」；
    /// 命中后按 [`FOLDER_SEARCH_RESULT_CAP`] 截断并提示。
    fn render_sidebar_folder_search(
        &mut self,
        window: &mut Window,
        q: &str,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let Some(scan) = self.folder_search_scan(q, window, cx) else {
            return self.wrap_search_scroll(
                "sidebar-search-results",
                ui::empty_state::compact_empty_state(
                    "sidebar-search-empty",
                    IconName::Folder,
                    "先打开文件夹，再搜索整个文件夹",
                    cx,
                ),
            );
        };
        // 扫描进行中且尚无任何命中 → 「搜索中…」占位（有命中则走汇总头计数 + 分组行）。
        if scan.running && scan.total_hits == 0 {
            return self.folder_search_loading();
        }
        if scan.total_hits == 0 {
            return self.wrap_search_scroll(
                "sidebar-search-results",
                ui::empty_state::compact_empty_state(
                    "sidebar-search-empty",
                    IconName::Search,
                    "未找到匹配内容",
                    cx,
                ),
            );
        }
        let (fg, muted) = (self.text_primary(), self.text_minor());
        let active_bg = self.tab_active_bg();
        let rows = self.sidebar_folder_file_rows(window, &scan, fg, muted, active_bg, cx);
        let inner = div()
            .child(self.sidebar_folder_search_summary(&scan, muted))
            .child(div().flex().flex_col().py_1().children(rows));
        self.wrap_search_scroll("sidebar-search-results", inner)
    }

    /// 文件夹搜索「搜索中…」占位（扫描进行中、尚无命中时的整块提示）。
    fn folder_search_loading(&self) -> gpui::AnyElement {
        let muted = self.text_minor();
        let container = div()
            .id("sidebar-search-loading")
            .debug_selector(|| "sidebar-search-loading".into())
            .px_3()
            .py_3()
            .flex()
            .items_center()
            .gap_2()
            .text_size(px(12.))
            .text_color(muted)
            .child(Icon::new(IconName::Search).size(px(14.)).text_color(muted))
            .child("搜索中…");
        self.wrap_search_scroll("sidebar-search-results", container)
    }

    /// 文件夹搜索汇总头：n 处匹配（m 个文件）/ 搜索中计数 / 已达上限截断提示。
    fn sidebar_folder_search_summary(
        &self,
        scan: &FolderSearchScan,
        muted: gpui::Hsla,
    ) -> gpui::AnyElement {
        let text = if scan.running {
            format!("搜索中… 已找到 {} 处匹配", scan.total_hits)
        } else if scan.truncated {
            format!("{} 处匹配（已达上限，结果截断）", scan.total_hits)
        } else {
            format!("{} 处匹配（{} 个文件）", scan.total_hits, scan.files.len())
        };
        div()
            .id("sidebar-folder-search-summary")
            .debug_selector(|| "sidebar-folder-search-summary".into())
            .px_3()
            .py_2()
            .text_size(px(11.))
            .text_color(muted)
            .child(text)
            .into_any_element()
    }

    /// 文件夹级搜索**分组渲染**：每文件一个组 = 文件头行（文件名 + 命中词频计数）+
    /// 逐命中行（`L行号  命中行预览`，命中词以强调色高亮）。点击命中行 → 打开/激活 +
    /// 光标定位 + 滚动可见 + 命中词临时高亮（见 [`EditorApp::folder_search_open_hit`]）。
    #[allow(clippy::too_many_arguments)] // 三色 + 数据源 + 滚动句柄，与其它行渲染同构
    fn sidebar_folder_file_rows(
        &self,
        window: &mut Window,
        scan: &FolderSearchScan,
        fg: gpui::Hsla,
        muted: gpui::Hsla,
        active_bg: gpui::Hsla,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let accent: gpui::Hsla = ui::theme::Theme::global(cx).colors.accent.into();
        let mut rows: Vec<gpui::AnyElement> = Vec::new();
        for (fi, file) in scan.files.iter().enumerate() {
            let fname = file
                .path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| file.path.to_string_lossy().into_owned());
            // 文件头行（文件名 + 命中词频计数）。
            let hdr = div()
                .id(SharedString::from(format!("sidebar-folder-file-{fi}")))
                .debug_selector(move || format!("sidebar-folder-file-{fi}"))
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .px_2()
                .py_1()
                .mx(px(4.))
                .rounded_md()
                .child(Icon::new(IconName::File).size(px(13.)).text_color(fg))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_overflow(gpui::TextOverflow::Truncate("…".into()))
                        .text_size(px(12.))
                        .text_color(fg)
                        .child(fname),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .text_size(px(11.))
                        .text_color(muted)
                        .child(format!("{} 处", file.hits.len())),
                );
            rows.push(hdr.into_any_element());
            // 命中行：行号 + 预览（命中词高亮）。
            for (ki, hit) in file.hits.iter().enumerate() {
                let li = hit.line;
                let match_range = hit.range.clone();
                let snippet = hit.snippet.clone();
                let q_owned = scan.query.clone();
                let path = file.path.clone();
                // P2-10：hover 状态跨帧记录 → 行背景淡入。
                let hover = window
                    .use_keyed_state(format!("sf-hit-{fi}-{ki}-hover"), cx, |_, _| false);
                let hovered = *hover.read(cx);
                let view_id = window.current_view();
                let line_prefix = format!("L{}  ", li + 1);
                let display = format!("{line_prefix}{snippet}");
                // 命中词在预览中高亮：snippet 内全部大小写不敏感命中区间，再平移到显示串。
                let mut hl = highlight_ranges(&snippet, &q_owned);
                for r in &mut hl {
                    r.start += line_prefix.len();
                    r.end += line_prefix.len();
                }
                let styled = StyledText::new(SharedString::from(display))
                    .with_highlights(
                        hl.into_iter()
                            .map(|r| {
                                (
                                    r,
                                    HighlightStyle {
                                        color: Some(accent),
                                        ..Default::default()
                                    },
                                )
                            })
                            .collect::<Vec<_>>(),
                    );
                let r = div()
                    .id(SharedString::from(format!(
                        "sidebar-search-folder-hit-{fi}-{ki}"
                    )))
                    .debug_selector(move || format!("sidebar-search-folder-hit-{fi}-{ki}"))
                    .px_2()
                    .py_1()
                    .mx(px(4.))
                    .rounded_md()
                    .text_size(px(12.))
                    .text_color(muted)
                    .cursor(CursorStyle::PointingHand)
                    .on_hover({
                        let st = hover.clone();
                        move |h, _w, cx| {
                            st.update(cx, |v, _| *v = *h);
                            cx.notify(view_id);
                        }
                    })
                    .on_click(cx.listener(move |this: &mut EditorApp, _ev, window, cx| {
                        this.folder_search_open_hit(&path, li, match_range.clone(), &q_owned, window, cx);
                    }))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .child(styled),
                    );
                rows.push(
                    r.bg(if hovered { active_bg } else { active_bg.opacity(0.) })
                        .into_any_element(),
                );
            }
        }
        rows
    }

    /// 文件夹搜索「取扫描或发起扫描」：按键 `(根, 查询词)` 复用缓存（含进行中/已完成），
    /// 未打开文件夹返回 `None`；查询词 / 根变化 → 发起新一轮后台扫描（新代，旧代结果丢弃）。
    fn folder_search_scan(
        &mut self,
        q: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Rc<FolderSearchScan>> {
        let root = self.sidebar.files.root.clone()?;
        if let Some(s) = &self.folder_search
            && s.query == q
            && s.root.as_deref() == Some(root.as_path())
        {
            return Some(s.clone());
        }
        self.start_folder_scan(root, q, window, cx);
        self.folder_search.clone()
    }

    /// 发起一把文件夹级后台扫描：**不在主线程同步扫整棵**——生产者（后台线程）递归
    /// 读盘并逐文件外推命中块（有界 channel 背压，参考 `spawn_progressive_open` 大文档
    /// 分块读盘姿势），主线程只收块增量装配到 [`Self::folder_search`]，扫描中 UI 显示
    /// 「搜索中…」。查询词 / 根变化 → 换代；旧代异步回调按代丢弃（不覆盖新结果）。
    fn start_folder_scan(
        &mut self,
        root: PathBuf,
        q: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let generation = self.folder_search_gen.wrapping_add(1);
        self.folder_search_gen = generation;
        self.folder_search = Some(Rc::new(FolderSearchScan {
            query: q.to_string(),
            root: Some(root.clone()),
            files: Vec::new(),
            truncated: false,
            running: true,
            generation,
            total_hits: 0,
        }));
        let entity = cx.entity();
        let q_owned = q.to_string();
        let (tx, rx) = futures::channel::mpsc::channel::<FolderScanUpdate>(4);
        window
            .spawn(cx, async move |cx| {
                use futures::{SinkExt as _, StreamExt as _};
                // ---- 生产者（后台线程）：递归扫文件夹 → 逐文件命中按块外推 ----
                let mut prod_tx = tx.clone();
                let prod_root = root;
                let prod_q = q_owned;
                let prod_sess = UserPrefs::session_dir();
                cx.background_executor()
                    .spawn(async move {
                        let mut total = 0usize;
                        let truncated = folder_scan_collect(
                            &prod_root,
                            &prod_q,
                            prod_tx.clone(),
                            &prod_sess,
                            &mut total,
                            FOLDER_SEARCH_RESULT_CAP,
                        )
                        .await;
                        let _ = prod_tx.send(FolderScanUpdate::Done { truncated }).await;
                    })
                    .detach();
                // 原始 sender 立即释放：生产者外推完毕后 channel 关闭 → 消费者收完退出。
                drop(tx);

                // ---- 消费者（主线程任务）：逐块增量装配，扫描中逐块刷新 ----
                let mut rx = rx;
                while let Some(update) = rx.next().await {
                    let scan_gen = generation;
                    let _ = cx.update_window_entity(&entity, move |this, _w, cx| {
                        let Some(existing) = this.folder_search.as_ref() else {
                            return;
                        };
                        if existing.generation != scan_gen {
                            return; // 已被更新的查询/换根取代 → 丢弃旧代结果
                        }
                        // 逐块增量：基于当前状态克隆应用增量后放回（Rc 共享读、写时替换）。
                        let mut next = (**existing).clone();
                        match update {
                            FolderScanUpdate::File(f) => {
                                next.total_hits += f.hits.len();
                                next.files.push(f);
                            }
                            FolderScanUpdate::Done { truncated } => {
                                next.truncated = truncated;
                                next.running = false;
                            }
                        }
                        this.folder_search = Some(Rc::new(next));
                        cx.notify();
                    });
                }
            })
            .detach();
    }

    /// 文件夹搜索点击命中行：路径失效 → 提示不打开；有效 → 打开/激活（已打开激活既有
    /// 选项卡）+ 光标定位到命中行 + 滚动可见 + 命中词临时高亮（编辑区选区 / 预览搜索高亮）。
    pub(super) fn folder_search_open_hit(
        &mut self,
        path: &std::path::Path,
        line: usize,
        range: std::ops::Range<usize>,
        q_owned: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 错误分类：命中路径在点击时已失效（文件被删除/改名）→ 不打开并提示。
        if !path.is_file() {
            self.status = SharedString::from(format!("命中文件已不存在：{}", path.display()));
            self.status_ok = false;
            cx.notify();
            return;
        }
        self.open_path_or_activate(path, window, cx);
        let Some(ti) = self.tabs.iter().position(|t| t.file_path.as_deref() == Some(path))
        else {
            return;
        };
        self.jump_tab_to_hit(ti, line, range, q_owned, cx);
        cx.notify();
    }

    /// 跳转到命中行并高亮命中词：源码编辑区 = 光标落行 + 选中命中区间（选中即高亮、
    /// `reveal_caret` 滚动可见）；只读预览 / WYSIWYG / 统一组件 = `set_scroll_target` +
    /// `highlight_search`（命中词临时高亮）。大文档后台打开尚未挂载（空缓冲）→ 跳过定位。
    fn jump_tab_to_hit(
        &mut self,
        ti: usize,
        li: usize,
        range: std::ops::Range<usize>,
        q_owned: &str,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.get(ti) else {
            return;
        };
        if tab.readonly_preview {
            if let Some(wy) = tab.md_wy.clone() {
                wy.update(cx, |v, cx| {
                    v.set_scroll_target(li);
                    v.highlight_search(li, q_owned, cx);
                });
            }
            return;
        }
        if let Some(l2) = tab.live2.clone() {
            l2.update(cx, |v, cx| v.highlight_search(li, q_owned, cx));
        }
        if let Some(wy) = tab.md_wy.clone() {
            wy.update(cx, |v, cx| v.highlight_search(li, q_owned, cx));
        }
        let ed = tab.editor.clone();
        ed.update(cx, |ed, cx| {
            if ed.char_count() == 0 {
                return; // 大文档后台打开尚未挂载 → 跳过定位（文件回到顶部，随后内容到达）
            }
            ed.set_caret_to(ed.offset_for_line_col(li, 0));
            ed.set_selection_range(range, cx);
        });
    }

    /// 文件名命中（跨所有打开文件）：相当于「打开文件」快速选择器，点击激活选项卡。
    fn sidebar_search_file_rows(
        &self,
        window: &mut Window,
        scan: &SidebarSearchScan,
        fg: gpui::Hsla,
        muted: gpui::Hsla,
        active_bg: gpui::Hsla,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let mut file_rows: Vec<gpui::AnyElement> = Vec::new();
        for ti in scan.file_tabs.iter().copied() {
            let name = self.tabs[ti]
                .file_path
                .as_ref()
                .and_then(|p| p.file_name())
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "未命名".to_string());
            let row_fg = if ti == self.active_tab { fg } else { muted };
            let nm = name;
            // P2-10：hover 状态跨帧记录 → 行背景淡入。
            let hover = window.use_keyed_state(format!("ss-file-{ti}-hover"), cx, |_, _| false);
            let hovered = *hover.read(cx);
            let view_id = window.current_view();
            let r = div()
                .id(SharedString::from(format!("sidebar-search-file-{ti}")))
                .debug_selector(move || format!("sidebar-search-file-{ti}"))
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .px_2()
                .py_1()
                .mx(px(4.))
                .rounded_md()
                .text_color(row_fg)
                .cursor(CursorStyle::PointingHand)
                .on_hover({
                    let st = hover.clone();
                    move |h, _w, cx| {
                        st.update(cx, |v, _| *v = *h);
                        cx.notify(view_id);
                    }
                })
                .on_click(cx.listener(move |this: &mut EditorApp, _ev, window, cx| {
                    this.activate_tab(ti, window, cx);
                }))
                .child(Icon::new(IconName::File).size(px(13.)).text_color(row_fg))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_size(px(12.))
                        .text_color(row_fg)
                        .child(nm),
                );
            file_rows.push(
                r.bg(if hovered { active_bg } else { active_bg.opacity(0.) })
                    .into_any_element(),
            );
        }
        file_rows
    }

    /// 行内容命中（只在「当前活动文档」内搜索）——若仍扫描所有打开的选项卡，
    /// 在 PDF 预览（无可搜索文本，content 为空）下会冒出别的文件（如 EPUB 完整正文）
    /// 的内容，看起来像搜索到了错误文档。点击：激活文档 + 光标/预览滚到命中行并高亮。
    fn sidebar_search_line_rows(
        &self,
        window: &mut Window,
        scan: &SidebarSearchScan,
        muted: gpui::Hsla,
        active_bg: gpui::Hsla,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let ti = self.active_tab;
        let mut line_rows: Vec<gpui::AnyElement> = Vec::new();
        for hit in &scan.line_hits {
            let li = hit.line;
            let match_range = hit.range.clone();
            let snippet = hit.snippet.clone();
            let q_owned = scan.query.clone();
            // P2-10：hover 状态跨帧记录 → 行背景淡入。
            let hover =
                window.use_keyed_state(format!("ss-line-{ti}-{li}-hover"), cx, |_, _| false);
            let hovered = *hover.read(cx);
            let view_id = window.current_view();
            let r = div()
                .id(SharedString::from(format!("sidebar-search-line-{ti}-{li}")))
                .debug_selector(move || format!("sidebar-search-line-{ti}-{li}"))
                .px_2()
                .py_1()
                .mx(px(4.))
                .rounded_md()
                .text_size(px(12.))
                .text_color(muted)
                .cursor(CursorStyle::PointingHand)
                .on_hover({
                    let st = hover.clone();
                    move |h, _w, cx| {
                        st.update(cx, |v, _| *v = *h);
                        cx.notify(view_id);
                    }
                })
                .on_click(cx.listener(move |this: &mut EditorApp, _ev, window, cx| {
                    this.activate_tab(ti, window, cx);
                    this.jump_tab_to_hit(ti, li, match_range.clone(), &q_owned, cx);
                    cx.notify();
                }))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .child(format!("L{}  {}", li + 1, snippet)),
                );
            line_rows.push(
                r.bg(if hovered { active_bg } else { active_bg.opacity(0.) })
                    .into_any_element(),
            );
        }
        line_rows
    }

    /// 搜索结果头：0 命中提示 / n 处匹配。
    fn sidebar_search_summary(&self, total: usize, muted: gpui::Hsla) -> gpui::AnyElement {
        if total == 0 {
            div()
                .px_3()
                .py_2()
                .text_size(px(12.))
                .text_color(muted)
                .child("未找到匹配内容")
                .into_any_element()
        } else {
            div()
                .px_3()
                .py_2()
                .text_size(px(11.))
                .text_color(muted)
                .child(format!("{total} 处匹配"))
                .into_any_element()
        }
    }

    /// PDF 标签的搜索视图：顶部一行「n 处匹配」计数（右侧为 ↑/↓ 导航），
    /// 下方逐命中上下文行（`L N  前5字…目标…后5字`，单行显示超长省略，点击跳转到该命中页）。
    /// 检索本身由 [`pdf::PdfViewer::set_search`] 在阅读器后台执行，这里只读状态并驱动跳转。
    fn render_sidebar_pdf_search(
        &mut self,
        window: &mut Window,
        pdf: &gpui::Entity<pdf::PdfViewer>,
        q: &str,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let (fg, muted) = (self.text_primary(), self.text_minor());
        let active_bg = self.tab_active_bg();
        let (count, truncated, active) = pdf.read(cx).search_summary();
        let snippets: Vec<(usize, String)> = pdf.read(cx).search_snippets();

        let prev_btn = self.sidebar_pdf_nav_btn(window, "↑", -1, pdf.clone(), fg, active_bg, cx);
        let next_btn = self.sidebar_pdf_nav_btn(window, "↓", 1, pdf.clone(), fg, active_bg, cx);

        // 无匹配 → 居中空态占位（灰色搜索图标 + 提示文案），与普通搜索空态同款视觉。
        let summary: gpui::AnyElement = if count == 0 {
            let hint = if q.trim().is_empty() {
                "在 PDF 全文检索；输入关键字"
            } else {
                "PDF 中没有匹配"
            };
            ui::empty_state::compact_empty_state(
                "sidebar-search-pdf-empty",
                IconName::Search,
                hint,
                cx,
            )
        } else {
            Self::sidebar_pdf_search_summary(count, truncated, fg, prev_btn, next_btn)
        };
        let hit_rows =
            self.sidebar_pdf_hit_rows(window, pdf, &snippets, active, fg, muted, active_bg, cx);

        div()
            .id("sidebar-search-results-pdf")
            .debug_selector(|| "sidebar-search-results-pdf".into())
            .flex_1()
            .min_w(px(0.))
            .min_h(px(0.))
            .overflow_y_scroll()
            .child(summary)
            .child(div().flex().flex_col().py_1().children(hit_rows))
            .into_any_element()
    }

    /// PDF 搜索头部「上一处 / 下一处」导航按钮（↑/↓，点击在命中间移动）。
    /// 返回 owned `AnyElement`（避免 RPIT 捕获 `cx` 生命周期导致连续两按钮借用冲突）。
    #[allow(clippy::too_many_arguments)] // 导航按钮：命中实体 + 几何 + 配色均为正交参数，拆结构体反添噪音
    fn sidebar_pdf_nav_btn(
        &self,
        window: &mut Window,
        symbol: &'static str,
        delta: i64,
        target: gpui::Entity<pdf::PdfViewer>,
        fg: gpui::Hsla,
        active_bg: gpui::Hsla,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let id = if delta < 0 {
            "sidebar-search-pdf-prev"
        } else {
            "sidebar-search-pdf-next"
        };
        // P2-9：tooltip 文案（上一处 / 下一处匹配）。
        let label = if delta < 0 { "上一处匹配" } else { "下一处匹配" };
        // P2-10：hover 状态跨帧记录 → 背景淡入。
        let hover = window.use_keyed_state(format!("{id}-hover"), cx, |_, _| false);
        let hovered = *hover.read(cx);
        // P2-9：tooltip 状态（同按钮 id 键控）。
        let tip = ui::tooltip::tooltip_state(id, window, cx);
        let tip_on = ui::tooltip::tooltip_visible(&tip, cx);
        let view_id = window.current_view();
        let btn = div()
            .id(SharedString::from(id))
            .debug_selector(move || SharedString::from(id).into())
            .flex()
            .items_center()
            .justify_center()
            .w(px(28.))
            .h(px(24.))
            .mx(px(2.))
            .rounded_md()
            .text_size(px(16.))
            .text_color(fg)
            .cursor(CursorStyle::PointingHand)
            .on_hover({
                let st = hover.clone();
                let tip = tip.clone();
                move |h, _w, cx| {
                    st.update(cx, |v, _| *v = *h);
                    ui::tooltip::tooltip_hover(&tip, *h, cx);
                    cx.notify(view_id);
                }
            })
            .on_click(cx.listener(move |_this: &mut EditorApp, _ev, _w, cx| {
                target.update(cx, |v, cx| v.move_match(delta, cx));
                cx.notify();
            }))
            .child(symbol)
            // P2-9：tooltip 浮层（贴导航按钮右缘垂直居中；按钮 28×24）。
            .child(ui::tooltip::tooltip_layer(
                id, label, tip_on, 28.0, 24.0, cx,
            ));
        hover_bg_fade(btn, format!("{id}-bg"), active_bg, hovered).into_any_element()
    }

    /// PDF 搜索头部「n 处匹配」计数（计数在左、导航靠右；计数过长尾部省略）。
    /// 调用方已处理 `count == 0` 的空态分支，这里只渲染命中计数头。
    #[allow(clippy::too_many_arguments)] // 双色 + 计数来源 + 导航按钮，与 render_* 同构
    fn sidebar_pdf_search_summary(
        count: usize,
        truncated: bool,
        fg: gpui::Hsla,
        prev: impl IntoElement,
        next: impl IntoElement,
    ) -> gpui::AnyElement {
        let text = if truncated {
            format!("{count} 处匹配（已截断）")
        } else {
            format!("{count} 处匹配")
        };
        div()
            .id("sidebar-search-pdf-head")
            .px_3()
            .py_2()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_overflow(gpui::TextOverflow::Truncate("…".into()))
                    .text_size(px(12.))
                    .text_color(fg)
                    .child(text),
            )
            .child(
                div()
                    .id("sidebar-search-pdf-nav")
                    .flex_shrink_0()
                    .flex()
                    .flex_row()
                    .items_center()
                    .child(prev)
                    .child(next),
            )
            .into_any_element()
    }

    /// PDF 搜索命中行（每个命中一行）：`L N  前5字…目标…后5字`，单行显示、超出省略；
    /// 当前激活命中高亮，点击跳转到该命中页。
    #[allow(clippy::too_many_arguments)] // 三色 + 数据源 + 命中列表，与 render_* 同构
    fn sidebar_pdf_hit_rows(
        &self,
        window: &mut Window,
        pdf: &gpui::Entity<pdf::PdfViewer>,
        snippets: &[(usize, String)],
        active: usize,
        fg: gpui::Hsla,
        muted: gpui::Hsla,
        active_bg: gpui::Hsla,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let mut hit_rows: Vec<gpui::AnyElement> = Vec::new();
        for (idx, (page, snippet)) in snippets.iter().enumerate() {
            let page = *page;
            let color = if idx == active { fg } else { muted };
            let row_pdf = pdf.clone();
            // P2-10：hover 状态跨帧记录 → 行背景淡入。
            let hover = window.use_keyed_state(format!("ss-pdf-hit-{idx}-hover"), cx, |_, _| false);
            let hovered = *hover.read(cx);
            let view_id = window.current_view();
            let r = div()
                .id(SharedString::from(format!("sidebar-search-pdf-hit-{idx}")))
                .debug_selector(move || format!("sidebar-search-pdf-hit-{idx}"))
                .px_2()
                .py_1()
                .mx(px(4.))
                .rounded_md()
                .flex()
                .flex_row()
                .items_center()
                .text_size(px(12.))
                .text_color(color)
                .cursor(CursorStyle::PointingHand)
                .on_hover({
                    let st = hover.clone();
                    move |h, _w, cx| {
                        st.update(cx, |v, _| *v = *h);
                        cx.notify(view_id);
                    }
                })
                .on_click(cx.listener(move |_this: &mut EditorApp, _ev, _w, cx| {
                    row_pdf.update(cx, |v, cx| v.focus_match_on_page(page, cx));
                    cx.notify();
                }))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_overflow(gpui::TextOverflow::Truncate("…".into()))
                        .child(format!("L{}  {}", page + 1, snippet)),
                );
            hit_rows.push(
                r.bg(if hovered { active_bg } else { active_bg.opacity(0.) })
                    .into_any_element(),
            );
        }
        hit_rows
    }

    /// 输入变化 → 登记防抖待提交词并重启防抖计时器（`base::SeqAfter` 代序作废旧
    /// 定时器）。每键只做 O(1) 更新 + 即时通知刷新输入框渲染；[`SEARCH_DEBOUNCE_MS`]
    /// 期满且无新输入才把 `search_pending` 提交为 `search_committed` 并通知（触发一次
    /// 扫描）。连续输入（间隔 < 防抖窗）时扫描零触发——大文档不再每键全量重扫正文。
    pub(super) fn schedule_search_debounce(&mut self, value: String, cx: &mut Context<Self>) {
        self.search_pending = normalize_search_query(&value);
        cx.notify();
        let weak = cx.weak_entity();
        self.search_debounce.after(
            cx,
            std::time::Duration::from_millis(SEARCH_DEBOUNCE_MS),
            move |app| {
                let _ = weak.update(app, |this, cx| {
                    // 期间有新输入 → 代序已前进，本定时器作废（防抖可重入）。
                    let pending = std::mem::take(&mut this.search_pending);
                    if this.search_committed != pending {
                        this.search_committed = pending;
                        cx.notify();
                    }
                });
            },
        );
    }

    /// 侧边栏搜索「扫描并缓存」：查询词 + 活动文档未变时命中缓存（O(1) Rc 克隆，
    /// 渲染帧不重扫）；否则对文件名（跨全部打开文件）与活动文档行内容扫一次。
    /// 行内容两条来源：只读预览直接借 `content` 镜像（零物化）；可编辑正文 `value()`
    /// 一次性物化后借 `&str` 逐行扫（`scan_line_hits`）——同一轮搜索只物化一次、逐行借用，
    /// 不再每行新分配小写临时串。匹配：ASCII 查询一次性小写全文副本 + std `str::find`
    /// （memmem 快路径，零逐行分配）；非 ASCII 查询词回退 [`find_ci`] 字符级折叠扫描。
    /// 结果在 [`EditorApp::search_scan`] 缓存，编辑/加载/切文档时清空重扫。
    fn sidebar_search_scan(&mut self, q: &str, cx: &App) -> Rc<SidebarSearchScan> {
        if let Some(s) = &self.search_scan
            && s.tab == self.active_tab && s.query == q {
                return s.clone();
            }
        self.search_scan_count += 1;

        let mut file_tabs: Vec<usize> = Vec::new();
        let mut line_hits: Vec<SearchLineHit> = Vec::new();
        // 文件名匹配（跨所有打开文件）
        for (ti, tab) in self.tabs.iter().enumerate() {
            if file_tabs.len() + line_hits.len() >= SEARCH_RESULT_CAP {
                break;
            }
            let name = tab
                .file_path
                .as_ref()
                .and_then(|p| p.file_name())
                .map(|s| s.to_string_lossy())
                .unwrap_or_default();
            if contains_ci(&name, q) {
                file_tabs.push(ti);
            }
        }
        // 行内容命中（仅当前活动文档；readonly 走 content 镜像，可编辑走编辑器 Rope
        // 一次物化）。文件名已占的配额扣掉后，剩余额度交给行内容扫描（共用 CAP）。
        let remaining = SEARCH_RESULT_CAP.saturating_sub(file_tabs.len());
        if remaining > 0
            && let Some(tab) = self.tabs.get(self.active_tab) {
                if tab.readonly_preview {
                    // 只读预览：直接借内容镜像（零物化；打开时已 LF 归一化）。
                    scan_line_hits(tab.content.as_ref(), q, remaining, &mut line_hits);
                } else {
                    // 可编辑正文：`value()` 一次性物化（LF 归一化，与预测/命中一致）。
                    // 相比逐行迭代 Rope（`iter_lines` 每行 `slice_line` O(log N)/行 →
                    // 139k 行 debug 实测超 1.7s；`lines(LF)` 虽 O(N) 但 debug 全遍历
                    // 仍 ~0.7s），物化 6.4MB ≈ 5ms、一次换取 O(N) 纯 `&str` 扫描——
                    // 防抖后每次只付一次，远快于逐行 Rope 迭代。整篇只物化一次后
                    // 借 `&str` 逐行扫（见 `scan_line_hits`：命中前零分配，唯一每行
                    // 分配是命中片段的 `trim` 副本，≤ remaining 次）。
                    let content = tab.editor.read(cx).value();
                    scan_line_hits(&content, q, remaining, &mut line_hits);
                }
            }

        let scan = std::rc::Rc::new(SidebarSearchScan {
            query: q.to_string(),
            tab: self.active_tab,
            file_tabs,
            line_hits,
        });
        self.search_scan = Some(scan.clone());
        scan
    }

    /// 打开文件行：已打开 → 激活既有选项卡（**不新建**）；未打开 → 复用 `open_file`
    /// 完整打开语义（内部已分流 PDF / EPUB / 图 / 视频 / 文本）。
    pub(super) fn open_path_or_activate(
        &mut self,
        path: &std::path::Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(idx) = self
            .tabs
            .iter()
            .position(|t| t.file_path.as_deref() == Some(path))
        {
            self.activate_tab(idx, window, cx);
        } else {
            self.open_file(&path.to_string_lossy(), window, cx);
        }
    }

    /// 文件树行点击（目录行）：展开/折叠切换。折叠只移出 `expanded`（children 缓存
    /// 保留，折叠再展开复用不重读）；首展未扫描 → 后台读直接子项。
    pub(super) fn file_tree_toggle(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.sidebar.files.expanded.contains(&path) {
            self.sidebar.files.expanded.remove(&path);
            self.sidebar.files.layout_gen = self.sidebar.files.layout_gen.wrapping_add(1);
            cx.notify();
            return;
        }
        // 展开：先入集合（行态立到「展开」），未扫描才异步读这一层子项。
        self.sidebar.files.expanded.insert(path.clone());
        self.sidebar.files.layout_gen = self.sidebar.files.layout_gen.wrapping_add(1);
        if !self.sidebar.files.children.contains_key(&path) {
            self.spawn_file_tree_scan(path, window, cx);
        }
        cx.notify();
    }

    /// 后台读取某目录的**直接子项**并缓存（懒加载：一次只读这一层，绝不整棵递归预
    /// 扫描）。`std::fs::read_dir` 在后台线程执行（大目录万级不卡主线程），完成后
    /// 回主线程更新 state + notify。已在缓存 → 不复读；已在途 → 幂等返回。
    pub(super) fn spawn_file_tree_scan(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.sidebar.files.children.contains_key(&path)
            || !self.sidebar.files.pending.insert(path.clone())
        {
            return;
        }
        let entity = cx.entity();
        window.spawn(cx, async move |cx| {
            let p_read = path.clone();
            let entries = cx
                .background_executor()
                .spawn(async move { read_dir_entries(&p_read) })
                .await;
            // 换根期间旧目录的在途扫描结果不落地（路径已不在当前根下 → 丢弃，防孤儿
            // 缓存膨胀；新根路径不同则互不干扰、同路径重开则幂等覆盖）。
            let _ = cx.update_window_entity(&entity, move |this, _w, cx| {
                this.sidebar.files.pending.remove(&path);
                let stale = this
                    .sidebar
                    .files
                    .root
                    .as_ref()
                    .map(|r| !path.starts_with(r))
                    .unwrap_or(true);
                if !stale {
                    this.sidebar.files.layout_gen = this.sidebar.files.layout_gen.wrapping_add(1);
                    this.sidebar.files.children.insert(path.clone(), entries);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 「文件」视图顶栏：「文件」标题 + 根目录名（路径尾段，超长省略）。
    fn render_sidebar_files_header(&mut self) -> impl IntoElement {
        let (fg, muted) = (self.text_primary(), self.text_minor());
        let border = self.bar_border();
        let root = self
            .sidebar
            .files
            .root
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy().into_owned());
        div()
            .id("sidebar-files-header")
            .debug_selector(|| "sidebar-files-header".into())
            .flex()
            .items_center()
            .gap_2()
            .px(px(10.))
            .h(px(36.))
            .border_b_1()
            .border_color(border)
            .child(
                div()
                    .text_size(px(13.5))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(fg)
                    .child("文件"),
            )
            .child(if let Some(name) = root {
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_overflow(gpui::TextOverflow::Truncate("…".into()))
                    .text_size(px(11.))
                    .text_color(muted)
                    .child(name)
                    .into_any_element()
            } else {
                div().into_any_element()
            })
    }

    /// 未打开文件夹 / 根路径失效的空态：文件夹图标 + 文案 + 可点「打开文件夹…」。
    fn sidebar_files_empty(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let muted = self.text_minor();
        let accent: gpui::Hsla = ui::theme::Theme::global(cx).colors.accent.into();
        div()
            .id("sidebar-files-empty")
            .debug_selector(|| "sidebar-files-empty".into())
            .flex_1()
            .min_w(px(0.))
            .min_h(px(0.))
            .h_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(8.))
            .px_3()
            .py_4()
            .child(Icon::new(IconName::Folder).size(px(20.)).text_color(muted))
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(muted)
                    .child("未打开文件夹"),
            )
            .child(
                div()
                    .id("sidebar-files-open-btn")
                    .debug_selector(|| "sidebar-files-open-btn".into())
                    .px_3()
                    .py_1()
                    .rounded_md()
                    .text_size(px(12.))
                    .text_color(accent)
                    .cursor(CursorStyle::PointingHand)
                    .on_click(cx.listener(
                        |this: &mut EditorApp, _: &gpui::ClickEvent, window, cx| {
                            this.open_folder_dialog(window, cx);
                        },
                    ))
                    .child("打开文件夹…"),
            )
            .into_any_element()
    }

    /// 文件树滚动容器包装：`relative` 包装层 + `overflow_y_scroll` 滚动容器 + 常显
    /// 纵向滚动条——与搜索/目录同款语义（内容超量可滚时随 [`Self::files_vscroll`]
    /// 显示 thumb，轨道点击跳转、thumb 可拖）。结构/样式收敛到
    /// `ui::scroll_panel::ScrollPanelBuilder`：包装层 / 滚动条覆盖层 id 显式覆盖为
    /// sidebar 既有 id（派生 `{id}-wrap` / `{id}-vscrollbar` 与之不一致）。
    fn wrap_files_scroll(&self, inner: impl IntoElement) -> gpui::AnyElement {
        let vhand = self.sidebar.files_vscroll.clone();
        ui::scroll_panel::ScrollPanelBuilder::new("sidebar-files-scroll", vhand)
            .wrap_id("sidebar-files-wrap")
            .bar_id("sidebar-files-vscrollbar")
            .build(inner)
            .into_any_element()
    }

    /// 「文件」视图：以打开文件夹为根的嵌套目录树。
    /// - 根 = `files.root`；未打开 / 路径失效 → 空态（可点「打开文件夹…」）。
    /// - 可见行 = 根 + 按 `expanded` 递归展开的子目录（懒加载：只物化已读层）。
    /// - 行样式复用搜索文件名行（active_bg hover、gap icon、单行截断）。
    fn render_sidebar_files(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let (fg, muted) = (self.text_primary(), self.text_minor());
        let active_bg = self.tab_active_bg();
        let content: gpui::AnyElement = if self.sidebar.files.root.is_none() {
            self.sidebar_files_empty(cx)
        } else {
            // 行描述按 `files.layout_gen` 缓存：树结构（根/展开/子项）没变逐帧复用，只把
            // 可视带内的行物化为元素——万级子项下不再每帧全量构造行元素/键控状态。
            let layout_gen = self.sidebar.files.layout_gen;
            let cached = match &self.sidebar.files_layout {
                Some((g, _)) => *g == layout_gen,
                None => false,
            };
            if !cached {
                let rows = self.sidebar_files_collect();
                self.sidebar.files_layout = Some((layout_gen, rows));
            }            let layout_rows = &self.sidebar.files_layout.as_ref().unwrap().1;
            let total = layout_rows.len();
            // 虚拟化：只物化可视带 + 上下 M 行缓冲（同目录视图 render_sidebar_toc 口径），
            // 行等高 `FILE_TREE_ROW_H` → divisor 精确；带外以等高 spacer 占位，滚动量程稳定。
            let (start, end) = if total <= TOC_VIRTUALIZE_MIN {
                (0usize, total)
            } else {
                let vhand = &self.sidebar.files_vscroll;
                let offset = (-vhand.offset().y).max(px(0.)).as_f32();
                let viewport_h = vhand.bounds().size.height.as_f32();
                let vh = if viewport_h > 0.0 {
                    viewport_h
                } else {
                    700.0
                };
                let first = (offset / FILE_TREE_ROW_H) as usize;
                let last = ((offset + vh) / FILE_TREE_ROW_H) as usize + 1;
                let start = first.saturating_sub(TOC_BAND_BUFFER);
                let end = (last + TOC_BAND_BUFFER).min(total);
                (start.min(end), end.max(start))
            };
            let mut rows: Vec<gpui::AnyElement> = Vec::with_capacity(end - start);
            for (i, row) in layout_rows.iter().enumerate().skip(start).take(end - start) {
                rows.push(self.sidebar_files_row(window, i, row, fg, muted, active_bg, cx));
            }
            let (top_h, bottom_h) = (
                start as f32 * FILE_TREE_ROW_H,
                (total - end) as f32 * FILE_TREE_ROW_H,
            );
            div()
                .flex()
                .flex_col()
                .py_1()
                .child(if top_h > 0.0 {
                    div().h(px(top_h)).into_any_element()
                } else {
                    div().into_any_element()
                })
                .children(rows)
                .child(if bottom_h > 0.0 {
                    div().h(px(bottom_h)).into_any_element()
                } else {
                    div().into_any_element()
                })
                .into_any_element()
        };
        div()
            .id("sidebar-files")
            .debug_selector(|| "sidebar-files".into())
            .flex_1()
            .min_w(px(0.))
            .min_h(px(0.))
            .overflow_hidden()
            .flex()
            .flex_col()
            .child(self.render_sidebar_files_header())
            .child(self.render_files_filter_input(window, cx))
            .child(self.wrap_files_scroll(content))
            // P2-10：面板开合轻淡入（同目录/搜索面板）。
            .with_spring(
                "sidebar-files-fade",
                hover_spring(1.0).from(0.0),
                |this, v| this.opacity(v),
            )
            .into_any_element()
    }

    /// 收集文件树全部**可见行描述**（根行 + 已展开递归子行 + 空目录占位）。
    /// 过滤词非空时按「名称不区分大小写子串」过滤：不匹配的条目（含目录）整体隐藏，
    /// 目录不匹配则其子树一并隐藏；过滤中省略「（空）」占位行（无意义）。
    /// O(可见行) 且只克隆轻量描述，不构造元素；结果按 [`SidebarChromeState::files_layout`]
    /// 缓存（`files.layout_gen` 失效），渲染逐帧 O(可视带) 物化。
    fn sidebar_files_collect(&self) -> Vec<FileRow> {
        let mut out: Vec<FileRow> = Vec::new();
        let Some(root) = self.sidebar.files.root.clone() else {
            return out;
        };
        let filter = self.files_filter.as_str();
        // 根行恒可见（树根不因过滤消失；其下子项按过滤词裁剪）。
        let root_name = root
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| root.to_string_lossy().into_owned());
        out.push(FileRow {
            path: root.clone(),
            label: root_name.into(),
            is_dir: true,
            depth: 0,
            placeholder: false,
        });
        self.sidebar_files_collect_children(&root, 0, filter, &mut out);
        // 过滤无匹配空态：仅剩根行且根的子项已加载（过滤有意义）→ 追加「无匹配结果」占位。
        if !filter.is_empty()
            && out.len() == 1
            && self.sidebar.files.children.contains_key(&root)
        {
            out.push(FileRow {
                path: root.clone(),
                label: SharedString::from("无匹配结果"),
                is_dir: false,
                depth: 1,
                placeholder: true,
            });
        }
        out
    }

    /// 递归收集某个**已展开目录**的直接子行描述（空目录 → 「（空）」占位行；过滤中省略）。
    fn sidebar_files_collect_children(
        &self,
        dir: &std::path::Path,
        depth: usize,
        filter: &str,
        out: &mut Vec<FileRow>,
    ) {
        let Some(children) = self.sidebar.files.children.get(dir) else {
            return; // 尚未扫描完成（加载中）→ 无子行，扫描完成后 `layout_gen` 递增重收
        };
        if children.is_empty() {
            if filter.is_empty() {
                out.push(FileRow {
                    path: dir.to_path_buf(),
                    label: SharedString::default(),
                    is_dir: false,
                    depth: depth + 1,
                    placeholder: true,
                });
            }
            return;
        }
        for child in children {
            // 过滤：名称不区分大小写子串不匹配 → 隐藏（含目录及其子树）。
            if !filter.is_empty() && !child.name.to_lowercase().contains(filter) {
                continue;
            }
            let is_expanded = child.is_dir && self.sidebar.files.expanded.contains(&child.path);
            out.push(FileRow {
                path: child.path.clone(),
                label: child.name.clone().into(),
                is_dir: child.is_dir,
                depth: depth + 1,
                placeholder: false,
            });
            if is_expanded {
                self.sidebar_files_collect_children(&child.path, depth + 1, filter, out);
            }
        }
    }

    /// 单条文件树行：缩进 + 目录指示（ChevronRight/Down + 文件夹图标）+ 名。
    /// 点击：目录行展开/折叠；文件行打开（已打开 → 激活既有选项卡）。当前激活
    /// 文件行高亮（active_bg 常显 + 主前景文字）。行强制 `FILE_TREE_ROW_H` 等高
    /// （虚拟化 divisor 精确）；空目录占位行显示「（空）」，无点击语义。
    fn sidebar_files_row(
        &self,
        window: &mut Window,
        key: usize,
        row: &FileRow,
        fg: gpui::Hsla,
        muted: gpui::Hsla,
        active_bg: gpui::Hsla,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let rows_id = format!("sidebar-file-row-{key}");
        // 空目录占位行（「（空）」）与过滤无匹配占位行（「无匹配结果」）：均不可点。
        if row.placeholder {
            let placeholder_text = if row.label.is_empty() {
                SharedString::from("（空）")
            } else {
                row.label.clone()
            };
            return div()
                .id(SharedString::from(rows_id.clone()))
                .debug_selector(move || rows_id.clone())
                .pl(px(10. + row.depth as f32 * FILE_TREE_INDENT))
                .py_1()
                .h(px(FILE_TREE_ROW_H))
                .mx(px(4.))
                .rounded_md()
                .text_size(px(12.))
                .text_color(muted)
                .child(placeholder_text)
                .into_any_element();
        }
        let is_expanded = row.is_dir && self.sidebar.files.expanded.contains(&row.path);
        let is_active = !row.is_dir && self.active_file_path_matches(&row.path);
        let row_fg = if is_active { fg } else { muted };
        // P2-10：hover 状态跨帧记录 → 行背景淡入（与搜索行同款）。
        let hover =
            window.use_keyed_state(format!("sidebar-file-row-{key}-hover"), cx, |_, _| false);
        let hovered = *hover.read(cx);
        let view_id = window.current_view();
        let chevron = if row.is_dir {
            Icon::new(if is_expanded {
                IconName::ChevronDown
            } else {
                IconName::ChevronRight
            })
            .size(px(12.))
            .text_color(row_fg)
            .into_any_element()
        } else {
            div().w(px(12.)).into_any_element()
        };
        let type_icon = if row.is_dir {
            Icon::new(if is_expanded {
                IconName::FolderOpen
            } else {
                IconName::Folder
            })
            .size(px(14.))
            .text_color(row_fg)
            .into_any_element()
        } else {
            Icon::new(IconName::File).size(px(13.)).text_color(row_fg).into_any_element()
        };
        let path = row.path.clone();
        let is_dir = row.is_dir;
        let label = row.label.clone();
        let depth = row.depth;
        div()
            .id(SharedString::from(rows_id.clone()))
            .debug_selector(move || rows_id.clone())
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .pl(px(8. + depth as f32 * FILE_TREE_INDENT))
            .pr_1()
            .py_1()
            .h(px(FILE_TREE_ROW_H))
            .mx(px(4.))
            .rounded_md()
            .min_w(px(0.))
            .text_color(row_fg)
            .cursor(CursorStyle::PointingHand)
            .on_hover({
                let st = hover.clone();
                move |h, _w, cx| {
                    st.update(cx, |v, _| *v = *h);
                    cx.notify(view_id);
                }
            })
            .on_click({
                let lc_path = path.clone();
                cx.listener(move |this: &mut EditorApp, _ev, window, cx| {
                    if is_dir {
                        this.file_tree_toggle(lc_path.clone(), window, cx);
                    } else {
                        this.open_path_or_activate(&lc_path, window, cx);
                    }
                })
            })
            .on_mouse_down(
                MouseButton::Right,
                {
                    let rc_path = path.clone();
                    cx.listener(move |this: &mut EditorApp, ev, window, cx| {
                        this.on_file_tree_right_click(&rc_path, is_dir, ev, window, cx);
                    })
                },
            )
            .child(chevron)
            .child(type_icon)
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_overflow(gpui::TextOverflow::Truncate("…".into()))
                    .text_size(px(12.))
                    .text_color(row_fg)
                    .child(label.to_string()),
            )
            .bg(if is_active || hovered {
                active_bg
            } else {
                active_bg.opacity(0.)
            })
            .into_any_element()
    }

    /// 某路径是否为当前激活选项卡的文件（文件树行高亮依据）。
    fn active_file_path_matches(&self, path: &std::path::Path) -> bool {
        self.tabs
            .get(self.active_tab)
            .and_then(|t| t.file_path.as_ref())
            .is_some_and(|p| p == path)
    }

    // -----------------------------------------------------------------
    // 文件树增强：过滤 / 右键菜单 / 新建 / 重命名 / 删除
    // -----------------------------------------------------------------

    /// 文件树过滤输入框：typeahead 按名字过滤树（防抖提交见
    /// [`Self::schedule_files_filter_debounce`]），清空恢复全树。
    /// 规则：过滤只作用于**已加载（已扫描且所在目录已展开）的条目**——匹配折叠/未
    /// 扫描目录内的名字不会命中（懒加载不整棵预扫，见 [`FileTreeState`]）；命中的目录
    /// 行保留并可展开（其子树仍按同词继续过滤）。
    fn render_files_filter_input(&mut self, window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        // 未打开文件夹 → 过滤框不显示（空态占满）。
        if self.sidebar.files.root.is_none() {
            return div().into_any_element();
        }
        let search = self.files_filter_state.clone();
        let focused = window.focused(cx) == Some(self.files_filter_state.read(cx).focus_handle(cx));
        let border_color: gpui::Hsla = if focused {
            ui::theme::Theme::global(cx).colors.focus_ring.into()
        } else {
            self.bar_border()
        };
        div()
            .id("sidebar-files-filter-input")
            .debug_selector(|| "sidebar-files-filter-input".into())
            .px_2()
            .py_1()
            .mx_2()
            .my_1()
            .w_full()
            .rounded_sm()
            .border_1()
            .border_color(border_color)
            .child(Input::new(&search))
            .into_any_element()
    }

    /// 输入变化 → 登记防抖待提交过滤词并重启防抖计时器（`SeqAfter` 代序作废旧定时器）。
    /// 防抖期满提交 `files_filter` + 作废文件树布局缓存（下次渲染按新过滤词重收集可见行）。
    pub(super) fn schedule_files_filter_debounce(&mut self, value: String, cx: &mut Context<Self>) {
        self.files_filter_pending = value.trim().to_lowercase();
        cx.notify();
        let weak = cx.weak_entity();
        self.files_filter_debounce.after(
            cx,
            std::time::Duration::from_millis(FILES_FILTER_DEBOUNCE_MS),
            move |app| {
                let _ = weak.update(app, |this, cx| {
                    let pending = std::mem::take(&mut this.files_filter_pending);
                    if this.files_filter != pending {
                        this.files_filter = pending;
                        this.sidebar.files.layout_gen = this.sidebar.files.layout_gen.wrapping_add(1);
                        cx.notify();
                    }
                });
            },
        );
    }

    /// 文件树项右键 → 弹上下文菜单（位置存窗口坐标）：目录 = 新建文件 / 新建文件夹 /
    /// 重命名 / 删除；文件 = 重命名 / 删除。新建类仅目录适用。关闭经 `DismissEvent`
    /// 订阅清理（同选项卡栏右键，见 [`Self::on_tab_bar_right_click`]）。
    pub(super) fn on_file_tree_right_click(
        &mut self,
        path: &std::path::Path,
        is_dir: bool,
        ev: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 事件继续冒泡到树根会触发图片/视频右键（on_media_right_click）等其它右键菜单，
        // 此处拦截保证右键文件树只弹本菜单。
        cx.stop_propagation();
        // 生命周期：菜单构建闭包是 'static，borrow 的 `path` 参数需先物化为 owned 再移动。
        let path = path.to_path_buf();
        self.file_tree_menu_entity = None;
        self.file_tree_menu_pos = None;
        self.file_tree_menu_sub = None;
        let weak = cx.weak_entity();
        let (menu, sub) = ui::menu::spawn_context_menu(
            window,
            cx,
            move |menu, _window, _cx| {
                let mut m = menu;
                if is_dir {
                    m = m.item({
                        let weak = weak.clone();
                        let parent = path.to_path_buf();
                        PopupMenuItem::element(move |_, _| {
                            div()
                                .w_full()
                                .flex()
                                .items_center()
                                .child("新建文件…")
                                .debug_selector(|| "file-tree-menu-new-file".into())
                        })
                        .on_click(move |_, window, cx| {
                            let _ = weak.update(cx, |this, cx| {
                                this.open_file_tree_prompt(
                                    FileTreePromptKind::NewFile { parent: parent.clone() },
                                    window,
                                    cx,
                                );
                            });
                        })
                    });
                    m = m.item({
                        let weak = weak.clone();
                        let parent = path.to_path_buf();
                        PopupMenuItem::element(move |_, _| {
                            div()
                                .w_full()
                                .flex()
                                .items_center()
                                .child("新建文件夹…")
                                .debug_selector(|| "file-tree-menu-new-folder".into())
                        })
                        .on_click(move |_, window, cx| {
                            let _ = weak.update(cx, |this, cx| {
                                this.open_file_tree_prompt(
                                    FileTreePromptKind::NewFolder { parent: parent.clone() },
                                    window,
                                    cx,
                                );
                            });
                        })
                    });
                }
                m = m.item({
                    let weak = weak.clone();
                    let target = path.to_path_buf();
                    PopupMenuItem::element(move |_, _| {
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .child("重命名…")
                            .debug_selector(|| "file-tree-menu-rename".into())
                    })
                    .on_click(move |_, window, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            this.open_file_tree_prompt(
                                FileTreePromptKind::Rename {
                                    path: target.clone(),
                                    is_dir: is_dir,
                                },
                                window,
                                cx,
                            );
                        });
                    })
                });
                m.item({
                    let weak = weak.clone();
                    let target = path.to_path_buf();
                    PopupMenuItem::element(move |_, _| {
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .child("删除…")
                            .debug_selector(|| "file-tree-menu-delete".into())
                    })
                    .on_click(move |_, window, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            this.file_tree_delete_request(&target, is_dir, window, cx);
                        });
                    })
                })
            },
            |this, emitter, cx| {
                if this.file_tree_menu_entity.as_ref() == Some(&emitter) {
                    this.file_tree_menu_entity = None;
                    this.file_tree_menu_pos = None;
                }
                cx.notify();
            },
        );
        self.file_tree_menu_pos = Some(ev.position);
        self.file_tree_menu_entity = Some(menu);
        self.file_tree_menu_sub = Some(sub);
        cx.notify();
    }

    /// 文件树右键菜单 Overlay（光标处弹出，同选项卡栏右键菜单定位姿势）。
    pub(super) fn render_file_tree_menu(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let Some(pos) = self.file_tree_menu_pos else {
            return div().into_any_element();
        };
        let Some(menu) = self.file_tree_menu_entity.clone() else {
            return div().into_any_element();
        };
        div()
            .id("file-tree-menu")
            .debug_selector(|| "file-tree-menu".into())
            .child(
                deferred(
                    anchored()
                        .position_mode(gpui::AnchoredPositionMode::Window)
                        .anchor(Anchor::TopLeft)
                        .position(pos)
                        .snap_to_window_with_margin(Edges::all(px(8.)))
                        .child(menu),
                )
                .with_priority(100),
            )
            .into_any_element()
    }

    /// 打开文件树输入弹层（新建/重命名共用）：建实体 + 订阅（Dismiss → 清引用）+
    /// 聚焦输入框。
    pub(super) fn open_file_tree_prompt(
        &mut self,
        kind: FileTreePromptKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let host = cx.weak_entity();
        let previous_focus = window.focused(cx);
        let prompt = cx.new(|cx| {
            file_tree_prompt::FileTreePrompt::new(host, kind, previous_focus, window, cx)
        });
        let sub = cx.subscribe(&prompt, |this, _e, _: &DismissEvent, cx| {
            this.file_tree_prompt = None;
            this._file_tree_prompt_sub = None;
            cx.notify();
        });
        let handle = prompt.read(cx).input_focus(cx);
        window.focus(&handle, cx);
        self.file_tree_prompt = Some(prompt);
        self._file_tree_prompt_sub = Some(sub);
        cx.notify();
    }

    /// 文件树输入弹层的渲染挂点（根级 overlay；无弹层 → 空元素）。
    pub(super) fn render_file_tree_prompt(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        match self.file_tree_prompt.clone() {
            Some(p) => p.into_any_element(),
            None => div().into_any_element(),
        }
    }

    /// 弹层提交入口：按类型分发到新建文件/新建文件夹/重命名。`Err(msg)` 回给弹层在
    /// 卡片内联提示（不静默关闭）；`Ok(())` 弹层自关。
    pub(super) fn file_tree_submit_prompt(
        &mut self,
        kind: &FileTreePromptKind,
        value: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let name = value.trim();
        if name.is_empty() {
            return Err("名称不能为空".to_string());
        }
        match kind {
            FileTreePromptKind::NewFile { parent } => {
                self.file_tree_create_file(parent, name, window, cx)
            }
            FileTreePromptKind::NewFolder { parent } => {
                self.file_tree_create_dir(parent, name, window, cx)
            }
            FileTreePromptKind::Rename { path, is_dir } => {
                self.file_tree_rename(path, *is_dir, name, window, cx)
            }
        }
    }

    /// 名称合法性校验（新建/重命名共用）：非空、非 `.`/`..`、无路径分隔/非法字符。
    fn validate_name(name: &str) -> Result<(), String> {
        if name.is_empty() {
            return Err("名称不能为空".to_string());
        }
        if name == "." || name == ".." {
            return Err("非法名称".to_string());
        }
        if name.contains(&['/', '\\', ':', '*', '?', '"', '<', '>', '|'][..]) {
            return Err("名称包含非法字符：/ \\ : * ? \" < > |".to_string());
        }
        Ok(())
    }

    /// 新建文件：建空文件 → 作废父目录缓存重扫 → **自动打开成选项卡** → 会话落盘。
    fn file_tree_create_file(
        &mut self,
        parent: &std::path::Path,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        Self::validate_name(name)?;
        let target = parent.join(name);
        if target.exists() {
            return Err("已存在同名文件或文件夹".to_string());
        }
        std::fs::File::create(&target).map_err(|e| format!("新建失败：{e}"))?;
        self.file_tree_invalidate(&target, window, cx);
        self.open_path_or_activate(&target, window, cx); // 新建文件 → 自动打开
        self.schedule_session_flush(cx, true);
        Ok(())
    }

    /// 新建文件夹：建空目录 → 作废父目录缓存重扫。
    fn file_tree_create_dir(
        &mut self,
        parent: &std::path::Path,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        Self::validate_name(name)?;
        let target = parent.join(name);
        if target.exists() {
            return Err("已存在同名文件或文件夹".to_string());
        }
        std::fs::create_dir(&target).map_err(|e| format!("新建文件夹失败：{e}"))?;
        self.file_tree_invalidate(&target, window, cx);
        Ok(())
    }

    /// 重命名：磁盘改名 → 树刷新 → **已打开选项卡路径同步**（含目录下子树）+ 会话落盘。
    /// 重命名根目录：换根（重建树）并按新路径持久化 `opened_folder`。
    fn file_tree_rename(
        &mut self,
        path: &std::path::Path,
        is_dir: bool,
        new_name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        Self::validate_name(new_name)?;
        let target = path
            .parent()
            .map(|p| p.join(new_name))
            .unwrap_or_else(|| PathBuf::from(new_name));
        if target == path {
            return Ok(()); // 名未变 → 无操作
        }
        if target.exists() {
            return Err("已存在同名文件或文件夹".to_string());
        }
        std::fs::rename(path, &target).map_err(|e| format!("重命名失败：{e}"))?;
        let was_root = self.sidebar.files.root.as_deref() == Some(path);
        if was_root {
            // 重命名根：换根重建树（展开/缓存全清，不可复用旧路径）。
            self.sidebar.files.set_root(Some(target.clone()));
            self.sidebar.files.expanded.insert(target.clone());
            self.spawn_file_tree_scan(target.clone(), window, cx);
            self.persist_prefs(); // opened_folder 跟随新根路径
        } else {
            self.file_tree_invalidate(path, window, cx);
        }
        // 已打开选项卡路径同步（文件精确 / 目录下子树整体前缀改写）。
        for tab in self.tabs.iter_mut() {
            let Some(fp) = tab.file_path.clone() else { continue };
            if fp == path {
                tab.file_path = Some(target.clone());
            } else if is_dir
                && fp.starts_with(path)
                && let Ok(rel) = fp.strip_prefix(path)
            {
                tab.file_path = Some(target.join(rel));
            }
        }
        self.schedule_session_flush(cx, true); // 会话清单记录新路径
        Ok(())
    }

    /// 删除请求：原生确认框（复用「未保存关闭确认」同款 `window.prompt`）→ 确认后执行。
    pub(super) fn file_tree_delete_request(
        &mut self,
        path: &std::path::Path,
        is_dir: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string_lossy().into_owned());
        let kind_label = if is_dir { "文件夹" } else { "文件" };
        let entity = cx.entity();
        let answer = window.prompt(
            PromptLevel::Warning,
            "删除",
            Some(&format!("确定要删除{kind_label}「{name}」？此操作不可恢复。")),
            &["删除", "取消"],
            cx,
        );
        let target = path.to_path_buf();
        window
            .spawn(cx, async move |cx| {
                let idx = answer.await.unwrap_or(1);
                if idx == 0 {
                    let _ = cx.update_window_entity(&entity, move |this, window, cx| {
                        this.file_tree_delete(&target, window, cx);
                    });
                }
                let _ = ();
            })
            .detach();
    }

    /// 执行删除：**任一受影响打开选项卡有未保存修改 → 不删磁盘**（不可静默丢内容，
    /// 提示后取消整个删除，请用户先保存/关闭相关选项卡）；否则干净关闭受影响选项卡
    /// → 删磁盘 → 树刷新。删根 → 清空文件夹根（等价关文件夹）。
    pub(super) fn file_tree_delete(
        &mut self,
        path: &std::path::Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let is_dir = path.is_dir();
        // 受影响的打开选项卡：文件精确匹配 / 目录按前缀覆盖子树。
        let affected: Vec<usize> = self
            .tabs
            .iter()
            .enumerate()
            .filter(|(_, t)| {
                t.file_path.as_ref().is_some_and(|p| {
                    if is_dir {
                        p.starts_with(path)
                    } else {
                        p == path
                    }
                })
            })
            .map(|(i, _)| i)
            .collect();
        // 决策：任一受影响选项卡有未保存修改 → 不删磁盘（不可静默丢内容），提示后取消。
        if affected.iter().any(|&i| self.tabs[i].dirty) {
            self.status = SharedString::from(
                "有未保存修改，未删除：请先保存或关闭相关选项卡，再重试删除",
            );
            self.status_ok = false;
            cx.notify();
            return;
        }
        // 干净受影响选项卡：同步关闭（下标降序防位移）。
        for &i in affected.iter().rev() {
            self.remove_tab(i, window, cx);
        }
        // 删磁盘。
        let res = if is_dir {
            std::fs::remove_dir_all(path)
        } else {
            std::fs::remove_file(path)
        };
        match res {
            Ok(()) => {
                let was_root = self.sidebar.files.root.as_deref() == Some(path);
                if was_root {
                    self.sidebar.files.set_root(None);
                    self.persist_prefs(); // opened_folder 写回 null
                } else {
                    self.file_tree_invalidate(path, window, cx);
                }
                self.schedule_session_flush(cx, true);
                self.status = SharedString::from(format!(
                    "已删除{}{}",
                    if is_dir { "文件夹" } else { "文件" },
                    path.display()
                ));
                self.status_ok = true;
            }
            Err(e) => {
                self.status = SharedString::from(format!("删除失败：{e}"));
                self.status_ok = false;
            }
        }
        cx.notify();
    }

    /// 树模型失效（新建/重命名/删除后的派生状态失效源）：父目录直接子项缓存作废并
    /// 重扫；`changed`（目录时含其后代）的展开集/子项缓存/在途扫描一并作废；布局代 +1。
    fn file_tree_invalidate(
        &mut self,
        changed: &std::path::Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(parent) = changed.parent()
            && self
                .sidebar
                .files
                .root
                .as_deref()
                .is_some_and(|r| parent.starts_with(r) || parent == r)
        {
            self.sidebar.files.children.remove(parent);
            self.sidebar.files.pending.remove(parent);
            self.spawn_file_tree_scan(parent.to_path_buf(), window, cx);
        }
        self.sidebar
            .files
            .children
            .retain(|p, _| p != changed && !p.starts_with(changed));
        self.sidebar.files.pending.retain(|p| p != changed && !p.starts_with(changed));
        self.sidebar.files.expanded.retain(|p| p != changed && !p.starts_with(changed));
        self.sidebar.files.layout_gen = self.sidebar.files.layout_gen.wrapping_add(1);
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
