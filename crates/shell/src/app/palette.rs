//! 命令面板宿主接线（P2-8）：打开/关闭 + 命令/文件条目构建。
//!
//! 单一来源说明：命令面板的命令 label / 快捷键与顶部菜单（`menu.rs`）共享公共命令表
//! （`shell_commands`）为唯一真值——两处从同一常量取 label + 快捷键展示串（个别命令
//! 的菜单/面板 label 历史不同，如「新建」 vs 「新建文件」，同在表内一行）。菜单还承载
//! disabled / 分隔线 / 动态勾选等渲染细节，仍保留各自结构（见 `shell_commands` 模块注释）；
//! 分类（文件/编辑/查看/设置）只被面板使用，随面板内联。

use super::*;
use crate::actions::{Copy, Cut, Redo, SelectAll, Undo};
use shell_commands::{
    COPY, CUT, DECREASE_FONT, FOLLOW_THEME, INCREASE_FONT, NEW_FILE, OPEN_FILE, OPEN_FOLDER,
    PASTE, PREVIEW, QUIT, REDO, RESET_FONT, SAVE, SAVE_AS, SELECT_ALL, THEME, TOGGLE_SIDEBAR,
    TOGGLE_WRAP, UNDO,
};
use ui::notification::{Notification, NotificationKind};
use ui::palette::{CommandPalette, PaletteItem, PaletteMode};

impl EditorApp {
    /// 命令模式（Ctrl+Shift+P）：打开命令面板；已打开则关闭（toggle）。
    pub(super) fn toggle_command_palette(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.palette.is_some() {
            self.close_palette(window, cx);
            return;
        }
        let items = self.command_palette_items(cx);
        self.open_palette(PaletteMode::Commands, items, window, cx, "");
    }

    /// 文件模式（Ctrl+P）：打开文件快选（已打开文件 + 「打开文件…」系统对话框入口 +
    /// 当前工作区递归候选）。有打开文件夹 → 后台分块扫描候选（主线程零同步读盘），
    /// 扫描未完成先显示已打开文件，完成后一次 `set_items` 刷新；无打开文件夹 →
    /// 面板内提示「请先打开文件夹…」，仍保留已打开文件与对话框条目。
    pub(super) fn quick_open_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette.is_some() {
            self.close_palette(window, cx);
            return;
        }
        let root = self.sidebar.files.root.clone();
        let items = self.file_palette_items(cx);
        let notice = if root.is_some() {
            ""
        } else {
            "请先打开文件夹（文件 → 打开文件夹）"
        };
        self.open_palette(PaletteMode::Files, items, window, cx, notice);
        if let Some(root) = root {
            self.start_quick_open_scan(root, window, cx);
        }
    }

    /// Ctrl+G：活动文本选项卡打开跳转行卡片（`ed.open_goto`）；非文本选项卡
    /// （PDF/图片/视频/EPUB/音乐整窗）→ toast「当前选项卡不支持跳转行」，不崩、不开卡。
    /// （旧命令面板 GoToLine 已被编辑区顶部居中卡片取代，见 `crates/editor/src/goto.rs`。）
    pub(super) fn goto_line_in_active_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // SC-2：把「媒体/只读判定 + 编辑器克隆」提到共享 guard 作用域内取完即释放——
        // 否则命名 `model: Ref` 会拦后续 `&mut self`（E0502），先量后放、语义不变。
        let (is_media, ed) = {
            let active = self.model.borrow().active_tab;
            let model = self.model.borrow();
            let Some(tab) = model.tabs.get(active) else {
                return; // 零选项卡欢迎页 → 静默
            };
            (tab.reader.is_reader() || tab.readonly_preview, tab.editor.clone())
        };
        // 守卫对齐 find 的判定：音乐整窗 / MediaReader / EPUB 只读预览。
        if self.model.borrow().music_mode || is_media {
            self.goto_line_toast("当前选项卡不支持跳转行");
            cx.notify();
            return;
        }
        ed.update(cx, |ed, cx| ed.open_goto(window, cx));
        cx.notify();
    }

    /// 非文本选项卡 Ctrl+G「不支持跳转行」提示（进入 toast，替换上一条同款提示，不堆积）。
    fn goto_line_toast(&mut self, msg: &str) {
        // 先移除上一条（自持 key，只在跳转行路径使用）。
        let key = self.model.borrow_mut().goto_line_toast_key.take();
        if let Some(key) = key {
            self.remove_notification1::<GotoLineToast>(key);
        }
        let key = gpui::ElementId::from(SharedString::from(format!(
            "goto-line-toast-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        )));
        self.model.borrow_mut().goto_line_toast_key = Some(key.clone());
        self.push_notification(
            Notification::new()
                .id1::<GotoLineToast>(key)
                .title("跳转到行")
                .message(msg)
                .with_type(NotificationKind::Error),
        );
    }

    /// Ctrl+F：活动文本选项卡打开查找条；非文本选项卡（PDF/图片/视频/EPUB/音乐整窗）
    /// → toast「当前选项卡不支持查找」，不崩、不开条。
    pub(super) fn find_in_active_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // SC-2：同 `goto_line_in_active_tab`——判定 + 克隆在 guard 作用域内取完即放。
        let (is_media, ed) = {
            let active = self.model.borrow().active_tab;
            let model = self.model.borrow();
            let Some(tab) = model.tabs.get(active) else {
                return; // 零选项卡欢迎页 → 静默
            };
            (tab.reader.is_reader() || tab.readonly_preview, tab.editor.clone())
        };
        // 守卫对齐 goto_line 的判定：音乐整窗 / MediaReader / EPUB 只读预览。
        if self.model.borrow().music_mode || is_media {
            self.find_toast("当前选项卡不支持查找");
            cx.notify();
            return;
        }
        ed.update(cx, |ed, cx| ed.open_find(window, cx));
        cx.notify();
    }

    /// F3 / Shift+F3：活动文本选项卡复用上次查询 → 下一个 / 上一个；无上次查询不动作。
    /// 非文本选项卡不 toast（同 VS Code：查找快捷键对非文本源静默）。
    pub(super) fn find_step_in_active_tab(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        next: bool,
    ) {
        // SC-2：同 `goto_line_in_active_tab`——判定 + 克隆在 guard 作用域内取完即放。
        let (is_media, ed) = {
            let active = self.model.borrow().active_tab;
            let model = self.model.borrow();
            let Some(tab) = model.tabs.get(active) else {
                return;
            };
            (tab.reader.is_reader() || tab.readonly_preview, tab.editor.clone())
        };
        if self.model.borrow().music_mode || is_media {
            return;
        }
        ed.update(cx, |ed, cx| {
            if next {
                ed.find_next(cx);
            } else {
                ed.find_prev(cx);
            }
        });
        let _ = window;
        cx.notify();
    }

    /// Find「不支持查找」提示（进入 toast，替换上一条同款提示，不堆积）。
    fn find_toast(&mut self, msg: &str) {
        let key = self.model.borrow_mut().find_toast_key.take();
        if let Some(key) = key {
            self.remove_notification1::<FindToast>(key);
        }
        let key = gpui::ElementId::from(SharedString::from(format!(
            "find-toast-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        )));
        self.model.borrow_mut().find_toast_key = Some(key.clone());
        self.push_notification(
            Notification::new()
                .id1::<FindToast>(key)
                .title("查找")
                .message(msg)
                .with_type(NotificationKind::Error),
        );
    }

    /// 创建面板实体 + 聚焦查询框 + 订阅关闭（关闭即清引用；与新开面板同生命周期）。
    /// `notice`：面板提示行文案（空 = 不显示；文件模式未打开文件夹时给出引导）。
    fn open_palette(
        &mut self,
        mode: PaletteMode,
        items: Vec<PaletteItem>,
        window: &mut Window,
        cx: &mut Context<Self>,
        notice: &str,
    ) {
        let previous_focus = window.focused(cx);
        let palette = cx.new(|cx| CommandPalette::new(window, cx, mode, items, previous_focus));
        if !notice.is_empty() {
            palette.update(cx, |p, _| p.set_notice(notice));
        }
        let sub = cx.subscribe(&palette, |this, _e, _: &DismissEvent, cx| {
            // 面板自关（选择/Esc/点击外）→ 清空持有引用，实体随之释放。
            this.palette = None;
            this._palette_sub = None;
            cx.notify();
        });
        // 打开时聚焦顶部查询输入框（键盘直接流入面板，不再冒泡给编辑器）。
        let input_handle = palette.read(cx).input_handle(cx);
        window.focus(&input_handle, cx);
        self.palette = Some(palette);
        self._palette_sub = Some(sub);
    }

    /// 关闭当前面板（toggle 复用：让面板走 Esc 同款路径归还焦点）。
    fn close_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(p) = self.palette.clone() {
            p.update(cx, |pal, cx| pal.dismiss(window, cx));
        }
    }

    /// 命令列表：与顶部菜单共享公共命令表（`shell_commands`）的 label/快捷键单一真值。
    fn command_palette_items(&self, cx: &Context<Self>) -> Vec<PaletteItem> {
        let weak = cx.weak_entity();
        macro_rules! command {
            ($category:expr, $cmd:expr, $handler:expr) => {{
                let w = weak.clone();
                let handler: fn(&mut EditorApp, &mut Window, &mut Context<EditorApp>) = $handler;
                PaletteItem::new(
                    $cmd.label,
                    $category,
                    $cmd.shortcut,
                    move |window, cx| {
                        let _ = w.update(cx, |this, cx| handler(this, window, cx));
                    },
                )
            }};
        }
        vec![
            // 文件
            command!("文件", NEW_FILE, |this, window, cx| {
                this.new_tab(window, cx)
            }),
            command!("文件", OPEN_FILE, |this, window, cx| {
                this.open_dialog(window, cx)
            }),
            command!("文件", OPEN_FOLDER, |this, window, cx| {
                this.open_folder_dialog(window, cx)
            }),
            command!("文件", SAVE, |this, window, cx| {
                this.save(window, cx)
            }),
            command!("文件", SAVE_AS, |this, window, cx| {
                this.save_as(window, cx)
            }),
            command!("文件", QUIT, |_this, _window, cx| cx.quit()),
            // 编辑
            command!("编辑", UNDO, |this, window, cx| {
                Self::dispatch_editor_action(this, window, cx, Box::new(Undo))
            }),
            command!("编辑", REDO, |this, window, cx| {
                Self::dispatch_editor_action(this, window, cx, Box::new(Redo))
            }),
            command!("编辑", CUT, |this, window, cx| {
                Self::dispatch_editor_action(this, window, cx, Box::new(Cut))
            }),
            command!("编辑", COPY, |this, window, cx| {
                Self::dispatch_editor_action(this, window, cx, Box::new(Copy))
            }),
            command!("编辑", PASTE, |this, window, cx| {
                this.paste_normalized(window, cx)
            }),
            command!("编辑", SELECT_ALL, |this, window, cx| {
                Self::dispatch_editor_action(this, window, cx, Box::new(SelectAll))
            }),
            // 查看
            command!("查看", PREVIEW, |this, window, cx| {
                let idx = this.model.borrow().active_tab;
                this.toggle_preview_for_tab(idx, window, cx);
            }),
            command!("查看", TOGGLE_SIDEBAR, |this, _window, cx| {
                this.toggle_sidebar(cx)
            }),
            command!("查看", INCREASE_FONT, |this, _window, cx| {
                this.change_font_size(1.0, cx)
            }),
            command!("查看", DECREASE_FONT, |this, _window, cx| {
                this.change_font_size(-1.0, cx)
            }),
            command!("查看", RESET_FONT, |this, _window, cx| {
                this.reset_font_size(cx)
            }),
            command!("查看", TOGGLE_WRAP, |this, window, cx| {
                this.toggle_wrap(window, cx)
            }),
            command!("设置", THEME, |this, window, cx| {
                this.toggle_theme(window, cx)
            }),
            command!("设置", FOLLOW_THEME, |this, _window, cx| {
                this.follow_system_theme(cx)
            }),
        ]
    }

    /// 文件模式条目：已打开选项卡（名称 + 路径提示），末尾追加「打开文件…」入口。
    ///
    /// 复用侧边栏搜索的「文件名匹配 + 点击激活选项卡」语义（`sidebar_search_file_rows`），
    /// 但只列**已打开文件**，不做全文检索（那是侧栏搜索的职责）；过滤走面板过滤。
    fn file_palette_items(&self, cx: &Context<Self>) -> Vec<PaletteItem> {
        let mut items = self.file_palette_opened_items(cx);
        items.push(Self::file_dialog_item(cx));
        items
    }

    /// 已打开选项卡条目（不含「打开文件…」入口；quick open 扫描刷新时复用打头部分）。
    fn file_palette_opened_items(&self, cx: &Context<Self>) -> Vec<PaletteItem> {
        let weak = cx.weak_entity();
        let mut items: Vec<PaletteItem> = Vec::with_capacity(self.model.borrow().tabs.len());
        for (idx, tab) in self.model.borrow().tabs.iter().enumerate() {
            let name = tab
                .file_path
                .as_ref()
                .and_then(|p| p.file_name())
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "未命名".to_string());
            let detail = tab
                .file_path
                .as_ref()
                .map(|p| {
                    p.parent()
                        .map(|d| d.display().to_string())
                        .unwrap_or_else(|| p.display().to_string())
                })
                .unwrap_or_else(|| "未保存的新文件".to_string());
            let w = weak.clone();
            items.push(PaletteItem::new(name, detail, "", move |window, cx| {
                let _ = w.update(cx, |this, cx| this.activate_tab(idx, window, cx));
            }));
        }
        items
    }

    /// 「打开文件…」系统对话框入口（复用于文件模式基础条目与 quick open 刷新）。
    fn file_dialog_item(cx: &Context<Self>) -> PaletteItem {
        let w = cx.weak_entity();
        PaletteItem::new(
            "打开文件…",
            "系统文件对话框",
            "Ctrl+O",
            move |window, cx| {
                let _ = w.update(cx, |this, cx| this.open_dialog(window, cx));
            },
        )
    }

    /// quick open 扫描完成后的完整候选集 = 已打开文件 + 工作区递归候选 + 「打开文件…」
    /// 入口（对话框条目恒在末尾，保持既有行为）。候选 label = 文件名（basename，
    /// 过滤权重高于路径）、detail = 相对工作区根目录的路径。
    fn build_quick_open_items(
        &self,
        collected: Vec<(PathBuf, String)>,
        cx: &Context<Self>,
    ) -> Vec<PaletteItem> {
        let mut items = self.file_palette_opened_items(cx);
        let weak = cx.weak_entity();
        // 已打开文件不重复列为候选（去重；标题栏部分复用打头，避免同一文件出现两行）。
        let opened: std::collections::HashSet<PathBuf> = self
            .model.borrow().tabs
            .iter()
            .filter_map(|t| t.file_path.clone())
            .collect();
        let mut candidates: Vec<PaletteItem> = Vec::with_capacity(collected.len());
        for (path, rel) in collected.into_iter().take(QUICK_OPEN_MAX_FILES) {
            if opened.contains(&path) {
                continue; // 已打开选项卡已在标题栏列出，跳过重复候选
            }
            let name = path
                .file_name()
                .and_then(|s| s.to_str())
                .map(str::to_owned)
                .unwrap_or_else(|| rel.clone());
            let w = weak.clone();
            candidates.push(PaletteItem::new(name, rel, "", move |window, cx| {
                let _ = w.update(cx, |this, cx| this.open_path_or_activate(&path, window, cx));
            }));
        }
        // 候选夹在已打开文件与「打开文件…」入口之间（扫描结果按查询过滤，不压既有两类）。
        items.extend(candidates);
        items.push(Self::file_dialog_item(cx));
        items
    }

    /// 发起一把 quick open 后台扫描（对齐 sidebar WP-A 姿势）：生产者在后台线程递归
    /// 收文件路径（有界 channel 分块外推），主线程零同步读盘；扫描未完成面板先显示
    /// 已打开文件，完成后一次 `set_items` 刷新候选。换代（重开面板 / 换工作区）时
    /// 旧代异步回调按代丢弃，不串库。
    fn start_quick_open_scan(
        &mut self,
        root: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let generation = self.model.borrow().quick_open_gen.wrapping_add(1);
        self.model.borrow_mut().quick_open_gen = generation;
        let Some(palette) = self.palette.clone() else {
            return;
        };
        // 扫描中：文件模式空态显示「正在扫描…」而非误报「无匹配文件」。
        palette.update(cx, |p, _| p.set_scanning(true));
        let entity = cx.entity();
        let session_dir = UserPrefs::session_dir();
        let (tx, rx) = futures::channel::mpsc::channel::<QuickOpenScanUpdate>(4);
        window
            .spawn(cx, async move |cx| {
                use futures::{SinkExt as _, StreamExt as _};
                // ---- 生产者（后台线程）：递归扫文件夹 → 收文件路径按块外推 ----
                let mut prod_tx = tx.clone();
                let prod_root = root;
                let prod_sess = session_dir;
                cx.background_executor()
                    .spawn(async move {
                        let mut collected: Vec<(PathBuf, String)> = Vec::new();
                        quick_open_collect(&prod_root, &prod_sess, &mut collected);
                        let _ = prod_tx.send(QuickOpenScanUpdate::Chunk(collected)).await;
                        let _ = prod_tx.send(QuickOpenScanUpdate::Done).await;
                    })
                    .detach();
                // 原始 sender 立即释放：生产者外推完毕后 channel 关闭 → 消费者收完退出。
                drop(tx);

                // ---- 消费者（主线程任务）：收齐后一次刷新候选 ----
                let mut rx = rx;
                let mut collected: Vec<(PathBuf, String)> = Vec::new();
                while let Some(update) = rx.next().await {
                    match update {
                        QuickOpenScanUpdate::Chunk(files) => collected.extend(files),
                        QuickOpenScanUpdate::Done => break,
                    }
                }
                let _ = cx.update_window_entity(&entity, move |this, _w, cx| {
                    if this.model.borrow().quick_open_gen != generation {
                        return; // 已被重新打开 / 换工作区取代 → 丢弃旧代结果
                    }
                    let Some(palette) = this.palette.clone() else {
                        return; // 面板已关闭 → 丢弃扫描结果
                    };
                    let items = this.build_quick_open_items(collected, cx);
                    palette.update(cx, |p, pcx| {
                        p.set_items(items);
                        pcx.notify();
                    });
                    cx.notify();
                });
            })
            .detach();
    }
}

/// GoToLine「不支持跳转行」提示通知的（伪）类型标签：配合唯一 key 精确移除
/// （见 `goto_line_toast`）。
struct GotoLineToast;

/// Find「不支持查找」提示通知的（伪）类型标签：配合唯一 key 精确移除（见 `find_toast`）。
struct FindToast;

/// quick open 后台扫描 → 主线程的一块更新（有界 channel，见 `start_quick_open_scan`）。
#[derive(Debug)]
pub(super) enum QuickOpenScanUpdate {
    /// 一批扫描到的文件（绝对路径 + 相对工作区根的位置串），生产者外推完即 `Done`。
    Chunk(Vec<(PathBuf, String)>),
    /// 扫描完成（消费者据此一次性刷新候选）。
    Done,
}

/// quick open 跳过的目录名（黑名单，对齐 sidebar WP-A 文件夹搜索）。
pub(super) const QUICK_OPEN_SKIP_DIRS: &[&str] = &[".git", "target", "node_modules"];
/// quick open 跳过的**超大文件**阈值：> 5MB 不列入候选（元数据判断，不读盘）。
/// `pub`：经 `app` 重导出供 `tests/palette.rs` quick open 断言（§8.6 外提清单）。
pub const QUICK_OPEN_MAX_FILE_BYTES: u64 = 5 * 1024 * 1024;
/// quick open 候选上限：超过即截断（文件夹内文件增删只按「打开面板/切文件夹」刷新）。
/// `pub`：经 `app` 重导出供 `tests/palette.rs` quick open 断言（§8.6 外提清单）。
pub const QUICK_OPEN_MAX_FILES: usize = 500;
/// 二进制 / 非文本扩展名黑名单（按 bool 跳过，不列入候选）。
pub(super) const QUICK_OPEN_BINARY_EXT: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "bmp", "ico", "webp", "heic", "heif", "svgz", "mp4", "mkv",
    "avi", "mov", "wmv", "flv", "webm", "mp3", "wav", "flac", "m4a", "ogg", "wma", "aac",
    "pdf", "exe", "dll", "bin", "obj", "lib", "so", "dylib", "zip", "7z", "rar", "gz", "xz",
    "tar", "iso", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "ttf", "otf", "woff", "woff2",
    "eot",
];

/// 后台递归收集候选文件路径（**不在主线程执行**）：跳过黑名单目录 / 会话目录、跳过
/// 超大文件与二进制扩展名（扩张名黑名单即可，不读盘测内容）。显式栈 DFS（避免深目录
/// 递归爆栈）；单目录读失败 / 单条目元数据失败 → 跳过该条目继续，不中断、不抛错。
/// 条目按文件名排序保证确定性；达到 [`QUICK_OPEN_MAX_FILES`] 即截断。
/// `pub`：经 `app` 重导出供 `tests/palette.rs` quick open 检索断言（§8.6 外提清单）。
pub fn quick_open_collect(
    root: &std::path::Path,
    session_dir: &std::path::Path,
    out: &mut Vec<(PathBuf, String)>,
) {
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if out.len() >= QUICK_OPEN_MAX_FILES {
            return;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue; // 单目录读失败 → 跳过继续
        };
        let mut entries: Vec<std::fs::DirEntry> = rd.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            if out.len() >= QUICK_OPEN_MAX_FILES {
                return;
            }
            let path = entry.path();
            // 目录判定用 `entry.file_type()`（**不跟随**符号链接）：链接指向的目录一律
            // 不递归进栈，防「目录循环引用 → 扫描无限循环」（同 sidebar WP-A 姿势）。
            let Ok(ft) = entry.file_type() else {
                continue; // 单条目元数据失败 → 跳过
            };
            if ft.is_dir() {
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default();
                if QUICK_OPEN_SKIP_DIRS.contains(&name) || path == *session_dir {
                    continue;
                }
                stack.push(path);
            } else {
                let Ok(meta) = std::fs::metadata(&path) else {
                    continue; // 单条目元数据失败 → 跳过
                };
                if !meta.is_file() {
                    continue;
                }
                if meta.len() > QUICK_OPEN_MAX_FILE_BYTES {
                    continue; // 超大文件：不列入候选
                }
                // 二进制 / 非文本扩展名黑名单（扩张名即可，不必读内容嗅探）。
                if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                    let ext = ext.to_ascii_lowercase();
                    if QUICK_OPEN_BINARY_EXT.contains(&ext.as_str()) {
                        continue;
                    }
                }
                let rel = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .into_owned();
                out.push((path, rel));
            }
        }
    }
}
