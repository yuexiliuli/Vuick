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
            pending_recolor: None,
            raster_reqs: 0,
            recolor_scope: Vec::new(),
            recolor_received: Vec::new(),
            tile_reqs: 0,
            cover_reqs: 0,
            tile_bytes: 0,
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
            loading: true,
            load_error: None,
            bg: None,
            recolor: None,
            recolor_epoch: 0, // W1：随 set_recolor 实际变更 +1
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
            scale_factor: 1.0,
            window_sig: None,
            window_settled: false,
            idle_gate_skips: 0,
            reload_anchor: None,
            last_modify_error: None,
            sel_card: None,
            pending_annots: Vec::new(),
            undo_stack: VecDeque::new(),
            pending_page_ops: 0,
            next_annot_id: 0,
            focused_annot: None,
            panning: false,
            pan_origin: gpui::point(px(0.), px(0.)),
            pan_offset0: gpui::point(px(0.), px(0.)),
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
                    let (worker, doc_id, dims, outlines) =
                        PdfWorker::open_and_dims_full(bytes)
                            .map_err(|e| format!("无法启动 pdfium：{e}"))?;
                    Ok::<_, String>((worker, doc_id, dims, outlines))
                })
                .await;
            let _ = weak.update(cx, |this, cx| {
                match result {
                    Ok((worker, doc_id, dims, outlines)) => {
                        this.apply_loaded(worker, doc_id, dims, outlines, cx);
                    }
                    Err(e) => {
                        this.loading = false;
                        this.load_error = Some(e);
                        cx.refresh_windows();
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    /// 打开成功后的统一装配（`start_load` 磁盘打开与待保存模型 `open_from_bytes`
    /// 内存打开共用）：铺页槽、旧位图转 pending_drops、恢复重载锚点、刷新窗口。
    fn apply_loaded(
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
        self.outlines = outlines;
        // 重着色补推：打开/重载时 worker 在 set_recolor 时刻可能尚未就绪，这里在 worker
        // 就位后**总是**把本阅读器的重着色状态按**本文档**推给它（worker 侧按 DocId 键控，
        // 多窗口/多标签各自独立）——保证新文档/重载后的栅格化与 view 状态一致（含 None
        // 复位：防某文档的全局重着色泄漏到其它文档）。**携带 view 世代**：接线前的多次
        // set_recolor 折叠成这一次补推，worker 靠消息里的最终世代对齐（评审 BLOCK）。
        if let Some(w) = self.worker.as_ref() {
            w.set_recolor(self.doc_id, self.recolor, self.recolor_epoch);
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
        self.loading = false;
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
    pub fn scroll_to_page(&mut self, page_index: usize) {
        if page_index >= self.dims.len() {
            return;
        }
        let top = self.page_tops.get(page_index).copied().unwrap_or(0.0);
        self.scroll.set_offset(gpui::point(px(0.), px(-top)));
    }

    /// 页数（文档尚未加载完时为 0）。P4 拆 crate 后根包 app::tests 亦消费，取消 `cfg(test)`。
    pub fn page_count(&self) -> usize {
        self.dims.len()
    }

    /// 当前文档路径（Phase B 书签按路径关联持久化用）。
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// 当前第一张可见页（1 基）与总页数（状态栏滚动位置显示用）。
    /// 仅派生自既有滚动状态（`scroll` 偏移 + 页列布局 [`first_visible_page`]），不新增
    /// 状态；文档未加载完成（`dims` 为空）时返回 `None`。与 EPUB 页码口径一致：
    /// 「第一张可见页」如实反映视口，不含 `keep_window` 的 MARGIN 缓冲。
    pub fn current_page(&self) -> Option<(usize, usize)> {
        if self.dims.is_empty() {
            return None;
        }
        // 首帧 `scroll` 偏移为 0 → `first_visible_page` 起始页即第 1 页，布局后精确。
        let scroll_y = f32::from(-self.scroll.offset().y);
        let first = first_visible_page(&self.dims, &self.page_tops, self.page_width(), scroll_y);
        Some((first + 1, self.dims.len()))
    }

    /// 是否已加载完成。P4 拆 crate 后根包 app::tests 亦消费，取消 `cfg(test)`。
    pub fn is_loaded(&self) -> bool {
        !self.loading && self.worker.is_some() && !self.dims.is_empty()
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
            // T4 GPU 通道：切档只改图片元素参数（着色在 GPU 绘制时完成）→ 0 重算 0 重栅，
            // 不发起重着色刷新（worker 又不会现算新副本——门控下它本就跳过 CPU 着色）。
        }
        // 旧的「清槽重栅失效」已由「重着色刷新」取代（见上方决策留痕）：只复位门控，让
        // 下一帧走一次调度来收集回传结果并重绘；不碰 slots / pending / 在途请求。
        self.window_sig = None;
        self.window_settled = false;
        cx.notify();
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
                scope.push(RecolorKey { kind: RawKind::Cover, page, row: 0, col: 0 });
            }
            for &(row, col) in slot.tiles.keys() {
                scope.push(RecolorKey { kind: RawKind::Tile, page, row, col });
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
        let dark = ui::theme::Theme::global(cx).is_dark();
        ui::theme::ChromeColors::for_mode(dark).window_bg.into()
    }

    /// 页面“卡片”配色：纸面白 / 深色纸面，配中性边框（只动 chrome 边框，纸面不改）。
    pub(super) fn page_card_colors(cx: &App) -> (Hsla, Hsla) {
        if ui::theme::Theme::global(cx).is_dark() {
            (rgb(0x1f2937).into(), rgb(0x484848).into())
        } else {
            (gpui::white(), rgb(0xe0e0e0).into())
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
            && self.doc_id != 0 {
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
        // L1/L2：大跳/停稳状态随文档释放复位（worker 侧 navigate_gen 已在 `worker.close` 内
        // 清掉，nav_gen 单调递增不碍复用——worker 采纳 max，旧世代永不误 drop 新请求）。
        self.jump_armed = false;
        self.jump_target = None;
        self.last_center_page = None;
        self.stop_small_frames = 0;
        // S1/S3：缩放手势状态随文档释放复位（`adjust_zoom` 置位、防抖到期清零）。
        self.zoom_armed = false;
        self.zoom_idle_frames = 0;
        // Z2：缩放世代随文档释放复位（worker 镜像侧重置到 0，新档从 0 起）。
        self.zoom_epoch = 0;
        self.prefetch_reqs = 0;
        self.loading_placeholder_hits.set(0);
        self.pending_recolor = None;
        self.recolor_scope.clear();
        self.recolor_received.clear();
        self.tile_bytes = 0;
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

    /// 当前缩放倍数（Ctrl+滚轮在 `base_w` 基线上的倍率；1.0 = 适应窗口宽的 100%）。
    /// 外壳状态栏缩放百分比显示用：`zoom_factor() * 100` 即 zoom%。
    pub fn zoom_factor(&self) -> f32 {
        self.zoom
    }

    /// 书签目录访问器（侧边栏目录使用）。
    pub fn outlines(&self) -> &[PdfOutlineEntry] {
        &self.outlines
    }

    // ---- 每帧装配 / 调度驱动 ----

    /// 每帧：回收在途结果、估速、刷盘、按视口调度瓦片（常态）/占位（快速拖动）、
    /// 淘汰超预算瓦片。
    pub(super) fn ensure_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // 每帧刷新窗口缩放密度：`page_width()` 的 1:1 量化用它（必须在下方 refit/调度
        // 消费 page_width 之前写入，保证本帧布局/位图/显示全用同一 sf）。
        self.scale_factor = window.scale_factor();
        if timing_view() {
            eprintln!(
                "[{:.1}ms] [VIEW-TICK] loading={} dims={} pending_t={} pending_c={} fast={} bounds_h={:.0} off_y={:.0}",
                crate::pdf::timing_ms(),
                self.loading,
                self.dims.len(),
                self.pending_tiles.len(),
                self.pending_cover.len(),
                self.prev_fast,
                f32::from(self.scroll.bounds().size.height),
                f32::from(-self.scroll.offset().y),
            );
        }
        for arc in std::mem::take(&mut self.pending_drops) {
            cx.drop_image(arc, Some(window));
        }
        // Phase B1：选中弹卡挂起期间任一来源的滚动（滚轮 / 滚动条 / 跳页）都使偏移变化
        // → 关闭卡片，防卡片锚点贴死旧选区位置（与鼠标跟随的重选不同源）。
        if self.sel_card.is_some() && self.sel_card.as_ref().is_some_and(|s| s.scroll != self.scroll.offset()) {
            self.sel_card = None;
            cx.notify();
        }
        if self.dims.is_empty() || self.worker.is_none() {
            return;
        }
        // **自适应窗口宽**：页宽基线 = 滚动容器可视宽 − 两侧页边距（容器首帧未布局宽为
        // 0 → 回退窗口宽；侧边栏/分屏使可视区窄于窗口时按容器实宽，页列不相对可视区偏）。
        // 窗口/侧边栏拉宽拉窄 → 基线变 → 重排页列布局（页面随窗口缩放），瓦片按新密度
        // 由 `schedule_tiles` 惰性重渲；Ctrl+滚轮在基线上再乘 `zoom`（`adjust_zoom` 重排）。
        let container_w = f32::from(self.scroll.bounds().size.width);
        let view_w = if container_w > 0.0 {
            container_w
        } else {
            window.bounds().size.width.as_f32()
        };
        let new_base = (view_w - 2.0 * PAGE_PAD_X).max(1.0);
        if (new_base - self.base_w).abs() > 1e-4 {
            self.base_w = new_base;
            let pw = self.page_width();
            let (tops, total) = compute_layout(&self.dims, pw);
            self.page_tops = tops;
            self.total_height = total;
            if timing_view() {
                eprintln!(
                    "[{:.1}ms] [VIEW-REFIT] base_w={:.0} page_width={:.0} view_w={:.0} zoom={:.2}",
                    crate::pdf::timing_ms(),
                    self.base_w,
                    pw,
                    view_w,
                    self.zoom,
                );
            }
            cx.notify();
        }
        let scale_factor = window.scale_factor();
        let page_width = self.page_width();
        let scroll_y = f32::from(-self.scroll.offset().y);

        // 内存水位巡检（仅计时模式）：每 ~30 帧打一行构成，供定位内存大头
        if timing_view() {
            static MEM_TICK: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            if MEM_TICK.fetch_add(1, std::sync::atomic::Ordering::Relaxed).is_multiple_of(30) {
                self.log_memory_waterline(scale_factor);
            }
        }

        // 1) 回收在途结果（占位 → 缓存；瓦片 → 拼图；文本/检索 → 各自缓存）——非阻塞
        if self.collect_thumb_results() {
            cx.notify();
        }
        if self.collect_mid_results() {
            cx.notify();
        }
        if self.collect_text_results() {
            cx.notify();
        }
        if self.collect_image_results() {
            cx.notify();
        }
        if self.collect_search_results() {
            cx.notify();
        }
        // 封面（首屏整页锐化）先于瓦片收集：一个 render 即铺满首屏，立即清晰
        if self.collect_cover_results(scale_factor) {
            cx.notify();
        }
        // F4 空闲预取走廊结果收集（0.5 密度整页底图，独立于封面/瓦片密度守卫）。
        if self.collect_prefetch_cover_results(scale_factor) {
            cx.notify();
        }
        if self.collect_tile_results(scale_factor) {
            cx.notify();
        }
        // 重着色刷新结果收集（切档不重栅：worker 对缓存原图现算的新着色副本，替换显示位图）。
        if self.collect_recolor_results(scale_factor) {
            cx.notify();
        }

        // 2) 滚动速度估计（EMA，px/帧）+ 迟滞判定 + 滚动方向（F2 封面单向预取用）
        let jump = scroll_y - self.last_scroll_y;
        // 方向：>0 向下（页号增大）、<0 向上；delta==0（静止帧）保持上一方向（迟滞，
        // 稳态封面窗口不因「停稳」扩回对称 ±1）。缩放（`adjust_zoom` 同比缩放偏移）只在
        // 交互瞬间给一次伪方向，可见页恒在窗口内，无碍。
        self.scroll_dir = match jump.partial_cmp(&0.0) {
            Some(core::cmp::Ordering::Greater) => 1,
            Some(core::cmp::Ordering::Less) => -1,
            _ => self.scroll_dir,
        };
        let dy = jump.abs();
        self.scroll_velocity = self.scroll_velocity * 0.7 + dy * 0.3;
        self.update_fast_flag();
        // L1.3 停稳快进：连续 K=3 帧 |Δy|<2px → 速度直落 Slow（绕过 0.7ⁿ EMA 衰减），
        // 令落点封面立刻可被 Slow/Idle 调度消费（拖拽停稳不再等 150-250ms 的指数衰减）。
        self.update_stop_detection(dy);
        self.last_scroll_y = scroll_y;

        let viewport_h = f32::from(self.scroll.bounds().size.height);

        // 2.5) **静态帧门控（Sprint 5）**：滚动/缩放/DPI/视口/页数全未变 + 在途清空 +
        //      上帧已常态调度确认无待派（`window_settled`）→ 本帧 keep_window/内存封顶/
        //      schedule/evict 全部跳过。调度是异步投机——跳过只影响后续滚动时冷片延迟，
        //      恢复帧立即补调；首帧（sig=None）、加载/缩放/尺寸变化帧、任何「在途未决」
        //      帧、快速拖动帧都绝不门（防贴图迟滞）。跳过帧不采样泵（无在途 + 视口非空
        //      + 非快速 → `pump_still_needed` 本就为 false，早退不改变任何泵语义）。
        let sig = WindowSig::of(self, scroll_y, page_width, scale_factor, viewport_h);
        let idle_sig = self.window_sig.is_some_and(|s| s == sig);
        // S3：缩放防抖（zoom_armed）期**必不门**——静态帧早退会跳过 S3 的 idle 计数，
        // 缩放事件后只剩一帧渲染，`zoom_idle_frames` 到不了 3 帧、防抖永不收口。
        if idle_sig
            && self.window_settled
            && !self.prev_fast
            && !self.zoom_armed
            && viewport_h > 0.0
            && self.pending_tiles.is_empty()
            && self.pending_cover.is_empty()
            && self.pending_mid.is_empty()
            && self.pending_thumb.is_empty()
        {
            if timing_view() {
                eprintln!(
                    "[{:.1}ms] [VIEW-IDLE] 静置帧：跳过 keep_window/schedule/evict",
                    crate::pdf::timing_ms()
                );
            }
            self.idle_gate_skips += 1;
            // F4 空闲预取走廊（WPS「好像全部已加载」的来源）：静置帧早退**之前**为走廊
            // [vis±2] 补 0.5 密度封面——idle 四队列全空（上方已核），把低成本整页底图在
            // 空闲期消化掉；高速平移（VeryFast）切入时新页大概率已有可显示底图。派发在途后
            // 自启一帧收图泵（idle 分支本会早退，不泵则下一帧不会来、走廊结果无人收集）。
            let vis_first = first_visible_page(&self.dims, &self.page_tops, page_width, scroll_y);
            self.maybe_prefetch_corridor(vis_first, page_width, scale_factor);
            self.ensure_tile_pump(cx);
            // 不更新 `window_sig`/`window_settled`：保持同一静置签名与确认状态。
            // --- 真机取证探针（纯只读，不改调度语义，见 view.rs `diag_enabled`）---
            // 用户真机症状：缩放后在固定比例有时整页雾糊（低密度底图在服务），静止不
            // 清晰、只能靠再次缩放恢复；headless 单 worker 时序与真机有别无法复现。
            // 本探针在「糊住不动」的静态帧门控早退前（keep_window 可见窗，含 MARGIN
            // 缓冲）逐可见页打印 LOD 账：各档底图（cover/prev/pref/mid/thumb/tiles）
            // 在位情况 + 是否有当前档 cover 在途 + 期望 cover 档，供锁定是哪一档低密
            // 底图在服务、为何无当前档 cover 补渲。GPUI_EDITOR_DIAG 未设时零开销。
            if diag_enabled() {
                let (dstart, dend) = keep_window(&self.dims, &self.page_tops, page_width, scroll_y, viewport_h);
                if !self.dims.is_empty() && dstart <= dend {
                    for p in dstart..=dend {
                        if let Some(slot) = self.slots.get(p) {
                            let cover = slot
                                .cover
                                .as_ref()
                                .map(|_| format!("{:.2}", slot.cover_scale))
                                .unwrap_or_else(|| "none".to_owned());
                            let prev = if slot.prev_cover.is_some() { "some" } else { "none" };
                            let pref = slot
                                .prefetch_cover
                                .as_ref()
                                .map(|_| format!("{:.2}", slot.prefetch_cover_scale))
                                .unwrap_or_else(|| "none".to_owned());
                            let tiles = if slot.tiles.is_empty() {
                                "0".to_owned()
                            } else {
                                format!("{}@{}", slot.tiles.len(), slot.tile_scale)
                            };
                            let cpend = self.pending_cover.contains_key(&p) as u8;
                            let tpend = self.pending_tiles.keys().any(|(pg, _, _)| *pg == p) as u8;
                            let want = cover_scale(self.dims[p], page_width, scale_factor);
                            let line = format!(
                                "[DIAG-IDLE] p{p} cover={cover} prev={prev} pref={pref} mid={} thumb={} tiles={tiles} cpend={cpend} tpend={tpend} want={want:.2}",
                                slot.mid.is_some() as u8,
                                slot.thumb.is_some() as u8,
                            );
                            eprintln!("{line}");
                        }
                    }
                }
            }
            return;
        }

        let (start, end) = keep_window(&self.dims, &self.page_tops, page_width, scroll_y, viewport_h);
        let center = (start + end) / 2;
        // L1.1 大跳检测（每帧 update_fast_flag 后、调度前）：当前可见中心页位移超过
        // JUMP_PAGES_THRESHOLD → 置 jump_armed/jump_target + nav_gen++ 并清掉旧世代在途
        // 条目（预算立即释放）；快速连续跳每帧重算落点、后到覆盖先到。
        self.update_nav_jump(start, end, page_width, scroll_y, viewport_h, window);

        // 4) L0 缩略图内存封顶：保留数超 THUMB_CACHE_PAGES 时丢离视口中心最远的，
        //    拆除走 `drop_image`（显式下线 GPU 纹理工卡，否则仅丢 Arc 会累积显存提交）。
        let thumb_retained = self.slots.iter().filter(|s| s.thumb.is_some()).count();
        if thumb_retained > THUMB_CACHE_PAGES {
            let mut th_candidates: Vec<(usize, u64)> = self
                .slots
                .iter()
                .enumerate()
                .filter(|(_, s)| s.thumb.is_some())
                .map(|(i, _)| (i, (i as i64 - center as i64).unsigned_abs()))
                .collect();
            th_candidates.sort_by_key(|&(i, d)| (d, i));
            let excess = thumb_retained - THUMB_CACHE_PAGES;
            let mut dropped: Vec<Arc<RenderImage>> = Vec::new();
            for (i, _) in th_candidates.into_iter().take(excess) {
                if let Some(slot) = self.slots.get_mut(i)
                    && let Some(arc) = slot.thumb.take() {
                        dropped.push(arc);
                    }
            }
            for arc in dropped {
                cx.drop_image(arc, Some(window));
            }
        }
        // 5) L0.5 中清内存封顶：中清页（~1.5MB/张）数量硬顶。删除统一走 `pending_drops`
        //    （下一帧开头 `cx.drop_image` 显式下线 GPU 纹理，防显存/提交内存缓慢累积）。
        let mid_retained = self.slots.iter().filter(|s| s.mid.is_some()).count();
        if mid_retained > MID_CACHE_PAGES {
            // LRU 先与槽同步（删除即丢引用，保证预算与队列一致），再按 LRU 淘汰。
            self.mid_lru
                .retain(|&p| self.slots.get(p).is_some_and(|s| s.mid.is_some()));
            let excess = mid_retained - MID_CACHE_PAGES;
            let mut evicted = 0usize;
            let mut dropped: Vec<Arc<RenderImage>> = Vec::new();
            // 第一遍：只淘汰可见窗之外的**最久未用**页（回翻场景保最近看过的）；
            // 第二遍：窗口内兜底（全部可见时仍能压回预算）。
            for pass in 0..2 {
                if evicted >= excess {
                    break;
                }
                let idxs: Vec<usize> = self
                    .mid_lru
                    .iter()
                    .copied()
                    .filter(|&p| pass == 1 || p < start || p > end)
                    .collect();
                for idx in idxs {
                    if evicted >= excess {
                        break;
                    }
                    let Some(slot) = self.slots.get_mut(idx) else {
                        continue;
                    };
                    if let Some(arc) = slot.mid.take() {
                        self.mid_lru.retain(|&p| p != idx);
                        dropped.push(arc);
                        evicted += 1;
                    }
                }
            }
            self.pending_drops.extend(dropped);
        }

        // 6) 调度（F3 三速分带）：常态（Idle/Slow）→ 瓦片+封面（瓦片先派防封面堵小片）；
        //    中速带（Fast）→ 满密度封面优先、封面不可负担页回退中清/缩略占位；
        //    高速带（VeryFast）→ 纯平移不派新栅格（含占位，渲染队列不加活），新进页由 F4
        //    预取走廊兜底。之后统一做瓦片字节预算淘汰。
        // L1.2：速度离开 VeryFast → 落点直达使命结束（后续由停稳/常态调度接管），清零
        // `jump_armed`/`jump_target`——防止旧落点标记在下次高速带残留派发旧窗封面。
        if self.speed_state != SpeedState::VeryFast {
            self.jump_armed = false;
            self.jump_target = None;
        }
        // S3 缩放防抖收口：`zoom_armed` 期间每帧自增 idle 计数；连续
        // [`config::pdf::ZOOM_DEBOUNCE_FRAMES`] 帧无新缩放事件 → 清 `zoom_armed` 并强制
        // Slow（绕过 EMA 衰减），让下方 Idle/Slow 分支的常态调度 `schedule_tiles` 按
        // **最终 cscale** 一次性满密 cover+tile 收口（在途旧档由既有 scale 守卫自动弃，
        // 无需 cancelRendering）。`adjust_zoom` 每档都重置 `zoom_idle_frames=0`，缩放中
        // 档间间隙不误收口。
        if self.zoom_armed {
            // S2/S4 前置（评审 BLOCK②）：armed 推进首帧把可见窗内 stale cover 迁入
            // prev_cover（cover=None）——手势前那张高密位图转存为唯一平滑底图基（GPU 拉伸），
            // 手势期**不再置换**（下方 collect 框架 + preview 不换底共同守住；撤 #3）。
            let reloc = self.relocate_stale_zoomed_covers(start, end, page_width, scale_factor);
            self.pending_drops.extend(reloc);
            // Z2 收口前置：释放缩放中间档的在途预算（cover/tile/preview 旧档条目直接移除，
            // 且 `collect_cover_results` 的 armed 守卫会在已渲染到达层丢弃中间档封面）。
            self.drop_stale_scale_pending(page_width, scale_factor);
            // ⚠️ 第 4 轮真机实证：armed 期**不派任何满密 cover**（撤 #3 中心满密 cover 派发）
            // ——否则每档中间档满密封面经 collect 写入 prev_cover 桥 → 屏显底图每档换密度 →
            // 每档一次整页栅格重排（「松手瞬间跳一下、整页一起动」）。手势期零满密换底：
            // 屏显维持手势前那张高密位图（prev_cover 拉伸），settle 一次性按最终档收口。
            self.zoom_idle_frames += 1;
            if self.zoom_idle_frames >= config::pdf::ZOOM_DEBOUNCE_FRAMES {
                self.zoom_armed = false;
                self.speed_state = SpeedState::Slow;
                self.prev_fast = false;
                // #5 去 E（高倍收口雾窗）：收口当帧对 cur>4 纯瓦片页把 TILE_INFLIGHT 临时
                // 抬到 SETTLE_TILE_BURST_MAX 一次性出清可见瓦片（schedule_tiles 消费后自清）。
                self.settle_tile_burst = true;
            }
        }
        match self.speed_state {
            SpeedState::Idle | SpeedState::Slow => {
                if self.zoom_armed {
                    // S2 缩放手势防抖窗内（本应落常态调度的 Idle/Slow 帧）：不派满密
                    // cover/tile——每档 cscale 都在变，满密渲染 ~30ms 大概率被下一档弃掉
                    // = 逐档抢渲空转；只按当前 cscale 派 ≤PRE_FETCH_COVER_INFLIGHT 枚
                    // 0.5 密度 preview 顶显示。防抖到期由上方清 `zoom_armed` → 下一帧
                    // 落回本分支的 `schedule_tiles` 满密收口。
                    self.maybe_dispatch_zoom_preview(
                        start, end, scroll_y, viewport_h, page_width, scale_factor, window,
                    );
                } else {
                    self.schedule_tiles(window, start, end, page_width, scale_factor, scroll_y);
                }
            }
            SpeedState::Fast => {
                if self.zoom_armed {
                    // 缩放 + 中速拖拽复合：满密封面同样逐档抢派 → preview 顶住。
                    self.maybe_dispatch_zoom_preview(
                        start, end, scroll_y, viewport_h, page_width, scale_factor, window,
                    );
                } else {
                    self.schedule_fast_covers(
                        window,
                        start,
                        end,
                        scroll_y,
                        viewport_h,
                        page_width,
                        scale_factor,
                    );
                }
            }
            SpeedState::VeryFast => {
                // 极快平移主路径：不派可见页新栅格（封面/瓦片/占位一律不加活）——已缓存
                // 封面/瓦片原位即锐；新进视口未缓存页由 F4 预取走廊兜底（L3.1 起走廊在
                // 四主队列空时仍补 0.5 密度封面，≤2/帧、40MB 预算封顶），个别无隙页短暂
                // 「加载中」、降速即恢复，属可接受取舍。
                // **L1+L2 大跳落点直达**：`jump_armed` 为真时不再纯平移零派发——转对落点页
                // （`update_nav_jump` 刚武装的 `jump_target`）满密度派发封面（≤COVER_INFLIGHT
                // 复用既有预算；nav_gen 世代保证快速连续跳只渲染最新落点，旧请求出队即被
                // worker drop、在途旧结果被 view 清 pending 丢弃）。落点窗全部在槽/在途后
                // `maybe_dispatch_jump_covers` 自行清零；停稳后由 L1.3 直落 Slow 常态接管。
                // **S2 缩放 + 高速拖拽复合**：`zoom_armed` 为真（且非大跳）时，VeryFast 纯
                // 平移原本零派发——缩放期 visible 窗仍按当前 cscale 派 preview 顶住，防抖
                // 到期收口由常态调度接管；滚动大跳仍走 `jump_armed` 优先。
                if self.jump_armed {
                    self.maybe_dispatch_jump_covers(page_width, scale_factor);
                } else if self.zoom_armed {
                    self.maybe_dispatch_zoom_preview(
                        start, end, scroll_y, viewport_h, page_width, scale_factor, window,
                    );
                }
            }
        }
        // F4/B：非 idle 帧（窗口有移动/调度运行）按当前可见中心收回走廊窗外的旧廊封面
        // （距离 LRU + 字节预算）。idle 静置期的挂在 idle 分支的 `maybe_prefetch_corridor` 内。
        let vis_first = first_visible_page(&self.dims, &self.page_tops, page_width, scroll_y);
        self.evict_prefetch_covers(vis_first);
        self.evict_tiles(start, end, scale_factor);
        // S5 高倍 cover 回收（高倍稳态 1:1 瓦片接管后，cap 截断的 4.0 密度 cover 只是
        // 32MB/页 双驻留 + 边隙透雾的旧密度整页图 → 回收；评审 BLOCK③ 后按可见矩形对账，
        // 未铺满可见区不回收）。
        self.retire_high_scale_covers(
            start, end, scroll_y, viewport_h, page_width, scale_factor, window,
        );
        // F4/B：清尾帧（非快速 + 四主队列全空）也尝试派发走廊——「最后一帧工作清空后泵
        // 须续出几帧把走廊填掉」，否则没有任何 idle 帧会到来（泵在清尾帧即停）。
        self.maybe_prefetch_corridor(vis_first, page_width, scale_factor);

        // 更新门控状态（本轮已真实跑过调度）：签名对齐当前帧；settled = 非快速态
        // 且调度后无在途瓦片/封面（即确认「已无待派的新 tile 请求」→ 下帧可门）。
        // 快速态（占位调度）或仍有在途 → settled=false，绝不允许静态帧跳过调度。
        self.window_sig = Some(sig);
        self.window_settled = !self.prev_fast
            && self.pending_tiles.is_empty()
            && self.pending_cover.is_empty();

        // 7) 瓦片在途自刷新：若无用户输入就没有下一次重绘，而瓦片结果要等下一次
        //    `ensure_window` 才收集——启动 ~16ms 泵直到在途瓦片清空（静置也能出图）。
        self.ensure_tile_pump(cx);
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
                .map(|q| QuadPt { x0: q[0], y0: q[1], x1: q[2], y1: q[3] })
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
        let annot = PendingAnnot { id: self.next_annot_id, page: page_index, kind, quads_pt };
        self.pending_annots.push(annot.clone());
        self.undo_stack.push_back(UndoEntry::RemoveAnnot { id: annot.id });
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

/// 待保存页面操作的种类（[`PdfViewer::submit_mem_op`]）：只改 worker 内存工作副本。
enum MemOpKind {
    /// 在 `at_index` 处插入一页空白页（0 基；= 当前页数即末尾追加）。
    Insert { at_index: usize, w: f32, h: f32, anchor: usize },
    /// 删除第 `index` 页。
    Delete { index: usize, anchor: usize },
}

impl PdfViewer {
    /// 提交一次**内存级**页面操作（插页/删页）：worker 只改内存工作副本、**不写盘**，
    /// 成功回传新镜像 → viewer 内存级重载；待存注释页号随后平移/丢弃；撤销栈记录
    /// 逆操作（恢复操作前字节镜像 + 注释快照）。失败记 [`Self::last_modify_error`]。
    fn submit_mem_op(&mut self, op: MemOpKind, cx: &mut Context<Self>) {
        let Some(worker) = self.worker.clone() else {
            self.last_modify_error = Some("文档尚未加载".into());
            return;
        };
        if self.doc_id == 0 {
            self.last_modify_error = Some("文档尚未加载".into());
            return;
        }
        let doc_id = self.doc_id;
        self.last_modify_error = None;
        let anchor = match op {
            MemOpKind::Insert { anchor, .. } | MemOpKind::Delete { anchor, .. } => anchor,
        };
        // 页号平移闭包：插页 → 原 ≥ at_index 的待存注释 +1；删页 → 原 > index 的 -1、
        // == index 的丢弃（在待存注释上原地平移，确保结构变化后叠加重载仍对位）。
        let shift: Box<dyn Fn(&mut Vec<PendingAnnot>) + Send> = match op {
            MemOpKind::Insert { at_index, .. } => {
                Box::new(move |annots| {
                    for a in annots.iter_mut() {
                        if a.page >= at_index {
                            a.page += 1;
                        }
                    }
                })
            }
            MemOpKind::Delete { index, .. } => {
                Box::new(move |annots| {
                    annots.retain(|a| a.page != index);
                    for a in annots.iter_mut() {
                        if a.page > index {
                            a.page -= 1;
                        }
                    }
                })
            }
        };
        // ⚠️ 必须 `.detach()`（K37）：不 detach 句柄在 drop 即取消，页操作静默不执行。
        cx.spawn(async move |this, cx| {
            let res = cx.background_executor().spawn(async move { match op {
                MemOpKind::Insert { at_index, w, h, .. } => {
                    worker.mem_insert_blank_page(doc_id, at_index, w, h)
                }
                MemOpKind::Delete { index, .. } => worker.mem_delete_page(doc_id, index),
            } }).await;
            let _ = this.update(cx, move |v, cx| match res {
                Ok(mem) => {
                    v.last_modify_error = None;
                    // 撤销条目：恢复操作前的内存镜像 + 操作前待存注释快照
                    v.undo_stack.push_back(UndoEntry::RestoreMemory {
                        bytes: mem.before,
                        annots_before: v.pending_annots.clone(),
                    });
                    v.pending_page_ops += 1;
                    shift(&mut v.pending_annots);
                    // 结构变了 → 灰框几何可能漂移，清空最稳
                    v.focused_annot = None;
                    v.open_from_bytes(mem.after, Some(anchor), cx);
                    cx.notify();
                }
                Err(e) => {
                    v.last_modify_error = Some(format!("{e:#}"));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// 在 `page_index` 之后插入一页空白页（**内存级**：只改 worker 内存工作副本并重载，
    /// **不写盘**；`save()` 时才落盘）。（新页尺寸 = 当前页；插入后滚动到新页。）
    pub fn insert_blank_page_after(&mut self, page_index: usize, cx: &mut Context<Self>) {
        let Some(&(w, h)) = self.dims.get(page_index) else {
            self.last_modify_error = Some(format!("页码越界：第 {page_index} 页"));
            return;
        };
        self.submit_mem_op(
            MemOpKind::Insert { at_index: page_index + 1, w, h, anchor: page_index + 1 },
            cx,
        );
    }

    /// 删除 `page_index` 页（**内存级**：不写盘；仅剩 1 页拒绝）；删除后滚动保位（夹取）。
    pub fn delete_page(&mut self, page_index: usize, cx: &mut Context<Self>) {
        if page_index >= self.dims.len() {
            self.last_modify_error = Some(format!("页码越界：第 {page_index} 页"));
            return;
        }
        if self.dims.len() <= 1 {
            self.last_modify_error = Some("不能删除仅剩的 1 页".into());
            return;
        }
        self.submit_mem_op(MemOpKind::Delete { index: page_index, anchor: page_index }, cx);
    }

    /// 把 `page_index` 页提取成独立 PDF 文件并原子写盘（源文档不变，**即时生效、不影响
    /// 当前 dirty**——提取本质是「另存为新文件」）。
    pub fn extract_page_to(&mut self, page_index: usize, out_path: PathBuf, cx: &mut Context<Self>) {
        if page_index >= self.dims.len() {
            self.last_modify_error = Some(format!("页码越界：第 {page_index} 页"));
            return;
        }
        let Some(worker) = self.worker.clone() else {
            self.last_modify_error = Some("文档尚未加载".into());
            return;
        };
        if self.doc_id == 0 {
            self.last_modify_error = Some("文档尚未加载".into());
            return;
        }
        let doc_id = self.doc_id;
        self.last_modify_error = None;
        cx.spawn(async move |this, cx| {
            let res = cx
                .background_executor()
                .spawn(async move { worker.extract_page_to(doc_id, page_index, out_path) })
                .await;
            let _ = this.update(cx, move |v, cx| match res {
                Ok(_) => {
                    v.last_modify_error = None;
                    cx.notify();
                }
                Err(e) => {
                    v.last_modify_error = Some(format!("{e:#}"));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// 显式保存（Ctrl+S / 保存菜单）：把待存注释合并 + 内存工作副本原子写盘。无待存 →
    /// 直接返回 false（已是干净态）；发起异步保存返回 true——成功清 pending/undo/dirty
    /// 并 notify，失败记 [`Self::last_modify_error`]（壳层经 `_pdf_sub` toast 透出）。
    pub fn save(&mut self, cx: &mut Context<Self>) -> bool {
        self.submit_save(cx).is_some()
    }

    /// 另存为：把**当前内容**（内存工作副本 + 待存注释合并）写到 `new_path`，成功后当前
    /// 阅读器重指新路径并清 dirty（原文件保持原样）。失败记 [`Self::last_modify_error`]，
    /// 当前文档/待存状态不变。异步 fire-and-forget（壳层「另存为」对话框后调用）。
    pub fn save_as(&mut self, new_path: PathBuf, cx: &mut Context<Self>) {
        let Some(worker) = self.worker.clone() else {
            self.last_modify_error = Some("文档尚未加载".into());
            return;
        };
        if self.doc_id == 0 {
            self.last_modify_error = Some("文档尚未加载".into());
            return;
        }
        let doc_id = self.doc_id;
        self.last_modify_error = None;
        let has_annots = !self.pending_annots.is_empty();
        let items: Vec<AnnotateItem> = self
            .pending_annots
            .iter()
            .map(|a| AnnotateItem { page_index: a.page, kind: a.kind, quads: a.quads_pt.clone() })
            .collect();
        let new_path_set = new_path.clone();
        cx.spawn(async move |this, cx| {
            let res = cx
                .background_executor()
                .spawn(async move {
                    if has_annots {
                        worker.annotate(doc_id, items, new_path.clone()).map(|_| ())
                    } else {
                        worker.save(doc_id, new_path.clone())
                    }
                })
                .await;
            let _ = this.update(cx, move |v, cx| match res {
                Ok(_) => {
                    v.last_modify_error = None;
                    v.path = new_path_set;
                    // 另存后：内容已（合并待存/含页操作后）写入新路径 → 一律清待存
                    // （dirty 回落；worker.annotate 已顺带同步 stores 镜像）。
                    v.pending_annots.clear();
                    v.undo_stack.clear();
                    v.pending_page_ops = 0;
                    v.focused_annot = None;
                    v.sel_card = None;
                    cx.notify();
                }
                Err(e) => {
                    v.last_modify_error = Some(format!("{e:#}"));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// 显式保存并返回完成信号通道（关闭标签「保存并关闭」用；`None` = 无待存）。
    /// 接收端 `Ok(())` = 已落盘；`Err` = 保存失败（仍留在待存态）。
    pub fn save_channels(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<mpsc::Receiver<std::result::Result<(), String>>> {
        self.submit_save(cx)
    }

    #[allow(clippy::type_complexity)]
    fn submit_save(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<mpsc::Receiver<std::result::Result<(), String>>> {
        if !self.is_dirty() {
            return None;
        }
        let worker = self.worker.clone()?;
        if self.doc_id == 0 {
            self.last_modify_error = Some("文档尚未加载".into());
            return None;
        }
        let doc_id = self.doc_id;
        let path = self.path.clone();
        self.last_modify_error = None;
        let items: Vec<AnnotateItem> = self
            .pending_annots
            .iter()
            .map(|a| AnnotateItem { page_index: a.page, kind: a.kind, quads: a.quads_pt.clone() })
            .collect();
        let (tx, rx) = mpsc::channel();
        cx.spawn(async move |this, cx| {
            let res = cx
                .background_executor()
                .spawn(async move {
                    if items.is_empty() {
                        worker.save(doc_id, path.clone()).map(|_| None)
                    } else {
                        // 注释合并写盘成功后，顺带读回已写文件 bytes 供 live 文档重载——
                        // 否则 save 清掉叠加层后基础文档并不含注释，高亮会在视图中消失
                        // 直到重开才回来（b1 旧口径靠「重开读回」绕开了这个可见缺陷）。
                        worker
                            .annotate(doc_id, items, path.clone())
                            .map(|_| std::fs::read(&path).ok())
                    }
                })
                .await;
            let _ = tx.send(res.as_ref().map(|_| ()).map_err(|e: &anyhow::Error| format!("{e:#}")));
            let _ = this.update(cx, move |v, cx| match res {
                Ok(annotated_bytes) => {
                    v.last_modify_error = None;
                    v.pending_annots.clear();
                    v.undo_stack.clear();
                    v.pending_page_ops = 0;
                    v.focused_annot = None;
                    v.sel_card = None;
                    // 有注释待存 → live 文档重载为「含已写注释的新文件」并保持当前页，
                    // 叠加层清空后注释仍在基础文档里上图（零闪烁的注释添加路径不受影响）。
                    if let Some(bytes) = annotated_bytes {
                        let anchor = v.current_page().map(|(pg, _)| pg.saturating_sub(1));
                        v.open_from_bytes(bytes, anchor, cx);
                    }
                    cx.notify();
                }
                Err(e) => {
                    v.last_modify_error = Some(format!("{e:#}"));
                    cx.notify();
                }
            });
        })
        .detach();
        Some(rx)
    }

    /// 撤销上一条 PDF 编辑（Ctrl+Z）：注释逐条撤（加/删互逆）；页面插入/删除按**操作前
    /// 内存镜像**恢复（内容精确还原）。无待回退条目 → 空操作。
    pub fn undo_last(&mut self, cx: &mut Context<Self>) {
        let Some(entry) = self.undo_stack.pop_back() else {
            return;
        };
        match entry {
            UndoEntry::RemoveAnnot { id } => {
                self.pending_annots.retain(|a| a.id != id);
            }
            UndoEntry::AddAnnotBack { annot } => {
                self.pending_annots.push(annot);
            }
            UndoEntry::RestoreMemory { bytes, annots_before } => {
                self.pending_page_ops = self.pending_page_ops.saturating_sub(1);
                self.pending_annots = annots_before;
                self.focused_annot = None;
                let anchor = self.current_page().map(|(pg, _)| pg.saturating_sub(1));
                self.open_from_bytes(bytes, anchor, cx);
            }
        }
        cx.notify();
    }

    /// 删除一条待存注释（右键「删除注释」）：从 pending 移除并清灰框焦；撤销栈记录
    /// 逆操作（Ctrl+Z 可恢复该条）。
    pub fn delete_pending_annot(&mut self, id: u64, cx: &mut Context<Self>) {
        if let Some(pos) = self.pending_annots.iter().position(|a| a.id == id) {
            let annot = self.pending_annots.remove(pos);
            self.undo_stack.push_back(UndoEntry::AddAnnotBack { annot });
            if self.focused_annot == Some(id) {
                self.focused_annot = None;
            }
            cx.notify();
        }
    }

    /// 最近一次写回/页操作的错误文案（None = 无错误）。
    pub fn last_modify_error(&self) -> Option<&str> {
        self.last_modify_error.as_deref()
    }

    /// 读回某一页已**写盘**的注释（类型 + quad 几何）。需要已加载文档；走 worker 读回。
    /// 待保存模型下只反映 save() 之后落盘的内容；待存注释以 [`Self::pending_annots`]
    /// 叠加层渲染呈现。
    pub fn annotations_for_page(&self, page_index: usize) -> Option<Vec<AnnotationInfo>> {
        let worker = self.worker.as_ref()?;
        worker.annotations_for_page(self.doc_id, page_index).ok()
    }

    /// 从**内存字节**（非磁盘）重开文档：worker 侧 Close 旧文档 → 失效渲染派生缓存 →
    /// 后台打开新 bytes。待保存模型的页面操作/撤销恢复用它做「只一次内存级重载、不写盘」。
    /// 待存状态（pending_annots / undo / dirty）**保留**（页号已由调用方平移妥当）；
    /// 渲染派生缓存（文本层/选区/检索/门控）随结构变化全量失效。
    fn open_from_bytes(&mut self, bytes: Vec<u8>, anchor: Option<usize>, cx: &mut Context<Self>) {
        if let Some(w) = &self.worker
            && self.doc_id != 0 {
                w.close(self.doc_id);
            }
        self.worker = None;
        self.doc_id = 0;
        // 全量失效渲染派生缓存（同旧 reload_after_modify；不 pending_drops.clear()——
        // 旧位图应由下一帧 ensure_window 逐个 drop_image 下线，清掉只会泄漏显存提交，K14）。
        self.text_pages.clear();
        self.text_lru.clear();
        self.pending_text.clear();
        // 图片盒缓存与选中图片随结构变化全量失效（页号可能已平移）。
        self.image_pages.clear();
        self.image_lru.clear();
        self.pending_images.clear();
        self.selected_image = None;
        self.pending_thumb.clear();
        self.pending_mid.clear();
        self.pending_tiles.clear();
        self.pending_cover.clear();
        self.pending_recolor = None;
        self.recolor_scope.clear();
        self.recolor_received.clear();
        self.mid_lru.clear();
        self.tile_bytes = 0;
        self.tile_pump_running = false;
        self.sel_anchor = None;
        self.sel_active = None;
        self.selecting = false;
        self.sel_card = None;
        self.search_query.clear();
        self.search_matches.clear();
        self.search_truncated = false;
        self.search_active = 0;
        self.search_pending = None;
        self.window_sig = None;
        self.window_settled = false;
        self.reload_anchor = anchor;
        cx.spawn(async move |weak, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    PdfWorker::open_and_dims_full(bytes)
                        .map_err(|e| format!("无法启动 pdfium：{e}"))
                })
                .await;
            let _ = weak.update(cx, |this, cx| match result {
                Ok((worker, doc_id, dims, outlines)) => {
                    this.apply_loaded(worker, doc_id, dims, outlines, cx);
                }
                Err(e) => {
                    this.loading = false;
                    this.load_error = Some(e);
                    cx.refresh_windows();
                    cx.notify();
                }
            });
        })
        .detach();
    }
}

