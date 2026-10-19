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
    Anchor, AnimationExt, App, Bounds, Context, CursorStyle, DefiniteLength, Edges, Hsla,
    InteractiveElement, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ObjectFit, ParentElement, Pixels, Point, Render, RenderImage, ScrollHandle,
    ScrollWheelEvent, StatefulInteractiveElement, Styled, StyledImage, Window, anchored, canvas,
    deferred, div, img, point, px, prelude::FluentBuilder as _,
};
use image::{Frame, RgbaImage};

use ui::button::Button;
use ui::button::ButtonVariants as _;
use ui::icon::{Icon, IconName};
use ui::motion::hover_spring;

use super::audio::AudioSink;
use super::decode::{AudioReader, VideoReader};
use super::hw::{CopyError, SharedCopier, create_worker_d3d11_device};
use super::subtitle::{SubtitleEntry, SubtitleStyle, find_active_subtitle, parse_ass_text};
// 拆分子模块后 `super` 变成本 viewer（不再是 crate 根）：给子模块内保留原 `super::decode::…`
// 路径的调用兜底——重新导出 crate 根 `decode` 模块，正文无需改动。
pub(crate) use crate::decode;

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
/// seek 首帧落地等待期的**事件驱动泵**兜底超时（ms）：`pending_seek` 活跃时泵不再按
/// ~2ms 轮询，而是等待 worker「帧就绪通知」——worker 入队一帧即唤醒 drain+上屏，帧到屏
/// 从轮询「Windows 定时器对齐/低调度会偶发 20-40ms 尖峰」收敛到「通知即到」（≈0-2ms）。
/// 本值只作「无新帧」时的兜底超时（= 粗粒度刷新节拍，兼推进度条；K12 合规，不引入常驻
/// 轮询、不持续逐帧重绘）。平时仍按帧率节拍，播放稳态 CPU 不增。
const SEEK_WAKE_CAP_MS: u64 = COARSE_REPAINT_MS;
/// 事件驱动泵的**切片兜底**（ms）：seek/播放两条等待统一走「wake + 超时 = min(切片, 帧率
/// 节拍)」（见 `pump_event_timeout`）。无通知时不在 [`SEEK_WAKE_CAP_MS`] 上整睡一拍，而是
/// 每 ≤ 切片/节拍 醒来由 pump tick 排空帧通道——通知被 bound cap-1 级联丢弃但帧已在通道时，
/// 本片即上屏（真机实测同落点连点曾漏通知睡满 200ms 静默→尖峰；播放支路旧定时整睡一拍节拍，
/// 曾致「seek 落点恰逢播放节拍休眠」时 frame2screen 偶发 38-76ms 末梢）。低帧率（节拍 >
/// 32ms）取本值兜底、高帧率取节拍（不改变播放节奏与上下限语义）；多出的空排空拍可忽略——
/// 正常事件驱动路径「通知即到」，本值只作兜底粒度，不影响典型帧到屏 ≈0-2ms。
#[cfg(not(test))]
const SEEK_WAKE_SLICE_MS: u64 = 32;
/// cfg(test) 下 pump 事件等待的兜底 fake 定时器粒度（ms）：fake 调度器**无跨线程唤醒**
///（确定性护栏拒绝 foreign wake），事件通知改用「先 `try_recv` 排空已就绪通知、无则睡细
/// 拍 fake 定时器」的半阻塞姿势表达——细拍 ≈ 旧 2ms 泵，`advance_clock` 驱动 pump 及时
/// 排空 worker 已入队的帧（bench 帧到屏 ≈ 细拍粒度，可作回归锚）。生产仍走真实事件通知
///（见 [`SEEK_WAKE_CAP_MS`] 与 `ensure_pump`），本常量只影响测试。
#[cfg(test)]
const SEEK_WAKE_POLL_MS_TEST: u64 = 2;
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
// --- F1 悬停时间提示 ---
/// F1 悬停时间浮层与进度条上缘的纵向间隙（px）。
const HOVER_TIME_GAP_PX: f32 = 6.0;
/// F1 悬停时间浮层的 deferred 绘制优先级：高于普通 deferred（note card = 1）/tooltip（60），
/// 悬浮在控制条之上，低于全局 toast（200）/菜单（100→120 不与其冲突）。
const HOVER_TIME_LAYER_PRIORITY: usize = 120;

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
    // —— 各节点细分探针（[SEEKNODE]，env 开才打印；普通运行 None 空置，零开销）——
    /// cmd 实际投递完成的 GUI 线程时刻（`start_decoder_at` 记）：[SEEK] total 起点与
    /// [SEEKNODE]「submit」终点。
    sent: Option<Instant>,
    /// worker 收到 cmd、开始处理的时刻（`video_worker` 命令开头记）：[SEEKNODE]「wake」终点。
    recv: Option<Instant>,
    /// worker 打开/复用完成、`reader.seek` 之前的时刻（[SEEKNODE] 把「open」与「seek 调用」拆开）。
    opened: Option<Instant>,
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

/// 测试探针：worker 最近一次打开 reader 时**双设备 GPU 直通**是否真正接管（区别于 SAFE
/// 硬解 BGRA——`hw_engaged` 两者都置 true，双设备测试需单独断言直通引擎生效）。
#[cfg(test)]
static DUAL_ENGAGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
pub(crate) fn dual_engaged() -> bool {
    DUAL_ENGAGED.load(Ordering::Relaxed)
}

#[cfg(test)]
pub(crate) fn dual_engaged_reset() {
    DUAL_ENGAGED.store(false, Ordering::Relaxed);
}

/// 双设备直通允许开关：**生产恒开**（`dual_path_allowed` = true，auto 模式直通优先）；
/// **测试默认关**（保持既有 SAFE BGRA 硬解/软解基准与回归测试的原路径不动），仅
/// `gpu_passthrough_*` 测试用 [`DualPathGuard`] 显式开启。这是双设备与现有 40+ 真解码
/// 测试基线互不污染的隔离阀门（同 `hw_fault` 注入的 cfg(test) 折叠纪律）。
#[cfg(test)]
static DUAL_PATH_ENABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
pub(crate) fn dual_path_set_enabled(v: bool) {
    DUAL_PATH_ENABLED.store(v, Ordering::Relaxed);
}

#[cfg(test)]
fn dual_path_allowed() -> bool {
    DUAL_PATH_ENABLED.load(Ordering::Relaxed)
}

#[cfg(not(test))]
fn dual_path_allowed() -> bool {
    true
}

/// 测试专用 RAII 守卫：构造开启双设备直通、Drop 复位（任何 return/panic 路径都不把
/// 直通首选项泄漏给后续测试）。
#[cfg(test)]
struct DualPathGuard;

#[cfg(test)]
impl DualPathGuard {
    fn new() -> Self {
        dual_path_set_enabled(true);
        Self
    }
}

#[cfg(test)]
impl Drop for DualPathGuard {
    fn drop(&mut self) {
        dual_path_set_enabled(false);
    }
}

/// 测试专用故障注入（模拟硬解设备丢失 / 瞬时解码错误）——仅 `cfg(test)` 存在，生产零字段
/// 零开销（编译期折叠）：视频 worker 在给**硬解** reader 发解码/预滚 FFI 前查 [`hw_fault_hook`]，
/// 返回 true 即视为本次调用失败（走与真实 FFI 报错完全相同的恢复路径）。
///
/// 计数语义：`HW_FAULT_AFTER > 0` 的每一拍 = 放行一帧成功并 -1；归 0 后按 [`HW_FAULT_KIND`]
/// 触发（1 = 一次性注入后自动撤销=瞬时错误场景；2 = 持续注入=设备丢失场景，直到 worker 重开
/// 软解不再走硬解路径）。
#[cfg(test)]
static HW_FAULT_AFTER: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(-1);
#[cfg(test)]
static HW_FAULT_KIND: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
/// 注入已实际触发过（测试等待把「错误已发生」与「重开已完成」解耦）。
#[cfg(test)]
static HW_FAULT_FIRED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 测试专用：置故障注入。`after` = 之前还要放过的**成功**硬解帧数（0 = 下一次硬解调用即注入）；
/// `persistent` = true 持续失败（设备丢失），false 一次性失败后自动撤销（瞬时错误）。配合
/// HwFaultGuard（Drop 复位）保证任何退出路径都不把注入泄漏给后续测试。
#[cfg(test)]
pub(crate) fn hw_fault_fail_after(after: i64, persistent: bool) {
    HW_FAULT_AFTER.store(after, Ordering::Relaxed);
    HW_FAULT_KIND.store(if persistent { 2 } else { 1 }, Ordering::Relaxed);
    HW_FAULT_FIRED.store(false, Ordering::Relaxed);
}

/// 测试专用：撤销故障注入。
#[cfg(test)]
pub(crate) fn hw_fault_disable() {
    HW_FAULT_AFTER.store(-1, Ordering::Relaxed);
    HW_FAULT_KIND.store(0, Ordering::Relaxed);
}

/// 测试专用：注入是否已实际触发过至少一次。
#[cfg(test)]
pub(crate) fn hw_fault_fired() -> bool {
    HW_FAULT_FIRED.load(Ordering::Relaxed)
}

/// 测试专用：worker 硬解调用前的故障钩子。返回 true = 本次调用模拟失败（`skip_to_bgra_into`
/// 与 `next_frame_bgra_into` 两条 FFI 路径共用）。
#[cfg(test)]
fn hw_fault_hook() -> bool {
    let after = HW_FAULT_AFTER.load(Ordering::Relaxed);
    if after > 0 {
        // 放行一帧成功（计数 -1），直到归 0 才触发注入
        HW_FAULT_AFTER.fetch_sub(1, Ordering::Relaxed);
        return false;
    }
    if after != 0 {
        return false; // -1 = 未启用
    }
    match HW_FAULT_KIND.load(Ordering::Relaxed) {
        1 => {
            // 一次性：注入一次后自动撤销（瞬时错误场景——播放继续、不应触发重开）
            HW_FAULT_KIND.store(0, Ordering::Relaxed);
            HW_FAULT_AFTER.store(-1, Ordering::Relaxed);
            HW_FAULT_FIRED.store(true, Ordering::Relaxed);
            true
        }
        2 => {
            // 持续失败（TDR / 设备丢失）：worker 重开软解前一直报错
            HW_FAULT_FIRED.store(true, Ordering::Relaxed);
            true
        }
        _ => false,
    }
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

/// 解码线程经通道传递的一帧：软 BGRA 帧（sws/回读转好、主线程包 `RenderImage` 上屏）或
/// 双设备 GPU 直通帧（worker 自有设备解码 + GPU 侧拷进共享槽，主线程 `paint_external_yuv`
/// 上屏，CPU 零回读/零 sws）。
enum FrameItem {
    /// 解码 BGRA 帧（sws/回读转好、主线程包 `RenderImage` 上屏）。
    Soft(SoftFrame),
    /// 双设备 GPU 直通帧：worker 已把解码帧拷进共享 NV12 槽（keyed mutex 已释放），
    /// 跨线程只传 `shared_handle` 数值；主线程上屏时渲染设备打开该句柄采样。
    Gpu(GpuFrame),
}

/// 解码一帧（软 BGRA）。
struct SoftFrame {
    pts: f64,
    w: u32,
    h: u32,
    bgra: Vec<u8>,
}

/// 一帧双设备 GPU 直通产物（worker 产出、主线程经通道消费）。
struct GpuFrame {
    pts: f64,
    w: u32,
    h: u32,
    /// DXGI 共享句柄（usize 数值；worker 槽池持有 COM 引用，主线程用后经释放消息回池）。
    shared_handle: usize,
}

impl FrameItem {
    fn pts(&self) -> f64 {
        match self {
            FrameItem::Soft(f) => f.pts,
            FrameItem::Gpu(f) => f.pts,
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
    /// 帧就绪通知发送端（P1 事件驱动泵）：worker 每次成功入队一帧后 `try_send(())` 唤醒
    /// 主线程 pump（`pending_seek` 期事件驱动唤醒，替代旧 ~2ms 轮询；bounded try_send
    /// 满则丢弃，级联共享）。
    wake: std::sync::mpsc::SyncSender<()>,
}

/// 帧就绪通知（P1 事件驱动泵）：worker 每成功把一帧入队主线程就 `try_send` 一个空通知。
/// **bounded cap 1 + 级联语义**（满则丢弃——存在即「有新帧待排空」，多帧共享同一信号；
/// 无界则长播期间通知无限堆积，故必须 bounded）。接收端由主线程 pump 任务经
/// `Arc<Mutex<Receiver>>` 共享（同 `release_cmd` 姿势），`pending_seek` 期等待「新帧
/// 就绪」替代旧 ~2ms 轮询。生命周期与 viewer 同长（构造即建，恒 Some）；worker 侧经
/// `DecodeCmd` 里的 Sender 克隆发通知，命令换代/收尾只 drop 克隆，viewer 持原始端保活。
///
/// ⚠️ 测试/生产两态（见 `ensure_pump`）：生产 = 真实后台线程阻塞 `recv_timeout`（事件
/// 驱动、帧到屏「通知即到」）；`cfg(test)` fake 调度器**无跨线程唤醒**（确定性护栏拒绝
/// foreign wake，本仓实测 waker 方案被拒）→ 泵改「先 `try_recv` 排空已就绪通知、无则睡
/// 兜底 fake 定时器」的半阻塞姿势（可 park、不死等），由 `advance_clock` 驱动。
struct FrameWake {
    /// worker 侧通知发送端（克隆进每条 `DecodeCmd`；viewer 持原始端保活通道）。
    tx: std::sync::mpsc::SyncSender<()>,
    /// 主线程 pump 任务接收端：`ensure_pump` 阻塞 `recv_timeout` 用（独占共享，无并发竞争）。
    rx: Arc<std::sync::Mutex<std::sync::mpsc::Receiver<()>>>,
}

/// 把一个解码帧经 `tx` 入队，成功则 `wake` fire-and-forget 唤醒 pump（P1 事件驱动泵：
/// 帧已入队 → 通知 pump，级联语义由 bounded cap 1 承载，满则丢弃不阻塞、不堆积）。
/// 阻塞版 `send`（通道满时等 pump 排空）——常规播放/seek 预滚送帧共用。返回是否入队成功。
fn send_relay(
    tx: &std::sync::mpsc::SyncSender<FrameItem>,
    wake: &std::sync::mpsc::SyncSender<()>,
    item: FrameItem,
) -> bool {
    if tx.send(item).is_ok() {
        let _ = wake.try_send(());
        true
    } else {
        false
    }
}

/// 非阻塞版 [`send_relay`]：`try_send` 立刻失败（=丢弃）不阻塞 worker——EOF/换代保底
/// 路径专用（接收端已换走 / 通道满都不必等 pump 排空）。
fn try_send_relay(
    tx: &std::sync::mpsc::SyncSender<FrameItem>,
    wake: &std::sync::mpsc::SyncSender<()>,
    item: FrameItem,
) -> bool {
    if tx.try_send(item).is_ok() {
        let _ = wake.try_send(());
        true
    } else {
        false
    }
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
    /// 当前上屏帧（软 BGRA：`RenderImage` 图集 tile）。
    frame: Option<Arc<RenderImage>>,
    /// 当前上屏帧（双设备 GPU 直通：共享 NV12 槽句柄）。二者互斥：装软帧清 GPU 帧、
    /// 装 GPU 帧清软帧（切换模式/回退时旧帧槽显式回池）。
    gpu_frame: Option<GpuFrame>,
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
    /// `avformat_open_input` 会卡死”（72.MP4 复现），单 worker 串行重开则稳定。
    decode_cmd: Option<std::sync::mpsc::Sender<DecodeCmd>>,
    /// 双设备 GPU 直通：主线程把「已换下的 GPU 帧共享槽」归还 worker 的释放通道（无界，
    /// 只传数值；worker 逐帧拉取回池复用——与帧通道同生命周期，viewer Drop 时随之断连）。
    release_cmd: Option<std::sync::mpsc::Sender<usize>>,
    /// 帧就绪通知（P1 事件驱动泵）：`ensure_pump` 在 `pending_seek` 活跃时阻塞等待 worker
    /// 的帧入队通知（取代旧 ~2ms 轮询）；构造即建，恒 `Some`。
    frame_wake: Option<FrameWake>,
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
    /// F1 悬停时间浮层：悬停光标的**窗口坐标** x（px；在 seek 进度条盒内才记录，随 `on_seek_move`
    /// 刷新；开始拖拽 / 移出进度条 / 换文件时清 None → 浮层隐藏）。不在 render 里做命中检测，
    /// 由事件回调记录这一帧的横向位置，浮层紧随光标移动。
    hover_x: Option<f32>,
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
    // --- 解码偏好（控制条按钮循环：自动 ↔ 强制软解）---
    /// 查看器解码偏好：`0` = 自动（硬解优先）、`1` = 强制软解。随控制条「解码偏好」按钮
    /// 循环切换；变更时同步写进程级旋钮 [`crate::decode::set_decode_mode`]（worker 打开
    /// reader 时按它选模式，模式在 reader 复用键里 → 切换即重开）并重开当前 reader @当前
    /// 播放位置续播（同设备丢失自恢复的「重开 + seek 续播」姿势）。
    decode_pref: u8,
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
            gpu_frame: None,
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
            hover_x: None,
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
            rng_state: seed_rng(),
            status: None,
            status_seq: 0,
            decode_pref: 0,
            subtitles: Vec::new(),
            subtitle_visible: true,
            subtitle_style: SubtitleStyle::default(),
        };
        this.start(cx);
        // 初始浮现控制条、无悬停 1s 后自隐（hover 会作废本次隐藏）
        this.show_controls_temporarily(cx);
        this
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

    /// 当前画布背景色（`set_background` 传入；None = 未设置 → render 回落 `default_canvas`）。
    pub fn background(&self) -> Option<Hsla> {
        self.bg
    }
}

impl Drop for VideoViewer {
    fn drop(&mut self) {
        // 换代让还在跑的视频/音频解码线程下一趟自退，避免实体销毁后僵尸线程继续占核
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.audio_gen.fetch_add(1, Ordering::SeqCst);
    }
}

// 扫描「当前视频所在文件夹全部视频」已收敛到 [`FolderNav::scan`]
//（`crate::video::is_video_extension` 作扩展名白名单）——不再各自实现。


// 拆分子模块：workers（解码/悬停/音频 worker 与 pump）、playback（播放核心/seek/音量/速度/
// 文件切换）、display（帧上屏/截图/几何缩放）、input（键盘/鼠标/控制条交互）、render（渲染
// 与控制条）、tests（真解码回归与基准）。方法以 `impl super::VideoViewer` 定义、跨模块调用
// 经 `pub(super)` 可见（E0624：同型固有方法在兄弟子模块私有不可见，仅字段可见）。模块根保留
// 类型/常量/测试探针、`VideoViewer` 结构、`new` 与宿主访问器、`impl Drop`。

mod workers;
mod playback;
mod display;
mod input;
mod render;

#[cfg(test)]
mod tests;
