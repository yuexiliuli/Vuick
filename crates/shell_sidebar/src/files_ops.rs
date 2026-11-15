//! 侧边栏「文件树操作」实体子模块。
//!
//! 右键菜单（新建文件/文件夹、重命名、删除、资源管理器定位、复制路径、剪切/复制）的
//! **菜单结构 / 输入弹层生命周期**收在域实体；菜单动作与「新建/重命名/删除」等
//! **文件系统副作用**（落盘 + 树失效 + 会话落盘）经 [`crate::host::SidebarHost`] /
//! [`crate::prompt::FileTreePromptHost`] 分发到宿主（shell 的 `app/file_tree_ops.rs`）。
//! 右键菜单 / 输入弹层的实体句柄（pos/menu/sub/prompt）为实体自有，宿主仅
//! 镜像菜单实体与弹层实体（测试断言用）。

use gpui::{
    div, AppContext, Context, DismissEvent, Entity, InteractiveElement, IntoElement,
    MouseDownEvent, ParentElement, Window,
};
use ui::menu::label_menu_item;

use crate::chrome::Sidebar;
use crate::host::SidebarHost;
use crate::prompt::{FileTreePrompt, FileTreePromptHost, FileTreePromptKind};

impl<H: SidebarHost + FileTreePromptHost> Sidebar<H> {
    /// 文件树项右键 → 弹上下文菜单（位置存窗口坐标）：目录 = 新建文件 / 新建文件夹 /
    /// 重命名 / 删除；文件 = 重命名 / 删除。新建类仅目录适用。关闭经 `DismissEvent`
    /// 订阅清理（同选项卡栏右键，见 shell 侧 `on_tab_bar_right_click`）。
    /// 菜单项动作经 [`crate::host::SidebarHost`] 分发到宿主（文件副作用/弹层入口）。
    pub fn on_file_tree_right_click(
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
        self.file_tree_menu.clear();
        let weak = self.host_weak();
        let (menu, sub) = ui::menu::spawn_context_menu(
            window,
            cx,
            move |menu, _window, _cx| {
                let mut m = menu;
                if is_dir {
                    m = m.item({
                        let weak = weak.clone();
                        let parent = path.to_path_buf();
                        label_menu_item("新建文件…", "file-tree-menu-new-file")
                            .on_click(move |_, window, cx| {
                            let _ = weak.update(cx, |this, cx| {
                                this.sidebar_open_file_tree_prompt(
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
                        label_menu_item("新建文件夹…", "file-tree-menu-new-folder")
                            .on_click(move |_, window, cx| {
                            let _ = weak.update(cx, |this, cx| {
                                this.sidebar_open_file_tree_prompt(
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
                    label_menu_item("重命名…", "file-tree-menu-rename")
                        .on_click(move |_, window, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            this.sidebar_open_file_tree_prompt(
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
                m = m.item({
                    let weak = weak.clone();
                    let target = path.clone();
                    label_menu_item("删除…", "file-tree-menu-delete")
                        .on_click(move |_, window, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            this.sidebar_file_tree_delete_request(&target, is_dir, window, cx);
                        });
                    })
                });
                // ── 第二组：在资源管理器中显示（Explorer /select,<绝对路径>）──
                m = m.separator();
                m = m.item({
                    let weak = weak.clone();
                    let target = path.clone();
                    label_menu_item("在资源管理器中显示", "file-tree-menu-reveal")
                        .on_click(move |_, _window, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            this.sidebar_file_tree_reveal_in_explorer(&target, cx);
                        });
                    })
                });
                // ── 第三组：复制路径 / 复制相对路径（无引号文本）──
                m = m.separator();
                m = m.item({
                    let weak = weak.clone();
                    let target = path.clone();
                    label_menu_item("复制路径", "file-tree-menu-copy-path")
                        .on_click(move |_, _window, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            this.sidebar_file_tree_copy_path(&target, cx);
                        });
                    })
                });
                m = m.item({
                    let weak = weak.clone();
                    let target = path.clone();
                    label_menu_item("复制相对路径", "file-tree-menu-copy-rel-path")
                        .on_click(move |_, _window, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            this.sidebar_file_tree_copy_relative_path(&target, cx);
                        });
                    })
                });
                // ── 第四组：剪切 / 复制（文件操作，CF_HDROP + PreferredDropEffect）──
                m.separator()
                    .item({
                        let weak = weak.clone();
                        let target = path.clone();
                        label_menu_item("剪切", "file-tree-menu-cut")
                            .on_click(move |_, _window, cx| {
                            let _ = weak.update(cx, |this, cx| {
                                this.sidebar_file_tree_cut(&target, cx);
                            });
                        })
                    })
                    .item({
                        let weak = weak.clone();
                        let target = path.clone();
                        label_menu_item("复制", "file-tree-menu-copy-file")
                            .on_click(move |_, _window, cx| {
                            let _ = weak.update(cx, |this, cx| {
                                this.sidebar_file_tree_copy_file(&target, cx);
                            });
                        })
                    })
            },
            |this, emitter, cx| {
                // 身份校验（与实体侧同款）：只有被关的菜单仍是当前菜单才清字段 + 同步宿主
                // 测试镜像——旧菜单的关闭不得误清新菜单的镜像（同选项卡栏右键姿势）。
                if this.file_tree_menu.entity.as_ref() == Some(&emitter) {
                    this.file_tree_menu.clear();
                    this.host_call(cx, |app, cx| app.sidebar_set_file_tree_menu(None, cx));
                }
            },
        );
        self.file_tree_menu.pos = Some(ev.position);
        self.file_tree_menu.entity = Some(menu.clone());
        self.file_tree_menu.sub = Some(sub);
        self.host_call(cx, |app, cx| app.sidebar_set_file_tree_menu(Some(menu), cx));
    }

    /// 文件树右键菜单 Overlay（光标处弹出，同选项卡栏右键菜单定位姿势）。
    /// 无窗口/上下文依赖（pos/菜单实体在实体自有字段），宿主渲染期经 `sidebar_view.read`
    /// 直调。
    pub fn render_file_tree_menu(&self) -> gpui::AnyElement {
        let Some(pos) = self.file_tree_menu.pos else {
            return div().into_any_element();
        };
        let Some(menu) = self.file_tree_menu.entity.clone() else {
            return div().into_any_element();
        };
        div()
            .id("file-tree-menu")
            .debug_selector(|| "file-tree-menu".into())
            .child(ui::menu::popup_overlay(pos, menu, ui::menu::MENU_EDGE_MARGIN))
            .into_any_element()
    }

    /// 打开文件树输入弹层（新建/重命名共用）：建实体 + 订阅（Dismiss → 清引用）+
    /// 聚焦输入框。弹层实体/订阅收进域实体（泛型宿主 `H`）。宿主镜像同步由**宿主侧
    /// caller**（facade `open_file_tree_prompt` / 菜单服务）在 `sidebar_view.update` 返回后
    /// 经 [`Sidebar::file_tree_prompt_entity`] 读取 + `cx.notify()` 完成——本方法多在宿主
    /// 已持 `&mut` 的 `app.update`/服务闭包内被调，**不得**在此反调宿主（RefCell 双借）。
    /// 关闭（Dismiss 事件）回调是独立事件派发，可安全反调宿主清镜像。
    pub fn open_file_tree_prompt(
        &mut self,
        kind: FileTreePromptKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let host = self.host_weak();
        let previous_focus = window.focused(cx);
        let prompt = cx.new(|cx| {
            FileTreePrompt::new(host, kind, previous_focus, window, cx)
        });
        let sub = cx.subscribe(&prompt, |this, _e, _: &DismissEvent, cx| {
            this.file_tree_prompt = None;
            this.file_tree_prompt_sub = None;
            this.host_call(cx, |app, cx| app.sidebar_set_file_tree_prompt(None, cx));
        });
        let handle = prompt.read(cx).input_focus(cx);
        window.focus(&handle, cx);
        self.file_tree_prompt = Some(prompt);
        self.file_tree_prompt_sub = Some(sub);
    }

    /// 弹层实体句柄（宿主 facade 在 `sidebar_view.update` 后读取并镜像 + notify）。
    pub fn file_tree_prompt_entity(&self) -> Option<Entity<FileTreePrompt<H>>> {
        self.file_tree_prompt.clone()
    }

    /// 文件树输入弹层的渲染挂点（根级 overlay；无弹层 → 空元素）。
    /// 无窗口/上下文依赖（弹层实体在实体自有字段），宿主渲染期经 `sidebar_view.read` 直调。
    pub fn render_file_tree_prompt(&self) -> gpui::AnyElement {
        match self.file_tree_prompt.clone() {
            Some(p) => p.into_any_element(),
            None => div().into_any_element(),
        }
    }
}
