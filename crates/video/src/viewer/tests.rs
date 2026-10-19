    use super::*;
    use super::render::audio_read_rate;
    use gpui::AppContext as _;
    use gpui::TestAppContext;
    use gpui::VisualTestContext;

    fn fixture() -> PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures")
            .join("video测试.mp4")
    }

    /// 真解码测试共用闸：FFMPEG_LOCK（cdylib 内）全局串行所有 FFmpeg 操作，多个真解码测试
    /// **并行**跑会让各自 worker 在锁上排队，`loads_first_frame`/`switch_reopens_*` 等 guard
    /// 循环在排队等待里耗尽 → 偶发超时（已知 flake）。持锁把这些视频测试串行化后不再竞争
    ///（同 pdf 的 `test_util::lock()` 姿势；`MutexGuard` 跨 gpui::test await 可持，见 pdf 用例）。
    fn ffmpeg_guard_lock() -> &'static std::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
    }

    fn blank(path: PathBuf, cx: &mut Context<VideoViewer>) -> VideoViewer {
        VideoViewer {
            path,
            focus_handle: cx.focus_handle(),
            frame: None,
            gpu_frame: None,
            #[cfg(test)]
            dropped_frame: None,
            #[cfg(test)]
            last_frame_pts: None,
            intrinsic: None,
            loading: false,
            load_error: None,
            bg: None,
            playing: false,
            playhead: 0.0,
            duration: 2.0,
            fps: None,
            decode_cap: DECODE_MAX_DIM,
            hovered_target: None,
            hover_x: None,
            hover_cmd: None,
            hover_generation: Arc::new(AtomicU64::new(0)),
            hover_cache: None,
            served_hover_target: None,
            last_tick: None,
            nav: FolderNav::new(vec![fixture()], 0),
            rx: None,
            overflow: None,
            generation: Arc::new(AtomicU64::new(0)),
            pump_running: false,
            pending_seek: None,
            scrubbing: false,
            scrub_target: 0.0,
            last_scrub_retarget: None,
            audio: None,
            audio_base: 0.0,
            audio_gen: Arc::new(AtomicU64::new(0)),
            audio_err: false,
            diag_ticks: 0,
            // 最近一次 seek 命令的诊断会话号（`GPUI_EDITOR_VIDEO_SEEK_DIAG=1` 时 tick
            // 据此匹配共享槽并聚合打印；普通运行恒 0/新值）。
            diag_session: 0,
            decode_cmd: None,
            release_cmd: None,
            frame_wake: {
                let (wtx, wrx) = std::sync::mpsc::sync_channel::<()>(1);
                Some(FrameWake {
                    tx: wtx,
                    rx: Arc::new(std::sync::Mutex::new(wrx)),
                })
            },
            audio_cmd: None,
            audio_ready: Arc::new(AtomicBool::new(false)),
            audio_anchor_pending: true,
            volume: 1.0,
            muted: false,
            speed: 1.0,
            zoom: 1.0,
            pan: (0.0, 0.0),
            drag: None,
            locked: false,
            pre_fullscreen_locked: false,
            controls_auto: auto_hide::AutoHide::new(),
            seek_bounds: Rc::new(Cell::new(Bounds::default())),
            vol_bounds: Rc::new(Cell::new(Bounds::default())),
            viewport: ScrollHandle::new(),
            playback_mode: PlaybackMode::Sequential,
            rng_state: 42, // 固定种子：随机播放测试可预期
            status: None,
            status_seq: 0,
            decode_pref: 0,
            subtitles: Vec::new(),
            subtitle_visible: true,
            subtitle_style: SubtitleStyle::default(),
        }
    }

    fn frame_of(pts: f64, v: u8) -> FrameItem {
        FrameItem::Soft(SoftFrame {
            pts,
            w: 320,
            h: 240,
            bgra: vec![v; (320 * 240 * 4) as usize],
        })
    }

    /// 回归：换接收端/换文件时旧通道里缓存的 GPU 直通帧槽位必须归还 —— 不能随
    /// `drop(old)` 静默丢失（那是拖动重定向偶发丢帧定格的根因：泄漏槽累积到槽池耗尽 →
    /// Backpressure → 跳过本帧，画面定格）。
    #[gpui::test]
    fn replaced_rx_drain_releases_gpu_slots(cx: &mut TestAppContext) {
        let (rel_tx, rel_rx) = std::sync::mpsc::channel::<usize>();
        let viewer = cx.update(|app| app.new(|cx| VideoViewer::new(fixture(), cx)));
        viewer.update(cx, |v, _| v.release_cmd = Some(rel_tx));

        // 造「旧接收端」并缓存：GPU 直通帧 ×2（槽已占 in_use=true，仅传句柄数值）+ 软帧 ×1
        let (tx, old_rx) = std::sync::mpsc::sync_channel::<FrameItem>(4);
        tx.send(FrameItem::Gpu(GpuFrame {
            pts: 1.0,
            w: 320,
            h: 240,
            shared_handle: 111,
        }))
        .unwrap();
        let _ = tx.send(frame_of(2.0, 0x55));
        tx.send(FrameItem::Gpu(GpuFrame {
            pts: 3.0,
            w: 320,
            h: 240,
            shared_handle: 222,
        }))
        .unwrap();
        drop(tx);

        viewer.update(cx, |v, _| v.drop_replaced_rx_frames(old_rx));

        let got: Vec<usize> = rel_rx.try_iter().collect();
        assert!(got.contains(&111), "GPU 槽 111 未归还（泄漏）: {got:?}");
        assert!(got.contains(&222), "GPU 槽 222 未归还（泄漏）: {got:?}");
        assert_eq!(got.len(), 2, "软帧不应触发槽归还: {got:?}");
    }

    /// 设备丢失回归的测试卫生守卫：任何退出路径（含 panic unwind）都清空硬解黑名单并撤销
    /// 故障注入，防进程级静态泄漏污染后续测试（黑名单/注入均为进程级持久状态）。
    struct FaultAndBlGuard;
    impl Drop for FaultAndBlGuard {
        fn drop(&mut self) {
            crate::decode::hw_blacklist_clear_for_test();
            hw_fault_disable();
            crate::hw::copy_fail_disable();
        }
    }

    /// 双设备直通的机器能力**诊断**：建 worker 自有 D3D11 设备 + 查询驱动是否支持「D3D11VA
    /// 解码数组纹理带 SHADER_RESOURCE」。`None` = 本机连 worker D3D11 设备都建不起（测试
    /// SKIP）；`Some(..)` = 该驱动约束的**诊断值**——**不再是直通门控**（直通恒用
    /// DECODER-only 解码池，渲染端只采样共享槽）。Arc（`Some(false)`）现在断言直通**照常
    /// 生效**（`gpu_passthrough_machine_adaptive_gate` 的 Arc 回归分支）。
    fn dual_gate_capability() -> Option<bool> {
        use windows::core::Interface as _;
        let worker = crate::hw::create_worker_d3d11_device()?;
        Some(crate::decode::VideoReader::supports_shader_bound_textures(
            worker.device.as_raw() as usize,
            worker.context.as_raw() as usize,
        ))
    }

    /// delta 回归（音频 worker 复用）：常驻 `AudioReader` 连续 seek 回不同落点，每次重新
    /// 起播首块 pts 落在目标附近（reuse 不重开文件、`AudioReader::seek` 冲掉 swr 残留）。
    /// 真音频（`target\视频测试.mp4` 有音轨），缺夹具 SKIP（AGENTS.md §5）。
    #[test]
    fn audio_reader_reuses_across_seeks_on_realfile() {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/视频测试.mp4");
        if !path.exists() {
            eprintln!("SKIP: missing target/视频测试.mp4（夹具不入库，本机无真视频）");
            return;
        }
        let _g = ffmpeg_guard_lock().lock().unwrap_or_else(|e| e.into_inner());
        let mut r = match crate::decode::AudioReader::open_pcm_f32(&path, 48_000) {
            Ok(r) => r,
            Err(_) => {
                eprintln!("SKIP: 音频打开失败（无声轨/运行库缺）");
                return;
            }
        };
        for target in [0.1, 6.0, 1.5, 9.9] {
            r.seek(target).unwrap();
            let (pts, samples) = match r.next_pcm_f32() {
                Ok(Some(x)) => x,
                Ok(None) => panic!("seek 后不应 EOF（target={target}）"),
                Err(_) => panic!("音频复用 seek 解码失败（target={target}）"),
            };
            // 音频容器 seek 的包边界粒度本就有 ~0.5s 预滚（fresh 同样，实测 target=1.5→0.98）；
            // 复用若损坏会从旧位置续解（偏差数秒）→ 1.0 容差仍能区分「复用正确」与「复用失效」。
            assert!(
                (pts - target).abs() < 1.0,
                "音频复用 seek 首块落点偏离 target={target} pts={pts}"
            );
            assert!(
                !samples.is_empty() && samples.iter().all(|s| s.is_finite()),
                "首块样本应非空且有限"
            );
        }
    }

    /// delta 回归：seek 复用常驻 reader（同文件 + 同 cap 不再重开文件，省每次 seek 的
    /// avformat 重开/初始化 →「点击进度条后画面立即切换」的关键）。连续多次 seek 回到
    /// 不同落点，每次首帧上屏都落在落点附近：证明复用不残留旧状态、正向/反向回 seek、
    /// 近结尾（EOF 回退）全正确。
    #[gpui::test]
    #[allow(clippy::await_holding_lock)]
    async fn repeated_seeks_land_on_each_target(cx: &mut TestAppContext) {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/视频测试.mp4");
        if !path.exists() {
            eprintln!("SKIP: missing target/视频测试.mp4（夹具不入库，本机无真视频）");
            return;
        }
        let _ffmpeg_guard_test = ffmpeg_guard_lock().lock().unwrap_or_else(|e| e.into_inner());
        let viewer: gpui::Entity<VideoViewer> =
            cx.update(|app| app.new(|cx| VideoViewer::new(path, cx)));
        let mut wait = 0;
        loop {
            let ready = cx.update(|app| {
                let v = viewer.read(app);
                v.frame.is_some() && v.duration > 1.0 && v.pending_seek.is_none()
            });
            if ready {
                break;
            }
            viewer.update(cx, |v, cx| {
                let _ = v.tick(cx);
            });
            cx.background_executor.timer(Duration::from_millis(5)).await;
            wait += 1;
            assert!(wait < 6000, "等待首帧/时长超时");
        }
        let duration = cx.update(|app| viewer.read(app).duration);
        // A→B→A（反向回 seek）+ 近结尾（EOF 回退）四连 seek，全程复用同一 reader
        let targets = [duration * 0.15, duration * 0.62, duration * 0.15, duration * 0.95];
        let mut landed = Vec::new();
        for &target in &targets {
            viewer.update(cx, |v, cx| v.seek(target - v.playhead, cx));
            let deadline = Instant::now() + Duration::from_secs(15);
            let pts: Option<f64> = loop {
                let s = cx.update(|app| {
                    let v = viewer.read(app);
                    (v.pending_seek, v.last_frame_pts)
                });
                if s.0.is_none() {
                    break s.1;
                }
                viewer.update(cx, |v, cx| {
                    let _ = v.tick(cx);
                });
                assert!(
                    Instant::now() < deadline,
                    "复用 seek 落点帧上屏超时 target={target}"
                );
                std::thread::sleep(Duration::from_millis(2));
            };
            landed.push((target, pts.expect("seek 后应有一帧上屏")));
        }
        for (target, pts) in landed {
            assert!(
                (pts - target).abs() < 0.2,
                "复用 seek 后首帧上屏应在落点附近（帧级），target={target} 实得 pts={pts}"
            );
        }
        cx.run_until_parked();
    }

    /// 回归：拖拽（scrub）不得逐 mouse_move 换代/重开音频——换代风暴会饿死中间命令
    /// （generation 入队即推进，worker 只跑最后一条 → 拖拽全程画面冻结）；逐 move 重开
    /// 音频则声音越位/持续出声 + HE-AAC(SBR) 选中流坏帧刷 `env_facs_q`。断言：密集拖拽
    /// 期间视频 `generation` 只涨一次（节流重定位）；`playhead` 恒跟手指落点；松手
    /// `end_scrub` 才走一次完整提交（视频 generation +1）。反向回归护栏：若有人把拖拽
    /// 改回「每 move 一次提交 seek」，本节即红。
    #[gpui::test]
    #[allow(clippy::await_holding_lock)]
    async fn scrub_throttles_video_and_defers_commit(cx: &mut TestAppContext) {
        let _ffmpeg_guard_test = ffmpeg_guard_lock().lock().unwrap_or_else(|e| e.into_inner());
        let ent = cx.update(|app| {
            app.new(|cx| {
                let mut v = blank(fixture(), cx);
                v.playing = true;
                v.duration = 10.0;
                v
            })
        });
        ent.update(cx, |v, cx| v.begin_scrub(0.10, cx));
        let (g0, a0, p0) = cx.update(|app| {
            let v = ent.read(app);
            (
                v.generation.load(Ordering::SeqCst),
                v.audio_gen.load(Ordering::SeqCst),
                v.playhead,
            )
        });
        assert!((p0 - 1.0).abs() < 1e-6, "begin_scrub 应先落点 playhead=1.0");
        assert_eq!(a0, 0, "blank 无音频：begin 不重开音频（audio_gen 恒 0）");
        // 密集拖拽 30 次 move（全部落在 SCRUB_RETARGET_MS 节流窗口内）
        for i in 0..30u32 {
            let f = (0.15f32 + i as f32 * 0.02).min(1.0);
            ent.update(cx, |v, cx| v.update_scrub(f, cx));
        }
        let (g1, p1, scrub) = cx.update(|app| {
            let v = ent.read(app);
            (
                v.generation.load(Ordering::SeqCst),
                v.playhead,
                v.scrubbing,
            )
        });
        assert!(scrub, "拖拽态应持续到松手");
        assert!(
            g1 <= g0 + 1,
            "密集 move 不得逐 move 换代（换代风暴饿死中间命令 → 画面冻结）g0={g0} g1={g1}"
        );
        let expected_p = f64::from((0.15f32 + 29.0 * 0.02).clamp(0.0, 1.0) * 10.0);
        assert!(
            (p1 - expected_p).abs() < 1e-6,
            "playhead 应跟到最后一个落点 {p1} vs {expected_p}"
        );
        // 拖拽中 tick 也不应挪动 playhead（advance_playhead 对 scrubbing 短路 → 不越位）
        ent.update(cx, |v, cx| {
            let _ = v.tick(cx);
        });
        assert!(
            (cx.update(|app| ent.read(app).playhead) - expected_p).abs() < 1e-6,
            "拖拽中时钟不得推进 playhead（音频不越位）"
        );
        // 松手：一次完整提交 seek（视频换代）
        ent.update(cx, |v, cx| v.end_scrub(0.80, cx));
        let (g2, scrub2) = cx.update(|app| {
            let v = ent.read(app);
            (v.generation.load(Ordering::SeqCst), v.scrubbing)
        });
        assert!(
            g2 > g1,
            "松手才走一次完整提交 seek（换代）g1={g1} g2={g2}"
        );
        assert!(!scrub2, "松手后退出拖拽态");
        cx.run_until_parked();
    }

    /// 回归/行为：真拖拽（begin → 密集若干 update → end）——画面随落点逐步跟随（跟手、
    /// 不冻结：每次 move 后都有新帧上屏），松手精确落点并保持播放。旧实现逐 move 换代，
    /// 密集拖拽里中间命令被 generation 入队即覆盖 → 拖拽全程无中间帧（冻结），本节即红。
    /// 真解码 + 真音频设备，需 `target\视频测试.mp4`；缺夹具 SKIP。
    #[gpui::test]
    #[allow(clippy::await_holding_lock)]
    async fn scrub_drag_follows_and_release_lands(cx: &mut TestAppContext) {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/视频测试.mp4");
        if !path.exists() {
            eprintln!("SKIP: missing target/视频测试.mp4（夹具不入库，本机无真视频）");
            return;
        }
        let _ffmpeg_guard_test = ffmpeg_guard_lock().lock().unwrap_or_else(|e| e.into_inner());
        let viewer: gpui::Entity<VideoViewer> =
            cx.update(|app| app.new(|cx| VideoViewer::new(path, cx)));
        let ready = wait_ready(&viewer, cx).await;
        let ready_err = ready.2.clone();
        assert!(ready.0, "首帧应就绪 err={ready_err:?}");
        let duration = cx.update(|app| viewer.read(app).duration);
        viewer.update(cx, |v, cx| v.begin_scrub(0.10, cx));
        // 密集拖拽：连续 move 不睡，模拟快速来回拖；每次 move 之后等新帧上屏（画面在动）
        let mut seen: Vec<Option<f64>> = Vec::new();
        for &f in &[0.22f32, 0.34, 0.46, 0.58, 0.30] {
            viewer.update(cx, |v, cx| v.update_scrub(f, cx));
            let before = cx.update(|app| viewer.read(app).last_frame_pts);
            let deadline = Instant::now() + Duration::from_millis(600);
            let landed: Option<f64> = loop {
                viewer.update(cx, |v, cx| {
                    let _ = v.tick(cx);
                });
                let now = cx.update(|app| viewer.read(app).last_frame_pts);
                if now != before {
                    break now;
                }
                if Instant::now() >= deadline {
                    break None;
                }
                std::thread::sleep(Duration::from_millis(4));
            };
            seen.push(landed);
        }
        let mut distinct: Vec<f64> = seen.iter().flatten().copied().collect();
        distinct.sort_by(|a, b| a.partial_cmp(b).unwrap());
        distinct.dedup();
        assert!(
            distinct.len() >= 2,
            "拖拽中画面应逐步跟随（跟手，非全程冻结），实得 {distinct:?}"
        );
        // 松手：精确落点 + 退出拖拽 + 保持播放
        std::thread::sleep(Duration::from_millis(80));
        viewer.update(cx, |v, cx| v.end_scrub(0.80, cx));
        let target = duration * 0.8;
        let deadline = Instant::now() + Duration::from_secs(15);
        let (landed, scrub_after) = loop {
            let s = cx.update(|app| {
                let v = viewer.read(app);
                (v.pending_seek, v.last_frame_pts, v.scrubbing)
            });
            if s.0.is_none() {
                break (s.1, s.2);
            }
            viewer.update(cx, |v, cx| {
                let _ = v.tick(cx);
            });
            assert!(Instant::now() < deadline, "松手后落点帧上屏超时");
            std::thread::sleep(Duration::from_millis(2));
        };
        assert!(!scrub_after, "松手后应退出拖拽态");
        let pts = landed.expect("松手后应有一帧上屏");
        assert!(
            (pts - target).abs() < 0.3 && pts <= target + 0.1,
            "松手后应落在最终落点附近 target={target:.3} pts={pts:.3}"
        );
        assert!(
            cx.update(|app| viewer.read(app).playing),
            "松手后播放状态保持（提交 seek 不打断播放）"
        );
        cx.run_until_parked();
    }

    /// 边界：**暂停态拖拽**——`playing=false` 时拖拽预览也要工作：泵因 `scrubbing` 持续
    /// 运行（落点帧上屏后 `pending_seek` 已空也不停）、`playhead` 不随时钟走、松手提交
    /// 后按最终落点（暂停态保持静音预览）。
    #[gpui::test]
    #[allow(clippy::await_holding_lock)]
    async fn scrub_paused_drag_keeps_pump_alive(cx: &mut TestAppContext) {
        let _ffmpeg_guard_test = ffmpeg_guard_lock().lock().unwrap_or_else(|e| e.into_inner());
        let ent = cx.update(|app| {
            app.new(|cx| {
                let mut v = blank(fixture(), cx);
                v.playing = false;
                v.duration = 10.0;
                v
            })
        });
        ent.update(cx, |v, cx| v.begin_scrub(0.20, cx));
        // 模拟首个拖拽落点帧已上屏：worker 已喂帧、pending_seek 被 tick 清空
        ent.update(cx, |v, _| v.pending_seek = None);
        assert!(
            cx.update(|app| ent.read(app).pump_wanted()),
            "暂停态拖拽中泵必须持续（scrubbing 真），否则画面不再更新"
        );
        ent.update(cx, |v, cx| v.update_scrub(0.40, cx));
        ent.update(cx, |v, cx| {
            let _ = v.tick(cx);
        });
        let (p, scrub) = cx.update(|app| (ent.read(app).playhead, ent.read(app).scrubbing));
        assert!((p - 4.0).abs() < 1e-6, "暂停态拖拽 playhead 仍跟手指 {p}");
        assert!(scrub, "拖拽态保持");
        ent.update(cx, |v, cx| v.end_scrub(0.50, cx));
        let (p2, scrub2) = cx.update(|app| (ent.read(app).playhead, ent.read(app).scrubbing));
        assert!((p2 - 5.0).abs() < 1e-6, "松后提交到最终落点 {p2}");
        assert!(!scrub2, "退出拖拽态");
        assert!(
            !cx.update(|app| ent.read(app).playing),
            "暂停态拖拽松手不自动播放"
        );
        cx.run_until_parked();
    }

    /// 回归：播放中 seek 不得把墙钟基准的滞留时间一次算进 playhead。音频时钟期间
    /// `advance_playhead` 从不更新 `last_tick`（停留在音频上线前那拍）；seek 重开音频
    /// 的窗口走墙钟时若不对 `last_tick` 重锚，第一拍 `dt = now - last_tick` 会把点击前
    /// 累积的整段播放时长（本例 50s）一口吞进 playhead → 视频突然前跳数秒/加速（根因①）。
    /// 断言：seek 后首拍只前进约一帧幅度，不出尖峰。
    #[gpui::test]
    async fn seek_does_not_jump_playhead_from_stale_last_tick(cx: &mut TestAppContext) {
        let ent = cx.update(|app| app.new(|cx| {
            let mut v = blank(fixture(), cx);
            v.playing = true;
            v.duration = 1000.0;
            // 模拟音频时钟已运行良久：last_tick 停留在 50s 前（音频时钟期间不更新它）
            v.last_tick = Some(Instant::now() - Duration::from_secs(50));
            v
        }));
        ent.update(cx, |v, cx| {
            v.seek_to_fraction(0.4, cx); // 点进度条 seek 到 40%（=400s）
            assert!((v.playhead - 400.0).abs() < 0.01, "seek 落点先到位: {}", v.playhead);
            v.tick(cx); // 音频重开窗口：第一拍走墙钟
        });
        let p = cx.update(|app| ent.read(app).playhead);
        assert!(
            (p - 400.0).abs() < 1.0,
            "seek 后不得把 50s 滞留当一拍前进（落点后尖峰），实得 {p}"
        );
        cx.run_until_parked();
    }

    /// 回归：**单击（down→up 同落点未拖动）合并为一次视频换代**——`begin_scrub` 那代同
    /// target 的预览解码仍在飞（`pending_seek == Some(target)`）时松手不再换代重解第二遍
    ///（现状每次单击 = begin 一遍 + end 一遍同 target 全量 pre-roll 解码，长 GOP 里
    /// 解码量翻倍、显示等第二遍）。断言：begin 后松手 `generation` 不再上涨（一次换代），
    /// `pending_seek` 仍指向该 target（泵等 begin 帧上屏），playhead 已到位。
    #[gpui::test]
    #[allow(clippy::await_holding_lock)]
    async fn click_commit_reuses_inflight_begin_decode(cx: &mut TestAppContext) {
        let _ffmpeg_guard_test = ffmpeg_guard_lock().lock().unwrap_or_else(|e| e.into_inner());
        let ent = cx.update(|app| {
            app.new(|cx| {
                let mut v = blank(fixture(), cx);
                v.playing = true;
                v.duration = 100.0; // 定位虚构时长，begin/end 同分数即可触发合并
                v
            })
        });
        // 单击：down=begin(0.5)、up=end(0.5)，两点同分数/同落点
        ent.update(cx, |v, cx| v.begin_scrub(0.50, cx));
        let (g0, p0, pend0) = cx.update(|app| {
            let v = ent.read(app);
            (
                v.generation.load(Ordering::SeqCst),
                v.playhead,
                v.pending_seek,
            )
        });
        assert!((p0 - 50.0).abs() < 1e-6, "begin 落点到 50s");
        assert_eq!(pend0, Some(50.0), "begin 已投同 target 预览解码（pending）");
        // 松手时 begin 代仍在飞（不落帧、不换代中间态）→ end 应走音频-only 收尾，不换代
        ent.update(cx, |v, cx| v.end_scrub(0.50, cx));
        let (g1, p1, pend1, scrub) = cx.update(|app| {
            let v = ent.read(app);
            (
                v.generation.load(Ordering::SeqCst),
                v.playhead,
                v.pending_seek,
                v.scrubbing,
            )
        });
        assert_eq!(g1, g0, "单击松手不得第二次换代（复用 begin 那代）g={g0}");
        assert!((p1 - 50.0).abs() < 1e-6, "playhead 仍为落点");
        assert_eq!(pend1, Some(50.0), "pending 保留：pump 等 begin 帧上屏");
        assert!(!scrub, "松手后退出拖拽态");
        cx.run_until_parked();
    }

    /// M0 单元（无真解码，走 `blank`）：悬停缓存命中 = 消费一次 + 不换代投解码 —— begin_scrub
    /// 命中取走缓存帧装屏（generation 不动、不挂 pending_seek），end_scrub 同落点走「音频-only
    /// 收尾」不再对同落点重新 seek（第二次换代不发生）。
    #[gpui::test]
    async fn hover_cache_hit_serves_once_and_no_reseek(cx: &mut TestAppContext) {
        let ent = cx.update(|app| app.new(|cx| {
            let mut v = blank(fixture(), cx);
            v.duration = 20.0;
            // 预置一片「恰好命中 10.0s 落点」的悬停缓存（pts=10.04 ≥ 10.0 且差 0.04 ≤ 0.35）
            v.hover_cache = Some(HoverFrame {
                pts: 10.04,
                w: 64,
                h: 48,
                bgra: vec![9u8; (64 * 48 * 4) as usize],
            });
            v
        }));
        let g0 = cx.update(|app| ent.read(app).generation.load(Ordering::SeqCst));
        ent.update(cx, |v, cx| v.begin_scrub(0.50, cx)); // target = 10.0
        let (served, cached, pend, pts) = cx.update(|app| {
            let v = ent.read(app);
            (
                v.served_hover_target,
                v.hover_cache.is_some(),
                v.pending_seek,
                v.last_frame_pts,
            )
        });
        assert!(served == Some(10.0), "命中应服务缓存（记下 10.0s 落点）");
        assert!(!cached, "缓存消费一次即清");
        assert_eq!(pend, None, "缓存命中不应挂 pending seek");
        assert!(
            pts.is_some_and(|p| (p - 10.04).abs() < 1e-9),
            "上屏帧即缓存帧 pts（不得早于落点），实得 {pts:?}"
        );
        let g1 = cx.update(|app| ent.read(app).generation.load(Ordering::SeqCst));
        assert_eq!(g0, g1, "缓存命中不应换代投解码 g={g0}");
        ent.update(cx, |v, cx| v.end_scrub(0.50, cx));
        let (served2, g2, scrub2) = cx.update(|app| {
            let v = ent.read(app);
            (v.served_hover_target, v.generation.load(Ordering::SeqCst), v.scrubbing)
        });
        assert_eq!(g1, g2, "end 同落点不应再 seek（无第二次换代）g={g1}");
        assert_eq!(served2, None, "收尾后 served 标记复位");
        assert!(!scrub2, "松手后退出拖拽态");
        cx.run_until_parked();
    }

    /// M0 回归：悬停命中点击后再**拖离**（60ms 节流窗内，无换代发生）→ 松手落点 ≠ 已服务落点，
    /// 必须落回正常 seek（真实解码新落点），不得停在旧缓存帧。
    #[gpui::test]
    async fn hover_cache_hit_then_drag_away_falls_back_to_seek(cx: &mut TestAppContext) {
        let ent = cx.update(|app| app.new(|cx| {
            let mut v = blank(fixture(), cx);
            v.duration = 20.0;
            v.hover_cache = Some(HoverFrame {
                pts: 10.04,
                w: 64,
                h: 48,
                bgra: vec![9u8; (64 * 48 * 4) as usize],
            });
            v
        }));
        let g0 = cx.update(|app| ent.read(app).generation.load(Ordering::SeqCst));
        ent.update(cx, |v, cx| v.begin_scrub(0.50, cx)); // 命中 10.0s 缓存，未换代
        let g1 = cx.update(|app| ent.read(app).generation.load(Ordering::SeqCst));
        assert_eq!(g1, g0, "begin 命中缓存不换代");
        // 60ms 内拖到 0.60（12.0s）：节流窗内不换代（画面仍是缓存帧，但没有新解码）
        ent.update(cx, |v, _cx| v.last_scrub_retarget = Some(Instant::now()));
        ent.update(cx, |v, cx| v.update_scrub(0.60, cx));
        let (served_after_drag, g2, pend2) = cx.update(|app| {
            let v = ent.read(app);
            (v.served_hover_target, v.generation.load(Ordering::SeqCst), v.pending_seek)
        });
        assert_eq!(served_after_drag, Some(10.0), "拖拽不换代、served 保持 10.0s");
        assert_eq!(g2, g1, "拖拽节流窗内不换代");
        assert_eq!(pend2, None, "拖拽节流窗内无在途解码");
        // 松手在 12.0s：与已服务落点不匹配 → 必须走正常 seek（换代一次），不得停在 10.0s 帧
        ent.update(cx, |v, cx| v.end_scrub(0.60, cx));
        let (served3, g3, playhead3) = cx.update(|app| {
            let v = ent.read(app);
            (v.served_hover_target, v.generation.load(Ordering::SeqCst), v.playhead)
        });
        assert_eq!(served3, None, "收尾后 served 复位");
        assert_eq!(g3, g2 + 1, "拖离后松手应换代一次投新解码");
        assert!((playhead3 - 12.0).abs() < 1e-6, "松手落点在拖拽位置");
        cx.run_until_parked();
    }

    /// M0 单元（无真解码）：命中条件逐项 —— 陈旧（pts < 落点）、过冲/太远（pts − 落点 > 窗）
    /// 一律不命中、缓存保留（点击走正常 seek）；仅在「pts ≥ 落点且差 ≤ 窗」时命中并消费一次。
    #[gpui::test]
    async fn hover_cache_condition_rejects_stale_or_overshoot(cx: &mut TestAppContext) {
        let mk = |pts: f64, cx: &mut gpui::Context<VideoViewer>| {
            let mut v = blank(fixture(), cx);
            v.duration = 20.0;
            v.hover_cache = Some(HoverFrame {
                pts,
                w: 64,
                h: 48,
                bgra: vec![1u8; (64 * 48 * 4) as usize],
            });
            v
        };
        // 陈旧：缓存帧早于点击落点 → 不命中（不得展示早于点击位置的帧）、缓存保留
        let ent = cx.update(|app| app.new(|cx| mk(5.0, cx)));
        let (hit, left) = ent.update(cx, |v, cx| {
            let hit = v.take_hover_cache_if_hit(10.0, cx);
            (hit, v.hover_cache.is_some())
        });
        assert!(!hit, "pts<落点陈旧缓存不命中");
        assert!(left, "未命中不消费缓存");
        // 过冲/太远：缓存帧远超前落点（差 > 0.35s 窗）→ 不命中、保留
        let ent2 = cx.update(|app| app.new(|cx| mk(11.0, cx)));
        let (hit2, left2) = ent2.update(cx, |v, cx| {
            let hit = v.take_hover_cache_if_hit(10.0, cx);
            (hit, v.hover_cache.is_some())
        });
        assert!(!hit2, "超窗缓存不命中");
        assert!(left2, "超窗不消费");
        // 窗内且 ≥ 落点 → 命中并消费
        let ent3 = cx.update(|app| app.new(|cx| mk(10.04, cx)));
        let (hit3, left3) = ent3.update(cx, |v, cx| {
            let hit = v.take_hover_cache_if_hit(10.0, cx);
            (hit, v.hover_cache.is_some())
        });
        assert!(hit3, "窗内命中");
        assert!(!left3, "命中消费一次");
        #[cfg(test)]
        assert!(
            ent3.update(cx, |v, _| v.frame.is_some()),
            "命中后应有帧装屏"
        );
        cx.run_until_parked();
    }

    /// 回归：HE-AAC + 字幕流的 MKV（`target\视频测试字幕.mkv`）反复点进度条（等价连续
    /// seek）——每次 seek 后首帧上屏都落在落点附近、`pending_seek` 及时清空（不冻结/
    /// 不越位）。该文件 MKV demux 落点常大幅回退（音频侧实测 -12s）+ HE-AAC SBR 断链
    /// 坏帧是本次 `env_facs_q` 刷屏根因；解码内核已对 seek 后音频做「落点前包/坏帧
    /// 丢弃」，此处端到端（真 worker + 真解码）验证视频侧同样干净落地。缺夹具 SKIP。
    #[gpui::test]
    #[allow(clippy::await_holding_lock)]
    async fn repeated_seeks_on_he_aac_mkv_land_cleanly(cx: &mut TestAppContext) {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/视频测试字幕.mkv");
        if !path.exists() {
            eprintln!("SKIP: missing target/视频测试字幕.mkv（夹具不入库，本机无 MKV 样例）");
            return;
        }
        let _ffmpeg_guard_test = ffmpeg_guard_lock().lock().unwrap_or_else(|e| e.into_inner());
        let viewer: gpui::Entity<VideoViewer> =
            cx.update(|app| app.new(|cx| VideoViewer::new(path, cx)));
        let mut wait = 0;
        loop {
            let ready = cx.update(|app| {
                let v = viewer.read(app);
                v.frame.is_some() && v.duration > 1.0 && v.pending_seek.is_none()
            });
            if ready {
                break;
            }
            viewer.update(cx, |v, cx| {
                let _ = v.tick(cx);
            });
            cx.background_executor.timer(Duration::from_millis(8)).await;
            wait += 1;
            assert!(wait < 6000, "等待 MKV 首帧/时长超时");
        }
        let duration = cx.update(|app| viewer.read(app).duration);
        let targets = [
            duration * 0.05,
            duration * 0.3,
            duration * 0.62,
            duration * 0.42,
            duration * 0.85,
        ];
        for &target in &targets {
            viewer.update(cx, |v, cx| v.seek(target - v.playhead, cx));
            let deadline = Instant::now() + Duration::from_secs(20);
            let pts: Option<f64> = loop {
                let s = cx.update(|app| {
                    let v = viewer.read(app);
                    (v.pending_seek, v.last_frame_pts)
                });
                if s.0.is_none() {
                    break s.1;
                }
                viewer.update(cx, |v, cx| {
                    let _ = v.tick(cx);
                });
                assert!(Instant::now() < deadline, "MKV seek 落点帧上屏超时 target={target}");
                std::thread::sleep(Duration::from_millis(3));
            };
            let pts = pts.expect("MKV seek 后应有一帧上屏");
            assert!(
                (pts - target).abs() < 0.25,
                "MKV seek 后首帧应在落点附近（帧级），target={target} 实得 pts={pts}"
            );
        }
        cx.run_until_parked();
    }

    /// 回归：seek 后首帧上屏不得整段回放「落点前关键帧」的预滚帧。`VideoReader::seek`
    /// 回拉到目标前关键帧，worker 若全数送帧，主线程排空会把 keyframe→落点整段当
    /// 「迟到帧」快速连放 = seek 后一闪/加速（根因②）。断言：seek 后首个上屏帧的
    /// pts 落在落点附近（≥ 落点 - 一帧），而不是落后整段预滚。真解码、需 `target\视频测试.mp4`。
    #[gpui::test]
    #[allow(clippy::await_holding_lock)] // 持 FFMPEG 串行锁跨 await（同 loads_first_frame）
    async fn seek_lands_on_target_without_preroll_fastforward(cx: &mut TestAppContext) {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/视频测试.mp4");
        if !path.exists() {
            eprintln!("SKIP: missing target/视频测试.mp4（夹具不入库，本机无真视频）");
            return;
        }
        let _ffmpeg_guard_test = ffmpeg_guard_lock().lock().unwrap_or_else(|e| e.into_inner());
        let viewer: gpui::Entity<VideoViewer> =
            cx.update(|app| app.new(|cx| VideoViewer::new(path, cx)));
        // 等首帧上屏且时长已知（真解码异步）
        let mut wait = 0;
        loop {
            let ready = cx.update(|app| {
                let v = viewer.read(app);
                v.frame.is_some() && v.duration > 1.0 && v.pending_seek.is_none()
            });
            if ready {
                break;
            }
            viewer.update(cx, |v, cx| {
                let _ = v.tick(cx);
            });
            cx.background_executor.timer(Duration::from_millis(5)).await;
            wait += 1;
            assert!(wait < 6000, "等待视频首帧/时长超时");
        }
        let target = cx.update(|app| viewer.read(app).duration * 0.62);
        // 播放中 seek 到中部较远落点
        viewer.update(cx, |v, cx| v.seek(target - v.playhead, cx));
        // 持续 tick 直到 seek 落点帧上屏（pending_seek 清空）。worker 真解码重开文件
        // 需真实时间，等待用墙钟 deadline（测试上下文 timer 几乎立即解析，不能作节拍）
        let deadline = Instant::now() + Duration::from_secs(15);
        let landed: Option<f64> = loop {
            let s = cx.update(|app| {
                let v = viewer.read(app);
                (v.pending_seek, v.last_frame_pts)
            });
            if s.0.is_none() {
                break s.1;
            }
            viewer.update(cx, |v, cx| {
                let _ = v.tick(cx);
            });
            assert!(
                Instant::now() < deadline,
                "等待 seek 落点帧上屏超时（pending_seek 未清）"
            );
            std::thread::sleep(Duration::from_millis(2));
        };
        let pts = landed.expect("seek 后应有一帧上屏");
        assert!(
            pts + 0.05 >= target,
            "seek 后首帧上屏应落在落点附近（≥ target-一帧），target={target} 实得 pts={pts}"
        );
        cx.run_until_parked();
    }

    /// 回归：**播放中 seek 落点帧即时上屏**（不挨时钟追量化尾差）。播放中 `pending_seek`
    /// 活跃时，tick 须走 `land_seek` 分支把首个「pts ≥ 落点」帧即时收下；旧门
    /// `scrubbing || !playing` 把播放中 seek 排除 → 落点帧被当「逾线帧」塞 overflow，要等
    /// 墙钟/音钟再走完 ~1 帧量化（25fps→40ms、30fps→33ms）+ 泵节拍才上屏——真机实测
    /// frame2screen 残余 44-50ms 静默、集中在同落点连点第二次点击。断言：单拍 tick 即上屏
    /// 并清 pending，播放态保留、落点帧不进 overflow。
    #[gpui::test]
    async fn playing_seek_lands_on_quantized_first_frame_immediately(cx: &mut TestAppContext) {
        let ent = cx.update(|app| {
            app.new(|cx| {
                let mut v = blank(fixture(), cx);
                v.playing = true;
                v.duration = 1000.0;
                v.playhead = 400.0; // seek 已把 playhead 落到落点
                v.fps = Some((25, 1)); // 帧率已知 → 量化尾差 40ms 量级
                v.pending_seek = Some(400.0);
                v
            })
        });
        // 首个 ≥ 落点帧：pts 恰超落点一帧量化（硬解 skip 后首帧的典型形态）
        let (tx, rx) = std::sync::mpsc::sync_channel::<FrameItem>(2);
        let _ = tx.try_send(frame_of(400.04, 1));
        ent.update(cx, |v, cx| {
            v.rx = Some(rx);
            let inst = v.tick(cx);
            assert!(inst, "播放中 seek 首拍即应把落点帧上屏（旧门等时钟才上 → 44-50ms 静默）");
            assert_eq!(v.pending_seek, None, "落点帧上屏即清 pending");
            assert_eq!(v.last_frame_pts, Some(400.04));
            assert!(v.playing, "播放态必须保留");
            assert!(v.overflow.is_none(), "落点帧不得被当逾线帧塞 overflow");
        });
        cx.run_until_parked();
    }

    /// 性能基准（纯代码、不开 GUI，真解码 + 真墙钟）：模拟「点击进度条」seek——begin_scrub
    /// 与 end_scrub 同分数 → 走单击合并路径（仅一次换代，正是用户真实点击链路）——量「点击那刻
    /// → 首个新帧上屏（pending_seek 清空）」的端到端墙钟。测试上下文 timer 是 fake-clock
    ///（几乎立即解析），泵拍不在测量里——恰好对应用户基线「瓶颈是解码（seek2first）不是上屏」
    /// 的现状，数字 ≈ seek2first 主导段，供对标真机 [SEEK] 行 / 后续预解码/硬解 before/after。
    /// 每落点打一行与 GUI [SEEK] **同字段/同序/同单位**的 8 字段行（前缀 [SEEKBENCH]），
    /// 取自 `take_seek_diag_test`（tick 首帧上屏沉淀）；fake-clock 无泵拍 → frame2screen≈0。
    /// 夹具 `target\视频测试字幕.mkv`（长 GOP）；缺夹具 SKIP（AGENTS.md §5）。
    #[gpui::test]
    #[allow(clippy::await_holding_lock)]
    async fn bench_click_seek_land_frame_wall(cx: &mut TestAppContext) {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/视频测试字幕.mkv");
        if !path.exists() {
            eprintln!("SKIP: missing target/视频测试字幕.mkv（长 GOP 夹具不入库）");
            return;
        }
        let _ffmpeg_guard_test = ffmpeg_guard_lock().lock().unwrap_or_else(|e| e.into_inner());
        // 软解锚点：强制软解（解码偏好旋钮 1）——[SEEKBENCH] 软解基准（65-1876ms）以此为
        // 基线对表；RAII 复位把任何 panic 中断也还原，防污染后续硬解基准。
        let _decode_mode = crate::decode::DecodeModeGuard::new(1);
        let viewer: gpui::Entity<VideoViewer> =
            cx.update(|app| app.new(|cx| VideoViewer::new(path, cx)));
        // 等首帧上屏 + 时长已知（真解码异步）；沿用既有姿势
        let mut wait = 0;
        loop {
            let ready = cx.update(|app| {
                let v = viewer.read(app);
                v.frame.is_some() && v.duration > 1.0 && v.pending_seek.is_none()
            });
            if ready {
                break;
            }
            viewer.update(cx, |v, cx| {
                let _ = v.tick(cx);
            });
            cx.background_executor.timer(Duration::from_millis(5)).await;
            wait += 1;
            assert!(wait < 6000, "等待首帧/时长超时");
        }
        let duration = cx.update(|app| viewer.read(app).duration);
        // 目标秒（贴近真机点过的、覆盖短/中/长 GOP）；钳 [0.05, duration-2s] 避开近结尾 EOF
        let raw = [0.11f64, 72.0, 191.0, 354.0, 447.0, 900.0, 1335.0, 1640.0];
        let hi = (duration - 2.0).max(0.05);
        let mut rows: Vec<(f64, f64)> = Vec::with_capacity(raw.len());
        for &sec in &raw {
            let sec = sec.clamp(0.05, hi);
            // 用户单击：down=begin_scrub(f)、up=end_scrub(f)，同分数 → 合并路径（单次换代解码）
            let fraction = (sec / duration).clamp(0.0, 1.0) as f32;
            let t0 = Instant::now();
            viewer.update(cx, |v, cx| v.begin_scrub(fraction, cx));
            viewer.update(cx, |v, cx| v.end_scrub(fraction, cx));
            let target = cx.update(|app| viewer.read(app).playhead);
            // 等「首个新帧上屏」：pending_seek 只在 tick 内某帧上屏时清空 → 即落地信号。
            // 测试 ctx 里 worker 真解码，等待用墙钟 deadline（fake timer 不能作节拍）。
            let deadline = Instant::now() + Duration::from_secs(30);
            let landed = loop {
                viewer.update(cx, |v, cx| {
                    let _ = v.tick(cx);
                });
                let s = cx.update(|app| {
                    let v = viewer.read(app);
                    (v.pending_seek, v.last_frame_pts)
                });
                if s.0.is_none() {
                    break s.1;
                }
                assert!(
                    Instant::now() < deadline,
                    "点击 seek 落点帧上屏超时（target={target:.2}s）"
                );
                std::thread::sleep(Duration::from_millis(1));
            };
            let ms = t0.elapsed().as_secs_f64() * 1e3;
            // 与 GUI [SEEK] 直接对表：取 tick 首帧上屏沉淀的 8 字段快照（slot 语义同 [SEEK]，
            // total=点击→首帧上屏、seek2first=worker 解码段、frame2screen≈0 因无泵拍）。
            // 缺快照（落点帧上屏瞬间槽尚未写入的罕见时序）回退兜底行，不 panic。
            match take_seek_diag_test() {
                Some((total, reuse, regress, skipped, s2f, f2s, target, firstpts)) => {
                    rows.push((target, total));
                    eprintln!(
                        "[SEEKBENCH] total={:.1}ms reuse={} regress={:+.2}s skipped={} seek2first={:.1}ms frame2screen={:.1}ms target={:.2}s firstpts={:.2}s",
                        total, reuse, regress, skipped, s2f, f2s, target, firstpts,
                    );
                }
                None => {
                    rows.push((target, ms));
                    eprintln!(
                        "[SEEKBENCH] target={target:.1}s click2frame={ms:.1}ms landed={:?}（无 diag 快照，兜底）",
                        landed.map(|p| format!("{p:.2}s"))
                    );
                }
            }
        }
        // 摘要：min / median / max（total 口径）。首目标也命中 reader 复用（文件在等待首帧
        // 阶段已 open，同 path+cap）≡ 真机 [SEEK] reuse=1；含 ≤1ms 轮询粒度；frame2screen
        // 假时钟≈0 故 total≈seek2first。性能基准不断言硬性指标。
        let mut sorted: Vec<f64> = rows.iter().map(|r| r.1).collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        eprintln!(
            "[SEEKBENCH] 摘要 n={} min={:.1}ms median={:.1}ms max={:.1}ms（total 口径，全 reuse=1；含 ≤1ms 轮询粒度）",
            sorted.len(),
            sorted[0],
            sorted[sorted.len() / 2],
            sorted[sorted.len() - 1],
        );
        cx.run_until_parked();
    }

    /// 性能基准（纯代码、不开 GUI，真硬解 + 真墙钟）——SAFE 硬解 seek 延迟。
    /// 解码偏好旋钮置 2（强制 `open_scaled_hw`：FFmpeg 自有设备，**无调用方注入**，worker 用
    /// `skip_to_bgra_into` 做 fast preroll——跳帧免转换）→ 跑与
    /// `bench_click_seek_land_frame_wall` 同构的深 GOP 落点（含重回退 ≈−9s 与同点复用），
    /// 墙钟从请求到首帧 ≥ 落点上屏，断言 `firstpts ≥ target`。
    /// [SEEKBENCHHW] 与 GUI [SEEK] 同 8 字段/同序/同单位 + `hw=` 标志（1=硬解接管、
    /// 0=内核软回退——后者数字与其软解基准同量级，便于对表）。
    #[gpui::test]
    #[allow(clippy::await_holding_lock)]
    #[cfg(target_os = "windows")]
    async fn bench_seek_hw_d3d11_wall(cx: &mut TestAppContext) {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/视频测试字幕.mkv");
        if !path.exists() {
            eprintln!("SKIP: missing target/视频测试字幕.mkv（长 GOP 夹具不入库）");
            return;
        }
        let _ffmpeg_guard_test = ffmpeg_guard_lock().lock().unwrap_or_else(|e| e.into_inner());
        // 强制硬件优先（open_scaled_hw；RAII 复位守卫把任何 panic 中断也还原，防污染后续软解测试）
        let _decode_mode = crate::decode::DecodeModeGuard::new(2);
        hw_engaged_reset();
        let viewer: gpui::Entity<VideoViewer> =
            cx.update(|app| app.new(|cx| VideoViewer::new(path, cx)));
        let mut wait = 0;
        loop {
            let ready = cx.update(|app| {
                let v = viewer.read(app);
                v.frame.is_some() && v.duration > 1.0 && v.pending_seek.is_none()
            });
            if ready {
                break;
            }
            viewer.update(cx, |v, cx| {
                let _ = v.tick(cx);
            });
            cx.background_executor.timer(Duration::from_millis(5)).await;
            wait += 1;
            assert!(wait < 6000, "等待 HW 首帧/时长超时");
        }
        eprintln!(
            "[SEEKBENCHHW] 模式=hw 硬解接管={}",
            if hw_engaged() { 1 } else { 0 }
        );
        let duration = cx.update(|app| viewer.read(app).duration);
        // 深 GOP 落点（hover 基准实测深回退 regress≈−5.3/−9.9/−10.2/−3.0s）+ 末位同点复用
        let raw = [191.0f64, 354.0, 447.0, 1335.0, 354.0];
        let hi = (duration - 2.0).max(0.05);
        let mut rows: Vec<(f64, f64)> = Vec::with_capacity(raw.len());
        for &sec in &raw {
            let sec = sec.clamp(0.05, hi);
            let fraction = (sec / duration).clamp(0.0, 1.0) as f32;
            let t0 = Instant::now();
            viewer.update(cx, |v, cx| v.begin_scrub(fraction, cx));
            viewer.update(cx, |v, cx| v.end_scrub(fraction, cx));
            let target = cx.update(|app| viewer.read(app).playhead);
            let deadline = Instant::now() + Duration::from_secs(30);
            let landed = loop {
                viewer.update(cx, |v, cx| {
                    let _ = v.tick(cx);
                });
                let s = cx.update(|app| {
                    let v = viewer.read(app);
                    (v.pending_seek, v.last_frame_pts)
                });
                if s.0.is_none() {
                    break s.1;
                }
                assert!(
                    Instant::now() < deadline,
                    "HW seek 落点帧上屏超时（target={target:.2}s）"
                );
                std::thread::sleep(Duration::from_millis(1));
            };
            let firstpts = landed.expect("HW seek 首帧应上屏");
            assert!(
                firstpts >= target,
                "HW seek 首帧不得早于落点 target={target:.2}s firstpts={firstpts:.2}s"
            );
            let ms = t0.elapsed().as_secs_f64() * 1e3;
            // 与 GUI [SEEK] 直接对表：取 tick 沉淀的 8 字段快照（seek2first ≈ seek 预滚墙钟）
            let (total, reuse, regress, skipped, s2f, f2s, tgt, fp) = match take_seek_diag_test() {
                Some(x) => x,
                None => (ms, 1u8, f64::NAN, 0u32, ms, 0.0, target, firstpts),
            };
            rows.push((target, total));
            eprintln!(
                "[SEEKBENCHHW] total={:.1}ms reuse={} regress={:+.2}s skipped={} seek2first={:.1}ms frame2screen={:.1}ms target={:.2}s firstpts={:.2}s hw={}",
                total, reuse, regress, skipped, s2f, f2s, tgt, fp, if hw_engaged() { 1 } else { 0 },
            );
        }
        let mut sorted: Vec<f64> = rows.iter().map(|r| r.1).collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        eprintln!(
            "[SEEKBENCHHW] 摘要 n={} min={:.1}ms median={:.1}ms max={:.1}ms（total 口径，hw={}）",
            sorted.len(),
            sorted[0],
            sorted[sorted.len() / 2],
            sorted[sorted.len() - 1],
            if hw_engaged() { 1 } else { 0 },
        );
        cx.run_until_parked();
    }

    /// SAFE 硬解预滚回归（纯代码、不开 GUI）：`open_scaled_hw`（FFmpeg 自有设备）打开长 GOP
    /// 夹具，`skip_to_bgra_into` 对深落点做 seek 预滚——断言返回首帧 pts ≥ target、BGRA 缓冲
    /// 实际写入（非纯零像素）、跳过帧数 > 0、reader 硬解接管；随后 `next_frame_bgra_into`
    /// 续读 pts > target（播放稳态接续）。替代旧 M2e `hw_frame_roundtrips_into_scene`
    ///（paint 直通已废除、无探针可对）。缺夹具 / 无硬解能力 SKIP。
    #[gpui::test]
    #[allow(clippy::await_holding_lock)]
    #[cfg(target_os = "windows")]
    async fn hw_skip_to_bgra_seek_preroll_lands_on_target(cx: &mut TestAppContext) {
        let _ffmpeg_guard_test = ffmpeg_guard_lock().lock().unwrap_or_else(|e| e.into_inner());
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/视频测试字幕.mkv");
        if !path.exists() {
            eprintln!("SKIP: missing target/视频测试字幕.mkv（长 GOP 夹具不入库）");
            return;
        }
        // 解码偏好旋钮置 2（文档化本测试的 hw 前提；RAII 复位防污染后续软解测试）
        let _decode_mode = crate::decode::DecodeModeGuard::new(2);
        let mut r = crate::decode::VideoReader::open_scaled_hw(&path, 1920)
            .expect("open_scaled_hw 应打开成功（内核含软回退）");
        if !r.is_hw() {
            eprintln!("SKIP: 本机无 D3D11VA/DXVA2 硬解能力（open_scaled_hw 回软解）");
            return;
        }
        assert!(r.is_hw(), "强制 hw 模式下 reader 应已硬解接管");
        let p = r.params();
        assert!(p.duration > 1.0, "夹具时长应已知，实得 {:.2}s", p.duration);
        // 深 GOP 落点（保证 pre-roll 跳帧 > 0）
        let target = (p.duration * 0.66).min(p.duration - 2.0).max(0.05);
        let mut buf = vec![0u8; r.source_cap()];
        let hit = r
            .skip_to_bgra_into(target, &mut buf)
            .expect("skip_to_bgra_into 不应报错")
            .expect("深落点应命中（非 EOF）");
        let (pts0, w, h, skipped) = hit;
        assert!(
            pts0 >= target,
            "硬解预滚首帧不得早于落点 target={target:.2}s pts={pts0:.2}s"
        );
        assert!(skipped > 0, "深 GOP 落点应有跳帧，实得 {skipped}");
        assert!(w > 0 && h > 0, "返回尺寸应为正，实得 {w}x{h}");
        let need = (w as usize) * (h as usize) * 4;
        assert!(need <= buf.len(), "缓冲应够装首帧 need={need} cap={}", buf.len());
        assert!(
            buf[..need].iter().any(|&b| b != 0),
            "首帧 BGRA 缓冲应为非平凡像素（非纯零）"
        );
        // 播放稳态接续：后续帧 pts > 首帧
        let next = r
            .next_frame_bgra_into(&mut buf)
            .expect("续读不应报错")
            .expect("首帧后应还有帧");
        assert!(
            next.0 > pts0,
            "续读帧 pts 应 > 首帧 pts {pts0:.2}s，实得 {:.2}s",
            next.0
        );
        eprintln!(
            "[HW skiptobgra] target={target:.2}s firstpts={pts0:.2}s skipped={skipped} {w}x{h} 续读pts={:.2}s",
            next.0
        );
        cx.run_until_parked();
    }

    /// 双设备 GPU 直通主回归（纯代码、不开 GUI，真实 D3D11 设备 + 真硬解 + 真墙钟）：
    /// 显式开启直通（[`DualPathGuard`]）→ auto 模式 worker 打开 `target\视频测试.mp4`
    /// 应进入双设备（`dual_engaged`）→ 等首 GPU 帧上屏 → **槽池循环复用**：步进 30 帧
    ///（0.5s/步，远超共享槽池上限 8）断言 pts 单调、`gpu_frame` 持续在屏（若槽池不回收而
    /// 耗尽会触发「池耗尽」错误 → 误回退 SAFE BGRA —— 本测为「pool recycling over ~30 frames
    /// no pool exceeded」的实证）→ 开测试窗口强制 draw：canvas paint 回调里
    /// `paint_external_yuv` 收到当前共享句柄（`take_hw_paint_probe` 断言非 0 且等于在屏帧）。
    /// **无 SHR 门控**：驱动是否支持「解码数组纹理带 SHADER_RESOURCE」只作诊断打印，不改变
    /// 直通断言——包括 Intel Arc（能力 false）在内，只要双设备能接管就断言直通生效。
    #[gpui::test]
    #[allow(clippy::await_holding_lock)]
    #[cfg(target_os = "windows")]
    async fn gpu_passthrough_first_frame_paints_shared_handle(cx: &mut TestAppContext) {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/视频测试.mp4");
        if !path.exists() {
            eprintln!("SKIP: missing target/视频测试.mp4（夹具不入库）");
            return;
        }
        let _ffmpeg_guard_test = ffmpeg_guard_lock().lock().unwrap_or_else(|e| e.into_inner());
        let _dual = DualPathGuard::new();
        let _decode_mode = crate::decode::DecodeModeGuard::new(0); // auto（直通优先）
        let _fault_bl = FaultAndBlGuard; // 黑名单/注入都不带进本测
        // 机器能力**诊断**（仅建 worker 设备失败才 SKIP；能力值只打印、不门控直通）
        let shr_supported = match dual_gate_capability() {
            Some(v) => v,
            None => {
                eprintln!("SKIP: 本机连 worker 自有 D3D11 设备都建不起（双设备不可测）");
                return;
            }
        };
        eprintln!("[GPU passthrough] 本机 D3D11VA 平面 SRV 采样能力 = {shr_supported}（诊断，非直通门控）");
        dual_engaged_reset();
        hw_engaged_reset();
        let viewer: gpui::Entity<VideoViewer> =
            cx.update(|app| app.new(|cx| VideoViewer::new(path, cx)));
        // 等首 GPU 帧上屏 + 时长已知 + seek 落地（真解码异步）
        let mut wait = 0;
        loop {
            let ready = cx.update(|app| {
                let v = viewer.read(app);
                v.has_frame() && v.duration > 1.0 && v.pending_seek.is_none()
            });
            if ready {
                break;
            }
            viewer.update(cx, |v, cx| {
                let _ = v.tick(cx);
            });
            cx.background_executor.timer(Duration::from_millis(5)).await;
            wait += 1;
            assert!(wait < 6000, "等待双设备首帧上屏超时");
        }
        if !dual_engaged() {
            // 本机无 D3D11VA h264 硬解 / 解码器不支持外部设备注入 / 语义级不支持 → 直通必然
            // 未接管；SKIP（直通不可测时 SAFE 回退路径由 `gpu_passthrough_fallback_to_safe` 兜底）
            eprintln!("SKIP: 本机无法双设备直通（无 D3D11VA h264 硬解或外部注入不支持），回退 SAFE 路径");
            return;
        }
        assert!(
            !cx.update(|app| viewer.read(app).load_error.is_some()),
            "双设备首帧期不应报告加载错误"
        );
        // 槽池循环复用：步进 30 帧（0.5s/步）→ 每次主线程装新帧归还旧槽 → worker reap 复用；
        // 断言 pts 单调 + 不落回 SAFE（`dual_engaged` 不被回退覆盖）+ `gpu_frame` 持续在屏。
        let mut last_pts = -1.0f64;
        let mut gpu_seen = false;
        let mut advanced = 0u32;
        for _ in 0..30 {
            viewer.update(cx, |v, cx| {
                v.playhead += 0.5;
                let _ = v.tick(cx);
            });
            // 让真解码线程有空逐帧出帧（fake 时钟只推进调度，worker 是真实线程）
            cx.background_executor.timer(Duration::from_millis(8)).await;
            let (pts, gpu) = cx.update(|app| {
                let v = viewer.read(app);
                (v.last_frame_pts.unwrap_or(last_pts), v.gpu_frame.is_some())
            });
            gpu_seen |= gpu;
            assert!(
                pts >= last_pts - 1e-6,
                "GPU 直通帧 pts 应单调: {pts:.2} < {last_pts:.2}"
            );
            last_pts = pts;
            advanced += 1;
        }
        assert!(
            gpu_seen && dual_engaged(),
            "步进期间 GPU 帧应持续上屏且直通未被回退（槽池循环复用无 'pool exceeded'），gpu_seen={gpu_seen}"
        );
        let current_handle = cx
            .update(|app| viewer.read(app).gpu_frame.as_ref().map(|f| f.shared_handle))
            .expect("GPU 帧仍在屏，应有共享句柄");
        // 开测试窗口承载同一 viewer 并强制 draw：canvas paint 回调调 paint_external_yuv 写探针
        struct ViewerHost {
            viewer: gpui::Entity<VideoViewer>,
        }
        impl gpui::Render for ViewerHost {
            fn render(
                &mut self,
                _window: &mut gpui::Window,
                _cx: &mut gpui::Context<Self>,
            ) -> impl gpui::IntoElement {
                self.viewer.clone()
            }
        }
        let (_host, vcx) = cx.add_window_view(|_window, _cx| ViewerHost {
            viewer: viewer.clone(),
        });
        vcx.update(|window, cx| {
            window.draw(cx).clear(cx);
        });
        let probe_handle = crate::hw::take_hw_paint_probe();
        assert_ne!(
            probe_handle, 0,
            "paint_external_yuv 应在 canvas paint 里被调用（探针记录 shared_handle）"
        );
        assert_eq!(
            probe_handle, current_handle,
            "外部 YUV 精灵应使用当前 GPU 上屏共享句柄（probe={probe_handle:#x} vs 当前 {current_handle:#x}）"
        );
        eprintln!(
            "[GPU passthrough] 探针 shared_handle={probe_handle:#x} 步进 {advanced} 帧 pts 至 {last_pts:.2}s（槽池循环复用，无 pool exceeded）"
        );
        cx.run_until_parked();
    }

    /// 机器自适应回归（纯代码、不开 GUI）：开启直通（[`DualPathGuard]`）+ auto 模式 +
    /// 真实夹具 `target\视频测试.mp4`。**无 SHR 门控**——驱动能力只分两种断言口径：
    /// - 能力 false（**Intel Arc 回归测试**）：默认应支持平面 SRV 采样，但 Arc 实测拒绝
    ///   `DECODER|SHADER_RESOURCE` 组合 → 以前门控停用双设备；现在恒 DECODER-only 解码池，
    ///   故**必须断言直通照常生效**：`dual_engaged()==true`、首帧为 GPU 帧
    ///   （`gpu_frame` Some）、步进 pts 单调（`SharedCopier::copy` 的单调用拷贝在
    ///   DECODER-only 源上照常工作）；
    /// - 能力 true → 双设备可走（既有 `gpu_passthrough_first_frame_paints_shared_handle`
    ///   已断言直通细节），本测**不额外断言**直通细节、只确认能出首帧。
    #[gpui::test]
    #[allow(clippy::await_holding_lock)]
    #[cfg(target_os = "windows")]
    async fn gpu_passthrough_machine_adaptive_gate(cx: &mut TestAppContext) {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/视频测试.mp4");
        if !path.exists() {
            eprintln!("SKIP: missing target/视频测试.mp4（夹具不入库）");
            return;
        }
        let _ffmpeg_guard_test = ffmpeg_guard_lock().lock().unwrap_or_else(|e| e.into_inner());
        let _dual = DualPathGuard::new();
        let _decode_mode = crate::decode::DecodeModeGuard::new(0); // auto（直通优先）
        let _fault_bl = FaultAndBlGuard;
        // 机器能力**诊断**（仅建 worker 设备失败才 SKIP；能力值不门控、只区分断言口径）
        let shr_supported = match dual_gate_capability() {
            Some(v) => v,
            None => {
                eprintln!("SKIP: 本机连 worker 自有 D3D11 设备都建不起（双设备不可测）");
                return;
            }
        };
        eprintln!("[GPU adaptive] 本机 D3D11VA 平面 SRV 采样能力 = {shr_supported}（诊断，非直通门控）");
        dual_engaged_reset();
        hw_engaged_reset();
        let viewer: gpui::Entity<VideoViewer> =
            cx.update(|app| app.new(|cx| VideoViewer::new(path, cx)));
        // 等首帧上屏（真解码异步；两分支都要求首帧可用）
        let mut wait = 0;
        loop {
            let ready = cx.update(|app| {
                let v = viewer.read(app);
                v.has_frame() && v.duration > 1.0 && v.pending_seek.is_none()
            });
            if ready {
                break;
            }
            viewer.update(cx, |v, cx| {
                let _ = v.tick(cx);
            });
            cx.background_executor.timer(Duration::from_millis(5)).await;
            wait += 1;
            assert!(wait < 6000, "等待自适应回归首帧上屏超时");
        }
        assert!(
            !cx.update(|app| viewer.read(app).load_error.is_some()),
            "首帧期不应报告加载错误"
        );
        if !shr_supported {
            // (a) 能力 false（本机 Intel Arc）：无 SHR 门控 → 断言直通**照常生效**
            if !dual_engaged() {
                eprintln!("SKIP: 本机无 D3D11VA h264 硬解/外部注入（能力 false 且直通未接管）——非 Arc 目标机器");
                cx.run_until_parked();
                return;
            }
            assert!(
                dual_engaged(),
                "能力 false（Arc）无门控后双设备直通应照常生效（dual_engaged=true）"
            );
            eprintln!(
                "[GPU adaptive] 能力 false（Arc）：双设备直通生效，hw_engaged={}",
                hw_engaged()
            );
            let (first_gpu, first_frame) = cx.update(|app| {
                let v = viewer.read(app);
                (v.gpu_frame.is_some(), v.frame.is_some())
            });
            assert!(
                first_gpu,
                "能力 false（Arc）首帧应为 GPU 帧（gpu_frame Some，直通接管）"
            );
            let mut last_pts = -1.0f64;
            for _ in 0..10 {
                viewer.update(cx, |v, cx| {
                    v.playhead += 0.5;
                    let _ = v.tick(cx);
                });
                cx.background_executor.timer(Duration::from_millis(8)).await;
                let (pts, gpu) = cx.update(|app| {
                    let v = viewer.read(app);
                    (v.last_frame_pts.unwrap_or(last_pts), v.gpu_frame.is_some())
                });
                assert!(gpu, "能力 false（Arc）直通路径帧应持续为 GPU 帧");
                assert!(
                    pts >= last_pts - 1e-6,
                    "直通帧 pts 应单调: {pts:.2} < {last_pts:.2}"
                );
                last_pts = pts;
            }
            eprintln!(
                "[GPU adaptive] 能力 false（Arc）：直通占用、GPU 帧持续上屏（hw_engaged={}，first_frame={first_frame}，pts 至 {last_pts:.2}s）",
                hw_engaged()
            );
        } else {
            // (b) 能力 true：双设备由既有直通测试断言（本测不做额外直通细节断言）
            eprintln!(
                "[GPU adaptive] 能力 true：双设备直通由 gpu_passthrough_first_frame_paints_shared_handle 断言（本测仅确认首帧可用）"
            );
        }
        cx.run_until_parked();
    }

    /// 零回读双设备直通 vs SAFE 硬解回读的**性能证据**（纯代码、不开 GUI、真 D3D11 设备 +
    /// 真墙钟；非回归基准，**不硬断言快慢**——驱动时序抖动多，只打印可解析数字供用户对表）：
    /// ① mechanism proof：auto 模式经 viewer 打开 `target\视频测试.mp4` → 断言
    ///   `dual_engaged()` / `hw_engaged()` / 首帧 `gpu_frame` 上屏（= 直通接管开关真开；
    ///   Intel Arc 亦应 true——本机即 Arc 回归机）；
    /// ② 逐帧墙钟对照：直通 leg = 外部设备 `open_hw` + [`SharedCopier::copy`]（GPU 侧零回读
    ///   拷贝，仅 `AcquireSync→Copy→Flush→ReleaseSync`）；SAFE leg = `open_scaled_hw`（FFmpeg
    ///   自有设备）+ `next_frame_bgra_into`（`av_hwframe_transfer_data` 回读 + sws 转 BGRA）。
    ///   各剔除前若干暖机帧采样 N 帧，报 min/中位/均值 ms·帧 与 SAFE/copy 比值。
    /// 缺夹具 / 无 D3D11 设备 / 外部注入解码打不开 → SKIP（机制证明缺口径时不算 PASS）。
    #[gpui::test]
    #[allow(clippy::await_holding_lock)]
    #[cfg(target_os = "windows")]
    async fn bench_dual_device_copy_vs_safe_readback(cx: &mut TestAppContext) {
        use std::time::Instant;
        use windows::core::Interface as _;
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/视频测试.mp4");
        if !path.exists() {
            eprintln!("SKIP: missing target/视频测试.mp4（夹具不入库）");
            return;
        }
        let _ffmpeg_guard_test = ffmpeg_guard_lock().lock().unwrap_or_else(|e| e.into_inner());
        let Some(worker) = crate::hw::create_worker_d3d11_device() else {
            eprintln!("SKIP: D3D11 硬件/WARP 设备均不可用");
            return;
        };

        // ① mechanism proof：auto 直通接管（Intel Arc 无 SHR 也必须接管）
        let _dual = DualPathGuard::new();
        let _decode_mode = crate::decode::DecodeModeGuard::new(0); // auto（直通优先）
        let _fault_bl = FaultAndBlGuard;
        dual_engaged_reset();
        hw_engaged_reset();
        let viewer: gpui::Entity<VideoViewer> =
            cx.update(|app| app.new(|cx| VideoViewer::new(path.clone(), cx)));
        let mut wait = 0;
        loop {
            let ready = cx.update(|app| {
                let v = viewer.read(app);
                v.has_frame() && v.duration > 1.0 && v.pending_seek.is_none()
            });
            if ready {
                break;
            }
            viewer.update(cx, |v, cx| {
                let _ = v.tick(cx);
            });
            cx.background_executor.timer(Duration::from_millis(5)).await;
            wait += 1;
            assert!(wait < 6000, "等待直通首帧上屏超时");
        }
        let gpu_on_screen = cx.update(|app| {
            let v = viewer.read(app);
            v.gpu_frame.is_some()
        });
        if !dual_engaged() {
            // 机器自适应：真实 D3D11VA h264 硬解未接管（无 h264 VA / 外部注入不支持）→
            // SKIP 不 FAIL（同 `gpu_passthrough_machine_adaptive_gate` 的 SKIP 风格）
            eprintln!(
                "SKIP: 本机双设备未接管（dual_engaged=false hw_engaged={}）——非 Arc 目标机器",
                hw_engaged()
            );
            cx.run_until_parked();
            return;
        }
        assert!(
            dual_engaged() && hw_engaged() && gpu_on_screen,
            "直通应接管并上 GPU 帧（dual_engaged={} hw_engaged={} gpu_frame={gpu_on_screen}）",
            dual_engaged(),
            hw_engaged()
        );
        eprintln!(
            "[PASSTHROUGH] mechanism: dual_engaged={} hw_engaged={} gpu_frame={gpu_on_screen} 设备={}",
            dual_engaged(),
            hw_engaged(),
            worker.kind
        );
        // 停掉 viewer 泵（generation 自退）再进计时，避免后台 worker 抢 FFmpeg 影响墙钟
        drop(viewer);
        cx.run_until_parked();
        std::thread::sleep(std::time::Duration::from_millis(50));

        // ② 直通 leg：外部设备 open_hw + SharedCopier::copy
        const N: u32 = 200;
        const WARMUP: u32 = 10;
        let raw_dev = worker.device.as_raw() as usize;
        let raw_ctx = worker.context.as_raw() as usize;
        let mut r_hw = match crate::decode::VideoReader::open_hw(&path, raw_dev, raw_ctx) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("SKIP: 外部设备注入解码打开失败（无 h264 硬解?）: {e:#}");
                return;
            }
        };
        assert!(r_hw.is_hw(), "open_hw 直通 reader 必须真硬解（mechanism proof）");
        let mut copier = crate::hw::SharedCopier::new(worker);
        let mut p_times: Vec<f64> = Vec::with_capacity(N as usize);
        let mut n: u32 = 0;
        loop {
            match r_hw.next_frame_hw() {
                Ok(Some(fr)) => {
                    let t = Instant::now();
                    let copied = copier.copy(fr.w, fr.h, fr.tex, fr.array_index);
                    let dt = t.elapsed().as_secs_f64() * 1e3;
                    crate::decode::VideoReader::release_hw_frame(fr.frame_ptr);
                    let cf = copied.expect("直通 GPU 拷贝失败");
                    // 立即回池：直通测量只关心单帧 `SharedCopier::copy` 墙钟，不消费帧
                    copier.recycle(cf.shared_handle);
                    if n >= WARMUP {
                        assert_ne!(cf.shared_handle, 0, "直通拷贝应返回非 0 共享句柄");
                        p_times.push(dt);
                    }
                    n += 1;
                }
                Ok(None) => {
                    // 夹具足够长一般到不了这；到了就 seek 回 0 凑够采样
                    r_hw.seek(0.0).unwrap();
                }
                Err(e) => panic!("直通解码失败: {e:#}"),
            }
            if n >= N + WARMUP {
                break;
            }
        }

        // SAFE leg：open_scaled_hw + next_frame_bgra_into（回读+sws）
        let mut r_safe = crate::decode::VideoReader::open_scaled_hw(&path, 1920)
            .expect("SAFE 硬解打开失败");
        eprintln!(
            "[SAFE] 硬解接管={}（1=FFmpeg 自有设备 D3D11VA 回读+sws；0=内核回软解）",
            if r_safe.is_hw() { 1 } else { 0 }
        );
        let mut out = vec![0u8; r_safe.source_cap()];
        let mut s_times: Vec<f64> = Vec::with_capacity(N as usize);
        let mut n: u32 = 0;
        loop {
            let t = Instant::now();
            match r_safe.next_frame_bgra_into(&mut out) {
                Ok(Some(_)) => {
                    let dt = t.elapsed().as_secs_f64() * 1e3;
                    if n >= WARMUP {
                        s_times.push(dt);
                    }
                    n += 1;
                }
                Ok(None) => r_safe.seek(0.0).unwrap(),
                Err(e) => panic!("SAFE 解码失败: {e:#}"),
            }
            if n >= N + WARMUP {
                break;
            }
        }

        let stat = |v: &mut Vec<f64>| -> (f64, f64, f64) {
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let (min, med) = (v[0], v[v.len() / 2]);
            (min, med, v.iter().sum::<f64>() / v.len() as f64)
        };
        let (p_min, p_med, p_mean) = stat(&mut p_times);
        let (s_min, s_med, s_mean) = stat(&mut s_times);
        eprintln!(
            "[PASSTHROUGH] copy={p_med:.3}ms/帧（中位; min={p_min:.3} 均值={p_mean:.3}; n={}）",
            p_times.len()
        );
        eprintln!(
            "[SAFE] readback+sws={s_med:.3}ms/帧（中位; min={s_min:.3} 均值={s_mean:.3}; n={}）",
            s_times.len()
        );
        eprintln!(
            "[PASSTHROUGH RATIO] SAFE/copy ≈ {:.1}×（{:.3}ms / {:.3}ms，越小越划算）",
            s_med / p_med.max(1e-6),
            s_med,
            p_med
        );
        cx.run_until_parked();
    }

    /// 双设备回退回归：**不开启**直通（= [`DualPathGuard`] 缺省关）＝双设备「建设备/open_hw 失败」
    /// 的毒化钩子（`dual_path_allowed` 返回 false 即等价于直通任何一步失败）——worker 必须回落
    /// SAFE BGRA 路径（auto → `open_scaled_hw` → 内核软回退）：画面照常 BGRA（`RenderImage`）
    /// 上屏、pts 单调、**GPU paint 探针恒 0**（canvas 走 img 不调 paint_external_yuv）。
    #[gpui::test]
    #[allow(clippy::await_holding_lock)]
    #[cfg(target_os = "windows")]
    async fn gpu_passthrough_fallback_to_safe(cx: &mut TestAppContext) {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/视频测试.mp4");
        if !path.exists() {
            eprintln!("SKIP: missing target/视频测试.mp4（夹具不入库）");
            return;
        }
        let _ffmpeg_guard_test = ffmpeg_guard_lock().lock().unwrap_or_else(|e| e.into_inner());
        let _decode_mode = crate::decode::DecodeModeGuard::new(0); // auto；直通被默认关 = 毒化
        let _fault_bl = FaultAndBlGuard;
        dual_engaged_reset();
        hw_engaged_reset();
        let viewer: gpui::Entity<VideoViewer> =
            cx.update(|app| app.new(|cx| VideoViewer::new(path, cx)));
        // 等首帧上屏：BGRA（`frame` 有、`gpu_frame` 恒 None）+ 时长已知 + seek 落地
        let mut wait = 0;
        loop {
            let ready = cx.update(|app| {
                let v = viewer.read(app);
                v.frame.is_some() && !v.gpu_frame.is_some() && v.duration > 1.0 && v.pending_seek.is_none()
            });
            if ready {
                break;
            }
            viewer.update(cx, |v, cx| {
                let _ = v.tick(cx);
            });
            cx.background_executor.timer(Duration::from_millis(5)).await;
            wait += 1;
            assert!(wait < 6000, "等待 SAFE 回退首帧上屏超时");
        }
        assert!(!dual_engaged(), "直通被毒化（未开启）时不应进入双设备");
        // 回退路径播放正常：步进 10 帧断言 BGRA 帧持续上屏、pts 单调
        let mut last_pts = -1.0f64;
        for _ in 0..10 {
            viewer.update(cx, |v, cx| {
                v.playhead += 0.5;
                let _ = v.tick(cx);
            });
            cx.background_executor.timer(Duration::from_millis(8)).await;
            let (pts, gpu) = cx.update(|app| {
                let v = viewer.read(app);
                (v.last_frame_pts.unwrap_or(last_pts), v.gpu_frame.is_some())
            });
            assert!(!gpu, "回退路径不应有 GPU 帧上屏");
            assert!(
                pts >= last_pts - 1e-6,
                "SAFE 回退帧 pts 应单调: {pts:.2} < {last_pts:.2}"
            );
            last_pts = pts;
        }
        // 强制 draw：BGRA 走 img（无 canvas paint_external_yuv）→ 探针恒 0
        struct ViewerHost {
            viewer: gpui::Entity<VideoViewer>,
        }
        impl gpui::Render for ViewerHost {
            fn render(
                &mut self,
                _window: &mut gpui::Window,
                _cx: &mut gpui::Context<Self>,
            ) -> impl gpui::IntoElement {
                self.viewer.clone()
            }
        }
        let (_host, vcx) = cx.add_window_view(|_window, _cx| ViewerHost {
            viewer: viewer.clone(),
        });
        vcx.update(|window, cx| {
            window.draw(cx).clear(cx);
        });
        let probe_handle = crate::hw::take_hw_paint_probe();
        assert_eq!(
            probe_handle, 0,
            "SAFE 回退路径不应触发 GPU paint 探针（probe={probe_handle:#x}）"
        );
        eprintln!(
            "[GPU fallback] 直通被毒化时 worker 回落 SAFE BGRA（frame 上屏、gpu_frame=None、无 paint 探针），pts 至 {last_pts:.2}s"
        );
        cx.run_until_parked();
    }

    /// 回归（暂停态硬解 seek 落地）：硬解 seek 首帧 pts 常比暂停冻结的 playhead（=落点）
    /// 微超前 ~1 帧（帧号量化）——`tick` 排空按「pts ≤ playhead」判据会把第一帧当逾线帧塞
    /// 回 overflow、`pending_seek` 永不清 → seek 冻结（pause→hover→seek 复现）。软解靠
    /// 「预滚保底帧（≤落点）先行」掩盖；硬解 skip 只送 ≥ 落点帧直接暴露。断言：暂停态
    /// 下 seek 到「非整帧点」，首个 ≥ 落点帧仍上屏、pending 清空。缺夹具/无硬解 SKIP。
    #[gpui::test]
    #[allow(clippy::await_holding_lock)]
    #[cfg(target_os = "windows")]
    async fn paused_hw_seek_lands_on_quantized_frame(cx: &mut TestAppContext) {
        let _ffmpeg_guard_test = ffmpeg_guard_lock().lock().unwrap_or_else(|e| e.into_inner());
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/视频测试字幕.mkv");
        if !path.exists() {
            eprintln!("SKIP: missing target/视频测试字幕.mkv（长 GOP 夹具不入库）");
            return;
        }
        let _decode_mode = crate::decode::DecodeModeGuard::new(2);
        let viewer: gpui::Entity<VideoViewer> =
            cx.update(|app| app.new(|cx| VideoViewer::new(path, cx)));
        let mut wait = 0;
        loop {
            let ready = cx.update(|app| {
                let v = viewer.read(app);
                v.frame.is_some() && v.duration > 1.0 && v.pending_seek.is_none()
            });
            if ready {
                break;
            }
            viewer.update(cx, |v, cx| {
                let _ = v.tick(cx);
            });
            cx.background_executor.timer(Duration::from_millis(5)).await;
            wait += 1;
            assert!(wait < 6000, "等待 HW 首帧/时长超时");
        }
        // 暂停 + seek 到「非整帧点」：首解码帧 pts 必然 > 冻结 playhead（量化 +1 帧）
        let duration = cx.update(|app| viewer.read(app).duration);
        let target = (duration * 0.5).clamp(0.05, duration - 2.0);
        viewer.update(cx, |v, _cx| v.playing = false);
        viewer.update(cx, |v, cx| v.seek(target - v.playhead, cx));
        let deadline = Instant::now() + Duration::from_secs(20);
        let landed = loop {
            viewer.update(cx, |v, cx| {
                let _ = v.tick(cx);
            });
            let s = cx.update(|app| {
                let v = viewer.read(app);
                (v.pending_seek, v.last_frame_pts)
            });
            if s.0.is_none() {
                break s.1;
            }
            assert!(
                Instant::now() < deadline,
                "暂停 hw seek 落点帧上屏超时（target={target:.2}s）"
            );
            std::thread::sleep(Duration::from_millis(1));
        };
        let landed = landed.expect("暂停 hw seek 应有首帧上屏");
        assert!(
            landed >= target,
            "落点帧不得早于 target={target:.2}s future={landed:.2}s"
        );
        eprintln!("[HW paused seek] target={target:.2}s firstpts={landed:.2}s 落地");
        cx.run_until_parked();
    }

    /// 回归：**硬解设备丢失（D3D11 TDR / driver reset / device-removed）播放中途自动回软解续播**
    /// ——不再把帧解码错误当致命断代（冻结在最后一帧）。恢复策略（见 worker 内层循环）：
    /// 硬解 reader 连续 2 次解码/预滚失败 → 判定「该文件硬解设备已丢」→ (a) 黑名单该路径 +
    /// 静默重开**软解** reader @当前播放位置（最后成功解码 pts / seek 落点）续播（一个定格可接受，
    /// 不弹错、不冻结）；单次瞬时错误容忍、不重开。
    ///
    /// 断言（故障由测试注入在 worker 硬解 FFI 前返回错误模拟，走与真实报错同一条恢复路径）：
    /// (a) 重开后 soft reader 接管（`hw_engaged()==false`、reader is_hw()==false）；
    /// (b) 续播帧 pts ≥ 丢失前最后上屏位置；(c) 该路径进硬解黑名单——再 `open_scaled_hw` 直接
    /// 回软解；(d) 另一文件仍走硬解（黑名单路径级不外溢，worker 切文件再验一次）；另断言单次
    /// 瞬时错误后仍是硬解且播放继续（不触发重开）。纯代码不开 GUI；缺夹具/无硬解能力 SKIP。
    #[gpui::test]
    #[allow(clippy::await_holding_lock)]
    #[cfg(target_os = "windows")]
    async fn device_lost_auto_soft_fallback(cx: &mut TestAppContext) {
        let _ffmpeg_guard_test = ffmpeg_guard_lock().lock().unwrap_or_else(|e| e.into_inner());
        // 卫生：清黑名单 + 撤注入（`_cleanup` 在任意退出路径 Drop 复位；开始时也显式清一次，
        // 兼容本测试进程内有遗留状态的情形）
        let _cleanup = FaultAndBlGuard;
        crate::decode::hw_blacklist_clear_for_test();
        hw_fault_disable();
        hw_engaged_reset();
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/视频测试字幕.mkv");
        let other = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/视频测试.mp4");
        if !path.exists() || !other.exists() {
            eprintln!("SKIP: missing target/视频测试字幕.mkv / 视频测试.mp4（夹具不入库）");
            return;
        }
        // 强制硬解
        let _decode_mode = crate::decode::DecodeModeGuard::new(2);
        // 先确认本机确有硬解能力（open_scaled_hw 内核含软回退；无能力整个场景不成立 → SKIP；
        // 顺带预热 D3D11 设备）
        let cap_probe = crate::decode::VideoReader::open_scaled_hw(&path, 1920)
            .expect("open_scaled_hw 应打开成功（内核含软回退）");
        if !cap_probe.is_hw() {
            eprintln!("SKIP: 本机无 D3D11VA/DXVA2 硬解能力（open_scaled_hw 回软解）");
            return;
        }
        drop(cap_probe);
        // 经 viewer → 常驻 worker 打开（真解码异步；等首帧 + 时长 + pending 清 → 硬解已接管）
        let viewer: gpui::Entity<VideoViewer> =
            cx.update(|app| app.new(|cx| VideoViewer::new(path.clone(), cx)));
        let mut wait = 0;
        loop {
            let ready = cx.update(|app| {
                let v = viewer.read(app);
                v.frame.is_some() && v.duration > 1.0 && v.pending_seek.is_none()
            });
            if ready {
                break;
            }
            viewer.update(cx, |v, cx| {
                let _ = v.tick(cx);
            });
            cx.background_executor.timer(Duration::from_millis(5)).await;
            wait += 1;
            assert!(wait < 6000, "等待 HW 首帧/时长超时");
        }
        assert!(hw_engaged(), "强制 hw 下 worker 打开应已硬解接管");
        // 开始播放（真实墙钟推进 playhead → pump 排空 → worker 持续解码），先推进一段保证
        // 注入命中「稳态逐帧解码」而非打开/预滚路径
        let mut driving = 0;
        loop {
            let p = cx.update(|app| viewer.read(app).last_frame_pts);
            if p.is_some_and(|p| p >= 0.3) {
                break;
            }
            viewer.update(cx, |v, cx| {
                v.playing = true;
                let _ = v.tick(cx);
            });
            cx.background_executor.timer(Duration::from_millis(5)).await;
            driving += 1;
            assert!(driving < 6000, "播放推进超时（last_frame_pts 未前进）");
        }

        // —— 单次瞬时错误：容忍，不触发重开（仍硬解），且下一次解码成功、播放继续 ——
        let pts_before_t = cx
            .update(|app| viewer.read(app).last_frame_pts)
            .expect("已有上屏帧");
        hw_fault_fail_after(0, false); // 下一次硬解调用注入一次后自动撤销
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            viewer.update(cx, |v, cx| {
                let _ = v.tick(cx);
            });
            if hw_fault_fired() {
                break;
            }
            assert!(Instant::now() < deadline, "瞬时错误未在期限内注入");
            std::thread::sleep(Duration::from_millis(2));
        }
        let deadline = Instant::now() + Duration::from_secs(15);
        let after_t = loop {
            viewer.update(cx, |v, cx| {
                let _ = v.tick(cx);
            });
            let p = cx.update(|app| viewer.read(app).last_frame_pts);
            if let Some(p) = p {
                if p > pts_before_t + 0.1 {
                    break p;
                }
            }
            assert!(Instant::now() < deadline, "瞬时错误后播放应继续（未冻结）");
            std::thread::sleep(Duration::from_millis(2));
        };
        assert!(hw_engaged(), "单次瞬时错误不应触发重开（仍应硬解接管）");
        assert!(after_t > pts_before_t, "瞬时错误后应继续出帧");

        // —— 设备丢失：持续失败 ≥2 次 → 黑名单 + 自动重开软解续播 ——
        let pts_before_lost = cx
            .update(|app| viewer.read(app).last_frame_pts)
            .expect("已有上屏帧");
        hw_fault_fail_after(0, true); // 持续注入（模拟 TDR / device-removed）
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            viewer.update(cx, |v, cx| {
                let _ = v.tick(cx);
            });
            if !hw_engaged() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "硬解丢失后应自动重开软解（hw_engaged 未复位）"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        // (b) 续播：软解新帧 pts ≥ 丢失前最后上屏位置（重开位置 = 最后成功解码 pts）
        let deadline = Instant::now() + Duration::from_secs(30);
        let resume_pts = loop {
            viewer.update(cx, |v, cx| {
                let _ = v.tick(cx);
            });
            let p = cx.update(|app| viewer.read(app).last_frame_pts);
            if let Some(p) = p {
                if p + 0.05 >= pts_before_lost {
                    break p;
                }
            }
            assert!(Instant::now() < deadline, "软解续播未在期限内出帧");
            std::thread::sleep(Duration::from_millis(2));
        };
        assert!(
            resume_pts + 0.05 >= pts_before_lost,
            "续播帧 pts 应 ≥ 丢失前位置 pts_before_lost={pts_before_lost:.2}s 实得={resume_pts:.2}s"
        );
        eprintln!(
            "[device-lost] 软解接管：丢失前 pts={pts_before_lost:.2}s 续播首新帧={resume_pts:.2}s blacklist={}",
            crate::decode::hw_path_blacklisted(&path)
        );
        // (c) 该路径进黑名单：后续 open_scaled_hw 直接回软解（不重试已失效的 D3D11 设备）
        assert!(
            crate::decode::hw_path_blacklisted(&path),
            "设备丢失路径应已列入硬解黑名单"
        );
        let r_black = crate::decode::VideoReader::open_scaled_hw(&path, 1920)
            .expect("黑名单路径应能打开（回软解）");
        assert!(
            !r_black.is_hw(),
            "黑名单路径再 open_scaled_hw 应为软解（不再重试失效硬件）"
        );
        drop(r_black);
        // (d) 另一文件仍走硬解（黑名单是路径级，不扩散到其它文件）
        hw_fault_disable(); // 撤注入：下一支/直开不应再被注入
        let r_other = crate::decode::VideoReader::open_scaled_hw(&other, 1920)
            .expect("另一文件应能打开");
        assert!(r_other.is_hw(), "另一文件 open_scaled_hw 仍应硬解（黑名单不外溢）");
        drop(r_other);
        // (d-2) worker 切到另一文件：仍硬解（黑名单路径级在 worker 打开路径同样生效）
        cx.update(|app| {
            viewer.update(app, |v, cx| {
                v.nav = FolderNav::new(vec![path, other], 0);
                v.open_folder_index(1, cx);
            })
        });
        let mut wait2 = 0;
        loop {
            let ready = cx.update(|app| {
                let v = viewer.read(app);
                v.frame.is_some() && v.duration > 1.0 && v.pending_seek.is_none()
            });
            if ready {
                break;
            }
            viewer.update(cx, |v, cx| {
                let _ = v.tick(cx);
            });
            cx.background_executor.timer(Duration::from_millis(5)).await;
            wait2 += 1;
            assert!(wait2 < 6000, "切换到另一文件等待首帧/时长超时");
        }
        assert!(
            hw_engaged(),
            "切换到的另一文件应仍硬解（黑名单不外溢到其它路径）"
        );
        eprintln!("[device-lost] 另一文件仍硬解接管（hw_engaged={}）", hw_engaged());
        cx.run_until_parked();
    }

    /// 回归：**共享槽拷贝背压 ≠ GPU 设备丢失**——`SharedCopier::copy` 瞬态失败（渲染端未及时
    /// 归还槽 → 槽池瞬满 / 获取互斥超时）不得被当成设备丢失处理：不得进硬解黑名单、不得静默
    /// 重开软解（silent soft downgrade），直通持续接管、后续 seek 仍产 GPU 帧。
    ///
    /// 注入：测试钩子强制接下来 K=3 次 `copy` 返回 `Err("test: 槽回压")`（然后自卸）。旧实现
    /// 把 copy 错误当解码错误计入 `hw_fail_count` → 连续 2 次即黑名单 + 重开软解（本测修前在
    /// 首帧期就应触发软降级 → 断言必红）。纯代码不开 GUI，真 D3D11 设备 + 真硬解 + 真墙钟；
    /// 缺夹具 / 无 D3D11 设备 / 直通未接管 → SKIP。
    #[gpui::test]
    #[allow(clippy::await_holding_lock)]
    #[cfg(target_os = "windows")]
    async fn copy_backpressure_does_not_soft_downgrade(cx: &mut TestAppContext) {
        let _ffmpeg_guard_test = ffmpeg_guard_lock().lock().unwrap_or_else(|e| e.into_inner());
        let _dual = DualPathGuard::new(); // 生产默认：双设备直通优先
        let _decode_mode = crate::decode::DecodeModeGuard::new(0); // auto（直通优先）
        let _cleanup = FaultAndBlGuard; // 清黑名单 + 撤注入（任意退出路径）
        crate::decode::hw_blacklist_clear_for_test();
        hw_fault_disable();
        crate::hw::copy_fail_disable();
        hw_engaged_reset();
        dual_engaged_reset();
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/视频测试.mp4");
        if !path.exists() {
            eprintln!("SKIP: missing target/视频测试.mp4（夹具不入库）");
            return;
        }
        // 注入：接下来 3 次 copy 强制失败（然后自动撤销）——拷贝背压应由重试循环吸收，
        // 绝不进 hw_fail_count（修前第 2 次连续 copy 失败即黑名单+软降级，本测必红）
        crate::hw::copy_fail_for(3);
        let viewer: gpui::Entity<VideoViewer> =
            cx.update(|app| app.new(|cx| VideoViewer::new(path.clone(), cx)));
        // 等首帧上屏 + 时长已知 + seek 落地（真解码异步；双设备/SAFE/软解任一都会出帧）
        let mut wait = 0;
        loop {
            let ready = cx.update(|app| {
                let v = viewer.read(app);
                v.has_frame() && v.duration > 1.0 && v.pending_seek.is_none()
            });
            if ready {
                break;
            }
            viewer.update(cx, |v, cx| {
                let _ = v.tick(cx);
            });
            cx.background_executor.timer(Duration::from_millis(5)).await;
            wait += 1;
            assert!(wait < 6000, "等待首帧/时长超时");
        }
        if !dual_engaged() {
            // 本机无 D3D11VA h264 硬解 / 外部注入不支持 → 直通未接管，回退 SAFE 由
            // `gpu_passthrough_fallback_to_safe` 兜底；本测 SKIP（同其它直通测试口径）
            eprintln!("SKIP: 本机无法双设备直通（无 D3D11VA h264 硬解或外部注入不支持），回退 SAFE 路径");
            return;
        }
        // 直通接管后首帧即走 copy → 钩子必火；短暂补等防时序抖动
        let deadline = Instant::now() + Duration::from_secs(5);
        while !crate::hw::copy_fail_fired() {
            viewer.update(cx, |v, cx| {
                let _ = v.tick(cx);
            });
            assert!(
                Instant::now() < deadline,
                "拷贝回压注入未在期限内触发（直通首帧应命中 copy hook）"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        // ① 注入的拷贝失败**不得**导致黑名单（背压 ≠ 设备丢失）
        assert!(
            !crate::decode::hw_path_blacklisted(&path),
            "拷贝背压不得把该文件列入硬解黑名单（背压 ≠ 设备丢失）"
        );
        // ② 直通持续接管（未被静默重开软解）
        assert!(
            dual_engaged(),
            "拷贝背压后双设备直通应保持接管（不得 silent soft downgrade）"
        );
        // ②′ 合同断言：两条件**同时**成立（直通接管 ∧ 不黑名单）才是「无静默软降级」
        assert!(
            dual_engaged() && !crate::decode::hw_path_blacklisted(&path),
            "拷贝背压后必须同时满足：直通保持接管 dual={} 且不黑名单 bl={}",
            dual_engaged(),
            crate::decode::hw_path_blacklisted(&path)
        );
        eprintln!(
            "[copy backpressure] 注入 K=3 拷贝失败后：blacklist=false dual={} hw={}",
            dual_engaged(),
            hw_engaged()
        );
        // ③ 钩子已撤销（K=3 自卸 + 显式双保险）后，后续 seek 仍产 GPU 帧（探针 == 在屏句柄）
        crate::hw::copy_fail_disable();
        let duration = cx.update(|app| viewer.read(app).duration);
        let target = (duration * 0.5).clamp(0.05, duration - 2.0);
        viewer.update(cx, |v, cx| v.seek(target - v.playhead, cx));
        let deadline = Instant::now() + Duration::from_secs(20);
        let current_pts = loop {
            viewer.update(cx, |v, cx| {
                let _ = v.tick(cx);
            });
            let s = cx.update(|app| {
                let v = viewer.read(app);
                (v.pending_seek, v.last_frame_pts, v.gpu_frame.is_some())
            });
            if s.0.is_none() && s.2 {
                break s.1;
            }
            assert!(Instant::now() < deadline, "seek 后 GPU 帧上屏超时（target={target:.2}s）");
            std::thread::sleep(Duration::from_millis(2));
        };
        let current_handle = cx
            .update(|app| {
                let v = viewer.read(app);
                v.gpu_frame.as_ref().map(|f| f.shared_handle)
            })
            .expect("seek 后应在屏 GPU 帧（dual 未被软降级）");
        assert!(
            (current_pts.unwrap_or(target) - target).abs() < 0.5,
            "seek 落点帧应到 target 附近: {current_pts:?}"
        );
        // 开测试窗口承载同一 viewer 并强制 draw：paint_external_yuv 收到当前 GPU 帧句柄
        struct ViewerHost {
            viewer: gpui::Entity<VideoViewer>,
        }
        impl gpui::Render for ViewerHost {
            fn render(
                &mut self,
                _window: &mut gpui::Window,
                _cx: &mut gpui::Context<Self>,
            ) -> impl gpui::IntoElement {
                self.viewer.clone()
            }
        }
        let (_host, vcx) = cx.add_window_view(|_window, _cx| ViewerHost {
            viewer: viewer.clone(),
        });
        vcx.update(|window, cx| {
            window.draw(cx).clear(cx);
        });
        let probe_handle = crate::hw::take_hw_paint_probe();
        assert_ne!(
            probe_handle, 0,
            "拷贝背压恢复后 GPU 帧应仍走 paint_external_yuv（探针非 0，直通未掉）"
        );
        assert_eq!(
            probe_handle, current_handle,
            "外部 YUV 精灵应使用当前 GPU 上屏共享句柄（probe={probe_handle:#x} vs 当前 {current_handle:#x}）"
        );
        eprintln!(
            "[copy backpressure] seek 后仍产 GPU 帧 probe={probe_handle:#x} pts={current_pts:?} blacklist={}",
            crate::decode::hw_path_blacklisted(&path)
        );
        cx.run_until_parked();
    }

    /// 回归（独立评审 MUST-FIX 伴生测试）：**格式不支持 ≠ 背压**——把 `CopyError::Unsupported`
    ///（如 P010 10-bit HEVC）当成背压重试/跳过会永久无画面（修前回归：不黑名单、不软重开）。
    /// 断言完整恢复契约：`copy` 前几次强制报 Unsupported → 按「设备丢失」路径**黑名单 + 软重开**
    /// + **软解续播帧真正上屏**（P010 续播保证）。
    /// 注：此前「软重开首帧解码失败」根因在**我方代码**而非 cdylib——双设备 reader
    /// `source_cap()==1`（无 scaler），`src_cap` 命令开头只取一次、软重开后未刷新 → 常驻 buf
    /// 停在 1 字节 → 软解 `_into` 报「解码输出缓冲过小」→ 断代无画面。worker 软重开分支已在
    /// 公共路径刷新 `src_cap`（workers.rs 软重开分支），故本测等到 **BGRA 帧真正上屏**才算恢复。
    /// 纯代码不开 GUI；缺夹具 / 无 D3D11 设备 / 直通未接管 → SKIP。
    #[gpui::test]
    #[allow(clippy::await_holding_lock)]
    #[cfg(target_os = "windows")]
    async fn copy_unsupported_format_reopens_soft(cx: &mut TestAppContext) {
        let _ffmpeg_guard_test = ffmpeg_guard_lock().lock().unwrap_or_else(|e| e.into_inner());
        let _dual = DualPathGuard::new(); // 生产默认：双设备直通优先
        let _decode_mode = crate::decode::DecodeModeGuard::new(0); // auto（直通优先）
        let _cleanup = FaultAndBlGuard; // 清黑名单 + 撤注入（任意退出路径）
        crate::decode::hw_blacklist_clear_for_test();
        hw_fault_disable();
        crate::hw::copy_fail_disable();
        hw_engaged_reset();
        dual_engaged_reset();
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/视频测试字幕.mkv");
        if !path.exists() {
            eprintln!("SKIP: missing target/视频测试字幕.mkv（长 GOP 夹具不入库）");
            return;
        }
        let viewer: gpui::Entity<VideoViewer> =
            cx.update(|app| app.new(|cx| VideoViewer::new(path.clone(), cx)));
        // 先等首个 GPU 帧上屏（直通接管确认、resume_pts 有个非 0 落点再注入，对齐
        // device_lost 的「播放中故障」形态）
        let mut wait = 0;
        loop {
            let ready = cx.update(|app| {
                let v = viewer.read(app);
                v.has_frame() && v.duration > 1.0 && v.pending_seek.is_none()
            });
            if ready {
                break;
            }
            viewer.update(cx, |v, cx| {
                let _ = v.tick(cx);
            });
            cx.background_executor.timer(Duration::from_millis(5)).await;
            wait += 1;
            assert!(wait < 6000, "等待直通首帧/时长超时");
        }
        if !dual_engaged() {
            eprintln!("SKIP: 本机无法双设备直通（无 D3D11VA h264 硬解或外部注入不支持），回退 SAFE 路径");
            return;
        }
        // 推进播放到 >0.2s（resume_pts 非 0，软重开续播位置明确）
        let mut driving = 0;
        loop {
            let p = cx.update(|app| viewer.read(app).last_frame_pts);
            if p.is_some_and(|p| p >= 0.2) {
                break;
            }
            viewer.update(cx, |v, cx| {
                v.playing = true;
                let _ = v.tick(cx);
            });
            cx.background_executor.timer(Duration::from_millis(5)).await;
            driving += 1;
            assert!(driving < 6000, "播放推进超时（last_frame_pts 未前进）");
        }
        // 注入：接下来 4 次 copy 强制「格式不支持」（Unsupported 不进重试、逐帧计数——连续 2 帧
        // 即触发黑名单 + 软重开；软路径不再调 copy，剩余注入自然作废）
        crate::hw::copy_unsupported_for(4);
        // 等完整恢复闭环：该文件进黑名单 ∧ 软解接管（hw_engaged 复位）∧ **BGRA 帧真正上屏**
        //（`frame` 有、`gpu_frame` 掉、pending 清）——P010 续播成功而非断代无画面
        let mut wait2 = 0;
        loop {
            let done = cx.update(|app| {
                let v = viewer.read(app);
                let gpu = v.gpu_frame.is_some();
                let soft = v.frame.is_some();
                crate::decode::hw_path_blacklisted(&path)
                    && !hw_engaged()
                    && !gpu
                    && soft
                    && v.pending_seek.is_none()
            });
            if done {
                break;
            }
            viewer.update(cx, |v, cx| {
                let _ = v.tick(cx);
            });
            cx.background_executor.timer(Duration::from_millis(5)).await;
            wait2 += 1;
            assert!(wait2 < 6000, "等待 Unsupported 软重开 + BGRA 续播帧上屏超时");
        }
        // 直通接管后首帧即走 copy → Unsupported 钩子必火（软重开完成前已触发）
        assert!(
            crate::hw::copy_unsupported_fired(),
            "Unsupported 注入应已实际触发（直通首帧即命中 copy hook）"
        );
        // ① 该文件进硬解黑名单（Unsupported = 「该文件不支持直通」→ 黑名单 + 软重开恢复路径）
        assert!(
            crate::decode::hw_path_blacklisted(&path),
            "格式不支持应把该文件列入硬解黑名单（按设备丢失恢复路径），而非背压式永久跳过"
        );
        // ② 软解接管：硬解关闭、GPU 帧掉、BGRA 帧上屏（恢复播放，非断代无画面）
        assert!(!hw_engaged(), "格式不支持后应软重开（hw_engaged=false，软解接管）");
        let (gpu, soft) = cx.update(|app| {
            let v = viewer.read(app);
            (v.gpu_frame.is_some(), v.frame.is_some())
        });
        assert!(!gpu, "软重开后不应再有 GPU 帧上屏（gpu_frame 掉）");
        assert!(soft, "软重开后 BGRA 帧应真正上屏（P010 续播保证，src_cap 已刷新）");
        eprintln!(
            "[copy unsupported] 格式不支持 → 黑名单={} 软接管（hw={} gpu_frame={gpu} frame={soft}）",
            crate::decode::hw_path_blacklisted(&path),
            hw_engaged()
        );
        cx.run_until_parked();
    }

    // ---- 解码偏好切换（控制条按钮：自动 ↔ 强制软解）----

    /// (a) 状态循环：`cycle_decode_pref` 每点一次在「自动（0）↔ 强制软解（1）」间翻转，
    /// 且进程级旋钮 `decode_mode()` 同步为 0/1/0；空 viewer（无活跃 reader）只改偏好、
    /// 不误触重开（`frame.is_some()` 短路）。
    #[gpui::test]
    async fn decode_pref_cycle_toggles_mode_and_knob(cx: &mut TestAppContext) {
        let _decode_mode = crate::decode::DecodeModeGuard::new(0);
        crate::decode::set_decode_mode(0);
        let ent = cx.update(|app| app.new(|cx| blank(fixture(), cx)));
        let read = |cx: &mut TestAppContext| {
            cx.update(|app| {
                let v = ent.read(app);
                (v.decode_pref, crate::decode::decode_mode())
            })
        };
        assert_eq!(read(cx), (0u8, 0u8), "初始 auto / 旋钮 0");
        ent.update(cx, |v, cx| v.cycle_decode_pref(cx));
        assert_eq!(read(cx), (1u8, 1u8), "点击 → 强制软解 / 旋钮 1");
        ent.update(cx, |v, cx| v.cycle_decode_pref(cx));
        assert_eq!(read(cx), (0u8, 0u8), "再点 → 回自动 / 旋钮 0");
        cx.run_until_parked();
    }

    /// (b) 重开-on-toggle：真实解码下（夹具 `target\视频测试.mp4`，缺夹具 SKIP），强制硬解
    /// 起手 → 播放推进 → 点「解码偏好」切到强制软解：worker 重开为软解 reader
    /// （`hw_engaged()==false`），续播新帧 pts ≥ 切换前最后上屏位置（@当前播放位置 seek 续播，
    /// 不跳帧不重头）；再点回自动：worker 再重开为硬解 reader（`hw_engaged()==true`，新 reader）
    /// 且播放继续。收尾守卫（DecodeModeGuard Drop 复位旋钮 0 + FaultAndBlGuard 清黑名单/撤
    /// 注入）保证任何退出路径都不把模式/黑名单状态泄漏给后续测试（纯代码不开 GUI）。
    #[gpui::test]
    #[allow(clippy::await_holding_lock)]
    #[cfg(target_os = "windows")]
    async fn decode_toggle_reopens_reader_from_current_pts(cx: &mut TestAppContext) {
        let _ffmpeg_guard_test = ffmpeg_guard_lock().lock().unwrap_or_else(|e| e.into_inner());
        let _cleanup = FaultAndBlGuard;
        crate::decode::hw_blacklist_clear_for_test();
        hw_fault_disable();
        hw_engaged_reset();
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/视频测试.mp4");
        if !path.exists() {
            eprintln!("SKIP: missing target/视频测试.mp4（夹具不入库）");
            return;
        }
        // 起手强制 hw（旋钮 2 → worker 走 open_scaled_hw）；`decode_pref` 仍 0（按钮态=自动）
        let _decode_mode_guard = crate::decode::DecodeModeGuard::new(2);
        // 确认本机确有硬解能力（内核含软回退；无能力整个场景不成立 → SKIP；顺带预热设备）
        let cap_probe = crate::decode::VideoReader::open_scaled_hw(&path, 1920)
            .expect("open_scaled_hw 应打开成功（内核含软回退）");
        if !cap_probe.is_hw() {
            eprintln!("SKIP: 本机无 D3D11VA/DXVA2 硬解能力（open_scaled_hw 回软解）");
            return;
        }
        drop(cap_probe);
        let viewer: gpui::Entity<VideoViewer> =
            cx.update(|app| app.new(|cx| VideoViewer::new(path.clone(), cx)));
        let mut wait = 0;
        loop {
            let ready = cx.update(|app| {
                let v = viewer.read(app);
                v.frame.is_some() && v.duration > 1.0 && v.pending_seek.is_none()
            });
            if ready {
                break;
            }
            viewer.update(cx, |v, cx| {
                let _ = v.tick(cx);
            });
            cx.background_executor.timer(Duration::from_millis(5)).await;
            wait += 1;
            assert!(wait < 6000, "等待 hw 首帧/时长超时");
        }
        assert!(hw_engaged(), "强制 hw 下 worker 打开应已硬解接管");
        // 播放推进到 >0.3s（稳态逐帧解码，切换前有可比较的最新 pts）
        let mut driving = 0;
        loop {
            let p = cx.update(|app| viewer.read(app).last_frame_pts);
            if p.is_some_and(|p| p >= 0.3) {
                break;
            }
            viewer.update(cx, |v, cx| {
                v.playing = true;
                let _ = v.tick(cx);
            });
            cx.background_executor.timer(Duration::from_millis(5)).await;
            driving += 1;
            assert!(driving < 6000, "播放推进超时（last_frame_pts 未前进）");
        }
        let pts_before = cx
            .update(|app| viewer.read(app).last_frame_pts)
            .expect("切换前已有上屏帧");
        // —— 切到强制软解：worker 重开软解 reader，续播不跳 ——
        viewer.update(cx, |v, cx| {
            v.cycle_decode_pref(cx);
        });
        // 等「软解重开完成且续播出新帧」：`hw_engaged()` 在 soft open 起点就拍 false（早于
        // 真正接管），故必须等一个「严格大于切换前 pts 的新帧」落地——新帧只可能来自重开后的
        // 软解 reader（旧 reader 最后一帧 pts = pts_before），同时说明 pending_seek 已清。
        let deadline = Instant::now() + Duration::from_secs(30);
        let resume = loop {
            viewer.update(cx, |v, cx| {
                let _ = v.tick(cx);
            });
            let p = cx.update(|app| viewer.read(app).last_frame_pts);
            if let Some(p) = p {
                if !hw_engaged() && p > pts_before {
                    break p;
                }
            }
            assert!(Instant::now() < deadline, "切软解后应重开软解续播出新帧");
            std::thread::sleep(Duration::from_millis(2));
        };
        assert!(
            resume > pts_before,
            "软解续播首新帧 pts 应 > 切换前 pts_before={pts_before:.2}s 实得={resume:.2}s（不跳回更早位置）"
        );
        // —— 切回自动（硬解优先）：worker 再重开为硬解 reader ——
        viewer.update(cx, |v, cx| {
            v.cycle_decode_pref(cx);
        });
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            viewer.update(cx, |v, cx| {
                let _ = v.tick(cx);
            });
            if hw_engaged() {
                break;
            }
            assert!(Instant::now() < deadline, "切回自动应重开为硬解（hw_engaged 未恢复）");
            std::thread::sleep(Duration::from_millis(2));
        }
        // 硬解接管后播放继续（新帧 > 上次软解帧 pts：重开的 hw reader @当前播放位置续播）
        let deadline = Instant::now() + Duration::from_secs(30);
        let hw_resume = loop {
            viewer.update(cx, |v, cx| {
                let _ = v.tick(cx);
            });
            let p = cx.update(|app| viewer.read(app).last_frame_pts);
            if let Some(p) = p {
                if p > resume {
                    break p;
                }
            }
            assert!(Instant::now() < deadline, "硬解续播未在期限内出新帧");
            std::thread::sleep(Duration::from_millis(2));
        };
        eprintln!(
            "[decode-toggle] 硬解接管：切软解前 pts={pts_before:.2}s 软解续播={resume:.2}s 回硬解续播={hw_resume:.2}s"
        );
        cx.run_until_parked();
    }

    /// 性能基准（纯代码、不开 GUI，真解码 + 真墙钟）：M0 hover 预解码 —— 对同一组（含深 GOP）
    /// 落点先量「无悬停基线」（= 正常点击 seek 全量解码墙钟，同 `bench_click_seek_land_frame_wall`
    /// 链路），再量「暂停 → 悬停该落点 → 等后台预解码缓存抵达 → 点击」的端到端墙钟。缓存命中时
    /// 点击直接装屏、无 seek：断言 <80ms 即现且首帧 pts ≥ 落点、无名 pending。
    /// [SEEKBENCHHOVER] 与 GUI [SEEK] 同 8 字段/同序/同单位布局；基线侧取 tick 沉淀快照，
    /// 悬停侧（无 seek 无快照）按命中语义手工填该 8 字段（seek2first≈0、reuse=1、regress=pts−落点）。
    /// 夹具 `target\视频测试字幕.mkv`（长 GOP）；缺夹具 SKIP（AGENTS.md §5）。
    #[gpui::test]
    #[allow(clippy::await_holding_lock)]
    async fn bench_hover_click_land_frame_wall(cx: &mut TestAppContext) {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/视频测试字幕.mkv");
        if !path.exists() {
            eprintln!("SKIP: missing target/视频测试字幕.mkv（长 GOP 夹具不入库）");
            return;
        }
        let _ffmpeg_guard_test = ffmpeg_guard_lock().lock().unwrap_or_else(|e| e.into_inner());
        // 同 bench_click_seek_land_frame_wall 口径：基线走软解（解码偏好旋钮 1）与其
        // [SEEKBENCH] 软解基线对表；hover 预解码 worker 本来就独立软解。RAII 复位防污染。
        let _decode_mode = crate::decode::DecodeModeGuard::new(1);
        let viewer: gpui::Entity<VideoViewer> =
            cx.update(|app| app.new(|cx| VideoViewer::new(path, cx)));
        let mut wait = 0;
        loop {
            let ready = cx.update(|app| {
                let v = viewer.read(app);
                v.frame.is_some() && v.duration > 1.0 && v.pending_seek.is_none()
            });
            if ready {
                break;
            }
            viewer.update(cx, |v, cx| {
                let _ = v.tick(cx);
            });
            cx.background_executor.timer(Duration::from_millis(5)).await;
            wait += 1;
            assert!(wait < 6000, "等待首帧/时长超时");
        }
        let duration = cx.update(|app| viewer.read(app).duration);
        // 目标秒：贴近既有 bench 实测深回退点（regress ≈ −5.3s / −9.9s / −10.2s / −3.0s），
        // 覆盖浅/中/深 GOP；钳 [0.05, duration−2s] 避开近结尾 EOF。
        let raw = [191.0f64, 354.0, 447.0, 1335.0];
        let hi = (duration - 2.0).max(0.05);
        let mut base_rows: Vec<(f64, f64, f64)> = Vec::new(); // (target, total_ms, regress)
        let mut hover_rows: Vec<(f64, f64, f64)> = Vec::new(); // (target, hover_ms, regress)
        for &sec in &raw {
            let sec = sec.clamp(0.05, hi);
            let fraction = (sec / duration).clamp(0.0, 1.0) as f32;
            // (a) 无悬停基线：同既有 bench 单击链路（begin+end 一次换代全量解码）
            let t0 = Instant::now();
            viewer.update(cx, |v, cx| v.begin_scrub(fraction, cx));
            viewer.update(cx, |v, cx| v.end_scrub(fraction, cx));
            let btarget = cx.update(|app| viewer.read(app).playhead);
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                viewer.update(cx, |v, cx| {
                    let _ = v.tick(cx);
                });
                let s = cx.update(|app| {
                    let v = viewer.read(app);
                    (v.pending_seek, v.last_frame_pts)
                });
                if s.0.is_none() {
                    break;
                }
                assert!(Instant::now() < deadline, "基线 seek 落点超时（target={btarget:.2}s）");
                std::thread::sleep(Duration::from_millis(1));
            }
            let bms = t0.elapsed().as_secs_f64() * 1e3;
            let (btotal, bregress) = match take_seek_diag_test() {
                Some((total, _, regress, _, _, _, _, _)) => (total, regress),
                None => (bms, f64::NAN),
            };
            base_rows.push((sec, btotal, bregress));
            eprintln!(
                "[SEEKBENCHHOVER] base target={sec:.2}s total={btotal:.1}ms regress={bregress:+.2}s"
            );
            // (b) 暂停 → 悬停同一落点 → 等后台预解码缓存抵达 → 点击（begin+end）→ 量墙钟
            viewer.update(cx, |v, _cx| {
                v.playing = false; // 暂停：悬停预解码只在空闲态投（机会式）
                v.pending_seek = None;
            });
            viewer.update(cx, |v, cx| v.update_hover_target(sec, cx));
            let hdeadline = Instant::now() + Duration::from_secs(15);
            loop {
                // 等交付任务跑 + worker 真解码（墙钟 deadline；fake timer 不作节拍）
                cx.background_executor.timer(Duration::from_millis(5)).await;
                viewer.update(cx, |v, cx| {
                    let _ = v.tick(cx);
                });
                let got = cx.update(|app| viewer.read(app).hover_cache.is_some());
                if got {
                    break;
                }
                assert!(Instant::now() < hdeadline, "hover 预解码缓存超时（target={sec:.2}s）");
            }
            let hover_pts0 = cx
                .update(|app| viewer.read(app).hover_cache.as_ref().map(|f| f.pts))
                .expect("悬停缓存已抵达");
            assert!(hover_pts0 >= sec, "预解码帧 pts 不得早于落点 target={sec} pts={hover_pts0}");
            let th = Instant::now();
            viewer.update(cx, |v, cx| v.begin_scrub(fraction, cx));
            viewer.update(cx, |v, cx| v.end_scrub(fraction, cx));
            let hms = th.elapsed().as_secs_f64() * 1e3;
            let (hover_pend, hover_pts) = cx.update(|app| {
                let v = viewer.read(app);
                (v.pending_seek, v.last_frame_pts)
            });
            assert!(hover_pend.is_none(), "悬停命中点击后不应有待解 pending（target={sec}）");
            let hp = hover_pts.expect("悬停命中点击后应即时有帧上屏");
            assert!(
                hp >= sec && hp - sec <= HOVER_HIT_WINDOW_S,
                "悬停点击首帧应在命中窗内且不早于落点 target={sec} pts={hp}"
            );
            assert!(hms < 80.0, "悬停缓存命中点击应 <80ms 即现 target={sec} 实得 {hms:.1}ms");
            hover_rows.push((sec, hms, hp - sec));
            eprintln!(
                "[SEEKBENCHHOVER] total={hms:.1}ms reuse=1 regress={:+.2}s skipped=0 seek2first=0.0ms frame2screen=0.0ms target={:.2}s firstpts={:.2}s（命中 hover 缓存，无 seek）",
                hp - sec, sec, hp,
            );
        }
        // 摘要：基线 vs 悬停（同为 total 口径）
        let bms_all: Vec<f64> = base_rows.iter().map(|r| r.1).collect();
        let hms_all: Vec<f64> = hover_rows.iter().map(|r| r.1).collect();
        let (bmin, bmedian, bmax) = {
            let mut s = bms_all.clone();
            s.sort_by(|a, b| a.partial_cmp(b).unwrap());
            (s[0], s[s.len() / 2], s[s.len() - 1])
        };
        let (hmin, hmedian, hmax) = {
            let mut s = hms_all.clone();
            s.sort_by(|a, b| a.partial_cmp(b).unwrap());
            (s[0], s[s.len() / 2], s[s.len() - 1])
        };
        eprintln!(
            "[SEEKBENCHHOVER] 摘要 基线 n={} min={bmin:.1}ms median={bmedian:.1}ms max={bmax:.1}ms ｜ 悬停命中 n={} min={hmin:.1}ms median={hmedian:.1}ms max={hmax:.1}ms",
            bms_all.len(), hms_all.len(),
        );
        cx.run_until_parked();
    }

    /// 性能基准（纯代码、不开 GUI，真解码 + 真墙钟）：**P1 事件驱动泵**——真实 seek 链路
    ///（begin_scrub+end_scrub 单击）不再 ~2ms 轮询，而是由 worker 帧就绪通知唤醒 pump 自行
    /// 落地首帧：本测试**不手动 tick**（pump 把帧 drain+上屏），量「点击 → 首帧上屏」端到端
    /// 墙钟与 [SEEK] 诊断的 frame2screen（入队→上屏）。断言（动态基准取中位，非绝对值）：
    /// pump 能自行落地，frame2screen 中位数 < 30ms（远低于帧率节拍 41ms；若泵退化回
    /// 「等一拍」节拍即红）。不引入常驻轮询由 `pump_wait_interval` 的兜底超时保证。
    ///（bounded cap 1 级联通知）[SEEKPUMP] 与 GUI [SEEK] 同字段口径。夹具
    /// `target\视频测试字幕.mkv`；缺夹具 SKIP。
    #[gpui::test]
    #[allow(clippy::await_holding_lock)]
    async fn bench_pump_framewake_lands_seek_wall(cx: &mut TestAppContext) {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/视频测试字幕.mkv");
        if !path.exists() {
            eprintln!("SKIP: missing target/视频测试字幕.mkv（长 GOP 夹具不入库）");
            return;
        }
        let _ffmpeg_guard_test = ffmpeg_guard_lock().lock().unwrap_or_else(|e| e.into_inner());
        let _decode_mode = crate::decode::DecodeModeGuard::new(2); // SAFE 硬解优先（skip 快路径）
        let viewer: gpui::Entity<VideoViewer> =
            cx.update(|app| app.new(|cx| VideoViewer::new(path, cx)));
        let mut wait = 0;
        loop {
            let ready = cx.update(|app| {
                let v = viewer.read(app);
                v.frame.is_some() && v.duration > 1.0 && v.pending_seek.is_none()
            });
            if ready {
                break;
            }
            viewer.update(cx, |v, cx| {
                let _ = v.tick(cx);
            });
            cx.background_executor.timer(Duration::from_millis(5)).await;
            wait += 1;
            assert!(wait < 6000, "等待首帧/时长超时");
        }
        let duration = cx.update(|app| viewer.read(app).duration);
        let raw = [72.0f64, 191.0, 354.0, 900.0];
        let hi = (duration - 2.0).max(0.05);
        let mut total: Vec<f64> = Vec::with_capacity(raw.len());
        let mut f2s: Vec<f64> = Vec::with_capacity(raw.len());
        for &sec in &raw {
            let sec = sec.clamp(0.05, hi);
            let fraction = (sec / duration).clamp(0.0, 1.0) as f32;
            let t0 = Instant::now();
            viewer.update(cx, |v, cx| v.begin_scrub(fraction, cx));
            viewer.update(cx, |v, cx| v.end_scrub(fraction, cx));
            let target = cx.update(|app| viewer.read(app).playhead);
            let deadline = Instant::now() + Duration::from_secs(30);
            let landed = loop {
                // 不手动 tick：pump 由「worker 帧就绪通知」事件驱动自行落地。测试执行器是
                // fake 调度器（background spawn 与 foreground 同线程跑，真实 worker 通知经
                // 真实后台驱动 fake 执行器）——轮询需三者合拍：advance_clock 驱 fake 定时器
                //（泵在两目标间可能停在帧率节拍放大定时器上 → 放行后重入事件等待）、
                // 真实后台 timer await 让调度器推进、真实 worker（后台线程）解码产出通知。
                cx.executor()
                    .advance_clock(Duration::from_millis(1));
                cx.background_executor.timer(Duration::from_millis(2)).await;
                let s = cx.update(|app| {
                    let v = viewer.read(app);
                    (v.pending_seek, v.last_frame_pts)
                });
                if s.0.is_none() {
                    break s.1;
                }
                assert!(
                    Instant::now() < deadline,
                    "事件驱动泵未自行落地 seek（target={target:.2}s）"
                );
                std::thread::sleep(Duration::from_millis(1));
            };
            let ms = t0.elapsed().as_secs_f64() * 1e3;
            match take_seek_diag_test() {
                Some((t, _reuse, _re, _sk, _s2f, f2s_ms, _tg, _fp)) => {
                    total.push(t);
                    f2s.push(f2s_ms);
                    eprintln!(
                        "[SEEKPUMP] total={t:.1}ms frame2screen={f2s_ms:.1}ms target={target:.1}s landed={:?}",
                        landed.map(|p| format!("{p:.2}s"))
                    );
                }
                None => {
                    // 罕见丢快照：以端到端作为 frame2screen 兜底（保守），不影响多数行
                    total.push(ms);
                    f2s.push(41.0);
                    eprintln!(
                        "[SEEKPUMP] target={target:.1}s click2frame={ms:.1}ms landed={:?}（无 diag 快照，兜底）",
                        landed.map(|p| format!("{p:.2}s"))
                    );
                }
            }
        }
        let mut ts = total.clone();
        let mut fs = f2s.clone();
        ts.sort_by(|a, b| a.partial_cmp(b).unwrap());
        fs.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let (t_med, f_med) = (ts[ts.len() / 2], fs[fs.len() / 2]);
        eprintln!(
            "[SEEKPUMP] 摘要 n={} total 中位={t_med:.1}ms frame2screen 中位={f_med:.1}ms（<30ms 护栏：不得劣化回 41ms 帧率节拍）",
            total.len(),
        );
        assert!(
            f_med < 30.0,
            "事件驱动泵 frame2screen 中位数应远低于帧率节拍（41ms），实得 {f_med:.1}ms"
        );
        cx.run_until_parked();
    }

    /// 性能基准（纯代码、不开 GUI，真解码 + 真墙钟）：**P1 事件驱动泵「同落点连点两次」
    /// 回归护栏**——真机实测「同落点第二次点击」偶发 214–232ms frame2screen 尖峰（量级 ≈
    /// [`SEEK_WAKE_CAP_MS`]=200ms 兜底超时）：上一 seek worker 续播尾通知残留 cap-1 wake
    /// 通道，新泵首片消费它后其真实通知又撞 → 泵睡满整拍静默。本测试模拟该窗口：第一次
    /// seek 落地后向 wake 通道**注入一条残留通知**（等价续播尾通知与泵退出/重起的间隙），
    /// 再在**同落点**连点第二次 seek，断言第二次的 frame2screen 中位 < 30ms 护栏（泵退化回
    /// 「睡整拍」即红）。与 [`bench_pump_framewake_lands_seek_wall`] 同姿势：真解码 + 真
    /// 墙钟、不手动 tick（pump 自行落地）。fake 调度器盖不住生产「通知丢失」竞态，本测试以
    /// 残留通知注入逼近窗口 + 护栏锚定「同落点第二次点击」延迟不劣化。夹具
    /// `target\视频测试字幕.mkv`；缺夹具 SKIP。
    #[gpui::test]
    #[allow(clippy::await_holding_lock)]
    async fn bench_pump_double_click_same_spot_seek_wall(cx: &mut TestAppContext) {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/视频测试字幕.mkv");
        if !path.exists() {
            eprintln!("SKIP: missing target/视频测试字幕.mkv（长 GOP 夹具不入库）");
            return;
        }
        let _ffmpeg_guard_test = ffmpeg_guard_lock().lock().unwrap_or_else(|e| e.into_inner());
        let _decode_mode = crate::decode::DecodeModeGuard::new(2); // SAFE 硬解优先（skip 快路径）
        let viewer: gpui::Entity<VideoViewer> =
            cx.update(|app| app.new(|cx| VideoViewer::new(path, cx)));
        let mut wait = 0;
        loop {
            let ready = cx.update(|app| {
                let v = viewer.read(app);
                v.frame.is_some() && v.duration > 1.0 && v.pending_seek.is_none()
            });
            if ready {
                break;
            }
            viewer.update(cx, |v, cx| {
                let _ = v.tick(cx);
            });
            cx.background_executor.timer(Duration::from_millis(5)).await;
            wait += 1;
            assert!(wait < 6000, "等待首帧/时长超时");
        }
        let duration = cx.update(|app| viewer.read(app).duration);
        let raw = [72.0f64, 191.0, 354.0];
        let hi = (duration - 2.0).max(0.05);
        let mut second_f2s: Vec<f64> = Vec::with_capacity(raw.len());
        for &sec in &raw {
            let sec = sec.clamp(0.05, hi);
            let fraction = (sec / duration).clamp(0.0, 1.0) as f32;
            // 第一次点击：正常 seek 落地后取 frame2screen 作对照（不作护栏）
            let t0 = Instant::now();
            viewer.update(cx, |v, cx| v.begin_scrub(fraction, cx));
            viewer.update(cx, |v, cx| v.end_scrub(fraction, cx));
            let target = cx.update(|app| viewer.read(app).playhead);
            let deadline = t0 + Duration::from_secs(30);
            let _first_landed = loop {
                // 不手动 tick：pump 由「worker 帧就绪通知」事件驱动自行落地（同
                // bench_pump_framewake_lands_seek_wall 的三者合拍轮询）
                cx.executor().advance_clock(Duration::from_millis(1));
                cx.background_executor.timer(Duration::from_millis(2)).await;
                let s = cx.update(|app| {
                    let v = viewer.read(app);
                    (v.pending_seek, v.last_frame_pts)
                });
                if s.0.is_none() {
                    break s.1;
                }
                assert!(
                    Instant::now() < deadline,
                    "第一次 seek 未落地（target={target:.2}s）"
                );
                std::thread::sleep(Duration::from_millis(1));
            };
            let first_f2s = match take_seek_diag_test() {
                Some((_t, _re, _re2, _sk, _s2f, f2s, _tg, _fp)) => f2s,
                None => 41.0, // 罕见丢快照兜底（保守），不影响护栏断言
            };
            // 注入残留通知：模拟「上一 seek worker 续播尾通知」停在 cap-1 wake 通道，
            // 泵退出/第二次点击重起之间无人消费（生产真实窗口；测试以注入逼近）
            viewer.update(cx, |v, _| {
                if let Some(w) = &v.frame_wake {
                    let _ = w.tx.try_send(());
                }
            });
            // 第二次点击：同落点连点，断言 frame2screen 不劣化（旧实现可睡满 200ms 静默）
            let t1 = Instant::now();
            viewer.update(cx, |v, cx| v.begin_scrub(fraction, cx));
            viewer.update(cx, |v, cx| v.end_scrub(fraction, cx));
            let deadline = t1 + Duration::from_secs(30);
            let second_landed = loop {
                cx.executor().advance_clock(Duration::from_millis(1));
                cx.background_executor.timer(Duration::from_millis(2)).await;
                let s = cx.update(|app| {
                    let v = viewer.read(app);
                    (v.pending_seek, v.last_frame_pts)
                });
                if s.0.is_none() {
                    break s.1;
                }
                assert!(
                    Instant::now() < deadline,
                    "第二次 seek 未落地（target={target:.2}s）"
                );
                std::thread::sleep(Duration::from_millis(1));
            };
            let second_f2s_ms = match take_seek_diag_test() {
                Some((_t, _re, _re2, _sk, _s2f, f2s, _tg, _fp)) => f2s,
                None => 41.0,
            };
            second_f2s.push(second_f2s_ms);
            eprintln!(
                "[SEEKPUMP2] target={target:.1}s 第一次 frame2screen={first_f2s:.1}ms 第二次 frame2screen={second_f2s_ms:.1}ms landed={:?}",
                second_landed.map(|p| format!("{p:.2}s"))
            );
        }
        let mut fs2 = second_f2s.clone();
        fs2.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let f_med = fs2[fs2.len() / 2];
        eprintln!(
            "[SEEKPUMP2] 摘要 n={} 第二次(frame2screen) 中位={f_med:.1}ms（<30ms 护栏：不得劣化回 200ms 兜底静默）",
            second_f2s.len(),
        );
        assert!(
            f_med < 30.0,
            "同落点连点两次，第二次 frame2screen 中位应 <30ms，实得 {f_med:.1}ms（疑似泵睡整拍静默）"
        );
        cx.run_until_parked();
    }

    /// 性能基准（纯代码、不开 GUI，真解码 + 真墙钟）：**播放节拍等待中发起 seek 的回归
    /// 护栏**——pump 从**播放稳态**（`pending_seek` 空、停于播放支路等待）发起 seek
    ///（begin_scrub+end_scrub 单击）。若播放支路仍像旧实现「整睡一拍帧率节拍定时」，落点帧
    /// 入队通知要等下一拍才被排空，frame2screen 会偶发撑到 ≈ 节拍（≈41ms）——本 bench 真解码、
    /// 真墙钟、不手动 tick（pump 自行落地），断言 frame2screen 中位 < 30ms 护栏（播放等待也
    /// 监听 wake 通道，事件即醒消末梢）。与 [`bench_pump_framewake_lands_seek_wall`] 同姿势；
    /// 每次 seek 前垫一小段真实时间让泵渡回播放支路等待。夹具 `target\视频测试字幕.mkv`；
    /// 缺夹具 SKIP。
    #[gpui::test]
    #[allow(clippy::await_holding_lock)]
    async fn bench_pump_seek_from_playback_beat_wall(cx: &mut TestAppContext) {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/视频测试字幕.mkv");
        if !path.exists() {
            eprintln!("SKIP: missing target/视频测试字幕.mkv（长 GOP 夹具不入库）");
            return;
        }
        let _ffmpeg_guard_test = ffmpeg_guard_lock().lock().unwrap_or_else(|e| e.into_inner());
        let _decode_mode = crate::decode::DecodeModeGuard::new(2); // SAFE 硬解优先（skip 快路径）
        let viewer: gpui::Entity<VideoViewer> =
            cx.update(|app| app.new(|cx| VideoViewer::new(path, cx)));
        let mut wait = 0;
        loop {
            let ready = cx.update(|app| {
                let v = viewer.read(app);
                v.frame.is_some() && v.duration > 1.0 && v.pending_seek.is_none()
            });
            if ready {
                break;
            }
            viewer.update(cx, |v, cx| {
                let _ = v.tick(cx);
            });
            cx.background_executor.timer(Duration::from_millis(5)).await;
            wait += 1;
            assert!(wait < 6000, "等待首帧/时长超时");
        }
        let duration = cx.update(|app| viewer.read(app).duration);
        let raw = [72.0f64, 191.0, 354.0, 900.0];
        let hi = (duration - 2.0).max(0.05);
        let mut f2s: Vec<f64> = Vec::with_capacity(raw.len());
        for &sec in &raw {
            let sec = sec.clamp(0.05, hi);
            // 垫播放稳态期：上一 seek 落地后 pending_seek 已空、泵停在播放支路；再推 ~35ms
            // 让泵渡完上一轮 tick 并落进新的播放等待（回归窗口：本次 seek 恰逢播放节拍/
            // 事件等待的任意相位发起）。
            let settle = Instant::now() + Duration::from_millis(35);
            while Instant::now() < settle {
                cx.executor().advance_clock(Duration::from_millis(1));
                cx.background_executor.timer(Duration::from_millis(2)).await;
            }
            let fraction = (sec / duration).clamp(0.0, 1.0) as f32;
            let t0 = Instant::now();
            viewer.update(cx, |v, cx| v.begin_scrub(fraction, cx));
            viewer.update(cx, |v, cx| v.end_scrub(fraction, cx));
            let target = cx.update(|app| viewer.read(app).playhead);
            let deadline = Instant::now() + Duration::from_secs(30);
            let landed = loop {
                // 不手动 tick：pump 自行落地（播放等待中 seek → 事件即醒；测试执行器
                // fake + 真实 worker 通知经 advance_clock/真实 timer 合拍驱动，同既有 bench）
                cx.executor().advance_clock(Duration::from_millis(1));
                cx.background_executor.timer(Duration::from_millis(2)).await;
                let s = cx.update(|app| {
                    let v = viewer.read(app);
                    (v.pending_seek, v.last_frame_pts)
                });
                if s.0.is_none() {
                    break s.1;
                }
                assert!(
                    Instant::now() < deadline,
                    "播放节拍等待中的 seek 未自行落地（target={target:.2}s）"
                );
                std::thread::sleep(Duration::from_millis(1));
            };
            match take_seek_diag_test() {
                Some((_t, _re, _re2, _sk, _s2f, f2s_ms, _tg, _fp)) => {
                    f2s.push(f2s_ms);
                    eprintln!(
                        "[SEEKPLAYW] total={:.1}ms frame2screen={f2s_ms:.1}ms target={target:.1}s landed={:?}",
                        t0.elapsed().as_secs_f64() * 1e3,
                        landed.map(|p| format!("{p:.2}s"))
                    );
                }
                None => {
                    f2s.push(t0.elapsed().as_secs_f64() * 1e3);
                    eprintln!(
                        "[SEEKPLAYW] target={target:.1}s click2frame={:.1}ms landed={:?}（无 diag 快照，兜底）",
                        t0.elapsed().as_secs_f64() * 1e3,
                        landed.map(|p| format!("{p:.2}s"))
                    );
                }
            }
        }
        let mut fs = f2s.clone();
        fs.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let f_med = fs[fs.len() / 2];
        eprintln!(
            "[SEEKPLAYW] 摘要 n={} frame2screen 中位={f_med:.1}ms max={:.1}ms（<30ms 护栏：播放节拍等待中 seek 不得整睡一拍）",
            f2s.len(),
            fs[fs.len() - 1],
        );
        assert!(
            f_med < 30.0,
            "播放节拍等待中 seek 的 frame2screen 中位应 <30ms，实得 {f_med:.1}ms（疑似播放支路整睡一拍节拍）"
        );
        cx.run_until_parked();
    }

    /// 性能基准（纯代码、不开 GUI，真 SAFE 硬解 + 真墙钟）：**P2 hover SAFE**——hover 预解码
    /// worker 改走 SAFE 硬解（`open_scaled_hw` + `skip_to_bgra_into` 跳帧免转换快路径，独立
    /// 线程独立 reader）。暂停 → 悬停深 GOP 落点 → 等预解码缓存抵达（量 preroll 墙钟）→ 点击
    /// 命中亚毫秒装屏：断言首帧 pts≥落点、命中窗内、无待解 pending（命中/未命中语义与软解
    /// 基准 `bench_hover_click_land_frame_wall` 逐字一致）。
    /// [SEEKBENCHHOVERHW] 行与 [SEEKBENCHHOVER] 同口径。夹具 `target\视频测试字幕.mkv`；
    /// 缺夹具 SKIP。
    #[gpui::test]
    #[allow(clippy::await_holding_lock)]
    async fn bench_hover_safe_hw_preroll_wall(cx: &mut TestAppContext) {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/视频测试字幕.mkv");
        if !path.exists() {
            eprintln!("SKIP: missing target/视频测试字幕.mkv（长 GOP 夹具不入库）");
            return;
        }
        let _ffmpeg_guard_test = ffmpeg_guard_lock().lock().unwrap_or_else(|e| e.into_inner());
        let _decode_mode = crate::decode::DecodeModeGuard::new(2); // SAFE 硬解优先
        hw_engaged_reset();
        let viewer: gpui::Entity<VideoViewer> =
            cx.update(|app| app.new(|cx| VideoViewer::new(path, cx)));
        let mut wait = 0;
        loop {
            let ready = cx.update(|app| {
                let v = viewer.read(app);
                v.frame.is_some() && v.duration > 1.0 && v.pending_seek.is_none()
            });
            if ready {
                break;
            }
            viewer.update(cx, |v, cx| {
                let _ = v.tick(cx);
            });
            cx.background_executor.timer(Duration::from_millis(5)).await;
            wait += 1;
            assert!(wait < 6000, "等待首帧/时长超时");
        }
        let duration = cx.update(|app| viewer.read(app).duration);
        let raw = [191.0f64, 354.0, 447.0, 1335.0];
        let hi = (duration - 2.0).max(0.05);
        let mut rows: Vec<(f64, f64, f64)> = Vec::new(); // (target, preroll_ms, regress=pts-target)
        for &sec in &raw {
            let sec = sec.clamp(0.05, hi);
            // 暂停 + 清在途：悬停预解码只在空闲态投（机会式）
            viewer.update(cx, |v, _cx| {
                v.playing = false;
                v.pending_seek = None;
            });
            let t0 = Instant::now();
            viewer.update(cx, |v, cx| v.update_hover_target(sec, cx));
            let hdeadline = Instant::now() + Duration::from_secs(15);
            loop {
                cx.background_executor.timer(Duration::from_millis(5)).await;
                viewer.update(cx, |v, cx| {
                    let _ = v.tick(cx);
                });
                let got = cx.update(|app| viewer.read(app).hover_cache.is_some());
                if got {
                    break;
                }
                assert!(Instant::now() < hdeadline, "SAFE hover 预解码缓存超时（target={sec:.2}s）");
            }
            let preroll_ms = t0.elapsed().as_secs_f64() * 1e3;
            let hp = cx
                .update(|app| viewer.read(app).hover_cache.as_ref().map(|f| f.pts))
                .expect("悬停缓存已抵达");
            assert!(hp >= sec, "SAFE 预解码帧 pts 不得早于落点 target={sec} pts={hp}");
            // 点击命中：begin+end 同分数 → 直接装屏，无 seek（未命中语义由既有 soft 基准盖）
            let fraction = (sec / duration).clamp(0.0, 1.0) as f32;
            viewer.update(cx, |v, cx| v.begin_scrub(fraction, cx));
            viewer.update(cx, |v, cx| v.end_scrub(fraction, cx));
            let (pend, pts) = cx.update(|app| {
                let v = viewer.read(app);
                (v.pending_seek, v.last_frame_pts)
            });
            assert!(pend.is_none(), "SAFE 悬停命中点击后不应有待解 pending（target={sec}）");
            let p = pts.expect("命中点击应即时有帧上屏");
            assert!(
                p >= sec && p - sec <= HOVER_HIT_WINDOW_S,
                "SAFE 悬停点击首帧应在命中窗内且不早于落点 target={sec} pts={p}"
            );
            rows.push((sec, preroll_ms, hp - sec));
            eprintln!(
                "[SEEKBENCHHOVERHW] preroll={preroll_ms:.1}ms total≈0ms reuse=1 regress={:+.2}s skipped=0 firstpts={hp:.2}s target={sec:.2}s（命中 SAFE hover 缓存，无 seek）",
                hp - sec,
            );
        }
        let mut pr: Vec<f64> = rows.iter().map(|r| r.1).collect();
        pr.sort_by(|a, b| a.partial_cmp(b).unwrap());
        eprintln!(
            "[SEEKBENCHHOVERHW] 摘要 n={} preroll min={:.1}ms median={:.1}ms max={:.1}ms（SAFE 硬解 skip 快路径）",
            pr.len(),
            pr[0],
            pr[pr.len() / 2],
            pr[pr.len() - 1],
        );
        cx.run_until_parked();
    }

    /// P3 回归：`pending_seek` 在途期间 `update_hover_target` **不得投新 hover 预解码命令**
    ///（省 FFMPEG_LOCK 竞争 + 多余解码；换代逻辑本可丢弃过期命令，但白投/白解码也要省）。
    /// seek 清空后恢复投递（悬停→点击行为表：在途时不投、空闲时投，单击命中由既有
    /// `bench_hover_click_land_frame_wall` 覆盖）。
    #[gpui::test]
    async fn hover_not_dispatched_during_pending_seek(cx: &mut TestAppContext) {
        let ent = cx.update(|app| app.new(|cx| {
            let mut v = blank(fixture(), cx);
            v.duration = 10.0;
            v
        }));
        // 在途 seek：悬停仅记录位置、不投命令（hover_generation 不增、worker 不被拉起）
        ent.update(cx, |v, _| v.pending_seek = Some(5.0));
        let gen_before = ent.update(cx, |v, _| v.hover_generation.load(Ordering::SeqCst));
        ent.update(cx, |v, cx| v.update_hover_target(4.0, cx));
        let gen_after = ent.update(cx, |v, _| v.hover_generation.load(Ordering::SeqCst));
        assert_eq!(gen_after, gen_before, "pending_seek 在途：不得投新 hover 预解码命令");
        assert!(
            ent.update(cx, |v, _| v.hover_cmd.is_none()),
            "在途 seek 不应拉起 hover worker（零白投）"
        );
        // 位置仍被记录（悬停本身不丢）
        assert_eq!(ent.update(cx, |v, _| v.hovered_target), Some(4.0));
        // seek 清空后恢复投递（机会式空闲态）
        ent.update(cx, |v, _| v.pending_seek = None);
        let gen_b = ent.update(cx, |v, _| v.hover_generation.load(Ordering::SeqCst));
        ent.update(cx, |v, cx| v.update_hover_target(4.0, cx));
        let gen_a = ent.update(cx, |v, _| v.hover_generation.load(Ordering::SeqCst));
        assert!(gen_a > gen_b, "pending_seek 清空后应恢复投递 hover 预解码");
        assert!(
            ent.update(cx, |v, _| v.hover_cmd.is_some()),
            "空闲态悬停应拉起 hover worker 投递预解码"
        );
        cx.run_until_parked();
    }
    #[gpui::test]
    async fn pump_installs_latest_frame_in_window(cx: &mut TestAppContext) {
        let ent = cx.update(|app| app.new(|cx| blank(fixture(), cx)));
        let (tx, rx) = sync_channel::<FrameItem>(4);
        tx.send(frame_of(0.0, 1)).unwrap();
        tx.send(frame_of(0.4, 2)).unwrap();
        tx.send(frame_of(1.5, 3)).unwrap();
        /* 设置 playhead 0.5 后 tick：0.0/0.4 ≤0.5 → 取 0.4 上屏；1.5 越线进 overflow */
        ent.update(cx, |v, cx| {
            v.rx = Some(rx);
            v.playhead = 0.5;
            v.tick(cx);
        });
        let s = cx.update(|app| {
            let v = ent.read(app);
            (v.frame.is_some(), v.overflow.as_ref().map(|f| f.pts()))
        });
        assert!(s.0, "应有帧上屏");
        assert_eq!(s.1, Some(1.5), "越线帧保留在 overflow");
        // playhead 越过 1.5：旧 overflow(1.5) 移交上屏、通道里的 1.6 进位为 overflow
        tx.send(frame_of(1.6, 4)).unwrap();
        ent.update(cx, |v, cx| {
            v.playhead = 1.55;
            v.tick(cx);
        });
        let o = cx.update(|app| ent.read(app).overflow.as_ref().map(|f| f.pts()));
        assert_eq!(o, Some(1.6), "越过旧 overflow 后新越线帧进位");
        cx.run_until_parked();
    }

    /// P0 回归：旧帧被新帧替换时显式 `drop_image` 释放被换下的旧图集 tile。视频每帧
    /// 新建 `RenderImage`（`ImageId` 全局自增）→ 若只换 `self.frame` 不释放，sprite
    /// atlas 键只增不减 → 长会话 VRAM 上涨。断言：首帧无旧帧不 drop；替换时只 drop
    /// 旧帧 id、当前帧留存；旧帧 Arc 不再被查看器持有。
    #[gpui::test]
    async fn install_bgra_drops_old_frame(cx: &mut TestAppContext) {
        let ent = cx.update(|app| app.new(|cx| blank(fixture(), cx)));
        let (w, h) = (64u32, 48u32);
        let bytes = |v: u8| vec![v; (w * h * 4) as usize];
        ent.update(cx, |v, cx| v.install_bgra(w, h, bytes(1), cx));
        let first = cx.update(|app| ent.read(app).frame.clone().expect("首帧已上屏"));
        assert_eq!(
            cx.update(|app| ent.read(app).dropped_frame),
            None,
            "首帧无旧帧可 drop"
        );
        ent.update(cx, |v, cx| v.install_bgra(w, h, bytes(2), cx));
        let (dropped, current) = cx.update(|app| {
            let v = ent.read(app);
            (v.dropped_frame, v.frame.as_ref().map(|f| f.id.0))
        });
        assert_eq!(dropped, Some(first.id.0), "替换时旧帧应被 drop");
        assert_ne!(Some(first.id.0), current, "当前帧不被 drop");
        assert_eq!(Arc::strong_count(&first), 1, "旧帧 Arc 不再被查看器持有");
        cx.run_until_parked();
    }

    /// P0 补充：`frame` 被置空（切文件把旧媒体最后一帧换下）时同样显式释放旧帧图集
    /// tile，否则切换 Playlist 多支每切一次漏一片。走 `drop_old_frame` 直接断言释放
    /// 语义（不入 `open_folder_index`——那会触发真实解码，见真解码测试的 FFMPEG_LOCK）。
    #[gpui::test]
    async fn replacing_with_none_releases_old_frame(cx: &mut TestAppContext) {
        let ent = cx.update(|app| app.new(|cx| blank(fixture(), cx)));
        let (w, h) = (64u32, 48u32);
        ent.update(cx, |v, cx| v.install_bgra(w, h, vec![1; (w * h * 4) as usize], cx));
        let first = cx.update(|app| ent.read(app).frame.clone().expect("首帧已上屏"));
        ent.update(cx, |v, cx| {
            if let Some(old) = v.frame.take() {
                v.drop_old_frame(old, cx);
            }
        });
        let (dropped, current) = cx.update(|app| {
            let v = ent.read(app);
            (v.dropped_frame, v.frame.as_ref().map(|f| f.id.0))
        });
        assert_eq!(dropped, Some(first.id.0), "置空替换应释放旧帧");
        assert_eq!(current, None, "旧帧之后是空帧（加载占位前）");
        assert_eq!(Arc::strong_count(&first), 1, "旧帧 Arc 不再被持有");
        cx.run_until_parked();
    }

    // ---- 真实图集断言（评审新增）：tile 释放必须命中「正在播放的窗口」----
    struct AtlasImg {
        cur: std::rc::Rc<std::cell::RefCell<Option<Arc<RenderImage>>>>,
    }
    impl AtlasImg {
        fn new(_: &mut gpui::Context<Self>) -> Self {
            Self {
                cur: std::rc::Rc::new(std::cell::RefCell::new(None)),
            }
        }
    }
    impl gpui::Render for AtlasImg {
        fn render(
            &mut self,
            _window: &mut gpui::Window,
            _cx: &mut gpui::Context<Self>,
        ) -> impl gpui::IntoElement {
            if let Some(img) = self.cur.borrow().clone() {
                return gpui::img(gpui::ImageSource::Render(img))
                    .w(gpui::px(8.))
                    .h(gpui::px(8.))
                    .into_any_element();
            }
            gpui::div().into_any_element()
        }
    }
    struct AtlasHost {
        img: gpui::Entity<AtlasImg>,
    }
    impl gpui::Render for AtlasHost {
        fn render(
            &mut self,
            _window: &mut gpui::Window,
            _cx: &mut gpui::Context<Self>,
        ) -> impl gpui::IntoElement {
            self.img.clone()
        }
    }
    fn atlas_render_image(w: u32, h: u32, v: u8) -> Arc<RenderImage> {
        Arc::new(RenderImage::new(vec![Frame::new(
            RgbaImage::from_raw(w, h, vec![v; (w * h * 4) as usize]).unwrap(),
        )]))
    }
    /// 真实图集回归（评审发现并修复）：`drop_image(_, None)` 必须释放**正在播放的窗口**
    /// 的 tile——不管是在顶层 App 上下文（泵/自动切）还是窗口 update 上下文（Button/Key
    /// 手动切）触发。复现：gpui `App::drop_image(image, None)` 只遍历 `App.windows` 里未被
    /// 正在更新取走的窗口；窗口自己的回调在 `update_window_erased`（当前窗口被临时移出
    /// `App.windows`）里跑，当场 drop 会漏掉当前窗口 → 每手动切一次漏一片 tile。修法：
    /// `drop_old_frame` 把释放 `cx.defer` 到本拍更新末尾（当前窗口已放回）再 drop。
    /// 断言①②：顶层/窗口两种上下文都不会在当前窗口残留 tile；当前帧进图集后又可出。
    #[gpui::test]
    fn drop_image_reaches_display_window_in_all_contexts(cx: &mut TestAppContext) {
        let (view, mut vcx) = cx.add_window_view(|_window, cx| AtlasHost {
            img: cx.new(AtlasImg::new),
        });
        let upload = |vcx: &mut VisualTestContext, render: &Arc<RenderImage>| {
            vcx.update(|window, cx| {
                let img = view.read(cx).img.clone();
                img.update(cx, |p, _| *p.cur.borrow_mut() = Some(render.clone()));
                window.refresh();
                window.draw(cx).clear(cx);
            });
        };
        let in_atlas = |vcx: &mut VisualTestContext, render: &Arc<RenderImage>| {
            vcx.update(|window, _| window.has_image_atlas_entry(render))
        };
        // ① 顶层 App update（非窗口 update，= 泵/自动切路径）：drop_image(_, None) 命中当前窗口
        let render = atlas_render_image(8, 8, 1);
        upload(&mut vcx, &render);
        assert!(in_atlas(&mut vcx, &render), "画过后应入图集");
        vcx.cx.update(|app| app.drop_image(render.clone(), None));
        assert!(
            !in_atlas(&mut vcx, &render),
            "顶层 App 上下文 drop_image(_, None) 应释放当前窗口 tile"
        );
        // ② 窗口 update 内 entity.update 的 drop（= Button/Key 手动切路径）：经 defer 也应落盘
        let render = atlas_render_image(8, 8, 2);
        upload(&mut vcx, &render);
        assert!(in_atlas(&mut vcx, &render), "画过后应入图集");
        vcx.update(|_window, cx| {
            // 同 `drop_old_frame`：先把自己这条 `drop_image` defer 到本拍更新末尾（当前窗口
            // 已放回 `App.windows`）再释放——窗口上下文下直接 drop 会漏掉当前窗口。
            let img = view.read(cx).img.clone();
            let r = render.clone();
            img.update(cx, move |_p, cx| {
                cx.defer(move |app| app.drop_image(r.clone(), None));
            });
        });
        assert!(
            !in_atlas(&mut vcx, &render),
            "窗口更新上下文里 drop_image(_, None)（defer 后）也应释放当前窗口 tile"
        );
    }

    /// 播放推进：playing 时 playhead 前进；暂停时不前进；到结尾自动停。
    #[gpui::test]
    async fn playhead_respects_pause_and_end(cx: &mut TestAppContext) {
        let ent = cx.update(|app| app.new(|cx| {
            let mut v = blank(fixture(), cx);
            v.playhead = 1.0;
            v.intrinsic = Some((320.0, 240.0));
            v
        }));
        // 暂停：tick 不前
        ent.update(cx, |v, cx| {
            v.last_tick = Some(Instant::now() - Duration::from_millis(100));
            v.tick(cx);
        });
        let p0 = cx.update(|app| ent.read(app).playhead);
        assert!((p0 - 1.0).abs() < 1e-9, "暂停不该前进");
        // 播放：前进
        ent.update(cx, |v, cx| {
            v.playing = true;
            v.last_tick = None;
            v.tick(cx);
        });
        assert!(cx.update(|app| ent.read(app).playhead) > 1.0, "播放应前进");
        // 到结尾：自动停
        ent.update(cx, |v, cx| {
            v.playhead = 2.0;
            v.last_tick = None;
            v.playing = true;
            v.tick(cx);
        });
        let s = cx.update(|app| {
            let v = ent.read(app);
            (v.playhead, v.playing)
        });
        assert_eq!(s.0, 2.0);
        assert!(!s.1, "到结尾应自动暂停");
        cx.run_until_parked();
    }

    /// 音频时钟是主时钟：播放中 `playhead = audio_base + 设备已播位置`；暂停不推进；
    /// 越过时长自动停（有音频时此即 A/V 对齐的帧钟）。
    #[gpui::test]
    async fn playhead_uses_audio_clock_when_present(cx: &mut TestAppContext) {
        let ent = cx.update(|app| app.new(|cx| {
            let mut v = blank(fixture(), cx);
            v.playing = true;
            v.audio_base = 2.0;
            v.playhead = 9.0; // 将被音频时钟覆盖
            v.duration = 100.0;
            v.audio_anchor_pending = false; // 手动给定基准：跳过「上线锚定」
            v
        }));
        ent.update(cx, |v, _| v.advance_playhead(Some(3.5)));
        assert_eq!(
            cx.update(|app| ent.read(app).playhead),
            5.5,
            "音频时钟 playhead = audio_base + 设备已播位置"
        );
        // 暂停：不推进
        ent.update(cx, |v, _| v.playing = false);
        ent.update(cx, |v, _| v.advance_playhead(Some(50.0)));
        assert_eq!(cx.update(|app| ent.read(app).playhead), 5.5, "暂停不推进");
        // 越过时长：自动停并钳到 duration
        ent.update(cx, |v, _| {
            v.playing = true;
            v.duration = 3.0;
            v.advance_playhead(Some(4.0));
        });
        let s = cx.update(|app| {
            let v = ent.read(app);
            (v.playing, v.playhead)
        });
        assert!(!s.0, "音频时钟越过时长应自动停");
        assert_eq!(s.1, 3.0);
        cx.run_until_parked();
    }

    /// seek 钳位：负超/正超都被限制在 [0, duration]。
    #[gpui::test]
    async fn seek_clamps_to_duration(cx: &mut TestAppContext) {
        let ent = cx.update(|app| app.new(|cx| {
            let mut v = blank(fixture(), cx);
            v.playhead = 1.0;
            v
        }));
        ent.update(cx, |v, cx| v.seek(-100.0, cx));
        assert_eq!(cx.update(|app| ent.read(app).playhead), 0.0, "负超钳到 0");
        ent.update(cx, |v, cx| v.seek(999.0, cx));
        assert_eq!(cx.update(|app| ent.read(app).playhead), 2.0, "正超钳到 duration");
        ent.update(cx, |v, _| v.playing = false);
        cx.run_until_parked();
    }

    /// M1 保留 → M2 播放链路：打开真视频 → 后台解析 + 解码线程 + pump 首帧上屏。
    /// 注意：测试环境不驱动 16ms 定时器泵，这里**手动喂 tick**（与 pump 同一条
    /// `tick()→通道→上屏` 路径），证明「解码线程 → 有界通道 → 主线程上屏」端到端。
    #[gpui::test]
    #[allow(clippy::await_holding_lock)] // 持锁跨 await：测试锁与 FFMPEG_LOCK 独立，await 的是定时器而非锁，无死锁风险
    async fn loads_first_frame(cx: &mut TestAppContext) {
        // 缺夹具即 SKIP：真实 mp4 不入库（>1MB），CI/装好夹具的机器才真跑首帧解码。
        if !fixture().exists() {
            eprintln!("SKIP: missing fixtures/video测试.mp4（夹具不入库，本机无真视频）");
            return;
        }
        let _ffmpeg_guard_test = ffmpeg_guard_lock().lock().unwrap_or_else(|e| e.into_inner());
        let viewer: gpui::Entity<VideoViewer> =
            cx.update(|app| app.new(|cx| VideoViewer::new(fixture(), cx)));
        let mut guard = 0;
        let (frame_ok, intrinsic, err) = loop {
            let s = cx.update(|app| {
                let v = viewer.read(app);
                (v.frame.is_some(), v.intrinsic, v.load_error.clone())
            });
            if s.0 || s.2.is_some() {
                break s;
            }
            viewer.update(cx, |v, cx| v.tick(cx));
            cx.run_until_parked();
            // 让常驻 worker 有真实时间解码（并行视频测试在 FFMPEG_LOCK 上会互相竞争，
            // 纯 tick 自旋可能在 worker 没出首帧前就把 guard 跑完 → 偶发超时）
            cx.background_executor.timer(Duration::from_millis(5)).await;
            guard += 1;
            assert!(guard < 6000, "等待视频首帧上屏超时");
        };
        assert!(frame_ok, "首帧应上屏, load_error={err:?}");
        assert!(intrinsic.is_some(), "应取得视频像素尺寸");
        // 多喂几拍：播放状态保持、画面持续在屏（overflow 的排空语义由
        // `pump_installs_latest_frame_in_window` 确定性覆盖；真实解码速度与手动
        // tick 时序不同，不在此断言 overflow 是否为空）
        for _ in 0..60 {
            viewer.update(cx, |v, cx| v.tick(cx));
            cx.run_until_parked();
        }
        let s = cx.update(|app| {
            let v = viewer.read(app);
            (v.playing, v.frame.is_some())
        });
        assert!(s.0, "打开即自动播放且持续在播");
        assert!(s.1, "画面持续在屏");
    }

    /// M1 保留：打开损坏 / 不存在文件：落错误占位不死等。
    #[gpui::test]
    #[allow(clippy::await_holding_lock)] // 持锁跨 await：测试锁与 FFMPEG_LOCK 独立，await 的是定时器而非锁，无死锁风险
    async fn missing_file_reports_error(cx: &mut TestAppContext) {
        let _ffmpeg_guard_test = ffmpeg_guard_lock().lock().unwrap_or_else(|e| e.into_inner());
        let bad = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("no-such-video.mp4");
        let viewer: gpui::Entity<VideoViewer> =
            cx.update(|app| app.new(|cx| VideoViewer::new(bad, cx)));
        let mut guard = 0;
        let (ready, err) = loop {
            let s = cx.update(|app| {
                let v = viewer.read(app);
                (!v.loading, v.load_error.clone())
            });
            if s.0 {
                break s;
            }
            cx.run_until_parked();
            guard += 1;
            assert!(guard < 4000, "等待错误落盘超时");
        };
        assert!(ready);
        assert!(err.is_some(), "应报告打开错误，而不是挂起");
    }

    // ---- M5：音量 / 静音 / 倍速 / 进度条 ----

    /// 音量钳位到 0..=1；静音切换让生效音量 0 ⇄ 原音量（不动 `volume`）。
    #[gpui::test]
    async fn volume_clamps_and_mute_switches(cx: &mut TestAppContext) {
        let ent = cx.update(|app| app.new(|cx| blank(fixture(), cx)));
        ent.update(cx, |v, cx| v.set_volume(2.0, cx));
        assert_eq!(cx.update(|app| ent.read(app).volume), 1.0, "音量钳到 1");
        ent.update(cx, |v, cx| v.set_volume(-1.0, cx));
        assert_eq!(cx.update(|app| ent.read(app).volume), 0.0, "音量钳到 0");
        ent.update(cx, |v, cx| v.set_volume(0.6, cx));
        assert_eq!(cx.update(|app| ent.read(app).effective_volume()), 0.6);
        // 静音：生效音量 0，`volume` 保留
        ent.update(cx, |v, cx| v.toggle_mute(cx));
        let s = cx.update(|app| {
            let v = ent.read(app);
            (v.muted, v.effective_volume(), v.volume)
        });
        assert!(s.0, "静音开关置位");
        assert_eq!(s.1, 0.0, "静音时生效音量 0");
        assert_eq!(s.2, 0.6, "volume 保留以便恢复");
        // 恢复
        ent.update(cx, |v, cx| v.toggle_mute(cx));
        assert_eq!(
            cx.update(|app| ent.read(app).effective_volume()),
            0.6,
            "取消静音恢复原音量"
        );
        cx.run_until_parked();
    }

    /// 倍速档位循环（0.5→1.0→1.5→2.0→0.5）。
    #[gpui::test]
    async fn speed_clamps_and_step_cycles(cx: &mut TestAppContext) {
        let ent = cx.update(|app| app.new(|cx| blank(fixture(), cx)));
        assert_eq!(cx.update(|app| ent.read(app).speed), 1.0, "默认 1×");
        ent.update(cx, |v, cx| v.set_speed(99.0, cx));
        assert_eq!(cx.update(|app| ent.read(app).speed), 4.0, "倍速上限钳位");
        ent.update(cx, |v, cx| v.set_speed(-3.0, cx));
        assert_eq!(cx.update(|app| ent.read(app).speed), 0.25, "倍速下限钳位");
        ent.update(cx, |v, cx| v.set_speed(1.0, cx));
        // 循环：1.0 → 1.5 → 2.0 → 0.5（+1），0.5 → 2.0（-1）
        ent.update(cx, |v, cx| v.speed_step(1, cx));
        assert_eq!(cx.update(|app| ent.read(app).speed), 1.5);
        ent.update(cx, |v, cx| v.speed_step(1, cx));
        assert_eq!(cx.update(|app| ent.read(app).speed), 2.0);
        ent.update(cx, |v, cx| v.speed_step(1, cx));
        assert_eq!(cx.update(|app| ent.read(app).speed), 0.5, "2.0 后循环回 0.5");
        ent.update(cx, |v, cx| v.speed_step(-1, cx));
        assert_eq!(cx.update(|app| ent.read(app).speed), 2.0, "0.5 回退到 2.0");
        cx.run_until_parked();
    }

    /// 音频重采样输出率 = 设备率 ÷ 倍速（2× → 半采样率，0.5× → 双倍），并钳位。
    #[test]
    fn audio_read_rate_scales_with_speed() {
        assert_eq!(audio_read_rate(48000, 1.0), 48000, "1× 原采样率");
        assert_eq!(audio_read_rate(48000, 2.0), 24000, "2× 半采样率");
        assert_eq!(audio_read_rate(48000, 0.5), 96000, "0.5× 双倍采样率");
        assert_eq!(audio_read_rate(48000, 4.0), 12000, "4× 四分之一");
        assert_eq!(audio_read_rate(48000, 99.0), 12000, "超上限按 4×");
        assert_eq!(audio_read_rate(48000, 0.001), 192000, "超下限按 0.25×");
        assert!(audio_read_rate(8000, 16.0) >= 1, "至少 1Hz 兜底");
    }

    /// 墙钟（无音频）推进 × 倍速：speed=2 比 speed=1 前进快一倍。
    /// 以 100ms 为「上次 tick」基准，容忍 ≤10ms 计时游走（真实经过略大于设定值）。
    #[gpui::test]
    async fn wall_clock_advances_at_speed(cx: &mut TestAppContext) {
        let ent = cx.update(|app| app.new(|cx| {
            let mut v = blank(fixture(), cx);
            v.playing = true;
            v.duration = 100.0;
            v
        }));
        ent.update(cx, |v, _| {
            v.playhead = 0.0;
            v.speed = 1.0;
            v.last_tick = Some(Instant::now() - Duration::from_millis(100));
            v.advance_playhead(None);
        });
        let p1 = cx.update(|app| ent.read(app).playhead);
        assert!((p1 - 0.1).abs() < 0.01, "speed=1 100ms 前进 ~0.1s, 实得 {p1}");
        ent.update(cx, |v, _| {
            v.playhead = 0.0;
            v.speed = 2.0;
            v.last_tick = Some(Instant::now() - Duration::from_millis(100));
            v.advance_playhead(None);
        });
        let p2 = cx.update(|app| ent.read(app).playhead);
        assert!((p2 - 0.2).abs() < 0.01, "speed=2 100ms 前进 ~0.2s, 实得 {p2}");
        assert!(p2 > p1 * 1.9, "2× 应比 1× 快近一倍");
        cx.run_until_parked();
    }

    /// 音频时钟 × 倍速：`playhead = audio_base + 设备位置 × speed`。
    #[gpui::test]
    async fn audio_clock_scales_with_speed(cx: &mut TestAppContext) {
        let ent = cx.update(|app| app.new(|cx| {
            let mut v = blank(fixture(), cx);
            v.playing = true;
            v.audio_base = 2.0;
            v.speed = 2.0;
            v.duration = 100.0;
            v.audio_anchor_pending = false; // 手动给定基准：跳过「上线锚定」
            v
        }));
        ent.update(cx, |v, _| v.advance_playhead(Some(3.5)));
        assert_eq!(
            cx.update(|app| ent.read(app).playhead),
            2.0 + 3.5 * 2.0,
            "2× 下设备播 3.5s → 源进度 7s"
        );
        // 暂停不推进（倍速不影响暂停判定）
        ent.update(cx, |v, _| v.playing = false);
        ent.update(cx, |v, _| v.advance_playhead(Some(50.0)));
        assert_eq!(cx.update(|app| ent.read(app).playhead), 9.0, "暂停不推进");
        cx.run_until_parked();
    }

    /// **上线锚定**（修复切视频音画错位）：音频一上线时把 `audio_base` 锚到当前墙钟进度
    /// `playhead − 位置×speed`——切新媒体后设备在静音间隙也累积 `position()`，不锚定会把
    /// playhead 带飞（画面快于声音）。本拍与锚定前 playhead 严格连续（无跳变）。
    #[gpui::test]
    async fn audio_clock_anchors_on_first_sighted(cx: &mut TestAppContext) {
        let ent = cx.update(|app| app.new(|cx| {
            let mut v = blank(fixture(), cx);
            v.playing = true;
            v.playhead = 9.0; // 墙钟已推进到 9s（音频未上线期间）
            v.speed = 2.0;
            v.duration = 100.0;
            v.audio_anchor_pending = true; // 本拍音频首上线 → 触发锚定
            v
        }));
        ent.update(cx, |v, _| v.advance_playhead(Some(3.5)));
        let s = cx.update(|app| {
            let v = ent.read(app);
            (v.audio_base, v.playhead, v.audio_anchor_pending)
        });
        assert_eq!(s.0, 9.0 - 3.5 * 2.0, "audio_base 锚到墙钟（消掉启动间隙）");
        assert_eq!(s.1, 9.0, "playhead 无跳变（连续）");
        assert!(!s.2, "锚定只执行一次");
        // 后续走音频时钟（不再重锚）
        ent.update(cx, |v, _| v.advance_playhead(Some(4.0)));
        assert!(
            (cx.update(|app| ent.read(app).playhead) - (9.0 - 3.5 * 2.0 + 4.0 * 2.0)).abs() < 1e-6,
            "锚定后按音频时钟推进"
        );
        cx.run_until_parked();
    }

    /// 回归（② 首帧落地即唤醒的机制本体）：`pending_seek` 活跃时泵进入**事件驱动**等待
    ///（阻塞在 worker 帧就绪通知上），`pump_wait_interval()` 返回兜底超时
    ///（[`SEEK_WAKE_CAP_MS`] = 粗粒度刷新节拍），不再 ~2ms 轮询；无待定 seek 时保持帧率
    /// 节拍（fps=24 → 41ms）。
    #[gpui::test]
    async fn pump_wait_interval_uses_framewake_cap_during_pending_seek(cx: &mut TestAppContext) {
        let ent = cx.update(|app| app.new(|cx| {
            let mut v = blank(fixture(), cx);
            v.playing = true;
            v.fps = Some((24, 1)); // pump_interval ≈ 41ms
            v
        }));
        let normal = cx.update(|app| ent.read(app).pump_wait_interval());
        assert_eq!(normal, Duration::from_millis(41), "无待定 seek = 帧率节拍");
        ent.update(cx, |v, _| v.pending_seek = Some(5.0));
        let cap = cx.update(|app| ent.read(app).pump_wait_interval());
        assert_eq!(
            cap,
            Duration::from_millis(SEEK_WAKE_CAP_MS),
            "pending_seek 活跃 = 事件驱动泵兜底超时（不再 ~2ms 轮询）"
        );
        cx.run_until_parked();
    }

    /// 回归（② 端到端）：seek 首帧就绪时，**事件驱动泵**由「worker 帧就绪通知」即时唤醒并
    /// drain+上屏——不依赖帧率节拍（fps=24 → 41ms）、无需轮询窗口。入队帧 + 补发帧就绪
    /// 通知后，pump 立即上屏首帧、`pending_seek` 清空；反向护栏：若泵退化成纯帧率节拍轮询
    ///（只等 41ms 定时），发通知不会让它提前醒来，首帧不会在通知后的短窗内上屏，断言即红。
    #[gpui::test]
    async fn pump_framewake_lands_seek_first_frame_fast(cx: &mut TestAppContext) {
        let ent = cx.update(|app| app.new(|cx| {
            let mut v = blank(fixture(), cx);
            v.playing = true;
            v.duration = 10.0;
            v.fps = Some((24, 1)); // 正常节拍 41ms（>事件驱动，护栏判据）
            v
        }));
        // 构造 seek 交付态：落点/帧通道就位，pending_seek 置位，再起泵
        let (tx, rx) = sync_channel::<FrameItem>(DECODE_AHEAD_FRAMES);
        ent.update(cx, |v, _| {
            v.playhead = 5.0;
            v.rx = Some(rx);
            v.pending_seek = Some(5.0);
        });
        ent.update(cx, |v, cx| v.ensure_pump(cx));
        // worker 行为：预滚最后一帧（≤ 落点，首帧即可上屏）+ 首个 ≥ 落点帧；
        // 每入队一帧补一个帧就绪信号（P1 事件驱动泵的信号来源；真实 worker 见
        // `video_worker` 的 `cmd.wake.try_send`）
        tx.send(frame_of(4.96, 1)).ok();
        tx.send(frame_of(5.1, 9)).ok();
        ent.update(cx, |v, _| {
            if let Some(w) = &v.frame_wake {
                let _ = w.tx.try_send(());
            }
        });
        // 帧就绪通知由后台送达：轮询等待 pump 落地（墙钟 deadline；fake 时钟
        // 不能作节拍）。`advance_clock` 兼驱 cfg(test) 泵的兜底 fake 定时器（其首个 await
        // 已 `try_recv` 到本通知 → 直落，无需等整拍）。
        let deadline = Instant::now() + Duration::from_secs(5);
        let s = loop {
            cx.executor().advance_clock(Duration::from_millis(1));
            cx.run_until_parked();
            let s = cx.update(|app| {
                let v = ent.read(app);
                (v.frame.is_some(), v.pending_seek)
            });
            if s.0 && s.1.is_none() {
                break s;
            }
            assert!(Instant::now() < deadline, "帧就绪通知后 pump 未及时上屏");
            std::thread::sleep(Duration::from_millis(1));
        };
        assert!(s.0, "帧就绪通知应即时唤醒 pump 上屏首帧（不待帧率节拍 41ms）");
        assert_eq!(s.1, None, "首帧上屏后 pending_seek 应立即清空");
        cx.run_until_parked();
    }

    /// 回归（③ 播放节拍等待期间发起 seek）：泵在**播放稳态**（`pending_seek` 为空）的等待
    /// 也统一走「wake 事件驱动 + 兜底超时」——帧就绪通知即醒，不再整睡一拍帧率节拍。
    /// 本测试把泵固定在播放等待（fps=24 → 帧率节拍 41ms），再注入 seek 交付态（pending_seek +
    /// 落点帧 + 帧就绪通知），以 fake 时钟推进量计落地上屏耗时：断言远小于 41ms 节拍
    ///（若播放支路退化成 `timer(41ms)` 整睡一拍即红——真机对应「seek 落点恰逢播放节拍休眠」
    /// 的 frame2screen 38-76ms 末梢）。
    #[gpui::test]
    async fn pump_playback_beat_wait_wakes_promptly_on_seek_event(cx: &mut TestAppContext) {
        let ent = cx.update(|app| app.new(|cx| {
            let mut v = blank(fixture(), cx);
            v.playing = true;
            v.duration = 10.0;
            v.fps = Some((24, 1)); // 播放节拍 41ms（旧播放支路整睡一拍 = 回归判据）
            v.playhead = 5.0;
            v
        }));
        let (tx, rx) = sync_channel::<FrameItem>(DECODE_AHEAD_FRAMES);
        ent.update(cx, |v, _| {
            v.rx = Some(rx);
        });
        // 泵进入播放等待（pending_seek 为空 → 播放支路）；run_until_parked 让泵落进等待
        ent.update(cx, |v, cx| v.ensure_pump(cx));
        cx.run_until_parked();
        // 此时发起 seek：置落点 + 交付①落点帧（≥ 落点，模拟 worker 首帧入队）②帧就绪通知。
        // `scrubbing=true` 让 land_seek = pending_seek：首个 ≥ 落点帧立即装屏（与真实点按
        // seek 的落地语义一致，不依赖 playhead 推进）。
        ent.update(cx, |v, _| {
            v.scrubbing = true;
            v.pending_seek = Some(5.0);
        });
        tx.send(frame_of(5.1, 9)).ok();
        ent.update(cx, |v, _| {
            if let Some(w) = &v.frame_wake {
                let _ = w.tx.try_send(());
            }
        });
        // 驱动 fake 时钟：播放支路事件等待每细拍（2ms）自查一次，落点帧应在其后 ≤ 数拍内
        // 装屏——若手写旧实现（播放支路 `timer(41ms)` 兜底），节拍未到泵不动 → 断言红。
        let mut steps = 0u32;
        let s = loop {
            cx.executor().advance_clock(Duration::from_millis(1));
            cx.run_until_parked();
            steps += 1;
            let s = cx.update(|app| {
                let v = ent.read(app);
                (v.frame.is_some(), v.pending_seek)
            });
            if s.0 && s.1.is_none() {
                break s;
            }
            assert!(
                steps < 20,
                "泵在播放节拍等待中收到 seek 事件后未及时上屏（steps={steps}ms，远未到 41ms 节拍即应落屏）"
            );
        };
        assert!(s.0, "seek 落点帧应上屏");
        assert_eq!(s.1, None, "落点帧上屏后 pending_seek 应立即清空");
        eprintln!(
            "[PUMPWAKE] 播放等待→seek 落地 fake 时钟 {steps}ms（节拍 41ms 判据，需 ≤ ~8）"
        );
        assert!(
            steps <= 8,
            "泵应在事件通知后 ≤ 细拍粒度上屏（实测 {steps}ms），播放支路整睡一拍即回归"
        );
        cx.run_until_parked();
    }

    /// 回归（② 暂停态）：暂停中 seek（pump 尚未跑）→ `seek()` 置 `pending_seek` 并起泵；
    /// 泵在事件驱动等待中续跑（等帧就绪通知），不因 `playing=false` 停泵。
    #[gpui::test]
    async fn paused_seek_keeps_framewake_pump_alive(cx: &mut TestAppContext) {
        let ent = cx.update(|app| app.new(|cx| {
            let mut v = blank(fixture(), cx);
            v.playing = false; // 暂停态：初始无 pump
            v.duration = 10.0;
            v
        }));
        ent.update(cx, |v, cx| v.seek(2.0, cx));
        // `ensure_pump` 先置 `pump_running` 再 spawn 任务：seek 返回后同步可见
        let s = cx.update(|app| {
            let v = ent.read(app);
            (v.pending_seek, v.pump_running, v.pump_wanted())
        });
        assert_eq!(s.0, Some(2.0), "暂停 seek 应保留 pending_seek（等首帧上屏）");
        assert!(s.1, "暂停 seek 也应起泵（pending_seek 活跃 → 事件驱动续跑）");
        assert!(s.2, "pending_seek 未清前泵必须持续运行（不因 playing=false 停）");
        // 泵在事件等待上 park（cfg(test) 兜底 fake 定时器可 park；生产为真实阻塞 recv 等
        // worker 通知）——test 结束留给 fake 调度器丢弃即可，无后台泄漏。
        cx.run_until_parked();
        let running = cx.update(|app| ent.read(app).pump_running);
        assert!(running, "pending_seek 未清前泵应持续运行");
        cx.run_until_parked();
    }

    /// 进度条点按/拖拽共用：分数 → 秒数 seek（钳位到 [0, duration]）。
    #[gpui::test]
    async fn seek_to_fraction_clamps(cx: &mut TestAppContext) {
        let ent = cx.update(|app| app.new(|cx| {
            let mut v = blank(fixture(), cx);
            v.playhead = 1.0;
            v.duration = 2.0;
            v.intrinsic = Some((320.0, 240.0));
            v
        }));
        ent.update(cx, |v, cx| v.seek_to_fraction(0.5, cx));
        assert!(
            (cx.update(|app| ent.read(app).playhead) - 1.0).abs() < 1e-6,
            "0.5×2s = 1.0s"
        );
        ent.update(cx, |v, cx| v.seek_to_fraction(3.0, cx));
        assert_eq!(cx.update(|app| ent.read(app).playhead), 2.0, "超 1 夹到 duration");
        ent.update(cx, |v, _| v.playing = false);
        cx.run_until_parked();
    }

    /// 条的窗口坐标 → 分数映射：左缘=0、右缘=1、越界钳位、未布局返回 None。
    #[gpui::test]
    async fn strip_fraction_maps_window_position(cx: &mut TestAppContext) {
        let bounds = Cell::new(Bounds::default());
        assert_eq!(
            fraction_at(&bounds, gpui::point(px(5.), px(5.))),
            None,
            "未布局（bounds=0）不应出分数"
        );
        bounds.set(Bounds::new(
            gpui::point(px(10.), px(0.)),
            gpui::size(px(100.), px(16.)),
        ));
        assert_eq!(
            fraction_at(&bounds, gpui::point(px(10.), px(8.))),
            Some(0.0),
            "左缘=0"
        );
        assert_eq!(
            fraction_at(&bounds, gpui::point(px(35.), px(8.))),
            Some(0.25)
        );
        assert_eq!(
            fraction_at(&bounds, gpui::point(px(110.), px(8.))),
            Some(1.0),
            "右缘=1"
        );
        assert_eq!(
            fraction_at(&bounds, gpui::point(px(5.), px(8.))),
            Some(0.0),
            "越左钳 0"
        );
        assert_eq!(
            fraction_at(&bounds, gpui::point(px(200.), px(8.))),
            Some(1.0),
            "越右钳 1"
        );
        cx.update(|_| {});
    }

    /// contain 适配（手动像素框）：竖屏在宽视口里高度贴边、两侧居中留白（不再按固有
    /// 尺寸放大只露顶部）；横屏宽度贴边、上下留白；任意比例保持原 aspect。
    #[test]
    fn contain_fit_preserves_aspect_and_letterboxes() {
        // 竖屏 720×1280 @ 1200×600 视口（预留控制条后 area_h=548）
        let (dw, dh) = VideoViewer::contain_fit(720.0, 1280.0, 1200.0, 548.0);
        assert!((dw / dh - 720.0 / 1280.0).abs() < 1e-3, "保持竖屏比例");
        assert!((dh - 548.0).abs() < 0.001, "高度贴边 dh={dh}");
        assert!(dw < 1200.0, "宽度留白（两侧黑边）dw={dw}");
        assert!(dw > 100.0, "仍有可见宽度");
        // 横屏 1280×720 @ 同视口
        let (dw, dh) = VideoViewer::contain_fit(1280.0, 720.0, 1200.0, 548.0);
        assert!((dw / dh - 1280.0 / 720.0).abs() < 1e-3, "保持横屏比例");
        assert!((dh - 548.0).abs() < 0.001, "横屏高度贴边 dh={dh}");
        // 超宽再验证两轴都 ≤ 视口
        let (dw, dh) = VideoViewer::contain_fit(4096.0, 2160.0, 800.0, 548.0);
        assert!(dw <= 800.0 && dh <= 548.0 + 0.001, "两轴都不超视口");
        // 方形视口里竖屏：高度贴边、左右留白（取较小缩放比 48…）
        let (dw, dh) = VideoViewer::contain_fit(720.0, 1280.0, 548.0, 548.0);
        assert!((dh - 548.0).abs() < 0.001, "方形视口竖屏高度贴边 dh={dh}");
        assert!(dw < 548.0, "方形视口竖屏左右留白 dw={dw}");
        assert!((dw / dh - 720.0 / 1280.0).abs() < 1e-3, "比例保持");
    }

    /// 滚轮语义：普通滚轮**不控制进度**（也不再切文件），Ctrl 放大/缩小（钳位），
    /// Shift 同文件夹切换。
    #[gpui::test]
    async fn wheel_semantics_zoom_not_seek(cx: &mut TestAppContext) {
        let ent = cx.update(|app| app.new(|cx| {
            let mut v = blank(fixture(), cx);
            v.nav = FolderNav::new(vec![fixture(), fixture()], 0);
            v.playhead = 0.7;
            v.zoom = 1.0;
            v
        }));
        // 普通滚轮：playhead / current 都不动
        ent.update(cx, |v, cx| v.handle_wheel(1.0, false, false, cx));
        let s = cx.update(|app| {
            let v = ent.read(app);
            (v.playhead, v.nav.current())
        });
        assert_eq!(s, (0.7, 0), "普通滚轮不应 seek / 切文件");
        // Ctrl+滚轮向上 → 放大
        ent.update(cx, |v, cx| v.handle_wheel(1.0, true, false, cx));
        assert!((cx.update(|app| ent.read(app).zoom) - 1.12).abs() < 1e-5, "1 档放大");
        // Ctrl+滚轮向下 → 缩小，并钳到下限
        for _ in 0..200 {
            ent.update(cx, |v, cx| v.handle_wheel(-1.0, true, false, cx));
        }
        assert_eq!(cx.update(|app| ent.read(app).zoom), ZOOM_MIN, "缩到下限");
        for _ in 0..200 {
            ent.update(cx, |v, cx| v.handle_wheel(1.0, true, false, cx));
        }
        assert_eq!(cx.update(|app| ent.read(app).zoom), ZOOM_MAX, "放大到上限");
        // Shift+滚轮向下 → 下一支
        ent.update(cx, |v, cx| v.handle_wheel(-1.0, false, true, cx));
        assert_eq!(cx.update(|app| ent.read(app).nav.current()), 1, "Shift 向下切下一支");
        cx.run_until_parked();
    }

    /// 平移 clamp（同 ImageViewer）：未放大（图适合画布区）恒居中、拖拽无效果；
    /// 放大后按 ±(图−画布区)/2 各轴独立 clamp，界内不动。
    #[test]
    fn clamp_pan_covers_viewport_and_stays_centered_when_unzoomed() {
        let cp = VideoViewer::clamp_pan;
        // 未放大（图 500×300 适合画布区 1000×700）：任一轴 pan 都 clamp 到 0 → 居中
        assert_eq!(cp((50.0, -30.0), 1000.0, 700.0, 500.0, 300.0), (0.0, 0.0));
        // 横向放大（图 1500 > 视口 1000）：可滑 ±250，越界裁剪；纵向仍适配 → y=0
        assert_eq!(cp((1000.0, 999.0), 1000.0, 700.0, 1500.0, 700.0), (250.0, 0.0));
        assert_eq!(cp((-1000.0, 0.0), 1000.0, 700.0, 1500.0, 700.0), (-250.0, 0.0));
        // 两轴都放大：各轴独立 clamp 到 ±(图−视口)/2；界内保持
        assert_eq!(cp((-1000.0, 200.0), 1000.0, 700.0, 1500.0, 900.0), (-250.0, 100.0));
        assert_eq!(cp((80.0, -40.0), 1000.0, 700.0, 1500.0, 900.0), (80.0, -40.0));
    }

    /// 左键按住拖动画面：放大态下 `#video-main` 拖拽 → `pan` 按鼠标增量跟随并 clamp 到
    /// 图覆盖画布区；未放大（zoom=1）拖动无效果（clamp 归零）；松开清拖拽态。真窗口
    /// 事件驱动（模拟 down/move/up），覆盖「读写 viewport 实测尺寸 + display_size 同步」链路。
    #[gpui::test]
    async fn drag_pans_when_zoomed_and_clamps(cx: &mut TestAppContext) {
        cx.update(|cx| {
            ui::theme::Theme::set(ui::theme::ThemeMode::Dark, cx);
        });
        let (viewer, wcx) = cx.add_window_view(|_window, cx| {
            // 用不存在的路径：start 快速落错误、不开真实解码 → 不占 FFmpeg 锁
            // （拖拽只依赖 intrinsic/zoom，毋需真实帧）
            VideoViewer::new(
                std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/no-such-drag.mp4"),
                cx,
            )
        });
        // 定住放大态：intrinsic + zoom=4 让显示图远大于画布区（无需真实解码帧）
        wcx.cx.update(|app| {
            viewer.update(app, |v, _| {
                v.intrinsic = Some((640.0, 360.0));
                v.zoom = 4.0;
                v.playing = false;
            })
        });
        let _ = wcx.refresh();
        wcx.run_until_parked();

        let center = wcx.cx.read(|app| {
            let v = viewer.read(app);
            let s = v.viewport.bounds().size;
            gpui::Point::new(px(s.width.as_f32() * 0.5), px(s.height.as_f32() * 0.5))
        });
        // 拖动增量 (120, 80) → pan 跟随（未放大拖柄起点 pan 0）
        wcx.simulate_mouse_down(center, MouseButton::Left, Default::default());
        wcx.simulate_mouse_move(
            gpui::Point::new(center.x + px(120.), center.y + px(80.)),
            Some(MouseButton::Left),
            Default::default(),
        );
        wcx.run_until_parked();
        let pan = wcx.cx.read(|app| viewer.read(app).pan);
        assert!(
            (pan.0 - 120.0).abs() < 1e-3 && (pan.1 - 80.0).abs() < 1e-3,
            "拖动应平移，实得 pan={pan:?}"
        );
        // 松开：清拖拽态
        wcx.simulate_mouse_up(
            gpui::Point::new(center.x + px(120.), center.y + px(80.)),
            MouseButton::Left,
            Default::default(),
        );
        wcx.run_until_parked();
        assert!(
            wcx.cx.read(|app| viewer.read(app).drag.is_none()),
            "松开后拖拽应结束"
        );
        // 再次按下（起点 pan 沿上一段拖拽结果）+ 小增量拖动 → pan 继续累积（平移跨拖拽保持）
        wcx.simulate_mouse_down(
            gpui::Point::new(center.x + px(120.), center.y + px(80.)),
            MouseButton::Left,
            Default::default(),
        );
        wcx.simulate_mouse_move(
            gpui::Point::new(center.x + px(160.), center.y + px(110.)),
            Some(MouseButton::Left),
            Default::default(),
        );
        wcx.run_until_parked();
        let pan2 = wcx.cx.read(|app| viewer.read(app).pan);
        assert!(
            (pan2.0 - 160.0).abs() < 1e-3 && (pan2.1 - 110.0).abs() < 1e-3,
            "再次拖动应从上次 pan 继续累积，实得 pan2={pan2:?}"
        );
        wcx.simulate_mouse_up(
            gpui::Point::new(center.x + px(160.), center.y + px(110.)),
            MouseButton::Left,
            Default::default(),
        );
        cx.run_until_parked();
    }

    /// 纯视频（锁定）切换：`toggle_lock` 翻转 `is_locked` 并把放大后的平移/拖拽归零（干净播放）。
    #[gpui::test]
    async fn toggle_lock_flips_state_and_resets_pan(cx: &mut TestAppContext) {
        let ent = cx.update(|app| app.new(|cx| {
            let mut v = blank(fixture(), cx);
            v.zoom = 4.0;
            v.pan = (100.0, -50.0); // 放大平移中
            v
        }));
        assert!(!cx.update(|app| ent.read(app).is_locked()));
        cx.update(|app| ent.update(app, |v, cx| v.toggle_lock(cx)));
        assert!(cx.update(|app| ent.read(app).is_locked()));
        assert_eq!(cx.update(|app| ent.read(app).pan), (0.0, 0.0), "锁定应归零平移");
        assert!(
            cx.update(|app| ent.read(app).controls_auto.visible()),
            "进锁定控制条默认可见"
        );
        cx.update(|app| ent.update(app, |v, cx| v.toggle_lock(cx)));
        assert!(!cx.update(|app| ent.read(app).is_locked()));
        cx.run_until_parked();
    }

    /// 暂停标志可见性纯函数：有画面且非播放（暂停/自然结尾）→ 显示；播放中 / 无帧（加载中）/
    /// 打开出错（占位）→ 不显示。
    #[test]
    fn paused_badge_shown_only_when_paused_with_frame() {
        // (playing, has_frame, load_error)
        let cases = [
            // 播放中 → 不显示
            (true, true, false),
            (true, false, false),
            // 暂停/结尾且有画面 → 显示
            (false, true, false),
            // 无帧（加载中）或出错 → 不显示
            (false, false, false),
            (false, true, true),
        ];
        for (playing, has_frame, load_error) in cases {
            let want = !playing && has_frame && !load_error;
            assert_eq!(
                VideoViewer::paused_badge_shown(playing, has_frame, load_error),
                want,
                "playing={playing} has_frame={has_frame} load_error={load_error}"
            );
        }
    }

    /// 回归：render 调用点须按「有错 = true」的语义把 `load_error.is_some()` 传入（纯函数
    /// 第三参是 `load_error`）。曾误传 `is_none()` 导致两次取反——无错时徽标永远不显
    /// （真实复现：暂停/结尾有画面不显示）。用「无错/有错/播放中」三态断言调用点公式。
    #[test]
    fn paused_badge_render_uses_has_error_sense() {
        fn shown(playing: bool, has_frame: bool, load_error: Option<()>) -> bool {
            VideoViewer::paused_badge_shown(playing, has_frame, load_error.is_some())
        }
        // 无错 + 暂停 + 有帧 → 显示（若调用点误传 is_none()，这里会 fail）
        assert!(shown(false, true, None));
        // 出错 → 不显示
        assert!(!shown(false, true, Some(())));
        // 播放中 → 不显示
        assert!(!shown(true, true, None));
    }

    /// 回归：控制条玻璃面板底色随明暗主题（白主题=亮玻璃、暗主题=深玻璃）。曾硬编码
    /// #191a1b 面板底色，白主题下控制条仍为黑条（用户实测反馈）；用「亮度」公式钉住：
    /// 亮面 l 必须 >0.9（白 surface）、暗面 l<0.4（深 surface）、且亮面 > 暗面。
    #[test]
    fn control_bar_panel_follows_theme() {
        let light_surface: Hsla = ui::theme::Theme::light().colors.surface.into();
        let dark_surface: Hsla = ui::theme::Theme::dark().colors.surface.into();
        let light_panel = VideoViewer::glass_panel_bg(light_surface, false);
        let dark_panel = VideoViewer::glass_panel_bg(dark_surface, true);
        assert!(light_panel.l > 0.9, "白主题控制条应为亮玻璃，l={}", light_panel.l);
        assert!(dark_panel.l < 0.4, "暗主题控制条应为深玻璃，l={}", dark_panel.l);
        assert!(light_panel.l > dark_panel.l, "亮主题面板必须比暗主题亮");
    }

    /// 向窗口派发一次**真实双击**（mousedown `click_count=2` + mouseup `click_count=2`）：
    /// 走 `simulate_event` 完整分发路径，双击识别与欢迎页同套语义
    /// （`MouseDownEvent.click_count >= 2`）；`simulate_click` 恒为单击，无法覆盖。
    fn simulate_double_click(cx: &mut gpui::VisualTestContext, at: gpui::Point<gpui::Pixels>) {
        cx.simulate_event(gpui::MouseDownEvent {
            button: gpui::MouseButton::Left,
            position: at,
            modifiers: gpui::Modifiers::default(),
            click_count: 2,
            first_mouse: false,
        });
        cx.simulate_event(gpui::MouseUpEvent {
            button: gpui::MouseButton::Left,
            position: at,
            modifiers: gpui::Modifiers::default(),
            click_count: 2,
        });
    }

    /// 全屏退出方向纯函数：未全屏 → 进入；已全屏 → 退出。gpui 测试窗 `is_fullscreen` 从
    /// `toggle_fullscreen` 反读，进出两路真跑；此处仍把方向判定钉死为可注入断言（防以后
    /// 平台改行为只测到半条路）。
    #[test]
    fn should_exit_fullscreen_is_direction_by_current_fs() {
        assert!(!VideoViewer::should_exit_fullscreen(false), "未全屏应进入全屏");
        assert!(VideoViewer::should_exit_fullscreen(true), "已全屏应退出全屏");
    }

    /// 单击画面（click_count<2）不触发全屏/锁定：双击回调直接 return，单点击仍是
    /// `on_pan_down` 的拖拽起点语义（drag 已建立）。不占 FFmpeg 锁（新开不存在路径，
    /// 快速落错误占位）。
    #[gpui::test]
    async fn single_click_does_not_fullscreen(cx: &mut TestAppContext) {
        cx.update(|cx| {
            ui::theme::Theme::set(ui::theme::ThemeMode::Dark, cx);
        });
        let (viewer, wcx) = cx.add_window_view(|_window, cx| {
            VideoViewer::new(
                std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("../../fixtures/no-such-fullscreen.mp4"),
                cx,
            )
        });
        let _ = wcx.refresh();
        wcx.run_until_parked();
        let at = gpui::Point::new(px(120.), px(100.));
        wcx.simulate_mouse_down(at, MouseButton::Left, Default::default());
        wcx.run_until_parked();
        let s = wcx.cx.read(|app| {
            let v = viewer.read(app);
            (v.is_locked(), v.drag.is_some())
        });
        assert!(!s.0, "单击不应进入锁定");
        assert!(s.1, "单击仍是拖拽起点（drag 已建立），双击全屏未抢单击语义");
        wcx.simulate_mouse_up(at, MouseButton::Left, Default::default());
        cx.run_until_parked();
    }

    /// 控制条/进度条命中双击**不**触发全屏/锁定（回归：gpui 鼠标事件在命中栈内冒泡，
    /// 双击进度条曾漏到 `#video-main` 误进全屏——现由控制条 `on_controls_mouse_down`
    /// `stop_propagation` 拦截）。常驻模式与锁定浮层两态都验：seek 条中点双击后
    /// `locked`/`pre_fullscreen_locked` 均不应出现「进入全屏」信号。
    #[gpui::test]
    async fn double_click_on_control_bar_does_not_fullscreen(cx: &mut TestAppContext) {
        cx.update(|cx| {
            ui::theme::Theme::set(ui::theme::ThemeMode::Dark, cx);
        });
        let (viewer, mut wcx) = cx.add_window_view(|_window, cx| {
            VideoViewer::new(
                std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("../../fixtures/no-such-probe.mp4"),
                cx,
            )
        });
        let _ = wcx.refresh();
        wcx.run_until_parked();
        let seek = wcx.cx.read(|app| viewer.read(app).seek_bounds.get());
        assert!(f32::from(seek.size.width) > 0.0, "seek 条应已布局");
        let at = seek.center();
        // 常驻（未锁定）模式：双击进度条不得进全屏
        simulate_double_click(&mut wcx, at);
        wcx.run_until_parked();
        let s = wcx.cx.read(|app| {
            let v = viewer.read(app);
            (v.is_locked(), v.pre_fullscreen_locked)
        });
        assert!(!s.0, "常驻模式双击进度条不应进入锁定/全屏，实得 locked={}", s.0);
        assert!(!s.1, "常驻模式双击进度条不应记录进入前锁态");
        // 锁定浮层（可见）模式：双击进度条同样不得漏到画面
        wcx.cx.update(|app| {
            viewer.update(app, |v, cx| {
                v.set_locked(true, cx);
                v.pre_fullscreen_locked = false; // 手动清零隔离信号
            })
        });
        let _ = wcx.refresh();
        wcx.run_until_parked();
        assert!(
            wcx.cx.read(|app| viewer.read(app).controls_auto.visible()),
            "锁定时控制条应可见（浮层）"
        );
        simulate_double_click(&mut wcx, at);
        wcx.run_until_parked();
        let s = wcx.cx.read(|app| {
            let v = viewer.read(app);
            (v.is_locked(), v.pre_fullscreen_locked)
        });
        assert!(s.0, "锁定浮层双击进度条应保持锁定");
        assert!(!s.1, "锁定浮层双击进度条不应误进全屏（pre_fullscreen_locked 不应被写）");
        cx.run_until_parked();
    }

    /// 双击画面 → 全屏 + 锁定（记录进入前锁态）；再双击 → 退出全屏并**恢复进入前的模式**
    /// （默认未锁定进入 → 退出后回到未锁定）。真实事件分发走真 `toggle_fullscreen`（gpui
    /// 测试平台反读 `is_fullscreen`），进出两路都真跑。
    #[gpui::test]
    async fn double_click_toggles_fullscreen_and_restores_prior_lock(cx: &mut TestAppContext) {
        cx.update(|cx| {
            ui::theme::Theme::set(ui::theme::ThemeMode::Dark, cx);
        });
        let (viewer, mut wcx) = cx.add_window_view(|_window, cx| {
            VideoViewer::new(
                std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("../../fixtures/no-such-fullscreen.mp4"),
                cx,
            )
        });
        let _ = wcx.refresh();
        wcx.run_until_parked();
        let at = gpui::Point::new(px(120.), px(100.));
        // 双击 1：未全屏 → 进入全屏 + 纯视频锁定，记录进入前锁态（false）
        simulate_double_click(&mut wcx, at);
        wcx.run_until_parked();
        let s = wcx.cx.read(|app| {
            let v = viewer.read(app);
            (v.is_locked(), v.pre_fullscreen_locked)
        });
        assert!(s.0, "双击应进入纯视频锁定");
        assert!(!s.1, "应记录进入前的锁态（默认未锁定）");
        // 双击 2：已全屏 → 退出全屏，锁定恢复为进入前模式（未锁定）
        simulate_double_click(&mut wcx, at);
        wcx.run_until_parked();
        let s = wcx.cx.read(|app| {
            let v = viewer.read(app);
            (v.is_locked(), v.pre_fullscreen_locked)
        });
        assert!(!s.0, "再双击应退出全屏并恢复进入前模式（未锁定）");
        assert!(!s.1, "pre_fullscreen_locked 保持快照语义");
        cx.run_until_parked();
    }

    /// 已锁定状态下双击进全屏 → 再双击退出 → **仍保持锁定**（恢复进入前模式 = 原锁定，
    /// 验收口径「进入前若已锁定则保持锁定」）。
    #[gpui::test]
    async fn double_click_keeps_lock_when_was_locked_before(cx: &mut TestAppContext) {
        cx.update(|cx| {
            ui::theme::Theme::set(ui::theme::ThemeMode::Dark, cx);
        });
        let (viewer, mut wcx) = cx.add_window_view(|_window, cx| {
            VideoViewer::new(
                std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("../../fixtures/no-such-fullscreen.mp4"),
                cx,
            )
        });
        let _ = wcx.refresh();
        wcx.run_until_parked();
        let at = gpui::Point::new(px(120.), px(100.));
        // 进入前已锁定（如锁按钮先锁定，再双击全屏）
        wcx.cx.update(|app| viewer.update(app, |v, cx| v.set_locked(true, cx)));
        simulate_double_click(&mut wcx, at);
        wcx.run_until_parked();
        simulate_double_click(&mut wcx, at);
        wcx.run_until_parked();
        let s = wcx.cx.read(|app| {
            let v = viewer.read(app);
            (v.is_locked(), v.pre_fullscreen_locked)
        });
        assert!(s.0, "进入前已锁定 → 退出全屏后应保持锁定");
        assert!(s.1, "pre_fullscreen_locked 记录进入前的 true");
        cx.run_until_parked();
    }

    /// 纯视频（锁定）模式控制条浮层时序：进锁定默认可见 → 无悬停 1s 自隐 → 悬停不隐藏 →
    /// 移开 1s 隐藏 → 切视频浮现。普通模式控制条常驻（`controls_auto.visible()` 不参与渲染判断）。
    #[gpui::test]
    async fn locked_controls_bar_floats_on_hover_and_auto_hides(cx: &mut TestAppContext) {
        let a = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/no-a-lock.mp4");
        let b = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/no-b-lock.mp4");
        let ent = cx.update(|app| app.new(|cx| {
            let mut v = blank(a.clone(), cx);
            v.nav = FolderNav::new(vec![a, b], 0);
            v
        }));
        // 进入锁定 → 控制条默认可见（随后无悬停 1s 自隐）
        cx.update(|app| ent.update(app, |v, cx| v.toggle_lock(cx)));
        assert!(cx.update(|app| ent.read(app).is_locked()));
        assert!(cx.update(|app| ent.read(app).controls_auto.visible()), "进锁定默认可见");
        cx.executor().advance_clock(std::time::Duration::from_secs(1));
        cx.run_until_parked();
        assert!(
            !cx.update(|app| ent.read(app).controls_auto.visible()),
            "无悬停 1s 后应自隐"
        );
        // 悬停 → 浮现，且 2s 内不隐藏
        cx.update(|app| ent.update(app, |v, cx| v.set_controls_hover(true, cx)));
        assert!(cx.update(|app| ent.read(app).controls_auto.visible()));
        cx.executor().advance_clock(std::time::Duration::from_secs(2));
        cx.run_until_parked();
        assert!(
            cx.update(|app| ent.read(app).controls_auto.visible()),
            "悬停期间不自动隐藏"
        );
        // 移开 → 1s 后隐藏
        cx.update(|app| ent.update(app, |v, cx| v.set_controls_hover(false, cx)));
        assert!(
            cx.update(|app| ent.read(app).controls_auto.visible()),
            "刚移开仍可见（排队 1s）"
        );
        cx.executor().advance_clock(std::time::Duration::from_secs(1));
        cx.run_until_parked();
        assert!(
            !cx.update(|app| ent.read(app).controls_auto.visible()),
            "移开后 1s 应隐藏"
        );
        // 切视频 → 浮现
        cx.update(|app| ent.update(app, |v, cx| v.open_folder_index(1, cx)));
        assert!(
            cx.update(|app| ent.read(app).controls_auto.visible()),
            "切视频应浮现控制条"
        );
        // 解锁 → 普通模式常驻（可见性标志不再影响渲染；只确认切换不炸状态机）
        cx.update(|app| ent.update(app, |v, cx| v.toggle_lock(cx)));
        assert!(!cx.update(|app| ent.read(app).is_locked()));
        cx.run_until_parked();
    }

    /// 控制条渲染 + 进度条点按回归：真窗口里 draw 一帧后，**在 seek 条中点模拟点击**，
    /// 应把 `playhead` seek 到 ≈ 50% 时长（覆盖「canvas 记录盒 → 屏 x→分数 → seek」全链路，
    /// 且不触 `use_keyed_state` 渲染期限制的闪退路径）。
    #[gpui::test]
    #[allow(clippy::await_holding_lock)] // 持锁跨 await：测试锁与 FFMPEG_LOCK 独立，await 的是定时器而非锁，无死锁风险
    async fn control_bar_seek_click_seeks(cx: &mut TestAppContext) {
        let _ffmpeg_guard_test = ffmpeg_guard_lock().lock().unwrap_or_else(|e| e.into_inner());
        cx.update(|cx| {
            ui::theme::Theme::set(ui::theme::ThemeMode::Dark, cx);
        });
        let (viewer, wcx) = cx.add_window_view(|_window, cx| VideoViewer::new(fixture(), cx));
        // 定住已知时长，解耦后台解码线程时序；先画一帧让 canvas 把 seek 条盒写进 seek_bounds
        wcx.cx.update(|app| {
            viewer.update(app, |v, _| {
                v.duration = 2.0;
                v.playhead = 0.3;
            })
        });
        let _ = wcx.refresh();
        wcx.run_until_parked();
        let strip = wcx.cx.read(|app| viewer.read(app).seek_bounds.get());
        assert!(f32::from(strip.size.width) > 0.0, "seek 条应已布局出宽（>0）");
        assert!(
            f32::from(strip.size.width) >= 20.0,
            "seek 条应拉伸出可见宽度，实得 {}",
            f32::from(strip.size.width)
        );
        // 点击条中点 → 应 seek ≈50%
        wcx.simulate_click(strip.center(), Default::default());
        wcx.run_until_parked();
        let playhead = wcx.cx.read(|app| viewer.read(app).playhead);
        assert!(
            (playhead - 1.0).abs() < 0.11,
            "中点点击应 seek 到≈50%×2s=1s，实得 {playhead}"
        );
    }

    /// 视口测量接线：draw 一帧后 `#video-main`（`track_scroll` 绑定）给出**真实布局尺寸**
    ///（>0），供 contain 适配使用——回归「百分比高度在 flex 子级失效 → 竖屏放大只露
    /// 顶部」的根因。
    #[gpui::test]
    #[allow(clippy::await_holding_lock)] // 持锁跨 await：测试锁与 FFMPEG_LOCK 独立，await 的是定时器而非锁，无死锁风险
    async fn viewport_handle_reports_real_size(cx: &mut TestAppContext) {
        let _ffmpeg_guard_test = ffmpeg_guard_lock().lock().unwrap_or_else(|e| e.into_inner());
        let (viewer, wcx) =
            cx.add_window_view(|_window, cx| VideoViewer::new(fixture(), cx));
        let _ = wcx.refresh();
        wcx.run_until_parked();
        let (w, h) = wcx.cx.read(|app| {
            let v = viewer.read(app);
            let s = v.viewport.bounds().size;
            (f32::from(s.width), f32::from(s.height))
        });
        assert!(w > 100.0, "video-main 应布局出宽 {w}");
        assert!(h > 100.0, "video-main 应布局出高 {h}");
    }

    /// 回归：**同文件夹切换不再冻结/卡死**（用户 72.MP4 复现：切换后画面静止）。
    /// 根因 = 每次开媒体先「预开一次 VideoReader 取参数」再丢弃，会污染其后的 worker
    /// open（`avformat_open_input` 卡死）→ 已改为 worker 开成功后回送参数、不再预开，
    /// 且视频/音频改为**常驻单 worker 线程**（同线程串行重开，规避“新线程在上一解码器
    /// teardown 后立刻 open”的卡死）。真文件存在才跑（CI/他机跳过）。
    #[gpui::test]
    #[allow(clippy::await_holding_lock)] // 持锁跨 await：测试锁与 FFMPEG_LOCK 独立，await 的是定时器而非锁，无死锁风险
    async fn switch_reopens_next_video_cleanly(cx: &mut TestAppContext) {
        let root = std::path::PathBuf::from(r"C:\Users\20294\Videos\school");
        let a = root.join("73.MP4");
        let b = root.join("72.MP4");
        if !a.exists() || !b.exists() {
            eprintln!("[video-test] 缺真实文件，跳过切换回归");
            return;
        }
        let _ffmpeg_guard_test = ffmpeg_guard_lock().lock().unwrap_or_else(|e| e.into_inner());
        let viewer: gpui::Entity<VideoViewer> =
            cx.update(|app| app.new(|cx| VideoViewer::new(a.clone(), cx)));
        let s0 = wait_ready(&viewer, cx).await;
        assert!(s0.0 && s0.1, "首支应出帧且拿到参数 err={:?}", s0.2);
        // 切到 72
        cx.update(|app| {
            viewer.update(app, |v, cx| {
                v.nav = FolderNav::new(vec![a.clone(), b.clone()], 0);
                v.open_folder_index(1, cx);
            })
        });
        let sb = wait_ready(&viewer, cx).await;
        let final_s = cx.update(|app| {
            let v = viewer.read(app);
            (v.frame.is_some(), v.intrinsic, v.nav.current())
        });
        eprintln!(
            "[video-test] 切换后 frame={} intrinsic={:?} cur={} err={:?}",
            final_s.0, final_s.1, final_s.2, sb.2
        );
        assert!(
            final_s.0,
            "切换后应能上到新视频帧（不再冻结）err={:?}",
            sb.2
        );
        assert!(final_s.1.is_some(), "切换后应仍持新视频参数 intrinsic");
    }

    /// 回归「切换后间歇出现画面暂停但声音还在放」：`open_folder_index` 必须**同步**（在
    /// `start` 异步任务跑起来之前）切断旧媒体的音频时钟（`audio_ready`/`audio_base`/
    /// `audio_anchor_pending`）与输入通道（旧接收端里的未消费帧），否则 pump 一拍会用
    /// 旧音频时钟把 playhead 带回旧位置、或把旧帧重新上屏，而新媒体音频已独立开喂。
    #[gpui::test]
    async fn switch_synchronously_clears_stale_audio_clock_and_input(cx: &mut TestAppContext) {
        let ent = cx.update(|app| app.new(|cx| {
            let mut v = blank(fixture(), cx);
            v.playing = true;
            v.nav = FolderNav::new(vec![fixture(), fixture()], 0);
            // 旧媒体残留态：音频时钟已锚、ready 仍为 true，旧接收端里还卡着未消费帧
            v.audio_ready.store(true, Ordering::Relaxed);
            v.audio_base = 10.0;
            v.audio_anchor_pending = false;
            let (tx, rx) = sync_channel::<FrameItem>(4);
            tx.send(frame_of(9.9, 1)).unwrap();
            v.rx = Some(rx);
            v
        }));
        ent.update(cx, |v, cx| v.open_folder_index(1, cx));
        let s = cx.update(|app| {
            let v = ent.read(app);
            let old_frame_still_queued = match &v.rx {
                Some(rx) => rx.try_recv().is_ok(),
                None => false,
            };
            (
                v.audio_ready.load(Ordering::Relaxed),
                v.audio_base,
                v.audio_anchor_pending,
                old_frame_still_queued,
                (v.playhead - 0.0).abs() < 1e-6,
            )
        });
        assert!(!s.0, "切换后音频时钟应立即失效（不再用旧媒体位置推 playhead）");
        assert!((s.1 - 0.0).abs() < 1e-9, "audio_base 应归零");
        assert!(s.2, "应重新进入「音频上线锚定」待执行态");
        assert!(!s.3, "旧媒体残留帧不应再从接收端取到（pump 无法把旧帧重新上屏）");
        assert!(s.4, "playhead 不应在窗口期起跳");
        cx.run_until_parked();
    }

    // ---- M6：播放模式三态 ----

    // ---- M6：播放模式三态 ----

    /// 播放模式三态循环（顺序 → 循环 → 随机 → 顺序）与按钮标签。
    #[test]
    fn playback_mode_three_state_cycle() {
        assert_eq!(PlaybackMode::Sequential.next(), PlaybackMode::Loop);
        assert_eq!(PlaybackMode::Loop.next(), PlaybackMode::Shuffle);
        assert_eq!(PlaybackMode::Shuffle.next(), PlaybackMode::Sequential);
        assert_eq!(PlaybackMode::Sequential.label(), "顺序");
        assert_eq!(PlaybackMode::Loop.label(), "循环");
        assert_eq!(PlaybackMode::Shuffle.label(), "随机");
    }

    /// 播完去向的纯决策：顺序 = 停住（None）；循环 = 下一支（结尾回绕首支）；
    /// 随机 = 挑 ≠ current 的下标（多文件）；单文件循环/随机退化为回自身（重播）。
    #[gpui::test]
    async fn natural_end_target_decides_next_index(cx: &mut TestAppContext) {
        let read_target = |cx: &mut TestAppContext, e: &gpui::Entity<VideoViewer>| {
            cx.update(|app| e.update(app, |v, _| v.natural_end_target()))
        };
        let seq = {
            let e = cx.update(|app| app.new(|cx| {
                let mut v = blank(fixture(), cx);
                v.playback_mode = PlaybackMode::Sequential;
                v.nav = FolderNav::new(
                    vec![fixture().with_file_name("f0.mp4"), fixture().with_file_name("f1.mp4")],
                    0,
                );
                v
            }));
            read_target(cx, &e)
        };
        assert!(seq.is_none(), "顺序播放多文件也停住");
        // 循环：0→1、末位→0 回绕
        for (cur, want) in [(0usize, 1), (1, 0)] {
            let e = cx.update(|app| app.new(|cx| {
                let mut v = blank(fixture(), cx);
                v.playback_mode = PlaybackMode::Loop;
                v.nav = FolderNav::new(
                    vec![fixture().with_file_name("f0.mp4"), fixture().with_file_name("f1.mp4")],
                    cur,
                );
                v
            }));
            assert_eq!(read_target(cx, &e), Some(want), "循环 current={cur} 应切到 {want}");
        }
        // 单文件循环 / 随机：回自身（走 seek 0 重播）
        for mode in [PlaybackMode::Loop, PlaybackMode::Shuffle] {
            let e = cx.update(|app| app.new(|cx| {
                let mut v = blank(fixture(), cx);
                v.playback_mode = mode;
                v.nav = FolderNav::new(vec![fixture()], 0);
                v
            }));
            assert_eq!(read_target(cx, &e), Some(0), "{mode:?} 单文件重播自身");
        }
        // 随机多文件：结果 ≠ current 且在清单内
        let e = cx.update(|app| app.new(|cx| {
            let mut v = blank(fixture(), cx);
            v.playback_mode = PlaybackMode::Shuffle;
            v.nav = FolderNav::new(
                (0..3).map(|i| fixture().with_file_name(format!("s{i}.mp4"))).collect(),
                0,
            );
            v
        }));
        let got = read_target(cx, &e).expect("随机多文件应出下标");
        assert!(got < 3 && got != 0, "随机结果应在 [0,3) 且 ≠ current，实得 {got}");
        cx.run_until_parked();
    }

    /// 随机下标：xorshift 自推进下 64 连抽全部 ∈ [0,n) 且 ≠ current；去向多样化（不卡死
    /// 在某个下标）。同一实体连续抽 = 模拟多次自然结尾随机切。
    #[gpui::test]
    async fn pick_shuffle_index_never_repeats_current(cx: &mut TestAppContext) {
        let e = cx.update(|app| app.new(|cx| {
            let mut v = blank(fixture(), cx);
            v.playback_mode = PlaybackMode::Shuffle;
            v.rng_state = 0x1234_5678_9ABC_DEF0;
            v.nav = FolderNav::new(
                (0..5).map(|i| fixture().with_file_name(format!("d{i}.mp4"))).collect(),
                0,
            );
            v
        }));
        let mut seen = std::collections::HashSet::new();
        for _ in 0..64 {
            let i = cx.update(|app| e.update(app, |v, _| v.pick_shuffle_index()));
            assert!((1..5).contains(&i), "随机下标应在 [1,5)（≠0），实得 {i}");
            seen.insert(i);
        }
        assert!(seen.len() > 1, "随机去向应多样化，实得 {seen:?}");
        cx.run_until_parked();
    }

    /// tick 到结尾的自动去向接线：循环模式撞结尾 → 切下一支（current/路径变、继续播放）；
    /// 顺序模式撞结尾 → 停下（current 不变、playing=false）。用不存在的路径，解码在后台
    /// 落错误占位，不影响同步状态断言（open_folder_index 同步切状态后才 spawn）。
    #[gpui::test]
    async fn tick_natural_end_respects_playback_mode(cx: &mut TestAppContext) {
        // 循环：撞结尾 → 自动切下一支
        let a = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/no-a-mode.mp4");
        let b = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/no-b-mode.mp4");
        let ent = cx.update(|app| app.new(|cx| {
            let mut v = blank(a.clone(), cx);
            v.playback_mode = PlaybackMode::Loop;
            v.nav = FolderNav::new(vec![a, b], 0);
            v.duration = 2.0;
            v.playhead = 2.0; // 已到结尾，下一拍 tick 触发自然完
            v.playing = true;
            v
        }));
        ent.update(cx, |v, cx| {
            v.tick(cx);
        });
        // 同步断言：open_folder_index 在 spawn start 之前就已同步切好 current/path/playing
        //（后台 start 会对【不存在的】路径重扫 folder 并归一 current=0，故不能等 park 后再断言）
        let s = cx.update(|app| {
            let v = ent.read(app);
            (v.nav.current(), v.playing, v.path.file_name().map(|n| n.to_string_lossy().into_owned()))
        });
        assert_eq!(s.0, 1, "列表循环撞结尾应切下一支");
        assert!(s.1, "循环模式切换后继续播放");
        assert_eq!(s.2.as_deref(), Some("no-b-mode.mp4"), "路径应更新");
        cx.run_until_parked(); // 让后台 start 任务收尾（不存在路径落错误占位，无碍）
        // 顺序：撞结尾 → 停住
        let ent = cx.update(|app| app.new(|cx| {
            let mut v = blank(fixture(), cx);
            v.playback_mode = PlaybackMode::Sequential;
            v.nav = FolderNav::new(vec![fixture(), fixture()], 0);
            v.duration = 2.0;
            v.playhead = 2.0;
            v.playing = true;
            v
        }));
        ent.update(cx, |v, cx| {
            v.tick(cx);
        });
        let s = cx.update(|app| {
            let v = ent.read(app);
            (v.playing, v.nav.current(), v.playhead)
        });
        assert!(!s.0, "顺序播放撞结尾应停下");
        assert_eq!(s.1, 0, "顺序播放不切文件");
        assert_eq!(s.2, 2.0, "playhead 停在结尾");
        cx.run_until_parked();
    }

    // ---- 结尾重播 ----

    /// 停在结尾（顺序模式自然播完：playhead == duration、playing=false）点播放 →
    /// 立即 seek 0 重播：playhead 归零、playing 转 true、pump 需要继续跑。
    #[gpui::test]
    async fn toggle_play_at_end_replays_from_zero(cx: &mut TestAppContext) {
        let ent = cx.update(|app| app.new(|cx| {
            let mut v = blank(fixture(), cx);
            v.duration = 2.0;
            v.playhead = 2.0; // 停在结尾（advance_playhead 会把它钉死在 duration）
            v
        }));
        let s = ent.update(cx, |v, cx| {
            v.toggle_play(cx);
            (v.playhead, v.playing, v.pump_wanted())
        });
        assert_eq!(s.0, 0.0, "结尾重播应 seek 到 0（必须真归零，不只翻播放标志）");
        assert!(s.1, "结尾重播后应处于播放状态");
        assert!(s.2, "结尾重播后播放泵应持续运行（pump_wanted 为 true）");
        cx.run_until_parked(); // 让 seek 起的解码 worker / 泵任务收尾（夹具路径解码在后台落错误占位）
    }

    /// 普通暂停/播放切换的行为不变：中途暂停后点播放 → 从当前位置续播，playhead 不归零。
    /// 同时覆盖 `duration <= 0`（未知时长）时不触发重播分支、维持现状翻转。
    #[gpui::test]
    async fn toggle_play_midstream_resumes_without_replay(cx: &mut TestAppContext) {
        // 中途暂停续播：不归零
        let ent = cx.update(|app| app.new(|cx| {
            let mut v = blank(fixture(), cx);
            v.duration = 2.0;
            v.playhead = 1.0; // 中途
            v.playing = false;
            v
        }));
        let s = ent.update(cx, |v, cx| {
            v.toggle_play(cx);
            (v.playhead, v.playing)
        });
        assert_eq!(s.0, 1.0, "中途续播不应归零");
        assert!(s.1, "中途续播应继续播放");
        // duration <= 0（未知时长）：不触发重播分支（playhead 即使 >= duration 也不 seek）
        let ent = cx.update(|app| app.new(|cx| {
            let mut v = blank(fixture(), cx);
            v.duration = 0.0;
            v.playhead = 1.0; // 假装远超 0 也没已知结尾
            v.playing = false;
            v
        }));
        let s = ent.update(cx, |v, cx| {
            v.toggle_play(cx);
            (v.playhead, v.playing)
        });
        assert_eq!(s.0, 1.0, "duration<=0 不应触发重播 seek");
        assert!(s.1, "duration<=0 应走正常播放翻转");
        cx.run_until_parked();
    }

    /// 端到端状态链，复现「播完点播放看起来无效」的完整 bug 过程：结尾前一拍 tick 自然
    /// 播完（advance_playhead 把 playhead 钉在 duration、playing 转 false、handle_end 顺序
    /// 模式停住）→ 此时点播放键 → 应立即 seek 到 0 并保持播放，而不是又被钉回结尾。
    #[gpui::test]
    async fn natural_end_then_toggle_play_replays(cx: &mut TestAppContext) {
        let ent = cx.update(|app| app.new(|cx| {
            let mut v = blank(fixture(), cx);
            v.duration = 2.0;
            v.playhead = 1.98; // 结尾前一拍
            v.playing = true;
            v.last_tick = Some(Instant::now() - Duration::from_millis(100)); // 下一拍 dt=100ms
            v
        }));
        ent.update(cx, |v, cx| {
            v.tick(cx);
        });
        let s = cx.update(|app| {
            let v = ent.read(app);
            (v.playhead, v.playing)
        });
        assert_eq!(s.0, 2.0, "自然播完应停住且 playhead 钉在结尾");
        assert!(!s.1, "自然播完后 playing 应为 false");
        let s = ent.update(cx, |v, cx| {
            v.toggle_play(cx);
            (v.playhead, v.playing)
        });
        assert_eq!(s.0, 0.0, "停在结尾点播放应 seek 归零");
        assert!(s.1, "停在结尾点播放后应自动重播");
        cx.run_until_parked(); // 让 seek 起的解码 worker / 泵任务收尾
    }

    // ---- M6：截图当前帧存 PNG ----

    /// 截图字节序正确性：4×2 纯红（BGRA = B0 G0 R255）互换 R/B 成 RGBA 存 PNG，读回像素
    /// 应为 [255,0,0,255]。这是截图路径唯一容易出错的环节（PNG 是 RGBA、上屏是 BGRA）。
    #[test]
    fn screenshot_bgra_to_png_keeps_correct_rgb() {
        let dir = std::env::temp_dir().join(format!(
            "gpui-video-shot-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("red.png");
        // BGRA 纯红 + 一个青色点（G255 B255 R0）验证逐像素互换
        let mut buf: Vec<u8> = (0..(4 * 2 * 4)).map(|_| 0).collect();
        for px in buf.as_chunks_mut::<4>().0 {
            px.copy_from_slice(&[0, 0, 255, 255]); // BGRA 红
        }
        buf[8..12].copy_from_slice(&[255, 255, 0, 255]); // BGRA 青
        // —— 与 screenshot_png 一致的互换（BGRA → RGBA）——
        for px in buf.as_chunks_mut::<4>().0 {
            px.swap(0, 2);
        }
        image::save_buffer_with_format(
            &path,
            &buf,
            4,
            2,
            image::ExtendedColorType::Rgba8,
            image::ImageFormat::Png,
        )
        .unwrap();
        let img = image::open(&path).unwrap().to_rgba8();
        assert_eq!(img.dimensions(), (4, 2));
        assert_eq!(img.get_pixel(0, 0).0, [255, 0, 0, 255], "红点应还原为 RGBA 红");
        assert_eq!(img.get_pixel(2, 0).0, [0, 255, 255, 255], "青点 BGR→RGB 应为 (0,255,255)");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 截图时间戳与落盘路径：`YYYYMMDD-HHMMSS`（15 字符）、落在视频同目录、后缀 .png 且
    /// 带「-截图-」段。
    #[gpui::test]
    async fn screenshot_path_and_timestamp_shape(cx: &mut TestAppContext) {
        let ts = VideoViewer::utc_timestamp();
        assert_eq!(ts.len(), 15, "应为 YYYYMMDD-HHMMSS，实得 {ts}");
        assert!(ts.as_bytes().iter().all(|b| b.is_ascii_digit() || *b == b'-'));
        let dir = std::env::temp_dir().join("gpui-video-shot-name");
        std::fs::create_dir_all(&dir).unwrap();
        let ent = cx.update(|app| app.new(|cx| {
            let mut v = blank(fixture(), cx);
            v.path = dir.join("课.mp4");
            v
        }));
        let shot = cx.update(|app| ent.read(app).screenshot_path());
        assert_eq!(
            shot.parent().map(|d| d.to_path_buf()),
            Some(dir.clone()),
            "截图应落在视频同目录"
        );
        assert_eq!(shot.extension().and_then(|e| e.to_str()), Some("png"));
        let name = shot.file_name().and_then(|n| n.to_str()).unwrap().to_string();
        assert!(
            name.starts_with("课-截图-") && name.ends_with(".png"),
            "命名应为 片名-截图-时间戳.png，实得 {name}"
        );
        let _ = std::fs::remove_dir_all(&dir);
        cx.run_until_parked();
    }

    /// 无画面时截图：不拷贝、不写文件、不打后台任务，只提示状态浮层（screenshot 早退路径）。
    #[gpui::test]
    async fn screenshot_without_frame_just_flashes_status(cx: &mut TestAppContext) {
        let ent = cx.update(|app| app.new(|cx| blank(fixture(), cx))); // frame = None
        ent.update(cx, |v, cx| v.screenshot_png(cx));
        let s = cx.update(|app| {
            let v = ent.read(app);
            (v.status.is_some(), v.frame.is_none())
        });
        assert!(s.0, "无画面应提示「暂无画面可截图」");
        assert!(s.1, "frame 仍为空（未解码）");
        cx.run_until_parked();
    }

    /// 零每帧开销的按需截图提取：把合成的 RenderImage（BGRA 红/青）→ RGBA（点截图才拷贝），
    /// 尺寸取回帧真实像素。覆盖「snapshot_rgba 只读当前帧、不做每帧 clone」的正确性。
    #[test]
    fn snapshot_rgba_reads_current_frame_bgra_on_demand() {
        let (w, h) = (4u32, 2u32);
        let mut bgra = vec![0u8; (w * h * 4) as usize];
        for px in bgra.as_chunks_mut::<4>().0 {
            px.copy_from_slice(&[0, 0, 255, 255]); // BGRA 红
        }
        bgra[8..12].copy_from_slice(&[255, 255, 0, 255]); // BGRA 青
        let frame = gpui::RenderImage::new(vec![image::Frame::new(
            image::RgbaImage::from_raw(w, h, bgra).unwrap(),
        )]);
        // 提取调用前确认 size/as_bytes 可及（一旦 gpui 改 API 这里先红）
        assert_eq!(frame.size(0).width.0, 4);
        assert_eq!(frame.as_bytes(0).unwrap().len(), w as usize * h as usize * 4);
        let got = VideoViewer::snapshot_rgba(&frame).expect("有帧应能提取");
        assert_eq!((got.0, got.1), (w, h), "尺寸应取回帧真实像素");
        assert_eq!(&got.2[0..4], &[255, 0, 0, 255], "BGRA 红应转成 RGBA 红");
        assert_eq!(&got.2[8..12], &[0, 255, 255, 255], "BGRA 青应转成 RGBA (0,255,255)");
    }

    /// 等视频出帧 + intrinsic 就绪（真实时间推进让常驻 worker 干活），最多 ~30s。
    async fn wait_ready(v: &gpui::Entity<VideoViewer>, cx: &mut TestAppContext) -> (bool, bool, Option<String>) {
        for _ in 0..3000 {
            let s = cx.update(|app| {
                let v = v.read(app);
                (v.frame.is_some(), v.intrinsic.is_some(), v.load_error.clone())
            });
            if (s.0 && s.1) || s.2.is_some() {
                return s;
            }
            v.update(cx, |v, cx| v.tick(cx));
            cx.background_executor.timer(std::time::Duration::from_millis(10)).await;
        }
        cx.update(|app| {
            let v = v.read(app);
            (v.frame.is_some(), v.intrinsic.is_some(), v.load_error.clone())
        })
    }

    /// F1 文本语义：`fmt_time` = `m:ss`；≥1h 自动 `h:mm:ss`（控制条时间与 F1 悬停浮层共用）。
    /// 回归断言：旧 `{:01}:{:02}` 对 ≥1h 会错误输出 `90:05`（隐含 小时×60），新语义必须
    /// `h:mm:ss`（F1-1「悬停显示 m:ss，≥1h 自动 h:mm:ss」的纯函数基座）。
    #[test]
    fn fmt_time_m_ss_and_h_mm_ss() {
        assert_eq!(VideoViewer::fmt_time(0.0), "0:00");
        assert_eq!(VideoViewer::fmt_time(59.9), "0:59");
        assert_eq!(VideoViewer::fmt_time(60.0), "1:00");
        assert_eq!(VideoViewer::fmt_time(65.9), "1:05");
        assert_eq!(VideoViewer::fmt_time(599.0), "9:59");
        assert_eq!(VideoViewer::fmt_time(600.0), "10:00");
        assert_eq!(VideoViewer::fmt_time(3599.0), "59:59");
        assert_eq!(VideoViewer::fmt_time(3600.0), "1:00:00");
        assert_eq!(VideoViewer::fmt_time(3661.5), "1:01:01");
        assert_eq!(VideoViewer::fmt_time(7325.0), "2:02:05");
        assert_eq!(VideoViewer::fmt_time(-5.0), "0:00", "负值按 0 处理（与旧语义一致）");
    }

    /// F1：进度条悬停时间浮层——「悬停 → 元素出现 + 文本 = m:ss + 横向跟随 + 纵向固定浮在
    /// 进度条上方 + 移出隐藏」（回归断言，走真实事件分发 + rendered_frame debug_bounds）。
    /// 不开解码：用不存在的路径（加载落错误占位，控制条照常布局）；固定 duration=100 供
    /// 文本断言（render 文本唯一来源 = `fmt_time(hovered_target)`）。
    #[gpui::test]
    async fn hover_time_float_shows_follows_and_hides(cx: &mut TestAppContext) {
        cx.update(|cx| {
            ui::theme::Theme::set(ui::theme::ThemeMode::Dark, cx);
        });
        let (viewer, wcx) = cx.add_window_view(|_window, cx| {
            VideoViewer::new(
                std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("../../fixtures/no-hover-time.mp4"),
                cx,
            )
        });
        wcx.cx.update(|app| {
            viewer.update(app, |v, _| {
                v.duration = 100.0;
            })
        });
        let _ = wcx.refresh();
        wcx.run_until_parked();
        let seek = wcx.cx.read(|app| viewer.read(app).seek_bounds.get());
        assert!(seek.size.width.as_f32() > 0.0, "seek 条应已布局");
        // (1) 悬停 seek 条中点：浮层出现、文本 = 中点对应 m:ss、纵向在进度条上方
        let mid = seek.center();
        wcx.simulate_mouse_move(mid, None, Default::default());
        let _ = wcx.refresh();
        wcx.run_until_parked();
        let s = wcx.cx.read(|app| {
            let v = viewer.read(app);
            (v.hovered_target, v.hover_x, v.scrubbing)
        });
        assert!(!s.2, "未拖拽才出浮层");
        assert_eq!(s.0, Some(50.0), "中点 → 50s");
        assert_eq!(VideoViewer::fmt_time(50.0), "0:50", "浮层文本 = m:ss");
        let float = wcx.debug_bounds("HOVER_TIME").expect("悬停浮层出现");
        assert!(
            float.bottom().as_f32() <= seek.origin.y.as_f32() + 0.5,
            "浮层纵向固定浮在进度条上方（bottom ≤ seek.top），bottom={} seek.top={}",
            float.bottom().as_f32(),
            seek.origin.y.as_f32()
        );
        assert!(
            (float.center().x.as_f32() - mid.x.as_f32()).abs() <= 1.0,
            "浮层底边中点对准光标 x"
        );
        // (2) 移到另一 x（左 1/4）：文本与横坐标都变
        let q = gpui::Point::new(px(seek.origin.x.as_f32() + seek.size.width.as_f32() * 0.25), mid.y);
        wcx.simulate_mouse_move(q, None, Default::default());
        let _ = wcx.refresh();
        wcx.run_until_parked();
        let (target_q, x_q) = wcx.cx.read(|app| {
            let v = viewer.read(app);
            (v.hovered_target, v.hover_x)
        });
        assert_eq!(x_q, Some(q.x.as_f32()), "hover_x 记录新光标x");
        assert!(
            (target_q.unwrap() - 25.0).abs() < 1.0,
            "1/4 处 → ~25s 实得 {:?}",
            target_q
        );
        assert_eq!(VideoViewer::fmt_time(target_q.unwrap()), "0:25", "文本随光标变化");
        let float_q = wcx.debug_bounds("HOVER_TIME").expect("跟随光标仍显示");
        assert!(
            (float_q.center().x.as_f32() - q.x.as_f32()).abs() <= 1.0,
            "浮层横向跟随光标移动"
        );
        assert!(
            (float_q.center().x.as_f32() - float.center().x.as_f32()).abs() > 5.0,
            "x 变化 → 浮层横向真正移动"
        );
        // (3) 移到 seek 条外（控制条内其他区域，如左缘按钮带）→ 浮层隐藏
        let off = gpui::Point::new(px(seek.origin.x.as_f32() - 60.0), mid.y);
        wcx.simulate_mouse_move(off, None, Default::default());
        let _ = wcx.refresh();
        wcx.run_until_parked();
        assert!(wcx.debug_bounds("HOVER_TIME").is_none(), "移出 seek 条 → 浮层隐藏");
        // (4) 回到条最左缘（0 秒）：文本 0:00 且不越出窗口左边界（Clamp）
        let left = gpui::Point::new(seek.origin.x, mid.y);
        wcx.simulate_mouse_move(left, None, Default::default());
        let _ = wcx.refresh();
        wcx.run_until_parked();
        let t0 = wcx.cx.read(|app| viewer.read(app).hovered_target);
        assert_eq!(t0, Some(0.0), "条左缘 → 0 秒");
        assert_eq!(VideoViewer::fmt_time(0.0), "0:00");
        let fb = wcx.debug_bounds("HOVER_TIME").expect("0 秒悬停也显示浮层");
        assert!(fb.origin.x.as_f32() >= 0.0, "0 秒悬停浮层不越出窗口左边界");
        cx.run_until_parked();
    }

    /// F1-2 边界：悬停 seek 条最右缘（片尾附近）→ 浮层右缘被 Clamp 在窗口内（不被截断）。
    #[gpui::test]
    async fn hover_time_float_clamps_to_right_window_edge(cx: &mut TestAppContext) {
        cx.update(|cx| {
            ui::theme::Theme::set(ui::theme::ThemeMode::Dark, cx);
        });
        let (viewer, wcx) = cx.add_window_view(|_window, cx| {
            VideoViewer::new(
                std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("../../fixtures/no-hover-right.mp4"),
                cx,
            )
        });
        let _ = wcx.refresh();
        wcx.run_until_parked();
        let seek = wcx.cx.read(|app| viewer.read(app).seek_bounds.get());
        assert!(seek.size.width.as_f32() > 0.0, "seek 条应已布局");
        // 悬停最右缘内侧 1px（精确在右缘会命中右侧下一元素 vol 条）：无 Clamp 时浮层
        //（底边中点对齐光标）会右探出窗口 ~半卡宽。
        let right = gpui::Point::new(seek.origin.x + seek.size.width - px(1.0), seek.center().y);
        wcx.simulate_mouse_move(right, None, Default::default());
        let _ = wcx.refresh();
        wcx.run_until_parked();
        let float = wcx.debug_bounds("HOVER_TIME").expect("片尾悬停浮层出现");
        let ww = wcx
            .cx
            .windows()
            .first()
            .copied()
            .expect("测试窗把手")
            .update(&mut wcx.cx, |_, window, _| window.bounds().size.width.as_f32())
            .expect("读测试窗宽");
        assert!(
            float.right().as_f32() <= ww - 5.0,
            "右缘悬停浮层被 Clamp 在窗口内（right={} 窗口宽={}）",
            float.right().as_f32(),
            ww
        );
        // 片尾时间文本随条尾（duration > 0）
        let t = wcx.cx.read(|app| viewer.read(app).hovered_target);
        assert!(t.is_some(), "最右缘仍记悬停秒数（≈duration）");
        cx.run_until_parked();
    }

    /// F1-3：开始拖拽 → 浮层隐藏；拖拽松手仍在进度条上 → 恢复正常 hover（浮层复现）。
    #[gpui::test]
    async fn hover_time_float_hides_during_scrub_restores_after_release(cx: &mut TestAppContext) {
        cx.update(|cx| {
            ui::theme::Theme::set(ui::theme::ThemeMode::Dark, cx);
        });
        let (viewer, wcx) = cx.add_window_view(|_window, cx| {
            VideoViewer::new(
                std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("../../fixtures/no-hover-drag.mp4"),
                cx,
            )
        });
        let _ = wcx.refresh();
        wcx.run_until_parked();
        let seek = wcx.cx.read(|app| viewer.read(app).seek_bounds.get());
        let mid = seek.center();
        // 先悬停 → 浮层出现
        wcx.simulate_mouse_move(mid, None, Default::default());
        let _ = wcx.refresh();
        wcx.run_until_parked();
        assert!(wcx.debug_bounds("HOVER_TIME").is_some(), "悬停浮层出现");
        // 按下左键（拖拽开始）→ 浮层隐藏
        wcx.simulate_mouse_down(mid, MouseButton::Left, Default::default());
        let _ = wcx.refresh();
        wcx.run_until_parked();
        assert!(
            wcx.cx.read(|app| viewer.read(app).scrubbing),
            "按下进入拖拽"
        );
        assert!(wcx.debug_bounds("HOVER_TIME").is_none(), "拖拽中浮层隐藏");
        // 按住拖动 → 仍隐藏（当前时间已跟手指，无需浮层）
        let d = gpui::Point::new(px(mid.x.as_f32() + 40.0), mid.y);
        wcx.simulate_mouse_move(d, Some(MouseButton::Left), Default::default());
        let _ = wcx.refresh();
        wcx.run_until_parked();
        assert!(wcx.debug_bounds("HOVER_TIME").is_none(), "拖拽移动仍隐藏");
        // 松手仍在进度条上 → 恢复正常 hover（浮层即刻复现，不等下一次 mouse_move）
        wcx.simulate_mouse_up(d, MouseButton::Left, Default::default());
        let _ = wcx.refresh();
        wcx.run_until_parked();
        // 回归（C1）：阈值刷新——浮层文本必须=松手落点，不得残留拖拽前悬停(中点 50s)旧秒数
        let (scrub2, hover_x2, hover_tgt2) = wcx.cx.read(|app| {
            let v = viewer.read(app);
            (v.scrubbing, v.hover_x, v.hovered_target)
        });
        assert!(!scrub2, "松手退出拖拽");
        assert!(hover_x2.is_some(), "松手在条上 → 恢复 hover_x（浮层复现）");
        let f_release =
            ((d.x.as_f32() - seek.origin.x.as_f32()) / seek.size.width.as_f32()).clamp(0.0, 1.0);
        let expect_t = wcx.cx.read(|app| viewer.read(app).clamped_scrub_target(f_release));
        let tgt = hover_tgt2.expect("松手后 hovered_target 应就位");
        assert!(
            (tgt - expect_t).abs() < 1e-6,
            "浮层文本应刷新到松手落点（expect {expect_t} 实得 {tgt}），不得残留 50s"
        );
        assert_eq!(VideoViewer::fmt_time(tgt), VideoViewer::fmt_time(expect_t));
        assert!(wcx.debug_bounds("HOVER_TIME").is_some(), "松手后浮层复现并跟随新落点");
        // 第二次拖拽后往条**上方**松手（x 仍在条内、y 出条）→ 浮层保持隐藏、不残影
        wcx.simulate_mouse_down(d, MouseButton::Left, Default::default());
        let _ = wcx.refresh();
        wcx.run_until_parked();
        let above = gpui::Point::new(d.x, px(mid.y.as_f32() - 40.0));
        wcx.simulate_mouse_move(above, Some(MouseButton::Left), Default::default());
        let _ = wcx.refresh();
        wcx.run_until_parked();
        wcx.simulate_mouse_up(above, MouseButton::Left, Default::default());
        let _ = wcx.refresh();
        wcx.run_until_parked();
        let hover_x3 = wcx.cx.read(|app| viewer.read(app).hover_x);
        assert_eq!(hover_x3, None, "条外纵向松手 → 不恢复 hover_x");
        assert!(wcx.debug_bounds("HOVER_TIME").is_none(), "条外纵向松手 → 浮层不残影");
        cx.run_until_parked();
    }

    /// F2 回归（锁「悬停防抖/不卡顿」语义）：播放中悬停后**快速**点击（<250ms 即点，hover
    /// 预解码缓存来不及回填）——既有 idle 门（`!playing`）不投预解码（hover_generation 不
    /// 推进、拉起不起 worker），点击直接走正常 seek 兜底（pending_seek 就位），主线程不被
    /// 悬停机制阻塞（begin+end 同步返回）。快速点击绝不能误用「未就绪」的悬停缓存装屏。
    /// （②长悬停命中、③pending_seek 期停投已由既有 `hover_cache_hit_serves_once_and_no_reseek`
    /// 与 `hover_not_dispatched_during_pending_seek` 覆盖，此处不重复。）
    #[gpui::test]
    async fn hover_quick_click_while_playing_falls_back_to_normal_seek(cx: &mut TestAppContext) {
        let ent = cx.update(|app| app.new(|cx| {
            let mut v = blank(fixture(), cx);
            v.duration = 10.0;
            v.playing = true; // 播放中：悬停预解码机会式跳过（防抖门）
            v
        }));
        // 悬停：记录位置；播放中不投预解码（generation 不推进 / worker 不被拉起）
        ent.update(cx, |v, cx| v.update_hover_target(6.0, cx));
        let (gen_before, got_cmd, cached) = ent.update(cx, |v, _| {
            (
                v.hover_generation.load(Ordering::SeqCst),
                v.hover_cmd.is_some(),
                v.hover_cache.is_some(),
            )
        });
        assert_eq!(gen_before, 0, "播放中悬停不推进 hover_generation（不投预解码）");
        assert!(!got_cmd, "播放中悬停不拉起 hover worker（零白投）");
        assert!(!cached, "缓存未就绪（快速点击 <250ms 的前提）");
        // 快速点击：begin+end 同步完成（主线程不被悬停机制阻塞），走正常 seek
        ent.update(cx, |v, cx| v.begin_scrub(0.6, cx));
        ent.update(cx, |v, cx| v.end_scrub(0.6, cx));
        let (served, pend, scrub) = ent.update(cx, |v, _| {
            (v.served_hover_target, v.pending_seek, v.scrubbing)
        });
        assert_eq!(served, None, "缓存未就绪 → 不得由 hover 服务（走正常 seek 兜底）");
        let pend = pend.expect("快速点击应即时进入正常 seek（pending_seek 就位）");
        assert!(
            (pend - 6.0).abs() < 1e-6,
            "pending_seek 应指向点击落点 6.0s，实得 {pend}"
        );
        assert!(!scrub, "点击后退出拖拽态");
        cx.run_until_parked();
    }

    /// [INVESTIGATION-2026-10-19] 读者级 SAFE vs DUAL seek 预滚墙钟分解（临时测量，K27 留证）：
    /// 同一夹具 `target\视频测试.mp4`、同一组落点（早 10% / 中 50% / 晚 85% / 晚→早回退），
    /// 逐条测量：
    /// - SAFE：`open_scaled_hw` + 一次 `skip_to_bgra_into(target, buf)`（内核内部逐帧跳帧，
    ///   单次 FFI；落点帧一次性回读+sws 转 BGRA）。
    /// - DUAL：`open_hw`（worker 自有 D3D11 设备注入）+ `seek` + 逐帧 `next_frame_hw` 丢弃
    ///   预滚帧（免拷贝、每帧 release_hw_frame，镜像 workers.rs 356-396），落点帧 `SharedCopier::copy`
    ///   + release + recycle。
    /// 逐表报「每目标墙钟 ms + 预滚帧数 + 丢弃循环均摊 ms/帧」（后项 ≈ FFI 逐帧调用的每帧成本）。
    /// 缺夹具 / 无 D3D11 设备 / open_hw 失败 → SKIP。
    #[test]
    #[cfg(target_os = "windows")]
    fn bench_reader_seek_preroll_safe_vs_dual() {
        use std::time::Instant;
        use windows::core::Interface as _;
        let _ffmpeg_guard_test = ffmpeg_guard_lock().lock().unwrap_or_else(|e| e.into_inner());
        let _cleanup = FaultAndBlGuard;

        let base = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target");
        let fixtures = [
            base.join("视频测试.mp4"),
            base.join("视频测试字幕.mkv"),
        ];
        for path in &fixtures {
            if !path.exists() {
                eprintln!("SKIP: missing {}（夹具不入库）", path.display());
                continue;
            }
            // DUAL 腿：worker 自有设备 + 外部注入解码（生产每次 open 新建设备）
            let Some(worker) = crate::hw::create_worker_d3d11_device() else {
                eprintln!("SKIP: D3D11 硬件/WARP 设备均不可用");
                return;
            };
            let worker_kind = worker.kind;
            let raw_dev = worker.device.as_raw() as usize;
            let raw_ctx = worker.context.as_raw() as usize;
            let mut r_dual = match crate::decode::VideoReader::open_hw(path, raw_dev, raw_ctx) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("SKIP: 外部设备注入解码打开失败（无 h264 硬解?）: {e:#}");
                    return;
                }
            };
            assert!(r_dual.is_hw(), "DUAL open_hw 必须真硬解");
            let mut copier = crate::hw::SharedCopier::new(worker);

            // SAFE 腿
            let mut r_safe =
                crate::decode::VideoReader::open_scaled_hw(path, 1920).expect("SAFE 硬解打开失败");
            let safe_hw = r_safe.is_hw();
            eprintln!(
                "[SEEKRT] fixture={} params={}x{} dur={:.2}s SAFE_hw={} DUAL_device={}",
                path.file_name().unwrap_or_default().to_string_lossy(),
                r_safe.params().width,
                r_safe.params().height,
                r_safe.params().duration,
                safe_hw,
                worker_kind,
            );

            let dur = r_safe.params().duration;
            let mut buf = vec![0u8; r_safe.source_cap().max(1)];
            // 早 10% / 中 50% / 晚 85% / 晚→早回退（far-back re-seek）
            let raw_frac = [0.10f64, 0.50, 0.85];
            let mut targets: Vec<f64> = raw_frac
                .iter()
                .map(|f| (dur * f).clamp(0.05, (dur - 2.0).max(0.05)))
                .collect();
            targets.push(targets[2]); // far-back = 回到晚落点那一个（跨大段 re-seek）
            targets.push(targets[0]);

            eprintln!(
                "[SEEKRT] 目标(秒): {:?}",
                targets
                    .iter()
                    .map(|t| format!("{t:.2}"))
                    .collect::<Vec<_>>()
            );

            for (i, &target) in targets.iter().enumerate() {
                // ---- DUAL 腿 ----
                let t0 = Instant::now();
                r_dual.seek(target).expect("DUAL seek 失败");
                let mut preroll_dual: u32 = 0;
                let (dual_ms, preroll_dual_out, copy_ms) = loop {
                    let fr = match r_dual.next_frame_hw() {
                        Ok(Some(f)) => f,
                        Ok(None) => panic!("DUAL EOF 提前（target={target:.2}s）"),
                        Err(e) => panic!("DUAL 解码失败: {e:#}"),
                    };
                    if fr.pts >= target {
                        let tc = Instant::now();
                        let cf = copier
                            .copy(fr.w, fr.h, fr.tex, fr.array_index)
                            .expect("DUAL copy 失败");
                        let copy_ms = tc.elapsed().as_secs_f64() * 1e3;
                        let dual_ms = t0.elapsed().as_secs_f64() * 1e3;
                        crate::decode::VideoReader::release_hw_frame(fr.frame_ptr);
                        copier.recycle(cf.shared_handle);
                        break (dual_ms, preroll_dual, copy_ms);
                    }
                    preroll_dual += 1;
                    crate::decode::VideoReader::release_hw_frame(fr.frame_ptr);
                };
                let preroll_dual = preroll_dual_out;

                // ---- SAFE 腿 ----
                let t0 = Instant::now();
                let (safe_ms, safe_skipped) = match r_safe.skip_to_bgra_into(target, &mut buf) {
                    Ok(Some((_p, _w, _h, skipped))) => {
                        (t0.elapsed().as_secs_f64() * 1e3, skipped)
                    }
                    Ok(None) => panic!("SAFE EOF 提前（target={target:.2}s）"),
                    Err(e) => panic!("SAFE 预滚失败: {e:#}"),
                };

                eprintln!(
                    "[SEEKRT #{i}] target={target:.2}s | DUAL total={dual_ms:.1}ms (preroll={preroll_dual}帧 per-frame≈{:.3}ms, copy={copy_ms:.2}ms, seek+drop={:.1}ms) | SAFE total={safe_ms:.1}ms (skipped={safe_skipped}) | 比值 DUAL/SAFE={:.1}×",
                    if preroll_dual > 0 { dual_ms / preroll_dual as f64 } else { 0.0 },
                    (dual_ms - copy_ms),
                    dual_ms / safe_ms.max(1e-6),
                );
            }
        }
    }

    /// [INVESTIGATION-2026-10-19] 查看器级（用户可感知）DUAL 直通 seek 墙钟（临时测量，K27
    /// 留证）：与 [`bench_seek_hw_d3d11_wall`] 同构（同 mkv 夹具、同落点、同 8 字段 diag），
    /// 唯一差异 = `DualPathGuard` 开启双设备直通 + auto 模式（生产默认路径）→ 断言
    /// `dual_engaged()`（Arc 上双设备真正接管）。与 SAFE 基准数字直接对表。
    #[gpui::test]
    #[allow(clippy::await_holding_lock)]
    #[cfg(target_os = "windows")]
    async fn bench_seek_dual_passthrough_land_wall(cx: &mut TestAppContext) {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/视频测试字幕.mkv");
        if !path.exists() {
            eprintln!("SKIP: missing target/视频测试字幕.mkv（长 GOP 夹具不入库）");
            return;
        }
        let _ffmpeg_guard_test = ffmpeg_guard_lock().lock().unwrap_or_else(|e| e.into_inner());
        let _dual = DualPathGuard::new(); // 生产默认：双设备直通优先
        let _decode_mode = crate::decode::DecodeModeGuard::new(0); // auto（生产默认）
        let _fault_bl = FaultAndBlGuard;
        dual_engaged_reset();
        hw_engaged_reset();
        let viewer: gpui::Entity<VideoViewer> =
            cx.update(|app| app.new(|cx| VideoViewer::new(path.clone(), cx)));
        let mut wait = 0;
        loop {
            let ready = cx.update(|app| {
                let v = viewer.read(app);
                v.has_frame() && v.duration > 1.0 && v.pending_seek.is_none()
            });
            if ready {
                break;
            }
            viewer.update(cx, |v, cx| {
                let _ = v.tick(cx);
            });
            cx.background_executor.timer(Duration::from_millis(5)).await;
            wait += 1;
            assert!(wait < 6000, "等待 DUAL 首帧上屏超时");
        }
        if !dual_engaged() {
            eprintln!(
                "SKIP: 本机双设备未接管（dual_engaged=false hw_engaged={}）——非 Arc 目标机器",
                hw_engaged()
            );
            cx.run_until_parked();
            return;
        }
        eprintln!(
            "[SEEKBENCHDUAL] 模式=auto 双设备=1 hw=1 gpu_frame={}",
            cx.update(|app| {
                let v = viewer.read(app);
                v.gpu_frame.is_some()
            })
        );
        let duration = cx.update(|app| viewer.read(app).duration);
        let raw = [191.0f64, 354.0, 447.0, 1335.0, 354.0];
        let hi = (duration - 2.0).max(0.05);
        let mut rows: Vec<f64> = Vec::with_capacity(raw.len());
        for &sec in &raw {
            let sec = sec.clamp(0.05, hi);
            let fraction = (sec / duration).clamp(0.0, 1.0) as f32;
            let t0 = Instant::now();
            viewer.update(cx, |v, cx| v.begin_scrub(fraction, cx));
            viewer.update(cx, |v, cx| v.end_scrub(fraction, cx));
            let target = cx.update(|app| viewer.read(app).playhead);
            let deadline = Instant::now() + Duration::from_secs(30);
            let landed = loop {
                viewer.update(cx, |v, cx| {
                    let _ = v.tick(cx);
                });
                let s = cx.update(|app| {
                    let v = viewer.read(app);
                    (v.pending_seek, v.last_frame_pts)
                });
                if s.0.is_none() {
                    break s.1;
                }
                assert!(
                    Instant::now() < deadline,
                    "DUAL seek 落点帧上屏超时（target={target:.2}s）"
                );
                std::thread::sleep(Duration::from_millis(1));
            };
            let firstpts = landed.expect("DUAL seek 首帧应上屏");
            let ms = t0.elapsed().as_secs_f64() * 1e3;
            let (total, reuse, regress, skipped, s2f, f2s, tgt, fp) = match take_seek_diag_test()
            {
                Some(x) => x,
                None => (ms, 1u8, f64::NAN, 0u32, ms, 0.0, target, firstpts),
            };
            rows.push(total);
            eprintln!(
                "[SEEKBENCHDUAL] total={total:.1}ms reuse={reuse} regress={regress:+.2}s skipped={skipped} seek2first={s2f:.1}ms frame2screen={f2s:.1}ms target={tgt:.2}s firstpts={fp:.2}s dual=1"
            );
        }
        let mut sorted = rows.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        eprintln!(
            "[SEEKBENCHDUAL] 摘要 n={} min={:.1}ms median={:.1}ms max={:.1}ms（total 口径，dual=1）",
            sorted.len(),
            sorted[0],
            sorted[sorted.len() / 2],
            sorted[sorted.len() - 1],
        );
        cx.run_until_parked();
    }
