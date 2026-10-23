//! MusicView 视图测试：不真播（player 随机 dll/设备缺失时纯状态播放即可）。
//!
//! 覆盖：非全屏 Esc → `MusicEvent::ExitRequested`；F 键翻转 `fs_open`；
//! 歌曲行点击 → `player.current` 指向该行（`handle_play_idx` 直调，不做像素命中）；
//! 搜索过滤纯函数（大小写不敏感 / 中文 / 空查询）；默认状态。

use std::cell::Cell;
use std::path::Path;
use std::rc::Rc;

use gpui::{AppContext as _, Entity, TestAppContext, VisualTestContext};

use super::theme;
use super::{MusicEvent, MusicView};
use crate::model::Track;
use crate::player::MusicPlayer;

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
