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

/// W2：封面**连续失败**达此阈值即拉黑该页（加入 `cover_blacklist`，回退瓦片供给、
/// 停止盲目重试封面）。取 3：首载 1 次失败可容忍（pdfium 偶发 OOM），连续 3 次基本
/// 判定「封面机不可达」，再等只会让该页永久停在占位/模糊（评审 t3 发现）。
const COVER_FAIL_BLACKLIST_THRESHOLD: u32 = 3;

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
        // S3 缩放防抖：`zoom_armed` 期间持续出帧——否则缩放事件后的渲染帧只有一次
        // （preview 收完泵就停、无输入就无新帧），`zoom_idle_frames` 永远到不了
        // [`config::pdf::ZOOM_DEBOUNCE_FRAMES`]，防抖永不收敛收口（雾感/空白无从消灭）。
        if self.zoom_armed {
            return true;
        }
        if self.prev_fast {
            return true;
        }
        if !self.pending_tiles.is_empty() || !self.pending_cover.is_empty() {
            return true;
        }
        // F4/B：中清/缩略在途也要续帧收集——否则 Fast 带派发的中清/缩略在换挡后
        // `pump_still_needed` 停泵、结果滞留无人收集（静置 idle 门/预取走廊要求四队列
        // 全空才能触发，滞留会让走廊永远等不到空闲帧）。中清/缩略渲染 ~1-5ms，多出几帧即清。
        if !self.pending_mid.is_empty() || !self.pending_thumb.is_empty() {
            return true;
        }
        // F4 走廊封面在途：继续出帧收集空闲预取结果（idle 分支派发后自启动泵的延续）。
        if !self.pending_prefetch_cover.is_empty() {
            return true;
        }
        // 重着色刷新在途：继续出帧直到 worker 回传的新着色副本 drain 完（否则晚到的
        // 回传没有下一帧可收集，重着色画面停在旧着色上）。
        if self.pending_recolor.is_some() {
            return true;
        }
        if !self.state.loading && !self.dims.is_empty() && self.state.load_error.is_none() {
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
        // **F2 封面单向预取（收窄驻留窗）**：封面请求与驻留窗收窄为「可见页 + 滚动方向
        // 前瞻 1 页」。方向未知（初始未滚动，`scroll_dir==0`）→ 退化为旧保守对称窗
        // （可见页 ±1），首开/缩放帧不缩水。
        //
        // **回看/反向清晰度（评审 D1 修正，注意非「瓦片全程兜底」）**：封面可负担页
        // （典型 A4）因 T1 跳瓦片从不产生瓦片，封面到位又把 mid/thumb 回收 → 反向微调
        // 滚动（overshoot 一档再回）时，方向翻位刚退位的那一页若直接丢封面就「无底图闪
        // 加载中」。故退位页封面**转存为有界 `prev_cover` 桥底图**（`evict_covers` 的
        // `retire` 分支，同一时刻至多 1 张），反向重读该页期间底图链 cover→prev_cover→
        // mid→thumb 恒非空。仅地缘上相邻的退位页有桥；更远窗外页（≥2 页）回翻重渲期间
        // 仍可能瞬时落「加载中」（既有行为，不在本兜底范围）。
        let (cover_first, cover_last) = match self.scroll_dir {
            1 => (vis_first, (vis_last + 1).min(self.dims.len().saturating_sub(1))),
            -1 => (vis_first.saturating_sub(1), vis_last),
            _ => (first, last),
        };
        // 方向翻位时的「退位页」= 反方向一侧刚出窗的那一页（其封面转存有界 prev_cover）。
        // 对称退化窗（dir=0）两侧邻居都在窗内 → 无退位页。
        // ⚠️ 必须用 checked_sub/add（惰性求值）：走值语义的 `(p>0).then_some(p-1)` 会**先**算
        // `p-1`，p==0 时 debug 下直接算术下溢 panic（首帧滚动到页 0 即触发）。
        let retire = match self.scroll_dir {
            1 => cover_first.checked_sub(1),
            -1 => cover_last.checked_add(1).filter(|&p| p < self.dims.len()),
            _ => None,
        };
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
            self.settle_tile_burst = false;
        } else {
            // 靠近视口的瓦片先渲染（正在看的那片先填）
            need.sort_by_key(|&(_, _, _, d)| d);
            let worker = self.worker.clone().unwrap();
            // #5 收口一次性出清：防抖到期收口帧（cur>4 cap 截断、纯瓦片供给页）把单一
            // [`TILE_INFLIGHT`]=6 临时抬到 [`config::pdf::SETTLE_TILE_BURST_MAX`]，可见瓦片
            // 一次入队 → 高倍收口不再 3-6 帧逐片雾窗。只抬**在途**不增驻留（TILE_CACHE_BYTES
            // 不变）；普通滚动恒 false（语义不变）。`settle_burst` 每帧即清：只在收口当帧突发，
            // 尾片回落常态节流。`settle_tile_burst_dispatch` 取**最近一次突发帧**的净超量
            // （每突发帧前置零再累加；headless 断言「出清预算到位」的确定性证据，不依赖 worker）。
            let (budget, burst) = if self.settle_tile_burst {
                let b = config::pdf::SETTLE_TILE_BURST_MAX
                    .saturating_sub(self.pending_tiles.len());
                self.settle_tile_burst_dispatch = 0;
                (b, b.saturating_sub(TILE_INFLIGHT))
            } else {
                (TILE_INFLIGHT.saturating_sub(self.pending_tiles.len()), 0)
            };
            self.settle_tile_burst = false;
            if burst > 0 {
                self.settle_tile_burst_dispatch += burst as u64;
            }
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
                    self.nav_gen, // L2.2：发送时打戳当前导航世代
                    self.zoom_epoch, // Z2：发送时打戳当前缩放世代
                );
                self.raster_reqs += 1; // 切档不重栅回归计数
                self.tile_reqs += 1; // T1 首屏「封面 vs 瓦片」构成计数
                self.pending_tiles.insert((page, row, col), (scale, self.nav_gen, rx));
            }
        }

        // 封面（**后派**，`schedule_covers` 内含 evict）：**只服务封面窗 [cover_first..=cover_last]**
        // （可见页 + 滚动方向前瞻 1 页；非 MARGIN 全窗——否则候选页数超驻留上限 → 换页风暴）。
        // 请求窗与 `evict_covers` 驻留窗一致，避免「派了即逐」。中心页优先出队。**T1**：封面与
        // 瓦片同密度的页因规划跳过已无瓦片，封面即是该页唯一供给 → 无同页双渲。
        // **F4/A 兜底（评审 F1 修正：不可与封面同帧双渲）**：封面**派完之后**再给「可见且
        // 封面机不可负担/封面在途预算卡满未能派发」的页补一枚 L0 缩略——worker 每轮先
        // render_q 后 thumb_q（parser.rs），同帧「缩略+封面」时封面先渲、缩略非但没先填窗反成
        // 同页双渲浪费。故缩略**只服务「封面真的不会马上来」的页**：本帧已被 `schedule_covers`
        // 派封面（或更早在途）的页由 `pending_cover` 守卫跳过；封面可负担且预算有空 → 封面
        // 必然马上派到，缩略即是双渲冗余，同样跳过。缩略实际兜底的正是「封面不可负担
        // （病理性页）或封面在途预算卡满」这两类空窗。封面到位时 `collect_cover_results`
        // 顺带下线缩略（F2 既有语义）。只服务真正可见页（不铺 MARGIN 缓冲页，避免缩略驻留
        // 超 `THUMB_CACHE_PAGES`）。
        let center_vis = (vis_first + vis_last) / 2;
        let wrap_w = self.wrap_width(window, page_width);
        self.schedule_covers(
            cover_first,
            cover_last,
            center_vis,
            retire,
            scroll_y,
            viewport_h,
            wrap_w,
            page_width,
            scale_factor,
        );
        self.schedule_thumb_backdrops(vis_first, vis_last, page_width, scale_factor);
    }

    /// F4/A：常态 `schedule_tiles` 主路径的**缩略待底图补发**（评审 F1 修正后语义）——在
    /// `schedule_covers` **之后**调用，只对「封面真的不会马上来」的**可见**页派一枚 L0 缩略
    /// （~1-5ms/页）：封面不可负担（病理性超预算页）或封面在途已占满 [`COVER_INFLIGHT`]、
    /// 本帧未给该页派封面（也非更早在途）→ 页卡有缩略先显（底图链 cover→prev_cover→廊封面→
    /// mid→thumb，缩略即最后兜底），`collect_cover_results` 在封面最终到位时顺带下线缩略。
    /// **杜绝同页 cover+thumb 双渲（T1 纪律）**：本帧已被派封面（`pending_cover` 含该页）的页
    /// 直接跳过；封面可负担且封面在途预算未满 → 封面必然马上到，缩略即双渲冗余也跳过。
    /// `MAX_THUMB_INFLIGHT` 独立额度。只服务可见页（不铺 MARGIN 缓冲页）。
    pub(super) fn schedule_thumb_backdrops(
        &mut self,
        vis_first: usize,
        vis_last: usize,
        page_width: f32,
        scale_factor: f32,
    ) {
        if self.pending_thumb.len() >= MAX_THUMB_INFLIGHT {
            return;
        }
        let Some(worker) = self.worker.clone() else {
            return;
        };
        let mut budget = MAX_THUMB_INFLIGHT - self.pending_thumb.len();
        let cover_inflight_full = self.pending_cover.len() >= COVER_INFLIGHT;
        for page in vis_first..=vis_last {
            if budget == 0 {
                break;
            }
            // ① 封面本帧已派 / 更早在途 → 缩略跳过（不双渲）。排在 schedule_covers 之后，
            //    `pending_cover` 已含本帧所有封面派发，正是「同帧 cover+thumb 双渲」的守门。
            if self.pending_cover.contains_key(&page) {
                continue;
            }
            let dim = self.dims[page];
            let cscale = cover_scale(dim, page_width, scale_factor);
            // ② 封面可负担且封面在途预算未满 → 封面必然马上派到（下帧/本帧即补），缩略是冗余。
            //    只有「封面不可负担」或「COVER_INFLIGHT 满、本帧派不到它」才落到缩略兜底。
            if cover_affordable(dim, cscale) && !cover_inflight_full {
                continue;
            }
            let slot = &self.slots[page];
            let has_base = slot.cover.is_some()
                || slot.mid.is_some()
                || slot.thumb.is_some()
                || slot.prev_cover.is_some()
                || slot.prefetch_cover.is_some();
            if has_base || slot.thumb_loading || slot.mid_loading {
                continue;
            }
            self.slots[page].thumb_loading = true;
            let rx = worker.request_thumb(self.doc_id, page, self.nav_gen, self.zoom_epoch); // L2.2 enqueue-n_gen
            self.raster_reqs += 1; // 切档不重栅回归计数
            self.pending_thumb.insert(page, (self.nav_gen, rx));
            budget -= 1;
        }
    }

    // ---- F4 空闲预取走廊（WPS「好像全部已加载」的来源）----

    /// F4/B 守卫化走廊预取入口：仅当「四个主在途队列全空」（真正的空闲/清尾帧）才派发走廊
    /// 封面。Idle/Slow 清尾帧与 idle 静置帧共用本入口——正是「最后一帧工作清空后泵仍须续出
    /// 几帧把走廊填掉」的关键：非 idle 路径在队尾调用本方法，派发在途即让 `pump_still_needed`
    /// 保持 true，泵继续驱动 idle 帧直到走廊填满/预算。
    /// **L3.1 走廊重武装**：不再直接因 `prev_fast` 拒派——高速平移（VeryFast→Fast 交界、各主
    /// 队列恰好排空的帧）也能补 0.5 密度廊封面，让新进视口页在高速期就有低清底图（WPS
    /// 「好像全部已加载」的观感在高速带也成立）；只要任一主队列有在途即仍拒派（不抢栅格化
    /// 带宽）。速度越高窗口越大的收缩由 [`prefetch_corridor`] 的 M(v) 与 [`evict_prefetch_covers`]
    /// 的 40MB 预算兜底，不会因放开 `prev_fast` 而失控。
    pub(super) fn maybe_prefetch_corridor(
        &mut self,
        vis_first: usize,
        page_width: f32,
        scale_factor: f32,
    ) {
        if !self.pending_tiles.is_empty()
            || !self.pending_cover.is_empty()
            || !self.pending_mid.is_empty()
            || !self.pending_thumb.is_empty()
        {
            return;
        }
        self.prefetch_corridor(vis_first, page_width, scale_factor);
    }

    /// L3.2：**走廊窗口随速度扩大**——M(v)=clamp(round(v/平均页高)+1, [`PREFETCH_COVER_MARGIN`],
    /// [`PREFETCH_COVER_MARGIN_MAX`])，滑窗 [vis±M(v)]。高速平移（VeryFast，单帧穿越多页）期
    /// 走廊不可能只用基线 [vis±2] 盖住「一次平移 4+ 页」，按速度把滑窗拉大；`v` 由
    /// [`Self::scroll_velocity`]（px/帧 EMA）给出，「平均页高」用 `total_height/页数` 近似
    /// （含间隙/边距，只作速度→页数的除法量纲）。v=0（idle）→ M 回落到基线 2，既有 idle
    /// 走廊语义/回归（`idle_prefetch_corridor` 的 [vis±2] 断言）不变。预算仍由
    /// [`PREFETCH_COVER_BUDGET_BYTES`]=40MB LRU + [`PRE_FETCH_COVER_INFLIGHT`]=2/帧封顶——
    /// 扩张只影响「先派哪个/留哪个」，不改变总占用。
    pub(super) fn corridor_margin(&self) -> usize {
        let n = self.dims.len();
        let avg_page_h = if n > 0 && self.total_height > 0.0 {
            self.total_height / n as f32
        } else {
            PAGE_WIDTH // 兜底：未知布局时用页宽近似
        };
        let v = self.scroll_velocity;
        let m = ((v / avg_page_h).round() as isize + 1)
            .max(config::pdf::PREFETCH_COVER_MARGIN as isize)
            .min(config::pdf::PREFETCH_COVER_MARGIN_MAX as isize) as usize;
        m
    }

    /// F4/B：**空闲预取走廊**——在 `ensure_window` 静置早退**之前**为走廊 [vis±2] 的空闲页
    /// 派发 0.5 密度整页封面（每帧 ≤ [`PRE_FETCH_COVER_INFLIGHT`]，跨帧预算）。idle 时四个
    /// 在途队列本就空，worker 队列无竞争 → 「空闲期把窗口两侧的底图消化掉」，高速平移
    /// （VeryFast）切入时新页大概率已有可显示底图（WPS「好像全部已加载」的来源）。
    /// worker 只有空闲期消费（`request_render_preview` 出图后 view 收进 `slot.prefetch_cover`），
    /// 优先/几何同封面、但按 Mid 语义不写 raw 缓存（廊封面是瞬态底图，不挤占重着色刷新）。
    ///
    /// 触发前提（调用方 `open::ensure_window` 的 idle 门）全为真：非 fast、四在途队列空、
    /// `window_settled` 且签名一致。本函数只管「选中走廊页 + 跨帧限流 + 回收旧廊封面」。
    pub(super) fn prefetch_corridor(
        &mut self,
        vis_center: usize,
        page_width: f32,
        scale_factor: f32,
    ) {
        // ① 先按当前可见中心收回走廊外的旧廊封面（窗口移动 → 距离/预算 LRU）。
        self.evict_prefetch_covers(vis_center);
        // ② 跨帧预算派发：只派「无满封面/无廊封面/无在途、可负担、在走廊内」的页。
        if self.pending_prefetch_cover.len() >= config::pdf::PRE_FETCH_COVER_INFLIGHT {
            return;
        }
        let n = self.dims.len();
        if n == 0 {
            return;
        }
        // L3.2：窗口随速度扩大（`corridor_margin`），滑窗 [vis±M(v)]（idle v≈0 → M=2 基线）。
        let margin = self.corridor_margin();
        let lo = vis_center.saturating_sub(margin);
        let hi = (vis_center + margin).min(n - 1);
        let mut order: Vec<usize> = (lo..=hi).collect();
        order.sort_by_key(|&p| (p as i64 - vis_center as i64).unsigned_abs());
        let Some(worker) = self.worker.clone() else {
            return;
        };
        let mut budget = config::pdf::PRE_FETCH_COVER_INFLIGHT
            .saturating_sub(self.pending_prefetch_cover.len());
        for page in order {
            if budget == 0 {
                break;
            }
            let slot = &self.slots[page];
            // 已有满密度封面/廊封面/任一在途 → 无需再派（不重复派发）。
            if slot.cover.is_some()
                || slot.prefetch_cover.is_some()
                || self.pending_prefetch_cover.contains_key(&page)
                || self.pending_cover.contains_key(&page)
            {
                continue;
            }
            let dim = self.dims[page];
            // **P1 ④（同页去重）**：当前密度 1:1 瓦片已在位（近期可见、预算未逐的残留片）→
            // 该页已有 ≥0.5 密度供给，0.5 preview 是与瓦片双驻留的纯浪费，跳过。
            // 仅「真无底」新页保留 preview（防首帧空白）。旧密度瓦片（缩放中途 tile_scale
            // 不匹配）不拦——它们马上会被惰性作废，preview 仍是该页过渡底图。
            if !slot.tiles.is_empty()
                && (slot.tile_scale - render_scale(page_width, scale_factor, dim.0)).abs() <= 1e-4
            {
                continue;
            }
            let cscale = cover_scale(dim, page_width, scale_factor);
            // 0.5 密度：像素 1/4、~5-12ms/页；可负担才派（病理性页跳过，交给常态路径）。
            let pscale = (cscale * config::pdf::PREFETCH_COVER_DENSITY)
                .min(config::pdf::COVER_SCALE_CAP);
            if !cover_affordable(dim, pscale) {
                continue;
            }
            // 走 `request_render_preview`（RenderPreview 消息）：与封面同优先/同几何，但按
            // Mid 语义**不入 raw 缓存、不参与重着色刷新**——廊封面是瞬态低清底图，raw 入缓存
            // 只会挤出可见封面/瓦片原图（切档刷新无消费方，白占 RAW_CACHE_BYTES 预算）。
            let rx = worker.request_render_preview(
                self.doc_id,
                RenderRequest { page, scale: pscale },
                self.nav_gen, // L2.2 enqueue-n_gen
                self.zoom_epoch, // Z2：发送时打戳当前缩放世代
            );
            self.raster_reqs += 1; // 切档不重栅回归计数
            self.prefetch_reqs += 1; // F4 观测计数（断言 idle 预取确实发生）
            self.pending_prefetch_cover.insert(page, (pscale, self.nav_gen, rx));
            budget -= 1;
        }
        // 派发在途后由调用方（idle 分支）`ensure_tile_pump` 启动收图泵。
    }

    /// F4/B：走廊封面回收（距视口中心距离 LRU + 字节预算 [`PREFETCH_COVER_BUDGET_BYTES`]）。
    /// 任一非 idle 帧（窗口移动）与 idle 期派发前都调用：只作用于 `slot.prefetch_cover`
    /// （独立于窗口 `slot.cover`，不参与封面驻留计数）。
    /// ① 走廊窗 [vis±M(v)]（L3.2 随速度扩大，idle 回落基线 2）外的全部收回（最远优先）；
    /// ② 窗内超字节预算再逐最远页。**L3.3 方向感知逐出**：排序键 `dist − dir_bonus`，bonus 只给
    /// 「当前滚动方向回退侧」的近期过页——保退位页廊封面驻留（滚回瞬间有底图，不落「加载中」）；
    /// 只影响同距离内的竞争顺序，不改变任何预算/驻留计数。
    /// 逐出的纹理走 `pending_drops` 显式 `drop_image` 下线（防显存/提交内存缓慢累积）。
    pub(super) fn evict_prefetch_covers(&mut self, vis_center: usize) {
        let n = self.dims.len();
        if n == 0 {
            return;
        }
        // L3.2：窗口随速度扩大（`corridor_margin`），滑窗 [vis±M(v)]（idle v≈0 → M=2 基线）。
        let margin = self.corridor_margin();
        let lo = vis_center.saturating_sub(margin);
        let hi = (vis_center + margin).min(n - 1);
        let dist = |p: usize| (p as i64 - vis_center as i64).unsigned_abs();
        // L3.3：回退侧加成——仅给「当前滚动方向回退侧、且在走廊窗内」的页（正滚向下时回退侧
        // 在视口上方 p<vis_center；反滚时在下方 p>vis_center；方向未知/静止（dir=0）不加成）。
        let dir = self.scroll_dir;
        let bonus = |p: usize| {
            if (dir == 1 && p < vis_center) || (dir == -1 && p > vis_center) {
                config::pdf::PREFETCH_RETURN_SIDE_BONUS as u64
            } else {
                0u64
            }
        };
        let est = |dim: (f32, f32), s: f32| -> u64 {
            let (w, h) = page_bitmap_size(dim, s);
            (w as u64) * (h as u64) * 4
        };
        let mut drops: Vec<Arc<RenderImage>> = Vec::new();
        let mut in_window: Vec<usize> = Vec::new();
        for (p, slot) in self.slots.iter_mut().enumerate() {
            if slot.prefetch_cover.is_none() {
                continue;
            }
            if p < lo || p > hi {
                // ① 走廊窗外 → 收回
                if let Some(img) = slot.prefetch_cover.take() {
                    drops.push(img);
                }
            } else {
                in_window.push(p);
            }
        }
        // ② 窗内超字节预算 → 逐最远页
        let total: u64 = in_window
            .iter()
            .map(|&p| est(self.dims[p], self.slots[p].prefetch_cover_scale))
            .sum();
        if total > config::pdf::PREFETCH_COVER_BUDGET_BYTES {
            // L3.3：排序键 = dist − dir_bonus（回退侧同距离页排名靠后 = 后逐、多驻留）。
            in_window.sort_by(|&a, &b| {
                let ka = dist(a).saturating_sub(bonus(a));
                let kb = dist(b).saturating_sub(bonus(b));
                kb.cmp(&ka) // 键越大（距中心越远/无回退加成）优先逐
            });
            let mut cur = total;
            for p in in_window {
                if cur <= config::pdf::PREFETCH_COVER_BUDGET_BYTES {
                    break;
                }
                if let Some(slot) = self.slots.get_mut(p) {
                    let b = est(self.dims[p], slot.prefetch_cover_scale);
                    if let Some(img) = slot.prefetch_cover.take() {
                        drops.push(img);
                        cur = cur.saturating_sub(b);
                    }
                }
            }
        }
        self.pending_drops.extend(drops);
    }

    /// F4/B：回收在途**走廊封面**（0.5 密度整页）：worker 回传 → 装入 `slot.prefetch_cover`
    /// （`prefetch_cover_scale` 记派发时的 0.5× 密度，render 按当前页宽 CSS 拉伸铺满）。
    /// 若该页此刻已有满密度封面临位/在途（窗口已移到它身上）→ 结果作废直接下线（满封面
    /// 将接管显示，廊封面不与其竞争）。返回是否有新走廊封面入槽（调用方决定重绘）。
    ///
    /// WARN-2（评审接受项）：本类瞬态占位无 recolor 世代守卫——切档前入队的旧色廊封面
    /// 可能短暂落槽闪旧色；瞬态、随后被下一世代满封面/瓦片接管，可接受（不留脏槽）。
    pub(super) fn collect_prefetch_cover_results(&mut self, scale_factor: f32) -> bool {
        let finished = collect_finished(&mut self.pending_prefetch_cover, |(_, _, rx)| rx.try_recv());
        let mut any = false;
        for (page, res) in finished {
            let req_scale = self
                .pending_prefetch_cover
                .remove(&page)
                .map(|(s, _, _)| s)
                .unwrap_or(0.0);
            let Some(res) = res else {
                continue;
            };
            if page >= self.dims.len() {
                continue;
            }
            // 满密度封面**已按当前 cscale 密度入槽** → 走廊结果作废（已锐利，低密 preview
            // 不再需要）。
            // ⚠️ **不再因「封面在途」作废**（A 两段式底图前提）：跳变/常态滚动时满密 cover
            // 渲染 ~30ms（串行），在途期间让 0.5 密度 preview 先入槽作过渡底图（替换「加载中」），
            // cover 到位后 `page_base_image` 自然以 cover 优先、preview 仍留 slot（受 40MB 预算
            // 逐出）——只省下「到达的廉价渲染被白弃」的浪费，不改变任何预算/驻留上限。
            // **S2 修正（评审 BLOCK①）**：仅当「当前 cscale 密度 cover」在位才弃——缩放中途
            // stale cover（`cover_scale ≠ 当前 cscale`，正持旧档的可见页原地连续缩放）在位时
            // preview 应保留入槽，屏显底图从「旧封面 1.12^k 复利拉伸」降为「当前档 0.5 密度」
            // （与 S4 最新档桥同一目标；配合 armed 首帧的 stale cover 迁移 ② 后该场景激活）。
            let cscale = cover_scale(self.dims[page], self.page_width(), scale_factor);
            let s = &self.slots[page];
            // **手势期 preview 不换底（第 4 轮同根因的 low-dens 侧）**：`zoom_armed` 期间
            // 已有任何底（cover/prev_cover/prefetch_cover）的页**不**用新档 preview 替换——
            // 屏显底图链 cover→prev_cover 优先，新 preview 到也只会被链挡住/徒增逐档换密
            // churn；仅「全无底」的空页（真正 placeholder）才由新 preview 填一次（拍板：
            // 与 dispatch 侧 `maybe_dispatch_zoom_preview` 的 armed「有底不派」守卫对称）。
            if self.zoom_armed
                && (s.cover.is_some() || s.prev_cover.is_some() || s.prefetch_cover.is_some())
            {
                continue;
            }
            if s.cover.is_some() && (s.cover_scale - cscale).abs() <= 1e-4 {
                continue;
            }
            let Ok(img) =
                crate::pdf::wrap_render_image(res.bgra, res.width, res.height, res.luma)
            else {
                continue;
            };
            if let Some(s) = self.slots.get_mut(page) {
                if let Some(old) = s.prefetch_cover.replace(img) {
                    self.pending_drops.push(old);
                }
                s.prefetch_cover_scale = req_scale;
                any = true;
            }
        }
        any
    }

    /// 封面派发（`schedule_tiles` 停稳帧 与 `schedule_fast_covers` 中速带共用）：只服务封面窗
    /// [cover_first..=cover_last]，按离可见中心 `center_vis` 近远排序、`COVER_INFLIGHT` 封顶。
    /// 缩放过期封面移入 `prev_cover` 过渡桥再重派新密度；派后 `evict_covers` 维护封面驻留窗 +
    /// 方向退位页转存有界桥（F2）。封面 = 满显示密度整页（`cover_scale == render_scale`），
    /// 一渲铺满、逐片锐化——「滚动即清晰」的中速带主战场。
    ///
    /// **P1 ③（邻居降密）**：封面密度按该页**是否真可见**分档——可见页（页与视口 bbox
    /// 相交）满密度；封面窗内的滑入/滑出邻居页（`page_visible_rect` 为 None）降到
    /// [`config::pdf::COVER_SCALE_CAP_NEIGHBOR`]（默认 2.0）档，A4 每张从 ~31MB 落到
    /// ~2MB 量级。邻居入视瞬间：`cover_scale` 与当前满密度不等 → 既有 stale→prev_cover 桥
    /// + 满密度重派无缝接管，无闪白（`cover_retire_bridge` 类测试守卫不变）。
    fn schedule_covers(
        &mut self,
        cover_first: usize,
        cover_last: usize,
        center_vis: usize,
        retire: Option<usize>,
        scroll_y: f32,
        viewport_h: f32,
        wrap_w: f32,
        page_width: f32,
        scale_factor: f32,
    ) {
        let cover_worker = self.worker.clone().unwrap();
        let mut cover_order: Vec<usize> = (cover_first..=cover_last).collect();
        cover_order.sort_by_key(|&p| (p as i64 - center_vis as i64).unsigned_abs());
        for page in cover_order {
            // R4：已降级/降级候选页不派封面——已降级页显示由均匀 0.5 preview/既有底图链供给
            // （瓦片调度同源跳过）；候选页（预算触顶静态计数中）也不派，避免「candidate 期间
            // 重派 cap cover → activation 的 preview 派发被 pending_cover 挡住」的互相挡住
            // （且其显示由既有底图链经 settle 门控统一，宁均匀不格子）。
            if self.degraded.contains(&page) || self.degrade_frames.contains_key(&page) {
                continue;
            }
            let dim = self.dims[page];
            // **P1 ③**：按「真可见」分档密度——可见页满密度；仅露角/窗外邻居（有封面窗内
            // 但不可见）降档，低驻留同时靠既有桥机制无缝接管（见方法注释）。
            let cscale = {
                let full = cover_scale(dim, page_width, scale_factor);
                if self
                    .page_visible_rect(page, scroll_y, viewport_h, page_width, wrap_w)
                    .is_some()
                {
                    full
                } else {
                    full.min(config::pdf::COVER_SCALE_CAP_NEIGHBOR)
                }
            };
            // 缩放过期（缩放/DPI 已变）→ 旧封面移入 `prev_cover` 作 CSS 拉伸兜底底图
            // （不再立即下线：本帧渲染还靠它铺满新页宽，防「Ctrl+滚轮缩放」闪「加载中」）。
            // `cover` 置空 → 下方按新密度重派封面，新封面到位后取代并下线旧封面。
            // 含「可见↔邻居」档间翻转：入视（邻居 2.0 → 满密度）与退位（满密度 → 邻居 2.0）
            // 都经同一 stale→桥→重派通道，无闪白。
            if let Some(slot) = self.slots.get_mut(page) {
                let stale = slot.cover.is_some() && (slot.cover_scale - cscale).abs() > 1e-4;
                if stale
                    && let Some(old) = slot.cover.take() {
                        slot.prev_cover = Some(old);
                    }
            }
            // 整页不超预算 且 无封面 且 未在途 → 派发（W2：封面已拉黑页跳过——不盲目重试，
            // 该页由瓦片供给）。**S5 防环**：仅「S5 刚回收的高倍页」（`cover_scale` 已被
            // retire 置 0 且瓦片在位）跳过封面重派——否则 S5 回收后下一帧又会重派 cover，
            // 形成「回收→重派→渲染→再回收」死环；普通「瓦片在位」页照常由封面接管
            // （`cover_serves_tile_density` 由 plan_tiles_in_window 决定是否 skip 瓦片）。
            if !self.cover_blacklist.contains(&page)
                && cover_affordable(dim, cscale)
                && self.slots[page].cover.is_none()
                && !self.pending_cover.contains_key(&page)
                && self.pending_cover.len() < COVER_INFLIGHT
                && !(self.slots[page].cover_scale.abs() <= 1e-4
                    && !self.slots[page].tiles.is_empty())
            {
                if timing_view() {
                    eprintln!("[{:.1}ms] [VIEW-COVER-REQ] page={page}", crate::pdf::timing_ms());
                }
                let rx =
                    cover_worker.request_render(self.doc_id, RenderRequest { page, scale: cscale }, self.nav_gen, self.zoom_epoch); // L2.2+Z2 stamp
                self.raster_reqs += 1; // 切档不重栅回归计数
                self.cover_reqs += 1; // T1 首屏「封面 vs 瓦片」构成计数
                // 记录**请求时** recolor 世代（NIT：旧世代请求的失败不拉黑，语义纯净）。
                // L2.4：记录导航世代 nav_gen（大跳世代推进时按世代清旧条目、预算立即释放）。
                self.pending_cover.insert(page, (cscale, self.recolor_epoch, self.nav_gen, rx));
            }
        }
        self.evict_covers(cover_first, cover_last, retire);
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
    /// - `pub(crate)`：lib.rs 的「静止必收敛」回归复用本函数作唯一几何权威（零重实现）。
    pub(crate) fn plan_tiles_in_window(
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
            // R4：预算触顶的降级候选/已降级页（`update_tile_budget_and_degrade` 维护）——
            // 不再派瓦片。已降级页由均匀 0.5 底提供显示；候选页（静态计数中）显示由既有
            // cap 封面整页统一底提供，两者都不再「派→逐→重派」震荡。
            if self.degrade_frames.contains_key(&page) || self.degraded.contains(&page) {
                continue;
            }
            let scale = render_scale(page_width, scale_factor, dim.0);
            let Some(rect) =
                self.page_visible_rect(page, scroll_y, viewport_h, page_width, wrap_w)
            else {
                continue;
            };
            // 整页封面已在当前密度下覆盖整页 → 该页瓦片全部冗余，**不再派发**（省渲染与
            // 内存：封面即全页锐化底图，瓦片只服务于「无封面/封面被密度上限截断」的页）。
            // **T1：封面 affordable 且与瓦片同密度**（`cover_scale == scale`，封面可整页
            // 一次到位取代瓦片）的页**在封面真的在槽/在途时**同样跳过瓦片——这类页由
            // `schedule_tiles` 的同帧封面派发供给（瓦片跳过后封面即该页唯一栅格），首屏/
            // 翻页不再出现「瓦片先派 + 同密度封面后到再作废」的双重栅格。本函数
            // 是 `schedule_tiles` 与快速拖动减速预热 `schedule_placeholders` 的共用几何，
            // 后者因此也不会在停稳前预派这类页的瓦片（避免补封面时双渲）。仅当封面密度被
            // COVER_SCALE_CAP 截断（`cscale < scale`，超高倍率「底图+精修」——瓦片更细）
            // 或页位图超 COVER_SIZE_BUDGET（纯瓦片路径）时，才继续按瓦片派发。
            let cscale = cover_scale(dim, page_width, scale_factor);
            // **D cap 刀刃稳健化**：密度判定用 `cover_serves_scale`（含 cap 邻域等效区间
            // `[COVER_SCALE_CAP, COVER_DENSITY_EPS_ZOOM]` 强制 cover-served）——3.99 vs 4.02
            // 跨 cap 两翼的 LOD 决策不再一个 cover(锐) 一个瓦片(糊) 翻转（见布局层纯函数）。
            let cover_serves_tile_density =
                cover_affordable(dim, cscale) && crate::pdf::layout::cover_serves_scale(cscale, scale);
            // **伪机制根治**：skip_covered 必须「封面真的在槽/在途」才成立——仅凭
            // cover_serves_tile_density（hypothetical「封面 affordable && 能等效瓦片密度」）
            // 就跳瓦片：真机封面渲染被丢/失败/逐出而未到位（headless 单 worker 封面瞬达
            // 掩盖）时，该页瓦片也被跳 → 停在低密底图、零在途、静止冻结（糊），只能靠
            // 再次缩放强制重渲。故 skip = 封面**在槽**（`cover_scale ≈ cscale`）**或**在途
            // （`pending_cover` 含该页且请求密度 = 当前 cscale——旧密度在途结果到后会被
            // `collect_cover_results` 密度守卫丢弃，不能作数）；封面缺失即 fall-through 派
            // 当前密度 1:1 瓦片兜底，瓦片成为最终锐利供给。
            // - 与既有 T1/同密度语义：schedule_covers 同帧对 affordable 页派封面，仅「在槽/
            //   在途」页才跳瓦片 → 封面先行主路径的「不双渲」不变；封面缺失页的瓦片由
            //   TILE_INFLIGHT/budget 自理，封面到位后 `collect_tile_results`/`evict_tiles`
            //   的「同密度封面冗余」自动回收，不长期双驻留。
            // - 与 S5 回收（`cover_scale==0` + 瓦片在位）不冲突：S5 页 cover_scale=0 ≠ cscale
            //   → 在槽判定失败 → 派瓦片（正是「cover 已回收、1:1 瓦片接管」的应有供给）。
            // - **W2**：封面持续失败的页（`cover_blacklist`）天然在槽/在途都缺 → 回退瓦片
            //   核发（否则封面永不再派而瓦片又永远跳过 → 页面停在占位/模糊）。
            let cover_in_slot =
                self.slots[page].cover.is_some() && (self.slots[page].cover_scale - cscale).abs() <= 1e-4;
            let cover_inflight = self
                .pending_cover
                .get(&page)
                .is_some_and(|(ps, _, _, _)| (ps - cscale).abs() <= 1e-3);
            let skip_covered = !self.cover_blacklist.contains(&page)
                && cover_serves_tile_density
                && (cover_in_slot || cover_inflight);
            if skip_covered {
                continue;
            }
            let bmp = page_bitmap_size(dim, scale);
            let disp_h = display_height(dim, page_width);
            let Some((r0, c0, r1, c1)) = visible_tile_range_axis(
                bmp,
                page_width,
                disp_h,
                rect,
                tile_size,
                TILE_MARGIN,
                config::pdf::TILE_MARGIN_COL,
            )
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

    // ---- 快速拖动占位（F3：中速带满密度封面优先）----

    /// 中速带（[`SpeedState::Fast`]）调度：**满密度封面优先**——为可见页 + 滚动方向前瞻 1 页
    /// 按 `schedule_covers` 派发整页锐化封面（复用停稳帧同一封面循环：cover_affordable + 在途
    /// 守卫 + [`COVER_INFLIGHT`] 预算 + F2 的 cover_first/cover_last 窗与退位桥），新进视口页
    /// 滚到哪清晰到哪，不再落 520/260 低清占位。**不派瓦片**（渲染队列不加活，防「封面堵在
    /// 瓦片之后」）。
    ///
    /// 对「封面不可负担」（病理性 > [`COVER_SIZE_BUDGET`] 的页）回退 `schedule_placeholders`
    /// 的中清/缩略占位（保留既有 LOD 路径作回退，不裸退「加载中」）。
    pub(super) fn schedule_fast_covers(
        &mut self,
        window: &Window,
        start: usize,
        end: usize,
        scroll_y: f32,
        viewport_h: f32,
        page_width: f32,
        scale_factor: f32,
    ) {
        // 封面窗与 `schedule_tiles` 同源（可见页 + 滚动方向前瞻 1；方向未知退对称 ±1）。
        let (vis_first, vis_last) =
            self.visible_page_span(start, end, scroll_y, viewport_h, page_width, window);
        let vis_first = vis_first.unwrap_or(start);
        let vis_last = vis_last.unwrap_or(end);
        let first = vis_first.saturating_sub(1);
        let last = (vis_last + 1).min(self.dims.len().saturating_sub(1));
        let (cover_first, cover_last) = match self.scroll_dir {
            1 => (vis_first, (vis_last + 1).min(self.dims.len().saturating_sub(1))),
            -1 => (vis_first.saturating_sub(1), vis_last),
            _ => (first, last),
        };
        let retire = match self.scroll_dir {
            1 => cover_first.checked_sub(1),
            -1 => cover_last.checked_add(1).filter(|&p| p < self.dims.len()),
            _ => None,
        };
        let center_vis = (vis_first + vis_last) / 2;
        // ① 满密度封面（封面不可负担的页本循环自然跳过，不派）。
        self.schedule_covers(
            cover_first,
            cover_last,
            center_vis,
            retire,
            scroll_y,
            viewport_h,
            self.wrap_width(window, page_width),
            page_width,
            scale_factor,
        );
        // ② 回退占位：仅「无封面/无封面在途」的页（封面满密度，占位是它的低价兜底，
        //    不重复占用）；已由 `schedule_covers` 派的页此处跳过。
        self.schedule_placeholders(window, start, end, scroll_y, viewport_h, scale_factor);
    }

    /// LOD 占位派发（中速带封面不可负担页的回退路径）：可见页派 L0.5 中清（比缩略图清晰，
    /// 可辨识标题/段落）、L0 缩略图（纯内存，无磁盘缓存）。**只为「无满密度封面（或在途）」
    /// 的页派发**——F3 起封面是中速带主供给，中清/缩略只给病理性超预算页兜底；停稳后常态
    /// 调度自动补齐瓦片、替换占位。旧「减速期瓦片预热」段随 F3 移除（中速带改派满密度封面，
    /// 停稳帧 `schedule_tiles` 补瓦片；派瓦片只会让封面排在瓦片之后、违背「滚到哪清晰到哪」）。
    pub(super) fn schedule_placeholders(
        &mut self,
        _window: &Window,
        start: usize,
        end: usize,
        _scroll_y: f32,
        _viewport_h: f32,
        scale_factor: f32,
    ) {
        let worker = self.worker.clone().unwrap();
        // 1) L0.5 中清：优先派发（比缩略图清晰），独立额度不挤占瓦片/缩略图
        let has_cover = |v: &Self, page: usize| {
            v.slots[page].cover.is_some() || v.pending_cover.contains_key(&page)
        };
        if self.pending_mid.len() < MAX_MID_INFLIGHT {
            let mut budget = MAX_MID_INFLIGHT - self.pending_mid.len();
            for page in start..=end {
                if budget == 0 {
                    break;
                }
                if has_cover(self, page) {
                    continue; // 满密度封面（或在途）在位 → 无需占位代价
                }
                if self.mid_cached(page).is_some() {
                    continue; // 主线程缓存命中：直接复用（含 LRU 触摸），不发 WorkerMsg::Mid
                }
                if self.slots[page].mid_loading {
                    continue;
                }
                self.slots[page].mid_loading = true;
                let rx = worker.request_mid(self.doc_id, page, scale_factor, self.nav_gen, self.zoom_epoch); // L2.2+Z2 stamp
                self.raster_reqs += 1; // 切档不重栅回归计数
                self.pending_mid.insert(page, (self.nav_gen, rx));
                budget -= 1;
            }
        }
        // 2) L0 缩略图：仅为「无中清/无在途中清」且「无封面」的可见页派发
        if self.pending_thumb.len() < MAX_THUMB_INFLIGHT {
            let mut tb = MAX_THUMB_INFLIGHT - self.pending_thumb.len();
            for page in start..=end {
                if tb == 0 {
                    break;
                }
                if has_cover(self, page) {
                    continue;
                }
                if self.slots[page].thumb.is_some()
                    || self.slots[page].thumb_loading
                    || self.slots[page].mid.is_some()
                    || self.slots[page].mid_loading
                {
                    continue;
                }
                self.slots[page].thumb_loading = true;
                let rx = worker.request_thumb(self.doc_id, page, self.nav_gen, self.zoom_epoch); // L2.2 enqueue-n_gen
                self.raster_reqs += 1; // 切档不重栅回归计数
                self.pending_thumb.insert(page, (self.nav_gen, rx));
                tb -= 1;
            }
        }
    }

    // ---- 在途结果回收 ----

    /// 回收已完成的在途瓦片结果：worker 回传 RGBA → 重建 `RenderImage` 装入对应页槽
    /// （密度守卫：请求时的 `scale` ≠ 当前 `scale` → 丢弃，缩放已变）。替换旧瓦片时
    /// 旧纹理走 `pending_drops` 显式下线。返回是否有新瓦片入槽（调用方决定重绘）。
    pub(super) fn collect_tile_results(&mut self, scale_factor: f32) -> bool {
        let finished = collect_finished(&mut self.pending_tiles, |(_, _, rx)| rx.try_recv());
        if finished.is_empty() {
            return false;
        }
        let page_width = self.page_width();
        let mut any = false;
        for (key, res) in finished {
            let req_scale = self
                .pending_tiles
                .remove(&key)
                .map(|(s, _, _)| s)
                .unwrap_or(0.0);
            let (page, row, col) = key;
            let Some(res) = res else {
                continue;
            };
            if page >= self.dims.len() {
                continue;
            }
            // W1：CPU 回退路径下，切档（SetRecolor/RecolorRefresh）与在途渲染结果走两条
            // mpsc 无总体次序——worker 按**出队当时** recolor 着色，若该结果晚于新世代
            // 刷新条目到达，会覆盖新鲜槽位却无后续纠正。世代号守卫：回传世代 ≠ 当前 →
            // 旧着色结果丢弃（GPU 门控下结果恒为 raw，着色在绘制时，guard 跳过零开销）。
            if !self.gpu_recolor && res.recolor_epoch != self.recolor_epoch {
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
            let Ok(img) = crate::pdf::wrap_render_image(res.bgra, res.w, res.h, res.luma) else {
                continue;
            };
            if let Some(slot) = self.slots.get_mut(page) {
                // R4：降级页不接受新瓦片（预算释放、显示由 0.5 均匀底提供）——否则 in-flight
                // 旧瓦片晚到会重填槽、被逐出/重派震荡。
                if self.degraded.contains(&page) {
                    self.pending_drops.push(img);
                    continue;
                }
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

    /// 瓦片字节预算淘汰。**R1/R2/R3 改造**（高倍「方形格子、清晰度不一」根治）：
    /// - **① 正确性清扫先行（不变）**：对**所有页**统一清「同密度整页封面在位 → 瓦片冗余」
    ///   与「`tile_scale` 与当前密度不符 → 过期」两类的瓦片（封面即全页锐化底图、过期片
    ///   回看也必重渲）——这是清扫，**先于且不跳过**钉住判断（钉住格同样被清）。
    /// - **② 预算 LRU 逐出（R1）**：逐出候选**跳过当前钉住格**（`tile_budget_state.pinned`
    ///   = 各可见页 `visible_tile_range_axis(可见 bbox + 纵轴 TILE_MARGIN / 横轴 TILE_MARGIN_COL)` 覆盖的格子，由
    ///   `update_tile_budget_and_degrade` 每帧同源计算）——可见区绝不逐出、下帧不再重请 →
    ///   消灭「逐出→重派→每帧震荡永不收敛」；预算用**动态值**（`tile_budget_state.budget`
    ///   = max(固定, 可见格数×单格字节×1.25)，夹 [`config::pdf::TILE_CACHE_BYTES_MAX`]
    ///   硬顶），钉住集 ≤ 预算**天然成立**。
    /// - **③ 淘汰排名（R2）改视口相对**：`|page − 可见中心页|×1e6 + |row − 该页可见行中心|`
    ///   （与 `plan_tiles_in_window` 派发优先级同源语义），消除「留已滚过的顶行、逐出刚入视
    ///   的行」。
    ///
    /// `tile_budget_state == None`（未走 `ensure_window` 的直接调用，如单测）→ 回退旧语义：
    /// 固定 [`config::pdf::TILE_CACHE_BYTES`] 预算、无钉住、绝对行号排名。旧纹理走
    /// `pending_drops` 显式下线 GPU 纹理。
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
        // R3 预算 + R1 钉住集（None = 直接调用回退固定预算/无钉住/绝对行号排名）。
        let (budget, center, pinned, mid_rows) = match &self.tile_budget_state {
            Some(s) => (s.budget, s.center, Some(&s.pinned), Some(&s.mid_rows)),
            None => (TILE_CACHE_BYTES, (start + end) / 2, None, None),
        };
        if self.tile_bytes <= budget {
            return;
        }
        let mut candidates: Vec<(u64, usize, u32, u32)> = Vec::new();
        for (page, slot) in self.slots.iter().enumerate() {
            for &(row, col) in slot.tiles.keys() {
                // R1：钉住格（可见矩形 + TILE_MARGIN）绝不逐出。
                if pinned.is_some_and(|ps| ps.contains(&(page, row, col))) {
                    continue;
                }
                // R2：视口相对排名（该页可见行中心；无钉住态的页回落绝对行号=旧语义）。
                let mid = mid_rows.and_then(|m| m.get(&page)).copied().unwrap_or(0);
                let d = (page as i64 - center as i64).unsigned_abs() * 1_000_000
                    + (row as i64 - mid as i64).unsigned_abs();
                candidates.push((d, page, row, col));
            }
        }
        candidates.sort_by_key(|c| std::cmp::Reverse(c.0)); // 最远优先
        for (_, page, row, col) in candidates {
            if self.tile_bytes <= budget {
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

    /// **R1/R3 动态瓦片预算 + R4 均匀降级状态机**（每帧由 `open::ensure_window` 在调度/
    /// 淘汰前调用，含静态帧门控帧——门控帧也须推进降级计数与钉住态，否则「静态 N 帧」停在 0）。
    ///
    /// 职责三件：
    /// 1. 用与调度同源的几何（`page_visible_rect` + `visible_tile_range_axis(可见 bbox + 纵轴 TILE_MARGIN / 横轴 TILE_MARGIN_COL`
    ///    TILE_MARGIN)`）算出各可见页钉住格，写入 [`Self::tile_budget_state`]（供
    ///    [`Self::evict_tiles`] 消费：预算与钉住集同帧一致、杜绝漂移）。
    /// 2. 预算 = `max(TILE_CACHE_BYTES, 可见格数 × 单格字节 × 1.25)`、夹
    ///    [`config::pdf::TILE_CACHE_BYTES_MAX`]；未触顶 → 全钉住（R1：可见区绝不逐出）。
    /// 3. 预算触顶（可见格 >~102）→ 从**最远可见页**起把放不下的页选为 **R4 降级候选**
    ///    （静态 + 连续 [`config::pdf::DEGRADE_STATIC_FRAMES`] 帧）→ 激活
    ///    [`Self::degraded`]：清瓦片 + 退休 cap 截断 cover + 派发**均匀 0.5 密度 preview**
    ///    垫底（宁均匀不格子、内存有界、无死锁；正常路径预算够时 cap cover 照旧）。
    ///    候选/已降级页退出钉住 → `plan_tiles_in_window`/`schedule_covers` 跳过派发（无震荡）；
    ///    帧不再触顶或页滑出可见窗 → 清除降级态并自愈回 1:1。
    ///
    /// 返回本帧因降级退休的 cover 与清出的瓦片 `Arc`（调用方入 `pending_drops`）。
    pub(super) fn update_tile_budget_and_degrade(
        &mut self,
        start: usize,
        end: usize,
        scroll_y: f32,
        viewport_h: f32,
        page_width: f32,
        scale_factor: f32,
        window: &Window,
    ) -> Vec<Arc<RenderImage>> {
        // P2 R8：门控激活（GPU + FullBand + ink）时瓦片按单通道亮度编码（1BPP），单格名义
        // 字节随之 /4——动态预算按**实际 bpp** 收敛，避免「字节已除 4 而预算仍按 4BPP 估」
        // 导致预算语义错 4 倍（R8 下同一可见格数应允许 4× 更多格驻留、内存更省）。
        let nominal_per_tile =
            (TILE_SIZE as u64) * (TILE_SIZE as u64) * crate::recolor::tile_bytes_per_pixel(self.recolor, self.gpu_recolor); // 512²×bpp
        let demand_of = |cells: u64| {
            (cells as f64 * nominal_per_tile as f64
                * config::pdf::TILE_BUDGET_KEEP_FACTOR as f64) as u64
        };
        let mut drops: Vec<Arc<RenderImage>> = Vec::new();
        let n = self.dims.len();
        if n == 0 || start > end || viewport_h <= 0.0 {
            self.tile_budget_state = None;
            self.degrade_frames.clear();
            self.recover_degraded();
            return drops;
        }
        let end = end.min(n - 1);
        let wrap_w = self.wrap_width(window, page_width);
        // ① 各可见页钉住几何（页 → (格数, 行中心)）。
        let (v_first, v_last) =
            self.visible_page_span(start, end, scroll_y, viewport_h, page_width, window);
        let (Some(vis_first), Some(vis_last)) = (v_first, v_last) else {
            self.tile_budget_state = Some(TileBudgetState {
                budget: TILE_CACHE_BYTES,
                center: (start + end) / 2,
                pinned: HashSet::new(),
                mid_rows: HashMap::new(),
            });
            self.degrade_frames.clear();
            self.recover_degraded();
            return drops;
        };
        let vis_last = vis_last.min(n - 1);
        let center_vis = (vis_first + vis_last) / 2;
        #[derive(Clone, Copy)]
        struct Pin {
            demand: u64,
            mid_row: u32,
        }
        let mut pins: Vec<(usize, Pin)> = Vec::new();
        for page in vis_first..=vis_last {
            let dim = self.dims[page];
            let scale = render_scale(page_width, scale_factor, dim.0);
            let bmp = page_bitmap_size(dim, scale);
            let disp_h = display_height(dim, page_width);
            let Some(rect) = self.page_visible_rect(page, scroll_y, viewport_h, page_width, wrap_w)
            else {
                continue;
            };
            let Some((r0, c0, r1, c1)) =
                visible_tile_range_axis(bmp, page_width, disp_h, rect, TILE_SIZE, TILE_MARGIN, config::pdf::TILE_MARGIN_COL)
            else {
                continue;
            };
            let cells = (r1 - r0 + 1) as u64 * (c1 - c0 + 1) as u64;
            pins.push((
                page,
                Pin {
                    demand: demand_of(cells),
                    mid_row: (r0 + r1) / 2,
                },
            ));
        }
        // ② 未触顶：全钉住（预算自动包住钉住集 ≤ 1.25×），清一切降级态。
        //    硬顶可被测试钩子覆写（`tile_budget_cap_override` → 构造「预算触顶」场景）。
        let max_budget = self
            .tile_budget_cap_override
            .unwrap_or(config::pdf::TILE_CACHE_BYTES_MAX);
        let total_requested = pins.iter().map(|&(_, p)| p.demand).sum::<u64>();
        if total_requested <= max_budget {
            let mut pinned = HashSet::new();
            let mut mid_rows = HashMap::new();
            for &(page, pin) in &pins {
                mid_rows.insert(page, pin.mid_row);
                let dim = self.dims[page];
                let scale = render_scale(page_width, scale_factor, dim.0);
                let bmp = page_bitmap_size(dim, scale);
                let disp_h = display_height(dim, page_width);
                if let Some(rect) = self
                    .page_visible_rect(page, scroll_y, viewport_h, page_width, wrap_w)
                    && let Some((r0, c0, r1, c1)) =
                        visible_tile_range_axis(bmp, page_width, disp_h, rect, TILE_SIZE, TILE_MARGIN, config::pdf::TILE_MARGIN_COL)
                {
                    for row in r0..=r1 {
                        for col in c0..=c1 {
                            pinned.insert((page, row, col));
                        }
                    }
                }
            }
            self.tile_budget_state = Some(TileBudgetState {
                budget: total_requested.max(TILE_CACHE_BYTES),
                center: center_vis,
                pinned,
                mid_rows,
            });
            self.degrade_frames.clear();
            self.recover_degraded();
            return drops;
        }
        // ③ 触顶：从最远可见页起，放不下的页 → R4 降级候选/已降级（它们退出钉住、不占预算）。
        let mut order: Vec<(usize, Pin)> = pins.clone();
        order.sort_by_key(|&(p, _)| core::cmp::Reverse((p as i64 - center_vis as i64).unsigned_abs()));
        let mut remaining = max_budget as u64;
        let mut sharp: HashSet<usize> = HashSet::new();
        for &(page, pin) in &order {
            if self.degraded.contains(&page) {
                continue; // 已降级页不占预算、保持降级（稳定不抖动）
            }
            if pin.demand > remaining {
                continue; // 放不下 → 候选（连续静态 N 帧后激活降级）
            }
            remaining -= pin.demand;
            sharp.insert(page);
        }
        let static_ok = !self.zoom_armed && !self.speed_state.is_fast();
        // 候选页 = 可见页 ∩ 非 sharp ∩ 非已降级（已降级保持）。
        let candidate_set: HashSet<usize> = pins
            .iter()
            .map(|&(p, _)| p)
            .filter(|p| !sharp.contains(p) && !self.degraded.contains(p))
            .collect();
        if static_ok {
            for &page in &candidate_set {
                let e = self.degrade_frames.entry(page).or_insert(0);
                *e = e.saturating_add(1);
                if *e >= config::pdf::DEGRADE_STATIC_FRAMES {
                    drops.extend(self.activate_degrade(page, page_width, scale_factor));
                }
            }
            // 计数只保留候选页（已激活的移走、非候选/滑出可见窗的清除）。
            self.degrade_frames.retain(|p, _| candidate_set.contains(p));
        } else {
            self.degrade_frames.clear(); // 运动中不推进降级计数
        }
        // 钉住集 = sharp 可见页的格子；预算 = sharp 需求之和（≤ 硬顶）。
        let sharp_demand = pins
            .iter()
            .filter(|&(p, _)| sharp.contains(p))
            .map(|&(_, pin)| pin.demand)
            .sum::<u64>();
        let mut pinned = HashSet::new();
        let mut mid_rows = HashMap::new();
        for &(page, pin) in &pins {
            if !sharp.contains(&page) {
                continue;
            }
            mid_rows.insert(page, pin.mid_row);
            let dim = self.dims[page];
            let scale = render_scale(page_width, scale_factor, dim.0);
            let bmp = page_bitmap_size(dim, scale);
            let disp_h = display_height(dim, page_width);
            if let Some(rect) = self
                .page_visible_rect(page, scroll_y, viewport_h, page_width, wrap_w)
                && let Some((r0, c0, r1, c1)) =
                    visible_tile_range_axis(bmp, page_width, disp_h, rect, TILE_SIZE, TILE_MARGIN, config::pdf::TILE_MARGIN_COL)
            {
                for row in r0..=r1 {
                    for col in c0..=c1 {
                        pinned.insert((page, row, col));
                    }
                }
            }
        }
        self.tile_budget_state = Some(TileBudgetState {
            budget: sharp_demand.max(TILE_CACHE_BYTES).min(max_budget),
            center: center_vis,
            pinned,
            mid_rows,
        });
        drops
    }

    /// R4 降级激活：把页 `page` 从「1:1 瓦片」切到「**均匀 0.5 密度 preview**」——
    /// 清空该页瓦片（不再显示、不再派发、立即回内存）、退休 cap 截断 cover（render 侧判等
    /// 失败 → `cover_now=false` → settle 门控对部分瓦片整帧禁用 → 屏显整页统一底图，
    /// **宁均匀不格子**）、并派发一版 0.5 密度 preview 作垫底（复用
    /// [`config::pdf::PREFETCH_COVER_DENSITY`] 机制/几何：`request_render_preview` →
    /// `slot.prefetch_cover`）。preview 不可负担（病理性超大页）→ 放弃降级（保持 cap cover
    /// 均匀底，虽糊但非格子、内存有界）。返回被线下/退休的位图（调用方入 `pending_drops`）。
    fn activate_degrade(
        &mut self,
        page: usize,
        page_width: f32,
        scale_factor: f32,
    ) -> Vec<Arc<RenderImage>> {
        let mut drops = Vec::new();
        let dim = self.dims[page];
        let cscale = cover_scale(dim, page_width, scale_factor);
        let pscale = (cscale * config::pdf::PREFETCH_COVER_DENSITY)
            .min(config::pdf::COVER_SCALE_CAP);
        // 0.5 preview 都渲不动（病理性超大页）→ 放弃降级（保持现状，宁糊不空）。
        if !cover_affordable(dim, pscale) {
            return drops;
        }
        self.degraded.insert(page);
        self.degrade_frames.remove(&page);
        {
            let slot = &mut self.slots[page];
            // 清瓦片：从 1:1 供给撤下（不显示、不再派发），内存立即释放。
            let bytes = slot.tile_bytes;
            slot.tile_bytes = 0;
            self.tile_bytes = self.tile_bytes.saturating_sub(bytes);
            for (_, ts) in slot.tiles.drain() {
                drops.push(ts.image);
            }
            // 退休 cap 截断 cover：render 侧判等失败 → settle 门控「宁糊不混」统一底图生效。
            if let Some(old) = slot.cover.take() {
                drops.push(old);
            }
            slot.cover_scale = 0.0;
        }
        // 派发均匀 0.5 底（已有同密度 preview/在途则不重复派）。不设 `pending_cover` 守卫：
        // 在途 cap cover 结果到达时会被 `collect_cover_results` 的 degraded 守卫丢弃，双在途
        // 无害（preview 才是降级页的唯一底图）。
        let has_preview = self.slots[page]
            .prefetch_cover
            .as_ref()
            .is_some_and(|_| (self.slots[page].prefetch_cover_scale - pscale).abs() <= 1e-4);
        if !has_preview
            && !self.pending_prefetch_cover.contains_key(&page)
            && let Some(worker) = self.worker.clone()
        {
            let rx = worker.request_render_preview(
                self.doc_id,
                RenderRequest { page, scale: pscale },
                self.nav_gen, // L2.2 enqueue-n_gen（世代语义不变）
                self.zoom_epoch, // Z2：发送时打戳当前缩放世代
            );
            self.raster_reqs += 1;
            self.prefetch_reqs += 1; // F4 观测计数（降级 preview 与走廊同计通道，预算语义一致）
            self.pending_prefetch_cover.insert(page, (pscale, self.nav_gen, rx));
        }
        drops
    }

    /// 清退所有降级页（帧未触顶 / 无可见页时调）：把 `self.degraded` 逐页复原
    /// （瓦片由常态调度重新派发、封面可重派 → 恢复 1:1）。只遍历小的降级集，O(降级数)；
    /// 降级页的瓦片/封面已在 activate 时清空，此处仅复位状态标记让调度恢复供给，
    /// 残留的 0.5 preview 底图由既有走廊预算/封面到位逻辑自然下线。
    fn recover_degraded(&mut self) {
        self.degraded.clear();
    }

    /// 回收已完成的在途**封面**结果：worker 回传整页（屏幕密度）BGRA → 装入 `slot.cover`
    /// （首屏锐化，先于瓦片到达；密度守卫：过期即弃）。返回是否有新封面入槽。
    pub(super) fn collect_cover_results(&mut self, scale_factor: f32) -> bool {
        let finished = collect_finished(&mut self.pending_cover, |(_, _, _, rx)| rx.try_recv());
        let mut any = false;
        for (page, res) in finished {
            let (req_scale, req_epoch) = self
                .pending_cover
                .remove(&page)
                .map(|(s, e, _g, _)| (s, e))
                .unwrap_or((0.0, 0));
            let Some(res) = res else {
                // W2：封面**渲染失败**（worker 返回 None：pdfium 页取不到 / OOM 等）→ 计失败
                // 并连续达阈值拉黑（不再盲目重试，回退瓦片供给）。旧实现仅 remove 后 continue，
                // 每帧盲重试、瓦片又因 skip_covered 永不再派 → 该页停在占位/模糊。
                // **NIT**：仅当前世代请求的失败计数——旧世代请求（切档前入队）的失败不反映
                // 当前参数供给能力，避免旧色失败误拉黑；GPU 门控无世代守卫，恒计（raw 失败
                // 与世代无关）。
                if !self.gpu_recolor && req_epoch != self.recolor_epoch {
                    continue;
                }
                self.cover_failed(page);
                continue;
            };
            if page >= self.dims.len() {
                continue;
            }
            // R4：降级页不接受新封面（退休的 cap cover 不重填；其显示由 0.5 均匀底供给）。
            if self.degraded.contains(&page) {
                continue;
            }
            let cscale = cover_scale(self.dims[page], self.page_width(), scale_factor);
            // **P1 ③（邻居降密可采）**：封面可接受密度 = 满密度 **或** 邻居降档密度
            // （`COVER_SCALE_CAP_NEIGHBOR`）。可见页请求满密度、邻居页请求降档——收集侧无
            // 需区分「当前是否可见」即可接受两者（真正过期档 = 两者都不匹配 → 走 S4 桥）。
            // 可见↔邻居档间翻转由下次 `schedule_covers` 的 stale 判定（旧档 → 桥 → 重派新档）
            // 自愈，入视不闪白。
            let want = |s: f32| {
                (s - cscale).abs() <= 1e-4
                    || (s - cscale.min(config::pdf::COVER_SCALE_CAP_NEIGHBOR)).abs() <= 1e-4
            };
            // W1：同 `collect_tile_results` 的世代守卫——旧着色封面晚到即弃。
            if !self.gpu_recolor && res.recolor_epoch != self.recolor_epoch {
                continue;
            }
            // **手势期零满密换底（第 4 轮真机根因，本批人格分界）**：`zoom_armed` 期间到达的
            // 封面**一律丢弃**——不写 prev_cover 桥、不入 slot.cover。手势前那张高密位图
            // （armed 首帧 `relocate_stale_zoomed_covers` 已迁入 prev_cover）保持为唯一平滑底
            // 基（GPU 拉伸），中间档整页栅格到此为止、不再置换底图 → 无每档整页栅格重排
            // （「松手瞬间跳一下、整页一起动」的根治）。与 Z2 的分工：Z2 在 worker **出队时**
            // 对「stamp 超前」才弃，出队恰新的中间档照样渲完到达——本守卫是「已渲染到达」
            // 层的最后一道闸；settle（armed 清）后走原逻辑（最终档 cover 入 slot、prev_cover
            // take 归位）。
            if self.zoom_armed {
                continue; // 移除了 pending（非封面失败，不计数）
            }
            if !want(req_scale) {
                // 缩放/DPI 已变 → 本密度封面过期。**S4 最新档桥**：过期封面正是本手势的
                // 中间档（req_scale ≠ 当前 cscale），且该页当前档封面尚未入槽 → 比「手势前
                // 旧封面」更贴近当前显示密度的中间档，转存进有界 `prev_cover` 槽（覆盖旧桥，
                // 至多一档 1.12 拉伸），而不是直接丢弃让屏显继续用「手势前旧封面」复利拉伸
                // （1.12^k 雾）。最终档封面入槽后见下方既有 `prev_cover.take()` 自动回收。
                // 缩小（req_scale > cscale）同理。z 过期若该页当前档封面已入槽则不覆盖
                // （新封面即锐，桥无用）。
                if self.slots[page].cover.is_none() {
                    if let Ok(img) =
                        crate::pdf::wrap_render_image(res.bgra, res.width, res.height, res.luma)
                    {
                        if let Some(s) = self.slots.get_mut(page) {
                            if let Some(prev) = s.prev_cover.replace(img) {
                                self.pending_drops.push(prev);
                            }
                        }
                    }
                }
                continue; // 移除 pending（非封面失败，不计数）
            }
            let (w, h) = (res.width, res.height);
            let Ok(img) = crate::pdf::wrap_render_image(res.bgra, w, h, res.luma) else {
                // W2：建图失败视同封面失败（计数路径同上）。
                self.cover_failed(page);
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
                s.cover_scale = req_scale; // 实际渲染密度（可见满密度 / 邻居降档，P1 ③）
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
                // **F2**：封面到位 = 该页显示由整页封面供给 → 同页缩略图同时回收（省每页
                // ~0.37MB，取消「封面+缩略」双份驻留）。回看路径在封面被逐后才走缩略图
                // （`page_base_image` 链 cover→prev_cover→mid→thumb）属既有语义，此处只
                // 回收「已被覆盖」页的冗余缩略。⚠️ 封面退位后该页底图靠 `evict_covers` 的
                // 有界 prev_cover 桥（方向翻位的反向邻居）或 mid/thumb；封面可负担页无瓦片，
                // 若既无桥又无低清占位（更远窗外页回翻重渲期间）仍可能瞬时落「加载中」。
                if let Some(old) = s.thumb.take() {
                    self.pending_drops.push(old);
                }
                // **F4**：满密度封面到位 = 该页显示由整页封面供给 → 同页空闲预取走廊封面
                // 同时回收（廊封面只是高速平移切入的瞬时底图，满封面接管即下线，不双份驻留）。
                if let Some(old) = s.prefetch_cover.take() {
                    self.pending_drops.push(old);
                }
                // W2（NIT 注释修正）：封面成功到位 → 清失败计数与黑名单。主恢复路径其实是
                // 「滚出封面窗」（`evict_covers` 窗外页复位，瓦片供给已接管）；此处只处理
                // 「拉黑时仍有在途请求、其后恰好成功」的边角（不常见但无害，恢复封面供给）。
                self.cover_recovered(page);
                any = true;
            }
        }
        any
    }

    /// W2：封面失败计数 → 拉黑。连续失败达 [`COVER_FAIL_BLACKLIST_THRESHOLD`] 后把该页
    /// 加入 `cover_blacklist`（`plan_tiles_in_window` 对该页回退瓦片供给、`schedule_covers`
    /// 不再盲目重试）；已拉黑页再次失败不再递增（幂等——已进入瓦片供给，重试无意义）。
    /// **复位路径**：拉黑页由 `schedule_covers` 停派封面，封面成功几乎不可达——实际复位
    /// 走「滚出封面窗」（`evict_covers` 窗外页清黑名单/计数，回滚时重新尝试封面）。
    fn cover_failed(&mut self, page: usize) {
        if page >= self.dims.len() || self.cover_blacklist.contains(&page) {
            return;
        }
        let n = self.cover_failures.entry(page).or_insert(0);
        *n += 1;
        if *n >= COVER_FAIL_BLACKLIST_THRESHOLD {
            self.cover_blacklist.insert(page);
        }
    }

    /// W2：封面成功到位 → 清失败计数与黑名单（该页恢复封面供给；见 `cover_failed` 的
    /// 复位路径说明——成功复位是「拉黑时仍有在途请求且恰成功」的边角，主路径是滚出封面窗）。
    fn cover_recovered(&mut self, page: usize) {
        self.cover_failures.remove(&page);
        self.cover_blacklist.remove(&page);
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
            // 回归计数：记录收到的条目 key（含被守卫丢弃者）——「切档只刷可见集」断言：
            // 收到的集合 == 请求的可见集 scope（窗外/已淘汰条目不回传）。
            self.recolor_received.push((entry.kind, entry.page, entry.row, entry.col));
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

    /// 封面内存管理：**只保留当前想要的封面 `[wanted_first..=wanted_last]`**——F2 起该窗为
    /// 「可见页 + 滚动方向前瞻 1 页」（方向未知时退化对称可见页 ±1），窗外封面逐帧回收。
    /// 窗内 ≤ 可见页+1 页，内存有界且**绝不触发换页风暴**：当前窗口的封面永不被淘汰 →
    /// 不会被反复重请求；窗外的旧封面（随滚动累积）下帧即清。
    ///
    /// `retire` = 方向翻位时**刚退位**的反向相邻页（评审 D1 兜底）：其封面不直接下线，
    /// 转存为有界 `prev_cover`（同一时刻至多 1 张）作该页过渡底图——反向微调滚动重读
    /// 该页、且新封面在途时，底图链 cover→prev_cover→mid→thumb 仍非空，不落「加载中」；
    /// 该页重新入窗（新封面到位由 `collect_cover_results` 顺带回收桥）、或让位给新的
    /// 退位页时即释放。更远窗外页（≥2 页）回翻重渲期间仍可能瞬时落「加载中」（既有行为）。
    /// **L3.3 方向感知（与走廊同款精神）**：本函数的「回退侧驻留」由两处现存机制承担——
    /// ① 封面窗的 `cover_first`（正滚时 = 可见首页，含刚滚过的回退侧邻居页）；② `retire` 桥
    /// （方向翻位瞬间的反向邻居）。窗口封面驻留数不随方向权重扩张（`covers ≤ 可见+1` 内存
    /// 断言不受影响），更远的退位页回翻底图由 L3.2/L3.3 的走廊（0.5 密度、独立 40MB 预算）
    /// 供给——滚动方向一侧的近期过页廊封面已按 `dist−dir_bonus` 保驻留。
    pub(super) fn evict_covers(
        &mut self,
        wanted_first: usize,
        wanted_last: usize,
        retire: Option<usize>,
    ) {
        let mut drops: Vec<Arc<RenderImage>> = Vec::new();
        let mut out_window: Vec<usize> = Vec::new(); // W2：窗外页（黑名单随窗回收）
        for (page, slot) in self.slots.iter_mut().enumerate() {
            if page >= wanted_first && page <= wanted_last {
                continue;
            }
            out_window.push(page);
            if Some(page) == retire {
                // 刚退位的反向邻居：封面转存为有界过渡底图（prev_cover），不立即下线。
                if slot.cover.is_some()
                    && let Some(old) = slot.cover.take() {
                        if let Some(prev) = slot.prev_cover.replace(old) {
                            drops.push(prev); // 旧过渡图（如缩放陈旧封面）逐出，保持有界 1 张
                        }
                    }
            } else {
                if slot.cover.is_some()
                    && let Some(old) = slot.cover.take() {
                        drops.push(old);
                    }
                // 旧的过渡兜底同样只保留在当前封面窗口/退位页内（窗外随滚即回收，防越滚越积）。
                if slot.prev_cover.is_some()
                    && let Some(old) = slot.prev_cover.take() {
                        drops.push(old);
                    }
            }
        }
        // W2：封面窗外的页清失败计数与黑名单（滚出可视区 → 该页回滚时重新尝试封面）。
        for page in out_window {
            self.cover_failures.remove(&page);
            self.cover_blacklist.remove(&page);
        }
        self.pending_drops.extend(drops);
    }

    /// 回收已完成的在途 **L0 缩略图**结果：worker 回传低清 BGRA → 装入 `slot.thumb`
    /// （纯内存占位，不落盘）。非阻塞：仅检查已就绪通道；返回是否有新缩略图入槽。
    ///
    /// WARN-2（评审接受项）：无 recolor 世代守卫——切档前入队的旧色缩略可能短暂落槽闪
    /// 旧色；瞬态、随后被覆盖，可接受。
    pub(super) fn collect_thumb_results(&mut self) -> bool {
        let finished = collect_finished(&mut self.pending_thumb, |(_, rx)| rx.try_recv());
        let mut any = false;
        for (i, res) in finished {
            self.pending_thumb.remove(&i);
            match res {
                Some(res) => {
                    let (w, h) = (res.width, res.height);
                    if let Some(s) = self.slots.get_mut(i) {
                        s.thumb_loading = false;
                        if let Ok(img) =
                            crate::pdf::wrap_render_image(res.bgra, w, h, res.luma)
                        {
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
    ///
    /// WARN-2（评审接受项）：无 recolor 世代守卫——切档前入队的旧色中清可能短暂落槽闪
    /// 旧色；瞬态、随后被当前世代封/瓦片接管，可接受。
    pub(super) fn collect_mid_results(&mut self) -> bool {
        let finished = collect_finished(&mut self.pending_mid, |(_, rx)| rx.try_recv());
        let mut any = false;
        for (i, res) in finished {
            self.pending_mid.remove(&i);
            match res {
                Some(res) => {
                    let (w, h) = (res.width, res.height);
                    if let Some(s) = self.slots.get_mut(i) {
                        s.mid_loading = false;
                        if let Ok(img) =
                            crate::pdf::wrap_render_image(res.bgra, w, h, res.luma)
                        {
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

    /// 迟滞比较（F3 三速状态机）：进入快速态用 [`MOVE_THRESHOLD_PX`]，退出用其×
    /// [`MOVE_THRESHOLD_LOWER_RATIO`]（更小）；再上跳高速带（VeryFast）用 ×
    /// [`MOVE_THRESHOLD_VERY_RATIO`]（≈480px/帧），退出再乘退出比率（≈336px/帧）。
    /// 中间区间保持上一状态，避免临界速度下 LOD 降级/恢复反复横跳造成调度抖动。
    /// `speed_state` 为新真值，`prev_fast` = `is_fast()` 派生布尔（两者恒同步）。
    pub(super) fn update_fast_flag(&mut self) {
        let enter = MOVE_THRESHOLD_PX;
        let exit = MOVE_THRESHOLD_PX * MOVE_THRESHOLD_LOWER_RATIO;
        let very = MOVE_THRESHOLD_PX * MOVE_THRESHOLD_VERY_RATIO;
        let very_exit = very * MOVE_THRESHOLD_LOWER_RATIO;
        let v = self.scroll_velocity;
        self.speed_state = match self.speed_state {
            // 非快速带：近静止 → Idle；穿 enter → Fast；高速 flick（首帧即 >very）直接冲进
            // VeryFast（纯平移，不派新栅格——见 `ensure_window` 的高速带分支）。
            SpeedState::Idle | SpeedState::Slow => {
                if v > very {
                    SpeedState::VeryFast
                } else if v > enter {
                    SpeedState::Fast
                } else if v > 1.0 {
                    SpeedState::Slow
                } else {
                    SpeedState::Idle
                }
            }
            // 中速带内：向上穿 very → VeryFast；向下穿 exit → 慢速（保持进入阈值迟滞）。
            SpeedState::Fast => {
                if v > very {
                    SpeedState::VeryFast
                } else if v > exit {
                    SpeedState::Fast
                } else if v > 1.0 {
                    SpeedState::Slow
                } else {
                    SpeedState::Idle
                }
            }
            // 高速带内：退出用更小的 very_exit（防 480px/帧 附近横跳）；回落幅度大直接进 Fast。
            SpeedState::VeryFast => {
                if v > very_exit {
                    SpeedState::VeryFast
                } else if v > exit {
                    SpeedState::Fast
                } else if v > 1.0 {
                    SpeedState::Slow
                } else {
                    SpeedState::Idle
                }
            }
        };
        self.prev_fast = self.speed_state.is_fast();
    }

    // ---- L1 快速跨页跳：停稳快进 / 大跳检测 / 落点直达派发 / 世代失效 ----

    /// L1.3 **停稳快进**：`ensure_window` 每帧在 `update_fast_flag` 后调用。连续
    /// [`config::pdf::STOP_DETECT_FRAMES`] 帧 |Δy| < [`config::pdf::STOP_DETECT_PX`] → 速度**直落
    /// Slow**（绕过 0.7ⁿ EMA 衰减）——拖拽停稳瞬间不再等 ~150-250ms 的指数衰减，落点封面立刻
    /// 可被既有 Slow/Idle 调度（`schedule_tiles`）消费。需 **K 帧连续**约束：拖拽中单帧微停/
    /// 手指微抖不误触；任何 ≥2px 位移帧清零计数。只影响快速态（Fast/VeryFast）——本就 Slow/
    /// Idle 时直落无意义（且不破坏 E1/E2/E3 速度门：200/700px/帧 的 dy 恒 > 2px，计数恒 0）。
    pub(super) fn update_stop_detection(&mut self, dy: f32) {
        if dy < config::pdf::STOP_DETECT_PX {
            self.stop_small_frames = self.stop_small_frames.saturating_add(1);
            if self.stop_small_frames >= config::pdf::STOP_DETECT_FRAMES
                && self.speed_state.is_fast()
            {
                // 直落 Slow：绕过 0.7ⁿ 衰减，`schedule_tiles`（含封面）即刻接管落点。prev_fast
                // 派生同步为 false（`Slow.is_fast()==false`），泵/门控/文本预取等消费点同相。
                // C 实测（E 帧账）：跳变 end-to-end 已由自然 EMA 衰减在 2 帧内降入 Slow、无
                // 弹回（首 Slow f2 由 velocity < very_exit 自然触发，早于 K=3 stop 检测）——
                // 同帧归零 velocity 实测首 Slow 帧/VF 帧数零变化 ⇒ 无帧数收益，按「无收益即
                // 舍弃」已还原，仅保留 K=3 防误触停稳检测。
                self.speed_state = SpeedState::Slow;
                self.prev_fast = false;
            }
        } else {
            self.stop_small_frames = 0;
        }
    }

    /// L1.1 **大跳检测**（每帧在 `update_fast_flag` 后、调度前调用；需已算 `start..=end`）：
    /// 比较当前可见中心页与上帧可见中心页，位移超过 [`config::pdf::JUMP_PAGES_THRESHOLD`]
    /// （MARGIN 与走廊覆盖够不到的 ~6-8 页）→ 判定「大跳」：置 `jump_armed=true`、
    /// `jump_target=当前可见中心`，并 L2.4 推进 `nav_gen`（**每帧每 doc 至多一次**）+ 清掉
    /// 世代 < 新 n_gen 的全部在途条目（预算立即释放，解开 COVER_INFLIGHT 被旧在途占死）。
    /// 快速连续跳/变向时每帧以当前中心重算（后到覆盖先到，无积压队列）。速度离开快速带
    /// （VeryFast/Fast）后 `jump_armed` 由调用方随 `dispatch_jump_covers` 落点全到位清空；
    /// 本方法只负责「检测 + 武装 + 世代推进」，不动调度分派。
    pub(super) fn update_nav_jump(
        &mut self,
        start: usize,
        end: usize,
        page_width: f32,
        scroll_y: f32,
        viewport_h: f32,
        window: &Window,
    ) {
        let n = self.dims.len();
        if n == 0 {
            self.last_center_page = None;
            return;
        }
        let start = start.min(n - 1);
        let end = end.max(start).min(n - 1);
        // 当前可见中心页：优先取「真可见页 span」的算术中心（`keep_window` 中心含 MARGIN 缓冲，
        // 与本帧落点窗口径一致），span 为空（极端横溢/空视口）退化为 keep_window 中心。
        let center = match self.visible_page_span(start, end, scroll_y, viewport_h, page_width, window)
        {
            (Some(a), Some(b)) => (a + b) / 2,
            _ => (start + end) / 2,
        };
        let jumped = self
            .last_center_page
            .is_some_and(|prev| center.abs_diff(prev) > config::pdf::JUMP_PAGES_THRESHOLD);
        if jumped {
            self.jump_armed = true;
            self.jump_target = Some(center);
            // L2.4：nav_gen++ 当帧清掉世代 < 新 n_gen 的在途条目（预算立即释放）+ 通知 worker
            // 镜像（控制消息，sweep 当下应用；FIFO 保证早于本帧新渲染请求出队生效）。
            self.nav_gen = self.nav_gen.saturating_add(1);
            if let Some(w) = self.worker.as_ref() {
                w.navigate(self.doc_id, self.nav_gen);
            }
            self.clear_stale_pending();
        }
        self.last_center_page = Some(center);
    }

    /// L2.4 **预算即释**：清掉世代 < 当前 `nav_gen` 的全部在途条目（含封面/瓦片/中清/缩略/
    /// 走廊封面）——`nav_gen++` 当帧调用，旧在途条目即刻不再占 `COVER_INFLIGHT`/各额度，目标
    /// 封面能立刻被派发；到达结果若对应 pending 已不存在则丢弃（rx drop 后 worker 的
    /// `let _ = tx.send(...)` 静默失败）。只清世代标记：**不清** `cover_failures`/`cover_blacklist`
    /// （W2 语义：封面失败按 recolor 世代判定、黑名单随封面窗滚动复位，与导航世代正交）。
    fn clear_stale_pending(&mut self) {
        let n_gen = self.nav_gen;
        self.pending_cover.retain(|_, e| e.2 >= n_gen);
        self.pending_tiles.retain(|_, e| e.1 >= n_gen);
        self.pending_mid.retain(|_, e| e.0 >= n_gen);
        self.pending_thumb.retain(|_, e| e.0 >= n_gen);
        self.pending_prefetch_cover.retain(|_, e| e.1 >= n_gen);
    }

    /// L1.2 **VeryFast 落点直达派发**：`jump_armed` 时以 `jump_target`（落点中心）为窗、
    /// 距中心近远排序、≤[`COVER_INFLIGHT`] 满密度封面派发（复用既有 `schedule_covers` 预算，
    /// 不加新预算）——高速带不再纯平移零派发，停靠目标页封面立即在途。落点窗封面全部在槽/
    /// 在途后清零 `jump_armed`（后续由停稳后常态调度接管）；速度离开 VeryFast 时本方法不再被
    /// 调用，`jump_armed` 由调用方清零。
    ///
    /// ⚠️ 落点窗与 `schedule_tiles`/`schedule_fast_covers` 的封面窗同义（`[vis..=vis+dir前骤]`），
    /// **不带** `retire` 桥、也不把回退侧邻居拉进窗：大跳是一次性远距离平移（含 jump 前就在
    /// 窗内、被整体清掉的旧页），没有「方向翻位刚退位」的邻居要桥接——带上 retire 会把
    /// 落在窗缘外侧的孤立页（如向下跳时 `cf-1`）转存成桥，而该页不会被回翻复用 → 残留过渡桥
    /// 破坏 `cover_retire_bridge` 的「下滚稳态零残留」断言（回归实测命中）。
    pub(super) fn maybe_dispatch_jump_covers(
        &mut self,
        page_width: f32,
        scale_factor: f32,
    ) {
        if !self.jump_armed {
            return;
        }
        let Some(target) = self.jump_target else {
            self.jump_armed = false;
            return;
        };
        let n = self.dims.len();
        if n == 0 {
            self.jump_armed = false;
            return;
        }
        // 落点窗按滚动方向与 normal 封面窗同义（可见页 + 方向前瞻 1）。
        let (cf, cl) = match self.scroll_dir {
            1 => (target.min(n - 1), (target + 1).min(n - 1)),
            -1 => (target.saturating_sub(1), target.min(n - 1)),
            _ => (target.saturating_sub(1), (target + 1).min(n - 1)),
        };
        // A **两段式底图即显**：跳变突发先把落点整窗以 0.5 密度 RenderPreview 底图**一帧铺满**
        // （~1-6ms/张，替换「第 N 页 加载中…」占位）——目标页「有底图」从 ~11 帧（满密 cover
        // 30ms 串行渲染）压到 ~1-2 帧（low-cost preview 先行）；满密 cover→tile 精修交给停稳后
        // 既有常态调度（schedule_tiles），弹跳窗始终有底图桥接、绝不闪空白。
        //
        // ⚠️ **此处特意不派满密 cover**：`collect_prefetch_cover_results` 的守卫对「满密封面
        // 在途页」直接丢弃 preview（防与封面竞争显示）。若同时派 cover+preview，preview 会在
        // cover（30ms）仍排队时被弃 → 双渲全白、底图仍等 cover 30ms。二选一时选 preview：
        // 「更快有底图」是本线目标；精修由停稳调度照常完成。preview 入 `slot.prefetch_cover`
        // （独立槽），不作废/不覆盖任何满密底图，换档期间存续（L3.4 不变式同源守护）。
        //
        // 一次性突发（跳变当帧、非稳态节流），每页至多一次：突破 [`PRE_FETCH_COVER_INFLIGHT`]
        // =2/帧 的常态节流以整窗一帧铺底，但 0.5 密度字节仍由
        // [`PREFETCH_COVER_BUDGET_BYTES`]=40MB 的 evict 全局封顶（旁路照常 `evict_prefetch_covers`
        // 每日回收），不改变任何预算语义。
        self.dispatch_jump_preview_base(cf, cl, page_width, scale_factor);
        // 落点窗 preview 全部在槽/在途 → 清零 jump_armed（后续由停稳常态调度接管精修；
        // 已有满密 cover 也计入「已有底图」）。
        let all_covered = (cf..=cl).all(|p| {
            self.slots[p].cover.is_some()
                || self.slots[p].prefetch_cover.is_some()
                || self.pending_cover.contains_key(&p)
                || self.pending_prefetch_cover.contains_key(&p)
        });
        if all_covered {
            self.jump_armed = false;
            self.jump_target = None;
        }
    }

    /// A 两段式底图（第一段：低成本铺底）：对落点窗 `cf..=cl` 内**尚无任何底图**的页一次性
    /// 派发 0.5 密度 RenderPreview（1.6ms/张量级），一帧内把「加载中」占位替换为可辨识底图。
    /// 与 `prefetch_corridor` 同款请求（不入 raw 缓存、不参与重着色刷新、瞬态低清），只突破
    /// 常态 2/帧 节流（见调用方注释，受 40MB 预算 evict 兜底）。
    fn dispatch_jump_preview_base(
        &mut self,
        cf: usize,
        cl: usize,
        page_width: f32,
        scale_factor: f32,
    ) {
        let Some(worker) = self.worker.clone() else {
            return;
        };
        let n = self.dims.len();
        for page in cf..=cl.min(n.saturating_sub(1)) {
            if page >= n {
                continue;
            }
            let slot = &self.slots[page];
            // 已有任何底图（满密/廊/缩略/中清）或在途 → 跳过（不重复派发）。
            if slot.cover.is_some()
                || slot.prefetch_cover.is_some()
                || slot.thumb.is_some()
                || slot.mid.is_some()
            {
                continue;
            }
            // **P1 ④（同页去重）**：当前密度 1:1 瓦片在位的页已有 ≥0.5 供给，跳过 preview。
            if !slot.tiles.is_empty()
                && (slot.tile_scale - render_scale(page_width, scale_factor, self.dims[page].0))
                    .abs()
                    <= 1e-4
            {
                continue;
            }
            if self.pending_cover.contains_key(&page)
                || self.pending_prefetch_cover.contains_key(&page)
            {
                continue;
            }
            let dim = self.dims[page];
            let cscale = cover_scale(dim, page_width, scale_factor);
            let pscale = (cscale * config::pdf::PREFETCH_COVER_DENSITY)
                .min(config::pdf::COVER_SCALE_CAP);
            if !cover_affordable(dim, pscale) {
                continue;
            }
            let rx = worker.request_render_preview(
                self.doc_id,
                RenderRequest { page, scale: pscale },
                self.nav_gen, // L2.2 enqueue-gen（跳变后续世代推进可清 stale，只渲最新落点）
                self.zoom_epoch, // Z2：发送时打戳当前缩放世代
            );
            self.raster_reqs += 1;
            self.prefetch_reqs += 1; // F4 观测计数（跳变铺底也计走廊预取，预算/收敛语义一致）
            self.pending_prefetch_cover.insert(page, (pscale, self.nav_gen, rx));
        }
    }

    /// S2 **缩放防抖 preview**：`zoom_armed` 期间对**可见窗**按当前 cscale 派 ≤
    /// [`config::pdf::PRE_FETCH_COVER_INFLIGHT`] 枚 0.5 密度 RenderPreview（复用走廊
    /// `request_render_preview`：不入 raw 缓存、不参与重着色刷新、受 40MB 走廊预算 evict
    /// 兜底），每帧至多 2 枚、绝不派满密 cover/tile——杜绝「每档 cscale 变、满密 ~30ms
    /// 渲染大概率被下一档弃」的逐档抢渲空转。
    ///
    /// 与跳变 preview（`dispatch_jump_preview_base`）的区别：**按当前密度去重**——缩放后
    /// 旧封面由 `relocate_stale_zoomed_covers` 迁入 `prev_cover`（单档桥），显式底图为
    /// 「旧封面单档拉伸」为主（`page_base_image` 优先级 cover→prev_cover→preview），
    /// 本函数派出的当前档 0.5 密度 preview 补位仅对**无封面桥**的页生效，避免复利雾；
    /// 最终档 cover 到位后 `prev_cover.take()` 自动回收桥。跳变 preview 是一次性突发
    /// （每页至多一次），本函数是防抖窗内逐帧渐进预算。
    pub(super) fn maybe_dispatch_zoom_preview(
        &mut self,
        start: usize,
        end: usize,
        scroll_y: f32,
        viewport_h: f32,
        page_width: f32,
        scale_factor: f32,
        window: &Window,
    ) {
        if !self.zoom_armed {
            return;
        }
        let Some(worker) = self.worker.clone() else {
            return;
        };
        let (Some(vis_first), Some(vis_last)) =
            self.visible_page_span(start, end, scroll_y, viewport_h, page_width, window)
        else {
            return;
        };
        let n = self.dims.len();
        if n == 0 {
            return;
        }
        let vis_last = vis_last.min(n - 1);
        // 可见页按距中心近远排序（与封面/走廊同款），减去已有在途后的单帧预算。
        let mut budget = config::pdf::PRE_FETCH_COVER_INFLIGHT
            .saturating_sub(self.pending_prefetch_cover.len());
        if budget == 0 {
            return;
        }
        let center = (vis_first + vis_last) / 2;
        let mut order: Vec<usize> = (vis_first..=vis_last).collect();
        order.sort_by_key(|&p| (p as i64 - center as i64).unsigned_abs());
        for page in order {
            if budget == 0 {
                break;
            }
            let slot = &self.slots[page];
            let dim = self.dims[page];
            let cscale = cover_scale(dim, page_width, scale_factor);
            let pscale = (cscale * config::pdf::PREFETCH_COVER_DENSITY)
                .min(config::pdf::COVER_SCALE_CAP);
            // **手势期 preview 只填不换（dispatch 侧）**：`zoom_armed` 期间已有任何底
            // （cover/prev_cover/prefetch_cover）的页不再派新档 preview——这些页屏显由高密
            // cover/prev_cover（手势前那张）供给，新 preview 到也只会被链挡住/徒增逐档换密
            // churn；仅「全无底」的空页（真正 placeholder）才派一次垫底。与 collect 侧
            // `collect_prefetch_cover_results` 的 armed「有底不换」守卫对称（一面不派、一面
            // 不写，手势期 low-dens 底稳定）。
            if slot.cover.is_some() || slot.prev_cover.is_some() || slot.prefetch_cover.is_some() {
                continue;
            }
            // 当前密度守卫：满密 cover 已是当前档 → 页已锐利无需 preview；当前档 preview
            // 已在槽 / 任一在途 → 不重复派。旧密度 preview（`prefetch_cover_scale≠pscale`）
            // 不拦——缩放中途要按新档重派（顶显示从旧档抬升到当前档）。
            if slot.cover.is_some() && (slot.cover_scale - cscale).abs() <= 1e-4 {
                continue;
            }
            if slot.prefetch_cover.is_some()
                && (slot.prefetch_cover_scale - pscale).abs() <= 1e-4
            {
                continue;
            }
            // **P1 ④（同页去重）**：当前密度 1:1 瓦片已在位 → 已有 ≥0.5 供给，跳过 preview。
            if !slot.tiles.is_empty()
                && (slot.tile_scale - render_scale(page_width, scale_factor, dim.0)).abs() <= 1e-4
            {
                continue;
            }
            if self.pending_prefetch_cover.contains_key(&page)
                || self.pending_cover.contains_key(&page)
            {
                continue;
            }
            if !cover_affordable(dim, pscale) {
                continue;
            }
            let rx = worker.request_render_preview(
                self.doc_id,
                RenderRequest { page, scale: pscale },
                self.nav_gen, // L2.2 enqueue-n_gen（世代语义不变：防抖收口后下一档若推进可清 stale）
                self.zoom_epoch, // Z2：发送时打戳当前缩放世代
            );
            self.raster_reqs += 1;
            self.prefetch_reqs += 1; // F4 观测计数（缩放 preview 与走廊同计通道，预算/收敛语义一致）
            self.pending_prefetch_cover.insert(page, (pscale, self.nav_gen, rx));
            budget -= 1;
        }
    }

    /// S5 **高倍 cover 回收**：高倍稳态（`render_scale` 超出 [`config::pdf::COVER_SCALE_CAP`]，
    /// `cover_scale` 被 cap 截断在 4.0）下、可见页瓦片已按 `render_scale` 1:1 全到位
    /// （`tile_scale` 匹配当前密度且非空）——cover 只是 4.0 密度的旧密度整页图，在
    /// 显示 4.x~9.66 倍率下必然拉伸发糊，还叠加 32MB/页 的双驻留与边隙透雾 → 回收该页
    /// `cover`（显式 `drop_image` 下线）。瓦片画在 cover 之上且覆盖可见区，回收后显示不变。
    ///
    /// 动态 guard：`zoom_armed` 期间不回收（防手势中翻面回放时还需要 cover）；回收后若
    /// zoom 反向又需要 cover，由正常 `schedule_covers` 按新密度重派。cap 不升（4.0 恰顶穿
    /// A4 32MB 预算）。配合既有 `evict_tiles` 的「同密度封面冗余清瓦片」，正反向都不再
    /// 双驻留，`double_hold == 0` 内存回归恒成立。
    ///
    /// **评审 BLOCK③ 收窄**：判定从「`!tiles.is_empty()`（≥1 片在槽）」收紧为「该页**可见
    /// 矩形**内全部瓦片都在槽且 `tile_scale≈render_scale`」——否则高倍滚动/收口期首片到达
    /// 就回收 cover，会让未铺满可见区的页瞬间失去底图（闪底）。可见矩形用与瓦片调度同源的
    /// `page_visible_rect`+`visible_tile_range`（含 `TILE_MARGIN` 缓冲）对账，未铺满不回收。
    pub(super) fn retire_high_scale_covers(
        &mut self,
        start: usize,
        end: usize,
        scroll_y: f32,
        viewport_h: f32,
        page_width: f32,
        scale_factor: f32,
        window: &Window,
    ) {
        if self.zoom_armed {
            return;
        }
        let n = self.dims.len();
        if n == 0 || start > end {
            return;
        }
        let end = end.min(n - 1);
        let wrap_w = self.wrap_width(window, page_width);
        let mut drops: Vec<Arc<RenderImage>> = Vec::new();
        for page in start..=end {
            let dim = self.dims[page];
            let cur = render_scale(page_width, scale_factor, dim.0);
            let cscale = cover_scale(dim, page_width, scale_factor);
            if self.slots[page].cover.is_none() {
                continue;
            }
            // 阶段一（只读对账）：cover 密度被 cap 截断（cscale < cur）**且截断差明显**、
            // tile_scale 匹配当前密度、且**可见矩形内全部瓦片在槽**——三者齐备才可回收。
            if cscale < cur - 1e-4
                && cur > cscale * config::pdf::COVER_RETIRE_MIN_RATIO
                && (self.slots[page].tile_scale - cur).abs() <= 1e-4
            {
                let Some(rect) =
                    self.page_visible_rect(page, scroll_y, viewport_h, page_width, wrap_w)
                else {
                    continue;
                };
                let bmp = page_bitmap_size(dim, cur);
                let disp_h = display_height(dim, page_width);
                let Some((r0, c0, r1, c1)) = visible_tile_range_axis(
                    bmp,
                    page_width,
                    disp_h,
                    rect,
                    TILE_SIZE,
                    TILE_MARGIN,
                    config::pdf::TILE_MARGIN_COL,
                ) else {
                    continue;
                };
                let s = &self.slots[page];
                // 与 render 侧「原子换」判定共用同一几何（`tile_range_all_in_slot`）：
                // 可见矩形 + margin 每格都在槽 → 瓦片已整组覆盖可见区，cover 可安全回收。
                if !crate::pdf::layout::tile_range_all_in_slot(&s.tiles, r0, c0, r1, c1) {
                    continue;
                }
                // 阶段二（可回收才取 &mut）：离线 cover，置 cover_scale=0（render 侧判等失败、
                // 兼作「S5 已回收」标记，schedule_covers 跳过重派封面防回收-重派死环）。
                let slot = &mut self.slots[page];
                if let Some(old) = slot.cover.take() {
                    drops.push(old);
                    slot.cover_scale = 0.0;
                }
            }
        }
        self.pending_drops.extend(drops);
    }

    /// S2/S4 前置（评审 BLOCK②）：把可见窗内 `cover_scale ≠ 当前 cscale` 的 **stale cover**
    /// （缩放中途正持的旧档整页）迁入有界 `prev_cover` 槽（覆盖旧桥、旧桥下线），`cover=None`
    /// ——让 S2 preview（`collect_prefetch_cover_results` 不再是「cover 在位即弃」……
    /// 此迁移使其从 stale 转 cover=None 后 preview 必然入槽）与 S4 最新档桥对「持旧封面页」
    /// 同时激活；最终档封面由常态 `schedule_covers` 按新 cscale 重派。仅 `zoom_armed` 路径
    /// 调用（jump/滚动语义不受影响——它们走各自既有封面搬迁）。
    /// 返回被顶替下线的旧桥（调用方入 `pending_drops`）。
    pub(super) fn relocate_stale_zoomed_covers(
        &mut self,
        start: usize,
        end: usize,
        page_width: f32,
        scale_factor: f32,
    ) -> Vec<Arc<RenderImage>> {
        let n = self.dims.len();
        let mut dropped = Vec::new();
        if n == 0 || start > end {
            return dropped;
        }
        let end = end.min(n - 1);
        for page in start..=end {
            let cscale = cover_scale(self.dims[page], page_width, scale_factor);
            let slot = &mut self.slots[page];
            let stale =
                slot.cover.is_some() && (slot.cover_scale - cscale).abs() > 1e-4;
            if stale
                && let Some(old) = slot.cover.take()
            {
                if let Some(prev) = slot.prev_cover.replace(old) {
                    dropped.push(prev);
                }
                // cover_scale 保留下次 stale 判定所需的旧值？——置 0 表示无封面（render 侧
                // 不再按旧档判等显示；preview 已随时可见）。
                slot.cover_scale = 0.0;
            }
        }
        dropped
    }

    /// Z2 armed 收口前置：立即释放「缩放中间档」的在途预算——`pending_cover`/`pending_tiles`/
    /// `pending_prefetch_cover` 里 `pscale ≠ 当前 cscale` 的条目**直接移除**（rx drop：worker 侧
    /// 的 zoom-epoch 镜像会把它 drop 不渲，静默；预算即释、解开 [`COVER_INFLIGHT`]/瓦片在途被
    /// 中间档占死——P3「队列堆叠」在调度侧的直接释放）。`collect_*` 既有 scale 守卫会弃任何
    /// 漏网脏结果，无需改。中清/缩略无密度区分，不参与（低清底图缩放无关）。幂等，armed 每帧
    /// 调用安全。
    pub(super) fn drop_stale_scale_pending(&mut self, page_width: f32, scale_factor: f32) {
        // 在途封面：键=页，条目 (scale, recolor_epoch, nav_gen, rx)。**P1 ③**：密度判据与
        // `collect_cover_results`/`schedule_covers` 同源——满密度 **或** 邻居降档密度都视为有效
        // （否则邻居页 2.0 在途封面会在每个 armed 帧被误清 → 下次 settle 重派 → 逐档抢派计数
        // 虚增，`zoom_gesture_dispatches_only_preview` 回归命中）。
        self.pending_cover.retain(|page, v| {
            let s = v.0;
            *page < self.dims.len() && {
                let cscale = cover_scale(self.dims[*page], page_width, scale_factor);
                (s - cscale).abs() <= 1e-4
                    || (s - cscale.min(config::pdf::COVER_SCALE_CAP_NEIGHBOR)).abs() <= 1e-4
            }
        });
        // 在途瓦片：键=(页,row,col)，条目 (scale, nav_gen, rx)。
        self.pending_tiles.retain(|key, v| {
            let s = v.0;
            key.0 < self.dims.len()
                && (s - render_scale(page_width, scale_factor, self.dims[key.0].0)).abs() <= 1e-4
        });
        // 在途 preview（走廊）：键=页，条目 (pscale, nav_gen, rx)。
        self.pending_prefetch_cover.retain(|page, v| {
            let ps = v.0;
            *page < self.dims.len() && {
                let cscale = cover_scale(self.dims[*page], page_width, scale_factor);
                let want = (cscale * config::pdf::PREFETCH_COVER_DENSITY)
                    .min(config::pdf::COVER_SCALE_CAP);
                (ps - want).abs() <= 1e-3
            }
        });
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
