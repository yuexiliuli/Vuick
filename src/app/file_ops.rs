//! file_ops.

use super::*;
use gpui_component::notification::{Notification, NotificationType};
use gpui_component::WindowExt as _;

use crate::app::open_progress::TextChunk;
use crate::config::limits::{EDITOR_WRAP_MAX_LINES, OPEN_ASYNC_MIN_BYTES};
use crate::config::timing::OPEN_FAILURE_AUTOHIDE_SECS;

/// [`EditorApp::apply_open_chunk`] 的结果：一块被如何处理（消费者据此决定继续/中止）。
enum ChunkApply {
    /// 首块装配成功，携带「打开瞬间的编辑版本号」（收尾的未编辑守卫）。
    Mounted(u64),
    /// 后续块追加成功（继续收下一块）。
    Appended,
    /// 目标选项卡已不存在 / 下标被换成其它文件（中止本打开流水线）。
    Stale,
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
    fn report_open_failure(
        &mut self,
        p: &std::path::Path,
        e: &std::io::Error,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
        window.push_notification(
            Notification::new()
                .id1::<OpenFailureNotification>(key.clone())
                .title("打开失败")
                .message(format!("无法读取文件内容：{}\n{e}", p.display()))
                .with_type(NotificationType::Error)
                .autohide(false),
            cx,
        );
        let entity = cx.entity();
        window
            .spawn(cx, async move |cx| {
                use gpui::VisualContext as _;
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(OPEN_FAILURE_AUTOHIDE_SECS))
                    .await;
                let _ = cx.update_window_entity(&entity, |_, window, cx| {
                    window.remove_notification1::<OpenFailureNotification>(key, cx)
                });
            })
            .detach();
        cx.notify();
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
        _window: &mut Window,
        cx: &mut Context<Self>,
        defer_preview: bool,
    ) {
        self.search_scan = None; // 同一选项卡内容被替换 → 搜索缓存失效
        let panel_bg = self.panel_bg();
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
        let t_editor = std::time::Instant::now();
        tab.editor.update(cx, |ed, _cx| {
            // O(1) 装配：行宽统计在后台算好（快速挂载时为占位，finish 再补真值），
            // 不再 UI 线程 recompute_max_line（≈350ms）。
            ed.set_rope_with_stats(prep.rope, prep.max_line_chars, prep.line_width_histogram);
            if is_md && lines <= EDITOR_WRAP_MAX_LINES {
                // 自研编辑器尚不支持软换行（占位，API 一致；大文档无 DisplayMap 布局问题）
                ed.set_soft_wrap(true);
            }
            ed.set_language(Some(lang));
        });
        let elapsed_editor = t_editor.elapsed();
        // 编辑器即刻挂载（大文档不再是「预览优先」：内容瞬时可见优先于预览/滚动条精度）。
        tab.editor_pending = false;
        let t_preview = std::time::Instant::now();
        tab.md_preview.update(cx, |view, cx| {
            view.set_background(panel_bg);
            view.set_base_dir(p.parent().map(|d| d.to_path_buf()));
            if is_md {
                // Markdown 默认/始终自动换行
                view.set_wrap(true, cx);
                if !defer_preview {
                    // 同步小文档：直接全量解析（行块索引现算；文件小故可接受在 UI 线程）。
                    view.set_source(&prep.text, cx);
                }
                // defer=true（大文档快速挂载）：预览留空，行块索引用 finish_file_text
                // 的 set_source_with_index 补上（并登记增量基底 preview_prev）。
            }
        });
        let elapsed_preview = t_preview.elapsed();
        if std::env::var_os("MD_OPEN_DEBUG").is_some() {
            eprintln!(
                "[MD_OPEN_DEBUG] load_file_text:{src_len} B/{} 行: 编辑器(from_parts+wrap+highlight)={} ms, 预览={} ms{}",
                lines,
                elapsed_editor.as_millis(),
                elapsed_preview.as_millis(),
                if defer_preview { " (预览延迟，finish 补齐)" } else { "" },
            );
        }
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
        // 运行时内存诊断：`MD_MEM_DEBUG=1 cargo run` 打开文件时输出各份文本副本与
        // 预览解析驻留（定位“同样内容，.md 比 .txt 内存高”到底高在哪）。
        if std::env::var_os("MD_MEM_DEBUG").is_some() {
            let content_len = tab.content.len();
            let editor_len = tab.editor.read(cx).value().len();
            let (rows, preview_bytes) = tab.md_preview.read(cx).memory_estimate();
            eprintln!(
                "[MD_MEM_DEBUG] 打开 {:?}: 源文件 {} B / {} 行 / {} 字符 → 副本 content={} B + editor={} B + 预览 {rows} 行≈{} KB",
                p.file_name(),
                src_len,
                lines,
                chars,
                content_len,
                editor_len,
                preview_bytes / 1024,
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
        cx.notify();
    }

    /// 大文档「先瞬时显示、后台补齐」的第二段：后台并行算好行宽统计 + (md)scan_blocks
    /// 后调用。守卫：选项卡仍存、仍是同一文件、且**打开后未编辑**（已编辑则跳过，让
    /// 增量同步/编辑自身维护）。应用后 O(1) 修正横向滑动条量程，并给 Markdown 预览装上
    /// 预建索引（登记增量基底，后续编辑即可走增量扫描）。
    #[allow(clippy::too_many_arguments)]
    fn finish_file_text(
        &mut self,
        idx: usize,
        p: &std::path::Path,
        max_line_chars: usize,
        line_width_histogram: crate::editor::buffer::LineWidthHistogram,
        index: Option<crate::markdown::BlockIndex>,
        resume_version: Option<u64>,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.get_mut(idx) else {
            return;
        };
        if tab.file_path.as_deref() != Some(p) {
            return; // 选项卡已被换成其它文件
        }
        self.open_finish_count += 1; // 大文档两段式打开：慢段（统计 + 预览索引补齐）
        if resume_version.is_some_and(|v| tab.edit_version != v) {
            return; // 打开后已编辑：统计/索引已过期，交给编辑/增量同步链路维护
        }
        tab.editor.update(cx, |ed, _| {
            ed.apply_stats(max_line_chars, line_width_histogram);
        });
        if let Some(index) = index {
            if Self::is_markdown_path(p) && tab.preview_on {
                let rc = std::rc::Rc::new(index);
                let rope = tab.editor.read(cx).rope().clone();
                tab.preview_prev = Some((rope.clone(), rc.clone()));
                tab.md_preview.update(cx, |view, cx| {
                    view.set_source_with_index(crate::markdown::PreviewSrc::Rope(rope), rc, cx);
                });
            }
        }
        if std::env::var_os("MD_OPEN_DEBUG").is_some() {
            // 诊断“拖到底不到底”：量程就绪后两栏纵向量程总高。编辑器精确时应 ≈
            // 行数×行高；预览(精确平均高) 应 ≈ 全篇块高之和。值远小于预期 → 该栏量程坍缩。
            let editor_total = tab.editor.read(cx).vertical_total_px();
            let preview_total = tab.md_preview.read(cx).vertical_total_px();
            eprintln!(
                "[MD_SCROLL] finish 后纵向 total——编辑器={editor_total:.0}px ({} 行), 预览={preview_total:.0}px ({} 块)",
                tab.line_count,
                tab.md_preview.read(cx).memory_estimate().0,
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
    /// `idx` 处须已存在选项卡（`open_file` 新建 / `open_file_in_tab` 复用）。
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

        /// 生产者 → 消费者的消息。
        enum OpenMsg {
            Chunk(crate::app::open_progress::TextChunk),
            Eof { src_len: u64 },
            Err(std::io::Error),
        }

        // 异步**有界**通道（容量 4）：消费端 `next()` 空时让出调度，生产端 `send()`
        // 满时也 `await` 让出（背压）——既不物化整篇 String，也不让通道囤积整篇
        // （测试的协作式调度器下，blocking recv 会卡死，故绝不用 std::sync::mpsc）。
        let (tx, rx) = futures::channel::mpsc::channel::<OpenMsg>(4);
        window
            .spawn(cx, async move |cx| {
                use futures::{SinkExt as _, StreamExt as _};
                // ---- 生产者（后台线程；逐块读盘 → `send().await` 背压外推）----
                let mut prod_tx = tx.clone();
                let prod_p = p.clone();
                cx.background_executor()
                    .spawn(async move {
                        let t = std::time::Instant::now();
                        let mut reader =
                            match crate::app::open_progress::ChunkReader::open(&prod_p) {
                                Ok(r) => r,
                                Err(e) => {
                                    let _ = prod_tx.send(OpenMsg::Err(e)).await;
                                    prod_tx.close_channel();
                                    return;
                                }
                            };
                        loop {
                            match reader.next_chunk() {
                                Ok(Some(chunk)) => {
                                    if prod_tx.send(OpenMsg::Chunk(chunk)).await.is_err() {
                                        return; // 消费端已放弃 → 停止读盘
                                    }
                                }
                                Ok(None) => {
                                    if std::env::var_os("MD_OPEN_DEBUG").is_some() {
                                        eprintln!(
                                            "[MD_OPEN_DEBUG] 后台 分块读盘+归一化+逐块统计 合计: {} ms",
                                            t.elapsed().as_millis()
                                        );
                                    }
                                    let _ = prod_tx
                                        .send(OpenMsg::Eof { src_len: reader.total_bytes() })
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
                    })
                    .detach();

                // ---- 消费者：逐块取（空时让出调度），再到主线程追加装配 ----
                let mut cum_mx = 0usize;
                let mut cum_hist = crate::editor::buffer::LineWidthHistogram::new();
                let mut src_len = 0u64;
                let mut first = true;
                let mut resume_version: Option<u64> = None;
                let mut io_err: Option<std::io::Error> = None;
                let mut aborted = false;
                let mut rx = rx;
                while let Some(msg) = rx.next().await {
                    match msg {
                        OpenMsg::Chunk(chunk) => {
                            // 累计精确统计（与 编辑器每个块合并 的是同一份；finish 用它复位）。
                            cum_mx = cum_mx.max(chunk.max_line_chars);
                            for (w, c) in chunk.hist.iter() {
                                *cum_hist.entry(*w).or_insert(0) += c;
                            }
                            let is_first = first;
                            let p_ch = p.clone();
                            let r = cx
                                .update_window_entity(&entity, move |this, window, cx| {
                                    this.apply_open_chunk(idx, &p_ch, chunk, is_first, window, cx)
                                });
                            match r {
                                Ok(ChunkApply::Mounted(v)) => {
                                    if resume_version.is_none() {
                                        resume_version = Some(v);
                                    }
                                    first = false;
                                }
                                Ok(ChunkApply::Appended) => {}
                                _ => {
                                    aborted = true; // 选项卡已关 / 下标被换成其它文件
                                    break;
                                }
                            }
                        }
                        OpenMsg::Eof { src_len: s } => {
                            src_len = s;
                            break;
                        }
                        OpenMsg::Err(e) => {
                            io_err = Some(e);
                            break;
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
                // md：后台扫块建预览索引（快照 rope O(1) 克隆，行块统计已逐块精确）。
                let index = if is_md {
                    let p_snap = p.clone();
                    let snap = cx
                        .update_window_entity(&entity, move |this, _w, cx| {
                            this.tabs
                                .get(idx)
                                .filter(|t| t.file_path.as_deref() == Some(p_snap.as_path()))
                                .map(|t| t.editor.read(cx).rope().clone())
                        })
                        .ok()
                        .flatten();
                    let Some(snap) = snap else { return };
                    let t = std::time::Instant::now();
                    let index = cx
                        .background_executor()
                        .spawn(async move {
                            let out = crate::markdown::scan_blocks_rope(&snap);
                            if std::env::var_os("MD_OPEN_DEBUG").is_some() {
                                eprintln!(
                                    "[MD_OPEN_DEBUG] 后台 scan_blocks 合计: {} ms",
                                    t.elapsed().as_millis()
                                );
                            }
                            out
                        })
                        .await;
                    Some(index)
                } else {
                    None
                };
                // 最终：装算统计/预览索引 + 恢复「已打开」状态。
                let hist_lines = cum_hist.values().sum::<usize>();
                let p_fin = p.clone();
                let _ = cx.update_window_entity(&entity, move |this, _window, cx| {
                    this.finish_file_text(
                        idx,
                        &p_fin,
                        cum_mx,
                        cum_hist,
                        index,
                        resume_version,
                        cx,
                    );
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
                    // 折叠候选 / 括号标尺：末块落定后**后台**重扫一次（打开全程不再每块
                    // 整篇重扫 → 打开 160k 行文档从 ~2s UI 卡顿降到毫秒级装配）。
                    if let Some(tab) = this.tabs.get_mut(idx).filter(|t| t.file_path.as_deref() == Some(p_fin.as_path())) {
                        tab.editor.update(cx, |ed, cx| {
                            ed.refresh_fold_brackets(cx);
                        });
                    }
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

    /// 把后台读到的第 `first` 块（首块）挂上 / 后续块追到编辑器末尾。
    ///
    /// - 首块：走 [`Self::load_file_text`](defer=true) 快速挂载（`open_fast_count` +1、
    ///   `editor_pending`=false → **几十 KB 即出内容**），并返回打开瞬间的编辑版本号
    ///   作为收尾的「未编辑」守卫；
    /// - 后续块：`CodeEditor::append_mounted_chunk` O(块大小) 追加（滚动/撤销保留），
    ///   行/字缓存与横向量程随块精确补齐。
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
            let TextChunk { text, hist, max_line_chars } = chunk;
            let rope = Rope::from(text.as_str());
            let lines = prep_first_lines_hint(&text);
            let chars = text.chars().count();
            let src_len = text.len();
            let prep = PreparedText {
                text,
                rope,
                lines,
                chars,
                src_len,
                max_line_chars,
                line_width_histogram: hist,
            };
            self.load_file_text(idx, p, prep, window, cx, true);
            // 首块后内容已可见，但整篇仍在读盘 → 保持「正在打开」，读完置最终态。
            if let Some(tab) = self.tabs.get(idx) {
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
        self.search_scan = None; // 内容被追加 → 搜索缓存失效
        let TextChunk { text, hist, max_line_chars } = chunk;
        tab.editor.update(cx, |ed, _| {
            ed.append_mounted_chunk(&text, max_line_chars, hist);
        });
        let (lc, cc) = {
            let st = tab.editor.read(cx);
            (st.display_line_count(), st.char_count())
        };
        tab.line_count = lc;
        tab.char_count = cc;
        cx.notify();
        ChunkApply::Appended
    }

    /// 把 EPUB 解析结果载入指定选项卡：纯展示（只读预览），编辑区置空。
    fn load_epub(
        &mut self,
        idx: usize,
        p: &std::path::Path,
        doc: crate::epub::EpubDocument,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.search_scan = None; // 同一选项卡内容被替换 → 搜索缓存失效
        let panel_bg = self.panel_bg();
        let Some(tab) = self.tabs.get_mut(idx) else {
            return;
        };
        let text = doc.markdown;
        let lines = text.lines().count().max(1);
        let chars = text.chars().count();
        // 编辑区置为提示文本（EPUB 只读，不参与文本编辑）
        tab.editor.update(cx, |ed, _cx| {
            ed.set_text("");
            ed.set_language(Some("markdown"));
        });
        tab.md_preview.update(cx, |view, cx| {
            view.set_background(panel_bg);
            view.set_base_dir(p.parent().map(|d| d.to_path_buf()));
            // EPUB 图片保持内存，渲染时由预览查表直接解码（不写硬盘）
            view.set_memory_images(doc.images.clone());
            // EPUB 阅读默认开换行（正文自然折行），fullscreen 时占满为阅读模式
            view.set_wrap(true, cx);
            // EPUB 阅读排版：标题居中、正文首行缩进 2 字符、脚注 `[n]` 悬浮卡片
            view.set_epub(true, doc.notes.clone(), cx);
            view.set_source(&text, cx);
        });
        tab.file_path = Some(p.to_path_buf());
        tab.content = SharedString::from(text);
        tab.line_count = lines;
        tab.char_count = chars;
        tab.cursor_row = 0;
        tab.cursor_col = 0;
        tab.dirty = false;
        tab.preview_on = true;
        tab.preview_fullscreen = true;
        // EPUB 正文只存在于预览，编辑区为空 → 预览锁定，toggle 不翻转也不显示空编辑区
        tab.readonly_preview = true;
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

    /// 把一个新 PDF 阅读器载入指定选项卡：编辑区置空、全屏只读预览。
    /// 解析由 [`crate::pdf::PdfViewer`] 自己在后台完成，窗口不阻塞。
    fn load_pdf(
        &mut self,
        idx: usize,
        p: &std::path::Path,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.search_scan = None; // 同一选项卡内容被替换 → 搜索缓存失效
        self.pdf_search = None; // 新 PDF 阅读器 → 需重新推送查询
        // 阅读画布用灰色系（非白面板），让页面“纸面”卡片与画布形成分隔
        let canvas_bg = self.window_bg();
        let Some(tab) = self.tabs.get_mut(idx) else {
            return;
        };
        // 编辑区置为提示文本（PDF 只读，不参与文本编辑）
        tab.editor.update(cx, |ed, _cx| {
            ed.set_text("");
            ed.set_language(Some("markdown"));
        });
        tab.md_preview.update(cx, |view, cx| {
            view.set_background(canvas_bg);
            view.set_source("", cx);
        });
        // PdfViewer：自包含阅读器（异步读文件+解析+页级按需栅格化，不解析为 Markdown）
        let viewer = cx.new(|cx| crate::pdf::PdfViewer::new(p.to_path_buf(), cx));
        viewer.update(cx, |view, _| view.set_background(canvas_bg));
        tab.pdf_preview = Some(viewer);
        // 订阅阅读器 notify：检索结果/加载进度到达 → 重绘 app（侧边栏 PDF 命中计数/页列表更新）
        let preview = tab.pdf_preview.clone().expect("上面刚写入");
        self._pdf_observe = Some(cx.observe(&preview, |_this, _e, cx| {
            cx.notify();
        }));
        tab.file_path = Some(p.to_path_buf());
        tab.content = SharedString::default();
        tab.line_count = 0;
        tab.char_count = 0;
        tab.cursor_row = 0;
        tab.cursor_col = 0;
        tab.dirty = false;
        tab.preview_on = true;
        tab.preview_fullscreen = true;
        // PDF 正文只存在于阅读器，编辑区为空 → 预览锁定，toggle 不翻转也不显示空编辑区
        tab.readonly_preview = true;
        self.status = SharedString::from(format!("已打开 PDF：{}（解析中…）", p.display()));
        self.status_ok = true;
        cx.notify();
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
        self.load_pdf(idx, &p, window, cx);
    }

    /// 把文件内容载入已存在的选项卡（读取失败仅提示，保留原选项卡）。
    pub(super) fn open_file_in_tab(
        &mut self,
        idx: usize,
        path: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let p = PathBuf::from(path.trim());
        if Self::is_epub_path(&p) {
            match crate::epub::parse_epub_file(&p) {
                Ok(doc) => self.load_epub(idx, &p, doc, window, cx),
                Err(e) => {
                    self.status = SharedString::from(format!("打开失败：{e}"));
                    self.status_ok = false;
                    cx.notify();
                }
            }
            return;
        }
        if Self::is_pdf_path(&p) {
            self.open_pdf_async(idx, p.clone(), window, cx);
            return;
        }
        // 大文件（按字节阈值判断）：后台打开（复用已有选项卡），
        // 分块读盘→归一化→按块统计→逐块装配 全在后台线程，UI 每块只做 O(块大小) 追加。
        if is_large_path(&p) {
            self.spawn_progressive_open(idx, p, window, cx);
            return;
        }
        match fs::read_to_string(&p) {
            Ok(text) => {
                let prep = prepare_text(Self::normalize_newlines(&text));
                self.load_file_text(idx, &p, prep, window, cx, false);
            }
            Err(e) => self.report_open_failure(&p, &e, window, cx),
        }
    }

    /// 打开文件：新建一个文件选项卡并在其中载入文件内容，随后把焦点移到新选项卡的编辑区。
    ///
    /// 先确认内容可读/可解析再创建选项卡：读取失败时只给出错误提示，
    /// 不再生成一个内容空白的标签页。
    pub(super) fn open_file(&mut self, path: &str, window: &mut Window, cx: &mut Context<Self>) {
        let p = PathBuf::from(path.trim());
        // EPUB / PDF：先生成解析结果，失败则仅提示、不建空标签页
        if Self::is_epub_path(&p) {
            match crate::epub::parse_epub_file(&p) {
                Ok(doc) => {
                    let idx = self.make_tab(window, cx);
                    self.load_epub(idx, &p, doc, window, cx);
                    let focus = self.tab_focus_handle(idx, cx);
                    window.focus(&focus, cx);
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
            return;
        }
        // 小文档：同步打开（读成功再建 tab；小而快，保持既有行为）
        let t_read = std::time::Instant::now();
        let text = match fs::read_to_string(&p) {
            Ok(text) => Self::normalize_newlines(&text),
            Err(e) => {
                self.report_open_failure(&p, &e, window, cx);
                return;
            }
        };
        if std::env::var_os("MD_OPEN_DEBUG").is_some() {
            eprintln!(
                "[MD_OPEN_DEBUG] 读文件+归一化: {} ms ({} B)",
                t_read.elapsed().as_millis(),
                text.len()
            );
        }
        let t_mk = std::time::Instant::now();
        let idx = self.make_tab(window, cx);
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

    pub(super) fn open_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let entity = cx.entity();
        window
            .spawn(cx, async move |cx| {
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

    pub(super) fn write_to_path(
        &mut self,
        idx: usize,
        path: &std::path::Path,
        text: String,
        cx: &mut Context<Self>,
    ) -> bool {
        match fs::write(path, text) {
            Ok(()) => {
                if let Some(tab) = self.tabs.get_mut(idx) {
                    tab.file_path = Some(path.to_path_buf());
                    tab.dirty = false;
                }
                self.status = SharedString::from(format!("已保存：{}", path.display()));
                self.status_ok = true;
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
        // WYSIWYG（live2）模式下源码编辑区是**去抖镜像 / 退出时写回**，未停手可能比
        // WyEditor 陈旧——直接读 WyEditor 的 rope 文本（等效实时内容）。
        let text = tab
            .live2
            .as_ref()
            .map(|l2| l2.read(cx).text().to_string())
            .unwrap_or_else(|| tab.editor.read(cx).value().to_string());
        self.write_to_path(idx, &p, text, cx)
    }

    pub(super) fn save_tab_as(
        &mut self,
        idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(tab) = self.tabs.get(idx) else {
            return false;
        };
        let file_name = self.tab_file_name(idx);
        // 同 `save_tab`：WYSIWYG 开启时读 WyEditor 实时内容，而非陈旧的镜像。
        let text = tab
            .live2
            .as_ref()
            .map(|l2| l2.read(cx).text().to_string())
            .unwrap_or_else(|| tab.editor.read(cx).value().to_string());
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
        tab.editor.update(cx, |editor, cx| {
            editor.paste_text(&text, cx);
        });
    }
}

/// 读盘→归一化→Rope→行宽统计 的打开预处理产物：UI 线程据此 O(1) 装配，
/// 不在 UI 线程做任何 O(n) 重扫。可跨线程（`String`/`Rope`/`BTreeMap` 均 `Send`）。
struct PreparedText {
    /// 归一化后的全文（同步小文档预览的 `set_source` 用；大文档预览走 Rope）。
    text: String,
    /// 全文 Rope（编辑器 + Markdown 预览共享，O(1) 克隆）。
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
    line_width_histogram: crate::editor::buffer::LineWidthHistogram,
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
    let (max_line_chars, line_width_histogram) = crate::editor::buffer::compute_line_stats(&rope);
    let stats_ms = t.elapsed().as_millis();
    if std::env::var_os("MD_OPEN_DEBUG").is_some() {
        eprintln!(
            "[MD_OPEN_DEBUG] 准备文本: 计数(lines+chars)={counts_ms} ms, Rope::from({src_len} B)={rope_ms} ms, compute_line_stats({lines} 行)={stats_ms} ms"
        );
    }
    PreparedText {
        text,
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
