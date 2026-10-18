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
//! 对齐 [`::image_viewer::ImageViewer`] 的「自包含阅读器」姿势；上屏字节序 BGRA
//! 同 PDF 模块 [`crate::pdf::render_image_from_bgra`]。

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, sync_channel};
use std::time::{Duration, Instant};

use gpui::{
    AnimationExt, App, Bounds, Context, CursorStyle, DefiniteLength, Hsla, InteractiveElement,
    IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    ObjectFit, ParentElement, Pixels, Point, Render, RenderImage, ScrollHandle, ScrollWheelEvent,
    StatefulInteractiveElement, Styled, StyledImage, Window, canvas, div, img, px,
    prelude::FluentBuilder as _,
};
use image::{Frame, RgbaImage};

use ui::button::Button;
use ui::button::ButtonVariants as _;
use ui::icon::{Icon, IconName};
use ui::motion::hover_spring;

use super::audio::AudioSink;
use super::decode::{AudioReader, VideoReader};
use super::subtitle::{SubtitleEntry, SubtitleStyle, find_active_subtitle, parse_ass_text};

use ::viewer::auto_hide;
use ::viewer::folder::FolderNav;
use ::viewer::pan_zoom;
use config::video::{ZOOM_MAX, ZOOM_MIN, ZOOM_STEP};

/// 解码通道槽位数（帧数）：回压 + 内存有界；播放/seek 时缓冲少量前跳。
const DECODE_AHEAD_FRAMES: usize = 2;
/// ←/→ seek 步进（秒）。
const SEEK_STEP_SEC: f64 = 5.0;
/// 播放推进周期（≈60Hz，对齐 PDF 瓦片泵）。
const PUMP_MS: u64 = 16;
/// seek 首帧落地等待期的泵短拍（ms）：`pending_seek` 活跃时不再等帧率节拍（16-41ms），
/// 用 ~2ms 轮询让 worker 首个 ≥ 落点帧「入队即 drain+上屏」——frame2screen 从 25-45ms
/// 收敛到 ~2ms（消灭泵一拍白等）。短拍只在 seek 落点待定时生效，不改变播放节拍。
const SEEK_WAKE_POLL_MS: u64 = 2;
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
/// 玻璃拟态浮层透明度（控制条 / toast / 占位卡共用；基于 theme `surface` token 派生
/// alpha，亮/暗各自一档——半透明才能透出视频画面，同时保留文字对比）。
const GLASS_PANEL_ALPHA_LIGHT: f32 = 0.80;
const GLASS_PANEL_ALPHA_DARK: f32 = 0.72;
/// 右上角瞬时状态浮层（截图结果提示等）的显示时长（ms），到期自动清除。
const STATUS_MS: u64 = 2500;
/// 拖拽（scrub）期间视频重定位的最小间隔（ms）：拖拽天然高频，若每次 mouse_move 都
/// 换代重开解码（`start_decoder_at`），中间命令的 generation 在入队时就已被后续命令
/// 超过 → worker 直接跳过、只跑最后一条 → 拖拽全程无中间帧上屏（画面冻结）。节流到
/// 该间隔让 worker 有完整拍把最新落点的帧解出来；worker 的预滚保底帧只在 EOF/解码
/// 错误时送到（换代时旧接收端已被换走，送不进也非必需——画面留最近上屏帧），
/// 预览跟手、松手精确落点完全由本节流保证。
const SCRUB_RETARGET_MS: u64 = 60;
// --- M0 hover 预解码 ---
/// hover 缓存命中容差（秒）：预解码帧 pts 与点击落点距离 ≤ 此值（且 pts ≥ 落点）视为命中，
/// 直接装屏不重解；更远判 stale（防止把「很久前悬停/过冲」的帧误当点击落点）。
const HOVER_HIT_WINDOW_S: f64 = 0.35;

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

/// seek 全链路性能探针开关（`GPUI_EDITOR_VIDEO_SEEK_DIAG=1`）：用户开 env 后自点
/// 进度条，stderr 每个 seek 打一行 `[SEEK]` 聚合（total/reuse/regress/skipped/
/// seek2first/frame2screen/target/firstpts）。默认关闭：`OnceLock` 缓存 env 判定，
/// 关闭时每条 seek 命令只多一次 `result_tx.is_none()` 判断与一次 u64 槽位写，
/// 不在逐帧热路径加分支。
fn seek_diag_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("GPUI_EDITOR_VIDEO_SEEK_DIAG").is_some())
}

/// 探针「采集」是否开启：运行态只在 [`seek_diag_enabled`]（env）为真时采集，关态零热路径
///（每条 seek 只多一次 bool 判断）；**test 构建无条件采集每次 seek 快照**——供
/// `bench_click_seek_land_frame_wall` 与 GUI [SEEK] 同格式对表，打印仍只按 [`seek_diag_enabled`]。
/// `cfg!(test)` 为编译期常量（test→true、非 test→false），非测试构建等价回 [`seek_diag_enabled`]，
/// 非测试行为逐字节不变。
fn seek_diag_collect() -> bool {
    if cfg!(test) {
        true
    } else {
        seek_diag_enabled()
    }
}

/// 一次 seek 命令的解码/上屏阶段数据（worker ↔ tick 经全局槽汇总，首帧上屏打印一次）。
#[derive(Default)]
struct SeekDiag {
    session: u64,
    /// 目标落点（秒）。
    target: f64,
    /// seek 命令提交时刻（`start_decoder_at`）。
    trigger: Option<Instant>,
    /// worker 内 `reader.seek` 完成、开始解码时刻。
    worker_start: Option<Instant>,
    /// 首个 ≥ target 帧入队时刻。
    t_sent: Option<Instant>,
    /// reader 复用（true）或重开文件（false）。
    reuse: bool,
    /// 第一个被解码帧 pts − target（负 = 回退越早、预滚解码越重）。
    regress: f64,
    /// 预滚丢弃帧数（pts < target）。
    skipped: u32,
    /// 首个 ≥ target 帧 pts。
    firstpts: f64,
}

static SEEK_DIAG_SLOT: std::sync::OnceLock<std::sync::Mutex<Option<SeekDiag>>> =
    std::sync::OnceLock::new();
fn seek_diag_slot() -> &'static std::sync::Mutex<Option<SeekDiag>> {
    SEEK_DIAG_SLOT.get_or_init(|| std::sync::Mutex::new(None))
}

/// seek 诊断会话号（每次 seek 命令递增，worker/tick 凭它对齐清理）。
static SEEK_DIAG_SESSION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 测试探针：worker 最近一次打开读取器时硬解是否真正接管（无关 GUI/生产；bench 据此
/// 区分「硬解命中」与「硬解打开失败回退软解」——后者打印 hw=0 便于对表）。
#[cfg(test)]
static HW_ENGAGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
pub(crate) fn hw_engaged() -> bool {
    HW_ENGAGED.load(Ordering::Relaxed)
}

#[cfg(test)]
pub(crate) fn hw_engaged_reset() {
    HW_ENGAGED.store(false, Ordering::Relaxed);
}

/// 测试基准取数：tick 在「seek 首帧上屏」那刻把 8 字段快照（字段名/顺序/单位与 GUI [SEEK]
/// 一行完全一致）沉淀到本测试线程槽，`bench_click_seek_land_frame_wall` 每次落点后取走即清。
/// thread_local 避免全局槽跨测试线程竞争（每个 gpui::test 独立线程）；仅 cfg(test) 存在，
/// 运行态零字段零开销。
#[cfg(test)]
struct SeekDiagTestRow {
    total: f64,
    reuse: u8,
    regress: f64,
    skipped: u32,
    seek2first: f64,
    frame2screen: f64,
    target: f64,
    firstpts: f64,
}

#[cfg(test)]
thread_local! {
    static SEEK_DIAG_TEST_ROW: std::cell::RefCell<Option<SeekDiagTestRow>> =
        const { std::cell::RefCell::new(None) };
}

/// 取走本测试线程「最近一次 seek 首帧上屏」的 8 字段快照（顺序与 GUI [SEEK] 对齐）：
/// `(total_ms, reuse, regress_s, skipped, seek2first_ms, frame2screen_ms, target_s, firstpts_s)`。
#[cfg(test)]
pub(crate) fn take_seek_diag_test() -> Option<(f64, u8, f64, u32, f64, f64, f64, f64)> {
    SEEK_DIAG_TEST_ROW.with(|s| s.borrow_mut().take()).map(|r| {
        (
            r.total,
            r.reuse,
            r.regress,
            r.skipped,
            r.seek2first,
            r.frame2screen,
            r.target,
            r.firstpts,
        )
    })
}

/// 解码线程经通道传递的一帧：恒为软 BGRA（硬解 reader 内核自行回读+sws，输出与软解逐帧同构；
/// 旧 M2e「GPU 目标纹理直通」已废除——渲染像素只信 BGRA/RenderImage）。
enum FrameItem {
    /// 解码 BGRA 帧（sws/回读转好、主线程包 `RenderImage` 上屏）。
    Soft(SoftFrame),
}

/// 解码一帧。
struct SoftFrame {
    pts: f64,
    w: u32,
    h: u32,
    bgra: Vec<u8>,
}

impl FrameItem {
    fn pts(&self) -> f64 {
        match self {
            FrameItem::Soft(f) => f.pts,
        }
    }
}

/// 常驻视频 worker 当前 reader 的打开模式（决定 open 路径与 seek 预滚路径；复用判据同款）。
#[derive(Clone, Copy, PartialEq, Debug)]
enum ReaderMode {
    /// 硬件优先（`open_scaled_hw`：FFmpeg 自有设备，不支持/失败内核自动回软解）。
    Auto(u32),
    /// 强制软解（`open_scaled`，cap 封顶）。
    Soft(u32),
}

/// hover 预解码产出的单帧缓存（解码 worker 线程产出、主线程装屏后即弃）。
struct HoverFrame {
    /// 帧 pts（秒，恒 ≥ 落点——与正常 seek 同一「不展示早于落点帧」规则）。
    pts: f64,
    w: u32,
    h: u32,
    /// BGRA 像素（worker 线程 `to_vec` 拷贝；点击命中时整帧装屏，无需再解码）。
    bgra: Vec<u8>,
}

/// 投递给常驻 hover 预解码 worker 的命令（独立线程 + 独立 `VideoReader`）：单帧解码，
/// 结果经单次通道回传主线程；`generation` 供 worker 比对共享代序提前放弃被取代的旧命令。
struct HoverCmd {
    /// 本命令对应的 hover 代序号（主线程每新落点/换文件/真实 seek 递增）。
    generation: u64,
    path: PathBuf,
    target: f64,
    cap: u32,
    tx: std::sync::mpsc::Sender<Result<HoverFrame, String>>,
}

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
    /// 本命令对应的 seek 诊断会话号（`GPUI_EDITOR_VIDEO_SEEK_DIAG=1` 时 worker
    /// 据此把解码阶段数据写进共享槽，tick 首帧落地时聚合打印；普通运行只传递值）。
    seek_session: u64,
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
    /// 测试探针：最近一次 `drop_image` 释放掉的旧帧 `ImageId.0`（仅 test 编译，供
    /// 回归断言「替换只 drop 旧帧、当前帧留存」；运行态不占字段）。
    #[cfg(test)]
    dropped_frame: Option<usize>,
    /// 测试探针：最近一次上屏帧的 pts（秒）——seek 回归断言「seek 后首帧上屏落在
    /// 落点附近、不把落点前整段预滚当迟到帧快进」用（仅 test 编译，运行态不占字段）。
    #[cfg(test)]
    last_frame_pts: Option<f64>,
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
    /// 同文件夹兄弟文件导航（清单文件名升序、恒含当前视频；循环翻页语义收敛在
    /// [`FolderNav`]）。
    nav: FolderNav,
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
    /// 进度条拖拽（scrub）进行中：拖拽期音频静音、视频按节流重定位、`playhead`
    /// 固定在手指落点不随时钟走；松手走一次完整提交 seek（换代 + 重开音频）恢复播放。
    /// 前提是「每次 mouse_move 都换代重开」会饿死中间命令（generation 入队即推进，
    /// worker 只跑最后一条）→ 拖拽中画面冻结；且每次 move 重开音频 → 声音越位/持续
    /// 出声、HE-AAC(SBR) seek 落点的坏首帧被解码刷 `env_facs_q ... is invalid`。
    scrubbing: bool,
    /// 拖拽当前预览落点（秒）；`scrubbing` 期间 `advance_playhead` 直接短路，
    /// `playhead` 恒等于它（不随音频/墙钟钟走）。
    scrub_target: f64,
    /// 拖拽视频重定位节流：距上次真实重定位不足 [`SCRUB_RETARGET_MS`] 只记落点，
    /// 不换代（让 worker 有空把最新落点帧解出来上屏）。
    last_scrub_retarget: Option<Instant>,
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
    /// 最近一次 seek 命令的诊断会话号（`GPUI_EDITOR_VIDEO_SEEK_DIAG=1` 时 tick 据此
    /// 匹配共享槽并聚合打印一条 `[SEEK]`；普通运行恒为递增 u64，不读不写槽）。
    diag_session: u64,
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
    /// 进入全屏前的锁定态快照：双击进全屏时记录、双击退全屏时恢复原模式（进入前若已锁定
    /// 则保持锁定；全屏期间手动切锁定会造成退出时按快照恢复的微偏——可接受）。
    pre_fullscreen_locked: bool,
    /// 底部控制条浮层时序：可见位 + 自隐代序号 + 悬停态 + 延时（收敛于
    /// [`::viewer::auto_hide`]；进锁定默认可见、切视频/悬停浮现、离开 1s 自隐、
    /// hover 抑制）。
    controls_auto: auto_hide::AutoHide,
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
    // --- M0 hover 预解码 ---
    /// 进度条悬停位置（秒，钳位到 [0, duration]；未悬停 None，仅未拖拽的 `on_seek_move` 记录）。
    hovered_target: Option<f64>,
    /// hover 预解码 worker 命令通道（独立线程 + 独立 `VideoReader`，与播放 worker 彻底隔离，
    /// 专做「悬停落点」单帧后台解码，永不阻塞/饿死播放 worker）。
    hover_cmd: Option<std::sync::mpsc::Sender<HoverCmd>>,
    /// hover 命令代序号：新落点 / 换文件 / cap 变化 / 任一真实 seek 时 +1；worker 回填结果时
    /// 按它丢弃过期（不覆盖更新的落点缓存）。
    hover_generation: Arc<std::sync::atomic::AtomicU64>,
    /// hover 预解码完成、待点击消费的单帧缓存（命中即「消费一次」取走）。
    hover_cache: Option<HoverFrame>,
    /// `begin_scrub` 已用 hover 缓存帧直接上屏的落点（秒；未命中 None）：`end_scrub_to` 凭它
    /// 判定「松手仍在同一落点」→ 走音频-only 收尾、不再对同落点重新 seek；用户拖走后不匹配，
    /// 照常落回正常 seek（防 60ms 内拖离仍端 Served 旧帧）。
    served_hover_target: Option<f64>,
    // --- M6 播放模式 / 截图 ---
    /// 播放模式三态（播完去向；控制条按钮循环切换）。
    playback_mode: PlaybackMode,
    /// 随机播放的 PRNG 状态（xorshift64*；`pick_shuffle_index` 自推进，测试可注入定值）。
    rng_state: u64,
    /// 瞬时状态浮层（右上角）：「文本, 出现时刻」；[`STATUS_MS`] 后自动清除。
    status: Option<(String, Instant)>,
    /// 状态浮层清空任务的代序号（防旧定时器误清新浮层，同 [`::viewer::auto_hide`] 姿势）。
    status_seq: u32,
    // --- 字幕 ---
    /// 字幕清单（打开时一次性加载）。
    subtitles: Vec<SubtitleEntry>,
    /// 字幕是否可见（`v` 键切换）。
    subtitle_visible: bool,
    /// 字幕样式。
    subtitle_style: SubtitleStyle,
}

impl VideoViewer {
    /// 创建查看器并异步打开 `path`（参数解析在后台；解码线程随后启动即播）。
    pub fn new(path: PathBuf, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            path: path.clone(),
            focus_handle: cx.focus_handle(),
            frame: None,
            #[cfg(test)]
            dropped_frame: None,
            #[cfg(test)]
            last_frame_pts: None,
            intrinsic: None,
            loading: true,
            load_error: None,
            bg: None,
            playing: true,
            playhead: 0.0,
            duration: 1.0,
            fps: None,
            decode_cap: DECODE_MAX_DIM,
            hovered_target: None,
            hover_cmd: None,
            hover_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            hover_cache: None,
            served_hover_target: None,
            last_tick: None,
            nav: FolderNav::new(vec![path.clone()], 0),
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
            rng_state: seed_rng(),
            status: None,
            status_seq: 0,
            subtitles: Vec::new(),
            subtitle_visible: true,
            subtitle_style: SubtitleStyle::default(),
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
                                    .map(|r| r.subtitles().to_vec())
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
        // M0 失效源（全部经此入口）：任一真实 seek（点按/拖拽/键盘/切文件/cap 重开）都让悬停
        // 缓存与在途预解码作废——缓存只服务「静止悬停」，换代即不再可信；`hover_generation` 递增
        // 使 worker 回填的过期结果被丢弃。缓存命中路径（begin_scrub 的 take_hover_cache_if_hit）
        // 不调用本方法，故不会把它刚装屏的帧误清。
        self.hover_cache = None;
        self.served_hover_target = None;
        self.hover_generation.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = sync_channel::<FrameItem>(DECODE_AHEAD_FRAMES);
        // 换掉旧接收端 → worker 若正阻塞在 send 会收到失联错误、本命令内层循环退出；
        // 旧越线帧（软 BGRA）随接收端 drop 释放，无跨线程资源要回收
        self.clear_overflow();
        if let Some(old) = self.rx.replace(rx) {
            drop(old);
        }
        self.pending_seek = Some(target);
        // seek 全链路探针：会话号递增；仅「seek 型」命令（result_tx 空）开槽记触发时刻，
        // 换媒体（挂握手通道）不算 seek、不打印。普通运行只多一次 u64 写。
        let seek_session = SEEK_DIAG_SESSION.fetch_add(1, Ordering::Relaxed) + 1;
        self.diag_session = seek_session;
        if seek_diag_collect() && result_tx.is_none() {
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

    /// 惰性启动常驻 hover 预解码 worker（一生一个；`hover_cmd` Sender 随 viewer Drop 弃、
    /// worker `recv()` 返回 Err 即退；与 [`Self::video_worker`] 完全独立——独立线程 +
    /// 独立 `VideoReader`，不会被播放/seek 换代影响）。
    fn ensure_hover_worker(&mut self) {
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
    /// auto/hw → `open_scaled_hw`（FFmpeg 自有 D3D11VA/DXVA2 设备，worker 线程独享、无调用方
    /// 注入、失败内核自动回软解）；soft → `open_scaled`（纯软解）。不再读
    /// `gpui_platform::d3d11_device_context()`——旧 M2e 把渲染线程 immediate context 注入
    /// 解码 worker 共享导致真机崩溃（exit 2173），注入路径已废除。
    fn video_worker(
        rx: std::sync::mpsc::Receiver<DecodeCmd>,
        gen_arc: Arc<AtomicU64>,
    ) {
        // 常驻 reader：同路径 + 同模式（auto 或 soft cap）的命令直接复用——seek 只做容器内
        // 定位（av_seek_frame + 解码器 flush），不重开文件；重开（avformat open + 解码器/sws
        // 初始化）在一次 seek 命令里可能几十 ms，是「点击进度条后画面要等片刻」的主因
        //（探针实测 open_scaled≈57ms、复用 seek≈0ms）。换文件 / cap 变化（窗口缩放）/
        // 模式切换才重开。同一线程内串行使用，无并发；reader 非 Send，仅在本 worker
        // 线程持有。
        let mut active: Option<(std::path::PathBuf, ReaderMode, VideoReader)> = None;
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
            let same = matches!(&active, Some((p, m, _)) if *p == cmd.path && *m == mode);
            if !same {
                // 打开（按模式；两者都失败 → 报错跳过本命令）
                match Self::open_reader_for_mode(&cmd.path, &mode) {
                    Ok((opened_mode, reader)) => {
                        if let Some(out) = &cmd.result_tx {
                            let _ = out.send(Ok(reader.params()));
                        }
                        active = Some((cmd.path.clone(), opened_mode, reader));
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
            let (_, _, reader) = active.as_mut().expect("复用分支已确保有 reader");
            // 硬解 reader（FFmpeg 自有设备）：seek 预滚走 `skip_to_bgra_into`（一次调用完成
            // seek + 跳帧免转换 + 首帧 BGRA，seek 墙钟主要加速）；软解保持旧逐帧口径字节不动。
            let hw_reader = reader.is_hw();
            if !hw_reader {
                let _ = reader.seek(cmd.target);
            }
            // seek 全链路探针（env 开或 test 构建采集，打印仍只按 env；本命仅 seek 命令开）：
            // 收集「回退量 / 预滚数 / 首帧≥target 入队时刻」，首帧入队瞬间写槽（此时
            // 解码量已定——预滚帧全在它之前），tick 首帧上屏时聚合打印一次。
            let seek_diag_on = seek_diag_collect() && cmd.result_tx.is_none();
            let seek_worker_start = if seek_diag_on { Some(Instant::now()) } else { None };
            let mut seek_first_decoded = true;
            let mut seek_regress: f64 = 0.0;
            let mut seek_skipped: u32 = 0;
            let mut seek_first_sent: Option<Instant> = None;
            // 硬解首帧是否已由 skip_to_bgra 产出（软解恒 false → 走旧逐帧预滚过滤）
            let mut hw_primed = false;
            // seek 预滚过滤：ffmpeg seek 回拉到目标前关键帧（video-ffmpeg decode.rs 的
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
            while gen_arc.load(Ordering::SeqCst) == cmd.generation {
                let t0 = Instant::now();
                let item = if hw_reader && !hw_primed {
                    hw_primed = true;
                    // ---- 硬解 seek 预滚快路径：一次 `skip_to_bgra_into` 完成 seek + 跳帧
                    //（免转换/免回读）+ 首帧 BGRA —— seek 墙钟主要加速 ----
                    if buf.len() < reader.source_cap() {
                        buf.resize(reader.source_cap(), 0);
                    }
                    match reader.skip_to_bgra_into(cmd.target, &mut buf[..]) {
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
                    // 扩容目标：始终按源顶格（source_cap）兜底——任何解码输出 ≤ 源尺寸，
                    // 缓冲永不小于未知的当前帧 need，`_into` 的 slice 上界恒安全。零初始化
                    // 只在扩容瞬间发生一次，逐帧不重置。
                    if buf.len() < reader.source_cap() {
                        buf.resize(reader.source_cap(), 0);
                    }
                    // 复用持久缓冲 `_into`（零初始化只在扩容时发生一次）；帧产物
                    // `buf[..need].to_vec()`（一次 memcpy、无逐帧 memset）计入 decode 窗——
                    // 与旧版 `next_frame_bgra` 的「内部 vec![0;N]+sws 写入」口径对齐，VPROF
                    // 才可比。
                    match reader.next_frame_bgra_into(&mut buf[..]) {
                        Ok(Some((pts, w, h))) => {
                            let need = (w as usize) * (h as usize) * 4;
                            // 不做 `truncate`：它只缩短 len、不释放容量（顶格内存照样驻留），
                            // 反而把下次 `_into` 的上界从恒安全的 source_cap 缩成上一帧 need——
                            // 若解码输出尺寸变化（未来/其它源）会拿到过小的 slice（cdylib 报错
                            // 断代）。保持 len=source_cap，need 只用于本帧 to_vec 切片。
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
                let keep = match item {
                    Ok(Some(frame)) if frame.pts() >= cmd.target => {
                        // 落点前一帧保底（排空先有帧可上屏）；软帧发送失败直接丢弃（无池资源）
                        if let Some(p) = pre_roll.take() {
                            let _ = cmd.tx.send(p);
                        }
                        // seek 探针写槽（每个 seek 一次）：此刻预滚数/回退量已定，
                        // 把解码阶段数据落进共享槽，供 tick 首帧落地时聚合打印。
                        if seek_diag_on && seek_first_sent.is_none() {
                            seek_first_sent = Some(Instant::now()); // 首帧 ≥ target 入队
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
                                    worker_start,
                                    t_sent: seek_first_sent,
                                    reuse: same,
                                    regress: seek_regress,
                                    skipped: seek_skipped,
                                    firstpts: frame.pts(),
                                });
                            }
                        }
                        // 发送成功（主线程接收）或换代（接收端已换走 = 帧丢弃）；软帧无池资源要回
                        match cmd.tx.send(frame) {
                            Ok(()) => true,
                            Err(_) => false,
                        }
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
                            let _ = cmd.tx.send(p);
                        }
                        false
                    }
                    Err(_) => false, // 解码错误：本代结束
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
                    // 本命以 EOF/解码错误结束（换代时不走此分支进入——换代已把旧接收端
                    // 换走，`try_send` 连不上、帧被丢弃且非必需；画面保留最近上屏帧，跟手
                    // 由 `SCRUB_RETARGET_MS` 节流保证，不依赖这里的保底）：把缓存的「落点
                    // 前一帧」尽力送出，让 pump 在 EOF/出错时至少有一帧上屏。`try_send`：
                    // 接收端已换走/通道满都立刻失败（=丢弃），不阻塞 worker。
                    if let Some(p) = pre_roll.take() {
                        let _ = cmd.tx.try_send(p);
                    }
                    break; // 接收端已被换代/弃用
                }
            }
        }
    }

    /// 按模式打开 reader：返回 `(实际模式, reader)`。auto → `open_scaled_hw`（FFmpeg 自有
    /// 设备，硬解优先、失败内核自动回软解，行为不降级）；soft → `open_scaled`（纯软解）。
    /// 两者都失败才 `Err`。
    fn open_reader_for_mode(
        path: &std::path::Path,
        mode: &ReaderMode,
    ) -> anyhow::Result<(ReaderMode, VideoReader)> {
        match mode {
            ReaderMode::Auto(cap) => {
                let r = VideoReader::open_scaled_hw(path, *cap)?;
                #[cfg(test)]
                HW_ENGAGED.store(r.is_hw(), Ordering::Relaxed);
                Ok((ReaderMode::Auto(*cap), r))
            }
            ReaderMode::Soft(cap) => {
                #[cfg(test)]
                HW_ENGAGED.store(false, Ordering::Relaxed);
                VideoReader::open_scaled(path, *cap).map(|r| (ReaderMode::Soft(*cap), r))
            }
        }
    }

    /// 常驻 hover 预解码 worker 主循环（独立线程 + 独立 `VideoReader`，与播放 worker 彻底
    /// 隔离）：复用同 path+cap 的 reader，`seek(target)` 后跳过全部 `pts<target` 的预滚帧、
    /// 取第一帧 `≥target` 经单次通道回传主线程（与播放 worker 同一「不展示早于落点帧」规则）。
    /// 每拍解码前比对共享 [`Self::hover_generation`]：已被更新的悬停/换文件/真实 seek 取代
    /// 的命令立即放弃（帧间检查，省被跳过命令的剩余解码）；与播放 worker 只在 cdylib 内
    /// FFMPEG_LOCK 上全局串行（FFmpeg 内核闸，属预期内竞争，不做额外锁）。
    fn hover_worker(
        rx: std::sync::mpsc::Receiver<HoverCmd>,
        hgen: Arc<std::sync::atomic::AtomicU64>,
    ) {
        let mut active: Option<(std::path::PathBuf, u32, VideoReader)> = None;
        let mut buf: Vec<u8> = Vec::new();
        while let Ok(cmd) = rx.recv() {
            // 已在队列中被更新的 hover 命令取代（代序号已推进）→ 直接放弃：连 avformat 重开
            // 都省掉；`cmd` drop 使结果通道断连，主线程按断连丢弃（不误覆盖新落点缓存）。
            if hgen.load(Ordering::SeqCst) != cmd.generation {
                continue;
            }
            let same = matches!(&active, Some((p, c, _)) if *p == cmd.path && *c == cmd.cap);
            if !same {
                match VideoReader::open_scaled(&cmd.path, cmd.cap) {
                    Ok(r) => active = Some((cmd.path.clone(), cmd.cap, r)),
                    Err(e) => {
                        let _ = cmd.tx.send(Err(e.to_string()));
                        continue;
                    }
                }
            }
            let reader = &mut active.as_mut().expect("打开成功必有 reader").2;
            let _ = reader.seek(cmd.target);
            // 解码缓冲预分配：按源顶格扩容一次（同播放 worker 的持久缓冲姿势）
            if buf.len() < reader.source_cap() {
                buf.resize(reader.source_cap(), 0);
            }
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
        // 常驻音频 reader：同文件 + 同 out_rate（倍速改变 out_rate 才需重开）的命令直接
        // 复用——seek 只做容器定位 + 冲重采样器（dll 内核 `AudioReader::seek`），省每次
        // seek 的全量 avformat 重开（~40-50ms），也免去与视频 worker 的 seek/解码抢
        // FFMPEG_LOCK（重开期间长持锁会拖延视频落点帧 ——「点击进度条后画面要等片刻」
        // 的残余主因）。换文件或倍速（out_rate 变）才重开。
        let mut active: Option<(std::path::PathBuf, u32, AudioReader)> = None;
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

    /// 播放泵是否仍需运行（拖拽中也必须持续——暂停态拖拽时 `playing` 为假、落点帧
    /// 上屏前 `pending_seek` 也可能短暂为空，但预览要跟着手指走）。
    fn pump_wanted(&self) -> bool {
        self.playing || self.pending_seek.is_some() || self.scrubbing
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

    /// 泵等待一拍的最长时长：seek 首帧落地等待期（`pending_seek` 活跃）用短拍——把
    /// frame2screen 从「等一拍」（帧率节拍 16-41ms）收敛到 ~[`SEEK_WAKE_POLL_MS`]ms，
    /// 消灭泵一拍白等；其余按帧率节拍（不改变播放节奏）。
    fn pump_wait_interval(&self) -> Duration {
        if self.pending_seek.is_some() {
            Duration::from_millis(SEEK_WAKE_POLL_MS)
        } else {
            self.pump_interval()
        }
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
                    .update(cx, |v, _| v.pump_wait_interval())
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

        // 排空「pts ≤ playhead」的帧取最新上屏；越过 playhead 的下一帧保留在 overflow。
        // 暂停/拖拽冻结（playhead 停在落点、`advance_playhead` 短路）期间：首个「pts ≥ 落点」
        // 的帧（帧号量化常比落点微超前 ~1 帧）就是落点帧本身，必须收下；否则它会被当
        // 「逾线帧」塞回 overflow、`pending_seek` 永不清 → seek 冻结。软解靠「预滚保底帧
        //（≤ 落点）先行」掩盖此缺口；硬解 skip 只送 ≥ 落点帧，直接暴露（pause→hover→seek
        // 回归实测）。
        let mut land_seek = if self.scrubbing || !self.playing {
            self.pending_seek
        } else {
            None
        };
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
                Some(item) if item.pts() <= self.playhead => {
                    // 先到的帧被更新的帧顶掉：旧软帧直接 drop（无跨线程资源）
                    if let Some(prev) = latest.replace(item) {
                        let _ = prev;
                    }
                }
                Some(item) if land_seek.is_some() && item.pts() >= land_seek.unwrap() => {
                    // 暂停/拖拽落点帧：只认首个「≥ 落点」帧，收下即落地；随后解除本分支，
                    // 后续帧恢复正常 overflow 规则（不整段快进到最新帧）。
                    land_seek = None;
                    if let Some(prev) = latest.replace(item) {
                        let _ = prev;
                    }
                }
                Some(item) => {
                    self.overflow = Some(item);
                    break;
                }
                None => break,
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
            }
            self.pending_seek = None; // 落点已上屏（首个可用帧）
            true
        } else {
            false
        };
        // seek 全链路探针聚合（`GPUI_EDITOR_VIDEO_SEEK_DIAG=1` 打印；test 构建无条件把
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
    fn advance_playhead(&mut self, audio_pos: Option<f64>) -> bool {
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
    fn natural_end_target(&mut self) -> Option<usize> {
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
    fn pick_shuffle_index(&mut self) -> usize {
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
    fn handle_end(&mut self, cx: &mut Context<Self>) {
        match self.natural_end_target() {
            // 顺序播放：停在结尾（既有行为）
            None => {}
            // 单文件循环/随机：seek 0 重播（保留缩放/平移，不重开文件）
            Some(idx) if idx == self.nav.current() => self.seek(-self.duration, cx),
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

    /// 释放被替换下的旧帧图集 tile（只在 `frame` 替换点调用；`None` 直接返回零成本）。
    ///
    /// **必须延迟到本拍更新末尾再 `drop_image`**：gpui `App::drop_image(image, None)` 只遍历
    /// `App.windows` 里**未被正在更新取走**的窗口——窗口自身的 key/button 回调都是在
    /// `update_window_erased`（当前窗口被临时移出 `App.windows`）里跑的，当场 drop 会漏掉
    /// **正在播放的那个窗口**（其 atlas tile 残留，每手动切一次漏一片）。`Effect::Defer`
    /// 在本拍 `finish_update` flush，其时当前窗口已放回 `App.windows` → 所有入口（泵/自动切/
    /// 按键/按钮）都命中断言窗口；帧字节在 Arc 未 drop 前仍在、可重传，不丢渲染
    ///（同 markdown-wysiwyg `pending_math_drop` 的「延迟一帧释放」姿势）。
    fn drop_old_frame(&mut self, old: Arc<RenderImage>, cx: &mut Context<Self>) {
        #[cfg(test)]
        {
            self.dropped_frame = Some(old.id.0);
        }
        cx.defer(move |app| app.drop_image(old, None));
    }

    /// 把 BGRA 包成 `RenderImage` 上屏。`intrinsic` 优先保留打开期从 `params` 读到的
    /// **源尺寸**（decode-to-display 下采样后逐帧 w/h 是输出尺寸，宽高比一致即可）。
    fn install_bgra(&mut self, w: u32, h: u32, bgra: Vec<u8>, cx: &mut Context<Self>) {
        let t0 = Instant::now();
        // 帧字节移入 RenderImage（零拷贝）即持有时；截图在点击时经 `frame.as_bytes` 取回
        let buf = RgbaImage::from_raw(w, h, bgra).expect("解码帧长度恒为 w*h*4");
        let new = Arc::new(RenderImage::new(vec![Frame::new(buf)]));
        // 旧帧被新帧替换的唯一点：显式 `drop_image` 释放被换下的旧图集 tile（P0 资源
        // 泄漏——视频每帧新建 `RenderImage` → 全局自增 `ImageId`，图集 key 必 miss →
        // 新 tile 只进不出，长会话 VRAM/图集持续上涨；`ImageSource::Render` 源图只经
        // `drop_image` 下线，同 pdf/markdown-wysiwyg 姿势。gpui 语义：删 sprite_atlas
        // 键并卸载对应 backend tile；已在先前帧 present 的旧场景不含此键，安全。
        // 释放经 `drop_old_frame` 延迟到本拍更新末尾（见其注释：窗口上下文下当前窗口
        // 不在 `App.windows`，当场 drop 会漏掉正在播放的窗口）。
        if let Some(old) = self.frame.replace(new) {
            self.drop_old_frame(old, cx);
        }
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

    /// 清空 `overflow`（换代 / 切文件共用）：软越线帧随 drop 释放，无跨线程资源要收。
    fn clear_overflow(&mut self) {
        self.overflow.take();
    }

    /// 关闭当前媒体、打开 `nav[current]`（同文件夹切换共用）。
    fn open_folder_index(&mut self, idx: usize, cx: &mut Context<Self>) {
        if self.nav.set_current(idx).is_none() {
            return;
        }
        self.path = self.nav.item(idx).expect("set_current 已校验越界");
        // 切文件：旧媒体最后一帧的图集 tile 一并显式释放（与 install_bgra 同款 P0 姿势，
        // 否则切换 Playlist 多支时每切一次漏一片 tile）。
        if let Some(old) = self.frame.take() {
            self.drop_old_frame(old, cx);
        }
        self.intrinsic = None;
        self.loading = true;
        self.load_error = None;
        self.playing = true;
        self.playhead = 0.0;
        self.duration = 1.0;
        self.fps = None;
        self.last_tick = None;
        self.clear_overflow();
        self.pending_seek = None;
        self.zoom = 1.0; // 切文件：缩放复位为适应视口
        self.pan = (0.0, 0.0); // 平移同步归零（拖拽中的惯性也清掉）
        self.drag = None;
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
        // `hover_generation` 同步递增，旧文件 worker 回填结果一律丢弃。
        self.hovered_target = None;
        self.hover_cache = None;
        self.served_hover_target = None;
        self.hover_generation.fetch_add(1, Ordering::SeqCst);
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

    fn toggle_play(&mut self, cx: &mut Context<Self>) {
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

    fn seek(&mut self, delta: f64, cx: &mut Context<Self>) {
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
    fn seek_to_fraction(&mut self, fraction: f32, cx: &mut Context<Self>) {
        let target = f64::from(fraction.clamp(0.0, 1.0)) * self.duration.max(0.0);
        self.seek(target - self.playhead, cx);
    }

    /// 分数 → 拖拽落点秒数（钳位到 [0, duration]；与 [`Self::seek_to_fraction`] 同一换算）。
    fn clamped_scrub_target(&self, fraction: f32) -> f64 {
        f64::from(fraction.clamp(0.0, 1.0)) * self.duration.max(0.0)
    }

    /// M0：记录进度条悬停位置；空闲（暂停且不在拖拽 / 不等待 seek 落点）时向独立后台 worker
    /// 投一条「该落点单帧预解码」。机会式：拖拽/seek 期主 worker 正忙、落点随时变，播放期
    /// hover 解码在 FFMPEG_LOCK 上会跟 playback worker 抢带宽 → stutter，均跳过并保留上一份
    /// 缓存（「不能无干扰地跑就跳过」）；悬停位置仍记录，点击照常可走正常 seek 兜底。
    fn update_hover_target(&mut self, target: f64, cx: &mut Context<Self>) {
        self.hovered_target = Some(target);
        if self.scrubbing || self.pending_seek.is_some() || self.playing {
            return;
        }
        // 已缓存「离 target 足够近」的帧（命中窗内点击即可直接上屏）→ 不重复投（省解码带宽）
        if let Some(f) = &self.hover_cache {
            if f.pts >= target && f.pts - target <= HOVER_HIT_WINDOW_S {
                return;
            }
        }
        self.kick_hover_predecode(target, cx);
    }

    /// M0：向常驻 hover 预解码 worker 投一条单帧命令；结果异步回填 [`Self::hover_cache`]。
    /// 换代（新落点 / 换文件 / 真实 seek / cap 变化）由 [`Self::hover_generation`] 把关：
    /// 过期结果不落地。**主线程不碰任何解码 FFI**——只有独立 worker 线程解，UI 零阻塞。
    fn kick_hover_predecode(&mut self, target: f64, cx: &mut Context<Self>) {
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
        cx.spawn(async move |weak, cx| {
            // 后台线程等 worker 单次回传（真解码耗时由 worker 占，不在主线程）
            let outcome = cx
                .background_executor()
                .spawn(async move { frx.recv_timeout(std::time::Duration::from_secs(15)) })
                .await;
            let _ = weak.update(cx, |this, cx| {
                // 只落地「仍是悬停的那个目标、仍在同文件同 cap」的最新代：过期/换文件丢弃
                if this.hover_generation.load(Ordering::SeqCst) != hgen
                    || this.path != path_check
                    || this.decode_cap != cap_check
                {
                    return;
                }
                if let Ok(Ok(frame)) = outcome {
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
    fn take_hover_cache_if_hit(&mut self, target: f64, cx: &mut Context<Self>) -> bool {
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
    fn begin_scrub(&mut self, fraction: f32, cx: &mut Context<Self>) {
        self.scrubbing = true;
        self.scrub_target = self.clamped_scrub_target(fraction);
        self.playhead = self.scrub_target;
        self.last_tick = None;
        if let Some(sink) = &self.audio {
            sink.flush(); // 清掉暂停前已入队的旧段（即便 pause 未即时生效也无声）
            let _ = sink.pause();
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
    fn update_scrub(&mut self, fraction: f32, cx: &mut Context<Self>) {
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
    fn end_scrub(&mut self, fraction: f32, cx: &mut Context<Self>) {
        let target = self.clamped_scrub_target(fraction);
        self.end_scrub_to(target, cx);
    }

    /// 拖拽结束到当前预览落点（鼠标在窗外松开、`fraction_at` 拿不到位置时兜底）。
    fn end_scrub_at_current(&mut self, cx: &mut Context<Self>) {
        self.end_scrub_to(self.scrub_target, cx);
    }

    fn end_scrub_to(&mut self, target: f64, cx: &mut Context<Self>) {
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

    /// 字幕开关切换。
    fn toggle_subtitle(&mut self, cx: &mut Context<Self>) {
        self.subtitle_visible = !self.subtitle_visible;
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
        Some(self.nav.current_path().to_path_buf())
    }

    /// 当前视频在同文件夹清单中的下标（0 基）。
    pub fn current_index(&self) -> usize {
        self.nav.current()
    }

    /// 同文件夹视频总数。
    pub fn folder_len(&self) -> usize {
        self.nav.len()
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
            "v" => self.toggle_subtitle(cx),
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
        let delta_y = pan_zoom::wheel_delta_y(ev);
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
            self.adjust_zoom(pan_zoom::zoom_factor(delta_y, ZOOM_STEP), cx);
        } else if shift {
            // Shift+滚轮：同文件夹切换（向下滚 = 下一支）
            self.switch_file(if delta_y < 0.0 { 1 } else { -1 }, cx);
        }
    }

    /// 应用缩放因子并约束在 [`ZOOM_MIN`..=`ZOOM_MAX`]（钳位/无变化短路由
    /// [`pan_zoom::apply_zoom`]）；变化才 notify 重绘。
    fn adjust_zoom(&mut self, factor: f32, cx: &mut Context<Self>) {
        if !pan_zoom::apply_zoom(&mut self.zoom, factor, ZOOM_MIN, ZOOM_MAX) {
            return;
        }
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
            self.drag = Some(pan_zoom::pan_drag_begin(
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
        let Some(drag) = self.drag else {
            return;
        };
        let (vw, vh) = self.viewport_size(window);
        let area_h = if self.locked {
            vh.max(1.0)
        } else {
            (vh - CONTROL_BAR_H).max(1.0)
        };
        let (dw, dh) = self.display_size(vw, vh);
        let pos = (ev.position.x.as_f32(), ev.position.y.as_f32());
        let (dx, dy) = pan_zoom::pan_drag_delta(&drag, pos);
        self.pan = Self::clamp_pan((drag.1.0 + dx, drag.1.1 + dy), vw, area_h, dw, dh);
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
    /// 逻辑收敛在 [`pan_zoom::clamp_pan`]。
    fn clamp_pan(pan: (f32, f32), vw: f32, vh: f32, dw: f32, dh: f32) -> (f32, f32) {
        pan_zoom::clamp_pan(pan, vw, vh, dw, dh)
    }

    // ---- 纯视频模式（参考纯图片锁定）：隐藏外壳、视频充满窗口、控制条为浮层 ----

    /// 是否「纯视频」干净播放模式（宿主据此隐藏菜单栏/选项卡栏/侧边栏/状态栏，视频充满窗口）。
    pub fn is_locked(&self) -> bool {
        self.locked
    }

    /// 暂停标志是否该显示（右上角玻璃徽标）：**有画面且未在播放**（暂停/自然结尾都算停）；
    /// 加载中（无帧）与打开出错（占位）时不显示。纯函数，测试可注入三态确定性断言。
    fn paused_badge_shown(playing: bool, has_frame: bool, load_error: bool) -> bool {
        !playing && has_frame && !load_error
    }

    /// 切换纯视频模式：锁定 = 隐藏非视频 UI、视频充满窗口（宿主壳体重排后自动缩放填满）、
    /// 控制条浮层按需浮现；解锁回归。切换时平移/拖拽归零。
    pub fn toggle_lock(&mut self, cx: &mut Context<Self>) {
        let locked = !self.locked;
        self.set_locked(locked, cx);
    }

    /// 直接设置纯视频（锁定）模式（`toggle_lock` 翻转与双击全屏共用同一副作用，避免平行
    /// 重写）：锁定 = 隐藏非视频 UI、视频充满窗口；解锁回归。切换时平移/拖拽归零。
    fn set_locked(&mut self, locked: bool, cx: &mut Context<Self>) {
        self.locked = locked;
        self.pan = (0.0, 0.0);
        self.drag = None;
        // 进/出锁定都让控制条浮现一次（锁定态随即 1s 自隐）
        self.show_controls_temporarily(cx);
        cx.notify();
    }

    /// 双击画面区是否应**退出**全屏（纯函数，不读窗口，测试可注入 `is_fs` 确定性断言退出
    /// 路径）：当前已全屏 → 退出；未全屏 → 进入（进入后自动进纯视频锁定，再双击退出并恢复
    /// 进入前的模式）。
    fn should_exit_fullscreen(is_fs: bool) -> bool {
        is_fs
    }

    /// 视频画面区（`#video-main`）双击：进入 OS 窗口全屏 + 纯视频锁定；已全屏时再双击退出
    /// 并恢复进入前的锁定模式（进入前已锁定则保持锁定）。控制条/进度条/状态浮层是画面上层
    /// 覆盖子元素、优先消费其命中的双击，不会到达这里；纯事件驱动，不引入定时器/每帧工作。
    fn on_video_surface_mouse_down(
        &mut self,
        ev: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 双击语义与欢迎页/编辑区同套（click_count >= 2；左键由 on_mouse_down(Left) 保证，
        // 单击 = click_count 1 直接 return，不抢单击/拖拽语义）
        if ev.click_count < 2 {
            return;
        }
        if Self::should_exit_fullscreen(window.is_fullscreen()) {
            // 退出全屏：恢复进入全屏前的模式（全屏期间手动切锁定会按进入前快照恢复——可接受微偏）
            window.toggle_fullscreen();
            let restore = self.pre_fullscreen_locked;
            self.set_locked(restore, cx);
        } else {
            // 进入全屏：先记录进入前锁态，再进全屏并锁定（set_locked 内已归零平移/浮现控制条）
            self.pre_fullscreen_locked = self.locked;
            window.toggle_fullscreen();
            self.set_locked(true, cx);
        }
    }

    // ---- 控制条浮现 / 自隐（锁定模式浮层；普通模式常驻） ----

    /// 控制条浮现并排队「离开 1s 后自隐」（切视频 / hover 离开时调用；悬停中不隐藏）。
    /// 时序逻辑收敛于 [`auto_hide::AutoHide::show_temporarily`]。
    fn show_controls_temporarily(&mut self, cx: &mut Context<Self>) {
        self.controls_auto
            .show_temporarily(|_| true, |v| &mut v.controls_auto, cx);
    }

    /// 查看器悬停态：`#video-viewer` 根 hover。进入 → 浮现且不隐藏；离开 → 1s 后自隐。
    fn on_controls_hover(&mut self, hovered: &bool, _window: &mut Window, cx: &mut Context<Self>) {
        self.set_controls_hover(*hovered, cx);
    }

    /// 悬停态设置（独立于窗口，便于测试）：悬停期间不受自动隐藏影响。
    fn set_controls_hover(&mut self, hovered: bool, cx: &mut Context<Self>) {
        if hovered {
            self.controls_auto.hover_in();
            cx.notify();
        } else {
            self.controls_auto.hover_out();
            self.show_controls_temporarily(cx);
        }
    }

    /// 控制条 / 状态浮层命中**双击** → 终止向 `#video-main` 冒泡（与画面双击全屏互斥）：
    /// seek/音量条等子元素先消费各自的 mousedown，轮到本处理器时只吞掉 `click_count>=2`，
    /// 使双击进度条/按钮/空隙不再误触进入/退出全屏。单击透传不拦截——验收口径「单击/
    /// 拖拽行为全不变」（拖拽起点仍按既有方式落到画面元素）。
    fn on_controls_mouse_down(
        &mut self,
        ev: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if ev.click_count >= 2 {
            cx.stop_propagation();
        }
    }

    /// 居中占位玻璃卡（加载中 / 加载失败共用）：铺满画布再 flex 居中，信息收敛在
    /// 半透明圆角卡内（与 toast / 控制条同一套玻璃样式）。
    fn placeholder(text: &str, color: Hsla, cx: &App) -> gpui::AnyElement {
        let theme = ui::theme::Theme::global(cx);
        let dark = theme.is_dark();
        let surface: Hsla = theme.colors.surface.into();
        let border: Hsla = theme.colors.border.into();
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .px_4()
                    .py_2()
                    .rounded_lg()
                    .bg(surface.opacity(if dark {
                        GLASS_PANEL_ALPHA_DARK
                    } else {
                        GLASS_PANEL_ALPHA_LIGHT
                    }))
                    .border_1()
                    .border_color(border)
                    .shadow(vec![
                        gpui::BoxShadow::new(px(0.), px(4.), gpui::hsla(0., 0., 0., 0.16))
                            .blur_radius(px(12.)),
                        gpui::BoxShadow::new(px(0.), px(1.), gpui::hsla(0., 0., 0., 0.12))
                            .blur_radius(px(3.)),
                    ])
                    .text_size(px(14.))
                    .text_color(color)
                    .child(text.to_string()),
            )
            .into_any_element()
    }

    /// 阅读画布底色（同 ImageViewer：亮 #F5F5F5 / 暗 #202020，中性灰阶、与外壳 `window_bg` 同源）。
    fn default_canvas(cx: &App) -> Hsla {
        let dark = ui::theme::Theme::global(cx).is_dark();
        ui::theme::ChromeColors::for_mode(dark).window_bg.into()
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
    /// 逻辑收敛在 [`pan_zoom::contain_fit`]。
    fn contain_fit(iw: f32, ih: f32, vw: f32, vh: f32) -> (f32, f32) {
        pan_zoom::contain_fit(iw, ih, vw, vh)
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
    /// `controls_auto.visible()` 时渲染整条，隐藏时是透明热区——hover **只挂在这根条带**上（进入即
    /// 浮现且不隐藏，离开 1s 后自隐；鼠标在条带之外的画面区域不触发，锁定的初始自隐不会被
    /// 画面内停留的鼠标打断）。
    fn control_bar(&mut self, window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let strip = div()
            .id("video-controls")
            .absolute()
            .bottom_0()
            .left_0()
            .right_0()
            .h(px(CONTROL_BAR_H))
            .on_hover(cx.listener(Self::on_controls_hover));
        if self.locked && !self.controls_auto.visible() {
            // 纯视频模式隐藏：透明热区（继续占底部、可接收 hover 触发浮现）
            return strip.into_any_element();
        }
        // 可见控制条：双击不让其漏到画面区（与 `#video-main` 双击全屏互斥）；须挂在
        // `if 隐藏早退` 之后——隐藏态透明热区下方就是视频画面，双击仍需命中画面元素。
        let strip = strip.on_mouse_down(MouseButton::Left, cx.listener(Self::on_controls_mouse_down));
        let colors = ui::theme::Theme::global(cx).colors;
        let dark = ui::theme::Theme::global(cx).is_dark();
        let fg: Hsla = colors.fg.into();
        let muted: Hsla = colors.fg_muted.into();
        let border: Hsla = colors.border.into();
        let accent: Hsla = colors.accent.into();
        let entity = cx.entity();

        let seek_strip = Self::strip_element(
            "video-seek",
            self.seek_bounds.clone(),
            self.ratio(),
            muted,
            accent,
            true,
            window,
            cx,
        );
        let vol_strip = Self::strip_element(
            "video-vol",
            self.vol_bounds.clone(),
            self.effective_volume(),
            muted,
            accent,
            false,
            window,
            cx,
        );

        let time = format!(
            "{} / {}",
            Self::fmt_time(self.playhead),
            Self::fmt_time(self.duration)
        );
        let playlist = format!(
            "{}/{} · {:.0}%",
            self.nav.current() + 1,
            self.nav.len(),
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

        // 玻璃拟态：半透明面板（透出视频）+ 顶部直角 + 细边框 +
        // 上抛柔和投影（矩阵浮层风格）。面板底色固定 #191a1b（用户指定，非 theme token），
        // 仍保留半透明 alpha（亮/暗各一档）。纯视觉，不碰 seek/vol 坐标与交互路径。
        let panel = gpui::hsla(210., 0.04, 0.102, 1.0).opacity(if dark {
            GLASS_PANEL_ALPHA_DARK
        } else {
            GLASS_PANEL_ALPHA_LIGHT
        });
        strip
            .bg(panel)
            .border_1()
            .border_color(border)
            .shadow(vec![
                gpui::BoxShadow::new(px(0.), px(-6.), gpui::hsla(0., 0., 0., 0.18))
                    .blur_radius(px(16.)),
                gpui::BoxShadow::new(px(0.), px(-2.), gpui::hsla(0., 0., 0., 0.14))
                    .spread_radius(px(-1.))
                    .blur_radius(px(4.)),
            ])
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
                    .rounded_full()
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
                    .rounded_full()
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

    /// 可交互横向条（seek 进度条 / 音量条共用）：玻璃细轨 + 主色填充 + hover 放大 thumb。
    /// 轨道视觉厚度从 16px 收到 4px（交互热区仍是 16px，`bounds` 盒不变），canvas 记录
    /// **窗口坐标**盒到 `bounds`（渲染期写入；事件回调读）。`seek=true` 时 `flex_1`
    /// 拉伸并走 seek 处理器，否则固定宽走音量处理器。
    fn strip_element(
        id: &'static str,
        bounds: Rc<Cell<Bounds<Pixels>>>,
        fraction: f32,
        track: Hsla,
        fill: Hsla,
        seek: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let f = fraction.clamp(0.0, 1.0);
        // 元素级 hover 态：只驱动 thumb 放大/淡入（native 按钮同款姿势），不碰
        // seek/vol 已有的 on_mouse_down/on_mouse_move 交互路径。
        let hover_flag = window.use_keyed_state(format!("{id}-hover"), cx, |_, _| false);
        let hovered = *hover_flag.read(cx);
        let view_id = window.current_view();
        let mut el = div()
            .id(id)
            .h(px(16.))
            .relative()
            .cursor(CursorStyle::PointingHand)
            .on_hover({
                let flag = hover_flag.clone();
                move |hovered, _w, cx| {
                    flag.update(cx, |v, _| *v = *hovered);
                    cx.notify(view_id);
                }
            })
            .child(
                // 玻璃细轨（4px 半透明）+ 主色填充 + hover thumb：thumb 圆心始终对准
                // 填充末端（= 播放位置）；大小/偏移/透明度共用同一 0..1 弹簧值，增大的
                // 同时圆心不漂移（top/right 由尺寸反推）。
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .relative()
                            .h(px(4.))
                            .rounded_full()
                            .bg(track.opacity(0.30))
                            .child(
                                div()
                                    .absolute()
                                    .left_0()
                                    .top_0()
                                    .bottom_0()
                                    .w(DefiniteLength::Fraction(f))
                                    .rounded_full()
                                    .bg(fill)
                                    .child(
                                        div()
                                            .absolute()
                                            .with_spring(
                                                format!("{id}-thumb"),
                                                hover_spring(if hovered { 1.0 } else { 0.0 }),
                                                {
                                                    let fill = fill;
                                                    move |this, v| {
                                                        let s = 4.0 + 6.0 * v;
                                                        this.w(px(s))
                                                            .h(px(s))
                                                            // 垂直居中：轨道半高 2px − 半径 s/2
                                                            .top(px(2.0 - s / 2.0))
                                                            // 圆心对准填充末端：右缘外扩 s/2
                                                            .right(px(-s / 2.0))
                                                            .opacity(0.15 + 0.85 * v)
                                                            .bg(fill)
                                                    }
                                                },
                                            ),
                                    ),
                            ),
                    ),
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
                .on_mouse_move(cx.listener(Self::on_seek_move))
                .on_mouse_up(MouseButton::Left, cx.listener(Self::on_seek_up))
                .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_seek_up));
        } else {
            el = el
                .on_mouse_down(MouseButton::Left, cx.listener(Self::on_vol_down))
                .on_mouse_move(cx.listener(Self::on_vol_move));
        }
        el.into_any_element()
    }

    /// seek 点按：进入拖拽预览（begin_scrub）——先落点预览、静音；松手提交。
    fn on_seek_down(
        &mut self,
        e: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(f) = fraction_at(&self.seek_bounds, e.position) {
            self.begin_scrub(f, cx);
        }
    }

    /// seek 拖拽（左键按住才推进；移出条到窗外松开由 `on_seek_up`/`on_mouse_up_out` 兜底）。
    fn on_seek_move(
        &mut self,
        e: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !e.dragging() {
            // 已在拖拽但这一拍 dragging 变假（松开）而 mouse_up 未落在条上 → 按当前落点收尾
            if self.scrubbing {
                self.end_scrub_at_current(cx);
                return;
            }
            // M0：未拖拽的悬停 → 记录悬停位置并（空闲时）后台预解码该落点帧，点击即现。
            if let Some(f) = fraction_at(&self.seek_bounds, e.position) {
                self.update_hover_target(self.clamped_scrub_target(f), cx);
            }
            return;
        }
        if let Some(f) = fraction_at(&self.seek_bounds, e.position) {
            if self.scrubbing {
                self.update_scrub(f, cx);
            } else {
                self.begin_scrub(f, cx);
            }
        }
    }

    /// seek 拖拽结束（窗口内松开）：按松手位置最终提交。
    fn on_seek_up(
        &mut self,
        e: &MouseUpEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.scrubbing {
            return;
        }
        if let Some(f) = fraction_at(&self.seek_bounds, e.position) {
            self.end_scrub(f, cx);
        } else {
            self.end_scrub_at_current(cx);
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
        let theme = ui::theme::Theme::global(cx);
        let is_dark = theme.is_dark();
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
        // 视频实际显示矩形（contain 等比+缩放+平移后的像素框，单位 f32）。暂停徽标以此为基准
        // 挂右上角内边距，不随窗口/画布走——防止 contain 后有黑边时徽标飘出视频范围。
        let mut vrect = (0.0, 0.0, vw, area_h);
        let body: gpui::AnyElement = if let Some(err) = &self.load_error {
            Self::placeholder(&format!("无法打开视频：{err}"), muted, cx)
        } else {
            match (self.frame.clone(), self.intrinsic) {
                // 上屏帧恒为 BGRA（`RenderImage`）：硬解 reader 的帧在解码线程内核已回读+sws
                // 转 BGRA，与软解逐字同构；旧 M2e `paint_external_yuv` 直通管线已废除。
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
                    vrect = (ox, oy, dw, dh);
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
                    Self::placeholder("正在打开视频…", muted, cx)
                }
                (_, _) => Self::placeholder("暂停", muted, cx),
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
                    // 双击画面 → 全屏 + 锁定（单击/拖拽仍是 on_pan_down）。互斥不靠「上层覆盖
                    // 元素优先消费」：gpui 鼠标事件在命中栈内冒泡，控制条/状态浮层/进度条命中
                    // 的双击由各自 `on_controls_mouse_down` `stop_propagation` 拦截，到不了这里；
                    // 隐藏态控制条透明热区不挂该拦截 → 双击仍是画面全屏。
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::on_video_surface_mouse_down))
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
                    .top_3()
                    .right_3()
                    .px_3()
                    .py_1()
                    .rounded_lg()
                    .border_1()
                    .border_color(toast_border)
                    // 状态浮层同样吞双击（归属 `on_controls_mouse_down` 互斥族：不向画面漏）。
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::on_controls_mouse_down))
                    // 玻璃卡片：半透明表面 + 圆角 + 高光内阴影；出现时短弹簧淡入（收敛即静止，
                    // 无持续重绘）。消失仍由 `STATUS_MS` 到期卸载，不做离场动画。
                    .bg(toast_surface.opacity(if is_dark {
                        GLASS_PANEL_ALPHA_DARK
                    } else {
                        GLASS_PANEL_ALPHA_LIGHT
                    }))
                    .shadow(vec![
                        gpui::BoxShadow::new(px(0.), px(4.), gpui::hsla(0., 0., 0., 0.18))
                            .blur_radius(px(10.)),
                        gpui::BoxShadow::new(px(0.), px(1.), gpui::hsla(0., 0., 0., 0.14))
                            .blur_radius(px(2.)),
                    ])
                    .text_size(px(12.))
                    .text_color(toast_fg)
                    .child(text.clone())
                    .with_spring(
                        "video-toast-fade",
                        hover_spring(1.0).from(0.0),
                        |this, v| this.opacity(v),
                    ),
            );
        }
        // 暂停标志（右上角）：仅放大 ❚❚ 双竖条图标，无圆底/边框/投影；暂停/停在结尾时
        // 显示。以**视频实际矩形** `vrect` 为基准（右上角内 32px），不随窗口/画布走；纯视觉
        // 无交互（不遮双击全屏）；仅在有画面且非播放、未出错时挂出。
        let badge_shown = Self::paused_badge_shown(self.playing, self.frame.is_some(), self.load_error.is_some());
        if badge_shown {
            let badge_left = (vrect.0 + vrect.2 - 80.0).max(4.0);
            let badge_top = (vrect.1 + 30.0).max(4.0);
            root = root.child(
                div()
                    .id("video-paused-badge")
                    .absolute()
                    .left(px(badge_left))
                    .top(px(badge_top))
                    .child(Icon::new(IconName::Pause).size(px(60.)).text_color(toast_fg))
                    .with_spring(
                        "video-paused-fade",
                        hover_spring(1.0).from(0.0),
                        |this, v| this.opacity(v),
                    ),
            );
        }
        // 字幕渲染层：有字幕、字幕可见、且当前时间有活跃字幕时叠加。
        // 定位以**视频实际矩形** `vrect`（contain 适配后含缩放/平移的画面像素框，
        // 本函数前段逐帧就绪时更新）为基准：画面底部居中、留安全边距，字幕不越出
        // 画面、不遮控制条；无画面（占位/黑边）时退回画布视频区底部。gpui 没有文字
        // 描边能力（styled/text_system 无 text_stroke），故退化为**半透明深色圆角
        // 底条**保证可读性（颜色走 subtitle_style.outline_color，默认黑，即
        // 「白字黑描边」的落底实现，零新依赖）。
        if self.subtitle_visible && !self.subtitles.is_empty() {
            if let Some(subtitle) = find_active_subtitle(&self.subtitles, self.playhead) {
                let text = parse_ass_text(&subtitle.text);
                if !text.trim().is_empty() {
                    let (vx, vy, vdw, vdh) = vrect;
                    // 随画面高度适度放大（参考高度 480，钳位 0.8x..=1.8x）
                    let scale = (vdh / 480.0).clamp(0.8, 1.8);
                    let font_size = self.subtitle_style.font_size * scale;
                    let strip_bg = self.subtitle_style.outline_color.opacity(0.55);
                    root = root.child(
                        div()
                            .absolute()
                            .left(px(vx))
                            .top(px(vy))
                            .w(px(vdw))
                            .h(px(vdh))
                            .flex()
                            .flex_col()
                            .items_center()
                            .justify_end()
                            .pb_4()
                            .child(
                                div()
                                    .max_w_full()
                                    .px_4()
                                    .py_2()
                                    .rounded_lg()
                                    .bg(strip_bg)
                                    .text_size(px(font_size))
                                    .text_color(self.subtitle_style.primary_color)
                                    .child(text),
                            ),
                    );
                }
            }
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
        // 圆形 hover 底：靠 `Button` 自带的选定/按压 backplate 弹簧打在圆内高亮
        .rounded_full()
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

// 扫描「当前视频所在文件夹全部视频」已收敛到 [`FolderNav::scan`]
//（`crate::video::is_video_extension` 作扩展名白名单）——不再各自实现。

#[cfg(test)]
mod tests {
    use super::*;
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

    /// 回归（② 首帧落地即唤醒的机制本体）：`pending_seek` 活跃时泵用短拍轮询
    ///（[`SEEK_WAKE_POLL_MS`]），不再等帧率节拍（fps=24 → 41ms）；平时保持帧率节拍。
    #[gpui::test]
    async fn pump_wait_interval_tightens_during_pending_seek(cx: &mut TestAppContext) {
        let ent = cx.update(|app| app.new(|cx| {
            let mut v = blank(fixture(), cx);
            v.playing = true;
            v.fps = Some((24, 1)); // pump_interval ≈ 41ms
            v
        }));
        let normal = cx.update(|app| ent.read(app).pump_wait_interval());
        assert_eq!(normal, Duration::from_millis(41), "无待定 seek = 帧率节拍");
        ent.update(cx, |v, _| v.pending_seek = Some(5.0));
        let short = cx.update(|app| ent.read(app).pump_wait_interval());
        assert_eq!(
            short,
            Duration::from_millis(SEEK_WAKE_POLL_MS),
            "pending_seek 活跃 = 短拍（frame2screen 不再白等一拍）"
        );
        cx.run_until_parked();
    }

    /// 回归（② 端到端）：seek 首帧就绪时，短拍泵在 ~[`SEEK_WAKE_POLL_MS`]ms 内即 drain+上屏，
    /// 不等完整帧率节拍（fps=24 → 41ms）。fake clock 只推进短拍窗（2ms << 41ms）→ 帧应
    /// 已上屏、`pending_seek` 清空；反向护栏：若泵退化成纯帧率节拍轮询，2ms 拍不会到，
    /// 首帧不会上屏，断言即红。
    #[gpui::test]
    async fn pump_short_poll_lands_seek_first_frame_fast(cx: &mut TestAppContext) {
        let ent = cx.update(|app| app.new(|cx| {
            let mut v = blank(fixture(), cx);
            v.playing = true;
            v.duration = 10.0;
            v.fps = Some((24, 1)); // 正常节拍 41ms（>短拍，护栏判据）
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
        // worker 行为：预滚最后一帧（≤ 落点，首帧即可上屏）+ 首个 ≥ 落点帧
        tx.send(frame_of(4.96, 1)).ok();
        tx.send(frame_of(5.1, 9)).ok();
        // 只推进短拍窗：41ms 节拍远未到
        cx.executor().advance_clock(Duration::from_millis(SEEK_WAKE_POLL_MS));
        cx.run_until_parked();
        let s = cx.update(|app| {
            let v = ent.read(app);
            (v.frame.is_some(), v.pending_seek)
        });
        assert!(s.0, "短拍期内 pump 应立即上屏首帧（不待帧率节拍 41ms）");
        assert_eq!(s.1, None, "首帧上屏后 pending_seek 应立即清空");
        cx.run_until_parked();
    }

    /// 回归（② 暂停态）：暂停中 seek（pump 尚未跑）→ `seek()` 置 `pending_seek` 并起泵，
    /// 泵以短拍续跑（等首帧上屏）；不因 `playing=false` 停泵。
    #[gpui::test]
    async fn paused_seek_keeps_short_poll_pump_alive(cx: &mut TestAppContext) {
        let ent = cx.update(|app| app.new(|cx| {
            let mut v = blank(fixture(), cx);
            v.playing = false; // 暂停态：初始无 pump
            v.duration = 10.0;
            v
        }));
        ent.update(cx, |v, cx| v.seek(2.0, cx));
        cx.run_until_parked();
        let s = cx.update(|app| {
            let v = ent.read(app);
            (v.pending_seek, v.pump_running)
        });
        assert_eq!(s.0, Some(2.0), "暂停 seek 应保留 pending_seek（等首帧上屏）");
        assert!(s.1, "暂停 seek 也应起泵（pending_seek 活跃 → 短拍续跑）");
        // 短拍窗内泵仍活跃（不因 playing=false 停）
        cx.executor().advance_clock(Duration::from_millis(SEEK_WAKE_POLL_MS));
        cx.run_until_parked();
        let running = cx.update(|app| ent.read(app).pump_running);
        assert!(running, "pending_seek 未清前泵应持续短拍运行");
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
}
