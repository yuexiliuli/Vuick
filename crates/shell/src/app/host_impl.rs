//! SC-4（2026-11-10）：`impl AppHost for EditorApp`——宿主服务面的 shell 侧实现。
//!
//! 借用纪律（见 `shell_core::host` 模块注释）：`AppHost` 方法一律 `&self`、实现**不得
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
            // 执行与既有 `subscribe_editor` 逐字一致的订阅体（行为/时机零变化）。
            let _ = weak.update(cx, |this, cx| {
                let pos = this.model.borrow().tabs.iter().position(|t| t.editor == emitter);
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

    /// 「测试构建」运行时注入（R3，2026-11-10）：integration 测试目标下 shell lib 以
    /// **非 test** 编译 → `cfg!(test)` 恒 false，`persist_session`/`persist_prefs` 会
    /// 真写 `%APPDATA%`（K8 复发）。正解 = 由各 integration 目标 `tests/common/mod.rs`
    /// 的 `init_test()` 在 gpui App 启动前设 `GPUI_EDITOR_TEST=1`，此处运行时判定：
    /// lib 单测仍走 `cfg!(test)`；integration 走 env；生产不设该 env → 恒 false。
    fn is_test_build(&self) -> bool {
        // 只认字面 "1"（fail-closed 到生产行为）：CI/容器/共享 shell 残留 export 或意外值
        // （"false"/"0"/"off"…）一律按非测试态处理，防 persist_session/prefs 被静默关停（丢设置）。
        cfg!(test) || matches!(std::env::var("GPUI_EDITOR_TEST").as_deref(), Ok("1"))
    }
}

// 2026-11-11：侧栏域 crate 宿主服务面（shell_sidebar）。事件回调改侧栏状态统一经此
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
    /// 实体（目录行 on_click 回调期间实体已租用，回租即 `double_lease_panic`，D1 评审
    /// BLOCK 方向 A；与 toc_collapse 同构）。
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

// 2026-11-11：文件树输入弹层宿主服务面（shell_sidebar）。提交动作在宿主
// （`app/file_tree_ops.rs` 的既有 `file_tree_submit_prompt`），行为零变。
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
