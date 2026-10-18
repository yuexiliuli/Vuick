//! `EditorApp` 构造与窗口注册：实体创建/注册、初始主题与字号偏好落地、
//! 焦点句柄，以及命令行首参打开与自动化测试钩子（非渲染/预览职责）。

use super::*;

impl EditorApp {
    /// 组件编辑器内部统一使用 LF，避免 Windows 剪贴板 CRLF 进入文本布局。
    pub(super) fn normalize_newlines(text: &str) -> String {
        text.replace("\r\n", "\n").replace('\r', "\n")
    }

    /// 创建并注册 `EditorApp` 实体（作为窗口的第一层视图，外层再由
    /// `ui::root::Root` 宿主包裹）。
    /// P5：shell 独立 crate 后由二进制壳 `main.rs` 消费，`pub(crate)` → `pub`。
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> EditorApp {
        Self::new_impl(window, cx, None, false)
    }

    /// 「拖出拆分」创建独立文件窗口：首个选项卡直接采用拖出的选项卡快照。
    /// 返回的 `EditorApp` 标记为 `is_detached`，它的选项卡再被拖出 = 还原回主窗口。
    pub(crate) fn new_detached(
        window: &mut Window,
        cx: &mut Context<Self>,
        first_tab: DetachedTab,
    ) -> EditorApp {
        Self::new_impl(window, cx, Some(first_tab), true)
    }

    pub(super) fn new_impl(
        window: &mut Window,
        cx: &mut Context<Self>,
        first_tab: Option<DetachedTab>,
        is_detached: bool,
    ) -> EditorApp {
        let focus_handle = cx.focus_handle();
        // 侧边栏搜索输入框：内容变化 → 重刷 app（从而重算并展示搜索结果）
        let search = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("搜索文件与内容…")
                .default_value("")
        });
        let search_state = search.clone();
        // 侧边栏搜索输入框：内容变化 → 更新「防抖待提交查询」并即时刷新输入框渲染；
        // 防抖期满才提交给扫描（连续输入不再每键全量重扫大文档正文）。
        let search_sub = cx.subscribe(&search, |this, _emitter, _ev: &InputEvent, cx| {
            let value = this.search_state.read(cx).value().to_string();
            this.schedule_search_debounce(value, cx);
        });
        // 加载持久化用户偏好（主题三态 / 字号 / 侧边栏宽度；缺失或损坏 → 默认）。
        let prefs = UserPrefs::load();
        // 打开文件夹根路径（启动恢复文件树；`prefs` 随后被逐字段消费，先取出）。
        let opened_folder = prefs.opened_folder.clone();
        // 最近打开（文件夹 / 文件）：新→旧的路径列表，跨重启恢复「打开最近」面板。
        let recent_folders = prefs.recent_folders.clone();
        let recent_files = prefs.recent_files.clone();
        // 初始主题：按偏好解析（Follow → 当前系统明暗；Light/Dark → 强制）。
        let start_dark = prefs.theme.resolved_dark(cx.window_appearance());
        let start_font = prefs
            .font_size
            .map(|v| v.clamp(FONT_MIN, FONT_MAX))
            .unwrap_or(DEFAULT_FONT_SIZE);
        let mut app = EditorApp {
            tabs: Vec::new(),
            active_tab: 0,
            tab_bar_hscroll: TableScrollState::new(),
            tab_hscroll_delay: SeqAfter::default(),
            status: SharedString::from("就绪"),
            status_ok: true,
            notifications: Vec::new(),
            font_size: start_font,
            dark_mode: start_dark,
            // 手动切主题会退到强制档；此处仅为「跟随 / 强制」的偏好记录。
            theme_pref: prefs.theme,
            split_ratio: 0.5,
            // 编辑区软换行默认关闭；仅 Markdown 文件的编辑区默认换行（load_file_text 显式开启）。
            wrap_enabled: false,
            // 会话恢复开关：None（未设置）= 默认开。
            session_restore: prefs.session_restore.unwrap_or(true),
            // 会话持久化防抖（可重入、代序作废） / 恢复期间抑制标记 / 槽位指纹表（见 session 模块）。
            session_flush_debounce: SeqAfter::default(),
            session_restoring: false,
            session_slot_hashes: std::collections::HashMap::new(),
            session_last_manifest_digest: None,
            split_drag: None,
            splitter_hovered: false,
            sidebar: sidebar::SidebarChromeState {
                open: true,
                view: sidebar::SidebarView::Toc,
                // 目录面板默认收起（仅留 46px 活动栏，不挤占正文）；
                // 点活动栏「目录」按钮再展开。
                panel_open: false,
                toc_collapsed: Default::default(),
                toc_tab: None,
                toc_vscroll: gpui::ScrollHandle::new(),
                search_vscroll: gpui::ScrollHandle::new(),
                files: sidebar::FileTreeState::default(),
                files_layout: None,
                files_vscroll: gpui::ScrollHandle::new(),
                toc_width: None,
                toc_cache: None,
                // 有持久化宽度 → 恢复并固定为手动宽度；否则保持随内容自适应。
                width: prefs.sidebar_width.unwrap_or(SIDEBAR_WIDTH),
                width_auto: prefs.sidebar_width.is_none(),
                resize: None,
                resize_hovered: false,
            },
            search_state,
            _search_sub: search_sub,
            _pdf_observe: None,
            pdf_search: None,
            search_scan: None,
            search_scan_count: 0,
            search_committed: String::new(),
            search_pending: String::new(),
            search_debounce: SeqAfter::default(),
            open_fast_count: 0,
            open_finish_count: 0,
            open_dialog_calls: 0,
            last_title_ctrl_fg: None,
            tab_dragging: None,
            is_detached,
            media_lock_menu_pos: None,
            media_lock_menu_entity: None,
            media_lock_menu_sub: None,
            tab_bar_menu_pos: None,
            tab_bar_menu_entity: None,
            tab_bar_menu_sub: None,
            recent_folders,
            recent_files,
            recent_panel: None,
            _recent_panel_sub: None,
            focus_handle,
            md_scroll_sync: MdScrollSync::default(),
            _appearance_sub: None,
            palette: None,
            _palette_sub: None,
            // A：旧 live2 实体延迟回收队列（`commit_live` 暂存、下一帧 on_next_frame 清空）。
            retired_live2: Vec::new(),
        };
        app.register_window(window, cx);
        // 打开文件夹持久化 → 启动恢复：prefs.opened_folder 存在且目录仍有效 → 设为
        // 文件树根 + 展开根（后台读根直接子项）+ 侧边栏切到「文件」视图展开。
        // 路径失效 / 不存在 → 不设根，首次 `persist_prefs` 时清掉（写 null）。
        if let Some(folder) = opened_folder {
            let p = PathBuf::from(&folder);
            if p.is_dir() {
                app.sidebar.files.set_root(Some(p.clone()));
                app.sidebar.files.expanded.insert(p.clone());
                app.spawn_file_tree_scan(p, window, cx);
                app.sidebar.view = sidebar::SidebarView::Files;
                app.sidebar.open = true;
                app.sidebar.panel_open = true;
            } else {
                // 根失效：files.root 保持 None，persist_prefs 把 opened_folder 写回 null。
                app.persist_prefs();
            }
        }
        // 把解析出的初始主题落地到全局 ui::theme（覆盖 main.rs 写死的基准主题，
        // 首帧前完成，避免「先亮后暗」闪一下）。
        ui::theme::Theme::set(
            if app.dark_mode {
                ui::theme::ThemeMode::Dark
            } else {
                ui::theme::ThemeMode::Light
            },
            cx,
        );
        // 运行中跟随系统深浅色：事件订阅（Windows 经 WM_SETTINGCHANGE 驱动）。
        // 仅在「跟随」模式生效；强制档下外观变化被忽略，切回跟随由
        // `follow_system_theme` 立即按当前系统落地。
        let weak = cx.weak_entity();
        app._appearance_sub = Some(window.observe_window_appearance(
            move |_window: &mut Window, cx: &mut App| {
                if let Some(entity) = weak.upgrade() {
                    entity.update(cx, |this, cx| {
                        let dark = matches!(
                            cx.window_appearance(),
                            gpui::WindowAppearance::Dark | gpui::WindowAppearance::VibrantDark
                        );
                        if this.theme_pref == ThemePref::Follow && this.dark_mode != dark {
                            this.dark_mode = dark;
                            this.refresh_theme_ui(cx);
                        }
                    });
                }
            },
        ));
        if let Some(first_tab) = first_tab {
            // 拆出/拖拽产生的独立窗口：首个选项卡直接收纳（不额外建空选项卡）。
            app.adopt_tab(first_tab, Some(window), cx);
        }
        // 不再给普通窗口预建初始空选项卡：新模型下启动即「零选项卡 → 欢迎页」，
        // 只有用户点了「新建文件」、双击欢迎页或打开文件（内部 `open_file` → `make_tab`）
        // 才出现第一个选项卡（见 `render_body`：`tabs.is_empty()` 整屏欢迎页）。

        // 命令行参数：第一个参数作为初始打开文件（`open_file` 新建一个选项卡载入；
        // 之前的「复用首个空选项卡」随零 tab 启动模型一并移除——效果等价，都是只留
        // 打开的那个文件选项卡）。**显式打开文件时跳过会话恢复**（显式意图优先）。
        if !is_detached {
            let mut args = std::env::args_os();
            args.next(); // 跳过程序名
            let mut opened_cli_file = false;
            if let Some(first) = args.next() {
                let p = PathBuf::from(&first);
                if p.is_file() {
                    app.open_file(&p.to_string_lossy(), window, cx);
                    opened_cli_file = true;
                }
            }
            // 无显式打开文件才尝试恢复上次会话（开关开启 + 清单存在且有效才恢复；
            // 测试构建不触碰真实会话目录，见 session 模块）。
            if !opened_cli_file {
                #[cfg(not(test))]
                {
                    app.restore_session(window, cx);
                }
            }
            // 自动化测试钩子（与 MD_OPEN_DEBUG 一类调试入口同款）：仅当显式用 CLI
            // 打开文件后立即开启「本项目 WYSIWYG（预览编辑测试）」左面板，便于截图/识图验证。
            if opened_cli_file
                && std::env::var_os("WY_AUTOTEST").is_some()
                && app.is_markdown_tab(app.active_tab)
            {
                app.toggle_live_test(window, cx);
            }
        }
        // 退出钩子：仅**主窗口**（is_detached == false）实体注册——退出前把主窗口的
        // 选项卡集合保存到会话目录，覆盖全部退出路径（菜单退出 / Ctrl+Q / 命令面板 /
        // 点窗口关闭）。detach 拆分窗口不参与会话持久化/恢复（见 session 模块注释）。
        // 已核实本平台/GPUI 版本下最后一个窗口关闭触发 quit 不走 `App::shutdown()` →
        // `on_app_quit`（退出钩子在此是死路），故本钩子仅作**兜底**：万一某平台确实
        // 触发仍做最终同步落盘（无害）。常态保存依赖「状态变化即持久化」的连续落盘。
        if !is_detached {
            #[cfg(not(test))]
            {
                cx.on_app_quit(|this, cx| {
                    this.save_session(cx);
                    async {}
                })
                .detach();
                // 关窗默认保存：不再注册 `on_window_should_close` 询问钩子——临时文件
                // 由「状态变化即持久化」的连续落盘常态写在盘上，点窗口 × 直接关闭，
                // 下次启动原样恢复（见 session 模块注释）。
            }
        }
        app
    }

    /// 把本窗口登记进全局 `EditorAppRegistry`（惰性清理已关闭窗口的失效项）。
    pub(super) fn register_window(&self, window: &mut Window, cx: &mut Context<Self>) {
        if !cx.has_global::<EditorAppRegistry>() {
            cx.set_global(EditorAppRegistry::default());
        }
        let weak = cx.weak_entity();
        let handle = window.window_handle();
        let registry = cx.global_mut::<EditorAppRegistry>();
        registry.windows.retain(|(weak, _)| weak.upgrade().is_some());
        registry.windows.push((weak, handle));
    }

    /// 选项卡的编辑焦点句柄：预览编辑模式返回 WYSIWYG 编辑器（否则键盘进
    /// 不可见的源码编辑区）；否则返回源码编辑区。
    pub(super) fn tab_focus_handle(&self, idx: usize, cx: &App) -> FocusHandle {
        let Some(tab) = self.tabs.get(idx) else {
            return self.focus_handle.clone();
        };
        if tab.live_edit
            && let Some(l2) = &tab.live2 {
                let h = gpui::Focusable::focus_handle(l2.read(cx), cx);
                return h;
            }
        tab.editor.read(cx).focus_handle()
    }

    /// 当前激活选项卡的编辑区焦点句柄（供入口聚焦，二进制壳 `main.rs` 消费）。
    pub fn editor_focus_handle(&self, cx: &App) -> FocusHandle {
        self.tab_focus_handle(self.active_tab, cx)
    }
}
