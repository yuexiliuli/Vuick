//! file_ops.

use super::*;
use ui::notification::{Notification, NotificationKind};

use crate::app::open_progress::TextChunk;
use config::limits::{EDITOR_WRAP_MAX_LINES, OPEN_ASYNC_MIN_BYTES};
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

/// 生产者 → 消费者的消息（大文档分块打开的异步通道）。
enum OpenMsg {
    Chunk(TextChunk),
    Eof { src_len: u64 },
    Err(std::io::Error),
}

/// 后台分块读盘 + 归一化 + 逐块统计的生产者：`send().await` 背压外推（读满让出），
/// EOF / 出错 / 消费端放弃即收尾关闭通道。
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
            ed.set_rope_with_stats(prep.rope, prep.max_line_chars, prep.line_width_histogram);
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
        line_width_histogram: editor::buffer::LineWidthHistogram,
        md_wy_open: Option<markdown::wysiwyg::WyOpenResult>,
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
                        t.md_wy_early,
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
        self.open_finish_count += 1; // 大文档两段式打开：慢段（统计 + 预览索引补齐）
        if resume_version.is_some_and(|v| tab.edit_version != v) {
            return; // 打开后已编辑：统计/索引已过期，交给编辑/增量同步链路维护
        }
        tab.editor.update(cx, |ed, _| {
            ed.apply_stats(max_line_chars, line_width_histogram);
        });
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
                tab.md_wy_pending_full = None;
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

                // ---- 消费者：批级装配（一次 UI 更新尽量多收已缓冲块、逐块追加、批末统一
                // notify；空时让出调度），再到主线程追加装配 ----
                let mut cum_mx = 0usize;
                let mut cum_hist = editor::buffer::LineWidthHistogram::new();
                let mut src_len = 0u64;
                let mut first = true;
                let mut resume_version: Option<u64> = None;
                let mut io_err: Option<std::io::Error> = None;
                let mut aborted = false;
                let mut rx = rx;
                // 组批时把通道里非 Chunk 的尾巴（Eof/Err）暂存到下一篇处理（`try_next`
                // 会把它取出，不能放回通道）。
                let mut pending_msg: Option<OpenMsg> = None;
                'consume: loop {
                    let msg = match pending_msg.take() {
                        Some(m) => m,
                        None => match rx.next().await {
                            Some(m) => m,
                            None => break 'consume,
                        },
                    };
                    let OpenMsg::Chunk(first_chunk) = msg else {
                        match msg {
                            OpenMsg::Eof { src_len: s } => {
                                src_len = s;
                                break 'consume;
                            }
                            OpenMsg::Err(e) => {
                                io_err = Some(e);
                                break 'consume;
                            }
                            OpenMsg::Chunk(_) => unreachable!(), // 上面 let-else 已排除
                        }
                    };
                    // —— 收齐通道已缓冲的块入批（通道容量 4；Eof/Err 暂存下一篇按序处理）——
                    let mut batch: Vec<TextChunk> = vec![first_chunk];
                    while let Ok(m) = rx.try_recv() {
                        match m {
                            OpenMsg::Chunk(c) => batch.push(c),
                            other => {
                                pending_msg = Some(other);
                                break;
                            }
                        }
                    }
                    // —— 批内累计精确统计（每块自身携带精确 hist/max_line_chars；finish 复位用）——
                    #[cfg(test)]
                    PROGRESSIVE_OPEN_CHUNKS
                        .fetch_add(batch.len(), std::sync::atomic::Ordering::Relaxed);
                    for chunk in &batch {
                        cum_mx = cum_mx.max(chunk.max_line_chars);
                        for (w, c) in chunk.hist.iter() {
                            *cum_hist.entry(*w).or_insert(0) += c;
                        }
                    }
                    // —— 一次 UI 更新内依次挂载/追加（各自不 notify），批末统一一次 notify：
                    // 每块不再触发整窗重绘（P1b；配合 P1a 高亮门控，打开期整窗重绘/重解析
                    // 从 ~块数 降到 ~批数）——保留首块挂载、Stale 中止、resume_version 语义。
                    let is_first = first;
                    let p_ch = p.clone();
                    let res = cx
                        .update_window_entity(&entity, move |this, window, cx| {
                            let mut out = (None, false, false); // (Mounted版本, 是否挂过载, 是否中止)
                            for (i, chunk) in batch.into_iter().enumerate() {
                                let r = this.apply_open_chunk(
                                    idx,
                                    &p_ch,
                                    chunk,
                                    is_first && i == 0,
                                    window,
                                    cx,
                                );
                                match r {
                                    ChunkApply::Mounted(v) => {
                                        if out.0.is_none() {
                                            out.0 = Some(v);
                                        }
                                        out.1 = true;
                                    }
                                    ChunkApply::Appended => {}
                                    ChunkApply::Stale => {
                                        out.2 = true;
                                        break;
                                    }
                                }
                            }
                            #[cfg(test)]
                            PROGRESSIVE_OPEN_NOTIFIES
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            cx.notify(); // 批末统一一次通知（本批块不逐块重绘）
                            out
                        });
                    match res {
                        Ok((Some(v), _, false)) => {
                            if resume_version.is_none() {
                                resume_version = Some(v);
                            }
                            first = false;
                        }
                        Ok((_, mounted, false)) => {
                            if mounted {
                                first = false;
                            }
                        }
                        Ok((_, _, true)) | Err(_) => {
                            aborted = true; // 选项卡已关 / 下标被换成其它文件
                            break 'consume;
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
                                    t.md_wy_early,
                                    Some(MdWyEarly::Pending { .. }) | Some(MdWyEarly::Done)
                                )
                            })
                        })
                        .unwrap_or(false);
                let md_wy_open = if early_live {
                    None
                } else if is_md {
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
                    let t_wy = std::time::Instant::now();
                    let wy_open = cx
                        .background_executor()
                        .spawn(async move {
                            let wy = markdown::wysiwyg::WyEditor::open_from_rope(snap);
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
                // 最终：装算统计/预览装配 + 恢复「已打开」状态。
                let hist_lines = cum_hist.values().sum::<usize>();
                let p_fin = p.clone();
                let _ = cx.update_window_entity(&entity, move |this, window, cx| {
                    this.finish_file_text(
                        idx,
                        &p_fin,
                        cum_mx,
                        cum_hist,
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
        // 不在此 notify：P1b 批级装配在 `spawn_progressive_open` 批末统一 notify 一次
        // （每块整窗重绘 → 每批一次），首块挂载自有 `load_file_text` 的 notify。
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
        let Some(tab) = self.tabs.get_mut(idx) else {
            return;
        };
        match kind {
            MediaKind::Pdf => {
                // PdfViewer：自包含阅读器（异步读文件+解析+页级按需栅格化，不解析为 Markdown）
                let viewer = cx.new(|cx| pdf::PdfViewer::new(p.to_path_buf(), cx));
                viewer.update(cx, |view, _| view.set_background(canvas_bg));
                // 订阅阅读器 notify：检索结果/加载进度到达 → 重绘 app（侧边栏 PDF 命中计数/页列表更新）
                self._pdf_observe = Some(cx.observe(&viewer, |_this, _e, cx| {
                    cx.notify();
                }));
                tab.reader = MediaReader::Pdf(viewer);
            }
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
        // EPUB / PDF：先生成解析结果，失败则仅提示、不建空标签页
        if Self::is_epub_path(&p) {
            match epub::parse_epub_file(&p) {
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
        if Self::is_image_path(&p) {
            // 先建标签页、立即返回：解码放到后台（ImageViewer 自包含）
            let idx = self.make_tab(window, cx);
            self.load_media(idx, &p, MediaKind::Image, window, cx);
            let focus = self.focus_handle(cx);
            window.focus(&focus, cx);
            return;
        }
        #[cfg(feature = "video")]
        if Self::is_video_path(&p) {
            // 先建标签页、立即返回：解码放到后台（VideoViewer 自包含）
            let idx = self.make_tab(window, cx);
            self.load_media(idx, &p, MediaKind::Video, window, cx);
            let focus = self.focus_handle(cx);
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
