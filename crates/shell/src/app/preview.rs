//! 编辑 ⇄ 预览切换与统一 MD 预览/所见即所得（WYSIWYG）：预览开关与全屏、
//! 统一 WyEditor 预览组件（`md_wy`）的创建/喂源/滚动同步、WYSIWYG 活区
//! （`live2`）的进入/写回，以及路径 → 类型/语言推断等路由辅助。

use std::ops::Range;
use std::path::Path;

use super::*;

/// 右栏只读预览 `md_wy` 走「逐 hunk 增量重放」的最大基线跨度（被替换的 base 字节总量）。
/// 超过即改用整篇 `set_source_text`：WYSIWYG 对深部大结构改动的增量会退化成本应整篇做
/// 的重排、并把它推迟到切回后第一渲染帧（实测 6.4MB 大段替换首帧卡 5.5s）；一次性 O(n)
/// Doc 打开（实测 ~1s）成本落在同步写回里、首帧零卡——总墙钟更短。典型小改动（几字符/
/// 几行）跨度远低于此，走增量（几十 ms）。
const MD_WY_DIFF_MAX_SPAN_BYTES: usize = 64 * 1024;

/// B3：右栏 `md_wy` 整篇喂源时「后台异步装配」的内容大小门槛（对齐 toggle ON 的
/// `ASYNC_LOAD_MIN_BYTES`）。大内容（如 6.4MB 大段切回）走后台 `open_from_str` 解析、
/// 点击路径零 O(n)，右栏内容「后到」；小内容保持同步（几十 KB 级 Doc::open 极快，
/// 无「旧→新」等待窗口）。
const MD_WY_ASYNC_INSTALL_MIN_BYTES: usize = 512 * 1024;

/// I2：右栏预览「Scrub 延后」进入判定的连续跨块帧下限。预览连续 ≥ 此帧数快速跨块
/// （右栏拖滚动条 / 高频滚屏）即判定为 Scrub 期——期内的编辑区 `scroll_to_line` 跟随
/// **延后**到静止后一次收敛，消除「拖动每帧嵌编辑器全窗重建」的 28ms/帧卡顿
/// （D1：ed.build_rows+prepaint+paint ≈11ms 叠加 WYSIWYG ~17ms）。低频单步滚屏
/// 跨块数不足、不触发，保持慢滚逐帧对齐。
const SCRUB_ENTER_STREAK: u32 = 2;
/// Scrub 延后期内「预览静止」的收敛帧数：预览连续 ≥ 此帧数不再跨块（拖条松手 /
/// 快速连滚停下）即**一次收敛**到延后期内最新预览顶块。帧数与墙钟兜底二者取先到。
const SCRUB_IDLE_FRAMES: u32 = 2;
/// Scrub 延后期内「预览静止」的收敛墙钟兜底：即使渲染帧稀疏（静止后无持续重绘），
/// 距上次跨块 ≥ 此时长后任一后续同步帧都会一次收敛，避免两栏长期错位。
const SCRUB_IDLE: std::time::Duration = std::time::Duration::from_millis(200);

/// 测试探针（`cfg(test)`）：B3 **后台异步整篇装配**发起次数——大跨度切回登记
/// `open_from_str` 后台解析（而非同步整篇 `set_source_text`）的路径计数。回归测试
/// 据此断言「大段切回同步路径不再 O(n) 解析」。
#[cfg(test)]
pub(crate) static LIVE_COMMIT_MD_WY_ASYNC: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// 测试探针（`cfg(test)`）：toggle ON 命中**共享复用右栏已解析 Doc** 的次数——大文档
/// 切 live2 走 `md_wy.doc().clone()` O(1) 复用（消双解析）而非 `open_from_rope` 整篇
/// 重解析。回归测试据此断言「复用路径真的被走上」。
#[cfg(test)]
pub(crate) static LIVE_ON_MD_WY_REUSE: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// 测试探针（`cfg(test)`）：`commit_live` 退出路径计数——整篇写回（有改动）vs
/// 未改动直接销毁实体（O(1) 跳过写回）。app 层回归测试据此断言「未编辑不写回」。
#[cfg(test)]
pub(crate) static LIVE_COMMIT_WRITEBACK: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
#[cfg(test)]
pub(crate) static LIVE_COMMIT_SKIP_UNCHANGED: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
/// 测试探针（`cfg(test)`）：走**增量 diff 写回**（replaces 非空 → `apply_replaces` /
/// `preview_apply_edit` 被调用）的 `commit_live` 次数。方案2 后仅「编辑日志重放护栏
/// 失败 / 有脏标记但无日志」的兜底路径累加。
#[cfg(test)]
pub(crate) static LIVE_COMMIT_DIFF_APPLIED: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
/// 测试探针（`cfg(test)`）：增量写回实际应用的替换 hunk 累计数（>0 即非整篇替换）。
/// 仅「护栏/无日志」兜底走 diff 时累加——主路径已被编辑日志重放（[`LIVE_COMMIT_OPS_*`]）
/// 取代。
#[cfg(test)]
pub(crate) static LIVE_COMMIT_DIFF_HUNKS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
/// 测试探针（`cfg(test)`）：走**编辑日志重放**（方案2，`apply_live_replay` 主路径）写回
/// 的 `commit_live` 次数（op 非空且护栏自检通过）。
#[cfg(test)]
pub(crate) static LIVE_COMMIT_OPS_REPLAYED: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
/// 测试探针（`cfg(test)`）：编辑日志重放实际应用的 op 累计数（= 该 live 会话缴存的
/// 内容变更条数；引用计数断言「单字符恰 1 条」「大粘贴单大 op」）。
#[cfg(test)]
pub(crate) static LIVE_COMMIT_OPS_COUNT: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

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

    /// A2：当前激活选项卡的右栏 WyEditor 预览实体（仅当该选项卡**真正以 WyEditor 作
    /// 预览渲染**时才返回）：Markdown / EPUB / 纯文本（都可能有 `md_wy`）返回宿主；
    /// PDF / 图片 / 视频（自带阅读器）→ `None`。`render_preview_pane` /
    /// `render_fullscreen_preview` 据此决定走统一组件宿主。
    ///（M5-B5 P4-2：`MarkdownPreview` 已删，预览统一 WyEditor。）
    pub(super) fn md_wy_preview(&self) -> Option<gpui::Entity<WyEditor>> {
        let tab = self.tabs.get(self.active_tab)?;
        // PDF/图片/视频用自带阅读器，绝不以 WyEditor 渲染。
        if tab.reader.is_reader() {
            return None;
        }
        tab.md_wy.clone()
    }

    /// EPUB 轻量视口分页读数 `(当前页, 总页数)`（1 基；状态栏页码，P4-1/P4-2）：
    /// 读 WyEditor 只读宿主 WYSIWYG 确定性量程（`MarkdownPreview` 回退后端已删）。
    /// 非 EPUB / 就绪前 → `None`（状态栏不显示页码）。
    pub(super) fn epub_page_bounds(&self, window: &mut Window, cx: &gpui::App) -> Option<(usize, usize)> {
        let idx = self.active_tab;
        if !self.is_epub_tab(idx) {
            return None;
        }
        let tab = self.tabs.get(idx)?;
        // 视口高：以窗口内容高估算（容器 ≈ 窗口正文区；误差 ≤ 顶部/底部工具条高，
        // 对「第 x/N 页」展示无碍）。
        let view_h = f32::from(window.viewport_size().height).max(1.);
        tab.md_wy.as_ref().and_then(|w| w.read(cx).viewport_page_bounds(view_h))
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
            scrub: false,
            scrub_streak: 0,
            scrub_idle_frames: 0,
            scrub_target: None,
            scrub_last_cross: std::time::Instant::now(),
        };
        return;
    }
    let prev = self.md_scroll_sync;
    self.md_scroll_sync.editor_block = editor_block;
    self.md_scroll_sync.preview_block = preview_block;

    // I2 Scrub 检测前置量：预览本帧是否「自主跨块」（顶块较上帧变化，且**不是**编辑器
    // 主导下的预览到达——编辑器驱动预览期 `driven_preview_target=Some`，到达帧绝不计入
    // Scrub，防编辑器→预览对齐被误判成拖动）。逐帧维护连续跨块计数（非连续即清零），
    // 供进入延后判定用；同时刷新墙钟兜底时间戳。
    let preview_moved = self.md_scroll_sync.driven_preview_target.is_none()
        && prev.preview_block != preview_block
        && preview_block != self.md_scroll_sync.driven_preview_target;
    if preview_moved {
        self.md_scroll_sync.scrub_streak = self.md_scroll_sync.scrub_streak.saturating_add(1);
        self.md_scroll_sync.scrub_last_cross = std::time::Instant::now();
    } else {
        self.md_scroll_sync.scrub_streak = 0;
    }

    let Some(eb) = editor_block else { return };
    if preview_block == Some(eb) {
        // 两侧同一块，已同步：作废「等待预览到达」与「等待编辑器到达」的驱动目标（已对齐）。
        // Scrub 延后态一并退出（preview_moved=false 帧已把 streak 清零）。
        self.md_scroll_sync.driven_preview_target = None;
        self.md_scroll_sync.driven_editor_target = None;
        self.md_scroll_sync.driven_editor_await = 0;
        self.md_scroll_sync.scrub = false;
        self.md_scroll_sync.scrub_idle_frames = 0;
        self.md_scroll_sync.scrub_target = None;
        return;
    }
    // 我方正在驱动编辑器到达 `driven_editor_target`：期间 editor_block 的连续变化是
    // 跟随落位（边界块顶行读取还会滞后/抖动一两帧），**不是**用户主动滚动 → 不得触发
    // 下方「编辑器先动 → 驱动预览」回声，否则预览会被编辑器落位拽回（来回跳动）。
    // 宽限期内只计数，超限才放手交给正常双向同步（防永远到不了目标时停摆）。
    if prev.editor_block != editor_block {
        if self.md_scroll_sync.driven_editor_target.is_some() {
            // 我方驱动编辑器落位中（含 Scrub 收敛 / 进入延后前的最后一次跟随落位）：
            // 抑制回声，只给宽限计数；超限才放手交给正常双向同步。**不**清 Scrub——
            // 延后期内编辑器落位余波不得把延后态打断（防拖动中延后被落位逐帧打断）。
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
        // 编辑器真正用户滚动（无我方驱动在途且不在 Scrub 延后期）→ 预览跟到编辑器
        // 顶行所在块；Scrub 延后不适用，退出。
        self.md_scroll_sync.scrub = false;
        self.md_scroll_sync.scrub_idle_frames = 0;
        self.md_scroll_sync.scrub_target = None;
        // **不能在本帧 render 直接调** `scroll_to_line`（宿主 `ScrollHandle::set_offset`
        // 在渲染期会被本帧布局复位）：只把目标行写进 WY 的 `scroll_target`，由其元素
        // prepaint 逐帧重钉宿主偏移直到稳住（有 [`WyEditor::SCROLL_TARGET_MAX_APPLIES`]
        // 兜底）。记录「等待预览到达」目标：预览重钉落位到它时不被误判为预览用户滚动。
        md.update(cx, |v, _| v.set_scroll_target(eb));
        self.md_scroll_sync.driven_preview_target = Some(eb);
    } else if self.md_scroll_sync.scrub && !preview_moved {
        // Scrub 延后期内预览已静止（拖条松手 / 连滚停下）：静止帧计数推进，达到帧数
        // 或墙钟阈值（二者取先到）即**一次收敛**到延后期内最新预览顶块，恢复双侧对齐
        // （收敛后由正常双向同步照常跟进——幂等，目标已到即静止）。
        self.md_scroll_sync.scrub_idle_frames += 1;
        if self.md_scroll_sync.scrub_idle_frames >= SCRUB_IDLE_FRAMES
            || self.md_scroll_sync.scrub_last_cross.elapsed() >= SCRUB_IDLE
        {
            if let Some(t) = self.md_scroll_sync.scrub_target {
                editor.update(cx, |ed, _| ed.scroll_to_line(t));
                self.md_scroll_sync.driven_editor_target = Some(t);
                self.md_scroll_sync.driven_editor_await = 0;
                self.md_scroll_sync.editor_block = Some(t);
                self.md_scroll_sync.driven_preview_target = None;
            }
            self.md_scroll_sync.scrub = false;
            self.md_scroll_sync.scrub_idle_frames = 0;
            self.md_scroll_sync.scrub_target = None;
        }
    } else if preview_moved {
        // 预览自主跨块 → 预览主导。统一收尾：旧等待预览目标作废。
        self.md_scroll_sync.driven_preview_target = None;
        if self.md_scroll_sync.scrub {
            // 延后期内继续跨块：只推进收敛目标（最新预览顶块），**不逐帧**驱动编辑器
            // （延后的全部意义所在：拖动中不再每帧嵌编辑器全窗重建）。`driven_editor_target`
            // 保持不动——延后期前的最后一次跟随落位（若有）仍由 `driven_editor_await` 收敛。
            // **不预填 editor_block**：延后期内编辑器未被驱动，顶块就按实读数，避免伪造
            // 「编辑器在动」触发上方分支二、把延后态打断。
            self.md_scroll_sync.scrub_target = preview_block;
            self.md_scroll_sync.scrub_idle_frames = 0;
            return;
        }
        if self.md_scroll_sync.scrub_streak >= SCRUB_ENTER_STREAK {
            // 连续跨块（拖动 / 高频滚屏）→ 进入 Scrub 延后：本次起编辑器跟随延后到
            // 静止后一次收敛。首/低频单次跨块不满足连跨阈值，仍走下方立即跟随。
            // 同理不预填 editor_block（编辑器未被驱动，看实读数）。
            self.md_scroll_sync.scrub = true;
            self.md_scroll_sync.scrub_idle_frames = 0;
            self.md_scroll_sync.scrub_target = preview_block;
            return;
        }
        // 首次/低频跨块：照常立即跟随（同块内微滚 / 慢速单步滚仍逐帧对齐，行为不变）。
        if let Some(pb) = preview_block {
            editor.update(cx, |ed, _| ed.scroll_to_line(pb));
            // 预置编辑器基线为驱动目标：编辑器下一帧实际到位即与基线一致，不再被误判
            // 成「编辑器用户主动滚动」而把预览拽回（来回跳动）；预览现为主导，旧等待
            // 目标作废。
            self.md_scroll_sync.driven_editor_target = Some(pb);
            self.md_scroll_sync.driven_editor_await = 0;
            self.md_scroll_sync.editor_block = Some(pb);
        }
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
    /// MD / EPUB 统一预览都走 WyEditor 只读宿主（条目 = 源码行，点击滚动统一组件）；
    /// 无 WyEditor（未开启预览 / 非预览文档）→ `None`。
    pub(super) fn md_toc_source(
        &self,
        cx: &gpui::App,
    ) -> Option<(markdown_wysiwyg::TocEntries, MdTocScroll)> {
        let tab = self.tabs.get(self.active_tab)?;
        let wy = tab.md_wy.as_ref()?;
        Some((wy.read(cx).toc_entries(), MdTocScroll::Unified(wy.clone())))
    }

    /// 右栏 WyEditor 懒创建的共用引导块（`ensure_md_wy` / `ensure_epub_wy` 两处手抄的
    /// ~15 行设置收敛于此）：尚未创建时新建并套 A1 只读门控 `set_editable(false)` +
    /// 全局字号 + 基准目录；`configure` 注入阅读器差分支（当前 MD/EPUB 创建期均无专属
    /// 设置 → 传空闭包；EPUB 的只读居中/内存图/注记等仍在调用方喂源块）。保持 lazy-once
    /// 语义（不预建），各阅读器创建后的喂源差异逻辑留在各自调用方。
    fn ensure_wy(
        &mut self,
        idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
        configure: impl FnOnce(&mut WyEditor, &mut gpui::App),
    ) {
        let fs = self.font_size;
        let base_dir = self.tabs[idx]
            .file_path
            .as_ref()
            .and_then(|p| p.parent())
            .map(|d| d.to_path_buf());
        if self.tabs[idx].md_wy.is_none() {
            let ed = cx.new(|cx| WyEditor::new(window, cx));
            ed.update(cx, |ed, cx| {
                ed.set_editable(false); // A1 只读门控：右栏预览不可编辑
                ed.set_font_size(gpui::px(fs));
                ed.set_base_dir(base_dir);
                configure(ed, cx);
            });
            self.tabs[idx].md_wy = Some(ed.clone());
        }
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
        // B3：喂源/就地换源前作废在途后台装配（防旧切回的装配把新喂内容覆盖回去）。
        self.md_wy_invalidate_async(idx);
        // 懒创建引导块共用；MD 创建期无专属设置 → 空配置闭包。
        self.ensure_wy(idx, window, cx, |_ed, _cx| {});
        if feed {
            // 同步喂源：源 = 编辑区规范文本（与预览宿主同口径）。
            let fs = self.font_size;
            let base_dir = self.tabs[idx]
                .file_path
                .as_ref()
                .and_then(|p| p.parent())
                .map(|d| d.to_path_buf());
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
    /// （`set_memory_images`）、注记浮卡/引文跳转正文（`set_notes`，P4-2 迁入）。
    pub(super) fn ensure_epub_wy(
        &mut self,
        idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
        text: &str,
        images: &std::collections::HashMap<String, std::sync::Arc<gpui::Image>>,
        notes: &std::collections::BTreeMap<usize, String>,
    ) {
        if idx >= self.tabs.len() {
            return;
        }
        // B3：喂源/就地换源前作废在途后台装配（防旧切回装配覆盖新喂内容）。
        self.md_wy_invalidate_async(idx);
        // 懒创建引导块共用；EPUB 创建期无专属设置 → 空配置闭包。
        self.ensure_wy(idx, window, cx, |_ed, _cx| {});
        let fs = self.font_size;
        let base_dir = self.tabs[idx]
            .file_path
            .as_ref()
            .and_then(|p| p.parent())
            .map(|d| d.to_path_buf());
        if let Some(ed) = self.tabs[idx].md_wy.clone() {
            ed.update(cx, |ed, _| {
                ed.set_font_size(gpui::px(fs));
                ed.set_base_dir(base_dir);
                ed.set_memory_images(images.clone());
                ed.set_epub(true);
                ed.set_notes(notes.clone());
                ed.set_source_text(text);
            });
        }
    }

    /// P4-1：EPUB 章节翻转「上一章/下一章」。章界 = 目录标题条目（每章以标题起始）。
    /// 统一 WyEditor 宿主：当前章 = 顶行所在标题，滚动到相邻章标题（`set_scroll_target`，
    /// 统一 WyEditor 宿主：当前章 = 顶行所在标题，滚动到相邻章标题（`set_scroll_target`，
    /// prepaint 重钉，无需 window；`MarkdownPreview` 回退后端已删）。
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
        let MdTocScroll::Unified(wy) = scroll;
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
            // 统一预览（M5-B5 P4-2）：所有非只读文本选项卡（MD / 纯文本）→ `ensure_md_wy`
            // 同步喂源；EPUB（只读）→ `ensure_epub_wy`（WyEditor 只读宿主 + 内存图/注记/
            // 引文/页码）。`MarkdownPreview` 后端已删除，无回退分支。
            if !readonly {
                self.ensure_md_wy(idx, window, cx, true);
            } else if self.is_epub_tab(idx) {
                let images = self.tabs[idx].epub_images.clone();
                let notes = self.tabs[idx].epub_notes.clone();
                let content = self.tabs[idx].content.clone();
                self.ensure_epub_wy(idx, window, cx, content.as_ref(), &images, &notes);
            }
        } else {
            // 关闭预览：右栏 WyEditor 预览随预览开关释放（整篇解析/测量缓存随之释放，
            // 内存回到基线）。B3：在途后台整篇装配一并作废（实体被丢弃，完成回调按
            // tab 归属/代号不再安装）。
            self.md_wy_invalidate_async(idx);
            self.tabs[idx].md_wy = None;
            self.tabs[idx].md_wy_vscroll = gpui::ScrollHandle::new();
        }
        cx.notify();
    }

    /// 切换当前选项卡预览是否全屏占满（EPUB 阅读模式）。
    /// P4-2：预览统一 WyEditor（恒换行），全屏只影响编辑区/布局，无需再设预览换行。
    pub(super) fn toggle_preview_fullscreen(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let idx = self.active_tab;
        let (fullscreen, editor) = {
            let Some(tab) = self.tabs.get_mut(idx) else {
                return;
            };
            tab.preview_fullscreen = !tab.preview_fullscreen;
            let fs = tab.preview_fullscreen;
            (fs, tab.editor.clone())
        };
        // 编辑区软换行：进入全屏阅读 → 编辑区强制换行；退出全屏 → 恢复默认。
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

    /// 是否 Markdown 文件（编辑区/预览默认换行的依据）。
    pub(super) fn is_markdown_path(p: &Path) -> bool {
        classify(p) == PathKind::Markdown
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

    /// 是否 EPUB 文件（按扩展名识别；EPUB 预览走 WyEditor 只读宿主的依据）。
    pub(super) fn is_epub_path(p: &Path) -> bool {
        classify(p) == PathKind::Epub
    }

    /// 是否 PDF 文件（按扩展名识别）。
    pub(super) fn is_pdf_path(p: &Path) -> bool {
        classify(p) == PathKind::Pdf
    }

    /// 是否图片文件（按扩展名识别；清单见 [`image_viewer::IMAGE_EXTENSIONS`]）。
    pub(super) fn is_image_path(p: &Path) -> bool {
        classify(p) == PathKind::Image
    }

    /// 是否视频文件（按扩展名识别；清单见 [`video::VIDEO_EXTENSIONS`]）。
    #[cfg(feature = "video")]
    pub(super) fn is_video_path(p: &Path) -> bool {
        classify(p) == PathKind::Video
    }

    /// 按扩展名推断 gpui-component 语法高亮的语言名（未知扩展名回退 "text"）。
    pub(super) fn language_id(p: &Path) -> &'static str {
        classify(p).language_id()
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

    /// 按扩展名推断语言标签（仅用于状态栏展示；单一真值见 [`Self::classify`]）。
    pub(super) fn language_label(&self) -> &'static str {
        let Some(p) = self
            .tabs
            .get(self.active_tab)
            .and_then(|t| t.file_path.as_ref())
        else {
            return "纯文本";
        };
        classify(p).language_label()
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

    /// （仅测试）当前 `retired_live2` 延迟回收队列长度——A2 探针：切回瞬间应暂存 1 个
    /// 旧 live2 实体，下一帧 `on_next_frame` 清空后回 0（无泄漏到下一次 open）。
    #[cfg(test)]
    pub(crate) fn retired_live2_len(&self) -> usize {
        self.retired_live2.len()
    }

    /// 退出预览编辑：把 WYSIWYG 内容写回源码编辑区 + 内容镜像 + 预览；live2 实体**延迟**
    /// 到下一帧渲染后销毁（A：实体级联 Drop ~36ms 移出点击同步路径）。
    pub(super) fn commit_live(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get_mut(idx) else { return };
        // 本 live 会话是否有内容编辑（读后再清，供下方「未改动跳过写回」判定）。
        let l2_dirty = tab.live2_dirty;
        tab.live2_dirty = false;
        tab.live_edit = false;
        tab._live2_sub = None;
        let Some(l2) = tab.live2.take() else { return };
        // A2：**延迟回收**。live2 实体（WyEditor 大对象：Doc 的 rope/行索引/派生缓存 +
        // 整`Rc` 级联）的销毁占切回 ~36ms，不能丢在点击事件路径里同步 Drop。先把实体
        // 暂存到 app 侧 `retired_live2`（clone 是 O(1) 句柄；本函数内 `l2` 继续照常读写），
        // 再用 `cx.on_next_frame`（gpui 保证「下一帧渲染完之后」触发）清空队列放实体
        // 出作用域 Drop。**渲染引用安全性**：此处已 `tab.live2 = None`，切回后的任何
        // render 都不再引用旧实体；`on_next_frame` 又晚于「下一帧已渲染/呈现」，故延迟
        // Drop 时既无渲染在读它、也不占用展示切回结果的那一帧的 CPU。本函数所有提前
        // return 路径共用此延迟（未改动 O(1) 跳过 / 加载中退出同样受益）。
        self.retired_live2.push(l2.clone());
        cx.on_next_frame(window, |this, _window, _cx| {
            this.retired_live2.clear();
        });
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
        // 未改动（live 期间无任何内容编辑，`live2_dirty == false`）：live2 文本恒等于
        // 打开瞬间的源码 COW 快照（toggle ON 只从该快照建实体，无其它写入路径），
        // 整篇物化 + `set_text` 写回 + 喂右栏纯属 O(n) 浪费（大文档级联重排卡 7-8s）——
        // 直接销毁实体切回（O(1)），源码编辑区仍是原内容、可继续编辑。
        if !l2_dirty {
            #[cfg(test)]
            LIVE_COMMIT_SKIP_UNCHANGED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.status = SharedString::from("预览编辑测试：关（未改动，无需写回）");
            self.status_ok = true;
            return;
        }
        #[cfg(test)]
        LIVE_COMMIT_WRITEBACK.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // —— 编辑日志重放（M：方案2，取代行级 diff 的主路径）——
        // live2 在 live 会话期把每笔内容变更（`replace_range` / 撤销 / 重做统一缴存）记成
        // `(字节区间, 新文本)` op 日志：两步起点同源（toggle ON 的源码 COW Rope 快照、
        // live 期间源码编辑区不被改写）→ op 字节偏移在源码编辑区与 live2 恒等，**按序
        // 重放即得最终态**，无需行级 LCS / 整篇物化比对。单 op = O(log n) Rope 编辑 +
        // 窗口化高亮增量。实测（debug，6.4MB）：写回本体 1 字符 ~1ms、3 行 ~3ms（旧 diff
        // 光行级扫描就 ~45ms）；`tab.content` 经 A1 走 SharedString 零拷贝分配（大文档 rope
        // 源首次 `text_shared` 恰一次 O(n) 物化 ~20ms 属固有下限）；live2 实体销毁（~36ms）
        // 已由 A2 延迟到下一帧（见上 `retired_live2`），不再计进本点击路径。
        let ops = l2.update(cx, |ed, _| ed.take_live_ops());
        if !ops.is_empty() {
            // 最终态 = live2 文本（A1：`SharedString` 句柄的 O(1) clone 赋给 `tab.content`，
            // 零拷贝共享同一缓冲；大文档 rope 源首次 `text_shared` 恰一次 O(n) 物化是
            // `tab.content` 必须为整段字符串的固有下限，已是最小化作一次）。
            let content = l2.read(cx).text();
            let src_len;
            let ok = {
                let tab = &mut self.tabs[idx];
                tab.content = content.clone();
                // 行/字符计数直接取 WyEditor 增量维护的 O(1) 值（订阅已随每次编辑更新；
                // 不再对 6.4MB 全文做 lines()/chars() 线性扫——那是旧路径 ~90ms 的主来源）。
                tab.line_count = l2.read(cx).line_count();
                tab.char_count = l2.read(cx).char_count();
                // 重放前的源缓冲区长度（O(1)；右栏 md_wy 逐 op 重放的前提 = 起点与源同长，
                // 长度不等即空壳/失步 → 整篇喂源兜底，见 `md_wy_align_after_ops`）。
                src_len = tab.editor.read(cx).text().len();
                // 重放到源码编辑区（`apply_live_replay` 内置「干跑 + O(1) 长度栏」护栏）：
                // 自检失败 → 缓冲区未被改动，落下方既有 diff 兜底（content 仍是最终态）。
                tab.editor.update(cx, |ed, _| ed.apply_live_replay(&ops, content.as_ref()))
            };
            if ok {
                #[cfg(test)]
                {
                    LIVE_COMMIT_OPS_REPLAYED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    LIVE_COMMIT_OPS_COUNT
                        .fetch_add(ops.len(), std::sync::atomic::Ordering::Relaxed);
                }
                // A2/A3 + B3：右栏只读预览 `md_wy`（若存在）随写回对齐。小跨度维持增量
                // `preview_apply_edit`（1-3ms，不回归）；大跨度/失步改整篇喂源——大内容
                // **后台异步装配**（`md_wy_feed_full`），同步路径不再 O(n) `Doc::open`。
                self.md_wy_align_after_ops(idx, &ops, src_len, content, cx);
                self.status = SharedString::from("预览编辑（WYSIWYG）：关（内容已写回编辑区）");
                self.status_ok = true;
                return;
            }
            // 护栏失败 → 落到既有行级 diff 兜底（源码编辑区未被重放污染，`base` 仍有效）。
        }
        // —— 既有行级 diff 写回（护栏失败 / 有脏标记但无日志时的兜底，见 src/app/diff.rs）——
        // 与「toggle ON 的源快照」（= 当前源码编辑区缓冲区，live 期间无其它写入路径）
        // 做**行级 diff**，产出少量（字节区间, 文本）替换 → 应用到：
        //   1) 源码编辑区缓冲区：`CodeEditor::apply_replaces`（每处 O(log n) Rope 编辑 +
        //      窗口化高亮局部重解析，**不整篇重建**——区别于旧 `set_text` 的 reset_document
        //      级联，大文档 debug ~11s）；
        //   2) 右栏只读预览 `md_wy`：同 diff 逐 hunk 用 `preview_apply_edit` 增量重放
        //      （与 A3 逐键桥同一无门控增量管线；大跨度改动按 `md_wy_align_after_diff`
        //      改整篇喂源——大内容后台异步装配）。
        // 典型小改动（几字符/几行）切回落到数百 ms；大范围改动也只重排受影响块，不退化
        // 成整篇 set_text 级联。基线文本与 live2 同源（全 LF 规范化），无需再处理 CRLF。
        let base;
        let content;
        let replaces;
        {
            let tab = &mut self.tabs[idx];
            base = tab.editor.read(cx).text().to_string();
            // A1：`l2.text()` 直接返回 `SharedString`（`Doc::text_shared` 的 O(1) 句柄），
            // 零拷贝赋给 `tab.content` —— 不再 `.to_string()` 又 `SharedString::from` 绕一圈
            // （省掉那份 6.4MB 副本与分配）。`content` 与 live2 的 rope 同源同值；SharedString
            // 不可变（仅 `Deref<Target=str>`），源码编辑区后续任何写入都会新建缓冲，不会
            // 改到被 `tab.content` 持有的这份 → 保存/脏标整篇读取安全。
            content = l2.read(cx).text();
            tab.content = content.clone();
            let n_lines = content.as_ref().lines().count().max(1);
            let n_chars = content.as_ref().chars().count();
            tab.line_count = n_lines;
            tab.char_count = n_chars;
            replaces = text_open::line_diff(base.as_str(), content.as_ref());
            if !replaces.is_empty() {
                tab.editor.update(cx, |ed, _| ed.apply_replaces(&replaces));
            }
        }
        #[cfg(test)]
        LIVE_COMMIT_DIFF_HUNKS.fetch_add(replaces.len(), std::sync::atomic::Ordering::Relaxed);
        #[cfg(test)]
        if !replaces.is_empty() {
            LIVE_COMMIT_DIFF_APPLIED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        // A2/A3 + B3：右栏 WyEditor 预览（若存在）随写回对齐。优先与源码编辑区同 diff
        // 逐 hunk 增量重放（后→前；`preview_apply_edit` 绕过只读门控、不写撤销栈、增量
        // 块重排，典型小改动几十 ms）。**大跨度 diff（被替换基线字节总量超阈值）改整篇
        // 喂源**：WYSIWYG 对深部大结构改动的「增量」会把最贵的整篇重排推迟到切回后第一
        // 帧，实测 6.4MB 大段替换更慢；B3 起大内容整篇喂源改后台异步装配（`md_wy_feed_full`）
        // ——同步路径不再 O(n)。md_wy 内容 ≠ 源快照（含空壳未就绪）也整篇兜底。
        self.md_wy_align_after_diff(idx, &replaces, base.as_str(), content, cx);
        self.status = SharedString::from("预览编辑（WYSIWYG）：关（内容已写回编辑区）");
        self.status_ok = true;
    }

    // —— B3：右栏 `md_wy` 整篇喂源 / 后台异步装配 ——

    /// B3：作废右栏 `md_wy` 的在途后台整篇装配（装配代号 +1、清 pending）。任何**同步**
    /// 改写 `md_wy` 内容的路径（增量重放 / 同步整篇喂源 / 换源 install_open / 预览关闭）
    /// 都必须在改动前调用——否则旧装配完成后会按旧代号命中、把刚改好的内容覆盖回去
    /// （装错）。`file_ops`（打开换源）也调用，故 `pub(super)`。
    pub(super) fn md_wy_invalidate_async(&mut self, idx: usize) {
        if let Some(t) = self.tabs.get_mut(idx) {
            t.md_wy_install.seq += 1;
            t.md_wy_install.pending = None;
            t.md_wy_install.pending_full = None; // Step3：任何同步改写 md_wy 内容 → 整篇在途标记一并作废
        }
    }

    /// B3：整篇喂右栏 `md_wy` 的统一入口（替代同步 `set_source_text`）。`content` 大
    /// （≥ [`MD_WY_ASYNC_INSTALL_MIN_BYTES`]）→ **后台** `open_from_str` 解析、就绪后带
    /// tab 归属 + 装配代号守卫 `install_open`（点击路径零 O(n)，右栏内容「后到」可接受）；
    /// 小内容 → 同步（几十 KB 级解析极快，无「旧→新」等待）。每次调用递增装配代号——
    /// 本次装配胜出，覆盖在途旧装配。`open_from_str` 在本管线内无失败路径；若守卫不通过
    /// （切走/关闭/更新换代）则完成回调直接丢弃结果，md_wy 保持当前（旧）内容，由后续
    /// 喂源/装配追平，不装错、不泄漏。
    fn md_wy_feed_full(&mut self, idx: usize, content: SharedString, cx: &mut Context<Self>) {
        let Some(wy) = self.tabs.get(idx).and_then(|t| t.md_wy.clone()) else {
            return;
        };
        let seq = {
            let t = &mut self.tabs[idx];
            t.md_wy_install.seq += 1;
            t.md_wy_install.seq
        };
        if content.len() < MD_WY_ASYNC_INSTALL_MIN_BYTES {
            // 同步小整篇（无「旧→新」等待；`open_from_str` 免 `set_source_text` 的一次
            // 字符串复制，直接共享这份 `content`）。
            wy.update(cx, |w, _| {
                w.install_open(markdown_wysiwyg::WyEditor::open_from_str(content))
            });
            if let Some(t) = self.tabs.get_mut(idx) {
                t.md_wy_install.pending = None;
                t.md_wy_install.pending_full = None; // Step3：同步装好即 == 编辑区 → 解除在途标记
            }
            return;
        }
        self.tabs[idx].md_wy_install.pending = Some((seq, content.clone()));
        // Step3：异步整篇在途、预览仍为旧内容（<新整篇）→ 置在途标记，增量桥打旧内容会
        // 错位，转为「作废 + 重喂」（见 `sync::on_editor_edited`）。
        self.tabs[idx].md_wy_install.pending_full = Some((seq, 0));
        #[cfg(test)]
        LIVE_COMMIT_MD_WY_ASYNC.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self::md_wy_spawn_async_install(idx, wy, content, seq, cx);
    }

    /// B3：切回后右栏 `md_wy` 对齐——**op 重放主路径**版（A2/A3 语义保留）。
    /// 小跨度（≤ 阈值）且 md_wy 与源同长 → 逐 op 增量 `preview_apply_edit`（1-3ms）；
    /// 大跨度 / 长度失步 → 整篇喂源 [`Self::md_wy_feed_full`]（大内容后台装配）。前提
    /// 只做 O(1) 的 md_wy 长度比对（同源、live 期间未被改写 → 起点一致）——**不做全篇
    /// 文本比对**（那会冷物化 md_wy 全文，6.4MB 又 +20ms）。
    fn md_wy_align_after_ops(
        &mut self,
        idx: usize,
        ops: &[(Range<usize>, SharedString)],
        src_len: usize,
        content: SharedString,
        cx: &mut Context<Self>,
    ) {
        let Some(wy) = self.tabs.get(idx).and_then(|t| t.md_wy.clone()) else {
            return;
        };
        let span: usize = ops
            .iter()
            .map(|(r, t)| (r.end - r.start).max(t.len()))
            .sum();
        if span <= MD_WY_DIFF_MAX_SPAN_BYTES && wy.read(cx).len() == src_len {
            // 增量重放会逐 op 把 md_wy 改成最终态 → 先作废在途后台装配（不装陈旧整篇）。
            self.md_wy_invalidate_async(idx);
            for (r, t) in ops {
                wy.update(cx, |w, cx| w.preview_apply_edit(r.clone(), t.as_ref(), cx));
            }
            // 重放后 md_wy 长度必然 == content（同起点 + 确定性 op）；若因未知原因不等 →
            // 整篇喂源兜底（仅长度 O(1) 比对）。
            if wy.read(cx).len() != content.len() {
                self.md_wy_feed_full(idx, content, cx);
            }
        } else {
            self.md_wy_feed_full(idx, content, cx);
        }
    }

    /// B3：切回后右栏 `md_wy` 对齐——**行级 diff 兜底**版（A2/A3 语义保留）。
    /// 增量 hunk（后→前 `preview_apply_edit`）；大跨度 diff 或 md_wy 内容 ≠ 基线 →
    /// [`Self::md_wy_feed_full`] 整篇喂源（大内容后台装配）。
    fn md_wy_align_after_diff(
        &mut self,
        idx: usize,
        replaces: &[(Range<usize>, String)],
        base: &str,
        content: SharedString,
        cx: &mut Context<Self>,
    ) {
        let Some(wy) = self.tabs.get(idx).and_then(|t| t.md_wy.clone()) else {
            return;
        };
        let span: usize = replaces.iter().map(|(r, _)| r.end - r.start).sum();
        let use_diff = !replaces.is_empty()
            && span <= MD_WY_DIFF_MAX_SPAN_BYTES
            && wy.read(cx).text().as_ref() == base;
        if use_diff {
            self.md_wy_invalidate_async(idx);
            for (r, t) in replaces.iter().rev() {
                wy.update(cx, |w, cx| w.preview_apply_edit(r.clone(), t, cx));
            }
        } else if !replaces.is_empty() || wy.read(cx).text().as_ref() != base {
            self.md_wy_feed_full(idx, content, cx);
        }
    }

    /// Step2/3：大文档 md「打开即提前整篇解析」——首块挂载（`apply_open_chunk`）后立刻以
    /// 独立后台任务**从文件路径读全文**（复用 `text_open::ChunkReader` 归一化：
    /// CRLF/孤立 CR → LF、UTF-8 校验，结果与分块读盘后的编辑区逐字节一致）+ 整篇
    /// `open_from_str` 解析，与编辑区分块追加**并行**（不再等 EOF 才开跑 Doc::open）。
    /// Step3：与 [`Self::md_wy_spawn_prefix_install`] **两阶段**协作——prefix 首装先行
    /// （首屏真内容、`pending_full` 在途），本函数为「整篇在那之后就绪 → 带守卫换入
    /// 全文」，两者共用同一装配代号，互不废对方；**不得双 install、不得 full→prefix
    /// 降级**（见 prefix 完成回调对 `md_wy_early==Done` 的屏蔽）。
    ///
    /// 完成回调守卫，任一不满足即丢弃（不装错、不泄漏）：
    ///   1) **tab 归属**：该 tab 的 `md_wy` 仍指向 `wy`（未关闭 / 未重建 / 未切走）；
    ///   2) **装配代号**：`md_wy_install_seq == seq`（期间换源/重喂/编辑在途作废）；
    ///   3) **未编辑**：`edit_version == 启动时版本`（用户已编辑 → 丢弃在途整篇，避免
    ///      把「装旧」覆盖用户对编辑器做的改动；该场景由 `sync.rs` 的重喂/增量链路接管）。
    ///
    /// 成功经 [`Self::md_wy_install_full_swap`] 换入全文（滚动锚 src_line 钉顶行）+ 置
    /// `md_wy_early = Done`（EOF 收尾据此跳过重复 install）；失败置 `Failed`（EOF 回退
    /// 走既有整篇装配兜底）。返回本次装配代号（供 prefix 首装共用）。
    pub(super) fn md_wy_spawn_early_install(
        &mut self,
        idx: usize,
        p: std::path::PathBuf,
        cx: &mut Context<Self>,
    ) -> usize {
        let Some(wy) = self.tabs.get(idx).and_then(|t| t.md_wy.clone()) else {
            return 0;
        };
        let (seq, ver) = {
            let t = &mut self.tabs[idx];
            t.md_wy_install.seq += 1;
            (t.md_wy_install.seq, t.edit_version)
        };
        self.tabs[idx].md_wy_install.early = Some(MdWyEarly::Pending { seq, edit_version: ver });
        cx.spawn(async move |this, cx| {
            // 独立后台任务：读全文（归一化）→ 整篇解析（UI 线程不背 O(n)）。
            let result = cx
                .background_executor()
                .spawn(async move {
                    use text_open::ChunkReader;
                    let mut text = String::new();
                    let mut reader = ChunkReader::open(&p)?;
                    while let Some(chunk) = reader.next_chunk()? {
                        text.push_str(&chunk.text);
                    }
                    Ok::<_, std::io::Error>(
                        markdown_wysiwyg::WyEditor::open_from_str(text.into()),
                    )
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                // 守卫 1/2（短借用）：tab 归属 + 装配代号。
                let ok = this.tabs.get(idx).is_some_and(|t| {
                    t.md_wy.as_ref() == Some(&wy) && t.md_wy_install.seq == seq
                });
                if !ok {
                    // 代号已换代 / 实体已易主（换源、重喂、切走重建）→ 本结果作废；清掉
                    // 本代号的在途标记（不误标 Failed——新装配会接管）。
                    if let Some(t) = this.tabs.get_mut(idx)
                        && t.md_wy_install.early.as_ref()
                            == Some(&MdWyEarly::Pending { seq, edit_version: ver })
                    {
                        t.md_wy_install.early = None;
                        t.md_wy_install.pending_full = None;
                    }
                    return;
                }
                match result {
                    // 守卫 3：未编辑 → 带滚动锚换入全文。
                    Ok(result) if this.tabs[idx].edit_version == ver => {
                        this.md_wy_install_full_swap(idx, &wy, result, cx);
                    }
                    Ok(_) => {
                        // 已编辑：丢弃整篇结果（sync 自愈/重喂链路接管），复位在途标记
                        //（EOF 收尾不再指望它，走既有收尾兜底）。
                        let t = &mut this.tabs[idx];
                        t.md_wy_install.early = None;
                        t.md_wy_install.pending_full = None;
                    }
                    Err(_) => {
                        // 读盘/解析失败：置 Failed，EOF 收尾回退走既有整篇装配兜底——*前提是
                        // EOF 尚未发生*（早装 Pending 已让位时 EOF 不会跑兜底 install）。
                        // 因此这里**即时兜底**：以当前编辑区 Rope（O(1) COW 快照）异步整篇
                        // 重喂，两种时序都不让右栏卡在占位：
                        //   - EOF 已过（早装 Pending 时让位）→ 本重喂落地装出预览；
                        //   - EOF 未过 → EOF 的 `md_wy_invalidate_async` 会作废本在途重喂，
                        //     EOF 兜底整篇装配接管（无双 install、不装陈旧）。
                        {
                            let t = &mut this.tabs[idx];
                            // 仅当依旧命中本代号（期间无换代）才接管，否则交新装配。
                            t.md_wy_install.early = Some(MdWyEarly::Failed);
                            t.md_wy_install.pending_full = None;
                        }
                        let rope = this.tabs[idx].editor.read(cx).rope().clone();
                        this.md_wy_feed_full_rope(idx, rope, cx);
                    }
                }
            });
        })
        .detach();
        seq
    }

    /// Step3：整篇就绪后**带守卫换入全文**的落地函数（`md_wy_spawn_early_install` 完成
    /// 回调与测试共用）。换入前抓当前顶行 `src_line`（prefix 帧；帧未就绪则 `None` 跳过），
    /// `install_open` 后经 `set_scroll_target` 由元素 prepaint（`advance_pending_scroll`）
    /// 逐帧重钉宿主偏移——prefix→全文**视觉不跳**；随后置 `md_wy_early = Done`、清
    /// `md_wy_pending_full`（整篇已装 → 增量桥恢复逐键）。
    pub(super) fn md_wy_install_full_swap(
        &mut self,
        idx: usize,
        wy: &gpui::Entity<WyEditor>,
        result: markdown_wysiwyg::WyOpenResult,
        cx: &mut Context<Self>,
    ) {
        let anchor = wy.read(cx).top_source_line();
        wy.update(cx, |ed, _| ed.install_open(result));
        if let Some(sl) = anchor {
            wy.update(cx, |ed, _| ed.set_scroll_target(sl));
        }
        if let Some(tab) = self.tabs.get_mut(idx) {
            tab.md_wy_install.early = Some(MdWyEarly::Done);
            tab.md_wy_install.pending_full = None;
        }
        cx.notify();
    }

    /// Step3：大文档 md「首块 prefix 首装」——首块挂载（`apply_open_chunk`）后即后台
    /// `open_from_rope(prefix)` 并 install（`loaded=true`，首屏立刻真内容），并置
    /// `md_wy_pending_full=(full_seq, prefix_len)`「整篇在途」；整篇（[`Self::md_wy_spawn_early_install`]）
    /// 就绪后带守卫换入全文。**prefix 来源 = 首块挂载时编辑区 rope 快照**（O(1) COW，
    /// 与编辑器同值同归一化）→ 增量桥在 prefix 上按编辑区坐标天然对齐；不用磁盘首读
    /// 字节（避免归一化口径分叉——同一份首块文本，磁盘读与编辑器 rope 理论上逐字节
    /// 一致，但快照零风险且零拷贝）。
    ///
    /// 完成守卫（任一不满足即丢弃，不装错、不降级）：
    ///   1) tab 归属：`md_wy` 仍指向 `wy`；
    ///   2) 装配代号：`md_wy_install_seq == full_seq`（与整篇**同代号**，两者互不废对方；
    ///      期间换源/重喂/编辑 → 代号换代皆弃）；
    ///   3) **整篇未先装**：`md_wy_early` 非 `Done`（整篇若已先到，prefix 再装会把全文
    ///      退回首屏——严禁降级）；
    ///   4) **未编辑**：`edit_version == 启动时版本`（已编辑 → prefix 快照已过期，交给
    ///      `sync` 自愈/重喂链路接管）。
    pub(super) fn md_wy_spawn_prefix_install(
        &mut self,
        idx: usize,
        prefix: Rope,
        full_seq: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(wy) = self.tabs.get(idx).and_then(|t| t.md_wy.clone()) else {
            return;
        };
        let ver = self.tabs[idx].edit_version;
        let prefix_len = prefix.len();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { markdown_wysiwyg::WyEditor::open_from_rope(prefix) })
                .await;
            let _ = this.update(cx, |this, cx| {
                let Some(tab) = this.tabs.get_mut(idx) else {
                    return;
                };
                if tab.md_wy.as_ref() != Some(&wy) || tab.md_wy_install.seq != full_seq {
                    return; // 归属/代号换代 → 丢弃（新装配接管）
                }
                if matches!(tab.md_wy_install.early, Some(MdWyEarly::Done)) {
                    return; // 整篇已先装：不降级回 prefix
                }
                if tab.edit_version != ver {
                    return; // 已编辑：prefix 快照已过期，sync 重喂接管
                }
                wy.update(cx, |ed, _| ed.install_open(result));
                tab.md_wy_install.pending_full = Some((full_seq, prefix_len));
                cx.notify();
            });
        })
        .detach();
    }

    /// Step3：整篇重喂的 **Rope 版**（`md_wy_feed_full` 同语义）：以**当前编辑区 Rope**
    /// （O(1) COW 快照）异步重喂全文——pending_full 期间（预览=prefix ≠ 编辑区）用户编辑
    /// 时增量桥坐标会错位，由 `sync::on_editor_edited` 调用本函数把预览重拉回「== 编辑
    /// 区」，随后退化逐键增量。小 rope 同步装、大 rope 后台 parse，均带装配代号守卫；
    /// 异步在途期间置 `md_wy_pending_full`（禁止对旧内容增量）。
    pub(super) fn md_wy_feed_full_rope(&mut self, idx: usize, rope: Rope, cx: &mut Context<Self>) {
        let Some(wy) = self.tabs.get(idx).and_then(|t| t.md_wy.clone()) else {
            return;
        };
        let seq = {
            let t = &mut self.tabs[idx];
            t.md_wy_install.seq += 1;
            t.md_wy_install.seq
        };
        if rope.len() < MD_WY_ASYNC_INSTALL_MIN_BYTES {
            // 同步小整篇：装好后预览 == 编辑区 → 解除在途标记。
            wy.update(cx, |w, _| {
                w.install_open(markdown_wysiwyg::WyEditor::open_from_rope(rope))
            });
            if let Some(t) = self.tabs.get_mut(idx) {
                t.md_wy_install.pending = None;
                t.md_wy_install.pending_full = None;
            }
            return;
        }
        self.tabs[idx].md_wy_install.pending = None;
        self.tabs[idx].md_wy_install.pending_full = Some((seq, 0));
        Self::md_wy_spawn_async_install_rope(idx, wy, rope, seq, cx);
    }

    /// Step3：`md_wy_feed_full_rope` 的后台装配（Rope 版，见 [`Self::md_wy_spawn_async_install`]）。
    /// 完成回调守卫同非 rope 版；成功后清 pending / pending_full。
    fn md_wy_spawn_async_install_rope(
        tab_idx: usize,
        wy: gpui::Entity<WyEditor>,
        rope: Rope,
        seq: usize,
        cx: &mut Context<Self>,
    ) {
        let wy_install = wy;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    markdown_wysiwyg::WyEditor::open_from_rope(rope)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.tabs.get(tab_idx).is_some_and(|t| {
                    t.md_wy.as_ref() == Some(&wy_install) && t.md_wy_install.seq == seq
                }) {
                    wy_install.update(cx, |ed, _| ed.install_open(result));
                    if let Some(tab) = this.tabs.get_mut(tab_idx) {
                        tab.md_wy_install.pending = None;
                        tab.md_wy_install.pending_full = None;
                    }
                    cx.notify();
                } else if let Some(tab) = this.tabs.get_mut(tab_idx)
                    && tab.md_wy_install.pending_full.as_ref().is_some_and(|(s, _)| *s == seq)
                {
                    // 守卫不通过（切走/关闭/换代）→ 若仍挂本代号则清悬空标记；已换新代号保留。
                    tab.md_wy_install.pending_full = None;
                }
            });
        })
        .detach();
    }

    /// B3：启动「右栏 `md_wy` 后台整篇装配」并登记完成回调。**不走 `&mut self`**（仅借用
    /// `cx`），便于 `commit_live` 内仍持 `&mut self.tabs[i]` 借用时调用。整篇 O(n) 解析
    /// （`Doc::open` + 派生标记计数）在 `background_executor`，UI 只做 O(1) 快照 + 完成
    /// 回调里的 `install_open`（对齐 toggle ON 的既有异步通道）。
    ///
    /// 完成回调两层守卫，任一不满足即丢弃结果（不装错、不泄漏）：
    ///   1) **tab 归属**：该 tab 的 `md_wy` 仍指向 `wy`（未关闭 / 未重建实体 / 未切走）；
    ///   2) **装配代号**：`md_wy_install_seq == seq`（期间又有新装配 / 同步喂源 / 换源 →
    ///      旧装配作废，不装陈旧整篇）。
    ///
    /// 满足才 `install_open` 并清 pending。
    fn md_wy_spawn_async_install(
        tab_idx: usize,
        wy: gpui::Entity<WyEditor>,
        content: SharedString,
        seq: usize,
        cx: &mut Context<Self>,
    ) {
        let wy_install = wy;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    markdown_wysiwyg::WyEditor::open_from_str(content)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.tabs.get(tab_idx).is_some_and(|t| {
                    t.md_wy.as_ref() == Some(&wy_install) && t.md_wy_install.seq == seq
                }) {
                    wy_install.update(cx, |ed, _| ed.install_open(result));
                    if let Some(tab) = this.tabs.get_mut(tab_idx) {
                        tab.md_wy_install.pending = None;
                        // Step3：异步整篇落地 → 预览已 == 目标，解除在途标记（增量桥恢复）。
                        tab.md_wy_install.pending_full = None;
                    }
                    cx.notify();
                } else if let Some(tab) = this.tabs.get_mut(tab_idx)
                    && (tab.md_wy_install.pending.as_ref().is_some_and(|(s, _)| *s == seq)
                        || tab.md_wy_install.pending_full.as_ref().is_some_and(|(s, _)| *s == seq))
                {
                    // 守卫不通过（切走/关闭/更新换代后结果被丢弃）：若 pending / pending_full
                    // 仍挂本代号（期间无更新装配）则清掉悬空标记；已换新代号则保留新标记。
                    tab.md_wy_install.pending = None;
                    tab.md_wy_install.pending_full = None;
                }
            });
        })
        .detach();
    }

    /// 「预览编辑测试」开关：用本项目 `markdown_wysiwyg::WyEditor` 渲染左面板。
    pub(super) fn toggle_live_test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let idx = self.active_tab;
        if !self.is_markdown_tab(idx) {
            self.status = SharedString::from("预览编辑测试仅支持 Markdown 文件");
            self.status_ok = false;
            cx.notify();
            return;
        }
        if self.tabs[idx].live2.is_some() {
            self.commit_live(idx, window, cx);
        } else {
            // 源快照：O(1) COW Rope。大文档走**后台解析**（与预览的后台 scan_blocks 同构）：
            // UI 线程只做 O(1) 快照 + 空壳编辑器创建 + 完成装配；整篇 O(n) 解析
            // （`Doc::open` 6.4MB 实测 ~200ms）在 `background_executor`。小文档保持同步
            // （无「空白→内容」闪烁）。加载完成前 `is_loaded()==false`，宿主读写文本
            // （保存/退出）回退源码编辑区（`tab_effective_text` / `commit_live` 已守卫）。
            const ASYNC_LOAD_MIN_BYTES: usize = 512 * 1024;
            let rope = self.tabs[idx].editor.read(cx).text().clone();
            let async_load = rope.len() >= ASYNC_LOAD_MIN_BYTES;
            // —— 共享复用右栏已解析 Doc（消双解析、O(1) 即见）——
            // 守卫沿增量桥不变式：`md_wy` 已 loaded（且与编辑区字节等长 → 内容一致）、无
            // 整篇在途（`md_wy_pending`/`md_wy_pending_full` 为 None、`md_wy_early` 非
            // Pending|Failed）。命中 → O(1) 克隆共享右栏已解析结构（`Doc::clone`），后台只
            // 补算首帧预建（~36ms release 的 Pass1 输入）即装配——不再 `open_from_rope`
            // 整篇重解析（原大文档切 live 白屏比解析还久的根因之一）。未命中 → 保留既有
            // 异步 `open_from_rope` 兜底。小文档（`!async_load`）不蹭复用：同步 `with_text`
            // 本就无空白帧、零风险不变行为。
            let reuse = self
                .tabs[idx]
                .md_wy
                .as_ref()
                .filter(|w| {
                    async_load
                        && w.read(cx).is_loaded()
                        && self.tabs[idx].md_wy_install.pending.is_none()
                        && self.tabs[idx].md_wy_install.pending_full.is_none()
                        && !matches!(
                            self.tabs[idx].md_wy_install.early,
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
            #[cfg(test)]
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
            let fs = self.font_size;
            ed.update(cx, |ed, _| ed.set_font_size(gpui::px(fs)));
            // 图片相对路径基于文件所在目录解析（与预览一致）
            let base_dir = self.tabs[idx]
                .file_path
                .as_ref()
                .and_then(|p| p.parent())
                .map(|d| d.to_path_buf());
            ed.update(cx, |ed, _| ed.set_base_dir(base_dir));
            // 开启编辑日志收集（方案2）：仅本 live2 实例缴存 op（右栏 `md_wy` 等不开启）。
            // 大文档后台加载的 `install_open` 会清空加载窗口内的陈旧 op（收集开关保留）。
            ed.update(cx, |ed, _| ed.capture_live_ops());
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
                            // 本 live 会话有内容编辑（仅当事件来自 live2 实体——订阅
                            // 只挂在 live2、且上方已按 `live2 == emitter` 过滤，md_wy 右栏
                            // 的 Changed 到不了这里）：切回时据此决定是否整篇写回。
                            tab.live2_dirty = true;
                            if !tab.dirty {
                                tab.dirty = true;
                            }
                            cx.notify();
                            // WYSIWYG 内容变更 → 防抖持久化（结构变化则即时，见 session 模块）。
                            this.schedule_session_flush(cx, false);
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
                cx.spawn(async move |this, cx| {
                    let t0 = std::time::Instant::now();
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
                    crate::prof::add_absolute("wy.app.reuse_prebuild", t0.elapsed());
                    let _ = this.update(cx, |this, cx| {
                        // 只装配仍挂在原 tab 的 live2（用户在装配期间切换/退出则丢弃）。
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
            } else if async_load {
                let tab_idx = idx;
                let ed_install = ed.clone();
                cx.spawn(async move |this, cx| {
                    let t0 = std::time::Instant::now();
                    let result =
                        cx.background_executor()
                            .spawn(async move {
                                markdown_wysiwyg::WyEditor::open_from_rope(rope)
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

// ---------------------------------------------------------------------------
// 文件分类（模块级单一真值）：`classify` 把「扩展名→类型」与「语言 id / 状态栏
// label」三张手写表合为一处；`is_*` 判定与 `language_id`/`language_label` 全部改调它。
// ---------------------------------------------------------------------------

/// 文件类型分类。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum PathKind {
    Markdown,
    Epub,
    Pdf,
    Image,
    Video,
    /// 代码 / 语言文件（语法高亮 id + 状态栏 label 内联，见 [`Self::language_id`]）。
    Code { language_id: &'static str, language_label: &'static str },
    /// 纯文本（`.txt`）。
    Text,
    /// 未知扩展名。
    Other,
}

impl PathKind {
    /// 语法高亮语言 id（未知扩展名回退 "text"）。
    pub(super) fn language_id(self) -> &'static str {
        match self {
            PathKind::Markdown => "markdown",
            PathKind::Code { language_id, .. } => language_id,
            _ => "text",
        }
    }

    /// 状态栏语言标签（未知扩展名回退 "文本"）。
    pub(super) fn language_label(self) -> &'static str {
        match self {
            PathKind::Markdown => "Markdown",
            PathKind::Epub => "EPUB",
            PathKind::Pdf => "PDF",
            PathKind::Image => "图片",
            PathKind::Code { language_label, .. } => language_label,
            PathKind::Text => "纯文本",
            _ => "文本",
        }
    }
}

/// 按扩展名分类文件（大小写不敏感）。图片/视频检测委托各自查看器白名单
/// （`image_viewer::IMAGE_EXTENSIONS` / `video::VIDEO_EXTENSIONS`，那里仍是是否存在
/// 该媒体的唯一来源；本表只依据结果给 [`PathKind`] 与 label）。
pub(super) fn classify(p: &Path) -> PathKind {
    let ext = p
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "md" | "markdown" => PathKind::Markdown,
        "epub" => PathKind::Epub,
        "pdf" => PathKind::Pdf,
        "txt" => PathKind::Text,
        _ if image_viewer::is_image_extension(p) => PathKind::Image,
        #[cfg(feature = "video")]
        _ if video::is_video_extension(p) => PathKind::Video,
        "rs" => PathKind::Code { language_id: "rust", language_label: "Rust" },
        "toml" => PathKind::Code { language_id: "toml", language_label: "TOML" },
        "json" => PathKind::Code { language_id: "json", language_label: "JSON" },
        "py" => PathKind::Code { language_id: "python", language_label: "Python" },
        "js" | "jsx" => PathKind::Code { language_id: "javascript", language_label: "JavaScript" },
        "ts" | "tsx" => PathKind::Code { language_id: "typescript", language_label: "JavaScript" },
        "html" | "htm" => PathKind::Code { language_id: "html", language_label: "HTML" },
        "css" | "scss" => PathKind::Code { language_id: "css", language_label: "CSS" },
        "c" | "h" => PathKind::Code { language_id: "c", language_label: "C" },
        "cpp" | "cc" | "hpp" => PathKind::Code { language_id: "cpp", language_label: "C++" },
        "go" => PathKind::Code { language_id: "go", language_label: "Go" },
        "java" => PathKind::Code { language_id: "java", language_label: "Java" },
        "sh" | "bash" => PathKind::Code { language_id: "bash", language_label: "Shell" },
        "yaml" | "yml" => PathKind::Code { language_id: "yaml", language_label: "YAML" },
        "xml" => PathKind::Code { language_id: "text", language_label: "XML" },
        "sql" => PathKind::Code { language_id: "sql", language_label: "SQL" },
        _ => PathKind::Other,
    }
}
