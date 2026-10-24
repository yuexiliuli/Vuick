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
use super::{MusicEvent, MusicView, RightTab};
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
    // 手动滚远 → 暂停跟随。
    let handle = wcx.cx.read(|app| view.read(app).lyrics_scroll.clone());
    handle.set_offset(gpui::point(px(0.), px(-700.)));
    pump_ticks(wcx, 1);
    assert!(wcx.cx.read(|app| view.read(app).lyrics_manual_since.is_some()));
    // 切到第 1 首（path 变化 → 跟随态复位 → 新曲从自动跟随开始，不沿用手动暂停）。
    wcx.update(|_w, app| {
        player.update(app, |p, _| p.test_start_playing(1));
    });
    pump_ticks(wcx, 3);
    let expected = expected_follow_y(&view, wcx, 2);
    let got = handle.offset().y.as_f32();
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
