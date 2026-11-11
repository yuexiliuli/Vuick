//! WYSIWYG 活区（`live2`，本项目 `markdown_wysiwyg::WyEditor`）的**进入**。
//!
//! 即「预览编辑测试」toggle ON 分支：以当前编辑区源快照（O(1) COW Rope）为数据源创建
//! live2 实体（大文档后台解析 / 共享复用右栏已解析 Doc·消双解析），开启编辑日志收集
//! （方案2）并订阅变更 → 行/字/脏计数 + 防抖会话持久化（经宿主
//! [`crate::host::PreviewHost`]）。toggle OFF 分支（`commit_live` 写回）是宿主收尾，留 shell。
//!
//! ⚠️ `EditorApp::toggle_live_test` 的**前置守卫**（`is_markdown_tab` 校验 + 状态栏
//! 文案 + 已开则 `commit_live` 关闭）留在 shell wrapper，此处只实现「未开则进入」。

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{AppContext, Context, SharedString, Window};
use markdown_wysiwyg::{WyEditor, WyEvent};
use shell_core::{AppModel, MdWyEarly};

use crate::PreviewHost;

/// 测试探针：toggle ON 命中**共享复用右栏已解析 Doc** 的次数——大文档切 live2 走
/// `md_wy.doc().clone()` O(1) 复用（消双解析）而非 `open_from_rope` 整篇重解析。
/// 回归测试据此断言「复用路径真的被走上」。
/// 常驻编译（无 `#[cfg(test)]`）：shell 的测试构建把本 crate 当普通依赖编译（`cfg(test)`
/// 恒关），探针须常驻才可被 shell 测试经重导出读取。
#[doc(hidden)]
pub static LIVE_ON_MD_WY_REUSE: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// 进入 WYSIWYG 活区（toggle ON 分支；前置守卫由 shell wrapper 完成）：创建 live2
/// WyEditor（编辑日志收集 + 变更订阅），源 = 当前编辑区 Rope 快照。
pub fn enter_live<H: PreviewHost>(
    model: &Rc<RefCell<AppModel>>,
    idx: usize,
    window: &mut Window,
    cx: &mut Context<H>,
) {
    // 源快照：O(1) COW Rope。大文档走**后台解析**（与预览的后台 scan_blocks 同构）：
    // UI 线程只做 O(1) 快照 + 空壳编辑器创建 + 完成装配；整篇 O(n) 解析
    // （`Doc::open` 6.4MB 实测 ~200ms）在 `background_executor`。小文档保持同步
    // （无「空白→内容」闪烁）。加载完成前 `is_loaded()==false`，宿主读写文本
    // （保存/退出）回退源码编辑区（`tab_effective_text` / `commit_live` 已守卫）。
    const ASYNC_LOAD_MIN_BYTES: usize = 512 * 1024;
    let rope = model.borrow().tabs[idx].editor.read(cx).text().clone();
    let async_load = rope.len() >= ASYNC_LOAD_MIN_BYTES;
    // —— 共享复用右栏已解析 Doc（消双解析、O(1) 即见）——
    // 守卫沿增量桥不变式：`md_wy` 已 loaded（且与编辑区字节等长 → 内容一致）、无
    // 整篇在途（`md_wy_pending`/`md_wy_pending_full` 为 None、`md_wy_early` 非
    // Pending|Failed）。命中 → O(1) 克隆共享右栏已解析结构（`Doc::clone`），后台只
    // 补算首帧预建（~36ms release 的 Pass1 输入）即装配——不再 `open_from_rope`
    // 整篇重解析（原大文档切 live 白屏比解析还久的根因之一）。未命中 → 保留既有
    // 异步 `open_from_rope` 兜底。小文档（`!async_load`）不蹭复用：同步 `with_text`
    // 本就无空白帧、零风险不变行为。
    let reuse = model.borrow().tabs[idx].md_wy.as_ref().filter(|w| {
        async_load
            && w.read(cx).is_loaded()
            && model.borrow().tabs[idx].md_wy_install.pending.is_none()
            && model.borrow().tabs[idx].md_wy_install.pending_full.is_none()
            && !matches!(
                model.borrow().tabs[idx].md_wy_install.early,
                Some(MdWyEarly::Pending { .. }) | Some(MdWyEarly::Failed)
            )
            && w.read(cx).len() == rope.len()
    })
    .map(|w| {
        // O(1) Doc 克隆（Arc/rope COW 段共享，不复制数据）；$ / [! 标记计数
        // 克隆自只读 md_wy（与克隆 Doc 同源，编辑前不变式成立）。
        (
            w.read(cx).doc().clone(),
            w.read(cx).math_dollar_cnt(),
            w.read(cx).alert_marker_cnt(),
        )
    });
    if reuse.is_some() {
        LIVE_ON_MD_WY_REUSE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    let ed = if reuse.is_some() || async_load {
        cx.new(|cx| WyEditor::new(window, cx))
    } else {
        let text = SharedString::from(rope.to_string());
        cx.new(|cx| WyEditor::new(window, cx).with_text(text.as_ref()))
    };
    // 同步全局字号（新建即套用；WY 字号由编辑器自持，measure/prepaint 一致）
    let fs = model.borrow().font_size;
    ed.update(cx, |ed, _| ed.set_font_size(gpui::px(fs)));
    // 图片相对路径基于文件所在目录解析（与预览一致）
    let base_dir = model.borrow().tabs[idx]
        .file_path
        .as_ref()
        .and_then(|p| p.parent())
        .map(|d| d.to_path_buf());
    ed.update(cx, |ed, _| ed.set_base_dir(base_dir));
    // 开启编辑日志收集（方案2）：仅本 live2 实例缴存 op（右栏 `md_wy` 等不开启）。
    // 大文档后台加载的 `install_open` 会清空加载窗口内的陈旧 op（收集开关保留）。
    ed.update(cx, |ed, _| ed.capture_live_ops());
    let model_c = model.clone();
    let ed_sub = cx.subscribe(&ed, move |this, emitter, ev: &WyEvent, cx| {
        match ev {
            WyEvent::Changed => {
                // 命名 RefMut guard + 用完即 drop（借用约束），
                // 否则宿主 `preview_schedule_session_flush` 在 guard 存活期 → E0502。
                let mut model_sub = model_c.borrow_mut();
                if let Some(tab) =
                    model_sub.tabs.iter_mut().find(|t| t.live2.as_ref() == Some(&emitter))
                {
                    // **O(1) 增量计数**（`WyEditor::line_count/char_count` 直读
                    // `Doc` 行起始表/增量字符计数，不物化全文），源码编辑区与右侧
                    // Markdown 预览不再做每键/停手整篇镜像（大文档 set_text 级联
                    // 重解析会卡 7-8s）——它们随左面板 WYSIWYG 一起，到退出
                    // `commit_live` 时统一写回。
                    tab.line_count = emitter.read(cx).line_count();
                    tab.char_count = emitter.read(cx).char_count();
                    // 本 live 会话有内容编辑（仅当事件来自 live2 实体——订阅
                    // 只挂在 live2、且上方已按 `live2 == emitter` 过滤，md_wy 右栏
                    // 的 Changed 到不了这里）：切回时据此决定是否整篇写回。
                    tab.live2_dirty = true;
                    if !tab.dirty {
                        tab.dirty = true;
                    }
                    cx.notify();
                    drop(model_sub);
                    // WYSIWYG 内容变更 → 防抖持久化（结构变化则即时，见宿主 session 模块）。
                    this.preview_schedule_session_flush(cx);
                }
            }
            WyEvent::OpenPage(p) => {
                model_c.borrow_mut().status = SharedString::from(format!("WYSIWYG 点击链接：{p}"));
                model_c.borrow_mut().status_ok = true;
            }
        }
    });
    let mut m = model.borrow_mut();
    let tab = &mut m.tabs[idx];
    tab.live2 = Some(ed.clone());
    tab._live2_sub = Some(ed_sub);
    tab.live_edit = true;
    // 新 live 会话从「源快照」起，本会话编辑标记清 false（仅编辑才置位）。
    tab.live2_dirty = false;
    // 源编辑区即将被 WYSIWYG 取代（隐藏/切离）：在此「被隐藏」生命周期点复位
    // 其鼠标拖选态（与 `commit_live` 同用 `cancel_mouse_drag` 统一钩子）。菜单
    // 点击漏进的 MouseDown 恰在切离前把 `dragging` 置位过（见 `commit_live` 注释），
    // 若不在此刻清掉，隐藏期的残留会在切回后仅移动鼠标时把选区从 anchor 拉长。
    tab.editor.update(cx, |ed, _| ed.cancel_mouse_drag());
    if let Some((doc_clone, math_dollar, alert_marker)) = reuse {
        let tab_idx = idx;
        let ed_install = ed.clone();
        let model_a = model.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    // 复用路径：零拷贝复用右栏已解析 Doc，只补算首帧 Pass1 预建
                    // （纯 doc 依赖量、无窗口/主题）——首帧经 `pass1_from_prebuild`
                    // 轻量物化 + arm 冷开保温（prebuild=Some），不丢冷开路径设施。
                    let prebuild =
                        markdown_wysiwyg::frame::build_prebuild_pass1(&doc_clone);
                    markdown_wysiwyg::WyEditor::open_result_from_doc(
                        doc_clone,
                        math_dollar,
                        alert_marker,
                        Some(prebuild),
                    )
                })
                .await;
            let _ = this.update(cx, |_this, cx| {
                // 只装配仍挂在原 tab 的 live2（用户在装配期间切换/退出则丢弃）。
                if model_a
                    .borrow().tabs
                    .get(tab_idx)
                    .is_some_and(|t| t.live2.as_ref() == Some(&ed_install))
                {
                    ed_install.update(cx, |ed, _| ed.install_open(result));
                    let mut m = model_a.borrow_mut();
                    let tab = &mut m.tabs[tab_idx];
                    tab.line_count = ed_install.read(cx).line_count();
                    tab.char_count = ed_install.read(cx).char_count();
                    cx.notify();
                }
            });
        })
        .detach();
    } else if async_load {
        let tab_idx = idx;
        let ed_install = ed.clone();
        let model_a = model.clone();
        cx.spawn(async move |this, cx| {
            let result =
                cx.background_executor()
                    .spawn(async move {
                        markdown_wysiwyg::WyEditor::open_from_rope(rope)
                    })
                    .await;
            let _ = this.update(cx, |_this, cx| {
                // 只装配仍挂在原 tab 的 live2（用户在解析期间切换/退出则丢弃）。
                if model_a
                    .borrow().tabs
                    .get(tab_idx)
                    .is_some_and(|t| t.live2.as_ref() == Some(&ed_install))
                {
                    ed_install.update(cx, |ed, _| ed.install_open(result));
                    let mut m = model_a.borrow_mut();
                    let tab = &mut m.tabs[tab_idx];
                    tab.line_count = ed_install.read(cx).line_count();
                    tab.char_count = ed_install.read(cx).char_count();
                    cx.notify();
                }
            });
        })
        .detach();
    }
    m.status = SharedString::from("预览编辑测试：开（本项目 Doc 管线 WYSIWYG，初步实现）");
    m.status_ok = true;
    let focus = gpui::Focusable::focus_handle(ed.read(cx), cx);
    window.focus(&focus, cx);
}
