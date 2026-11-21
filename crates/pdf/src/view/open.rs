//! PDF 阅读器子模块：文档打开 / 状态访问器 / 资源释放 / 每帧装配。
//!
//! - [`super::PdfViewer::new`] 与 [`super::PdfViewer::start_load`]：后台读文件 + 经单例
//!   pdfium 工作线程解析、量页、取书签目录，完成后在主线程铺页槽（打开即显，不预栅格化）；
//! - 状态只读访问器（页数 / 当前页 / 加载态 / 调度进度 / 画布背景 / 书签）供外壳
//!   （侧边栏、状态栏、`app`）消费；
//! - [`super::PdfViewer::release`]：关闭标签页 / 丢弃视图时显式 `drop_image` 下线 GPU 纹理，
//!   并关闭 worker 侧文档；
//! - [`super::PdfViewer::ensure_window`]：**每帧装配入口**——回收在途结果（瓦片/封面/
//!   缩略/中清/文本/检索）、估速、静态帧门控、按视口调度瓦片（常态）/占位（快速拖动）、
//!   淘汰超预算缓存。调度细节在各职责子模块（`tiles` / `search`）。

use super::*;
use crate::pdf::parser::grid_bucket_width;

/// 两路径是否指向**同一文件**（P1 数据丢失防护用）：先各自 `canonicalize` 对拍（把盘上
/// 已存在文件归一化成绝对路径，`.`/`..`、大小写、分隔符差异都能消除）；`out` 目标尚未
/// 创建时 `canonicalize` 失败 → 退化回字面路径相等比较（防漏纯拼写同路径）。
fn same_file_path(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// 提取输出路径的**同步可写性预检**：原子写盘先在同目录建临时文件（`save_raw_doc_to_file`
/// → `File::create(tmp)`，`tmp = 父目录.join(..)`），故父目录缺失 / 目标是现有目录必然
/// 失败——在菜单决策点（异步提取发起前）即置错，杜绝「成功 toast 恒假 + 失败双 toast」。
/// 空父路径（裸文件名）回退系统临时目录，放行。返回 `Some(错误文案)` = 注定失败。
/// wave8 评审 P1：目标 == 当前打开的文档路径也要拒——导出/拆分原子写盘会把盘上源文件
/// **覆盖成子集**（内存镜像不变、dirty 不置），原文件丢失且无撤销保护。
pub(super) fn extract_target_precheck(cur_path: &Path, out: &Path) -> Option<String> {
    if same_file_path(cur_path, out) {
        return Some(format!(
            "无法导出/拆分到 {}：不能覆盖当前打开的文档",
            out.display()
        ));
    }
    if let Some(parent) = out.parent()
        && !parent.as_os_str().is_empty() && !parent.is_dir() {
            return Some(format!(
                "无法提取到 {}：目标目录不存在（{}）",
                out.display(),
                parent.display()
            ));
        }
    if out.is_dir() {
        return Some(format!("无法提取到 {}：目标路径是现有目录", out.display()));
    }
    None
}

/// 合并源的**读端有界读盘**（wave8 评审 R7/P2-A2：把「元数据预检 + 整档读盘」从 pdfium
/// worker 线程挪到 view 侧后台线程，worker 只收字节消息——绝不让合并源读盘阻塞全局
/// pdfium 渲染/页操作队列）：
/// 1. **元数据预检**：`len > PDF_MERGE_MAX_SRC_BYTES` → 快拒（零分配）；
/// 2. **真边界读**：`Read::take(上限 + 1)` 兜住「元数据之后文件被外部追加」的 TOCTOU，
///    读满即判超限 Err——是 check-then-read 的真边界、非 `metadata` 一次 check-then-use。
///
/// 返回 `Err` 时绝无字节进 worker。超限/读失败统走 `last_modify_error` 既有错误面。
pub(super) fn read_merge_source_bounded(path: &Path) -> anyhow::Result<Vec<u8>> {
    use std::io::Read;
    let meta = std::fs::metadata(path)
        .map_err(|e| anyhow::Error::msg(format!("无法读取合并源 {}：{e}", path.display())))?;
    if meta.len() > PDF_MERGE_MAX_SRC_BYTES {
        return Err(anyhow::Error::msg(format!(
            "合并源过大（{} 字节，上限 {}）：{}",
            meta.len(),
            PDF_MERGE_MAX_SRC_BYTES,
            path.display()
        )));
    }
    let mut file = std::fs::File::open(path)
        .map_err(|e| anyhow::Error::msg(format!("无法读取合并源 {}：{e}", path.display())))?;
    // 有界读：最多读 `上限 + 1` 字节（再多也截断返回），读满判超限——超大源不会一次性
    // 整档进内存（未满上限的正常源自然读到 EOF 得完整内容）。
    let mut buf = Vec::new();
    file.by_ref()
        .take(PDF_MERGE_MAX_SRC_BYTES + 1)
        .read_to_end(&mut buf)?;
    if buf.len() as u64 > PDF_MERGE_MAX_SRC_BYTES {
        return Err(anyhow::Error::msg(format!(
            "合并源过大（实际读取 {} 字节，上限 {}）：{}",
            buf.len(),
            PDF_MERGE_MAX_SRC_BYTES,
            path.display()
        )));
    }
    Ok(buf)
}

/// 撤销栈「整档快照」条目预算（wave8 评审 R6）：连续页面操作在撤销栈里各持一份整档
/// `Vec<u8>` 镜像（内存 ~ 全档大小），无界增长会随操作数线性吃内存（N 次操作 = N × 档大
/// 小）。预算 = 最多保留 [`UNDO_MAX_MEM_ENTRIES`] 条 `RestoreMemory`，超出淘汰最旧一条
/// （`RemoveAnnot`/`AddAnnotBack` 注释逆操作大小恒定、不含整档镜像，不受此限）。
/// 推导：32 条对正常文档（<50MB）≈ 1.6GB 最坏上限、可回滚最近一轮批量操作；512MB 巨型
/// 档 × 32 = 16GB 理论最坏，仍比「无界随操作数增长」可控（save/另存时 `undo_stack.clear()`
/// 既有语义会整栈清空）。
const UNDO_MAX_MEM_ENTRIES: usize = 32;

/// 撤销栈预算裁剪（**纯逻辑，便于单测**，所有 `RestoreMemory` 入栈点唯一入口的底层）：
/// `RestoreMemory` 条目数 > [`UNDO_MAX_MEM_ENTRIES`] → 从最前**循环**淘汰最旧一条
/// RestoreMemory（其余类型条目保留原位置），直到条目数 ≤ 预算——幂等归位（生产按每次
/// push 调用，一次至多淘汰 1 条；测试可一次灌入多条目后单次归位）。保证栈内整档快照恒
/// ≤ 预算。
pub(super) fn enforce_undo_mem_budget(stack: &mut VecDeque<UndoEntry>) {
    loop {
        let mem_count = stack
            .iter()
            .filter(|e| matches!(e, UndoEntry::RestoreMemory { .. }))
            .count();
        if mem_count <= UNDO_MAX_MEM_ENTRIES {
            break;
        }
        if let Some(pos) = stack
            .iter()
            .position(|e| matches!(e, UndoEntry::RestoreMemory { .. }))
        {
            stack.remove(pos);
        } else {
            break;
        }
    }
}

impl PdfViewer {
    /// 创建阅读器并异步加载 `path`（读文件 + 解析 + 量页在后台，UI 不阻塞）。
    pub fn new(path: PathBuf, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            path,
            worker: None,
            doc_id: 0,
            dims: Vec::new(),
            page_tops: Vec::new(),
            total_height: 0.0,
            slots: Vec::new(),
            pending_thumb: HashMap::new(),
            pending_mid: HashMap::new(),
            mid_lru: VecDeque::new(),
            pending_tiles: HashMap::new(),
            pending_cover: HashMap::new(),
            pending_prefetch_cover: HashMap::new(),
            prefetch_reqs: 0,
            loading_placeholder_hits: std::cell::Cell::new(0),
            last_frame_tiles_drawn: std::cell::Cell::new(0),
            pending_recolor: None,
            raster_reqs: 0,
            recolor_scope: Vec::new(),
            recolor_received: Vec::new(),
            tile_reqs: 0,
            cover_reqs: 0,
            cover_re_renders: 0,
            tile_bytes: 0,
            tile_budget_state: None,
            degrade_frames: HashMap::new(),
            degraded: HashSet::new(),
            tile_budget_cap_override: None,
            tile_pump_running: false,
            last_scroll_y: 0.0,
            scroll_dir: 0,
            scroll_velocity: 0.0,
            prev_fast: false,
            speed_state: SpeedState::Idle,
            nav_gen: 0, // L1/L2：导航世代（view 唯一真源，worker 经 Navigate 镜像）
            jump_armed: false,
            jump_target: None,
            last_center_page: None,
            stop_small_frames: 0,
            zoom_armed: false,
            settle_tile_burst: false,
            settle_tile_burst_dispatch: 0,
            zoom_idle_frames: 0,
            zoom_epoch: 0,
            scroll: ScrollHandle::new(),
            x_zoom_offset: 0.0,
            last_mouse_focus: None,
            last_ui_scroll: (0.0, 0.0),
            state: ui::AsyncState::loading(),
            bg: None,
            recolor: None,
            recolor_epoch: 0,                // W1：随 set_recolor 实际变更 +1
            cover_failures: HashMap::new(),  // W2
            cover_blacklist: HashSet::new(), // W2
            gpu_recolor: crate::recolor::gpu_recolor_gate(),
            pending_drops: Vec::new(),
            outlines: Vec::new(),
            // 焦点句柄须由 Context 创建（gpui 内部注册），同 markdown 预览
            focus_handle: cx.focus_handle(),
            text_pages: HashMap::new(),
            text_lru: VecDeque::new(),
            pending_text: HashMap::new(),
            image_pages: HashMap::new(),
            image_lru: VecDeque::new(),
            pending_images: HashMap::new(),
            selected_image: None,
            sel_anchor: None,
            sel_active: None,
            selecting: false,
            search_query: String::new(),
            search_matches: Vec::new(),
            search_truncated: false,
            search_active: 0,
            search_pending: None,
            search_pending_seq: 0,
            search_seq: 0,
            zoom: 1.0,
            base_w: PAGE_WIDTH,
            doc_min_ar: 1.0,
            scale_factor: 1.0,
            window_sig: None,
            window_settled: false,
            idle_gate_skips: 0,
            reload_anchor: None,
            restore_anchor: None,
            restore_pending: None,
            last_modify_error: None,
            last_extract_success: None,
            sel_card: None,
            pending_annots: Vec::new(),
            undo_stack: VecDeque::new(),
            pending_page_ops: 0,
            next_annot_id: 0,
            focused_annot: None,
            panning: false,
            pan_origin: gpui::point(px(0.), px(0.)),
            pan_offset0: gpui::point(px(0.), px(0.)),
            // ---- 网格视图（Grid）状态（wave3）：见 view.rs 结构字段注释 ----
            grid_mode: false,
            page_mode: PageMode::Continuous,
            facing_anchor_col: 0,
            grid_zoom: 1.0,
            last_grid_viewport_w: 0.0,
            grid_scroll: ScrollHandle::new(),
            grid_thumbs: HashMap::new(),
            pending_grid: HashMap::new(),
            grid_last_range: std::cell::Cell::new((0, 0)),
            grid_last_drawn: std::cell::Cell::new(0),
            grid_placeholder_hits: std::cell::Cell::new(0),
            grid_storm: false,
            grid_last_offset: 0.0,
            grid_storm_idle_frames: 0,
            grid_scroll_dir: 0,
            grid_dispatch_order: Vec::new(),
            grid_bytes: 0,
            grid_prewarm_pushed: None,
            // wave5 网格多选：进入/退出网格时清空，见 `set_grid_mode`
            grid_selected: HashSet::new(),
            grid_anchor: None,
            // wave6 网格拖拽重组：无拖拽（残留安全默认），布局快照空（尚未渲染）
            grid_press: None,
            grid_drag: None,
            grid_layout_cell: std::cell::RefCell::new(None),
            paged_layout_cell: std::cell::RefCell::new(None),
            grid_hold_on_reload: false,
        };
        this.start_load(cx);
        this
    }

    /// 后台加载：读文件 + 经单例 pdfium 工作线程解析 + 量页 + 书签目录，完成后在主线程铺页槽。
    pub(super) fn start_load(&mut self, cx: &mut Context<Self>) {
        let path = self.path.clone();
        cx.spawn(async move |weak, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
                    // 进程级单例工作线程：打开文档 + 量页 + 书签目录，返回句柄与 DocId
                    let (worker, doc_id, dims, outlines) = PdfWorker::open_and_dims_full(bytes)
                        .map_err(|e| format!("无法启动 pdfium：{e}"))?;
                    Ok::<_, String>((worker, doc_id, dims, outlines))
                })
                .await;
            match result {
                Ok((worker, doc_id, dims, outlines)) => {
                    // 打开成功：worker 侧此刻已登记 `doc_id`（`docs`/`stores` 插表）。若实体在
                    // 加载中已被释放（关标签页 / 丢弃视图），`weak.update` 返回 Err → 无人落地
                    // `apply_loaded` → 也无人 close → 每次加载中关 tab 泄漏整份 pdfium 句柄 +
                    // 字节附件（`PdfWorker` 无 Drop 钩子，泄漏只能靠显式 Close 消息回收，H1）。
                    // 失败分支补上 close，防 worker 文档表残留。
                    if weak
                        .update(cx, |this, cx| {
                            this.apply_loaded(worker.clone(), doc_id, dims, outlines, cx);
                        })
                        .is_err()
                    {
                        worker.close(doc_id);
                    }
                }
                Err(e) => {
                    let _ = weak.update(cx, |this, cx| {
                        this.state.set_error(e);
                        cx.refresh_windows();
                        cx.notify();
                    });
                }
            }
        })
        .detach();
    }

    /// 打开成功后的统一装配（`start_load` 磁盘打开与待保存模型 `open_from_bytes`
    /// 内存打开共用）：铺页槽、旧位图转 pending_drops、恢复重载锚点、刷新窗口。
    pub(super) fn apply_loaded(
        &mut self,
        worker: PdfWorker,
        doc_id: DocId,
        dims: Vec<(f32, f32)>,
        outlines: Vec<PdfOutlineEntry>,
        cx: &mut Context<Self>,
    ) {
        self.worker = Some(worker);
        self.doc_id = doc_id;
        self.dims = dims;
        // 缓存全文档最小纵横比（contain-fit 用；读缓存字段，勿每帧 O(n) 全扫 dims）。
        self.doc_min_ar = min_ar(&self.dims);
        self.outlines = outlines;
        // 重着色补推：打开/重载时 worker 在 set_recolor 时刻可能尚未就绪，这里在 worker
        // 就位后**总是**把本阅读器的重着色状态按**本文档**推给它（worker 侧按 DocId 键控，
        // 多窗口/多标签各自独立）——保证新文档/重载后的栅格化与 view 状态一致（含 None
        // 复位：防某文档的全局重着色泄漏到其它文档）。**携带 view 世代**：接线前的多次
        // set_recolor 折叠成这一次补推，worker 靠消息里的最终世代对齐（评审 BLOCK）。
        if let Some(w) = self.worker.as_ref() {
            w.set_recolor(self.doc_id, self.recolor, self.recolor_epoch);
        }
        // 阅读进度恢复锚消费：先落 zoom，再把 `(页, 页内比例)` 暂存为待落地滚动——真实
        // scroll 施加延到**首个 ensure_window（base_w refit 后）**，因本刻 base_w 还是
        // `PAGE_WIDTH` 窗口宽回退值、非真实可视宽（refit 会重算 tops；若即刻 set_offset
        // 会按回退档 tops 算页，refit 后仍不搬 scroll → 落到错档/错页）。
        if let Some((page, frac, zoom)) = self.restore_anchor.take() {
            self.zoom = zoom.clamp(config::pdf::ZOOM_MIN, config::pdf::ZOOM_MAX);
            if !self.dims.is_empty() {
                self.restore_pending = Some((page, frac));
            }
        }
        let (tops, total) = compute_layout(&self.dims, self.page_width());
        self.page_tops = tops;
        self.total_height = total;
        // 替换槽位前，旧槽的 GPU 位图（封面/缩略/中清/瓦片）先转入 pending_drops，
        // 由下一帧 ensure_window 逐个 cx.drop_image 下线（仅丢 Arc 不 drop_image
        // 会累积显存提交，K14；写回重载路径此前漏此步 → 每次修改泄漏整页显存）。
        let mut old_images: Vec<std::sync::Arc<gpui::RenderImage>> = Vec::new();
        for mut slot in self.slots.drain(..) {
            if let Some(arc) = slot.cover.take() {
                old_images.push(arc);
            }
            if let Some(arc) = slot.prev_cover.take() {
                old_images.push(arc);
            }
            if let Some(arc) = slot.thumb.take() {
                old_images.push(arc);
            }
            if let Some(arc) = slot.mid.take() {
                old_images.push(arc);
            }
            for (_, ts) in slot.tiles.drain() {
                old_images.push(ts.image);
            }
        }
        self.pending_drops.extend(old_images);
        self.slots = vec![Slot::default(); self.dims.len()];
        // 文档重载/重开：网格模式复位到单列，旧网格缩略图与在途请求清空（旧 doc 尺寸
        // 已不适用，卡图与行带全作废）。
        self.grid_mode = false;
        self.clear_grid_resources();
        // wave5：页数已变 → 选中集/锚点按新页码集合作废（选中集是页码下标语义）。
        self.grid_selected.clear();
        self.grid_anchor = None;
        // wave6：拖拽态随文档作废；拖拽重排后的 reload 保持网格模式（见
        // `grid_hold_on_reload` 字段注释——拖拽重组应在网格内继续，不丢回单列）。
        self.grid_drag = None;
        self.grid_press = None;
        if self.grid_hold_on_reload {
            self.grid_hold_on_reload = false;
            self.grid_mode = true;
        }
        self.state.loading = false;
        // 重载锚点：恢复滚动到目标页（越界夹取到末页；None 保持首帧原位）
        if let Some(anchor) = self.reload_anchor.take() {
            let p = anchor.min(self.dims.len().saturating_sub(1));
            if let Some(top) = self.page_tops.get(p).copied() {
                self.scroll.set_offset(gpui::point(px(0.), px(-top)));
            }
        }
        if timing_view() {
            eprintln!(
                "[{:.1}ms] [VIEW-OPEN-DONE] pages={} tops0={:.1} total={:.1}",
                crate::pdf::timing_ms(),
                self.dims.len(),
                self.page_tops.first().copied().unwrap_or(0.0),
                self.total_height,
            );
        }
        // 关键：`refresh_windows()` 无条件要求重绘（不依赖实体被窗口 tracking）。
        // 仅 `notify()` 在实体未注册 window invalidator 时是空操作 → 首帧后
        // 永远不会再跑 `ensure_window` → 瓦片/封面从未被调度（“CPU 低、不变化”）。
        cx.refresh_windows();
        cx.notify();
    }

    /// 滚动到指定页（目录跳转）。页码越界时忽略。
    ///
    /// 分页页拍（Facing，R6）：目标页所在行 = `page / cols`，让该行**行顶对齐视口顶**
    /// （`offset = −row_top[r]×zoom`）；连续 = 既有 `page_tops[page]` 顶对齐。分页快照缺失
    /// （模式刚切、布局未装配）→ 回退连续口径（页列对齐，保证目录跳转永不落空）。
    pub fn scroll_to_page(&mut self, page_index: usize) {
        if page_index >= self.dims.len() {
            return;
        }
        // 跳页 = 确定性锚事件：复位非溢出水平偏移/沟槽兜底锚（回中，见复位规则）。
        self.reset_x_zoom();
        if self.paged_mode_active()
            && let Some(layout) = self.paged_layout_cell.borrow().clone()
                && layout.rows > 0 {
                    // 目录/导航跳页 = 确定性锚事件：把 sticky 列钉到所跳之页所在的列
                    // （`page % cols`），供 `reading_progress` 分页分支按同列解析「真正
                    // 当前页」——右/奇页跳页不丢列（K177：`col = page % cols`，row_page_start
                    // = r×cols 同源）。
                    if layout.cols > 0 {
                        self.facing_anchor_col = page_index % layout.cols;
                    }
                    let r = (page_index / layout.cols).min(layout.rows - 1);
                    let mag = (layout.row_top[r] * self.zoom.max(0.01))
                        .clamp(0.0, self.paged_max_scroll());
                    self.scroll.set_offset(gpui::point(px(0.), px(-mag)));
                    return;
                }
        let top = self.page_tops.get(page_index).copied().unwrap_or(0.0);
        self.scroll.set_offset(gpui::point(px(0.), px(-top)));
    }

    /// 把页列滚到「内容已滚动的像素数」（0 = 顶部；越界夹到文档量程内）。`#[doc(hidden)]`：
    /// 主要供 headless 集成测试构造「页内非零偏移」的阅读位置（生产滚动由滚轮 / 滚动条 /
    /// [`Self::scroll_to_page`] 目录跳页驱动）。
    #[doc(hidden)]
    pub fn scroll_to_top_px(&mut self, scroll_y: f32) {
        if self.dims.is_empty() {
            return;
        }
        let sy = scroll_y.clamp(0.0, self.total_height.max(0.0));
        self.scroll.set_offset(gpui::point(px(0.), px(-sy)));
    }

    /// 当前阅读位置：`(页下标 0 基, 页内比例, zoom)`；`None` = 文档未加载完成。
    ///
    /// **连续模式**：页内比例 = `(scroll_y − tops[K]) / 显示高`（`display_height(pw)`，随
    /// zoom/页宽伸缩）——绝不可以用 pt 高（页高 pt 与显示高相差一个 `pw/pt_w` 缩放因子，跨
    /// zoom 会失配）。与 [`Self::set_restore_anchor`] 的恢复公式互逆：同档往返无漂移。
    ///
    /// **分页模式**（Facing）：返回 `(真正当前页, 该页页内比例, zoom)`——`r0` =
    /// `first_visible_row` 首行，当前页 = `row_page_start[r0] + facing_anchor_col`
    /// （**sticky 列记忆**，0=左/偶、1=右/奇；末行 ragged 仅 1 页时夹回 0）。同行两列
    /// 顶对齐 → y 轴无法区分「在哪一列」，必须靠确定性锚事件（[`Self::set_page_mode`]
    /// restore / Facing `scroll_to_page`）把列钉进 `facing_anchor_col` 再解析。frac =
    /// `(内容顶 − 行顶) / cell_h[page]`（**页内比例**，分母用该页自身高，非行高——与
    /// `ensure_window` 分页 restore 同分母互逆，连续 ↔ Facing 的 `(page, frac)` 互为精确逆；
    /// 与 zoom 无关：内容顶/行顶/页高同为显示口径，zoom 约掉）。
    pub fn reading_progress(&self) -> Option<(usize, f32, f32)> {
        if self.dims.is_empty() || self.page_tops.len() != self.dims.len() {
            return None;
        }
        let scroll_y = f32::from(-self.scroll.offset().y);
        // 分页分支：首可见行 → (真正当前页, 页内比例)。快照缺失/空布局回落连续口径。
        if self.paged_mode_active()
            && let Some(layout) = self.paged_layout_cell.borrow().as_ref()
            && layout.rows > 0
        {
            let zoom = self.zoom.max(0.01);
            let content_top = scroll_y.max(0.0);
            let vh = f32::from(self.scroll.bounds().size.height) / zoom;
            // 布局（zoom=1）负偏移口径：内容显示 = 布局 × zoom → 布局滚动偏移 = 显示 / zoom。
            let scroll_y_fit = -(content_top / zoom);
            if let Some((r0, _)) = layout.first_visible_row(scroll_y_fit, vh) {
                let row_top = layout.row_top[r0];
                // 页解析（K177 演算）：行页区间 `[row_page_start[r0], row_page_start[r0+1])`，
                // 列 = sticky `facing_anchor_col`（0/1），末行 ragged 仅 1 页 → 夹回列 0。
                let pages_in_row = layout.row_page_start[r0 + 1] - layout.row_page_start[r0];
                let col = self.facing_anchor_col.min(pages_in_row.saturating_sub(1));
                let page = layout.row_page_start[r0] + col;
                // 页内比例：分母 = 该页自身高 `cell_h[page]`（非行高）——与 ensure_window
                // 分页 restore 同分母，行内两页不等高时把「页内»行内」换算收进该页自身。
                let cell_h = layout.cell_h.get(page).copied().unwrap_or(1.0).max(f32::EPSILON);
                let frac = ((content_top / zoom - row_top) / cell_h).clamp(0.0, 1.0);
                return Some((page, frac, self.zoom));
            }
        }
        // 连续分支（原样）。
        let k = first_visible_page(&self.dims, &self.page_tops, self.page_width(), scroll_y);
        let top = self.page_tops.get(k).copied().unwrap_or(0.0);
        let disp_h = display_height(self.dims[k], self.page_width()).max(1.0);
        let frac = ((scroll_y - top) / disp_h).clamp(0.0, 1.0);
        Some((k, frac, self.zoom))
    }

    /// 设置「阅读进度恢复锚」（外壳打开 PDF 时写入；`apply_loaded` 装配后消费）。
    /// `page_index` = 页下标（消费时夹取到合法页）；`in_page_frac` 与 `zoom` 在此防呆 clamp
    /// （比例 [0,1]、zoom [ZOOM_MIN, ZOOM_MAX]）。
    pub fn set_restore_anchor(&mut self, page_index: usize, in_page_frac: f32, zoom: f32) {
        self.restore_anchor = Some((
            page_index,
            in_page_frac.clamp(0.0, 1.0),
            zoom.clamp(ZOOM_MIN, ZOOM_MAX),
        ));
    }

    /// 程序化设置**阅读缩放**倍数（clamp 到 [ZOOM_MIN, ZOOM_MAX]），并按
    /// [`layout::zoom_rescroll_anchor`] 把当前滚动重锚到**页内显示比例不变**（页顶线性缩放 +
    /// 页内偏移同比）。`#[doc(hidden)]`：供 headless 集成测试「跨 zoom 校验阅读比例语义」用；
    /// 真实交互缩放走 `adjust_zoom`（含 worker 世代 / 调度状态机 / notify）。**只改阅读缩放，
    /// 不联动网格卡宽**（网格卡宽独立由 `grid_zoom` 派生，见 [`Self::grid_card_width`]）。
    #[doc(hidden)]
    pub fn set_zoom(&mut self, zoom: f32) {
        if self.dims.is_empty() {
            return;
        }
        let zoom = zoom.clamp(ZOOM_MIN, ZOOM_MAX);
        if (self.zoom - zoom).abs() < 1e-4 {
            return;
        }
        // 程序化绝对缩放（非鼠标焦点）→ 复位非溢出水平偏移（回中）。
        self.reset_x_zoom();
        let old_w = self.page_width();
        self.zoom = zoom;
        let new_w = self.page_width();
        let ratio = new_w / old_w;
        let sy = f32::from(-self.scroll.offset().y);
        let old_tops = std::mem::take(&mut self.page_tops);
        let (new_tops, total) = compute_layout(&self.dims, new_w);
        self.page_tops = new_tops;
        self.total_height = total;
        // 与新布局量程一致的 clamp（防停底锚点越界假速度，同 adjust_zoom #2 语义）。
        let viewport_h = f32::from(self.scroll.bounds().size.height);
        let max_sy = (self.total_height - viewport_h).max(0.0);
        let new_y = zoom_rescroll_anchor(&self.dims, &old_tops, &self.page_tops, old_w, ratio, sy)
            .clamp(0.0, max_sy.max(self.total_height.max(0.0)));
        self.scroll.set_offset(gpui::point(
            px(-f32::from(self.scroll.offset().x)),
            px(-new_y),
        ));
        self.last_scroll_y = new_y;
        // 网格卡宽独立于阅读缩放（`grid_card_width() = GRID_CARD_WIDTH × grid_zoom`），
        // 故 `set_zoom` **不再联动网格**——网格卡宽只由 `set_grid_zoom`/网格 Ctrl+滚轮改。
    }

    /// 应用**绝对**阅读缩放目标（`target` 直接入参，clamp 到 [ZOOM_MIN, ZOOM_MAX]）：
    /// **无鼠标焦点**（状态栏「显示比例」滑条 / 程序化）→ 视口顶锚（Z1 / Z1-S6 语义）；
    /// 真实 Ctrl+滚轮带鼠标焦点走 [`Self::zoom_target_mouse`]。两者落地共用
    /// [`Self::zoom_target_impl`]（同一实现，避免逻辑分叉，K32）。
    /// `pub`：外壳状态栏「显示比例」滑条经 entity update 调用（滑条先 clamp，这里再兜底）。
    pub fn zoom_target(&mut self, target: f32, cx: &mut Context<Self>) {
        self.zoom_target_impl(target, None, None, cx);
    }

    /// 鼠标焦点缩放落地（真实 Ctrl+滚轮专属，见
    /// [`super::view::input::PdfViewer::adjust_zoom_mouse`]）：`focus = None`（鼠标在沟槽/
    /// 留白/状态栏 → [`super::view::input::PdfViewer::window_to_pt`] 未命中）→ 与无焦点
    /// [`Self::zoom_target`] 完全一致（视口顶锚）。鼠标锚演算见 [`Self::zoom_target_impl`]。
    pub(super) fn zoom_target_mouse(
        &mut self,
        target: f32,
        focus: Option<ZoomMouseFocus>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        self.zoom_target_impl(target, focus, Some(window), cx);
    }

    /// 缩放落地共享实现（`zoom_target` / `zoom_target_mouse` 二合一）——维护缩放后一致性：
    /// Z2 worker 缩放世代步进、Z1 锚点重锚、S1 缩放源头治理（防抖武装 + 强制 Slow）、
    /// notify 全在这里。行为与 adjust_zoom **逐行一致**，仅把「target 来自 factor×当前
    /// zoom」改为「target 直接入参」；clamp 后与当前 zoom 差 < 1e-4 早退（同
    /// [`pan_zoom::apply_zoom`] 语义）。`mouse_focus` = `Some((焦点, window))` 时缩放宽
    /// 锚改为「鼠标所指页面点」（缩放前后同内容点同屏），`None` 时维持视口顶锚。
    fn zoom_target_impl(
        &mut self,
        target: f32,
        mouse_focus: Option<ZoomMouseFocus>,
        window: Option<&Window>,
        cx: &mut Context<Self>,
    ) {
        if self.dims.is_empty() {
            return;
        }
        let old_w = self.page_width();
        let target = target.clamp(ZOOM_MIN, ZOOM_MAX);
        if (target - self.zoom).abs() < 1e-4 {
            return;
        }
        // **D1（评审）程序化/滑条绝对缩放**：`window == None` ⇔ 无鼠标焦点（`set_zoom_abs`
        // 状态栏滑条 / `adjust_zoom` 测试驱动）。滑条 = 非「鼠标缩放」水平位移 → 复位非溢出
        // 水平偏移与沟槽兜底锚（回中，与 [`Self::set_zoom`] 一致）。此前只判
        // `last_mouse_focus.is_none()` 会因上一档页内 Ctrl+滚轮留下的 `Some` 而不清 → 页列
        // 保持水平偏移不回中（fit 下可达 ±居中边距）。`focus Some ⇒ window Some`（生产调用面），
        // 此守卫不会误伤真实滚轮。
        if window.is_none() {
            self.reset_x_zoom();
        }
        // **R5 分页精确锚（S6）**：缩放前捕获「首可见行 r0 + 显示口径滚动 sy_old + zoom=1
        // 布局快照 pl_fit」。分页几何 = PagedLayout × zoom（均匀缩放、无常量区）→ 缩放后行号
        // 不变（rows 由页数/cols 决定），行内页相对位置按 ratio 线性。缩放后用
        // `zoom_rescroll_anchor_paged(pl_old, pl_new, r0, ratio, sy_old)` 精确重锚（pl_old/pl_new
        // = pl_fit 分别乘旧/新 zoom 的显示口径布局）。
        let old_zoom = self.zoom;
        let paged_anchor = if self.paged_mode_active() {
            self.paged_layout_cell.borrow().as_ref().and_then(|l| {
                if l.rows == 0 || old_zoom <= 0.01 {
                    return None;
                }
                let content_top = f32::from(-self.scroll.offset().y).max(0.0);
                let scroll_y_fit = -(content_top / old_zoom);
                let vh_fit = f32::from(self.scroll.bounds().size.height) / old_zoom;
                l.first_visible_row(scroll_y_fit, vh_fit)
                    .map(|(r0, _)| (r0, content_top, l.clone()))
            })
        } else {
            None
        };
        self.zoom = target;
        let new_w = self.page_width();
        let ratio = new_w / old_w;
        // Z2 缩放世代步进（view 唯一真源）→ 推送 worker 采纳镜像。渲染请求随后 stamp 新世代；
        // 停在 worker 队列的缩放**中间档** cover/瓦片/preview 出队时 `stamp < mirror` → 直接
        // drop 不渲（旧档不再排队空耗 pdfium 时隙；最终档必达、无中间档堆叠）。
        self.zoom_epoch += 1;
        if let Some(w) = self.worker.clone() {
            w.set_zoom_epoch(self.doc_id, self.zoom_epoch);
        }
        // Z1 锚点重锚：页列布局随 page_width 线性缩放但 `PAGE_PAD_Y`/`PAGE_GAP` 常量不缩放
        // → 旧实现「整个 offset × ratio」把常量区也当线性量乘，每档漂移
        // `-(ratio-1)×(PAD+K×GAP)`（K=锚点页号，深页可跳 2-3 行高，放大上跳/缩小下跳离散跳）。
        // 改为「锚定 scroll 落在的页 K：页顶线性缩放 + 页内相对偏移同比缩放」（纯函数
        // [`layout::zoom_rescroll_anchor`] 锁死公式）。横向量列居中同款同比缩放。
        let sx = f32::from(-self.scroll.offset().x);
        let sy = f32::from(-self.scroll.offset().y);
        let old_tops = std::mem::take(&mut self.page_tops);
        let (new_tops, total) = compute_layout(&self.dims, new_w);
        self.page_tops = new_tops;
        self.total_height = total;
        // 分页锚存在性提前探（paged_anchor 随后被 move 进 if-let）。
        let has_paged_anchor = paged_anchor.is_some();
        // **鼠标焦点锚**（缩放目标 = 鼠标所指页面点，连续 / Facing 两模式同式）：缩放前
        // `window_to_pt` 已把鼠标定位到页内 PDF 点 `focus`。缩放后先求该点在「新布局 +
        // 旧滚动」下的窗口位 `after`（[`super::view::input::PdfViewer::pt_to_window`]，与
        // 命中/渲染共用同一条几何链），再令 `新滚动 = 旧滚动 + (after − 鼠标窗位)`——
        // 窗口位与 scroll 是 `窗口 = 容器原点 + content − scroll` 的线性关系，由此线性和
        // 可逆：`after = b + c′ − s_old`，要 `b + c′ − s_new = m` ⇒ `s_new = s_old + (after − m)`，
        // 两轴同式（K177 演算）。`page_local_to_window` 此刻读的是**尚未 set_offset 的旧
        // 滚动** → 恰与 `after` 定义吻合。
        //
        // **三态锚**（2026-11-20 非溢出 x 锚 + 沟槽兜底）：返回值 = `(new_y 锚, x 增量)`：
        // ① 本档 `window_to_pt` 命中页内 → 更新 `last_mouse_focus`，y 锚 + `Some(delta)`（x
        //    delta = `after.x − m.x`，**纯几何增量**——新旧通道值在 `after − m` 中自动相消）；
        // ② 本档命中空白（鼠标落沟槽/留白）但上一档在页内 → 用上一档页内内容点锚 **y**，
        //    **x 增量 None**（维持既有通道值，防「x 重居中把鼠标推出页缘 → 瞬时回落视口顶锚」）；
        // ③ 无任何页内焦点 → `(None, None)`（视口顶锚，既有 Z1/Z1-S6 语义）。
        // x 的**通道结算**在下方 `max_sx` 分派统一做（容器 = `column_left + xoff − sx`，两模型
        // 通道值各自归位；跨档须扣除被移除通道的旧值，见 D2）。
        let (mouse_y, x_delta): (Option<f32>, Option<f32>) = match mouse_focus {
            Some(focus) => {
                self.last_mouse_focus = Some(focus);
                match window.map(|w| {
                    let after = self.pt_to_window(w, focus.page, focus.px, focus.py);
                    (
                        sy + (after.y.as_f32() - focus.pos.y.as_f32()),
                        after.x.as_f32() - focus.pos.x.as_f32(),
                    )
                }) {
                    Some((ny, dx)) => (Some(ny), Some(dx)),
                    None => (None, None),
                }
            }
            None => match (self.last_mouse_focus, window) {
                (Some(prev), Some(w)) => {
                    let after = self.pt_to_window(w, prev.page, prev.px, prev.py);
                    // 兜底：y 锚到上一档页内内容点；x 增量 None（维持既有通道值，下方结算）。
                    (Some(sy + (after.y.as_f32() - prev.pos.y.as_f32())), None)
                }
                _ => (None, None),
            },
        };
        let new_y = match mouse_y {
            Some(ny) => ny,
            None => {
                if let Some((r0, sy_old, pl_fit)) = paged_anchor {
                    // 分页精确锚：pl_old/pl_new = pl_fit × 旧/新 zoom（显示口径），
                    // `zoom_rescroll_anchor_paged` 锁死 `行顶_new + (sy_old − 行顶_old) × ratio`。
                    let pl_old = paged_layout_scaled(&pl_fit, old_zoom);
                    let pl_new = paged_layout_scaled(&pl_fit, self.zoom.max(0.01));
                    zoom_rescroll_anchor_paged(&pl_old, &pl_new, r0, ratio, sy_old)
                } else {
                    zoom_rescroll_anchor(&self.dims, &old_tops, &self.page_tops, old_w, ratio, sy)
                }
            }
        };
        // #2 停底锚点回拉修正：Z1 公式对页尾/文档底附近给出的 `new_y` 可能超出新布局的
        // `[0, max_offset]` ——gpui 只在**布局**时才 clamp（div.rs `clamp_scroll_position`），
        // 本帧读到的是未夹值：视觉已停底、`last_scroll_y` 却记了越界假值 → 下一帧
        // `ensure_window` 算 jump≈「max−越界」假速度（S1 复核期失真）+ 再放大反向补一跳。
        // 这里按新布局量程先 clamp，并把 `last_scroll_y` 回写为 clamp 后值（防 S1 假速度），
        // 横向量程同款（内容宽 = 可视宽与页宽+边距的较大者，`zoom==1` 适配宽时恒 0）。
        // 分页页拍的纵向量程按 PagedLayout 总高 × zoom（分页滚动范围 ≠ 连续 total_height）。
        let viewport_h = f32::from(self.scroll.bounds().size.height);
        let max_sy = if has_paged_anchor {
            self.paged_layout_cell
                .borrow()
                .as_ref()
                .map(|l| (l.total_height * self.zoom.max(0.01) - viewport_h).max(0.0))
                .unwrap_or_else(|| (self.total_height - viewport_h).max(0.0))
        } else {
            (self.total_height - viewport_h).max(0.0)
        };
        let new_y = new_y.clamp(0.0, max_sy);
        let viewport_w = f32::from(self.scroll.bounds().size.width);
        // 横向内容宽 = 可视宽与内容更宽者的较大者。**分页页拍的容器内容宽 = `spread_max ×
        // zoom`**（渲染容器 `.w(spread_max*zoom)`，见 render_pdf_pages_paged）——若按连续
        // 的 `page_width + 2·PAD` 算会把横向量程算小（分页双页 + gap 常远超单页宽）→ 分页
        // 横向缩放锚被错误早钳（鼠标 x 失守，且 `new_x` clamp 与 DOM 实测范围不符）。
        let content_w = if self.paged_mode_active() {
            let spread = self
                .paged_layout_cell
                .borrow()
                .as_ref()
                .map(|l| l.spread_max * self.zoom.max(0.01))
                .unwrap_or_else(|| new_w + 2.0 * PAGE_PAD_X);
            viewport_w.max(spread)
        } else {
            viewport_w.max(new_w + 2.0 * PAGE_PAD_X)
        };
        let max_sx = (content_w - viewport_w).max(0.0);
        // **横向量程通道统一结算**（渲染 / 命中 / 横向量程三处同口径，K153；D2：跨档不跳）：
        // 屏幕 x 恒等式 `m = 容器原点 + column_left + xoff − sx`；要 `column_left(w1)+lx1 +
        // new_xoff − new_sx = m` ⇒ `new_xoff − new_sx = (old_xoff − old_sx) − delta`（delta =
        // 几何增量，`after − m` 已把 old 通道值相消，见上）。据此：
        // - **进溢出**（old 通道是 offset：old_xoff ≠ 0、old_sx = sx）→ scroll 的
        //   `new_sx = sx + delta − old_xoff`（**扣除被移除的偏移通道旧值**，否则当档鼠标 x
        //   带 xoff_old 赤 失守几十 px）；
        // - **回非溢出**（old 通道是 scroll：old_xoff = 0、old_sx = sx ≠ 0）→ offset 的
        //   `new_xoff = old_xoff − sx − delta`（**扣除被移除的滚动通道旧值**，否则带 sx_old 漂移）。
        // 稳态（同通道连续缩放）上式退化为「纯 delta / offset −= delta」，与既有行为逐值一致。
        // 沟槽兜底（x 增量 None 且 y 有锚）维持既有通道值；顶锚（y 无锚）溢出走 `sx × ratio`、
        // 非溢出归 0 回中。`last_mouse_focus` 在溢出期清空（横向完全由 scroll 承担）、非溢出期
        // 保留给连续 Ctrl+滚轮序列。
        let old_xoff = self.x_zoom_offset;
        let x_top = mouse_y.is_none(); // 顶锚（无任何页内焦点）
        let x_keep = !x_top && x_delta.is_none(); // 沟槽兜底：维持既有通道值
        if max_sx > 1e-3 {
            // 横向溢出：横向锚走既有滚动量程 `[0, max_sx]`，偏移恒 0。
            self.x_zoom_offset = 0.0;
            self.last_mouse_focus = None;
            let new_x = if let Some(delta) = x_delta {
                (sx + delta - old_xoff).clamp(0.0, max_sx)
            } else if x_keep {
                sx.clamp(0.0, max_sx)
            } else {
                (sx * ratio).clamp(0.0, max_sx)
            };
            self.scroll.set_offset(gpui::point(px(-new_x), px(-new_y)));
        } else {
            // 非溢出：横向滚动恒 0，横向由 `x_zoom_offset` 承担（居中 = 0，正值 = 页列右移）。
            let max_xoff = self.nonoverflow_max_xoff(new_w);
            self.x_zoom_offset = if let Some(delta) = x_delta {
                (old_xoff - sx - delta).clamp(-max_xoff, max_xoff)
            } else if x_keep {
                old_xoff
            } else {
                0.0
            };
            self.scroll.set_offset(gpui::point(px(0.), px(-new_y)));
        }
        // S1 缩放源头治理：重锚把 scroll offset 同比缩放后，滚动速度「账面」与滚动锚点立即
        // 同步——下一帧 `ensure_window` 的 `jump = scroll_y - last_scroll_y ≈ 0`，不再把缩放
        // 重锚误判为滚动（旧实现：同帧 offset 突变 → velocity 飙 → VeryFast → 手势期零派发
        // + 2-4 帧「加载中」空白 + 屏显恒为手势前旧封面 1.12^k 复利拉伸）。
        // 强制 Slow 保证该缩放事件帧不被高速带缠住；`zoom_armed` 置位令缩放防抖窗内调度
        // 只派当前档 preview（绝不满密 cover/tile 逐档抢渲被弃空转），防抖到期才满密收口。
        // `last_scroll_y` = clamp 后的 `new_y`（#2：与写入 scroll 的 offset 一致，防 S1 假速度）。
        self.last_scroll_y = new_y;
        self.scroll_velocity = 0.0;
        self.speed_state = SpeedState::Slow;
        self.prev_fast = false;
        self.zoom_armed = true;
        self.zoom_idle_frames = 0;
        cx.notify();
    }

    /// 复位**非溢出档水平内容偏移**与沟槽兜底锚（归 0 回中）：普通滚动 / 跳页 /
    /// 模式切换 / 进出网格 / 滑条绝对缩放等任何非「鼠标缩放」的水平位移统一走本函数
    /// （复位规则见 [`Self::x_zoom_offset`] / [`Self::last_mouse_focus`]）。
    pub(super) fn reset_x_zoom(&mut self) {
        self.x_zoom_offset = 0.0;
        self.last_mouse_focus = None;
    }

    /// **非溢出档水平内容偏移的上界**（[`Self::x_zoom_offset`] 的夹幅）：= 页列居中边距
    /// `(视口 − 内容宽)/2`（连续内容宽 = 页宽，分页内容宽 = spread）。非溢出下偏移幅值不许
    /// 超过它——超过即「页列负 padding / 页出视口」（渲染 padding/margin 与命中 `column_left
    /// ± offset` 同界，K153/K177）。仅在 `zoom_target_impl` 非溢出分派里调用（此时必 ≥ 0）。
    pub(super) fn nonoverflow_max_xoff(&self, new_w: f32) -> f32 {
        let viewport_w = f32::from(self.scroll.bounds().size.width);
        if self.paged_mode_active()
            && let Some(l) = self.paged_layout_cell.borrow().as_ref() {
                let spread = l.spread_max * self.zoom.max(0.01);
                return ((viewport_w - spread) / 2.0).max(0.0);
            }
        ((viewport_w - new_w) / 2.0).max(0.0)
    }

    /// 外壳状态栏「显示比例」滑条驱动：以**绝对缩放值**设置阅读缩放。
    /// 滑条值域（0.5..4.0）已由滑条映射好，这里仍走 [`Self::zoom_target`] 内 clamp 兜底。
    pub fn set_zoom_abs(&mut self, value: f32, cx: &mut Context<Self>) {
        self.zoom_target(value, cx);
    }

    /// 页数（文档尚未加载完时为 0）。P4 拆 crate 后根包 app::tests 亦消费，取消 `cfg(test)`。
    pub fn page_count(&self) -> usize {
        self.dims.len()
    }

    /// 当前文档路径（Phase B 书签按路径关联持久化用）。
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// 重命名同步：文件被外部改名后更新阅读器内部路径（标题 / 保存写回目标随之正确）。
    /// 文件内容未变，`worker`/`doc_id` 保持有效，无需重载。
    pub fn set_path(&mut self, path: PathBuf) {
        self.path = path;
    }

    /// 当前第一张可见页（1 基）与总页数（状态栏滚动位置显示用）。
    /// 仅派生自既有滚动状态（`scroll` 偏移 + 页列布局 [`first_visible_page`]），不新增
    /// 状态；文档未加载完成（`dims` 为空）时返回 `None`。与 EPUB 页码口径一致：
    /// 「第一张可见页」如实反映视口，不含 `keep_window` 的 MARGIN 缓冲。
    ///
    /// **分页模式**（Facing）：返回 `(行首页下标 + 1 = row_page_start[r0] + 1, 总页数)`。
    pub fn current_page(&self) -> Option<(usize, usize)> {
        if self.dims.is_empty() {
            return None;
        }
        // 分页分支：首可见行 r0 → 行首页 +1（1 基）。快照缺失/空布局回落连续。
        if self.paged_mode_active()
            && let Some(layout) = self.paged_layout_cell.borrow().as_ref()
            && layout.rows > 0
        {
            let zoom = self.zoom.max(0.01);
            let content_top = f32::from(-self.scroll.offset().y).max(0.0);
            let vh = f32::from(self.scroll.bounds().size.height) / zoom;
            let scroll_y_fit = -(content_top / zoom);
            if let Some((r0, _)) = layout.first_visible_row(scroll_y_fit, vh) {
                let page = layout.row_page_start[r0] + 1;
                return Some((page, self.dims.len()));
            }
        }
        // 首帧 `scroll` 偏移为 0 → `first_visible_page` 起始页即第 1 页，布局后精确。
        let scroll_y = f32::from(-self.scroll.offset().y);
        let first = first_visible_page(&self.dims, &self.page_tops, self.page_width(), scroll_y);
        Some((first + 1, self.dims.len()))
    }

    /// 网格模式滚动容器可视区 `(宽, 高)`：容器已布局回真实 bounds；首帧 bounds=0 回退
    /// 窗口尺寸（与 `wrap_width` 首帧口径一致），两轴夹 ≥1 防退化布局。
    pub(super) fn grid_viewport(&self, window: &Window) -> (f32, f32) {
        let bw = f32::from(self.grid_scroll.bounds().size.width);
        let bh = f32::from(self.grid_scroll.bounds().size.height);
        let w = if bw > 0.0 {
            bw
        } else {
            window.bounds().size.width.as_f32()
        };
        let h = if bh > 0.0 {
            bh
        } else {
            window.bounds().size.height.as_f32()
        };
        (w.max(1.0), h.max(1.0))
    }

    /// 分页页排（Facing）布局视口 `(宽, 高)`：读取**阅读滚动容器**（`scroll`）真实
    /// bounds——分页页排复用阅读 scroll，故视口源与连续阅读一致（不同于网格的
    /// [`grid_viewport`] 读 `grid_scroll`）；首帧 `scroll.bounds()` 为 0 回退窗口尺寸
    /// （同 [`Self::wrap_width`] 首帧口径），两轴夹 ≥1 防退化布局。
    pub(super) fn paged_viewport(&self, window: &Window) -> (f32, f32) {
        let bw = f32::from(self.scroll.bounds().size.width);
        let bh = f32::from(self.scroll.bounds().size.height);
        let w = if bw > 0.0 {
            bw
        } else {
            window.bounds().size.width.as_f32()
        };
        let h = if bh > 0.0 {
            bh
        } else {
            window.bounds().size.height.as_f32()
        };
        (w.max(1.0), h.max(1.0))
    }

    /// 网格**请求卡宽**（逻辑 px）：由**独立网格缩放 `grid_zoom`** 派生（`GRID_CARD_WIDTH ×
    /// grid_zoom`），**独立于单列阅读缩放 `self.zoom`**——网格 Ctrl+滚轮只改 `grid_zoom`
    /// （卡宽/列数变化 + 缩略图按新分桶重取，view/input.rs `on_scroll_wheel` 网格臂），
    /// 不碰阅读 `zoom`；阅读 Ctrl+滚轮 / `set_zoom` 也不再联动网格卡宽。render 布局 /
    /// tiles 调度 / evict 三处都走本口径，保证「物化 = 派发 = 保留」同一几何。grid_zoom
    /// 按 `grid_zoom_limits_for_cols` **列数钳制**（2/9 列，见 [`Self::set_grid_zoom`]）。
    pub(crate) fn grid_card_width(&self) -> f32 {
        GRID_CARD_WIDTH * self.grid_zoom
    }

    /// 程序化设置**网格独立缩放** `grid_zoom`（按当前视口宽**列数钳制**到
    /// `[GRID_MIN_COLS, GRID_MAX_COLS]` 列，见 [`grid_zoom_limits_for_cols`]），同比重锚
    /// `grid_scroll`（保位）+ **仅跨分桶**清缩略图（同 input.rs 网格 Ctrl+滚轮臂口径）+
    /// `notify`。`#[doc(hidden)]`：供 headless 网格缩放独立性测试用；真实交互走网格
    /// Ctrl+滚轮（input.rs `on_scroll_wheel` 网格臂），只改 `grid_zoom`、不碰阅读 `self.zoom`。
    #[doc(hidden)]
    pub fn set_grid_zoom(&mut self, zoom: f32, cx: &mut Context<Self>) {
        let old_card = self.grid_card_width();
        let (z_min, z_max) = grid_zoom_limits_for_cols(
            GRID_CARD_WIDTH,
            GRID_GAP,
            GRID_PADDING,
            self.grid_zoom_viewport_w(),
            GRID_MAX_COLS,
            GRID_MIN_COLS,
        );
        let zoom = zoom.clamp(z_min, z_max);
        if (self.grid_zoom - zoom).abs() < 1e-4 {
            return;
        }
        self.grid_zoom = zoom;
        let new_card = self.grid_card_width();
        if (new_card - old_card).abs() > 1e-3 {
            let ratio = new_card / old_card;
            let sy = f32::from(-self.grid_scroll.offset().y) * ratio;
            self.grid_scroll.set_offset(gpui::point(px(0.), px(-sy)));
            if grid_bucket_width(new_card) != grid_bucket_width(old_card) {
                // 跨分桶：保图清在途——旧桶缩略图保留作桥（渲染拉伸顶住，防整屏占位白闪），
                // 只丢弃旧桶在途请求；新桶图异步到达后替换桥。
                self.reset_grid_dispatch_for_zoom_bucket();
            }
            // 本批「带宽预熟」配合：卡宽变化推 worker 更新预熟目标（跨桶 → worker 端按桶级
            // 比较发现 target bucket 变了、从新中心重扫新桶；同桶小步缩放桶不变不动扫描）。
            self.push_grid_prewarm(None);
        }
        cx.notify();
    }

    /// **带宽预熟几何**（本线）：按「渲染同口径」（[`grid_layout`] + 真实视口/滚动偏移）
    /// 计算当前视口的
    /// - `center_page` = 可视行带中心行首页（外向扫描对称中心）；
    /// - `near` = 中心行 ±[`config::pdf::GRID_NBAND_ROWS`] 行的页区间（**完整覆盖可视带 +
    ///   保留窗** GRID_ROW_BUFFER + GRID_KEEP_ROWS = ±4 行——「可视带永不失图」不变式，
    ///   worker 近带保位图、view keep_range 保纹理，回看零重栅）；
    /// - `window` = 中心行 ±(近+中)行的页区间（中带 = window \ near 仅低清）。
///
    /// 无可见行（视口在内容外 / 空文档 / 未布局）→ `None`（本帧不推，下帧可重算）。
    /// `window` 为 `None` 时用 `grid_scroll` 自身 bounds 口径（进入网格/缩放无 Window 路径）。
    pub(crate) fn grid_prewarm_geom(&self, window: Option<&Window>) -> Option<GridPrewarmGeom> {
        if self.dims.is_empty() {
            return None;
        }
        let (vw, vh) = match window {
            Some(w) => self.grid_viewport(w),
            None => {
                let bw = f32::from(self.grid_scroll.bounds().size.width);
                let bh = f32::from(self.grid_scroll.bounds().size.height);
                (bw.max(1.0), bh.max(1.0))
            }
        };
        let layout = grid_layout(
            &self.dims,
            self.grid_card_width(),
            GRID_GAP,
            GRID_PADDING,
            vw,
        );
        if layout.rows == 0 {
            return None;
        }
        let off = f32::from(-self.grid_scroll.offset().y);
        let (r0, r1) = layout.first_visible_grid_row(off, vh)?;
        let rows = layout.rows;
        let center_row = ((r0 + r1) / 2).min(rows.saturating_sub(1));
        let n = GRID_NBAND_ROWS as isize;
        let m = (GRID_NBAND_ROWS + GRID_MBAND_ROWS) as isize;
        let center_row = center_row as isize;
        let near_lo_row = (center_row - n).max(0) as usize;
        let near_hi_row = ((center_row + n) as usize).min(rows - 1);
        let lo_row = (center_row - m).max(0) as usize;
        let hi_row = ((center_row + m) as usize).min(rows - 1);
        // 行 → 页闭区间（row_page_start[len]=总页数；闭区间端点 -1 并 min 夹回文档末页）。
        let last_page = self.dims.len().saturating_sub(1);
        let near = (
            layout.row_page_start[near_lo_row],
            layout.row_page_start[near_hi_row + 1].saturating_sub(1).min(last_page),
        );
        let window_interval = (
            layout.row_page_start[lo_row],
            layout.row_page_start[hi_row + 1].saturating_sub(1).min(last_page),
        );
        let center_page = layout.row_page_start[center_row as usize];
        Some(GridPrewarmGeom {
            center_page,
            window: window_interval,
            near,
        })
    }

    /// **带宽预熟推送（节流）**（本线）：进入网格 / 卡宽变化 / 每帧滚动重定中心共用——
    /// 计算当前几何并 `set_grid_prewarm` 下发给 worker；**节流** = `(中心页, 窗口, 近带, 全清
    /// 桶)` 全部未变（同一窗口内微移/静止）→ 幂等跳过（不每帧刷 worker、不打断推进中的
    /// 预熟）。几何或跨桶变化才推。`window`：`Some` = 有真实 Window（每帧 `ensure_window_grid`），
    /// `None` = 进入网格/缩放无 Window 路径（用 `grid_scroll` 自身 bounds 口径；首帧未布局
    /// 几何不可得 → 本帧不推，下帧 `ensure_window_grid` 补推）。
    pub(crate) fn push_grid_prewarm(&mut self, window: Option<&Window>) {
        let Some(worker) = self.worker.clone() else {
            return;
        };
        let scale = self.scale_factor.max(1.0);
        let full_card = self.grid_card_width();
        let quick_card = grid_quick_target_px(full_card * scale) / scale;
        let pack_key = |g: &GridPrewarmGeom| {
            (
                g.center_page,
                g.window,
                g.near,
                grid_bucket_width(full_card),
            )
        };
        let Some(geom) = self.grid_prewarm_geom(window) else {
            return;
        };
        let key = pack_key(&geom);
        if self.grid_prewarm_pushed == Some(key) {
            return; // 同窗口同桶幂等：不重推（不打断推进中的带宽预熟）。
        }
        worker.set_grid_prewarm(self.doc_id, full_card, quick_card, self.scale_factor, geom);
        self.grid_prewarm_pushed = Some(key);
    }

    /// 网格列数钳用的**当前视口宽**：优先网格滚动容器 bounds（与 [`Self::grid_viewport`]
    /// 同一口径，网格已布局后恒 > 0）；未布局（首帧前 bounds==0）回退最近一次网格 render
    /// 已知宽 [`Self::last_grid_viewport_w`]；仍为 0（从未渲染过网格）→ 退化 1.0。
    /// 本方法与 `input.rs` 网格轮臂的 `self.grid_viewport(window).0` 同源，保证两处钳界一致。
    pub(super) fn grid_zoom_viewport_w(&self) -> f32 {
        let bw = f32::from(self.grid_scroll.bounds().size.width);
        if bw > 1.0 {
            bw
        } else {
            self.last_grid_viewport_w.max(1.0)
        }
    }

    /// 是否处于网格模式（页卡缩略图视图；shell 状态栏/粘贴面板与测试消费）。
    pub fn grid_mode(&self) -> bool {
        self.grid_mode
    }

    /// 进入/退出网格模式（供电层 wave4 与测试直接调用）。
    /// - **进入**：网格独立滚动归零（从顶部开始，简单确定；后续 wave 若需可改为「滚动到
    ///   当前页所在行」——非本次范围）。卡宽由 `Self::grid_card_width()`（= GRID_CARD_WIDTH ×
    ///   grid_zoom，独立缩放字段，与阅读 `self.zoom` 无关）派生，进入时不另存固定宽度。
    /// - **退出**：全部网格缩略图与在途请求清理（旧卡图转 `pending_drops` 下帧集中
    ///   `drop_image` 下线 GPU 纹理，不留残留引用；与 `release`/`apply_loaded` 同姿势）。
///
    /// 单列 `scroll` 偏移保持原样——切模式互不干扰，读者退出网格后回到离开时的阅读位置。
    pub fn set_grid_mode(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.grid_mode == enabled {
            return;
        }
        // 进出网格 = 非「鼠标缩放」水平位移 → 复位非溢出水平偏移（回中，见复位规则）。
        self.reset_x_zoom();
        self.grid_mode = enabled;
        if enabled {
            self.grid_scroll.set_offset(gpui::point(px(0.), px(0.)));
            self.grid_prewarm_pushed = None; // 进入新会话：确保下帧/下推用新几何开启带宽预熟
            // 本批「带宽预熟」杠杆：进入网格即通知 worker 以**全清 + 低清两档**目标宽 +
            // 带宽几何开启当前文档的中心带宽预熟（worker 只在四桶全空时空闲补渲近/中带缓存，
            // 优先级最低、任何真实消息到达即抢占，绝不耽误阅读模式——见
            // `WorkerMsg::SetGridPrewarm`；近带两档都预熟才能让风暴低清 / idle 全清两个方向
            // 都命中缓存真零闪，中带仅低清、更远不预熟）。
            self.push_grid_prewarm(None);
        } else {
            self.clear_grid_resources();
        }
        // wave5：选中集 = **一次网格会话语义**——进入清空（上一会话残留不串场）、退出清空
        // （本会话选中作废）都在模式翻转时复位，Shift 锚点一并清（点卡跳页经
        // `set_grid_mode(false)` 走同一条，故点卡退出也自动清选）。
        self.grid_selected.clear();
        self.grid_anchor = None;
        // wave6：拖拽态/待决单击随模式翻转作废（进入 = 无残留拖拽；退出 = 拖拽不可能
        // 跨模式存活）。
        self.grid_drag = None;
        self.grid_press = None;
        // wave8 评审 R1：模式翻转（进入/退出）都清「保持网格」待办位——该位只该存活于
        // 「页操作在途 → reload 消费」的瞬时窗口；任意网格翻转（如退出后手动操作）都取
        // 消它，防「页操作预检失败后 hold 残留 → 下一文档意外以网格模式打开」。
        self.grid_hold_on_reload = false;
        cx.notify();
    }

    /// 切换网格模式（供 shell 快捷键/工具栏与测试调用）。
    pub fn toggle_grid(&mut self, cx: &mut Context<Self>) {
        self.set_grid_mode(!self.grid_mode, cx);
    }

    /// 设置阅读页排模式（连续/双页；状态层 + 跨模式**页对应重锚**）。
    /// - **幂等**：模式未变且不在网格态时直接返回（不重复 `notify`）。
    /// - **网格互斥**：若正处于网格总览，先退出网格（`set_grid_mode(false)`——清网格
    ///   选中集/在途资源，单列滚动保持）再落新模式：网格只是一时总览，选新阅读页拍即
    ///   回到阅读主面。
    /// - **页对应重锚**（2026-11-20 对齐修复，K30/K49）：模式真的变化（非网格态）时，翻转
    ///   前用 [`Self::reading_progress`] 现有口径捕获当前 `(页, 页内比例)` 挂进
    ///   `restore_pending`，下一帧 `ensure_window` 装配新模式分页快照后按既有逆换算
    ///   （分页 `r = page/cols`、连续 `page_tops[k]`）落地到**同一页**——连续 ↔ Facing 互切
    ///   不跳页、切后首可见行贴行顶（`frac==0` 时）；`frac>0` 时按页内比例保留偏移，行顶可
    ///   不贴视口（自由滚动无吸附，停在行中间即保持，勿误判为回归）。此前只
    ///   翻状态不动 scroll 像素：同一像素跨模式深向差
    ///   ≈4.5×（连续页高 2079px vs Facing 行高 912px）→ 切到 Facing 页号 ≈4× 跳变 + 内容
    ///   偏上半个行高（无吸附时代由行内比例保留偏移兜底）。
    /// - **网格正交守卫**：网格激活时不重锚——网格滚动由 [`Self::grid_scroll`] 独立驱动、
    ///   不与 page_mode 联动，此时 `reading_progress` 捕到的阅读位是过期值，跳过以免错锚。
    pub fn set_page_mode(&mut self, mode: PageMode, cx: &mut Context<Self>) {
        if self.page_mode == mode && !self.grid_mode {
            return;
        }
        // 模式切换 = 非「鼠标缩放」水平位移 → 复位非溢出水平偏移（回中，见复位规则）。
        self.reset_x_zoom();
        // 真实模式翻转（非网格态）→ 页对应重锚：用当前模式既有口径捕获 (页, 页内比例)，
        // 交给 ensure_window 的 restore_pending 逆换算在下一帧落地（镜像阅读进度恢复时序）。
        // 文档未加载（reading_progress=None）→ 不设 pending（保持滚动位，等效旧行为）。
        if self.page_mode != mode && !self.grid_mode
            && let Some((page, frac, _zoom)) = self.reading_progress()
        {
            self.restore_pending = Some((page, frac));
        }
        if self.grid_mode {
            self.set_grid_mode(false, cx);
        }
        self.page_mode = mode;
        cx.notify();
    }

    // ---- 网格多选（wave5）：选中集模型 / Shift 范围选 / 全选清空 ----

    /// 切换第 `i` 页在网格多选中的选中态（Ctrl+单击卡 / 卡 checkbox 勾选共用）：选中集
    /// 含该页 ⇄ 不含，并把 Shift 范围锚点更新为该页（锚点 = 最近一次「单选 / 切换 / 勾选」）。
    /// 页下标越界忽略；`notify` 驱动重绘。仅在网格模式语义下调用（网格外空转无副作用）。
    pub(crate) fn grid_toggle_select(&mut self, i: usize, cx: &mut Context<Self>) {
        if i >= self.dims.len() {
            return;
        }
        if !self.grid_selected.remove(&i) {
            self.grid_selected.insert(i);
        }
        self.grid_anchor = Some(i);
        cx.notify();
    }

    /// Shift+单击范围选：从锚点页到第 `i` 页的**闭区间**（含端点；锚点 > 当前页也正向覆盖）
    /// 全部选中，**其它已选页保持不变**（不重算成仅该区间）——语义即「把锚点→当前页之间的
    /// 页加入选中集」；锚点随后更新为该页。无锚点时退化为单选第 `i` 页（等价 toggle 置位）。
    /// 页下标越界忽略；`notify` 驱动重绘。
    pub(crate) fn grid_select_range(&mut self, i: usize, cx: &mut Context<Self>) {
        if i >= self.dims.len() {
            return;
        }
        match self.grid_anchor {
            Some(a) => {
                let (lo, hi) = if a <= i { (a, i) } else { (i, a) };
                for p in lo..=hi {
                    if p < self.dims.len() {
                        self.grid_selected.insert(p);
                    }
                }
            }
            None => {
                self.grid_selected.insert(i);
            }
        }
        self.grid_anchor = Some(i);
        cx.notify();
    }

    /// 全选：把全部页加入选中集（工具栏「全选」）。锚点保持（批量操作不属「单选」）。
    pub(crate) fn grid_select_all(&mut self, cx: &mut Context<Self>) {
        self.grid_selected.extend(0..self.dims.len());
        cx.notify();
    }

    /// 清空选中集（工具栏「清空选择」）。锚点保持。
    pub(crate) fn grid_clear_selection(&mut self, cx: &mut Context<Self>) {
        self.grid_selected.clear();
        cx.notify();
    }

    /// 第 `i` 页是否在网格选中集（O(1) 存在查询）。
    pub(crate) fn grid_selected(&self, i: usize) -> bool {
        self.grid_selected.contains(&i)
    }

    /// 当前选中页数（工具栏操作按钮计数 badge 与可用/禁用判定用）。
    pub(crate) fn grid_selected_count(&self) -> usize {
        self.grid_selected.len()
    }

    /// Shift 范围选锚点页（`None` = 尚无锚点）。唯一消费方是测试断言 → `#[cfg(test)]`
    /// 收窄，消除生产构建 dead-code 告警（wave8 评审 R3；禁删，网格 Shift 选测试依赖）。
    #[cfg(test)]
    pub(crate) fn grid_selection_anchor(&self) -> Option<usize> {
        self.grid_anchor
    }

    // ---- 网格拖拽重组（wave6）：on_drag 建立 / on_drag_move 更新 / on_mouse_up 收尾 ----

    /// 拖拽开始（卡 `on_drag` 构造器内调用）：若被按页未选中 → **清空既有选择改为单卡选中**
    /// （契约：拖未选卡先选中再整组拖）；被拖组 = 当前选中集快照（升序）。拖拽态单例
    /// （覆盖可能残留的旧拖拽态）；`grid_press` 作废（转入拖拽不再当单击）。越界忽略。
    pub(crate) fn grid_drag_begin(&mut self, page: usize) {
        if page >= self.dims.len() {
            return;
        }
        if !self.grid_selected.contains(&page) {
            self.grid_selected.clear();
            self.grid_selected.insert(page);
            self.grid_anchor = Some(page);
        }
        let mut dragging: Vec<usize> = self.grid_selected.iter().copied().collect();
        dragging.sort_unstable();
        self.grid_press = None;
        // ghost 起始位 = 被按卡中心（内容坐标；首帧即有合理位置，on_drag_move 后随指针）。
        let pointer = self
            .grid_layout_cell
            .borrow()
            .as_ref()
            .and_then(|l| {
                l.cell_rect(page)
                    .map(|(x, y, w, h)| gpui::point(x + w / 2.0, y + h / 2.0))
            })
            .unwrap_or_else(|| gpui::point(0.0, 0.0));
        self.grid_drag = Some(GridDrag {
            dragging,
            insert: None,
            pointer,
        });
    }

    /// 拖拽移动（`pdf-grid` 内容容器 `on_drag_move`）：把指针**窗口**坐标折成**网格内容**
    /// 坐标（`ev.bounds` = 内容容器本帧 laid-out 边界、含滚动平移 → 内容坐标 = 指针 − 原点），
    /// 读上一帧布局快照（[`Self::grid_layout_cell`]）算线性插入位并写回拖拽态（ghost 随
    /// 指针、插入指示线随 `insert` 更新）。无拖拽态早退（幂等）。
    pub(crate) fn grid_drag_move(&mut self, ev: &DragMoveEvent<GridDragData>) {
        let Some(drag) = self.grid_drag.as_mut() else {
            return;
        };
        let x = ev.event.position.x.as_f32() - ev.bounds.origin.x.as_f32();
        let y = ev.event.position.y.as_f32() - ev.bounds.origin.y.as_f32();
        let vw = ev.bounds.size.width.as_f32();
        drag.pointer = gpui::point(x, y);
        drag.insert = self
            .grid_layout_cell
            .borrow()
            .as_ref()
            .and_then(|layout| compute_insert_index(layout, vw, x, y));
    }

    /// 网格内松手（网格根 `on_mouse_up(Left)`，指针在网格根内即达）：
    /// - 有拖拽态 → 消费：`insert` Some → 提交重排（`reorder_pages` 成功路径 reload，
    ///   选中集由 `apply_loaded` 清空，拖拽态在此前已清；无变化 = 拖回原位，不提交但清选）；
    ///   `insert` None（指针在内容外/工具条/尚未有移动事件）→ 放弃不提交、**选中保持**。
    /// - 无拖拽态 → 按「待决单击」消费 wave5 语义（见 [`Self::grid_click_commit`]）。
    pub(crate) fn grid_drag_release(&mut self, cx: &mut Context<Self>) {
        let Some(drag) = self.grid_drag.take() else {
            self.grid_click_commit(cx);
            return;
        };
        self.grid_press = None;
        let n = self.dims.len();
        let Some(insert) = drag.insert.filter(|&p| p <= n) else {
            // 网格外松手：放弃提交，选中保持原样（契约）。
            cx.notify();
            return;
        };
        let new_order = apply_reorder(n, &drag.dragging, insert);
        if new_order != (0..n).collect::<Vec<_>>() {
            // 拖拽重排是网格内操作：reload 后保持网格模式（apply_loaded 消费该位）。
            if self.grid_mode {
                self.grid_hold_on_reload = true;
            }
            self.reorder_pages(new_order, cx);
        } else {
            // 拖回原位 / 序列无变化：不提交，但拖拽已结束 → 清选中（一次拖拽会话完结）。
            self.grid_selected.clear();
            self.grid_anchor = None;
            cx.notify();
        }
    }

    /// 网格外松手（网格根 `on_mouse_up_out(Left)`）：与「取消」同语义——放弃提交、
    /// 拖拽态复位、选中保持原样。无拖拽态幂等。
    pub(crate) fn grid_drag_release_outside(&mut self, cx: &mut Context<Self>) {
        self.grid_drag_cancel(cx);
    }

    /// 取消拖拽（Esc → `on_key_down`；网格外松手；测试直调）：放弃提交、拖拽态复位、
    /// **选中保持原样**（契约）。`notify` 驱动 ghost/指示线撤下。
    pub(crate) fn grid_drag_cancel(&mut self, cx: &mut Context<Self>) {
        if self.grid_drag.take().is_none() {
            return;
        }
        self.grid_press = None;
        cx.notify();
    }

    /// 待决单击消费（未进入拖拽的松手）：按按下时修饰键 + 点击计数分发——
    /// Ctrl = 切换选中（不退出网格）、Shift = 范围选中（不退出）、**双击**（click_count ≥ 2）
    /// = 退出网格 + 跳页、普通单击（click_count == 1）= 切换选中且**留在网格**。
    /// 「按下即触发」改为「松手判定」是拖拽互斥的必然（位移超阈值转拖拽后不得再误触）。
    /// gpui 双击序列：第一击 count=1 先「选中」（文件管理器「先选定」）、随后第二击 count=2
    /// 才「退出跳页」——两语义同卡依次成立，与文件管理器一致。
    fn grid_click_commit(&mut self, cx: &mut Context<Self>) {
        let Some(press) = self.grid_press.take() else {
            return;
        };
        if press.page >= self.dims.len() {
            return;
        }
        if press.modifiers.control {
            self.grid_toggle_select(press.page, cx);
        } else if press.modifiers.shift {
            self.grid_select_range(press.page, cx);
        } else if press.click_count >= 2 {
            // 双击：退出网格 + 单列滚动到该页（wave3 跳页语义迁到双击）。
            self.set_grid_mode(false, cx);
            self.scroll_to_page(press.page);
            cx.notify();
        } else {
            // 普通单击：切换该页选中态，**留在网格**（不退出、不跳页）。
            self.grid_toggle_select(press.page, cx);
        }
    }

    /// 是否已加载完成。P4 拆 crate 后根包 app::tests 亦消费，取消 `cfg(test)`。
    pub fn is_loaded(&self) -> bool {
        !self.state.loading && self.worker.is_some() && !self.dims.is_empty()
    }

    /// 无任何输入下「已调度渲染」进度快照（根包 app::tests 的 PDF 用例消费，验证
    /// 「负载完成 → notify → 窗口重绘」链条会主动发起瓦片/封面调度）：
    /// `(首槽已落瓦片数, 首槽封面是否已入, 在途瓦片请求数)`。
    /// P4 拆 crate 后 app::tests 不能直接读 `slots`/`pending_tiles`（pdf crate 私有字段），
    /// 经此只读访问器取数。
    pub fn tiles_schedule_progress(&self) -> (usize, bool, usize) {
        let s0 = self.slots.first();
        (
            s0.map(|s| s.tiles.len()).unwrap_or(0),
            s0.map(|s| s.cover.is_some()).unwrap_or(false),
            self.pending_tiles.len(),
        )
    }

    /// 设置阅读画布背景色。
    pub fn set_background(&mut self, bg: Hsla) {
        self.bg = Some(bg);
    }

    /// 清空显式画布背景（回退默认）：`bg` 置 `None` → render 回落 [`Self::default_canvas`]
    /// （= 跟随主题 `window_bg`）。PDF 右键「背景色 → 默认」用；主题切换时 default 档也
    /// 走它保持跟随（显式档跳过，见 chrome.rs `refresh_theme_ui` 守卫）。
    pub fn clear_background(&mut self) {
        self.bg = None;
    }

    /// 设置**页面位图显示层**纸墨重着色（`None` = 直通无重着色；只改显示像素、绝不写回
    /// 文件）。`Some`：本阅读器重着色状态更新，把新参数推给 worker（置参数）并请求一次
    /// **重着色刷新**，随后触发一帧重渲。
    ///
    /// 【切档不重栅决策留痕：取代旧 `invalidate_recolored_caches` 全量重栅路径】
    /// 旧语义：变更 → 清全窗页槽缓存 + 重置门控 → 下一帧按新参数重派 → worker 重调 pdfium
    /// 全量重栅（夜间档加载慢的根因）。新语义：worker 缓存**未着色原图**，本方法只把新参数
    /// 推给 worker，并对[当前显示槽需要的可见集](Self::current_recolor_scope)（可见页
    /// 在位封面 + 在位瓦片）现算新着色副本回传——**不失效缓存槽、不重置调度门控、
    /// 不重调 pdfium 栅格、不刷窗外/已淘汰条目**；`window_sig`/`window_settled`
    /// 仅作「触发一帧调度来收集重着色结果并重绘」的复位（调度因 view 缓存命中而不再派新
    /// 栅格请求）。同参短值 → 空操作。
    pub fn set_recolor(&mut self, recolor: Option<RecolorParams>, cx: &mut Context<Self>) {
        if self.recolor == recolor {
            return;
        }
        // P2：记录翻转前的 R8 门控（GPU + FullBand + ink），用于切档后判定是否需重栅。
        let old_r8 = crate::recolor::luma_gate_active(self.recolor, self.gpu_recolor);
        self.recolor = recolor;
        // W1：世代随参数**实际变更** +1（在 worker 推送之前，保证 view 世代 ≥ worker 世代；
        // GPU 门控下结果恒为 raw 不涉着色竞态，但世代保持单调无副作用）。view 是世代唯一
        // 真源：worker 采纳本方法推送时携带的最终世代（评审 BLOCK 对齐）。
        self.recolor_epoch += 1;
        if let Some(w) = self.worker.as_ref() {
            // 按本文档键控（worker 侧按 DocId，多窗口互不干扰）：
            // 置参数（不回传）——之后栅格化/重着色刷新一律按新参数。
            w.set_recolor(self.doc_id, recolor, self.recolor_epoch);
            if !self.gpu_recolor {
                // CPU 通道：重着色刷新，圈定「显示槽真实需要的可见集」只刷它（窗外/已淘汰
                // 条目保留在 raw 缓存，滚回重取时现算）。scope 按近中心页排序 → worker 分批
                // 分帧回传时首批即首可视页（首帧尽早呈现）。
                self.recolor_scope = self.current_recolor_scope();
                self.recolor_received.clear();
                let rx = w.request_recolor_refresh(self.doc_id, self.recolor_scope.clone());
                self.pending_recolor = Some(rx);
            }
            // T4 GPU 通道：同门控内的切档只改图片元素参数（着色在 GPU 绘制时完成）→ 0 重算
            // 0 重栅，不发起重着色刷新（worker 又不会现算新副本——门控下它本就跳过 CPU 着色）。
        }
        // P2 GPU 通道 R8 门控翻转：位图数据格式（luma 1BPP ↔ BGRA 4BPP）已变，不能只换图元
        // 参数（旧格式位图被新参数读取 = 错色）——受影响页**重栅化一次**（用户已接受）。
        if self.gpu_recolor
            && old_r8 != crate::recolor::luma_gate_active(self.recolor, self.gpu_recolor)
        {
            self.invalidate_page_bitmaps_for_r8_flip();
        }
        // 旧的「清槽重栅失效」已由「重着色刷新」取代（见上方决策留痕）：只复位门控，让
        // 下一帧走一次调度来收集回传结果并重绘；不碰 slots / pending / 在途请求。
        self.window_sig = None;
        self.window_settled = false;
        cx.notify();
    }

    /// **R8 门控翻转 → 受影响页重栅化一次**（P2 接受口径）：R8 位图（单通道亮度 1BPP）与
    /// BGRA（4BPP）是**不同数据格式**，不能只改图元参数混显（luma 数据被按 `sample.rgb`
    /// 读取 = 错色）——门控翻转（`ink` 开↔关 / FullBand 深浅切换）清空显示槽位图 + 丢弃
    /// 在途请求，下一帧按新格式重派栅格。同门控内的参数微调（纸/墨色值变化）不触发
    /// （位图格式没变、只改 shader 参数，0 重栅）。旧纹理经 `pending_drops` 显式下线。
    fn invalidate_page_bitmaps_for_r8_flip(&mut self) {
        let mut drops: Vec<std::sync::Arc<gpui::RenderImage>> = Vec::new();
        for slot in self.slots.iter_mut() {
            if let Some(arc) = slot.cover.take() {
                drops.push(arc);
            }
            if let Some(arc) = slot.prev_cover.take() {
                drops.push(arc);
            }
            if let Some(arc) = slot.thumb.take() {
                drops.push(arc);
            }
            if let Some(arc) = slot.mid.take() {
                drops.push(arc);
            }
            if let Some(arc) = slot.prefetch_cover.take() {
                drops.push(arc);
            }
            slot.cover_scale = 0.0;
            slot.tile_scale = 0.0;
            slot.tile_bytes = 0;
            slot.thumb_loading = false;
            slot.mid_loading = false;
            for (_, ts) in slot.tiles.drain() {
                drops.push(ts.image);
            }
        }
        self.pending_drops.extend(drops);
        // 丢弃在途请求（rx drop → 结果作废），避免旧格式结果晚到覆盖新槽位。
        self.pending_cover.clear();
        self.pending_tiles.clear();
        self.pending_mid.clear();
        self.pending_thumb.clear();
        self.pending_prefetch_cover.clear();
        self.tile_bytes = 0;
    }

    /// 当前**显示槽需要的重着色刷新可见集**（可见集定义）：遍历全部页槽，只收「在位的
    /// 页面内容」——在位封面 + 在位瓦片（中清/缩略是快速拖动瞬态占位，不是"正在看"的内容，
    /// 不参与本次重着色刷新；窗外/已淘汰条目保留在 worker raw 缓存，滚回需要时再现算，
    /// 互不污染）。顺序按「离当前首可见页的距离」升序（先刷新首可视页；worker 分批分帧
    /// 按此序回传，首批即首可视页的封面/瓦片）。
    fn current_recolor_scope(&self) -> Vec<RecolorKey> {
        // 排序中心 = 当前首可见页（`current_page` 为 1 基）。
        let center = self
            .current_page()
            .map(|(first, _)| first.saturating_sub(1))
            .unwrap_or(0);
        let mut pages: Vec<(usize, u64)> = self
            .slots
            .iter()
            .enumerate()
            .filter(|(_, s)| s.cover.is_some() || !s.tiles.is_empty())
            .map(|(p, _)| (p, (p as i64 - center as i64).unsigned_abs()))
            .collect();
        pages.sort_by_key(|&(p, d)| (d, p));
        let mut scope = Vec::new();
        for (page, _) in pages {
            let slot = &self.slots[page];
            if slot.cover.is_some() {
                scope.push(RecolorKey {
                    kind: RawKind::Cover,
                    page,
                    row: 0,
                    col: 0,
                });
            }
            for &(row, col) in slot.tiles.keys() {
                scope.push(RecolorKey {
                    kind: RawKind::Tile,
                    page,
                    row,
                    col,
                });
            }
        }
        scope
    }

    /// 当前纸墨重着色参数（`None` = 直通）。测试 / shell 断言用。
    pub fn recolor(&self) -> Option<RecolorParams> {
        self.recolor
    }

    /// 当前画布背景色（`set_background` 传入；None = 未设置 → render 回落 `default_canvas`）。
    pub fn background(&self) -> Option<Hsla> {
        self.bg
    }

    /// 画布默认底色：亮 #F5F5F5 / 暗 #202020（中性灰阶，与外壳 `window_bg` 同源；
    /// 应用启动后 `set_background` 会用 ChromeColors 覆盖，此处仅兜底）。
    pub(super) fn default_canvas(cx: &App) -> Hsla {
        ui::theme::window_bg(cx)
    }

    /// 页面“卡片”配色：纸面白 / 深色纸面，配中性边框（只动 chrome 边框，纸面不改）。
    /// 已收编：`ui::content_colors::PDF_PAPER_*`（读纸偏好评级，属 config 非 chrome——
    /// 与 `shell_pdf::bg` 用户 PDF 背景偏好同源，随处现观感自然存在；暗
    /// `0x1f2937`/`0x484848`、亮白/`0xe0e0e0` 均非既有 theme token 同值）。
    pub(super) fn page_card_colors(cx: &App) -> (Hsla, Hsla) {
        if ui::theme::Theme::global(cx).is_dark() {
            (
                ui::content_colors::PDF_PAPER_BG_DARK,
                ui::content_colors::PDF_PAPER_BORDER_DARK,
            )
        } else {
            (
                ui::content_colors::PDF_PAPER_BG_LIGHT,
                ui::content_colors::PDF_PAPER_BORDER_LIGHT,
            )
        }
    }

    /// 释放所有已上传的 GPU 纹理（关闭标签页 / 丢弃视图时调用），关闭 worker 侧文档。
    pub fn release(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut drops = std::mem::take(&mut self.pending_drops);
        for slot in self.slots.iter_mut() {
            if let Some(arc) = slot.cover.take() {
                drops.push(arc);
            }
            if let Some(arc) = slot.prev_cover.take() {
                drops.push(arc);
            }
            if let Some(arc) = slot.thumb.take() {
                drops.push(arc);
            }
            if let Some(arc) = slot.mid.take() {
                drops.push(arc);
            }
            if let Some(arc) = slot.prefetch_cover.take() {
                drops.push(arc);
            }
            for (_, ts) in slot.tiles.drain() {
                drops.push(ts.image);
            }
            slot.tile_bytes = 0;
        }
        for arc in drops {
            cx.drop_image(arc, Some(window));
        }
        if let Some(worker) = &self.worker
            && self.doc_id != 0
        {
            worker.close(self.doc_id);
            self.doc_id = 0;
        }
        // 文本层与选区随文档一起释放
        self.text_pages.clear();
        self.text_lru.clear();
        self.pending_text.clear();
        // 图片盒缓存与选中图片随文档一起释放
        self.image_pages.clear();
        self.image_lru.clear();
        self.pending_images.clear();
        self.selected_image = None;
        self.pending_thumb.clear();
        self.pending_mid.clear();
        self.pending_tiles.clear();
        self.pending_cover.clear();
        self.pending_prefetch_cover.clear();
        // 网格模式随文档释放复位到单列 + 清空网格缩略图/在途（防关闭后残留卡图引用）。
        self.grid_mode = false;
        self.clear_grid_resources();
        // wave5：文档已释放 → 选中集随网格会话一并清空（防反向复用旧页码集合）。
        self.grid_selected.clear();
        self.grid_anchor = None;
        // wave8 评审 R1：关闭/释放时清「保持网格」待办位——防上一文档页操作残留的
        // `grid_hold_on_reload` 让**下一文档**加载时意外进入网格模式。
        self.grid_hold_on_reload = false;
        // L1/L2：大跳/停稳状态随文档释放复位（worker 侧 navigate_gen 已在 `worker.close` 内
        // 清掉，nav_gen 单调递增不碍复用——worker 采纳 max，旧世代永不误 drop 新请求）。
        self.jump_armed = false;
        self.jump_target = None;
        self.last_center_page = None;
        self.stop_small_frames = 0;
        // S1/S3：缩放手势状态随文档释放复位（`adjust_zoom` 置位、防抖到期清零）。
        self.zoom_armed = false;
        self.zoom_idle_frames = 0;
        // 网格**独立缩放**随文档释放复位（网格会话语义；同文档内存重载 `apply_loaded` 期间保持）。
        self.grid_zoom = 1.0;
        // Z2：缩放世代随文档释放复位（worker 镜像侧重置到 0，新档从 0 起）。
        self.zoom_epoch = 0;
        self.prefetch_reqs = 0;
        self.loading_placeholder_hits.set(0);
        self.grid_placeholder_hits.set(0);
        self.pending_recolor = None;
        self.recolor_scope.clear();
        self.recolor_received.clear();
        self.tile_bytes = 0;
        // 快速态随文档释放复位：若 release 前处于 fast 态（`update_fast_flag` 置位、滚动骤停
        // 未及 EMA 衰减），不自清 `prev_fast`/`speed_state` 会让 `pump_still_needed` 的
        // `prev_fast` 分支恒真 → 自刷新泵 16ms 周期持续 `refresh_windows` + 强 `this: Entity`
        // 永不 drop（关闭标签页后的僵尸泵，P1-C10 H2）。复位到 Idle 同时保全
        // 「`prev_fast` ≡ `speed_state.is_fast()`」的派生布尔不变式（见 `update_fast_flag`）。
        self.prev_fast = false;
        self.speed_state = SpeedState::Idle;
        self.tile_pump_running = false;
        self.mid_lru.clear();
        self.sel_anchor = None;
        self.sel_active = None;
        self.selecting = false;
        self.sel_card = None;
        // 待保存状态随文档一起释放（关闭标签 = 放弃待存；磁盘从未被触碰）
        self.pending_annots.clear();
        self.undo_stack.clear();
        self.pending_page_ops = 0;
        self.focused_annot = None;
        // 检索随文档一起释放
        self.search_query.clear();
        self.search_matches.clear();
        self.search_pending = None;
    }

    /// 当前页列显示宽度 = 自适应容器宽的基线 `base_w` × 缩放 `zoom`。
    /// 打开即「适应窗口宽」（见 [`Self::base_w`]），Ctrl+滚轮在其上缩放。
    ///
    /// **设备像素网格量化**（缩放发糊根治的最小点）：显示矩形在 GPUI 绘制时被
    /// [`layout::snap_to_device`] 同款/`snap_bounds` 取整到设备像素网格，而 worker 位图宽
    /// = `ceil(page_width·sf)`（固定取整）→ 当 `page_width·sf` 非整数时，`round(X)/ceil(X)≠1`
    /// → GPU 双线性重采样，随缩放相位交替糊/清晰。这里把页宽按当前 DPI 量化
    /// （`round(page_width·sf)/sf`），`page_width·sf` 恒为整数 → 位图与显示矩形逐纹素 1:1。
    /// 全部消费方（布局/tile 规划/封面/worker 栅格/overlay）都经本 getter → 自动自洽，
    /// 页宽仅偏移 ≤0.5 设备 px，对 fit-width / Z1 锚定 / scroll 范围无感知影响。
    pub(crate) fn page_width(&self) -> f32 {
        crate::pdf::layout::snap_to_device(self.base_w * self.zoom, self.scale_factor)
    }

    /// 分页页拍（Facing）是否激活：`!grid_mode && page_mode != Continuous && 非空`。
    /// 与 `ensure_window` 建分页快照的判据同源——命中/滚动/跳页统一走它，
    /// 保证「有分页快照 ⇔ 分页几何生效」两处不漂移（R1-R7 共用的门）。
    pub(super) fn paged_mode_active(&self) -> bool {
        !self.grid_mode && self.page_mode != PageMode::Continuous && !self.dims.is_empty()
    }

    /// **每页显示宽**（R1 密度收敛 helper）：分页页拍 = `PagedLayout::cell_w_per_row[page/cols]
    /// × zoom`（行内 contain-fit 页宽，几何 = `cell_rect(page).w × zoom`，渲染同口径）；
    /// 连续 / 网格 / 无分页快照 = 既有 [`Self::page_width`]（适配宽 × zoom）。
    ///
    /// 分页页拍与连续页列的显示宽口径不同（前者 fit 整页入视口、后者只 fit 宽）——
    /// 消耗方在分页模式必须喂本值（命中几何 `pt_to_local`/`local_to_pt` 用该页真实宽，
    /// 否则页内坐标换算失真）。**瓦片/封面位图密度暂仍按 [`Self::page_width`]**
    /// （render.rs 的三路分发对 tile_ok/cover 密度用连续 page_width 硬核对，S5b 再精修）。
    pub(crate) fn page_cell_width(&self, page: usize) -> f32 {
        if let Some(layout) = self.paged_layout_cell.borrow().as_ref() {
            let r = page / layout.cols;
            if let Some(&w) = layout.cell_w_per_row.get(r) {
                return w * self.zoom.max(0.01);
            }
        }
        self.page_width()
    }

    /// 分页态调度窗口（A 线，闭区间页下标）：换算后经 `layout::paged_schedule_window`，
    /// `k_rows = PAGED_WINDOW_ROWS`；非分页 / 无快照 / 空 → `None`（`ensure_window` 回落
    /// 连续 `keep_window` ±MARGIN 窗口）。布局快照以 `Ref` guard 借出 `&PagedLayout` 直读
    /// （不整表 clone，`ensure_window` 每帧执行路径零分配）。
    pub(super) fn paged_schedule_window(&self, scroll_y: f32, viewport_h: f32) -> Option<(usize, usize)> {
        if !self.paged_mode_active() {
            return None;
        }
        let guard = self.paged_layout_cell.borrow();
        let layout = guard.as_ref()?;
        if layout.rows == 0 {
            return None;
        }
        let zoom = self.zoom.max(0.01);
        let scroll_y_fit = -(scroll_y.max(0.0) / zoom);
        let vh_fit = viewport_h / zoom;
        crate::pdf::layout::paged_schedule_window(
            layout,
            scroll_y_fit,
            vh_fit,
            self.dims.len(),
            config::pdf::PAGED_WINDOW_ROWS,
        )
    }

    /// 分页态可见中心页（F4 走廊中心 / `evict_prefetch_covers` 的回收锚）：可见行页区间
    /// 算术中心。非分页 / 无快照 / 空 → `None`（调用方回落连续 `first_visible_page`）。
    /// 快照同样以 `Ref` guard 借出直读，不整表 clone。
    pub(super) fn paged_vis_center(&self, scroll_y: f32, viewport_h: f32) -> Option<usize> {
        if !self.paged_mode_active() {
            return None;
        }
        let guard = self.paged_layout_cell.borrow();
        let layout = guard.as_ref()?;
        if layout.rows == 0 {
            return None;
        }
        let zoom = self.zoom.max(0.01);
        let scroll_y_fit = -(scroll_y.max(0.0) / zoom);
        let vh_fit = viewport_h / zoom;
        let (s, e) =
            crate::pdf::layout::paged_visible_pages(layout, scroll_y_fit, vh_fit, self.dims.len())?;
        Some((s + e) / 2)
    }

    /// **分页页拍的唯一显示矩形源**（R1，K153 单一真源）：把 zoom=1 fit 口径的
    /// `PagedLayout::cell_rect(page)` 换算到当前 `zoom` 的**显示矩形** `(x, y, w, h)`。
    /// 渲染卡定位 / 命中（`window_to_pt`）/ 选中弹卡（`page_local_to_window`）/ 瓦片可见矩形
    /// （`page_visible_rect`）**全部只经本函数**——杜绝各处 `r.x*zoom` 散算导致的「水平不
    /// 居中 / zoom≠1 命中错位」（原 bug 2：把居中坐标 `r.x` 也乘 zoom → zoom<1 越缩越左偏）。
    ///
    /// - 尺寸随 zoom 缩放：`cw = r.w·z`、`ch = r.h·z`。
    /// - **水平按视口重算居中**（关键修复）：行内容宽 `spread = cols·cw + (cols−1)·gap·z`；
    ///   `left_pad = max(0, (viewport_w − spread)/2)`；`x = left_pad + c·(cw + gap·z)`。
    ///   `padding` 不乘 zoom。`spread ≤ 视口` 时整行在视口内水平居中（`zoom<1` 缩小只让页变小、
    ///   周围留白，不左偏）；`spread > 视口` 时 `left_pad=0` 左对齐 + 横向溢出可平移。
    ///   （注：`viewport_w − spread)/2` 与旧的 `padding + (content_w − spread)/2` 在
    ///   `spread ≤ content_w`（即所有 zoom ≤ 1）时代数恒等，故 zoom=1 与 `cell_rect` 逐值一致。）
    /// - 垂直沿用 S5b 口径：`y = r.y·z`（= `row_top[r]·z`，与 wheel/snap/scroll_to_page 同行界）。
    pub(crate) fn paged_display_rect(&self, page: usize) -> Option<crate::pdf::layout::Rect> {
        let guard = self.paged_layout_cell.borrow();
        let layout = guard.as_ref()?;
        let r = layout.cell_rect(page)?;
        let z = self.zoom.max(0.01);
        let cw = r.w * z;
        let ch = r.h * z;
        let cols = layout.cols as f32;
        let gap = layout.h_gap * z;
        let spread = cols * cw + (cols - 1.0) * gap;
        // 视口宽 = 阅读滚动容器宽（= 视口宽）；首帧/退化（≤0）回退到「恰容纳整行 + 两侧 padding」。
        let mut viewport_w = f32::from(self.scroll.bounds().size.width);
        if viewport_w <= 0.0 {
            viewport_w = spread + 2.0 * layout.padding;
        }
        let left_pad = ((viewport_w - spread) / 2.0).max(0.0);
        let c = (page % layout.cols) as f32;
        // 非溢出档鼠标缩放水平偏移叠加（语义与连续列 `column_left + x_zoom_offset` 同界，
        // 2026-11-20；`|x_zoom_offset| ≤ left_pad` 由 `zoom_target_impl` 非溢出分派夹住，
        // 保证整行仍在视口内）。渲染/命中/瓦片同经本唯一显示矩形源（K153）。
        let x = left_pad + self.x_zoom_offset + c * (cw + gap);
        Some(crate::pdf::layout::Rect {
            x,
            y: r.y * z,
            w: cw,
            h: ch,
        })
    }

    /// (**B 线密度真源**) 页卡显示尺寸 `(w, h)`（CSS px）：分页 = `paged_display_rect(page)`
    /// 的 `(w, h)`（cell 宽 × zoom、卡高按纵横比）；连续 / 网格 / 无分页快照 =
    /// `(page_width, display_height)`（连续口径，行为逐字节不变）。瓦片/封面密度 +
    /// `visible_tile_range_axis` 对账 + render 侧封面有效性判定**全部只经本函数**——消除
    /// 「分页卡按连续 page_width 密度过采样 ~2.3×（线性）/ ~5×（像素面积，本窗口）」（S5b 落地）。
    pub(crate) fn page_card_display_size(&self, page: usize) -> (f32, f32) {
        if let Some(rect) = self.paged_display_rect(page) {
            return (rect.w, rect.h);
        }
        let pw = self.page_width();
        let h = self
            .dims
            .get(page)
            .copied()
            .map_or(pw, |d| display_height(d, pw));
        (pw, h)
    }

    /// 页卡显示宽（CSS px，`page_card_display_size` 的宽分量；密度计算最常用）。
    #[inline]
    pub(crate) fn page_card_display_w(&self, page: usize) -> f32 {
        self.page_card_display_size(page).0
    }

    /// 分页滚动**纵向量程**（显示 px）：可视内容可滚范围 = `PagedLayout 总高 × zoom − 视口高`
    /// （≥0）。分页容器内容高 = `total_height×zoom`、与连续 `total_height` 无关——跳页/恢复
    /// 目标都须按它夹（末行顶 + 视口高可能超出文档底，gpui 布局会把偏移再夹到下界，
    /// 这里先夹保证 `last_scroll_y` 记录的也是夹后值 → 无假速度）。无分页快照回落连续量程。
    pub(super) fn paged_max_scroll(&self) -> f32 {
        let guard = self.paged_layout_cell.borrow();
        let Some(layout) = guard.as_ref() else {
            return self.total_height.max(0.0);
        };
        let viewport_h = f32::from(self.scroll.bounds().size.height);
        (layout.total_height * self.zoom.max(0.01) - viewport_h).max(0.0)
    }

    /// 当前缩放倍数（Ctrl+滚轮在 `base_w` 基线上的倍率；1.0 = 适应窗口宽的 100%）。
    /// 外壳状态栏缩放百分比显示用：`zoom_factor() * 100` 即 zoom%。
    pub fn zoom_factor(&self) -> f32 {
        self.zoom
    }

    /// 书签目录访问器（侧边栏目录使用）。
    pub fn outlines(&self) -> &[PdfOutlineEntry] {
        &self.outlines
    }

    // ---- 待保存模型（P1）：注释只进内存叠加层 + 页面操作内存级 + save() 才写盘 ----

    /// 追加一条**待保存**注释（只进内存叠加层 + 置脏；**零磁盘零重载**，`save()` 时才由
    /// worker 合并写盘）。`quads_pt` = 页内 PDF 点矩形 `[x0, y0, x1, y1]`（y 向上；每页
    /// 可传多个矩形即跨行选区）。
    pub fn add_annotation(
        &mut self,
        page_index: usize,
        kind: AnnotKind,
        quads_pt: Vec<[f32; 4]>,
        cx: &mut Context<Self>,
    ) {
        self.push_pending_annot(
            page_index,
            kind,
            quads_pt
                .into_iter()
                .map(|q| QuadPt {
                    x0: q[0],
                    y0: q[1],
                    x1: q[2],
                    y1: q[3],
                })
                .collect(),
            cx,
        );
    }

    /// 内部：把一条注释记入 pending + 撤销栈 + 置脏（页号越界 / 无几何 → 忽略）。零磁盘。
    pub(super) fn push_pending_annot(
        &mut self,
        page_index: usize,
        kind: AnnotKind,
        quads_pt: Vec<QuadPt>,
        cx: &mut Context<Self>,
    ) {
        if page_index >= self.dims.len() {
            self.last_modify_error = Some(format!("页码越界：第 {page_index} 页"));
            return;
        }
        if quads_pt.is_empty() {
            return;
        }
        self.next_annot_id += 1;
        let annot = PendingAnnot {
            id: self.next_annot_id,
            page: page_index,
            kind,
            quads_pt,
        };
        self.pending_annots.push(annot.clone());
        self.undo_stack
            .push_back(UndoEntry::RemoveAnnot { id: annot.id });
        cx.notify();
    }

    /// 把一批注释（跨页选区）全部记入**待存**（Phase A 的批量落盘改为批量待存）：撤销栈
    /// 逐条记录、dirty 派生置真；**零磁盘零 reload**（叠加层渲染即时上屏）。
    pub(super) fn submit_annotations(&mut self, items: Vec<AnnotateItem>, cx: &mut Context<Self>) {
        for item in items {
            self.push_pending_annot(item.page_index, item.kind, item.quads, cx);
        }
    }

    /// 是否有待保存修改（注释非空 || 页面操作计数非 0）。壳层关闭提示 / dirty 圆点用。
    pub fn is_dirty(&self) -> bool {
        !self.pending_annots.is_empty() || self.pending_page_ops != 0
    }

    /// 待存注释条数（测试 / 壳层断言）。
    pub fn pending_annot_count(&self) -> usize {
        self.pending_annots.len()
    }

    /// 当前「灰框」焦注释 id（右键「删除注释」用；None = 无）。
    pub fn focused_annot_id(&self) -> Option<u64> {
        self.focused_annot
    }
}

#[cfg(test)]
#[path = "open_tests.rs"]
mod tests;
