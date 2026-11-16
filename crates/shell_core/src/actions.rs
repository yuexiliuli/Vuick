//! `AppModel` 的**动作方法体**：tabs 动作半（make/new/reorder/
//! activate/remove/tab_close_*）、tab_drag payload 半（take_tab_payload）、session 编排
//! 纯逻辑（persist_session / save_session_to / collect_session_data）与它们依赖的模型直读
//! helper（tab_focus_handle / tab_effective_text）。
//!
//! 签名统一为 `(&mut self, host: &dyn AppHost, …)` 形式：模型字段同 crate 直取，宿主
//! 服务经 [`crate::host::AppHost`] 反向调用。宿主 glue（按钮 / 菜单 / 事件 / 异步实体
//! 再入）留在 shell 的 `impl EditorApp`，经 `self.model.borrow_mut().yyy(host_self, …)`
//! 转发。
//!
//! 借用约束：host 方法 `&self`、host 实现不得同步触碰 `AppModel`（见 host.rs 模块注释）；
//! 本模块内 `self` 即 `&mut AppModel`，随取随用，不需 RefCell。

use std::path::{Path, PathBuf};

use gpui::{App, FocusHandle, Window};
use gpui::AppContext as _;
use session_store::{
    ReadingProgress, SessionManifest, SessionTab, clear_session, prune_unreferenced_slots,
    reading_progress, write_manifest, write_slot,
};

use config::user::UserPrefs;

use crate::host::AppHost;
use crate::model::{AppModel, DetachedTab, EditorTab, MediaReader, MdWyInstall, TabPayload};

impl AppModel {
    // ---------- tabs 动作半 ----------

    /// 新建一个空文件选项卡并激活：自研编辑器组件（空文档；语法高亮语言在打开文件时
    /// `set_language`）→ 订阅编辑事件 → 入列 + 置活动 → 结构变化立即持久化。
    pub fn make_tab(&mut self, host: &dyn AppHost, cx: &mut App) -> usize {
        // 自研编辑器组件：创建即空文档；语法高亮语言在打开文件时 set_language。
        let editor = cx.new(|cx| editor::CodeEditor::new(cx));
        // 编辑内容被修改 → 置脏 + 同步内容/行列缓存 + 预览增量入口。
        let edit_sub = host.subscribe_editor(&editor, cx);

        self.tabs.push(EditorTab {
            payload: TabPayload {
                editor,
                reader: MediaReader::None,
                md_wy: None,
                md_wy_vscroll: gpui::ScrollHandle::new(),
                md_wy_install: MdWyInstall::default(),
                epub_images: Default::default(),
                epub_notes: Default::default(),
                file_path: None,
                dirty: false,
                source_encoding: None, // 新建选项卡 = 纯 UTF-8
                content: gpui::SharedString::default(),
                line_count: 1,
                char_count: 0,
                cursor_row: 0,
                cursor_col: 0,
                preview_on: false,
                preview_fullscreen: false,
                readonly_preview: false,
                loading_lock: false,
                edit_version: 0,
                reading_restore: None,
                md_restore_pending: None,
            },
            live_edit: false,
            live2: None,
            live2_dirty: false,
            _edit_sub: edit_sub,
            _pdf_sub: None,
            _live2_sub: None,
            live_vscroll: gpui::ScrollHandle::new(),
        });
        let idx = self.tabs.len() - 1;
        self.active_tab = idx;
        // 新选项卡 → 结构变化立即持久化（恢复期间由 `session_restoring` 抑制）。
        self.persist_session(host, cx);
        idx
    }

    /// 「新建文件」动作：建空选项卡 → 聚焦编辑区 → 状态栏提示 → 持久化 + 重绘。
    pub fn new_tab(&mut self, host: &dyn AppHost, window: &mut Window, cx: &mut App) {
        let idx = self.make_tab(host, cx);
        let focus = self.tab_focus_handle(idx, host, cx);
        window.focus(&focus, cx);
        self.status = gpui::SharedString::from("新建文件");
        self.status_ok = true;
        // make_tab 已即时持久化；此处再安排一次（幂等）以覆盖「新建即激活」语义。
        self.persist_session(host, cx);
        host.notify_host(cx);
    }

    /// 栏内拖拽排序落地：把 `from` 移到 `target`（插回原位/原相邻位不动作；单选项卡天然
    /// 不动作）。活动下标随移动校正；结构变化即时持久化。
    pub fn reorder_tab(&mut self, host: &dyn AppHost, from: usize, target: usize, cx: &mut App) {
        let n = self.tabs.len();
        if n <= 1 || from >= n {
            return;
        }
        let target = target.min(n);
        // remove(from) 后相对原数组的插入位：`from < target` 时左移一位。
        if target == from || target == from + 1 {
            return; // 落回原位 / 原相邻交界 = 顺序无变化
        }
        let tab = self.tabs.remove(from);
        let insert_at = if from < target { target - 1 } else { target };
        self.tabs.insert(insert_at, tab);
        // 活动选项卡跟随：被拖动项 = insert_at；其余按「先删后插」的下标迁移校正——
        // 删 from（>from 者 -1）再插 insert_at（≥insert_at 者 +1）。
        let a0 = self.active_tab;
        if a0 == from {
            self.active_tab = insert_at;
        } else {
            let a1 = if a0 > from { a0 - 1 } else { a0 };
            self.active_tab = if a1 >= insert_at { a1 + 1 } else { a1 };
        }
        self.search_scan = None; // 选项卡集合结构变化 → 搜索缓存失效
        self.persist_session(host, cx);
        host.notify_host(cx);
    }

    /// 激活指定选项卡：置活动 → 持久化 → 焦点落位（只读预览给根、否则给编辑区）→ 重绘
    /// （拆窗构造期抑制多余 notify）。
    pub fn activate_tab(
        &mut self,
        host: &dyn AppHost,
        idx: usize,
        window: &mut Window,
        cx: &mut App,
    ) {
        if idx >= self.tabs.len() {
            return;
        }
        self.active_tab = idx;
        // 活动选项卡变化（用户点击切换）→ 立即持久化（便宜；make_tab/remove_tab
        // 等已自带，这里覆盖纯「切换激活」的用户操作）。
        self.persist_session(host, cx);
        // PDF / EPUB 等只读预览文档没有可见编辑区，不把焦点给隐藏的编辑器，
        // 而是给 App 根（让窗口级快捷键仍可用）。
        let readonly = self.tabs[idx].payload.readonly_preview;
        if readonly {
            let root = host.root_focus_handle(cx);
            window.focus(&root, cx);
        } else {
            let focus = self.tab_focus_handle(idx, host, cx);
            window.focus(&focus, cx);
        }
        // 拆出窗口构造期（`constructing=true`）抑制：`window.focus` 已直接 `refresh`
        // （首帧前落定、被同步首帧清除），无需再入队 notify 让首帧后补刷一整帧。
        if !host.is_constructing() {
            host.notify_host(cx);
        }
    }

    /// 关闭选项卡：干净命名文件夹闭直通 [`Self::remove_tab`]；脏 / 未命名走宿主 glue
    /// （`EditorApp::close_tab`，弹原生确认框异步等待）——本方法只承载「无需询问」路径。
    /// 实际上闭路（弹确认）由宿主 `close_tab` 处理，此处保留 `remove_tab` 本体供两者共用。
    pub fn remove_tab(&mut self, host: &dyn AppHost, idx: usize, window: &mut Window, cx: &mut App) {
        if idx >= self.tabs.len() {
            return;
        }
        // 阅读进度：关闭前保存该 tab 位置（开关开着才写；恢复期抑制；未命名跳过；
        // 必须在 reader 被拔出/移除之前读——PDF 位置来自阅读器实体）。
        self.save_reading_progress(host, idx, cx);
        self.search_scan = None; // 选项卡集合变化 → 搜索缓存失效
        // 释放被移除选项卡持有的重资源（PDF 纹理 / 图片解码），实体随后随之释放。
        // 顺序保持旧行为：先 PDF、再图片；视频无显式 release（随选项卡直接释放）。
        let mut reader = MediaReader::None;
        std::mem::swap(&mut self.tabs[idx].payload.reader, &mut reader);
        match reader {
            MediaReader::Pdf(pdf) => {
                pdf.update(cx, |view, cx| view.release(window, cx));
            }
            MediaReader::Image(img) => {
                img.update(cx, |view, cx| view.release(window, cx));
            }
            MediaReader::None => {}
            #[cfg(feature = "video")]
            MediaReader::Video(_) => {}
        }
        self.tabs.remove(idx);
        // 选项卡被移除 → 结构变化立即持久化（零 tab 与重排后都写）。
        self.persist_session(host, cx);
        if self.tabs.is_empty() {
            // 关闭最后一个选项卡 → 零选项卡，回到欢迎页：不退出、不新建空选项卡、
            // 不 panic。此时没有编辑区可聚焦，焦点给应用根（窗口级快捷键照样可用）。
            self.active_tab = 0;
            let root = host.root_focus_handle(cx);
            window.focus(&root, cx);
            self.status = gpui::SharedString::from("已关闭");
            self.status_ok = true;
            host.notify_host(cx);
            return;
        }
        // 先提纯 active/len（owned）再分支，避免 `if` 条件里的临时 Ref 跨过块内
        // 可变借用（借用约束）。
        let active = self.active_tab;
        let len = self.tabs.len();
        if idx < active {
            self.active_tab -= 1;
        } else if active >= len {
            self.active_tab = len - 1;
        }
        let active = self.active_tab;
        self.activate_tab(host, active, window, cx);
    }

    /// 批量关闭前的脏/未命名审查：`affected(i)` 为真的选项卡中**任一** `dirty` 或
    /// `file_path.is_none()`（未命名）→ 中止整次批量（一个都不关）+ 状态栏提示；
    /// 全部干净 → 返回 true 放行。headless 可测：中止路径不下发任何 `remove_tab`。
    pub fn tab_close_guard(
        &mut self,
        host: &dyn AppHost,
        affected: &dyn Fn(usize) -> bool,
        cx: &mut App,
    ) -> bool {
        let any_unsaved = self
            .tabs
            .iter()
            .enumerate()
            .any(|(i, t)| affected(i) && (t.payload.dirty || t.payload.file_path.is_none()));
        if any_unsaved {
            self.status = gpui::SharedString::from("有未保存的更改，未关闭");
            self.status_ok = false;
            host.notify_host(cx);
            return false;
        }
        true
    }

    /// 「关闭其他」：关闭除 `idx` 外全部；任一受影响脏/未命名 → 中止一个都不关。
    pub fn tab_close_others(
        &mut self,
        host: &dyn AppHost,
        idx: usize,
        window: &mut Window,
        cx: &mut App,
    ) {
        if !self.tab_close_guard(host, &|i| i != idx, cx) {
            return;
        }
        let len = self.tabs.len();
        for i in (0..len).rev() {
            if i != idx {
                self.remove_tab(host, i, window, cx);
            }
        }
        host.notify_host(cx);
    }

    /// 「关闭右侧」：关闭 `idx` 右侧全部；任一受影响脏/未命名 → 中止一个都不关。
    pub fn tab_close_right(
        &mut self,
        host: &dyn AppHost,
        idx: usize,
        window: &mut Window,
        cx: &mut App,
    ) {
        if !self.tab_close_guard(host, &|i| i > idx, cx) {
            return;
        }
        let len = self.tabs.len();
        for i in (idx + 1..len).rev() {
            self.remove_tab(host, i, window, cx);
        }
        host.notify_host(cx);
    }

    /// 「关闭全部」：关闭全部；任一脏/未命名 → 中止一个都不关。
    pub fn tab_close_all(&mut self, host: &dyn AppHost, window: &mut Window, cx: &mut App) {
        if !self.tab_close_guard(host, &|_| true, cx) {
            return;
        }
        let len = self.tabs.len();
        for i in (0..len).rev() {
            self.remove_tab(host, i, window, cx);
        }
        host.notify_host(cx);
    }

    /// 「关闭已保存」：只关 `有路径 && !dirty` 的选项卡（定义如此，无中止语义）；
    /// 脏 / 未命名 / 无路径空选项卡保留。
    pub fn tab_close_saved(&mut self, host: &dyn AppHost, window: &mut Window, cx: &mut App) {
        let len = self.tabs.len();
        for i in (0..len).rev() {
            let saved = self.tabs[i].payload.file_path.is_some() && !self.tabs[i].payload.dirty;
            if saved {
                self.remove_tab(host, i, window, cx);
            }
        }
        host.notify_host(cx);
    }

    // ---------- tab_drag payload 半 ----------

    /// 从本窗口「拔出」一个选项卡：移除选项卡（**不**释放 PDF 纹理、不重置编辑器——
    /// 实体与元数据整体移到目标窗口），下标与激活态自动校正。
    /// 整结构 move：`From<EditorTab> for DetachedTab` 一次解构 23 个搬迁字段
    /// （`..` 丢弃源窗口专属的 live 态 / 订阅），不再逐字段手抄。
    pub fn take_tab_payload(&mut self, idx: usize) -> Option<DetachedTab> {
        if idx >= self.tabs.len() {
            return None;
        }
        let tab = self.tabs.remove(idx);
        // 先提纯 active/len（owned）再分支写——避免同一语句里
        // 可变与共享借用同时存活（同 crate 内已无 RefCell，仍按「先取后写」保持原语义）。
        let active = self.active_tab;
        let len = self.tabs.len();
        if idx < active {
            self.active_tab -= 1;
        } else if active >= len {
            self.active_tab = len.saturating_sub(1);
        }
        self.search_scan = None; // 选项卡集合变化 → 搜索缓存失效
        Some(tab.into())
    }

    // ---------- tabs 动作的模型直读 helper ----------

    /// 选项卡的编辑焦点句柄：预览编辑模式返回 WYSIWYG 编辑器（否则键盘进
    /// 不可见的源码编辑区）；缺省（越界/无 tab）返回应用根焦点。
    pub fn tab_focus_handle(&self, idx: usize, host: &dyn AppHost, cx: &App) -> FocusHandle {
        let Some(tab) = self.tabs.get(idx) else {
            return host.root_focus_handle(cx);
        };
        if tab.live_edit
            && let Some(l2) = &tab.live2
        {
            let h = gpui::Focusable::focus_handle(l2.read(cx), cx);
            return h;
        }
        tab.payload.editor.read(cx).focus_handle()
    }

    /// 选项卡的「有效内容」：`live2`（WyEditor，实时）开启时读它，否则读源码编辑区。
    /// 保存/关闭/写回统一走此口径——**live 期间源码编辑区不镜像**（去 O(n)），直接读
    /// WyEditor 的 rope 才不丢内容（大文档 close→保存 若读旧编辑区会整段丢失）。
    /// 大文档后台解析**未完成**（`is_loaded() == false`）时 live2 是空壳，回退源码编辑区。
    pub fn tab_effective_text(&self, idx: usize, cx: &App) -> String {
        match self.tabs.get(idx) {
            Some(t) => match &t.live2 {
                Some(l2) => {
                    if l2.read(cx).is_loaded() {
                        l2.read(cx).text().to_string()
                    } else {
                        t.payload.editor.read(cx).value().to_string()
                    }
                }
                None => t.payload.editor.read(cx).value().to_string(),
            },
            None => String::new(),
        }
    }

    // ---------- session 编排纯逻辑 ----------

    /// 把当前**主窗口**选项卡集合**立即**持久化到会话目录（连续持久化核心）。
    /// 仅在「会话责任窗口」（首个主窗口 `session_persist`）、非 detached、开关开启、
    /// 非恢复期间落盘；测试构建恒跳过（**绝不**触碰真实会话目录）。
    pub fn persist_session(&mut self, host: &dyn AppHost, cx: &mut App) {
        if !host.is_session_persist()
            || host.is_detached()
            || !self.session_restore
            || self.session_restoring
            // 测试构建恒短路早退（cfg!(test) 经 host 回传）；磁盘写不触碰真实 APPDATA。
            || host.is_test_build()
        {
            return;
        }
        let _ = self.save_session_to(&UserPrefs::session_dir(), cx);
    }

    // ---------- 阅读进度编排纯逻辑 ----------

    /// 阅读进度文件的**默认路径** = 配置文件父目录 / `reading_progress.json`
    /// （`%APPDATA%\vuick\`；测试构建经 config `test-mode` 落隔离临时目录）。
    /// `pub`：shell 宿主与集成测试经 `shell::app` 重导出访问（§8.6 外提清单）。
    pub fn reading_progress_path() -> PathBuf {
        match UserPrefs::config_path().parent() {
            Some(p) if !p.as_os_str().is_empty() => p.join("reading_progress.json"),
            _ => PathBuf::from("reading_progress.json"),
        }
    }

    /// 读取某选项卡当前的「阅读位置」（尚未落盘；`None` = 不可定位：
    /// 未命名选项卡 / PDF 未加载完 / EPUB 右栏宿主未就绪）。
    /// `pub`：shell 宿主**每帧渲染**的阅读进度脏哨兵比对（`check_reading_progress`）
    /// 经 `model.borrow().tab_reading_position(idx, &*cx)` 复用同一读数。
    pub fn tab_reading_position(&self, idx: usize, cx: &App) -> Option<ReadingProgress> {
        let tab = self.tabs.get(idx)?;
        if tab.payload.file_path.is_none() {
            return None; // 未命名选项卡不记进度
        }
        // PDF：页 + 页内比例 + 缩放（阅读器自算）。
        if let MediaReader::Pdf(pv) = &tab.payload.reader {
            let (page, frac, zoom) = pv.read(cx).reading_progress()?;
            return Some(ReadingProgress::Pdf { page_index: page, in_page_frac: frac, zoom });
        }
        // EPUB：只读预览宿主（WyEditor）的视口顶源码行。
        if tab.payload.readonly_preview {
            let line = tab.payload.md_wy.as_ref()?.read(cx).top_source_line()?;
            return Some(ReadingProgress::Epub { top_line: line });
        }
        // md / txt：编辑区视口顶源码行（0 基）。
        let top_line = tab.payload.editor.read(cx).viewport_top_line();
        Some(ReadingProgress::Markdown { top_line })
    }

    /// 保存**单个选项卡**的阅读进度：更新模型 map（恢复读取源）+ 落盘（未变跳过；
    /// 测试构建只更新内存、不触碰磁盘）。关 tab / 滚动防抖 / 退出兜底共用。
    /// 只读 `&App`：仅经 `pv.read/ed.read/wy.read` 读实体状态 + 写 `self.reading_progress`，
    /// 不触发任何实体动作（退出钩子 `save_session(&Context)` 可直接复用）。
    /// `pub`：shell 宿主 glue（`schedule_reading_progress_flush`）与集成测试经
    /// `shell::app` 重导出访问（§8.6 外提清单）。
    pub fn save_reading_progress(&mut self, host: &dyn AppHost, idx: usize, cx: &App) {
        if !host.is_session_persist()
            || host.is_detached()
            || !self.restore_reading_progress
            || self.session_restoring
        {
            return;
        }
        let Some(progress) = self.tab_reading_position(idx, cx) else {
            return;
        };
        let Some(tab) = self.tabs.get(idx) else {
            return;
        };
        let Some(path) = tab.payload.file_path.clone() else {
            return;
        };
        let key = reading_progress::normalize_key(&path);
        let unchanged = self.reading_progress.get(&key) == Some(&progress);
        crate::rp_diag::diag(
            "save",
            &format!(
                "key={key} {} written={}",
                crate::rp_diag::rp_value(&progress),
                !unchanged
            ),
        );
        if unchanged {
            return; // 原地未变：不重复落盘
        }
        self.reading_progress.insert(key, progress);
        if !host.is_test_build() {
            reading_progress::save(&Self::reading_progress_path(), &self.reading_progress);
        }
    }

    /// 保存**全部**选项卡的阅读进度（退出兜底：平台最后窗口关闭不回调 `on_app_quit`，
    /// 各 tab 的滚动防抖已在实际读位置持续落盘，这里补一发全量收口）。
    /// `pub`：shell 宿主退出钩子 glue 与集成测试经 `shell::app` 重导出访问。
    pub fn save_all_reading_progress(&mut self, host: &dyn AppHost, cx: &App) {
        let len = self.tabs.len();
        for i in 0..len {
            self.save_reading_progress(host, i, cx);
        }
    }

    /// 保存会话（纯逻辑，显式目录，可测试）：开关关闭 → 只清不做；否则先写槽位内容、
    /// 再原子写清单。文件选项卡只记路径，未命名选项卡经 [`Self::tab_effective_text`]
    /// 取内容写槽位。跳过未变槽位（`session_slot_hashes` 指纹）；清单未变则跳过清单写 +
    /// prune。写清单成功后清理不再被引用的陈旧槽位。
    pub fn save_session_to(&mut self, dir: &Path, cx: &App) -> bool {
        if !self.session_restore {
            // 开关关闭：清盘面 + 清指纹（重开时旧指纹会误判「盘上有同内容」而跳过写槽位
            // → 恢复读到缺失槽位为空内容；此处一并清掉，杜绝该竞态）。
            self.session_slot_hashes.clear();
            self.session_last_manifest_digest = None;
            return clear_session(dir);
        }
        let (manifest, slots) = self.collect_session_data(cx);
        for (slot, text) in &slots {
            let h = content_hash(text);
            // 槽位名与指纹都未变 → 内容与盘面一致，跳过重写（消除切选项卡/结构
            // 变化时的全量同步磁盘写）。写失败仅当确实要写时返回（跳过不影响盘面）。
            let unchanged = self.session_slot_hashes.get(slot).copied() == Some(h);
            if !unchanged {
                if !write_slot(dir, slot, text) {
                    return false;
                }
                self.session_slot_hashes.insert(slot.clone(), h);
            }
        }
        // 收敛指纹表规模：本次不再存在的旧槽位（选项卡被关/重编号后）直接淘汰，
        // 避免 HashMap 随「反复新建/关闭未命名选项卡」长期累积（内存不放大）。
        let n_slots = slots.len();
        self.session_slot_hashes.retain(|slot, _| {
            slot.strip_prefix('t')
                .and_then(|s| s.parse::<usize>().ok())
                .map_or(false, |n| n < n_slots)
        });
        // 清单是"本次会话可恢复"的原子信号，但写它恰是切选项卡/新建时最贵的原语
        // （本机防病毒下原子写 ~5-25ms）。清单未变（`new_tab` 紧随 `make_tab` 的二次
        // 即时落盘、内容防抖收敛后的等价重写等「无变化重入」）→ 字节级相同，盘面已最
        // 新，直接跳过清单写 + prune（零文件 IO）；槽位已在上面写完。
        let body = manifest.to_value().to_string();
        let digest = content_hash(&body);
        if self.session_last_manifest_digest == Some(digest) {
            return true;
        }
        if !write_manifest(dir, &manifest) {
            return false;
        }
        self.session_last_manifest_digest = Some(digest);
        let _ = prune_unreferenced_slots(dir, &manifest);
        true
    }

    /// 从当前选项卡集合生成会话数据（清单 + 各未命名槽位内容，纯内存、无磁盘副作用）。
    /// 与「导出设置」（宿主 `settings_xfer`）共用同一序列化口径：文件选项卡只记路径、
    /// 未命名选项卡取 `tab_effective_text` 有效内容；槽位名 `t0..tN` 随未命名顺序位移。
    pub fn collect_session_data(&self, cx: &App) -> (SessionManifest, Vec<(String, String)>) {
        let mut tabs = Vec::new();
        let mut slots = Vec::new();
        let mut n_slots = 0usize;
        for (idx, tab) in self.tabs.iter().enumerate() {
            match &tab.payload.file_path {
                Some(p) => tabs.push(SessionTab::File { path: p.clone() }),
                None => {
                    let slot = format!("t{n_slots}");
                    n_slots += 1;
                    let text = self.tab_effective_text(idx, cx);
                    slots.push((slot.clone(), text));
                    tabs.push(SessionTab::New {
                        slot,
                        name: "未命名".to_string(),
                    });
                }
            }
        }
        let manifest = SessionManifest {
            version: 1,
            active_tab: self.active_tab,
            tabs,
        };
        (manifest, slots)
    }
}

/// 去重置顶 + 截断（每类 ≤[`config::limits::RECENT_MAX`]）：相同路径先移除旧位再插到最前。
pub fn push_recent(list: &mut Vec<String>, path: &str) {
    if let Some(i) = list.iter().position(|p| p == path) {
        list.remove(i);
    }
    list.insert(0, path.to_string());
    list.truncate(config::limits::RECENT_MAX);
}

impl AppModel {
    // ---------- 偏好持久化 / 最近文件（file_ops 动作的纯逻辑半） ----------

    /// 汇总当前偏好（主题三态 / 字号 / 侧栏宽度 / 会话与托盘开关 / 打开文件夹根 /
    /// 最近打开），供持久化与「导出设置」（宿主 `settings_xfer`）共用同一口径——导出即
    /// 「当前交互态」，两者始终一致。侧栏宽度/文件树根来自宿主（`AppHost` 纯取值）。
    pub fn collect_prefs(&self, host: &dyn AppHost) -> UserPrefs {
        // 音乐播放器偏好（music_dir / music_liked / 音量 / 上次曲目）不由 shell 维护：
        // 此处沿用已存值（与持久化既有口径一致，避免放空音乐目录/收藏）。
        let existing = UserPrefs::load();
        UserPrefs {
            theme: self.theme_pref,
            font_size: Some(self.font_size),
            sidebar_width: host.sidebar_width_manual(),
            // 会话恢复开关随其它偏好一并落盘（否则一次字号/主题保存就会把它重置回默认开）。
            session_restore: Some(self.session_restore),
            // 关闭到托盘开关随其它偏好一并落盘（否则一次字号/主题保存就把它重置回默认关）。
            close_to_tray: Some(self.close_to_tray),
            // 「记住阅读进度」开关随其它偏好一并落盘（否则一次字号/主题保存就重置回默认开）。
            restore_reading_progress: Some(self.restore_reading_progress),
            // 媒体打开位置两个开关随其它偏好一并落盘（否则一次字号/主题保存就把开关重置
            // 回默认开；collect_prefs 是持久化唯一口径）。
            media_new_window: Some(self.media_new_window),
            reuse_media_window: Some(self.reuse_media_window),
            // 媒体窗口记录尺寸一并落盘（否则一次字号/主题保存就把记录清空回 None，白记）。
            // Size<Pixels> → (f32, f32) 逻辑像素；None（未记录）→ JSON null。
            music_window_size: self
                .music_window_size
                .map(|s| (s.width.as_f32(), s.height.as_f32())),
            video_window_size: self
                .video_window_size
                .map(|s| (s.width.as_f32(), s.height.as_f32())),
            // 打开文件夹的根路径随偏好落盘（首次写前为空；路径失效时宿主 files_root 为
            // None → 这里写 null，下次启动不再恢复该失效文件夹）。
            opened_folder: host
                .files_root()
                .map(|p| p.to_string_lossy().into_owned()),
            // 最近打开（文件夹 / 文件）：随偏好落盘（空 → `[]`），跨重启恢复（只随
            // 打开/移除/清空这类状态变化写，不逐帧 / 不重复全量重写）。
            recent_folders: self.recent_folders.clone(),
            recent_files: self.recent_files.clone(),
            music_dir: existing.music_dir,
            music_liked: existing.music_liked,
            music_volume: existing.music_volume,
            music_last_track: existing.music_last_track,
            music_progress_secs: existing.music_progress_secs,
            // PDF 画布背景偏好随其它偏好一并落盘（否则一次字号/主题保存就把它重置回默认）。
            pdf_bg: self.pdf_bg,
            pdf_bg_custom: self.pdf_bg_custom.clone(),
        }
    }

    /// 把当前偏好（主题三态 / 字号 / 侧栏宽度 / 最近打开）原子写盘。
    /// 侧栏宽度只在用户手动拖过（`width_auto == false`）时才持久化，
    /// 否则下次启动仍保持随内容自适应。
    /// **测试构建不写盘**（config crate 是依赖编译、不受其自身 `cfg!(test)` 门控，
    /// 若这里写盘会污染真实 `%APPDATA%\vuick\config.json`）。
    pub fn persist_prefs(&self, host: &dyn AppHost) {
        // 测试构建恒跳过（cfg!(test) 经 host 回传）：避免污染真实/隔离配置。
        if host.is_test_build() {
            return;
        }
        self.collect_prefs(host).save();
    }

    /// 记录最近打开的文件：去重置顶 + 截 [`config::limits::RECENT_MAX`] + 立即持久化。
    /// 「打开最近」副作用集中入口（一次调用内完成「列表更新 + 落盘」，
    /// 消费点只调它，不散点直调 persist）。
    pub fn record_recent_file(&mut self, host: &dyn AppHost, path: &Path) {
        push_recent(&mut self.recent_files, &path.to_string_lossy());
        self.persist_prefs(host);
    }

    /// 记录最近打开的文件夹：去重置顶 + 截 [`config::limits::RECENT_MAX`] + 立即持久化
    /// （同 [`Self::record_recent_file`] 的副作用集中入口）。
    pub fn record_recent_folder(&mut self, host: &dyn AppHost, path: &Path) {
        push_recent(&mut self.recent_folders, &path.to_string_lossy());
        self.persist_prefs(host);
    }

    /// 从最近文件列表移除失效路径（点击失效项 → 命中同步路径外移除 + 落盘）：
    /// 状态栏提示，不尝试打开。
    pub fn remove_recent_file(
        &mut self,
        host: &dyn AppHost,
        path: &str,
        _window: &Window,
        cx: &mut App,
    ) {
        self.recent_files.retain(|p| p != path);
        self.persist_prefs(host);
        self.status = gpui::SharedString::from(format!("{path} 不存在，已从最近列表移除"));
        self.status_ok = false;
        host.notify_host(cx);
    }

    /// 从最近文件夹列表移除失效路径（同 [`Self::remove_recent_file`]）。
    pub fn remove_recent_folder(
        &mut self,
        host: &dyn AppHost,
        path: &str,
        _window: &Window,
        cx: &mut App,
    ) {
        self.recent_folders.retain(|p| p != path);
        self.persist_prefs(host);
        self.status = gpui::SharedString::from(format!("{path} 不存在，已从最近列表移除"));
        self.status_ok = false;
        host.notify_host(cx);
    }
}

/// 内容指纹：u64 哈希（`std` 默认 Hasher / SipHash-1-3，无新增依赖）。仅用于「内容未变
/// 则跳过槽位重写」的判定；撞车率 ~2^-64，且被跳过时盘面恰是本次内容（此前自身写入的
/// 产物），碰撞失真的风险可忽略。
fn content_hash(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}
