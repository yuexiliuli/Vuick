use super::*;
use super::render::audio_read_rate;
impl super::VideoViewer {


    /// 后台解析参数 + 扫描同文件夹 → 启动解码线程与 pump。
    /// `VideoReader` 含裸指针（非 Send）：只在该后台任务内取参数，不把实体带回主线程。
    pub(super) fn start(&mut self, cx: &mut Context<Self>) {
        let path = self.path.clone();
        cx.spawn(async move |weak, cx| {
            // 视频运行库缺失（主 exe 不再静态导入 FFmpeg，缺库也能启动）：不触碰任何
            // 解码 FFI，直接在窗口内报错提示。
            if !crate::video::decode::available() {
                let _ = weak.update(cx, |this, cx| {
                    this.loading = false;
                    this.load_error = Some(
                        "缺少视频运行库（media_ffmpeg.dll 或 avcodec-63.dll / avformat-63.dll 等 FFmpeg 运行库），视频功能不可用。请把它们放到程序 exe 同目录后重新打开。"
                            .to_string(),
                    );
                    this.stop_pump_on_failure();
                    cx.notify();
                });
                return;
            }
            let scan_path = path.clone();
            let nav = cx
                .background_executor()
                .spawn(async move { FolderNav::scan(&scan_path, crate::video::is_video_extension) })
                .await;
            // 不单独预开 VideoReader 取参数（实测同一文件稍后被 worker 再 open 会
            // 卡死在 avformat_open_input）——改为给首个命令挂 outcome 通道，由 worker
            // 开成功回送参数 / 失败回送错误，本任务异步等结果再补齐时长/尺寸/音轨。
            let (out_tx, out_rx) = std::sync::mpsc::channel::<Result<super::decode::VideoParams, String>>();
            let _ = weak.update(cx, |this, cx| {
                this.nav = nav;
                this.start_decoder_at(0.0, Some(out_tx), true);
                this.ensure_pump(cx);
            });
            let outcome = cx
                .background_executor()
                .spawn(async move {
                    out_rx.recv_timeout(std::time::Duration::from_secs(8))
                })
                .await;
            match outcome {
                Ok(Ok(p)) => {
                    let _ = weak.update(cx, |this, cx| {
                        this.loading = false;
                        this.duration = p.duration.max(0.001);
                        this.fps = p.frame_rate;
                        this.intrinsic.get_or_insert((p.width as f32, p.height as f32));
                        if p.has_audio {
                            this.ensure_audio();
                        }
                        if let Some(sink) = &this.audio {
                            let _ = sink.play();
                        }
                        // 加载字幕（后台线程）
                        cx.spawn(async move |weak, cx| {
                            let load_path = path.clone();
                            let subtitles = cx.background_executor().spawn(async move {
                                super::decode::SubtitleReader::open(&load_path)
                                    .map(|r| {
                                        let mut subs = r.subtitles().to_vec();
                                        // 字幕文本一次性清洗（原先每帧 render 热路径重跑
                                        // `parse_ass_text`：每帧分配 String + 2×replace）：加载完
                                        // 即摊平为纯文本，render 直接消费 `entry.text`——渲染输出
                                        // 逐字节一致（`parse_ass_text` 幂等，双洗同输出）。
                                        for e in &mut subs {
                                            e.text = parse_ass_text(&e.text);
                                        }
                                        subs
                                    })
                                    .unwrap_or_else(|_| Vec::new())
                            }).await;
                            let _ = weak.update(cx, |this, cx| {
                                // 防陈旧覆盖：后台加载期间若已切到别的文件（`path` 不符），
                                // 丢弃这次结果；字幕随当前文件走，不残留上一支。
                                if this.path != path {
                                    return;
                                }
                                this.subtitles = subtitles;
                                cx.notify();
                            });
                        }).detach();
                        cx.notify();
                    });
                }
                Ok(Err(e)) => {
                    let _ = weak.update(cx, |this, cx| {
                        this.loading = false;
                        this.load_error = Some(e);
                        this.stop_pump_on_failure();
                        cx.notify();
                    });
                }
                Err(_) => {
                    // 超时：至少把 loading 关掉（画面静态，不卡佔位）
                    let _ = weak.update(cx, |this, cx| {
                        this.loading = false;
                        this.stop_pump_on_failure();
                        cx.notify();
                    });
                }
            }
        })
        .detach();
    }


    /// 换接收端/换文件时回收旧接收端里仍缓存着的帧：GPU 直通帧的共享槽经
    /// `drop_frame_item` 发 release 归还 worker 池；软 BGRA 帧为纯字节 no-op。
    /// 不能直接 `drop(old)` —— 拖动重定向/切文件时旧端里可能缓存着 ≤
    /// `DECODE_AHEAD_FRAMES` 个已拷入共享槽的 GPU 帧（槽 `in_use=true`），随通道析构
    /// 静默丢弃 = 槽泄漏，累积到槽池耗尽 → Backpressure → 丢帧定格（真机拖进度条偶发
    /// 停顿的根因）。
    pub(super) fn drop_replaced_rx_frames(&mut self, old: std::sync::mpsc::Receiver<FrameItem>) {
        while let Ok(item) = old.try_recv() {
            self.drop_frame_item(item);
        }
    }


    /// 启动（或重开）解码，从 `target` 秒开始。向**常驻视频 worker** 投新命令换代：
    /// 旧命令内层循环按 `generation` 立即停，worker 在**同一线程**内 drop 旧 reader 后
    /// 串行重开新文件——实测规避“新线程在上一解码器 teardown 后 avformat_open_input 卡死”
    ///（72.MP4 复现；同线程串行重开稳定：见 diag6）。
    /// `restart_audio`：seek/切换为 true（音频随命令重开、flush 归零）；纯「按显示尺寸
    /// 重开视频解码器」为 false（音频保持、播放进度不中断）。
    pub(super) fn start_decoder_at(
        &mut self,
        target: f64,
        result_tx: Option<std::sync::mpsc::Sender<Result<super::decode::VideoParams, String>>>,
        restart_audio: bool,
    ) {
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.generation.store(generation, Ordering::SeqCst);
        // M0 失效源（全部经此入口）：任一真实 seek（点按/拖拽/键盘/切文件/cap 重开）都让悬停
        // 缓存与在途预解码作废——缓存只服务「静止悬停」，换代即不再可信；`hover_generation` 递增
        // 使 worker 回填的过期结果被丢弃。缓存命中路径（begin_scrub 的 take_hover_cache_if_hit）
        // 不调用本方法，故不会把它刚装屏的帧误清。
        self.hover_cache = None;
        self.served_hover_target = None;
        self.hover_generation.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = sync_channel::<FrameItem>(DECODE_AHEAD_FRAMES);
        // 换掉旧接收端 → worker 若正阻塞在 send 会收到失联错误、本命令内层循环退出；
        // 旧端里可能缓存着已拷入共享槽的 GPU 帧 → drop_replaced_rx_frames 逐条归还
        //（不能随接收端 drop 静默丢句柄，否则槽泄漏累积到 16 耗尽 → 丢帧定格）
        self.clear_overflow();
        if let Some(old) = self.rx.replace(rx) {
            self.drop_replaced_rx_frames(old);
        }
        self.pending_seek = Some(target);
        // seek 全链路探针：会话号递增；仅「seek 型」命令（result_tx 空）开槽记触发时刻，
        // 换媒体（挂握手通道）不算 seek、不打印。普通运行只多一次 u64 写。
        let is_seek = result_tx.is_none();
        let seek_session = SEEK_DIAG_SESSION.fetch_add(1, Ordering::Relaxed) + 1;
        self.diag_session = seek_session;
        if seek_diag_collect() && is_seek {
            *seek_diag_slot().lock().unwrap_or_else(|e| e.into_inner()) = Some(SeekDiag {
                session: seek_session,
                target,
                trigger: Some(Instant::now()),
                ..Default::default()
            });
        }
        self.ensure_video_worker();
        let cmd = DecodeCmd {
            generation,
            path: self.path.clone(),
            target,
            cap: self.decode_cap,
            tx,
            result_tx,
            seek_session,
            scrubbing: self.scrubbing,
            wake: self
                .frame_wake
                .as_ref()
                .map(|w| w.tx.clone())
                .expect("帧就绪通知通道构造即建"),
        };
        if let Some(tx) = &self.decode_cmd {
            // 探针：先记「投稿完成时刻」再 send——worker 首帧写槽会整块替换 SeekDiag，
            // 若 send 后才补写，竞争窗口内 worker 替换会把它记成 None、[SEEKNODE] 丢行。
            if seek_diag_collect() && is_seek {
                if let Some(d) = seek_diag_slot()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_mut()
                    .filter(|d| d.session == seek_session)
                {
                    d.sent = Some(Instant::now());
                }
            }
            let _ = tx.send(cmd);
        }
        if restart_audio {
            self.start_audio_at(target);
        }
    }


    /// 惰性启动常驻视频解码 worker（一生一个；viewer Drop 时命令发送端被弃，
    /// worker 的 `recv()` 返回 Err 即退出）。
    pub(super) fn ensure_video_worker(&mut self) {
        if self.decode_cmd.is_some() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel::<DecodeCmd>();
        let gen_arc = self.generation.clone();
        // GPU 槽归还通道：主线程释放「已换下的 GPU 帧共享槽」→ worker 回收（见
        // `video_worker` 的 `reap_releases`；Arc<Mutex<Receiver>> 供内层帧循环逐帧拉取）
        let (rel_tx, rel_rx) = std::sync::mpsc::channel::<usize>();
        let rel_rx = Arc::new(std::sync::Mutex::new(rel_rx));
        std::thread::spawn(move || Self::video_worker(rx, gen_arc, rel_rx));
        self.decode_cmd = Some(tx);
        self.release_cmd = Some(rel_tx);
    }


    /// 惰性启动常驻 hover 预解码 worker（一生一个；`hover_cmd` Sender 随 viewer Drop 弃、
    /// worker `recv()` 返回 Err 即退；与 [`Self::video_worker`] 完全独立——独立线程 +
    /// 独立 `VideoReader`，不会被播放/seek 换代影响）。
    pub(super) fn ensure_hover_worker(&mut self) {
        if self.hover_cmd.is_some() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel::<HoverCmd>();
        let hgen = self.hover_generation.clone();
        std::thread::spawn(move || Self::hover_worker(rx, hgen));
        self.hover_cmd = Some(tx);
    }


    /// 常驻视频 worker 主循环：逐条执行命令（打开/复用 reader → seek → 解码推帧，
    /// 直到换代/EOF）。
    ///
    /// 打开路径一律按进程级解码偏好旋钮（[`crate::decode::decode_mode`]，生产恒 auto）：
    /// auto/hw → **双设备 GPU 直通优先**（[`Self::open_reader_for_mode`]：worker 自有
    /// D3D11 设备 + `open_hw`，失败自动回落 SAFE BGRA 硬解 `open_scaled_hw`，再回软解）；
    /// soft → `open_scaled`（纯软解）。双设备不注入渲染线程的 immediate context（旧 M2e
    /// 崩溃路径已废除——帧经 shared handle + keyed mutex 跨设备移动）。
    pub(super) fn video_worker(
        rx: std::sync::mpsc::Receiver<DecodeCmd>,
        gen_arc: Arc<AtomicU64>,
        rel_rx: Arc<std::sync::Mutex<std::sync::mpsc::Receiver<usize>>>,
    ) {
        // 常驻 reader：同路径 + 同模式（auto 或 soft cap）的命令直接复用——seek 只做容器内
        // 定位（av_seek_frame + 解码器 flush），不重开文件；重开（avformat open + 解码器/sws
        // 初始化）在一次 seek 命令里可能几十 ms，是「点击进度条后画面要等片刻」的主因
        //（探针实测 open_scaled≈57ms、复用 seek≈0ms）。换文件 / cap 变化（窗口缩放）/
        // 模式切换才重开。同一线程内串行使用，无并发；reader 非 Send，仅在本 worker
        // 线程持有。第 4 元 = 双设备共享槽拷贝引擎（`Some` ⇔ 直通已接管；`None` ⇔ SAFE
        // BGRA/软解——与 reader 同生共死，换代/重开一并 drop 释放）。
        let mut active: Option<(std::path::PathBuf, ReaderMode, VideoReader, Option<SharedCopier>)> = None;
        // 常驻帧缓冲（worker 线程独占，软解 / 硬解 seek 预滚共用）：跨命令存活，零初始化
        // 只在扩容发生一次（按源顶格 `source_cap`），随后逐帧覆写复用。
        let mut buf: Vec<u8> = Vec::new();
        while let Ok(cmd) = rx.recv() {
            // 模式：auto/hw → `open_scaled_hw`（内核按环境硬解优先/软回退）；soft → `open_scaled`
            let mode = if crate::decode::decode_mode() == 1 {
                ReaderMode::Soft(cmd.cap)
            } else {
                ReaderMode::Auto(cmd.cap)
            };
            // 探针：seek 全链路细化（env 开或 test 构建采集，打印仍只按 env；媒体切换命令
            // 不采集）。`t_recv` = 命令到 worker 的处理开始时刻（[SEEKNODE]「wake」终点）。
            let seek_diag_on = seek_diag_collect() && cmd.result_tx.is_none();
            let t_recv = if seek_diag_on { Some(Instant::now()) } else { None };
            let same = matches!(&active, Some((p, m, _, _)) if *p == cmd.path && *m == mode);
            if !same {
                // 打开（按模式；双设备/SAFE/软解任一成功即用，全失败 → 报错跳过本命令）
                match Self::open_reader_for_mode(&cmd.path, &mode) {
                    Ok((opened_mode, reader, copier)) => {
                        if let Some(out) = &cmd.result_tx {
                            let _ = out.send(Ok(reader.params()));
                        }
                        active = Some((cmd.path.clone(), opened_mode, reader, copier));
                    }
                    Err(e) => {
                        if let Some(out) = &cmd.result_tx {
                            let _ = out.send(Err(e.to_string()));
                        }
                        continue;
                    }
                }
            } else if let Some(out) = &cmd.result_tx {
                // 复用分支同样完成「打开成功」握手：同文件+同模式（含同 cap）的重复加载
                //（快速切回刚在解码的同一支）不重开文件，但参数直接取常驻 reader——同路径
                // 同模式下参数必然不变。否则 `start` 的 result 通道等满 8s 超时、时长/帧率/
                // 尺寸不填（loading 只关参数缺失），切换窗口期对撞时即触发。
                let _ = out.send(Ok(active.as_ref().expect("复用分支已有 reader").2.params()));
            }
            // 探针：打开/复用完成、`reader.seek` 之前时刻——[SEEKNODE] 把「open」（含打开/复用）
            // 与「seek 调用」（reader.seek，软解/双设备才走；SAFE 硬解靠 skip_to_bgra 内部完成）拆开
            let t_opened = if seek_diag_on { Some(Instant::now()) } else { None };
            // 直通（worker 自有设备 open_hw）是否接管：`active[3]` Some ⇔ 双设备已接管
            let mut dual = matches!(&active, Some((_, _, _, Some(_))));
            // 硬解 reader 判别：SAFE BGRA（FFmpeg 自有设备）与双设备（worker 自有设备）
            // 都是 is_hw()==true；软解为 false。驱动 seek 预滚分支与「设备丢失计数」。
            let mut hw_reader = active
                .as_ref()
                .map(|(_, _, r, _)| r.is_hw())
                .unwrap_or(false);
            // 拖拽快扫：预览命令只解关键帧（Video Decode 引擎每轮从「关键帧→落点整段
            // P/B」降到「1 个 I 帧」）；松手/停稳后的精确命令恢复全帧。必在首次 seek 前
            // 重设（复用 reader 也按命令切换，seek 紧跟即 flush）。
            if let Some((_, _, r, _)) = active.as_mut() {
                let _ = r.set_skip_nonkey(cmd.scrubbing);
            }
            if !hw_reader || dual {
                // 软解照旧：seek 后才逐帧过滤预滚。双设备：skip_to_bgra 不做（零回读），
                // 需显式 seek 目标，预滚帧在帧循环里免拷贝丢弃。
                let _ = active.as_mut().map(|(_, _, r, _)| r.seek(cmd.target));
            }
            // 源顶格容量：内层循环扩容判据（省逐帧取参）。**重开后可变**——双设备直通 reader
            //（`open_hw`）无 scaler、`source_cap()==1`（HW 帧按源尺寸、cap 忽略）；硬解/软解
            // reader 有 scaler、cap 是真实输出字节数。若命令中途「设备丢失/格式不支持 → 重开软解」，
            // 必须在此按新 reader 刷新，否则 buf 停在旧（dual 的 1）容量 → 软解 `_into` 报
            // 「解码输出缓冲过小」→ 首帧解码失败、本命令断代无画面。
            let mut src_cap = active
                .as_ref()
                .map(|(_, _, r, _)| r.source_cap())
                .unwrap_or(1);
            // seek 全链路探针（`seek_diag_on` 已在命令开头定）：收集「回退量 / 预滚数 /
            // 首帧≥target 入队时刻」，首帧入队瞬间写槽（此时解码量已定——预滚帧全在它之前），
            // tick 首帧上屏时聚合打印一次。
            let seek_worker_start = if seek_diag_on { Some(Instant::now()) } else { None };
            let mut seek_first_decoded = true;
            let mut seek_regress: f64 = 0.0;
            let mut seek_skipped: u32 = 0;
            let mut seek_first_sent: Option<Instant> = None;
            // 泵竞态探针（env 门控）：首帧 wake 投递结果只打一次
            let mut seek_wake_dbg_printed = false;
            // 硬解首帧是否已由 skip_to_bgra 产出（软解恒 false → 走旧逐帧预滚过滤）
            let mut hw_primed = false;
            // —— 设备丢失自恢复（本命令内状态）——
            // 帧接纳阈值（pts ≥ 此值才送）：初始 = seek 落点；设备丢失重开软解后抬到「最后
            // 成功解码 pts」，保证续播从该位置起、不把更早的预滚帧当新内容送出去。
            let mut frame_target = cmd.target;
            // 硬解 reader 连续解码/预滚失败计数（成功即清零；EOF 不计入）
            let mut hw_fail_count: u32 = 0;
            // 最近一次成功解码的帧 pts（重开软解时的续播位置；一帧未解过则 = seek 落点）
            let mut resume_pts = cmd.target;
            // seek 预滚过滤：ffmpeg seek 回拉到目标前关键帧（media-ffmpeg decode.rs 的
            // seek 文档注明「调用方须丢弃 pts < 目标的帧」）；不丢的话，keyframe→落点
            // 整段帧会被主线程排空当「迟到帧」快速连放 = seek 后视频加速/一闪而过。
            // 规则：落点前的预滚帧只保留最新一帧（≈落点前一帧，保证排空先有帧可上屏、
            // seek 到片尾有末帧回退），其余丢弃；越过落点的帧照常全送。
            let mut pre_roll: Option<FrameItem> = None;
            // VPROF=1 拆账：解码（`_into` 覆写复用缓冲 + 帧产物 to_vec memcpy / 硬解
            // 内核回读+sws）与推帧背压（tx.send，bounded 通道满=主线程没在及时消费）的每帧
            // 墙钟，每 ~2s 打一行忙核。
            let vp_on = vprof_enabled();
            let mut vp_decode = Duration::ZERO;
            let mut vp_send = Duration::ZERO;
            let mut vp_frames: u64 = 0;
            let mut vp_window = Instant::now();
            // —— 拷贝失败分类（≠ 一律背压）参数：`SharedCopier::copy` 的失败按 [`CopyError`]
            // 分类——背压（槽池瞬满）重试同一帧 + 间隙 reap 吸收；格式不支持（P010 等）/ 键控
            // 互斥或资源创建设备侧故障 → 按「设备丢失」走黑名单 + 软重开恢复（不能永久无画面）。
            const COPY_RETRY_BUDGET: usize = 128;
            // 重试墙钟预算上限：最坏 ≈ 1×AcquireSync（上限 2000ms 的超时） + 128×~1ms ≈ ~2.2s；
            // 正常「槽瞬满立即失败」时 ≈ 128×~1ms ≈ 130ms。预算在**单次成功时清零**，按突发计
            //（不跨突发累计，避免一次爆发后连跳后续帧）。
            const COPY_RETRY_WALL_CAP: Duration = Duration::from_millis(300);
            // 本命令累计 copy 背压/重试尝试次数（成功清零；超预算跳过/降级诊断用；**不**参与 hw_fail_count）
            let mut copy_backpressure_tries: usize = 0;
            // 本命令「纯背压超预算跳过本帧」诊断只打一次（真机 GUI 卡顿可从日志看到，不刷屏）
            let mut backpressure_skip_printed = false;
            while gen_arc.load(Ordering::SeqCst) == cmd.generation {
                // 回收主线程归还的 GPU 共享槽（逐帧拉取：连续播放时主线程每消费一帧归还一槽，
                // 不及时 reap 会让 slot 池在 decode-ahead 窗口内堆积耗尽）
                Self::reap_releases(&mut active, &rel_rx);
                let t0 = Instant::now();
                // 设备丢失/瞬时错误注入（仅测试构建）：硬解 reader 下一次解码/预滚前查一次
                let fault_now = {
                    #[cfg(test)]
                    {
                        hw_reader && hw_fault_hook()
                    }
                    #[cfg(not(test))]
                    {
                        false
                    }
                };
                let item = {
                    if fault_now {
                        Err(anyhow::anyhow!("测试注入：模拟硬解设备丢失/解码失败"))
                    } else if dual {
                        // ---- 双设备 GPU 直通（零回读）：worker 自有设备逐帧解码 → GPU 侧
                        // 拷进共享槽（AcquireSync→Copy→Flush→ReleaseSync）→ 送主线程 ----
                        // 预滚帧（pts<target）免拷贝丢弃（不占槽/不占跨设备带宽）；首个 ≥target
                        // 帧拷贝后立即 release 解码器槽位（帧池 headroom≈1-2）。
                        // 一次解构取 reader 解一帧（块退出即释放借用，拷贝重试间隙可整借 active reap）。
                        let next = {
                            let reader = &mut active.as_mut().expect("打开成功必有 reader").2;
                            reader.next_frame_hw()
                        };
                        match next {
                            Ok(Some(fr)) if fr.pts >= frame_target => {
                                let pts = fr.pts;
                                let w = fr.w;
                                let h = fr.h;
                                // ---- 拷贝失败按 [`CopyError`] 分类的重试 / 降级 ----
                                // · Backpressure（槽池瞬满）：重试**同一帧**（`fr` 仍持解码器槽位、
                                //   不推进解码）+ 间隙 reap 主线程归还的槽 → 渲染端追上进度；预算
                                //   封顶后跳过本帧（保留最近上屏帧），**永不**进 hw_fail_count。
                                // · Sync / Resource（键控互斥超时 / 驱动器创建失败）：同预算内重试
                                //   （瞬时滞后可吸收）；预算耗尽仍未出帧 ≈ 疑似渲染端/设备死亡 → 按
                                //   真实失败计数（连续 2 条命令 → 黑名单 + 软重开恢复，防永久无画面）。
                                // · Unsupported（P010 等格式）：不进重试，立即按「该文件不支持直通」
                                //   的真实失败计数 → 连续 2 帧即黑名单 + 软重开恢复。
                                let t_copy_start = Instant::now();
                                let copied = {
                                    // 本突发是否见到过 Sync/Resource 类硬失败（预算耗尽时据此
                                    // 判定「计数/降级」而非「纯背压跳过」）
                                    let mut hard_err: Option<CopyError> = None;
                                    loop {
                                        let once = active
                                            .as_mut()
                                            .expect("打开成功必有 reader")
                                            .3
                                            .as_mut()
                                            .expect("直通接管必有共享拷贝引擎")
                                            .copy(w, h, fr.tex, fr.array_index);
                                        match once {
                                            Ok(cf) => {
                                                // 成功：本命令背压预算清零（按突发计，不跨突发累计）
                                                copy_backpressure_tries = 0;
                                                break Ok(Some(cf));
                                            }
                                            Err(e) => match e {
                                                CopyError::Unsupported => {
                                                    // 该文件格式不走 GPU 直通：立即按真实失败处理
                                                    //（hw_fail_count → 黑名单 + 软重开恢复）
                                                    break Err(e);
                                                }
                                                CopyError::Backpressure => {
                                                    copy_backpressure_tries += 1;
                                                    if copy_backpressure_tries >= COPY_RETRY_BUDGET
                                                        || gen_arc.load(Ordering::SeqCst)
                                                            != cmd.generation
                                                        || t_copy_start.elapsed()
                                                            >= COPY_RETRY_WALL_CAP
                                                    {
                                                        break Ok(None);
                                                    }
                                                    // 回收主线程刚归还的空闲槽 → 给渲染端追进度的窗口
                                                    Self::reap_releases(&mut active, &rel_rx);
                                                    std::thread::sleep(
                                                        std::time::Duration::from_millis(1),
                                                    );
                                                }
                                                CopyError::Sync | CopyError::Resource => {
                                                    copy_backpressure_tries += 1;
                                                    hard_err.get_or_insert(e);
                                                    if gen_arc.load(Ordering::SeqCst)
                                                        != cmd.generation
                                                    {
                                                        // 换代：本帧已过期/被接收端弃用，不是设备
                                                        // 死亡 → 与纯背压换代同口径不计数（镜像
                                                        // Backpressure 分支的 `break Ok(None)`）
                                                        break Ok(None);
                                                    }
                                                    if copy_backpressure_tries >= COPY_RETRY_BUDGET
                                                        || t_copy_start.elapsed()
                                                            >= COPY_RETRY_WALL_CAP
                                                    {
                                                        // 预算/墙钟耗尽仍见 Sync/Resource：疑似设备
                                                        // 死亡 → 计数（≥2 条命令即黑名单 + 软重开恢复）
                                                        break Err(hard_err.unwrap_or(e));
                                                    }
                                                    Self::reap_releases(&mut active, &rel_rx);
                                                    std::thread::sleep(
                                                        std::time::Duration::from_millis(1),
                                                    );
                                                }
                                            },
                                        }
                                    }
                                };
                                // 无论拷贝成败，本帧解码器槽位**恰好归还一次**（headroom 纪律）
                                VideoReader::release_hw_frame(fr.frame_ptr);
                                match copied {
                                    Ok(Some(cf)) => Ok(Some(FrameItem::Gpu(GpuFrame {
                                        pts,
                                        w,
                                        h,
                                        shared_handle: cf.shared_handle,
                                    }))),
                                    Ok(None) => {
                                        // 纯背压超预算：跳过本帧续解（显示保最近帧）；诊断每命令
                                        // 一次——真机 GUI 卡顿能在日志里看到；不计数、不黑名单、不重开
                                        if !backpressure_skip_printed {
                                            backpressure_skip_printed = true;
                                            eprintln!(
                                                "[passthrough] slot 回压，重试后仍超预算，跳过本帧（n={}）",
                                                copy_backpressure_tries
                                            );
                                        }
                                        continue;
                                    }
                                    Err(e) => {
                                        // Unsupported / Sync / Resource：真实失败 → 进入
                                        // `hw_fail_count`「设备丢失」记账（≥2 → 黑名单 + 软重开）
                                        Err(anyhow::anyhow!("GPU 直通拷贝失败（{e:?}），按设备丢失处理"))
                                    }
                                }
                            }
                            Ok(Some(fr)) => {
                                // 预滚帧（pts<target）：免拷贝丢弃；计入 seek 诊断
                                //（回归量取首个解码帧、跳帧数累加）
                                if seek_diag_on {
                                    if seek_first_decoded {
                                        seek_first_decoded = false;
                                        seek_regress = fr.pts - cmd.target;
                                    }
                                    seek_skipped += 1;
                                }
                                resume_pts = resume_pts.max(fr.pts);
                                VideoReader::release_hw_frame(fr.frame_ptr);
                                pre_roll = None;
                                continue;
                            }
                            Ok(None) => Ok(None),
                            Err(e) => Err(e),
                        }
                    } else if hw_reader && !hw_primed {
                        hw_primed = true;
                        // ---- 硬解 seek 预滚快路径：一次 `skip_to_bgra_into` 完成 seek + 跳帧
                        //（免转换/免回读）+ 首帧 BGRA —— seek 墙钟主要加速 ----
                        let reader = &mut active.as_mut().expect("打开成功必有 reader").2;
                        if buf.len() < src_cap {
                            buf.resize(src_cap, 0);
                        }
                        match reader.skip_to_bgra_into(frame_target, &mut buf[..]) {
                            Ok(Some((pts, w, h, skipped))) => {
                                // skip 计数由内核给出（旧预滚逐帧累加，口径同为「pts<target 已跳帧」；
                                // 首帧 pts≥target 由内核保证）
                                if seek_diag_on {
                                    seek_skipped = skipped.min(u32::MAX as u64) as u32;
                                }
                                let need = (w as usize) * (h as usize) * 4;
                                Ok(Some(FrameItem::Soft(SoftFrame {
                                    pts,
                                    w,
                                    h,
                                    bgra: buf[..need].to_vec(),
                                })))
                            }
                            Ok(None) => Ok(None),
                            Err(e) => Err(e),
                        }
                    } else {
                        // ---- 软解 / 硬解稳态逐帧路径（与旧版逐帧同口径）----
                        // 扩容目标：始终按源顶格（src_cap）兜底——任何解码输出 ≤ 源尺寸，
                        // 缓冲永不小于未知的当前帧 need，`_into` 的 slice 上界恒安全。零初始化
                        // 只在扩容瞬间发生一次，逐帧不重置。
                        let reader = &mut active.as_mut().expect("打开成功必有 reader").2;
                        if buf.len() < src_cap {
                            buf.resize(src_cap, 0);
                        }
                        // 复用持久缓冲 `_into`（零初始化只在扩容时发生一次）；帧产物
                        // `buf[..need].to_vec()`（一次 memcpy、无逐帧 memset）计入 decode 窗——
                        // 与旧版 `next_frame_bgra` 的「内部 vec![0;N]+sws 写入」口径对齐，VPROF
                        // 才可比。
                        match reader.next_frame_bgra_into(&mut buf[..]) {
                            Ok(Some((pts, w, h))) => {
                                let need = (w as usize) * (h as usize) * 4;
                                // 不做 `truncate`：它只缩短 len、不释放容量（顶格内存照样驻留），
                                // 反而把下次 `_into` 的上界从恒安全的 src_cap 缩成上一帧 need——
                                // 若解码输出尺寸变化（未来/其它源）会拿到过小的 slice（cdylib 报错
                                // 断代）。保持 len=src_cap，need 只用于本帧 to_vec 切片。
                                Ok(Some(FrameItem::Soft(SoftFrame {
                                    pts,
                                    w,
                                    h,
                                    bgra: buf[..need].to_vec(),
                                })))
                            }
                            Ok(None) => Ok(None),
                            Err(e) => Err(e),
                        }
                    }
                };
                let t1 = Instant::now();
                // 回退量：第一个被解码的帧 pts − target（负值越大 = 从越早的关键帧预滚解起）
                if seek_diag_on {
                    if let Ok(Some(frame)) = &item {
                        if seek_first_decoded {
                            seek_first_decoded = false;
                            seek_regress = frame.pts() - cmd.target;
                        }
                    }
                }
                // —— 设备丢失/瞬时错误记账（仅硬解 reader；软解零分支零开销）——
                // 成功帧 → 清失败计数 + 续播位置前移；错误帧 → 连续失败计数 +1（EOF 不算）。
                let mut item_err = false;
                if hw_reader {
                    match &item {
                        Ok(Some(f)) => {
                            hw_fail_count = 0;
                            resume_pts = resume_pts.max(f.pts());
                        }
                        Ok(None) => {}
                        Err(_) => {
                            item_err = true;
                            hw_fail_count += 1;
                        }
                    }
                }
                // 拖拽（scrubbing）：落点帧送出即停、不做解码超前——超前帧会在下一次重定向
                // 时全作废，是拖拽期 GPU 瞬时占用高（相对 VLC 只解到落点）的主因。
                let mut landed = false;
                let keep = match item {
                    Ok(Some(frame)) if frame.pts() >= frame_target => {
                        landed = true;
                        // 落点前一帧保底（排空先有帧可上屏）；软帧发送失败直接丢弃（无池资源）
                        if let Some(p) = pre_roll.take() {
                            let _ = send_relay(&cmd.tx, &cmd.wake, p);
                        }
                        // seek 探针写槽（每个 seek 一次）：此刻预滚数/回退量已定，
                        // 把解码阶段数据落进共享槽，供 tick 首帧落地时聚合打印。
                        if seek_diag_on && seek_first_sent.is_none() {
                            seek_first_sent = Some(Instant::now()); // 首帧 ≥ target 入队
                            // 泵竞态定位探针（env 门控）：首帧 ≥ target 就绪时刻，供与 pump
                            // 侧 [SEEKPUMPDBG] P/T 行对表（帧入队 → 泵唤醒 → 排空上屏时序）。
                            if seek_diag_enabled() {
                                eprintln!(
                                    "[SEEKPUMPDBG] W first>=t pts={:.2} target={:.2} gen={} reuse={} side={}",
                                    frame.pts(),
                                    cmd.target,
                                    cmd.generation,
                                    if same { 1 } else { 0 },
                                    if dual { "gpu" } else if hw_reader { "safe" } else { "soft" }
                                );
                            }
                            let mut sslot = seek_diag_slot().lock().unwrap_or_else(|e| e.into_inner());
                            let mine = sslot
                                .as_ref()
                                .map_or(true, |s| s.session == cmd.seek_session);
                            if mine {
                                let worker_start = seek_worker_start;
                                *sslot = Some(SeekDiag {
                                    session: cmd.seek_session,
                                    target: cmd.target,
                                    // 触发时刻沿用 start_decoder_at 预填（用户提交时刻）；
                                    // 槽被清/换代时退回 worker 开始时刻兜底
                                    trigger: sslot
                                        .as_ref()
                                        .and_then(|s| s.trigger)
                                        .or(worker_start),
                                    // GUI 线程投递时刻沿用 start_decoder_at 预填（[SEEKNODE]「submit」）
                                    sent: sslot.as_ref().and_then(|s| s.sent),
                                    recv: t_recv,
                                    opened: t_opened,
                                    worker_start,
                                    t_sent: seek_first_sent,
                                    reuse: same,
                                    regress: seek_regress,
                                    skipped: seek_skipped,
                                    firstpts: frame.pts(),
                                });
                            }
                        }
                        // 发送成功（主线程接收）或换代（接收端已换走 = 帧丢弃）；软帧无池资源、
                        // 无需回收；**GPU 帧发送失败=槽无人接收 → 当场回收回池**（防 worker 侧
                        // 槽泄漏：换代窗口每丢一帧都漏一块 in_use 槽，池越用越少）
                        let sent = if seek_diag_enabled() {
                            // seek 探针：wake 投递成功与否也落探针（仅 diag 下才关心 wake 细节）
                            match cmd.tx.send(frame) {
                                Ok(()) => {
                                    let w_ok = cmd.wake.try_send(()).is_ok();
                                    if seek_first_sent.is_some() && !seek_wake_dbg_printed {
                                        seek_wake_dbg_printed = true;
                                        eprintln!(
                                            "[SEEKPUMPDBG] W first-wake sent={} target={:.2}",
                                            if w_ok { 1 } else { 0 },
                                            cmd.target
                                        );
                                    }
                                    true
                                }
                                Err(e) => {
                                    Self::recycle_unsent_gpu(e.0, &mut active);
                                    false
                                }
                            }
                        } else {
                            match cmd.tx.send(frame) {
                                Ok(()) => {
                                    let _ = cmd.wake.try_send(());
                                    true
                                }
                                Err(e) => {
                                    Self::recycle_unsent_gpu(e.0, &mut active);
                                    false
                                }
                            }
                        };
                        sent
                    }
                    Ok(Some(frame)) => {
                        // 预滚（pts<落点，仅软解路径）：只记最新一帧，不送
                        if seek_diag_on {
                            seek_skipped += 1; // 预滚丢弃计数（pre_roll 分支只留最新一帧）
                        }
                        pre_roll = Some(frame);
                        true
                    }
                    Ok(None) => {
                        // EOF 且一帧未过落点（seek 到片尾外）：回退最后预滚帧，不黑屏/不挂泵
                        if let Some(p) = pre_roll.take() {
                            let _ = send_relay(&cmd.tx, &cmd.wake, p);
                        }
                        false
                    }
                    Err(_) => false, // 解码错误：本代结束
                };
                // 拖拽命令：首帧 ≥ target 已送出 → 停下不再解码超前（等下一条重定向命令，
                // 省拖拽期纯浪费的 GPU 解码；播放/点按 seek 的 scrubbing=false 不受影响）
                if cmd.scrubbing && landed && keep {
                    break;
                }
                let t2 = Instant::now();
                if vp_on {
                    vp_decode += t1 - t0;
                    vp_send += t2 - t1;
                    vp_frames += 1;
                    if Instant::now().duration_since(vp_window) >= Duration::from_secs(2) {
                        let wall = vp_window.elapsed().as_secs_f64();
                        eprintln!(
                            "[VPROF] worker {vp_frames}帧: decode={:.3}ms/f send={:.3}ms/f 忙核≈{:.2}",
                            vp_decode.as_secs_f64() * 1000.0 / vp_frames as f64,
                            vp_send.as_secs_f64() * 1000.0 / vp_frames as f64,
                            (vp_decode.as_secs_f64() + vp_send.as_secs_f64()) / wall
                        );
                        vp_decode = Duration::ZERO;
                        vp_send = Duration::ZERO;
                        vp_frames = 0;
                        vp_window = Instant::now();
                    }
                }
                if !keep {
                    // —— 设备丢失自恢复：硬解 reader 连续解码/预滚失败 ≥2 次 → 判定「该
                    // 文件硬解设备已丢」（TDR / driver reset / device-removed），静默重开软解
                    // reader 从当前播放位置续播（一个定格可接受，不弹错、不冻结）；单次瞬时
                    // 错误容忍（继续下一帧，不重开、不断代——若设备真丢，下一拍同样报错即进
                    // 上面分支）——
                    if hw_reader && item_err && hw_fail_count >= 2 {
                        hw_fail_count = 0;
                        // 本文件永久弃用硬解：后续 open 直接软解（其它文件仍可硬解）
                        crate::decode::blacklist_hw_path(&cmd.path);
                        let reopen_pos = resume_pts;
                        // 先 drop 旧 reader 再重开：释放外部注入设备 + FFmpeg 句柄后再做同文件
                        // 重开（SAFE→软 同样受益，更稳）
                        let _old = active.take();
                        match Self::open_reader_for_mode(&cmd.path, &ReaderMode::Soft(cmd.cap)) {
                            Ok((soft_mode, soft_reader, soft_copier)) => {
                                active = Some((
                                    cmd.path.clone(),
                                    soft_mode,
                                    soft_reader,
                                    soft_copier,
                                ));
                                hw_reader = false;
                                dual = false;
                                hw_primed = true;
                                // **刷新源顶格容量**：旧 reader 若是双设备直通（`source_cap()==1`），
                                // 不刷新会让 buf 停在 1 字节 → 软解 `_into` 报「解码输出缓冲过小」、
                                // 首帧解码失败断代（本命令无画面）。按新软 reader 刷新后，软解路径的
                                // `buf.len() < src_cap` 扩容自动把 buf 涨到真实容量。
                                src_cap = active
                                    .as_ref()
                                    .map(|(_, _, r, _)| r.source_cap())
                                    .unwrap_or(1);
                                // 续播接纳阈值抬到最后成功解码 pts：首个软帧 ≥ 此值才送
                                frame_target = reopen_pos;
                                pre_roll = None;
                                let _ = active
                                    .as_mut()
                                    .map(|(_, _, rd, _)| rd.seek(reopen_pos));
                                continue; // 下一拍起走软解逐帧路径，播放无感接续
                            }
                            Err(e) => {
                                active = None;
                                // 重开软解也失败：落断代（与既有错误断代一致，不弹错；无 reader，
                                // 命令从下方 break 退出）
                                let _ = e;
                            }
                        }
                    } else if hw_reader && item_err {
                        // 单次瞬时错误：容忍——不重开、不断代，继续解下一帧
                        continue;
                    }
                    // 本命以 EOF/解码错误结束（换代时不走此分支进入——换代已把旧接收端
                    // 换走，`try_send` 连不上、帧被丢弃且非必需；画面保留最近上屏帧，跟手
                    // 由 `SCRUB_RETARGET_MS` 节流保证，不依赖这里的保底）：把缓存的「落点
                    // 前一帧」尽力送出，让 pump 在 EOF/出错时至少有一帧上屏。`try_send_relay`：
                    // 接收端已换走/通道满都立刻失败（=丢弃），不阻塞 worker。
                    if let Some(p) = pre_roll.take() {
                        let _ = try_send_relay(&cmd.tx, &cmd.wake, p);
                    }
                    break; // 接收端已被换代/弃用
                }
            }
        }
    }


    /// 逐帧拉取主线程归还的 GPU 共享槽并回池（直通模式才有 copier；软解/SAFE BGRA 幂等 no-op）。
    /// 每次进内层循环调用一次：播放稳态下主线程每消费一帧归还一槽，不及时 reap 会让槽池在
    /// decode-ahead + 在途窗口内堆积 → `copy` 触发「池耗尽」错误 → 误回退 SAFE 路径。
    pub(super) fn reap_releases(
        active: &mut Option<(std::path::PathBuf, ReaderMode, VideoReader, Option<SharedCopier>)>,
        rel_rx: &std::sync::Mutex<std::sync::mpsc::Receiver<usize>>,
    ) {
        let Some((_, _, _, Some(copier))) = active.as_mut() else {
            return;
        };
        let _rx = rel_rx.lock().unwrap_or_else(|e| e.into_inner());
        while let Ok(shared_handle) = _rx.try_recv() {
            copier.recycle(shared_handle);
        }
    }


    /// GPU 帧在 worker 侧「未送出即丢弃」时的槽回收：换代/发送失败路径把一帧 `FrameItem::Gpu`
    /// 直接 drop 的话，其共享槽已 `in_use`、若不回收会在 worker 槽池泄漏（decode-ahead 窗口内
    /// 越堆越多 → 放大槽池耗尽 → 误触发背压重试——正是本修复要消除的「瞬态回压」）。软帧无池
    /// 资源无需回收（no-op）。**display.rs 主线程侧的归还路径保持原样**（那里已正确回池）。
    pub(super) fn recycle_unsent_gpu(
        frame: FrameItem,
        active: &mut Option<(std::path::PathBuf, ReaderMode, VideoReader, Option<SharedCopier>)>,
    ) {
        if let FrameItem::Gpu(f) = frame {
            if let Some((_, _, _, Some(copier))) = active.as_mut() {
                copier.recycle(f.shared_handle);
            }
        }
    }


    /// 按模式打开 reader：返回 `(实际模式, reader, 直通 copier)`。auto → **双设备 GPU 直通
    /// 优先**（worker 自有设备 + `open_hw`；任一环节失败 → 回落 SAFE BGRA 硬解
    /// `open_scaled_hw`，行为不降级）；soft → `open_scaled`（纯软解）。三者都失败才 `Err`。
    pub(super) fn open_reader_for_mode(
        path: &std::path::Path,
        mode: &ReaderMode,
    ) -> anyhow::Result<(ReaderMode, VideoReader, Option<SharedCopier>)> {
        use windows::core::Interface as _;
        match mode {
            ReaderMode::Auto(cap) => {
                // 设备丢失黑名单路径：不再尝试 hw init，直接软解（返回 Soft 模式便于复用判据
                // 对齐；其它文件不受影响，仍走硬解）
                if crate::decode::hw_path_blacklisted(path) {
                    #[cfg(test)]
                    HW_ENGAGED.store(false, Ordering::Relaxed);
                    return VideoReader::open_scaled(path, *cap)
                        .map(|r| (ReaderMode::Soft(*cap), r, None));
                }
                // 双设备 GPU 直通：worker 自有设备（硬件→WARP 兜底）+ 外部设备注入解码——
                // **无 SHR 门控**：解码池恒 DECODER-only（Intel Arc 拒绝 DECODER|SHR 组合；
                // 拷贝源只需 USAGE_DEFAULT，BIND_DECODER 同样可作拷贝源）。渲染设备只采样
                // 共享 NV12 槽（平面 SRV PlaneSlice），从不直接采解码纹理。任一环节失败
                //（建设备/open_hw）→ 回落 SAFE BGRA 硬解（与旧 auto 行为一致）。
                if dual_path_allowed() {
                    if let Some(worker) = create_worker_d3d11_device() {
                        let raw_dev = worker.device.as_raw() as usize;
                        let raw_ctx = worker.context.as_raw() as usize;
                        if let Ok(r) = VideoReader::open_hw(path, raw_dev, raw_ctx) {
                            #[cfg(test)]
                            {
                                DUAL_ENGAGED.store(true, Ordering::Relaxed);
                                HW_ENGAGED.store(r.is_hw(), Ordering::Relaxed);
                            }
                            return Ok((
                                ReaderMode::Auto(*cap),
                                r,
                                Some(SharedCopier::new(worker)),
                            ));
                        }
                    }
                }
                let r = VideoReader::open_scaled_hw(path, *cap)?;
                #[cfg(test)]
                HW_ENGAGED.store(r.is_hw(), Ordering::Relaxed);
                Ok((ReaderMode::Auto(*cap), r, None))
            }
            ReaderMode::Soft(cap) => {
                #[cfg(test)]
                HW_ENGAGED.store(false, Ordering::Relaxed);
                VideoReader::open_scaled(path, *cap).map(|r| (ReaderMode::Soft(*cap), r, None))
            }
        }
    }


    /// 按进程级解码偏好打开 **SAFE** hover 预解码 reader（独立线程 + 独立设备，无需双设备
    /// 直通的共享槽/释放通道管理，故不走 `open_reader_for_mode` 的双设备分支）：
    /// - `decode_mode()==1`（强制软解）→ `open_scaled`（纯软解）；
    /// - 否则 auto/hw → `open_scaled_hw`（FFmpeg 自有设备硬解优先、失败内核自动回软解）；
    ///   硬解设备丢失黑名单路径直接软解（与播放 worker `open_reader_for_mode` 的 SAFE
    ///   选择逻辑对齐）。
    pub(super) fn open_hover_reader(path: &std::path::Path, cap: u32) -> Result<VideoReader, String> {
        if crate::decode::decode_mode() == 1 {
            VideoReader::open_scaled(path, cap).map_err(|e| e.to_string())
        } else if crate::decode::hw_path_blacklisted(path) {
            VideoReader::open_scaled(path, cap).map_err(|e| e.to_string())
        } else {
            VideoReader::open_scaled_hw(path, cap).map_err(|e| e.to_string())
        }
    }


    /// 常驻 hover 预解码 worker 主循环（独立线程 + 独立 `VideoReader`，与播放 worker 彻底
    /// 隔离）：复用同 path+cap+**模式** 的 reader（模式按进程级解码偏好，同播放 worker 的
    /// 复用键判据），打开走 [`Self::open_hover_reader`] 的 SAFE 选择（硬解优先软回退）。
    /// 预滚：硬解 reader 一次 `skip_to_bgra_into` 完成 seek + 跳帧（免转换/免回读）+ 首帧
    /// ≥落点 BGRA；软解照旧 `seek` 后逐帧跳过 `pts<target` 的预滚帧、取第一帧 `≥target`。
    /// 结果经单次通道回传主线程（与播放 worker 同一「不展示早于落点帧」规则）。
    /// 每拍解码前比对共享 [`Self::hover_generation`]：已被更新的悬停/换文件/真实 seek 取代
    /// 的命令立即放弃（帧间检查，省被跳过命令的剩余解码）；与播放 worker 只在 cdylib 内
    /// FFMPEG_LOCK 上全局串行（FFmpeg 内核闸，属预期内竞争，不做额外锁）。
    pub(super) fn hover_worker(
        rx: std::sync::mpsc::Receiver<HoverCmd>,
        hgen: Arc<std::sync::atomic::AtomicU64>,
    ) {
        let mut active: Option<(std::path::PathBuf, ReaderMode, VideoReader)> = None;
        let mut buf: Vec<u8> = Vec::new();
        while let Ok(cmd) = rx.recv() {
            // 已在队列中被更新的 hover 命令取代（代序号已推进）→ 直接放弃：连 avformat 重开
            // 都省掉；`cmd` drop 使结果通道断连，主线程按断连丢弃（不误覆盖新落点缓存）。
            if hgen.load(Ordering::SeqCst) != cmd.generation {
                continue;
            }
            // 模式与播放 worker 同源（decode_mode()==1 → 强制软解；否则 auto → SAFE 硬解优先）
            let mode = if crate::decode::decode_mode() == 1 {
                ReaderMode::Soft(cmd.cap)
            } else {
                ReaderMode::Auto(cmd.cap)
            };
            let same = matches!(&active, Some((p, m, _)) if *p == cmd.path && *m == mode);
            if !same {
                match Self::open_hover_reader(&cmd.path, cmd.cap) {
                    Ok(r) => active = Some((cmd.path.clone(), mode, r)),
                    Err(e) => {
                        let _ = cmd.tx.send(Err(e));
                        continue;
                    }
                }
            }
            let reader = &mut active.as_mut().expect("打开成功必有 reader").2;
            // 解码缓冲预分配：按源顶格扩容一次（同播放 worker 的持久缓冲姿势）
            if buf.len() < reader.source_cap() {
                buf.resize(reader.source_cap(), 0);
            }
            if reader.is_hw() {
                // —— SAFE 硬解 seek 预滚快路径：一次 `skip_to_bgra_into` 完成 seek + 跳帧
                //（免转换/免回读）+ 首帧 ≥ 落点 BGRA（首帧 pts≥target 由内核保证）——
                match reader.skip_to_bgra_into(cmd.target, &mut buf[..]) {
                    Ok(Some((pts, w, h, _skipped)))
                        if hgen.load(Ordering::SeqCst) == cmd.generation =>
                    {
                        let need = (w as usize) * (h as usize) * 4;
                        let _ = cmd.tx.send(Ok(HoverFrame {
                            pts,
                            w,
                            h,
                            bgra: buf[..need].to_vec(),
                        }));
                    }
                    Ok(_) | Err(_) => {} // EOF/失败/中途换代：无可用帧，按失败丢弃
                }
                continue;
            }
            // —— 软解照旧：seek 后逐帧跳过 pts<落点 的预滚帧，取首帧 ≥ 落点 ——
            let _ = reader.seek(cmd.target);
            loop {
                if hgen.load(Ordering::SeqCst) != cmd.generation {
                    break; // 已被更新悬停/换文件/seek 取代：放弃（不送结果，主线程按断连丢弃）
                }
                match reader.next_frame_bgra_into(&mut buf[..]) {
                    Ok(Some((pts, w, h))) if pts >= cmd.target => {
                        let need = (w as usize) * (h as usize) * 4;
                        let _ = cmd.tx.send(Ok(HoverFrame {
                            pts,
                            w,
                            h,
                            bgra: buf[..need].to_vec(),
                        }));
                        break;
                    }
                    Ok(Some(_)) => {} // 预滚帧（pts<落点）：继续解下一帧
                    Ok(None) | Err(_) => break, // EOF / 解码错误：无可用帧，按失败丢弃
                }
            }
        }
    }


    /// 惰性打开音频输出（不可用则纯画面播放，只报错一次）。
    pub(super) fn ensure_audio(&mut self) {
        if self.audio.is_some() || self.audio_err {
            return;
        }
        match AudioSink::open() {
            Ok(sink) => {
                eprintln!(
                    "[video] 音频输出就绪：{}Hz 立体声",
                    sink.sample_rate()
                );
                self.audio = Some(sink);
                self.apply_volume();
            }
            Err(e) => {
                self.audio_err = true;
                eprintln!("[video] 音频输出不可用，纯画面播放：{e}");
            }
        }
    }


    /// 启动（或重开）音频解码：向**常驻音频 worker** 投新命令。命令开头由 worker 统一
    /// `flush`（清残音 + 位置归零）——worker 是唯一推音者，顺序处理命令，无并发竞态。
    /// `audio_ready` 复位待 worker 开成功再置位；倍速=重采样输出率 `设备率÷speed`
    ///（变速变调）；音频时钟 `playhead = audio_base + 设备位置 × speed`。
    pub(super) fn start_audio_at(&mut self, target: f64) {
        let Some(sink) = self.audio.clone() else {
            return;
        };
        self.audio_base = target;
        self.audio_ready.store(false, Ordering::Relaxed);
        self.audio_anchor_pending = true;
        let agen = self.audio_gen.fetch_add(1, Ordering::SeqCst) + 1;
        self.audio_gen.store(agen, Ordering::SeqCst);
        self.ensure_audio_worker();
        let out_rate = audio_read_rate(sink.sample_rate(), self.speed);
        let cmd = AudioCmd {
            generation: agen,
            path: self.path.clone(),
            target,
            out_rate,
            ready: self.audio_ready.clone(),
            sink,
        };
        if let Some(tx) = &self.audio_cmd {
            let _ = tx.send(cmd);
        }
    }


    /// 惰性启动常驻音频解码 worker（同视频 worker 的收尾理由）。
    pub(super) fn ensure_audio_worker(&mut self) {
        if self.audio_cmd.is_some() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel::<AudioCmd>();
        let gen_arc = self.audio_gen.clone();
        std::thread::spawn(move || Self::audio_worker(rx, gen_arc));
        self.audio_cmd = Some(tx);
    }


    /// 常驻音频 worker 主循环：命令开头 flush（旧音残余清掉、位置归零），打开音轨 →
    /// 重采样成 F32 立体声推入 sink（回压守 1s 缓冲）。`ready` 开成功置 true（无声轨/
    /// 开失败保持 false → 主线程走墙钟，画面照常前进）。
    pub(super) fn audio_worker(rx: std::sync::mpsc::Receiver<AudioCmd>, gen_arc: Arc<AtomicU64>) {
        // 常驻音频 reader：同文件 + 同 out_rate（倍速改变 out_rate 才需重开）的命令直接
        // 复用——seek 只做容器定位 + 冲重采样器（dll 内核 `AudioReader::seek`），省每次
        // seek 的全量 avformat 重开（~40-50ms），也免去与视频 worker 的 seek/解码抢
        // FFMPEG_LOCK（重开期间长持锁会拖延视频落点帧 ——「点击进度条后画面要等片刻」
        // 的残余主因）。换文件或倍速（out_rate 变）才重开。
        let mut active: Option<(std::path::PathBuf, u32, AudioReader)> = None;
        // R2-2：常驻音频块缓冲（跨命令/复用 reader 全程复用，省旧 `next_pcm_f32` 每块
        // `vec![0f32; 65536]` 256KB 全零初始化 + 分配热点——`next_pcm_f32_into` 只按需
        // 扩容、`truncate(set_len)` 写回，稳态零分配）。
        let mut pcm_buf: Vec<f32> = Vec::new();
        while let Ok(cmd) = rx.recv() {
            cmd.sink.flush();
            let same = matches!(&active, Some((p, r, _)) if *p == cmd.path && *r == cmd.out_rate);
            if !same {
                let reader = match AudioReader::open_pcm_f32(&cmd.path, cmd.out_rate) {
                    Ok(r) => r,
                    Err(_) => continue,
                };
                active = Some((cmd.path.clone(), cmd.out_rate, reader));
            }
            cmd.ready.store(true, Ordering::Relaxed);
            let reader = &mut active.as_mut().expect("复用分支已确保有 reader").2;
            let _ = reader.seek(cmd.target);
            while gen_arc.load(Ordering::SeqCst) == cmd.generation {
                match reader.next_pcm_f32_into(&mut pcm_buf) {
                    Ok(Some(_pts)) => {
                        // 回压：缓冲满时等待（不把整段音频预解码塞爆内存）
                        while gen_arc.load(Ordering::SeqCst) == cmd.generation
                            && cmd.sink.queued_len() >= AUDIO_QUEUE_AIM
                        {
                            std::thread::sleep(std::time::Duration::from_millis(8));
                        }
                        if gen_arc.load(Ordering::SeqCst) != cmd.generation {
                            break;
                        }
                        cmd.sink.push(&pcm_buf);
                    }
                    _ => break, // EOF / 解码错误：本代结束
                }
            }
        }
    }


    /// 播放泵是否仍需运行（拖拽中也必须持续——暂停态拖拽时 `playing` 为假、落点帧
    /// 上屏前 `pending_seek` 也可能短暂为空，但预览要跟着手指走）。
    pub(super) fn pump_wanted(&self) -> bool {
        self.playing || self.pending_seek.is_some() || self.scrubbing
    }


    /// 终局性打开/解码失败专用的「停泵」清理：清掉全部 [`Self::pump_wanted`] 依赖源
    ///（`playing` / `pending_seek` / `scrubbing`），让正在运行的自续 pump 在下一拍
    /// `pump_wanted()==false` 即退出、不再自续定时器。之前失败分支只关 `loading`，
    /// `playing`（new 默认 true）与 `pending_seek`（`start_decoder_at` 置 Some、仅首个帧
    /// 上屏才清）残留 → `pump_wanted()` 恒真 → pump 无限自续：测试执行器永不静止
    ///（shell 两个开损坏视频的测试挂死）、生产态打开损坏视频后遗留永久空转 pump。
    /// 只应在终局失败分支调用；成功/正常播放/seek 路径不得触碰。
    pub(super) fn stop_pump_on_failure(&mut self) {
        self.playing = false;
        self.pending_seek = None;
        self.scrubbing = false;
    }


    /// 泵节拍：按内容帧率自适应（30fps→~33ms、60fps→16ms、25fps→40ms），下限
    /// 8ms（120fps）、上限 50ms（20fps 兜底未知/异常帧率）。旧版恒 16ms（62Hz）：
    /// 多出的拍没有新帧、纯为推进度条而强刷整窗，是播放 CPU 大头之一。
    pub(super) fn pump_interval(&self) -> Duration {
        let ms = self
            .fps
            .filter(|(n, d)| *n > 0 && *d > 0)
            .map(|(n, d)| 1000 * u64::from(d) / u64::from(n))
            .unwrap_or(PUMP_MS);
        Duration::from_millis(ms.clamp(8, 50))
    }


    /// 按显示尺寸定解码长边封顶（px）：`视口最大轴 × 缩放` 向上对齐到 `CAP_BUCKET`
    /// 桶、钳进 `[DECODE_MIN_DIM, DECODE_MAX_DIM]`。缩放后画面可能超出视口，仍受上限约束；
    /// 桶对齐避免窗口拖动时逐像素重开解码器（省 decode/swscale/纹理上传）。
    pub(super) fn desired_decode_cap(&self, vw: f32, area_h: f32) -> u32 {
        let longest = (vw.max(area_h).max(1.0) * self.zoom).ceil() as u32;
        let snapped = longest.saturating_add(CAP_BUCKET - 1) / CAP_BUCKET * CAP_BUCKET;
        snapped.clamp(DECODE_MIN_DIM, DECODE_MAX_DIM)
    }


    /// 泵事件等待的「兜底超时」：统一 seek/播放两路（[`Self::ensure_pump`]）的「wake +
    /// 超时」等待——事件驱动是主路径（worker 入队一帧即醒，帧到屏「通知即到」），本值只作
    /// **无新帧**时的节拍兜底 = min(切片, 帧率节拍)：绝不大于节拍（播放稳态 playhead 照常
    /// 推进、进度条平滑、K12 不空转、不忙唤醒），也绝不大于切片（通知被 cap-1 级联丢弃而
    /// 帧已在通道时 ≤ 切片回 pump tick 排空上屏，不整睡静默）。120fps 节拍下限 8ms、慢帧率
    /// 50ms 兜底语义不变（只参与 min 取小）。仅生产路径使用（cfg(test) 泵事件等待直接走
    /// 细拍兜底 [`SEEK_WAKE_POLL_MS_TEST`]，不依赖本值）。
    #[cfg(not(test))]
    pub(super) fn pump_event_timeout(&self) -> Duration {
        let beat_ms = self.pump_interval().as_millis() as u64;
        Duration::from_millis(SEEK_WAKE_SLICE_MS.min(beat_ms))
    }


    /// 泵等待的「兜底超时口径」：[`ensure_pump`] 的正常等待走「wake + 超时=min(切片,节拍)」
    ///（[`Self::pump_event_timeout`]）；本值仅作「帧就绪通知通道缺失（`frame_wake` 为 None，
    /// 实际构造即建不出现）」时的最后防线：seek 首帧落地等待期（`pending_seek` 活跃）
    /// = [`COARSE_REPAINT_MS`]，其余按帧率节拍（不改变播放节奏）。
    pub(super) fn pump_wait_interval(&self) -> Duration {
        if self.pending_seek.is_some() {
            Duration::from_millis(SEEK_WAKE_CAP_MS)
        } else {
            self.pump_interval()
        }
    }


    pub(super) fn ensure_pump(&mut self, cx: &mut Context<Self>) {
        if self.pump_running || !self.pump_wanted() {
            return;
        }
        self.pump_running = true;
        cx.spawn(async move |this, cx| {
            // 上次强制重绘时刻：仅「有新帧落屏」才每拍重绘；无新帧的闲置拍最多每
            // COARSE_REPAINT_MS 一次（推进度条/控制条），不再 16ms 把整窗全刷一遍。
            let mut last_paint: Option<Instant> = None;
            // seek 泵竞态定位探针（env 门控）：本拍起点 / 上一拍排空时刻，供 [SEEKPUMPDBG]
            // P/T 行对表（帧入队 → 泵唤醒方式 → 排空上屏的墙钟缺口）。
            let pump_t0 = std::time::Instant::now();
            let mut pump_prev_land: Option<std::time::Instant> = None;
            loop {
                let pump_it_t0 = std::time::Instant::now();
                let mut pump_dbg_wait: &'static str = "none";
                let mut pump_dbg_wait_ms = 0.0f64;
                let mut pump_dbg_new = false;
                let mut pump_dbg_tick_ms = 0.0f64;
                // 等待方式：seek/播放两条统一为一条「wake 事件驱动 + 兜底超时」路径，不改变
                // 帧率节拍的上下限语义。无论 `pending_seek` 是否活跃，都阻塞在 worker「帧
                // 就绪通知」通道上等待——worker 入队一帧即唤醒 drain+上屏（seek 首帧落地与
                // 播放稳态取帧都收敛到「通知即到」，帧到屏 ≈0-2ms）；**播放支路不再整睡一拍
                // 节拍**：真机实测「seek 落点正逢 pump 于播放节拍定时休眠（非 seek 事件等待）
                // 时触发，泵要等下一拍才排空」→ frame2screen 偶发 38-76ms 末梢，由本路径
                // 消掉（事件即醒）。无新帧的等待以 **min(切片,节拍)（≤ 节拍）** 为界：兜底
                // 超时既自查帧通道（通知被 cap-1 级联丢弃而帧已在通道时 ≤ 切片上屏、绝不
                // 整睡 [`SEEK_WAKE_CAP_MS`]），也让播放稳态照常推进 playhead（进度条平滑、
                // K12 不空转、不忙唤醒）。[`Self::pump_wait_interval`] 仅作「帧就绪通道缺失
                //（构造即建，实际不出现）」时的最后防线。
                let wait = this
                    .update(cx, |v, _| v.pump_wait_interval())
                    .unwrap_or(Duration::from_millis(PUMP_MS));
                let (rx, pend) = this
                    .update(cx, |v, _| {
                        (
                            v.frame_wake.as_ref().map(|w| w.rx.clone()),
                            v.pending_seek.is_some(),
                        )
                    })
                    .unwrap_or((None, false));
                match rx {
                    Some(rx) => {
                        #[cfg(test)]
                        {
                            // 测试 fake 调度器无「跨线程唤醒」（确定性护栏拒绝 foreign
                            // wake，本仓实测）：事件通知用「先 try_recv 排空已就绪通知、
                            // 无则睡兜底 fake 定时器」的半阻塞姿势表达——run_until_parked
                            // 对无通知的等待正常 park（不阻塞线程、不死等）。兜底拍用细粒度
                            //（[`SEEK_WAKE_POLL_MS_TEST`]）让 advance_clock 驱动 pump 及时
                            // 排空 worker 已入队的帧（bench 帧到屏 ≈ 细拍粒度，可作回归锚）；
                            // 生产走下方真实事件路径。**播放支路同样走本姿势**：真节拍定时
                            // 在 fake 调度器下盖不住「事件即醒」，回归测试据此区分新旧行为。
                            pump_dbg_wait = "recv:test";
                            pump_dbg_wait_ms = 0.0;
                            let cap = Duration::from_millis(SEEK_WAKE_POLL_MS_TEST);
                            let has = rx
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .try_recv()
                                .is_ok();
                            if !has {
                                cx.background_executor().timer(cap).await;
                            }
                        }
                        #[cfg(not(test))]
                        {
                            // 生产：事件驱动 + 兜底超时（min(切片,节拍)）。
                            // ① 先**非阻塞排空** wake 通道：上一拍 tick 期间 worker 又入队
                            //    帧的通知可能已就绪——有信号立即放行 tick，杜绝「通知已到、
                            //    泵却睡整拍」。同落点第二次点击的残留窗口：新泵首片消化上一
                            //    seek worker 续播尾通知后再等真实通知，旧实现整睡 200ms。
                            // ② 无信号才阻塞 recv 一小片（`pump_event_timeout`），worker
                            //    真实通知「即到」（事件驱动，帧到屏 ≈0-2ms 不变）——播放
                            //    支路也在此监听：seek 落点恰逢播放节拍休眠时，首帧通知即醒
                            //    泵、不等下一拍（消 38-76ms 末梢）。
                            // ③ 超时（无新帧 / 通知被 cap-1 级联丢弃而帧已在通道）→ 回 pump
                            //    tick 排空帧通道：帧已入队本拍即上屏，未入队再进下一片等待。
                            //    超时 ≤ 节拍：播放稳态 playhead 照常推进、进度条平滑，
                            //    无新帧的等待以节拍为界（K12 不空转）。
                            let timeout = this
                                .update(cx, |v, _| v.pump_event_timeout())
                                .unwrap_or(Duration::from_millis(SEEK_WAKE_SLICE_MS));
                            let notified = {
                                let wake = rx.lock().unwrap_or_else(|e| e.into_inner());
                                let mut any = false;
                                while wake.try_recv().is_ok() {
                                    any = true;
                                }
                                any
                            };
                            if !notified {
                                let recv_t0 = std::time::Instant::now();
                                let recv_kind = cx
                                    .background_executor()
                                    .spawn(async move {
                                        let wake = rx.lock().unwrap_or_else(|e| e.into_inner());
                                        wake.recv_timeout(timeout).is_ok()
                                    })
                                    .await;
                                pump_dbg_wait_ms = recv_t0.elapsed().as_secs_f64() * 1000.0;
                                pump_dbg_wait = if recv_kind { "recv:wake" } else { "recv:timeout" };
                            } else {
                                pump_dbg_wait = "drain:early";
                            }
                        }
                    }
                    None => {
                        cx.background_executor().timer(wait).await;
                    }
                }
                let now = Instant::now();
                let keep = this
                    .update(cx, |v, cx| {
                        let tick_t0 = std::time::Instant::now();
                        let new_frame = v.tick(cx);
                        pump_dbg_tick_ms = tick_t0.elapsed().as_secs_f64() * 1000.0;
                        pump_dbg_new = new_frame;
                        let coarse_due = last_paint
                            .map(|t| {
                                now.duration_since(t)
                                    >= Duration::from_millis(COARSE_REPAINT_MS)
                            })
                            .unwrap_or(true);
                        if new_frame || coarse_due {
                            // 关键：新帧/粗粒度推进时强制重绘（单靠 notify 在实体未被
                            // 窗口 tracking 时可能空操作）——否则播放只在状态里推进、画面
                            // 停在首帧占位。见 pdf 瓦片泵。
                            last_paint = Some(now);
                            cx.refresh_windows();
                        }
                        let wanted = v.pump_wanted();
                        if !wanted {
                            // 泵收尾与循环退出**同拍完成**：不再让旧泵「已退出、标记未清」
                            // 的异步窗口挡住下一次 seek 的 ensure_pump（真机同落点连点第二次
                            // 点击 44-50ms 静默——旧泵收尾滞后时新泵启不动、首帧无人排空）。
                            v.pump_running = false;
                        }
                        wanted
                    })
                    .unwrap_or(false);
                // 泵竞态定位探针（env 门控，仅待 seek 落点期）：一拍=P 行（等待类型/墙钟）+
                // T 行（tick 是否排到帧/耗时），与 worker [SEEKPUMPDBG] W 行对表。
                if pend && seek_diag_enabled() {
                    let it_ms = pump_t0.elapsed().as_secs_f64() * 1000.0;
                    let iter_ms = pump_it_t0.elapsed().as_secs_f64() * 1000.0;
                    let since_ms = pump_prev_land
                        .map(|t| t.elapsed().as_secs_f64() * 1000.0)
                        .unwrap_or(0.0);
                    if pump_dbg_wait == "drain:early" {
                        eprintln!(
                            "[SEEKPUMPDBG] P t={it_ms:8.2} iter={iter_ms:7.2} wait=drain:early new={} tick={:.3}ms since_prev_tick={since_ms:7.2}",
                            if pump_dbg_new { 1 } else { 0 },
                            pump_dbg_tick_ms
                        );
                    } else {
                        eprintln!(
                            "[SEEKPUMPDBG] P t={it_ms:8.2} iter={iter_ms:7.2} wait={} wait_ms={:.2} new={} tick={:.3}ms since_prev_tick={since_ms:7.2}",
                            pump_dbg_wait,
                            pump_dbg_wait_ms,
                            if pump_dbg_new { 1 } else { 0 },
                            pump_dbg_tick_ms
                        );
                    }
                    pump_prev_land = Some(pump_it_t0);
                }
                if !keep {
                    break;
                }
            }
            // 收尾安全网（仅当泵已不再被需要才清旗标）：不要让无条件清写覆盖「在循环内清旗标
            // 后、本拍 break 前」期间新启动的泵（同落点/EOF 后立刻再 seek，新泵 pump_running
            // 已置 true 时会被旧泵此处误清 → 第三次 ensure_pump 双泵并跑）。
            let _ = this.update(cx, |v, _cx| {
                if !v.pump_wanted() {
                    v.pump_running = false;
                }
            });
        })
        .detach();
    }
}
