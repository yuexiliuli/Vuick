//! PDF 右键菜单的**动作逻辑与错误映射**。
//!
//! 菜单的**视图/挂载**（`ui::menu` 弹出结构 + 宿主 `pdf_menu_*` 字段 + 浮层渲染）留 shell
//! 薄适配；此处收敛页操作动作（插入/删除/提取/删除注释）、添加书签动作、失败 toast 判定
//! 与错误文案映射，以及宿主服务面 [`PdfHost`]。
//!
//! ## 宿主服务面 [`PdfHost`]
//!
//! 页面操作成功/失败 toast 与「右键新弹前互关弹窗」是对宿主的唯二交互：动作函数经泛型
//! 宿主 `H: PdfHost` + `Context<H>` 参数化（宿主 `EditorApp` 实现 + 薄适配转发），
//! 经 `cx.update_window_entity` 再入宿主实体后调 [`PdfHost::pdf_host_toast`] 落地。

use gpui::{App, Context, Entity, PromptLevel, VisualContext, Window};
use pdf::{PdfViewer, is_writeback_busy_error};
use shell_core::AppModel;

use crate::bookmarks::add_bookmark;

/// PDF 右键 toast 的自托管 key 标记类型（同 goto_line / find 的 GotoLineToast 姿势；
/// 宿主 `PdfHost::pdf_host_toast` 以它作通知去重键——先移除同 key 再推送，不堆积）。
pub struct PdfToast;

/// PDF 域**宿主服务面**：把 PDF 域动作对宿主
/// 的交互收敛为最小接口，使本 crate 不依赖 shell 的具体宿主类型。shell 侧以
/// `impl PdfHost for EditorApp` 提供实现（method 转发既有通知/菜单字段清理）。
pub trait PdfHost: 'static {
    /// PDF 操作 toast（自托管 key：先移除上一条再推送，不堆积；宿主实现）。
    fn pdf_host_toast(&mut self, title: &str, msg: String)
    where
        Self: Sized;

    /// 右键新弹前互关宿主全部自绘弹出菜单（选项卡栏右键 / 媒体锁定 / 上份 PDF 菜单）。
    /// 宿主实现清 `tab_bar_menu_*` / `media_lock_menu_*` / `pdf_menu_*` 槽位（域动作经
    /// 它收口，不直读宿主字段）。
    fn pdf_host_close_all_popups(&mut self)
    where
        Self: Sized;
}

/// 把 PDF 写回失败文案整理成 toast 消息：命中「目标被其他程序占用/只读」类错误时，
/// 在 pdf crate 的指引（已含路径 1：关闭占用程序后重试）上补第二条路——「另存为新副本」
/// （副本为文案指引；一次性另存 UI 未接线，headless 无法端到端验证，R3 低成本默认）。
pub fn pdf_modify_error_msg(err: &str) -> String {
    if is_writeback_busy_error(err) {
        format!("{err}；若暂时无法关闭占用程序，可「另存为新副本」以保留本次修改")
    } else {
        err.to_string()
    }
}

/// 「添加书签」动作的**模型 + 文案**部分（PDF 右键菜单项 on_click 调用）：当前页 +
/// 标题（优先当前大纲标题；无则「第 N 页」）落盘。返回要 toast 的 `(title, msg)`；
/// 宿主在模型借用结束后经自身 `pdf_host_toast` 落地（与迁移前
/// `EditorApp::pdf_add_bookmark_action` = `add_pdf_bookmark` + `pdf_toast` 逐字一致，
/// notify 由宿主适配层的模型写方法负责）。
pub fn add_bookmark_action(
    m: &mut AppModel,
    pdf: &Entity<PdfViewer>,
    cx: &mut App,
) -> Option<(String, String)> {
    let (page1, _) = pdf.read(cx).current_page()?;
    let page_index = page1.saturating_sub(1);
    let title = pdf
        .read(cx)
        .outlines()
        .iter()
        .find(|o| o.page_index == page_index)
        .map(|o| o.title.clone())
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| format!("第 {page1} 页"));
    let path = pdf.read(cx).path().to_path_buf();
    add_bookmark(m, &path, page_index, title.clone());
    Some(("已添加书签".to_string(), format!("第 {page1} 页 · {title}")))
}

/// 「删除注释」动作：删除右键命中的那条**待存**注释（只用未保存模型：从 pending 移除 +
/// 撤销栈记录可 Ctrl+Z 恢复；不落盘、不清 dirty 语义——save 后才真正消失）。纯
/// PdfViewer 实体操作，无模型/宿主依赖。
pub fn delete_annot_action(pdf: &Entity<PdfViewer>, id: u64, cx: &mut App) {
    pdf.update(cx, |v, cx| v.delete_pending_annot(id, cx));
}

/// PDF 页操作失败 toast 判定 + 模型去重更新（供宿主 `_pdf_sub` observe 变化时调用；
/// `last_modify_error` 由 None → Some 才算「变化」，同一错误不重复提示）。
/// 去重键 = PDF 文档实体（`EntityId → 文案`），每个文档独立去重：A 文档已提示的
/// 同文案错误不吞 B 文档的首次失败（单槽跨文档共享污染；实体消亡后槽位随文档数缓慢
/// 增长、entry 极小且对其他文档无害，EntityId 带代际号不复用）。
/// 返回要 toast 的失败文案（`None` = 无新变化，不加提示）。
pub fn notify_last_error(m: &mut AppModel, pdf: &Entity<PdfViewer>, cx: &mut App) -> Option<String> {
    let err = pdf.read(cx).last_modify_error().map(|s| s.to_string());
    let key = pdf.entity_id();
    // 先提纯 owned 再 match（match 分支守卫里的 borrow 临时 Ref 存活整个
    // 分支体 → 块内 borrow_mut 运行期双借 panic）。
    let seen = m.pdf_last_error_seen.get(&key).cloned();
    let changed = err.as_ref().is_some_and(|e| seen.as_deref() != Some(e.as_str()));
    match err {
        Some(e) if changed => {
            m.pdf_last_error_seen.insert(key, e.clone());
            Some(pdf_modify_error_msg(&e))
        }
        Some(_) => None,
        None => {
            // 只清该死文档的槽位（保持「无变化不重复提示」语义，不动其它文档）。
            m.pdf_last_error_seen.remove(&key);
            None
        }
    }
}

/// 「插入页面」动作：确认框（插入页为待存、消除「覆盖原文件」措辞）→
/// `insert_blank_page_after`（内存级）。成功由页数变化/重载呈现；失败经
/// `last_modify_error`（即时或 `_pdf_sub` 变化时）toast（toast 经宿主 [`PdfHost`]）。
pub fn insert_page_action<H: PdfHost>(
    window: &mut Window,
    pdf: &Entity<PdfViewer>,
    cx: &mut Context<H>,
) {
    let Some((page1, total)) = pdf.read(cx).current_page() else {
        return;
    };
    if total == 0 {
        return;
    }
    let page_index = page1.saturating_sub(1);
    let entity = cx.entity();
    let pdf2 = pdf.clone();
    let answer = window.prompt(
        PromptLevel::Warning,
        "插入页面",
        Some(&format!("将在第 {page1} 页后插入一页空白页（尚未保存，保存时方写回原文件）。继续？")),
        &["插入", "取消"],
        cx,
    );
    window
        .spawn(cx, async move |cx| {
            if !matches!(answer.await, Ok(0)) {
                return;
            }
            let _ = cx.update_window_entity(&entity, |this, _window, cx| {
                let p = pdf2.clone();
                p.update(cx, |v, cx| v.insert_blank_page_after(page_index, cx));
                if let Some(err) = pdf2.read(cx).last_modify_error() {
                    this.pdf_host_toast("插入页面失败", pdf_modify_error_msg(err));
                }
            });
        })
        .detach();
}

/// 「删除页面」动作：原生确认框（破坏性）→ `delete_page`。唯一页在菜单即禁用，
/// 此处再兜底（Phase A `delete_page` 对唯一页同样拒绝）。失败经 `last_modify_error` toast。
pub fn delete_page_action<H: PdfHost>(
    window: &mut Window,
    pdf: &Entity<PdfViewer>,
    cx: &mut Context<H>,
) {
    let Some((maybe_page1, total)) = pdf.read(cx).current_page() else {
        return;
    };
    if total == 0 {
        return;
    }
    let page1 = maybe_page1.min(total);
    let page_index = page1.saturating_sub(1);
    let entity = cx.entity();
    let pdf2 = pdf.clone();
    let answer = window.prompt(
        PromptLevel::Warning,
        "删除页面",
        Some(&format!("确定删除第 {page1} 页？（尚未保存，保存时方写回原文件）")),
        &["删除", "取消"],
        cx,
    );
    window
        .spawn(cx, async move |cx| {
            if !matches!(answer.await, Ok(0)) {
                return;
            }
            let _ = cx.update_window_entity(&entity, |this, _window, cx| {
                let p = pdf2.clone();
                p.update(cx, |v, cx| v.delete_page(page_index, cx));
                if let Some(err) = pdf2.read(cx).last_modify_error() {
                    this.pdf_host_toast("删除页面失败", pdf_modify_error_msg(err));
                }
            });
        })
        .detach();
}

/// 「提取页面」动作：另存为对话框选目标路径 → `extract_page_to`（源文档不变）。
/// 成功 toast 目标路径；失败经 `last_modify_error` toast。测试构建（`#[cfg(test)]`——
/// 本 crate 自测）不弹系统对话框——端到端文件产出由 shell 集成测试直接驱动
/// `extract_page_to` 覆盖。
pub fn extract_page_action<H: PdfHost>(
    window: &mut Window,
    pdf: &Entity<PdfViewer>,
    cx: &mut Context<H>,
) {
    let Some((page1, _)) = pdf.read(cx).current_page() else {
        return;
    };
    let page_index = page1.saturating_sub(1);
    let base_name = pdf
        .read(cx)
        .path()
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "extract".to_string());
    let entity = cx.entity();
    let pdf2 = pdf.clone();
    #[cfg(not(test))]
    {
        window
            .spawn(cx, async move |cx| {
                let Some(file) = rfd::AsyncFileDialog::new()
                    .set_title("提取页面为 PDF")
                    .set_file_name(format!("{base_name}_第{page1}页.pdf"))
                    .save_file()
                    .await
                else {
                    return;
                };
                let out = file.path().to_path_buf();
                let _ = cx.update_window_entity(&entity, |this, _window, cx| {
                    let p = pdf2.clone();
                    p.update(cx, |v, cx| v.extract_page_to(page_index, out.clone(), cx));
                    // 只有「同步预检命中」（如目标目录缺失）在这里立即提示失败；**成败主判
                    // 决移出菜单决策点**——`extract_page_to` 是异步（同步段先清错误、真实
                    // 结果后台完成才落地），同步臆断会成功 toast 恒假（失败也先弹「已提取
                    // 页面」再补失败 = 自相矛盾双 toast）。真实成败由异步完成结果驱动：
                    // 成功经 `last_extract_success` 信号 → `_pdf_sub` observe 弹成功 toast；
                    // 失败经 `last_modify_error` → 既有 `_pdf_sub` 补发失败提示。
                    if let Some(err) = pdf2.read(cx).last_modify_error() {
                        this.pdf_host_toast("提取页面失败", pdf_modify_error_msg(err));
                    }
                });
            })
            .detach();
        let _ = base_name;
    }
    #[cfg(test)]
    {
        let _ = (window, cx, pdf);
        let _ = (base_name, entity, pdf2, page_index, page1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::AppContext as _;

    /// 红测 Bug B：失败 toast 去重单槽 `AppModel.pdf_last_error_seen` 挂在宿主级，**跨 PDF
    /// 文档共享**（除 `notify_last_error` 外无任何随文档切换重置点，见 model.rs:295/386/571 +
    /// menu.rs:96/101）。文档 A 首次失败提示后，切到文档 B：B 的首次失败若与 A 的 been 文案
    /// 相同（越界「页码越界：第 N 页」/「文档尚未加载」/ 同一文件被占用等真实同文案场景），
    /// 会被误判「已见」而**吞掉 B 的首次提示**。
    /// 修复方向：去重键含文档身份（按 PdfViewer entity 分槽）或随文档切换清 seen。
    #[gpui::test]
    async fn pdf_error_dedup_slot_swallows_other_docs_first_failure(cx: &mut gpui::TestAppContext) {
        // 两个独立 PDF 文档实体。未加载即可：`extract_page_to(999, …)` 越界错误是同步路径
        //（open.rs:1233-1236，dims 空 → 999 ≥ 0），无需 pdfium/真实文档。
        let a = cx.update(|app| app.new(|cx| PdfViewer::new("A.pdf".into(), cx)));
        let b = cx.update(|app| app.new(|cx| PdfViewer::new("B.pdf".into(), cx)));
        let mut m = AppModel::default();

        // 文档 A 首次失败（越界提取）→ 应提示一次，seen = A 的错误文案。
        let toast_a = cx.update(|app| {
            a.update(app, |v, cx| v.extract_page_to(999, "out-a.pdf".into(), cx));
            notify_last_error(&mut m, &a, app)
        });
        assert!(toast_a.is_some(), "A 首次失败应提示一次");

        // 切到文档 B：B 首次失败（同文案越界错误）→ 独立文档的首次失败也**应**提示。
        // 当前被 A 的 been 槽污染 → toast 为 None = 红测失败。
        let toast_b = cx.update(|app| {
            b.update(app, |v, cx| v.extract_page_to(999, "out-b.pdf".into(), cx));
            notify_last_error(&mut m, &b, app)
        });
        assert!(
            toast_b.is_some(),
            "B 首次失败不应被 A 的 been 槽吞掉（单槽跨 PDF 共享污染去重）"
        );
    }
}
