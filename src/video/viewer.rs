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
//! ## 音频（M3/M4）
//! - `AudioReader` 把音轨重采样成 F32 立体声喂 CPAL 输出（`AudioSink`）；有音轨时
//!   播放进度以**音频时钟**为主（`playhead = audio_base + 设备已播位置`），墙钟兜底。
//! - **音量/静音（M5）**：`AudioSink::set_volume` 在回调内逐样本乘音量（无锁原子）；
//!   静音 = 生效音量 0。
//! - **播放速度（M5）**：按 `speed` 降低重采样输出率（`设备率÷speed`）重开音频线程 +
//!   音频时钟乘 `speed`（变速变调，resample 方案；不变调需时间拉伸，留待后续）；
//!   纯画面走墙钟 `dt×speed`，视频帧按「源秒」时间轴排空，A/V 始终同速对齐。
//!
//! ## 降载/M5
//! - **decode-to-display**：解码线程用 `open_scaled` 把 sws 输出按 [`DECODE_MAX_DIM`]
//!   长边封顶（4K 源只解 ~1080p），`params` 仍报源尺寸保宽高比。
//! - **控制条（全屏底部浮层）**：上一支 / 播放·暂停 / 下一支 / 时间 / 可拖 seek 进度条 /
//!   静音 + 音量条 / 倍速循环 / 清单位置。
//!
//! ## M6（播放模式 / 截图）
//! - **播放模式三态**（控制条按钮循环）：顺序播放（播完停）/ 列表循环（播完自动切下一支、
//!   结尾回绕首支）/ 随机播放（播完随机挑一支，不含当前）；单文件时循环/随机退化为
//!   seek 0 重播（保留缩放/平移）。
//! - **截图当前帧**：`s` / 控制条相机按钮 → 点截图时才把当前帧 BGRA 经 `RenderImage::as_bytes`
//!   拷贝一份（**零每帧开销**）存 PNG（视频同目录，只读时回退系统临时目录），右上角瞬时
//!   浮层提示保存路径。
//!
//! ## 控制（焦点落在本视图时）
//! - `Space`：播放 / 暂停；`←`/`→`：±[`SEEK_STEP_SEC`] 秒；
//! - `Ctrl+滚轮`：放大 / 缩小（[`ZOOM_STEP`] 档，钳位 [`ZOOM_MIN`..=[`ZOOM_MAX`]）；
//!   `Shift+滚轮`：同文件夹切换（向下滚 = 下一支）；普通滚轮**不控制进度**；
//!   `PageDown`/`PageUp`：下一支 / 上一支；
//! - `m`：静音切换；`-`/`=`：减/加音量（±[`VOLUME_STEP`]）；`[`/`]`：降/升倍速；
//!   `s`：截图当前帧存 PNG。
//!
//! 对齐 [`crate::image::ImageViewer`] 的「自包含阅读器」姿势；上屏字节序 BGRA
//! 同 PDF 模块 [`crate::pdf::render_image_from_bgra`]。

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, sync_channel};
use std::time::{Duration, Instant};

use gpui::{
    App, Bounds, Context, CursorStyle, DefiniteLength, Hsla, InteractiveElement, IntoElement,
    KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ObjectFit,
    ParentElement, Pixels, Point, Render, RenderImage, ScrollDelta, ScrollHandle,
    ScrollWheelEvent, StatefulInteractiveElement, Styled, StyledImage, Window, canvas, div,
    img, px, prelude::FluentBuilder as _,
};
use image::{Frame, RgbaImage};

use crate::ui::button::Button;
use crate::ui::button::ButtonVariants as _;
use crate::ui::icon::{Icon, IconName};

use super::audio::AudioSink;
use super::decode::{AudioReader, VideoReader};

/// 解码通道槽位数（帧数）：回压 + 内存有界；播放/seek 时缓冲少量前跳。
const DECODE_AHEAD_FRAMES: usize = 2;
/// ←/→ seek 步进（秒）。
const SEEK_STEP_SEC: f64 = 5.0;
/// 播放推进周期（≈60Hz，对齐 PDF 瓦片泵）。
const PUMP_MS: u64 = 16;
/// 无新帧的闲置拍最长重绘间隔（ms）：播放中帧暂缺（VFR / 解码卡顿）时仍低频刷新
/// 进度条/控制条，不再每 16ms 强刷整窗（62Hz 全窗重绘是播放 CPU 大头之一）。
const COARSE_REPAINT_MS: u64 = 200;
/// 音频解码回压阈值（样本数 ≈ 1s × 立体声）：缓冲满则音频线程等待，不无限塞内存。
const AUDIO_QUEUE_AIM: usize = 48_000 * 2;
/// decode-to-display 下采样：sws 输出长边封顶（4K 源只解 ~1080p，省 ~4× 解码/内存/上屏）。
const DECODE_MAX_DIM: u32 = 1920;
/// 解码封顶下限（px）：窗口极小/截图预览时也不低于此，保留可读画面。
const DECODE_MIN_DIM: u32 = 256;
/// 解码封顶的「桶」对齐粒度（px）：窗口拖动按桶变化才重开解码器，避免逐像素重开。
/// 256→128：视口近桶沿时少 over-alloc 10–20% 上传像素（sws/上传随输出尺寸走）。
const CAP_BUCKET: u32 = 128;
/// 循环可选的播放倍速（按钮逐档循环 + `[`/`]` 步进）。
const SPEED_STEPS: [f32; 4] = [0.5, 1.0, 1.5, 2.0];
/// `-`/`=` 音量步进。
const VOLUME_STEP: f32 = 0.1;
/// 底部控制条预留高度（px）：视频 contain 适配到「视口高 − 此值」，避免画面底部被
/// 浮层控制条遮住。值略大于控制条实际高度（py 8×2 + 按钮 28 ≈ 44）。
const CONTROL_BAR_H: f32 = 52.0;
/// Ctrl+滚轮缩放区间 / 步进（语义同图片/PDF 阅读器）。
const ZOOM_MIN: f32 = 0.25;
const ZOOM_MAX: f32 = 8.0;
const ZOOM_STEP: f32 = 0.12;
/// 右上角瞬时状态浮层（截图结果提示等）的显示时长（ms），到期自动清除。
const STATUS_MS: u64 = 2500;

/// 播放模式三态（控制条按钮循环切换；播完「自然结尾」时的去向）：
/// - [`PlaybackMode::Sequential`]：顺序播放——播完当前自动停（默认，保持历史行为）；
/// - [`PlaybackMode::Loop`]：列表循环——播完自动切下一支，列表结尾回绕首支（可无限循环）；
/// - [`PlaybackMode::Shuffle`]：随机播放——播完随机挑一支（不含当前）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PlaybackMode {
    Sequential,
    Loop,
    Shuffle,
}

impl PlaybackMode {
    /// 三态循环的下一个（顺序 → 循环 → 随机 → 顺序）。
    fn next(self) -> Self {
        match self {
            Self::Sequential => Self::Loop,
            Self::Loop => Self::Shuffle,
            Self::Shuffle => Self::Sequential,
        }
    }

    /// 控制条按钮上的短标签（中文两字，与倍速按钮同款紧凑布局）。
    fn label(self) -> &'static str {
        match self {
            Self::Sequential => "顺序",
            Self::Loop => "循环",
            Self::Shuffle => "随机",
        }
    }
}

/// 播放管线拆账剖析（`VPROF=1` 开启）：video worker 每 ~2s 打一行「解码/推帧背压」墙钟，
/// 主线程 `install_bgra` 每 120 帧打一行「上屏装配」耗时；默认关闭、不改变行为。
fn vprof_enabled() -> bool {
    std::env::var_os("VPROF").is_some()
}

// 主线程上屏装配累计（`install_bgra` 写；`VPROF=1` 才写，普通运行零成本空置）。
thread_local! {
    static VPROF_PRESENT: std::cell::RefCell<(u64, std::time::Duration)> = const {
        std::cell::RefCell::new((0, std::time::Duration::ZERO))
    };
}

/// 解码线程经通道传递的一帧：`(pts 秒, 宽, 高, BGRA)`。
type FrameItem = (f64, u32, u32, Vec<u8>);

/// 投递给常驻视频解码线程的命令（切换/seek）：内层循环以 `generation` 判定换代。
/// `result_tx` 只在开媒体（非 seek）时挂：worker 开成功后回送 `Ok(params)` 或失败
/// `Err(msg)`，供宿主补齐时长/尺寸/音轨或落错误（替代会污染后续 open 的“预开参数 reader”）。
struct DecodeCmd {
    generation: u64,
    path: PathBuf,
    target: f64,
    cap: u32,
    tx: std::sync::mpsc::SyncSender<FrameItem>,
    result_tx: Option<std::sync::mpsc::Sender<Result<super::decode::VideoParams, String>>>,
}

/// 投递给常驻音频解码线程的命令（同视频；worker 持 sink 克隆、命令开头统一 flush）。
struct AudioCmd {
    generation: u64,
    path: PathBuf,
    target: f64,
    out_rate: u32,
    ready: Arc<AtomicBool>,
    sink: AudioSink,
}

/// 把条的**窗口坐标**盒 + 屏幕 x 映射成 0..=1 的进度（钳位）。盒从未布局过
///（bounds=0，即某窗口从未画过本条）时返回 `None`。
fn fraction_at(bounds: &Cell<Bounds<Pixels>>, point: Point<Pixels>) -> Option<f32> {
    let b = bounds.get();
    let width = f32::from(b.size.width);
    if width <= 0.0 {
        return None;
    }
    let x = f32::from(point.x);
    let origin_x = f32::from(b.origin.x);
    Some(((x - origin_x) / width).clamp(0.0, 1.0))
}

/// 随机播放的初始种子：系统墙钟时钟与栈地址位混合（仅用于随机选曲，不需要密码学强度；
/// 每开一个查看器不同，会话内确定性演进）。
fn seed_rng() -> u64 {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0) as u64;
    let addr = (&t as *const _) as usize as u64;
    t.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(addr)
}

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
    /// 音频输出（有音轨且设备可用时 Some；否则纯画面 + 墙钟）。
    audio: Option<AudioSink>,
    /// 音频时钟基准：seek 落点（秒）。有音频播放时 `playhead = audio_base + 设备已播位置`。
    audio_base: f64,
    /// 音频解码线程换代号（seek/重开生效，旧音频线程自退）。
    audio_gen: Arc<AtomicU64>,
    /// 音频输出初始化失败标志（避免每次打开重试）。
    audio_err: bool,
    /// 音频健康诊断节拍（每 ~60 tick ≈1s 打一行 stats；播放无音频时不用）。
    diag_ticks: u32,
    /// 常驻视频解码线程的命令通道。worker 一生一个，切换/seek 全投命令；新文件在
    /// **同一线程内**串行重开旧 reader —— 实测“新线程在上一解码器 teardown 后立刻
    /// `avformat_open_input`”会卡死（72.MP4 复现），单 worker 串行重开则稳定。
    decode_cmd: Option<std::sync::mpsc::Sender<DecodeCmd>>,
    /// 常驻音频解码线程的命令通道（同 `decode_cmd` 的收尾理由）。
    audio_cmd: Option<std::sync::mpsc::Sender<AudioCmd>>,
    /// 本媒体音频是否已真正开喂（音频 worker 开成功置 true；切媒体/seek 复位 false）。
    /// 为 false 时播放时钟走墙钟（静音/无声轨的媒体也能前进，不再冻结）。
    audio_ready: Arc<AtomicBool>,
    /// 音频时钟锚定待执行：音频一上线就把 `audio_base` 锚到当前墙钟进度（无跳变），
    /// 否则开新媒体的静音间隙会被设备消费计入 `position()`，把 playhead 带飞 → 音画错位。
    audio_anchor_pending: bool,
    // --- M5 播放体验 ---
    /// 音量（0..=1）。静音时不改此值，只让生效音量归零。
    volume: f32,
    /// 是否静音（生效音量 = `volume` 或 0）。
    muted: bool,
    /// 播放倍速（`SPEED_STEPS` 之一；音频时钟/墙钟均按此推进）。
    speed: f32,
    /// 显示缩放（1.0 = 适应视口；Ctrl+滚轮 ±[`ZOOM_STEP`] 档，钳位 [`ZOOM_MIN`..[`ZOOM_MAX`]）。
    zoom: f32,
    /// 放大后的拖拽平移偏移（px；以「视口中心 = 图中心」为 0 基准，未放大恒 0）。
    pan: (f32, f32),
    /// 拖拽平移进行中：`(左键按下位置, 按下时 pan 起点)`。
    drag: Option<((f32, f32), (f32, f32))>,
    // --- 纯视频模式（参考纯图片锁定）：控制条为浮层，按需浮现 / 自隐 ---
    /// 是否「纯视频」干净播放模式（宿主据此隐藏全部非视频 UI，视频充满窗口）。
    locked: bool,
    /// 底部控制条是否可见（锁定模式：进锁定默认可见、切视频 / 悬停浮现，离开 1s 自隐）。
    controls_visible: bool,
    /// 控制条自隐定时器的代序号（悬停 / 切视频刷新，防旧定时器误清）。
    controls_seq: u32,
    /// 是否悬停在查看器上（`#video-viewer` 根 hover；悬停期间不自动隐藏）。
    controls_hovering: bool,
    /// seek 进度条自身在**窗口坐标**下的盒（每次 prepaint 由 canvas 子元素刷新；
    /// 事件回调读它做屏 x→分数。**不**用 `use_keyed_state`——那只能在渲染期调用）。
    seek_bounds: Rc<Cell<Bounds<Pixels>>>,
    /// 音量条盒（同 `seek_bounds`）。
    vol_bounds: Rc<Cell<Bounds<Pixels>>>,
    /// `#video-main` 的滚动句柄：读它拿到**布局后**的可视区尺寸（百分比高度在 flex
    /// 子级里会退化为 auto → img 落到固有尺寸，竖屏被放大只露顶部；改按实测尺寸
    /// 手动 contain 适配，同 ImageViewer 的姿势）。
    viewport: ScrollHandle,
    /// 当前视频解码输出的长边封顶（px）：按显示尺寸自适应（render 里窗口变化时经
    /// `start_decoder_at(…, restart_audio=false)` 重开解码器而不打断音频）。
    decode_cap: u32,
    // --- M6 播放模式 / 截图 ---
    /// 播放模式三态（播完去向；控制条按钮循环切换）。
    playback_mode: PlaybackMode,
    /// 随机播放的 PRNG 状态（xorshift64*；`pick_shuffle_index` 自推进，测试可注入定值）。
    rng_state: u64,
    /// 瞬时状态浮层（右上角）：「文本, 出现时刻」；[`STATUS_MS`] 后自动清除。
    status: Option<(String, Instant)>,
    /// 状态浮层清空任务的代序号（防旧定时器误清新浮层，同 `controls_seq` 姿势）。
    status_seq: u32,
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
            decode_cap: DECODE_MAX_DIM,
            last_tick: None,
            folder: vec![path.clone()],
            current: 0,
            rx: None,
            overflow: None,
            generation: Arc::new(AtomicU64::new(0)),
            pump_running: false,
            pending_seek: None,
            audio: None,
            audio_base: 0.0,
            audio_gen: Arc::new(AtomicU64::new(0)),
            audio_err: false,
            diag_ticks: 0,
            decode_cmd: None,
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
            controls_visible: true,
            controls_seq: 0,
            controls_hovering: false,
            seek_bounds: Rc::new(Cell::new(Bounds::default())),
            vol_bounds: Rc::new(Cell::new(Bounds::default())),
            viewport: ScrollHandle::new(),
            playback_mode: PlaybackMode::Sequential,
            rng_state: seed_rng(),
            status: None,
            status_seq: 0,
        };
        this.start(cx);
        // 初始浮现控制条、无悬停 1s 后自隐（hover 会作废本次隐藏）
        this.show_controls_temporarily(cx);
        this
    }

    /// 后台解析参数 + 扫描同文件夹 → 启动解码线程与 pump。
    /// `VideoReader` 含裸指针（非 Send）：只在该后台任务内取参数，不把实体带回主线程。
    fn start(&mut self, cx: &mut Context<Self>) {
        let path = self.path.clone();
        cx.spawn(async move |weak, cx| {
            // 视频运行库缺失（主 exe 不再静态导入 FFmpeg，缺库也能启动）：不触碰任何
            // 解码 FFI，直接在窗口内报错提示。
            if !crate::video::decode::available() {
                let _ = weak.update(cx, |this, cx| {
                    this.loading = false;
                    this.load_error = Some(
                        "缺少视频运行库（video_ffmpeg.dll 或 avcodec-63.dll / avformat-63.dll 等 FFmpeg 运行库），视频功能不可用。请把它们放到程序 exe 同目录后重新打开。"
                            .to_string(),
                    );
                    cx.notify();
                });
                return;
            }
            let (folder, current) = cx
                .background_executor()
                .spawn(async move { scan_folder(&path) })
                .await;
            // 不单独预开 VideoReader 取参数（实测同一文件稍后被 worker 再 open 会
            // 卡死在 avformat_open_input）——改为给首个命令挂 outcome 通道，由 worker
            // 开成功回送参数 / 失败回送错误，本任务异步等结果再补齐时长/尺寸/音轨。
            let (out_tx, out_rx) = std::sync::mpsc::channel::<Result<super::decode::VideoParams, String>>();
            let _ = weak.update(cx, |this, cx| {
                this.folder = folder;
                this.current = current;
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
                        cx.notify();
                    });
                }
                Ok(Err(e)) => {
                    let _ = weak.update(cx, |this, cx| {
                        this.loading = false;
                        this.load_error = Some(e);
                        cx.notify();
                    });
                }
                Err(_) => {
                    // 超时：至少把 loading 关掉（画面静态，不卡佔位）
                    let _ = weak.update(cx, |this, cx| {
                        this.loading = false;
                        cx.notify();
                    });
                }
            }
        })
        .detach();
    }

    /// 启动（或重开）解码，从 `target` 秒开始。向**常驻视频 worker** 投新命令换代：
    /// 旧命令内层循环按 `generation` 立即停，worker 在**同一线程**内 drop 旧 reader 后
    /// 串行重开新文件——实测规避“新线程在上一解码器 teardown 后 avformat_open_input 卡死”
    ///（72.MP4 复现；同线程串行重开稳定：见 diag6）。
    /// `restart_audio`：seek/切换为 true（音频随命令重开、flush 归零）；纯「按显示尺寸
    /// 重开视频解码器」为 false（音频保持、播放进度不中断）。
    fn start_decoder_at(
        &mut self,
        target: f64,
        result_tx: Option<std::sync::mpsc::Sender<Result<super::decode::VideoParams, String>>>,
        restart_audio: bool,
    ) {
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.generation.store(generation, Ordering::SeqCst);
        let (tx, rx) = sync_channel::<FrameItem>(DECODE_AHEAD_FRAMES);
        // 换掉旧接收端 → worker 若正阻塞在 send 会收到失联错误、本命令内层循环退出
        self.overflow = None;
        if let Some(old) = self.rx.replace(rx) {
            drop(old);
        }
        self.pending_seek = Some(target);
        self.ensure_video_worker();
        let cmd = DecodeCmd {
            generation,
            path: self.path.clone(),
            target,
            cap: self.decode_cap,
            tx,
            result_tx,
        };
        if let Some(tx) = &self.decode_cmd {
            let _ = tx.send(cmd);
        }
        if restart_audio {
            self.start_audio_at(target);
        }
    }

    /// 惰性启动常驻视频解码 worker（一生一个；viewer Drop 时命令发送端被弃，
    /// worker 的 `recv()` 返回 Err 即退出）。
    fn ensure_video_worker(&mut self) {
        if self.decode_cmd.is_some() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel::<DecodeCmd>();
        let gen_arc = self.generation.clone();
        std::thread::spawn(move || Self::video_worker(rx, gen_arc));
        self.decode_cmd = Some(tx);
    }

    /// 常驻视频 worker 主循环：逐条执行命令（打开 → seek → 解码推帧，直到换代/EOF）。
    fn video_worker(
        rx: std::sync::mpsc::Receiver<DecodeCmd>,
        gen_arc: Arc<AtomicU64>,
    ) {
        while let Ok(cmd) = rx.recv() {
            // decode-to-display：sws 输出长边封顶 `cmd.cap`（按显示尺寸自适应，省
            // 内存带宽/上传；4K 源降载）；上一命令的 reader 在循环本轮开头已被 drop
            //（同线程串行，无并发 FFmpeg）
            let mut reader = match VideoReader::open_scaled(&cmd.path, cmd.cap) {
                Ok(r) => r,
                Err(e) => {
                    if let Some(out) = &cmd.result_tx {
                        let _ = out.send(Err(e.to_string()));
                    }
                    continue;
                }
            };
            if let Some(out) = &cmd.result_tx {
                let _ = out.send(Ok(reader.params()));
            }
            let _ = reader.seek(cmd.target);
            // VPROF=1 拆账：解码（FFI 内 decode+sws+to_vec）与推帧背压（tx.send，bounded
            // 通道满=主线程没在及时消费）的每帧墙钟，每 ~2s 打一行忙核。
            let vp_on = vprof_enabled();
            let mut vp_decode = Duration::ZERO;
            let mut vp_send = Duration::ZERO;
            let mut vp_frames: u64 = 0;
            let mut vp_window = Instant::now();
            while gen_arc.load(Ordering::SeqCst) == cmd.generation {
                let t0 = Instant::now();
                let item = reader.next_frame_bgra();
                let t1 = Instant::now();
                let keep = match item {
                    Ok(Some(item)) => cmd.tx.send(item).is_ok(),
                    _ => false, // EOF / 解码错误：本代结束
                };
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
                    break; // 接收端已被换代/弃用
                }
            }
        }
    }

    /// 惰性打开音频输出（不可用则纯画面播放，只报错一次）。
    fn ensure_audio(&mut self) {
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
    fn start_audio_at(&mut self, target: f64) {
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
    fn ensure_audio_worker(&mut self) {
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
    fn audio_worker(rx: std::sync::mpsc::Receiver<AudioCmd>, gen_arc: Arc<AtomicU64>) {
        while let Ok(cmd) = rx.recv() {
            cmd.sink.flush();
            let mut reader = match AudioReader::open_pcm_f32(&cmd.path, cmd.out_rate) {
                Ok(r) => r,
                Err(_) => continue,
            };
            cmd.ready.store(true, Ordering::Relaxed);
            let _ = reader.seek(cmd.target);
            while gen_arc.load(Ordering::SeqCst) == cmd.generation {
                match reader.next_pcm_f32() {
                    Ok(Some((_pts, samples))) => {
                        // 回压：缓冲满时等待（不把整段音频预解码塞爆内存）
                        while gen_arc.load(Ordering::SeqCst) == cmd.generation
                            && cmd.sink.queued_len() >= AUDIO_QUEUE_AIM
                        {
                            std::thread::sleep(std::time::Duration::from_millis(8));
                        }
                        if gen_arc.load(Ordering::SeqCst) != cmd.generation {
                            break;
                        }
                        cmd.sink.push(&samples);
                    }
                    _ => break, // EOF / 解码错误：本代结束
                }
            }
        }
    }

    /// 播放泵是否仍需运行。
    fn pump_wanted(&self) -> bool {
        self.playing || self.pending_seek.is_some()
    }

    /// 泵节拍：按内容帧率自适应（30fps→~33ms、60fps→16ms、25fps→40ms），下限
    /// 8ms（120fps）、上限 50ms（20fps 兜底未知/异常帧率）。旧版恒 16ms（62Hz）：
    /// 多出的拍没有新帧、纯为推进度条而强刷整窗，是播放 CPU 大头之一。
    fn pump_interval(&self) -> Duration {
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
    fn desired_decode_cap(&self, vw: f32, area_h: f32) -> u32 {
        let longest = (vw.max(area_h).max(1.0) * self.zoom).ceil() as u32;
        let snapped = longest.saturating_add(CAP_BUCKET - 1) / CAP_BUCKET * CAP_BUCKET;
        snapped.clamp(DECODE_MIN_DIM, DECODE_MAX_DIM)
    }

    fn ensure_pump(&mut self, cx: &mut Context<Self>) {
        if self.pump_running || !self.pump_wanted() {
            return;
        }
        self.pump_running = true;
        cx.spawn(async move |this, cx| {
            // 上次强制重绘时刻：仅「有新帧落屏」才每拍重绘；无新帧的闲置拍最多每
            // COARSE_REPAINT_MS 一次（推进度条/控制条），不再 16ms 把整窗全刷一遍。
            let mut last_paint: Option<Instant> = None;
            loop {
                let interval = this
                    .update(cx, |v, _| v.pump_interval())
                    .unwrap_or(Duration::from_millis(PUMP_MS));
                cx.background_executor()
                    .timer(interval)
                    .await;
                let now = Instant::now();
                let keep = this
                    .update(cx, |v, cx| {
                        let new_frame = v.tick(cx);
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

    /// 播放泵单拍（纯状态推进 + 上屏；调用方负责重绘）。返回本拍是否把**新帧上屏**
    ///（调用方据此只在有新帧时强制重绘，闲置拍不再每 16ms 全刷）。`hit_end` 也要重绘
    ///（控制条播放/暂停图标、以及循环/随机模式切到下一支的加载占位都要反映出来）。
    fn tick(&mut self, cx: &mut Context<Self>) -> bool {
        // 以音频时钟为播放进度（**仅当本媒体音频真在喂**；否则/未上线走墙钟——无声轨
        // 或切媒体后音频未就绪时画面也要前进，否则冻结）
        let audio_pos = (self.audio.is_some() && self.audio_ready.load(Ordering::Relaxed))
            .then(|| self.audio.as_ref().map(|s| s.position()).unwrap_or(0.0));
        let hit_end = self.advance_playhead(audio_pos);

        // 音频健康诊断：仅 `GPUI_EDITOR_VIDEO_AUDIO_DIAG=1` 时每 ~1s 打一行喂送统计
        //（欠载/锁竞争是滋滋声的根源形态；默认静默）
        const DIAG_ENV: &str = "GPUI_EDITOR_VIDEO_AUDIO_DIAG";
        if self.audio.is_some()
            && std::env::var_os(DIAG_ENV).is_some()
        {
            self.diag_ticks += 1;
            if self.diag_ticks >= 60 {
                self.diag_ticks = 0;
                if let Some(sink) = &self.audio {
                    let (calls, underrun, lockfail, played) = sink.stats();
                    eprintln!(
                        "[video-audio] calls={calls} underrun_samples={underrun} lockfail={lockfail} played_frames={played}"
                    );
                }
            }
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
        let new_frame = if let Some((_pts, w, h, bgra)) = latest {
            self.install_bgra(w, h, bgra);
            self.pending_seek = None; // 落点已上屏（首个可用帧）
            true
        } else {
            false
        };
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
    fn advance_playhead(&mut self, audio_pos: Option<f64>) -> bool {
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
    fn natural_end_target(&mut self) -> Option<usize> {
        let n = self.folder.len();
        if n <= 1 {
            return match self.playback_mode {
                PlaybackMode::Sequential => None,
                _ => Some(self.current),
            };
        }
        match self.playback_mode {
            PlaybackMode::Sequential => None,
            PlaybackMode::Loop => Some((self.current + 1) % n),
            PlaybackMode::Shuffle => Some(self.pick_shuffle_index()),
        }
    }

    /// 随机挑一个「≠ current」的下标（调用前须 `folder.len() ≥ 2`）：xorshift64* 自推进，
    /// `(x % n)` 撞上当前则顺移一位——结果恒在 `[0, n)` 且不等于 `current`；`rng_state`
    /// 可注入定值做确定性测试。
    fn pick_shuffle_index(&mut self) -> usize {
        let n = self.folder.len();
        debug_assert!(n >= 2, "随机播放仅在多文件清单下调用");
        let mut x = self.rng_state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng_state = x;
        let mut idx = (x % n as u64) as usize;
        if idx == self.current {
            idx = (idx + 1) % n;
        }
        idx
    }

    /// 自然播完的处理：按 [`PlaybackMode`] 切换到下一支 / 重播自身 / 保持暂停。
    fn handle_end(&mut self, cx: &mut Context<Self>) {
        match self.natural_end_target() {
            // 顺序播放：停在结尾（既有行为）
            None => {}
            // 单文件循环/随机：seek 0 重播（保留缩放/平移，不重开文件）
            Some(idx) if idx == self.current => self.seek(-self.duration, cx),
            Some(idx) => self.open_folder_index(idx, cx),
        }
    }

    /// 播放模式三态循环切换（控制条按钮）。
    fn cycle_playback_mode(&mut self, cx: &mut Context<Self>) {
        self.playback_mode = self.playback_mode.next();
        cx.notify();
    }

    // ---- M6：截图当前帧存 PNG ----

    /// 把当前上屏帧转成 PNG 用的 RGBA 像素（**点截图这次才拷贝**——`self.frame` 的
    /// `RenderImage` 始终持帧 BGRA，`as_bytes` 直接取回、不另存每帧副本）。返回 `None` =
    /// 尚无可截图画面。
    fn snapshot_rgba(frame: &gpui::RenderImage) -> Option<(u32, u32, Vec<u8>)> {
        let s = frame.size(0);
        let (w, h) = (s.width.0.max(0) as u32, s.height.0.max(0) as u32);
        if w == 0 || h == 0 {
            return None;
        }
        let mut rgba = frame.as_bytes(0)?.to_vec();
        // BGRA → RGBA（PNG 字节序；上屏字节序 BGRA 同 GPUI RenderImage，见 decode.rs 注释）
        for px in rgba.as_chunks_mut::<4>().0 {
            px.swap(0, 2);
        }
        Some((w, h, rgba))
    }

    /// 截图当前帧存 PNG：点截图时才从 `self.frame` 拷贝一份（零每帧开销），BGRA → RGBA
    /// 后写视频同目录（不可写回退系统临时目录）。PNG 编码放后台线程（1080p ~几十 ms，
    /// 不卡主线程播放泵）；完成经 [`Self::show_status`] 右上角浮层提示路径。
    fn screenshot_png(&mut self, cx: &mut Context<Self>) {
        let Some(frame) = self.frame.clone() else {
            self.show_status("暂无画面可截图：视频尚未解码出帧".to_string(), cx);
            return;
        };
        let Some((w, h, rgba)) = Self::snapshot_rgba(&frame) else {
            self.show_status("暂无画面可截图：视频尚未解码出帧".to_string(), cx);
            return;
        };
        let path = self.screenshot_path();
        let display = path.display().to_string();
        self.show_status("正在保存截图…".to_string(), cx);
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    image::save_buffer_with_format(
                        &path,
                        &rgba,
                        w,
                        h,
                        image::ExtendedColorType::Rgba8,
                        image::ImageFormat::Png,
                    )
                })
                .await;
            let _ = this.update(cx, |this, cx| match result {
                Ok(()) => this.show_status(format!("已保存截图：{display}"), cx),
                Err(e) => this.show_status(format!("截图保存失败：{e}"), cx),
            });
        })
        .detach();
    }

    /// 截图落盘路径：视频同目录 `<片名>-截图-<UTC时间戳>.png`；同目录不可写时回退系统
    /// 临时目录（截图像素已按显示尺寸 cap 好，不随 PNG 增大）。
    fn screenshot_path(&self) -> PathBuf {
        let name = format!(
            "{}-截图-{}.png",
            self.path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "视频".to_string()),
            Self::utc_timestamp()
        );
        let dir = self
            .path
            .parent()
            .filter(|d| d.is_dir())
            .map(|d| d.to_path_buf())
            .unwrap_or_else(std::env::temp_dir);
        dir.join(name)
    }

    /// 当前 UTC 时间的 `YYYYMMDD-HHMMSS`（截图文件名用）：Howard Hinnant 民用算法从
    /// Unix 秒直接算日历，不依赖 chrono——唯一且可排序，够截图命名。
    fn utc_timestamp() -> String {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let days = secs / 86400;
        let rem = secs % 86400;
        let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = if m <= 2 { y + 1 } else { y };
        format!("{y:04}{m:02}{d:02}-{hh:02}{mm:02}{ss:02}")
    }

    /// 右上角瞬时状态浮层：显示 `text`，[`STATUS_MS`] 后自动清除（带代序号防旧定时器
    /// 误清新浮层，同 `show_controls_temporarily` 姿势）。
    fn show_status(&mut self, text: String, cx: &mut Context<Self>) {
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

    /// 把 BGRA 包成 `RenderImage` 上屏。`intrinsic` 优先保留打开期从 `params` 读到的
    /// **源尺寸**（decode-to-display 下采样后逐帧 w/h 是输出尺寸，宽高比一致即可）。
    fn install_bgra(&mut self, w: u32, h: u32, bgra: Vec<u8>) {
        let t0 = Instant::now();
        // 帧字节移入 RenderImage（零拷贝）即持有时；截图在点击时经 `frame.as_bytes` 取回
        let buf = RgbaImage::from_raw(w, h, bgra).expect("解码帧长度恒为 w*h*4");
        self.frame = Some(Arc::new(RenderImage::new(vec![Frame::new(buf)])));
        self.intrinsic.get_or_insert((w as f32, h as f32));
        if vprof_enabled() {
            VPROF_PRESENT.with(|acc| {
                let mut acc = acc.borrow_mut();
                acc.0 += 1;
                acc.1 += t0.elapsed();
                if acc.0 % 120 == 0 {
                    eprintln!(
                        "[VPROF] main present: {:.3}ms/帧（Frame/RenderImage 装配）",
                        acc.1.as_secs_f64() * 1000.0 / acc.0 as f64
                    );
                }
            });
        }
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
        self.zoom = 1.0; // 切文件：缩放复位为适应视口
        self.pan = (0.0, 0.0); // 平移同步归零（拖拽中的惯性也清掉）
        self.drag = None;
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
        let (_tx, rx) = sync_channel::<FrameItem>(DECODE_AHEAD_FRAMES);
        if let Some(old) = self.rx.replace(rx) {
            drop(old);
        }
        self.start(cx);
        // 切视频：控制条浮现（锁定模式默认可见随后自隐；普通模式常驻）
        self.show_controls_temporarily(cx);
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
            if let Some(sink) = &self.audio {
                let _ = sink.play();
            }
        } else if let Some(sink) = &self.audio {
            sink.pause();
        }
        cx.notify();
    }

    fn seek(&mut self, delta: f64, cx: &mut Context<Self>) {
        let target = (self.playhead + delta).clamp(0.0, self.duration.max(0.0));
            self.playhead = target;
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
    fn seek_to_fraction(&mut self, fraction: f32, cx: &mut Context<Self>) {
        let target = f64::from(fraction.clamp(0.0, 1.0)) * self.duration.max(0.0);
        self.seek(target - self.playhead, cx);
    }

    /// 生效音量：静音 → 0，否则 `volume`（只改一条路径，供 `AudioSink::set_volume`）。
    fn effective_volume(&self) -> f32 {
        if self.muted {
            0.0
        } else {
            self.volume
        }
    }

    /// 把当前生效音量推给音频输出（打开/调节/静音切换后调用；无音频时空操作）。
    fn apply_volume(&mut self) {
        if let Some(sink) = &self.audio {
            sink.set_volume(self.effective_volume());
        }
    }

    /// 设置音量（钳位 0..=1，不改静音态；静音时仅记录，恢复后生效）。
    fn set_volume(&mut self, volume: f32, cx: &mut Context<Self>) {
        self.volume = volume.clamp(0.0, 1.0);
        self.apply_volume();
        cx.notify();
    }

    /// 静音切换：生效音量 0 ⇄ 原音量。
    fn toggle_mute(&mut self, cx: &mut Context<Self>) {
        self.muted = !self.muted;
        self.apply_volume();
        cx.notify();
    }

    /// 设置倍速：音频时钟进程不变（`audio_base` 保持当前进度），只换重采样速率
    /// 重启音频线程；视频帧按源秒时间轴照旧排空，A/V 同速。纯画面（无音频）只需
    /// 更新 `speed`（墙钟按 `dt×speed`）。
    fn set_speed(&mut self, speed: f32, cx: &mut Context<Self>) {
        self.speed = speed.clamp(0.25, 4.0);
        if self.audio.is_some() {
            self.start_audio_at(self.playhead);
        }
        cx.notify();
    }

    /// 倍速档位循环步进（`dir` = +1 下一档 / -1 上一档）。
    fn speed_step(&mut self, dir: i32, cx: &mut Context<Self>) {
        let n = SPEED_STEPS.len() as i64;
        let idx = SPEED_STEPS
            .iter()
            .position(|&s| (s - self.speed).abs() < 1e-6)
            .unwrap_or_else(|| SPEED_STEPS.iter().position(|&s| (s - 1.0).abs() < 1e-6).unwrap_or(1));
        let next = (idx as i64 + i64::from(dir)).rem_euclid(n) as usize;
        self.set_speed(SPEED_STEPS[next], cx);
    }

    /// 当前视频路径（宿主 `EditorApp` 同步标签标题 / 状态栏用；同文件夹切换后即为新路径）。
    pub fn current_path(&self) -> Option<PathBuf> {
        Some(self.path.clone())
    }

    /// 当前视频在同文件夹清单中的下标（0 基）。
    pub fn current_index(&self) -> usize {
        self.current
    }

    /// 同文件夹视频总数。
    pub fn folder_len(&self) -> usize {
        self.folder.len()
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
            "m" => self.toggle_mute(cx),
            "-" => self.set_volume(self.volume - VOLUME_STEP, cx),
            "=" => self.set_volume(self.volume + VOLUME_STEP, cx),
            "[" => self.speed_step(-1, cx),
            "]" => self.speed_step(1, cx),
            "s" => self.screenshot_png(cx),
            _ => return, // 其余键不拦截
        }
        cx.stop_propagation();
    }

    /// 滚轮处理（视频画布自己吃掉，不参与进度）：`Ctrl+滚轮` → 缩放；`Shift+滚轮` →
    /// 同文件夹切换；普通滚轮**不做任何事**（不再控制进度）。`stop_propagation` 保持
    /// 「视频占满整区、滚轮不滚动任何东西」的既有语义。
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
        self.handle_wheel(delta_y, ev.modifiers.control, ev.modifiers.shift, cx);
        cx.stop_propagation();
    }

    /// 滚轮分发（可单测）：`ctrl` → 缩放；`shift` → 同文件夹切换；否则空操作。
    fn handle_wheel(
        &mut self,
        delta_y: f32,
        ctrl: bool,
        shift: bool,
        cx: &mut Context<Self>,
    ) {
        if delta_y == 0.0 {
            return;
        }
        if ctrl {
            // Ctrl+滚轮：放大/缩小（向上滚 = 放大，向下 = 缩小；与图片/PDF 阅读器一致）
            let step = delta_y.clamp(-1.0, 1.0);
            self.adjust_zoom(1.0 + step * ZOOM_STEP, cx);
        } else if shift {
            // Shift+滚轮：同文件夹切换（向下滚 = 下一支）
            self.switch_file(if delta_y < 0.0 { 1 } else { -1 }, cx);
        }
    }

    /// 应用缩放因子并约束在 [`ZOOM_MIN`..=`ZOOM_MAX`]；变化才 notify 重绘。
    fn adjust_zoom(&mut self, factor: f32, cx: &mut Context<Self>) {
        let new_zoom = (self.zoom * factor).clamp(ZOOM_MIN, ZOOM_MAX);
        if (new_zoom - self.zoom).abs() < 1e-4 {
            return;
        }
        self.zoom = new_zoom;
        cx.notify();
    }

    // ---- 放大后左键按住拖动画面（平移；未放大时 clamp 恒 0 = 无效果） ----

    /// 当前图在**画布区**（视口高 − 控制条高）内的 contain 显示尺寸（× 缩放）。
    /// 拖拽平移与 render 共用同一套尺寸，保证 clamp 与画面一致。
    fn display_size(&self, vw: f32, vh: f32) -> (f32, f32) {
        let area_h = if self.locked {
            vh.max(1.0)
        } else {
            (vh - CONTROL_BAR_H).max(1.0)
        };
        let (iw, ih) = self.intrinsic.unwrap_or((0.0, 0.0));
        let (dw, dh) = Self::contain_fit(iw, ih, vw, area_h);
        (dw * self.zoom, dh * self.zoom)
    }

    /// 左键按下开始拖拽：记录起点鼠标位置 + 当前 `pan`（未放大时 clamp 恒 0，拖动无效果）。
    fn on_pan_down(
        &mut self,
        ev: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if ev.button == MouseButton::Left {
            self.drag = Some((
                (ev.position.x.as_f32(), ev.position.y.as_f32()),
                self.pan,
            ));
            cx.notify();
        }
    }

    /// 拖拽平移：按「当前鼠标 − 起点」增量更新 `pan`，clamp 到图始终覆盖画布区。
    fn on_pan_move(
        &mut self,
        ev: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((start_pos, start_pan)) = self.drag else {
            return;
        };
        let (vw, vh) = self.viewport_size(window);
        let area_h = if self.locked {
            vh.max(1.0)
        } else {
            (vh - CONTROL_BAR_H).max(1.0)
        };
        let (dw, dh) = self.display_size(vw, vh);
        let dx = ev.position.x.as_f32() - start_pos.0;
        let dy = ev.position.y.as_f32() - start_pos.1;
        self.pan = Self::clamp_pan((start_pan.0 + dx, start_pan.1 + dy), vw, area_h, dw, dh);
        cx.notify();
    }

    /// 拖拽结束（窗口内/外松开统一清态；`on_mouse_up` / `on_mouse_up_out` 共用）。
    fn on_pan_up(&mut self, _ev: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if self.drag.is_some() {
            self.drag = None;
            cx.notify();
        }
    }

    /// 平移 clamp：放大后（`dw>vw` 或 `dh>vh`）允许在图内滑动，范围 = ±(图−视口)/2；
    /// 未放大时范围 0（永远居中，拖拽无效果）。以「视口中心 = 图中心」为 0 基准，
    /// 缩放/切图后范围变化由 render 逐帧重新 clamp 收敛（同 ImageViewer）。
    fn clamp_pan(pan: (f32, f32), vw: f32, vh: f32, dw: f32, dh: f32) -> (f32, f32) {
        let ax = (dw - vw).max(0.0) * 0.5;
        let ay = (dh - vh).max(0.0) * 0.5;
        (pan.0.clamp(-ax, ax), pan.1.clamp(-ay, ay))
    }

    // ---- 纯视频模式（参考纯图片锁定）：隐藏外壳、视频充满窗口、控制条为浮层 ----

    /// 是否「纯视频」干净播放模式（宿主据此隐藏菜单栏/选项卡栏/侧边栏/状态栏，视频充满窗口）。
    pub fn is_locked(&self) -> bool {
        self.locked
    }

    /// 切换纯视频模式：锁定 = 隐藏非视频 UI、视频充满窗口（宿主壳体重排后自动缩放填满）、
    /// 控制条浮层按需浮现；解锁回归。切换时平移/拖拽归零。
    pub fn toggle_lock(&mut self, cx: &mut Context<Self>) {
        self.locked = !self.locked;
        self.pan = (0.0, 0.0);
        self.drag = None;
        // 进/出锁定都让控制条浮现一次（锁定态随即 1s 自隐）
        self.show_controls_temporarily(cx);
        cx.notify();
    }

    // ---- 控制条浮现 / 自隐（锁定模式浮层；普通模式常驻） ----

    /// 控制条浮现并排队「离开 1s 后自隐」（切视频 / hover 离开时调用；悬停中不隐藏）。
    /// 带代序号防旧定时器误清新浮现。
    fn show_controls_temporarily(&mut self, cx: &mut Context<Self>) {
        self.controls_visible = true;
        cx.notify();
        self.controls_seq = self.controls_seq.wrapping_add(1);
        let seq = self.controls_seq;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_secs(1))
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.controls_seq == seq && !this.controls_hovering {
                    this.controls_visible = false;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// 查看器悬停态：`#video-viewer` 根 hover。进入 → 浮现且不隐藏；离开 → 1s 后自隐。
    fn on_controls_hover(&mut self, hovered: &bool, _window: &mut Window, cx: &mut Context<Self>) {
        self.set_controls_hover(*hovered, cx);
    }

    /// 悬停态设置（独立于窗口，便于测试）：悬停期间不受自动隐藏影响。
    fn set_controls_hover(&mut self, hovered: bool, cx: &mut Context<Self>) {
        self.controls_hovering = hovered;
        if hovered {
            self.controls_seq = self.controls_seq.wrapping_add(1); // 作废排队中的隐藏
            self.controls_visible = true;
            cx.notify();
        } else {
            self.show_controls_temporarily(cx);
        }
    }

    /// 居中占位文本（加载中 / 加载失败共用）：铺满画布再 flex 居中。
    fn placeholder(text: &str, color: Hsla) -> gpui::AnyElement {
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(14.))
            .text_color(color)
            .child(text.to_string())
            .into_any_element()
    }

    /// 阅读画布底色（同 ImageViewer：亮 #F5F5F5 / 暗 #202020，中性灰阶、与外壳 `window_bg` 同源）。
    fn default_canvas(cx: &App) -> Hsla {
        let dark = crate::ui::theme::Theme::global(cx).is_dark();
        crate::ui::theme::ChromeColors::for_mode(dark).window_bg.into()
    }

    // --- M5 控制条 ---

    /// 可视区尺寸：`#video-main` 布局后的真实尺寸（`track_scroll` 记录）；首帧未布局
    /// （bounds 为 0）回退到窗口尺寸（对齐 ImageViewer `viewport_size` 的既有做法）。
    fn viewport_size(&self, window: &Window) -> (f32, f32) {
        let s = self.viewport.bounds().size;
        let w = if s.width.as_f32() > 0.0 {
            s.width.as_f32()
        } else {
            window.bounds().size.width.as_f32()
        };
        let h = if s.height.as_f32() > 0.0 {
            s.height.as_f32()
        } else {
            window.bounds().size.height.as_f32()
        };
        (w, h)
    }

    /// 适应视口（Contain，**贴边无留白**）：取较小缩放比，一端正好贴住宽/高、另一端
    /// 居中留白（同 ImageViewer `contain_fit`——竖屏/横屏都保持原始比例缩放）。
    fn contain_fit(iw: f32, ih: f32, vw: f32, vh: f32) -> (f32, f32) {
        let fit = (vw / iw).min(vh / ih).max(0.001);
        (iw * fit, ih * fit)
    }

    /// 播放进度比例（0..=1；未知时长时 0）。
    fn ratio(&self) -> f32 {
        if self.duration > 0.0 {
            (self.playhead / self.duration) as f32
        } else {
            0.0
        }
    }

    /// `秒` → `m:ss`（控制条时间显示）。
    fn fmt_time(secs: f64) -> String {
        let s = secs.max(0.0) as u64;
        format!("{:01}:{:02}", s / 60, s % 60)
    }

    /// 底部全屏控制条：上一支 / 播放·暂停 / 下一支 / 时间 / seek 进度条 /
    /// 静音 + 音量条 / 倍速循环 / 清单位置。
    ///
    /// 普通模式**常驻**显示。纯视频（锁定）模式为浮层：恒挂一根底部热区（`#video-controls`），
    /// `controls_visible` 时渲染整条，隐藏时是透明热区——hover **只挂在这根条带**上（进入即
    /// 浮现且不隐藏，离开 1s 后自隐；鼠标在条带之外的画面区域不触发，锁定的初始自隐不会被
    /// 画面内停留的鼠标打断）。
    fn control_bar(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let strip = div()
            .id("video-controls")
            .absolute()
            .bottom_0()
            .left_0()
            .right_0()
            .h(px(CONTROL_BAR_H))
            .on_hover(cx.listener(Self::on_controls_hover));
        if self.locked && !self.controls_visible {
            // 纯视频模式隐藏：透明热区（继续占底部、可接收 hover 触发浮现）
            return strip.into_any_element();
        }
        let colors = crate::ui::theme::Theme::global(cx).colors;
        let fg: Hsla = colors.fg.into();
        let muted: Hsla = colors.fg_muted.into();
        let surface: Hsla = colors.surface.into();
        let accent: Hsla = colors.accent.into();
        let entity = cx.entity();

        let seek_strip = Self::strip_element(
            "video-seek",
            self.seek_bounds.clone(),
            self.ratio(),
            muted,
            accent,
            true,
            cx,
        );
        let vol_strip = Self::strip_element(
            "video-vol",
            self.vol_bounds.clone(),
            self.effective_volume(),
            muted,
            accent,
            false,
            cx,
        );

        let time = format!(
            "{} / {}",
            Self::fmt_time(self.playhead),
            Self::fmt_time(self.duration)
        );
        let playlist = format!(
            "{}/{} · {:.0}%",
            self.current + 1,
            self.folder.len(),
            self.zoom * 100.0
        );

        let play_icon = if self.playing {
            IconName::Pause
        } else {
            IconName::Play
        };
        let vol_icon = if self.muted || self.effective_volume() <= 0.001 {
            IconName::VolumeX
        } else {
            IconName::Volume
        };

        strip
            .bg(surface)
            .flex()
            .items_center()
            .gap_x(px(8.))
            .px_3()
            .child(control_icon_button(
                "video-prev",
                IconName::SkipBack,
                fg,
                entity.clone(),
                |v, cx| v.switch_file(-1, cx),
            ))
            .child(control_icon_button(
                "video-play",
                play_icon,
                accent,
                entity.clone(),
                |v, cx| v.toggle_play(cx),
            ))
            .child(control_icon_button(
                "video-next",
                IconName::SkipForward,
                fg,
                entity.clone(),
                |v, cx| v.switch_file(1, cx),
            ))
            .child(
                div()
                    .whitespace_nowrap()
                    .text_size(px(11.))
                    .text_color(muted)
                    .child(time),
            )
            .child(seek_strip)
            .child(control_icon_button(
                "video-mute",
                vol_icon,
                fg,
                entity.clone(),
                |v, cx| v.toggle_mute(cx),
            ))
            .child(vol_strip)
            .child({
                // 模式按钮在 speed/clone 之外单独 clone：后文 camera 按钮仍需用 entity
                let entity = entity.clone();
                Button::new("video-mode")
                    .ghost()
                    .label(self.playback_mode.label())
                    .w(px(44.))
                    .h(px(28.))
                    .text_size(px(11.))
                    .on_click(move |_, _w, app| {
                        entity.update(app, |v, cx| v.cycle_playback_mode(cx));
                    })
            })
            .child({
                let entity = entity.clone();
                Button::new("video-speed")
                    .ghost()
                    .label(format!("{:.2}×", self.speed))
                    .w(px(40.))
                    .h(px(28.))
                    .text_size(px(11.))
                    .on_click(move |_, _w, app| {
                        entity.update(app, |v, cx| v.speed_step(1, cx));
                    })
            })
            .child(
                div()
                    .whitespace_nowrap()
                    .text_size(px(11.))
                    .text_color(muted)
                    .child(playlist),
            )
            .child(control_icon_button(
                "video-shot",
                IconName::Camera,
                accent,
                entity.clone(),
                |v, cx| v.screenshot_png(cx),
            ))
            .into_any_element()
    }

    /// 可交互横向条（seek 进度条 / 音量条共用）：轨道 + 填充（宽度=分数×父宽）+ 无绘制
    /// canvas 记录**窗口坐标**盒到 `bounds`（渲染期写入；事件回调读）。`seek=true`
    /// 时 `flex_1` 拉伸并走 seek 处理器，否则固定宽走音量处理器。
    fn strip_element(
        id: &'static str,
        bounds: Rc<Cell<Bounds<Pixels>>>,
        fraction: f32,
        track: Hsla,
        fill: Hsla,
        seek: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let f = fraction.clamp(0.0, 1.0);
        let mut el = div()
            .id(id)
            .h(px(16.))
            .relative()
            .cursor(CursorStyle::PointingHand)
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .rounded_full()
                    .bg(track),
            )
            .child(
                div()
                    .absolute()
                    .left_0()
                    .top_0()
                    .bottom_0()
                    .w(DefiniteLength::Fraction(f))
                    .rounded_full()
                    .bg(fill),
            )
            .child(canvas(
                move |b, _w, _app| {
                    bounds.set(b);
                },
                |_, _, _, _| {},
            ))
            .when(seek, |this| this.flex_1())
            .when(!seek, |this| this.w(px(56.)));
        if seek {
            el = el
                .on_mouse_down(MouseButton::Left, cx.listener(Self::on_seek_down))
                .on_mouse_move(cx.listener(Self::on_seek_move));
        } else {
            el = el
                .on_mouse_down(MouseButton::Left, cx.listener(Self::on_vol_down))
                .on_mouse_move(cx.listener(Self::on_vol_move));
        }
        el.into_any_element()
    }

    /// seek 点按：屏 x → 分数 → seek。
    fn on_seek_down(
        &mut self,
        e: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(f) = fraction_at(&self.seek_bounds, e.position) {
            self.seek_to_fraction(f, cx);
        }
    }

    /// seek 拖拽（左键按住才推进；移出条后松开无后续）。
    fn on_seek_move(
        &mut self,
        e: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !e.dragging() {
            return;
        }
        if let Some(f) = fraction_at(&self.seek_bounds, e.position) {
            self.seek_to_fraction(f, cx);
        }
    }

    /// 音量条点按：屏 x → 分数 → 音量。
    fn on_vol_down(
        &mut self,
        e: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(f) = fraction_at(&self.vol_bounds, e.position) {
            self.set_volume(f, cx);
        }
    }

    /// 音量条拖拽。
    fn on_vol_move(
        &mut self,
        e: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !e.dragging() {
            return;
        }
        if let Some(f) = fraction_at(&self.vol_bounds, e.position) {
            self.set_volume(f, cx);
        }
    }
}

impl Drop for VideoViewer {
    fn drop(&mut self) {
        // 换代让还在跑的视频/音频解码线程下一趟自退，避免实体销毁后僵尸线程继续占核
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.audio_gen.fetch_add(1, Ordering::SeqCst);
    }
}

impl Render for VideoViewer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let canvas = self.bg.unwrap_or_else(|| Self::default_canvas(cx));
        let theme = crate::ui::theme::Theme::global(cx);
        let muted: Hsla = theme.colors.fg_muted.into();
        // 状态浮层（右上角）配色：Copy 抽出，避免在 `control_bar(&mut cx)` 之后借用 theme
        let toast_surface: Hsla = theme.colors.surface.into();
        let toast_border: Hsla = theme.colors.border.into();
        let toast_fg: Hsla = theme.colors.fg.into();

        let (vw, vh) = self.viewport_size(window);
        // 普通模式预留底部控制条高度（控制条常驻，画面 contain 到其上）；
        // 纯视频（锁定）模式控制条是浮层 → 画面充满整个画布区。
        let area_h = if self.locked {
            vh.max(1.0)
        } else {
            (vh - CONTROL_BAR_H).max(1.0)
        };
        // 按显示尺寸自适应解码封顶：画面已上屏且窗口尺寸落进新的 256px「桶」时，只
        // 重开视频解码器（restart_audio=false 不打断音频/进度），把 sws 输出与逐帧
        // 上传切成显示所需像素数（省 decode/swscale/纹理上传，播放 CPU 大头之一）。
        if self.frame.is_some()
            && !self.loading
            && self.pending_seek.is_none()
            && vw > 0.0
            && area_h > 0.0
        {
            let wanted = self.desired_decode_cap(vw, area_h);
            if wanted != self.decode_cap {
                self.decode_cap = wanted;
                let t = self.playhead;
                self.start_decoder_at(t, None, false);
            }
        }
        let body: gpui::AnyElement = if let Some(err) = &self.load_error {
            Self::placeholder(&format!("无法打开视频：{err}"), muted)
        } else {
            match (self.frame.clone(), self.intrinsic) {
                (Some(render), Some((iw, ih))) => {
                    // 手动 contain 适配 × 缩放（绝对定位 + 显式像素框，**不用 100% 尺寸**）：
                    // 竖屏/横屏都等比缩放居中，不会因 flex 子级百分比高度失效而按固有
                    // 尺寸放大只露局部（对齐 ImageViewer 的 display_size 姿势）。放大超出
                    // 视口时由 `#video-viewer overflow_hidden` 裁剪在画布内。
                    let (dw, dh) = Self::contain_fit(iw, ih, vw, area_h);
                    let (dw, dh) = (dw * self.zoom, dh * self.zoom);
                    // 放大后拖拽平移：clamp 收敛到图覆盖画布区（未放大时 pan 恒 0 =
                    // 居中；中心锚定缩放），再把偏移并到像素框坐标。
                    self.pan = Self::clamp_pan(self.pan, vw, area_h, dw, dh);
                    let (ox, oy) = ((vw - dw) * 0.5 + self.pan.0, (area_h - dh) * 0.5 + self.pan.1);
                    img(gpui::ImageSource::Render(render))
                        .object_fit(ObjectFit::Contain)
                        .absolute()
                        .left(px(ox))
                        .top(px(oy))
                        .w(px(dw))
                        .h(px(dh))
                        .into_any_element()
                }
                (_, _) if self.playing || self.loading => {
                    Self::placeholder("正在打开视频…", muted)
                }
                _ => Self::placeholder("暂停", muted),
            }
        };

        let mut root = div()
            .id("video-viewer")
            .size_full()
            .relative()
            .overflow_hidden()
            .bg(canvas)
            .child(
                div()
                    .id("video-main")
                    .size_full()
                    .relative()
                    .track_scroll(&self.viewport)
                    .track_focus(&self.focus_handle)
                    .cursor(if self.drag.is_some() {
                        CursorStyle::ClosedHand
                    } else {
                        CursorStyle::OpenHand
                    })
                    .on_key_down(cx.listener(Self::on_key_down))
                    .on_scroll_wheel(cx.listener(Self::on_scroll_wheel))
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::on_pan_down))
                    .on_mouse_move(cx.listener(Self::on_pan_move))
                    .on_mouse_up(MouseButton::Left, cx.listener(Self::on_pan_up))
                    .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_pan_up))
                    .child(body),
            );
        // 控制条：普通模式常驻（画面预留底部高度）；纯视频（锁定）模式为浮层按需浮现，
        // 画面充满整个画布区。
        root = root.child(self.control_bar(window, cx));
        // 右上角瞬时状态浮层（截图结果提示等）：在期内才挂，过期由定时器清除兜底。
        if let Some((text, at)) = &self.status
            && at.elapsed() < std::time::Duration::from_millis(STATUS_MS)
        {
            root = root.child(
                div()
                    .absolute()
                    .top_2()
                    .right_2()
                    .px_3()
                    .py_1()
                    .rounded_md()
                    .border_1()
                    .border_color(toast_border)
                    .bg(toast_surface)
                    .text_size(px(12.))
                    .text_color(toast_fg)
                    .child(text.clone()),
            );
        }
        root
    }
}

/// 控制条图标按钮：ghost 变体 + 指定图标，点击回调经 `entity.update` 落到查看器方法
///（`Button::on_click` 收 `&mut App`，故经实体中转）。
fn control_icon_button(
    id: &'static str,
    icon: IconName,
    tint: Hsla,
    entity: gpui::Entity<VideoViewer>,
    action: impl Fn(&mut VideoViewer, &mut Context<VideoViewer>) + 'static,
) -> gpui::AnyElement {
    Button::new(id)
        .ghost()
        .w(px(28.))
        .h(px(28.))
        .child(Icon::new(icon).size(px(16.)).text_color(tint))
        .on_click(move |_, _w, app| {
            entity.update(app, |v, cx| action(v, cx));
        })
        .into_any_element()
}

/// 倍速下喂给 `AudioReader` 的重采样输出率：`设备率 ÷ speed`（钳位 0.25..=4，至少 1）。
/// 证明：1 源秒 = `out_rate` 个样本，设备每秒消费 `device_rate` 个 → 1 源秒播
/// `out_rate/device_rate = 1/speed` 墙秒，即源以 `speed` 倍播放。
fn audio_read_rate(device_rate: u32, speed: f32) -> u32 {
    let speed = speed.clamp(0.25, 4.0);
    (f64::from(device_rate) / f64::from(speed)).round().max(1.0) as u32
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
            intrinsic: None,
            loading: false,
            load_error: None,
            bg: None,
            playing: false,
            playhead: 0.0,
            duration: 2.0,
            fps: None,
            decode_cap: DECODE_MAX_DIM,
            last_tick: None,
            folder: vec![fixture()],
            current: 0,
            rx: None,
            overflow: None,
            generation: Arc::new(AtomicU64::new(0)),
            pump_running: false,
            pending_seek: None,
            audio: None,
            audio_base: 0.0,
            audio_gen: Arc::new(AtomicU64::new(0)),
            audio_err: false,
            diag_ticks: 0,
            decode_cmd: None,
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
            controls_visible: true,
            controls_seq: 0,
            controls_hovering: false,
            seek_bounds: Rc::new(Cell::new(Bounds::default())),
            vol_bounds: Rc::new(Cell::new(Bounds::default())),
            viewport: ScrollHandle::new(),
            playback_mode: PlaybackMode::Sequential,
            rng_state: 42, // 固定种子：随机播放测试可预期
            status: None,
            status_seq: 0,
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
        ent.update(cx, |v, cx| {
            v.rx = Some(rx);
            v.playhead = 0.5;
            v.tick(cx);
        });
        let s = cx.update(|app| {
            let v = ent.read(app);
            (v.frame.is_some(), v.overflow.as_ref().map(|f| f.0))
        });
        assert!(s.0, "应有帧上屏");
        assert_eq!(s.1, Some(1.5), "越线帧保留在 overflow");
        // playhead 越过 1.5：旧 overflow(1.5) 移交上屏、通道里的 1.6 进位为 overflow
        tx.send(frame_of(1.6, 4)).unwrap();
        ent.update(cx, |v, cx| {
            v.playhead = 1.55;
            v.tick(cx);
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
            v.folder = vec![fixture(), fixture()];
            v.playhead = 0.7;
            v.zoom = 1.0;
            v
        }));
        // 普通滚轮：playhead / current 都不动
        ent.update(cx, |v, cx| v.handle_wheel(1.0, false, false, cx));
        let s = cx.update(|app| {
            let v = ent.read(app);
            (v.playhead, v.current)
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
        assert_eq!(cx.update(|app| ent.read(app).current), 1, "Shift 向下切下一支");
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
            crate::ui::theme::Theme::set(crate::ui::theme::ThemeMode::Dark, cx);
        });
        let (viewer, wcx) = cx.add_window_view(|_window, cx| {
            // 用不存在的路径：start 快速落错误、不开真实解码 → 不占 FFmpeg 锁
            // （拖拽只依赖 intrinsic/zoom，毋需真实帧）
            VideoViewer::new(
                std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/no-such-drag.mp4"),
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
            cx.update(|app| ent.read(app).controls_visible),
            "进锁定控制条默认可见"
        );
        cx.update(|app| ent.update(app, |v, cx| v.toggle_lock(cx)));
        assert!(!cx.update(|app| ent.read(app).is_locked()));
        cx.run_until_parked();
    }

    /// 纯视频（锁定）模式控制条浮层时序：进锁定默认可见 → 无悬停 1s 自隐 → 悬停不隐藏 →
    /// 移开 1s 隐藏 → 切视频浮现。普通模式控制条常驻（`controls_visible` 不参与渲染判断）。
    #[gpui::test]
    async fn locked_controls_bar_floats_on_hover_and_auto_hides(cx: &mut TestAppContext) {
        let a = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/no-a-lock.mp4");
        let b = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/no-b-lock.mp4");
        let ent = cx.update(|app| app.new(|cx| {
            let mut v = blank(a.clone(), cx);
            v.folder = vec![a, b];
            v.current = 0;
            v
        }));
        // 进入锁定 → 控制条默认可见（随后无悬停 1s 自隐）
        cx.update(|app| ent.update(app, |v, cx| v.toggle_lock(cx)));
        assert!(cx.update(|app| ent.read(app).is_locked()));
        assert!(cx.update(|app| ent.read(app).controls_visible), "进锁定默认可见");
        cx.executor().advance_clock(std::time::Duration::from_secs(1));
        cx.run_until_parked();
        assert!(
            !cx.update(|app| ent.read(app).controls_visible),
            "无悬停 1s 后应自隐"
        );
        // 悬停 → 浮现，且 2s 内不隐藏
        cx.update(|app| ent.update(app, |v, cx| v.set_controls_hover(true, cx)));
        assert!(cx.update(|app| ent.read(app).controls_visible));
        cx.executor().advance_clock(std::time::Duration::from_secs(2));
        cx.run_until_parked();
        assert!(
            cx.update(|app| ent.read(app).controls_visible),
            "悬停期间不自动隐藏"
        );
        // 移开 → 1s 后隐藏
        cx.update(|app| ent.update(app, |v, cx| v.set_controls_hover(false, cx)));
        assert!(
            cx.update(|app| ent.read(app).controls_visible),
            "刚移开仍可见（排队 1s）"
        );
        cx.executor().advance_clock(std::time::Duration::from_secs(1));
        cx.run_until_parked();
        assert!(
            !cx.update(|app| ent.read(app).controls_visible),
            "移开后 1s 应隐藏"
        );
        // 切视频 → 浮现
        cx.update(|app| ent.update(app, |v, cx| v.open_folder_index(1, cx)));
        assert!(
            cx.update(|app| ent.read(app).controls_visible),
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
            crate::ui::theme::Theme::set(crate::ui::theme::ThemeMode::Dark, cx);
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
                v.folder = vec![a.clone(), b.clone()];
                v.current = 0;
                v.open_folder_index(1, cx);
            })
        });
        let sb = wait_ready(&viewer, cx).await;
        let final_s = cx.update(|app| {
            let v = viewer.read(app);
            (v.frame.is_some(), v.intrinsic, v.current)
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
            v.folder = vec![fixture(), fixture()];
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
                v.current = 0;
                v.folder = vec![fixture().with_file_name("f0.mp4"), fixture().with_file_name("f1.mp4")];
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
                v.current = cur;
                v.folder = vec![fixture().with_file_name("f0.mp4"), fixture().with_file_name("f1.mp4")];
                v
            }));
            assert_eq!(read_target(cx, &e), Some(want), "循环 current={cur} 应切到 {want}");
        }
        // 单文件循环 / 随机：回自身（走 seek 0 重播）
        for mode in [PlaybackMode::Loop, PlaybackMode::Shuffle] {
            let e = cx.update(|app| app.new(|cx| {
                let mut v = blank(fixture(), cx);
                v.playback_mode = mode;
                v.current = 0;
                v.folder = vec![fixture()];
                v
            }));
            assert_eq!(read_target(cx, &e), Some(0), "{mode:?} 单文件重播自身");
        }
        // 随机多文件：结果 ≠ current 且在清单内
        let e = cx.update(|app| app.new(|cx| {
            let mut v = blank(fixture(), cx);
            v.playback_mode = PlaybackMode::Shuffle;
            v.current = 0;
            v.folder = (0..3).map(|i| fixture().with_file_name(format!("s{i}.mp4"))).collect();
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
            v.current = 0;
            v.folder = (0..5).map(|i| fixture().with_file_name(format!("d{i}.mp4"))).collect();
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
        let a = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/no-a-mode.mp4");
        let b = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/no-b-mode.mp4");
        let ent = cx.update(|app| app.new(|cx| {
            let mut v = blank(a.clone(), cx);
            v.playback_mode = PlaybackMode::Loop;
            v.folder = vec![a, b];
            v.current = 0;
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
            (v.current, v.playing, v.path.file_name().map(|n| n.to_string_lossy().into_owned()))
        });
        assert_eq!(s.0, 1, "列表循环撞结尾应切下一支");
        assert!(s.1, "循环模式切换后继续播放");
        assert_eq!(s.2.as_deref(), Some("no-b-mode.mp4"), "路径应更新");
        cx.run_until_parked(); // 让后台 start 任务收尾（不存在路径落错误占位，无碍）
        // 顺序：撞结尾 → 停住
        let ent = cx.update(|app| app.new(|cx| {
            let mut v = blank(fixture(), cx);
            v.playback_mode = PlaybackMode::Sequential;
            v.folder = vec![fixture(), fixture()];
            v.current = 0;
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
            (v.playing, v.current, v.playhead)
        });
        assert!(!s.0, "顺序播放撞结尾应停下");
        assert_eq!(s.1, 0, "顺序播放不切文件");
        assert_eq!(s.2, 2.0, "playhead 停在结尾");
        cx.run_until_parked();
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
}
