//! file_ops.

use super::*;
use ui::notification::{Notification, NotificationKind};

use crate::app::open_progress::TextChunk;
use config::limits::{EDITOR_WRAP_MAX_LINES, OPEN_ASYNC_MIN_BYTES, RECENT_MAX};
use config::timing::OPEN_FAILURE_AUTOHIDE_SECS;

/// [`EditorApp::apply_open_chunk`] 的结果：一块被如何处理（消费者据此决定继续/中止）。
enum ChunkApply {
    /// 首块装配成功，携带「打开瞬间的编辑版本号」（收尾的未编辑守卫）。
    Mounted(u64),
    /// 后续块追加成功（继续收下一块）。
    Appended,
    /// 目标选项卡已不存在 / 下标被换成其它文件（中止本打开流水线）。
    Stale,
}

/// 自包含媒体阅读器类型（PDF / 图片 / 视频）——[`EditorApp::load_media`] 的装配参数；
/// 解析/解码各由对应查看器在后台完成，窗口不阻塞。`pub(super)`（app 内，含测试）可见。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum MediaKind {
    /// PDF 阅读器（`PdfViewer`）：整页原样 + 侧边栏全文检索 + 页码。
    Pdf,
    /// 图片查看器（`ImageViewer`）：整图展示 + 同文件夹切换 + Ctrl+滚轮缩放。
    Image,
    /// 视频查看器（`VideoViewer`）：M1 首帧静态阅读 + 帧钟/控制键。
    #[cfg(feature = "video")]
    Video,
}

/// 测试探针（`cfg(test)`）：大文档渐进打开消费者的「统一 notify」次数（= 批数）与
/// 收到/应用的块消息数。P1b 回归断言「批级装配确实把 per-chunk 重绘降为 per-batch」，
/// 且块消息无一丢失（两者相等则每批只收到 1 块的极端时序下也不少于块数）。
#[cfg(test)]
pub(crate) static PROGRESSIVE_OPEN_NOTIFIES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
#[cfg(test)]
pub(crate) static PROGRESSIVE_OPEN_CHUNKS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
/// 测试探针（`cfg(test)`）：收到/应用的**进度消息**（`OpenMsg::Progress`）数。P2 回归
/// 断言「状态栏行/字数随读随跳」：进度消息数 ≥ 1（内容消息仍只有首块 1 个），
/// notify = 2（首块挂载 + 收尾）+ ticks（每次进度一跳）。
#[cfg(test)]
pub(crate) static PROGRESSIVE_OPEN_TICKS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// 生产者 → 消费者的消息（大文档分块打开的异步通道）。
enum OpenMsg {
    /// 仅首块（即位挂载：内容瞬时可见）；其余块不再逐块上屏。
    Chunk(TextChunk),
    /// 后续块的后台字节级统计（不携内容，主线程 O(1) 更新）：状态栏行/字数
    /// **随读随跳**——不再是「挂载小计数 → 整篇换装瞬间跳到终值」（P2）。
    Progress {
        /// 已读完整行的累计行数（`hist` 求和，O(块)）。
        lines: usize,
        /// 已读完整行的累计字符数（`TextChunk::chars` 累积，Unicode 标量）。
        chars: usize,
    },
    /// 生产端在后台把整篇归一化文本建成 Rope + 汇总统计，EOF 一次性交给主线程换装
    /// （消除主线程逐块 `Rope::insert` 的 4.5s 冻结）。
    Eof {
        src_len: u64,
        full: ropey::Rope,
        hist: editor::buffer::LineWidthHistogram,
        max_line_chars: usize,
        /// 本文件探测出的源文本编码（None = 纯 UTF-8），消费端写回选项卡，保存按
        /// 此编码回写（见 [`crate::app::encoding`]）。
        src_encoding: Option<&'static encoding_rs::Encoding>,
    },
    Err(std::io::Error),
}

/// 后台分块读盘 + 归一化 + 逐块统计的生产者：`send().await` 背压外推（读满让出），
/// EOF / 出错 / 消费端放弃即收尾关闭通道。
///
/// **P1（消除主线程逐块追加冻结）**：只把**首块**推给主线程作即时挂载（内容瞬时可见）；
/// 其余块全部在后台攒进整篇 String + 汇总统计，EOF 时后台 `Rope::from_str` 建成整篇
/// rope，一次性交回主线程换装（O(1)）。主线程不再逐块 `Rope::insert`（128MB/312万行
/// debug 下每 1MB ~35ms → 免掉 4.5s 冻结）。
async fn read_chunks_async(
    mut prod_tx: futures::channel::mpsc::Sender<OpenMsg>,
    prod_p: std::path::PathBuf,
) {
    use futures::SinkExt as _;
    let t = std::time::Instant::now();
    let mut reader = match crate::app::open_progress::ChunkReader::open(&prod_p) {
        Ok(r) => r,
        Err(e) => {
            let _ = prod_tx.send(OpenMsg::Err(e)).await;
            prod_tx.close_channel();
            return;
        }
    };
    let mut full: String = String::new();
    let mut cum_hist = editor::buffer::LineWidthHistogram::new();
    let mut cum_mx = 0usize;
    let mut cum_lines = 0usize;
    let mut cum_chars = 0usize;
    let mut first_sent = false;
    loop {
        match reader.next_chunk() {
            Ok(Some(chunk)) => {
                // 逐块已在后台算好精确 hist/max → 汇总（EOF 换装用）。
                cum_mx = cum_mx.max(chunk.max_line_chars);
                for (w, c) in chunk.hist.iter() {
                    *cum_hist.entry(*w).or_insert(0) += c;
                }
                // 累计行/字数（字节级统计路径，O(1)/行）——状态栏「数字跳动」用。
                cum_lines += chunk.hist.values().sum::<usize>();
                cum_chars += chunk.chars;
                // 整篇（含首块）都在后台攒进 String —— 首块同时推送主线程作即时挂载，
                // 其余块只进 `full` 不再上主线程逐块 `Rope::insert`（→ 不冻结）。
                full.push_str(&chunk.text);
                if !first_sent {
                    if prod_tx.send(OpenMsg::Chunk(chunk)).await.is_err() {
                        return; // 消费端已放弃 → 停止读盘
                    }
                    first_sent = true;
                } else {
                    // 状态栏进度：每块 O(1) 推一次累计行/字（不携内容、不逐块上屏，
                    // 主线程只 O(1) 更新计数 + notify，不重绘整篇）。
                    if prod_tx
                        .send(OpenMsg::Progress {
                            lines: cum_lines,
                            chars: cum_chars,
                        })
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            }
            Ok(None) => {
                if std::env::var_os("MD_OPEN_DEBUG").is_some() {
                    eprintln!(
                        "[MD_OPEN_DEBUG] 后台 分块读盘+归一化+逐块统计 合计: {} ms",
                        t.elapsed().as_millis()
                    );
                }
                let t_rope = std::time::Instant::now();
                let rope = ropey::Rope::from_str(&full);
                drop(full);
                if std::env::var_os("MD_OPEN_DEBUG").is_some() {
                    eprintln!(
                        "[MD_OPEN_DEBUG] 后台 Rope::from_str(整篇) 合计: {} ms",
                        t_rope.elapsed().as_millis()
                    );
                }
                let _ = prod_tx
                    .send(OpenMsg::Eof {
                        src_len: reader.total_bytes(),
                        full: rope,
                        hist: cum_hist,
                        max_line_chars: cum_mx,
                        src_encoding: reader.source_encoding(),
                    })
                    .await;
                prod_tx.close_channel();
                return;
            }
            Err(e) => {
                let _ = prod_tx.send(OpenMsg::Err(e)).await;
                prod_tx.close_channel();
                return;
            }
        }
    }
}

/// 首块挂载的行数提示（`str::lines()` 口径）。首块可能不含完整文件，这只是过渡值，
/// 后续块追加后 [`CodeEditor::append_mounted_chunk`] 会经 `display_line_count()` 校正。
fn prep_first_lines_hint(text: &str) -> usize {
    text.lines().count().max(1)
}

/// 打开失败通知的 id 类型（配合唯一 key，便于 5 秒后精确移除）。
struct OpenFailureNotification;

impl EditorApp {
    /// 打开失败：状态栏置为失败，并弹出可点击关闭、5 秒后自动消失的通知。
    /// 同时**解锁**该路径选项卡的「加载期锁输入」：大文档分块流水线若在首块已挂载、
    /// EOF 之前读盘出错，`loading_lock` 只在 `finish_file_text` 复位——若失败路径不解锁，
    /// 选项卡将永久停在「已显示首块但不可编辑/滚动」的锁状态（评审 BLOCK-1）。
    /// 失败语义 = 保留已读到的部分内容、可继续查看/编辑（与旧同步失败的「不建 tab」
    /// 不同，这是异步路径的既有取舍）。
    fn report_open_failure(
        &mut self,
        p: &std::path::Path,
        e: &std::io::Error,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for tab in self.tabs.iter_mut() {
            if tab.file_path.as_deref() == Some(p) {
                tab.loading_lock = false;
            }
        }
        self.status = SharedString::from(format!("打开失败：{e}"));
        self.status_ok = false;
        // 自托管计时移除（而非依赖通知层按窗口激活状态倒计时），保证必然消失。
        let key = gpui::ElementId::from(SharedString::from(format!(
            "open-fail-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        )));
        self.push_notification(
            Notification::new()
                .id1::<OpenFailureNotification>(key.clone())
                .title("打开失败")
                .message(format!("无法读取文件内容：{}\n{e}", p.display()))
                .with_type(NotificationKind::Error)
                .autohide(false),
        );
        let entity = cx.entity();
        window
            .spawn(cx, async move |cx| {
                use gpui::VisualContext as _;
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(OPEN_FAILURE_AUTOHIDE_SECS))
                    .await;
                let _ = cx.update_window_entity(&entity, |app, _window, _cx| {
                    app.remove_notification1::<OpenFailureNotification>(key)
                });
            })
            .detach();
        cx.notify();
    }

    /// 推送一条通知（App 自持容器；调用方自行 `cx.notify()`）。
    pub(crate) fn push_notification(&mut self, note: Notification) {
        self.notifications.push(note);
    }

    /// 按「类型 + key」精确移除一条通知（供卡片关闭与自托管计时共用）。
    pub(crate) fn remove_notification1<T: 'static>(&mut self, key: gpui::ElementId) {
        let id_type = std::any::TypeId::of::<T>();
        self.notifications
            .retain(|n| !(n.id_type() == id_type && n.key() == &key));
    }

    /// 记录最近打开的文件：去重置顶 + 截 [`RECENT_MAX`] + 立即持久化。
    /// 「打开最近」副作用集中入口（K10/K11：一次调用内完成「列表更新 + 落盘」，
    /// 消费点只调它，不散点直调 persist）。
    pub(super) fn record_recent_file(&mut self, path: &std::path::Path) {
        Self::push_recent(&mut self.recent_files, &path.to_string_lossy());
        self.persist_prefs();
    }

    /// 记录最近打开的文件夹：去重置顶 + 截 [`RECENT_MAX`] + 立即持久化
    /// （同 [`Self::record_recent_file`] 的副作用集中入口）。
    pub(super) fn record_recent_folder(&mut self, path: &std::path::Path) {
        Self::push_recent(&mut self.recent_folders, &path.to_string_lossy());
        self.persist_prefs();
    }

    /// 从最近文件列表移除失效路径（点击失效项 → 命中同步路径外移除 + 落盘）：
    /// 状态栏提示，不尝试打开。
    pub(super) fn remove_recent_file(
        &mut self,
        path: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.recent_files.retain(|p| p != path);
        self.persist_prefs();
        self.status = SharedString::from(format!("{path} 不存在，已从最近列表移除"));
        self.status_ok = false;
        cx.notify();
    }

    /// 从最近文件夹列表移除失效路径（同 [`Self::remove_recent_file`]）。
    pub(super) fn remove_recent_folder(
        &mut self,
        path: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.recent_folders.retain(|p| p != path);
        self.persist_prefs();
        self.status = SharedString::from(format!("{path} 不存在，已从最近列表移除"));
        self.status_ok = false;
        cx.notify();
    }

    /// 去重置顶 + 截断（每类 ≤[`RECENT_MAX`]）：相同路径先移除旧位再插到最前。
    fn push_recent(list: &mut Vec<String>, path: &str) {
        if let Some(i) = list.iter().position(|p| p == path) {
            list.remove(i);
        }
        list.insert(0, path.to_string());
        list.truncate(RECENT_MAX);
    }

    /// 把已读取并预处理好的 UTF-8 文本载入指定选项卡，并刷新文件信息与状态。
    ///
    /// `prep`：读盘→归一化→Rope→行宽统计 的产物。同步小文档在 UI 线程现算
    /// （`defer_preview`=false，预览直接全量解析）；大文档由
    /// [`Self::spawn_background_open`] 走**两段式**：先快速挂载编辑器
    /// （`defer_preview`=true，统计为占位、预览延迟、`editor_pending`=false），
    /// **内容瞬时可见**；行宽统计与预览索引由后台完成后 `finish_file_text` O(1) 补齐。
    fn load_file_text(
        &mut self,
        idx: usize,
        p: &std::path::Path,
        prep: PreparedText,
        window: &mut Window,
        cx: &mut Context<Self>,
        defer_preview: bool,
    ) {
        self.search_scan = None; // 同一选项卡内容被替换 → 搜索缓存失效
        let Some(tab) = self.tabs.get_mut(idx) else {
            return;
        };
        let lang = Self::language_id(p);
        let lines = prep.lines;
        let chars = prep.chars;
        let src_len = prep.src_len;
        if defer_preview {
            self.open_fast_count += 1; // 大文档两段式打开：快段（内容瞬时挂载）
        }
        // Markdown 文件编辑区默认软换行（其他文件默认不换行，见 make_tab 的 wrap_enabled）
        let is_md = Self::is_markdown_path(p);
        tab.editor.update(cx, |ed, _cx| {
            // O(1) 装配：行宽统计在后台算好（快速挂载时为占位，finish 再补真值），
            // 不再 UI 线程 recompute_max_line（≈350ms）。
            // 大文档首块（defer_preview=true）：延后折叠/括号扫描到下一帧后台，
            // 首屏零同步重扫（首块 6561 行全行扫描 ≈146ms 挪出首帧临界路径）。
            if defer_preview {
                ed.set_rope_with_stats_defer(
                    prep.rope,
                    prep.max_line_chars,
                    prep.line_width_histogram,
                );
            } else {
                ed.set_rope_with_stats(prep.rope, prep.max_line_chars, prep.line_width_histogram);
            }
            if is_md && lines <= EDITOR_WRAP_MAX_LINES {
                // 自研编辑器尚不支持软换行（占位，API 一致；大文档无 DisplayMap 布局问题）
                ed.set_soft_wrap(true);
            }
            ed.set_language(Some(lang));
        });
        // 编辑器即刻挂载（大文档不再是「预览优先」：内容瞬时可见优先于预览/滚动条精度）。
        tab.editor_pending = false;
        // P4-2：MD 预览统一走 WyEditor（`ensure_md_wy` 下方喂源）——不再为
        // MarkdownPreview 喂源（该后端已删除，M5-B5 P4-2）。txt 无预览，不建。
        tab.file_path = Some(p.to_path_buf());
        // 普通文本/Markdown 选项卡：正文以编辑器 Rope 为准，不再整份镜像到
        // `content`（省一份全文副本，6MB 文件即省 6MB/卡）。EPUB/PDF 这类
        // `readonly_preview`（编辑区为空）才把正文存进 content。
        tab.content = SharedString::default();
        tab.line_count = lines;
        tab.char_count = chars;
        tab.cursor_row = 0;
        tab.cursor_col = 0;
        tab.dirty = false;
        // 运行时内存诊断：`MD_MEM_DEBUG=1 cargo run` 打开文件时输出各份文本副本
        // （定位"同样内容，.md 比 .txt 内存高"到底高在哪；预览解析驻留收进统一组件，
        // 用其内部 Doc 尺寸近似，不再单独读 MarkdownPreview——后端已删）。
        if std::env::var_os("MD_MEM_DEBUG").is_some() {
            let content_len = tab.content.len();
            let editor_len = tab.editor.read(cx).value().len();
            let wy_len = tab
                .md_wy
                .as_ref()
                .map(|w| w.read(cx).len())
                .unwrap_or(0);
            eprintln!(
                "[MD_MEM_DEBUG] 打开 {:?}: 源文件 {} B / {} 行 / {} 字符 → 副本 content={} B + editor={} B + WyEditor Doc={} B",
                p.file_name(),
                src_len,
                lines,
                chars,
                content_len,
                editor_len,
                wy_len,
            );
        }
        if is_md {
            tab.preview_on = true;
        }
        self.status = SharedString::from(if is_md && lines > EDITOR_WRAP_MAX_LINES {
            format!("已打开：{} · 大文档编辑区已关闭软换行（Ctrl+W 可开，预览保持换行）", p.display())
        } else {
            format!("已打开：{}", p.display())
        });
        self.status_ok = true;
        // A2：MD 选项卡右栏预览统一组件（WyEditor 只读）——创建并喂源。
        //  - 同步小文档：就地 `set_source_text`（小，UI 可承受，与 md_preview.set_source 同）；
        //  - 大文档快速挂载（defer=true）：只建空壳，整篇 `Doc::open` 由后台
        //    `spawn_progressive_open` 算好、`finish_file_text` 里 `install_open`（不占 UI）。
        if is_md {
            self.ensure_md_wy(idx, window, cx, !defer_preview);
        }
        // 文件路径就位（make_tab 时机为未命名）→ 立即持久化记成 File 型。
        self.schedule_session_flush(cx, true);
        cx.notify();
    }

    /// 把文本灌入**已建好**的选项卡（会话恢复的未命名选项卡用）：与
    /// [`Self::load_file_text`] 走同一条 O(n) 装配管线（`prepare_text` → Rope +
    /// 行宽统计 + 行/字计数），但不设 `file_path`（保持「未命名」语义）、不建
    /// Markdown 预览（未命名选项卡 = 纯文本）。`make_tab` 已建空编辑器。
    pub(super) fn set_tab_text(
        &mut self,
        idx: usize,
        text: String,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if idx >= self.tabs.len() {
            return;
        }
        self.search_scan = None; // 同一选项卡内容被替换 → 搜索缓存失效
        let PreparedText {
            rope,
            lines,
            chars,
            max_line_chars,
            line_width_histogram,
            src_len: _,
        } = prepare_text(text);
        let Some(tab) = self.tabs.get_mut(idx) else { return };
        tab.editor.update(cx, |ed, _cx| {
            // O(1) 装配：与 load_file_text 同款（Rope 共享、统计就地算好）。
            ed.set_rope_with_stats(rope, max_line_chars, line_width_histogram);
            // 未命名选项卡无文件 → 不设语言（保持纯文本高亮缺省）。
            ed.set_language(None);
        });
        tab.editor_pending = false;
        // 未命名选项卡正文以编辑器 Rope 为准，不整份镜像到 content（同 load_file_text）。
        tab.content = SharedString::default();
        tab.line_count = lines;
        tab.char_count = chars;
        tab.cursor_row = 0;
        tab.cursor_col = 0;
        tab.dirty = false;
        cx.notify();
    }

    /// 大文档「先瞬时显示、后台补齐」的收尾段：**P1 起整篇 rope 由生产端在后台建好，
    /// 此处 `set_rope_with_stats` 一次性换装**（O(1)，替换首块的部分 rope；主线程不再
    /// 逐块追加，故打开全程不冻结）。守卫：选项卡仍存、仍是同一文件、且**打开后未编辑**
    /// （已编辑则跳过，让增量同步/编辑自身维护）。换装后 O(1) 修正统计，并给 Markdown
    /// 预览装上预建索引（登记增量基底，后续编辑即可走增量扫描）。
    #[allow(clippy::too_many_arguments)]
    fn finish_file_text(
        &mut self,
        idx: usize,
        p: &std::path::Path,
        full: Option<ropey::Rope>,
        max_line_chars: usize,
        line_width_histogram: editor::buffer::LineWidthHistogram,
        md_wy_open: Option<markdown_wysiwyg::WyOpenResult>,
        resume_version: Option<u64>,
        cx: &mut Context<Self>,
    ) {
        // B3：文件换源会同步改写右栏 md_wy（install_open）→ 先作废任何在途后台装配
        //（否则旧切回装配在文件打开完成后把新打开的整篇覆盖回去）。仅 markdown 才喂
        // md_wy；此处提前作废不影响「本次 install_open」——它走同步、代号已换代。
        // Step2：**打开即提前整篇解析**（`md_wy_early` Pending/Done）在途 → EOF 收尾让位：
        // 首块挂载时 `ensure_md_wy` 已作废过陈旧在途，此处**不再 invalidate**（否则会把
        // 在途的早期解析丢弃，白做一次整篇 Doc::open），本次 EOF 也不重复 install（见下）。
        let early_live = Self::is_markdown_path(p)
            && self
                .tabs
                .get(idx)
                .is_some_and(|t| {
                    matches!(
                        t.md_wy_install.early,
                        Some(MdWyEarly::Pending { .. }) | Some(MdWyEarly::Done)
                    )
                });
        if Self::is_markdown_path(p) && !early_live {
            self.md_wy_invalidate_async(idx);
        }
        let Some(tab) = self.tabs.get_mut(idx) else {
            return;
        };
        if tab.file_path.as_deref() != Some(p) {
            return; // 选项卡已被换成其它文件
        }
        self.open_finish_count += 1; // 大文档两段式打开：慢段（整篇换装 + 预览索引补齐）
        if resume_version.is_some_and(|v| tab.edit_version != v) {
            // 打开后已编辑（输入锁下本不应发生，防御）：不换装，保留用户编辑，解锁。
            tab.loading_lock = false;
            return;
        }
        if let Some(full) = full {
            // P1 换装：整篇 rope + 统计一次装入（O(1)；取代逐块追加）。换装后行/字计数
            // 跟着整篇统计走（与 `load_file_text` 同款更新）。
            // P3：用 `set_rope_with_stats_defer`（延后折叠候选/括号配对扫描）——否则
            // `resync_list(false)` 在 UI 线程整篇同步重扫折叠候选＋括号配对：≤20 万行的
            // 中等大文档（如 6.1MB/15.6 万行）恰好触发全量，debug 实测 ~3.4s 主线程卡顿
            // （`大文档测试.txt` 的「会卡」即此）。延后后同帧由打开收尾的
            // `refresh_fold_brackets` 在后台补齐同一份结果，UI 只留 O(1) 换装。
            // P4：用 `set_rope_with_stats_keep_highlight` 保留 Buffer id + 高亮缓存——
            // 换装后顶部窗口与挂载首块逐位相同，`tail_append` A0 门控跨版本复用缓存，
            // 大 `.rs` 整窗重解析（debug ~620ms）不再发生（「打开 .rs 会卡一下」根因）。
            let tail_row = tab.editor.read(cx).line_count(); // 挂载首块行数 = 新内容起点
            tab.editor.update(cx, |ed, _| {
                ed.set_rope_with_stats_keep_highlight(
                    full,
                    max_line_chars,
                    line_width_histogram,
                    tail_row,
                );
            });
            tab.line_count = tab.editor.read(cx).display_line_count();
            tab.char_count = tab.editor.read(cx).char_count();
        } else {
            // 无整篇（异常兜底）：统计就位（内容已是首块）。
            tab.editor.update(cx, |ed, _| {
                ed.apply_stats(max_line_chars, line_width_histogram);
            });
        }
        tab.loading_lock = false; // 整篇就绪：解锁编辑；同帧 status 由消费者置「已打开」。
        // A4 脱离：MD 不再喂 MarkdownPreview / 登记增量基底（预览统一走 md_wy）——
        // 后台预建的 `WyOpenResult` 一次性 `install_open` 装配右栏 WyEditor 预览
        // （UI 线程只做 O(块) 换源重置，不背 Doc::open）。
        // Step2：早期解析在途/已装（early_live）→ 本次 EOF **跳过重复 install**
        //（`md_wy_open` 已按早装让位传 None；早期完成后自会 install）。
        if Self::is_markdown_path(p)
            && tab.preview_on
            && !early_live
            && let Some(ed) = tab.md_wy.clone()
            && let Some(wy) = md_wy_open {
                ed.update(cx, |ed, _| ed.install_open(wy));
                // Step3：EOF 兜底整篇装配落地 → 整篇已装，解除「整篇在途」标记。
                tab.md_wy_install.pending_full = None;
            }
        if std::env::var_os("MD_OPEN_DEBUG").is_some() {
            // 诊断“拖到底不到底”：量程就绪后的纵向总高。编辑器精确时应 ≈ 行数×行高；
            // 统一 WyEditor 预览也用确定性量程（`MarkdownPreview` 抽样模型已删，M5-B5
            // P4-2）。值远小于预期 → 对应栏量程坍缩。
            let editor_total = tab.editor.read(cx).vertical_total_px();
            let wy_total = tab
                .md_wy
                .as_ref()
                .map(|w| {
                    w.read(cx)
                        .frame_total()
                        .unwrap_or(0.)
                })
                .unwrap_or(0.);
            eprintln!(
                "[MD_SCROLL] finish 后纵向 total——编辑器={editor_total:.0}px ({} 行), WyEditor={wy_total:.0}px",
                tab.line_count,
            );
        }
        cx.notify();
    }

    /// 大文档「分块读盘 · 渐进装配」打开流水线：
    /// 1. **生产者**（后台线程）：[`open_progress::produce_open_chunks`] 逐块
    ///    `File::read`→UTF-8 校验→CRLF 归一化→按块精确算行宽统计（读多少、发多少，
    ///    不再整篇 `read_to_string` 物化 ~1× 文件字节的 String 临时代理），经 channel
    ///    把每一块推给消费者，首块下限 256KB（几十 KB 即出首屏）、之后 1MB；
    /// 2. **消费者**（逐块到主线程）：首块 [`Self::apply_open_chunk`] 走
    ///    `load_file_text`(defer) 快速挂载（内容瞬时可见）；后续块 O(块大小) 追加，
    ///    滚动/撤销/光标保留，横向量程随块精确补齐；
    /// 3. **收尾**：md 后台 `scan_blocks` 建索引 → `finish_file_text` O(1) 装预览 +
    ///    最终状态。
    ///
    /// `idx` 处须已存在选项卡（由 `open_file` 先 `make_tab` 新建）。
    fn spawn_progressive_open(
        &mut self,
        idx: usize,
        p: std::path::PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.status = SharedString::from(format!("正在打开大文档：{}…", p.display()));
        self.status_ok = true;
        let is_md = Self::is_markdown_path(&p);
        let entity = cx.entity();

        // 异步**有界**通道（容量 4）：消费端 `next()` 空时让出调度，生产端 `send()`
        // 满时也 `await` 让出（背压）——既不物化整篇 String，也不让通道囤积整篇
        // （测试的协作式调度器下，blocking recv 会卡死，故绝不用 std::sync::mpsc）。
        let (tx, rx) = futures::channel::mpsc::channel::<OpenMsg>(4);
        window
            .spawn(cx, async move |cx| {
                use futures::StreamExt as _;
                // ---- 生产者（后台线程；逐块读盘 → `send().await` 背压外推，见 `read_chunks_async`）----
                let prod_tx = tx.clone();
                let prod_p = p.clone();
                cx.background_executor()
                    .spawn(read_chunks_async(prod_tx, prod_p))
                    .detach();

                // ---- 消费者：首块即位挂载（内容瞬时可见）+ 锁输入；EOF 拿整篇 rope 一次换装。
                // P1：不再逐块追加（主线程逐块 `Rope::insert` 128MB/312万行 debug ~4.5s 冻结）——整篇
                // rope 由生产端在后台建好（`Rope::from_str`），本消费端只做首块挂载 + EOF O(1) 换装。
                let t0_prog = std::time::Instant::now();
                let mut full: Option<ropey::Rope> = None;
                let mut full_hist = editor::buffer::LineWidthHistogram::new();
                let mut full_mx = 0usize;
                let mut src_len = 0u64;
                let mut first = true;
                let mut resume_version: Option<u64> = None;
                let mut io_err: Option<std::io::Error> = None;
                let mut aborted = false;
                // 文件名 → 源编码（探测于生产端，EOF 带回，写回选项卡供保存回写）。
                let mut src_enc: Option<&'static encoding_rs::Encoding> = None;
                // WARN-1 节流门：状态字段每块 O(1) 都更新，但 `notify`（→整窗重绘）每
                // 4 条进度才触发一次（gpui 还会按帧合并）——128MB 打开期 ~127 条进度把
                // 重绘从 ~127 次压到 ~32 次，避免无谓帧压力；EOF 换装帧必然 notify，终值不丢。
                let mut progress_gate = 0usize;
                let mut rx = rx;
                'consume: loop {
                    let msg = match rx.next().await {
                        Some(m) => m,
                        None => break 'consume,
                    };
                    match msg {
                        OpenMsg::Err(e) => {
                            io_err = Some(e);
                            break 'consume;
                        }
                        OpenMsg::Progress { lines, chars } => {
                            // P2 状态栏「数字跳动」：O(1) 更新累计行/字（不逐块上屏）。
                            // WARN-1：notify 按 `progress_gate` 节流（每 4 条一跳）。
                            // tab 换成别的文件/已关 → 跳过。
                            let p_prg = p.clone();
                            let do_notify = progress_gate % 4 == 0;
                            progress_gate += 1;
                            let _ = cx.update_window_entity(&entity, move |this, _w, cx| {
                                if let Some(tab) = this.tabs.get_mut(idx).filter(|t| {
                                    t.file_path.as_deref() == Some(p_prg.as_path())
                                }) {
                                    tab.line_count = lines;
                                    tab.char_count = chars;
                                }
                                if do_notify {
                                    cx.notify();
                                }
                            });
                            #[cfg(test)]
                            {
                                PROGRESSIVE_OPEN_TICKS
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                if do_notify {
                                    PROGRESSIVE_OPEN_NOTIFIES
                                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                }
                            }
                        }
                        OpenMsg::Eof {
                            src_len: s,
                            full: r,
                            hist,
                            max_line_chars,
                            src_encoding,
                        } => {
                            src_len = s;
                            full = Some(r);
                            full_hist = hist;
                            full_mx = max_line_chars;
                            src_enc = src_encoding; // 源编码写回选项卡（保存按此回写）
                            break 'consume;
                        }
                        OpenMsg::Chunk(chunk) => {
                            if first {
                                let p_ch = p.clone();
                                let r = cx.update_window_entity(
                                    &entity,
                                    move |this, window, cx| {
                                        this.apply_open_chunk(idx, &p_ch, chunk, true, window, cx)
                                    },
                                );
                                match r {
                                    Ok(ChunkApply::Mounted(v)) => {
                                        if std::env::var_os("MD_OPEN_DEBUG").is_some() {
                                            eprintln!(
                                                "[MD_OPEN_DEBUG] 首块已挂载（内容可显示）@ {:.1} ms",
                                                t0_prog.elapsed().as_secs_f64() * 1000.0
                                            );
                                        }
                                        #[cfg(test)]
                                        {
                                            PROGRESSIVE_OPEN_CHUNKS
                                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                            PROGRESSIVE_OPEN_NOTIFIES
                                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                        }
                                        resume_version = Some(v);
                                        first = false;
                                    }
                                    Ok(ChunkApply::Stale) => {
                                        aborted = true;
                                        break 'consume;
                                    }
                                    Ok(ChunkApply::Appended) | Err(_) => {}
                                }
                            }
                            // 防御：P1 起不再有后续块；有则忽略（整篇在后台，EOF 一次换装）。
                        }
                    }
                }

                // ---- 收尾（主线程任务上下文）----
                if let Some(e) = io_err {
                    let p_err = p.clone();
                    let _ = cx.update_window_entity(&entity, move |this, window, cx| {
                        this.report_open_failure(&p_err, &e, window, cx)
                    });
                    return;
                }
                if aborted {
                    return; // 目标选项卡已不存在/被换成别的文件 → 放弃后续收尾
                }
                if first {
                    // 从未挂过首块（如空文件走漏进来的兜底）：挂一个空文档，
                    // 使 editor_pending 复位、编辑器可正常显示/保存。
                    let p_emp = p.clone();
                    let _ = cx.update_window_entity(&entity, move |this, window, cx| {
                        let prep = prepare_text(String::new());
                        this.load_file_text(idx, &p_emp, prep, window, cx, true);
                    });
                }
                // A4：MD 后台预建右栏 WyEditor 预览的整篇 `Doc::open`（快照 rope O(1)
                // 克隆）——**不再 scan_blocks 喂 MarkdownPreview**（MD 已走统一组件，
                // 省第二次扫块 + 一份 BlockIndex/源副本；本份 Doc 即预览唯一解析态）。
                // Step2：若「打开即提前整篇解析」已在途/已装（Pending/Done）→ 收尾让位
                // 它 install（本处不再快照编辑区 rope、不再重复整篇 Doc::open，避免双装）。
                // Failed / 未启动（空文件等）→ 走既有 EOF 整篇装配兜底。
                let early_live = is_md
                    && cx
                        .update_window_entity(&entity, |this, _w, _cx| {
                            this.tabs.get(idx).is_some_and(|t| {
                                matches!(
                                    t.md_wy_install.early,
                                    Some(MdWyEarly::Pending { .. }) | Some(MdWyEarly::Done)
                                )
                            })
                        })
                        .unwrap_or(false);
                let md_wy_open = if early_live {
                    None
                } else if is_md {
                    // P1：整篇 rope 由生产端在后台建好（`full`），直接快照它（O(1) COW），
                    // 不再快照编辑区的首块部分 rope。
                    let snap = full.clone();
                    let Some(snap) = snap else { return };
                    let t_wy = std::time::Instant::now();
                    let wy_open = cx
                        .background_executor()
                        .spawn(async move {
                            let wy = markdown_wysiwyg::WyEditor::open_from_rope(snap);
                            if std::env::var_os("MD_OPEN_DEBUG").is_some() {
                                eprintln!(
                                    "[MD_OPEN_DEBUG] 后台 右栏WyEditor(Doc::open) 合计: {} ms",
                                    t_wy.elapsed().as_millis()
                                );
                            }
                            wy
                        })
                        .await;
                    Some(wy_open)
                } else {
                    None
                };
                // 最终：整篇换装（O(1)）+ 预览装配 + 恢复「已打开」状态。
                let hist_lines = full_hist.values().sum::<usize>();
                let p_fin = p.clone();
                let _ = cx.update_window_entity(&entity, move |this, window, cx| {
                    // 源编码写回选项卡（保存 `write_to_path` 按此编码回写）。
                    if let Some(tab) = this.tabs.get_mut(idx).filter(|t| {
                        t.file_path.as_deref() == Some(p_fin.as_path())
                    }) {
                        tab.source_encoding = src_enc;
                    }
                    this.finish_file_text(
                        idx,
                        &p_fin,
                        full.take(),
                        full_mx,
                        full_hist,
                        md_wy_open,
                        resume_version,
                        cx,
                    );
                    // 自动化测试钩子：大文档异步打开补齐完成后启动 WYSIWYG 左面板
                    // （`new_impl` 的同步检查跑在 `tab.file_path` 就位之前，这里补齐；
                    // 小文档走 `load_file_text` 同步挂载，由 `new_impl` 直接开启）。
                    if std::env::var_os("WY_AUTOTEST").is_some()
                        && idx == this.active_tab
                        && this.tabs.get(idx).is_some_and(|t| t.live2.is_none())
                    {
                        this.toggle_live_test(window, cx);
                    }
                    // 最终状态：内容就绪，恢复「已打开」（含大文档关闭软换行的提示）。
                    if let Some(tab) = this.tabs.get(idx).filter(|t| t.file_path.as_deref() == Some(p_fin.as_path())) {
                        this.status = SharedString::from(if is_md && tab.line_count > EDITOR_WRAP_MAX_LINES {
                            format!(
                                "已打开：{} · 大文档编辑区已关闭软换行（Ctrl+W 可开，预览保持换行）",
                                p_fin.display()
                            )
                        } else {
                            format!("已打开：{}", p_fin.display())
                        });
                        this.status_ok = true;
                    }
                    // 折叠候选 / 括号标尺：整篇换装后**后台**重扫一次（打开全程不再每块
                    // 整篇重扫 → 打开 160k 行文档从 ~2s UI 卡顿降到毫秒级装配）。
                    if let Some(tab) = this.tabs.get_mut(idx).filter(|t| t.file_path.as_deref() == Some(p_fin.as_path())) {
                        tab.editor.update(cx, |ed, cx| {
                            ed.refresh_fold_brackets(cx);
                        });
                    }
                    #[cfg(test)]
                    PROGRESSIVE_OPEN_NOTIFIES
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    cx.notify(); // 换装 + 解锁同帧重绘
                });
                if std::env::var_os("MD_OPEN_DEBUG").is_some() {
                    eprintln!(
                        "[MD_OPEN_DEBUG] 分块渐进装配完成：{} 原始字节 / 累计 {} 行统计",
                        src_len, hist_lines
                    );
                }
            })
            .detach();
    }

    /// 把后台读到的第 `first` 块（首块）挂上 / 后续块防御忽略。
    ///
    /// - 首块：走 [`Self::load_file_text`](defer=true) 快速挂载（`open_fast_count` +1、
    ///   `editor_pending`=false → **几十 KB 即出内容**），并设 `loading_lock`（长文档
    ///   整篇仍在后台装配：挂载期锁输入 + 透明拦截，避免「打开期间编辑」与换装竞态）；
    ///   返回打开瞬间的编辑版本号作为收尾的「未编辑」守卫；
    /// - P1 起生产端**只发首块**（整篇在后台一次建好、EOF 换装）——非首块分支仅留防御：
    ///   万一收到额外块直接忽略，**绝不在主线程逐块 `Rope::insert`**（实测 128MB/312万行
    ///   debug 每 1MB ~35ms → 4.5s 冻结）。
    fn apply_open_chunk(
        &mut self,
        idx: usize,
        p: &std::path::Path,
        chunk: crate::app::open_progress::TextChunk,
        first: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> ChunkApply {
        if first {
            // 复用既有选项卡重开文件：file_path 此刻仍是旧文件，不拦截（首块即替换内容）。
            let TextChunk { text, hist, max_line_chars, .. } = chunk;
            let rope = Rope::from(text.as_str());
            let lines = prep_first_lines_hint(&text);
            let chars = text.chars().count();
            let src_len = text.len();
            let prep = PreparedText {
                rope,
                lines,
                chars,
                src_len,
                max_line_chars,
                line_width_histogram: hist,
            };
            self.load_file_text(idx, p, prep, window, cx, true);
            // Step2/3：md 大文档「打开即」后台整篇解析（与后续分块追加并行，不再等 EOF
            // 才开跑 Doc::open）——先**首块 prefix 首装**（编辑区 rope O(1) COW 快照 →
            // 后台 `open_from_rope` + install，首屏立刻真内容），再整篇续读/续解析；整篇
            // 就绪后带守卫换入全文。EOF 收尾（`finish_file_text`）据此跳过重复 install。
            if Self::is_markdown_path(p) {
                let prefix = self.tabs[idx].editor.read(cx).rope().clone();
                let full_seq = self.md_wy_spawn_early_install(idx, p.to_path_buf(), cx);
                self.md_wy_spawn_prefix_install(idx, prefix, full_seq, cx);
            }
            // 首块后内容已可见，但整篇仍在读盘 → 保持「正在打开」；锁输入防编辑竞态。
            if let Some(tab) = self.tabs.get_mut(idx) {
                tab.loading_lock = true;
                self.status = SharedString::from(format!("正在打开大文档：{}…", p.display()));
                self.status_ok = true;
                return ChunkApply::Mounted(tab.edit_version);
            }
            return ChunkApply::Stale;
        }
        let Some(tab) = self.tabs.get_mut(idx) else {
            return ChunkApply::Stale;
        };
        if tab.file_path.as_deref() != Some(p) {
            return ChunkApply::Stale; // 该下标已被换成其它文件
        }
        // P1 防御：不应再有后续块；有则忽略（整篇在后台，EOF 一次换装）。
        ChunkApply::Appended
    }

    /// 把 EPUB 解析结果载入指定选项卡：纯展示（只读预览），编辑区置空。
    /// `pub(super)`：装配回归测试（`app::tests`）直接驱动。
    pub(super) fn load_epub(
        &mut self,
        idx: usize,
        p: &std::path::Path,
        doc: epub::EpubDocument,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.search_scan = None; // 同一选项卡内容被替换 → 搜索缓存失效
        let text = doc.markdown;
        let lines = text.lines().count().max(1);
        let chars = text.chars().count();
        // P4-1/P4-2：EPUB 迁 WyEditor 只读宿主（无回退后端，`MarkdownPreview` 已删）。
        // 只读（禁编辑）、标题居中（set_epub）、`epubimg://` 内存图（set_memory_images）、
        // 注记浮卡/引文跳转正文（set_notes）。在 `tab` 可变借用之前调用（需 `&mut self`）。
        self.ensure_epub_wy(idx, _window, cx, &text, &doc.images, &doc.notes);
        // 公共字段统一重置（编辑区置空 + markdown 语言 + 文件路径/正文/计数 + 预览全屏只读）。
        if !self.reset_common_tab_fields(idx, p, SharedString::from(text), lines, chars, cx) {
            return;
        }
        let Some(tab) = self.tabs.get_mut(idx) else {
            return;
        };
        // P4-1：EPUB 内存图随选项卡持有（供 WyEditor 只读宿主注入；随选项卡搬迁）。
        tab.epub_images = doc.images.clone();
        // P4-2：EPUB 注记正文表随选项卡持有（`ensure_epub_wy` → `WyEditor::set_notes`）。
        tab.epub_notes = doc.notes.clone();
        let author = if doc.author.is_empty() {
            String::new()
        } else {
            format!("（{}）", doc.author)
        };
        self.status = SharedString::from(format!(
            "已打开 EPUB：{}{} · {} 章",
            doc.title, author, doc.chapter_count
        ));
        self.status_ok = true;
        cx.notify();
    }

    /// 把一个新建媒体阅读器（PDF / 图片 / 视频）载入指定选项卡：编辑区置空、全屏
    /// 只读预览。三项的**公共装配**（搜索缓存失效 / 画布底色 / 编辑区清空 + markdown
    /// 语言 / 文件路径与计数 / 预览全屏只读 / 状态与通知）统一在此，差异（查看器实体
    /// 创建 + PDF 订阅重绘 / 视频焦点 / 状态文案）按 `kind` 分支。解析/解码各由查看器
    /// 自己在后台完成，窗口不阻塞。
    pub(super) fn load_media(
        &mut self,
        idx: usize,
        p: &std::path::Path,
        kind: MediaKind,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.search_scan = None; // 同一选项卡内容被替换 → 搜索缓存失效
        if matches!(kind, MediaKind::Pdf) {
            self.pdf_search = None; // 新 PDF 阅读器 → 需重新推送查询
        }
        // 阅读画布用灰色系（非白面板），让阅读器与画布形成分隔（PDF / 图片 / 视频同源）
        let canvas_bg = self.window_bg();
        // 公共字段统一重置（编辑区置空 + markdown 语言 + 文件路径/计数 + 预览全屏只读）。
        // 选项卡不存在 → 早退（与旧逐条 load_* 一致：不创建查看器实体）。
        if !self.reset_common_tab_fields(idx, p, SharedString::default(), 0, 0, cx) {
            return;
        }
        if kind == MediaKind::Pdf {
            // PdfViewer：自包含阅读器（异步读文件+解析+页级按需栅格化，不解析为 Markdown）
            let viewer = cx.new(|cx| pdf::PdfViewer::new(p.to_path_buf(), cx));
            viewer.update(cx, |view, _| view.set_background(canvas_bg));
            // 订阅阅读器 notify：检索结果/加载进度到达 → 重绘 app（侧边栏 PDF 命中计数/
            // 页列表更新）。订阅收进**选项卡**（`_pdf_sub`）而非全局单槽：多 PDF 并存各自
            // 独立、不被后打开者覆盖（旧 `_pdf_observe` 单槽即此缺陷）。单一写点
            // `subscribe_pdf`（新建/跨窗口移交共用，K10 收敛）。
            self.subscribe_pdf(idx, &viewer, cx);
            let tab = self.tabs.get_mut(idx).expect("PDF 选项卡应已在场");
            tab.reader = MediaReader::Pdf(viewer);
        } else {
            let Some(tab) = self.tabs.get_mut(idx) else {
                return;
            };
            match kind {
                MediaKind::Image => {
                    // ImageViewer：自包含图片查看器（后台解码 + 同文件夹切换 + Ctrl+滚轮缩放）
                    let viewer = cx.new(|cx| image_viewer::ImageViewer::new(p.to_path_buf(), cx));
                    viewer.update(cx, |view, _| view.set_background(canvas_bg));
                    tab.reader = MediaReader::Image(viewer);
                }
                #[cfg(feature = "video")]
                MediaKind::Video => {
                    // VideoViewer：自包含视频播放器（后台解码 + 帧钟 + 控制）
                    let viewer = cx.new(|cx| video::VideoViewer::new(p.to_path_buf(), cx));
                    viewer.update(cx, |view, _| view.set_background(canvas_bg));
                    // 焦点落到查看器：Space/←/→/滚轮等控制键直达（按键监听挂在查看器焦点句柄上）
                    let fh = viewer.read(cx).focus_handle();
                    _window.focus(&fh, cx);
                    tab.reader = MediaReader::Video(viewer);
                }
                _ => unreachable!(),
            }
        }
        self.status = match kind {
            MediaKind::Pdf => {
                SharedString::from(format!("已打开 PDF：{}（解析中…）", p.display()))
            }
            MediaKind::Image => SharedString::from(format!("已打开图片：{}", p.display())),
            #[cfg(feature = "video")]
            MediaKind::Video => SharedString::from(format!("已打开视频：{}", p.display())),
        };
        self.status_ok = true;
        cx.notify();
    }

    /// 把「只读阅读」选项卡的公共字段统一重置：编辑区置空并切到 markdown 语言、
    /// 文件路径、内容/计数镜像、光标、脏标记与「全屏只读预览」标志（PDF / 图片 /
    /// 视频 / EPUB 共用；媒体走空内容与零计数，EPUB 走真实正文与计数）。
    /// 返回值 = 选项卡是否仍存在（不存在则未做任何写入，调用方应早退）。
    fn reset_common_tab_fields(
        &mut self,
        idx: usize,
        p: &std::path::Path,
        content: SharedString,
        line_count: usize,
        char_count: usize,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(tab) = self.tabs.get_mut(idx) else {
            return false;
        };
        // 编辑区置为提示文本（只读阅读，不参与文本编辑）
        tab.editor.update(cx, |ed, _cx| {
            ed.set_text("");
            ed.set_language(Some("markdown"));
        });
        tab.file_path = Some(p.to_path_buf());
        tab.content = content;
        tab.line_count = line_count;
        tab.char_count = char_count;
        tab.cursor_row = 0;
        tab.cursor_col = 0;
        tab.dirty = false;
        tab.preview_on = true;
        tab.preview_fullscreen = true;
        // 正文只存在于查看器/预览，编辑区为空 → 预览锁定，toggle 不翻转也不显示空编辑区
        tab.readonly_preview = true;
        // 媒体/EPUB 文件路径就位（make_tab 时机为未命名）→ 立即持久化记成 File 型。
        self.schedule_session_flush(cx, true);
        true
    }

    /// 打开 PDF：立即建标签页并创建自包含的 PdfViewer（在后台加载解析），
    /// 窗口保持响应；解析完成/失败由阅读器内部呈现。
    fn open_pdf_async(
        &mut self,
        idx: usize,
        p: std::path::PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.status = SharedString::from(format!("正在打开 PDF：{}…", p.display()));
        self.status_ok = true;
        self.load_media(idx, &p, MediaKind::Pdf, window, cx);
    }

    /// 打开文件：新建一个文件选项卡并在其中载入文件内容，随后把焦点移到新选项卡的编辑区。
    ///
    /// 先确认内容可读/可解析再创建选项卡：读取失败时只给出错误提示，
    /// 不再生成一个内容空白的标签页。
    pub(super) fn open_file(&mut self, path: &str, window: &mut Window, cx: &mut Context<Self>) {
        let p = PathBuf::from(path.trim());
        // 音频文件：整窗切到音乐模式播放（不进文本/媒体选项卡路由）。此拦截在 EPUB/PDF/
        // 图片/视频/文本分类**之前**——覆盖文件树双击、打开对话框、拖放、会话恢复、
        // 最近打开等全部进 open_file 的中心入口（clap 一下）。
        if music::is_audio_extension(&p) {
            self.open_music_path(p, window, cx);
            return;
        }
        // EPUB / PDF：先生成解析结果，失败则仅提示、不建空标签页
        if Self::is_epub_path(&p) {
            match epub::parse_epub_file(&p) {
                Ok(doc) => {
                    let idx = self.make_tab(window, cx);
                    self.load_epub(idx, &p, doc, window, cx);
                    let focus = self.tab_focus_handle(idx, cx);
                    window.focus(&focus, cx);
                    // 打开成功才记录最近（失败早退分支不记录）。
                    self.record_recent_file(&p);
                }
                Err(e) => {
                    self.status = SharedString::from(format!("打开失败：{e}"));
                    self.status_ok = false;
                    cx.notify();
                }
            }
            return;
        }
        if Self::is_pdf_path(&p) {
            // 先建标签页、立即返回：解析放到后台，避免拖窗卡顿
            let idx = self.make_tab(window, cx);
            self.open_pdf_async(idx, p.clone(), window, cx);
            let focus = self.tab_focus_handle(idx, cx);
            window.focus(&focus, cx);
            self.record_recent_file(&p);
            return;
        }
        if Self::is_image_path(&p) {
            // 先建标签页、立即返回：解码放到后台（ImageViewer 自包含）
            let idx = self.make_tab(window, cx);
            self.load_media(idx, &p, MediaKind::Image, window, cx);
            let focus = self.focus_handle(cx);
            window.focus(&focus, cx);
            self.record_recent_file(&p);
            return;
        }
        #[cfg(feature = "video")]
        if Self::is_video_path(&p) {
            // 先建标签页、立即返回：解码放到后台（VideoViewer 自包含）
            let idx = self.make_tab(window, cx);
            self.load_media(idx, &p, MediaKind::Video, window, cx);
            let focus = self.focus_handle(cx);
            window.focus(&focus, cx);
            self.record_recent_file(&p);
            return;
        }
        // 文本：大文件（按字节阈值判断，无需读盘）→ **后台打开**——先建标签页 +
        // “正在打开…”状态立即返回，分块读盘→归一化→按块统计→逐块装配 全在后台线程，
        // UI 每块只做 O(块大小) 追加。拖入瞬间不再卡死 UI，首屏几十 KB 即出内容。
        if is_large_path(&p) {
            let idx = self.make_tab(window, cx);
            self.spawn_progressive_open(idx, p.clone(), window, cx);
            let focus = self.tab_focus_handle(idx, cx);
            window.focus(&focus, cx);
            // 大文件分支「建 tab 即成功」也记录（读取失败由 `report_open_failure` 呈现）。
            self.record_recent_file(&p);
            return;
        }
        // 小文档：同步打开（读成功再建 tab；小而快，保持既有行为）。
        // 编码感知：BOM 优先 → 合法 UTF-8 → GB18030（见 `app::encoding`）；解码/检测
        // 失败走既有 `report_open_failure`（不建坏 tab）。
        let t_read = std::time::Instant::now();
        let (decoded, src_enc) = match std::fs::read(&p) {
            Ok(bytes) => match crate::app::encoding::decode_bytes(&bytes) {
                Ok(v) => v,
                Err(e) => {
                    self.report_open_failure(&p, &e, window, cx);
                    return;
                }
            },
            Err(e) => {
                self.report_open_failure(&p, &e, window, cx);
                return;
            }
        };
        let text = Self::normalize_newlines(&decoded);
        if std::env::var_os("MD_OPEN_DEBUG").is_some() {
            eprintln!(
                "[MD_OPEN_DEBUG] 读文件+归一化: {} ms ({} B)",
                t_read.elapsed().as_millis(),
                text.len()
            );
        }
        let t_mk = std::time::Instant::now();
        let idx = self.make_tab(window, cx);
        // 源编码写回选项卡（保存 `write_to_path` 按此编码回写；None=纯 UTF-8）。
        if let Some(tab) = self.tabs.get_mut(idx) {
            tab.source_encoding = src_enc;
        }
        if std::env::var_os("MD_OPEN_DEBUG").is_some() {
            eprintln!("[MD_OPEN_DEBUG] make_tab: {} ms", t_mk.elapsed().as_millis());
        }
        let t_load = std::time::Instant::now();
        let prep = prepare_text(text);
        self.load_file_text(idx, &p, prep, window, cx, false);
        if std::env::var_os("MD_OPEN_DEBUG").is_some() {
            eprintln!(
                "[MD_OPEN_DEBUG] load_file_text: {} ms",
                t_load.elapsed().as_millis()
            );
        }
        // 小文档同步打开成功 → 记录最近（读盘失败已早退，不记录）。
        self.record_recent_file(&p);
        if let Some(_tab) = self.tabs.get(idx) {
            let t_focus = std::time::Instant::now();
            let focus = self.tab_focus_handle(idx, cx);
            window.focus(&focus, cx);
            if std::env::var_os("MD_OPEN_DEBUG").is_some() {
                eprintln!(
                    "[MD_OPEN_DEBUG] focus 编辑器: {} ms",
                    t_focus.elapsed().as_millis()
                );
            }
        }
    }

    /// 外部拖入文件（整窗 / 编辑区 / 预览区共用）：逐个打开，目录跳过。
    pub(super) fn open_dropped_paths(
        &mut self,
        paths: &ExternalPaths,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for path in paths.paths() {
            if path.is_file() {
                self.open_file(&path.to_string_lossy(), window, cx);
            }
        }
    }

    pub(super) fn open_dialog(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        // 入口计数：空态「打开文件…」按钮 / 文件菜单 / Ctrl+O 都经此，测试据此断言
        // 空态按钮确实触发 open（`open_dialog_calls`）。
        self.open_dialog_calls = self.open_dialog_calls.wrapping_add(1);
        // 测试构建不真的弹出原生对话框（无头顶窗口，会弹出真实文件选择框干扰/挂起
        // 测试进程）；空态按钮的「触发 open」由上面的计数断言即可。
        #[cfg(not(test))]
        {
            let entity = _cx.entity();
            _window
                .spawn(_cx, async move |cx| {
                    let Some(file) = rfd::AsyncFileDialog::new()
                        .set_title("打开文件")
                        .pick_file()
                        .await
                    else {
                        return;
                    };
                    let path = file.path().to_string_lossy().into_owned();
                    let _ = cx.update_window_entity(&entity, |this, window, cx| {
                        this.open_file(&path, window, cx);
                    });
                })
                .detach();
        }
    }

    /// 打开文件夹对话框（rfd `pick_folder`；同 [`Self::open_dialog`] 异步姿势）。
    /// 取消 → 无操作。测试构建不真弹原生选择框（`#[cfg(not(test))]` 包裹）。
    pub(super) fn open_folder_dialog(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        #[cfg(not(test))]
        {
            let entity = _cx.entity();
            _window
                .spawn(_cx, async move |cx| {
                    let Some(folder) = rfd::AsyncFileDialog::new()
                        .set_title("打开文件夹")
                        .pick_folder()
                        .await
                    else {
                        return; // 取消：无操作
                    };
                    let path = folder.path().to_path_buf();
                    let _ = cx.update_window_entity(&entity, |this, window, cx| {
                        this.open_folder_path(path, window, cx);
                    });
                })
                .detach();
        }
    }

    /// 把一个文件夹设为文件树根：路径无效 → 清空树（后续写 prefs 同步清掉）；
    /// 有效 → 换根（清整棵树）→ 展开根 → 后台读根直接子项 → 切到「文件」视图 →
    /// 持久化根路径跨重启恢复。
    pub(super) fn open_folder_path(
        &mut self,
        path: std::path::PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 换根 / 关文件夹 → 旧文件夹级搜索结果失效（下次渲染按新根重扫；根为 None 时
        // 「整个文件夹」范围不可用，走未打开文件夹提示）。
        self.folder_search = None;
        if !path.is_dir() {
            self.sidebar.files.set_root(None);
            self.status = SharedString::from(format!("文件夹无效：{}", path.display()));
            self.status_ok = false;
            self.persist_prefs();
            cx.notify();
            return;
        }
        // 换根：set_root 清理 expanded/children/pending（状态失效源①换根）。
        self.sidebar.files.set_root(Some(path.clone()));
        self.sidebar.files.expanded.insert(path.clone());
        self.spawn_file_tree_scan(path.clone(), window, cx);
        self.sidebar.view = sidebar::SidebarView::Files;
        self.sidebar.open = true;
        self.sidebar.panel_open = true;
        self.status = SharedString::from(format!("已打开文件夹：{}", path.display()));
        self.status_ok = true;
        // 成功设根：记录最近文件夹（内部已含 persist_prefs —— 同时把 opened_folder
        // 根路径落盘，覆盖原末尾的 `persist_prefs()`；失败/无效路径分支不记录）。
        self.record_recent_folder(&path);
        cx.notify();
    }

    pub(super) fn write_to_path(
        &mut self,
        idx: usize,
        path: &std::path::Path,
        text: String,
        cx: &mut Context<Self>,
    ) -> bool {
        // 按源编码把缓冲 UTF-8 转回字节写盘（None=纯 UTF-8 现状；GBK/UTF-16 等原编码
        // 回写，含 BOM 还原——文档编码随文档走，「另存为」沿用同一 source_encoding）。
        let enc = self.tabs.get(idx).and_then(|t| t.source_encoding);
        let bytes = crate::app::encoding::encode_for_save(&text, enc);
        match fs::write(path, bytes) {
            Ok(()) => {
                if let Some(tab) = self.tabs.get_mut(idx) {
                    tab.file_path = Some(path.to_path_buf());
                    tab.dirty = false;
                }
                self.status = SharedString::from(format!("已保存：{}", path.display()));
                self.status_ok = true;
                // 未命名 → 有路径（save/save_as 成功落盘）：立即持久化纠正会话为 File 型。
                self.schedule_session_flush(cx, true);
                cx.notify();
                true
            }
            Err(e) => {
                self.status = SharedString::from(format!("保存失败：{e}"));
                self.status_ok = false;
                cx.notify();
                false
            }
        }
    }

    pub(super) fn save_tab(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(tab) = self.tabs.get(idx) else {
            return false;
        };
        let Some(p) = tab.file_path.clone() else {
            return self.save_tab_as(idx, window, cx);
        };
        // WYSIWYG（live2）模式下源码编辑区不再实时镜像（去 O(n)）——直接读 WyEditor
        // 的 rope 文本（等效实时内容），否则保存的是陈旧/空内容。
        let text = self.tab_effective_text(idx, cx);
        self.write_to_path(idx, &p, text, cx)
    }

    pub(super) fn save_tab_as(
        &mut self,
        idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if idx >= self.tabs.len() {
            return false;
        }
        let file_name = self.tab_file_name(idx);
        // 同 `save_tab`：WYSIWYG 开启时读 WyEditor 实时内容，而非陈旧的编辑区。
        let text = self.tab_effective_text(idx, cx);
        let entity = cx.entity();
        window
            .spawn(cx, async move |cx| {
                let Some(file) = rfd::AsyncFileDialog::new()
                    .set_title("保存文件")
                    .set_file_name(&file_name)
                    .save_file()
                    .await
                else {
                    return;
                };
                let file = file.path().to_path_buf();
                let _ = cx.update_window_entity(&entity, |this, _window, cx| {
                    this.write_to_path(idx, &file, text, cx);
                });
            })
            .detach();
        true
    }

    pub(super) fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.save_tab(self.active_tab, window, cx);
    }

    pub(super) fn save_as(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.save_tab_as(self.active_tab, window, cx);
    }

    pub(super) fn paste_normalized(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(clipboard) = cx.read_from_clipboard() else {
            return;
        };
        let Some(text) = clipboard.text() else {
            return;
        };
        let text = Self::normalize_newlines(&text);
        // 若侧边栏搜索框持有焦点 → 粘贴到搜索框（其不处理应用级 PasteNormalized）
        let search_focused = window
            .focused(cx)
            .is_some_and(|fh| fh == self.search_state.read(cx).focus_handle(cx));
        if search_focused {
            let search = self.search_state.clone();
            search.update(cx, |s, cx| {
                s.replace(text, window, cx);
            });
            return;
        }
        let Some(tab) = self.tabs.get(self.active_tab) else {
            return;
        };
        // 数学编辑卡内嵌编辑器聚焦（live2 WYSIWYG 的卡片输入区）：Ctrl+V（app 根
        // `PasteNormalized`）必须粘贴进卡片，而不是误粘进源码编辑区（P10 实测）。
        if let Some(live) = tab.live2.as_ref() {
            let routed = live.update(cx, |w, cx| w.paste_into_focused_math_edit(window, &text, cx));
            if routed {
                return;
            }
        }
        tab.editor.update(cx, |editor, cx| {
            editor.paste_text(&text, cx);
        });
    }
}

/// 读盘→归一化→Rope→行宽统计 的打开预处理产物：UI 线程据此 O(1) 装配，
/// 不在 UI 线程做任何 O(n) 重扫。可跨线程（`Rope`/`BTreeMap` 均 `Send`）。
struct PreparedText {
    /// 全文 Rope（编辑器 + WyEditor 预览共享，O(1) 克隆）。
    rope: Rope,
    /// 显示行数（`str::lines()` 语义，尾换行不追加行）。
    lines: usize,
    /// 字符总数（Unicode 标量）。
    chars: usize,
    /// 原文件字节数。
    src_len: usize,
    /// 全文最长行（字符数）。
    max_line_chars: usize,
    /// 行宽直方图（最宽行 / 横向滚动条量程依据；通常由后台线程算好）。
    line_width_histogram: editor::buffer::LineWidthHistogram,
}

/// 把归一化文本预处理成编辑器装配产物（纯函数，可在后台线程跑）：
/// Rope::from + 行/字计数 + 行宽直方图 —— 这一整条 O(n) 链路（大文档合计
/// 几百 ms）由调用方决定放在后台线程，UI 线程不再背。
fn prepare_text(text: String) -> PreparedText {
    let src_len = text.len();
    let t = std::time::Instant::now();
    let lines = text.lines().count().max(1);
    let chars = text.chars().count();
    let counts_ms = t.elapsed().as_millis();
    let t = std::time::Instant::now();
    let rope = Rope::from(text.as_str());
    let rope_ms = t.elapsed().as_millis();
    let t = std::time::Instant::now();
    let (max_line_chars, line_width_histogram) = editor::buffer::compute_line_stats(&rope);
    let stats_ms = t.elapsed().as_millis();
    if std::env::var_os("MD_OPEN_DEBUG").is_some() {
        eprintln!(
            "[MD_OPEN_DEBUG] 准备文本: 计数(lines+chars)={counts_ms} ms, Rope::from({src_len} B)={rope_ms} ms, compute_line_stats({lines} 行)={stats_ms} ms"
        );
    }
    PreparedText {
        rope,
        lines,
        chars,
        src_len,
        max_line_chars,
        line_width_histogram,
    }
}

/// 是否走「后台打开」流水线：按文件大小元数据判断（无需读盘）。
fn is_large_path(p: &std::path::Path) -> bool {
    std::fs::metadata(p)
        .map(|m| m.len() > OPEN_ASYNC_MIN_BYTES)
        .unwrap_or(false)
}
