//! 图片/视频查看器域逻辑（2026-11-11 迁自 shell 的 app/media.rs）。
//!
//! 活动选项卡的媒体判定与元数据同步（标签标题 / 状态栏跟随）、右键「锁定/解锁」菜单的
//! **逻辑段**：图片/视频/PDF 三指针已收敛成单一 [`MediaReader`]，此处读取经
//! [`MediaReader`] + `match` 去重。**菜单的视图/挂载**（`ui::menu` 弹出结构 + 宿主
//! `media_lock_menu_*` 槽位 + 浮层渲染）留 shell 薄适配；本模块收敛查看器拼接 /
//! 元数据同步 / 锁定判定 / 锁定菜单项与浮层元素构建。

use gpui::{
    div, px, Anchor, Edges, Entity, InteractiveElement, IntoElement, ParentElement, Pixels,
    Point, Styled,
};
use shell_core::{AppModel, MediaReader};
use ui::menu::{PopupMenu, PopupMenuItem};

/// 当前激活选项卡的「可锁定」媒体查看器（图片或视频；PDF 无锁定语义，置 None）。
pub fn active_media_viewer(m: &AppModel) -> Option<MediaReader> {
    let reader = m.tabs.get(m.active_tab).map(|t| t.reader.clone())?;
    reader.is_lockable().then_some(reader)
}

/// 当前激活选项卡是否在「锁定」干净阅读模式（图片/视频查看器实体状态）。
/// 与 [`active_media_viewer`] 同口径：图片 / 视频任一锁定即 true，其余 false。
pub fn active_media_locked(m: &AppModel, cx: &gpui::App) -> bool {
    match m.tabs.get(m.active_tab).map(|t| &t.reader) {
        Some(MediaReader::Image(v)) => v.read(cx).is_locked(),
        #[cfg(feature = "video")]
        Some(MediaReader::Video(v)) => v.read(cx).is_locked(),
        _ => false,
    }
}

/// 媒体查看器切换/加载后的标签标题与状态栏同步（**渲染期调用**，见 shell
/// `render_fullscreen_preview`）：查看器切换图片/视频后 `refresh_windows` 触发本窗口
/// 重绘，全屏预览渲染前据此把选项卡 `file_path` 与状态栏跟到当前媒体（标签标题跟着
/// 媒体走，状态栏显示「图片/视频 n/m：路径」）。仅在激活选项卡持有图片/视频查看器时
/// 更新；**幂等**：路径未变返回 false（调用方不 notify，无重绘循环）。
pub fn sync_active_media_meta(m: &mut AppModel, cx: &mut gpui::App) -> bool {
    let Some(reader) = m.tabs.get(m.active_tab).map(|t| t.reader.clone()) else {
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
    let Some(tab) = m.tabs.get_mut(m.active_tab) else {
        return false;
    };
    if tab.file_path.as_deref() == Some(cur.as_path()) {
        return false;
    }
    tab.file_path = Some(cur);
    m.status = gpui::SharedString::from(label);
    m.status_ok = true;
    true
}

/// 图片/视频「锁定/解锁」右键菜单**单项构建**（锁定时前导 ✓；点击 → 切换查看器锁定态
/// + 强制重绘——宿主重读媒体查看器锁定态 → 隐藏/恢复外壳 → 媒体自动填满/缩小）。
/// 调用方按 [`MediaReader`] 分支；PDF/无（[`active_media_viewer`] 已按 `is_lockable`
/// 过滤）不可达。
pub fn lock_menu_item(media: MediaReader) -> PopupMenuItem {
    match media {
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
    }
}

/// 图片/视频「锁定」右键菜单 Overlay 元素构建（按右键时记录的**窗口坐标**定位）：
/// `AnchoredPositionMode::Window` + `position(pos)`——`position()` 才是 gpui 语义里
/// 「窗口坐标」入参；旧的 `offset(pos)` 缺 `position()` 时会被当作「相对锚定元素自身
/// 静态布局位置的偏移」，而本菜单当前是外壳 flex 直子、静态位恰为 (0,0) 才幸免。
/// 与选项卡栏菜单同法改用 `.position(pos)` 加固，防将来被包进盒子后锚定点被加成非零
/// 静态原点而沉底。菜单是否仍该展示（切走/关闭媒体选项卡即收起）是宿主
/// `media_lock_menu_*` 槽位职责，宿主传入已清理后的 `(pos, menu)`。
pub fn lock_menu_overlay(pos: Point<Pixels>, menu: Entity<PopupMenu>) -> impl IntoElement {
    gpui::deferred(
        gpui::anchored()
            .position_mode(gpui::AnchoredPositionMode::Window)
            .anchor(Anchor::TopLeft)
            .position(pos)
            .snap_to_window_with_margin(Edges::all(px(8.)))
            .child(menu),
    )
    .with_priority(100)
    .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use shell_core::AppModel;

    /// 空模型（无活动选项卡）→ 无「可锁定」媒体查看器。
    #[test]
    fn active_media_viewer_empty_model() {
        assert!(active_media_viewer(&AppModel::default()).is_none());
    }
}
