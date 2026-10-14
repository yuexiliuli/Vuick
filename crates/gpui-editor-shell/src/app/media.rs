//! 图片 / 视频媒体查看器接线：活动选项卡的媒体判定与元数据同步（标签标题 /
//! 状态栏跟随）、右键「锁定/解锁」上下文菜单及其浮层渲染。

use super::*;

impl EditorApp {
    /// 图片查看器切换/加载后的标签标题与状态栏同步（**渲染期调用**，见
    /// `render_fullscreen_preview`）：查看器切换图片后 `refresh_windows` 触发本窗口重绘，
    /// 全屏预览渲染前据此把选项卡 `file_path` 与状态栏跟到当前图片（标签标题跟着图走，
    /// 状态栏显示「图片 n/m：路径」）。仅在激活选项卡持有图片查看器时更新；
    /// **幂等**：路径未变返回 false（调用方不 notify，无重绘循环）。
    pub(super) fn sync_active_image_meta(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(handle) = self
            .tabs
            .get(self.active_tab)
            .and_then(|t| t.image_viewer.clone())
        else {
            return false;
        };
        let (cur, idx, total) = {
            let v = handle.read(cx);
            (v.current_path(), v.current_index(), v.folder_len())
        };
        let Some(cur) = cur else {
            return false;
        };
        let Some(tab) = self.tabs.get_mut(self.active_tab) else {
            return false;
        };
        if tab.file_path.as_deref() == Some(cur.as_path()) {
            return false;
        }
        self.status = SharedString::from(format!("图片 {}/{}：{}", idx + 1, total, cur.display()));
        self.status_ok = true;
        tab.file_path = Some(cur);
        true
    }

    /// 同 [`Self::sync_active_image_meta`]，视频版：查看器同文件夹切换后把选项卡
    /// 标题/状态栏跟到新视频（渲染期幂等调用；路径未变返回 false）。
    #[cfg(feature = "video")]
    pub(super) fn sync_active_video_meta(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(handle) = self
            .tabs
            .get(self.active_tab)
            .and_then(|t| t.video_viewer.clone())
        else {
            return false;
        };
        let (cur, idx, total) = {
            let v = handle.read(cx);
            (v.current_path(), v.current_index(), v.folder_len())
        };
        let Some(cur) = cur else {
            return false;
        };
        let Some(tab) = self.tabs.get_mut(self.active_tab) else {
            return false;
        };
        if tab.file_path.as_deref() == Some(cur.as_path()) {
            return false;
        }
        self.status = SharedString::from(format!("视频 {}/{}：{}", idx + 1, total, cur.display()));
        self.status_ok = true;
        tab.file_path = Some(cur);
        true
    }

    /// 当前激活选项卡是否在「锁定」干净看图模式（读图片查看器实体状态）。
    pub(super) fn active_image_locked(&self, cx: &App) -> bool {
        self.tabs
            .get(self.active_tab)
            .and_then(|t| t.image_viewer.as_ref())
            .is_some_and(|v| v.read(cx).is_locked())
    }

    /// 当前激活选项卡是否在「纯视频」干净播放模式（读视频查看器实体状态）。
    #[cfg(feature = "video")]
    pub(super) fn active_video_locked(&self, cx: &App) -> bool {
        self.tabs
            .get(self.active_tab)
            .and_then(|t| t.video_viewer.as_ref())
            .is_some_and(|v| v.read(cx).is_locked())
    }

    /// 当前激活选项卡的媒体查看器（图片或视频；均有则取图片）。无媒体 → None。
    pub(super) fn active_media_viewer(&self) -> Option<ActiveMedia> {
        let tab = self.tabs.get(self.active_tab)?;
        if let Some(img) = &tab.image_viewer {
            return Some(ActiveMedia::Image(img.clone()));
        }
        #[cfg(feature = "video")]
        if let Some(vid) = &tab.video_viewer {
            return Some(ActiveMedia::Video(vid.clone()));
        }
        None
    }

    /// 图片/视频右键 → 在光标处弹「锁定/解锁」上下文菜单（仅活动选项卡是图片/视频时
    /// 生效）。锁定 = 隐藏全部非媒体 UI、媒体充满窗口；解锁回归。就地换掉旧的（若已打开）。
    pub(super) fn on_media_right_click(
        &mut self,
        ev: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(media) = self.active_media_viewer() else {
            return;
        };
        // 先收掉选项卡栏右键菜单与上一份媒体菜单（若有），再在光标处新建
        // （旧订阅带 emitter 身份校验，不会误清新菜单）
        self.tab_bar_menu_entity = None;
        self.tab_bar_menu_pos = None;
        self.tab_bar_menu_sub = None;
        self.media_lock_menu_entity = None;
        self.media_lock_menu_pos = None;
        self.media_lock_menu_sub = None;
        let menu = PopupMenu::build(window, cx, move |menu, _window, _cx| {
            let item = match media.clone() {
                ActiveMedia::Image(img) => {
                    let viewer_render = img.clone();
                    PopupMenuItem::element(move |_window, cx| {
                        let locked = viewer_render.read(cx).is_locked();
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(div().w(px(18.)).child(if locked { "✓" } else { "" }))
                            .child("锁定图片视图")
                            .debug_selector(|| "media-lock-menu-item".into())
                    })
                    .on_click(move |_click, window, cx| {
                        img.update(cx, |v, cx| v.toggle_lock(cx));
                        // 强制重绘：宿主重读媒体查看器锁定态 → 隐藏/恢复外壳 → 媒体自动填满/缩小
                        window.refresh();
                    })
                }
                #[cfg(feature = "video")]
                ActiveMedia::Video(vid) => {
                    let viewer_render = vid.clone();
                    PopupMenuItem::element(move |_window, cx| {
                        let locked = viewer_render.read(cx).is_locked();
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(div().w(px(18.)).child(if locked { "✓" } else { "" }))
                            .child("锁定视频视图")
                            .debug_selector(|| "media-lock-menu-item".into())
                    })
                    .on_click(move |_click, window, cx| {
                        vid.update(cx, |v, cx| v.toggle_lock(cx));
                        window.refresh();
                    })
                }
            };
            menu.item(item)
        });
        // 聚焦并记住开菜单前的焦点（关闭后归还）
        let previous_focus = window.focused(cx);
        let focus_handle = menu.read(cx).focus_handle(cx);
        menu.update(cx, |m, _| m.set_previous_focus(previous_focus));
        window.focus(&focus_handle, cx);
        // 关闭（选择/Esc/点击外）→ 清理；emitter 身份校验避免旧菜单的关闭误清新菜单
        let menu_entity = menu.clone();
        let sub = cx.subscribe(&menu_entity, move |this, emitter, _: &DismissEvent, cx| {
            if this.media_lock_menu_entity.as_ref() == Some(&emitter) {
                this.media_lock_menu_entity = None;
                this.media_lock_menu_pos = None;
            }
            cx.notify();
        });
        self.media_lock_menu_pos = Some(ev.position);
        self.media_lock_menu_entity = Some(menu);
        self.media_lock_menu_sub = Some(sub);
        cx.notify();
    }

    /// 图片/视频「锁定」右键菜单 Overlay：按右键时记录的**窗口坐标**定位
    /// （`AnchoredPositionMode::Window` + `position(pos)`——`position()` 才是 gpui 语义里
    /// 「窗口坐标」入参；旧的 `offset(pos)` 缺 `position()` 时会被当作「相对锚定元素自身
    /// 静态布局位置的偏移」，而本菜单当前是外壳 flex 直子、静态位恰为 (0,0) 才幸免）。
    /// 与选项卡栏菜单（tabs.rs `render_tab_bar_menu`）同法改用 `.position(pos)` 加固，
    /// 防将来被包进盒子后锚定点被加成非零静态原点而沉底。
    pub(super) fn render_media_lock_menu(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let still_media = self.active_media_viewer().is_some();
        if !still_media {
            // 切走/关闭了媒体选项卡 → 一并收起右键菜单
            self.media_lock_menu_entity = None;
            self.media_lock_menu_pos = None;
        }
        let Some(pos) = self.media_lock_menu_pos else {
            return div().into_any_element();
        };
        let Some(menu) = self.media_lock_menu_entity.clone() else {
            return div().into_any_element();
        };
        deferred(
            anchored()
                .position_mode(gpui::AnchoredPositionMode::Window)
                .anchor(Anchor::TopLeft)
                .position(pos)
                .snap_to_window_with_margin(Edges::all(px(8.)))
                .child(menu),
        )
        .with_priority(100)
        .into_any_element()
    }
}
