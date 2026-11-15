//! 媒体查看器（图片 / 视频 / PDF）接线**宿主薄适配**：活动选项卡的媒体
//! 判定与元数据同步（标签标题 / 状态栏跟随）、右键「锁定/解锁」上下文菜单及其浮层渲染
//! 的**逻辑段**在独立 crate `shell_media`（`sync_active_media_meta` /
//! `active_media_viewer` / `active_media_locked` / `lock_menu_item` / `lock_menu_overlay`）；
//! 本模块保留**菜单挂载**与宿主 `media_lock_menu_*` 槽位：
//!
//! - `on_media_right_click`：PDF 分支分派 `on_pdf_right_click`（PDF 无锁定语义）；
//!   图片/视频分支经 `shell_media::media::lock_menu_item` 建「锁定/解锁」菜单项。
//! - `render_media_lock_menu`：按右键记录的窗口坐标，经
//!   `shell_media::media::lock_menu_overlay` 渲染浮层。
//! - `sync_active_media_meta` / `active_media_locked` / `active_media_viewer` 薄适配转发。

use super::*;

impl EditorApp {
    /// 媒体查看器切换/加载后的标签标题与状态栏同步（**渲染期调用**，见
    /// `render_fullscreen_preview`）：查看器切换图片/视频后 `refresh_windows` 触发本窗口
    /// 重绘，全屏预览渲染前据此把选项卡 `file_path` 与状态栏跟到当前媒体。域逻辑
    /// [`shell_media::media::sync_active_media_meta`]。
    pub(super) fn sync_active_media_meta(&mut self, cx: &mut Context<Self>) -> bool {
        shell_media::media::sync_active_media_meta(&mut self.model.borrow_mut(), cx)
    }

    /// 当前激活选项卡是否在「锁定」干净阅读模式（图片/视频查看器实体状态）。域逻辑
    /// [`shell_media::media::active_media_locked`]。
    pub(super) fn active_media_locked(&self, cx: &App) -> bool {
        shell_media::media::active_media_locked(&self.model.borrow(), cx)
    }

    /// 当前激活选项卡是否在播放视频。域逻辑
    /// [`shell_media::media::active_media_is_video`]。
    pub(super) fn active_media_is_video(&self) -> bool {
        shell_media::media::active_media_is_video(&self.model.borrow())
    }

    /// 当前激活选项卡的「可锁定」媒体查看器（图片或视频；PDF 无锁定语义，置 None）。
    /// 域逻辑 [`shell_media::media::active_media_viewer`]。
    pub(super) fn active_media_viewer(&self) -> Option<MediaReader> {
        shell_media::media::active_media_viewer(&self.model.borrow())
    }

    /// 右键 → 活动选项卡是媒体（图片/视频/PDF）时弹相应上下文菜单。图片/视频 →
    /// 「锁定/解锁」（菜单项构建在 `shell_media::media::lock_menu_item`）；PDF →
    /// Phase B2 的 PdfViewer 页操作菜单（`on_pdf_right_click`，PDF 无锁定语义，归入
    /// 菜单槽位分派）。
    pub(super) fn on_media_right_click(
        &mut self,
        ev: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Phase B2：PDF 分支（此前 `is_lockable()` 过滤掉了 Pdf）。事件统一经根元素
        // 冒泡到此，PDF 无「锁定」菜单，直接分派给页操作菜单。
        let is_pdf = self
            .model.borrow().tabs
            .get(self.model.borrow().active_tab)
            .map(|t| matches!(t.payload.reader, MediaReader::Pdf(_)))
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
            move |menu, _window, _cx| menu.item(shell_media::media::lock_menu_item(media.clone())),
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

    /// 图片/视频「锁定」右键菜单 Overlay：按右键时记录的**窗口坐标**定位。浮层元素
    /// 构建在 [`shell_media::media::lock_menu_overlay`]，这里只清退/透传宿主
    /// `media_lock_menu_*` 槽位（切走/关闭媒体选项卡 → 一并收起）。
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
        shell_media::media::lock_menu_overlay(pos, menu).into_any_element()
    }
}
