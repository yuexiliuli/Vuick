//! 应用主视图 `EditorApp`：文本 / 代码 / Markdown 编辑器（多文件选项卡）。
//!
//! 布局（自上而下）：
//! - 菜单栏：文件 / 编辑 / 查看 / 帮助，点击弹出下拉菜单（[`EditorApp::render_menu_bar`]）
//! - 选项卡栏：每个文件一个选项卡，可新建（+）、切换、关闭（未保存时弹窗提示保存）
//! - 编辑区：**gpui-component 的 `Input`（CodeEditor 模式）**，内置语法高亮、
//!   行号、缩进参考线、搜索面板、撤销重做与滚动（替换原先自研 `TextArea`）
//! - 预览区（开启预览时）：右侧分屏实时渲染 Markdown（复用 [`markdown`]）
//! - 状态栏：文件名 / 脏标记 / 语言徽标 / 行列 / 行数 / 字数 / 字号调节
//!
//! 打开 / 保存 / 另存为均调用**系统原生文件对话框**（`rfd`），无需手动输入路径；
//! 也支持把文件从资源管理器拖拽到窗口/编辑区打开。
//!
//! 文本编辑动作（backspace/ctrl-v/undo…）由 gpui-component 的 `Input` 在 `Input`
//! 键上下文中自行处理，这里只订阅其 `InputEvent::Change` 刷新状态栏与脏标记。

mod file_ops;
mod menu;
mod panes;
mod sidebar;
mod status_bar;
mod tabs;

#[cfg(test)]
mod tests;

use std::any::Any;
use std::fs;
use std::path::{Path, PathBuf};

use gpui::{
    div, px, rgb, rgba, white, App, AppContext, Context, CursorStyle, ExternalPaths, FocusHandle,
    Focusable, InteractiveElement, IntoElement, MouseButton, ParentElement, PromptLevel, Render,
    SharedString, StatefulInteractiveElement, Styled, Subscription, VisualContext, Window,
};
use gpui_component::input::{Editor, EditorState, InputEvent, InputState, Rope, TabSize};
use gpui_component::{Icon, IconName};

use crate::actions::*;
use crate::config::layout::{
    DEFAULT_FONT_SIZE, FONT_MAX, FONT_MIN, MENU_BAR_H, SIDEBAR_WIDTH, SPLIT_RATIO_MAX,
    SPLIT_RATIO_MIN,
};
use crate::scrollbar::TableScrollState;

/// 一个文件选项卡：独立持有 gpui-component 的代码编辑器状态、文件路径与脏标记。
struct EditorTab {
    /// 本选项卡的编辑区（gpui-component `EditorState`）。
    editor: gpui::Entity<EditorState>,
    /// 当前文件路径（None = 未命名）
    file_path: Option<PathBuf>,
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
    /// 自研 Markdown 渲染器状态（行级虚拟化）。
    md_preview: gpui::Entity<crate::markdown::MarkdownPreview>,
    /// PDF 阅读器（打开 .pdf 时创建）：整页原样显示；None = 非 PDF 选项卡。
    pdf_preview: Option<gpui::Entity<crate::pdf::PdfViewer>>,
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
    /// 预览**增量解析**的基底：上次同步的整篇快照（编辑器正文 Rope 的 O(1) COW 克隆，
    /// 不物化 6.4MB 全文）与其块索引。下一次去抖后用
    /// [`crate::markdown::scan_blocks_incremental_rope`] 复用（只重扫变化区）。
    preview_prev: Option<(Rope, std::rc::Rc<crate::markdown::BlockIndex>)>,
    /// 编辑版本号：每次内容变更 +1。预览同步循环用它判断「扫描期间是否又有新编辑」，
    /// 从而在扫描结束后自动续跑合并，而不是丢给下一次按键才触发。
    edit_version: u64,
    /// 编辑侧通道：最近一次 `InputEdit` 的字节区间 `(start, old_end, new_end)`。
    /// 打字时由 Markdown 高亮器的 `update(Some(edit))` 写入（先于 `InputEvent::Change`）。
    edit_tracker: crate::syntax::EditTracker,
    /// 去抖窗口内累积的「dirty 字节区间」（bounding box，当前文本字节坐标）：供
    /// [`crate::markdown::scan_blocks_incremental_rope`] 跳过 O(n) 快照 diff；同步取走时清空。
    preview_dirty: Option<(usize, usize)>,
    /// 监听编辑事件 → 置脏 + 刷新状态栏（保持有效）
    _edit_sub: Subscription,
}

pub struct EditorApp {
    /// 所有文件选项卡
    tabs: Vec<EditorTab>,
    /// 当前激活的选项卡下标
    active_tab: usize,
    /// 选项卡栏横向滚动状态（标签过多时横向滚动 + 滚动条）
    tab_bar_hscroll: TableScrollState,
    /// 自动隐藏定时器的代序号：只清除同代定时器，避免旧定时器误清新滚动。
    tab_hscroll_seq: u64,
    /// 顶部状态文字与是否"正常"
    status: SharedString,
    status_ok: bool,
    /// 编辑器正文字号（px，用户可调）
    font_size: f32,
    /// Markdown 预览重渲染去抖是否已挂起（避免打字时逐键整篇重解析）。
    preview_pending: bool,
    /// 预览同步是否正在扫描/装配中：为 true 时不再起新一轮（防多轮 500ms 后台扫描
    /// 叠在一起；同步循环结束后复位，其间若有新编辑会自动续跑合并）。
    preview_syncing: bool,
    /// 预览同步的代序号：每轮去抖发起时 +1；后台 `scan_blocks` 结果只允许最新代
    /// 落盘，旧代迟到结果直接丢弃（防旧内容覆盖新编辑）。
    preview_seq: u64,
    /// 是否深色（黑）模式：控制组件主题、自绘色板与原生标题栏深浅
    dark_mode: bool,
    /// 编辑区 / 预览区分屏比例（0..1，= 编辑区占宽比例）
    split_ratio: f32,
    /// 编辑区是否自动换行（默认换行）。开关只作用编辑区；Markdown 预览
    /// 始终自动换行，不随此偏好变化（预览换行由文件类型/全屏阅读模式决定）。
    wrap_enabled: bool,
    /// 正在拖拽分屏分割线：`Some((起始指针x, 起始比例))`
    split_drag: Option<(f32, f32)>,
    /// 分割线是否被鼠标悬停（悬停时高亮提示可拖拽）
    splitter_hovered: bool,
    /// 侧边栏是否展开。
    sidebar_open: bool,
    /// 侧边栏当前视图（目录 / 搜索）。
    sidebar_view: sidebar::SidebarView,
    /// 侧边栏内容面板是否展开（false = 只留窄活动栏，目录/搜索面板收起）。
    sidebar_panel_open: bool,
    /// 侧边栏「搜索」关键字输入控件。
    search_state: gpui::Entity<InputState>,
    /// 侧边栏目录折叠集合（仅当前激活文档，切换文档时清空）。
    sidebar_toc_collapsed: std::collections::HashSet<usize>,
    /// 侧边栏目录折叠集合对应的选项卡下标（切换文档即失效清空）。
    sidebar_toc_tab: Option<usize>,
    /// 侧边栏当前宽度（px）：目录视图随内容自适应，也可右缘拖拽手动调整。
    sidebar_width: f32,
    /// 是否仍随目录内容自适应宽度（手动拖拽后关闭，切换文档重开）。
    sidebar_width_auto: bool,
    /// 侧边栏右缘拖拽：`Some((起始 x, 起始宽))`。
    sidebar_resize: Option<(f32, f32)>,
    /// 侧边栏右缘是否被鼠标悬停（悬停时高亮提示可拖拽）。
    sidebar_resize_hovered: bool,
    /// 监听搜索框输入变化 → 刷新搜索结果。
    _search_sub: Subscription,
    /// 侧边栏搜索的惰性扫描缓存：按（查询词, 活动文档）复用，渲染帧不重扫全文。
    /// 查询变化 / 切换文档 / 内容被编辑时清空，下次渲染重建。
    search_scan: Option<std::rc::Rc<sidebar::SidebarSearchScan>>,
    /// 实际执行过的搜索扫描次数（每次命中缓存复用不改）。测试用：验证「查询未变
    /// 时反复渲染不重扫」这一优化真实生效。
    pub(crate) search_scan_count: usize,
    focus_handle: FocusHandle,
}

impl Focusable for EditorApp {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EditorApp {
    /// 组件编辑器内部统一使用 LF，避免 Windows 剪贴板 CRLF 进入文本布局。
    fn normalize_newlines(text: &str) -> String {
        text.replace("\r\n", "\n").replace('\r', "\n")
    }

    /// 创建并注册 `EditorApp` 实体（作为窗口的第一层视图，外层再由
    /// `gpui_component::Root` 包裹）。
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> EditorApp {
        let focus_handle = cx.focus_handle();
        // 侧边栏搜索输入框：内容变化 → 重刷 app（从而重算并展示搜索结果）
        let search = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("搜索文件与内容…")
                .default_value("")
        });
        let search_state = search.clone();
        let search_sub = cx.subscribe(&search, |_this, _emitter, ev: &InputEvent, cx| {
            if let InputEvent::Change = ev {
                cx.notify();
            }
        });
        let mut app = EditorApp {
            tabs: Vec::new(),
            active_tab: 0,
            tab_bar_hscroll: TableScrollState::new(),
            tab_hscroll_seq: 0,
            status: SharedString::from("就绪"),
            status_ok: true,
            font_size: DEFAULT_FONT_SIZE,
            preview_pending: false,
            preview_syncing: false,
            preview_seq: 0,
            dark_mode: false,
            split_ratio: 0.5,
            // 编辑区软换行默认关闭；仅 Markdown 文件的编辑区默认换行（load_file_text 显式开启）。
            wrap_enabled: false,
            split_drag: None,
            splitter_hovered: false,
            sidebar_open: true,
            sidebar_view: sidebar::SidebarView::Toc,
            // 目录面板默认收起（仅留 46px 活动栏，不挤占正文）；
            // 点活动栏「目录」按钮再展开。
            sidebar_panel_open: false,
            search_state,
            sidebar_toc_collapsed: Default::default(),
            sidebar_toc_tab: None,
            sidebar_width: SIDEBAR_WIDTH,
            sidebar_width_auto: true,
            sidebar_resize: None,
            sidebar_resize_hovered: false,
            _search_sub: search_sub,
            search_scan: None,
            search_scan_count: 0,
            focus_handle,
        };
        app.make_tab(window, cx);

        // 命令行参数：第一个参数作为初始打开文件（复用首个空选项卡，不额外新建）
        let mut args = std::env::args_os();
        args.next(); // 跳过程序名
        if let Some(first) = args.next() {
            let p = PathBuf::from(&first);
            if p.is_file() {
                app.open_file_in_tab(0, &p.to_string_lossy(), window, cx);
            }
        }
        app
    }

    /// 当前激活选项卡的编辑区焦点句柄（供入口聚焦）。
    pub fn editor_focus_handle(&self, cx: &App) -> FocusHandle {
        self.tabs
            .get(self.active_tab)
            .map(|t| t.editor.read(cx).focus_handle(cx))
            .unwrap_or_else(|| self.focus_handle.clone())
    }

    /// 选项卡默认文件名（用于保存对话框）。
    fn tab_file_name(&self, idx: usize) -> String {
        self.tabs
            .get(idx)
            .and_then(|t| t.file_path.as_ref())
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "未命名.txt".into())
    }

    /// 当前激活选项卡是否开启预览。
    fn show_preview(&self) -> bool {
        self.tabs
            .get(self.active_tab)
            .map(|t| t.preview_on)
            .unwrap_or(false)
    }

    /// 把指定选项卡的当前内容同步到 Markdown 预览（开启预览时刷新一次）。
    /// 源用 `tab.content`（编辑区内容的镜像，EPUB 打开时正文只存在于 content/预览，
    /// 编辑区为空，不能用 editor.value() 否则会拿空串覆盖预览导致内容消失）。
    fn sync_md_preview(&mut self, idx: usize, cx: &mut Context<Self>) {
        let panel_bg = self.panel_bg();
        // 预览区始终自动换行（Markdown/EPUB，不随编辑区换行偏好）
        let Some(tab) = self.tabs.get_mut(idx) else {
            return;
        };
        let value = if !tab.content.is_empty() {
            tab.content.clone().to_string()
        } else {
            tab.editor.read(cx).value().to_string()
        };
        tab.md_preview.update(cx, |view, cx| {
            view.set_background(panel_bg);
            view.set_base_dir(tab.file_path.as_ref().and_then(|p| p.parent()).map(|d| d.to_path_buf()));
            view.set_wrap(true, cx);
            view.set_source(&value, cx);
        });
    }

    /// 切换指定选项卡的 Markdown 预览（眼睛图标 / 菜单共用）。
    /// 会先激活该选项卡，再翻转其 `preview_on`。
    /// EPUB（`readonly_preview`）：正文只在预览区、编辑区为空，翻转预览无意义，
    /// 点击仅重新同步内容（持久保持全屏预览，不会落入空白编辑区）。
    fn toggle_preview_for_tab(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.activate_tab(idx, window, cx);
        let readonly = self.tabs.get(idx).map(|t| t.readonly_preview).unwrap_or(false);
        let preview_on = {
            let tab = &mut self.tabs[idx];
            if readonly {
                tab.preview_on = true;
                tab.preview_fullscreen = true;
            } else {
                tab.preview_on = !tab.preview_on;
                // 关闭预览时同步退出全屏
                if !tab.preview_on {
                    tab.preview_fullscreen = false;
                    // 增量基底随预览关闭即失效（下次开启重新全量建立）
                    tab.preview_prev = None;
                }
            }
            tab.preview_on
        };
        // 开启预览 → 编辑区强制换行，方便在分屏里编辑 Markdown；
        // 关闭预览 → 恢复默认：Markdown 文件编辑区仍换行（md 默认），其余按全局偏好。
        let is_md = self.is_markdown_tab(idx);
        let wrap = preview_on || is_md || self.wrap_enabled;
        self.tabs[idx].editor.update(cx, |ed, cx| {
            ed.set_soft_wrap(wrap, window, cx);
        });
        if preview_on {
            self.sync_md_preview(idx, cx);
        } else {
            // 关闭预览：释放整篇解析出的行数据与测量缓存，内存回到基线
            // （否则隐藏的预览仍占着整篇文档，反复开关只涨不降）。
            self.tabs[idx].md_preview.update(cx, |view, cx| view.release(cx));
        }
        cx.notify();
    }

    /// 切换当前选项卡预览是否全屏占满（EPUB 阅读模式）。
    fn toggle_preview_fullscreen(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let idx = self.active_tab;
        let (fullscreen, preview, editor) = {
            let Some(tab) = self.tabs.get_mut(idx) else {
                return;
            };
            tab.preview_fullscreen = !tab.preview_fullscreen;
            let fs = tab.preview_fullscreen;
            (fs, tab.md_preview.clone(), tab.editor.clone())
        };
        // 进入全屏阅读模式 → 预览强制换行、编辑区强制换行；
        // 退出全屏 → 预览始终换行（不随偏好）；编辑区恢复默认（md 换行 / 其余按偏好）。
        preview.update(cx, |view, cx| view.set_wrap(true, cx));
        let is_md = self.is_markdown_tab(idx);
        let editor_wrap = if fullscreen {
            true
        } else {
            is_md || self.wrap_enabled
        };
        editor.update(cx, |ed, cx| {
            ed.set_soft_wrap(editor_wrap, window, cx);
        });
        cx.notify();
    }

    /// 切换当前选项卡编辑区的自动换行/不换行，并更新全局编辑器偏好。
    /// 只作用于编辑区；Markdown/EPUB 预览始终自动换行，不随此开关变化。
    fn toggle_wrap(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let idx = self.active_tab;
        let wrap_enabled = !self.wrap_enabled;
        self.wrap_enabled = wrap_enabled;
        // 全屏阅读模式强制换行，跳过（由全屏切换控制）；否则按编辑器偏好
        if self
            .tabs
            .get(idx)
            .map(|t| !t.preview_fullscreen)
            .unwrap_or(true)
        {
            if let Some(tab) = self.tabs.get(idx) {
                tab.editor.update(cx, |ed, cx| {
                    ed.set_soft_wrap(wrap_enabled, window, cx);
                });
            }
        }
        cx.notify();
    }

    fn is_markdown_path(p: &Path) -> bool {
        p.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("md") || e.eq_ignore_ascii_case("markdown"))
            .unwrap_or(false)
    }

    /// 选项卡是否 Markdown 文件（编辑区/预览默认换行的依据）。
    fn is_markdown_tab(&self, idx: usize) -> bool {
        self.tabs
            .get(idx)
            .and_then(|t| t.file_path.as_ref())
            .map(|p| Self::is_markdown_path(p))
            .unwrap_or(false)
    }

    /// 是否 EPUB 文件（按扩展名识别）。
    fn is_epub_path(p: &Path) -> bool {
        p.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("epub"))
            .unwrap_or(false)
    }

    /// 是否 PDF 文件（按扩展名识别）。
    fn is_pdf_path(p: &Path) -> bool {
        p.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("pdf"))
            .unwrap_or(false)
    }

    /// 按扩展名推断 gpui-component 语法高亮的语言名（未知扩展名回退 "text"）。
    fn language_id(p: &Path) -> &'static str {
        let ext = p
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .unwrap_or_default();
        match ext.as_str() {
            "md" | "markdown" => "markdown",
            "rs" => "rust",
            "toml" => "toml",
            "json" => "json",
            "py" => "python",
            "js" | "jsx" => "javascript",
            "ts" | "tsx" => "typescript",
            "html" | "htm" => "html",
            "css" | "scss" => "css",
            "c" | "h" => "c",
            "cpp" | "cc" | "hpp" => "cpp",
            "go" => "go",
            "java" => "java",
            "sh" | "bash" => "bash",
            "yaml" | "yml" => "yaml",
            "sql" => "sql",
            _ => "text",
        }
    }

    /// 当前文件显示名（无文件时为"未命名"）。
    fn file_display_name(&self) -> String {
        self.tabs
            .get(self.active_tab)
            .and_then(|t| t.file_path.as_ref())
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "未命名".to_string())
    }

    /// 按扩展名推断语言标签（仅用于状态栏展示）。
    fn language_label(&self) -> &'static str {
        let Some(p) = self
            .tabs
            .get(self.active_tab)
            .and_then(|t| t.file_path.as_ref())
        else {
            return "纯文本";
        };
        let ext = p
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .unwrap_or_default();
        match ext.as_str() {
            "md" | "markdown" => "Markdown",
            "epub" => "EPUB",
            "pdf" => "PDF",
            "rs" => "Rust",
            "toml" => "TOML",
            "json" => "JSON",
            "py" => "Python",
            "js" | "ts" | "jsx" | "tsx" => "JavaScript",
            "html" | "htm" => "HTML",
            "css" | "scss" => "CSS",
            "c" | "h" => "C",
            "cpp" | "cc" | "hpp" => "C++",
            "go" => "Go",
            "java" => "Java",
            "sh" | "bash" => "Shell",
            "yaml" | "yml" => "YAML",
            "xml" => "XML",
            "sql" => "SQL",
            "txt" => "纯文本",
            _ => "文本",
        }
    }

    /// 调节编辑器字号并刷新预览（范围约束见 FONT_MIN/FONT_MAX）。
    fn change_font_size(&mut self, delta: f32, cx: &mut Context<Self>) {
        self.font_size = (self.font_size + delta).clamp(FONT_MIN, FONT_MAX);
        cx.notify();
    }

    /// 切换侧边栏「展开/收起」。
    fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        self.sidebar_open = !self.sidebar_open;
        cx.notify();
    }

    /// 窗口整体背景（编辑区以外的画布）。
    fn window_bg(&self) -> gpui::Hsla {
        if self.dark_mode {
            rgb(0x111827).into()
        } else {
            rgb(0xf3f4f6).into()
        }
    }

    /// 菜单栏 / 选项卡栏 / 状态栏等工具条背景。
    fn bar_bg(&self) -> gpui::Hsla {
        if self.dark_mode {
            rgb(0x1f2937).into()
        } else {
            rgb(0xf3f4f6).into()
        }
    }

    /// 工具条分隔边框。
    fn bar_border(&self) -> gpui::Hsla {
        if self.dark_mode {
            rgb(0x374151).into()
        } else {
            rgb(0xe5e7eb).into()
        }
    }

    /// 编辑器/预览面板背景。
    fn panel_bg(&self) -> gpui::Hsla {
        if self.dark_mode {
            rgb(0x111827).into()
        } else {
            white()
        }
    }

    /// 主要文字颜色。
    fn text_primary(&self) -> gpui::Hsla {
        if self.dark_mode {
            rgb(0xe5e7eb).into()
        } else {
            rgb(0x111827).into()
        }
    }

    /// 次要/弱化文字颜色。
    fn text_minor(&self) -> gpui::Hsla {
        if self.dark_mode {
            rgb(0x9ca3af).into()
        } else {
            rgb(0x6b7280).into()
        }
    }

    /// 胶囊/小按钮等"芯片"背景。
    fn chip_bg(&self) -> gpui::Hsla {
        if self.dark_mode {
            rgb(0x374151).into()
        } else {
            rgb(0xe5e7eb).into()
        }
    }

    /// 芯片上的文字颜色。
    fn chip_text(&self) -> gpui::Hsla {
        if self.dark_mode {
            rgb(0xe5e7eb).into()
        } else {
            rgb(0x374151).into()
        }
    }

    /// 激活选项卡背景。
    fn tab_active_bg(&self) -> gpui::Hsla {
        if self.dark_mode {
            rgb(0x374151).into()
        } else {
            white()
        }
    }

    /// 激活选项卡边框。
    fn tab_active_border(&self) -> gpui::Hsla {
        if self.dark_mode {
            rgb(0x4b5563).into()
        } else {
            rgb(0xd1d5db).into()
        }
    }

    /// 状态栏/其他整体面板背景。
    fn surface_bg(&self) -> gpui::Hsla {
        if self.dark_mode {
            rgb(0x111b2a).into()
        } else {
            white()
        }
    }

    /// 切换黑白（深色/浅色）主题：驱动 gpui_component 组件主题与自绘色板。
    /// 自绘标题栏也走同一套色板，因此整窗（含标题栏）随之切换。
    /// 编辑器语法配色随明暗切换为 GitHub Light / GitHub Dark（见 [`crate::syntax`]）。
    fn toggle_theme(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.dark_mode = !self.dark_mode;
        gpui_component::Theme::change(
            if self.dark_mode {
                gpui_component::ThemeMode::Dark
            } else {
                gpui_component::ThemeMode::Light
            },
            None,
            cx,
        );
        // Theme::change 会重置 highlight_theme，这里重装 GitHub 语法配色
        // （暗色编辑区背景也由该配色承载：0x111827，与旧 apply_dark_editor_colors 一致；行号栏不设独立背景色）。
        crate::syntax::install_github_theme(cx);
        // 刷新所有 Markdown 预览背景：背景是自绘色板（panel_bg）快照，
        // 若不刷新，切换主题后预览文字色随组件主题变化而背景仍是旧色，导致内容看不清。
        let panel_bg = self.panel_bg();
        // PDF 阅读器画布用灰色系（window_bg），与页面“纸面”卡片形成分隔
        let canvas_bg = self.window_bg();
        for tab in &self.tabs {
            let md = tab.md_preview.clone();
            md.update(cx, |view, cx| {
                view.set_background(panel_bg);
                cx.notify();
            });
            // PDF 阅读器同样刷新背景（主题色变化后与画布保持一致）
            if let Some(pdf) = &tab.pdf_preview {
                let pv = pdf.clone();
                pv.update(cx, |view, cx| {
                    view.set_background(canvas_bg);
                    cx.notify();
                });
            }
        }
        cx.notify();
    }
    /// 自绘标题栏：无边框窗口下代替原生标题栏，颜色跟随黑白主题。
    /// 左侧为 文件/编辑/查看/帮助 菜单，中间空位为窗口拖拽区，
    /// 右侧为最小化 / 最大化 / 关闭按钮。
    ///
    /// 拖拽与窗口控制全部交给平台原生处理：各元素标上
    /// [`gpui::WindowControlArea`]，GPUI 在 WM_NCHITTEST 里返回对应
    /// HTCAPTION / HTMINMAXBUTTON，由系统完成移动（贴边吸附）、
    /// 最小化、最大化（含双击标题栏切换）与关闭。
    pub(super) fn render_title_bar(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let (bar_bg, bar_border, text_minor) =
            (self.bar_bg(), self.bar_border(), self.text_minor());
        let icon_style = |icon: IconName| {
            Icon::new(icon).size(px(14.)).text_color(text_minor)
        };

        // 侧边栏「展开/收起」开关：收拢时显示打开图标（点击展开），展开时显示收起图标。
        // 用自绘图标按钮（非 window_control_area，普通点击交互）。
        let sidebar_toggle = div()
            .id("title-sidebar-toggle")
            .debug_selector(|| "title-sidebar-toggle".into())
            .w(px(30.))
            .h(px(24.))
            .flex()
            .items_center()
            .justify_center()
            .rounded_md()
            .text_color(text_minor)
            .cursor(CursorStyle::PointingHand)
            .hover(|style| style.bg(self.bar_border()))
            .child(icon_style(if self.sidebar_open {
                IconName::PanelLeftClose
            } else {
                IconName::PanelLeftOpen
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this: &mut EditorApp, _ev, _window, cx| {
                    this.toggle_sidebar(cx);
                }),
            );

        let maximize_icon = if window.is_maximized() {
            IconName::WindowRestore
        } else {
            IconName::WindowMaximize
        };

        let minimize = div()
            .id("title-minimize")
            .w(px(30.))
            .h(px(24.))
            .flex()
            .items_center()
            .justify_center()
            .rounded_md()
            .text_color(text_minor)
            .cursor(CursorStyle::PointingHand)
            .child(icon_style(IconName::WindowMinimize))
            .window_control_area(gpui::WindowControlArea::Min);

        let maximize = div()
            .id("title-maximize")
            .w(px(30.))
            .h(px(24.))
            .flex()
            .items_center()
            .justify_center()
            .rounded_md()
            .text_color(text_minor)
            .cursor(CursorStyle::PointingHand)
            .child(icon_style(maximize_icon))
            .window_control_area(gpui::WindowControlArea::Max);

        let close = div()
            .id("title-close")
            .w(px(30.))
            .h(px(24.))
            .flex()
            .items_center()
            .justify_center()
            .rounded_md()
            .text_color(text_minor)
            .cursor(CursorStyle::PointingHand)
            .child(icon_style(IconName::WindowClose))
            .window_control_area(gpui::WindowControlArea::Close);

        div()
            .id("title-bar")
            .h(px(30.))
            .w_full()
            .flex()
            .items_center()
            .gap_1()
            .px_2()
            .bg(bar_bg)
            .border_b_1()
            .border_color(bar_border)
            .child(self.render_menu_buttons(cx))
            .child(
                div()
                    .id("title-drag")
                    .flex_1()
                    .h_full()
                    .window_control_area(gpui::WindowControlArea::Drag),
            )
            .children([
                sidebar_toggle.into_any_element(),
                minimize.into_any_element(),
                maximize.into_any_element(),
                close.into_any_element(),
            ])
    }
}

impl Render for EditorApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = if self.show_preview() {
            let ratio = self.split_ratio;
            // EPUB 等全屏预览：预览区占满整个内容区，不显示编辑区与分割线。
            let fullscreen = self
                .tabs
                .get(self.active_tab)
                .map(|t| t.preview_fullscreen)
                .unwrap_or(false);
            if fullscreen {
                div()
                    .id("app-fullscreen-preview")
                    .debug_selector(|| "app-fullscreen-preview".into())
                    .flex()
                    .flex_1()
                    .min_h(px(0.))
                    .child(self.render_fullscreen_preview(window, cx))
                    .into_any_element()
            } else {
                div()
                    .id("app-split-body")
                    .debug_selector(|| "app-split-body".into())
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_h(px(0.))
                    // 分割线拖拽：鼠标移动/松开事件冒泡到容器统一处理
                    .on_mouse_move(cx.listener(
                        |this: &mut EditorApp, ev: &gpui::MouseMoveEvent, window, cx| {
                            if let Some((start_x, start_ratio)) = this.split_drag {
                                let total = window.bounds().size.width.as_f32();
                                if total > 0. {
                                    let x = ev.position.x.as_f32();
                                    this.split_ratio = (start_ratio + (x - start_x) / total)
                                        .clamp(SPLIT_RATIO_MIN, SPLIT_RATIO_MAX);
                                    cx.notify();
                                }
                            }
                        },
                    ))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(
                            |this: &mut EditorApp, _: &gpui::MouseUpEvent, _w, cx| {
                                this.split_drag = None;
                                cx.notify();
                            },
                        ),
                    )
                    .on_mouse_up_out(
                        MouseButton::Left,
                        cx.listener(
                            |this: &mut EditorApp, _: &gpui::MouseUpEvent, _w, cx| {
                                this.split_drag = None;
                                cx.notify();
                            },
                        ),
                    )
                    .child(self.render_editor_pane(ratio, cx))
                    .child(self.render_splitter(cx))
                    .child(self.render_preview_pane(window, 1.0 - ratio, cx))
                    .into_any_element()
            }
        } else {
            div()
                .flex()
                .flex_1()
                .min_h(px(0.))
                .child(self.render_editor_pane(1.0, cx))
                .into_any_element()
        };

        div()
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .bg(self.window_bg())
            .track_focus(&self.focus_handle(cx))
            // 从资源管理器拖入文件到窗口任意处 → 打开每个文件（目录跳过）
            .can_drop(|drag: &dyn Any, _w, _c| drag.downcast_ref::<ExternalPaths>().is_some())
            .on_drop(cx.listener(
                |this: &mut EditorApp, paths: &ExternalPaths, window, cx| {
                    this.open_dropped_paths(paths, window, cx);
                },
            ))
            .on_action(cx.listener(|this: &mut EditorApp, _: &Save, window, cx| {
                this.save(window, cx)
            }))
            .on_action(
                cx.listener(|this: &mut EditorApp, _: &PasteNormalized, window, cx| {
                    this.paste_normalized(window, cx)
                }),
            )
            .on_action(cx.listener(|this: &mut EditorApp, _: &SaveAs, window, cx| {
                this.save_as(window, cx)
            }))
            .on_action(
                cx.listener(|this: &mut EditorApp, _: &NewFile, window, cx| {
                    this.new_tab(window, cx)
                }),
            )
            .on_action(
                cx.listener(|this: &mut EditorApp, _: &OpenFile, window, cx| {
                    this.open_dialog(window, cx)
                }),
            )
            .on_action(
                cx.listener(|this: &mut EditorApp, _: &CloseTab, window, cx| {
                    this.close_tab(this.active_tab, window, cx);
                }),
            )
            .on_action(
                cx.listener(|this: &mut EditorApp, _: &TogglePreview, window, cx| {
                    let idx = this.active_tab;
                    this.toggle_preview_for_tab(idx, window, cx);
                }),
            )
            .on_action(
                cx.listener(|this: &mut EditorApp, _: &IncreaseFontSize, _w, cx| {
                    this.change_font_size(1.0, cx)
                }),
            )
            .on_action(
                cx.listener(|this: &mut EditorApp, _: &DecreaseFontSize, _w, cx| {
                    this.change_font_size(-1.0, cx)
                }),
            )
            .on_action(
                cx.listener(|this: &mut EditorApp, _: &ResetFontSize, _w, cx| {
                    this.font_size = DEFAULT_FONT_SIZE;
                    cx.notify();
                }),
            )
            .on_action(
                cx.listener(|this: &mut EditorApp, _: &ToggleWordWrap, window, cx| {
                    this.toggle_wrap(window, cx);
                }),
            )
            .child(self.render_title_bar(window, cx))
            .child(self.render_tab_bar(cx))
            .child(self.render_body_sidebar(body, cx))
            .child(self.render_status_bar(cx))
            // 通知层（右上角 toast）：叠加在最上层，用于"无法读取文件"等提示
            .children(gpui_component::Root::render_notification_layer(window, cx))
    }
}
