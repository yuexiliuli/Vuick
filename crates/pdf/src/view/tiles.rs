//! PDF 阅读器子模块：worker 栅格化调度 / 瓦片与占位页缓存 / 内存预算（原 `view.rs` 拆出）。
//!
//! 覆盖瓦片级虚拟化的「产出侧」：自刷新泵（在途瓦片清空前的 ~16ms 重绘驱动）、
//! 常态瓦片调度（[`super::PdfViewer::schedule_tiles`] + 几何收敛点
//! [`super::PdfViewer::plan_tiles_in_window`]）、快速拖动占位（缩略/中清 + 瓦片预热）、
//! 在途结果回收（瓦片/封面/缩略/中清）与逐类内存预算淘汰（[`TILE_CACHE_BYTES`] /
//! [`THUMB_CACHE_PAGES`] / [`MID_CACHE_PAGES`]）。调度入口由 `open` 子模块的
//! [`super::PdfViewer::ensure_window`] 每帧驱动；回收的 `RenderImage` 一律经
//! `pending_drops` 显式 `drop_image` 下线（防显存/提交内存缓慢累积）。

use super::*;

impl PdfViewer {
    // ---- 瓦片在途自刷新泵 ----

    /// 瓦片在途期间的自刷新泵：仅当「有在途瓦片/封面」或「已加载但滚动容器尚未拿到真实
    /// 布局高度（首帧 bounds=0，此时视口为空 → 调度不到任何瓦片）」时启动，每 ~16ms 触发
    /// 一次窗口重绘（`refresh_windows`），直到工作清空后自动停止。避免"派发了瓦片但没有任何
    /// 事件触发重绘 → 首屏永远停留在占位/加载中"以及"首帧 bounds=0 → 永远调不到瓦片"。
    pub(super) fn ensure_tile_pump(&mut self, cx: &mut Context<Self>) {
        if self.tile_pump_running || !self.pump_still_needed() {
            return;
        }
        self.tile_pump_running = true;
        if timing_view() {
            eprintln!("[{:.1}ms] [VIEW-PUMP] start pending_t={}", crate::pdf::timing_ms(), self.pending_tiles.len());
        }
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(16))
                    .await;
                let still = this
                    .update(cx, |v, cx| {
                        if timing_view() {
                            eprintln!("[{:.1}ms] [VIEW-PUMP] tick pending_t={}", crate::pdf::timing_ms(), v.pending_tiles.len());
                        }
                        // 关键：`refresh_windows()` 无条件要求全部窗口重绘（不依赖实体是否被
                        // 窗口 tracking，测试/真实 app 都可靠）。单用 `notify()` 在实体未被
                        // `tracked_entities` 收录时是空操作 → “worker 渲完了但从未重绘”。
                        // 16ms 泵节流，工作清空即停。
                        cx.refresh_windows();
                        v.pump_still_needed()
                    })
                    .unwrap_or(false);
                if !still {
                    break;
                }
            }
            let _ = this.update(cx, |v, _cx| v.tile_pump_running = false);
        })
        .detach();
    }

    /// 泵是否仍需运行：有在途瓦片/封面 → 重绘收图；已加载但滚动容器布局仍为零高
    /// （首帧 layout 落后于 render）→ 重绘以拿到真实视口、否则瓦片永远调度不出来。
    /// **额外关键分支：仍处快速模式（`prev_fast`）时持续出帧。** 速度 EMA 靠“帧”衰减，
    /// 而帧由输入驱动——滚轮骤停时最后几帧速度还在快速阈值上（`fast=true` 且无在途 →
    /// 泵自停 → 再没有帧 → `prev_fast` 卡死、`ensure_window` 不再跑 → 可见页停在低清占位，
    /// 直到用户下一次输入才换挡补瓦片（实测日志：1.7 秒无 `[VIEW-TICK]`，正是“随机页先
    /// 模糊、数秒后清晰”的根因）。本分支让泵一直驱动到退出快速态并完成清尾，无需输入。
    pub(super) fn pump_still_needed(&self) -> bool {
        if self.prev_fast {
            return true;
        }
        if !self.pending_tiles.is_empty() || !self.pending_cover.is_empty() {
            return true;
        }
        // 重着色刷新在途：继续出帧直到 worker 回传的新着色副本 drain 完（否则晚到的
        // 回传没有下一帧可收集，重着色画面停在旧着色上）。
        if self.pending_recolor.is_some() {
            return true;
        }
        if !self.loading && !self.dims.is_empty() && self.load_error.is_none() {
            let viewport_h = f32::from(self.scroll.bounds().size.height);
            if viewport_h <= 0.0 {
                return true;
            }
        }
        false
    }

    /// 渲染驻留内存水位（计时日志，`GPUI_EDITOR_PDF_TIMING=1` 时每 ~30 帧打一行）：
    /// 瓦片字节 + 封面（整页位图，按当前密度估算字节）+ 中清/缩略计数，便于实时看
    /// “内存到底是谁占的”（瓦片 vs 封面 vs 中清 vs pdfium 内部未计入部分）。
    pub(super) fn log_memory_waterline(&self, scale_factor: f32) {
        let pw = self.page_width();
        let mids = self.slots.iter().filter(|s| s.mid.is_some()).count();
        let thumbs = self.slots.iter().filter(|s| s.thumb.is_some()).count();
        let mut covers = 0usize;
        let mut cover_bytes = 0u64;
        for (p, s) in self.slots.iter().enumerate() {
            if s.cover.is_some() {
                covers += 1;
                let cscale = cover_scale(self.dims[p], pw, scale_factor);
                let (w, h) = page_bitmap_size(self.dims[p], cscale);
                cover_bytes += (w as u64) * (h as u64) * 4;
            }
        }
        // 中清 ~2MB、缩略 ~0.37MB 为近似（不逐页精确，仅用于水位构成）
        let mid_bytes = mids as u64 * 2 * 1024 * 1024;
        let thumb_bytes = thumbs as u64 * 385 * 1024;
        let total = self.tile_bytes + cover_bytes + mid_bytes + thumb_bytes;
        eprintln!(
            "[{:.1}ms] [VIEW-MEM] tiles={:.1}MB covers={}({:.1}MB) mids={} thumbs={} est_total={:.1}MB",
            crate::pdf::timing_ms(),
            self.tile_bytes as f64 / 1048576.0,
            covers,
            cover_bytes as f64 / 1048576.0,
            mids,
            thumbs,
            total as f64 / 1048576.0,
        );
    }

    // ---- 常态瓦片调度（可见窗 ±1 邻居）----

    /// 常态瓦片调度：为可见页推算视口相交的瓦片网格区间，惰性作废旧密度瓦片，
    /// 按「离视口中心远近」排序后向 worker 派发（[`TILE_INFLIGHT`] 节流）。
    pub(super) fn schedule_tiles(
        &mut self,
        window: &Window,
        start: usize,
        end: usize,
        page_width: f32,
        scale_factor: f32,
        scroll_y: f32,
    ) {
        let viewport_h = f32::from(self.scroll.bounds().size.height);
        let tile_size = TILE_SIZE;
        // 先量「想看的窗」[真正的可见页 ±1 邻居]（与封面同源）：瓦片与封面都只服务这一窗。
        // `start..=end` 来自 `keep_window`、含 MARGIN 页；滚得更远的页**不再预取瓦片/封面**
        // ——滚动越过的页即时回收，驻留内存不再随文档长度增长（MARGIN 页到眼前自然进窗
        // 再按需渲染，由封面/瓦片补上）。
        let (vis_first, vis_last) =
            self.visible_page_span(start, end, scroll_y, viewport_h, page_width, window);
        let vis_first = vis_first.unwrap_or(start);
        let vis_last = vis_last.unwrap_or(end);
        let first = vis_first.saturating_sub(1);
        let last = (vis_last + 1).min(self.dims.len().saturating_sub(1));
        let center = (start + end) / 2;

        // 惰性失效：缩放/DPI 已变 → 清该页旧密度瓦片（在途旧密度结果由 scale 守卫丢弃）。
        // 与瓦片规划解耦成独立一遍：失效只写（账随槽位走，见 `Slot::tile_bytes`）、
        // 规划只读（[`super::PdfViewer::plan_tiles_in_window`]），互不串扰。
        for page in first..=last {
            let scale = render_scale(page_width, scale_factor, self.dims[page].0);
            let slot = &mut self.slots[page];
            if (slot.tile_scale - scale).abs() > 1e-4 {
                let old_bytes = slot.tile_bytes;
                slot.tile_bytes = 0;
                self.tile_bytes = self.tile_bytes.saturating_sub(old_bytes);
                for (_, ts) in slot.tiles.drain() {
                    self.pending_drops.push(ts.image);
                }
                slot.tile_scale = scale;
            }
        }
        // 瓦片规划（与快速拖动占位的瓦片预热共用同一份几何，见 `plan_tiles_in_window`）：
        // **T1**：`plan_tiles_in_window` 会跳过「封面 affordable 且与瓦片同密度」的页
        // （封面将整页一次到位），只有「封面不可负担/被 COVER_SCALE_CAP 截断的底图+精修」
        // 页才产生瓦片——首屏不再出现「同页瓦片先派 + 同密度封面后到再整体作废」的双渲。
        let mut need = self.plan_tiles_in_window(
            first..=last,
            center,
            page_width,
            scale_factor,
            scroll_y,
            viewport_h,
            window,
        );

        // 瓦片（**先派**，仅「封面不可负担」的页）：可见页瓦片先进 worker 队列 → 停稳瞬间
        // 可见条带每片 ~2-8ms 逐片出图、首片 ~5ms 就清晰。**保持封面后派**：worker 的
        // render_q 是严格 FIFO，把大封面排到瓦片前面会阻塞「底图+精修」的细瓦片（static_gate
        // 无真机 sleep 的 300 帧停不稳即此类）。T1 的「同页不双渲」由规划跳过保证，与派发
        // 先后无关。
        if need.is_empty() {
            if timing_view() {
                eprintln!("[{:.1}ms] [VIEW-SCHED] need empty pending_t={}", crate::pdf::timing_ms(), self.pending_tiles.len());
            }
        } else {
            // 靠近视口的瓦片先渲染（正在看的那片先填）
            need.sort_by_key(|&(_, _, _, d)| d);
            let worker = self.worker.clone().unwrap();
            let budget = TILE_INFLIGHT.saturating_sub(self.pending_tiles.len());
            for (page, row, col, _) in need.into_iter().take(budget) {
                let scale = render_scale(page_width, scale_factor, self.dims[page].0);
                let rx = worker.request_tile(
                    self.doc_id,
                    TileRequest {
                        page,
                        scale,
                        tile_size,
                        row,
                        col,
                    },
                );
                self.raster_reqs += 1; // 切档不重栅回归计数
                self.tile_reqs += 1; // T1 首屏「封面 vs 瓦片」构成计数
                self.pending_tiles.insert((page, row, col), (scale, rx));
            }
        }

        // 封面（**后派**）：**只服务想看的窗 [first..=last]**（可见页 ±1 邻居；非 MARGIN
        // 全窗——否则候选页数超驻留上限 → 换页风暴）。中心页优先出队。**T1**：封面与瓦片
        // 同密度的页因规划跳过已无瓦片，封面即是该页唯一供给 → 无同页双渲。
        let cover_worker = self.worker.clone().unwrap();
        let center_vis = (vis_first + vis_last) / 2;
        let mut cover_order: Vec<usize> = (first..=last).collect();
        cover_order.sort_by_key(|&p| (p as i64 - center_vis as i64).unsigned_abs());
        for page in cover_order {
            let dim = self.dims[page];
            let cscale = cover_scale(dim, page_width, scale_factor);
            // 缩放过期（缩放/DPI 已变）→ 旧封面移入 `prev_cover` 作 CSS 拉伸兜底底图
            // （不再立即下线：本帧渲染还靠它铺满新页宽，防「Ctrl+滚轮缩放」闪「加载中」）。
            // `cover` 置空 → 下方按新密度重派封面，新封面到位后取代并下线旧封面。
            if let Some(slot) = self.slots.get_mut(page) {
                let stale = slot.cover.is_some() && (slot.cover_scale - cscale).abs() > 1e-4;
                if stale
                    && let Some(old) = slot.cover.take() {
                        slot.prev_cover = Some(old);
                    }
            }
            // 整页不超预算 且 无封面 且 未在途 → 派发
            if cover_affordable(dim, cscale)
                && self.slots[page].cover.is_none()
                && !self.pending_cover.contains_key(&page)
                && self.pending_cover.len() < COVER_INFLIGHT
            {
                if timing_view() {
                    eprintln!("[{:.1}ms] [VIEW-COVER-REQ] page={page}", crate::pdf::timing_ms());
                }
                let rx =
                    cover_worker.request_render(self.doc_id, RenderRequest { page, scale: cscale });
                self.raster_reqs += 1; // 切档不重栅回归计数
                self.cover_reqs += 1; // T1 首屏「封面 vs 瓦片」构成计数
                self.pending_cover.insert(page, (cscale, rx));
            }
        }
        self.evict_covers(first, last);
    }

    /// 页 `page` 在当前视口内**可见**的卡片局部矩形 `(x0, y0, x1, y1)`（CSS px），
    /// 已夹在页内；完全不可见（滚出窗口/水平溢出）→ `None`。与 `hit_test` 用同一套
    /// 坐标链（窗口 → 滚动 → wrap → 去列左偏移），保证"圈中的瓦片 = 正在看的瓦片"。
    pub(super) fn page_visible_rect(
        &self,
        page: usize,
        scroll_y: f32,
        viewport_h: f32,
        page_width: f32,
        wrap_w: f32,
    ) -> Option<(f32, f32, f32, f32)> {
        let top = self.page_tops.get(page).copied()?;
        let dim = self.dims.get(page).copied()?;
        let disp_h = display_height(dim, page_width);
        let vy0 = (scroll_y - top).clamp(0.0, disp_h);
        let vy1 = (scroll_y + viewport_h - top).clamp(0.0, disp_h);
        if vy1 <= vy0 {
            return None;
        }
        let col_left = column_left(wrap_w, page_width);
        let off_x = f32::from(-self.scroll.offset().x);
        let viewport_w = f32::from(self.scroll.bounds().size.width);
        let vx0 = (off_x - col_left).clamp(0.0, page_width);
        let vx1 = (off_x - col_left + viewport_w).clamp(0.0, page_width);
        if vx1 <= vx0 {
            return None;
        }
        Some((vx0, vy0, vx1, vy1))
    }

    /// 真正**可见**页首/尾（`page_visible_rect` 非空页），排除 [`keep_window`] 的 MARGIN
    /// 缓冲页——`schedule_tiles`（封面向导的窗）与 `schedule_placeholders`（瓦片预热）共用，
    /// 收敛两处原各写一份的可见页首尾扫描。
    pub(super) fn visible_page_span(
        &self,
        start: usize,
        end: usize,
        scroll_y: f32,
        viewport_h: f32,
        page_width: f32,
        window: &Window,
    ) -> (Option<usize>, Option<usize>) {
        let wrap_w = self.wrap_width(window, page_width);
        let mut vis_first: Option<usize> = None;
        let mut vis_last: Option<usize> = None;
        for page in start..=end {
            if self
                .page_visible_rect(page, scroll_y, viewport_h, page_width, wrap_w)
                .is_some()
            {
                vis_first.get_or_insert(page);
                vis_last = Some(page);
            }
        }
        (vis_first, vis_last)
    }

    /// 瓦片规划**收敛点**：与 [`super::PdfViewer::schedule_placeholders`] 的瓦片预热共用同一份几何
    /// （两侧逐像素一致、收敛一处即消错位源）。对 `pages` 区间逐页推算「视口相交 +
    /// TILE_MARGIN 缓冲」的瓦片网格，去重（已在 `slot.tiles` / 在途 `pending_tiles`），
    /// 并按「离 `center` 远近 + 离网格中心行远近」打分，返回 `(page, row, col, d)` 待派集合。
    ///
    /// - `center` 由调用方给出：常态调度用它算 `keep_window` 区间的算术中心，快速拖动
    ///   占位预热用它算可见页中心（两侧本就不同，属调用方窗口语义，不入本函数）。
    /// - 整页封面同密度在位 → 该页瓦片冗余（跳过，省渲染与内存），判据与两侧完全一致。
    /// - 密度失效的清除是**副作用**（改 `slot.tiles` 与槽位账目 `Slot::tile_bytes`），
    ///   由调用方规划前单独做；本函数只读 `slots` / `pending_tiles`。
    pub(super) fn plan_tiles_in_window(
        &self,
        pages: std::ops::RangeInclusive<usize>,
        center: usize,
        page_width: f32,
        scale_factor: f32,
        scroll_y: f32,
        viewport_h: f32,
        window: &Window,
    ) -> Vec<(usize, u32, u32, u64)> {
        let wrap_w = self.wrap_width(window, page_width);
        let tile_size = TILE_SIZE;
        let mut need: Vec<(usize, u32, u32, u64)> = Vec::new();
        for page in pages {
            let dim = self.dims[page];
            let scale = render_scale(page_width, scale_factor, dim.0);
            let Some(rect) =
                self.page_visible_rect(page, scroll_y, viewport_h, page_width, wrap_w)
            else {
                continue;
            };
            // 整页封面已在当前密度下覆盖整页 → 该页瓦片全部冗余，**不再派发**（省渲染与
            // 内存：封面即全页锐化底图，瓦片只服务于「无封面/封面被密度上限截断」的页）。
            // **T1：封面 affordable 且与瓦片同密度**（`cover_scale == scale`，封面可整页
            // 一次到位取代瓦片）的页同样跳过瓦片——这类页由 `schedule_tiles` 的同帧封面
            // 派发供给（瓦片跳过后封面即该页唯一栅格），首屏/翻页不再出现「瓦片先派 +
            // 同密度封面后到再作废」的双重栅格。本函数
            // 是 `schedule_tiles` 与快速拖动减速预热 `schedule_placeholders` 的共用几何，
            // 后者因此也不会在停稳前预派这类页的瓦片（避免补封面时双渲）。仅当封面密度被
            // COVER_SCALE_CAP 截断（`cscale < scale`，超高倍率「底图+精修」——瓦片更细）
            // 或页位图超 COVER_SIZE_BUDGET（纯瓦片路径）时，才继续按瓦片派发。
            let cscale = cover_scale(dim, page_width, scale_factor);
            let cover_serves_tile_density =
                cover_affordable(dim, cscale) && (cscale - scale).abs() <= 1e-4;
            let skip_covered = cover_serves_tile_density
                || (self.slots[page].cover.is_some()
                    && (self.slots[page].cover_scale - scale).abs() <= 1e-4);
            if skip_covered {
                continue;
            }
            let bmp = page_bitmap_size(dim, scale);
            let disp_h = display_height(dim, page_width);
            let Some((r0, c0, r1, c1)) =
                visible_tile_range(bmp, page_width, disp_h, rect, tile_size, TILE_MARGIN)
            else {
                continue;
            };
            let mid_row = (r0 + r1) / 2;
            for row in r0..=r1 {
                for col in c0..=c1 {
                    if self.slots[page].tiles.contains_key(&(row, col))
                        || self.pending_tiles.contains_key(&(page, row, col))
                    {
                        continue;
                    }
                    let d = (page as i64 - center as i64).unsigned_abs() * 1_000_000
                        + (row as i64 - mid_row as i64).unsigned_abs();
                    need.push((page, row, col, d));
                }
            }
        }
        need
    }

    // ---- 快速拖动占位 ----

    /// 快速拖动占位：为可见页派发 L0.5 中清（比缩略图清晰，可辨识标题/段落）、
    /// L0 缩略图（纯内存，无磁盘缓存）。停止后常态调度自动补齐瓦片、替换占位。
    pub(super) fn schedule_placeholders(
        &mut self,
        window: &Window,
        start: usize,
        end: usize,
        scroll_y: f32,
        viewport_h: f32,
        scale_factor: f32,
    ) {
        let worker = self.worker.clone().unwrap();
        // 1) L0.5 中清：优先派发（比缩略图清晰），独立额度不挤占瓦片/缩略图
        if self.pending_mid.len() < MAX_MID_INFLIGHT {
            let mut budget = MAX_MID_INFLIGHT - self.pending_mid.len();
            for page in start..=end {
                if budget == 0 {
                    break;
                }
                if self.mid_cached(page).is_some() {
                    continue; // 主线程缓存命中：直接复用（含 LRU 触摸），不发 WorkerMsg::Mid
                }
                if self.slots[page].mid_loading {
                    continue;
                }
                self.slots[page].mid_loading = true;
                let rx = worker.request_mid(self.doc_id, page, scale_factor);
                self.raster_reqs += 1; // 切档不重栅回归计数
                self.pending_mid.insert(page, rx);
                budget -= 1;
            }
        }
        // 2) L0 缩略图：仅为「无中清/无在途中清」的可见页派发
        if self.pending_thumb.len() < MAX_THUMB_INFLIGHT {
            let mut tb = MAX_THUMB_INFLIGHT - self.pending_thumb.len();
            for page in start..=end {
                if tb == 0 {
                    break;
                }
                if self.slots[page].thumb.is_some()
                    || self.slots[page].thumb_loading
                    || self.slots[page].mid.is_some()
                    || self.slots[page].mid_loading
                {
                    continue;
                }
                self.slots[page].thumb_loading = true;
                let rx = worker.request_thumb(self.doc_id, page);
                self.raster_reqs += 1; // 切档不重栅回归计数
                self.pending_thumb.insert(page, rx);
                tb -= 1;
            }
        }
        // 3) 减速期可见条带瓦片预热：仍 fast（迟滞未退出）但速度已降到「进入阈值」之下 →
        //    用户即将停稳。此时把**真正可见页**缺失的瓦片提前派发（中心页优先、
        //    TILE_INFLIGHT 封顶）：停稳换挡瞬间可见条带多半已被瓦片填满 → “停稳即清晰”
        //    走快路径（小片 ~2-8ms 逐片出图）。
        //    ⚠️ 封面是“慢路径”的修正依据：日志佐证 **[PAGE-RENDER] scale=2.47 整页封面
        //    20-40ms 一张、窗内 2-3 张串行，且堵在瓦片前面**；而“停稳即清晰”的页都是纯瓦片
        //    填满、没有整页封面。故这里不再派封面（原来预热的封面恰恰制造了“模糊→换挡→
        //    重新清晰”的慢感）；封面改由停稳帧 `schedule_tiles` 在瓦片之后预取邻居/下折叠。
        //    ⚠️ 可见区间必须与 `schedule_tiles` 同源（`page_visible_rect` 非空页），不能用
        //    `keep_window` 的算术中心（含 MARGIN 缓冲页，会偏出真正可见页）。
        //    风暴安全：速度 > MOVE_THRESHOLD_PX 的长距飞掠不派（避免小片洪流）；同一片
        //    已在 slot / 在途则跳过；TILE_INFLIGHT 封顶。（本函数只在 `prev_fast` 分支被调用。）
        if self.scroll_velocity < MOVE_THRESHOLD_PX {
            let page_width = self.page_width();
            let (vis_first, vis_last) =
                self.visible_page_span(start, end, scroll_y, viewport_h, page_width, window);
            if let (Some(vf), Some(vl)) = (vis_first, vis_last) {
                // 可见区间中心（非 keep_window 算术中心：含 MARGIN 缓冲页会偏出真正可见页，
                // 见上方注释）。瓦片几何与停稳帧 `schedule_tiles` 同源（`plan_tiles_in_window`）。
                let center_vis = (vf + vl) / 2;
                let mut need = self.plan_tiles_in_window(
                    vf..=vl,
                    center_vis,
                    page_width,
                    scale_factor,
                    scroll_y,
                    viewport_h,
                    window,
                );
                // 中心页/中心行优先，只补在途预算内
                need.sort_by_key(|&(_, _, _, d)| d);
                let budget = TILE_INFLIGHT.saturating_sub(self.pending_tiles.len());
                for (page, row, col, _) in need.into_iter().take(budget) {
                    if timing_view() {
                        eprintln!("[{:.1}ms] [VIEW-TILE-REQ] warm p{page} r{row}c{col}", crate::pdf::timing_ms());
                    }
                    let scale = render_scale(page_width, scale_factor, self.dims[page].0);
                    let rx = worker.request_tile(
                        self.doc_id,
                        TileRequest {
                            page,
                            scale,
                            tile_size: TILE_SIZE,
                            row,
                            col,
                        },
                    );
                    self.raster_reqs += 1; // 切档不重栅回归计数
                    self.tile_reqs += 1; // T1 首屏「封面 vs 瓦片」构成计数
                    self.pending_tiles.insert((page, row, col), (scale, rx));
                }
            }
        }
    }

    // ---- 在途结果回收 ----

    /// 回收已完成的在途瓦片结果：worker 回传 RGBA → 重建 `RenderImage` 装入对应页槽
    /// （密度守卫：请求时的 `scale` ≠ 当前 `scale` → 丢弃，缩放已变）。替换旧瓦片时
    /// 旧纹理走 `pending_drops` 显式下线。返回是否有新瓦片入槽（调用方决定重绘）。
    pub(super) fn collect_tile_results(&mut self, scale_factor: f32) -> bool {
        let finished = collect_finished(&mut self.pending_tiles, |(_s, rx)| rx.try_recv());
        if finished.is_empty() {
            return false;
        }
        let page_width = self.page_width();
        let mut any = false;
        for (key, res) in finished {
            let req_scale = self
                .pending_tiles
                .remove(&key)
                .map(|(s, _)| s)
                .unwrap_or(0.0);
            let (page, row, col) = key;
            let Some(res) = res else { continue };
            if page >= self.dims.len() {
                continue;
            }
            let scale = render_scale(page_width, scale_factor, self.dims[page].0);
            // 结果一致性守卫：回传格号/页号必须与请求一致、页位图尺寸必须与当前密度
            // 同宗（否则丢弃，防错位拼片）。
            if res.page != page || res.row != row || res.col != col {
                continue;
            }
            let cur_bmp = page_bitmap_size(self.dims[page], scale);
            if res.full_w != cur_bmp.0 || res.full_h != cur_bmp.1 {
                continue;
            }
            if (req_scale - scale).abs() > 1e-4 {
                continue; // 缩放/DPI 已变 → 过期密度瓦片丢弃
            }
            let Ok(img) = render_image_from_bgra(res.bgra, res.w, res.h) else {
                continue;
            };
            if let Some(slot) = self.slots.get_mut(page) {
                // 该页此时已有同密度整页封面 → 瓦片冗余（调度侧已不派新的，此处兜底收到
                // 封面到位前/后仍可能在途的老瓦片）：直接回收，不占内存不占显存。
                if slot.cover.is_some() && (slot.cover_scale - scale).abs() <= 1e-4 {
                    self.pending_drops.push(img);
                    continue;
                }
                let ts = TileSlot {
                    image: img,
                    x0: res.x0,
                    y0: res.y0,
                    w: res.w,
                    h: res.h,
                };
                let est = ts.bytes();
                if let Some(old) = slot.tiles.insert((row, col), ts) {
                    slot.tile_bytes = slot.tile_bytes.saturating_sub(old.bytes());
                    self.tile_bytes = self.tile_bytes.saturating_sub(old.bytes());
                    self.pending_drops.push(old.image);
                }
                slot.tile_bytes = slot.tile_bytes.saturating_add(est);
                self.tile_bytes = self.tile_bytes.saturating_add(est);
                any = true;
            }
        }
        any
    }

    /// 瓦片字节预算淘汰（LRU by 距视口中心远近）。先重算精确占用（自愈记账漂移），
    /// 超预算时从最远瓦片开始移除，旧纹理走 `pending_drops` 显式下线 GPU 纹理。
    /// **每帧全局清理（同密度封面瓦片冗余 + 过期密度瓦片）**：`schedule_tiles` 只对窗内
    /// 页做密度失效，窗外页的旧密度瓦片会滞留到 32MB 预算满——这里对**所有页**统一清：
/// - 同密度整页封面在位（`cover_scale≈当前密度`）→ 瓦片冗余，清（封面即全页锐化底图）；
/// - `tile_scale` 与当前密度不符 → 瓦片已过期（回看也必重渲），清。
///
/// 只留下「无封面、当前密度」的窗外页瓦片作滚动回看缓存，受 [`TILE_CACHE_BYTES`] 约束。
    pub(super) fn evict_tiles(&mut self, start: usize, end: usize, scale_factor: f32) {
        let pw = self.page_width();
        // 统一从各槽位账簿汇总全局占用（O(页)）：替代原「O(页×瓦片) 全扫自愈重算」——
        // 各瓦片增删点已同步维护 `Slot::tile_bytes`（单点增删改账），此处只做兜底对账。
        self.tile_bytes = self.slots.iter().map(|s| s.tile_bytes).sum();
        {
            let mut drops: Vec<Arc<RenderImage>> = Vec::new();
            for (p, slot) in self.slots.iter_mut().enumerate() {
                let cur = render_scale(pw, scale_factor, self.dims[p].0);
                let redundant =
                    slot.cover.is_some() && (slot.cover_scale - cur).abs() <= 1e-4;
                let stale = (slot.tile_scale - cur).abs() > 1e-4;
                if redundant || stale {
                    let bytes = slot.tile_bytes;
                    slot.tile_bytes = 0;
                    self.tile_bytes = self.tile_bytes.saturating_sub(bytes);
                    for (_, ts) in slot.tiles.drain() {
                        drops.push(ts.image);
                    }
                }
            }
            self.pending_drops.extend(drops);
        }
        if self.tile_bytes <= TILE_CACHE_BYTES {
            return;
        }
        let center = (start + end) / 2;
        let mut candidates: Vec<(u64, usize, u32, u32)> = Vec::new();
        for (page, slot) in self.slots.iter().enumerate() {
            for &(row, col) in slot.tiles.keys() {
                let d = (page as i64 - center as i64).unsigned_abs() * 1_000_000
                    + row as u64;
                candidates.push((d, page, row, col));
            }
        }
        candidates.sort_by_key(|c| std::cmp::Reverse(c.0)); // 最远优先
        for (_, page, row, col) in candidates {
            if self.tile_bytes <= TILE_CACHE_BYTES {
                break;
            }
            if let Some(slot) = self.slots.get_mut(page)
                && let Some(ts) = slot.tiles.remove(&(row, col)) {
                    let bytes = ts.bytes();
                    slot.tile_bytes = slot.tile_bytes.saturating_sub(bytes);
                    self.tile_bytes = self.tile_bytes.saturating_sub(bytes);
                    self.pending_drops.push(ts.image);
                }
        }
    }

    /// 回收已完成的在途**封面**结果：worker 回传整页（屏幕密度）BGRA → 装入 `slot.cover`
    /// （首屏锐化，先于瓦片到达；密度守卫：过期即弃）。返回是否有新封面入槽。
    pub(super) fn collect_cover_results(&mut self, scale_factor: f32) -> bool {
        let finished = collect_finished(&mut self.pending_cover, |(_, rx)| rx.try_recv());
        let mut any = false;
        for (page, res) in finished {
            let req_scale = self
                .pending_cover
                .remove(&page)
                .map(|(s, _)| s)
                .unwrap_or(0.0);
            let Some(res) = res else {
                continue;
            };
            if page >= self.dims.len() {
                continue;
            }
            let cscale = cover_scale(self.dims[page], self.page_width(), scale_factor);
            if (req_scale - cscale).abs() > 1e-4 {
                continue; // 缩放/DPI 已变 → 过期封面丢弃
            }
            let (w, h) = (res.width, res.height);
            let Ok(img) = render_image_from_bgra(res.bgra, w, h) else {
                continue;
            };
            if let Some(s) = self.slots.get_mut(page) {
                if let Some(old) = s.cover.replace(img) {
                    self.pending_drops.push(old);
                }
                // 新密度封面到位 → 旧密度兜底底图（prev_cover）随之下线回收。
                if let Some(prev) = s.prev_cover.take() {
                    self.pending_drops.push(prev);
                }
                s.cover_scale = cscale;
                // **T1**：封面 = 全页同密度整页位图，覆盖整页 → **不再在封面到位时把该页
                // 已示瓦片整体作废/drain**（旧语义「同密度封面一到即清空瓦片」= 把「瓦片
                // 先派」的成果一轮作废，恰是首屏双重栅格税的后半段；封面先行后本分支应
                // 已无瓦片）。保留的瓦片与封面同密度、显示外观逐像素一致；统一由
                // `evict_tiles` 的「同密度封面冗余」每帧清扫回收（内存回归守卫
                // `double_hold == 0` 不变，封面与瓦片不会长期双份驻留）。
                // 中清占位仍随封面到位回收（低清占位被整页锐化封面取代；存量由
                // `mid_lru` 预算兜底，此处只清本页那份）。
                let _ = (w, h);
                if let Some(old) = s.mid.take() {
                    self.mid_lru.retain(|&p| p != page);
                    self.pending_drops.push(old);
                }
                any = true;
            }
        }
        any
    }

    /// 回收**重着色刷新**结果（切档不重栅主路径）：worker 把「已缓存未着色原图 → 按新参数
    /// 现算的着色副本」逐条回传，本方法按 `kind` 替换对应槽位显示位图（旧图走 `pending_drops`
    /// 显式下线）。密度守卫与各 `collect_*` 同宗：
    /// - 封面：`entry.scale` 与当前 `cover_scale` 不符 → 丢弃（缩放已变，旧密度不会显示）；
    /// - 瓦片：槽内**已有**同格瓦片才替换（view 侧槽位即「正在显示」的真值；已被淘汰/
    ///   密度失效的格不入新图，避免把过期密度的原图重新塞回），并按新尺寸更新字节账；
    /// - 中清/缩略：槽内已有相应占位才替换。
    /// 通道 drain 到 `Disconnected`（worker 发完）即清 `pending_recolor`（泵随之自停）。
    /// 返回是否有位图被替换（调用方决定重绘）。
    pub(super) fn collect_recolor_results(&mut self, scale_factor: f32) -> bool {
        let Some(rx) = self.pending_recolor.as_ref() else {
            return false;
        };
        let page_width = self.page_width();
        let mut any = false;
        let mut done = false;
        loop {
            let entry = match rx.try_recv() {
                Ok(e) => e,
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    done = true;
                    break;
                }
            };
            if entry.page >= self.dims.len() {
                continue;
            }
            let Ok(img) = render_image_from_bgra(entry.bgra, entry.w, entry.h) else {
                continue;
            };
            let page = entry.page;
            match entry.kind {
                RawKind::Cover => {
                    let cscale = cover_scale(self.dims[page], page_width, scale_factor);
                    if (entry.scale - cscale).abs() > 1e-4 {
                        self.pending_drops.push(img);
                        continue;
                    }
                    if let Some(s) = self.slots.get_mut(page)
                        // 只替换「当前正在显示的封面」：页槽无封面（瓦片驱动）时不把整页
                        // 封面塞回，避免重着色刷新把显示模式从「瓦片」翻成「封面」。
                        && s.cover.is_some()
                    {
                        if let Some(old) = s.cover.replace(img) {
                            self.pending_drops.push(old);
                        }
                        // 旧密度封面兜底（prev_cover）在封面刷新时一并回收（新封面即时覆盖）。
                        if let Some(prev) = s.prev_cover.take() {
                            self.pending_drops.push(prev);
                        }
                        s.cover_scale = entry.scale;
                        any = true;
                    } else {
                        self.pending_drops.push(img);
                    }
                }
                RawKind::Tile => {
                    let Some(s) = self.slots.get_mut(page) else {
                        self.pending_drops.push(img);
                        continue;
                    };
                    let Some(old) = s.tiles.get_mut(&(entry.row, entry.col)) else {
                        // 该格当前不在显示槽（已被淘汰/密度失效）→ 不把过期密度的原图塞回。
                        self.pending_drops.push(img);
                        continue;
                    };
                    let ts = TileSlot {
                        image: img,
                        x0: entry.x0,
                        y0: entry.y0,
                        w: entry.w,
                        h: entry.h,
                    };
                    let old_bytes = old.bytes();
                    let new_bytes = ts.bytes();
                    s.tile_bytes = s.tile_bytes.saturating_sub(old_bytes).saturating_add(new_bytes);
                    self.tile_bytes = self
                        .tile_bytes
                        .saturating_sub(old_bytes)
                        .saturating_add(new_bytes);
                    let old_img = std::mem::replace(old, ts).image;
                    self.pending_drops.push(old_img);
                    any = true;
                }
                RawKind::Mid => {
                    if let Some(s) = self.slots.get_mut(page)
                        && let Some(old) = s.mid.replace(img.clone())
                    {
                        self.mid_lru.retain(|&p| p != page);
                        self.mid_lru.push_back(page);
                        self.pending_drops.push(old);
                        any = true;
                    } else {
                        self.pending_drops.push(img);
                    }
                }
                RawKind::Thumb => {
                    if let Some(s) = self.slots.get_mut(page)
                        && let Some(old) = s.thumb.replace(img.clone())
                    {
                        self.pending_drops.push(old);
                        any = true;
                    } else {
                        self.pending_drops.push(img);
                    }
                }
            }
        }
        if done {
            self.pending_recolor = None;
        }
        any
    }

    /// 封面内存管理：**只保留当前想要的封面 `[wanted_first..=wanted_last]`（可见页 ±1
    /// 邻居），窗外封面逐帧回收**。窗口内 ≤ 可见页+2 页，内存有界且**绝不触发换页风暴**：
    /// 当前窗口的封面永不被淘汰 → 不会被反复重请求；窗外的旧封面（随滚动累积）下帧即清。
    pub(super) fn evict_covers(&mut self, wanted_first: usize, wanted_last: usize) {
        let mut drops: Vec<Arc<RenderImage>> = Vec::new();
        for (page, slot) in self.slots.iter_mut().enumerate() {
            if slot.cover.is_some() && (page < wanted_first || page > wanted_last)
                && let Some(old) = slot.cover.take() {
                    drops.push(old);
                }
            // 旧密度封面兜底同样只保留在当前封面窗口内（窗外随滚即回收，防越滚越积）。
            if slot.prev_cover.is_some() && (page < wanted_first || page > wanted_last)
                && let Some(old) = slot.prev_cover.take() {
                    drops.push(old);
                }
        }
        self.pending_drops.extend(drops);
    }

    /// 回收已完成的在途 **L0 缩略图**结果：worker 回传低清 BGRA → 装入 `slot.thumb`
    /// （纯内存占位，不落盘）。非阻塞：仅检查已就绪通道；返回是否有新缩略图入槽。
    pub(super) fn collect_thumb_results(&mut self) -> bool {
        let finished = collect_finished(&mut self.pending_thumb, |rx| rx.try_recv());
        let mut any = false;
        for (i, res) in finished {
            self.pending_thumb.remove(&i);
            match res {
                Some(res) => {
                    let (w, h) = (res.width, res.height);
                    if let Some(s) = self.slots.get_mut(i) {
                        s.thumb_loading = false;
                        if let Ok(img) = render_image_from_bgra(res.bgra, w, h) {
                            s.thumb = Some(img);
                        }
                        any = true;
                    }
                }
                None => {
                    if let Some(s) = self.slots.get_mut(i) {
                        s.thumb_loading = false; // 失败，允许之后重试
                    }
                }
            }
        }
        any
    }

    /// 回收已完成的在途 **L0.5 中清页**结果：worker 回传 ~520px BGRA → 装入 `slot.mid`
    /// （快速拖动占位底图）。非阻塞；返回是否有新中清入槽（调用方决定重绘）。
    pub(super) fn collect_mid_results(&mut self) -> bool {
        let finished = collect_finished(&mut self.pending_mid, |rx| rx.try_recv());
        let mut any = false;
        for (i, res) in finished {
            self.pending_mid.remove(&i);
            match res {
                Some(res) => {
                    let (w, h) = (res.width, res.height);
                    if let Some(s) = self.slots.get_mut(i) {
                        s.mid_loading = false;
                        if let Ok(img) = render_image_from_bgra(res.bgra, w, h) {
                            // 替换旧中清：旧纹理走 pending_drops 显式下线
                            if let Some(old) = s.mid.replace(img) {
                                self.pending_drops.push(old);
                            }
                            self.mid_lru.retain(|&p| p != i);
                            self.mid_lru.push_back(i);
                        }
                        any = true;
                    }
                }
                None => {
                    if let Some(s) = self.slots.get_mut(i) {
                        s.mid_loading = false; // 失败，允许之后重试
                    }
                }
            }
        }
        any
    }

    // ---- 快速拖动判定 / 中清缓存命中 ----

    /// 迟滞比较：进入快速模式用 [`MOVE_THRESHOLD_PX`]，退出用其×
    /// [`MOVE_THRESHOLD_LOWER_RATIO`]（更小）；中间区间保持上一状态。
    /// 避免临界速度下 LOD 降级/恢复反复横跳造成调度抖动。
    pub(super) fn update_fast_flag(&mut self) {
        let enter = MOVE_THRESHOLD_PX;
        let exit = MOVE_THRESHOLD_PX * MOVE_THRESHOLD_LOWER_RATIO;
        self.prev_fast = if self.prev_fast {
            self.scroll_velocity > exit
        } else {
            self.scroll_velocity > enter
        };
    }

    /// P2：O(1) 主线程中清缓存命中（`slots[i].mid` 即内存缓存本体）。命中触摸 `mid_lru`
    /// （最近使用在队尾）并**复用纹理**——调用方据此跳过 Worker 请求；未命中返回 `None`
    /// → 走 Worker 渲染路径。仅主线程调用、只做哈希/克隆，不阻塞。
    pub(super) fn mid_cached(&mut self, i: usize) -> Option<Arc<RenderImage>> {
        let img = self.slots.get(i).and_then(|s| s.mid.clone());
        if img.is_some() {
            self.mid_lru.retain(|&p| p != i);
            self.mid_lru.push_back(i);
        }
        img
    }
}
