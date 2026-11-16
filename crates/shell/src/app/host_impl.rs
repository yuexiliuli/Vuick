//! `impl AppHost for EditorApp`——宿主服务面的 shell 侧实现。
//!
//! 借用约束（见 `shell_core::host` 模块注释）：`AppHost` 方法一律 `&self`、实现**不得
//! 同步**读/写 `AppModel`（动作方法运行时宿主正持 `AppModel` 可变借用）；凡回调体需读
//! 模型（订阅 / 延迟），把模型读写放进**回调体**再经 `own_weak` 再入宿主实体——此刻
//! 动作方法的模型借用已结束。纯取值方法（`is_detached` 等）只读宿主字段，安全。

use super::*;
use shell_core::AppHost;

impl AppHost for EditorApp {
    fn self_entity_id(&self) -> EntityId {
        self.own_entity_id
    }

    fn self_weak(&self) -> gpui::AnyWeakEntity {
        self.own_weak.clone().into()
    }

    fn notify_host(&self, cx: &mut App) {
        cx.notify(self.own_entity_id);
    }

    fn is_constructing(&self) -> bool {
        self.constructing
    }

    fn is_detached(&self) -> bool {
        self.is_detached
    }

    fn is_session_persist(&self) -> bool {
        self.session_persist
    }

    fn root_focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }

    fn subscribe_editor(
        &self,
        editor: &gpui::Entity<editor::CodeEditor>,
        cx: &mut App,
    ) -> Subscription {
        let weak = self.own_weak.clone();
        let ed = editor.clone();
        cx.subscribe(&ed, move |emitter, ev: &editor::EditorEvent, cx| {
            // 事件回调触发时动作方法的模型借用早已结束：经自身弱句柄再入宿主实体，
            // 执行与 `subscribe_editor` 一致的订阅体。
            let _ = weak.update(cx, |this, cx| {
                let pos = this.model.borrow().tabs.iter().position(|t| t.payload.editor == emitter);
                if let Some(t) = pos {
                    match ev {
                        editor::EditorEvent::Edited => this.on_editor_edited(t, cx),
                        // 纯光标/选区移动（鼠标点击/方向键/搜索跳转）不走重活，
                        // 只刷新选项卡缓存的行/列 → 状态栏 `Ln x, Col y` 即时跟随。
                        editor::EditorEvent::SelectionChanged => {
                            this.on_editor_selection_changed(t, cx)
                        }
                        _ => {}
                    }
                }
            });
        })
    }

    fn files_root(&self) -> Option<std::path::PathBuf> {
        self.sidebar.files.root.clone()
    }

    fn sidebar_width_manual(&self) -> Option<f32> {
        if self.sidebar.width_auto {
            None
        } else {
            Some(self.sidebar.width)
        }
    }

    /// 「测试构建」运行时注入：integration 测试目标下 shell lib 以
    /// **非 test** 编译 → `cfg!(test)` 恒 false，`persist_session`/`persist_prefs` 会
    /// 真写 `%APPDATA%`（K8 复发）。正解 = 由各 integration 目标 `tests/common/mod.rs`
    /// 的 `init_test()` 在 gpui App 启动前设 `VUICK_TEST=1`，此处运行时判定：
    /// lib 单测仍走 `cfg!(test)`；integration 走 env；生产不设该 env → 恒 false。
    fn is_test_build(&self) -> bool {
        // 只认字面 "1"（fail-closed 到生产行为）：CI/容器/共享 shell 残留 export 或意外值
        // （"false"/"0"/"off"…）一律按非测试态处理，防 persist_session/prefs 被静默关停（丢设置）。
        cfg!(test) || matches!(std::env::var("VUICK_TEST").as_deref(), Ok("1"))
    }
}

// 侧栏域 crate 宿主服务面（shell_sidebar）。事件回调改侧栏状态统一经此
// trait 落地；宿主实现只调 `shell_sidebar::state` 的**状态纯变换** + notify/persist，或
// 转发宿主既有文件侧副作用（打开/激活/新建/重命名/删除/剪贴板/探索器），零业务逻辑。
impl shell_sidebar::SidebarHost for EditorApp {
    fn sidebar_toggle_view(&mut self, view: crate::app::SidebarView, cx: &mut Context<Self>) {
        self.sidebar.toggle_view(view);
        cx.notify();
    }

    fn sidebar_set_resize_hovered(&mut self, hovered: bool, cx: &mut Context<Self>) {
        self.sidebar.resize_hovered = hovered;
        cx.notify();
    }

    fn sidebar_resize_start(&mut self, start_x: f32, cx: &mut Context<Self>) {
        shell_sidebar::state::resize_start(&mut self.sidebar, start_x);
        cx.notify();
    }

    fn sidebar_toc_collapse(&mut self, idx: usize, has_child: bool, cx: &mut Context<Self>) {
        if shell_sidebar::state::toc_collapse(&mut self.sidebar, idx, has_child) {
            cx.notify();
        }
    }

    fn sidebar_remove_pdf_bookmark(
        &mut self,
        path: &std::path::Path,
        idx: usize,
        cx: &mut Context<Self>,
    ) {
        let _ = self.remove_pdf_bookmark(path, idx, cx);
    }

    /// 打开/激活文件：宿主 `open_file`/`activate_tab` 能力（file_ops.rs 的既有实现）。
    fn sidebar_open_path_or_activate(
        &mut self,
        path: &std::path::Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_path_or_activate(path, window, cx);
    }

    /// 「搜索」文件名命中/行命中点击「激活已有选项卡」：宿主 `activate_tab`（选项卡集合、
    /// 活动索引、脏标记等属宿主 panes/tab-bar 域；文件树「打开」经
    /// `sidebar_open_path_or_activate`，此处是按下标激活已打开文档，两者语义区分）。
    fn sidebar_activate_tab(&mut self, tab_idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.activate_tab(tab_idx, window, cx);
    }

    /// 目录行展开/折叠（生产事件入口）：转发宿主 facade `file_tree_toggle`——宿主侧
    /// 直落 `state::file_tree_toggle` 纯变换 + 首展宿主侧 spawn 扫描，全程不回租 Sidebar
    /// 实体（目录行 on_click 回调期间实体已租用，回租即 `double_lease_panic`；
    /// 与 toc_collapse 同构）。
    fn sidebar_file_tree_toggle(
        &mut self,
        path: std::path::PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.file_tree_toggle(path, window, cx);
    }

    /// 后台扫描完成落地：`shell_sidebar::state::file_tree_scan_complete` 纯变换 + notify。
    fn sidebar_file_tree_scan_complete(
        &mut self,
        path: std::path::PathBuf,
        entries: Vec<shell_sidebar::TreeEntry>,
        cx: &mut Context<Self>,
    ) {
        shell_sidebar::state::file_tree_scan_complete(&mut self.sidebar.files, &path, entries);
        cx.notify();
    }

    /// 过滤防抖提交：模型过滤词已由域实体写入，此处只作文件树布局失效 + notify。
    fn sidebar_files_filter_commit(&mut self, cx: &mut Context<Self>) {
        self.sidebar.files.layout_gen = self.sidebar.files.layout_gen.wrapping_add(1);
        cx.notify();
    }

    /// 右键菜单句柄镜像同步（实体侧置入/清除后调用）：写测试兼容镜像 + notify。
    fn sidebar_set_file_tree_menu(
        &mut self,
        menu: Option<gpui::Entity<ui::menu::PopupMenu>>,
        cx: &mut Context<Self>,
    ) {
        self.file_tree_menu_entity = menu;
        cx.notify();
    }

    /// 输入弹层句柄镜像同步（实体侧置入/清除后调用）：写测试兼容镜像 + notify。
    fn sidebar_set_file_tree_prompt(
        &mut self,
        prompt: Option<gpui::Entity<shell_sidebar::FileTreePrompt<EditorApp>>>,
        cx: &mut Context<Self>,
    ) {
        self.file_tree_prompt = prompt;
        cx.notify();
    }

    /// 打开文件树输入弹层（菜单「新建/重命名…」）：转发宿主 facade（实体建弹层+聚焦）。
    fn sidebar_open_file_tree_prompt(
        &mut self,
        kind: shell_sidebar::FileTreePromptKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_file_tree_prompt(kind, window, cx);
    }

    /// 空态「打开文件夹…」：宿主 `open_folder_dialog`（系统文件夹选择框）。
    fn sidebar_open_folder_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_folder_dialog(window, cx);
    }

    /// 删除请求（原生确认框 → 确认后删除）：宿主 file_ops 副作用。
    fn sidebar_file_tree_delete_request(
        &mut self,
        path: &std::path::Path,
        is_dir: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.file_tree_delete_request(path, is_dir, window, cx);
    }

    /// 在资源管理器中显示。
    fn sidebar_file_tree_reveal_in_explorer(&mut self, path: &std::path::Path, cx: &mut Context<Self>) {
        self.file_tree_reveal_in_explorer(path, cx);
    }

    /// 复制绝对路径到文本剪贴板。
    fn sidebar_file_tree_copy_path(&mut self, path: &std::path::Path, cx: &mut Context<Self>) {
        self.file_tree_copy_path(path, cx);
    }

    /// 复制相对路径到文本剪贴板。
    fn sidebar_file_tree_copy_relative_path(
        &mut self,
        path: &std::path::Path,
        cx: &mut Context<Self>,
    ) {
        self.file_tree_copy_relative_path(path, cx);
    }

    /// 剪切（CF_HDROP + Move）。
    fn sidebar_file_tree_cut(&mut self, path: &std::path::Path, cx: &mut Context<Self>) {
        self.file_tree_cut(path, cx);
    }

    /// 复制文件（CF_HDROP + Copy）。
    fn sidebar_file_tree_copy_file(&mut self, path: &std::path::Path, cx: &mut Context<Self>) {
        self.file_tree_copy_file(path, cx);
    }
}

// 文件树输入弹层宿主服务面（shell_sidebar）。提交动作在宿主
// （`app/file_tree_ops.rs` 的 `file_tree_submit_prompt`）。
impl shell_sidebar::FileTreePromptHost for EditorApp {
    fn file_tree_submit_prompt(
        &mut self,
        kind: &shell_sidebar::FileTreePromptKind,
        value: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        self.file_tree_submit_prompt(kind, value, window, cx)
    }
}

// PDF 域宿主服务面（shell_pdf）。PDF 页操作动作对宿主的交互收敛到此：
// toast 通知（宿主通知容器 + `PdfToast` 去重标记）与右键新弹前弹窗互关（清宿主
// `tab_bar_menu` / `media_lock_menu` / `pdf_menu` 槽位）。域逻辑在 shell_pdf，
// 宿主实现零业务逻辑。
impl shell_pdf::PdfHost for EditorApp {
    fn pdf_host_toast(&mut self, title: &str, msg: String) {
        self.pdf_toast(title, msg);
    }

    fn pdf_host_close_all_popups(&mut self) {
        self.close_pdf_popups_predecessors();
    }
}

// 媒体域宿主服务面（shell_media）。媒体窗口路由的「真正执行」与「最近其它
// 媒体窗口」查询收敛到此 trait（宿主方法体薄转发 `open_media_in_new_window` /
// `route_media_to_window` / `nearest_other_media_window`）；纯决策在
// `shell_media::media_window`，宿主编排（`maybe_redirect_media_open` 等）据此分发执行。
impl shell_media::MediaHost for EditorApp {
    /// 「最近其它媒体窗口」查询：`EditorAppRegistry` 依赖收敛到宿主实现（域不直读注册表）。
    /// 列表逆序首个媒体窗口 = 最近创建的媒体窗口；排除自身（按实体 id 比对）。
    fn media_host_nearest_other_media_window(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyWindowHandle> {
        if !cx.has_global::<EditorAppRegistry>() {
            return None;
        }
        let self_id = cx.entity().entity_id();
        let registry = cx.global::<EditorAppRegistry>();
        registry.windows.iter().rev().find_map(|(weak, wh)| {
            let entity = weak.upgrade()?;
            if entity.entity_id() == self_id {
                return None;
            }
            let is_media = entity.read_with(cx, |app, _| {
                shell_media::media_window::media_window_kind(&app.model.borrow()).is_some()
            });
            is_media.then_some(*wh)
        })
    }

    /// 媒体路由「真正执行」：新开同进程独立窗口并打开该媒体（原
    /// `EditorApp::open_media_in_new_window`）。`open_window` 闭包内只造 app + root（构造
    /// 期不打开文件——`new_media_window` 不读 CLI 首参/不恢复会话，防重定向循环与重复
    /// 恢复），窗口创建成功后经窗口句柄补开媒体。发起窗口内容零改动。
    /// 窗口尺寸：本次媒体种类（音频→音乐组 / 视频→视频组）有记录尺寸 → 按该尺寸居中开；
    /// 未记录 → **跟随当前窗口大小**（`window.bounds()`，解决「新窗口比原窗口大不一致」）。
    fn media_host_open_media_new_window(
        &mut self,
        path: &std::path::Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let owned = path.to_string_lossy().into_owned();
        // 首帧墙钟诊断（VUICK_DIAG 守卫，平时零开销零行为）：触发起点 = 本函数
        // 入口；下方 [MEDIAWNDSZ] open / 路由完成两处补 wall 字段（轨 D 基线）。
        let diag_t0 = if std::env::var_os("VUICK_DIAG").is_some() {
            Some(std::time::Instant::now())
        } else {
            None
        };
        // 按本次媒体种类取对应记录尺寸（新窗口尚未是媒体窗口，按扩展名决定角色即可；
        // 域决策 `shell_media::media_window::open_target_kind`）。
        let kind = shell_media::media_window::open_target_kind(path);
        // 取该角色记录尺寸：发起者是**媒体窗口**时自身内存即最新（本窗刚 record 过 /
        // 当前角色同组，防抖未落盘期间也正确）；否则（主窗口等非媒体窗）内存可能陈旧——
        // 媒体窗口 resize 自动写盘后不会广播到主窗口内存，会退化成「改好音乐窗尺寸→关→
        // 再开仍是旧尺寸」。此时以 config 盘上最新记录优先、内存兜底（`UserPrefs::load()`
        // 每次读盘，代价仅打开动作）。域决策 `shell_media::media_window::resolve_recorded_size`。
        let disk = UserPrefs::load();
        let recorded = shell_media::media_window::resolve_recorded_size(
            kind,
            self.media_window_kind().is_some(),
            self.model.borrow().music_window_size,
            self.model.borrow().video_window_size,
            disk.music_window_size,
            disk.video_window_size,
        );
        let bounds = match recorded {
            Some(size) => {
                // 最小尺寸护栏（防记录值退化到过小）：clamp 后居中于主显示器。
                let size = gpui::size(
                    px(size.width.as_f32().max(shell_media::media_window::MIN_MEDIA_WIN_W)),
                    px(size.height.as_f32().max(shell_media::media_window::MIN_MEDIA_WIN_H)),
                );
                Bounds::centered(None, size, cx)
            }
            None => window.bounds(),
        };
        let titlebar_options = gpui::TitlebarOptions {
            title: Some("GPUI 编辑器".into()),
            appears_transparent: true,
            ..Default::default()
        };
        if let Some(t0) = diag_t0 {
            eprintln!(
                "[MEDIAWNDSZ] open kind={kind:?} size={}x{} follow-source={} wall={:.1}ms",
                bounds.size.width.as_f32().round() as i64,
                bounds.size.height.as_f32().round() as i64,
                recorded.is_none(),
                t0.elapsed().as_secs_f64() * 1000.0,
            );
        }
        let handle: AnyWindowHandle = match cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(titlebar_options),
                ..Default::default()
            },
            |new_window, cx| {
                let view = cx.new(|cx| EditorApp::new_media_window(new_window, cx));
                cx.new(|cx| ui::root::Root::new(view, new_window, cx))
            },
        ) {
            Ok(h) => h.into(),
            Err(e) => {
                // 新窗创建失败（罕见 / 平台禁用）→ 回退当前窗口打开，保证媒体仍可用。
                self.model.borrow_mut().status = SharedString::from(format!("打开媒体失败：{e}"));
                self.model.borrow_mut().status_ok = false;
                self.open_file_here(&owned, window, cx);
                return;
            }
        };
        self.media_host_route_media_to_window(path, handle, cx);
        // 墙钟：触发 → 新窗口已创建 + 媒体已路由到目标窗口（`route_media_to_window` 内
        // 直调目标窗口 `open_file_here` 同步打开）≈ 新窗口首帧前。
        if let Some(t0) = diag_t0 {
            eprintln!(
                "[MEDIAWND] routed path={owned} wall={:.1}ms",
                t0.elapsed().as_secs_f64() * 1000.0
            );
        }
    }

    /// 媒体路由「真正执行」：把媒体文件经目标窗口句柄转交该窗口 EditorApp 本体打开（原
    /// `EditorApp::route_media_to_window`；同进程跨窗口实体 update；复用既有
    /// `open_file_here`，目标窗口注册表已含自身）。目标窗口可能聚焦（打开动作里的
    /// window.focus），复用已有媒体窗口时属预期行为。
    fn media_host_route_media_to_window(
        &self,
        path: &std::path::Path,
        wh: AnyWindowHandle,
        cx: &mut Context<Self>,
    ) {
        let owned = path.to_string_lossy().into_owned();
        let _ = wh.update(cx, |root, window, cx| {
            // AnyWindowHandle::update 给的是窗口根视图（ui::root::Root），经
            // Root::view() 下转型取承载的 EditorApp（root.rs 注释明确此用途）。
            if let Ok(root) = root.downcast::<ui::root::Root>() {
                let view = root.read(cx).view().clone();
                if let Ok(app) = view.downcast::<EditorApp>() {
                    let _ = app.update(cx, |app, cx| app.open_file_here(&owned, window, cx));
                }
            }
        });
    }
}

// 预览域宿主服务面（shell_preview）。预览域对宿主的副作用收敛到此 trait：
// WYSIWYG 活区内容变更的防抖会话持久化 / Ctrl+滚轮缩放字号 / 预览拖放打开。域逻辑在
// shell_preview（`live::enter_live` 订阅回调、`render::zoom_overlay`、`render::md_wy_host_children`
// 的 on_drop 经 `cx.listener` 的 `&mut H` 直调），宿主实现零业务逻辑（转发既有方法）。
impl shell_preview::PreviewHost for EditorApp {
    fn preview_schedule_session_flush(&mut self, cx: &mut Context<Self>) {
        self.schedule_session_flush(cx, false);
    }

    fn preview_change_font_size(&mut self, delta: f32, cx: &mut Context<Self>) {
        self.change_font_size(delta, cx);
    }

    fn preview_open_dropped_paths(
        &mut self,
        paths: &ExternalPaths,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_dropped_paths(paths, window, cx);
    }
}

// 选项卡栏域宿主服务面（shell_tabs）。选项卡栏渲染/事件在 `TabBar` 实体
// （`tab_bar_view`），凡事件回调必须落宿主副作用的动作统一经此 trait 反调；宿主实现
// 只薄转发既有方法（`tabs.rs` 生命周期 glue / `preview.rs` 预览切换 / 菜单槽位落地），
// 零业务逻辑。拖拽开始预建幽灵窗入口在 `shell_tabs::TabDragHost`（下方 impl）。
impl shell_tabs::TabsHost for EditorApp {
    fn tabs_host_activate_tab(
        &mut self,
        idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.activate_tab(idx, window, cx);
    }

    fn tabs_host_close_tab(
        &mut self,
        idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_tab(idx, window, cx);
    }

    fn tabs_host_close_others(
        &mut self,
        idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.tab_close_others(idx, window, cx);
    }

    fn tabs_host_close_right(
        &mut self,
        idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.tab_close_right(idx, window, cx);
    }

    fn tabs_host_close_saved(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.tab_close_saved(window, cx);
    }

    fn tabs_host_close_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.tab_close_all(window, cx);
    }

    fn tabs_host_move_to_new_window(
        &mut self,
        idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.tab_move_to_new_window(idx, window, cx);
    }

    fn tabs_host_copy_to_new_window(
        &mut self,
        idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.tab_copy_to_new_window(idx, window, cx);
    }

    fn tabs_host_new_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.new_tab(window, cx);
    }

    fn tabs_host_open_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_dialog(window, cx);
    }

    fn tabs_host_toggle_preview_for_tab(
        &mut self,
        idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_preview_for_tab(idx, window, cx);
    }

    fn tabs_host_poke_tab_hscroll(&mut self, cx: &mut Context<Self>) {
        self.poke_tab_hscroll(cx);
    }

    /// 右键菜单落地（测试直读宿主 `tab_bar_menu` 槽位 + media/pdf 前置弹窗互关直清，
    /// 故槽位留宿主）。
    fn tabs_host_open_tab_bar_menu(
        &mut self,
        pos: Point<Pixels>,
        menu: gpui::Entity<PopupMenu>,
        sub: Subscription,
        cx: &mut Context<Self>,
    ) {
        self.tab_bar_menu.pos = Some(pos);
        self.tab_bar_menu.entity = Some(menu);
        self.tab_bar_menu.sub = Some(sub);
        cx.notify();
    }

    /// 关闭清理（emitter 身份校验后清槽位 + notify；旧订阅带身份校验不会误清新菜单）。
    fn tabs_host_dismiss_tab_bar_menu(
        &mut self,
        menu: &gpui::Entity<PopupMenu>,
        cx: &mut Context<Self>,
    ) {
        if self.tab_bar_menu.entity.as_ref() == Some(menu) {
            self.tab_bar_menu.clear();
        }
        cx.notify();
    }
}

// 跨窗口选项卡拖拽宿主服务面（shell_tabs::drag）。拖拽流程的纯逻辑
// （命中 / 布局参数 / 状态机迁移 / 拆分决策）在 `shell_tabs::drag`；宿主侧「真正执行」
// （open_window 建新窗 / EditorAppRegistry 查询 / HWND·DWM / 幽灵窗实体创建销毁 /
// adopt·restore 目标窗操作）统一经本 trait 反调宿主。宿主方法体薄转发
// `app/tab_drag.rs` 既有实现（该文件仍为主编排列写点）。
impl shell_tabs::TabDragHost for EditorApp {
    fn tab_drag_host_prime_ghost(
        &mut self,
        idx: usize,
        name: SharedString,
        dirty: bool,
        size_cell: Rc<Cell<Size<Pixels>>>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        self.prime_drag_ghost(idx, name, dirty, size_cell, window, cx);
    }

    fn tab_drag_host_update_ghost(
        &mut self,
        idx: usize,
        drag_size: Rc<Cell<Size<Pixels>>>,
        pos: Point<Pixels>,
        outside: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        self.update_drag_ghost(idx, drag_size, pos, outside, window, cx);
    }

    fn tab_drag_host_destroy_ghost(&mut self, cx: &mut Context<Self>) {
        self.destroy_drag_ghost(cx);
    }

    fn tab_drag_host_take_payload(&mut self, idx: usize) -> Option<DetachedTab> {
        self.take_tab_payload(idx)
    }

    fn tab_drag_host_commit_live_and_take_payload(
        &mut self,
        idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<DetachedTab> {
        self.commit_live_and_take_tab_payload(idx, window, cx)
    }

    fn tab_drag_host_release_outside(
        &mut self,
        window: &mut Window,
        release_position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.release_tab_drag_outside(window, release_position, cx);
    }

    fn tab_drag_host_split_to_new_window(
        &mut self,
        payload: DetachedTab,
        release_position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.split_tab_to_new_window(payload, release_position, window, cx);
    }

    fn tab_drag_host_restore_to_primary(
        &mut self,
        payload: DetachedTab,
        cx: &mut Context<Self>,
    ) {
        self.restore_tab_to_primary(payload, cx);
    }
}
