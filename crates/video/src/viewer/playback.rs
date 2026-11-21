use super::*;
impl super::VideoViewer {


    /// 播放泵单拍（纯状态推进 + 上屏；调用方负责重绘）。返回本拍是否把**新帧上屏**
    ///（调用方据此只在有新帧时强制重绘，闲置拍不再每 16ms 全刷）。`hit_end` 也要重绘
    ///（控制条播放/暂停图标、以及循环/随机模式切到下一支的加载占位都要反映出来）。
    pub(super) fn tick(&mut self, cx: &mut Context<Self>) -> bool {
        // 以音频时钟为播放进度（**仅当本媒体音频真在喂**；否则/未上线走墙钟——无声轨
        // 或切媒体后音频未就绪时画面也要前进，否则冻结）
        let audio_pos = (self.audio.is_some() && self.audio_ready.load(Ordering::Relaxed))
            .then(|| self.audio.as_ref().map(|s| s.position()).unwrap_or(0.0));
        let hit_end = self.advance_playhead(audio_pos);

        // 音频健康诊断：仅 `VUICK_VIDEO_AUDIO_DIAG=1` 时每 ~1s 打一行喂送统计
        //（欠载/锁竞争是滋滋声的根源形态；默认静默）
        const DIAG_ENV: &str = "VUICK_VIDEO_AUDIO_DIAG";
        if self.audio.is_some()
            && std::env::var_os(DIAG_ENV).is_some()
        {
            self.diag_ticks += 1;
            if self.diag_ticks >= 60 {
                self.diag_ticks = 0;
                if let Some(sink) = &self.audio {
                    // 共享 sink 统一 5 元 stats（dropped_samples：生产者侧队头丢弃，正常回压下恒 0）
                    let (calls, underrun, lockfail, played, dropped) = sink.stats();
                    eprintln!(
                        "[video-audio] calls={calls} underrun_samples={underrun} lockfail={lockfail} played_frames={played} dropped_samples={dropped}"
                    );
                }
            }
        }

        // 排空「pts ≤ playhead」的帧取最新上屏；越过 playhead 的下一帧保留在 overflow。
        // 暂停/拖拽冻结（playhead 停在落点、`advance_playhead` 短路）期间：首个「pts ≥ 落点」
        // 的帧（帧号量化常比落点微超前 ~1 帧）就是落点帧本身，必须收下；否则它会被当
        // 「逾线帧」塞回 overflow、`pending_seek` 永不清 → seek 冻结。软解靠「预滚保底帧
        //（≤ 落点）先行」掩盖此缺口；硬解 skip 只送 ≥ 落点帧，直接暴露（pause→hover→seek
        // 回归实测）。
        // **播放中 seek 同此处理**：`pending_seek` 活跃（= 有在途落点等待）时第首个
        // 「pts ≥ 落点」帧即落点帧，须即时收下——否则它被当逾线帧塞 overflow、要等墙钟/
        // 音钟再走完 ~1 帧量化（25fps→40ms、30fps→33ms）+ 泵节拍才上屏（真机实测
        // frame2screen 残余 44-50ms 静默，集中在同落点连点第二次点击——播放中重复
        // seek 同一 target，时钟每轮重新追赶量化尾差）。正常播放（无 in-flight seek）
        // `pending_seek` 为 None → land_seek 仍 None，行为逐字节不变。
        let mut land_seek = if self.scrubbing || !self.playing || self.pending_seek.is_some() {
            self.pending_seek
        } else {
            None
        };
        let mut latest: Option<FrameItem> = None;
        // P1-24：落点等待期的预滚帧暂存（pts<落点）——**不提前落屏**。软解 seek「预滚保底帧
        //（≤落点）」先行时，tick 若把它当 `latest` 上屏并**无条件**清 `pending_seek`，泵会因
        // `pump_wanted()`（playing=false + pending 空 + 非拖拽）停转，随后到达的落点帧滞留
        // 通道 → 暂停点击进度条停在落点前 1 帧永久冻结（worker 送帧与 pump 排空分属两拍时的
        // 竞态，真机 pause→seek 复现）。预滚帧只在此处暂存：落点帧到达即弃；仅当「命令确已
        // 结束」（帧通道发送端已弃 = EOF/解码错误，下无落点帧）才回退上屏防黑屏。
        let mut pre_roll: Option<FrameItem> = None;
        // 落点等待期「命令结束」信号：`rx.try_recv()` 返回 `Disconnected` = 当前 worker 命令
        // 的 `cmd.tx` 已 drop（命令以 EOF/错误/换代结束）。据此才能把「解码已完成、落点不可达」
        // 与「仍在途（`Empty`，落点帧随后才到）」区分开——否则无法判定何时该清 `pending_seek`。
        let mut cmd_done = false;
        loop {
            let item: Option<FrameItem> = if let Some(o) = self.overflow.take() {
                Some(o)
            } else {
                match &self.rx {
                    Some(rx) => match rx.try_recv() {
                        Ok(it) => Some(it),
                        Err(std::sync::mpsc::TryRecvError::Empty) => None,
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                            cmd_done = true;
                            None
                        }
                    },
                    None => None,
                }
            };
            if land_seek.is_some() {
                // 落点等待期：只认首个「≥ 落点」帧为落点帧（即时落地）；「< 落点」的预滚帧
                // 一律暂存不落屏（仅作 EOF 回退）。通道空且命令已结束 → 出循环走回退。
                match item {
                    Some(item) if item.pts() >= land_seek.unwrap() => {
                        land_seek = None;
                        // 落点已到：预滚暂存清空（GPU 帧共享槽须回收，不能裸 drop）
                        if let Some(prev) = pre_roll.take() {
                            self.drop_frame_item(prev);
                        }
                        if let Some(prev) = latest.replace(item) {
                            self.drop_frame_item(prev);
                        }
                    }
                    Some(item) => {
                        if let Some(prev) = pre_roll.replace(item) {
                            self.drop_frame_item(prev);
                        }
                    }
                    None => break,
                }
            } else {
                // 正常播放 / 无在途落点：原判据逐字节不变。
                match item {
                    Some(item) if item.pts() <= self.playhead => {
                        // 先到的帧被更新的帧顶掉：旧帧直接弃（软帧无资源；GPU 帧共享槽回池）
                        if let Some(prev) = latest.replace(item) {
                            self.drop_frame_item(prev);
                        }
                    }
                    Some(item) => {
                        self.overflow = Some(item);
                        break;
                    }
                    None => break,
                }
            }
        }
        let new_frame = if let Some(item) = latest {
            match item {
                FrameItem::Soft(f) => {
                    self.install_bgra(f.w, f.h, f.bgra, cx);
                    #[cfg(test)]
                    {
                        self.last_frame_pts = Some(f.pts);
                    }
                }
                FrameItem::Gpu(f) => {
                    #[cfg(test)]
                    let gpts = f.pts;
                    self.install_gpu(f, cx);
                    #[cfg(test)]
                    {
                        self.last_frame_pts = Some(gpts);
                    }
                }
            }
            self.pending_seek = None; // 落点已上屏（首个可用帧）
            true
        } else if cmd_done {
            // 命令已结束（EOF/解码错误）且始终无落点帧：回退最后预滚帧，不黑屏/不挂泵。
            // `cmd_done` 门禁保证只在「解码确已完成」时落预滚帧——在途（Empty）时不提前落屏。
            if let Some(item) = pre_roll.take() {
                match item {
                    FrameItem::Soft(f) => {
                        self.install_bgra(f.w, f.h, f.bgra, cx);
                        #[cfg(test)]
                        {
                            self.last_frame_pts = Some(f.pts);
                        }
                    }
                    FrameItem::Gpu(f) => {
                        #[cfg(test)]
                        let gpts = f.pts;
                        self.install_gpu(f, cx);
                        #[cfg(test)]
                        {
                            self.last_frame_pts = Some(gpts);
                        }
                    }
                }
                self.pending_seek = None; // 解码已完成、落点不可达：清 pending 防挂泵
                true
            } else {
                // 命令已结束却无任何可上屏帧（空流/首帧即错）：仍清 pending 防 pump 永久自续
                //（P1-24），画面保持最近一帧。
                self.pending_seek = None;
                false
            }
        } else {
            // 在途（落点帧未到）本拍结束：预滚暂存不落屏、回收 GPU 槽位（软帧 no-op）
            if let Some(item) = pre_roll.take() {
                self.drop_frame_item(item);
            }
            false
        };
        // seek 全链路探针聚合（`VUICK_VIDEO_SEEK_DIAG=1` 打印；test 构建无条件把
        // 8 字段快照沉淀给基准对表）：首帧上屏那刻打一条 [SEEK]。
        // total = seek 提交 → 首帧上屏；seek2first = worker 内 seek→首帧≥target 入队；
        // frame2screen = 入队 → tick install（泵一拍最坏等待）；打完清槽，不重复刷。
        if new_frame && seek_diag_collect() && self.diag_session != 0 {
            let info = {
                let slot = seek_diag_slot().lock().unwrap_or_else(|e| e.into_inner());
                slot.as_ref()
                    .filter(|d| d.session == self.diag_session && d.t_sent.is_some())
                    .map(|d| {
                        let now = Instant::now();
                        let total = now
                            .duration_since(d.trigger.unwrap_or(now))
                            .as_secs_f64()
                            * 1000.0;
                        let seek2first = d
                            .t_sent
                            .unwrap()
                            .duration_since(d.worker_start.unwrap_or(now))
                            .as_secs_f64()
                            * 1000.0;
                        let frame2screen =
                            now.duration_since(d.t_sent.unwrap()).as_secs_f64() * 1000.0;
                        (
                            total,
                            if d.reuse { 1u8 } else { 0u8 },
                            d.regress,
                            d.skipped,
                            seek2first,
                            frame2screen,
                            d.target,
                            d.firstpts,
                        )
                    })
            };
            if let Some((total, reuse, regress, skipped, s2f, f2s, target, firstpts)) = info {
                // test 构建：先沉淀 8 字段快照（bench 取走即清，对表 GUI [SEEK]）；运行态
                // 该语句块 cfg(test) 折叠为零。
                #[cfg(test)]
                {
                    SEEK_DIAG_TEST_ROW.with(|s| {
                        *s.borrow_mut() = Some(SeekDiagTestRow {
                            total,
                            reuse,
                            regress,
                            skipped,
                            seek2first: s2f,
                            frame2screen: f2s,
                            target,
                            firstpts,
                        })
                    });
                }
                if seek_diag_enabled() {
                    eprintln!(
                        "[SEEK] total={:.1}ms reuse={} regress={:+.2}s skipped={} seek2first={:.1}ms frame2screen={:.1}ms target={:.2}s firstpts={:.2}s",
                        total, reuse, regress, skipped, s2f, f2s, target, firstpts,
                    );
                    // [SEEKNODE] 各节点细分：submit=GUI 提交、wake=channel+线程唤醒、
                    // open=打开/复用、seek=reader.seek 调用、preroll=seek 后首帧≥target 解码、
                    // screen=入队→tick 上屏、total=点击→上屏（应与 [SEEK] total 吻合）。
                    // 缺少任一关键时刻（如旧路径未采集）则不打印，[SEEK] 聚合不受影响。
                    if let Some(d) = seek_diag_slot()
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .as_ref()
                        .filter(|d| d.session == self.diag_session)
                        && let (Some(tr), Some(s), Some(r), Some(op), Some(ws), Some(ts)) = (
                            d.trigger,
                            d.sent,
                            d.recv,
                            d.opened,
                            d.worker_start,
                            d.t_sent,
                        ) {
                            let dn = Instant::now();
                            let ms =
                                |a: Instant, b: Instant| b.duration_since(a).as_secs_f64() * 1000.0;
                            eprintln!(
                                "[SEEKNODE] submit={:.1}ms wake={:.1}ms open={:.1}ms seek={:.1}ms preroll={:.1}ms screen={:.1}ms total={:.1}ms reuse={} skipped={} target={:.2}s",
                                ms(tr, s),
                                ms(s, r),
                                ms(r, op),
                                ms(op, ws),
                                ms(ws, ts),
                                ms(ts, dn),
                                ms(tr, dn),
                                if d.reuse { 1u8 } else { 0u8 },
                                d.skipped,
                                d.target,
                            );
                        }
                    *seek_diag_slot().lock().unwrap_or_else(|e| e.into_inner()) = None;
                    self.diag_session = 0;
                }
            }
        }
        // 自然播完的自动去向（顺序 = 停住；循环/随机 = 切换）在排空之后执行：先让结尾帧
        // 尽量上屏，再切换下一支（open_folder_index 内部换代 + 起新解码）。
        if hit_end {
            self.handle_end(cx);
        }
        // 命中结尾也要强制重绘（顺序模式停住：控制条播放图标翻转；循环/随机：进入下一支）
        new_frame || hit_end
    }


    /// 推进 `playhead`：`audio_pos` 有值（= 音频线程已上线并喂音）→ 音频时钟
    /// `audio_base + 设备已播位置 × speed`，A/V 对齐；`None` → 墙钟（`dt × speed`）。
    /// 音频**刚上线那一刻锚定** `audio_base = 当前 playhead − 位置 × speed`：设备在开新
    /// 媒体的静音间隙里也累计 `position()`，不锚定会把 playhead 带飞（音画错位）。
    /// 暂停不推进；到结尾自动停（有音频时顺带暂停输出）。返回是否「这一拍撞到结尾」，
    /// 供 [`Self::tick`] 决定是否执行 [`Self::handle_end`] 的自动切换。
    pub(super) fn advance_playhead(&mut self, audio_pos: Option<f64>) -> bool {
        // 拖拽（scrub）预览：进度由手指指定，不走任何时钟——音频已在 `begin_scrub`
        // 暂停、位置冻结；音频时钟若照常算 `audio_base + 位置×speed` 会把刚暂停的
        // 落点继续推进 → playhead 越位拖走、画面更对不上。
        if self.scrubbing {
            self.last_tick = None;
            return false;
        }
        if !self.playing {
            self.last_tick = None;
            return false;
        }
        let speed = f64::from(self.speed);
        match audio_pos {
            Some(pos) => {
                if self.audio_anchor_pending {
                    self.audio_anchor_pending = false;
                    self.audio_base = self.playhead - pos * speed;
                }
                self.playhead = self.audio_base + pos * speed;
            }
            None => {
                let now = Instant::now();
                let dt = self
                    .last_tick
                    .map(|t| now.duration_since(t).as_secs_f64())
                    .unwrap_or_else(|| 1.0 / 60.0);
                self.last_tick = Some(now);
                self.playhead += dt * speed;
            }
        }
        if self.duration > 0.0 && self.playhead >= self.duration {
            self.playhead = self.duration;
            self.playing = false; // 播到结尾自动停
            if let Some(sink) = &self.audio {
                sink.pause();
            }
            true
        } else {
            false
        }
    }


    /// 「自然播完」后的下一支决策（随机模式会推进 PRNG，故收 `&mut self`）：
    /// - `None` = 停住不切换（顺序播放，保持历史「播完自动停」行为）；
    /// - `Some(idx)` = 切到 `folder[idx]`；`idx == current`（单文件循环/随机）时由
    ///   [`Self::handle_end`] 走 seek 0 重播而非重开同一文件（保住缩放/平移）。
    pub(super) fn natural_end_target(&mut self) -> Option<usize> {
        let n = self.nav.len();
        if n <= 1 {
            return match self.playback_mode {
                PlaybackMode::Sequential => None,
                _ => Some(self.nav.current()),
            };
        }
        match self.playback_mode {
            PlaybackMode::Sequential => None,
            PlaybackMode::Loop => Some((self.nav.current() + 1) % n),
            PlaybackMode::Shuffle => Some(self.pick_shuffle_index()),
        }
    }


    /// 随机挑一个「≠ current」的下标（调用前须 `folder.len() ≥ 2`）：xorshift64* 自推进，
    /// `(x % n)` 撞上当前则顺移一位——结果恒在 `[0, n)` 且不等于 `current`；`rng_state`
    /// 可注入定值做确定性测试。
    pub(super) fn pick_shuffle_index(&mut self) -> usize {
        let n = self.nav.len();
        debug_assert!(n >= 2, "随机播放仅在多文件清单下调用");
        let mut x = self.rng_state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng_state = x;
        let mut idx = (x % n as u64) as usize;
        if idx == self.nav.current() {
            idx = (idx + 1) % n;
        }
        idx
    }


    /// 自然播完的处理：按 [`PlaybackMode`] 切换到下一支 / 重播自身 / 保持暂停。
    pub(super) fn handle_end(&mut self, cx: &mut Context<Self>) {
        match self.natural_end_target() {
            // 顺序播放：停在结尾（既有行为）
            None => {}
            // 单文件循环/随机：seek 0 重播（保留缩放/平移，不重开文件）
            Some(idx) if idx == self.nav.current() => {
                self.seek(-self.duration, cx);
                // F1：与 `toggle_play` 结尾重播分支对齐——`seek` 本身不恢复 `playing`
                //（seek 时保持暂停态、不提前出声），而 `advance_playhead` 撞结尾时已把
                // `playing` 置 false；此处不补回 `playing=true` 的话，单文件播完会永远
                // 停在 0s 暂停（`sink.play()` 又被 `if self.playing` 门控）而**不再自动
                // 重播**。多文件循环/随机走 `open_folder_index`（内部置 playing=true）不受影响。
                self.playing = true;
                if let Some(sink) = &self.audio {
                    let _ = sink.play();
                }
            }
            Some(idx) => self.open_folder_index(idx, cx),
        }
    }


    /// 播放模式三态循环切换（控制条按钮）。
    pub(super) fn cycle_playback_mode(&mut self, cx: &mut Context<Self>) {
        self.playback_mode = self.playback_mode.next();
        cx.notify();
    }


    /// 解码偏好循环切换（控制条「解码偏好」按钮）：自动（硬解优先）↔ 强制软解。变更后
    /// 同步写进程级旋钮 [`crate::decode::set_decode_mode`]（worker 下一条命令按新模式选
    /// Auto/Soft）并用 `start_decoder_at(playhead, None, false)` 重开当前 reader @当前播放
    /// 位置续播——与设备丢失自恢复同款「重开 + seek(resume_pts)」机制：模式是 worker 常驻
    /// reader 的复用键一部分（`path + cap + 模式`），模式一变即不复用、必然重开；不动音频
    /// （`restart_audio=false`）→ 播放进度不中断、不从头跳。尚无活跃 reader（loading / 无帧/
    /// 在途 seek）时只改偏好，下次打开即生效。
    pub(super) fn cycle_decode_pref(&mut self, cx: &mut Context<Self>) {
        self.decode_pref = if self.decode_pref == 0 { 1 } else { 0 };
        crate::decode::set_decode_mode(self.decode_pref);
        // 活跃 reader：重开 @当前播放位置（软/硬解都以 seek(playhead) 接续，不跳帧不重头）
        if self.frame.is_some() && !self.state.loading && self.pending_seek.is_none() {
            let t = self.playhead;
            self.start_decoder_at(t, None, false);
        }
        cx.notify();
    }


    /// 右上角瞬时状态浮层：显示 `text`，[`STATUS_MS`] 后自动清除（带代序号防旧定时器
    /// 误清新浮层，同 `show_controls_temporarily` 姿势）。
    pub(super) fn show_status(&mut self, text: String, cx: &mut Context<Self>) {
        self.status = Some((text, Instant::now()));
        cx.notify();
        self.status_seq = self.status_seq.wrapping_add(1);
        let seq = self.status_seq;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(STATUS_MS))
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.status_seq == seq {
                    this.status = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }


    /// 关闭当前媒体、打开 `nav[current]`（同文件夹切换共用）。
    pub(super) fn open_folder_index(&mut self, idx: usize, cx: &mut Context<Self>) {
        if self.nav.set_current(idx).is_none() {
            return;
        }
        self.path = self.nav.item(idx).expect("set_current 已校验越界");
        // 切文件：旧媒体最后一帧的图集 tile 与 GPU 直通共享槽一并释放（与 install_bgra 同款
        // P0 姿势，否则切换 Playlist 多支时每切一次漏一片 tile / 槽位）
        self.swap_display(cx);
        self.intrinsic = None;
        self.state.set_loading();
        self.playing = true;
        self.playhead = 0.0;
        self.duration = 1.0;
        self.fps = None;
        self.last_tick = None;
        self.clear_overflow();
        self.pending_seek = None;
        self.zoom = 1.0; // 切文件：缩放复位为适应视口
        self.pan = (0.0, 0.0); // 平移同步归零（拖拽中的惯性也清掉）
        self.drag = pan_zoom::PanGesture::default();
        // 切文件：立即清空旧字幕（新字幕由 `start` 后台加载覆盖；避免 switch 窗口期旧
        // 字幕在时间 0 附近残留显示）
        self.subtitles.clear();
        // —— 切换窗口期（`start` 的异步任务先 await scan_folder、再跑 start_decoder_at
        // 之前）立即切断旧媒体的时钟与输入：否则 pump 一拍可能仍按旧音频时钟把
        // playhead 带回旧位置、或把旧媒体残留帧重新上屏，而新媒体的音频 worker 已
        // 独立开喂 → 「画面暂停但声音还在放」（间歇出现）。
        self.audio_ready.store(false, Ordering::Relaxed);
        self.audio_anchor_pending = true;
        self.audio_base = 0.0;
        // 换代 + 换全新空接收端：pump 无法再取到旧媒体的未消费帧（worker 的旧命令
        // 会因 generation 不匹配立即退出，随后由 start_decoder_at 的新命令接手）。
        self.generation.fetch_add(1, Ordering::SeqCst);
        // M0 失效源（换文件）：悬停位置/缓存/在途预解码全作废（新文件的落点需重新解码）；
        // `hover_generation` 同步递增，旧文件 worker 回填结果一律丢弃。F1 悬停浮层同失效。
        self.hovered_target = None;
        self.hover_x = None;
        self.hover_cache = None;
        self.served_hover_target = None;
        self.hover_generation.fetch_add(1, Ordering::SeqCst);
        let (_tx, rx) = sync_channel::<FrameItem>(DECODE_AHEAD_FRAMES);
        // 换文件同款：旧接收端缓存里的 GPU 直通帧槽位须归还（见 `drop_replaced_rx_frames`，
        // 不能随 `drop(old)` 静默丢句柄）
        if let Some(old) = self.rx.replace(rx) {
            self.drop_replaced_rx_frames(old);
        }
        self.start(cx);
        // 切视频：控制条浮现（锁定模式默认可见随后自隐；普通模式常驻）
        self.show_controls_temporarily(cx);
        cx.notify();
    }


    pub(super) fn switch_file(&mut self, dir: i32, cx: &mut Context<Self>) {
        // 事件入口只传 ±1（PageDown/PageUp、Shift+滚轮）；环绕开启，next/prev 即循环步进。
        let idx = if dir > 0 {
            self.nav.next()
        } else {
            self.nav.prev()
        };
        let Some(idx) = idx else {
            return;
        };
        self.open_folder_index(idx, cx);
    }


    pub(super) fn toggle_play(&mut self, cx: &mut Context<Self>) {
        // 停在结尾（顺序模式自然播完：`playhead` 已 == `duration`）时点播放 → 立即 seek 0
        // 重播（YouTube/mpv/VLC 标准行为）。若只翻 `playing` 不 seek，下一拍
        // `advance_playhead` 会再次撞上结尾判定把 playhead 钉回结尾、`handle_end` 又停住，
        // 点播放看起来无效。—— seek 走既有路径（playhead 归零 + 解码换代 + 重开音轨）。
        if !self.playing && self.duration > 0.0 && self.playhead >= self.duration {
            self.seek(-self.playhead, cx);
            self.playing = true; // seek 时仍为暂停态（不提前出声），置播放后统一恢复
        } else {
        self.playing = !self.playing;
    }
    if self.playing {
            self.last_tick = None; // 恢复播放不随暂停跳变
            self.ensure_pump(cx);
            if let Some(sink) = &self.audio {
                let _ = sink.play();
            }
        } else if let Some(sink) = &self.audio {
            sink.pause();
        }
        cx.notify();
    }


    pub(super) fn seek(&mut self, delta: f64, cx: &mut Context<Self>) {
        // 任何「提交式」seek（键盘步进等）都退出拖拽态：拖拽期 playhead 由手指指定，
        // 一旦走完整换代 + 重开音频的提交路径就必须还时钟给它，否则永远冻结。
        self.scrubbing = false;
        let target = (self.playhead + delta).clamp(0.0, self.duration.max(0.0));
            self.playhead = target;
            // seek 重锚墙钟基准（同 toggle_play 恢复播放的姿势）：音频时钟期间 `last_tick`
            // 不更新，seek 重开音频的窗口走墙钟时若沿用旧基准，第一拍 `dt = now - last_tick`
            // 会把点击前累积的时长一口算进 playhead → 视频突然前跳数秒/加速。
            self.last_tick = None;
            self.start_decoder_at(target, None, true);
        // seek 后如果仍在播放则恢复出声（pause 态保持静音、只预览画面）
        if self.playing
            && let Some(sink) = &self.audio
        {
            let _ = sink.play();
        }
        self.ensure_pump(cx);
        cx.notify();
    }


    /// 进度条/音量条共用：分数 → 秒数 seek（进度条比例 × 总时长）。
    /// 进度条把 `fraction` 夹到 [0, 1] 再换算；本方法对绝对值 cur 也做钳位。
    /// 生产路径（点按/拖拽）已改走 [`Self::begin_scrub`]/[`Self::update_scrub`]/
    /// [`Self::end_scrub`]（拖拽不逐 move 换代）；本方法仅测试直接驱动用。
    #[cfg(test)]
    pub(super) fn seek_to_fraction(&mut self, fraction: f32, cx: &mut Context<Self>) {
        let target = f64::from(fraction.clamp(0.0, 1.0)) * self.duration.max(0.0);
        self.seek(target - self.playhead, cx);
    }


    /// 分数 → 拖拽落点秒数（钳位到 [0, duration]；与 [`Self::seek_to_fraction`] 同一换算）。
    pub(super) fn clamped_scrub_target(&self, fraction: f32) -> f64 {
        f64::from(fraction.clamp(0.0, 1.0)) * self.duration.max(0.0)
    }


    /// M0：记录进度条悬停位置；空闲（暂停且不在拖拽 / 不等待 seek 落点）时向独立后台 worker
    /// 投一条「该落点单帧预解码」。机会式：拖拽/seek 期主 worker 正忙、落点随时变，播放期
    /// hover 解码在 FFMPEG_LOCK 上会跟 playback worker 抢带宽 → stutter，均跳过并保留上一份
    /// 缓存（「不能无干扰地跑就跳过」）；悬停位置仍记录，点击照常可走正常 seek 兜底。
    pub(super) fn update_hover_target(&mut self, target: f64, cx: &mut Context<Self>) {
        self.hovered_target = Some(target);
        if self.scrubbing || self.pending_seek.is_some() || self.playing {
            return;
        }
        // 已缓存「离 target 足够近」的帧（命中窗内点击即可直接上屏）→ 不重复投（省解码带宽）
        if let Some(f) = &self.hover_cache
            && f.pts >= target && f.pts - target <= HOVER_HIT_WINDOW_S {
                return;
            }
        self.kick_hover_predecode(target, cx);
    }


    /// M0：向常驻 hover 预解码 worker 投一条单帧命令；结果异步回填 [`Self::hover_cache`]。
    /// 换代（新落点 / 换文件 / 真实 seek / cap 变化）由 [`Self::hover_generation`] 把关：
    /// 过期结果不落地。**主线程不碰任何解码 FFI**——只有独立 worker 线程解，UI 零阻塞。
    pub(super) fn kick_hover_predecode(&mut self, target: f64, cx: &mut Context<Self>) {
        self.ensure_hover_worker();
        let hgen = self.hover_generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.hover_generation.store(hgen, Ordering::SeqCst);
        // 新目标 → 旧缓存作废（悬停移到新位置后，旧帧不该在点击时误用；worker 回填时按代序覆盖）
        self.hover_cache = None;
        let (ftx, frx) = std::sync::mpsc::channel::<Result<HoverFrame, String>>();
        let cmd = HoverCmd {
            generation: hgen,
            path: self.path.clone(),
            target,
            cap: self.decode_cap,
            tx: ftx,
        };
        if let Some(tx) = &self.hover_cmd {
            let _ = tx.send(cmd);
        }
        let path_check = self.path.clone();
        let cap_check = self.decode_cap;
        // P1-25：代际检查随任务克隆一份；在途计数 +1（被取代/结束/超时会 -1，防堆积锚）
        let gen_check = self.hover_generation.clone();
        let inflight = self.hover_inflight.clone();
        inflight.fetch_add(1, Ordering::SeqCst);
        cx.spawn(async move |weak, cx| {
            // 后台线程等 worker 单次回传（真解码耗时由 worker 占，不在主线程）。P1-25：
            // 旧实现裸 `recv_timeout(15s)`——被新落点/换代取代的命令 worker 不再回传 → 每个
            // 废弃任务白等整 15s 堆在后台池。改为「≤200ms 代际轮询窗」：被取代即提前退出；
            // 15s 仍无结果按超时处理（点击走正常 seek 兜底）。
            let outcome = cx.background_executor().spawn(async move {
                const POLL: std::time::Duration = std::time::Duration::from_millis(200);
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
                let result = loop {
                    if gen_check.load(Ordering::SeqCst) != hgen {
                        break None; // 已被更新悬停/换文件/seek 取代：放弃（防过期任务堆积）
                    }
                    let now = std::time::Instant::now();
                    if deadline <= now {
                        break None; // 15s 超时
                    }
                    let slice = std::cmp::min(POLL, deadline - now);
                    match frx.recv_timeout(slice) {
                        Ok(res) => break Some(res),
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break None,
                    }
                };
                // 在途计数扣减放内层（后台线程）统一完成：等待结束（结果/超时/被取代/断连）即
                // -1，主线程直接读 Atomic 即可观测——不依赖「后台完成 → 唤醒主协程」的跨线程
                // 唤醒（gpui 测试 fake 调度器拒绝外来唤醒，主协程 resumption 不可靠；K14 同族）。
                inflight.fetch_sub(1, Ordering::SeqCst);
                result
            })
            .await;
            let _ = weak.update(cx, |this, cx| {
                // 只落地「仍是悬停的那个目标、仍在同文件同 cap」的最新代：过期/换文件丢弃
                if this.hover_generation.load(Ordering::SeqCst) != hgen
                    || this.path != path_check
                    || this.decode_cap != cap_check
                {
                    return;
                }
                if let Some(Ok(frame)) = outcome {
                    this.hover_cache = Some(frame);
                    cx.notify();
                }
                // 失败/超时/换代放弃：保持无缓存，点击走正常 seek 兜底
            });
        })
        .detach();
    }


    /// M0：悬停缓存命中判定 + 消费一次。仅当缓存帧 `pts ≥ 落点`（不展示早于点击位置的帧）且
    /// `pts − 落点 ≤ [HOVER_HIT_WINDOW_S]`（防陈旧/过冲误用）时命中：取走缓存帧整帧装屏（亚毫秒
    /// 即现，不再换代解码），返回 true；否则不动缓存、返回 false（调用方走正常 seek 路径）。
    pub(super) fn take_hover_cache_if_hit(&mut self, target: f64, cx: &mut Context<Self>) -> bool {
        let Some(hf) = &self.hover_cache else {
            return false;
        };
        if hf.pts >= target && hf.pts - target <= HOVER_HIT_WINDOW_S {
            let hf = self.hover_cache.take().expect("已判 Some");
            self.install_bgra(hf.w, hf.h, hf.bgra, cx);
            #[cfg(test)]
            {
                self.last_frame_pts = Some(hf.pts);
            }
            self.pending_seek = None; // 没有在途解码等待：帧已即时上屏
            true
        } else {
            false
        }
    }


    /// 拖拽开始（进度条按下/k_begin）：锁定预览态——`playhead` 固定到落点、音频
    /// 临时静音（flush 清缓存 + pause 停输出）、视频立即向新落点重定位。**不重开**
    /// 音频线程：若随每次 move 都 `start_audio_at`，声音会反复从当点重放/越位（持续
    /// 出声），且 HE-AAC(SBR) 每次选中流 mid-frame 首包都会解码报 `env_facs_q` 坏帧。
    pub(super) fn begin_scrub(&mut self, fraction: f32, cx: &mut Context<Self>) {
        self.scrubbing = true;
        self.hover_x = None; // F1：开始拖拽 → 悬停时间浮层隐藏
        self.scrub_target = self.clamped_scrub_target(fraction);
        self.playhead = self.scrub_target;
        self.last_tick = None;
        if let Some(sink) = &self.audio {
            sink.flush(); // 清掉暂停前已入队的旧段（即便 pause 未即时生效也无声）
            sink.pause();
        }
        self.last_scrub_retarget = Some(Instant::now());
        // M0：悬停预解码帧命中此落点 → 即时装屏（亚毫秒）+ 跳过本次换代重解（`served_hover_target`
        // 记下已服务落点，供 end_scrub_to 收尾识别——用户拖走后不匹配即落回正常 seek）；未命中
        // 保持原行为逐字不变（一次换代投解码）。
        self.served_hover_target = if self.take_hover_cache_if_hit(self.scrub_target, cx) {
            Some(self.scrub_target)
        } else {
            None
        };
        if self.served_hover_target.is_none() {
            self.start_decoder_at(self.scrub_target, None, false);
        }
        self.ensure_pump(cx);
        cx.notify();
    }


    /// 拖拽移动（进度条按住拖动）：只更新落点（`playhead` 跟手指）；距上一次真实
    /// 重定位不足 [`SCRUB_RETARGET_MS`] 就不再换代——否则密集 move 的中间命令被
    /// generation 入队即覆盖，worker 来不及解首帧 → 画面全拖全程冻结。
    pub(super) fn update_scrub(&mut self, fraction: f32, cx: &mut Context<Self>) {
        let target = self.clamped_scrub_target(fraction);
        self.scrub_target = target;
        self.playhead = target;
        self.last_tick = None;
        let due = match self.last_scrub_retarget {
            Some(t) => t.elapsed() >= Duration::from_millis(SCRUB_RETARGET_MS),
            None => true,
        };
        if due {
            self.last_scrub_retarget = Some(Instant::now());
            self.start_decoder_at(target, None, false);
        }
        cx.notify();
    }


    /// 拖拽结束（进度条松开）：按最终落点走一次完整提交 seek（换代 + 重开音频；
    /// 播放中恢复出声、暂停态保持静音预览）。音频 seek 从「每次 move 一次」降到
    /// 「每次拖拽一次」，AAC(SBR) 坏帧不再高频触发。
    pub(super) fn end_scrub(&mut self, fraction: f32, cx: &mut Context<Self>) {
        let target = self.clamped_scrub_target(fraction);
        self.end_scrub_to(target, cx);
    }


    /// 拖拽结束到当前预览落点（鼠标在窗外松开、`fraction_at` 拿不到位置时兜底）。
    pub(super) fn end_scrub_at_current(&mut self, cx: &mut Context<Self>) {
        self.end_scrub_to(self.scrub_target, cx);
    }


    pub(super) fn end_scrub_to(&mut self, target: f64, cx: &mut Context<Self>) {
        if !self.scrubbing {
            return; // 幂等：`on_mouse_up` 与 `on_mouse_up_out` 可能都触发
        }
        self.scrubbing = false;
        let target = target.clamp(0.0, self.duration.max(0.0));
        self.scrub_target = target;
        // M0：begin 已用 hover 缓存帧直接上屏（未投解码，`pending_seek` 恒 None）且松手仍在
        // 同一落点 → 不再对同落点重新 seek，走「音频-only 收尾」（与下方单击合并收尾同姿势）。
        if self.served_hover_target == Some(target) && (self.playhead - target).abs() < 1e-9 {
            self.served_hover_target = None;
            self.last_tick = None; // 同 seek 的墙钟重锚：音频时钟期不更新 last_tick
            if self.audio.is_some() {
                self.start_audio_at(target); // 音频 worker 复用 reader：只 seek + 喂送
                if self.playing
                    && let Some(sink) = &self.audio
                {
                    let _ = sink.play();
                }
            }
            self.ensure_pump(cx); // 通知性重绘；无在途解码可等
            cx.notify();
            return;
        }
        // 单击合并（一次换代）：点按 down→up 未拖动且未落点时，`begin_scrub` 那代
        // 同 target 的预览解码仍在飞（`pending_seek == Some(target)`）= 视频 worker 已
        // 在解这一段、pump 只等它出帧上屏——此时松手**不必换代重解第二遍**（现状每次
        // 单击 = begin 一遍 + end 一遍同 target 全量 pre-roll 解码，显示等的是第二遍，
        // 长 GOP 中段 1.1~1.8s 延迟、CPU 翻倍）。本分支只补音频收尾，视频就地沿用
        // begin 那代，上屏从点按起算一遍解码。
        if self.pending_seek == Some(target) && (self.playhead - target).abs() < 1e-9 {
            self.last_tick = None; // 同 seek 的墙钟重锚：音频时钟期不更新 last_tick
            if self.audio.is_some() {
                self.start_audio_at(target); // 音频 worker 复用 reader：只 seek + 喂送
                if self.playing
                    && let Some(sink) = &self.audio
                {
                    let _ = sink.play();
                }
            }
            self.ensure_pump(cx); // pending_seek 仍 Some(target)：泵持续到 begin 代帧上屏
            cx.notify();
            return;
        }
        self.seek(target - self.playhead, cx);
    }


    /// 生效音量：静音 → 0，否则 `volume`（只改一条路径，供 `AudioSink::set_volume`）。
    pub(super) fn effective_volume(&self) -> f32 {
        if self.muted {
            0.0
        } else {
            self.volume
        }
    }


    /// 把当前生效音量推给音频输出（打开/调节/静音切换后调用；无音频时空操作）。
    pub(super) fn apply_volume(&mut self) {
        if let Some(sink) = &self.audio {
            sink.set_volume(self.effective_volume());
        }
    }


    /// 设置音量（钳位 0..=1，不改静音态；静音时仅记录，恢复后生效）。
    pub(super) fn set_volume(&mut self, volume: f32, cx: &mut Context<Self>) {
        self.volume = volume.clamp(0.0, 1.0);
        self.apply_volume();
        cx.notify();
    }


    /// 静音切换：生效音量 0 ⇄ 原音量。
    pub(super) fn toggle_mute(&mut self, cx: &mut Context<Self>) {
        self.muted = !self.muted;
        self.apply_volume();
        cx.notify();
    }


    /// 字幕开关切换。
    pub(super) fn toggle_subtitle(&mut self, cx: &mut Context<Self>) {
        self.subtitle_visible = !self.subtitle_visible;
        cx.notify();
    }


    /// 设置倍速：音频时钟进程不变（`audio_base` 保持当前进度），只换重采样速率
    /// 重启音频线程；视频帧按源秒时间轴照旧排空，A/V 同速。纯画面（无音频）只需
    /// 更新 `speed`（墙钟按 `dt×speed`）。
    pub(super) fn set_speed(&mut self, speed: f32, cx: &mut Context<Self>) {
        self.speed = speed.clamp(0.25, 4.0);
        if self.audio.is_some() {
            self.start_audio_at(self.playhead);
        }
        cx.notify();
    }


    /// 倍速档位循环步进（`dir` = +1 下一档 / -1 上一档）。
    pub(super) fn speed_step(&mut self, dir: i32, cx: &mut Context<Self>) {
        let n = SPEED_STEPS.len() as i64;
        let idx = SPEED_STEPS
            .iter()
            .position(|&s| (s - self.speed).abs() < 1e-6)
            .unwrap_or_else(|| SPEED_STEPS.iter().position(|&s| (s - 1.0).abs() < 1e-6).unwrap_or(1));
        let next = (idx as i64 + i64::from(dir)).rem_euclid(n) as usize;
        self.set_speed(SPEED_STEPS[next], cx);
    }
}
