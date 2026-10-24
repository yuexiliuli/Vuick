//! MusicView 视图测试：不真播（player 随机 dll/设备缺失时纯状态播放即可）。
//!
//! 覆盖：非全屏 Esc → `MusicEvent::ExitRequested`；F 键翻转 `fs_open`；
//! 歌曲行点击 → `player.current` 指向该行（`handle_play_idx` 直调，不做像素命中）；
//! 搜索过滤纯函数（大小写不敏感 / 中文 / 空查询）；默认状态。

use std::cell::Cell;
use std::path::Path;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use gpui::{AppContext as _, Bounds, Entity, MouseButton, Pixels, TestAppContext, VisualTestContext, px};

use super::fullscreen::LYRIC_ROW_H;
use super::theme;
use super::{MainSection, MusicEvent, MusicView, RightTab};
use crate::model::Track;
use crate::player::MusicPlayer;

/// 极小有效 PNG（2×1 纯红）字节：作为同名封面侧车装进临时库。
fn tiny_png_bytes() -> Vec<u8> {
    let img: image::RgbaImage =
        image::ImageBuffer::from_pixel(2, 1, image::Rgba([255u8, 0, 0, 255]));
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut out, image::ImageFormat::Png)
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

/// 建含 `n` 曲的临时库：5 位艺术家轮转（`艺人{i%5}`）+ 唯一歌名（Library 分组、Home 热门表
/// 虚拟化回归用）。目录名带全局自增序号防并行互删。
static N_LIB_DIR_SEQ: AtomicUsize = AtomicUsize::new(0);
fn library_with_n(cx: &mut TestAppContext, n: usize) -> Entity<MusicPlayer> {
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
    cx.update(|app| {
        player.update(app, |p, cx| p.load_dir(&dir, cx));
    });
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

/// 建一个含 3 曲的临时音乐库播放器（中文文件名）。目录名带全局自增序号：多测试并行
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
    let b = wcx.cx.read(|app| view.read(app).bb_progress.get());
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
    let b = wcx.cx.read(|app| view.read(app).bb_progress.get());
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
    let b = wcx.cx.read(|app| view.read(app).bb_progress.get());
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
    let b = wcx.cx.read(|app| view.read(app).bb_progress.get());
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
/// 接管时长、position 变化，预期也随之变化——断言始终成立。
fn expected_follow_y(view: &Entity<MusicView>, cx: &mut VisualTestContext, n: usize) -> f32 {
    cx.update(|_w, app| {
        view.update(app, |v, cx| {
            let pos_ms = (v.player.read(cx).position() * 1000.0) as i64;
            let active = v
                .player
                .read(cx)
                .current_track()
                .and_then(|t| t.lyrics.clone())
                .and_then(|l| l.active_index(pos_ms))
                .unwrap_or(0);
            -v.lyrics_follow_scroll_top(active, n)
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
    let handle = wcx.cx.read(|app| view.read(app).lyrics_scroll.clone());
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
    let top_win = wcx.update(|_w, app| view.update(app, |v, _| v.lyrics_visible_window(n)));
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
    let bot_win = wcx.update(|_w, app| view.update(app, |v, _| v.lyrics_visible_window(n)));
    assert_eq!(bot_win.1, n, "滚到底应物化末行，实得 {:?}", bot_win);
    // 回顶：offset=0，物化窗口含首行。
    handle.set_offset(point(px(0.), px(0.)));
    wcx.update(|window, app| window.draw(app).clear(app));
    wcx.run_until_parked();
    let top2_win = wcx.update(|_w, app| view.update(app, |v, _| v.lyrics_visible_window(n)));
    assert_eq!(top2_win.0, 0, "回顶应物化首行，实得 {:?}", top2_win);
}

/// 用例 B：自动跟随——不手动滚动时，offset 把 active 行带入可视区（≈ 当前句居中）。
#[gpui::test]
async fn lyrics_auto_follow_centers_active(cx: &mut TestAppContext) {
    let player = player_with_lrc(cx, &lrc_30_lines());
    play_at_pos(&player, cx, 450.0); // 450s → 第 15 句
    let (view, wcx) = open_lyrics_tab(cx, &player);
    pump_ticks(wcx, 4);
    let handle = wcx.cx.read(|app| view.read(app).lyrics_scroll.clone());
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
    let handle = wcx.cx.read(|app| view.read(app).lyrics_scroll.clone());
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
        wcx.cx.read(|app| view.read(app).lyrics_manual_since.is_some()),
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
        wcx.cx.read(|app| view.read(app).lyrics_manual_since.is_none()),
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
    let handle = wcx.cx.read(|app| view.read(app).lyrics_scroll.clone());
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
    assert!(wcx.cx.read(|app| view.read(app).lyrics_manual_since.is_some()));
    // 切到第 1 首（path 变化 → 跟随态复位 → 新曲从自动跟随开始，不沿用手动暂停）。
    wcx.update(|_w, app| {
        player.update(app, |p, _| p.test_start_playing(1));
    });
    pump_ticks(wcx, 3);
    let expected = expected_follow_y(&view, wcx, 2);
    let got = wcx.cx.read(|app| view.read(app).lyrics_scroll.clone());
    let got = got.offset().y.as_f32();
    assert!(
        (got - expected).abs() < 2.0,
        "换曲后应复位并在新曲上跟随：got={got} expected={expected}"
    );
    assert!(
        wcx.cx.read(|app| view.read(app).lyrics_manual_since.is_none()),
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
    let max1 = w1.cx.read(|app| v1.read(app).lyrics_scroll.max_offset().y.as_f32());
    assert_eq!(max1, 0.0, "1 行歌词（内容≤视口）应无可滚范围，实得 {max1}");

    // 无歌词 + 无 current（未播放）：空态渲染不 panic。
    let p2 = player_with_lrc(cx, "[00:03.00]未播放不取歌词");
    let (v2, w2) = cx.add_window_view(|_w, cx| MusicView::new(p2, cx));
    w2.update(|window, app| {
        v2.update(app, |v, _| v.state.right_tab = RightTab::Lyrics);
        window.draw(app).clear(app);
    });
    w2.run_until_parked();
    let max2 = w2.cx.read(|app| v2.read(app).lyrics_scroll.max_offset().y.as_f32());
    assert!(max2 >= 0.0, "空态下滚动容器也应可布局，实得 {max2}");
}

// ---- 真机复报回归：尾部持续滚动被 3 秒误恢复拉回 / 滚到底继续滚轮「闪」 ----

/// 在歌词滚动容器上派发一次真实滚动事件（offset 已钳到界限时事件照样触发）。
fn wheel_on_lyrics(wcx: &mut VisualTestContext, view: &Entity<MusicView>) {
    use gpui::{Modifiers, ScrollDelta, ScrollWheelEvent, TouchPhase, point};
    let handle = wcx.cx.read(|app| view.read(app).lyrics_scroll.clone());
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
    let handle = wcx.cx.read(|app| view.read(app).lyrics_scroll.clone());
    let max = handle.max_offset().y.as_f32();
    let tail = -max;
    // 用户滚到尾部（手动滚动）→ 下一拍检测手动。
    handle.set_offset(gpui::point(px(0.), px(tail)));
    pump_ticks(wcx, 1);
    assert!(wcx.cx.read(|app| view.read(app).lyrics_manual_since.is_some()));
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
    let handle = wcx.cx.read(|app| view.read(app).lyrics_scroll.clone());
    let max = handle.max_offset().y.as_f32();
    handle.set_offset(gpui::point(px(0.), px(-max)));
    pump_ticks(wcx, 1); // 检测手动
    assert!(wcx.cx.read(|app| view.read(app).lyrics_manual_since.is_some()));
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
        wcx.cx.read(|app| view.read(app).lyrics_manual_since.is_none()),
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
    let handle = wcx.cx.read(|app| view.read(app).lyrics_scroll.clone());
    let max = handle.max_offset().y.as_f32();
    handle.set_offset(gpui::point(px(0.), px(-max)));
    pump_ticks(wcx, 1); // 检测手动
    // 尾部滚一轮：offset 钳位不动 → 连续两拍物化窗口一致。
    wheel_on_lyrics(wcx, &view);
    let off1 = handle.offset().y.as_f32();
    let win1 = wcx.update(|_w, app| view.update(app, |v, _| v.lyrics_visible_window(30)));
    pump_ticks(wcx, 2); // 未满 3s，不恢复
    let off2 = handle.offset().y.as_f32();
    let win2 = wcx.update(|_w, app| view.update(app, |v, _| v.lyrics_visible_window(30)));
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
    let handle = wcx.cx.read(|app| view.read(app).lyrics_scroll.clone());
    let n = 30usize;
    let max = handle.max_offset().y.as_f32();
    assert!(max > 0.0, "多行长歌词应可滚 max={max}");
    // 模拟滚轮在底部「先加后钳」的瞬时态：offset 越界到 max + 2 行（layout 尚未钳回）。
    handle.set_offset(gpui::point(px(0.), px(-(max + LYRIC_ROW_H * 2.0))));
    // 修复：物化窗口按钳位（max）算 first，与最终布局一致 → 不再漂移闪。
    let (first, _last) = wcx.update(|_w, app| view.update(app, |v, _| v.lyrics_visible_window(n)));
    let expected_first = ((max / LYRIC_ROW_H).floor() as usize).min(n - 1);
    assert_eq!(
        first, expected_first,
        "超界瞬时偏移的物化 first 应按钳位 max 算：got={first} expected={expected_first} (max={max})"
    );
    // 中段偏移不受钳位影响（正常浏览位置 still 精确，回归护栏）。
    handle.set_offset(gpui::point(px(0.), px(-(max * 0.5))));
    let (first2, _l) = wcx.update(|_w, app| view.update(app, |v, _| v.lyrics_visible_window(n)));
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
    let (f0, _) = wcx.update(|_w, app| view.update(app, |v, _| v.lyrics_visible_window(30)));
    assert_eq!(f0, 0, "未布局时行窗应从顶部开始，实得 {f0}");
}

// ---- 真机复报三：全屏 + 「歌词」tab 并存时树上双实例共用同一 ScrollHandle → 全屏抖动/闪 ----

/// 用例 L（复报三回归）：`fs_open` 时正常视图的右侧「歌词」tab 与全屏浮层**同时**渲染
/// `render_lyrics_panel_content`（同一 ScrollHandle / 同一套滚动状态被两套几何钳位）→
/// 全屏到底后共享 offset 被右栏更小 max 钳住、与 `max_offset()` 错位 = 上下跳动 + 闪。
/// 修复后右侧占位、树上仅全屏一个实例：到底时 offset 钳位 == 本容器 max（真到底）。
#[gpui::test]
async fn lyrics_fullscreen_plus_right_tab_single_instance(cx: &mut TestAppContext) {
    let player = player_with_lrc(cx, &lrc_30_lines());
    play_at_pos(&player, cx, 450.0);
    let (view, wcx) = open_lyrics_tab(cx, &player);
    pump_ticks(wcx, 2);
    // 打开全屏浮层（右栏仍停在「歌词」tab）——复现双实例共存的渲染时刻。
    wcx.update(|window, app| {
        view.update(app, |v, cx| v.toggle_fs(cx));
        window.draw(app).clear(app);
    });
    wcx.run_until_parked();
    let handle = wcx.cx.read(|app| view.read(app).lyrics_scroll.clone());
    let max_fs = handle.max_offset().y.as_f32();
    assert!(max_fs > 0.0, "全屏侧滚动容器应有可滚范围，实得 max_fs={max_fs}");
    // 全屏内滚到最底部（越界偏移 = 滚轮到底的瞬时态）。
    handle.set_offset(gpui::point(px(0.), px(-(max_fs + LYRIC_ROW_H * 4.0))));
    wcx.update(|window, app| window.draw(app).clear(app));
    wcx.run_until_parked();
    // 单实例语义：到底时 offset 应被钳到**本容器**的 max（与其 max_offset 对齐）。
    // 现状双实例：offset 被右栏更小 max 钳走 ≠ max_offset（全屏侧最后一次写）→ RED。
    let off = handle.offset().y.as_f32();
    let max_after = handle.max_offset().y.as_f32();
    assert!(
        (off + max_after).abs() < 2.0,
        "全屏+歌词tab 底部 offset 应钳到本容器 max：off={off} max_after={max_after}（双实例下被右栏钳走）"
    );
}

/// 右侧「歌词」tab 是否渲染真实歌词面板（纯函数，供 `render_right` 与测试共用）：
/// `fs_open` 时全屏浮层已渲染歌词 → 右侧改为静态占位，保证树上**单实例**（防两个
/// `render_lyrics_panel_content` 争夺同一 ScrollHandle → 全屏抖动/闪）。
#[test]
fn right_panel_live_lyrics_gates_on_fullscreen() {
    assert!(
        super::fullscreen::right_panel_live_lyrics(false),
        "非全屏时右侧应渲染真实歌词面板"
    );
    assert!(
        !super::fullscreen::right_panel_live_lyrics(true),
        "全屏浮层打开时右侧应静态占位（免双实例争夺）"
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
    // 用例全绿却漏真机穿透）。
    wcx.simulate_resize(gpui::size(px(900.), px(716.)));
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
