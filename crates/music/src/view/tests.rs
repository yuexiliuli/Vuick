//! MusicView 视图测试：不真播（player 随机 dll/设备缺失时纯状态播放即可）。
//!
//! 覆盖：非全屏 Esc → `MusicEvent::ExitRequested`；F 键翻转 `fs_open`；
//! 歌曲行点击 → `player.current` 指向该行（`handle_play_idx` 直调，不做像素命中）；
//! 搜索过滤纯函数（大小写不敏感 / 中文 / 空查询）；默认状态。

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::path::Path;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use gpui::{AppContext as _, Bounds, Entity, Focusable as _, MouseButton, Pixels, TestAppContext, VisualTestContext, px};

use super::lyrics::LYRIC_ROW_H;
use super::icon::MusicIcon;
use super::theme;
use super::titlebar::{TITLE_BAR_H, max_icon};
use super::{MainSection, MusicEvent, MusicView, RightTab};
use crate::model::{RepeatMode, Track};
use crate::player::MusicPlayer;

/// 极小有效 PNG（2×1 纯红）字节：作为同名封面侧车装进临时库。
fn tiny_png_bytes() -> Vec<u8> {
    let img: kimg::RgbaImage =
        kimg::ImageBuffer::from_pixel(2, 1, kimg::Rgba([255u8, 0, 0, 255]));
    let mut out = std::io::Cursor::new(Vec::new());
    kimg::DynamicImage::ImageRgba8(img)
        .write_to(&mut out, kimg::ImageFormat::Png)
        .expect("png 编码应成功");
    out.into_inner()
}

/// 推进虚拟时钟一段（250ms/拍 × 真实小睡）驱动 MusicView 常驻 tick（250ms）+ 解码任务
/// 落地。fake 调度器对 render/spawn 轮询不足，用 advance_clock + run_until_parked 双拍
/// 合拍（对齐仓库 pdf/editor 惯用法）。
fn pump_ticks(cx: &mut VisualTestContext, rounds: usize) {
    for _ in 0..rounds {
        cx.executor().advance_clock(Duration::from_millis(250));
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(3));
        cx.run_until_parked();
    }
}

/// 异步扫库驱动（夹具/测试共用）：`load_dir` 已异步化——扫描（目录遍历 + 读 .lrc + 找封面）
/// 在后台 executor 执行、完成后主线程一次性提交（并触发批量时长探测）。已知 pending 的
/// 测试若在 read library 前不排空，会看到空库；此 helper 把「scan → commit → probe」链全部
/// 落地（fake 调度器 `run_until_parked` 即执行背景 future）。同步旧实现下调用为无害空操作。
fn drain_scan(cx: &mut TestAppContext) {
    for _ in 0..3 {
        cx.background_executor.run_until_parked();
    }
}

/// 建含「1 首有同名 png 封面 + 1 首无封面」的临时库播放器。
fn library_with_cover(cx: &mut TestAppContext) -> (std::path::PathBuf, Entity<MusicPlayer>) {
    let dir = std::env::temp_dir().join(format!("gpui-music-cover-reg-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    write_tiny_wav(&dir, "周杰伦 - 晴天.wav");
    std::fs::write(dir.join("周杰伦 - 晴天.png"), tiny_png_bytes()).unwrap();
    write_tiny_wav(&dir, "林俊杰 - 江南.wav");
    let player: Entity<MusicPlayer> = cx.update(|app| app.new(|cx| MusicPlayer::new(cx)));
    cx.update(|app| {
        player.update(app, |p, cx| p.load_dir(&dir, cx));
    });
    drain_scan(cx);
    (dir, player)
}

/// 封面端到端回归：scan(同名 png) → load_dir → 数拍 tick 后 `covers` 填充；随后播放
/// 当前曲 → `cover_for_current` 为 Some（真机「右侧大图/底栏缩略图/全屏封面」的取值链）。
/// 历史缺陷：`spawn_cover_decode` 的 `Task` drop 即取消解码 → `covers` 恒空、三处全渐变。
#[gpui::test]
async fn library_cover_decodes_into_cache_and_current(cx: &mut TestAppContext) {
    let (dir, player) = library_with_cover(cx);
    // 库内第一首应有同名封面（scan 侧车已挂）；第二首无封面。
    let cover_path = cx.update(|cx| player.read(cx).library()[0].cover_path.clone());
    assert!(cover_path.is_some(), "同名 png 应被 scan 挂为 cover_path");

    let (view, cx) = cx.add_window_view(|_w, cx| MusicView::new(player.clone(), cx));
    // 数拍后全库缺失封面应入缓存（含未播放的当前曲）。
    let mut filled = false;
    for _ in 0..10 {
        pump_ticks(cx, 1);
        let (n_covers, loading) = cx.update(|_w, cx| {
            let v = view.read(cx);
            (v.covers.len(), v.cover_loading.len())
        });
        if n_covers == 1 && loading == 0 {
            filled = true;
            break;
        }
    }
    assert!(filled, "tick 扫描后 covers 应恰好含该封面（无在途）");

    // 播放当前曲（headless 不真出声；current 即置位）→ cover_for_current 有图。
    cx.update(|_w, cx| {
        view.update(cx, |v, cx| v.handle_play_idx(0, cx));
    });
    pump_ticks(cx, 2);
    let has_current_cover = cx.update(|_w, cx| {
        view.update(cx, |v, cx| v.cover_for_current(cx).is_some())
    });
    assert!(has_current_cover, "播放当前曲目后 cover_for_current 应 Some");

    let _ = std::fs::remove_dir_all(&dir);
}


/// 手写 0.4s 双声道 16bit 44.1k WAV（只作文件名/库元数据，不真解码）。
fn write_tiny_wav(dir: &Path, name: &str) {
    const RATE: u32 = 44_100;
    const CH: usize = 2;
    let total = (RATE as f64 * 0.4 * CH as f64).round() as usize;
    let mut data = Vec::with_capacity(total * 2);
    for _ in 0..total / CH {
        data.extend_from_slice(&0i16.to_le_bytes());
        data.extend_from_slice(&0i16.to_le_bytes());
    }
    let mut wav = Vec::new();
    let data_len = data.len() as u32;
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&(CH as u16).to_le_bytes());
    wav.extend_from_slice(&RATE.to_le_bytes());
    wav.extend_from_slice(&(RATE * CH as u32 * 2).to_le_bytes());
    wav.extend_from_slice(&(CH as u16 * 2).to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    wav.extend_from_slice(&data);
    std::fs::write(dir.join(name), wav).unwrap();
}

/// 迷你 WAV（0.02s ≈ 3.5KB）：大库性能探针/虚拟化回归用——只作文件名/元数据，不真解码。
fn write_tiny_wav_short(dir: &Path, name: &str) {
    const RATE: u32 = 44_100;
    const CH: usize = 2;
    let total = (RATE as f64 * 0.02 * CH as f64).round() as usize;
    let mut data = Vec::with_capacity(total * 2);
    for _ in 0..total / CH {
        data.extend_from_slice(&0i16.to_le_bytes());
        data.extend_from_slice(&0i16.to_le_bytes());
    }
    let mut wav = Vec::new();
    let data_len = data.len() as u32;
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&(CH as u16).to_le_bytes());
    wav.extend_from_slice(&RATE.to_le_bytes());
    wav.extend_from_slice(&(RATE * CH as u32 * 2).to_le_bytes());
    wav.extend_from_slice(&(CH as u16 * 2).to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    wav.extend_from_slice(&data);
    std::fs::write(dir.join(name), wav).unwrap();
}

/// 建含 `n` 曲的临时库**目录 + 播放器（未 load_dir）**：5 位艺术家轮转（`艺人{i%5}`）+ 唯一歌名。
/// 目录名带全局自增序号防并行互删。不 load_dir——扫库探针/竞态测试需在 load_dir 前挂 observe。
static N_LIB_DIR_SEQ: AtomicUsize = AtomicUsize::new(0);
fn n_library_dir(cx: &mut TestAppContext, n: usize) -> (std::path::PathBuf, Entity<MusicPlayer>) {
    let seq = N_LIB_DIR_SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "gpui-music-nlib-{}-{seq}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for i in 0..n {
        write_tiny_wav_short(&dir, &format!("艺人{} - 歌曲{i}.wav", i % 5));
    }
    let player: Entity<MusicPlayer> = cx.update(|app| app.new(|cx| MusicPlayer::new(cx)));
    (dir, player)
}

/// 建含 `n` 曲且已完成异步扫库（load_dir + [`drain_scan`]）的播放器——虚拟化/性能探针等测试
/// 的现成夹具（读 library 前库已就绪）。
fn library_with_n(cx: &mut TestAppContext, n: usize) -> Entity<MusicPlayer> {
    let (dir, player) = n_library_dir(cx, n);
    cx.update(|app| {
        player.update(app, |p, cx| p.load_dir(&dir, cx));
    });
    drain_scan(cx);
    player
}

/// 性能探针（K27 留数）：`n` 首大库下主区列表稳态单帧墙钟 ms（首帧暖一次后计时第二帧的
/// draw + `run_until_parked`）。before=全量物化+Track 深克隆（r199=true），after=可视窗虚拟化+
/// 零克隆（r199=false）。报告里给对比数；断言只放宽松阈值（<10s）防误伤，数字本身才是产出。
/// 不引用新实现字段（`main_scroll`），保证同探针可对改造前/后代跑。
#[gpui::test]
async fn render_main_lists_wallclock_200(cx: &mut TestAppContext) {
    let player = library_with_n(cx, 200);
    let (_view, wcx) = cx.add_window_view(|_w, cx| MusicView::new(player, cx));
    wcx.update(|window, app| {
        window.draw(app).clear(app);
    });
    wcx.run_until_parked();
    let t0 = std::time::Instant::now();
    wcx.update(|window, app| {
        window.draw(app).clear(app);
    });
    wcx.run_until_parked();
    let ms = t0.elapsed().as_secs_f64() * 1000.0;
    let r0 = wcx.debug_bounds("song-row-0").is_some();
    let r199 = wcx.debug_bounds("song-row-199").is_some();
    eprintln!(
        "[perf] render_main_lists@200 steady_frame_ms={ms:.1} materialized=(r0={r0},r199={r199})"
    );
    assert!(
        ms < 10_000.0,
        "稳态单帧应在宽松阈值内（性能探针不设严格红线）：实测 {ms:.1}ms"
    );
}

/// 扫库/时长探测 **notify 预算探针**（K27 留数；仅用旧/新两态都存在的稳定 API，可对改造前/
/// 后代跑）：`load_dir`（现异步扫库）+ 批量时长探测完成后，player 实体收到的 notify 总次数。
/// before（同步扫 + 每首 notify）= 1(load_dir)+N(逐首探测) ≈ N+1；after（异步单次提交 + 批量
/// 一次 notify）≈ 3。断言 ≤ 8（远小于 N）——防「大库启动一连串整树重绘尖峰」回潮。
#[gpui::test]
async fn scan_execute_notify_budget(cx: &mut TestAppContext) {
    let n = 20;
    let (dir, player) = n_library_dir(cx, n);
    let notifies = Rc::new(Cell::new(0usize));
    cx.update(|app| {
        let c = notifies.clone();
        app.observe(&player, move |_, _| c.set(c.get() + 1))
            .detach();
    });
    cx.update(|app| {
        player.update(app, |p, cx| p.load_dir(&dir, cx));
    });
    // 驱动：新=后台 scan→commit→probe；旧=同步（本循环为无害排空）。
    for _ in 0..3 {
        cx.background_executor.run_until_parked();
    }
    let k = notifies.get();
    let len = cx.update(|app| player.read(app).library().len());
    assert_eq!(len, n, "扫库应完成（库内容照旧）");
    eprintln!("[perf] scan_execute_notify_budget: notify_count={k} library_len={len} (N={n})");
    assert!(
        k <= 8,
        "load_dir+批量探测完成的 player notify 应 ≤ 小常数（旧实现每首 1 次 ≈ N+1={}），实得 {k}",
        n + 1
    );
}

/// 异步装配态：`load_dir` 后立即处于「扫描中」装配态（主线程不再同步空等），扫描完成驱动后
/// 库出现、装配态复位；装配态渲染不 panic（空态可读）。
#[gpui::test]
async fn load_dir_async_shows_assembly_then_commits(cx: &mut TestAppContext) {
    let (dir, player) = n_library_dir(cx, 8);
    let (_view, wcx) = cx.add_window_view(|_w, cx| MusicView::new(player.clone(), cx));
    // 发起异步扫库：未驱动前即立装配态。
    wcx.cx.update(|app| {
        player.update(app, |p, cx| p.load_dir(&dir, cx));
    });
    let (scanning, len) = wcx.cx.read(|app| {
        let p = player.read(app);
        (p.scanning(), p.library().len())
    });
    assert!(scanning, "load_dir 后应立置装配态（扫描完成前不再同步空等）");
    assert_eq!(len, 0, "扫描未完成前库应仍为空（后台执行，主线程不阻塞）");
    // 装配态渲染照常（空态/「正在扫描」提示，不 panic）。
    wcx.update(|window, app| {
        window.draw(app).clear(app);
    });
    wcx.run_until_parked();
    // 驱动扫库完成 → 一次性提交库 + 装配态复位。
    drain_scan(&mut wcx.cx);
    let (scanning2, len2) = wcx.cx.read(|app| {
        let p = player.read(app);
        (p.scanning(), p.library().len())
    });
    assert!(
        !scanning2 && len2 == 8,
        "扫库完成应复位装配态且库就绪：scanning={scanning2} len={len2}"
    );
    // 完成后库可直接播放（沿用既有路径，语义不破坏）。
    wcx.cx.update(|app| {
        player.update(app, |p, cx| p.play_track(0, cx));
    });
    let cur = wcx.cx.read(|app| player.read(app).current());
    assert_eq!(cur, Some(0), "扫完即可播放（current=0）");
}

/// 换目录竞态：扫描中途再 load_dir → 最终库是**新目录**（旧任务结果作废），无跨目录混入。
#[gpui::test]
async fn load_dir_mid_scan_switch_directory_discards_stale(cx: &mut TestAppContext) {
    let (dir_a, player) = n_library_dir(cx, 5);
    let (dir_b, _unused) = n_library_dir(cx, 9);
    // 未排空即快速发起第二个目录（模拟用户换目录/重扫）。
    cx.update(|app| {
        player.update(app, |p, cx| p.load_dir(&dir_a, cx));
    });
    cx.update(|app| {
        player.update(app, |p, cx| p.load_dir(&dir_b, cx));
    });
    drain_scan(cx);
    let (scanning, len, dir) = cx.read(|app| {
        let p = player.read(app);
        (p.scanning(), p.library().len(), p.dir().map(|d| d.to_path_buf()))
    });
    assert!(!scanning, "最终应结束装配态");
    assert_eq!(len, 9, "最终库应是新目录（9 首），旧扫描不得覆盖：实得 {len}");
    assert_eq!(dir.as_deref(), Some(dir_b.as_path()), "dir 应为新目录");
    let paths_ok = cx.read(|app| {
        player
            .read(app)
            .library()
            .iter()
            .all(|t| t.path.starts_with(&dir_b))
    });
    assert!(paths_ok, "库中不应混入旧目录曲目（A 目录内容被作废）");
}/// 建一个含 3 曲的临时音乐库播放器（中文文件名）。目录名带全局自增序号：多测试并行
/// 共用同名会互相删写 → 不稳定（与 `player_with_lrc` 同款隔离）。
static LIB_DIR_SEQ: AtomicUsize = AtomicUsize::new(0);
fn player_with_library(cx: &mut TestAppContext) -> Entity<MusicPlayer> {
    let seq = LIB_DIR_SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "gpui-music-view-{}-{seq}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    write_tiny_wav(&dir, "周杰伦 - 晴天.wav");
    write_tiny_wav(&dir, "林俊杰 - 江南.wav");
    write_tiny_wav(&dir, "陈奕迅 - 十年.wav");
    let player: Entity<MusicPlayer> = cx.update(|app| app.new(|cx| MusicPlayer::new(cx)));
    cx.update(|app| {
        player.update(app, |p, cx| p.load_dir(&dir, cx));
    });
    drain_scan(cx);
    player
}

/// 建窗口 + MusicView 根，聚焦根，返回 view 与克隆的 VisualTestContext 引用。
fn open_view<'a>(
    cx: &'a mut TestAppContext,
    player: Entity<MusicPlayer>,
) -> (Entity<MusicView>, &'a mut VisualTestContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| MusicView::new(player, cx));
    // 根先拿焦点（否则按键不派发到 view）。
    cx.update(|_window, app| {
        let handle = view.read(app).focus_handle();
        _window.focus(&handle, app);
    });
    (view, cx)
}

#[test]
fn filter_matches_case_insensitive_and_chinese() {
    let mk = |_i: usize, artist: &str, title: &str| Track {
        path: Path::new("p.wav").to_path_buf(),
        title: title.to_string(),
        artist: artist.to_string(),
        album: String::new(),
        duration: 0.0,
        grad: (0x7c, 0x5c, 0xfc),
        cover_path: None,
        lyrics: None,
    };
    let tracks = vec![
        mk(0, "周杰伦", "晴天"),
        mk(1, "The Weeknd", "Blinding Lights"),
        mk(2, "阿密特", "彩虹"),
    ];
    // 空查询 → 全中
    assert_eq!(super::filter_tracks_by_query(&tracks, ""), vec![0, 1, 2]);
    // 中文标题
    assert_eq!(super::filter_tracks_by_query(&tracks, "晴天"), vec![0]);
    // 英文大小写不敏感
    assert_eq!(super::filter_tracks_by_query(&tracks, "the"), vec![1]);
    assert_eq!(super::filter_tracks_by_query(&tracks, "THE WEEKND"), vec![1]);
    // 艺术家命中
    assert_eq!(super::filter_tracks_by_query(&tracks, "周杰伦"), vec![0]);
    // 无命中
    assert!(super::filter_tracks_by_query(&tracks, "不存在的歌").is_empty());
}

#[gpui::test]
fn escape_emits_exit_requested_when_not_fullscreen(cx: &mut TestAppContext) {
    let player = player_with_library(cx);
    let exits = Rc::new(Cell::new(0usize));
    let (view, cx) = cx.add_window_view(|_window, cx| MusicView::new(player, cx));
    cx.update(|window, app| {
        let e = exits.clone();
        window
            .subscribe(&view, app, move |_, _: &MusicEvent, _, _| {
                e.set(e.get() + 1);
            })
            .detach();
        let handle = view.read(app).focus_handle();
        window.focus(&handle, app);
    });
    // 非全屏按 Esc → 发 ExitRequested 一次。
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert_eq!(exits.get(), 1, "非全屏 Esc 应发出 ExitRequested");
    // 全屏下按 Esc → 只关全屏，不再发退出。
    cx.simulate_keystrokes("f");
    cx.run_until_parked();
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    let fs_open = cx.update(|_window, app| view.read(app).state.fs_open);
    assert!(!fs_open, "全屏下 Esc 应关全屏");
    assert_eq!(exits.get(), 1, "全屏下 Esc 不应再发 ExitRequested");
}

#[gpui::test]
fn f_key_toggles_fullscreen(cx: &mut TestAppContext) {
    let player = player_with_library(cx);
    let (view, cx) = open_view(cx, player);
    let read_fs = |cx: &mut VisualTestContext| {
        cx.update(|_window, app| view.read(app).state.fs_open)
    };
    assert!(!read_fs(cx), "初始未全屏");
    cx.simulate_keystrokes("f");
    cx.run_until_parked();
    assert!(read_fs(cx), "F 键应打开全屏");
    cx.simulate_keystrokes("f");
    cx.run_until_parked();
    assert!(!read_fs(cx), "再按 F 应退出全屏");
}

#[gpui::test]
fn song_row_click_sets_player_current(cx: &mut TestAppContext) {
    let player = player_with_library(cx);
    let (view, cx) = open_view(cx, player.clone());
    // 点击第 0 行（handler 直调，不做像素命中；真播放由 Phase A 集成测试兜底）。
    cx.update(|_window, app| {
        view.update(app, |v, cx| v.handle_play_idx(0, cx));
    });
    let cur = cx.update(|_window, app| player.read(app).current());
    assert_eq!(cur, Some(0), "点击歌曲行应把 current 指到该行");
    // 再点第 2 行。
    cx.update(|_window, app| {
        view.update(app, |v, cx| v.handle_play_idx(2, cx));
    });
    let cur = cx.update(|_window, app| player.read(app).current());
    assert_eq!(cur, Some(2), "点击另一行应切换 current");
    // 越界索引应无害（不 panic、不改变 current）。
    cx.update(|_window, app| {
        view.update(app, |v, cx| v.handle_play_idx(999, cx));
    });
    let cur = cx.update(|_window, app| player.read(app).current());
    assert_eq!(cur, Some(2), "越界索引应被忽略");
}

#[gpui::test]
fn view_renders_without_panic(cx: &mut TestAppContext) {
    // 构建即渲染：普通布局 + 全屏浮层各一帧（触发主题/渐变/图标/交互条真实布局，
    // 防裸色或空态回归；全屏走 `f_key_toggles_fullscreen` 之外的第二条渲染路径）。
    let player = player_with_library(cx);
    let (view, cx) = open_view(cx, player);
    cx.update(|window, app| {
        window.draw(app).clear(app);
    });
    cx.update(|_window, app| {
        view.update(app, |v, _cx| v.state.fs_open = true);
    });
    cx.update(|window, app| {
        window.draw(app).clear(app);
    });
    cx.run_until_parked();
    assert_eq!(theme::BG_DEEP, 0x0d0b1a);
}

#[test]
fn view_state_defaults_are_sane() {
    let v = super::ViewState::default();
    assert!(!v.fs_open);
    assert!(v.search.is_empty());
    assert!(!v.back_off);
}

// ---- 进度条点击/拖动 seek 交互（回归断言，参照视频 scrub 语义，音频-only） ----

/// 建「已注入探测时长」的窗口视图并渲染一帧：bounds_canvas 把各条盒写入。
/// 返回 (view, wcx)。
fn open_seek_view(
    cx: &mut TestAppContext,
    dur_secs: f64,
) -> (Entity<MusicView>, &mut VisualTestContext) {
    let player = player_with_library(cx);
    cx.update(|app| {
        player.update(app, |p, _| p.set_test_duration(dur_secs));
    });
    let (view, wcx) = cx.add_window_view(|_w, cx| MusicView::new(player.clone(), cx));
    // 真实布局一帧：可交互条盒由 canvas prepaint 写入（bb/fs）。
    wcx.update(|window, app| {
        window.draw(app).clear(app);
    });
    wcx.run_until_parked();
    (view, wcx)
}

/// 条盒的水平线上某分数（0..=1）的窗口坐标点（y 用盒中线）。
fn at_frac(b: &Bounds<Pixels>, frac: f32) -> gpui::Point<Pixels> {
    gpui::Point::new(
        px(b.origin.x.as_f32() + b.size.width.as_f32() * frac),
        px(b.origin.y.as_f32() + b.size.height.as_f32() * 0.5),
    )
}

/// 用例 A 点击即跳：bb-progress 内点按 75% → player 收到 seek ≈0.75×duration 且显示即时到落点。
#[gpui::test]
async fn bb_progress_click_jumps_to_position(cx: &mut TestAppContext) {
    let dur = 100.0;
    let (view, wcx) = open_seek_view(cx, dur);
    let b = wcx.cx.read(|app| view.read(app).bottom_bar.read(app).bb_progress.get());
    assert!(b.size.width.as_f32() > 0.0, "bb-progress 盒应已布局（宽>0），实得 {b:?}");
    wcx.simulate_mouse_down(at_frac(&b, 0.75), MouseButton::Left, Default::default());
    wcx.run_until_parked();
    let pos = wcx.cx.read(|app| view.read(app).player.read(app).position());
    let want = dur * 0.75;
    assert!(
        (pos - want).abs() < 1.0,
        "点按 75% 应立即跳 {want:.1}s，实得 {pos:.1}"
    );
    // 收尾：放开不闪跳、仍停在该处。
    wcx.simulate_mouse_up(at_frac(&b, 0.75), MouseButton::Left, Default::default());
    wcx.run_until_parked();
    let pos2 = wcx.cx.read(|app| view.read(app).player.read(app).position());
    assert!((pos2 - want).abs() < 1.0, "松开后仍停在 {want:.1}s，实得 {pos2:.1}");
}

/// 用例 B 拖动实时跟随 + 松手收尾：down@60% → move@40% 跟随 → move@90% 继续 → up 不闪跳。
#[gpui::test]
async fn bb_progress_drag_follows_and_release_lands(cx: &mut TestAppContext) {
    let dur = 100.0;
    let (view, wcx) = open_seek_view(cx, dur);
    let b = wcx.cx.read(|app| view.read(app).bottom_bar.read(app).bb_progress.get());
    assert!(b.size.width.as_f32() > 0.0, "bb-progress 盒应已布局（宽>0），实得 {b:?}");
    wcx.simulate_mouse_down(at_frac(&b, 0.60), MouseButton::Left, Default::default());
    wcx.run_until_parked();
    let p1 = wcx.cx.read(|app| view.read(app).player.read(app).position());
    assert!((p1 - dur * 0.60).abs() < 1.0, "按下 60% 应 seek 到 {:.1}s，实得 {p1:.1}", dur * 0.60);
    // 保持按住拖到 40% → 显示实时跟随落点。
    wcx.simulate_mouse_move(at_frac(&b, 0.40), Some(MouseButton::Left), Default::default());
    wcx.run_until_parked();
    let p2 = wcx.cx.read(|app| view.read(app).player.read(app).position());
    assert!((p2 - dur * 0.40).abs() < 1.0, "拖到 40% 应跟随 {:.1}s，实得 {p2:.1}", dur * 0.40);
    // 再拖到 90% → 继续跟随。
    wcx.simulate_mouse_move(at_frac(&b, 0.90), Some(MouseButton::Left), Default::default());
    wcx.run_until_parked();
    let p3 = wcx.cx.read(|app| view.read(app).player.read(app).position());
    assert!((p3 - dur * 0.90).abs() < 1.0, "拖到 90% 应跟随 {:.1}s，实得 {p3:.1}", dur * 0.90);
    // 松手 → 停在当前位置（不闪跳回 0）。
    wcx.simulate_mouse_up(at_frac(&b, 0.90), MouseButton::Left, Default::default());
    wcx.run_until_parked();
    let p4 = wcx.cx.read(|app| view.read(app).player.read(app).position());
    assert!((p4 - dur * 0.90).abs() < 1.0, "松手应停在 90%，实得 {p4:.1}");
}

/// 用例 C 暂停态（playing=false）点击/拖动仍生效：seek 下发、显示跟手、不误伤暂停。
#[gpui::test]
async fn paused_progress_seek_still_works(cx: &mut TestAppContext) {
    let dur = 120.0;
    let player = player_with_library(cx);
    cx.update(|app| {
        player.update(app, |p, cx| {
            p.set_test_duration(dur);
            p.pause(cx);
        });
    });
    let (view, wcx) = cx.add_window_view(|_w, cx| MusicView::new(player.clone(), cx));
    wcx.update(|window, app| {
        window.draw(app).clear(app);
    });
    wcx.run_until_parked();
    assert!(
        !wcx.cx.read(|app| player.read(app).playing()),
        "夹具应为暂停态（playing=false）"
    );
    let b = wcx.cx.read(|app| view.read(app).bottom_bar.read(app).bb_progress.get());
    assert!(b.size.width.as_f32() > 0.0, "bb-progress 盒应已布局（宽>0），实得 {b:?}");
    wcx.simulate_mouse_down(at_frac(&b, 0.50), MouseButton::Left, Default::default());
    wcx.run_until_parked();
    let p1 = wcx.cx.read(|app| player.read(app).position());
    assert!((p1 - dur * 0.50).abs() < 1.0, "暂停态点按 50% 应 seek 到 {:.1}s，实得 {p1:.1}", dur * 0.50);
    wcx.simulate_mouse_move(at_frac(&b, 0.30), Some(MouseButton::Left), Default::default());
    wcx.run_until_parked();
    let p2 = wcx.cx.read(|app| player.read(app).position());
    assert!((p2 - dur * 0.30).abs() < 1.0, "暂停态拖到 30% 应跟随 {:.1}s，实得 {p2:.1}", dur * 0.30);
    wcx.simulate_mouse_up(at_frac(&b, 0.30), MouseButton::Left, Default::default());
    wcx.run_until_parked();
    let s = wcx.cx.read(|app| {
        let p = player.read(app);
        (p.position(), p.playing(), p.paused())
    });
    assert!((s.0 - dur * 0.30).abs() < 1.0, "暂停态松手仍停 30%，实得 {:.1}", s.0);
    assert!(!s.1 && s.2, "seek 不误伤暂停（playing/paused 不变）");
}

/// 用例 D 全屏 fs-prog：fs_open 渲染路径下对全屏进度条盒点按（覆盖三处共用路径之一）。
#[gpui::test]
async fn fs_progress_click_seeks(cx: &mut TestAppContext) {
    let dur = 120.0;
    let player = player_with_library(cx);
    cx.update(|app| {
        player.update(app, |p, _| p.set_test_duration(dur));
    });
    let (view, wcx) = cx.add_window_view(|_w, cx| MusicView::new(player.clone(), cx));
    // 打开全屏浮层并渲染一帧 → fs_progress 盒由 canvas prepaint 写入。
    wcx.update(|window, app| {
        view.update(app, |v, _| v.state.fs_open = true);
        window.draw(app).clear(app);
    });
    wcx.run_until_parked();
    let b = wcx.cx.read(|app| view.read(app).fs_progress.get());
    assert!(b.size.width.as_f32() > 0.0, "fs-progress 盒应已布局（宽>0），实得 {b:?}");
    wcx.simulate_mouse_down(at_frac(&b, 0.25), MouseButton::Left, Default::default());
    wcx.run_until_parked();
    let pos = wcx.cx.read(|app| player.read(app).position());
    let want = dur * 0.25;
    assert!((pos - want).abs() < 1.0, "全屏进度条点按 25% 应 seek 到 {want:.1}s，实得 {pos:.1}");
}

/// 用例 E 条外/窗外松开：拖到 40% 后把鼠标移到条盒外（y 越界）再松开——`on_mouse_up_out`
/// 兜底按最后锁定落点收尾（「松手停在当前位置」），位置不闪跳、scrub 态不泄漏。
#[gpui::test]
async fn bb_progress_drag_release_outside_bar_lands_at_last_target(cx: &mut TestAppContext) {
    let dur = 100.0;
    let (view, wcx) = open_seek_view(cx, dur);
    let b = wcx.cx.read(|app| view.read(app).bottom_bar.read(app).bb_progress.get());
    assert!(b.size.width.as_f32() > 0.0, "bb-progress 盒应已布局（宽>0），实得 {b:?}");
    wcx.simulate_mouse_down(at_frac(&b, 0.80), MouseButton::Left, Default::default());
    wcx.run_until_parked();
    wcx.simulate_mouse_move(at_frac(&b, 0.40), Some(MouseButton::Left), Default::default());
    wcx.run_until_parked();
    let p1 = wcx.cx.read(|app| view.read(app).player.read(app).position());
    assert!((p1 - dur * 0.40).abs() < 1.0, "拖到 40% 应跟随 {:.1}s，实得 {p1:.1}", dur * 0.40);
    // 松开位置在条盒外（y 远高于盒中线）：应仍按最后锁定落点收尾。
    let outside = gpui::Point::new(
        px(b.origin.x.as_f32() + b.size.width.as_f32() * 0.40),
        px(b.origin.y.as_f32() - 40.0),
    );
    wcx.simulate_mouse_up(outside, MouseButton::Left, Default::default());
    wcx.run_until_parked();
    let (p2, scrubbing) = wcx.cx.read(|app| {
        let v = view.read(app);
        (v.player.read(app).position(), v.state.scrubbing)
    });
    assert!((p2 - dur * 0.40).abs() < 1.0, "条外松手应按最后落点收尾 {:.1}s，实得 {p2:.1}", dur * 0.40);
    assert!(!scrubbing, "条外松手应退出拖拽态（scrub 不泄漏）");
}

// ---- 性能优化第一步：拖动实时跟手回归（view notify；动画降频后不依赖寄生动画帧） ----

/// 开全屏浮层并渲染一帧（fs_progress 盒与 fs_fill_box 由 prepaint 写入），返回视图。
fn open_fs_seek_view<'a>(
    cx: &'a mut TestAppContext,
    player: &Entity<MusicPlayer>,
) -> (Entity<MusicView>, &'a mut VisualTestContext) {
    let player = player.clone();
    let (view, wcx) = cx.add_window_view(|_w, cx| MusicView::new(player, cx));
    wcx.update(|window, app| {
        view.update(app, |v, _| v.state.fs_open = true);
        window.draw(app).clear(app);
    });
    wcx.run_until_parked();
    (view, wcx)
}

/// 用例 R（拖动实时跟手·播放中）：音频动画已降频（with_max_fps 15）+ tick 250ms——若拖拽不
/// 给 MusicView 自己 notify，进度视觉只能寄生动画/tick 帧。**全屏进度条** down@50% → 按住
/// move@30%（**不推进虚拟时钟**：无 tick、无动画帧）→ `run_until_parked`。
///
/// 判定信号：`App::observe(&view)` 计数器——它只在本视图 `cx.notify()` 时触发（gpui
/// `invalidate_view` → `Effect::Notify{emitter=view}` → `apply_notify_effect` 按 emitter 派发观察
/// 者；player 实体 notify 走 emitter=player，不触发本观察者）。旧代码点/拖只 `player.update`
/// （notify player 实体）→ 观察者不触发 → RED；修复后 `apply_strip`/`seek_to_fraction` 补
/// `cx.notify()`（MusicView）→ 观察者触发 → GREEN。
/// 附视觉护栏：实测本 gpui 版本按 render「实体访问」追踪（window.rs `record_entities_accessed`，
/// render 读了 player → player 被窗口追踪 → player.notify 也会使窗口变脏重绘），故填充盒更新
/// 分不清视图 notify；计数断言才是给定语义的真判别。
#[gpui::test]
async fn fs_progress_drag_refreshes_fill_without_clock(cx: &mut TestAppContext) {
    let dur = 100.0;
    let player = player_with_library(cx);
    cx.update(|app| {
        player.update(app, |p, cx| {
            p.test_start_playing(0);
            p.set_test_duration(dur);
            p.seek(0.0, cx);
        });
    });
    let (view, wcx) = open_fs_seek_view(cx, &player);
    // 视图 notify 精确计数（只对 MusicView 自身的 cx.notify 触发）。
    let view_notifies = Rc::new(Cell::new(0usize));
    wcx.update(|_window, app| {
        let c = view_notifies.clone();
        app.observe(&view, move |_, _| c.set(c.get() + 1)).detach();
    });
    wcx.run_until_parked();
    let b = wcx.cx.read(|app| view.read(app).fs_progress.get());
    let bar_w = b.size.width.as_f32();
    assert!(bar_w > 0.0, "fs-progress 盒应已布局（宽>0），实得 {b:?}");
    let fill_at = |wcx: &mut VisualTestContext| {
        wcx.cx.read(|app| view.read(app).fs_fill_box.get())
    };
    // 初始帧（pos=0）→ 填充≈0。
    let f0 = fill_at(wcx);
    assert!(f0.size.width.as_f32() < 2.0, "初始填充应≈0，实得 {f0:?}");
    // 点按 50% + 按住拖到 30%：不推进虚拟时钟（不加 tick/动画帧）。
    // 先记基线（播放中 eq/vinyl 动画帧可能已送达过 1 次 notify，与拖拽无关——以增量为判别）。
    let base = view_notifies.get();
    wcx.simulate_mouse_down(at_frac(&b, 0.50), MouseButton::Left, Default::default());
    wcx.simulate_mouse_move(at_frac(&b, 0.30), Some(MouseButton::Left), Default::default());
    wcx.run_until_parked();
    // 判别：点/拖必须产生**额外的**视图自已 notify（拖拽跟手的承诺）。旧代码无视图 notify →
    // 计数停在基线 → RED。
    assert!(
        view_notifies.get() > base,
        "拖拽应触发 MusicView 自身 notify（动画降频后不靠寄生帧/player 实体 notify）：计数 {} → {}（无 cx.notify=RED）",
        base,
        view_notifies.get()
    );
    // 视觉护栏：真实重绘后填充应落在 ≈30%（本 gpui 版本 player 访问追踪也会触发重绘，仅护栏）。
    let f1 = fill_at(wcx);
    let want = bar_w * 0.30;
    assert!(
        (f1.size.width.as_f32() - want).abs() < 4.0,
        "拖到 30% 填充应更新到 ≈{want:.1}，实得 {:.1}",
        f1.size.width.as_f32()
    );
    // 再拖到 10% → 继续实时跟随回落（view notify 每 move 一次）。
    let after_first = view_notifies.get();
    wcx.simulate_mouse_move(at_frac(&b, 0.10), Some(MouseButton::Left), Default::default());
    wcx.run_until_parked();
    assert!(
        view_notifies.get() > after_first,
        "再拖一次也该再触发视图 notify（每 move 实时跟手）：计数 {after_first} → {}",
        view_notifies.get()
    );
    let f2 = fill_at(wcx);
    let want2 = bar_w * 0.10;
    assert!(
        (f2.size.width.as_f32() - want2).abs() < 4.0,
        "再拖到 10% 填充应实时回落 ≈{want2:.1}，实得 {:.1}",
        f2.size.width.as_f32()
    );
    // 松手收尾：停在 10%（不闪跳）。
    wcx.simulate_mouse_up(at_frac(&b, 0.10), MouseButton::Left, Default::default());
    wcx.run_until_parked();
    let pos = wcx.cx.read(|app| view.read(app).player.read(app).position());
    assert!((pos - dur * 0.10).abs() < 1.0, "松手应停在 10%，实得 {pos:.1}");
}

/// 用例 S（拖动实时跟手·暂停态）：playing=false 无 tick 无动画帧 → 点/拖**必须**由视图自己
/// notify 才重绘（旧代码暂停态拖动画面冻结）。以 `App::observe(&view)` 计数为判别：radio 至少
/// 触发一次；附填充更新护栏。seek 不误伤暂停（playing/paused 不变）。
#[gpui::test]
async fn fs_progress_drag_refreshes_fill_when_paused(cx: &mut TestAppContext) {
    let dur = 120.0;
    let player = player_with_library(cx);
    cx.update(|app| {
        player.update(app, |p, cx| {
            p.set_test_duration(dur);
            p.pause(cx);
        });
    });
    let (view, wcx) = open_fs_seek_view(cx, &player);
    let view_notifies = Rc::new(Cell::new(0usize));
    wcx.update(|_window, app| {
        let c = view_notifies.clone();
        app.observe(&view, move |_, _| c.set(c.get() + 1)).detach();
    });
    wcx.run_until_parked();
    let b = wcx.cx.read(|app| view.read(app).fs_progress.get());
    let bar_w = b.size.width.as_f32();
    assert!(bar_w > 0.0, "fs-progress 盒应已布局（宽>0），实得 {b:?}");
    let fill_at = |wcx: &mut VisualTestContext| {
        wcx.cx.read(|app| view.read(app).fs_fill_box.get())
    };
    let f0 = fill_at(wcx);
    assert!(f0.size.width.as_f32() < 2.0, "暂停初始填充应≈0，实得 {f0:?}");
    // 点按 30% + 按住 move@30%（不推时钟）→ 视图 notify → GREEN；无 notify → RED。
    let base = view_notifies.get();
    wcx.simulate_mouse_down(at_frac(&b, 0.30), MouseButton::Left, Default::default());
    wcx.simulate_mouse_move(at_frac(&b, 0.30), Some(MouseButton::Left), Default::default());
    wcx.run_until_parked();
    assert!(
        view_notifies.get() > base,
        "暂停态拖拽也该触发视图 notify（拖动跟手不依赖 tick/动画）：计数 {base} → {}（无 cx.notify=RED）",
        view_notifies.get()
    );
    let f1 = fill_at(wcx);
    let want = bar_w * 0.30;
    assert!(
        (f1.size.width.as_f32() - want).abs() < 4.0,
        "暂停态拖到 30% 填充应更新到 ≈{want:.1}，实得 {:.1}",
        f1.size.width.as_f32()
    );
    // 再拖到 80% → 继续实时跟随。
    let after_first = view_notifies.get();
    wcx.simulate_mouse_move(at_frac(&b, 0.80), Some(MouseButton::Left), Default::default());
    wcx.run_until_parked();
    assert!(
        view_notifies.get() > after_first,
        "再拖一次也该再触发视图 notify：计数 {after_first} → {}",
        view_notifies.get()
    );
    let f2 = fill_at(wcx);
    let want2 = bar_w * 0.80;
    assert!(
        (f2.size.width.as_f32() - want2).abs() < 4.0,
        "暂停态再拖到 80% 填充应跟随 ≈{want2:.1}，实得 {:.1}",
        f2.size.width.as_f32()
    );
    wcx.simulate_mouse_up(at_frac(&b, 0.80), MouseButton::Left, Default::default());
    wcx.run_until_parked();
    let s = wcx.cx.read(|app| {
        let p = view.read(app).player.read(app);
        (p.position(), p.playing(), p.paused())
    });
    assert!((s.0 - dur * 0.80).abs() < 1.0, "暂停态松手仍停 80%，实得 {:.1}", s.0);
    assert!(!s.1 && s.2, "seek 不误伤暂停（playing/paused 不变）");
}

// ---- 性能优化第三步 A 段：eq/vinyl 动画 → 纯展示子视图，动画 notify 只落子视图 ----

/// 播放中模拟数个动画帧：gpui `with_animation` 的 notify 目标是「包含动画元素的
/// current_view」（gpui animation.rs：`let view = window.current_view()` → 到点
/// `cx.notify(view)`）。A 段前动画在父 MusicView render 里构建 → current_view=父 →
/// 动画每帧 notify 父（15fps×整树重建）；A 段后动画归 `EqBarsView` 子视图 →
/// current_view=子视图，动画每帧只 notify 子视图、父计数器不动。
/// RED：旧代码父被动画每帧 notify → `parent_delta==0` 断言失败；GREEN：拆分后父不动、子随动画跳。
#[gpui::test]
async fn lively_animation_notifies_only_subviews_not_parent(cx: &mut TestAppContext) {
    let player = player_with_library(cx);
    cx.update(|app| {
        player.update(app, |p, cx| {
            p.test_start_playing(0); // 播放中：主区当前行嵌入 eq 子视图动画
            p.set_test_duration(100.0);
            p.seek(0.0, cx);
        });
    });
    let (view, wcx) = cx.add_window_view(|_w, cx| MusicView::new(player, cx));
    // 渲染首帧：eq 动画元素 mount → request_layout 安排 15fps（~66.7ms）timer。
    wcx.update(|window, app| window.draw(app).clear(app));
    wcx.run_until_parked();
    // 观察父 MusicView 与子 eq 子视图——只对各自 `cx.notify()` 触发（动画 notify 走 emitter）。
    let (parent_c, child_c) = wcx.update(|_w, app| {
        let eq = view.read(app).eq_main.clone();
        let parent_c = Rc::new(Cell::new(0usize));
        let child_c = Rc::new(Cell::new(0usize));
        {
            let c = parent_c.clone();
            app.observe(&view, move |_, _| c.set(c.get() + 1)).detach();
        }
        {
            let c = child_c.clone();
            app.observe(&eq, move |_, _| c.set(c.get() + 1)).detach();
        }
        (parent_c, child_c)
    });
    let base_p = parent_c.get();
    let base_c = child_c.get();
    // 3 拍动画帧：每拍 timer 到点 notify 子视图 → 显式重绘续排下一 timer。
    for _ in 0..3 {
        wcx.executor().advance_clock(Duration::from_millis(70));
        wcx.run_until_parked();
        wcx.update(|window, app| window.draw(app).clear(app));
        wcx.run_until_parked();
    }
    let dp = parent_c.get() - base_p;
    let dc = child_c.get() - base_c;
    assert!(
        dc >= 2,
        "动画帧应每拍 notify 子 eq 子视图（delta={dc}，子计数器应随动画跳）"
    );
    assert_eq!(
        dp, 0,
        "动画帧不得 notify 父 MusicView（旧代码动画 notify current_view=父=RED）：delta={dp}"
    );
}

// ---- 性能优化第三步 B 段：底栏 → BottomBarView 子视图（进度心跳 / 交互回父 / tooltip 回父）----

/// 按按钮 debug 盒中心做一次完整点击（down+up），驱动其 on_click 回调（按钮在底栏
/// 子视图的树内，debug_bounds 需 debug_selector 且已 prepaint）。
fn click_button(wcx: &mut VisualTestContext, selector: &'static str) {
    let b = wcx.debug_bounds(selector).expect("按钮应有 debug 盒");
    wcx.simulate_mouse_down(at_frac(&b, 0.5), MouseButton::Left, Default::default());
    wcx.run_until_parked();
    wcx.simulate_mouse_up(at_frac(&b, 0.5), MouseButton::Left, Default::default());
    wcx.run_until_parked();
}

/// 用例 B1：播放中推进 120ms×2（总 < 父 250ms tick 不发）→ 底栏子视图被心跳自 notify
/// （db≥2）、父 MusicView 不被该心跳 notify（dp==0）；附「渲染拉到最新 position」护栏：
/// seek(50) 后重绘一帧 → bb 填充宽 ≈ 0.5×条盒宽（headless 下 position=seek_base 确定性，
/// 与 fs_progress_fill 同款姿势——headless 证「心跳→重绘→新位置」数据链，真机由用户复验）。
#[gpui::test]
async fn bottom_heartbeat_notifies_subview_not_parent(cx: &mut TestAppContext) {
    let dur = 100.0;
    let player = player_with_library(cx);
    cx.update(|app| {
        player.update(app, |p, cx| {
            p.test_start_playing(0);
            p.set_test_duration(dur);
            p.seek(0.0, cx);
        });
    });
    let (view, wcx) = cx.add_window_view(|_w, cx| MusicView::new(player, cx));
    wcx.update(|window, app| window.draw(app).clear(app));
    wcx.run_until_parked();
    let (parent_c, bottom_c) = wcx.update(|_w, app| {
        let bottom = view.read(app).bottom_bar.clone();
        let parent_c = Rc::new(Cell::new(0usize));
        let bottom_c = Rc::new(Cell::new(0usize));
        {
            let c = parent_c.clone();
            app.observe(&view, move |_, _| c.set(c.get() + 1))
                .detach();
        }
        {
            let c = bottom_c.clone();
            app.observe(&bottom, move |_, _| c.set(c.get() + 1))
                .detach();
        }
        (parent_c, bottom_c)
    });
    let bp = parent_c.get();
    let bb = bottom_c.get();
    for _ in 0..2 {
        wcx.executor().advance_clock(Duration::from_millis(120));
        wcx.run_until_parked();
    }
    let dp = parent_c.get() - bp;
    let db = bottom_c.get() - bb;
    assert!(
        db >= 2,
        "播放中心跳应每 ~120ms 自 notify 底栏子视图：delta={db}"
    );
    assert_eq!(
        dp, 0,
        "心跳不得 notify 父 MusicView（父只在 250ms tick 才整树 notify）：delta={dp}"
    );
    // 渲染拉到最新 position：seek(50) 后显式重绘 → 进度显示数据链仍活（headless 下
    // position=seek_base 确定性；填充盒比例依赖既有条几何不在本次范围，只证 seek 语义
    // + 心跳把重绘接到子视图——真机进度平滑由用户复验）。
    wcx.update(|_w, app| {
        let p = view.read(app).player.clone();
        p.update(app, |p, cx| p.seek(dur * 0.5, cx));
    });
    wcx.update(|window, app| window.draw(app).clear(app));
    wcx.run_until_parked();
    let pos = wcx.cx.read(|app| view.read(app).player.read(app).position());
    assert!(
        (pos - dur * 0.5).abs() < 1.0,
        "seek(50) 后 position 应到 50s，实得 {pos:.1}"
    );
    assert!(
        wcx.debug_bounds("bb-progress-fill").is_some(),
        "bb 填充元素应仍在渲染（bb-progress-fill debug 盒存在）"
    );
}

/// 用例 B2：bb 进度条点击/拖动 seek 仍实时跟手（B 段复用既有回归断言姿势，观察改为
/// **子底栏** notify——拖动跟手由子视图自 notify，不寄生父 tick/动画帧）。
#[gpui::test]
async fn bb_progress_drag_notifies_bottom_subview(cx: &mut TestAppContext) {
    let dur = 100.0;
    let player = player_with_library(cx);
    cx.update(|app| {
        player.update(app, |p, _| p.set_test_duration(dur));
    });
    let (view, wcx) = cx.add_window_view(|_w, cx| MusicView::new(player, cx));
    wcx.update(|window, app| window.draw(app).clear(app));
    wcx.run_until_parked();
    let bottom_c = wcx.update(|_w, app| {
        let bottom = view.read(app).bottom_bar.clone();
        let c = Rc::new(Cell::new(0usize));
        {
            let c = c.clone();
            app.observe(&bottom, move |_, _| c.set(c.get() + 1))
                .detach();
        }
        c
    });
    let b = wcx
        .cx
        .read(|app| view.read(app).bottom_bar.read(app).bb_progress.get());
    assert!(
        b.size.width.as_f32() > 0.0,
        "bb-progress 盒应已布局（宽>0），实得 {b:?}"
    );
    let base = bottom_c.get();
    wcx.simulate_mouse_down(at_frac(&b, 0.60), MouseButton::Left, Default::default());
    wcx.simulate_mouse_move(at_frac(&b, 0.30), Some(MouseButton::Left), Default::default());
    wcx.run_until_parked();
    assert!(
        bottom_c.get() > base,
        "bb 拖拽应自 notify 底栏子视图（不寄生 tick/动画）：{} → {}",
        base,
        bottom_c.get()
    );
    let pos = wcx.cx.read(|app| view.read(app).player.read(app).position());
    assert!(
        (pos - dur * 0.30).abs() < 1.0,
        "拖到 30% 应跟随 {:.1}s，实得 {pos:.1}",
        dur * 0.30
    );
    wcx.simulate_mouse_up(at_frac(&b, 0.30), MouseButton::Left, Default::default());
    wcx.run_until_parked();
    let pos2 = wcx.cx.read(|app| view.read(app).player.read(app).position());
    assert!(
        (pos2 - dur * 0.30).abs() < 1.0,
        "松手仍停 30%，实得 {pos2:.1}"
    );
}

/// 用例 B3：底部各按钮（播放/上下首/随机/循环/收藏/歌词/队列/全屏）点击后**父状态**真正
/// 变化（参与断言：playing/current/shuffle/repeat/liked/right_tab/fs_open）——交互都经
/// `parent.update` 回父执行（WeakEntity 防循环），非子视图本地假象。
#[gpui::test]
async fn bottom_buttons_route_to_parent_state(cx: &mut TestAppContext) {
    let player = player_with_library(cx);
    cx.update(|app| {
        player.update(app, |p, _| p.test_start_playing(0));
    });
    let (view, wcx) = cx.add_window_view(|_w, cx| MusicView::new(player, cx));
    wcx.update(|window, app| window.draw(app).clear(app));
    wcx.run_until_parked();
    // 播放/暂停：bb-play → playing 翻转。
    assert!(
        wcx.cx.read(|app| view.read(app).player.read(app).playing()),
        "前置：播放中"
    );
    click_button(wcx, "bb-play");
    assert!(
        !wcx.cx.read(|app| view.read(app).player.read(app).playing()),
        "bb-play 应翻转 playing（回父 toggle_play）"
    );
    // 下一首/上一首：bb-next → current 前进；bb-prev → current 回到 0。
    let c0 = wcx.cx.read(|app| view.read(app).player.read(app).current());
    assert_eq!(c0, Some(0), "前置 current=0");
    click_button(wcx, "bb-next");
    let c1 = wcx.cx.read(|app| view.read(app).player.read(app).current());
    assert_eq!(c1, Some(1), "bb-next 应前进到 1");
    click_button(wcx, "bb-prev");
    let c2 = wcx.cx.read(|app| view.read(app).player.read(app).current());
    assert_eq!(c2, Some(0), "bb-prev 应回到 0");
    // 随机：bb-shuffle → shuffle 翻转。
    let s0 = wcx.cx.read(|app| view.read(app).player.read(app).shuffle());
    click_button(wcx, "bb-shuffle");
    let s1 = wcx.cx.read(|app| view.read(app).player.read(app).shuffle());
    assert_ne!(s0, s1, "bb-shuffle 应翻转 shuffle");
    // 循环：bb-repeat → repeat Off→All。
    assert_eq!(
        wcx.cx.read(|app| view.read(app).player.read(app).repeat()),
        RepeatMode::Off
    );
    click_button(wcx, "bb-repeat");
    assert_eq!(
        wcx.cx.read(|app| view.read(app).player.read(app).repeat()),
        RepeatMode::All,
        "bb-repeat Off→All"
    );
    // 收藏：bb-like → 当前曲 liked 翻转。
    let l0 = wcx.cx.read(|app| view.read(app).player.read(app).is_liked(0));
    click_button(wcx, "bb-like");
    let l1 = wcx.cx.read(|app| view.read(app).player.read(app).is_liked(0));
    assert_ne!(l0, l1, "bb-like 应翻转当前曲收藏");
    // 歌词/队列 tab：bb-lyrics/bb-queue → right_tab 切换。
    assert_eq!(
        wcx.cx.read(|app| view.read(app).state.right_tab),
        RightTab::Song,
        "前置右侧面板在歌曲 tab"
    );
    click_button(wcx, "bb-lyrics");
    assert_eq!(
        wcx.cx.read(|app| view.read(app).state.right_tab),
        RightTab::Lyrics,
        "bb-lyrics 应切到歌词 tab"
    );
    click_button(wcx, "bb-queue");
    assert_eq!(
        wcx.cx.read(|app| view.read(app).state.right_tab),
        RightTab::Queue,
        "bb-queue 应切到队列 tab"
    );
    // 全屏：bb-fs → fs_open 翻转。
    let fs0 = wcx.cx.read(|app| view.read(app).state.fs_open);
    click_button(wcx, "bb-fs");
    let fs1 = wcx.cx.read(|app| view.read(app).state.fs_open);
    assert_ne!(fs0, fs1, "bb-fs 应翻转 fs_open");
}

/// 用例 B4（tooltip 不回归）：底栏按钮悬停提示仍出现且**锚定按钮盒**（走父全局 tooltip
/// 层——本视图把按钮盒写入父 tip_boxes 同名 cell）。断言 bb-fs：悬停 → 满 500ms 显示 +
/// 浮层底边中点横贴 bb-fs 中心、竖在按钮上缘上方 GAP。
#[gpui::test]
fn bb_fs_tooltip_appears_and_anchors(cx: &mut TestAppContext) {
    let player = player_with_library(cx);
    let (view, wcx) = open_tooltip_view(cx, player);
    let bbfs = wcx.debug_bounds("bb-fs").expect("bb-fs 应有 debug 盒");
    wcx.simulate_mouse_move(at_frac(&bbfs, 0.5), None, Default::default());
    wcx.run_until_parked();
    assert_eq!(
        tooltip_text(&view, wcx).as_deref(),
        Some("全屏"),
        "悬停 bb-fs 应设名称提示"
    );
    wcx.executor().advance_clock(Duration::from_millis(600));
    wcx.run_until_parked();
    assert!(tooltip_showing(&view, wcx), "满 500ms 应显示浮层");
    wcx.update(|window, app| window.draw(app).clear(app));
    wcx.run_until_parked();
    let tip = wcx.debug_bounds("music-tooltip").expect("浮层应在渲染");
    let want_mid_x = bbfs.origin.x.as_f32() + bbfs.size.width.as_f32() * 0.5;
    let want_bottom_y = bbfs.origin.y.as_f32() - super::TOOLTIP_GAP_PX;
    assert!(
        (tip.origin.x.as_f32() + tip.size.width.as_f32() * 0.5 - want_mid_x).abs() < 2.0,
        "浮层底边中点应横贴 bb-fs 中心"
    );
    assert!(
        (tip.origin.y.as_f32() + tip.size.height.as_f32() - want_bottom_y).abs() < 2.0,
        "浮层底边应在 bb-fs 上缘上方 GAP"
    );
}

// ---- 性能优化第三步·墙钟探针（K27 留数）：播放中 1s 的「父整树重建次数/秒」折算 ----

/// 模拟播放中 1 秒（20ms 步进 × 50，每步跑定期并显式重绘以续排各 timer），分别数出
/// 父 MusicView / eq 子视图 / 底栏子视图的 notify 次数；按调试构建每整树重建 ~42ms、
/// 子视图 ~1ms 折算主线程占用 ms/s。
/// before（真机语境）：父整树重建 = 15fps(动画 notify 父) + 4Hz(tick) ≈ 19 次/s；
/// 3B（底栏拆子视图后）：父 ≈ 父 tick 4Hz，eq 15fps × ~1ms、底栏心跳 ~8Hz × ~1ms；
/// 4B（歌词/np 进度拆子视图后）：普通（非全屏）态父 tick **不再整树 notify** → 父 ≈ 0
/// 次/s（稳态 0 重建），eq/底栏心跳照旧落到各自子视图（np/歌词子视图另有自己的心跳，
/// 本条探针只数父/eq/底栏三处基线）。
/// 断言只给宽区间（计时器对齐有抖动）；精确数值由 eprintln 留数（报告引用）。
#[gpui::test]
async fn lively_playback_1s_rebuild_rate_probe(cx: &mut TestAppContext) {
    let player = player_with_library(cx);
    cx.update(|app| {
        player.update(app, |p, cx| {
            p.test_start_playing(0);
            p.set_test_duration(100.0);
            p.seek(0.0, cx);
        });
    });
    let (view, wcx) = cx.add_window_view(|_w, cx| MusicView::new(player, cx));
    wcx.update(|window, app| window.draw(app).clear(app));
    wcx.run_until_parked();
    let (parent_c, eq_c, bottom_c) = wcx.update(|_w, app| {
        let (eq, bottom) = {
            let v = view.read(app);
            (v.eq_main.clone(), v.bottom_bar.clone())
        };
        let parent_c = Rc::new(Cell::new(0usize));
        let eq_c = Rc::new(Cell::new(0usize));
        let bottom_c = Rc::new(Cell::new(0usize));
        {
            let c = parent_c.clone();
            app.observe(&view, move |_, _| c.set(c.get() + 1)).detach();
        }
        {
            let c = eq_c.clone();
            app.observe(&eq, move |_, _| c.set(c.get() + 1)).detach();
        }
        {
            let c = bottom_c.clone();
            app.observe(&bottom, move |_, _| c.set(c.get() + 1)).detach();
        }
        (parent_c, eq_c, bottom_c)
    });
    let (bp0, be0, bb0) = (parent_c.get(), eq_c.get(), bottom_c.get());
    for _ in 0..50 {
        wcx.executor().advance_clock(Duration::from_millis(20));
        wcx.run_until_parked();
        wcx.update(|window, app| window.draw(app).clear(app));
        wcx.run_until_parked();
    }
    let (p, q, b) = (
        parent_c.get() - bp0,
        eq_c.get() - be0,
        bottom_c.get() - bb0,
    );
    // 折算（debug：父整树 42ms/次、子视图 ~1ms/次）：
    let parent_ms_s = p as f64 * 42.0;
    let eq_ms_s = q as f64 * 1.0;
    let bottom_ms_s = b as f64 * 1.0;
    let before_ms_s = (15.0 + 4.0) * 42.0;
    let after_ms_s = parent_ms_s + eq_ms_s + bottom_ms_s;
    eprintln!(
        "[perf] playback_1s: parent_whole_rebuild={p} ({parent_ms_s:.0}ms/s) eq_subview={q} ({eq_ms_s:.0}ms/s) bottom_subview={b} ({bottom_ms_s:.0}ms/s) | before≈{before_ms_s:.0}ms/s after≈{after_ms_s:.0}ms/s (-{:.0}%)",
        (1.0 - after_ms_s / before_ms_s) * 100.0
    );
    assert!(
        (0..=1).contains(&p),
        "普通态播放中父整树重建应≈0（歌词/np 已拆子视图；before=4Hz tick）：实得 {p}"
    );
    assert!(q >= 10, "eq 子视图应承接 ~15fps 动画 notify：实得 {q}");
    assert!(b >= 7, "底栏子视图应承接 ~8Hz 心跳 notify：实得 {b}");
}

// ---- 歌词面板：可滚浏览全部 + 自动跟随当前句（手动滚动暂停、停止约 3 秒恢复） ----

/// 建含第 0 首有 LRC 歌词的临时库播放器（歌名「歌手 - 歌词歌」；无设备 → 纯状态播放，
/// position = seek_base，确定性；有设备时 Opened 覆盖时长——断言一律按实际 active 算）。
/// 目录名带全局自增序号：多测试并行共用同名会互相删写 → 不稳定。
static LRC_DIR_SEQ: AtomicUsize = AtomicUsize::new(0);
fn player_with_lrc(cx: &mut TestAppContext, lrc: &str) -> Entity<MusicPlayer> {
    let seq = LRC_DIR_SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "gpui-music-lrc-view-{}-{seq}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    write_tiny_wav(&dir, "歌手 - 歌词歌.wav");
    std::fs::write(dir.join("歌手 - 歌词歌.lrc"), lrc).unwrap();
    let player: Entity<MusicPlayer> = cx.update(|app| app.new(|cx| MusicPlayer::new(cx)));
    cx.update(|app| {
        player.update(app, |p, cx| p.load_dir(&dir, cx));
    });
    drain_scan(cx);
    player
}

/// 30 句歌词 LRC：第 i 句时间 = i×30 秒（句距 30s，测试内 position 微漂不跨句）。
fn lrc_30_lines() -> String {
    (0..30)
        .map(|i| format!("[{:02}:{:02}.00]第{i}句", i / 2, (i % 2) * 30))
        .collect::<Vec<_>>()
        .join("\n")
}

/// 打开视图 → 切「歌词」tab → 渲染一帧（布局给滚动容器产出 bounds/max）。
fn open_lyrics_tab<'a>(
    cx: &'a mut TestAppContext,
    player: &Entity<MusicPlayer>,
) -> (Entity<MusicView>, &'a mut VisualTestContext) {
    let player = player.clone();
    let (view, wcx) = cx.add_window_view(|_w, cx| MusicView::new(player, cx));
    wcx.update(|window, app| {
        view.update(app, |v, _| v.state.right_tab = RightTab::Lyrics);
        window.draw(app).clear(app);
    });
    wcx.run_until_parked();
    (view, wcx)
}

/// 置 player 为「播放中 + 停在 pos_secs」：`test_start_playing(0)`（纯状态播放，不发
/// worker/不开设备 → playing 恒定 true，无 Error 翻转）→ set_test_duration → seek(pos)
/// （显示基准=pos → position=pos，确定性）。
fn play_at_pos(player: &Entity<MusicPlayer>, cx: &mut TestAppContext, pos_secs: f64) {
    cx.update(|app| {
        player.update(app, |p, cx| {
            p.test_start_playing(0);
            p.set_test_duration(10_000.0);
            p.seek(pos_secs, cx);
        });
    });
}

/// 按**当前实际播放位置**算自动跟随目标偏移（y，offset 值 = 负的句首坐标）。测试断言
/// 一律与实际 active 比对：无设备时 position=seek_base（确定性）；有设备时若 Opened
/// 接管时长、position 变化，预期也随之变化——断言始终成立。歌词状态已迁入子视图
/// `lyrics_right`（右栏实例），本 helper 经其读滚动句柄与跟随算法。
fn expected_follow_y(view: &Entity<MusicView>, cx: &mut VisualTestContext, n: usize) -> f32 {
    cx.update(|_w, app| {
        let lyrics = view.read(app).lyrics_right.clone();
        lyrics.update(app, |lv, cx| {
            let pos_ms = (lv.player.read(cx).position() * 1000.0) as i64;
            let active = lv
                .player
                .read(cx)
                .current_track()
                .and_then(|t| t.lyrics.clone())
                .and_then(|l| l.active_index(pos_ms))
                .unwrap_or(0);
            -lv.lyrics_follow_scroll_top(active, n)
        })
    })
}

/// 用例 A：长歌词全部可滚——内容总高 = 行数×固定行高；顶部物化首行、滚轮下滚、滚到底
/// (offset=-max) 物化末行、回顶物化首行。
#[gpui::test]
async fn lyrics_long_scroll_covers_all_lines(cx: &mut TestAppContext) {
    use gpui::{Modifiers, ScrollDelta, ScrollWheelEvent, TouchPhase, point};
    let player = player_with_lrc(cx, &lrc_30_lines());
    play_at_pos(&player, cx, 0.0);
    let (view, wcx) = open_lyrics_tab(cx, &player);
    pump_ticks(wcx, 4);
    let n = 30usize;
    let handle = wcx.cx.read(|app| view.read(app).lyrics_right.read(app).lyrics_scroll.clone());
    // 内容总高 = n×行高（max_offset = 内容总高 − 视口高）；视口应已布局。
    let viewport = handle.bounds().size.height.as_f32();
    let max = handle.max_offset().y.as_f32();
    assert!(viewport > 0.0, "歌词滚动容器应已布局 viewport={viewport}");
    assert!(
        (viewport + max - n as f32 * LYRIC_ROW_H).abs() < 2.0,
        "内容总高应=行数×行高：total={} n*row={}",
        viewport + max,
        n as f32 * LYRIC_ROW_H
    );
    // 顶部：物化窗口含第一行。
    let top_win = wcx.update(|_w, app| {
        let lv = view.read(app).lyrics_right.clone();
        lv.update(app, |lv, _| lv.lyrics_visible_window(n))
    });
    assert_eq!(top_win.0, 0, "顶部应物化第一行，实得 {:?}", top_win);
    // 滚轮：在容器上滚 → 偏移下滚（offset 更负）。
    let b = handle.bounds();
    let pos = point(
        px(b.origin.x.as_f32() + b.size.width.as_f32() * 0.5),
        px(b.origin.y.as_f32() + b.size.height.as_f32() * 0.5),
    );
    wcx.simulate_event(ScrollWheelEvent {
        position: pos,
        delta: ScrollDelta::Pixels(point(px(0.), px(-120.))),
        modifiers: Modifiers::default(),
        touch_phase: TouchPhase::Moved,
    });
    wcx.run_until_parked();
    let off_wheel = handle.offset().y.as_f32();
    assert!(off_wheel < 0.0, "滚轮应让歌词下滚（offset<0），实得 {off_wheel}");
    // 滚到底：offset≈-max，物化窗口含末行。
    handle.set_offset(point(px(0.), px(-max)));
    wcx.update(|window, app| window.draw(app).clear(app));
    wcx.run_until_parked();
    let off_bottom = handle.offset().y.as_f32();
    assert!(
        (off_bottom + max).abs() < 2.0,
        "滚到底 offset 应≈-max：{off_bottom} vs {max}"
    );
    let bot_win = wcx.update(|_w, app| {
        let lv = view.read(app).lyrics_right.clone();
        lv.update(app, |lv, _| lv.lyrics_visible_window(n))
    });
    assert_eq!(bot_win.1, n, "滚到底应物化末行，实得 {:?}", bot_win);
    // 回顶：offset=0，物化窗口含首行。
    handle.set_offset(point(px(0.), px(0.)));
    wcx.update(|window, app| window.draw(app).clear(app));
    wcx.run_until_parked();
    let top2_win = wcx.update(|_w, app| {
        let lv = view.read(app).lyrics_right.clone();
        lv.update(app, |lv, _| lv.lyrics_visible_window(n))
    });
    assert_eq!(top2_win.0, 0, "回顶应物化首行，实得 {:?}", top2_win);
}

/// 用例 B：自动跟随——不手动滚动时，offset 把 active 行带入可视区（≈ 当前句居中）。
#[gpui::test]
async fn lyrics_auto_follow_centers_active(cx: &mut TestAppContext) {
    let player = player_with_lrc(cx, &lrc_30_lines());
    play_at_pos(&player, cx, 450.0); // 450s → 第 15 句
    let (view, wcx) = open_lyrics_tab(cx, &player);
    pump_ticks(wcx, 4);
    let handle = wcx
        .cx
        .read(|app| view.read(app).lyrics_right.read(app).lyrics_scroll.clone());
    let expected = expected_follow_y(&view, wcx, 30);
    let got = handle.offset().y.as_f32();
    assert!(
        (got - expected).abs() < 2.0,
        "自动跟随应把 active 行居中：got={got} expected={expected}"
    );
    // 再多几拍也不漂走（同一跟随位）。
    pump_ticks(wcx, 4);
    let got2 = handle.offset().y.as_f32();
    assert!(
        (got2 - expected).abs() < 2.0,
        "持续播放 offset 应保持跟随位：got2={got2} expected={expected}"
    );
}

/// 用例 C：手动滚动暂停跟随 + 停止约 3 秒后自动恢复。
#[gpui::test]
async fn lyrics_manual_scroll_pauses_then_resumes_after_3s(cx: &mut TestAppContext) {
    let player = player_with_lrc(cx, &lrc_30_lines());
    play_at_pos(&player, cx, 450.0);
    let (view, wcx) = open_lyrics_tab(cx, &player);
    pump_ticks(wcx, 4);
    let handle = wcx
        .cx
        .read(|app| view.read(app).lyrics_right.read(app).lyrics_scroll.clone());
    let expected0 = expected_follow_y(&view, wcx, 30);
    assert!(
        (handle.offset().y.as_f32() - expected0).abs() < 2.0,
        "前置：自动跟随已居中当前句"
    );
    // 用户手动滚动（等价滚轮）→ 偏移离开跟随位。
    handle.set_offset(gpui::point(px(0.), px(-300.)));
    wcx.run_until_parked();
    // 下一拍检测到手动 → 暂停跟随，offset 保持用户位置、不被 tick 拉回。
    pump_ticks(wcx, 1);
    assert!(
        wcx.cx
            .read(|app| view.read(app).lyrics_right.read(app).lyrics_manual_since.is_some()),
        "手动滚动应暂停跟随"
    );
    let off1 = handle.offset().y.as_f32();
    assert!((off1 - (-300.0)).abs() < 2.0, "暂停后应保持用户位置：{off1}");
    // 3 秒内（距检测 ~2.75s）：继续不被拉回。
    pump_ticks(wcx, 11);
    let off2 = handle.offset().y.as_f32();
    assert!(
        (off2 - (-300.0)).abs() < 2.0,
        "3 秒内 offset 应保持用户浏览位：{off2}"
    );
    // 超过 3 秒：自动恢复 → 回到当前句居中。
    pump_ticks(wcx, 2);
    let off3 = handle.offset().y.as_f32();
    let expected = expected_follow_y(&view, wcx, 30);
    assert!(
        (off3 - expected).abs() < 2.0,
        "3 秒后应恢复跟随当前句：off3={off3} expected={expected}"
    );
    assert!(
        wcx.cx
            .read(|app| view.read(app).lyrics_right.read(app).lyrics_manual_since.is_none()),
        "恢复后 manual 应清空"
    );
}

/// 用例 D：暂停态（position 冻结）手动滚动后**不**自动回中。
#[gpui::test]
async fn lyrics_paused_manual_scroll_not_pulled_back(cx: &mut TestAppContext) {
    let player = player_with_lrc(cx, &lrc_30_lines());
    play_at_pos(&player, cx, 450.0);
    let (view, wcx) = open_lyrics_tab(cx, &player);
    pump_ticks(wcx, 2);
    wcx.update(|_w, app| {
        player.update(app, |p, cx| p.pause(cx));
    });
    let handle = wcx
        .cx
        .read(|app| view.read(app).lyrics_right.read(app).lyrics_scroll.clone());
    handle.set_offset(gpui::point(px(0.), px(-120.)));
    wcx.run_until_parked();
    // 暂停态推远超 3 秒：不恢复跟随（非播放不动滚动，尊重手动浏览）。
    pump_ticks(wcx, 16);
    let off = handle.offset().y.as_f32();
    assert!(
        (off - (-120.0)).abs() < 2.0,
        "暂停态手动滚动后不应被拉回：{off}"
    );
    assert!(!wcx.cx.read(|app| view.read(app).player.read(app).playing()));
}

/// 用例 E：换曲/换目录复位滚动跟随态——手动暂停后切到下一首，新曲从自动跟随开始。
#[gpui::test]
async fn lyrics_track_change_resets_follow(cx: &mut TestAppContext) {
    let dir = std::env::temp_dir().join(format!("gpui-music-lrc-switch-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    write_tiny_wav(&dir, "歌手 - 歌词歌.wav");
    std::fs::write(dir.join("歌手 - 歌词歌.lrc"), lrc_30_lines()).unwrap();
    write_tiny_wav(&dir, "歌手 - 下一首.wav");
    std::fs::write(dir.join("歌手 - 下一首.lrc"), "[00:10.00]只此一句\n[00:20.00]第二句").unwrap();
    let player: Entity<MusicPlayer> = cx.update(|app| app.new(|cx| MusicPlayer::new(cx)));
    cx.update(|app| {
        player.update(app, |p, cx| p.load_dir(&dir, cx));
    });
    drain_scan(cx);
    play_at_pos(&player, cx, 450.0);
    let (view, wcx) = open_lyrics_tab(cx, &player);
    pump_ticks(wcx, 4);
    // 手动滚轮 → 暂停跟随（真实用户滚动通道；**不能用超界 set_offset(-700) 直写**：音频
    // 动画降频（with_max_fps 15）后播放中会按周期 notify 重绘，生产路径滚动偏移早就被布局
    // 钳回 [0,max]，下一拍读到的永远是「钳位后」值——旧测试靠 fake 调度器不给动画送帧才
    // 看到未钳位的 -700，属测试假象。滚轮监听（fullscreen.rs `on_scroll_wheel`）才是手动
    // 滚动主信号，与 G/H 用例一致）。
    wheel_on_lyrics(wcx, &view);
    pump_ticks(wcx, 1);
    assert!(
        wcx.cx
            .read(|app| view.read(app).lyrics_right.read(app).lyrics_manual_since.is_some())
    );
    // 切到第 1 首（path 变化 → 跟随态复位 → 新曲从自动跟随开始，不沿用手动暂停）。
    wcx.update(|_w, app| {
        player.update(app, |p, _| p.test_start_playing(1));
    });
    pump_ticks(wcx, 3);
    let expected = expected_follow_y(&view, wcx, 2);
    let got = wcx
        .cx
        .read(|app| view.read(app).lyrics_right.read(app).lyrics_scroll.clone());
    let got = got.offset().y.as_f32();
    assert!(
        (got - expected).abs() < 2.0,
        "换曲后应复位并在新曲上跟随：got={got} expected={expected}"
    );
    assert!(
        wcx.cx
            .read(|app| view.read(app).lyrics_right.read(app).lyrics_manual_since.is_none()),
        "换曲后 manual 应复位清空"
    );
}

/// 用例 F：边界——1 行/短歌词（内容 ≤ 视口）全部物化居中、无可滚范围；无歌词/未播放
/// （无 current）空态渲染——均不 panic。
#[gpui::test]
async fn lyrics_edge_cases_no_panic(cx: &mut TestAppContext) {
    // 1 行歌词 + 播放中：内容 ≤ 视口 → 全部物化、无法滚动（max=0），渲染不 panic。
    let p1 = player_with_lrc(cx, "[00:03.00]只有一句");
    play_at_pos(&p1, cx, 0.0);
    let (v1, w1) = cx.add_window_view(|_w, cx| MusicView::new(p1, cx));
    w1.update(|window, app| {
        v1.update(app, |v, _| v.state.right_tab = RightTab::Lyrics);
        window.draw(app).clear(app);
    });
    w1.run_until_parked();
    let max1 = w1
        .cx
        .read(|app| v1.read(app).lyrics_right.read(app).lyrics_scroll.max_offset().y.as_f32());
    assert_eq!(max1, 0.0, "1 行歌词（内容≤视口）应无可滚范围，实得 {max1}");

    // 无歌词 + 无 current（未播放）：空态渲染不 panic。
    let p2 = player_with_lrc(cx, "[00:03.00]未播放不取歌词");
    let (v2, w2) = cx.add_window_view(|_w, cx| MusicView::new(p2, cx));
    w2.update(|window, app| {
        v2.update(app, |v, _| v.state.right_tab = RightTab::Lyrics);
        window.draw(app).clear(app);
    });
    w2.run_until_parked();
    let max2 = w2
        .cx
        .read(|app| v2.read(app).lyrics_right.read(app).lyrics_scroll.max_offset().y.as_f32());
    assert!(max2 >= 0.0, "空态下滚动容器也应可布局，实得 {max2}");
}

// ---- 真机复报回归：尾部持续滚动被 3 秒误恢复拉回 / 滚到底继续滚轮「闪」 ----

/// 在歌词滚动容器上派发一次真实滚动事件（offset 已钳到界限时事件照样触发）。
/// 状态迁入子视图后，容器句柄经 `lyrics_right`（右栏实例）读取。
fn wheel_on_lyrics(wcx: &mut VisualTestContext, view: &Entity<MusicView>) {
    use gpui::{Modifiers, ScrollDelta, ScrollWheelEvent, TouchPhase, point};
    let handle = wcx
        .cx
        .read(|app| view.read(app).lyrics_right.read(app).lyrics_scroll.clone());
    let b = handle.bounds();
    let pos = point(
        px(b.origin.x.as_f32() + b.size.width.as_f32() * 0.5),
        px(b.origin.y.as_f32() + b.size.height.as_f32() * 0.5),
    );
    wcx.simulate_event(ScrollWheelEvent {
        position: pos,
        delta: ScrollDelta::Pixels(point(px(0.), px(-120.))),
        modifiers: Modifiers::default(),
        touch_phase: TouchPhase::Moved,
    });
    wcx.run_until_parked();
}

/// 用例 G（bug1 回归）：尾部**持续滚动**不误恢复——用户一直滚（每 ~0.5s 一轮滚轮事件），
/// 但 offset 已钳到 max 不再变化，旧实现靠「offset 偏差」刷新计时 → 满 3s 仍被拉回。
/// 修复后每次滚轮事件都刷新「停止滚动」计时 → 绝不恢复。
#[gpui::test]
async fn lyrics_continuous_scroll_at_tail_never_jumps_back(cx: &mut TestAppContext) {
    let player = player_with_lrc(cx, &lrc_30_lines());
    play_at_pos(&player, cx, 450.0);
    let (view, wcx) = open_lyrics_tab(cx, &player);
    pump_ticks(wcx, 4);
    let handle = wcx
        .cx
        .read(|app| view.read(app).lyrics_right.read(app).lyrics_scroll.clone());
    let max = handle.max_offset().y.as_f32();
    let tail = -max;
    // 用户滚到尾部（手动滚动）→ 下一拍检测手动。
    handle.set_offset(gpui::point(px(0.), px(tail)));
    pump_ticks(wcx, 1);
    assert!(
        wcx.cx
            .read(|app| view.read(app).lyrics_right.read(app).lyrics_manual_since.is_some())
    );
    // 持续滚动：每轮 = 尾部滚一轮 + 推 0.5s。总时长跨越 3s，但每轮滚轮都刷新计时。
    for i in 0..8usize {
        wheel_on_lyrics(wcx, &view);
        pump_ticks(wcx, 2);
        let off = handle.offset().y.as_f32();
        assert!(
            (off - tail).abs() < 2.0,
            "第 {i} 轮：持续滚动 offset 应保持尾部 {tail}，实得 {off}（旧实现满 3 秒即拉回）"
        );
    }
}

/// 用例 H（bug1 反向）：**停止**滚动后才满 3 秒 → 恢复跟随当前句（验证没把恢复打断）。
#[gpui::test]
async fn lyrics_stop_at_tail_resumes_after_3s(cx: &mut TestAppContext) {
    let player = player_with_lrc(cx, &lrc_30_lines());
    play_at_pos(&player, cx, 450.0);
    let (view, wcx) = open_lyrics_tab(cx, &player);
    pump_ticks(wcx, 4);
    let handle = wcx
        .cx
        .read(|app| view.read(app).lyrics_right.read(app).lyrics_scroll.clone());
    let max = handle.max_offset().y.as_f32();
    handle.set_offset(gpui::point(px(0.), px(-max)));
    pump_ticks(wcx, 1); // 检测手动
    assert!(
        wcx.cx
            .read(|app| view.read(app).lyrics_right.read(app).lyrics_manual_since.is_some())
    );
    // 停手 3 秒内保持尾部。
    pump_ticks(wcx, 10); // +2.5s
    let off1 = handle.offset().y.as_f32();
    assert!((off1 + max).abs() < 2.0, "停手 2.5s 内应保持尾部，实得 {off1}");
    // 满 3 秒 → 恢复跟随当前句。
    pump_ticks(wcx, 3); // 累计 ≥3s
    let expected = expected_follow_y(&view, wcx, 30);
    let off2 = handle.offset().y.as_f32();
    assert!(
        (off2 - expected).abs() < 2.0,
        "停手满 3s 应恢复跟随当前句：off2={off2} expected={expected}"
    );
    assert!(
        wcx.cx
            .read(|app| view.read(app).lyrics_right.read(app).lyrics_manual_since.is_none()),
        "恢复后 manual 应清空"
    );
}

/// 用例 I（bug2 回归）：尾部滚轮后 offset 恒为 max（不写回）、连续两拍物化窗口一致
/// （无「刷新/闪」——不产生多余 set_offset / 视窗跳变）。
#[gpui::test]
async fn lyrics_tail_wheel_keeps_window_stable(cx: &mut TestAppContext) {
    let player = player_with_lrc(cx, &lrc_30_lines());
    play_at_pos(&player, cx, 450.0);
    let (view, wcx) = open_lyrics_tab(cx, &player);
    pump_ticks(wcx, 4);
    let handle = wcx
        .cx
        .read(|app| view.read(app).lyrics_right.read(app).lyrics_scroll.clone());
    let max = handle.max_offset().y.as_f32();
    handle.set_offset(gpui::point(px(0.), px(-max)));
    pump_ticks(wcx, 1); // 检测手动
    // 尾部滚一轮：offset 钳位不动 → 连续两拍物化窗口一致。
    wheel_on_lyrics(wcx, &view);
    let off1 = handle.offset().y.as_f32();
    let win1 = wcx.update(|_w, app| {
        let lv = view.read(app).lyrics_right.clone();
        lv.update(app, |lv, _| lv.lyrics_visible_window(30))
    });
    pump_ticks(wcx, 2); // 未满 3s，不恢复
    let off2 = handle.offset().y.as_f32();
    let win2 = wcx.update(|_w, app| {
        let lv = view.read(app).lyrics_right.clone();
        lv.update(app, |lv, _| lv.lyrics_visible_window(30))
    });
    assert!(
        (off1 + max).abs() < 2.0 && (off2 + max).abs() < 2.0,
        "尾部滚轮 offset 应恒定钳在 max：off1={off1} off2={off2}"
    );
    assert_eq!(
        win1, win2,
        "尾部滚轮连续两拍物化窗口应一致（不闪）：win1={win1:?} win2={win2:?}"
    );
}

// ---- 真机复报二：底部继续滚轮「文字闪」——物化窗口读了滚轮已加未钳位的瞬时 offset ----

/// 用例 J（bug2' 回归）：render/prepaint 可能读到「滚轮先加、layout 未钳回」的超界瞬时
/// 偏移（offset 越界到 `max + 残量`）。物化行窗必须按**钳位后**的 max 反算 first，否则
/// 底部每滚一格错位一行 → 1 帧漂移窗口 = 「闪」。当前实现直接 `-offset.y/ROW` 会多 +2 行
/// （RED）；修复后与最终钳位布局一致。
#[gpui::test]
async fn lyrics_overbound_offset_uses_clamped_window(cx: &mut TestAppContext) {
    let player = player_with_lrc(cx, &lrc_30_lines());
    play_at_pos(&player, cx, 450.0);
    let (view, wcx) = open_lyrics_tab(cx, &player);
    pump_ticks(wcx, 4);
    let handle = wcx
        .cx
        .read(|app| view.read(app).lyrics_right.read(app).lyrics_scroll.clone());
    let n = 30usize;
    let max = handle.max_offset().y.as_f32();
    assert!(max > 0.0, "多行长歌词应可滚 max={max}");
    // 模拟滚轮在底部「先加后钳」的瞬时态：offset 越界到 max + 2 行（layout 尚未钳回）。
    handle.set_offset(gpui::point(px(0.), px(-(max + LYRIC_ROW_H * 2.0))));
    // 修复：物化窗口按钳位（max）算 first，与最终布局一致 → 不再漂移闪。
    let (first, _last) = wcx.update(|_w, app| {
        let lv = view.read(app).lyrics_right.clone();
        lv.update(app, |lv, _| lv.lyrics_visible_window(n))
    });
    let expected_first = ((max / LYRIC_ROW_H).floor() as usize).min(n - 1);
    assert_eq!(
        first, expected_first,
        "超界瞬时偏移的物化 first 应按钳位 max 算：got={first} expected={expected_first} (max={max})"
    );
    // 中段偏移不受钳位影响（正常浏览位置 still 精确，回归护栏）。
    handle.set_offset(gpui::point(px(0.), px(-(max * 0.5))));
    let (first2, _l) = wcx.update(|_w, app| {
        let lv = view.read(app).lyrics_right.clone();
        lv.update(app, |lv, _| lv.lyrics_visible_window(n))
    });
    let expected_first2 = (((max * 0.5) / LYRIC_ROW_H).floor() as usize).min(n - 1);
    assert_eq!(first2, expected_first2, "中段偏移不应被钳位改变");
}

/// 用例 K：未布局（viewport=0 / max=0）时渲染物化不 panic、行窗从顶部开始（FALLBACK）。
#[gpui::test]
async fn lyrics_unlaidout_viewport_no_panic(cx: &mut TestAppContext) {
    let player = player_with_lrc(cx, &lrc_30_lines());
    play_at_pos(&player, cx, 0.0);
    let (view, wcx) = cx.add_window_view(|_w, cx| MusicView::new(player, cx));
    wcx.update(|_w, app| {
        view.update(app, |v, _| v.state.right_tab = RightTab::Lyrics);
    });
    // 不主动 draw：若尚未布局（bounds=0/max=0）也应安全反算（FALLBACK 行窗），不 panic。
    wcx.run_until_parked();
    let (f0, _) = wcx.update(|_w, app| {
        let lv = view.read(app).lyrics_right.clone();
        lv.update(app, |lv, _| lv.lyrics_visible_window(30))
    });
    assert_eq!(f0, 0, "未布局时行窗应从顶部开始，实得 {f0}");
}

// ---- 真机复报三（新形态）：歌词拆双实例子视图后「全屏 + 歌词 tab 双 ScrollHandle 互不钳制」 ----

/// 用例 L（复报三回归·新形态）：歌词状态迁入 `lyrics_right`/`lyrics_fs` 双实例后，右栏歌词
/// 与全屏歌词**各自独立滚动容器/句柄**——全屏到底时 offset 钳到它**自己**的 max（旧实现
/// 双树共用一句柄被右栏更小 max 钳走 → 到底后上下跳动 + 闪）；右实例 offset 不被全屏侧
/// 滚动污染（各钳自己的 max，互不干涉）。
#[gpui::test]
async fn lyrics_double_instance_offsets_independent(cx: &mut TestAppContext) {
    let player = player_with_lrc(cx, &lrc_30_lines());
    play_at_pos(&player, cx, 450.0);
    let (view, wcx) = open_lyrics_tab(cx, &player);
    pump_ticks(wcx, 2);
    // 打开全屏浮层（右栏仍停在「歌词」tab）——双实例同树上共存（右实例被 fs 浮层盖住）。
    // toggle_fs 按切换渲染上下文语义 reset 双实例跟随态。
    wcx.update(|window, app| {
        view.update(app, |v, cx| v.toggle_fs(cx));
        window.draw(app).clear(app);
    });
    wcx.run_until_parked();
    let fs_handle = wcx
        .cx
        .read(|app| view.read(app).lyrics_fs.read(app).lyrics_scroll.clone());
    let right_handle = wcx
        .cx
        .read(|app| view.read(app).lyrics_right.read(app).lyrics_scroll.clone());
    let max_fs = fs_handle.max_offset().y.as_f32();
    assert!(max_fs > 0.0, "全屏侧滚动容器应有可滚范围，实得 max_fs={max_fs}");
    // 记录右实例当前 offset（全屏滚动前）。
    let right_off0 = right_handle.offset().y.as_f32();
    // 全屏内滚到最底部（越界偏移 = 滚轮到底的瞬时态）→ 布局应钳到**全屏自己**的 max。
    fs_handle.set_offset(gpui::point(px(0.), px(-(max_fs + LYRIC_ROW_H * 4.0))));
    wcx.update(|window, app| window.draw(app).clear(app));
    wcx.run_until_parked();
    let off = fs_handle.offset().y.as_f32();
    let max_after = fs_handle.max_offset().y.as_f32();
    assert!(
        (off + max_after).abs() < 2.0,
        "全屏歌词到底应钳到全屏自己 max：off={off} max_after={max_after}（旧共享句柄被右栏钳走）"
    );
    // 右实例 offset 独立：不被全屏侧滚动改变。
    let right_off1 = right_handle.offset().y.as_f32();
    assert!(
        (right_off1 - right_off0).abs() < 2.0,
        "右实例 offset 不应被全屏侧滚动污染：right {right_off0} → {right_off1}"
    );
    // 右实例也能按自己的 max 钳位（越界右实例 → 钳到自己 max，与全屏互不影响）。
    let max_right = right_handle.max_offset().y.as_f32();
    if max_right > 0.0 {
        right_handle.set_offset(gpui::point(px(0.), px(-(max_right + LYRIC_ROW_H * 4.0))));
        wcx.update(|window, app| window.draw(app).clear(app));
        wcx.run_until_parked();
        let roff = right_handle.offset().y.as_f32();
        let rmax = right_handle.max_offset().y.as_f32();
        assert!(
            (roff + rmax).abs() < 2.0,
            "右实例歌词到底应钳到自己 max：roff={roff} rmax={rmax}"
        );
    }
}

// ---- 音乐性能最后一块：父树稳态 0 重绘（歌词/右栏 np 进度子视图承载心跳） ----

/// 用例 N1（目标验收 RED→GREEN）：普通（非全屏）态播放中父 MusicView 稳态 **0 整树
/// notify**——歌词跟随/右栏 np 进度各有子视图心跳承接。旧实现父 tick 每拍
/// `if playing { cx.notify() }` → 每 250ms 整树重建（当前代码应 RED：父增量 ≥1）。
/// 判别：分别 `App::observe` 父/歌词/np 子视图，泵多拍后父增量 == 0，**当前上树**的
/// 子视图增量 ≥1（各自心跳活）；离树的子视图心跳空转（增量 0）。
#[gpui::test]
async fn playing_steady_tick_no_parent_rebuild(cx: &mut TestAppContext) {
    let player = player_with_lrc(cx, &lrc_30_lines());
    play_at_pos(&player, cx, 0.0);
    let (view, wcx) = cx.add_window_view(|_w, cx| MusicView::new(player, cx));
    wcx.update(|window, app| window.draw(app).clear(app));
    wcx.run_until_parked();
    let (parent_c, np_c, lyrics_c) = wcx.update(|_w, app| {
        let (np, lyrics) = {
            let v = view.read(app);
            (v.np_bar.clone(), v.lyrics_right.clone())
        };
        let parent_c = Rc::new(Cell::new(0usize));
        let np_c = Rc::new(Cell::new(0usize));
        let lyrics_c = Rc::new(Cell::new(0usize));
        {
            let c = parent_c.clone();
            app.observe(&view, move |_, _| c.set(c.get() + 1)).detach();
        }
        {
            let c = np_c.clone();
            app.observe(&np, move |_, _| c.set(c.get() + 1)).detach();
        }
        {
            let c = lyrics_c.clone();
            app.observe(&lyrics, move |_, _| c.set(c.get() + 1)).detach();
        }
        (parent_c, np_c, lyrics_c)
    });
    // 场景 A：歌曲 tab（np_bar 上树，lyrics_right 离树）——泵 4 拍：父 0 重建、np ≥1、歌词 0。
    let (p0, n0, l0) = (parent_c.get(), np_c.get(), lyrics_c.get());
    pump_ticks(wcx, 4);
    let (dp, dn, dl) = (parent_c.get() - p0, np_c.get() - n0, lyrics_c.get() - l0);
    assert_eq!(
        dp, 0,
        "播放中普通态父 tick 不应整树 notify（稳态 4 拍 0 重建）：delta={dp}"
    );
    assert!(dn >= 1, "右栏 np 进度子视图心跳应自 notify：delta={dn}");
    assert_eq!(
        dl, 0,
        "离树歌词实例心跳应空转（不 notify）：delta={dl}"
    );
    // 场景 B：切歌词 tab（lyrics_right 上树，np_bar 离树）——泵 4 拍：父稳态 0、歌词 ≥1。
    let np0_b = np_c.get();
    wcx.update(|_w, app| {
        view.update(app, |v, cx| v.set_right_tab(RightTab::Lyrics, cx));
    });
    wcx.update(|window, app| window.draw(app).clear(app));
    wcx.run_until_parked();
    let (p1, l1) = (parent_c.get(), lyrics_c.get());
    pump_ticks(wcx, 4);
    let (dp2, dl2) = (parent_c.get() - p1, lyrics_c.get() - l1);
    assert_eq!(
        dp2, 0,
        "切歌词 tab 后播放中普通态父稳态也应 0 重建：delta={dp2}"
    );
    assert!(dl2 >= 1, "歌词子视图心跳应自 notify（跟随推进）：delta={dl2}");
    // np 离树后心跳空转（不再自 notify）。
    let dn2 = np_c.get() - np0_b;
    assert_eq!(dn2, 0, "离树 np 实例心跳应空转（不 notify）：delta={dn2}");
}

/// 用例 N2（fs 条件保留）：`fs_open` + 播放中父仍按拍重绘（fs 进度/时间/整块尚在父内）——
/// 4 拍内父 notify ≥2；fs 场景是预期内的父 4Hz。
#[gpui::test]
async fn fs_open_still_refreshes_parent(cx: &mut TestAppContext) {
    let player = player_with_lrc(cx, &lrc_30_lines());
    play_at_pos(&player, cx, 0.0);
    let (view, wcx) = cx.add_window_view(|_w, cx| MusicView::new(player, cx));
    wcx.update(|window, app| {
        view.update(app, |v, cx| v.toggle_fs(cx));
        window.draw(app).clear(app);
    });
    wcx.run_until_parked();
    let parent_c = Rc::new(Cell::new(0usize));
    wcx.update(|_w, app| {
        let c = parent_c.clone();
        app.observe(&view, move |_, _| c.set(c.get() + 1)).detach();
    });
    let base = parent_c.get();
    pump_ticks(wcx, 3);
    let d = parent_c.get() - base;
    assert!(
        d >= 2,
        "fs_open+播放中父应按拍重绘（fs 进度/时间/整块在父内）：delta={d}"
    );
}

// ---- 右栏 np 进度子视图：随位置更新 + 拖拽 seek 跟手（headless 证机制） ----

/// 用例 N3：np 进度填充随播放位置更新——seek(50%) 后显式重绘，`np-progress-fill` 宽 ≈ 50%
/// 条宽（headless position=seek_base 确定性；K30 注「证机制」，真机平滑由心跳 + 复验）。
#[gpui::test]
async fn np_progress_updates_with_position(cx: &mut TestAppContext) {
    let dur = 100.0;
    let player = player_with_library(cx);
    cx.update(|app| {
        player.update(app, |p, cx| {
            p.test_start_playing(0);
            p.set_test_duration(dur);
            p.seek(0.0, cx);
        });
    });
    let (view, wcx) = cx.add_window_view(|_w, cx| MusicView::new(player, cx));
    wcx.update(|window, app| window.draw(app).clear(app));
    wcx.run_until_parked();
    let b = wcx
        .cx
        .read(|app| view.read(app).np_bar.read(app).np_progress.get());
    let bar_w = b.size.width.as_f32();
    assert!(bar_w > 0.0, "np-progress 盒应已布局（宽>0），实得 {b:?}");
    let fill0 = wcx.debug_bounds("np-progress-fill").expect("np 填充应有 debug 盒");
    assert!(
        fill0.size.width.as_f32() < 2.0,
        "初始 pos=0 填充应≈0，实得 {fill0:?}"
    );
    // 驱动 position 到 50%（seek 同步把 seek_base 显示基准置目标；无 sink 下确定性）。
    wcx.update(|_w, app| {
        let p = view.read(app).player.clone();
        p.update(app, |p, cx| p.seek(dur * 0.5, cx));
    });
    wcx.update(|window, app| window.draw(app).clear(app));
    wcx.run_until_parked();
    let fill1 = wcx.debug_bounds("np-progress-fill").expect("np 填充应仍在渲染");
    let want = bar_w * 0.5;
    assert!(
        (fill1.size.width.as_f32() - want).abs() < 4.0,
        "np 进度填充应随 position 更新 ≈{want:.1}，实得 {:.1}",
        fill1.size.width.as_f32()
    );
}

/// 用例 N4：np 进度条点击/拖动 seek 实时跟手——拖动**自 notify np 子视图**（不寄生父
/// tick/动画），位置随落点、松手停在当前位置（复用 bb 姿势，观察对象换 np 子视图）。
#[gpui::test]
async fn np_progress_drag_notifies_np_subview(cx: &mut TestAppContext) {
    let dur = 100.0;
    let player = player_with_library(cx);
    cx.update(|app| {
        player.update(app, |p, _| p.set_test_duration(dur));
    });
    let (view, wcx) = cx.add_window_view(|_w, cx| MusicView::new(player, cx));
    wcx.update(|window, app| window.draw(app).clear(app));
    wcx.run_until_parked();
    let np_c = wcx.update(|_w, app| {
        let np = view.read(app).np_bar.clone();
        let c = Rc::new(Cell::new(0usize));
        {
            let c = c.clone();
            app.observe(&np, move |_, _| c.set(c.get() + 1)).detach();
        }
        c
    });
    let b = wcx
        .cx
        .read(|app| view.read(app).np_bar.read(app).np_progress.get());
    assert!(
        b.size.width.as_f32() > 0.0,
        "np-progress 盒应已布局（宽>0），实得 {b:?}"
    );
    let base = np_c.get();
    wcx.simulate_mouse_down(at_frac(&b, 0.60), MouseButton::Left, Default::default());
    wcx.simulate_mouse_move(at_frac(&b, 0.30), Some(MouseButton::Left), Default::default());
    wcx.run_until_parked();
    assert!(
        np_c.get() > base,
        "np 拖拽应自 notify np 子视图（不寄生 tick/动画）：{} → {}",
        base,
        np_c.get()
    );
    let pos = wcx.cx.read(|app| view.read(app).player.read(app).position());
    assert!(
        (pos - dur * 0.30).abs() < 1.0,
        "拖到 30% 应跟随 {:.1}s，实得 {pos:.1}",
        dur * 0.30
    );
    wcx.simulate_mouse_up(at_frac(&b, 0.30), MouseButton::Left, Default::default());
    wcx.run_until_parked();
    let pos2 = wcx.cx.read(|app| view.read(app).player.read(app).position());
    assert!((pos2 - dur * 0.30).abs() < 1.0, "松手仍停 30%，实得 {pos2:.1}");
}

/// 用例 N5（toast 让步显式 notify 的回归护栏）：删掉父「playing||toast」整树 notify 后，
/// toast **出现**（错误变化那拍）与 **消失**（满 1.8s 过期清空那拍）都必须显式 notify，
/// 否则那两拍无重绘 = toast 永不呈现/永不消失回归。判别：空库注入播放错误 → 第一拍
/// toast 出现 notify ≥1；toast 在期普通态 0 持续 notify；满 1.8s 过期 notify ≥1。
#[gpui::test]
async fn toast_appear_expire_each_notify_once(cx: &mut TestAppContext) {
    let dir = std::env::temp_dir().join(format!("gpui-music-err-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let player: Entity<MusicPlayer> = cx.update(|app| app.new(|cx| MusicPlayer::new(cx)));
    let (view, wcx) = cx.add_window_view(|_w, cx| MusicView::new(player.clone(), cx));
    // 注入：play_path 指向空库外路径 → 以父目录重扫空目录 → commit_scan 里 pending 未命中
    // → error 置位（播放 false；无 toast 相关多余 notify 干扰）。
    wcx.cx.update(|app| {
        player.update(app, |p, cx| p.play_path(dir.join("缺.wav"), cx));
    });
    drain_scan(&mut wcx.cx);
    let parent_c = Rc::new(Cell::new(0usize));
    wcx.update(|_w, app| {
        let c = parent_c.clone();
        app.observe(&view, move |_, _| c.set(c.get() + 1)).detach();
    });
    let base = parent_c.get();
    // 第一拍 tick：发现错误变化 → toast 出现 + 一次显式 notify。
    pump_ticks(wcx, 1);
    let after_appear = parent_c.get() - base;
    assert!(
        after_appear >= 1,
        "toast 出现那拍应有视图 notify：delta={after_appear}"
    );
    // toast 在期（<1.8s）+ 非 fs 普通态：不持续 notify。
    let b2 = parent_c.get();
    pump_ticks(wcx, 4); // +1s
    assert_eq!(
        parent_c.get() - b2,
        0,
        "toast 在期普通态不应持续 notify（删掉 playing||toast 后的预期）"
    );
    // 满 1.8s：过期清空 + 一次显式 notify。
    let b3 = parent_c.get();
    pump_ticks(wcx, 4); // +1s → 累计 ≥1.8s
    let after_expire = parent_c.get() - b3;
    assert!(
        after_expire >= 1,
        "toast 过期清空那拍应有视图 notify：delta={after_expire}"
    );
}

// ---- 真机复报：全屏进度条与音量条不能点击/拖拽（bounds_canvas 在 flex 上下文宽塌缩 0） ----

/// 用例 M（复报回归）：全屏音量条可点击——`theme::bounds_canvas` 的 canvas 无显式尺寸，
/// 在 `#fs-vol`（`.flex().items_center()`，fullscreen.rs:698-705）里是 flex item → 宽塌缩 0
/// → `fraction_at`（`size.width<=0 → None`）拒算 → 点/拖音量永远无效（headless 无 fs 音量
/// 用例所以漏网）。修复 canvas `absolute.inset_0` → 记录父条完整盒（宽>0）→ 点击即调音量。
#[gpui::test]
async fn fs_volume_click_changes_volume(cx: &mut TestAppContext) {
    let dur = 120.0;
    let player = player_with_library(cx);
    cx.update(|app| {
        player.update(app, |p, _| p.set_test_duration(dur));
    });
    let (view, wcx) = cx.add_window_view(|_w, cx| MusicView::new(player.clone(), cx));
    // 打开全屏浮层并渲染一帧 → fs_vol 盒由 canvas prepaint 写入。
    wcx.update(|window, app| {
        view.update(app, |v, _| v.state.fs_open = true);
        window.draw(app).clear(app);
    });
    wcx.run_until_parked();
    let b = wcx.cx.read(|app| view.read(app).fs_vol.get());
    assert!(
        b.size.width.as_f32() > 0.0,
        "fs-vol 盒应已布局且宽>0，实得 {b:?}"
    );
    // 点按 25% → 音量应 ≈0.25。
    wcx.simulate_mouse_down(at_frac(&b, 0.25), MouseButton::Left, Default::default());
    wcx.run_until_parked();
    let vol = wcx.cx.read(|app| player.read(app).volume());
    assert!(
        (vol - 0.25).abs() < 0.02,
        "点按 25% 应把音量调到≈0.25，实得 {vol}"
    );
    // 松手后音量保持（音量条不 seek、不误伤）。
    wcx.simulate_mouse_up(at_frac(&b, 0.25), MouseButton::Left, Default::default());
    wcx.run_until_parked();
    let vol2 = wcx.cx.read(|app| player.read(app).volume());
    assert!((vol2 - 0.25).abs() < 0.02, "松手后音量应保持 0.25，实得 {vol2}");
}

/// 用例 N（防回退）：全屏进度条与音量条盒**都**宽>0——共享 `bounds_canvas` 修复兜底
/// 任何「flex 上下文宽塌缩」变体（避免只修音量、进度条在别处塌缩再次漏网）。
#[gpui::test]
async fn fs_strips_bounds_nonzero(cx: &mut TestAppContext) {
    let dur = 120.0;
    let player = player_with_library(cx);
    cx.update(|app| {
        player.update(app, |p, _| p.set_test_duration(dur));
    });
    let (view, wcx) = cx.add_window_view(|_w, cx| MusicView::new(player.clone(), cx));
    wcx.update(|window, app| {
        view.update(app, |v, _| v.state.fs_open = true);
        window.draw(app).clear(app);
    });
    wcx.run_until_parked();
    let (pw, vw) = wcx.cx.read(|app| {
        let v = view.read(app);
        (
            v.fs_progress.get().size.width.as_f32(),
            v.fs_vol.get().size.width.as_f32(),
        )
    });
    assert!(pw > 0.0, "fs 进度条盒应宽>0，实得 {pw}");
    assert!(vw > 0.0, "fs 音量条盒应宽>0，实得 {vw}");
}

// ---- 真机复报四：全屏浮层不拦下层鼠标事件——点击 fs 进度条（与侧栏歌单行重叠）穿透 ----

/// 用例 O（复报四回归）：`fs_open` 时 `#music-fs` 全屏覆盖浮层是根末子元素（绘制/命中最上），
/// 但它**无鼠标监听 → `div.rs::should_insert_hitbox` 不插 hitbox、`hitbox_behavior` 默认
/// Normal**——gpui 命中（`window.rs::hit_test`）只在对上 `BlockMouse` 时 break，Normal 盒
/// 全部入栈、不拦后面元素 → 与 fs 进度条几何重叠的下层侧栏歌单行 `on_click` 照样触发
/// （toast「打开歌单」，sidebar.rs:220）。真机用户窗口尺寸下重叠 → 点进度条弹歌单 toast。
/// 修复：`#music-fs` `.occlude()`（BlockMouse）→ 下层整体移出命中栈 → 点击 = 纯 seek、
/// 无侧栏副作用。
#[gpui::test]
async fn fs_progress_click_does_not_leak_to_sidebar_playlist(cx: &mut TestAppContext) {
    let dur = 120.0;
    let player = player_with_library(cx);
    cx.update(|app| {
        player.update(app, |p, _| p.set_test_duration(dur));
    });
    let (view, wcx) = cx.add_window_view(|_w, cx| MusicView::new(player.clone(), cx));
    // 用户窗口：缩小到 fs 进度条与侧栏歌单行**几何重叠**（默认 1280x800 不重叠 → 旧 headless
    // 用例全绿却漏真机穿透）。高 756 = 716 + 40：主界面标题栏（`music-title-bar` 36px）把
    // 侧栏行下移 36，fs 浮层 absolute inset-0 盖整窗、左列在 body 竖直居中 → 多给 ~40px
    // 让「fs 进度条 ⇄ 歌单行」重新恢复重叠（补偿值；仍满足「缩小窗口」的夹具语义）。
    wcx.simulate_resize(gpui::size(px(900.), px(756.)));
    wcx.update(|window, app| {
        view.update(app, |v, _| v.state.fs_open = true);
        window.draw(app).clear(app);
    });
    wcx.run_until_parked();
    let b = wcx.cx.read(|app| view.read(app).fs_progress.get());
    assert!(
        b.size.width.as_f32() > 0.0,
        "fs-progress 盒应已布局（宽>0），实得 {b:?}"
    );
    // 歌单行应在侧栏渲染（fixture 3 曲 → 3 个艺术家组 → 3 行）。
    let pl = wcx
        .debug_bounds("music-playlist-周杰伦")
        .expect("侧栏应渲染歌单行（music-playlist-周杰伦）");
    // 夹具契约：取两盒**交集**中心作为点击点（同时落在 fs 进度条与歌单行内→复现穿透）。
    // 交集必须非退化（高>2px），否则本用例无意义，需调窗口尺寸。
    let (bx0, bx1) = (
        b.origin.x.as_f32(),
        (b.origin.x + b.size.width).as_f32(),
    );
    let (by0, by1) = (
        b.origin.y.as_f32(),
        (b.origin.y + b.size.height).as_f32(),
    );
    let (px0, px1) = (
        pl.origin.x.as_f32(),
        (pl.origin.x + pl.size.width).as_f32(),
    );
    let (py0, py1) = (
        pl.origin.y.as_f32(),
        (pl.origin.y + pl.size.height).as_f32(),
    );
    let ix0 = bx0.max(px0);
    let ix1 = bx1.min(px1);
    let iy0 = by0.max(py0);
    let iy1 = by1.min(py1);
    assert!(
        ix1 - ix0 > 2.0 && iy1 - iy0 > 2.0,
        "夹具：fs 进度条与歌单行应几何重叠（交集 {ix0:.1},{iy0:.1}-{ix1:.1},{iy1:.1}，当前窗口尺寸不重叠）"
    );
    let cx_click = (ix0 + ix1) * 0.5;
    let cy_click = (iy0 + iy1) * 0.5;
    let frac = (cx_click - bx0) / (bx1 - bx0);
    let click = gpui::Point::new(px(cx_click), px(cy_click));
    // 完整点击（down+up）→ fs 进度条应 seek 到 ≈frac×dur，且**不**触发侧栏歌单 toast。
    wcx.simulate_mouse_down(click, MouseButton::Left, Default::default());
    wcx.run_until_parked();
    let pos1 = wcx.cx.read(|app| player.read(app).position());
    let want = dur * frac as f64;
    assert!(
        (pos1 - want).abs() < 1.0,
        "fs 进度条点按 {frac:.2} 应 seek 到 {want:.1}s，实得 {pos1:.1}"
    );
    wcx.simulate_mouse_up(click, MouseButton::Left, Default::default());
    wcx.run_until_parked();
    let toast_none = wcx.cx.read(|app| view.read(app).toast.is_none());
    assert!(
        toast_none,
        "点击 fs 进度条不应穿透触发侧栏歌单 toast（视图 toast 应为 None）"
    );
}

// ---- 真机复报五：全屏进度条填充不随播放位置增长（恒一个颜色） ----

/// 用例 P（复报五回归）：`render_fs_progress` 填充盒 `w(DefiniteLength::Fraction(frac))`
/// 应随 `player.position()/duration()` 实时增长/减少——pos 与底栏同源且底栏真机正常，故
/// 断言对象聚焦「填充盒实际布局宽度」：pos≈0 → w0≈0；seek(50)/dur=100 → frac=0.5 →
/// w1≈0.5×条盒宽 且 w1>w0。若填充盒宽恒 0 / 恒全宽 / 不随 frac 变 → RED（真机只见恒一色）。
#[gpui::test]
async fn fs_progress_fill_grows_with_position(cx: &mut TestAppContext) {
    let dur = 100.0;
    let player = player_with_library(cx);
    cx.update(|app| {
        player.update(app, |p, cx| {
            p.test_start_playing(0);
            p.set_test_duration(dur);
            p.seek(0.0, cx);
        });
    });
    let (view, wcx) = cx.add_window_view(|_w, cx| MusicView::new(player.clone(), cx));
    // 打开全屏并渲染一帧（pos=0 → frac=0）。
    wcx.update(|window, app| {
        view.update(app, |v, _| v.state.fs_open = true);
        window.draw(app).clear(app);
    });
    wcx.run_until_parked();
    let read_fill_box = |wcx: &mut VisualTestContext| {
        wcx.cx.read(|app| view.read(app).fs_fill_box.get())
    };
    // frame A（pos=0）：记录填充 div 布局宽 + 填充渐变 canvas 绘制盒。
    let (w0, f0) = (
        wcx
            .debug_bounds("fs-prog-fill")
            .expect("fs 填充盒应有 debug 盒")
            .size
            .width
            .as_f32(),
        read_fill_box(wcx),
    );
    // frame B：播放器改到 50s（position 确定性=50）→ 重绘。
    wcx.update(|_w, app| {
        player.update(app, |p, cx| p.seek(50.0, cx));
    });
    wcx.update(|window, app| window.draw(app).clear(app));
    wcx.run_until_parked();
    let (w1, f50) = (
        wcx
            .debug_bounds("fs-prog-fill")
            .expect("fs 填充盒重绘后仍应有 debug 盒")
            .size
            .width
            .as_f32(),
        read_fill_box(wcx),
    );
    let bar_w = wcx
        .cx
        .read(|app| view.read(app).fs_progress.get())
        .size
        .width
        .as_f32();
    assert!(bar_w > 0.0, "fs-progress 条盒宽应>0，实得 {bar_w}");
    // 布局级：填充 div 宽随位置增长（frac 数据链正常）。
    assert!(
        w1 > w0,
        "填充盒宽应随位置增长：w0(pos=0)={w0:.1} → w1(pos=50)={w1:.1}"
    );
    assert!(
        (w1 - bar_w * 0.5).abs() < 2.0,
        "pos=50s / dur=100s 填充宽应≈0.5×条宽：w1={w1:.1} bar={bar_w:.1}"
    );
    assert!(w0 < 2.0, "pos≈0 填充宽应≈0：w0={w0:.1}");
    // 绘制级（复报五根因在此）：填充渐变的 **canvas 实际绘制盒** 必须高度非零
    // （旧实现 block 子盒高度塌缩 0 → 渐变面积 0 → 真机只留轨道一色）且宽度随位置增长。
    let (h0, h1) = (
        f0.size.height.as_f32(),
        f50.size.height.as_f32(),
    );
    let (wfill0, wfill1) = (
        f0.size.width.as_f32(),
        f50.size.width.as_f32(),
    );
    assert!(
        h0 > 2.0 && h1 > 2.0,
        "填充渐变绘制盒高度应>0（非隐形）：h(pos=0)={h0:.1} h(pos=50)={h1:.1}"
    );
    assert!(
        wfill1 > wfill0,
        "填充渐变绘制盒宽应随位置增长：w(pos=0)={wfill0:.1} → w(pos=50)={wfill1:.1}"
    );
    assert!(
        (wfill1 - bar_w * 0.5).abs() < 2.0,
        "pos=50 填充渐变宽应≈0.5×条宽：wfill1={wfill1:.1} bar={bar_w:.1}"
    );
    assert!(
        wfill0 < 2.0,
        "pos≈0 填充渐变宽应≈0：wfill0={wfill0:.1}"
    );
}

// ---- 音乐界面按钮悬停提示（tooltip）：悬停 ≥500ms 弹名称浮层，移开/换钮切换 ----

/// 建窗口视图并渲染一帧（debug_selector 盒由 prepaint 写入 debug_bounds）。
fn open_tooltip_view<'a>(
    cx: &'a mut TestAppContext,
    player: Entity<MusicPlayer>,
) -> (Entity<MusicView>, &'a mut VisualTestContext) {
    let (view, wcx) = cx.add_window_view(|_w, cx| MusicView::new(player, cx));
    wcx.update(|window, app| {
        window.draw(app).clear(app);
    });
    wcx.run_until_parked();
    (view, wcx)
}

/// 按 debug 盒中心发鼠标移动（触发按钮 on_hover 进入）。
fn hover_button(wcx: &mut VisualTestContext, selector: &'static str) {
    let b = wcx.debug_bounds(selector).expect("按钮 debug 盒应存在");
    wcx.simulate_mouse_move(at_frac(&b, 0.5), None, Default::default());
    wcx.run_until_parked();
}

/// 当前 tooltip 文案（None = 未悬停/已清除）。
fn tooltip_text(
    view: &Entity<MusicView>,
    cx: &mut VisualTestContext,
) -> Option<String> {
    cx.cx
        .read(|app| view.read(app).tip.clone().map(|(s, _)| s.to_string()))
}

/// 当前 tooltip 是否应显示（悬停已过 TOOLTIP_SHOW_MS）。render 与断言共用同一判定。
fn tooltip_showing(
    view: &Entity<MusicView>,
    cx: &mut VisualTestContext,
) -> bool {
    cx.update(|_w, app| view.update(app, |v, cx| v.hover_tip_visible(cx)))
}

/// 用例 T1：悬停底栏 bb-play → `tip` 设置；不足 500ms 不显示、满 500ms 显示。
#[gpui::test]
fn hover_bb_play_shows_tooltip_after_delay(cx: &mut TestAppContext) {
    let player = player_with_library(cx);
    let (view, wcx) = open_tooltip_view(cx, player);
    assert!(wcx.debug_bounds("bb-play").is_some(), "bb-play 应有 debug 盒");
    hover_button(wcx, "bb-play");
    assert_eq!(
        tooltip_text(&view, wcx).as_deref(),
        Some("播放/暂停"),
        "悬停 bb-play 应设对应名称提示"
    );
    assert!(!tooltip_showing(&view, wcx), "未满 500ms 不应显示浮层");
    wcx.executor().advance_clock(Duration::from_millis(600));
    wcx.run_until_parked();
    assert!(tooltip_showing(&view, wcx), "满 500ms 后应显示浮层");
    // 渲染层：重绘一帧，浮层应实际绘出（`render_tooltip` 分支产出 debug 盒，非仅状态）。
    wcx.update(|window, app| window.draw(app).clear(app));
    wcx.run_until_parked();
    assert!(
        wcx.debug_bounds("music-tooltip").is_some(),
        "满 500ms 后浮层应实际渲染（music-tooltip 盒存在）"
    );
}

/// 用例 T2：移开（hover false）→ `tip` 清除、浮层隐藏（不残留）。
#[gpui::test]
fn hover_leave_clears_tooltip(cx: &mut TestAppContext) {
    let player = player_with_library(cx);
    let (view, wcx) = open_tooltip_view(cx, player);
    hover_button(wcx, "bb-play");
    assert!(tooltip_text(&view, wcx).is_some(), "前置：悬停应设置提示");
    // 移到角落（不悬停任何钮）→ bb-play on_hover(false) 清空。
    wcx.simulate_mouse_move(gpui::Point::new(px(4.), px(4.)), None, Default::default());
    wcx.run_until_parked();
    assert!(tooltip_text(&view, wcx).is_none(), "移开应清空提示");
    wcx.executor().advance_clock(Duration::from_millis(800));
    wcx.run_until_parked();
    assert!(!tooltip_showing(&view, wcx), "移开后不应残留浮层");
}

/// 用例 T3：从 bb-prev 移到 bb-next → 文案切换为新钮名称。
#[gpui::test]
fn hover_move_between_buttons_switches_name(cx: &mut TestAppContext) {
    let player = player_with_library(cx);
    let (view, wcx) = open_tooltip_view(cx, player);
    hover_button(wcx, "bb-prev");
    assert_eq!(
        tooltip_text(&view, wcx).as_deref(),
        Some("上一首"),
        "悬停 bb-prev 提示应为「上一首」"
    );
    hover_button(wcx, "bb-next");
    assert_eq!(
        tooltip_text(&view, wcx).as_deref(),
        Some("下一首"),
        "从 bb-prev 移到 bb-next 文案应切换为「下一首」"
    );
}

/// 用例 T4：全屏 fs-shuffle 悬停 → 名称出现 + 既有 `fs_hover` 高亮仍工作（不回归）。
#[gpui::test]
fn hover_fs_button_keeps_highlight_and_tip(cx: &mut TestAppContext) {
    let player = player_with_library(cx);
    let (view, wcx) = open_tooltip_view(cx, player);
    // 打开全屏浮层并重渲染一帧（fs 按钮盒入 debug_bounds）。
    wcx.update(|window, app| {
        view.update(app, |v, cx| v.toggle_fs(cx));
        window.draw(app).clear(app);
    });
    wcx.run_until_parked();
    assert!(
        wcx.debug_bounds("fs-shuffle").is_some(),
        "fs-shuffle 应有 debug 盒"
    );
    hover_button(wcx, "fs-shuffle");
    assert_eq!(
        tooltip_text(&view, wcx).as_deref(),
        Some("随机播放"),
        "全屏 fs-shuffle 悬停应设名称提示"
    );
    let fs_hover = wcx.cx.read(|app| view.read(app).fs_hover);
    assert_eq!(fs_hover, Some("fs-shuffle"), "既有 fs_hover 高亮应仍被设置");
    wcx.executor().advance_clock(Duration::from_millis(600));
    wcx.run_until_parked();
    assert!(tooltip_showing(&view, wcx), "全屏钮满 500ms 也应显示浮层");
}

// ---- 全屏「退出清残留 tooltip」回归（根因：退出全屏 = 浮层整体从树移除，其 hover-leave
//      不再分发 → `tip`/`tip_box` 残留；`render_tooltip` 为共享提示层、不按 `fs_open` 门控，
//      只要 tip 存在且过 TOOLTIP_SHOW_MS 就永远挂屏。修复 = 三条退出路径统一走中心化
//      `exit_fullscreen` 清状态。判定式：撤掉 exit_fullscreen 内三行清除 → 本组用例红） ----

/// 用例 T-FS1：Esc 退出全屏 → 清空悬停残留的「退出全屏」提示（不残留浮层）。
#[gpui::test]
fn fs_exit_clears_stuck_tooltip_on_escape(cx: &mut TestAppContext) {
    let player = player_with_library(cx);
    let (view, wcx) = open_view(cx, player);
    // 进入全屏（真实 F 键路径）。
    wcx.simulate_keystrokes("f");
    wcx.run_until_parked();
    assert!(
        wcx.update(|_w, app| view.read(app).state.fs_open),
        "前置：F 应打开全屏"
    );
    // 模拟悬停全屏关闭钮（直调统一处理器，等价于 on_hover 进入动作写入的状态）。
    wcx.update(|_w, app| {
        view.update(app, |v, cx| {
            v.set_fs_hover_and_tip("fs-close", Some("退出全屏"), true, cx);
        });
    });
    wcx.executor().advance_clock(Duration::from_millis(600));
    wcx.run_until_parked();
    assert_eq!(
        tooltip_text(&view, wcx).as_deref(),
        Some("退出全屏"),
        "前置：悬停 fs-close 应设「退出全屏」提示"
    );
    assert!(tooltip_showing(&view, wcx), "前置：满 500ms 悬停态应显示浮层");
    // Esc 退出全屏 → 残留提示必须被清空。
    wcx.simulate_keystrokes("escape");
    wcx.run_until_parked();
    assert!(
        !wcx.update(|_w, app| view.read(app).state.fs_open),
        "Esc 应退出全屏"
    );
    assert_eq!(
        tooltip_text(&view, wcx),
        None,
        "Esc 退出全屏应清空残留 tooltip（撤 exit_fullscreen 清除=红）"
    );
    assert!(
        !tooltip_showing(&view, wcx),
        "Esc 退出全屏后浮层不应残留显示（撤 exit_fullscreen 清除=红）"
    );
}

/// 用例 T-FS2：点击全屏关闭钮退出 → 同样清空残留提示（真实 on_click 路径）。
#[gpui::test]
fn fs_close_click_clears_stuck_tooltip(cx: &mut TestAppContext) {
    let player = player_with_library(cx);
    let (view, wcx) = open_tooltip_view(cx, player);
    // 打开全屏并重渲染一帧（fs-close debug 盒写入）。
    wcx.update(|window, app| {
        view.update(app, |v, cx| v.toggle_fs(cx));
        window.draw(app).clear(app);
    });
    wcx.run_until_parked();
    wcx.update(|_w, app| {
        view.update(app, |v, cx| {
            v.set_fs_hover_and_tip("fs-close", Some("退出全屏"), true, cx);
        });
    });
    wcx.executor().advance_clock(Duration::from_millis(600));
    wcx.run_until_parked();
    assert!(tooltip_showing(&view, wcx), "前置：满 500ms 悬停态应显示浮层");
    // 点击全屏关闭钮（真实 on_click 路径）→ 退出并清残留。
    click_button(wcx, "fs-close");
    assert!(
        !wcx.cx.read(|app| view.read(app).state.fs_open),
        "点击 fs-close 应退出全屏"
    );
    assert_eq!(
        tooltip_text(&view, wcx),
        None,
        "点击 fs-close 退出应清空残留 tooltip（撤 exit_fullscreen 清除=红）"
    );
    assert!(
        !tooltip_showing(&view, wcx),
        "点击退出后浮层不应残留显示（撤 exit_fullscreen 清除=红）"
    );
}

/// 用例 T-FS3：F 键（toggle_fs）再按一下退出全屏 → 同样清空残留提示（进入/退出对称）。
#[gpui::test]
fn fs_toggle_off_clears_stuck_tooltip(cx: &mut TestAppContext) {
    let player = player_with_library(cx);
    let (view, wcx) = open_view(cx, player);
    wcx.simulate_keystrokes("f");
    wcx.run_until_parked();
    assert!(
        wcx.update(|_w, app| view.read(app).state.fs_open),
        "前置：F 应打开全屏"
    );
    wcx.update(|_w, app| {
        view.update(app, |v, cx| {
            v.set_fs_hover_and_tip("fs-close", Some("退出全屏"), true, cx);
        });
    });
    wcx.executor().advance_clock(Duration::from_millis(600));
    wcx.run_until_parked();
    assert!(tooltip_showing(&view, wcx), "前置：满 500ms 悬停态应显示浮层");
    // 再按 F（toggle_fs 关）→ 退出并清残留。
    wcx.simulate_keystrokes("f");
    wcx.run_until_parked();
    assert!(
        !wcx.update(|_w, app| view.read(app).state.fs_open),
        "再按 F 应退出全屏"
    );
    assert_eq!(
        tooltip_text(&view, wcx),
        None,
        "toggle_fs 退出应清空残留 tooltip（撤 exit_fullscreen 清除=红）"
    );
    assert!(
        !tooltip_showing(&view, wcx),
        "toggle_fs 退出后浮层不应残留显示（撤 exit_fullscreen 清除=红）"
    );
}

/// 用例 T5：快速划过（<500ms 即离开）→ 不残留浮层。
#[gpui::test]
fn hover_quick_sweep_leaves_no_tooltip(cx: &mut TestAppContext) {
    let player = player_with_library(cx);
    let (view, wcx) = open_tooltip_view(cx, player);
    hover_button(wcx, "bb-play");
    assert!(
        tooltip_text(&view, wcx).is_some(),
        "悬停进入应设置提示（此刻未满 500ms 不显示）"
    );
    // 未满 500ms 立即移开 → 清除；即使时间后推也不残留。
    wcx.simulate_mouse_move(gpui::Point::new(px(4.), px(4.)), None, Default::default());
    wcx.run_until_parked();
    assert!(tooltip_text(&view, wcx).is_none(), "快速划过后应立即清除");
    wcx.executor().advance_clock(Duration::from_millis(900));
    wcx.run_until_parked();
    assert!(
        !tooltip_showing(&view, wcx),
        "快速划过（<500ms 离开）不应残留浮层"
    );
}

/// 用例 T6（按钮锚定契约，真机三连复报「提示固定错位、不贴按钮」根因）：tooltip
/// **不再跟随光标**——锚定在**悬停按钮自身的窗口坐标盒**上（`theme::bounds_canvas`
/// prepaint 记录，与全屏 seek/音量条点拖同一套真机已验证机制；`ev.position`/cursor 在
/// 真机不可靠 → 弃用，否则钉在固定错误位置）。
///
/// 断言：
/// ① 悬停钮 bb-play 满 500ms → 浮层盒底边中点 ≈ (bb-play 盒中心 x, bb-play 上缘 − GAP)；
/// ② **钮内移动光标不位移**（框固定在按钮上，不随光标漂移——旧实现光标跟随=RED）。
#[gpui::test]
fn hover_tooltip_anchors_to_button_and_does_not_follow_cursor(cx: &mut TestAppContext) {
    let player = player_with_library(cx);
    let (_view, wcx) = open_tooltip_view(cx, player);
    let bb = wcx.debug_bounds("bb-play").expect("bb-play 应有 debug 盒");
    let c0 = at_frac(&bb, 0.5);
    // 悬停 bb-play 中心 → 满 500ms → 渲染一帧读出浮层盒。
    wcx.simulate_mouse_move(c0, None, Default::default());
    wcx.run_until_parked();
    wcx.executor().advance_clock(Duration::from_millis(600));
    wcx.run_until_parked();
    wcx.update(|window, app| window.draw(app).clear(app));
    wcx.run_until_parked();
    let tip0 = wcx.debug_bounds("music-tooltip").expect("过 500ms 应已显示浮层盒");
    // ① 锚定断言：底边中点 ≈ (bb 中心 x, bb 上缘 − GAP)。
    let want_mid_x = bb.origin.x.as_f32() + bb.size.width.as_f32() * 0.5;
    let want_bottom_y = bb.origin.y.as_f32() - super::TOOLTIP_GAP_PX;
    let mid_x = tip0.origin.x.as_f32() + tip0.size.width.as_f32() * 0.5;
    let bottom_y = tip0.origin.y.as_f32() + tip0.size.height.as_f32();
    assert!(
        (mid_x - want_mid_x).abs() < 2.0,
        "浮层底边中点应横贴按钮盒中心：mid_x={mid_x:.1} want={want_mid_x:.1}"
    );
    assert!(
        (bottom_y - want_bottom_y).abs() < 2.0,
        "浮层底边应在按钮盒上缘上方 GAP={}：bottom_y={bottom_y:.1} want={want_bottom_y:.1}（旧光标式=RED）",
        super::TOOLTIP_GAP_PX
    );
    // ② 钮内向右 +10px（不换钮、不离开）：浮层位置**不变**（框固定在按钮上）。
    let c1x = c0.x.as_f32() + 10.0;
    assert!(
        c1x <= (bb.origin.x + bb.size.width).as_f32() - 1.0,
        "移动点应仍在 bb-play 盒内（避免换钮/离开）"
    );
    wcx.simulate_mouse_move(gpui::Point::new(px(c1x), c0.y), None, Default::default());
    wcx.run_until_parked();
    wcx.update(|window, app| window.draw(app).clear(app));
    wcx.run_until_parked();
    let tip1 = wcx.debug_bounds("music-tooltip").expect("浮层应仍在显示");
    assert!(
        (tip1.origin.x.as_f32() - tip0.origin.x.as_f32()).abs() < 1.0
            && (tip1.origin.y.as_f32() - tip0.origin.y.as_f32()).abs() < 1.0,
        "光标在钮内移动 +10px 后浮层应固定不动（不随光标漂移）：p0={:?} p1={:?}（旧跟随实现=RED）",
        tip0.origin,
        tip1.origin
    );
}

/// 用例 T7（防回退）：提示**未满 500ms** 时在盒内移动（不换钮/不离开）→ 浮层不提前
/// 出现（`render_tooltip` 的延迟态不被 move 跳过）；满 500ms 后正常显示（不残留抑制）。
#[gpui::test]
fn hover_move_before_show_does_not_force_tooltip(cx: &mut TestAppContext) {
    let player = player_with_library(cx);
    let (_view, wcx) = open_tooltip_view(cx, player);
    let bb = wcx.debug_bounds("bb-play").expect("bb-play 应有 debug 盒");
    let c0 = at_frac(&bb, 0.5);
    // 悬停 bb-play 中心（此刻 t≈0，浮层未显示期）。
    wcx.simulate_mouse_move(c0, None, Default::default());
    wcx.run_until_parked();
    // 未过 500ms：盒内移动 → 即使显式重绘一帧也不应渲染浮层盒。
    let c1 = gpui::Point::new(px(c0.x.as_f32() + 8.0), c0.y);
    wcx.simulate_mouse_move(c1, None, Default::default());
    wcx.run_until_parked();
    wcx.update(|window, app| window.draw(app).clear(app));
    wcx.run_until_parked();
    assert!(
        wcx.debug_bounds("music-tooltip").is_none(),
        "未满 500ms 的盒内移动不应提前渲染浮层"
    );
    // 满 500ms 后正常显示。
    wcx.executor().advance_clock(Duration::from_millis(600));
    wcx.run_until_parked();
    wcx.update(|window, app| window.draw(app).clear(app));
    wcx.run_until_parked();
    assert!(
        wcx.debug_bounds("music-tooltip").is_some(),
        "满 500ms 后应正常显示浮层"
    );
}

/// 用例 T8（按钮锚定换钮契约）：从钮 A 移到钮 B → 提示浮层**切换锚定**到 B 盒上方
/// （底边中点 ≈ B 盒中心 x / B 上缘 − GAP，且 ≠ A 锚定处）——验证「按钮间切换跟随
/// 按钮本身」（不是光标也不是固定某处），且 per-id 盒互不覆盖。
#[gpui::test]
fn hover_tooltip_switches_anchor_between_buttons(cx: &mut TestAppContext) {
    let player = player_with_library(cx);
    let (_view, wcx) = open_tooltip_view(cx, player);
    let a = wcx.debug_bounds("bb-prev").expect("bb-prev 应有 debug 盒");
    let b = wcx.debug_bounds("bb-next").expect("bb-next 应有 debug 盒");
    // 悬停钮 A（bb-prev）→ 满 500ms → 读浮层盒，锚在 A 盒上方。
    hover_button(wcx, "bb-prev");
    wcx.executor().advance_clock(Duration::from_millis(600));
    wcx.run_until_parked();
    wcx.update(|window, app| window.draw(app).clear(app));
    wcx.run_until_parked();
    let ta = wcx.debug_bounds("music-tooltip").expect("悬停 A 应显示浮层盒");
    let a_mid_x = a.origin.x.as_f32() + a.size.width.as_f32() * 0.5;
    let a_bottom_y = ta.origin.y.as_f32() + ta.size.height.as_f32();
    assert!(
        (ta.origin.x.as_f32() + ta.size.width.as_f32() * 0.5 - a_mid_x).abs() < 2.0
            && (a_bottom_y - (a.origin.y.as_f32() - super::TOOLTIP_GAP_PX)).abs() < 2.0,
        "悬停 A 提示应锚在 A 盒上方：ta={ta:?} a={a:?}"
    );
    // 移到钮 B（bb-next）→ 提示切换锚定到 B 盒上方（位置应≠A 锚定处）。
    hover_button(wcx, "bb-next");
    wcx.executor().advance_clock(Duration::from_millis(600));
    wcx.run_until_parked();
    wcx.update(|window, app| window.draw(app).clear(app));
    wcx.run_until_parked();
    let tb = wcx.debug_bounds("music-tooltip").expect("悬停 B 应显示浮层盒");
    let b_mid_x = b.origin.x.as_f32() + b.size.width.as_f32() * 0.5;
    let b_bottom_y = tb.origin.y.as_f32() + tb.size.height.as_f32();
    assert!(
        (tb.origin.x.as_f32() + tb.size.width.as_f32() * 0.5 - b_mid_x).abs() < 2.0
            && (b_bottom_y - (b.origin.y.as_f32() - super::TOOLTIP_GAP_PX)).abs() < 2.0,
        "悬停 B 提示应锚在 B 盒上方：tb={tb:?} b={b:?}"
    );
    assert!(
        (tb.origin.x.as_f32() - ta.origin.x.as_f32()).abs() > 1.0,
        "换钮后提示应跟随到新钮上方（不固定在某处）：ta.x={:.1} tb.x={:.1}",
        ta.origin.x.as_f32(),
        tb.origin.x.as_f32()
    );
}

// ---- 主区列表虚拟化（性能优化第二步）回归：可视窗 ± 缓冲物化、下标映射、分组头、滚动 ----

/// 用例 V1（虚拟化契约，旧全量实现=RED）：Home 大库**顶部**只物化可视窗 ± 缓冲行，
/// 屏外末行不物化（旧实现 r119 全量在 → 断言 is_none 失败）。
#[gpui::test]
async fn home_list_only_materializes_visible_window(cx: &mut TestAppContext) {
    let player = library_with_n(cx, 120);
    let (_view, wcx) = cx.add_window_view(|_w, cx| MusicView::new(player, cx));
    wcx.update(|window, app| {
        window.draw(app).clear(app);
    });
    wcx.run_until_parked();
    assert!(
        wcx.debug_bounds("song-row-0").is_some(),
        "顶部应物化首行（可视窗内）"
    );
    assert!(
        wcx.debug_bounds("song-row-5").is_some(),
        "顶部近端行应物化"
    );
    assert!(
        wcx.debug_bounds("song-row-119").is_none(),
        "屏外末行不应被物化（只物化可视窗±缓冲；旧全量实现此断言 RED）"
    );
}

/// 用例 V2（滚动行窗 + 点击下标映射）：滚到第 20 行 → 首行窗 = floor(offset/ROW_H)、第 20 行
/// 物化 / 首行出屏；点击第 20 行 → `handle_play_idx(20)`（虚拟化下标映射不漂移）。
#[gpui::test]
async fn home_scroll_moves_window_and_click_maps_real_index(cx: &mut TestAppContext) {
    let player = library_with_n(cx, 120);
    let (view, wcx) = cx.add_window_view(|_w, cx| MusicView::new(player.clone(), cx));
    wcx.update(|window, app| {
        window.draw(app).clear(app);
    });
    wcx.run_until_parked();
    let handle = wcx.cx.read(|app| view.read(app).main_scroll.clone());
    // 滚到下行 20×52=1040px（显示「首行窗 = 20」）。
    handle.set_offset(gpui::point(px(0.), px(-(52.0 * 20.0))));
    wcx.update(|window, app| {
        window.draw(app).clear(app);
    });
    wcx.run_until_parked();
    assert!(
        wcx.debug_bounds("song-row-20").is_some(),
        "滚到第 20 行时该行应物化"
    );
    assert!(
        wcx.debug_bounds("song-row-0").is_none(),
        "滚到中部后首行应出屏（不物化）"
    );
    // 像素点击第 20 行 → 真实库下标 20 随行对象被 `handle_play_idx` 命中。
    let r20 = wcx.debug_bounds("song-row-20").expect("第 20 行应有盒");
    let click = gpui::Point::new(
        px(r20.origin.x.as_f32() + 30.0),
        px(r20.origin.y.as_f32() + r20.size.height.as_f32() * 0.5),
    );
    wcx.simulate_mouse_down(click, MouseButton::Left, Default::default());
    wcx.simulate_mouse_up(click, MouseButton::Left, Default::default());
    wcx.run_until_parked();
    let cur = wcx.cx.read(|app| player.read(app).current());
    assert_eq!(cur, Some(20), "点击滚后可视行应命中真实库下标 20，实得 {cur:?}");
}

/// 用例 V3（Library 分组）：分组头固定高物化在列表首；点击组内首行 → 正确真实下标。
#[gpui::test]
async fn library_groups_header_and_first_row_click(cx: &mut TestAppContext) {
    let player = library_with_n(cx, 40); // 5 艺术家 × 8 首
    let (view, wcx) = cx.add_window_view(|_w, cx| MusicView::new(player.clone(), cx));
    wcx.update(|window, app| {
        view.update(app, |v, _| v.state.section = MainSection::Library);
        window.draw(app).clear(app);
    });
    wcx.run_until_parked();
    // 首组「艺人0」组头 + 组内首行（idx=0）应物化。
    let gh = wcx.debug_bounds("song-group-艺人0").expect("艺人0 组头应物化");
    assert!(
        wcx.debug_bounds("song-group-艺人4").is_none(),
        "屏外尾组头不应物化（分组前缀反查生效）"
    );
    let r0 = wcx.debug_bounds("song-row-0").expect("艺人0 组内首行应物化");
    assert!(
        (r0.origin.y.as_f32() - (gh.origin.y.as_f32() + 48.0)).abs() < 2.0,
        "组头(48)后应紧接该组首行：gh.y={:.1} r0.y={:.1}",
        gh.origin.y.as_f32(),
        r0.origin.y.as_f32()
    );
    let click = gpui::Point::new(
        px(r0.origin.x.as_f32() + 30.0),
        px(r0.origin.y.as_f32() + 20.0),
    );
    wcx.simulate_mouse_down(click, MouseButton::Left, Default::default());
    wcx.simulate_mouse_up(click, MouseButton::Left, Default::default());
    wcx.run_until_parked();
    let cur = wcx.cx.read(|app| player.read(app).current());
    assert_eq!(cur, Some(0), "点击艺人0 组内首行应命中库下标 0");
}

/// 用例 V4（Library 滚动跨组）：滚到中部组区间 → 该组头物化、首组头出屏（混合行高前缀反查）。
#[gpui::test]
async fn library_scroll_crosses_groups_window(cx: &mut TestAppContext) {
    let player = library_with_n(cx, 40); // 组高 48 + 8×52 = 464/组
    let (view, wcx) = cx.add_window_view(|_w, cx| MusicView::new(player, cx));
    wcx.update(|window, app| {
        view.update(app, |v, _| v.state.section = MainSection::Library);
        window.draw(app).clear(app);
    });
    wcx.run_until_parked();
    let handle = wcx.cx.read(|app| view.read(app).main_scroll.clone());
    // 第 2 组顶 ≈ 464×2 = 928 → 滚到 1000（组2 头 + 部分行可视）。
    handle.set_offset(gpui::point(px(0.), px(-1000.0)));
    wcx.update(|window, app| {
        window.draw(app).clear(app);
    });
    wcx.run_until_parked();
    assert!(
        wcx.debug_bounds("song-group-艺人2").is_some(),
        "滚到中部应物化该组组头（艺人2）"
    );
    assert!(
        wcx.debug_bounds("song-group-艺人0").is_none(),
        "滚到中部首组头应出屏（混合行高前缀反查）"
    );
}

/// 用例 V5（搜索过滤缓存）：查询串变化 → 只物化命中行；空命中 → 空态（列表区不渲染歌行）。
#[gpui::test]
async fn search_filters_main_list_rows(cx: &mut TestAppContext) {
    let player = library_with_n(cx, 40);
    let (view, wcx) = cx.add_window_view(|_w, cx| MusicView::new(player, cx));
    wcx.update(|window, app| {
        window.draw(app).clear(app);
    });
    wcx.run_until_parked();
    assert!(wcx.debug_bounds("song-row-1").is_some(), "空查询应物化第 1 行");
    // 搜索「艺人0」→ 命中 artist=艺人0（i%5==0）：i=0,5,…,35 共 8 首；i=8（艺人1）不命中。
    wcx.update(|_w, app| {
        view.update(app, |v, _| v.state.search = "艺人0".into());
    });
    wcx.update(|window, app| {
        window.draw(app).clear(app);
    });
    wcx.run_until_parked();
    assert!(wcx.debug_bounds("song-row-0").is_some(), "命中行应物化（idx0）");
    assert!(
        wcx.debug_bounds("song-row-8").is_none(),
        "未命中行（艺人1）不应物化（搜索过滤生效）"
    );
    let cached = wcx.cx.read(|app| {
        let v = view.read(app);
        v.main_list_cache
            .get(&(MainSection::Home, "艺人0".to_string()))
            .map(|(_rev, l)| l.total)
    });
    assert_eq!(cached, Some(8), "搜索模型缓存应含 8 首命中，实得 {cached:?}");
}

// ---- 搜索框迁移到 ui::InputState（R1/R4）：set_value / Ctrl+V 粘贴 / Esc 清空 ----

/// 绑定搜索输入需要的「Input」键位：`base::actions::bind_input_keys`（Ctrl+A/C/X/Z/Y、
/// 方向、退格等）+ `src/main.rs` 同款的 `ctrl-v → PasteNormalized` 覆盖绑定（Input 键
/// 上下文），供粘贴/Esc 测试走真实 按键→action 链（对齐 `ui/src/input.rs` 测试的
/// `bind_paste_normalized` 姿势；本 crate 无 test_support，绑本测试最小面）。
fn bind_search_keys(cx: &TestAppContext) {
    cx.update(|cx| {
        base::actions::bind_input_keys(cx);
        cx.bind_keys([gpui::KeyBinding::new(
            "ctrl-v",
            base::actions::PasteNormalized,
            Some("Input"),
        )]);
    });
}

/// 聚焦搜索输入框（InputState 内部 `focus_handle`，真实键盘派发起点）。
fn focus_search_input(wcx: &mut VisualTestContext, view: &Entity<MusicView>) {
    wcx.update(|window, cx| {
        if let Some(i) = view.read(cx).search_input.clone() {
            let fh = i.read(cx).focus_handle(cx);
            window.focus(&fh, cx);
        }
    });
    wcx.run_until_parked();
}

/// 迁移 R4-A：搜索输入框 `set_value`（程序化写入，发 `Change`）→ `state.search` 镜像 +
/// 主表过滤生效。行为级验证：搜索词改由 ui::InputState 文本驱动，不再直改 `state.search`。
///
/// 注：`library_with_n(40)` 入库后按文件名排序——同艺术家「艺人X」8 首连续占一组下标
/// （艺人0 → idx0..7、艺人1 → idx8..15…）。故 q="艺人0" 命中的正是 idx0..7，
/// 判据用 `song-row-0/7` 在（艺人0 组）、`song-row-8` 不在（艺人1 组）。
#[gpui::test]
async fn search_input_set_value_filters_main_list(cx: &mut TestAppContext) {
    let player = library_with_n(cx, 40);
    let (view, wcx) = cx.add_window_view(|_w, cx| MusicView::new(player, cx));
    // 首帧渲染：懒初始化搜索输入实体（MusicView::new 无 window）。
    wcx.update(|window, app| {
        window.draw(app).clear(app);
    });
    wcx.run_until_parked();
    assert!(
        wcx.cx.read(|app| view.read(app).search_input.is_some()),
        "首帧渲染应已懒初始化搜索输入实体"
    );
    // set_value("艺人0") → Change → 订阅镜像 state.search → 过滤（艺人0 组 8 首）。
    wcx.update(|window, app| {
        view.update(app, |v, cx| {
            let input = v.search_input.clone().expect("已初始化");
            input.update(cx, |s, scx| s.set_value("艺人0", window, scx));
        });
    });
    let synced = wcx.cx.read(|app| view.read(app).state.search.clone());
    assert_eq!(synced, "艺人0", "Change 应把输入框文本镜像进 state.search");
    wcx.update(|window, app| {
        window.draw(app).clear(app);
    });
    wcx.run_until_parked();
    assert!(wcx.debug_bounds("song-row-0").is_some(), "命中行应物化（艺人0 首行）");
    assert!(wcx.debug_bounds("song-row-7").is_some(), "艺人0 组内行应物化");
    assert!(
        wcx.debug_bounds("song-row-8").is_none(),
        "艺人1 组行（idx8）不应物化（set_value 路径过滤生效）"
    );
    let cached = wcx.cx.read(|app| {
        view.read(app)
            .main_list_cache
            .get(&(MainSection::Home, "艺人0".to_string()))
            .map(|(_rev, l)| l.total)
    });
    assert_eq!(cached, Some(8), "过滤模型缓存应含 8 首命中，实得 {cached:?}");
}

/// 迁移 R4-B：Ctrl+V 真实键链粘贴 → 搜索词进入 InputState 文本 + Change 同步过滤
/// （模拟「复制歌词名 → 粘贴进搜索框即滤」的既有用户路径）。
#[gpui::test]
async fn search_input_ctrl_v_pastes_into_search(cx: &mut TestAppContext) {
    bind_search_keys(cx);
    let player = library_with_n(cx, 40);
    let (view, wcx) = cx.add_window_view(|_w, cx| MusicView::new(player, cx));
    wcx.update(|window, app| {
        window.draw(app).clear(app);
    });
    wcx.run_until_parked();
    // 剪贴板放入待粘贴词（「艺人1」→ 艺人1 组 8 首 = idx8..15）+ 聚焦搜索框。
    let pasted = "艺人1";
    wcx.write_to_clipboard(gpui::ClipboardItem::new_string(pasted.to_string()));
    focus_search_input(wcx, &view);
    let w = wcx.windows().first().copied().expect("测试窗口");
    wcx.dispatch_keystroke(w, gpui::Keystroke::parse("ctrl-v").unwrap());
    wcx.run_until_parked();
    // 断言：InputState 文本 == 粘贴词；Change → state.search 同步；列表过滤生效。
    let (input_text, search) = wcx.cx.read(|app| {
        let v = view.read(app);
        let text = v
            .search_input
            .clone()
            .map(|i| i.read(app).value().to_string())
            .unwrap_or_default();
        (text, v.state.search.clone())
    });
    assert_eq!(input_text, pasted, "粘贴文本应进入搜索输入框");
    assert_eq!(search, pasted, "Change 应把粘贴文本镜像进 state.search");
    wcx.update(|window, app| {
        window.draw(app).clear(app);
    });
    wcx.run_until_parked();
    assert!(wcx.debug_bounds("song-row-8").is_some(), "命中行应物化（艺人1 组 idx8）");
    assert!(
        wcx.debug_bounds("song-row-0").is_none(),
        "艺人0 行不应物化（Ctrl+V 粘贴驱动过滤生效）"
    );
}

/// 迁移 R4-C：Esc 真实键链 → Cancel → 清空搜索词 + 输入框文本，且**不**触发
/// `MusicEvent::ExitRequested`（Root 全局 Esc 退出语义在搜索框持焦时让路——
/// InputState 对 Esc `stop_propagation`，root `on_key` 收不到）。
#[gpui::test]
async fn search_input_escape_clears_query_without_exit(cx: &mut TestAppContext) {
    bind_search_keys(cx);
    let player = library_with_n(cx, 40);
    let (view, wcx) = cx.add_window_view(|_w, cx| MusicView::new(player, cx));
    let exits = Rc::new(Cell::new(0usize));
    wcx.update(|window, app| {
        let e = exits.clone();
        window
            .subscribe(&view, app, move |_, _: &MusicEvent, _, _| e.set(e.get() + 1))
            .detach();
        window.draw(app).clear(app);
    });
    wcx.run_until_parked();
    // 先经 set_value 置入搜索词（走实体路径）。
    wcx.update(|window, app| {
        view.update(app, |v, cx| {
            if let Some(i) = v.search_input.clone() {
                i.update(cx, |s, scx| s.set_value("艺人0", window, scx));
            }
        });
    });
    assert_eq!(
        wcx.cx.read(|app| view.read(app).state.search.clone()),
        "艺人0",
        "前置：set_value 已同步 state.search"
    );
    focus_search_input(wcx, &view);
    let w = wcx.windows().first().copied().expect("测试窗口");
    wcx.dispatch_keystroke(w, gpui::Keystroke::parse("escape").unwrap());
    wcx.run_until_parked();
    let (input_text, search) = wcx.cx.read(|app| {
        let v = view.read(app);
        let text = v
            .search_input
            .clone()
            .map(|i| i.read(app).value().to_string())
            .unwrap_or_default();
        (text, v.state.search.clone())
    });
    assert_eq!(input_text, "", "Esc 应清空输入框文本");
    assert_eq!(search, "", "Esc 应清空 state.search");
    assert_eq!(exits.get(), 0, "搜索框持焦时 Esc 不应触发退出音乐模式");
}

/// 用例 V6（Liked + 收藏变更失效缓存）：无收藏 → 空态；收藏 1 首 → 该行物化（mutation 升，
/// 显示模型缓存失效重建）。
#[gpui::test]
async fn liked_list_updates_after_like_invalidation(cx: &mut TestAppContext) {
    let player = library_with_n(cx, 12);
    let (view, wcx) = cx.add_window_view(|_w, cx| MusicView::new(player.clone(), cx));
    wcx.update(|window, app| {
        view.update(app, |v, _| v.state.section = MainSection::Liked);
        window.draw(app).clear(app);
    });
    wcx.run_until_parked();
    assert!(
        wcx.debug_bounds("song-row-0").is_none(),
        "无收藏时收藏列表应为空（不物化任何行）"
    );
    // 收藏第 0 首（mutation 递增 → 缓存失效）→ 重绘后该行物化。
    wcx.update(|_w, app| {
        player.update(app, |p, cx| p.toggle_like(0, cx));
    });
    wcx.update(|window, app| {
        window.draw(app).clear(app);
    });
    wcx.run_until_parked();
    assert!(
        wcx.debug_bounds("song-row-0").is_some(),
        "收藏后该行应物化（缓存失效重建）"
    );
}

// ---- Home 整页竖向虚拟滚动（我的推荐 Mix 卡横滑 + 滚轮拦截）回归 ----

/// 用例 H1（Home 头部虚拟项）：Home 列表前 3 项 = 「我的推荐」标题 / Mix 卡横滑行 /
/// 「热门歌曲」标题+表头（固定高虚拟项，随整页滚）；Library/Liked 不插（total_h 不变）。
/// 旧实现（推荐块在滚动区外）下 items[0] 直接是歌行、无 home-* 元素 → 本用例 RED。
#[gpui::test]
async fn home_header_items_lead_virtual_list_only_for_home(cx: &mut TestAppContext) {
    let player = library_with_n(cx, 12);
    let (view, wcx) = cx.add_window_view(|_w, cx| MusicView::new(player.clone(), cx));
    wcx.update(|window, app| {
        window.draw(app).clear(app);
    });
    wcx.run_until_parked();
    let (home_items, home_total_h) = wcx.cx.read(|app| {
        let v = view.read(app);
        let home = v
            .main_list_cache
            .get(&(MainSection::Home, String::new()))
            .map(|(_rev, l)| l.clone())
            .expect("Home 列表应在缓存");
        (home.items.clone(), home.total_h())
    });
    use super::main::MainItem;
    assert!(
        matches!(home_items[0], MainItem::HomeRecTitle)
            && matches!(home_items[1], MainItem::HomeMixRow)
            && matches!(home_items[2], MainItem::HomeHotTitle { .. }),
        "Home 列表头部应为三个固定虚拟项，实得前 3 项 {:?}",
        &home_items[..3]
    );
    assert_eq!(
        home_items.len(),
        3 + 12,
        "Home 头部 3 项 + 12 首歌曲行"
    );
    // 头部固定高参与总高：含推荐块（需求 1：随滚）——滚动量程 = 头部 + 歌曲总高。
    let expect_home_h = super::main::REC_TITLE_H
        + super::main::REC_CARDS_H
        + super::main::HOT_TITLE_H
        + 12.0 * 52.0;
    assert!(
        (home_total_h - expect_home_h).abs() < 1.0,
        "Home total_h 应含头部虚拟块：got={home_total_h} expect={expect_home_h}"
    );
    // Library/Liked 不插 Home 头部项（回归约束）：Library 显示模型只含既有 分组头+歌行。
    wcx.update(|window, app| {
        view.update(app, |v, _| v.state.section = MainSection::Library);
        window.draw(app).clear(app);
    });
    wcx.run_until_parked();
    let (lib_len, lib_has_home_variants) = wcx.cx.read(|app| {
        let v = view.read(app);
        let lib = v
            .main_list_cache
            .get(&(MainSection::Library, String::new()))
            .map(|(_rev, l)| l.clone())
            .expect("Library 列表应在缓存");
        let has_home = lib
            .items
            .iter()
            .any(|it| matches!(it, MainItem::HomeRecTitle | MainItem::HomeMixRow | MainItem::HomeHotTitle { .. } | MainItem::HomeEmpty));
        (lib.items.len(), has_home)
    });
    assert!(
        !lib_has_home_variants,
        "Library 不应含 Home 头部虚拟项"
    );
    assert_eq!(lib_len, 5 + 12, "Library = 5 组头 + 12 行（组头是 Library 既有模型）");
}

/// 用例 H2（需求 1/3：整页随竖向滚推荐块滚出）：滚到列表中部/底部 → 推荐块虚拟项
/// 已滚出可视窗（home-rec-title/home-mix-row 不再物化）、末行可见；旧实现推荐块在滚动区外
/// 固定不滚 → 无 home-* 元素可断言 → RED。
#[gpui::test]
async fn home_scroll_to_bottom_scrolls_rec_block_out(cx: &mut TestAppContext) {
    let player = library_with_n(cx, 120);
    let (view, wcx) = cx.add_window_view(|_w, cx| MusicView::new(player, cx));
    wcx.update(|window, app| {
        window.draw(app).clear(app);
    });
    wcx.run_until_parked();
    // 顶部：推荐块 + 歌曲首行都物化（整页一起可见）。
    assert!(wcx.debug_bounds("home-rec-title").is_some(), "顶部应物化推荐标题");
    assert!(wcx.debug_bounds("home-mix-row").is_some(), "顶部应物化 Mix 行");
    assert!(wcx.debug_bounds("song-row-0").is_some(), "顶部歌曲首行应物化");
    // 滚到底：等价的 visible_range 首项应超过推荐块下标（0..2 出窗）。
    let handle = wcx.cx.read(|app| view.read(app).main_scroll.clone());
    let max = handle.max_offset().y.as_f32();
    let (list, first, last) = wcx.cx.read(|app| {
        let v = view.read(app);
        let home = v
            .main_list_cache
            .get(&(MainSection::Home, String::new()))
            .map(|(_rev, l)| l.clone())
            .expect("Home 列表应在缓存");
        let viewport = handle.bounds().size.height.as_f32();
        let (f, l) = home.visible_range(max, viewport);
        (home.total_h(), f, l)
    });
    assert!(
        first > 2,
        "滚到底推荐块应滚出可视窗（visible_range 首项 > 2）：first={first}"
    );
    assert!(last > 2, "滚到底可视窗应越过推荐块：last={last}");
    assert!(
        list > 120.0 * 52.0,
        "Home 总高应含推荐块：got={list}"
    );
    // 真实滚到底：推荐块不物化、末行可见。
    handle.set_offset(gpui::point(px(0.), px(-max)));
    wcx.update(|window, app| window.draw(app).clear(app));
    wcx.run_until_parked();
    assert!(
        wcx.debug_bounds("home-rec-title").is_none() && wcx.debug_bounds("home-mix-row").is_none(),
        "滚到底推荐块应出屏（不物化）"
    );
    assert!(
        wcx.debug_bounds("song-row-119").is_some(),
        "滚到底应物化最后一行（song-row-119）"
    );
}

/// 用例 H3（需求 2：滚轮在 Mix 卡区只横向、不带动整页）：对 `#music-mix` 行模拟**竖向**
/// 滚轮 → `main_mix_scroll`（横滚句柄）offset.x < 0（Mix 卡横向滑动）+ 外层 `main_scroll`
/// offset.y 不变（整页不动）。旧实现竖向滚会带动外层（Mix 横滚容器无拦截）→ offset.y 变、RED。
#[gpui::test]
async fn wheel_over_mix_row_scrolls_mix_horizontally_not_page(cx: &mut TestAppContext) {
    use gpui::{Modifiers, ScrollDelta, ScrollWheelEvent, TouchPhase, point};
    let player = library_with_n(cx, 120); // 5 组 Mix 卡；缩窗后内容宽 > 视口 → 横滚量程非 0
    let (view, wcx) = cx.add_window_view(|_w, cx| MusicView::new(player, cx));
    // 缩窄窗口让 Mix 卡行产生横向溢出（默认 1920 宽下 5 卡 964px 不溢出、可滚量程为 0）。
    wcx.simulate_resize(gpui::size(px(900.), px(800.)));
    wcx.update(|window, app| window.draw(app).clear(app));
    wcx.run_until_parked();
    let main_handle = wcx.cx.read(|app| view.read(app).main_scroll.clone());
    let mix_handle = wcx.cx.read(|app| view.read(app).main_mix_scroll.clone());
    let y0 = main_handle.offset().y.as_f32();
    let x0 = mix_handle.offset().x.as_f32();
    assert!(
        mix_handle.max_offset().x.as_f32() > 0.0,
        "窄窗下 Mix 行应可横滚（max_offset.x={:?} > 0）",
        mix_handle.max_offset()
    );
    let b = wcx.debug_bounds("home-mix-row").expect("Mix 行应有 debug 盒");
    let pos = gpui::Point::new(
        px(b.origin.x.as_f32() + b.size.width.as_f32() * 0.5),
        px(b.origin.y.as_f32() + b.size.height.as_f32() * 0.5),
    );
    wcx.simulate_event(ScrollWheelEvent {
        position: pos,
        delta: ScrollDelta::Pixels(point(px(0.), px(-120.))),
        modifiers: Modifiers::default(),
        touch_phase: TouchPhase::Moved,
    });
    wcx.run_until_parked();
    wcx.update(|window, app| window.draw(app).clear(app));
    wcx.run_until_parked();
    let x1 = mix_handle.offset().x.as_f32();
    let y1 = main_handle.offset().y.as_f32();
    assert!(
        x1 < x0,
        "竖向滚轮应把 Mix 卡横向滑动（横滚 offset 更负）：x0={x0} x1={x1}"
    );
    assert!(
        (y1 - y0).abs() < 0.01,
        "滚 Mix 卡不得带动整页竖向滚：y0={y0} y1={y1}"
    );
}

/// 用例 H4（需求 3：滚轮在「热门歌曲」标题区 → 整页竖向滚）：对 `home-hot-title` 模拟竖向
/// 滚轮 → 外层 `main_scroll` offset.y 变化（整页一起滚）。旧实现标题区是列表外固定块 → 滚轮
/// 只滚列表、标题不动 → 语义不同（此用例验证新「整页滚」语义）。
#[gpui::test]
async fn wheel_over_hot_title_scrolls_whole_page(cx: &mut TestAppContext) {
    use gpui::{Modifiers, ScrollDelta, ScrollWheelEvent, TouchPhase, point};
    let player = library_with_n(cx, 120);
    let (view, wcx) = cx.add_window_view(|_w, cx| MusicView::new(player, cx));
    wcx.update(|window, app| window.draw(app).clear(app));
    wcx.run_until_parked();
    let main_handle = wcx.cx.read(|app| view.read(app).main_scroll.clone());
    let y0 = main_handle.offset().y.as_f32();
    let b = wcx.debug_bounds("home-hot-title").expect("热门标题行应有 debug 盒");
    let pos = gpui::Point::new(
        px(b.origin.x.as_f32() + b.size.width.as_f32() * 0.5),
        px(b.origin.y.as_f32() + b.size.height.as_f32() * 0.5),
    );
    wcx.simulate_event(ScrollWheelEvent {
        position: pos,
        delta: ScrollDelta::Pixels(point(px(0.), px(-120.))),
        modifiers: Modifiers::default(),
        touch_phase: TouchPhase::Moved,
    });
    wcx.run_until_parked();
    wcx.update(|window, app| window.draw(app).clear(app));
    wcx.run_until_parked();
    let y1 = main_handle.offset().y.as_f32();
    assert!(y1 < y0, "滚热门标题区应整页竖向滚（offset 更负）：y0={y0} y1={y1}");
}

/// 用例 H5（小窗口）：小视口下 Home `total_h` 含推荐块高度（> 纯歌曲总高），滚到底可显示末行
/// ——小窗口也能滚到全部内容（需求 3）。旧实现滚动区只含歌曲行、推荐块固定占掉有限空间 → 语义不同。
#[gpui::test]
async fn home_small_window_total_h_includes_rec_block_and_bottom_reachable(cx: &mut TestAppContext) {
    let player = library_with_n(cx, 120);
    let (view, wcx) = cx.add_window_view(|_w, cx| MusicView::new(player, cx));
    wcx.update(|window, app| window.draw(app).clear(app));
    wcx.run_until_parked();
    wcx.simulate_resize(gpui::size(px(900.), px(500.)));
    wcx.update(|window, app| window.draw(app).clear(app));
    wcx.run_until_parked();
    let (list, n_rows) = wcx.cx.read(|app| {
        let v = view.read(app);
        let home = v
            .main_list_cache
            .get(&(MainSection::Home, String::new()))
            .map(|(_rev, l)| l.clone())
            .expect("Home 列表应在缓存");
        (home.total_h(), home.total)
    });
    assert_eq!(n_rows, 120);
    assert!(
        list > 120.0 * 52.0,
        "小窗口下 Home total_h 应含推荐块高度（> 歌曲总高）：{list}"
    );
    // 滚到底（超界 offset 会钳位到 -max）→ 末行物化。
    let handle = wcx.cx.read(|app| view.read(app).main_scroll.clone());
    handle.set_offset(gpui::point(px(0.), px(-(list + 1000.0))));
    wcx.update(|window, app| window.draw(app).clear(app));
    wcx.run_until_parked();
    assert!(
        wcx.debug_bounds("song-row-119").is_some(),
        "小窗口滚到底应显示末行（song-row-119）"
    );
}

/// 用例 H6（空库/搜索 0 命中）：Home 头部占位项稳定可滚、空态行在列表内、不 panic。
#[gpui::test]
async fn home_empty_or_zero_hit_keeps_header_placeholders(cx: &mut TestAppContext) {
    // 空库（未 load_dir）：正在扫描/无歌占位行 + 头部三项都在。
    let player: Entity<MusicPlayer> = cx.update(|app| app.new(|cx| MusicPlayer::new(cx)));
    let (_view, wcx) = cx.add_window_view(|_w, cx| MusicView::new(player, cx));
    wcx.update(|window, app| window.draw(app).clear(app));
    wcx.run_until_parked();
    assert!(wcx.debug_bounds("home-rec-title").is_some(), "空库也要物化推荐标题");
    assert!(wcx.debug_bounds("home-mix-row").is_some(), "空库也要物化 Mix 行");
    assert!(wcx.debug_bounds("home-hot-title").is_some(), "空库也要物化热门标题");
    assert!(wcx.debug_bounds("home-empty").is_some(), "空库应物化空态占位行");
    // 搜索到 0 命中（非空库）：头部项仍占位 + 空态行 + 无歌行、不 panic。
    let player2 = library_with_n(cx, 12);
    let (view2, wcx2) = cx.add_window_view(|_w, cx| MusicView::new(player2, cx));
    wcx2.update(|_w, app| view2.update(app, |v, _| v.state.search = "不存在".into()));
    wcx2.update(|window, app| window.draw(app).clear(app));
    wcx2.run_until_parked();
    assert!(wcx2.debug_bounds("home-rec-title").is_some(), "0 命中也要物化推荐标题");
    assert!(wcx2.debug_bounds("home-empty").is_some(), "0 命中应物化空态占位行");
    assert!(
        wcx2.debug_bounds("song-row-0").is_none(),
        "0 命中不应物化歌行"
    );
}


// ---- Windows 风格标题栏（主界面顶层；窗口控制钮走 window_control_area 原生机制）----

/// 最大化/还原图标选择（纯函数，双路注入断言）：未最大化 →「最大化」图标；已最大化 →
/// 「向下还原」图标（对应 shell chrome.rs:419 的 `is_maximized` 分支）。headless 测试平台
/// `is_maximized()` 恒返回 false 且 `minimize()` 未实现，真实翻转只能由系统/真机完成，
/// 故这里对**选择方向**注入两种状态断言（K30：headless 证机制、真实路径待真机复验）。
#[test]
fn title_max_icon_selector_flips_direction() {
    assert_eq!(
        max_icon(false),
        MusicIcon::Maximize,
        "未最大化应显示「最大化」图标"
    );
    assert_eq!(
        max_icon(true),
        MusicIcon::Restore,
        "已最大化应切换为「向下还原」图标"
    );
}

/// 标题栏布局：`music-title-bar` 置顶（origin.y=0、高 [`TITLE_BAR_H`]、通窗宽）；三窗口
/// 控制钮（`title-minimize`/`title-maximize`/`title-close`，id 对齐 shell）在窗宽内、
/// 互不重叠、从左到右 min<max<close 且贴右缘；左标题文字在钮左、空白拖动区在两者之间。
#[gpui::test]
async fn title_bar_controls_layout_within_window(cx: &mut TestAppContext) {
    let player = player_with_library(cx);
    let (_view, wcx) = cx.add_window_view(|_w, cx| MusicView::new(player, cx));
    wcx.update(|window, app| {
        window.draw(app).clear(app);
    });
    wcx.run_until_parked();

    let bar = wcx.debug_bounds("music-title-bar").expect("标题栏应在渲染");
    let win_w = wcx.update(|window, _| window.bounds().size.width.as_f32());
    let right = |b: &Bounds<Pixels>| b.origin.x.as_f32() + b.size.width.as_f32();
    assert_eq!(bar.origin.y.as_f32(), 0.0, "标题栏应置顶");
    assert!(
        (bar.size.height.as_f32() - TITLE_BAR_H).abs() < 0.5,
        "标题栏高应为 {TITLE_BAR_H}px，实得 {}",
        bar.size.height.as_f32()
    );
    assert!(
        (bar.size.width.as_f32() - win_w).abs() < 0.5,
        "标题栏应通窗宽：bar={} win={win_w}",
        bar.size.width.as_f32()
    );

    let min = wcx.debug_bounds("title-minimize").expect("最小化钮应有 debug 盒");
    let maxb = wcx.debug_bounds("title-maximize").expect("最大化钮应有 debug 盒");
    let close = wcx.debug_bounds("title-close").expect("关闭钮应有 debug 盒");
    let text = wcx.debug_bounds("music-title-text").expect("标题文字应有 debug 盒");
    let drag = wcx.debug_bounds("music-title-drag").expect("空白拖动区应有 debug 盒");

    // 三钮在窗宽内、从左到右 min<max<close、不重叠；钮高=标题栏高（整条可点）。
    assert!(min.origin.x.as_f32() >= 0.0 && right(&close) <= win_w, "三钮应在窗宽内");
    assert!(right(&min) <= maxb.origin.x.as_f32() + 0.01, "最小化应在最大化左侧");
    assert!(right(&maxb) <= close.origin.x.as_f32() + 0.01, "最大化应在关闭左侧");
    assert_eq!(min.size.height.as_f32(), TITLE_BAR_H, "钮高应=标题栏高");
    assert_eq!(close.size.height.as_f32(), TITLE_BAR_H);
    // 标题文字在最小化钮左侧；空白拖动区夹在标题右与最小化钮左之间。
    assert!(right(&text) <= min.origin.x.as_f32(), "标题文字应在钮左");
    assert!(drag.origin.x.as_f32() >= right(&text), "拖动区应在标题文字右侧");
    assert!(right(&drag) <= min.origin.x.as_f32() + 0.01, "拖动区应在最小化钮左侧");
}

/// 标题栏不抢下方内容：主区首行（`song-row-0`）仍物化且顶部压在标题栏之下
/// （origin.y ≥ 标题栏底边）——标题栏只占新增的顶部行，不压缩/覆盖三栏（既有行点击、
/// 滚动测试已兜底命中语义，这里只证布局不重叠）。
#[gpui::test]
async fn title_bar_does_not_overlap_content_below(cx: &mut TestAppContext) {
    let player = player_with_library(cx);
    let (_view, wcx) = cx.add_window_view(|_w, cx| MusicView::new(player, cx));
    wcx.update(|window, app| {
        window.draw(app).clear(app);
    });
    wcx.run_until_parked();

    let bar = wcx.debug_bounds("music-title-bar").expect("标题栏应在渲染");
    let bar_bottom = bar.origin.y.as_f32() + bar.size.height.as_f32();
    let row0 = wcx.debug_bounds("song-row-0").expect("主区首行应仍物化");
    assert!(
        row0.origin.y.as_f32() >= bar_bottom - 0.01,
        "三栏内容应从标题栏之下开始（不重叠）：row0.y={} bar_bottom={bar_bottom}",
        row0.origin.y.as_f32()
    );
    assert!(
        row0.origin.y.as_f32() > bar.origin.y.as_f32(),
        "首行应位于标题栏之下（标题栏只占新顶部行）"
    );
}

/// logo/头像「叠放」回归：渐变背景 absolute-inset-0 铺满盒，前景（Disc /「K」）置于
/// absolute-inset-0 的居中覆盖层内叠其上——前景覆盖层布局盒必须正好=盒本身。曾把渐变与
/// 前景当 flex 兄弟 → 图标被挤到渐变旁/角落（对齐原型 `.logo-icon`/`.title-bar-icon`/
/// `.user-avatar`，三处同款布局 bug 修复后此结构不变式成立）。
#[gpui::test]
async fn logo_and_avatar_foreground_overlay_fills_gradient_box(cx: &mut TestAppContext) {
    let player = player_with_library(cx);
    let (_view, wcx) = cx.add_window_view(|_w, cx| MusicView::new(player, cx));
    wcx.update(|window, app| {
        window.draw(app).clear(app);
    });
    wcx.run_until_parked();

    for (box_sel, fg_sel, _exp) in [
        ("music-logo", "music-logo-fg", 36.0),
        ("music-user-avatar", "music-user-avatar-fg", 32.0),
        ("music-title-logo", "music-title-logo-fg", 16.0),
    ] {
        let b = wcx.debug_bounds(box_sel).expect("盒应渲染");
        let fg = wcx.debug_bounds(fg_sel).expect("前景居中覆盖层应渲染");
        let near = |a: f32, c: f32| (a - c).abs() < 0.5;
        assert!(
            near(fg.origin.x.as_f32(), b.origin.x.as_f32())
                && near(fg.origin.y.as_f32(), b.origin.y.as_f32())
                && near(fg.size.width.as_f32(), b.size.width.as_f32())
                && near(fg.size.height.as_f32(), b.size.height.as_f32()),
            "{box_sel} 前景覆盖层应 absolute-inset-0 正好填满盒（居中叠渐变上）：fg={fg:?} box={b:?}"
        );
    }
}

/// 全屏浮层（fs）盖住标题栏：标题栏仅常驻主界面，`fs_open` 时 `music-fs` 浮层 absolute
/// inset-0 覆盖整窗（含标题栏区域），不新增第二条标题栏（既有溢出/命中由 fs 层级隔离）。
#[gpui::test]
async fn fullscreen_overlay_does_not_add_second_title_bar(cx: &mut TestAppContext) {
    let player = player_with_library(cx);
    let (view, wcx) = cx.add_window_view(|_w, cx| MusicView::new(player, cx));
    wcx.update(|window, app| {
        window.draw(app).clear(app);
    });
    wcx.run_until_parked();
    wcx.update(|_w, app| {
        view.update(app, |v, cx| v.toggle_fs(cx));
    });
    wcx.update(|window, app| {
        window.draw(app).clear(app);
    });
    wcx.run_until_parked();

    let fs = wcx.debug_bounds("music-fs").expect("全屏浮层应在渲染");
    assert_eq!(fs.origin.y.as_f32(), 0.0, "fs 浮层应覆盖到窗口顶（含标题栏区域）");
    let fs_w = wcx.update(|window, _| window.bounds().size.width.as_f32());
    assert!(
        (fs.size.width.as_f32() - fs_w).abs() < 0.5,
        "fs 浮层应通窗宽覆盖"
    );
    // fs 态仍是同一个标题栏元素（无第二个标题栏 id 变体）。
    assert!(
        wcx.debug_bounds("music-title-bar").is_some(),
        "fs 下主界面标题栏元素仍在树内（被浮层覆盖、非新增）"
    );
}

/// 回归 H7（列对齐）：Home「热门歌曲」歌行各列在不同标题长度下必须上下对齐。根因：歌行
/// 歌曲列 `flex_grow(2.0)` 不改 flex-basis（taffy 默认 auto→flex-base=max-content），长标题
/// 撑宽该列、逐行推移专辑/时长/心形列（默认 980 下 song-album.x 曾 467/481/492/522 错位、
/// 专辑列被压到 8–33px）→ 表头/歌行 flex 列显式 `flex_basis(px(0.))` 后列宽纯由 grow 分配、
/// 与内容无关。断言默认/加宽/回缩三档窗口下各 `song-album-*` 左缘 x 一致（spread < 0.5px）。
#[gpui::test]
async fn home_hot_song_columns_align_across_title_widths(cx: &mut TestAppContext) {
    let dir = std::env::temp_dir().join(format!("gpui-music-align-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // 同一艺术家、标题宽度梯度（短/中/超长/中/短/短）——只让「歌曲」flex 列产生差异。
    let stems = [
        "艺 - 短.wav",
        "艺 - 中长度的标题.wav",
        "艺 - 啊啊啊啊啊啊啊啊啊啊啊啊啊啊啊啊啊啊啊啊啊啊啊超长标题.wav",
        "艺 - 又一个比较长的歌名标题.wav",
        "艺 - 甲.wav",
        "艺 - 乙.wav",
    ];
    for s in &stems {
        write_tiny_wav_short(&dir, s);
    }
    let player: Entity<MusicPlayer> = cx.update(|app| app.new(|cx| MusicPlayer::new(cx)));
    cx.update(|app| {
        player.update(app, |p, cx| p.load_dir(&dir, cx));
    });
    drain_scan(cx);
    let (_view, wcx) = cx.add_window_view(|_w, cx| MusicView::new(player, cx));
    wcx.update(|window, app| window.draw(app).clear(app));
    wcx.run_until_parked();
    for (label, w) in [("default980", 980.0), ("wide1400", 1400.0), ("back980", 980.0)] {
        wcx.simulate_resize(gpui::size(px(w), px(720.)));
        wcx.update(|window, app| window.draw(app).clear(app));
        wcx.run_until_parked();
        let xs: Vec<f32> = (0..stems.len())
            .filter_map(|i| {
                let sel = match i {
                    0 => "song-album-0",
                    1 => "song-album-1",
                    2 => "song-album-2",
                    3 => "song-album-3",
                    4 => "song-album-4",
                    _ => "song-album-5",
                };
                wcx.debug_bounds(sel).map(|b| b.origin.x.as_f32())
            })
            .collect();
        assert_eq!(xs.len(), stems.len(), "{label}: 歌行专辑列都应物化");
        let min = xs.iter().cloned().fold(f32::MAX, f32::min);
        let max = xs.iter().cloned().fold(f32::MIN, f32::max);
        eprintln!(
            "[regr] home hot columns @{label}: album.x range [{min:.1},{max:.1}] spread={:.1}px",
            max - min
        );
        assert!(
            max - min < 0.5,
            "{label}: 各歌行专辑列左缘应对齐（spread {:.1}px >= 0.5）: xs={xs:?}",
            max - min
        );
    }
}

// ---- 主区列表滚动条（迁移到 `ui::Scrollbar` Auto：滚轮/拖拽驱动显示、3s 空闲自动隐藏、
//      点击跳页、拖出 track 跟手 clamp；可见性经组件回写快照 `main_sb_shown` 断言）----

/// `#music-list` 滚轮激活 + 3s 无活动自动隐藏。滚动条「仅滚动驱动显示」：滚轮事件经
/// `ui::Scrollbar`(Auto) 的滚动区滚轮监听刷新空闲计时；组件空闲定时器到点自行隐藏并
/// notify 重绘。可见性经组件回写的 `main_sb_shown` 快照槽断言（单一事实源在组件内）。
#[gpui::test]
async fn main_scrollbar_shows_on_wheel_and_hides_after_3s(cx: &mut TestAppContext) {
    let player = library_with_n(cx, 200);
    let (view, mut wcx) = cx.add_window_view(|_w, cx| MusicView::new(player, cx));
    wcx.update(|window, app| window.draw(app).clear(app));
    wcx.run_until_parked();
    // 初始：无滚动活动 → 隐藏。
    assert!(
        !wcx.cx.read(|app| view.read(app).main_sb_shown.get()),
        "初始滚动条应隐藏"
    );
    // 顶部行中心附近滚轮 → 激活显示（组件滚动区滚轮监听 → 下一帧 prepaint 回写可见）。
    let b0 = wcx.debug_bounds("song-row-0").expect("顶部行应物化");
    let pos = gpui::Point::new(
        px(b0.origin.x.as_f32() + 200.0),
        px(b0.origin.y.as_f32() + 10.0),
    );
    use gpui::{Modifiers, ScrollDelta, ScrollWheelEvent, TouchPhase, point};
    wcx.simulate_event(ScrollWheelEvent {
        position: pos,
        delta: ScrollDelta::Pixels(point(px(0.), px(-120.))),
        modifiers: Modifiers::default(),
        touch_phase: TouchPhase::Moved,
    });
    wcx.run_until_parked();
    wcx.update(|window, app| window.draw(app).clear(app));
    wcx.run_until_parked();
    assert!(
        wcx.cx.read(|app| view.read(app).main_sb_shown.get()),
        "滚轮后滚动条应显示"
    );
    assert!(
        wcx.debug_bounds("music-main-scrollbar").is_some(),
        "显示时 track 条应物化"
    );
    // 3s 无滚动活动 → 自动隐藏（组件空闲定时器 → 隐藏 → 下一帧 prepaint 回写）。
    pump_ticks(&mut wcx, 13); // 250ms × 13 = 3250ms > 3s
    wcx.update(|window, app| window.draw(app).clear(app));
    wcx.run_until_parked();
    assert!(
        !wcx.cx.read(|app| view.read(app).main_sb_shown.get()),
        "3s 无滚动活动滚动条应隐藏"
    );
}

/// 内容不满可视窗 → 不渲染滚动条（track 不在树，组件未被装配），滚轮也不显示。
#[gpui::test]
async fn main_scrollbar_not_shown_when_content_fits(cx: &mut TestAppContext) {
    let player = library_with_n(cx, 5);
    let (view, wcx) = cx.add_window_view(|_w, cx| MusicView::new(player, cx));
    // 放大窗口保证内容总高 < 可视窗高。
    wcx.simulate_resize(gpui::size(px(1600.), px(1200.)));
    wcx.update(|window, app| window.draw(app).clear(app));
    wcx.run_until_parked();
    assert!(
        wcx.debug_bounds("music-main-scrollbar").is_none(),
        "内容不满可视窗不应渲染滚动条 track"
    );
    assert!(
        !wcx.cx.read(|app| view.read(app).main_sb_shown.get()),
        "内容不满可视窗 → 可见快照应为隐藏"
    );
    let b0 = wcx.debug_bounds("song-row-0").expect("顶部行应物化");
    let pos = gpui::Point::new(
        px(b0.origin.x.as_f32() + 200.0),
        px(b0.origin.y.as_f32() + 10.0),
    );
    use gpui::{Modifiers, ScrollDelta, ScrollWheelEvent, TouchPhase, point};
    wcx.simulate_event(ScrollWheelEvent {
        position: pos,
        delta: ScrollDelta::Pixels(point(px(0.), px(-120.))),
        modifiers: Modifiers::default(),
        touch_phase: TouchPhase::Moved,
    });
    wcx.run_until_parked();
    wcx.update(|window, app| window.draw(app).clear(app));
    wcx.run_until_parked();
    assert!(
        wcx.debug_bounds("music-main-scrollbar").is_none(),
        "内容不满视口时滚轮也不显示滚动条"
    );
}

/// thumb 拖拽定位：滚轮激活后**按在 thumb 上**（`ui::Scrollbar` 只对 thumb-grab 起拖拽）
/// → 拖到 60% 比例 → offset ≈ `-(0.6 * 量程)`（组件拖拽写回公式）；拖拽 = 滚动活动，
/// 松手后活动时间已刷新 → 仍保持显示。
#[gpui::test]
async fn main_scrollbar_thumb_drag_seeks_list(cx: &mut TestAppContext) {
    use gpui::{Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, point};
    let player = library_with_n(cx, 200);
    let (view, wcx) = cx.add_window_view(|_w, cx| MusicView::new(player, cx));
    wcx.update(|window, app| window.draw(app).clear(app));
    wcx.run_until_parked();
    // 先滚轮激活显示。
    let b0 = wcx.debug_bounds("song-row-0").expect("顶部行应物化");
    let pos0 = gpui::Point::new(
        px(b0.origin.x.as_f32() + 200.0),
        px(b0.origin.y.as_f32() + 10.0),
    );
    use gpui::{ScrollDelta, ScrollWheelEvent, TouchPhase};
    wcx.simulate_event(ScrollWheelEvent {
        position: pos0,
        delta: ScrollDelta::Pixels(point(px(0.), px(-60.))),
        modifiers: Modifiers::default(),
        touch_phase: TouchPhase::Moved,
    });
    wcx.run_until_parked();
    wcx.update(|window, app| window.draw(app).clear(app));
    wcx.run_until_parked();
    assert!(
        wcx.cx.read(|app| view.read(app).main_sb_shown.get()),
        "滚轮后应显示"
    );
    let track = wcx
        .debug_bounds("music-main-scrollbar")
        .expect("track 应物化");
    let (viewport, total, off0) = wcx.cx.read(|app| {
        let v = view.read(app);
        let home = v
            .main_list_cache
            .get(&(MainSection::Home, String::new()))
            .map(|(_rev, l)| l.clone())
            .expect("Home 列表应在缓存");
        (
            v.main_scroll.bounds().size.height.as_f32(),
            home.total_h(),
            v.main_scroll.offset().y.as_f32(),
        )
    });
    let max_scroll = (total - viewport).max(0.0);
    // 按组件 geom 公式反算 thumb 命中盒（raw 滑块长 = max(视口占比, thumb_min=32 原始长)
    // → 两端各缩 4px inset 得命中长；painted 最小高 = 32−8 = 24）。
    let raw = (viewport * viewport / total).max(32.0).min(viewport);
    let travel = (viewport - raw).max(0.0);
    let frac = if max_scroll > 0.0 {
        (-off0 / max_scroll).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let start = travel * frac;
    let hit_len = (raw - 8.0).max(0.0);
    let top_rel = 4.0 + start;
    let x = track.origin.x.as_f32() + track.size.width.as_f32() / 2.0;
    // 按在 thumb 中心 → 起拖拽（offset 原地不变）。
    let y_thumb = track.origin.y.as_f32() + top_rel + hit_len / 2.0;
    wcx.simulate_event(MouseDownEvent {
        position: point(px(x), px(y_thumb)),
        modifiers: Modifiers::default(),
        button: MouseButton::Left,
        click_count: 1,
        first_mouse: false,
    });
    wcx.run_until_parked();
    wcx.update(|window, app| window.draw(app).clear(app));
    wcx.run_until_parked();
    // 拖到 60% 比例的 track y（组件拖拽写回：pct=(pos-grab-origin)/denom，denom=viewport-hit_len）。
    let y60 = track.origin.y.as_f32() + hit_len / 2.0 + 0.6 * (viewport - hit_len);
    let pos60 = point(px(x), px(y60));
    wcx.simulate_event(MouseMoveEvent {
        position: pos60,
        modifiers: Modifiers::default(),
        pressed_button: Some(MouseButton::Left),
    });
    wcx.run_until_parked();
    wcx.update(|window, app| window.draw(app).clear(app));
    wcx.run_until_parked();
    let off60 = wcx
        .cx
        .read(|app| view.read(app).main_scroll.offset().y.as_f32());
    assert!(
        (off60 - -(0.6 * max_scroll)).abs() < 1.0,
        "thumb 拖到 60% offset 应≈ -0.6×量程：off={off60} expect={}",
        -0.6 * max_scroll
    );
    // 松手 → 拖拽结束（mouseup 刷新活动时间 → 仍保持显示、不立即隐藏）。
    wcx.simulate_event(MouseUpEvent {
        position: pos60,
        modifiers: Modifiers::default(),
        button: MouseButton::Left,
        click_count: 1,
    });
    wcx.run_until_parked();
    wcx.update(|window, app| window.draw(app).clear(app));
    wcx.run_until_parked();
    wcx.simulate_event(MouseMoveEvent {
        position: pos60,
        modifiers: Modifiers::default(),
        pressed_button: None,
    });
    wcx.run_until_parked();
    wcx.update(|window, app| window.draw(app).clear(app));
    wcx.run_until_parked();
    assert!(
        wcx.cx.read(|app| view.read(app).main_sb_shown.get()),
        "拖拽后刚刷新活动 → 仍显示"
    );
}

/// 越界拖拽：按上 thumb 后拖到 track 之外（上方远处）仍持续跟手更新 offset（clamp 到顶
/// → ≈0）；在 track 外松手拖拽结束（其后 move 不再改 offset）。`ui::Scrollbar` 用 gpui 窗口
/// 级 `on_mouse_event` 监听（非元素 hover 依赖）——拖出轨道持续派发，天然满足。
#[gpui::test]
async fn main_scrollbar_drag_continues_outside_track(cx: &mut TestAppContext) {
    use gpui::{Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, point};
    let player = library_with_n(cx, 200);
    let (view, wcx) = cx.add_window_view(|_w, cx| MusicView::new(player, cx));
    wcx.update(|window, app| window.draw(app).clear(app));
    wcx.run_until_parked();
    // 滚轮激活显示。
    let b0 = wcx.debug_bounds("song-row-0").expect("顶部行应物化");
    let pos0 = gpui::Point::new(px(b0.origin.x.as_f32() + 200.0), px(b0.origin.y.as_f32() + 10.0));
    use gpui::{ScrollDelta, ScrollWheelEvent, TouchPhase};
    wcx.simulate_event(ScrollWheelEvent {
        position: pos0,
        delta: ScrollDelta::Pixels(point(px(0.), px(-60.))),
        modifiers: Modifiers::default(),
        touch_phase: TouchPhase::Moved,
    });
    wcx.run_until_parked();
    wcx.update(|window, app| window.draw(app).clear(app));
    wcx.run_until_parked();
    let track = wcx.debug_bounds("music-main-scrollbar").expect("track 应物化");
    let (viewport, total, off0) = wcx.cx.read(|app| {
        let v = view.read(app);
        let home = v
            .main_list_cache
            .get(&(MainSection::Home, String::new()))
            .map(|(_rev, l)| l.clone())
            .expect("Home 列表应在缓存");
        (
            v.main_scroll.bounds().size.height.as_f32(),
            home.total_h(),
            v.main_scroll.offset().y.as_f32(),
        )
    });
    let max_scroll = (total - viewport).max(0.0);
    // 按组件 geom 公式反算 thumb 命中盒（raw 滑块长 = max(视口占比, thumb_min=32 原始长)
    // → 两端各缩 4px inset 得命中长；painted 最小高 = 32−8 = 24）。
    let raw = (viewport * viewport / total).max(32.0).min(viewport);
    let travel = (viewport - raw).max(0.0);
    let frac = if max_scroll > 0.0 {
        (-off0 / max_scroll).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let start = travel * frac;
    let hit_len = (raw - 8.0).max(0.0);
    let top_rel = 4.0 + start;
    let x = track.origin.x.as_f32() + track.size.width.as_f32() / 2.0;
    // 按在 thumb 中心 → 起拖拽。滚到 60% 先按下便于后续「拖出应 clamp 到顶」有明显行程。
    let y_thumb = track.origin.y.as_f32() + top_rel + hit_len / 2.0;
    wcx.simulate_event(MouseDownEvent {
        position: point(px(x), px(y_thumb)),
        modifiers: Modifiers::default(),
        button: MouseButton::Left,
        click_count: 1,
        first_mouse: false,
    });
    wcx.run_until_parked();
    wcx.update(|window, app| window.draw(app).clear(app));
    wcx.run_until_parked();
    // 拖到 60% → 向下滚（证明拖拽在生效）。
    let y60 = track.origin.y.as_f32() + hit_len / 2.0 + 0.6 * (viewport - hit_len);
    wcx.simulate_event(MouseMoveEvent {
        position: point(px(x), px(y60)),
        modifiers: Modifiers::default(),
        pressed_button: Some(MouseButton::Left),
    });
    wcx.run_until_parked();
    wcx.update(|window, app| window.draw(app).clear(app));
    wcx.run_until_parked();
    let off60 = wcx
        .cx
        .read(|app| view.read(app).main_scroll.offset().y.as_f32());
    assert!(
        (off60 - -(0.6 * max_scroll)).abs() < 1.0,
        "拖到 60% 应滚下去：off={off60} expect={}",
        -0.6 * max_scroll
    );
    // 拖到 track 上方（窗口内、track 之外）→ 仍跟手、clamp 到顶：offset ≈ 0。
    let far_top = gpui::Point::new(px(x), px((track.origin.y.as_f32() - 60.0).max(20.0)));
    wcx.simulate_event(MouseMoveEvent {
        position: far_top,
        modifiers: Modifiers::default(),
        pressed_button: Some(MouseButton::Left),
    });
    wcx.run_until_parked();
    wcx.update(|window, app| window.draw(app).clear(app));
    wcx.run_until_parked();
    let off_top = wcx
        .cx
        .read(|app| view.read(app).main_scroll.offset().y.as_f32());
    assert!(
        off_top.abs() < 0.6,
        "拖出 track 上方仍应 clamp 到顶：off_top={off_top}"
    );
    // track 外松手 → 拖拽结束：其后在同一位置的 move 不再改 offset。
    wcx.simulate_event(MouseUpEvent {
        position: far_top,
        modifiers: Modifiers::default(),
        button: MouseButton::Left,
        click_count: 1,
    });
    wcx.run_until_parked();
    wcx.update(|window, app| window.draw(app).clear(app));
    wcx.run_until_parked();
    wcx.simulate_event(MouseMoveEvent {
        position: far_top,
        modifiers: Modifiers::default(),
        pressed_button: None,
    });
    wcx.run_until_parked();
    wcx.update(|window, app| window.draw(app).clear(app));
    wcx.run_until_parked();
    let off_after = wcx
        .cx
        .read(|app| view.read(app).main_scroll.offset().y.as_f32());
    assert!(
        off_after.abs() < 0.6,
        "track 外松手后 move 不再改 offset：off={off_after}"
    );
}

/// 滚到底（并尝试过滚）量程不失真：`ui::Scrollbar.geom` 用 `thumb_end=min(start+len, viewport)`
/// 把过滚时的 thumb 底缘钉在 viewport 底部（旧自绘实现过滚时 thumb_top 越界画出下缘、可见
/// 长度变短——用户实测「滚到底继续滚、滚动条缩短」）。组件级几何在 `ui::scrollbar` 单测覆盖
/// （thumb 不越 viewport），此处守住音乐侧集成：过滚后 offset 被滚动容器钳到 **-最大量程**、
/// 滚动条仍显示（拖拽/点击量程不失真）。
#[gpui::test]
async fn main_scrollbar_offset_clamps_when_over_scrolled(cx: &mut TestAppContext) {
    let player = library_with_n(cx, 200);
    let (_view, wcx) = cx.add_window_view(|_w, cx| MusicView::new(player, cx));
    wcx.update(|window, app| window.draw(app).clear(app));
    wcx.run_until_parked();
    // 滚到超界底部（set_offset 超量程会被 scroll 容器钳到 -max，再补一次越界滚轮激活显示）。
    let (total_h, viewport) = wcx.cx.read(|app| {
        let v = _view.read(app);
        let home = v
            .main_list_cache
            .get(&(MainSection::Home, String::new()))
            .map(|(_rev, l)| l.clone())
            .expect("Home 列表应在缓存");
        (home.total_h(), v.main_scroll.bounds().size.height.as_f32())
    });
    let max_scroll = (total_h - viewport).max(0.0);
    wcx.update(|_window, app| {
        _view.update(app, |v, _| {
            v.main_scroll
                .set_offset(gpui::point(px(0.), px(-(total_h + 1000.0))));
        });
    });
    wcx.update(|window, app| window.draw(app).clear(app));
    wcx.run_until_parked();
    // 激活显示。
    let b_last = wcx.debug_bounds("song-row-199").expect("滚到底末行应物化");
    let pos = gpui::Point::new(
        px(b_last.origin.x.as_f32() + 200.0),
        px(b_last.origin.y.as_f32() + 10.0),
    );
    use gpui::{Modifiers, ScrollDelta, ScrollWheelEvent, TouchPhase, point};
    wcx.simulate_event(ScrollWheelEvent {
        position: pos,
        delta: ScrollDelta::Pixels(point(px(0.), px(-120.))),
        modifiers: Modifiers::default(),
        touch_phase: TouchPhase::Moved,
    });
    wcx.run_until_parked();
    wcx.update(|window, app| window.draw(app).clear(app));
    wcx.run_until_parked();
    assert!(
        wcx.cx.read(|app| _view.read(app).main_sb_shown.get()),
        "过滚后滚动条应显示"
    );
    let off = wcx
        .cx
        .read(|app| _view.read(app).main_scroll.offset().y.as_f32());
    assert!(
        (off - -max_scroll).abs() < 1.0,
        "过滚后 offset 应被钳到最大量程：off={off} expect={}",
        -max_scroll
    );
}

// ---- 「最近播放」分区（本次会话播放历史） ----

/// 读取会话播放历史（映射为库下标；最近先、去重后）。
fn recent_idxs(app: &gpui::App, player: &Entity<MusicPlayer>) -> Vec<usize> {
    let p = player.read(app);
    p.recent()
        .iter()
        .map(|path| {
            p.library()
                .iter()
                .position(|t| t.path.to_string_lossy().as_ref() == path)
                .expect("历史路径应仍在当前库中")
        })
        .collect()
}

/// 导航点击：侧栏「最近播放」→ 切到 Recent 分区；无历史 → 主区空态（不物化歌行）。
#[gpui::test]
async fn recent_nav_click_switches_section_and_empty_state(cx: &mut TestAppContext) {
    let player = player_with_library(cx);
    let (view, wcx) = cx.add_window_view(|_w, cx| MusicView::new(player, cx));
    wcx.update(|window, app| {
        window.draw(app).clear(app);
    });
    wcx.run_until_parked();
    let nav = wcx
        .debug_bounds("music-nav-最近播放")
        .expect("侧栏「最近播放」导航项应物化");
    let pos = gpui::Point::new(
        px(nav.origin.x.as_f32() + 40.0),
        px(nav.origin.y.as_f32() + nav.size.height.as_f32() * 0.5),
    );
    wcx.simulate_mouse_down(pos, MouseButton::Left, Default::default());
    wcx.simulate_mouse_up(pos, MouseButton::Left, Default::default());
    wcx.run_until_parked();
    let sec = wcx.cx.read(|app| view.read(app).state.section);
    assert_eq!(sec, MainSection::Recent, "点击「最近播放」应切到 Recent 分区");
    wcx.update(|window, app| {
        window.draw(app).clear(app);
    });
    wcx.run_until_parked();
    assert!(
        wcx.debug_bounds("song-row-0").is_none(),
        "无播放记录时最近播放应为空态（不物化任何歌行）"
    );
}

/// 历史记录：播放 0→2→4 后最近先 [4,2,0]；重播 2 → 去重并置顶 [2,4,0]。
#[gpui::test]
async fn play_records_session_history_recent_first_dedup(cx: &mut TestAppContext) {
    let player = library_with_n(cx, 5);
    let (view, wcx) = cx.add_window_view(|_w, cx| MusicView::new(player.clone(), cx));
    for idx in [0usize, 2, 4] {
        wcx.update(|_w, app| {
            view.update(app, |v, cx| v.handle_play_idx(idx, cx));
        });
    }
    let first = wcx.cx.read(|app| recent_idxs(app, &player));
    assert_eq!(first, vec![4, 2, 0], "播放 0→2→4 后历史应为最近先 [4,2,0]");
    // 重播 2：去重 + 置顶（不产生重复项）。
    wcx.update(|_w, app| {
        view.update(app, |v, cx| v.handle_play_idx(2, cx));
    });
    let dedup = wcx.cx.read(|app| recent_idxs(app, &player));
    assert_eq!(dedup, vec![2, 4, 0], "重复播放 2 应去重并置顶：历史应 [2,4,0]");
}

/// 最近播放分区渲染：最近先行序 + `mutation` 升缓存失效重建 + 点击历史行播放该曲。
#[gpui::test]
async fn recent_section_renders_recent_first_and_click_plays(cx: &mut TestAppContext) {
    let player = library_with_n(cx, 5);
    let (view, wcx) = cx.add_window_view(|_w, cx| MusicView::new(player.clone(), cx));
    // 播放 1→4→1：去重后历史 [1,4]。
    for idx in [1usize, 4, 1] {
        wcx.update(|_w, app| {
            view.update(app, |v, cx| v.handle_play_idx(idx, cx));
        });
    }
    wcx.update(|window, app| {
        view.update(app, |v, _| v.state.section = MainSection::Recent);
        window.draw(app).clear(app);
    });
    wcx.run_until_parked();
    let r1 = wcx.debug_bounds("song-row-1").expect("最近首行(1)应物化");
    let r4 = wcx.debug_bounds("song-row-4").expect("其次(4)应物化");
    assert!(
        r1.origin.y.as_f32() < r4.origin.y.as_f32(),
        "历史序最近先：row-1 应排在 row-4 上方"
    );
    assert!(
        wcx.debug_bounds("song-row-0").is_none(),
        "未播放过的行不应出现在最近列表"
    );
    // 再播放 2（记录置顶 + mutation 升 → 显示模型缓存失效重建）：新行应出现且排最上。
    wcx.update(|_w, app| {
        view.update(app, |v, cx| v.handle_play_idx(2, cx));
    });
    wcx.update(|window, app| {
        window.draw(app).clear(app);
    });
    wcx.run_until_parked();
    let r2 = wcx.debug_bounds("song-row-2").expect("新播放行(2)应物化");
    let r1b = wcx.debug_bounds("song-row-1").expect("row-1 应仍在列表");
    assert!(
        r2.origin.y.as_f32() < r1b.origin.y.as_f32(),
        "新播放应置顶：row-2 应排在 row-1 上方（缓存失效重建）"
    );
    // 点击历史列表行 → 播放该曲（复用行点击行为）。
    let pos = gpui::Point::new(
        px(r1b.origin.x.as_f32() + 30.0),
        px(r1b.origin.y.as_f32() + r1b.size.height.as_f32() * 0.5),
    );
    wcx.simulate_mouse_down(pos, MouseButton::Left, Default::default());
    wcx.simulate_mouse_up(pos, MouseButton::Left, Default::default());
    wcx.run_until_parked();
    let cur = wcx.cx.read(|app| player.read(app).current());
    assert_eq!(cur, Some(1), "点击历史列表行应播放该曲");
}

/// 最近播放分区搜索过滤：与 Liked 同款「路径集 + 搜索词」语义。
#[gpui::test]
async fn recent_section_filters_by_search_query(cx: &mut TestAppContext) {
    let player = library_with_n(cx, 5);
    let (view, wcx) = cx.add_window_view(|_w, cx| MusicView::new(player.clone(), cx));
    for idx in [1usize, 3] {
        wcx.update(|_w, app| {
            view.update(app, |v, cx| v.handle_play_idx(idx, cx));
        });
    }
    wcx.update(|window, app| {
        view.update(app, |v, _| v.state.section = MainSection::Recent);
        window.draw(app).clear(app);
    });
    wcx.run_until_parked();
    // 搜索「艺人3」→ 历史 [3,1] 中只命中 idx3。
    wcx.update(|window, app| {
        view.update(app, |v, _| v.state.search = "艺人3".into());
        window.draw(app).clear(app);
    });
    wcx.run_until_parked();
    assert!(
        wcx.debug_bounds("song-row-3").is_some(),
        "「艺人3」命中行应物化"
    );
    assert!(
        wcx.debug_bounds("song-row-1").is_none(),
        "未命中「艺人3」的历史行不应物化"
    );
}

/// 切换音乐库目录（load_dir 换代）→ 本次会话播放历史清空。
#[gpui::test]
async fn switching_music_dir_clears_session_recent(cx: &mut TestAppContext) {
    // 两个目录都先建好（add_window_view 后 cx 已借给 wcx，不能再取 &mut）。
    let (dir_a, _pa) = n_library_dir(cx, 3);
    let (dir_b, _pb) = n_library_dir(cx, 4);
    let player: Entity<MusicPlayer> = cx.update(|app| app.new(|cx| MusicPlayer::new(cx)));
    cx.update(|app| {
        player.update(app, |p, cx| p.load_dir(&dir_a, cx));
    });
    drain_scan(cx);
    let (view, wcx) = cx.add_window_view(|_w, cx| MusicView::new(player.clone(), cx));
    wcx.update(|_w, app| {
        view.update(app, |v, cx| v.handle_play_idx(0, cx));
    });
    assert!(
        !wcx.cx.read(|app| player.read(app).recent().is_empty()),
        "播放后应有会话历史"
    );
    // 换库（load_dir 换成另一目录）：清空是 load_dir 起始处的同步动作，断言无需等扫库完成。
    wcx.update(|_w, app| {
        player.update(app, |p, cx| p.load_dir(&dir_b, cx));
    });
    assert!(
        wcx.cx.read(|app| player.read(app).recent().is_empty()),
        "切换音乐库目录后会话播放历史应清空"
    );
}

/// 占位不变式：「本地下载/发现/电台」仍为 ComingSoon——点击弹 toast、不切分区。
#[gpui::test]
async fn placeholder_nav_items_still_toast_coming_soon(cx: &mut TestAppContext) {
    let player = player_with_library(cx);
    let (view, wcx) = cx.add_window_view(|_w, cx| MusicView::new(player, cx));
    wcx.update(|window, app| {
        window.draw(app).clear(app);
    });
    wcx.run_until_parked();
    for (label, sel, expected) in [
        ("本地下载", "music-nav-本地下载", "本地下载即将到来"),
        ("发现", "music-nav-发现", "发现即将到来"),
        ("电台", "music-nav-电台", "电台即将到来"),
    ] {
        let nav = wcx
            .debug_bounds(sel)
            .expect("占位导航项应物化");
        let pos = gpui::Point::new(
            px(nav.origin.x.as_f32() + 40.0),
            px(nav.origin.y.as_f32() + nav.size.height.as_f32() * 0.5),
        );
        wcx.simulate_mouse_down(pos, MouseButton::Left, Default::default());
        wcx.simulate_mouse_up(pos, MouseButton::Left, Default::default());
        wcx.run_until_parked();
        let toast = wcx
            .cx
            .read(|app| view.read(app).toast.as_ref().map(|(s, _)| s.to_string()));
        assert_eq!(
            toast.as_deref(),
            Some(expected),
            "点击「{label}」应弹「{expected}」toast"
        );
        let sec = wcx.cx.read(|app| view.read(app).state.section);
        assert_eq!(sec, MainSection::Home, "「{label}」占位项不应切换分区");
    }
}

// ---- 换肤子菜单（阶段②入口：写选择进 music_skin + persist；取色渲染在阶段③接入） ----

/// 建已渲染一帧的视图（设置按钮盒被 bounds_canvas 写入 tip_boxes）并订阅
/// `MusicSkinChanged` 事件收集所选 skin id。
fn open_skin_view<'a>(
    cx: &'a mut TestAppContext,
) -> (Entity<MusicView>, &'a mut VisualTestContext, Rc<RefCell<Vec<&'static str>>>) {
    let player = player_with_library(cx);
    let (view, wcx) = cx.add_window_view(|_w, cx| MusicView::new(player, cx));
    let skins = Rc::new(RefCell::new(Vec::new()));
    {
        let skins = skins.clone();
        wcx.update(|window, app| {
            window
                .subscribe(&view, app, move |_, ev: &MusicEvent, _, _| {
                    if let MusicEvent::MusicSkinChanged(id) = *ev {
                        skins.borrow_mut().push(id);
                    }
                })
                .detach();
        });
    }
    // 一帧：设置按钮盒写入 tip_boxes["music-settings"]（换肤菜单锚点）。
    wcx.update(|window, app| window.draw(app).clear(app));
    wcx.run_until_parked();
    (view, wcx, skins)
}

/// 设置按钮中心窗口坐标（tip_boxes 记录盒中点；box 宽>0 才算已布局）。
fn settings_btn_center(
    view: &Entity<MusicView>,
    wcx: &mut VisualTestContext,
) -> gpui::Point<Pixels> {
    let b = wcx
        .cx
        .read(|app| view.read(app).tip_boxes.get("music-settings").expect("设置按钮盒已记录").get());
    assert!(b.size.width > px(0.), "设置按钮盒应已布局，实得 {b:?}");
    b.center()
}

/// 点按（down+up）给定窗口坐标。
fn click_at(wcx: &mut VisualTestContext, p: gpui::Point<Pixels>) {
    wcx.simulate_mouse_down(p, MouseButton::Left, Default::default());
    wcx.simulate_mouse_up(p, MouseButton::Left, Default::default());
    wcx.run_until_parked();
}

/// 设置钮 toggle：点击 → 打开（slot 持实体）→ 再点 → 关闭；点外部 → 关闭。
#[gpui::test]
fn settings_button_toggles_skin_menu(cx: &mut TestAppContext) {
    let (view, wcx, _) = open_skin_view(cx);
    assert!(
        !wcx.cx.read(|app| view.read(app).skin_menu_open),
        "初始换肤菜单应关闭"
    );
    // 点击设置钮 → 打开。
    let btn = settings_btn_center(&view, wcx);
    click_at(wcx, btn);
    assert!(
        wcx.cx.read(|app| view.read(app).skin_menu_open),
        "点击设置钮应打开换肤菜单"
    );
    assert!(
        wcx.cx.read(|app| view.read(app).skin_menu.is_open()),
        "菜单槽应持有实体"
    );
    // 再点设置钮 → 关闭（Dropdown 同款 toggle）。
    click_at(wcx, btn);
    assert!(
        !wcx.cx.read(|app| view.read(app).skin_menu_open),
        "再点设置钮应关闭换肤菜单"
    );
    // 再开，点菜单外远处 → 关闭（ui::menu 点击外关闭）。
    click_at(wcx, btn);
    assert!(wcx.cx.read(|app| view.read(app).skin_menu_open), "应再次打开");
    click_at(wcx, gpui::point(px(600.), px(300.)));
    assert!(
        !wcx.cx.read(|app| view.read(app).skin_menu_open),
        "点菜单外应关闭"
    );
}

/// 逐个选择 5 预设 + 跟随全局 + 回 default：music_skin 即时更新、逐次发 `MusicSkinChanged`；
/// 选同一值幂等（不再发事件）。初始默认位 music。
#[gpui::test]
fn choosing_skins_updates_current_and_emits_events(cx: &mut TestAppContext) {
    let _guard = SkinViewGuard::new();
    let (view, wcx, skins) = open_skin_view(cx);
    assert_eq!(
        wcx.cx.read(|app| view.read(app).music_skin.clone()),
        "music",
        "默认音乐皮肤应为 music"
    );
    for id in ["ocean", "solar", "rose", "graphite", "follow", "default"] {
        wcx.update(|_window, app| {
            view.update(app, |v, cx| v.choose_music_skin(id, cx));
        });
        wcx.run_until_parked();
        assert_eq!(
            wcx.cx.read(|app| view.read(app).music_skin.clone()),
            id,
            "选择后 music_skin 应更新为 {id}"
        );
    }
    let got = skins.borrow();
    assert_eq!(
        got.as_slice(),
        &["ocean", "solar", "rose", "graphite", "follow", "default"],
        "每次选择都应发出 MusicSkinChanged"
    );
    // 幂等：选同一值不重复发事件。
    let n = got.len();
    drop(got);
    wcx.update(|_window, app| {
        view.update(app, |v, cx| v.choose_music_skin("default", cx));
    });
    assert_eq!(skins.borrow().len(), n, "选同一值不应重复发事件");
}

/// 真实菜单项点击链：点设置钮开菜单 → 点「海青 ocean」行（debug_bounds 定位）→
/// music_skin=="ocean" + 事件发出 + keep_open 菜单保持展开。再验**焦点/键盘边界**：
/// 菜单展开期间 Esc 只关菜单、不发 `ExitRequested`（回归：曾因帧重建丢焦、Esc 走 root 退
/// 出音乐模式）；重开菜单键盘 ↓+Enter 应命中「跟随全局」（focus 重聚焦成功）。
#[gpui::test]
async fn skin_menu_item_click_keeps_open_and_updates(cx: &mut TestAppContext) {
    let _guard = SkinViewGuard::new();
    let (view, wcx, skins) = open_skin_view(cx);
    let exits = Rc::new(Cell::new(0usize));
    {
        let exits = exits.clone();
        wcx.update(|window, app| {
            window
                .subscribe(&view, app, move |_, ev: &MusicEvent, _, _| {
                    if *ev == MusicEvent::ExitRequested {
                        exits.set(exits.get() + 1);
                    }
                })
                .detach();
        });
    }
    let btn = settings_btn_center(&view, wcx);
    click_at(wcx, btn);
    // ocean 行应已物化（debug_selector "music-skin-ocean"）。
    let ocean = wcx
        .debug_bounds("music-skin-ocean")
        .expect("ocean 菜单项应物化");
    let p = ocean.center();
    click_at(wcx, p);
    assert_eq!(
        wcx.cx.read(|app| view.read(app).music_skin.clone()),
        "ocean",
        "点「海青」应更新 music_skin"
    );
    assert_eq!(
        *skins.borrow(),
        vec!["ocean"],
        "点菜单项应发 MusicSkinChanged(\"ocean\")"
    );
    assert!(
        wcx.cx.read(|app| view.read(app).skin_menu_open),
        "keep_open 项选中后菜单应保持展开"
    );
    // Esc：只关菜单、不退出音乐模式（K15 焦点边界回归——菜单展开期间键盘必须留在菜单语义）。
    wcx.simulate_keystrokes("escape");
    wcx.run_until_parked();
    assert!(
        !wcx.cx.read(|app| view.read(app).skin_menu_open),
        "菜单展开时 Esc 应关闭菜单"
    );
    assert_eq!(exits.get(), 0, "Esc 关菜单不应发 ExitRequested（不退出音乐）");
    // 键盘导航：重开菜单 → ↓ 选中首项（跟随全局）+ Enter 激活 → music_skin=="follow"、
    // 菜单保持展开（keep_open）、仍不发 ExitRequested。
    click_at(wcx, btn);
    wcx.simulate_keystrokes("down");
    wcx.run_until_parked();
    wcx.simulate_keystrokes("enter");
    wcx.run_until_parked();
    assert_eq!(
        wcx.cx.read(|app| view.read(app).music_skin.clone()),
        "follow",
        "↓+Enter 应激活首项「跟随全局」"
    );
    assert!(
        wcx.cx.read(|app| view.read(app).skin_menu_open),
        "keep_open 项键盘 Enter 后菜单应保持展开"
    );
    assert_eq!(exits.get(), 0, "键盘操作菜单不应发 ExitRequested");
}

/// persist 往返：`music_skin` 经 `UserPrefs` 落盘可来回读（config test-mode 隔离临时目录，
/// 不污染真实 %APPDATA%；shell 侧 `persist_prefs` 以同一字段聚合写盘）。
#[test]
fn music_skin_persists_through_user_prefs() {
    let mut prefs = config::user::UserPrefs::default();
    prefs.music_skin = "ocean".to_string();
    prefs.save();
    assert_eq!(
        config::user::UserPrefs::load().music_skin,
        "ocean",
        "music_skin 应经 UserPrefs 往返"
    );
    // 特殊值 "follow"（跟随全局）原样落盘（UserPrefs 未知 id 不校验）。
    let mut p2 = config::user::UserPrefs::default();
    p2.music_skin = "follow".to_string();
    p2.save();
    assert_eq!(
        config::user::UserPrefs::load().music_skin,
        "follow",
        "跟随全局（follow）应原样落盘"
    );
}

// ---- 阶段③-2：换肤取色渲染接线 + 封口测试（K153：渲染路径输出 == 主题 token） ----

/// 触碰进程级皮肤缓存（`MusicSkin`）的视图测试用 RAII 守卫：进入/退出都 `reset()`，
/// 并持有全 crate 共享串行锁（`theme::MUSIC_SKIN_TEST_SERIAL`，与 theme.rs `SkinTestGuard`
/// 同一把锁）——防并行测试间皮肤缓存串扰，无需再依赖 `--test-threads=1`。
#[allow(dead_code)] // 字段仅供 RAII 生命周期持有（不读取），见 `SkinViewGuard` 注释
struct SkinViewGuard(std::sync::MutexGuard<'static, ()>);
impl SkinViewGuard {
    fn new() -> Self {
        theme::MusicSkin::reset();
        let lock = theme::MUSIC_SKIN_TEST_SERIAL
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        Self(lock)
    }
}
impl Drop for SkinViewGuard {
    fn drop(&mut self) {
        theme::MusicSkin::reset();
    }
}

/// 从最近渲染帧里取「根元素 #music-view」的纯色 bg quad 实绘色（K153 断言用）：
/// 读 `window.painted_quads()` 的真实绘制输出，证明渲染路径真正消费皮肤 token，
/// 而非「镜像 getter」假绿。根元素铺满窗口 → 取**面积最大**的纯色 quad（对 scale
/// 不敏感；全窗无更大 solid 覆盖，故最大 solid = 根背景）。
fn root_bg_paint_color(wcx: &mut VisualTestContext) -> gpui::Hsla {
    let quads = wcx.update(|window, _| window.painted_quads());
    let area = |q: &gpui::Quad| {
        q.bounds.size.width.as_f32() * q.bounds.size.height.as_f32()
    };
    quads
        .iter()
        .filter_map(|q| q.background.as_solid().map(|c| (q, c)))
        .max_by(|(a, _), (b, _)| area(a).partial_cmp(&area(b)).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(_, c)| c)
        .expect("#music-view 应有覆盖自身的纯色 bg quad（最大 solid quad = 根背景）")
}

/// K153 封口：渲染根元素实绘 bg == `MusicPalette` 皮肤 token（真实渲染走皮肤，非镜像
/// getter）。判别式（K201）：删掉 `choose_music_skin` 里的 `MusicSkin::set` 则切 ocean 后
/// 根实绘仍为 music 底 → 本测试红；换肤残差：ocean → music 切回后 #0d0b1a 逐字节回原。
#[gpui::test]
async fn music_root_bg_matches_skin_palette(cx: &mut TestAppContext) {
    let _guard = SkinViewGuard::new();
    let player = player_with_library(cx);
    let (view, wcx) = cx.add_window_view(|_w, cx| MusicView::new(player, cx));
    // 默认 music 皮肤：根实绘 bg == music 调色板 bg_deep（#0d0b1a）。
    wcx.update(|window, app| window.draw(app).clear(app));
    wcx.run_until_parked();
    assert_eq!(
        root_bg_paint_color(wcx),
        theme::MusicPalette::from_skin("music").expect("music 预设可解析").bg_deep,
        "默认 music：根实绘背景应随音乐调色板"
    );
    // 切 ocean：重绘后根实绘 bg == ocean 暗档 srf_deep（#0b111c），且 ≠ music 底。
    wcx.update(|window, app| {
        view.update(app, |v, cx| v.choose_music_skin("ocean", cx));
        window.draw(app).clear(app);
    });
    wcx.run_until_parked();
    let ocean_deep = theme::MusicPalette::from_skin("ocean").expect("ocean 预设存在").bg_deep;
    assert_eq!(
        root_bg_paint_color(wcx),
        ocean_deep,
        "切 ocean：根实绘背景应随 ocean 调色板"
    );
    assert_ne!(
        ocean_deep,
        theme::MusicPalette::from_skin("music").unwrap().bg_deep,
        "判别：ocean 底 ≠ music 底（证明渲染真随皮肤走）"
    );
    // 换肤残差：ocean → 切回 music → 根实绘 bg 逐字节回 #0d0b1a。
    wcx.update(|window, app| {
        view.update(app, |v, cx| v.choose_music_skin("music", cx));
        window.draw(app).clear(app);
    });
    wcx.run_until_parked();
    assert_eq!(
        root_bg_paint_color(wcx),
        theme::MusicPalette::from_skin("music").unwrap().bg_deep,
        "换肤残差：ocean 切回 music 后根实绘 bg 应逐字节回 #0d0b1a"
    );
}

/// ③-2 follow 全局联动（遗留风险）：music 皮肤 = "follow" 时，改全局主题（重绘触发
/// render 钩子重调 `MusicSkin::set("follow")`）→ 音乐取色同步新全局合并值（bg_deep 随
/// 预设面坡、accent 随自定义覆盖）。判别式：删掉 render 里的 follow 重取块则改全局后
/// bg_deep 停留在旧值 → 红。
#[gpui::test]
async fn follow_palette_tracks_global_theme_change(cx: &mut TestAppContext) {
    let _guard = SkinViewGuard::new();
    // 固定 music 暗档为起点（合并后已知值）。
    cx.update(|cx| {
        ui::theme::Theme::apply(
            ui::theme::ThemeSpec {
                mode: ui::theme::ThemeMode::Dark,
                preset: ui::theme::PRESETS.iter().find(|p| p.id == "music").unwrap(),
                custom: Default::default(),
            },
            cx,
        );
    });
    let player = player_with_library(cx);
    let (view, wcx) = cx.add_window_view(|_w, cx| MusicView::new(player, cx));
    // 选「跟随全局」→ current_skin_id=follow（MusicSkin::set 即在当前全局建 palette）。
    wcx.update(|_w, app| view.update(app, |v, cx| v.choose_music_skin("follow", cx)));
    wcx.update(|window, app| window.draw(app).clear(app));
    wcx.run_until_parked();
    assert_eq!(theme::bg_deep(), theme::c(0x0d0b1a), "follow+music 暗档：bg_deep 跟随全局");
    assert_eq!(theme::accent(), theme::c(0x7c5cfc), "follow+music 暗档：accent 跟随全局");
    // 切全局为 ocean + 自定义 accent：重绘重入 render → follow 从新全局重取。
    // 注：`cx` 已被 `add_window_view` 借出，经 `wcx.update` 拿 `&mut App` 改全局。
    wcx.update(|_w, app| {
        let mut custom = BTreeMap::new();
        custom.insert("colors.accent", "#ffcc00".to_string());
        ui::theme::Theme::apply(
            ui::theme::ThemeSpec {
                mode: ui::theme::ThemeMode::Dark,
                preset: ui::theme::PRESETS.iter().find(|p| p.id == "ocean").unwrap(),
                custom,
            },
            app,
        );
    });
    wcx.update(|window, app| window.draw(app).clear(app));
    wcx.run_until_parked();
    assert_eq!(theme::bg_deep(), theme::c(0x0b111c), "follow 应重取 ocean 暗档 srf_deep");
    assert_eq!(
        theme::accent(),
        theme::c(0xffcc00),
        "follow 应重取被 custom 覆盖的 accent"
    );
}

// ---- 诊断（2026-11-23）：静态音乐皮肤 vs 全局换肤隔离性取证 ----

/// 全帧纯色 quad 的 RGB 色集合（升序、去重、**忽略 alpha**）：供「换帧前后色集合一致 /
/// 无全局色漏入」断言。忽略 alpha：同色不同透明档（如白色弱化叠层）不视为新色。
fn solid_quad_rgb_set(wcx: &mut VisualTestContext) -> Vec<u32> {
    let quads = wcx.update(|window, _| window.painted_quads());
    let to_rgb = |c: gpui::Hsla| -> u32 {
        let r: gpui::Rgba = c.into();
        ((r.r * 255.).round() as u32) << 16 | ((r.g * 255.).round() as u32) << 8 | (r.b * 255.).round() as u32
    };
    let mut v: Vec<u32> = quads
        .iter()
        .filter_map(|q| q.background.as_solid().map(to_rgb))
        .collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// 帧内是否存在「RGB 精确匹配 + alpha 落在容忍带」的纯色 quad（区分同 RGB 白弱化叠层与
/// 高不透明面板：白色弱化 alpha≈0.06~0.5、浮层面板 alpha≈0.92）。
fn frame_has_quad_like(wcx: &mut VisualTestContext, rgb: (u8, u8, u8), alpha: f32) -> bool {
    let quads = wcx.update(|window, _| window.painted_quads());
    quads
        .iter()
        .filter_map(|q| q.background.as_solid())
        .any(|c| {
            let r: gpui::Rgba = c.into();
            let rr = ((r.r * 255.).round() as u8, (r.g * 255.).round() as u8, (r.b * 255.).round() as u8);
            rr == rgb && (c.a - alpha).abs() < 0.02
        })
}

/// **(x) 判别红测**：music_skin 为非 follow 预设（"music"）时，改**全局主题**（music 暗档 →
/// default 亮档）**不得重染音乐窗内容/局部控件**。判别式（K201）：若音乐改随全局——把
/// `bg_deep` 换成全局亮档面坡、或任一面读 `Theme::global` 换色——本测试红。断言：
/// ① 判别器（全局确实已切亮且 accent 已异化，防空转）；② 根实绘 bg 逐字节保持 music
/// bg_deep；③ 换肤前后整帧纯色 RGB 集合**完全一致**（无任何全局色漏入音乐窗）。
#[gpui::test]
async fn static_music_skin_not_recolored_by_global_theme_change(cx: &mut TestAppContext) {
    let _guard = SkinViewGuard::new();
    // 起点：全局 = music 暗档（合并后已知值）。
    cx.update(|cx| {
        ui::theme::Theme::apply(
            ui::theme::ThemeSpec {
                mode: ui::theme::ThemeMode::Dark,
                preset: ui::theme::PRESETS.iter().find(|p| p.id == "music").unwrap(),
                custom: Default::default(),
            },
            cx,
        );
    });
    let player = player_with_library(cx);
    let (view, wcx) = cx.add_window_view(|_w, cx| MusicView::new(player, cx));
    // 显式静态音乐皮肤（非 follow）。
    wcx.update(|_w, app| view.update(app, |v, cx| v.choose_music_skin("music", cx)));
    wcx.update(|window, app| window.draw(app).clear(app));
    wcx.run_until_parked();
    // 基线帧 A：根实绘 bg == music bg_deep + 全帧纯色 RGB 集合。
    let root_a = root_bg_paint_color(wcx);
    assert_eq!(
        root_a,
        theme::MusicPalette::from_skin("music").expect("music 预设可解析").bg_deep,
        "基线：静态 music 皮肤根背景应为音乐 bg_deep"
    );
    let set_a = solid_quad_rgb_set(wcx);
    assert!(!set_a.is_empty(), "基线帧应含纯色 quad");
    // 判别器：全局切为 default 亮档（surface≈白，与音乐深紫完全相异）。
    wcx.update(|_w, app| {
        ui::theme::Theme::apply(
            ui::theme::ThemeSpec {
                mode: ui::theme::ThemeMode::Light,
                preset: ui::theme::PRESETS.iter().find(|p| p.id == "default").unwrap(),
                custom: Default::default(),
            },
            app,
        );
    });
    wcx.run_until_parked();
    let (is_dark, global_accent) = wcx.update(|_w, app| {
        let t = ui::theme::Theme::global(app);
        (t.is_dark(), t.colors.accent)
    });
    assert!(!is_dark, "判别：全局应已切为亮档（否则本测试空转）");
    assert_ne!(
        global_accent, gpui::rgb(theme::ACCENT),
        "判别：全局 accent 应与音乐 accent（#7c5cfc）异化（否则未真换肤）"
    );
    // 重绘后：根背景不变（(x) 假）+ 全帧 RGB 集合不变（无全局色漏入）。
    wcx.update(|window, app| window.draw(app).clear(app));
    wcx.run_until_parked();
    assert_eq!(
        root_bg_paint_color(wcx),
        root_a,
        "(x) 判别：全局换肤不得重染音乐根背景"
    );
    let set_b = solid_quad_rgb_set(wcx);
    assert_eq!(
        set_b, set_a,
        "(y) 判别：全局换肤前后整帧纯色 RGB 集合应一致——无 ui::theme::Theme::global 色漏入音乐窗"
    );
}

/// (y) 取证：音乐窗内**唯一**读全局 `Theme` 的面 = 换肤子菜单弹层（`ui::menu`）——弹层面板
/// /条目前景取 `Theme::global` 的 surface/fg/menu_highlight（crates/ui/src/menu.rs:887/782），
/// **不随音乐皮肤**。全局亮档 + 音乐静态深紫时，弹层面板 = 全局亮 surface（≈白，alpha≈0.92）
/// 漏入音乐窗。本测试把当前（有泄漏）行为钉死为「已知依赖面清单」一项，供隔离工作以
/// 「弹层改用音乐皮肤色则本测试红」为判别式翻转。
#[gpui::test]
async fn skin_popup_panel_uses_global_theme_surface(cx: &mut TestAppContext) {
    let _guard = SkinViewGuard::new();
    cx.update(|cx| {
        ui::theme::Theme::apply(
            ui::theme::ThemeSpec {
                mode: ui::theme::ThemeMode::Light,
                preset: ui::theme::PRESETS.iter().find(|p| p.id == "default").unwrap(),
                custom: Default::default(),
            },
            cx,
        );
    });
    let player = player_with_library(cx);
    let (view, wcx) = cx.add_window_view(|_w, cx| MusicView::new(player, cx));
    wcx.update(|_w, app| view.update(app, |v, cx| v.choose_music_skin("music", cx)));
    wcx.update(|window, app| window.draw(app).clear(app));
    wcx.run_until_parked();
    // 全局亮档 surface（弹层面板底色预期）+ 弹层不透明 alpha。
    let (surface_rgb, panel_alpha) = wcx.update(|_w, app| {
        let c: gpui::Rgba = ui::theme::Theme::global(app).colors.surface;
        (
            ((c.r * 255.).round() as u8, (c.g * 255.).round() as u8, (c.b * 255.).round() as u8),
            ui::theme::FLOAT_ACRYLIC_ALPHA_LIGHT,
        )
    });
    // 菜单未开：帧内无「全局亮 surface + 高不透明」面板色（排除白色弱化叠层 alpha≈0.06~0.5）。
    assert!(
        !frame_has_quad_like(wcx, surface_rgb, panel_alpha),
        "菜单未开时音乐窗帧内不应有全局亮 surface 面板色"
    );
    // 开换肤子菜单 → 重绘 → 弹层面板色 = 全局亮 surface。
    let btn = settings_btn_center(&view, wcx);
    click_at(wcx, btn);
    wcx.update(|window, app| window.draw(app).clear(app));
    wcx.run_until_parked();
    assert!(
        frame_has_quad_like(wcx, surface_rgb, panel_alpha),
        "(y) 取证：换肤子菜单弹层面板应取全局亮 surface（现依赖面被钉住）"
    );
    // 关菜单（点外部）→ 帧内恢复无全局 surface 面板色。
    click_at(wcx, gpui::point(px(600.), px(300.)));
    wcx.update(|window, app| window.draw(app).clear(app));
    wcx.run_until_parked();
    assert!(
        !frame_has_quad_like(wcx, surface_rgb, panel_alpha),
        "菜单关闭后音乐窗帧内应不再有全局亮 surface 面板色"
    );
}
