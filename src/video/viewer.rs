//! 视频查看器视图：M2 = 无声播放器（首帧阅读 + 帧钟播放 + 播放/暂停/seek + 同文件夹切换）。
//!
//! ## 播放模型（无音频，第一版）
//! - **解码线程**：打开（或 seek 后重开）`VideoReader`，逐帧解码 BGRA 推入**有界**
//!   通道（[`DECODE_AHEAD_FRAMES`] 槽）。有界 = 回压：暂停/停滞时解码线程阻塞在 send，
//!   内存有界；seek 换代（`generation` 原子 + 换接收端）让旧线程退出。
//! - **pump 泵**：`playing` 时每 ~16ms `tick` 一次 —— 按真实经过推进 `playhead`，
//!   从通道排空「pts ≤ playhead」的帧（迟到帧直接丢），取最新上屏；越线的下一帧
//!   存 `overflow` 不被丢；到结尾自动暂停。
//! - **seek**：重开解码线程（文件再 open + seek），`pending_seek` 挂起 pump 直到
//!   落点帧上屏（暂停态也能预览跳转后的画面）。
//!
//! ## 控制（焦点落在本视图时）
//! - `Space`：播放 / 暂停；`←`/`→`：±[`SEEK_STEP_SEC`] 秒；
//! - 滚轮：±[`WHEEL_SEEK_SEC`] 秒（向上滚 = 前进）；`Shift+滚轮`：同文件夹切换
//!   （向下滚 = 下一支）；`PageDown`/`PageUp`：下一支 / 上一支。
//!
//! 对齐 [`crate::image::ImageViewer`] 的「自包含阅读器」姿势；上屏字节序 BGRA
//! 同 PDF 模块 [`crate::pdf::render_image_from_bgra`]。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, sync_channel};
use std::time::{Duration, Instant};

use gpui::{
    App, Context, Hsla, InteractiveElement, IntoElement, KeyDownEvent, ObjectFit, ParentElement,
    Render, RenderImage, ScrollDelta, ScrollWheelEvent, SharedString, Styled, StyledImage, Window,
    div, img, px, rgb,
};
use image::{Frame, RgbaImage};

use super::decode::VideoReader;

/// 解码通道槽位数（帧数）：回压 + 内存有界；播放/seek 时缓冲少量前跳。
const DECODE_AHEAD_FRAMES: usize = 2;
/// ←/→ seek 步进（秒）。
const SEEK_STEP_SEC: f64 = 5.0;
/// 滚轮 seek 步进（秒）。
const WHEEL_SEEK_SEC: f64 = 10.0;
/// 播放推进周期（≈60Hz，对齐 PDF 瓦片泵）。
const PUMP_MS: u64 = 16;

/// 解码线程经通道传递的一帧：`(pts 秒, 宽, 高, BGRA)`。
type FrameItem = (f64, u32, u32, Vec<u8>);

/// 视频查看器：自包含的（无声）视频播放视图。
pub struct VideoViewer {
    path: PathBuf,
    focus_handle: gpui::FocusHandle,
    /// 当前上屏帧。
    frame: Option<Arc<RenderImage>>,
    /// 视频原始像素尺寸（Contain 显示比例依据）。
    intrinsic: Option<(f32, f32)>,
    /// 打开 / 解码初始化是否仍在进行。
    loading: bool,
    /// 打开 / 解码初始化失败信息。
    load_error: Option<String>,
    /// 画布背景（宿主显式设置；None → 主题推导）。
    bg: Option<Hsla>,
    // --- 播放状态 ---
    /// 是否在播放（false = 暂停 / 停在结尾）。
    playing: bool,
    /// 当前显示时间（秒）。
    playhead: f64,
    /// 总时长（秒；未知时 0）。
    duration: f64,
    /// 标称帧率（状态栏可显示）。
    fps: Option<(u32, u32)>,
    /// 上次 `tick` 时刻（播放推进用；暂停清空避免恢复时猛跳）。
    last_tick: Option<Instant>,
    /// 同文件夹视频清单（文件名升序，恒非空含当前）。
    folder: Vec<PathBuf>,
    /// 当前视频在 `folder` 中的下标。
    current: usize,
    /// 解码线程 → 主线程的有界帧通道。
    rx: Option<Receiver<FrameItem>>,
    /// 越过 playhead 的下一帧保留（排空时不丢）。
    overflow: Option<FrameItem>,
    /// 解码线程换代号：每次换解码器 +1，旧线程比对后自退。
    generation: Arc<AtomicU64>,
    /// pump 泵是否在跑（去重）。
    pump_running: bool,
    /// seek 落点待定（秒）：暂停态 seek 后仍需泵直到落点帧上屏。
    pending_seek: Option<f64>,
}

impl VideoViewer {
    /// 创建查看器并异步打开 `path`（参数解析在后台；解码线程随后启动即播）。
    pub fn new(path: PathBuf, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            path: path.clone(),
            focus_handle: cx.focus_handle(),
            frame: None,
            intrinsic: None,
            loading: true,
            load_error: None,
            bg: None,
            playing: true,
            playhead: 0.0,
            duration: 1.0,
            fps: None,
            last_tick: None,
            folder: vec![path.clone()],
            current: 0,
            rx: None,
            overflow: None,
            generation: Arc::new(AtomicU64::new(0)),
            pump_running: false,
            pending_seek: None,
        };
        this.start(cx);
        this
    }

    /// 后台解析参数 + 扫描同文件夹 → 启动解码线程与 pump。
    /// `VideoReader` 含裸指针（非 Send）：只在该后台任务内取参数，不把实体带回主线程。
    fn start(&mut self, cx: &mut Context<Self>) {
        let path = self.path.clone();
        cx.spawn(async move |weak, cx| {
            let (opened_params, (folder, current)) = cx
                .background_executor()
                .spawn(async move {
                    let opened = VideoReader::open(&path).map(|r| r.params());
                    let folder = scan_folder(&path);
                    (opened, folder)
                })
                .await;
            match opened_params {
                Ok(p) => {
                    let _ = weak.update(cx, |this, cx| {
                        this.folder = folder;
                        this.current = current;
                        this.loading = false;
                        this.duration = p.duration.max(0.001);
                        this.fps = p.frame_rate;
                        if this.intrinsic.is_none() {
                            this.intrinsic = Some((p.width as f32, p.height as f32));
                        }
                        this.start_decoder_at(0.0);
                        this.ensure_pump(cx);
                    });
                }
                Err(e) => {
                    let _ = weak.update(cx, |this, cx| {
                        this.loading = false;
                        this.load_error = Some(e.to_string());
                        cx.notify();
                    });
                }
            }
        })
        .detach();
    }

    /// 启动（或重开）解码线程，从 `target` 秒开始解码。换代取消旧线程。
    fn start_decoder_at(&mut self, target: f64) {
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.generation.store(generation, Ordering::SeqCst);
        let (tx, rx) = sync_channel::<FrameItem>(DECODE_AHEAD_FRAMES);
        // 换掉旧接收端 → 旧线程下一次 send 失联自退（与 generation 双保险）
        self.overflow = None;
        if let Some(old) = self.rx.replace(rx) {
            drop(old);
        }
        self.pending_seek = Some(target);
        let path = self.path.clone();
        let gen_arc = self.generation.clone();
        std::thread::spawn(move || {
            let mut reader = match VideoReader::open(&path) {
                Ok(r) => r,
                Err(_) => return,
            };
            let _ = reader.seek(target);
            while gen_arc.load(Ordering::SeqCst) == generation {
                match reader.next_frame_bgra() {
                    Ok(Some(item)) => {
                        if tx.send(item).is_err() {
                            break; // 接收端已被换代/弃用
                        }
                    }
                    _ => break, // EOF / 解码错误：本代结束
                }
            }
        });
    }

    /// 每 ~16ms 一拍的播放泵是否仍需运行。
    fn pump_wanted(&self) -> bool {
        self.playing || self.pending_seek.is_some()
    }

    fn ensure_pump(&mut self, cx: &mut Context<Self>) {
        if self.pump_running || !self.pump_wanted() {
            return;
        }
        self.pump_running = true;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(PUMP_MS)).await;
                let keep = this
                    .update(cx, |v, cx| {
                        v.tick();
                        // 关键：每拍强制重绘（单靠 notify 在实体未被窗口 tracking 时可能
                        // 空操作）——否则播放只在状态里推进、画面停在首帧占位。见 pdf 瓦片泵。
                        cx.refresh_windows();
                        v.pump_wanted()
                    })
                    .unwrap_or(false);
                if !keep {
                    break;
                }
            }
            let _ = this.update(cx, |v, _cx| v.pump_running = false);
        })
        .detach();
    }

    /// 播放泵单拍（纯状态推进 + 上屏；调用方负责重绘）。
    fn tick(&mut self) {
        let now = Instant::now();
        if self.playing {
            let dt = self
                .last_tick
                .map(|t| now.duration_since(t).as_secs_f64())
                .unwrap_or_else(|| 1.0 / 60.0);
            self.last_tick = Some(now);
            self.playhead += dt;
            if self.duration > 0.0 && self.playhead >= self.duration {
                self.playhead = self.duration;
                self.playing = false; // 播到结尾自动停
            }
        } else {
            self.last_tick = None;
        }

        // 排空「pts ≤ playhead」的帧取最新上屏；越过 playhead 的下一帧保留在 overflow
        let mut latest: Option<FrameItem> = None;
        loop {
            let item: Option<FrameItem> = if let Some(o) = self.overflow.take() {
                Some(o)
            } else {
                match &self.rx {
                    Some(rx) => rx.try_recv().ok(),
                    None => None,
                }
            };
            match item {
                Some((pts, w, h, bgra)) if pts <= self.playhead => {
                    latest = Some((pts, w, h, bgra));
                }
                Some(item) => {
                    self.overflow = Some(item);
                    break;
                }
                None => break,
            }
        }
        if let Some((_pts, w, h, bgra)) = latest {
            self.install_bgra(w, h, bgra);
            self.pending_seek = None; // 落点已上屏（首个可用帧）
        }
    }

    /// 把 BGRA 包成 `RenderImage` 上屏。
    fn install_bgra(&mut self, w: u32, h: u32, bgra: Vec<u8>) {
        let buf = RgbaImage::from_raw(w, h, bgra).expect("解码帧长度恒为 w*h*4");
        self.frame = Some(Arc::new(RenderImage::new(vec![Frame::new(buf)])));
        self.intrinsic = Some((w as f32, h as f32));
    }

    /// 关闭当前媒体、打开 `folder[current]`（同文件夹切换共用）。
    fn open_folder_index(&mut self, idx: usize, cx: &mut Context<Self>) {
        if idx >= self.folder.len() {
            return;
        }
        self.current = idx;
        self.path = self.folder[idx].clone();
        self.frame = None;
        self.intrinsic = None;
        self.loading = true;
        self.load_error = None;
        self.playing = true;
        self.playhead = 0.0;
        self.duration = 1.0;
        self.fps = None;
        self.last_tick = None;
        self.overflow = None;
        self.pending_seek = None;
        self.start(cx);
        cx.notify();
    }

    fn switch_file(&mut self, dir: i32, cx: &mut Context<Self>) {
        let n = self.folder.len();
        if n <= 1 {
            return;
        }
        let idx = ((self.current as i64 + i64::from(dir)).rem_euclid(n as i64)) as usize;
        self.open_folder_index(idx, cx);
    }

    fn toggle_play(&mut self, cx: &mut Context<Self>) {
        self.playing = !self.playing;
        if self.playing {
            self.last_tick = None; // 恢复播放不随暂停跳变
            self.ensure_pump(cx);
        }
        cx.notify();
    }

    fn seek(&mut self, delta: f64, cx: &mut Context<Self>) {
        let target = (self.playhead + delta).clamp(0.0, self.duration.max(0.0));
        self.playhead = target;
        self.start_decoder_at(target);
        self.ensure_pump(cx);
        cx.notify();
    }

    /// 当前视频路径（宿主 `EditorApp` 同步标签标题用）。
    #[allow(dead_code)] // M3 标签标题/状态栏接入后删除
    pub fn current_path(&self) -> Option<PathBuf> {
        Some(self.path.clone())
    }

    /// 是否在播放（状态栏 / 测试用）。
    #[allow(dead_code)] // M3 状态栏接入后删除
    pub fn is_playing(&self) -> bool {
        self.playing
    }

    /// 是否仍在后台加载。
    #[allow(dead_code)] // M3 状态栏接入后删除
    pub fn is_loading(&self) -> bool {
        self.loading
    }

    /// 焦点句柄（宿主打开时把焦点落上来，按键才进本视图）。
    pub fn focus_handle(&self) -> gpui::FocusHandle {
        self.focus_handle.clone()
    }

    /// 设置画布背景色（宿主按主题传入）。
    pub fn set_background(&mut self, bg: Hsla) {
        self.bg = Some(bg);
    }

    fn on_key_down(&mut self, e: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        match e.keystroke.key.as_str() {
            "space" => self.toggle_play(cx),
            "left" => self.seek(-SEEK_STEP_SEC, cx),
            "right" => self.seek(SEEK_STEP_SEC, cx),
            "pagedown" => self.switch_file(1, cx),
            "pageup" => self.switch_file(-1, cx),
            _ => return, // 其余键不拦截
        }
        cx.stop_propagation();
    }

    fn on_scroll_wheel(
        &mut self,
        ev: &ScrollWheelEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let delta_y = match ev.delta {
            ScrollDelta::Lines(lines) => lines.y,
            ScrollDelta::Pixels(pixels) => f32::from(pixels.y),
        };
        if delta_y == 0.0 {
            return;
        }
        if ev.modifiers.shift {
            // Shift+滚轮：同文件夹切换（向下滚 = 下一支）
            self.switch_file(if delta_y < 0.0 { 1 } else { -1 }, cx);
        } else {
            // 滚轮：seek（向上滚 = 前进）
            let step = f64::from(delta_y.clamp(-1.0, 1.0)) * WHEEL_SEEK_SEC;
            self.seek(step, cx);
        }
        cx.stop_propagation();
    }

    /// 居中占位文本（加载中 / 加载失败共用）。
    fn placeholder(text: &str, color: Hsla) -> gpui::AnyElement {
        div()
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(14.))
            .text_color(color)
            .child(text.to_string())
            .into_any_element()
    }

    /// 阅读画布底色（同 ImageViewer：非白色、与画布分隔的灰系）。
    fn default_canvas(cx: &App) -> Hsla {
        if crate::ui::theme::Theme::global(cx).is_dark() {
            rgb(0x111827).into()
        } else {
            rgb(0xe5e7eb).into()
        }
    }
}

impl Render for VideoViewer {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let canvas = self.bg.unwrap_or_else(|| Self::default_canvas(cx));
        let theme = crate::ui::theme::Theme::global(cx);
        let muted: Hsla = theme.colors.fg_muted.into();
        let surface: Hsla = theme.colors.surface.into();

        let body: gpui::AnyElement = if let Some(err) = &self.load_error {
            Self::placeholder(&format!("无法打开视频：{err}"), muted)
        } else {
            match self.frame.clone() {
                Some(render) => img(gpui::ImageSource::Render(render))
                    .object_fit(ObjectFit::Contain)
                    .w_full()
                    .h_full()
                    .into_any_element(),
                None if self.playing || self.loading => Self::placeholder("正在打开视频…", muted),
                None => Self::placeholder("暂停", muted),
            }
        };

        // 底部信息条：播放时间 / 总时长 · 进度为同文件夹第几支
        let info = SharedString::from(format!(
            "{:>5.1}s / {:.1}s · {}/{}",
            self.playhead,
            self.duration,
            self.current + 1,
            self.folder.len(),
        ));

        div()
            .id("video-viewer")
            .size_full()
            .relative()
            .bg(canvas)
            .child(
                div()
                    .id("video-main")
                    .size_full()
                    .relative()
                    .track_focus(&self.focus_handle)
                    .on_key_down(cx.listener(Self::on_key_down))
                    .on_scroll_wheel(cx.listener(Self::on_scroll_wheel))
                    .child(body),
            )
            .child(
                div()
                    .id("video-info")
                    .absolute()
                    .left(px(12.))
                    .bottom(px(12.))
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .bg(surface)
                    .text_size(px(12.))
                    .text_color(muted)
                    .child(info),
            )
    }
}

/// 扫描当前视频所在文件夹的全部视频（扩展名过滤、文件名升序），返回
/// (清单, 当前下标)。失败/空目录退化为「仅当前」单元素清单。
fn scan_folder(at: &Path) -> (Vec<PathBuf>, usize) {
    let Some(dir) = at.parent() else {
        return (vec![at.to_path_buf()], 0);
    };
    let mut list: Vec<PathBuf> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_file() && crate::video::is_video_extension(&p) {
                list.push(p);
            }
        }
    }
    if list.is_empty() {
        return (vec![at.to_path_buf()], 0);
    }
    list.sort();
    let idx = list.iter().position(|p| p == at).unwrap_or(0);
    (list, idx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::AppContext as _;
    use gpui::TestAppContext;

    fn fixture() -> PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures")
            .join("video测试.mp4")
    }

    fn blank(path: PathBuf, cx: &mut Context<VideoViewer>) -> VideoViewer {
        VideoViewer {
            path,
            focus_handle: cx.focus_handle(),
            frame: None,
            intrinsic: None,
            loading: false,
            load_error: None,
            bg: None,
            playing: false,
            playhead: 0.0,
            duration: 2.0,
            fps: None,
            last_tick: None,
            folder: vec![fixture()],
            current: 0,
            rx: None,
            overflow: None,
            generation: Arc::new(AtomicU64::new(0)),
            pump_running: false,
            pending_seek: None,
        }
    }

    fn frame_of(pts: f64, v: u8) -> FrameItem {
        (pts, 320, 240, vec![v; (320 * 240 * 4) as usize])
    }

    /// 直接构造（不走真实解码）：pump 排空逻辑 —— 取「pts ≤ playhead」的最新帧上屏，
    /// 越线的下一帧保留在 overflow。
    #[gpui::test]
    async fn pump_installs_latest_frame_in_window(cx: &mut TestAppContext) {
        let ent = cx.update(|app| app.new(|cx| blank(fixture(), cx)));
        let (tx, rx) = sync_channel::<FrameItem>(4);
        tx.send(frame_of(0.0, 1)).unwrap();
        tx.send(frame_of(0.4, 2)).unwrap();
        tx.send(frame_of(1.5, 3)).unwrap();
        /* 设置 playhead 0.5 后 tick：0.0/0.4 ≤0.5 → 取 0.4 上屏；1.5 越线进 overflow */
        ent.update(cx, |v, _| {
            v.rx = Some(rx);
            v.playhead = 0.5;
            v.tick();
        });
        let s = cx.update(|app| {
            let v = ent.read(app);
            (v.frame.is_some(), v.overflow.as_ref().map(|f| f.0))
        });
        assert!(s.0, "应有帧上屏");
        assert_eq!(s.1, Some(1.5), "越线帧保留在 overflow");
        // playhead 越过 1.5：旧 overflow(1.5) 移交上屏、通道里的 1.6 进位为 overflow
        tx.send(frame_of(1.6, 4)).unwrap();
        ent.update(cx, |v, _| {
            v.playhead = 1.55;
            v.tick();
        });
        let o = cx.update(|app| ent.read(app).overflow.as_ref().map(|f| f.0));
        assert_eq!(o, Some(1.6), "越过旧 overflow 后新越线帧进位");
        cx.run_until_parked();
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
        ent.update(cx, |v, _| {
            v.last_tick = Some(Instant::now() - Duration::from_millis(100));
            v.tick();
        });
        let p0 = cx.update(|app| ent.read(app).playhead);
        assert!((p0 - 1.0).abs() < 1e-9, "暂停不该前进");
        // 播放：前进
        ent.update(cx, |v, _| {
            v.playing = true;
            v.last_tick = None;
            v.tick();
        });
        assert!(cx.update(|app| ent.read(app).playhead) > 1.0, "播放应前进");
        // 到结尾：自动停
        ent.update(cx, |v, _| {
            v.playhead = 2.0;
            v.last_tick = None;
            v.playing = true;
            v.tick();
        });
        let s = cx.update(|app| {
            let v = ent.read(app);
            (v.playhead, v.playing)
        });
        assert_eq!(s.0, 2.0);
        assert!(!s.1, "到结尾应自动暂停");
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
    async fn loads_first_frame(cx: &mut TestAppContext) {
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
            viewer.update(cx, |v, _| v.tick());
            cx.run_until_parked();
            guard += 1;
            assert!(guard < 6000, "等待视频首帧上屏超时");
        };
        assert!(frame_ok, "首帧应上屏, load_error={err:?}");
        assert!(intrinsic.is_some(), "应取得视频像素尺寸");
        // 多喂几拍：播放状态保持、不上屏帧被正确消费而不堆积 overflow
        for _ in 0..60 {
            viewer.update(cx, |v, _| v.tick());
            cx.run_until_parked();
        }
        let s = cx.update(|app| {
            let v = viewer.read(app);
            (v.is_playing(), v.overflow.is_none(), v.frame.is_some())
        });
        assert!(s.0, "打开即自动播放且持续在播");
        assert!(s.1, "常规播放不该滞留 overflow（帧不超前投递）");
        assert!(s.2, "画面持续在屏");
    }

    /// M1 保留：打开损坏 / 不存在文件：落错误占位不死等。
    #[gpui::test]
    async fn missing_file_reports_error(cx: &mut TestAppContext) {
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
}
