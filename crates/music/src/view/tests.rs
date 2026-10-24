//! MusicView 视图测试：不真播（player 随机 dll/设备缺失时纯状态播放即可）。
//!
//! 覆盖：非全屏 Esc → `MusicEvent::ExitRequested`；F 键翻转 `fs_open`；
//! 歌曲行点击 → `player.current` 指向该行（`handle_play_idx` 直调，不做像素命中）；
//! 搜索过滤纯函数（大小写不敏感 / 中文 / 空查询）；默认状态。

use std::cell::Cell;
use std::path::Path;
use std::rc::Rc;
use std::time::Duration;

use gpui::{AppContext as _, Bounds, Entity, MouseButton, Pixels, TestAppContext, VisualTestContext, px};

use super::theme;
use super::{MusicEvent, MusicView};
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

/// 建一个含 3 曲的临时音乐库播放器（中文文件名）。
fn player_with_library(cx: &mut TestAppContext) -> Entity<MusicPlayer> {
    let dir = std::env::temp_dir().join(format!("gpui-music-view-{}", std::process::id()));
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
