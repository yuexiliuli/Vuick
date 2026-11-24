//! PDF 阅读器子模块：每帧装配 / 调度驱动（`ensure_window` / `ensure_window_grid`）：
//! 回收在途结果、估速、静态帧门控、按视口调度瓦片 / 网格缩略、淘汰超预算缓存。

use super::*;

impl PdfViewer {
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
                self.state.loading,
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
        if self.sel_card.is_some()
            && self
                .sel_card
                .as_ref()
                .is_some_and(|s| s.scroll != self.scroll.offset())
        {
            self.sel_card = None;
            cx.notify();
        }
        // 非「鼠标缩放」滚动检测（评审 nit）：滚动条拖拽 / 键盘滚动等不经过 `on_scroll_wheel`
        // 的缩放拦截 → 这里每帧比对 offset 快照，变化且**不在缩放防抖窗**（`zoom_armed`，连续
        // Ctrl+滚轮每档都变 offset，但属缩放勿误清）→ 复位水平偏移 + 沟槽兜底锚（回中，防
        // 兜底锚到过期页点）。纯平移/缩放同帧不触发（zoom_armed 为真）。
        let cur = (
            f32::from(self.scroll.offset().x),
            f32::from(self.scroll.offset().y),
        );
        if cur != self.last_ui_scroll {
            self.last_ui_scroll = cur;
            if !self.zoom_armed {
                self.reset_x_zoom();
            }
        }
        // 分页页排（Facing）布局快照：以 zoom=1（fit）口径、**真实阅读视口**算
        // [`PagedLayout`] 并抄入，render 读它做分页定位（显示统一乘 `self.zoom`）。
        // 放在 worker/dims 守卫**之前**：无 worker 的测试夹具/纯态期也保持快照新鲜。
        // 连续阅读（`page_tops`/`compute_layout`）与网格（`GridLayout`）为各自布局，
        // 不建分页快照（渲染按 `page_mode` 走对应分支，读不到残留）。
        // **问题 B**：`paged_layout` 只依赖 (dims, 视口, cols)——滚动不变时结果逐字节相同，
        // 故对 `paged_layout_cell` 快照加 [`PagedFingerprint`] 指纹：仅在指纹变化时重建，
        // 纯滚动/静止帧复用上帧快照（不再每帧分配 5×O(rows) 向量）。
        if !self.grid_mode && self.page_mode != PageMode::Continuous && !self.dims.is_empty() {
            let (pvw, pvh) = self.paged_viewport(window);
            // Single 单页模式已删：分页态只剩 Facing（恒 2 列），死支移除。
            let cols = 2;
            let fp = PagedFingerprint::of(&self.dims, pvw, pvh, cols);
            if self.paged_fingerprint != Some(fp) {
                *self.paged_layout_cell.borrow_mut() = Some(paged_layout(
                    &self.dims,
                    pvw,
                    pvh,
                    cols,        // 列数 = 恒 2（Facing 双页）
                    PAGED_GAP_X, // 水平页间距：同行左页↔右页之间（1px）
                    PAGED_GAP_Y, // 垂直行间距：上页↔下页之间（6px）
                    PAGE_PAD_X,  // 四周外白（X/Y 取齐 = PAGE_PAD_X，防分页几何不对称）
                ));
                self.paged_fingerprint = Some(fp);
                self.paged_layout_rebuilds += 1;
            }
        } else {
            // 连续/网格/空文档：清掉残留分页快照与指纹（模式切换后渲染走连续/网格分支，
            // 不读它；指纹清空保证再进 Facing 时首帧重建而非误用旧几何）。
            *self.paged_layout_cell.borrow_mut() = None;
            self.paged_fingerprint = None;
        }
        if self.dims.is_empty() || self.worker.is_none() {
            return;
        }
        // ---- 网格模式：独立管线（缩略图调度/收集/行带淘汰），单列 keep_window/schedule/
        // evict/门控 全部跳过——网格用独立 `grid_scroll` 偏移，不干扰单列阅读状态。----
        if self.grid_mode {
            self.ensure_window_grid(window, cx);
            return;
        }
        // **整页入屏**（contain-fit）：页宽基线 = min(内容宽, 视口高 × 全文档最小纵横比
        // `doc_min_ar`)——与双页（Facing）模式同 fit 口径（Facing 行内 `fit_w =
        // viewport_h·min_ar`）→ 同一缩放值（状态栏 zoom%）下两模式页视觉大小一致（用户
        // 明选「连续也改为整页入屏」方向）。内容宽 = 可视宽 − 两侧页边距（容器首帧未布局
        // 宽为 0 → 回退窗口宽；侧边栏/分屏使可视区窄于窗口时按容器实宽，页列不相对可视区
        // 偏）；视口高读滚动容器真实高（首帧 0 回退窗口高，同 [`Self::paged_viewport`] 兜底
        // 口径）。窗口/侧边栏拉宽拉窄 → 基线变 → 重排页列布局（页面随窗口缩放），瓦片按
        // 新密度由 `schedule_tiles` 惰性重渲；Ctrl+滚轮在基线上再乘 `zoom`（`adjust_zoom`
        // 重排）。
        let container_w = f32::from(self.scroll.bounds().size.width);
        let view_w = if container_w > 0.0 {
            container_w
        } else {
            window.bounds().size.width.as_f32()
        };
        let container_h = f32::from(self.scroll.bounds().size.height);
        let view_h = if container_h > 0.0 {
            container_h
        } else {
            window.bounds().size.height.as_f32()
        };
        let content_w = (view_w - 2.0 * PAGE_PAD_X).max(1.0);
        let new_base = content_w.min(view_h * self.doc_min_ar).max(1.0);
        if (new_base - self.base_w).abs() > 1e-4 {
            self.base_w = new_base;
            let pw = self.page_width();
            let (tops, total) = compute_layout(&self.dims, pw);
            self.page_tops = tops;
            self.total_height = total;
            if timing_view() {
                eprintln!(
                    "[{:.1}ms] [VIEW-REFIT] base_w={:.0} page_width={:.0} view_w={:.0} view_h={:.0} min_ar={:.4} zoom={:.2}",
                    crate::pdf::timing_ms(),
                    self.base_w,
                    pw,
                    view_w,
                    view_h,
                    self.doc_min_ar,
                    self.zoom,
                );
            }
            cx.notify();
        }
        // resize 重夹（评审 nit）：视口变化使非溢出偏移上界（居中边距）变 → 超界重夹，保持
        // 渲染/命中「页在视口内」口径（渲染 margin 已 `max(0)` 兜底防负边距，此夹归位坐标）。
        // 布局本帧后容器宽已就位（首帧宽 0 时 `nonoverflow_max_xoff` 回退同口径）；缩放序列内
        // offset 由 `zoom_target_impl` 以同界夹过，此处为 resize 兜底。
        let xoff_bound = self.nonoverflow_max_xoff(self.page_width());
        if self.x_zoom_offset.abs() > xoff_bound {
            self.x_zoom_offset = self.x_zoom_offset.clamp(-xoff_bound, xoff_bound);
        }
        // 阅读进度恢复**待落地滚动**：base_w 已按真实可视宽 refit、`page_tops` 为本帧
        // 布局值——按保存的 `(页, 页内比例)` 逆公式滚回（分母用**显示高**，与 `reading_progress()`
        // 保存侧互逆）。首帧 `scroll.bounds()` 宽为 0 → 容器宽回退窗口宽（base_w 非真实
        // 可视宽）→ 此时**不施加**（保持待落地），下一帧容器宽就位后再按真实布局滚。
        if container_w > 0.0
            && let Some((page, frac)) = self.restore_pending.take()
        {
            // 分页分支：`(page, frac)` → 行 `r = page/cols`、显示口径 `sy = 行顶×zoom +
            // frac·**该页自身高 cell_h[page]**×zoom`（与 `reading_progress()` 分页保存侧互逆，
            // 页内 frac 同分母）；并把 sticky 列钉到 `page % cols`——连续→Facing 时连续侧
            // `page` 已含「读者实际在读的页（含右/奇页）」，其列即 sticky 记忆，此后再切回
            // 连续按同列解析零漂移。受 `paged_max_scroll` 夹。
            let paged_applied = if self.paged_mode_active()
                && let Some(layout) = self.paged_layout_cell.borrow().as_ref()
                && layout.cols > 0
            {
                let zoom = self.zoom.max(0.01);
                let k = page.min(self.dims.len().saturating_sub(1));
                // K177：`r = page/cols`（向下取整 = 验证行号），`col = page % cols` 即 sticky。
                let r = (k / layout.cols).min(layout.rows.saturating_sub(1));
                if layout.cols > 0 {
                    self.facing_anchor_col = k % layout.cols;
                }
                let row_top = layout.row_top[r] * zoom;
                // 页内 frac 分母用该页自身高（行内两页不等高时，行高会把 frac 失真放大；
                // 用 cell_h[page] 与 reading_progress 分页分支同分母，往返精确）。
                let cell_h = layout
                    .cell_h
                    .get(k)
                    .copied()
                    .unwrap_or_else(|| layout.row_height(r))
                    * zoom;
                let sy =
                    (row_top + frac.clamp(0.0, 1.0) * cell_h).clamp(0.0, self.paged_max_scroll());
                self.scroll.set_offset(gpui::point(px(0.), px(-sy)));
                self.last_scroll_y = sy;
                true
            } else {
                false
            };
            if !paged_applied {
                let k = page.min(self.dims.len().saturating_sub(1));
                let disp_h = display_height(self.dims[k], self.page_width()).max(1.0);
                let top = self.page_tops.get(k).copied().unwrap_or(0.0);
                let sy = (top + frac.clamp(0.0, 1.0) * disp_h).min(self.total_height.max(0.0));
                self.scroll.set_offset(gpui::point(px(0.), px(-sy)));
            }
        }
        let scale_factor = window.scale_factor();
        let page_width = self.page_width();
        let scroll_y = f32::from(-self.scroll.offset().y);

        // 内存水位巡检（仅计时模式）：每 ~30 帧打一行构成，供定位内存大头
        if timing_view() {
            static MEM_TICK: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            if MEM_TICK
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                .is_multiple_of(30)
            {
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
        // A 线：分页页拍（Facing）调度窗改走**分页几何**（`paged_schedule_window`：
        // 可见行页区间 ±PAGED_WINDOW_ROWS 行）——连续 `keep_window` 按连续页高 ±MARGIN 选页，
        // 竖排页 + 宽视口下分页行高 ≈ 视口 < 连续页高，可见页会漂出 ±3 连续窗 → 显示页
        // L0/L0.5/走廊/封面全部不被请求、长驻「加载中」（主因 1/2）。非分页/无快照回落连续。
        let (start, end) = self
            .paged_schedule_window(scroll_y, viewport_h)
            .unwrap_or_else(|| {
                keep_window(
                    &self.dims,
                    &self.page_tops,
                    page_width,
                    scroll_y,
                    viewport_h,
                )
            });

        // 2.4) **动态瓦片预算 + 钉住集 + R4 均匀降级状态机**（每帧，含静态帧门控早退帧——
        // 门控帧也须推进降级计数/保持钉住状态一致，跳过会让「静态 N 帧」解码停在 0）。
        // 计算本帧预算/pinned（`evict_tiles` 消费）与降级激活（远距离页放不下 → 均匀 0.5 底）；
        // 返回本帧因降级退休的 cover/清出的瓦片（入 `pending_drops` 下帧 drop_image）。
        let degrade_drops = self.update_tile_budget_and_degrade(
            start,
            end,
            scroll_y,
            viewport_h,
            page_width,
            scale_factor,
            window,
        );
        self.pending_drops.extend(degrade_drops);

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
            // A 线：走廊中心分页下改走分页可见中心（paged_vis_center），否则竖排页深页走
            // 廊中心错、给「错的页」派邻居封面白耗栅格预算（主因 1 的走廊侧）。
            let vis_first = self
                .paged_vis_center(scroll_y, viewport_h)
                .unwrap_or_else(|| {
                    first_visible_page(&self.dims, &self.page_tops, page_width, scroll_y)
                });
            self.maybe_prefetch_corridor(vis_first, page_width, scale_factor);
            self.ensure_tile_pump(cx);
            // 不更新 `window_sig`/`window_settled`：保持同一静置签名与确认状态。
            // --- 真机取证探针（纯只读，不改调度语义，见 view.rs `diag_enabled`）---
            // 用户真机症状：缩放后在固定比例有时整页雾糊（低密度底图在服务），静止不
            // 清晰、只能靠再次缩放恢复；headless 单 worker 时序与真机有别无法复现。
            // 本探针在「糊住不动」的静态帧门控早退前（可见调度窗：连续 = keep_window ±MARGIN、
            // 分页 = paged_schedule_window 窗）逐可见页打印 LOD 账：各档底图
            // （cover/prev/pref/mid/thumb/tiles）在位情况 + 是否有当前档 cover 在途 + 期望
            // cover 档，供锁定是哪一档低密底图在服务、为何无当前档 cover 补渲。VUICK_DIAG
            // 未设时零开销。（A 线：复用已算 `start/end`，不再重复 keep_window 重算。）
            if diag_enabled() {
                let (dstart, dend) = (start, end);
                if !self.dims.is_empty() && dstart <= dend {
                    for p in dstart..=dend {
                        if let Some(slot) = self.slots.get(p) {
                            let cover = slot
                                .cover
                                .as_ref()
                                .map(|_| format!("{:.2}", slot.cover_scale))
                                .unwrap_or_else(|| "none".to_owned());
                            let prev = if slot.prev_cover.is_some() {
                                "some"
                            } else {
                                "none"
                            };
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
                            let want = cover_scale(
                                self.dims[p],
                                self.page_card_display_w(p),
                                scale_factor,
                            );
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

        // `start`/`end` 已在 2.4 处计算（keep_window 同源），此处直接沿用。
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
                    && let Some(arc) = slot.thumb.take()
                {
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
                        start,
                        end,
                        scroll_y,
                        viewport_h,
                        page_width,
                        scale_factor,
                        window,
                    );
                } else {
                    self.schedule_tiles(window, start, end, page_width, scale_factor, scroll_y);
                }
            }
            SpeedState::Fast => {
                if self.zoom_armed {
                    // 缩放 + 中速拖拽复合：满密封面同样逐档抢派 → preview 顶住。
                    self.maybe_dispatch_zoom_preview(
                        start,
                        end,
                        scroll_y,
                        viewport_h,
                        page_width,
                        scale_factor,
                        window,
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
                    self.maybe_dispatch_jump_covers(page_width, scale_factor, scroll_y, viewport_h);
                } else if self.zoom_armed {
                    self.maybe_dispatch_zoom_preview(
                        start,
                        end,
                        scroll_y,
                        viewport_h,
                        page_width,
                        scale_factor,
                        window,
                    );
                }
            }
        }
        // F4/B：非 idle 帧（窗口有移动/调度运行）按当前可见中心收回走廊窗外的旧廊封面
        // （距离 LRU + 字节预算）。idle 静置期的挂在 idle 分支的 `maybe_prefetch_corridor` 内。
        // A 线：中心页分页下走分页可见中心（与 idle 分支同口径），防深页「给错的页」派廊封面。
        let vis_first = self
            .paged_vis_center(scroll_y, viewport_h)
            .unwrap_or_else(|| {
                first_visible_page(&self.dims, &self.page_tops, page_width, scroll_y)
            });
        self.evict_prefetch_covers(vis_first);
        self.evict_tiles(start, end, scale_factor);
        // S5 高倍 cover 回收（高倍稳态 1:1 瓦片接管后，cap 截断的 4.0 密度 cover 只是
        // 32MB/页 双驻留 + 边隙透雾的旧密度整页图 → 回收；评审 BLOCK③ 后按可见矩形对账，
        // 未铺满可见区不回收）。
        self.retire_high_scale_covers(
            start,
            end,
            scroll_y,
            viewport_h,
            page_width,
            scale_factor,
            window,
        );
        // F4/B：清尾帧（非快速 + 四主队列全空）也尝试派发走廊——「最后一帧工作清空后泵
        // 须续出几帧把走廊填掉」，否则没有任何 idle 帧会到来（泵在清尾帧即停）。
        self.maybe_prefetch_corridor(vis_first, page_width, scale_factor);

        // 更新门控状态（本轮已真实跑过调度）：签名对齐当前帧；settled = 非快速态
        // 且调度后无在途瓦片/封面（即确认「已无待派的新 tile 请求」→ 下帧可门）。
        // 快速态（占位调度）或仍有在途 → settled=false，绝不允许静态帧跳过调度。
        self.window_sig = Some(sig);
        self.window_settled =
            !self.prev_fast && self.pending_tiles.is_empty() && self.pending_cover.is_empty();

        // 7) 瓦片在途自刷新：若无用户输入就没有下一次重绘，而瓦片结果要等下一次
        //    `ensure_window` 才收集——启动 ~16ms 泵直到在途瓦片清空（静置也能出图）。
        self.ensure_tile_pump(cx);
    }

    /// 网格模式的每帧管线（`ensure_window` 的 `grid_mode` 分支）：**滚动风暴检测（首行）→
    /// 网格缩略图在途收集 → 可视化带派发（低清/全清双档，限各档每帧预算）→ 行带外回收**。
    /// 静态帧门控 / keep_window / 瓦片调度概不参与——网格自带独立滚动偏移与独立缩略图槽。
    pub(super) fn ensure_window_grid(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // 滚动风暴检测（首行）：相邻帧滚动偏移差 ≥ GRID_STORM_CHANGE_PX → 风暴态（快滚期
        // 低清派发 + 占位弱化）；连续 GRID_STORM_IDLE_FRAMES 帧几乎静止 → 退出（idle 升级
        // 接管）。风暴标志本帧即被 schedule_grid_thumbs / build_grid_cards 同帧读取。
        let off = f32::from(-self.grid_scroll.offset().y);
        if (off - self.grid_last_offset).abs() >= GRID_STORM_CHANGE_PX {
            self.grid_storm = true;
            self.grid_storm_idle_frames = 0;
        } else {
            self.grid_storm_idle_frames += 1;
            if self.grid_storm_idle_frames >= GRID_STORM_IDLE_FRAMES {
                self.grid_storm = false;
            }
        }
        // 滚动方向（本批「方向感知预取」）：偏移显著增大 = 向下滚（dir=+1，超前补下方大行号
        // 页）、显著减小 = 向上滚（dir=−1）；静止微移保持上一方向（防抖动翻转）。
        let delta = off - self.grid_last_offset;
        if delta >= GRID_STORM_CHANGE_PX {
            self.grid_scroll_dir = 1;
        } else if delta <= -GRID_STORM_CHANGE_PX {
            self.grid_scroll_dir = -1;
        }
        self.grid_last_offset = off;
        // 带宽预熟**滚动重定中心推送（节流）**（本线）：每帧按当前视口计算 `(中心页, 近/中带
        // 区间, 全清桶)`，与上次推送比较——只有**跨行/跨桶**（几何真正变化）才 `set_grid_prewarm`
        // 重新定心（同一窗口内静止/微移幂等跳过）。滚动把中心重定 → worker 从新中心重扫
        // 近/中带（旧窗口已缓存页快速跳过、只渲新入带页）；风暴期不逐帧重启（成本只限新入
        // 带页，且 gate 后的 5.5 步本就在 view 派发排空后跑）。`push_grid_prewarm` 内部按
        // `grid_prewarm_pushed` 比较节流，故每帧调用零赘余。
        self.push_grid_prewarm(Some(window));
        // 泵保真：风暴期低清请求排空后渲染不再变化、无事件触发下一次重绘 → 强制 notify
        // 保泵，防「低清排空后缺帧升级全清」的停滞（对齐瓦片自刷新泵的静态帧驱动姿势）。
        if self.grid_storm {
            cx.notify();
        }
        if self.collect_grid_results() {
            cx.notify();
        }
        self.schedule_grid_thumbs(window);
        self.evict_grid_thumbs(window);
        self.ensure_tile_pump(cx);
    }
}
