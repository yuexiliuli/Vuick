//! 媒体查看器（图片 / 视频 / PDF）接线：活动选项卡的媒体判定与元数据同步（标签标题 /
//! 状态栏跟随）、右键「锁定/解锁」上下文菜单及其浮层渲染。图片/视频/PDF 三指针已收敛
//! 成单一 [`MediaReader`]，此处读取经 [`MediaReader`] + `match` 去重。

use super::*;

impl EditorApp {
    /// 媒体查看器切换/加载后的标签标题与状态栏同步（**渲染期调用**，见
    /// `render_fullscreen_preview`）：查看器切换图片/视频后 `refresh_windows` 触发本窗口
    /// 重绘，全屏预览渲染前据此把选项卡 `file_path` 与状态栏跟到当前媒体（标签标题跟着
    /// 媒体走，状态栏显示「图片/视频 n/m：路径」）。仅在激活选项卡持有图片/视频查看器时
    /// 更新；**幂等**：路径未变返回 false（调用方不 notify，无重绘循环）。
    pub(super) fn sync_active_media_meta(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(reader) = self.tabs.get(self.active_tab).map(|t| t.reader.clone()) else {
            return false;
        };
        let (label, cur) = match reader {
            MediaReader::Image(v) => {
                let (cur, idx, total) = {
                    let view = v.read(cx);
                    (view.current_path(), view.current_index(), view.folder_len())
                };
                let Some(cur) = cur else {
                    return false;
                };
                (format!("图片 {}/{}：{}", idx + 1, total, cur.display()), cur)
            }
            #[cfg(feature = "video")]
            MediaReader::Video(v) => {
                let (cur, idx, total) = {
                    let view = v.read(cx);
                    (view.current_path(), view.current_index(), view.folder_len())
                };
                let Some(cur) = cur else {
                    return false;
                };
                (format!("视频 {}/{}：{}", idx + 1, total, cur.display()), cur)
            }
            MediaReader::Pdf(_) | MediaReader::None => return false,
        };
        let Some(tab) = self.tabs.get_mut(self.active_tab) else {
            return false;
        };
        if tab.file_path.as_deref() == Some(cur.as_path()) {
            return false;
        }
        self.status = SharedString::from(label);
        self.status_ok = true;
        tab.file_path = Some(cur);
        true
    }

    /// 当前激活选项卡是否在「锁定」干净阅读模式（图片/视频查看器实体状态）。
    /// 与 `active_media_locked` 同口径：图片 / 视频任一锁定即 true，其余 false。
    pub(super) fn active_media_locked(&self, cx: &App) -> bool {
        match self.tabs.get(self.active_tab).map(|t| &t.reader) {
            Some(MediaReader::Image(v)) => v.read(cx).is_locked(),
            #[cfg(feature = "video")]
            Some(MediaReader::Video(v)) => v.read(cx).is_locked(),
            _ => false,
        }
    }

    /// 当前激活选项卡的「可锁定」媒体查看器（图片或视频；PDF 无锁定语义，置 None）。
    pub(super) fn active_media_viewer(&self) -> Option<MediaReader> {
        let reader = self.tabs.get(self.active_tab).map(|t| t.reader.clone())?;
        reader.is_lockable().then_some(reader)
    }

    /// 右键 → 活动选项卡是媒体（图片/视频/PDF）时弹相应上下文菜单。图片/视频 →
    /// 「锁定/解锁」；PDF → Phase B2 的 PdfViewer 页操作菜单（`on_pdf_right_click`，
    /// PDF 无锁定语义，归入菜单槽位分派）。
    pub(super) fn on_media_right_click(
        &mut self,
        ev: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Phase B2：PDF 分支（此前 `is_lockable()` 过滤掉了 Pdf）。事件统一经根元素
        // 冒泡到此，PDF 无「锁定」菜单，直接分派给页操作菜单。
        let is_pdf = self
            .tabs
            .get(self.active_tab)
            .map(|t| matches!(t.reader, MediaReader::Pdf(_)))
            .unwrap_or(false);
        if is_pdf {
            self.on_pdf_right_click(ev, window, cx);
            return;
        }
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
        let (menu, sub) = ui::menu::spawn_context_menu(
            window,
            cx,
            move |menu, _window, _cx| {
                let item = match media.clone() {
                    MediaReader::Image(img) => {
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
                    MediaReader::Video(vid) => {
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
                    // active_media_viewer 已按 is_lockable 过滤，PDF/无 不可达。
                    MediaReader::Pdf(_) | MediaReader::None => unreachable!(),
                };
                menu.item(item)
            },
            |this, emitter, cx| {
                // 关闭（选择/Esc/点击外）→ 清理；emitter 身份校验避免旧菜单的关闭误清新菜单
                if this.media_lock_menu_entity.as_ref() == Some(&emitter) {
                    this.media_lock_menu_entity = None;
                    this.media_lock_menu_pos = None;
                }
                cx.notify();
            },
        );
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
