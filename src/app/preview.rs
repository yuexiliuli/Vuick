//! 编辑 ⇄ 预览切换与统一 MD 预览/所见即所得（WYSIWYG）：预览开关与全屏、
//! 统一 WyEditor 预览组件（`md_wy`）的创建/喂源/滚动同步、WYSIWYG 活区
//! （`live2`）的进入/写回，以及路径 → 类型/语言推断等路由辅助。

use std::path::Path;

use super::*;

impl EditorApp {
    /// 选项卡默认文件名（用于保存对话框）。
    pub(super) fn tab_file_name(&self, idx: usize) -> String {
        self.tabs
            .get(idx)
            .and_then(|t| t.file_path.as_ref())
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "未命名.txt".into())
    }

    /// 当前激活选项卡是否开启预览。
    pub(super) fn show_preview(&self) -> bool {
        self.tabs
            .get(self.active_tab)
            .map(|t| t.preview_on)
            .unwrap_or(false)
    }

    /// 把指定选项卡的当前内容同步到 Markdown 预览（开启预览时刷新一次）。
    /// 源用 `tab.content`（编辑区内容的镜像，EPUB 打开时正文只存在于 content/预览，
    /// 编辑区为空，不能用 editor.value() 否则会拿空串覆盖预览导致内容消失）。
    pub(super) fn sync_md_preview(&mut self, idx: usize, cx: &mut Context<Self>) {
        let panel_bg = self.panel_bg();
        // 预览区始终自动换行（Markdown/EPUB，不随编辑区换行偏好）
        let Some(tab) = self.tabs.get_mut(idx) else {
            return;
        };
        let value = if !tab.content.is_empty() {
            tab.content.clone().to_string()
        } else {
            tab.editor.read(cx).value().to_string()
        };
        tab.md_preview.update(cx, |view, cx| {
            view.set_background(panel_bg);
            view.set_base_dir(tab.file_path.as_ref().and_then(|p| p.parent()).map(|d| d.to_path_buf()));
            view.set_wrap(true, cx);
            view.set_source(&value, cx);
        });
    }

    /// A2：当前激活选项卡的右栏 WyEditor 预览实体（仅当该选项卡**真正以 WyEditor 作
    /// 预览渲染**时才返回：Markdown 文件 → 已建 → 非 PDF；EPUB（P4-1 迁 WyEditor 只读
    /// 宿主，且回退开关未关）→ 已建）。`render_preview_pane` / `render_fullscreen_preview`
    /// 据此决定走统一组件宿主。
    pub(super) fn md_wy_preview(&self) -> Option<gpui::Entity<WyEditor>> {
        let tab = self.tabs.get(self.active_tab)?;
        let wy = tab.md_wy.clone()?;
        // PDF 用自带 PdfViewer，绝不以 WyEditor 渲染。
        if tab.pdf_preview.is_some() {
            return None;
        }
        if self.is_epub_tab(self.active_tab) {
            // EPUB 只读宿主：回退开关关 → 仍走 MarkdownPreview。
            if !self.epub_preview_backend_wy() {
                return None;
            }
            return Some(wy);
        }
        // Markdown 统一预览：仅 Markdown（非只读）走 WyEditor。
        if !self.is_markdown_tab(self.active_tab) || tab.readonly_preview {
            return None;
        }
        Some(wy)
    }

    /// EPUB 轻量视口分页读数 `(当前页, 总页数)`（1 基；状态栏页码，P4-1）：
    /// 新宿主（WyEditor 只读）读 WYSIWYG 确定性量程；回退后端读 `MarkdownPreview`
    /// （原有实现）。非 EPUB / 就绪前 → `None`（状态栏不显示页码）。
    pub(super) fn epub_page_bounds(&self, window: &mut Window, cx: &gpui::App) -> Option<(usize, usize)> {
        let idx = self.active_tab;
        if !self.is_epub_tab(idx) {
            return None;
        }
        let tab = self.tabs.get(idx)?;
        if self.epub_preview_backend_wy() && tab.md_wy.is_some() {
            // 视口高：以窗口内容高估算（容器 ≈ 窗口正文区；误差 ≤ 顶部/底部工具条高，
            // 对「第 x/N 页」展示无碍）。
            let view_h = f32::from(window.viewport_size().height).max(1.);
            return tab.md_wy.as_ref().and_then(|w| w.read(cx).viewport_page_bounds(view_h));
        }
        tab.md_preview.read(cx).viewport_page_bounds()
    }

    /// 编辑区⇄预览滚动同步（每帧渲染开头调用）。只在「活动选项卡以 WyEditor 作 MD
    /// 预览且在**非 live2 实时编辑**的分屏（非全屏 / 非只读 + 预览开启）」时生效——
    /// live2 下右栏预览不在元素树，驱动隐藏实体只会残留陈旧目标。
    ///
    /// 同步单位 = 源码行所在**块起点**（两侧都经 [`WyEditor::block_start_of_line`] /
    /// [`WyEditor::top_source_line`] 归一成块）：同一块（段落/代码块/标题）内微滚动
    /// 双方键不变、不互拉，避免反馈抖动；跨块才驱动对端。
    /// 逻辑：编辑器顶行块变了 → 预览 `scroll_to_line` 到该块；预览顶块变了（且编辑器
    /// 未同时动）→ 编辑器滚动到该块起点。双侧收敛到同一块后静止。
    ///
    /// 注意：两侧键读取的是**上一帧**的滚动/视口状态（`top_source_line` 读 `panes`
    /// 上一帧渲染期推入的 `viewport_top_px`）——本帧推入发生在 sync 之后，故同步天然
    /// 滞后一帧；对逐帧收敛无影响，反而更稳。
    pub(super) fn sync_editor_preview_scroll(&mut self, cx: &mut Context<Self>) {
        let idx = self.active_tab;
        if self.md_wy_preview().is_none() {
            return;
        }
        let tab = &self.tabs[idx];
        if tab.readonly_preview || tab.preview_fullscreen || !self.show_preview() || tab.live_edit {
            return;
        }
        let Some(md) = tab.md_wy.clone() else { return };
        let editor = tab.editor.clone();

        // 读两侧当前块起点键（预览帧未就绪 → None，本帧不动）。
        let preview_block = md.read(cx).top_source_line();
        let editor_block = Some(md.read(cx).block_start_of_line(editor.read(cx).viewport_top_line()));

        // 首次进入该选项卡 / 换文档（块号失义）/ 编辑器内容已编辑（块映射变）：
        // 重建基线，只记录不驱动。
        let ev = Some(editor.read(cx).version());
        if self.md_scroll_sync.tab != idx || self.md_scroll_sync.editor_version != ev {
        self.md_scroll_sync = MdScrollSync {
            tab: idx,
            editor_version: ev,
            editor_block,
            preview_block,
            driven_preview_target: None,
            driven_editor_target: None,
            driven_editor_await: 0,
        };
        return;
    }
    let prev = self.md_scroll_sync;
    self.md_scroll_sync.editor_block = editor_block;
    self.md_scroll_sync.preview_block = preview_block;
    let Some(eb) = editor_block else { return };
    if preview_block == Some(eb) {
        // 两侧同一块，已同步：作废「等待预览到达」与「等待编辑器到达」的驱动目标（已对齐）。
        self.md_scroll_sync.driven_preview_target = None;
        self.md_scroll_sync.driven_editor_target = None;
        self.md_scroll_sync.driven_editor_await = 0;
        return;
    }
    // 我方正在驱动编辑器到达 `driven_editor_target`：期间 editor_block 的连续变化是
    // 跟随落位（边界块顶行读取还会滞后/抖动一两帧），**不是**用户主动滚动 → 不得触发
    // 下方「编辑器先动 → 驱动预览」回声，否则预览会被编辑器落位拽回（来回跳动）。
    // 宽限期内只计数，超限才放手交给正常双向同步（防永远到不了目标时停摆）。
    if prev.editor_block != editor_block {
        if self.md_scroll_sync.driven_editor_target.is_some() {
            // 我方驱动编辑器落位中：抑制回声，只给宽限计数；超限才放手交给正常双向同步。
            // 注意：放手时**不得**顺便重钉预览（会把预览拉回，重蹈回声之弊）——清标后由
            // 后续帧的正常判定接管（真用户滚编辑器 → 下一帧 editor_block 再变 → 正常驱动
            // 预览；边界块卡住 → 编辑器不再变 → 安静保持，不再互拉）。
            if self.md_scroll_sync.driven_editor_await >= DRIVEN_EDITOR_MAX_AWAIT {
                self.md_scroll_sync.driven_editor_target = None;
                self.md_scroll_sync.driven_editor_await = 0;
            } else {
                self.md_scroll_sync.driven_editor_await += 1;
            }
            return;
        }
        // 编辑器先动（无我方驱动在途 = 用户滚动）→ 预览跟到编辑器顶行所在块。
        // **不能在本帧 render 直接调** `scroll_to_line`（宿主 `ScrollHandle::set_offset`
        // 在渲染期会被本帧布局复位）：只把目标行写进 WY 的 `scroll_target`，由其元素
        // prepaint 逐帧重钉宿主偏移直到稳住（有 [`WyEditor::SCROLL_TARGET_MAX_APPLIES`]
        // 兜底）。记录「等待预览到达」目标：预览重钉落位到它时不被误判为预览用户滚动。
        md.update(cx, |v, _| v.set_scroll_target(eb));
        self.md_scroll_sync.driven_preview_target = Some(eb);
    } else if let Some(pb) = preview_block
        && prev.preview_block != preview_block
        // 预览落位到「我方正在驱动的预览目标」= 编辑器主导下的跟随，不反向拉动。
        && preview_block != self.md_scroll_sync.driven_preview_target
    {
        // 预览先动 → 编辑器跟到预览顶块起点。
        editor.update(cx, |ed, _| ed.scroll_to_line(pb));
        // 预置编辑器基线为驱动目标：编辑器下一帧实际到位即与基线一致，不再被误判
        // 成「编辑器用户主动滚动」而把预览拽回（来回跳动）；预览现为主导，旧等待
        // 目标作废。
        self.md_scroll_sync.driven_editor_target = Some(pb);
        self.md_scroll_sync.driven_editor_await = 0;
        self.md_scroll_sync.editor_block = Some(pb);
        self.md_scroll_sync.driven_preview_target = None;
    } else if self.md_scroll_sync.driven_preview_target.is_none()
        // 预览已稳定（上帧到本帧未变）：
        && prev.preview_block == preview_block
    {
        if self.md_scroll_sync.driven_editor_target.is_some() {
            // 编辑器落位标记悬挂在「两侧静态未对齐」态（如大跳过冲）：推进宽限计数，
            // 超限清标（不重钉预览）后在下一帧走收敛。
            if self.md_scroll_sync.driven_editor_await >= DRIVEN_EDITOR_MAX_AWAIT {
                self.md_scroll_sync.driven_editor_target = None;
                self.md_scroll_sync.driven_editor_await = 0;
            } else {
                self.md_scroll_sync.driven_editor_await += 1;
            }
        } else if let Some(pb) = preview_block
            && Some(pb) != editor_block
        {
            // 以（用户当前所在的）静态预览为准回拉编辑器：幂等收敛「大跳后编辑器过冲
            // 未对齐」，不再依赖原「编辑器→预览」回声把预览拽回（那正是来回跳动/滚不
            // 下去的根因）。
            editor.update(cx, |ed, _| ed.scroll_to_line(pb));
            self.md_scroll_sync.driven_editor_target = Some(pb);
            self.md_scroll_sync.driven_editor_await = 0;
            self.md_scroll_sync.editor_block = Some(pb);
        }
    }
    }

    /// A4：当前激活选项卡的 Markdown TOC 数据源 + 跳转目标（侧边栏目录共用）：
    /// MD 统一预览（`md_wy` 在场，非只读）→ WyEditor（条目 = 源码行，点击滚动统一组件
    /// 宿主）；EPUB / 手动预览（`md_wy` 缺席）→ `MarkdownPreview`（渲染行列，`scroll_to_row`）。
    pub(super) fn md_toc_source(
        &self,
        cx: &gpui::App,
    ) -> Option<(crate::markdown::wysiwyg::TocEntries, MdTocScroll)> {
        let tab = self.tabs.get(self.active_tab)?;
        if let Some(wy) = &tab.md_wy {
            return Some((wy.read(cx).toc_entries(), MdTocScroll::Unified(wy.clone())));
        }
        Some((
            tab.md_preview.read(cx).toc_entries(),
            MdTocScroll::Legacy(tab.md_preview.clone()),
        ))
    }

    /// A2：确保某选项卡的右栏 WyEditor 预览已创建（懒创建 + A1 只读门控 + 全局字号/
    /// 基准目录）。`feed=true` 用当前编辑区文本**同步喂源**（小文档 / 切预览；
    /// 大文档异步走 `open_from_rope`(后台) + `finish_file_text` 的 `install_open`，
    /// 不在此做 O(n) UI 解析）。
    pub(super) fn ensure_md_wy(
        &mut self,
        idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
        feed: bool,
    ) {
        if idx >= self.tabs.len() {
            return;
        }
        let fs = self.font_size;
        let base_dir = self.tabs[idx]
            .file_path
            .as_ref()
            .and_then(|p| p.parent())
            .map(|d| d.to_path_buf());
        if self.tabs[idx].md_wy.is_none() {
            let ed = cx.new(|cx| WyEditor::new(window, cx));
            ed.update(cx, |ed, _| {
                ed.set_editable(false); // A1 只读门控：右栏预览不可编辑
                ed.set_font_size(gpui::px(fs));
                ed.set_base_dir(base_dir.clone());
            });
            self.tabs[idx].md_wy = Some(ed.clone());
        }
        if feed {
            // 同步喂源：源 = 编辑区规范文本（与 `sync_md_preview` 同口径）。
            let text = {
                let tab = &mut self.tabs[idx];
                tab.editor.read(cx).value().to_string()
            };
            if let Some(ed) = self.tabs[idx].md_wy.clone() {
                ed.update(cx, |ed, _| {
                    ed.set_font_size(gpui::px(fs));
                    ed.set_base_dir(base_dir);
                    ed.set_source_text(&text);
                });
            }
        }
    }

    /// EPUB：确保右栏 WyEditor **只读宿主**已创建（懒创建 + A1 只读门控 + 全局字号/
    /// 基准目录/内存图片表），并用 EPUB 源**同步喂源**（P4-1：EPUB 迁 WyEditor）。
    /// 只读（`set_editable(false)`）、标题居中（`set_epub(true)`）、`epubimg://…` 内存图
    /// （`set_memory_images`）。注记/引文/页码等 A 侧能力仍由 `md_preview` 承载（P4-2）。
    pub(super) fn ensure_epub_wy(
        &mut self,
        idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
        text: &str,
        images: &std::collections::HashMap<String, std::sync::Arc<gpui::Image>>,
    ) {
        if idx >= self.tabs.len() {
            return;
        }
        let fs = self.font_size;
        let base_dir = self.tabs[idx]
            .file_path
            .as_ref()
            .and_then(|p| p.parent())
            .map(|d| d.to_path_buf());
        if self.tabs[idx].md_wy.is_none() {
            let ed = cx.new(|cx| WyEditor::new(window, cx));
            ed.update(cx, |ed, _| {
                ed.set_editable(false); // A1 只读门控：EPUB 阅读不可编辑
                ed.set_font_size(gpui::px(fs));
                ed.set_base_dir(base_dir.clone());
            });
            self.tabs[idx].md_wy = Some(ed.clone());
        }
        if let Some(ed) = self.tabs[idx].md_wy.clone() {
            ed.update(cx, |ed, _| {
                ed.set_font_size(gpui::px(fs));
                ed.set_base_dir(base_dir);
                ed.set_memory_images(images.clone());
                ed.set_epub(true);
                ed.set_source_text(text);
            });
        }
    }

    /// P4-1：EPUB 章节翻转「上一章/下一章」。章界 = 目录标题条目（每章以标题起始）。
    /// 统一 WyEditor 宿主：当前章 = 顶行所在标题，滚动到相邻章标题（`set_scroll_target`，
    /// prepaint 重钉，无需 window）；回退 MarkdownPreview：滚动到目录渲染行（`scroll_to_row`）。
    /// 无 EPUB / 少于 2 章 / 已处边界章 → 不动，返回 `false`。
    pub(super) fn epub_flip_chapter(&mut self, dir: i32, cx: &mut Context<Self>) -> bool {
        let idx = self.active_tab;
        if !self.is_epub_tab(idx) {
            return false;
        }
        let Some((entries, scroll)) = self.md_toc_source(cx) else {
            return false;
        };
        if entries.len() < 2 {
            return false;
        }
        match scroll {
            MdTocScroll::Unified(wy) => {
                let current = wy.read(cx).top_source_line().unwrap_or(0);
                let cur_ix = entries
                    .iter()
                    .rposition(|&(l, _, _)| l <= current)
                    .unwrap_or(0);
                let next = (cur_ix as i32 + dir).clamp(0, entries.len() as i32 - 1) as usize;
                if next == cur_ix {
                    return false;
                }
                wy.update(cx, |v, _| v.set_scroll_target(entries[next].0));
                cx.notify();
                true
            }
            MdTocScroll::Legacy(md) => {
                let cur_ix = 0usize; // Legacy 无顶行读数：从首章起翻
                let next = (cur_ix as i32 + dir).clamp(0, entries.len() as i32 - 1) as usize;
                if next == cur_ix {
                    return false;
                }
                md.update(cx, |v, cx| {
                    v.scroll_to_row(entries[next].0);
                    cx.notify();
                });
                true
            }
        }
    }

    /// 切换指定选项卡的 Markdown 预览（眼睛图标 / 菜单 / 命令面板共用）。
    /// 会先激活该选项卡，再翻转其 `preview_on`。
    /// EPUB（`readonly_preview`）：正文只在预览区、编辑区为空，翻转预览无意义，
    /// 点击仅重新同步内容（持久保持全屏预览，不会落入空白编辑区）。
    pub(super) fn toggle_preview_for_tab(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.activate_tab(idx, window, cx);
        // 零选项卡（欢迎页）下「预览」无对象：菜单/命令面板都可达这里，先兜底返回。
        if self.tabs.get(idx).is_none() {
            return;
        }
        let readonly = self.tabs.get(idx).map(|t| t.readonly_preview).unwrap_or(false);
        let preview_on = {
            let tab = &mut self.tabs[idx];
            if readonly {
                tab.preview_on = true;
                tab.preview_fullscreen = true;
            } else {
                tab.preview_on = !tab.preview_on;
                // 关闭预览时同步退出全屏
                if !tab.preview_on {
                    tab.preview_fullscreen = false;
                    // 增量基底随预览关闭即失效（下次开启重新全量建立）
                    tab.preview_prev = None;
                }
            }
            tab.preview_on
        };
        // 开启预览 → 编辑区强制换行，方便在分屏里编辑 Markdown；
        // 关闭预览 → 恢复默认：Markdown 文件编辑区仍换行（md 默认），其余按全局偏好。
        let is_md = self.is_markdown_tab(idx);
        let wrap = preview_on || is_md || self.wrap_enabled;
        self.tabs[idx].editor.update(cx, |ed, _cx| {
            ed.set_soft_wrap(wrap);
        });
        if preview_on {
            // A4 脱离：MD（非只读）只走统一组件 WyEditor（ensure_md_wy 同步喂源）；
            // EPUB（P4-1）走 WyEditor 只读宿主（回退开关开时），MarkdownPreview 仍喂
            // （注记/引文/页码回退源，P4-2 收口）；手动预览仍用 MarkdownPreview。
            if is_md && !readonly {
                self.ensure_md_wy(idx, window, cx, true);
            } else if readonly && self.is_epub_tab(idx) && self.epub_preview_backend_wy() {
                let images = self.tabs[idx].epub_images.clone();
                self.sync_md_preview(idx, cx);
                let content = self.tabs[idx].content.clone();
                self.ensure_epub_wy(idx, window, cx, content.as_ref(), &images);
            } else {
                self.sync_md_preview(idx, cx);
            }
        } else {
            // 关闭预览：释放整篇解析出的行数据与测量缓存，内存回到基线
            // （否则隐藏的预览仍占着整篇文档，反复开关只涨不降）。
            self.tabs[idx].md_preview.update(cx, |view, cx| view.release(cx));
            // A2：右栏 WyEditor 预览随预览开关释放（同款内存回收）。
            self.tabs[idx].md_wy = None;
            self.tabs[idx].md_wy_vscroll = gpui::ScrollHandle::new();
        }
        cx.notify();
    }

    /// 切换当前选项卡预览是否全屏占满（EPUB 阅读模式）。
    pub(super) fn toggle_preview_fullscreen(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let idx = self.active_tab;
        let (fullscreen, preview, editor) = {
            let Some(tab) = self.tabs.get_mut(idx) else {
                return;
            };
            tab.preview_fullscreen = !tab.preview_fullscreen;
            let fs = tab.preview_fullscreen;
            (fs, tab.md_preview.clone(), tab.editor.clone())
        };
        // 进入全屏阅读模式 → 预览强制换行、编辑区强制换行；
        // 退出全屏 → 预览始终换行（不随偏好）；编辑区恢复默认（md 换行 / 其余按偏好）。
        preview.update(cx, |view, cx| view.set_wrap(true, cx));
        let is_md = self.is_markdown_tab(idx);
        let editor_wrap = if fullscreen {
            true
        } else {
            is_md || self.wrap_enabled
        };
        editor.update(cx, |ed, _cx| {
            ed.set_soft_wrap(editor_wrap);
        });
        cx.notify();
    }

    /// 切换当前选项卡编辑区的自动换行/不换行，并更新全局编辑器偏好。
    /// 只作用于编辑区；Markdown/EPUB 预览始终自动换行，不随此开关变化。
    pub(super) fn toggle_wrap(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let idx = self.active_tab;
        let wrap_enabled = !self.wrap_enabled;
        self.wrap_enabled = wrap_enabled;
        // 全屏阅读模式强制换行，跳过（由全屏切换控制）；否则按编辑器偏好
        if self
            .tabs
            .get(idx)
            .map(|t| !t.preview_fullscreen)
            .unwrap_or(true)
            && let Some(tab) = self.tabs.get(idx) {
                tab.editor.update(cx, |ed, _cx| {
                    ed.set_soft_wrap(wrap_enabled);
                });
            }
        cx.notify();
    }

    pub(super) fn is_markdown_path(p: &Path) -> bool {
        p.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("md") || e.eq_ignore_ascii_case("markdown"))
            .unwrap_or(false)
    }

    /// 选项卡是否 Markdown 文件（编辑区/预览默认换行的依据）。
    pub(super) fn is_markdown_tab(&self, idx: usize) -> bool {
        self.tabs
            .get(idx)
            .and_then(|t| t.file_path.as_ref())
            .map(|p| Self::is_markdown_path(p))
            .unwrap_or(false)
    }

    /// 选项卡是否 EPUB 文件（按扩展名识别；EPUB 预览走 WyEditor 只读宿主的依据）。
    pub(super) fn is_epub_tab(&self, idx: usize) -> bool {
        self.tabs
            .get(idx)
            .and_then(|t| t.file_path.as_ref())
            .map(|p| Self::is_epub_path(p))
            .unwrap_or(false)
    }

    /// 是否 EPUB 文件（按扩展名识别）。
    pub(super) fn is_epub_path(p: &Path) -> bool {
        p.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("epub"))
            .unwrap_or(false)
    }

    /// 是否 PDF 文件（按扩展名识别）。
    pub(super) fn is_pdf_path(p: &Path) -> bool {
        p.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("pdf"))
            .unwrap_or(false)
    }

    /// 是否图片文件（按扩展名识别；清单见 [`crate::image::IMAGE_EXTENSIONS`]）。
    pub(super) fn is_image_path(p: &Path) -> bool {
        crate::image::is_image_extension(p)
    }

    /// 是否视频文件（按扩展名识别；清单见 [`crate::video::VIDEO_EXTENSIONS`]）。
    #[cfg(feature = "video")]
    pub(super) fn is_video_path(p: &Path) -> bool {
        crate::video::is_video_extension(p)
    }

    /// 按扩展名推断 gpui-component 语法高亮的语言名（未知扩展名回退 "text"）。
    pub(super) fn language_id(p: &Path) -> &'static str {
        let ext = p
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .unwrap_or_default();
        match ext.as_str() {
            "md" | "markdown" => "markdown",
            "rs" => "rust",
            "toml" => "toml",
            "json" => "json",
            "py" => "python",
            "js" | "jsx" => "javascript",
            "ts" | "tsx" => "typescript",
            "html" | "htm" => "html",
            "css" | "scss" => "css",
            "c" | "h" => "c",
            "cpp" | "cc" | "hpp" => "cpp",
            "go" => "go",
            "java" => "java",
            "sh" | "bash" => "bash",
            "yaml" | "yml" => "yaml",
            "sql" => "sql",
            _ => "text",
        }
    }

    /// 当前文件显示名（无文件时为"未命名"）。
    pub(super) fn file_display_name(&self) -> String {
        self.tabs
            .get(self.active_tab)
            .and_then(|t| t.file_path.as_ref())
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "未命名".to_string())
    }

    /// 按扩展名推断语言标签（仅用于状态栏展示）。
    pub(super) fn language_label(&self) -> &'static str {
        let Some(p) = self
            .tabs
            .get(self.active_tab)
            .and_then(|t| t.file_path.as_ref())
        else {
            return "纯文本";
        };
        let ext = p
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .unwrap_or_default();
        match ext.as_str() {
            "md" | "markdown" => "Markdown",
            "epub" => "EPUB",
            "pdf" => "PDF",
            "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "ico" | "tif" | "tiff" => "图片",
            "rs" => "Rust",
            "toml" => "TOML",
            "json" => "JSON",
            "py" => "Python",
            "js" | "ts" | "jsx" | "tsx" => "JavaScript",
            "html" | "htm" => "HTML",
            "css" | "scss" => "CSS",
            "c" | "h" => "C",
            "cpp" | "cc" | "hpp" => "C++",
            "go" => "Go",
            "java" => "Java",
            "sh" | "bash" => "Shell",
            "yaml" | "yml" => "YAML",
            "xml" => "XML",
            "sql" => "SQL",
            "txt" => "纯文本",
            _ => "文本",
        }
    }

    /// 调节编辑器字号并刷新预览（范围约束见 FONT_MIN/FONT_MAX）。
    pub(super) fn change_font_size(&mut self, delta: f32, cx: &mut Context<Self>) {
        self.font_size = (self.font_size + delta).clamp(FONT_MIN, FONT_MAX);
        // 字号用户自调后持久化（文件极小 + 原子写，每步一次可接受）。
        self.persist_prefs();
        // 实时预览编辑（WYSIWYG）字号：EditorState 继承外层 text_size（宿主容器渲染时
        // 读取 self.font_size），无需像旧的 MarkdownLivePreview 那样内部 set_font_override。
        cx.notify();
    }

    /// 重置正文字号到默认值并持久化。
    pub(super) fn reset_font_size(&mut self, cx: &mut Context<Self>) {
        self.font_size = DEFAULT_FONT_SIZE;
        self.persist_prefs();
        cx.notify();
    }

    /// 选项卡的「有效内容」：`live2`（WyEditor，实时）开启时读它，否则读源码编辑区。
    /// 保存/关闭/写回统一走此口径——**live 期间源码编辑区不镜像**（去 O(n)），直接读
    /// WyEditor 的 rope 才不丢内容（大文档 close→保存 若读旧编辑区会整段丢失）。
    /// 大文档后台解析**未完成**（`is_loaded() == false`）时 live2 是空壳，回退源码编辑区。
    pub(crate) fn tab_effective_text(&self, idx: usize, cx: &App) -> String {
        match self.tabs.get(idx) {
            Some(t) => match &t.live2 {
                Some(l2) => {
                    if l2.read(cx).is_loaded() {
                        l2.read(cx).text().to_string()
                    } else {
                        t.editor.read(cx).value().to_string()
                    }
                }
                None => t.editor.read(cx).value().to_string(),
            },
            None => String::new(),
        }
    }

    /// 退出预览编辑：把 WYSIWYG 内容写回源码编辑区 + 内容镜像 + 预览，销毁实体。
    pub(super) fn commit_live(&mut self, idx: usize, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get_mut(idx) else { return };
        tab.live_edit = false;
        tab._live2_sub = None;
        let Some(l2) = tab.live2.take() else { return };
        // 视图从 WYSIWYG 换回源码编辑区：先清空两端编辑器可能残留的鼠标拖选状态
        // （须在任何提前 return 之前）。点击「预览编辑测试」菜单项时 GPUI 把 MouseDown
        // 按 `hitbox.is_hovered` 漏进编辑区置 `dragging=true`（不按 z-order 裁掉被菜单
        // 遮挡的区间）；对应的 MouseUp 又被菜单项 `activate` 的 `stop_propagation`
        // 截断派发、到不了编辑器 `on_mouse_up` → `dragging` 残留。切回后仅移动鼠标
        // 就会从 anchor=0（`set_text` 收拢到行首）自动扩选到指针处。宿主在此「切回/
        // 切离」生命周期边界触发编辑器的统一拖选复位钩子（`cancel_mouse_drag`），
        // 并覆盖提前 return 路径（大文档未加载完不走 `set_text`）：WYSIWYG 实体
        // （随后即销毁）与源码编辑区（转前台显示）都不留拖选态。
        l2.update(cx, |ed, _| ed.end_selection());
        tab.editor.update(cx, |ed, _| ed.cancel_mouse_drag());
        // 大文档后台解析在途就退出：live2 尚无内容，不能写回（否则源码编辑区被清空）。
        // 源码编辑区仍是权威源，直接丢弃空壳实体即可。
        if !l2.read(cx).is_loaded() {
            self.status = SharedString::from("预览编辑测试：关（加载中退出，未写回编辑区）");
            self.status_ok = false;
            return;
        }
        let text = l2.read(cx).text().to_string();
        tab.content = SharedString::from(text);
        let content = tab.content.clone();
        let n_lines = content.as_ref().lines().count().max(1);
        let n_chars = content.as_ref().chars().count();
        tab.line_count = n_lines;
        tab.char_count = n_chars;
        tab.editor.update(cx, |ed, _| ed.set_text(&content));
        tab.md_preview.update(cx, |v, cx| v.set_source(&content, cx));
        // A2/A3：右栏 WyEditor 预览（若存在）随 WYSIWYG 写回全量刷新——
        // 写回是整篇替换（非 Change 轨迹），增量桥不会替它补，这里一次性对正源。
        if let Some(wy) = tab.md_wy.clone() {
            wy.update(cx, |w, _cx| w.set_source_text(&content));
        }
        self.status = SharedString::from("预览编辑（WYSIWYG）：关（内容已写回编辑区）");
        self.status_ok = true;
    }

    /// 「预览编辑测试」开关：用本项目 `markdown::wysiwyg::WyEditor` 渲染左面板。
    pub(super) fn toggle_live_test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let idx = self.active_tab;
        if !self.is_markdown_tab(idx) {
            self.status = SharedString::from("预览编辑测试仅支持 Markdown 文件");
            self.status_ok = false;
            cx.notify();
            return;
        }
        if self.tabs[idx].live2.is_some() {
            self.commit_live(idx, cx);
        } else {
            // 源快照：O(1) COW Rope。大文档走**后台解析**（与预览的后台 scan_blocks 同构）：
            // UI 线程只做 O(1) 快照 + 空壳编辑器创建 + 完成装配；整篇 O(n) 解析
            // （`Doc::open` 6.4MB 实测 ~200ms）在 `background_executor`。小文档保持同步
            // （无「空白→内容」闪烁）。加载完成前 `is_loaded()==false`，宿主读写文本
            // （保存/退出）回退源码编辑区（`tab_effective_text` / `commit_live` 已守卫）。
            const ASYNC_LOAD_MIN_BYTES: usize = 512 * 1024;
            let rope = self.tabs[idx].editor.read(cx).text().clone();
            let async_load = rope.len() >= ASYNC_LOAD_MIN_BYTES;
            let ed = if async_load {
                cx.new(|cx| WyEditor::new(window, cx))
            } else {
                let text = SharedString::from(rope.to_string());
                cx.new(|cx| WyEditor::new(window, cx).with_text(text.as_ref()))
            };
            // 同步全局字号（新建即套用；WY 字号由编辑器自持，measure/prepaint 一致）
            let fs = self.font_size;
            ed.update(cx, |ed, _| ed.set_font_size(gpui::px(fs)));
            // 图片相对路径基于文件所在目录解析（与预览一致）
            let base_dir = self.tabs[idx]
                .file_path
                .as_ref()
                .and_then(|p| p.parent())
                .map(|d| d.to_path_buf());
            ed.update(cx, |ed, _| ed.set_base_dir(base_dir));
            let ed_sub = cx.subscribe(&ed, |this, emitter, ev: &WyEvent, cx| {
                let _t = crate::prof::begin();
                match ev {
                    WyEvent::Changed => {
                        if let Some(tab) =
                            this.tabs.iter_mut().find(|t| t.live2.as_ref() == Some(&emitter))
                        {
                            // **O(1) 增量计数**（`WyEditor::line_count/char_count` 直读
                            // `Doc` 行起始表/增量字符计数，不物化全文），源码编辑区与右侧
                            // Markdown 预览不再做每键/停手整篇镜像（大文档 set_text 级联
                            // 重解析会卡 7-8s）——它们随左面板 WYSIWYG 一起，到退出
                            // `commit_live` 时统一写回。
                            tab.line_count = emitter.read(cx).line_count();
                            tab.char_count = emitter.read(cx).char_count();
                            if !tab.dirty {
                                tab.dirty = true;
                            }
                            cx.notify();
                        }
                    }
                    WyEvent::OpenPage(p) => {
                        this.status = SharedString::from(format!("WYSIWYG 点击链接：{p}"));
                        this.status_ok = true;
                    }
                }
                crate::prof::end("wy.app.changed", _t);
            });
            let tab = &mut self.tabs[idx];
            tab.live2 = Some(ed.clone());
            tab._live2_sub = Some(ed_sub);
            tab.live_edit = true;
            // 源编辑区即将被 WYSIWYG 取代（隐藏/切离）：在此「被隐藏」生命周期点复位
            // 其鼠标拖选态（与 `commit_live` 同用 `cancel_mouse_drag` 统一钩子）。菜单
            // 点击漏进的 MouseDown 恰在切离前把 `dragging` 置位过（见 `commit_live` 注释），
            // 若不在此刻清掉，隐藏期的残留会在切回后仅移动鼠标时把选区从 anchor 拉长。
            tab.editor.update(cx, |ed, _| ed.cancel_mouse_drag());
            if async_load {
                let tab_idx = idx;
                let ed_install = ed.clone();
                cx.spawn(async move |this, cx| {
                    let t0 = std::time::Instant::now();
                    let result =
                        cx.background_executor()
                            .spawn(async move {
                                crate::markdown::wysiwyg::WyEditor::open_from_rope(rope)
                            })
                            .await;
                    crate::prof::add_absolute(
                        "wy.app.load_async_parse",
                        t0.elapsed(),
                    );
                    let _ = this.update(cx, |this, cx| {
                        // 只装配仍挂在原 tab 的 live2（用户在解析期间切换/退出则丢弃）。
                        if this
                            .tabs
                            .get(tab_idx)
                            .is_some_and(|t| t.live2.as_ref() == Some(&ed_install))
                        {
                            ed_install.update(cx, |ed, _| ed.install_open(result));
                            let tab = &mut this.tabs[tab_idx];
                            tab.line_count = ed_install.read(cx).line_count();
                            tab.char_count = ed_install.read(cx).char_count();
                            cx.notify();
                        }
                    });
                })
                .detach();
            }
            self.status = SharedString::from("预览编辑测试：开（本项目 Doc 管线 WYSIWYG，初步实现）");
            self.status_ok = true;
            let focus = gpui::Focusable::focus_handle(ed.read(cx), cx);
            window.focus(&focus, cx);
        }
        cx.notify();
    }
}
