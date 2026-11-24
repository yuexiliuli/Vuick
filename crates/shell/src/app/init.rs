//! `EditorApp` 构造与窗口注册：实体创建/注册、初始主题与字号偏好落地、
//! 焦点句柄，以及命令行首参打开与自动化测试钩子（非渲染/预览职责）。

use super::*;

impl EditorApp {
    /// 创建并注册 `EditorApp` 实体（作为窗口的第一层视图，外层再由
    /// `ui::root::Root` 宿主包裹）。
    /// shell 独立 crate 后由二进制壳 `main.rs` 消费，`pub(crate)` → `pub`。
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> EditorApp {
        Self::new_impl(window, cx, None, false, true)
    }

    /// 「拖出拆分」创建独立文件窗口：首个选项卡直接采用拖出的选项卡快照。
    /// 返回的 `EditorApp` 标记为 `is_detached`，它的选项卡再被拖出 = 还原回主窗口。
    pub(crate) fn new_detached(
        window: &mut Window,
        cx: &mut Context<Self>,
        first_tab: DetachedTab,
    ) -> EditorApp {
        Self::new_impl(window, cx, Some(first_tab), true, false)
    }

    /// 「媒体在新窗口打开」创建的独立媒体窗口：普通拖拽语义（`is_detached=false`），
    /// 但**不承担**启动/退出职责（不读 CLI 首参、不恢复会话、不挂退出/托盘钩子）——这些
    /// 职责只属于首个主窗口。媒体由调用方在窗口创建后经 `open_file_here` 补开（media_window.rs）。
    /// `#[doc(hidden)] pub`：`tests/media.rs` 新媒体窗口尺寸记录断言（§8.6 外提清单）。
    #[doc(hidden)]
    pub fn new_media_window(window: &mut Window, cx: &mut Context<Self>) -> EditorApp {
        Self::new_impl(window, cx, None, false, false)
    }

    pub(super) fn new_impl(
        window: &mut Window,
        cx: &mut Context<Self>,
        first_tab: Option<DetachedTab>,
        is_detached: bool,
        startup: bool,
    ) -> EditorApp {
        let focus_handle = cx.focus_handle();
        // 宿主实体自身的弱句柄与 id（AppHost `&self` 服务经它再入宿主实体）。
        let own_weak = cx.weak_entity();
        let own_entity_id = cx.entity_id();
        // 侧边栏搜索输入框：内容变化 → 重刷 app（从而重算并展示搜索结果）。
        // **输入实体为域自有态**（`shell_sidebar::Sidebar::search_input`），此处创建后
        // 传入实体构造；宿主只经 `search_state(cx)` 兼容读取器取句柄做订阅/粘贴。
        let search = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("搜索文件与内容…")
                .default_value("")
        });
        // 侧边栏搜索输入框：内容变化 → 更新「防抖待提交查询」并即时刷新输入框渲染；
        // 防抖期满才提交给扫描（连续输入不再每键全量重扫大文档正文）。订阅仍挂宿主
        // （`this.search_state(cx)` 取实体句柄），转发到域实体 `schedule_search_debounce`。
        let search_sub = cx.subscribe(&search, |this, _emitter, _ev: &InputEvent, cx| {
            let value = this.search_state(cx).read(cx).value().to_string();
            this.schedule_search_debounce(value, cx);
        });
        // 加载持久化用户偏好（主题三态 / 字号 / 侧边栏宽度；缺失或损坏 → 默认）。
        let prefs = UserPrefs::load();
        // 文件树「过滤文件名」输入框：内容变化 → 登记防抖待提交过滤词（typeahead）。
        let files_filter = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("过滤文件名…")
                .default_value("")
        });
        let files_filter_sub =
            cx.subscribe(&files_filter, |this, _emitter, _ev: &InputEvent, cx| {
                let value = this.files_filter_state.read(cx).value().to_string();
                this.schedule_files_filter_debounce(value, cx);
            });
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
        // 模型半状态统一容器在 shell_core::AppModel——启动初值在此**一次性汇集**
        // 交给 `AppModel::new`（语义与 `new_impl` 的同名字段一致；零选项卡 +
        // active_tab=0 起手，其余派生状态取模型内默认）。
        let model = Rc::new(RefCell::new(AppModel::new(
            // 顶部状态文字（状态栏）：默认「就绪 / 正常」。
            SharedString::from("就绪"),
            true,
            // 编辑器正文字号（按偏好 clamp 到允许区间）。
            start_font,
            // 初始主题：按偏好解析（Follow → 当前系统明暗；Light/Dark → 强制）。
            start_dark,
            // 手动切主题会退到强制档；此处仅为「跟随 / 强制」的偏好记录。
            prefs.theme,
            // 全站主题（人格预设 / 自定义覆盖 / 音乐皮肤）：随偏好恢复，refresh_theme_ui 消费。
            prefs.theme_preset.clone(),
            prefs.theme_custom.clone(),
            prefs.music_skin.clone(),
            // PDF 画布背景偏好：启动即随配置恢复（默认跟随主题；显式档在打开时冻结）。
            prefs.pdf_bg,
            prefs.pdf_bg_custom.clone(),
            // 会话恢复 / 托盘 / 媒体打开位置开关：None（未设置）= 默认开/关。
            prefs.session_restore.unwrap_or(true),
            prefs.close_to_tray.unwrap_or(false),
            prefs.media_new_window.unwrap_or(true),
            prefs.reuse_media_window.unwrap_or(true),
            // 「记住阅读进度」开关：None = 默认开。
            prefs.restore_reading_progress.unwrap_or(true),
            // 媒体窗口记录尺寸：None（未记录）= 新开跟随发起窗口大小。
            prefs.music_window_size.map(|(w, h)| gpui::size(px(w), px(h))),
            prefs.video_window_size.map(|(w, h)| gpui::size(px(w), px(h))),
            // 主窗口记录尺寸：None（未记录）= 启动恢复用默认 980×720。
            prefs
                .main_window_size
                .map(|(w, h)| gpui::size(px(w), px(h))),
            // 最近打开（文件夹 / 文件）：新→旧的路径列表，跨重启恢复「打开最近」面板。
            recent_folders,
            recent_files,
            // A4：PDF 书签进程级读盘一次（`load_cached`；每窗构造都读盘是浪费）。
            shell_pdf::bookmarks::load_cached(),
            // 阅读进度进程级读盘一次（路径守卫缓存；`VUICK_TEST` 下 .tmp 隔离目录）。
            session_store::reading_progress::load_cached(&AppModel::reading_progress_path()),
        )));
        // 侧边栏渲染/事件实体（shell_sidebar 域 crate）：经共享 model + 宿主弱引用
        // + 搜索输入实体（域自有态）构造；宿主渲染期以 `&mut self.sidebar` 入参调用其
        // 外壳/目录/搜索/文件树渲染。
        let sidebar_view = cx.new(|_| {
            shell_sidebar::Sidebar::new(model.clone(), own_weak.clone(), search.clone())
        });
        // 选项卡栏渲染/事件实体（shell_tabs 域 crate）：共享 model + 宿主弱引用构造；
        // 宿主渲染期以 `&mut self.tab_bounds`（拖拽命中槽）/ `&self.tab_bar_hscroll` 入参
        // 调用其 render_tab_bar，事件回调经 TabsHost 反调宿主。
        let tab_bar_view = cx.new(|_| shell_tabs::TabBar::new(model.clone(), own_weak.clone()));
        let mut app = EditorApp {
            model,
            sidebar_view,
            tab_bar_view,
            tab_bar_hscroll: TableScrollState::new(),
            tab_hscroll_delay: SeqAfter::default(),
            notifications: Vec::new(),
            // 分割条面板状态（PanesState 在 shell_core）：split_ratio=0.5 / split_drag / 
            // pending_split_ratio / splitter_hovered 四态收敛为一字段，默认值同旧逐字段。
            panes: PanesState::default(),
            sidebar: sidebar::SidebarChromeState {
                open: true,
                // 视频自动隐藏的手动打开覆盖默认关闭（视频激活即藏）。
                video_hide_override: false,
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
            _search_sub: search_sub,
            open_fast_count: 0,
            open_finish_count: 0,
            open_dialog_calls: 0,
            last_title_ctrl_fg: None,
            title_ctrl_fg_probe: std::rc::Rc::new(std::cell::RefCell::new(
                TitleCtrlIconFg::default(),
            )),
            material_layer_cache: None,
            tab_bounds: Vec::new(),
            drag_ghost: None,
            constructing: false,
            drag_ghost_pending: false,
            is_detached,
            // 会话持久化责任：仅首个主窗口（`startup` 仅 `new` 传 true；new_media_window /
            // new_detached 传 false）——会话清单单份全局，非主窗口参与写会顶替主窗口会话。
            session_persist: startup,
            media_lock_menu: Default::default(),
            pdf_menu: Default::default(),
            tab_bar_menu: Default::default(),
            file_tree_menu_entity: None,
            file_tree_prompt: None,
            files_filter_state: files_filter,
            _files_filter_sub: files_filter_sub,
            recent_panel: None,
            _recent_panel_sub: None,
            focus_handle,
            md_scroll_sync: MdScrollSync::default(),
            rp_last: None,
            _appearance_sub: None,
            // 专用媒体窗口尺寸变化订阅：仅 `new_media_window` 构造并在下方注册（未注册 =
            // None，见 `!startup && !is_detached` 分支）。
            _bounds_sub: None,
            // 主窗口尺寸变化订阅：仅 `startup` 构造并在下方注册（未注册 = None，见
            // `if startup` 分支）。
            _main_bounds_sub: None,
            palette: None,
            _palette_sub: None,
            about_panel: None,
            _about_sub: None,
            // 音乐播放器视图（惰性创建；退出音乐模式不销毁 MusicView——播放继续）。
            music: None,
            _music_sub: None,
            own_weak,
            own_entity_id,
        };
        app.register_window(window, cx);
        // 打开文件夹持久化 → 启动恢复：**仅当「恢复上次会话」开启时生效**（与恢复上次
        // 选项卡同一开关——关闭恢复 = 干净启动，不再自动重开上次打开的文件夹）。
        // opened_folder 仍持久化，手动打开文件夹 / 导入设置不受此门控影响。
        // 路径失效 / 不存在 → 不设根，首次 `persist_prefs` 时清掉（写 null）。
        let restore_folder = opened_folder.filter(|_| app.model.borrow().session_restore);
        if let Some(folder) = restore_folder {
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
        // 守卫 =「有效主题」是否相对当前全局有变化：明暗档 / 人格预设 / 自定义覆盖任一维。
        // Theme::apply 内部会 `refresh_windows`（入队 RefreshWindows 效果），构造期入队的
        // 该效果会在 `open_window` 同步首帧**之后**才 flush → 新窗口白画一整帧冗余重绘
        //（大档拆窗松手延时的大头之一）。跳过不丢视觉（同主题同色板）。
        // P1-3：「默认亮档 + 自定义重点色」重启时暗档相同但 custom 非空 → 仍须 apply
        //（旧守卫只看 is_dark 会跳过，preset/custom 不生效）。
        let theme_changed = {
            let model = app.model.borrow();
            let preset_is_default = ui::theme::PRESETS
                .iter()
                .find(|p| p.id == model.theme_preset)
                .map(|p| p.id == "default")
                .unwrap_or(true);
            ui::theme::Theme::global(cx).is_dark() != model.dark_mode
                || !preset_is_default
                || !model.theme_custom.is_empty()
        };
        if theme_changed {
            app.refresh_theme_ui(cx);
        }
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
                        // 先提纯 Boolean 再进 if（条件里的 borrow 临时 Ref 存活整个
                        // 分支 → 块内 borrow_mut 运行期双借 panic）。
                        let follow = this.model.borrow().theme_pref == ThemePref::Follow;
                        let not_equals_dark = this.model.borrow().dark_mode != dark;
                        if follow && not_equals_dark {
                            this.model.borrow_mut().dark_mode = dark;
                            this.refresh_theme_ui(cx);
                        }
                    });
                }
            },
        ));
        // 专用媒体窗口（`new_media_window` 创建 = `!startup && !is_detached`）尺寸变化订阅：
        // 用户 resize 后按当前角色自动记录音乐/视频窗口尺寸到 config.json（下次新开按记录
        // 尺寸）。主窗口 / 拖出 / 复用目标不注册（不记录，尺寸各自沿用自身，见 media_window.rs）。
        // 最大化/全屏不记录（尺寸随系统态跳变无意义）；重复 resize 去重在 `record_media_window_size`。
        if !startup && !is_detached {
            app._bounds_sub = Some(cx.observe_window_bounds(window, |this, window, cx| {
                if window.is_maximized() || window.is_fullscreen() {
                    return;
                }
                this.record_media_window_size(window.bounds().size, cx);
            }));
        }
        // 主窗口（`startup` 创建）尺寸变化订阅：用户 resize 后记录窗口化尺寸到
        // config.json（下次启动按记录尺寸居中打开，见 `src/main.rs` 启动恢复）。
        // 专精媒体窗口 / 拖出窗口不注册（不记录，各自尺寸语义见 media_window.rs）。
        // 最大化/全屏不记录（尺寸随系统态跳变无意义）；重复 resize 去重在
        // `record_main_window_size`；首帧若回调一次初始尺寸 → 去重（与记录相同不写）无害。
        if startup {
            app._main_bounds_sub = Some(cx.observe_window_bounds(window, |this, window, cx| {
                if window.is_maximized() || window.is_fullscreen() {
                    return;
                }
                this.record_main_window_size(window.bounds().size, cx);
            }));
        }
        if let Some(first_tab) = first_tab {
            // 拆出/拖拽产生的独立窗口：首个选项卡直接收纳（不额外建空选项卡）。
            // 构造期抑制 adopt/activate 的 `cx.notify()`（存入 effect 队列）——它们会在
            // `open_window` 同步首帧之后才 flush，白画一整帧冗余；构造后紧接同步首帧，
            // 无需这些预渲染 notifies（见字段 `constructing` 注释）。
            app.constructing = true;
            app.adopt_tab(first_tab, Some(window), cx);
            app.constructing = false;
        }
        // 不再给普通窗口预建初始空选项卡：新模型下启动即「零选项卡 → 欢迎页」，
        // 只有用户点了「新建文件」、双击欢迎页或打开文件（内部 `open_file` → `make_tab`）
        // 才出现第一个选项卡（见 `render_body`：`tabs.is_empty()` 整屏欢迎页）。

        // 命令行参数：第一个参数作为初始打开文件（`open_file` 新建一个选项卡载入；
        // 之前的「复用首个空选项卡」随零 tab 启动模型一并移除——效果等价，都是只留
        // 打开的那个文件选项卡）。**显式打开文件时跳过会话恢复**（显式意图优先）。
        // 仅首个主窗口（`startup=true`）执行；`new_detached` / `new_media_window` 不读
        // CLI 首参、不恢复会话——否则媒体重定向到新窗口后，新窗口会再次读同一 CLI 参数
        // 造成重定向循环，且会重复恢复全量会话（media_window 侧设计决策，见该模块）。
        if startup {
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
            // 原「WY_AUTOTEST：CLI 打开后立即开启 WYSIWYG」自动化测试钩子已随
            // 「md 打开默认进入 WYSIWYG」移除（2026-11-18）：小文档在 `open_file_here`
            // 同步进入、大文档在 `finish_file_text` 异步进入（同一真源 `enter_live_for_tab`），
            // 无需 env 门控；留着会在已进入的 tab 上再走 `toggle_live_test` 的 OFF 分支
            // （live2.is_some() → commit_live）反而退出——语义反向，故整段删除。
        }
        // 启动首帧墙钟（VUICK_STARTUP_DIAG=1 门控，默认零开销零行为）：
        // A1 后本段 ≈ `EditorApp::new` 结束段——`restore_session` 只完成「读清单 +
        // 登记首帧后渐进恢复」，**首帧前不做任何逐选项卡打开**（恢复在首帧 render
        // 之后按帧逐个进行，见 session.rs `schedule_restore`／`restore_step`）。
        if startup {
            base::startup::mark("restore");
        }
        // 退出钩子：仅**主窗口**（is_detached == false）实体注册——退出前把主窗口的
        // 选项卡集合保存到会话目录，覆盖全部退出路径（菜单退出 / Ctrl+Q / 命令面板 /
        // 点窗口关闭）。detach 拆分窗口不参与会话持久化/恢复（见 session 模块注释）。
        // 已核实本平台/GPUI 版本下最后一个窗口关闭触发 quit 不走 `App::shutdown()` →
        // `on_app_quit`（退出钩子在此是死路），故本钩子仅作**兜底**：万一某平台确实
        // 触发仍做最终同步落盘（无害）。常态保存依赖「状态变化即持久化」的连续落盘。
        if startup {
            #[cfg(not(test))]
            {
                cx.on_app_quit(|this, cx| {
                    this.save_session(cx);
                    // 优雅清理系统托盘（best-effort；进程退出系统也会自动清托盘图标）。
                    tray::shutdown();
                    async {}
                })
                .detach();
                // 关窗默认保存：不再注册 `on_window_should_close` 询问钩子——临时文件
                // 由「状态变化即持久化」的连续落盘常态写在盘上，点窗口 × 直接关闭，
                // 下次启动原样恢复（见 session 模块注释）。
                // 关闭到托盘接线（仅主窗口；detached 拆分窗口不参与托盘隐藏/恢复）：
                // 1) 提取主窗口原生 HWND 交给 tray crate 记录（0 = 未知）；
                // 2) 按当前开关建/撤托盘图标（默认关仅清状态不建图标）；
                // 3) 注册 `should_close` 钩子：开关开且非强制退出 → 隐藏到托盘并拦下关闭
                //    （返回 false）；否则放行真正退出。显式退出路径（Ctrl+Q / 菜单退出 /
                //    命令面板 → cx.quit，本平台走 PostQuitMessage 不触发 should_close）
                //    天然不受影响。
                #[cfg(windows)]
                {
                    use raw_window_handle::HasWindowHandle as _;
                    if let Ok(handle) = window.window_handle()
                        && let raw_window_handle::RawWindowHandle::Win32(win32) = From::from(handle)
                    {
                        tray::set_main_hwnd(win32.hwnd.get());
                    }
                    tray::set_close_to_tray(app.model.borrow().close_to_tray);
                    // A5：托盘图标 / 程序窗口图标 **defer 到首帧后**（窗口先显示、图标随后
                    // 毫秒级出现）。`add_tray_icon`（建隐藏消息窗口 + 解码 PNG +
                    // Shell_NotifyIconW）与 `WM_SETICON` 不再挡首帧；`set_main_hwnd` /
                    // `set_close_to_tray` / `on_window_should_close` 是纯 flag + 注册（零
                    // 窗口/图标成本），保持首帧前同步。图标在此注册，行为 = 稍晚出现。
                    // 托盘图标常驻（用户要求：任务栏 + 系统托盘都用变色龙；不随开关消失）。
                    window.on_next_frame(move |_window, _cx| {
                        tray::ensure_tray_icon();
                        // 程序图标：运行时对主窗口 HWND 发 WM_SETICON（任务栏 / Alt+Tab /
                        // 标题栏）= icon-chameleon_64x64.ico 图标（tray::APP_ICO，与托盘共用）。
                        tray::set_window_icon();
                    });
                    window.on_window_should_close(cx, |_window, _cx| {
                        if tray::should_close_to_tray() {
                            tray::hide_main_window();
                            false
                        } else {
                            // 真正关闭主窗口 = 应用即将退出 → 先移除托盘图标（NIM_DELETE），
                            // 杜绝「幽灵图标」残留：本平台最后窗口退出不走 on_app_quit（K9），
                            // 若 NIM_DELETE 不挂在此，每次启动留下的图标都不清、鼠标划过才消。
                            tray::shutdown();
                            true
                        }
                    });
                }
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

    /// 选项卡的编辑焦点句柄：逻辑在 `AppModel::tab_focus_handle`
    /// （shell_core::actions，模型直读）；此处 glue 转发。
    pub(super) fn tab_focus_handle(&self, idx: usize, cx: &App) -> FocusHandle {
        let host: &dyn AppHost = self;
        self.model.borrow().tab_focus_handle(idx, host, cx)
    }

    /// 当前激活选项卡的编辑区焦点句柄（供入口聚焦，二进制壳 `main.rs` 消费）。
    pub fn editor_focus_handle(&self, cx: &App) -> FocusHandle {
        let active_idx = self.model.borrow().active_tab;
        self.tab_focus_handle(active_idx, cx)
    }
}
