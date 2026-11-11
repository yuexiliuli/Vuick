//! 预览开关 / 全屏 / 换行的**模型半**。
//!
//! `toggle_preview_for_tab` 的「先激活目标选项卡」是宿主动作（tabs/pane 域），由 shell
//! wrapper 在调用本模块状态函数**之前**执行（`activate_tab` → [`toggle_preview_for_tab_state`]），
//! 顺序与迁移前逐字一致。开关后喂源/装预览走 [`crate::mdwy`]。

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{Context, Window};
use shell_core::AppModel;

/// 切换指定选项卡的 Markdown 预览（眼睛图标 / 菜单 / 命令面板共用）的**状态半**——
/// 调用方（shell wrapper）已先 `activate_tab(idx, window, cx)` 激活该选项卡，此处只翻转
/// 其 `preview_on` 及随后的喂源/释放。EPUB（`readonly_preview`）：正文只在预览区、
/// 编辑区为空，翻转预览无意义，点击仅重新同步内容（持久保持全屏预览，不会落入空白
/// 编辑区）。
pub fn toggle_preview_for_tab_state<H: 'static>(
    model: &Rc<RefCell<AppModel>>,
    idx: usize,
    window: &mut Window,
    cx: &mut Context<H>,
) {
    // 零选项卡（欢迎页）下「预览」无对象：菜单/命令面板都可达这里，先兜底返回。
    if model.borrow().tabs.get(idx).is_none() {
        return;
    }
    let readonly = model.borrow().tabs.get(idx).map(|t| t.readonly_preview).unwrap_or(false);
    let preview_on = {
        let mut m = model.borrow_mut();
        let tab = &mut m.tabs[idx];
        if readonly {
            tab.preview_on = true;
            tab.preview_fullscreen = true;
        } else {
            tab.preview_on = !tab.preview_on;
            // 关闭预览时同步退出全屏
            if !tab.preview_on {
                tab.preview_fullscreen = false;
            }
        }
        tab.preview_on
    };
    // 开启预览 → 编辑区强制换行，方便在分屏里编辑 Markdown；
    // 关闭预览 → 恢复默认：Markdown 文件编辑区仍换行（md 默认），其余按全局偏好。
    let is_md = crate::classify::is_markdown_tab(&model.borrow(), idx);
    let wrap = preview_on || is_md || model.borrow().wrap_enabled;
    model.borrow().tabs[idx].editor.update(cx, |ed, _cx| {
        ed.set_soft_wrap(wrap);
    });
    if preview_on {
        // 统一预览（M5-B5 P4-2）：所有非只读文本选项卡（MD / 纯文本）→ `ensure_md_wy`
        // 同步喂源；EPUB（只读）→ `ensure_epub_wy`（WyEditor 只读宿主 + 内存图/注记/
        // 引文/页码）。`MarkdownPreview` 后端已删除，无回退分支。
        if !readonly {
            crate::mdwy::ensure_md_wy(model, idx, window, cx, true);
        } else if crate::classify::is_epub_tab(&model.borrow(), idx) {
            let images = model.borrow().tabs[idx].epub_images.clone();
            let notes = model.borrow().tabs[idx].epub_notes.clone();
            let content = model.borrow().tabs[idx].content.clone();
            crate::mdwy::ensure_epub_wy(model, idx, window, cx, content.as_ref(), &images, &notes);
        }
    } else {
        // 关闭预览：右栏 WyEditor 预览随预览开关释放（整篇解析/测量缓存随之释放，
        // 内存回到基线）。B3：在途后台整篇装配一并作废（实体被丢弃，完成回调按
        // tab 归属/代号不再安装）。
        crate::mdwy::md_wy_invalidate_async(model, idx);
        model.borrow_mut().tabs[idx].md_wy = None;
        model.borrow_mut().tabs[idx].md_wy_vscroll = gpui::ScrollHandle::new();
    }
    cx.notify();
}

/// 切换当前选项卡预览是否全屏占满（EPUB 阅读模式）。
/// P4-2：预览统一 WyEditor（恒换行），全屏只影响编辑区/布局，无需再设预览换行。
pub fn toggle_preview_fullscreen<H: 'static>(
    model: &Rc<RefCell<AppModel>>,
    cx: &mut Context<H>,
) {
    let idx = model.borrow().active_tab;
    let (fullscreen, editor) = {
        let mut m = model.borrow_mut();
        let Some(tab) = m.tabs.get_mut(idx) else {
            return;
        };
        tab.preview_fullscreen = !tab.preview_fullscreen;
        let fs = tab.preview_fullscreen;
        (fs, tab.editor.clone())
    };
    // 编辑区软换行：进入全屏阅读 → 编辑区强制换行；退出全屏 → 恢复默认。
    let is_md = crate::classify::is_markdown_tab(&model.borrow(), idx);
    let editor_wrap = if fullscreen {
        true
    } else {
        is_md || model.borrow().wrap_enabled
    };
    editor.update(cx, |ed, _cx| {
        ed.set_soft_wrap(editor_wrap);
    });
    cx.notify();
}

/// 切换当前选项卡编辑区的自动换行/不换行，并更新全局编辑器偏好。
/// 只作用于编辑区；Markdown/EPUB 预览始终自动换行，不随此开关变化。
pub fn toggle_wrap<H: 'static>(model: &Rc<RefCell<AppModel>>, cx: &mut Context<H>) {
    let idx = model.borrow().active_tab;
    let wrap_enabled = !model.borrow().wrap_enabled;
    model.borrow_mut().wrap_enabled = wrap_enabled;
    // 全屏阅读模式强制换行，跳过（由全屏切换控制）；否则按编辑器偏好
    if model
        .borrow().tabs
        .get(idx)
        .map(|t| !t.preview_fullscreen)
        .unwrap_or(true)
        && let Some(tab) = model.borrow().tabs.get(idx) {
            tab.editor.update(cx, |ed, _cx| {
                ed.set_soft_wrap(wrap_enabled);
            });
        }
    cx.notify();
}
