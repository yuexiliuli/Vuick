//! 编辑 ⇄ 预览切换与统一 MD 预览/所见即所得（WYSIWYG）的**宿主薄适配**（域逻辑在
//! 独立 crate `shell_preview`）。
//!
//! 本模块只保留 `EditorApp` 的薄包装 / 宿主侧收尾：
//!
//! - **薄包装**：`tab_file_name` / `show_preview` / `md_wy_preview` / `epub_page_bounds` /
//!   `sync_editor_preview_scroll` / `ensure_md_wy` / `ensure_epub_wy` /
//!   `epub_flip_chapter` / `toggle_preview_for_tab` / `toggle_preview_fullscreen` /
//!   `toggle_wrap` / `is_*` / `language_id` / `language_label` / `md_wy_invalidate_async` /
//!   `md_wy_spawn_early_install` / `md_wy_install_full_swap` / `md_wy_spawn_prefix_install` /
//!   `md_wy_feed_full_rope` 全部转发 `shell_preview` 域函数（`app/tests/preview.rs` +
//!   `scroll_sync.rs` 等宿主↔预览联动集成测试靠此兼容面编译）。
//! - **宿主收尾**：`commit_live` 的视图收尾（`on_next_frame` 清 `retired_live2` + 行级
//!   diff 兜底 hunk 计算，`text_open::line_diff` 依赖留在宿主）+ `md_wy_align_after_*`
//!   对齐转域；`toggle_live_test` 的守卫/关闭（`commit_live`）+ ON 分支转域 `enter_live`；
//!   `change_font_size`/`reset_font_size`（字号持久化）+ 测试探针 `retired_live2_len`。
//! - **测试探针重导出**：`LIVE_COMMIT_MD_WY_ASYNC` / `LIVE_ON_MD_WY_REUSE` 在
//!   `shell_preview`（常驻编译），此处重导出维持 `crate::app::preview::…` 既有路径；
//!   `LIVE_COMMIT_DIFF_APPLIED` / `LIVE_COMMIT_DIFF_HUNKS` 仍由本模块 `commit_live` 的
//!   Diff 兜底分支维护（保留在宿主侧）。

use std::path::Path;

use super::*;

/// 测试探针重导出（定义在 `shell_preview`，域层 `md_wy_feed_full` 与 `live::enter_live`
/// 维护）：维持 `crate::app::preview::…` 既有路径（`app/tests/preview.rs` 引用）。
/// 仅在测试构建存在：lib（生产）构建无消费方，避免未使用告警。
#[cfg(test)]
pub(crate) use shell_preview::LIVE_COMMIT_MD_WY_ASYNC;
#[cfg(test)]
pub(crate) use shell_preview::LIVE_ON_MD_WY_REUSE;

/// 测试探针（`cfg(test)`）：走**增量 diff 写回**（replaces 非空 → `apply_replaces` /
/// `preview_apply_edit` 被调用）的 `commit_live` 次数。方案2 后仅「编辑日志重放护栏
/// 失败 / 有脏标记但无日志」的兜底路径累加。
/// （`LIVE_COMMIT_WRITEBACK` / `LIVE_COMMIT_SKIP_UNCHANGED` / `LIVE_COMMIT_OPS_REPLAYED`
/// / `LIVE_COMMIT_OPS_COUNT` 在 `shell_core`，随 `commit_live` 纯模型部分维护。）
#[cfg(test)]
pub(crate) static LIVE_COMMIT_DIFF_APPLIED: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
/// 测试探针（`cfg(test)`）：增量写回实际应用的替换 hunk 累计数（>0 即非整篇替换）。
/// 仅「护栏/无日志」兜底走 diff 时累加——主路径已被编辑日志重放（`shell_core::LIVE_COMMIT_OPS_*`）
/// 取代。
#[cfg(test)]
pub(crate) static LIVE_COMMIT_DIFF_HUNKS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

impl EditorApp {
    /// 选项卡默认文件名（用于保存对话框）。域逻辑 [`shell_preview::mdwy::tab_file_name`]。
    pub(super) fn tab_file_name(&self, idx: usize) -> String {
        shell_preview::mdwy::tab_file_name(&self.model.borrow(), idx)
    }

    /// 当前激活选项卡是否开启预览。
    pub(super) fn show_preview(&self) -> bool {
        shell_preview::mdwy::show_preview(&self.model.borrow())
    }

    /// A2：当前激活选项卡的右栏 WyEditor 预览实体（仅当该选项卡**真正以 WyEditor 作
    /// 预览渲染**时才返回）：Markdown / EPUB / 纯文本（都可能有 `md_wy`）返回宿主；
    /// PDF / 图片 / 视频（自带阅读器）→ `None`。`render_preview_pane` /
    /// `render_fullscreen_preview` 据此决定走统一组件宿主。
    /// 域逻辑 [`shell_preview::mdwy::md_wy_preview`]。
    /// `#[doc(hidden)] pub`：`tests/epub.rs` 预览宿主路由断言（§8.6 外提清单）。
    #[doc(hidden)]
    pub fn md_wy_preview(&self) -> Option<gpui::Entity<WyEditor>> {
        shell_preview::mdwy::md_wy_preview(&self.model.borrow())
    }

    /// EPUB 轻量视口分页读数 `(当前页, 总页数)`（1 基；状态栏页码，P4-1/P4-2）：
    /// 读 WyEditor 只读宿主 WYSIWYG 确定性量程（`MarkdownPreview` 回退后端已删）。
    /// 域逻辑 [`shell_preview::mdwy::epub_page_bounds`]。
    /// `#[doc(hidden)] pub`：`tests/pdf.rs` EPUB 状态栏页码断言（§8.6 外提清单）。
    #[doc(hidden)]
    pub fn epub_page_bounds(&self, window: &mut Window, cx: &gpui::App) -> Option<(usize, usize)> {
        shell_preview::mdwy::epub_page_bounds(&self.model, window, cx)
    }

    /// 编辑区⇄预览滚动同步（每帧渲染开头调用）：域状态机
    /// [`shell_preview::scroll::sync_editor_preview_scroll`]（基线 `md_scroll_sync` 是宿主
    /// 字段，传入域函数；语义与迁移前逐字一致）。
    pub(super) fn sync_editor_preview_scroll(&mut self, cx: &mut Context<Self>) {
        shell_preview::scroll::sync_editor_preview_scroll(&mut self.md_scroll_sync, &self.model, cx);
    }

    /// A2：确保某选项卡的右栏 WyEditor 预览已创建（懒创建 + A1 只读门控 + 全局字号/
    /// 基准目录）。`feed=true` 用当前编辑区文本**同步喂源**。域逻辑
    /// [`shell_preview::mdwy::ensure_md_wy`]。
    pub(super) fn ensure_md_wy(
        &mut self,
        idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
        feed: bool,
    ) {
        shell_preview::mdwy::ensure_md_wy(&self.model, idx, window, cx, feed);
    }

    /// EPUB：确保右栏 WyEditor **只读宿主**已创建，并用 EPUB 源**同步喂源**。域逻辑
    /// [`shell_preview::mdwy::ensure_epub_wy`]。
    pub(super) fn ensure_epub_wy(
        &mut self,
        idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
        text: &str,
        images: &std::collections::HashMap<String, std::sync::Arc<gpui::Image>>,
        notes: &std::collections::BTreeMap<usize, String>,
    ) {
        shell_preview::mdwy::ensure_epub_wy(&self.model, idx, window, cx, text, images, notes);
    }

    /// P4-1：EPUB 章节翻转「上一章/下一章」。域逻辑
    /// [`shell_preview::mdwy::epub_flip_chapter`]。
    /// `#[doc(hidden)] pub`：`tests/epub.rs` 章节翻转断言（§8.6 外提清单）。
    #[doc(hidden)]
    pub fn epub_flip_chapter(&mut self, dir: i32, cx: &mut Context<Self>) -> bool {
        shell_preview::mdwy::epub_flip_chapter(&self.model, dir, cx)
    }

    /// 切换指定选项卡的 Markdown 预览（眼睛图标 / 菜单 / 命令面板共用）。
    /// 先激活该选项卡（宿主动作，tabs 域）再翻转其 `preview_on` 并喂源/释放
    /// （域 [`shell_preview::toggle::toggle_preview_for_tab_state`]）。
    /// EPUB（`readonly_preview`）：正文只在预览区、编辑区为空，翻转预览无意义，
    /// 点击仅重新同步内容（持久保持全屏预览，不会落入空白编辑区）。
    #[doc(hidden)]
    pub fn toggle_preview_for_tab(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.activate_tab(idx, window, cx);
        shell_preview::toggle::toggle_preview_for_tab_state(&self.model, idx, window, cx);
    }

    /// 切换当前选项卡预览是否全屏占满（EPUB 阅读模式）。域逻辑
    /// [`shell_preview::toggle::toggle_preview_fullscreen`]。
    /// P4-2：预览统一 WyEditor（恒换行），全屏只影响编辑区/布局，无需再设预览换行。
    /// `#[doc(hidden)] pub`：`tests/pdf.rs` 全屏/换行切换断言（§8.6 外提清单）。
    #[doc(hidden)]
    pub fn toggle_preview_fullscreen(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        shell_preview::toggle::toggle_preview_fullscreen(&self.model, cx);
    }

    /// 切换当前选项卡编辑区的自动换行/不换行，并更新全局编辑器偏好。域逻辑
    /// [`shell_preview::toggle::toggle_wrap`]。
    /// `#[doc(hidden)] pub`：`tests/pdf.rs` 换行切换断言（§8.6 外提清单）。
    #[doc(hidden)]
    pub fn toggle_wrap(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        shell_preview::toggle::toggle_wrap(&self.model, cx);
    }

    /// 是否 Markdown 文件（编辑区/预览默认换行的依据）。
    pub(super) fn is_markdown_path(p: &Path) -> bool {
        shell_preview::classify::is_markdown_path(p)
    }

    /// 选项卡是否 Markdown 文件（编辑区/预览默认换行的依据）。
    pub(super) fn is_markdown_tab(&self, idx: usize) -> bool {
        shell_preview::classify::is_markdown_tab(&self.model.borrow(), idx)
    }

    /// 是否 EPUB 文件（按扩展名识别；EPUB 预览走 WyEditor 只读宿主的依据）。
    pub(super) fn is_epub_path(p: &Path) -> bool {
        shell_preview::classify::is_epub_path(p)
    }

    /// 是否 PDF 文件（按扩展名识别）。
    pub(super) fn is_pdf_path(p: &Path) -> bool {
        shell_preview::classify::is_pdf_path(p)
    }

    /// 是否图片文件（按扩展名识别；清单见 [`image_viewer::IMAGE_EXTENSIONS`]）。
    pub(super) fn is_image_path(p: &Path) -> bool {
        shell_preview::classify::is_image_path(p)
    }

    /// 是否视频文件（按扩展名识别；清单见 [`video::VIDEO_EXTENSIONS`]）。
    #[cfg(feature = "video")]
    pub(super) fn is_video_path(p: &Path) -> bool {
        shell_preview::classify::is_video_path(p)
    }

    /// 按扩展名推断 gpui-component 语法高亮的语言名（未知扩展名回退 "text"）。
    pub(super) fn language_id(p: &Path) -> &'static str {
        shell_preview::classify::language_id(p)
    }

    /// 按扩展名推断语言标签（仅用于状态栏展示；单一真值见 [`shell_preview::classify`]）。
    pub(super) fn language_label(&self) -> &'static str {
        let model = self.model.borrow();
        let Some(p) = model.tabs
            .get(model.active_tab)
            .and_then(|t| t.payload.file_path.as_ref())
        else {
            return "纯文本";
        };
        shell_preview::classify::classify(p).language_label()
    }

    /// 调节编辑器字号并刷新预览（范围约束见 FONT_MIN/FONT_MAX）。
    /// 宿主侧保留：字号持久化（`persist_prefs`）是宿主 session/prefs 域，域不重复实现。
    pub(super) fn change_font_size(&mut self, delta: f32, cx: &mut Context<Self>) {
        let font_size = (self.model.borrow().font_size + delta).clamp(FONT_MIN, FONT_MAX);
        self.model.borrow_mut().font_size = font_size;
        // 字号用户自调后持久化（文件极小 + 原子写，每步一次可接受）。
        self.persist_prefs();
        // 实时预览编辑（WYSIWYG）字号：EditorState 继承外层 text_size（宿主容器渲染时
        // 读取 model.font_size），无需像旧的 MarkdownLivePreview 那样内部 set_font_override。
        cx.notify();
    }

    /// 重置正文字号到默认值并持久化。
    pub(super) fn reset_font_size(&mut self, cx: &mut Context<Self>) {
        self.model.borrow_mut().font_size = DEFAULT_FONT_SIZE;
        self.persist_prefs();
        cx.notify();
    }

    /// （仅测试）当前 `retired_live2` 延迟回收队列长度——A2 探针：切回瞬间应暂存 1 个
    /// 旧 live2 实体，下一帧 `on_next_frame` 清空后回 0（无泄漏到下一次 open）。
    #[cfg(test)]
    pub(crate) fn retired_live2_len(&self) -> usize {
        self.model.borrow().retired_live2.len()
    }

    /// 退出预览编辑：把 WYSIWYG 内容写回源码编辑区 + 内容镜像 + 预览；live2 实体**延迟**
    /// 到下一帧渲染后销毁（A：实体级联 Drop ~36ms 移出点击同步路径）。
    ///
    /// `commit_live` 的**纯模型 + 实体**部分在 `AppModel::commit_live`
    /// （shell_core）。本方法保留必须留在宿主侧的视图收尾并按返回值分派：`on_next_frame`
    /// 清 `retired_live2`、右栏 `md_wy` ops/diff 对齐（转域 `shell_preview::mdwy`）、以及
    /// 行级 diff 兜底的 hunk 计算（`line_diff` 依赖 `text_open`，留在 shell，shell_core
    /// 不新增依赖）。
    pub(super) fn commit_live(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        let align = self.model.borrow_mut().commit_live(idx, cx);
        // 仅在确实取到 live2（已暂存 `retired_live2`）的路径调度下一帧清空；`NoLive2` 无
        // 暂存、不调度（与旧实现「取不到 live2 提前 return 不注册 on_next_frame」一致）。
        let mut schedule_retire_clear = false;
        match align {
            CommitLiveAlign::NoLive2 => {}
            CommitLiveAlign::NoAlign => schedule_retire_clear = true,
            // A2/A3 + B3：右栏只读预览 `md_wy`（若存在）随写回对齐。小跨度维持增量
            // `preview_apply_edit`（1-3ms，不回归）；大跨度/失步改整篇喂源——大内容
            // **后台异步装配**，同步路径不再 O(n) `Doc::open`。域逻辑
            // [`shell_preview::mdwy::md_wy_align_after_ops`]。
            CommitLiveAlign::Ops {
                ops,
                src_len,
                content,
            } => {
                schedule_retire_clear = true;
                shell_preview::mdwy::md_wy_align_after_ops(&self.model, idx, &ops, src_len, content, cx);
            }
            // 既有行级 diff 写回兜底（护栏失败 / 有脏标记但无日志时）：模型侧字段
            // （`tab.content` / `line_count` / `char_count`）已由 `AppModel::commit_live`
            // 更新；此处补「hunk 计算 + 源码编辑区应用 + 右栏 md_wy 对齐」——与旧实现
            // 同一 diff（同源基线，live 期间源码编辑区不被改写）逐 hunk 对齐。
            CommitLiveAlign::Diff { base, content } => {
                schedule_retire_clear = true;
                let replaces = text_open::line_diff(base.as_str(), content.as_ref());
                if !replaces.is_empty() {
                    self.model.borrow().tabs[idx]
                        .payload
                        .editor
                        .update(cx, |ed, _| ed.apply_replaces(&replaces));
                }
                #[cfg(test)]
                if !replaces.is_empty() {
                    LIVE_COMMIT_DIFF_APPLIED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                #[cfg(test)]
                LIVE_COMMIT_DIFF_HUNKS.fetch_add(replaces.len(), std::sync::atomic::Ordering::Relaxed);
                // A2/A3 + B3：右栏 WyEditor 预览（若存在）随写回对齐。优先与源码编辑区同
                // diff 逐 hunk 增量重放（后→前；`preview_apply_edit` 绕过只读门控、不写撤销
                // 栈、增量块重排，典型小改动几十 ms）。**大跨度 diff（被替换基线字节总量
                // 超阈值）改整篇喂源**：大内容整篇喂源改后台异步装配——同步路径不再 O(n)。
                // md_wy 内容 ≠ 源快照（含空壳未就绪）也整篇兜底。域逻辑
                // [`shell_preview::mdwy::md_wy_align_after_diff`]。
                shell_preview::mdwy::md_wy_align_after_diff(&self.model, idx, &replaces, base.as_str(), content, cx);
            }
        }
        if schedule_retire_clear {
            cx.on_next_frame(window, |this, _window, _cx| {
                this.model.borrow_mut().retired_live2.clear();
            });
        }
    }

    /// B3：作废右栏 `md_wy` 的在途后台整篇装配（装配代号 +1、清 pending）。域逻辑
    /// [`shell_preview::mdwy::md_wy_invalidate_async`]。
    pub(super) fn md_wy_invalidate_async(&mut self, idx: usize) {
        shell_preview::mdwy::md_wy_invalidate_async(&self.model, idx);
    }

    /// Step2/3：大文档 md「打开即提前整篇解析」——返回本次装配代号（供 prefix 首装共用）。
    /// 域逻辑 [`shell_preview::mdwy::md_wy_spawn_early_install`]（`file_ops` 调用）。
    pub(super) fn md_wy_spawn_early_install(
        &mut self,
        idx: usize,
        p: std::path::PathBuf,
        cx: &mut Context<Self>,
    ) -> usize {
        shell_preview::mdwy::md_wy_spawn_early_install(&self.model, idx, p, cx)
    }

    /// Step3：整篇就绪后**带守卫换入全文**的落地函数（域逻辑
    /// [`shell_preview::mdwy::md_wy_install_full_swap`]）。
    /// 生产路径由 shell_preview 域内完成回调直连域函数；本包装仅供
    /// `app/tests/preview.rs` 直接驱动（兼容面保留）→ 仅测试构建存在。
    #[cfg(test)]
    pub(super) fn md_wy_install_full_swap(
        &mut self,
        idx: usize,
        wy: &gpui::Entity<WyEditor>,
        result: markdown_wysiwyg::WyOpenResult,
        cx: &mut Context<Self>,
    ) {
        shell_preview::mdwy::md_wy_install_full_swap(&self.model, idx, wy, result, cx);
    }

    /// Step3：大文档 md「首块 prefix 首装」（`app/tests/preview.rs` 直接调用，兼容面保留）。
    /// 域逻辑 [`shell_preview::mdwy::md_wy_spawn_prefix_install`]。
    pub(super) fn md_wy_spawn_prefix_install(
        &mut self,
        idx: usize,
        prefix: Rope,
        full_seq: usize,
        cx: &mut Context<Self>,
    ) {
        shell_preview::mdwy::md_wy_spawn_prefix_install(&self.model, idx, prefix, full_seq, cx);
    }

    /// Step3：整篇重喂的 **Rope 版**（`md_wy_feed_full` 同语义）——shell `sync::on_editor_edited`
    /// 的增量桥错位自愈入口。域逻辑 [`shell_preview::mdwy::md_wy_feed_full_rope`]。
    pub(super) fn md_wy_feed_full_rope(&mut self, idx: usize, rope: Rope, cx: &mut Context<Self>) {
        shell_preview::mdwy::md_wy_feed_full_rope(&self.model, idx, rope, cx);
    }

    /// 「预览编辑测试」开关：用本项目 `markdown_wysiwyg::WyEditor` 渲染左面板。
    /// 前置守卫（仅支持 Markdown）+ 已开则 `commit_live` 关闭（宿主收尾）留在本 wrapper；
    /// ON 分支（创建 live2 活区）转域 [`shell_preview::live::enter_live`]。
    pub(super) fn toggle_live_test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let idx = self.model.borrow().active_tab;
        if !self.is_markdown_tab(idx) {
            self.model.borrow_mut().status = SharedString::from("预览编辑测试仅支持 Markdown 文件");
            self.model.borrow_mut().status_ok = false;
            cx.notify();
            return;
        }
        if self.model.borrow().tabs[idx].live2.is_some() {
            self.commit_live(idx, window, cx);
        } else {
            shell_preview::live::enter_live(&self.model, idx, window, cx);
        }
        cx.notify();
    }
}
