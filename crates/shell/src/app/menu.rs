//! menu：顶部菜单栏 + 下拉菜单（gpui_component DropdownMenu / PopupMenu）。

use super::*;
use crate::actions::{Copy, Cut, Redo, SelectAll, Undo};
use shell_commands::{
    COPY, CUT, DECREASE_FONT, FOLLOW_THEME, INCREASE_FONT, NEW_FILE, OPEN_FILE, OPEN_FOLDER,
    PASTE, PREVIEW, QUIT, REDO, RESET_FONT, SAVE, SAVE_AS, SELECT_ALL, THEME,
    TOGGLE_MUSIC_PLAYER, TOGGLE_WRAP, UNDO,
};
use session_store::RecentPanelHost;
use ui::button::{Button, ButtonVariants as _};
use ui::menu::{DropdownMenu as _, PopupMenu, PopupMenuItem};

impl EditorApp {
    /// 构造一个带右侧快捷键提示的菜单项；点击后通过弱引用更新本应用状态。
    fn menu_action_item<F>(
        label: &'static str,
        shortcut: &'static str,
        weak: gpui::WeakEntity<Self>,
        f: F,
    ) -> PopupMenuItem
    where
        F: Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    {
        PopupMenuItem::element(move |_, cx| {
            // 快捷键提示灰字：渲染期取当前明暗模式（外壳色单一来源）。
            let chrome = ui::theme::ChromeColors::for_mode(
                ui::theme::Theme::global(cx).is_dark(),
            );
            let mut content = div()
                .w_full()
                .flex()
                .items_center()
                .justify_between()
                .gap_3()
                .child(label);
            if !shortcut.is_empty() {
                content = content.child(div().text_color(chrome.text_tertiary).child(shortcut));
            }
            content
        })
        .on_click(move |_, window, cx| {
            let _ = weak.update(cx, |this, cx| f(this, window, cx));
        })
    }

    /// "文件"菜单。label / 快捷键来自公共命令表（`shell_commands` 单一真值）。
    /// 「打开最近…」已由「keep_open + 居中大面板」改为 **flyout 悬浮次级菜单**
    /// （悬停展开到右侧，见 [`Self::recent_flyout_items`]）；`cx` 用于在每次菜单
    /// 打开时快照最近列表（与 recent_panel「每次打开取最新」同语义）。
    fn file_items(
        menu: PopupMenu,
        weak: gpui::WeakEntity<Self>,
        _window: &mut Window,
        cx: &mut Context<PopupMenu>,
    ) -> PopupMenu {
        // 打开菜单时快照最近列表：菜单实体在本次开合周期内保持，快照在该周期内恒定
        // （recent_panel 的「每次打开取最新」语义；列表更新仍走 file_ops 记录/落盘）。
        let recent_folders = weak
            .upgrade()
            .map(|e| e.read(cx).model.borrow().recent_folders.clone())
            .unwrap_or_default();
        let recent_files = weak
            .upgrade()
            .map(|e| e.read(cx).model.borrow().recent_files.clone())
            .unwrap_or_default();
        menu.item(Self::menu_action_item(
            NEW_FILE.menu_label,
            NEW_FILE.shortcut,
            weak.clone(),
            |this, window, cx| {
                this.new_tab(window, cx)
            },
        ))
        .item(Self::menu_action_item(
            OPEN_FILE.menu_label,
            OPEN_FILE.shortcut,
            weak.clone(),
            |this, window, cx| this.open_dialog(window, cx),
        ))
        .item(Self::menu_action_item(
            OPEN_FOLDER.menu_label,
            OPEN_FOLDER.shortcut,
            weak.clone(),
            |this, window, cx| this.open_folder_dialog(window, cx),
        ))
        // 「打开最近…」：flyout 悬浮次级菜单（悬停展开到右侧；子项点击 → 打开/清理
        // + 整链关闭）。取代旧的 keep_open + 居中大面板（recent_panel 存 dormancy）。
        // 注意根行内容**不用 `w_full`**：flyout 根行右侧由组件自动追加「›」指示
        // （`ml_auto` 占满剩余宽），若内容撑满整行会把箭头挤出/错位（PDF 背景色同款姿势）。
        .item({
            PopupMenuItem::element(move |_, _| {
                div()
                    .debug_selector(|| "recent-menu-open".into())
                    .child("打开最近…")
            })
            .flyout(Self::recent_flyout_items(&weak, recent_folders, recent_files))
        })
        .item(Self::menu_action_item(
            SAVE.menu_label,
            SAVE.shortcut,
            weak.clone(),
            |this, window, cx| this.save(window, cx),
        ))
        .item(Self::menu_action_item(
            SAVE_AS.menu_label,
            SAVE_AS.shortcut,
            weak.clone(),
            |this, window, cx| this.save_as(window, cx),
        ))
        .separator()
        // 「导出/导入设置…」：配置 + 会话全量打包为单个 .json（系统保存/打开对话框）。
        .item(Self::menu_action_item(
            "导出设置…",
            "",
            weak.clone(),
            |this, window, cx| this.export_settings(window, cx),
        ))
        .item(Self::menu_action_item(
            "导入设置…",
            "",
            weak.clone(),
            |this, window, cx| this.import_settings(window, cx),
        ))
        .separator()
        .item(Self::session_restore_menu_item(weak.clone()))
        .item(Self::remember_reading_progress_menu_item(weak.clone()))
        .item(Self::close_to_tray_menu_item(weak.clone()))
        .separator()
        .item(Self::menu_action_item(
            QUIT.menu_label,
            QUIT.shortcut,
            weak,
            |_, _window, cx| cx.quit(),
        ))
    }

    /// 「打开最近…」flyout 次级菜单每类（文件夹 / 文件）的**最大行数**：与 recent_panel
    /// 四区域「每类 ≤10」对齐，避免一次子菜单塞满全部 [RECENT_MAX]（50 条）把次级面板
    /// 顶出窗口底缘（近窗缘适配受限时尤其明显）。
    const RECENT_FLYOUT_REGION_MAX: usize = 10;

    /// 「打开最近…」的 flyout 次级菜单项装配：最近文件夹（≤[`RECENT_FLYOUT_REGION_MAX`]）
    /// → 分隔线 → 最近文件（≤上限）→ 分隔线 → 「更多...」（打开最近大面板，清空前的
    /// 固定动作项）→ 分隔线 → 「清空最近记录」。
    /// `folders`/`files` 是**打开菜单时**的最近列表快照（每次打开取最新）。
    /// 两类全空 → 单个禁用占位项「暂无最近记录」（不可点，不出现「更多...」/清空）。
    /// 所有子项都不 `keep_open` → 点击后**整链关闭**（次级 + 父菜单，组件
    /// `activate_flyout` 的非 keep_open 路径——与「打开某路径/清空/更多...」动作一致）。
    fn recent_flyout_items(
        weak: &gpui::WeakEntity<Self>,
        folders: Vec<String>,
        files: Vec<String>,
    ) -> Vec<PopupMenuItem> {
        if folders.is_empty() && files.is_empty() {
            return vec![PopupMenuItem::element(move |_, cx| {
                let chrome = ui::theme::ChromeColors::for_mode(
                    ui::theme::Theme::global(cx).is_dark(),
                );
                div()
                    .w_full()
                    .debug_selector(|| "recent-empty".into())
                    .text_color(chrome.text_tertiary)
                    .child("暂无最近记录")
            })
            .disabled(true)];
        }
        let mut children = Vec::new();
        for (i, p) in folders
            .iter()
            .take(Self::RECENT_FLYOUT_REGION_MAX)
            .enumerate()
        {
            children.push(Self::recent_row_item(weak.clone(), true, p, i));
        }
        // 分隔线只在**其上方区域非空**时插入：单类为空（另一类非空）时避免「首行悬浮
        // 分隔线 / 两类间双分隔线」的可见瑕疵（recent_panel 空区域有「暂无」占位，此处
        // 空区域整段省略）。
        if !folders.is_empty() {
            children.push(PopupMenuItem::separator());
        }
        for (i, p) in files
            .iter()
            .take(Self::RECENT_FLYOUT_REGION_MAX)
            .enumerate()
        {
            children.push(Self::recent_row_item(weak.clone(), false, p, i));
        }
        if !files.is_empty() {
            children.push(PopupMenuItem::separator());
        }
        // 「更多...」：打开最近大面板（`open_recent_panel`，All 模式 = 搜索框 + 两组）。
        // 与「清空最近记录」同为底部固定动作项，二者以分隔线隔开。
        children.push(Self::recent_more_item(weak.clone()));
        children.push(PopupMenuItem::separator());
        children.push(Self::recent_clear_item(weak.clone()));
        children
    }

    /// 最近列表行（文件夹/文件）：**复用 recent_panel 的行命名**（`recent-folder-row-{i}` /
    /// `recent-file-row-{i}`，各测试的 debug 定位共用）；标签 = 名称（截断）+ 完整路径
    /// 灰字副文本（封顶宽截断，不把 flyout 面板撑出窗口）。
    /// 点击：路径存在 → 走打开中心入口（[`open_folder_path`] / [`open_file`]）；
    /// 失效 → 从对应列表移除 + 状态栏提示（与 recent_panel `run_item` 一致）。
    /// 非 keep_open → 点击后整链关闭。
    fn recent_row_item(
        weak: gpui::WeakEntity<Self>,
        is_folder: bool,
        path: &str,
        idx: usize,
    ) -> PopupMenuItem {
        let path = path.to_string();
        let name = std::path::Path::new(&path)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.clone());
        let debug = if is_folder {
            format!("recent-folder-row-{idx}")
        } else {
            format!("recent-file-row-{idx}")
        };
        // 渲染闭包与点击闭包各自持有一份（两个 `move` 闭包不能共享所有权，克隆待用）。
        let name_render = name.clone();
        let path_render = path.clone();
        let debug_render = debug.clone();
        PopupMenuItem::element(move |_, cx| {
            // 每次渲染重建本地副本：内层 debug_selector 的 `move` 闭包捕获**本地**（而非
            // 外层闭包捕获），避免「从 Fn 闭包捕获中移出」。名称/路径同理各建本地。
            let debug = debug_render.clone();
            let name = name_render.clone();
            let path = path_render.clone();
            let chrome = ui::theme::ChromeColors::for_mode(
                ui::theme::Theme::global(cx).is_dark(),
            );
            div()
                .w_full()
                .debug_selector(move || debug.clone())
                .flex()
                .items_center()
                .gap_2()
                .min_w(px(0.))
                // 名称：占满可伸展、超长截断。
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_overflow(gpui::TextOverflow::Truncate("…".into()))
                        .child(SharedString::from(name)),
                )
                // 完整路径：灰字副文本、封顶宽截断。
                .child(
                    div()
                        .max_w(px(120.))
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_overflow(gpui::TextOverflow::Truncate("…".into()))
                        .text_size(px(11.))
                        .text_color(chrome.text_tertiary)
                        .child(SharedString::from(path)),
                )
                .into_any_element()
        })
        .on_click(move |_click, window, cx| {
            let _ = weak.update(cx, |app, cx| {
                if is_folder {
                    if std::path::Path::new(&path).is_dir() {
                        app.open_folder_path(std::path::PathBuf::from(&path), window, cx);
                    } else {
                        app.remove_recent_folder(&path, window, cx);
                    }
                } else if std::path::Path::new(&path).is_file() {
                    app.open_file(&path, window, cx);
                } else {
                    app.remove_recent_file(&path, window, cx);
                }
            });
        })
    }

    /// 「清空最近记录」：清空文件夹 + 文件两类最近记录并落盘。非 keep_open →
    /// 点击后整链关闭。
    fn recent_clear_item(weak: gpui::WeakEntity<Self>) -> PopupMenuItem {
        PopupMenuItem::element(|_, _| {
            div()
                .w_full()
                .debug_selector(|| "recent-clear".into())
                .child("清空最近记录")
        })
        .on_click(move |_click, _window, cx| {
            let _ = weak.update(cx, |app, cx| {
                app.model.borrow_mut().recent_folders.clear();
                app.model.borrow_mut().recent_files.clear();
                app.persist_prefs();
                cx.notify();
            });
        })
    }

    /// 「更多...」：打开最近大面板（[`Self::open_recent_panel`]，全量两组 + 搜索框）。
    /// 非 keep_open → 点击后**整链关闭**（次级 + 父菜单）：点「更多...」→ 菜单链关 +
    /// 面板开。两类全空时不在 flyout 装配（走「暂无最近记录」占位分支）。
    fn recent_more_item(weak: gpui::WeakEntity<Self>) -> PopupMenuItem {
        PopupMenuItem::element(|_, _| {
            div()
                .w_full()
                .debug_selector(|| "recent-menu-more".into())
                .child("更多...")
        })
        .on_click(move |_click, window, cx| {
            let _ = weak.update(cx, |app, cx| {
                app.open_recent_panel(window, cx);
            });
        })
    }

    /// "编辑"菜单。
    fn edit_items(
        menu: PopupMenu,
        weak: gpui::WeakEntity<Self>,
        _window: &mut Window,
        _cx: &mut Context<PopupMenu>,
    ) -> PopupMenu {
        menu.item(Self::menu_action_item(
            UNDO.menu_label,
            UNDO.shortcut,
            weak.clone(),
            |this, window, cx| Self::dispatch_editor_action(this, window, cx, Box::new(Undo)),
        ))
        .item(Self::menu_action_item(
            REDO.menu_label,
            REDO.shortcut,
            weak.clone(),
            |this, window, cx| Self::dispatch_editor_action(this, window, cx, Box::new(Redo)),
        ))
        .separator()
        .item(Self::menu_action_item(
            CUT.menu_label,
            CUT.shortcut,
            weak.clone(),
            |this, window, cx| Self::dispatch_editor_action(this, window, cx, Box::new(Cut)),
        ))
        .item(Self::menu_action_item(
            COPY.menu_label,
            COPY.shortcut,
            weak.clone(),
            |this, window, cx| Self::dispatch_editor_action(this, window, cx, Box::new(Copy)),
        ))
        .item(Self::menu_action_item(
            PASTE.menu_label,
            PASTE.shortcut,
            weak.clone(),
            |this, window, cx| this.paste_normalized(window, cx),
        ))
        .separator()
        .item(Self::menu_action_item(
            SELECT_ALL.menu_label,
            SELECT_ALL.shortcut,
            weak,
            |this, window, cx| Self::dispatch_editor_action(this, window, cx, Box::new(SelectAll)),
        ))
    }

    /// "查看"菜单。
    fn view_items(
        menu: PopupMenu,
        weak: gpui::WeakEntity<Self>,
        _window: &mut Window,
        _cx: &mut Context<PopupMenu>,
    ) -> PopupMenu {
        menu.item(Self::menu_action_item(
            PREVIEW.menu_label,
            // 快捷键提示走命令表（空串 = 无提示）：P2-8 原 Ctrl+P = 预览切换已改开
            // 「打开文件快选」→ 预览切换现由标签栏眼睛按钮承担。
            PREVIEW.shortcut,
            weak.clone(),
            |this, window, cx| {
                let idx = this.model.borrow().active_tab;
                this.toggle_preview_for_tab(idx, window, cx);
            },
        ))
        .item(Self::live_test_menu_item(weak.clone()))
        .separator()
        // P4-1：EPUB 章节翻转（WyEditor 只读宿主 / MarkdownPreview 回退通用）。
        // 快捷键 Ctrl+[/Ctrl+] 仅为**菜单提示**（历史未实际绑定，本仓库不发其他键绑定）。
        .item(Self::menu_action_item(
            "上一章",
            "Ctrl+[",
            weak.clone(),
            |this, _window, cx| {
                this.epub_flip_chapter(-1, cx);
                cx.notify();
            },
        ))
        .item(Self::menu_action_item(
            "下一章",
            "Ctrl+]",
            weak.clone(),
            |this, _window, cx| {
                this.epub_flip_chapter(1, cx);
                cx.notify();
            },
        ))
        .separator()
        .item(Self::menu_action_item(
            INCREASE_FONT.menu_label,
            INCREASE_FONT.shortcut,
            weak.clone(),
            |this, _window, cx| this.change_font_size(1.0, cx),
        ))
        .item(Self::menu_action_item(
            DECREASE_FONT.menu_label,
            DECREASE_FONT.shortcut,
            weak.clone(),
            |this, _window, cx| this.change_font_size(-1.0, cx),
        ))
        .item(Self::menu_action_item(
            RESET_FONT.menu_label,
            RESET_FONT.shortcut,
            weak.clone(),
            |this, _window, cx| this.reset_font_size(cx),
        ))
        .separator()
        .item(Self::wrap_menu_item(weak.clone()))
        .separator()
        .item(Self::menu_action_item(
            THEME.menu_label,
            THEME.shortcut,
            weak.clone(),
            |this, window, cx| this.toggle_theme(window, cx),
        ))
        .item(Self::menu_action_item(
            FOLLOW_THEME.menu_label,
            FOLLOW_THEME.shortcut,
            weak.clone(),
            |this, _window, cx| this.follow_system_theme(cx),
        ))
        // 音乐播放器：整窗音乐模式开关（Ctrl+M / 命令面板 / 菜单共用同一动作）。
        .item(Self::menu_action_item(
            TOGGLE_MUSIC_PLAYER.menu_label,
            TOGGLE_MUSIC_PLAYER.shortcut,
            weak.clone(),
            |this, window, cx| this.toggle_music_player(window, cx),
        ))
        .separator()
        // 媒体打开位置偏好（与「音乐播放器」同区、加分隔）：媒体在新窗口打开 /
        // 复用已有媒体窗口（☑/☐ 动态项，切换即持久化，见 media_window.rs）。
        .item(Self::media_new_window_menu_item(weak.clone()))
        .item(Self::reuse_media_window_menu_item(weak))
    }

    /// ☑/☐ 复选标记字形（U+2611 / U+2610）。真实 Windows 字库对两码点**独立逐字符回退**，
    /// 若 glyph 与文本合成单 run，两码点会落不同 family → em-box/ascent 不同 → 尺寸/基线
    /// 不一致；故必须独立元素 + 统一 Segoe UI Symbol 字族（Win7+ 恒在，同时覆盖两码点，
    /// 同 metrics）。
    const TOGGLE_ON_MARK: &str = "☑";
    const TOGGLE_OFF_MARK: &str = "☐";

    /// ☑/☐ 前缀独立元素 + 统一 Segoe UI Symbol 字族：真实 Windows 字库逐字符回退
    /// 会让两码点落不同 family 导致尺寸/基线不一致，统一字族后同 metrics。
    /// 注：Segoe UI Symbol 是 Windows 符号字体（Win7+ 恒在）；非 Windows 平台无此族时
    /// 仍走系统回退（不劣化）。
    fn toggle_mark(on: bool) -> impl IntoElement {
        div()
            .font_family("Segoe UI Symbol")
            .child(if on {
                Self::TOGGLE_ON_MARK
            } else {
                Self::TOGGLE_OFF_MARK
            })
    }

    /// ☑/☐ 切换项行装配：glyph 独立元素（统一 Segoe UI Symbol 字族）+ 文本元素双
    /// child flex 布局，方块与文字间距对齐现有项。
    fn toggle_item(on: bool, text: impl Into<SharedString>) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(Self::toggle_mark(on))
            .child(text.into())
    }

    /// 「媒体在新窗口打开」菜单项：动态显示当前开关状态（前导 ☑/☐），点击切换并立即
    /// 持久化（同 [`Self::wrap_menu_item`] 的 ☑/☐ 动态项模式）。
    fn media_new_window_menu_item(weak: gpui::WeakEntity<Self>) -> PopupMenuItem {
        let weak2 = weak.clone();
        PopupMenuItem::element(move |_, cx| {
            let on = weak
                .upgrade()
                .map(|e| e.read(cx).model.borrow().media_new_window)
                .unwrap_or(true);
            let chrome = ui::theme::ChromeColors::for_mode(
                ui::theme::Theme::global(cx).is_dark(),
            );
            div()
                .w_full()
                .flex()
                .items_center()
                .justify_between()
                .gap_3()
                .child(Self::toggle_item(on, "媒体在新窗口打开"))
                .child(div().text_color(chrome.text_tertiary).child(""))
        })
        .on_click(move |_, window, cx| {
            let _ = weak2.update(cx, |this, cx| this.toggle_media_new_window(window, cx));
        })
    }

    /// 「复用已有媒体窗口」菜单项：动态显示当前开关状态（前导 ☑/☐），点击切换并立即
    /// 持久化。`media_new_window` 关闭时**置灰**（渲染期灰字 + 点击期兜底拒绝）——该项
    /// 只在「新窗口打开」开启时才有意义。
    fn reuse_media_window_menu_item(weak: gpui::WeakEntity<Self>) -> PopupMenuItem {
        let weak2 = weak.clone();
        PopupMenuItem::element(move |_, cx| {
            let enabled = weak
                .upgrade()
                .map(|e| e.read(cx).model.borrow().media_new_window)
                .unwrap_or(true);
            let on = weak
                .upgrade()
                .map(|e| e.read(cx).model.borrow().reuse_media_window)
                .unwrap_or(true);
            let chrome = ui::theme::ChromeColors::for_mode(
                ui::theme::Theme::global(cx).is_dark(),
            );
            div()
                .w_full()
                .flex()
                .items_center()
                .justify_between()
                .gap_3()
                // 置灰：media_new_window 关闭时该开关无意义（popup 菜单项 disabled 在构件
                // 期设置、此处无 App 上下文，改用渲染期灰字 + 点击期兜底拒绝，双保险）。
                .child(
                    div()
                        .text_color(if enabled {
                            chrome.text_primary
                        } else {
                            chrome.text_tertiary
                        })
                        .child(Self::toggle_item(on, "复用已有媒体窗口")),
                )
        })
        .on_click(move |_, window, cx| {
            let _ = weak2.update(cx, |this, cx| this.toggle_reuse_media_window(window, cx));
        })
    }

    /// "帮助"菜单。
    fn help_items(
        menu: PopupMenu,
        weak: gpui::WeakEntity<Self>,
        _window: &mut Window,
        _cx: &mut Context<PopupMenu>,
    ) -> PopupMenu {
        menu.item(Self::menu_action_item(
            "关于",
            "",
            weak,
            |this, _window, cx| {
                // P2-① 唯一性：修正错位文案（原为「剪贴板生效」的复制提示）——关于 =
                // 应用名 + 版本。
                this.model.borrow_mut().status = SharedString::from(format!(
                    "Vuick {}：基于 GPUI 的文本 / 代码 / Markdown 编辑器",
                    env!("CARGO_PKG_VERSION")
                ));
                this.model.borrow_mut().status_ok = true;
                cx.notify();
            },
        ))
    }

    /// "查看"菜单：「预览编辑测试」——本项目 `markdown_wysiwyg::WyEditor`
    /// （以 `Doc` 为数据源、所见即所得编辑的 WYSIWYG；已取代 zorite 移植的 `live`）。
    fn live_test_menu_item(weak: gpui::WeakEntity<Self>) -> PopupMenuItem {
        let weak2 = weak.clone();
        PopupMenuItem::element(move |_, cx| {
            let on = weak
                .upgrade()
                .map(|e| {
                    let e = e.read(cx);
                    let i = e.model.borrow().active_tab;
                    e.model.borrow().tabs.get(i).map(|t| t.live2.is_some()).unwrap_or(false)
                })
                .unwrap_or(false);
            div()
                .w_full()
                .flex()
                .items_center()
                .justify_between()
                .gap_3()
                .child(if on {
                    "● 预览编辑测试"
                } else {
                    "○ 预览编辑测试"
                })
        })
        .on_click(move |_, window, cx| {
            let _ = weak2.update(cx, |this, cx| this.toggle_live_test(window, cx));
        })
    }

    /// "自动换行"菜单项：动态显示当前是否开启（前导 ☑/☐），点击切换。
    /// label 与快捷键来自公共命令表（`shell_commands::TOGGLE_WRAP`）。
    fn wrap_menu_item(weak: gpui::WeakEntity<Self>) -> PopupMenuItem {
        let weak2 = weak.clone();
        PopupMenuItem::element(move |_, cx| {
            let on = weak
                .upgrade()
                .map(|e| e.read(cx).model.borrow().wrap_enabled)
                .unwrap_or(true);
            let chrome = ui::theme::ChromeColors::for_mode(
                ui::theme::Theme::global(cx).is_dark(),
            );
            div()
                .w_full()
                .flex()
                .items_center()
                .justify_between()
                .gap_3()
                .child(Self::toggle_item(on, TOGGLE_WRAP.menu_label))
                .child(div().text_color(chrome.text_tertiary).child(TOGGLE_WRAP.shortcut))
        })
        .on_click(move |_, window, cx| {
            let _ = weak2.update(cx, |this, cx| this.toggle_wrap(window, cx));
        })
    }

    /// "恢复上次会话"菜单项：动态显示当前开关状态（前导 ☑/☐），点击切换并立即持久化
    /// （同 [`Self::wrap_menu_item`] 的 ☑/☐ 动态项模式）。
    fn session_restore_menu_item(weak: gpui::WeakEntity<Self>) -> PopupMenuItem {
        let weak2 = weak.clone();
        PopupMenuItem::element(move |_, cx| {
            let on = weak
                .upgrade()
                .map(|e| e.read(cx).model.borrow().session_restore)
                .unwrap_or(true);
            div()
                .w_full()
                .flex()
                .items_center()
                .justify_between()
                .gap_3()
                .child(Self::toggle_item(on, "恢复上次会话"))
        })
        .on_click(move |_, window, cx| {
            let _ = weak2.update(cx, |this, cx| this.toggle_session_restore(window, cx));
        })
    }

    /// 「关闭到系统托盘」菜单标签（动态 ☑/☐ 复选标记，同 `session_restore_menu_item`
    /// 模式；菜单渲染已改用 `toggle_item`，本函数保留专供 R4 集成断言锁定 ☑/☐ 字面值）。
    #[doc(hidden)]
    pub fn close_to_tray_label(on: bool) -> &'static str {
        if on {
            "☑ 关闭时最小化到系统托盘"
        } else {
            "☐ 关闭时最小化到系统托盘"
        }
    }

    /// "关闭到系统托盘"菜单项：动态显示当前开关状态（前导 ☑/☐），点击切换并立即持久化
    /// （同 [`Self::session_restore_menu_item`] 的 ☑/☐ 动态项模式）。
    fn close_to_tray_menu_item(weak: gpui::WeakEntity<Self>) -> PopupMenuItem {
        let weak2 = weak.clone();
        PopupMenuItem::element(move |_, cx| {
            let on = weak
                .upgrade()
                .map(|e| e.read(cx).model.borrow().close_to_tray)
                .unwrap_or(false);
            div()
                .w_full()
                .flex()
                .items_center()
                .justify_between()
                .gap_3()
                .child(Self::toggle_item(on, "关闭时最小化到系统托盘"))
        })
        .on_click(move |_, window, cx| {
            let _ = weak2.update(cx, |this, cx| this.toggle_close_to_tray(window, cx));
        })
    }

    /// 「记住阅读进度」菜单标签（动态 ☑/☐ 复选标记，同 `session_restore_menu_item`
    /// 模式；菜单渲染已改用 `toggle_item`，本函数保留专供 R5 集成断言锁定 ☑/☐ 字面值）。
    #[doc(hidden)]
    pub fn remember_reading_progress_label(on: bool) -> &'static str {
        if on {
            "☑ 记住阅读进度"
        } else {
            "☐ 记住阅读进度"
        }
    }

    /// "记住阅读进度"菜单项：动态显示当前开关状态（前导 ☑/☐），点击切换并立即持久化
    /// （同 [`Self::close_to_tray_menu_item`] 的 ☑/☐ 动态项模式）。
    fn remember_reading_progress_menu_item(weak: gpui::WeakEntity<Self>) -> PopupMenuItem {
        let weak2 = weak.clone();
        PopupMenuItem::element(move |_, cx| {
            let on = weak
                .upgrade()
                .map(|e| e.read(cx).model.borrow().restore_reading_progress)
                .unwrap_or(true);
            div()
                .w_full()
                .flex()
                .items_center()
                .justify_between()
                .gap_3()
                .child(Self::toggle_item(on, "记住阅读进度"))
        })
        .on_click(move |_, window, cx| {
            let _ = weak2.update(cx, |this, cx| this.toggle_remember_reading_progress(window, cx));
        })
    }

    /// 把焦点还给当前激活选项卡的编辑区（编辑动作派发到 Input 上下文所需）。
    pub(super) fn focus_active_editor(this: &mut Self, window: &mut Window, cx: &mut Context<Self>) {
        let idx = this.model.borrow().active_tab;
        let focus = this.tab_focus_handle(idx, cx);
        window.focus(&focus, cx);
    }

    /// 编辑类命令助手：先 `focus_active_editor` 把焦点还给当前激活选项卡编辑区，再向
    /// 窗口派发编辑动作。右键「编辑」菜单（`edit_items`）与命令面板命令表（`palette.rs`）
    /// 共 5×2 个同构闭包（Undo/Redo/Cut/Copy/SelectAll）此前各自手抄，收敛于此。两处
    /// 目标解析一致（都走 `focus_active_editor` 的 active_tab），故直接共用；
    /// 粘贴（Paste）走 `paste_normalized` 不在此列。
    pub(super) fn dispatch_editor_action(
        this: &mut Self,
        window: &mut Window,
        cx: &mut Context<Self>,
        action: Box<dyn gpui::Action>,
    ) {
        Self::focus_active_editor(this, window, cx);
        window.dispatch_action(action, cx);
    }

    /// 菜单按钮组（文件/编辑/查看/帮助），并入自绘标题栏使用。
    pub(super) fn render_menu_buttons(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let weak = cx.weak_entity();
        let text_primary = self.text_primary();
        let make = |id: &'static str,
                    label: &'static str,
                    items: fn(
                        PopupMenu,
                        gpui::WeakEntity<Self>,
                        &mut Window,
                        &mut Context<PopupMenu>,
                    ) -> PopupMenu,
                    weak: gpui::WeakEntity<Self>| {
            Button::new(id)
                .label(label)
                .ghost()
                .w(px(56.))
                .h(MENU_BAR_H)
                .flex()
                .items_center()
                .px_2()
                .rounded_sm()
                .text_size(px(13.))
                .text_color(text_primary)
                .dropdown_menu(move |menu, window, cx| items(menu, weak.clone(), window, cx))
        };

        div()
            .id("menu-bar")
            .flex()
            .items_center()
            .gap_1()
            .child(make("menu-btn-file", "文件", Self::file_items, weak.clone()))
            .child(make("menu-btn-edit", "编辑", Self::edit_items, weak.clone()))
            .child(make("menu-btn-view", "查看", Self::view_items, weak.clone()))
            .child(make("menu-btn-help", "帮助", Self::help_items, weak))
    }

    /// 「打开最近」居中大面板入口：文件菜单「打开最近…」flyout 的「更多...」项点击
    /// （[`Self::recent_more_item`]）调用；打开即全量两组 + 搜索框（[`session_store::RecentPanel`]
    /// All 模式）。面板是 Ctrl+P 命令面板同款 Acrylic 居中卡片，打开即聚焦搜索框、
    /// 订阅 `DismissEvent`（择项/Esc/点击外 → 清引用释放实体）。
    /// 面板已开 → 直接复用既有单槽（`self.recent_panel`），不叠多层。
    #[doc(hidden)]
    pub fn open_recent_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.recent_panel.is_some() {
            return;
        }
        // 最近列表快照：面板打开期间被别处更新 → 下次打开才反映最新（面板实体不
        // 在渲染路径反读宿主演变状态，避免 re-entrant borrow；记录逻辑仍走 file_ops）。
        let folders = self.model.borrow().recent_folders.clone();
        let files = self.model.borrow().recent_files.clone();
        // 打开前的焦点 = 仍展开的文件菜单句柄；面板关闭（动作未带走焦点）时归还它。
        let previous_focus = window.focused(cx);
        let host = cx.weak_entity();
        let panel = cx.new(|cx| {
            session_store::RecentPanel::new(host, folders, files, previous_focus, cx)
        });
        let sub = cx.subscribe(&panel, |this, _e, _: &DismissEvent, cx| {
            this.recent_panel = None;
            this._recent_panel_sub = None;
            cx.notify();
        });
        // 打开即聚焦面板（↑↓/Enter/Esc 直达；源菜单无 dismiss 不再抢焦点）。
        let handle = panel.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
        self.recent_panel = Some(panel);
        self._recent_panel_sub = Some(sub);
        cx.notify();
    }
}

/// [`RecentPanelHost`]（session_store 定义的最小宿主接口）的 shell 实现：
/// 转发到 EditorApp 既有 `pub(super)` 同名方法，**行为完全不变**（方法不在此新增任何
/// 逻辑，只做宿主接入点）。实现体内 `self.x(...)` 方法调用在对 `EditorApp`（具体类型）
/// 做候选解析时固有方法优先于 trait 方法 → 命中既有方法，不会递归。
/// `clear_recents` 原为面板内联的「清两类 + 落盘 + notify」，现收敛到宿主一处（语义等价）。
impl RecentPanelHost for EditorApp {
    fn open_folder_path(
        &mut self,
        path: std::path::PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_folder_path(path, window, cx);
    }

    fn remove_recent_folder(&mut self, path: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.remove_recent_folder(path, window, cx);
    }

    fn open_file(&mut self, path: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.open_file(path, window, cx);
    }

    fn remove_recent_file(&mut self, path: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.remove_recent_file(path, window, cx);
    }

    fn clear_recents(&mut self, cx: &mut Context<Self>) {
        self.model.borrow_mut().recent_folders.clear();
        self.model.borrow_mut().recent_files.clear();
        self.persist_prefs();
        cx.notify();
    }
}
