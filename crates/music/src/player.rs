//! 音乐播放引擎（gpui Entity）：主线程权威状态 + 后台解码线程 + 事件回送。
//!
//! # 线程与驱动模型
//! - 主线程持有全部权威状态（`library`/`queue`/`current`/`playing`/`paused`…）；
//! - 一个 **worker 线程**（std mpsc 命令通道 + 事件通道）：`Load` 开曲目进入解码循环
//!   （`next_pcm`→`sink.push`，回压 `queued_len>1s×采样率×2` 时短轮询新命令）；
//!   `Seek` 调解码 seek + sink.flush；`Pause/Resume/Stop` 组 sink；EOF 发 `Finished`。
//!   worker 与 probe 都走 `media_ffmpeg.dll`（FFMPEG_LOCK 在 dll 内部，进程内已串行）。
//! - **事件消费由 `tick` 外部驱动**（Phase B 的 MusicView 持渲染/交互节拍调它；Phase A
//!   不加常驻定时器，避免测试悬空 + fake 调度器无跨线程唤醒（K26）——worker 事件经
//!   mpsc 入队，`tick` 逐条 `try_recv` 处理）。sink 只打开一次（Same instance 跨曲目复用）。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use gpui::{Context, IntoElement, Render, Window};

use crate::decode;
use crate::library::{build_mix_groups, probe_duration, scan_dir};
use crate::model::{MixGroup, RepeatMode, Track};
use crate::sink::AudioSink;

/// 后台解码线程命令（std mpsc）。
pub enum WorkerCmd {
    /// 载入并播放新曲目（代序号降陈旧 Opened 事件）。
    Load { gen_id: u64, path: PathBuf, rate: u32 },
    /// 跳转到 `seconds`（worker 内 decode seek + sink.flush）。
    Seek { gen_id: u64, seconds: f64 },
    Pause,
    Resume,
    Stop,
    Shutdown,
}

/// worker → 主线程事件。
pub enum Event {
    /// 曲目已打开（`duration` 秒）；`gen_id` 与命令代序号匹配才落地。
    Opened { gen_id: u64, duration: f64 },
    /// 播放到流末尾（主线程按 repeat/shuffle 决定下一首）。带 `gen_id`：与 Opened 同款
    /// 陈旧过滤——EOF 与用户手动切曲在同一 tick 窗口内竞争时，旧曲目的 Finished 不得
    /// 把播放再往前推一位（覆盖用户的 next）。
    Finished { gen_id: u64 },
    /// 解码/寻址失败。带 `gen_id`：旧曲目在用户已切走后失败不得污染当前错误态。
    Error { gen_id: u64, message: String },
}

/// 音乐播放器（gpui Entity）。Phase A 无 UI，`Render` 返回空占位。
pub struct MusicPlayer {
    library: Vec<Track>,
    mix_groups: Vec<MixGroup>,
    dir: Option<PathBuf>,
    /// 播放队列：`library` 下标（shuffle 时一键洗牌；`next`/`prev` 在队列内导航）。
    queue: Vec<usize>,
    queue_pos: usize,
    current: Option<usize>,
    playing: bool,
    paused: bool,
    repeat: RepeatMode,
    shuffle: bool,
    volume: f32,
    sink: Option<AudioSink>,
    worker_tx: Option<mpsc::Sender<WorkerCmd>>,
    worker_join: Option<std::thread::JoinHandle<()>>,
    evt_rx: mpsc::Receiver<Event>,
    error: Option<String>,
    duration: f64,
    liked: HashSet<String>,
    /// Load 代序号：每次 `play_track` 递增，worker 回送 Opened.gen_id 与之匹配过滤陈旧。
    generation: u64,
}

impl MusicPlayer {
    /// 新建播放器：事件通道读端就位（worker 在首次开 sink 时才启动并换入新通道），
    /// 事件经 `tick` 消费（见模块文档驱动模型）。
    pub fn new(_cx: &mut Context<Self>) -> Self {
        let (_evt_tx, evt_rx) = mpsc::channel::<Event>();
        Self {
            library: Vec::new(),
            mix_groups: Vec::new(),
            dir: None,
            queue: Vec::new(),
            queue_pos: 0,
            current: None,
            playing: false,
            paused: false,
            repeat: RepeatMode::Off,
            shuffle: false,
            volume: 1.0,
            sink: None,
            worker_tx: None,
            worker_join: None,
            evt_rx,
            error: None,
            duration: 0.0,
            liked: HashSet::new(),
            generation: 0,
        }
    }

    /// 消费 worker 事件（外部驱动口：Phase B 的 UI 在渲染/交互节拍调用；测试显式调用）。
    /// 逐个 `try_recv` 处理——Opened 更新时长/playing、Finished 按 repeat/shuffle 自动
    /// 下一首、Error 落地错误信息。无事件则空操作返回。
    pub fn tick(&mut self, cx: &mut Context<Self>) {
        let mut changed = false;
        while let Ok(evt) = self.evt_rx.try_recv() {
            self.on_event(evt, cx);
            changed = true;
        }
        if changed {
            cx.notify();
        }
    }

    fn on_event(&mut self, evt: Event, cx: &mut Context<Self>) {
        match evt {
            Event::Opened { gen_id, duration } => {
                if gen_id != self.generation {
                    return; // 陈旧（已切曲目/换代）：丢弃
                }
                self.duration = duration;
                if let Some(i) = self.current
                    && let Some(t) = self.library.get_mut(i)
                {
                    t.duration = duration;
                }
                self.playing = true;
                self.paused = false;
            }
            Event::Finished { gen_id } => {
                if gen_id != self.generation {
                    return; // 陈旧（已切曲目/换代）：丢弃，不再自动推进
                }
                self.paused = false;
                self.auto_advance(cx);
            }
            Event::Error { gen_id, message } => {
                if gen_id != self.generation {
                    return; // 陈旧曲目的失败：不污染当前错误态
                }
                self.error = Some(message);
                self.playing = false;
                self.paused = false;
            }
        }
    }

    /// 刷新音乐库（重扫 + 按艺术家分组 + 后台探测时长）；当前播放曲目若仍在列保持
    /// `current`/`queue` 按路径重定位（重扫后下标可能漂移）。不打断播放。
    pub fn load_dir(&mut self, dir: impl AsRef<Path>, cx: &mut Context<Self>) {
        let dir = dir.as_ref().to_path_buf();
        // 先按路径留底，再整体替换（重扫后下标漂移，用路径重定位）
        let old_paths: Vec<PathBuf> = self.library.iter().map(|t| t.path.clone()).collect();
        self.library = scan_dir(&dir);
        self.mix_groups = build_mix_groups(&self.library);
        self.dir = Some(dir);
        let reidx = |i: usize, lib: &[Track]| {
            old_paths
                .get(i)
                .and_then(|p| lib.iter().position(|t| t.path == *p))
        };
        if let Some(cur) = self.current {
            self.current =
                reidx(cur, &self.library).or_else(|| (!self.library.is_empty()).then_some(0));
        } else {
            self.current = None;
        }
        let mut new_queue = Vec::new();
        for i in self.queue.drain(..) {
            if let Some(n) = reidx(i, &self.library) {
                new_queue.push(n);
            }
        }
        self.queue = new_queue;
        self.queue_pos = match self.current {
            Some(cur) => self.queue.iter().position(|&i| i == cur).unwrap_or(0),
            None => 0,
        };
        self.error = None;
        cx.notify();
        self.probe_durations(cx);
    }

    /// 后台逐个探测曲目时长（open+params+drop），完成一个即回主线程更新该 track 的
    /// `duration` 并 notify；失败/无 dll 保持 0。采样率沿用 sink（未开则 44_100，
    /// duration 与速率无关）。
    fn probe_durations(&mut self, cx: &mut Context<Self>) {
        let targets: Vec<(usize, PathBuf)> = self
            .library
            .iter()
            .enumerate()
            .filter(|(_, t)| t.duration <= 0.0)
            .map(|(i, t)| (i, t.path.clone()))
            .collect();
        if targets.is_empty() {
            return;
        }
        let rate = self.sink.as_ref().map(|s| s.sample_rate()).unwrap_or(44_100);
        let weak = cx.weak_entity();
        cx.spawn(async move |_, cx| {
            for (i, path) in targets {
                let d = cx
                    .background_executor()
                    .spawn(async move { probe_duration(&path, rate).unwrap_or(0.0) })
                    .await;
                let _ = weak.update(cx, |this, cx| {
                    if let Some(t) = this.library.get_mut(i) {
                        t.duration = d;
                        cx.notify();
                    }
                });
            }
        })
        .detach();
    }

    /// 播放 `library[idx]`：queue 空则先建全库队列（shuffle 洗牌）；`idx` 不在队列则
    /// 追加；sink 首次打开（失败 → error 提示，纯状态播放）；发 `Load`、置状态、notify。
    pub fn play_track(&mut self, idx: usize, cx: &mut Context<Self>) {
        if idx >= self.library.len() {
            return;
        }
        if self.queue.is_empty() {
            let mut q: Vec<usize> = (0..self.library.len()).collect();
            if self.shuffle {
                shuffle_in_place(&mut q);
            }
            self.queue = q;
        }
        if let Some(p) = self.queue.iter().position(|&x| x == idx) {
            self.queue_pos = p;
        } else {
            self.queue.push(idx);
            self.queue_pos = self.queue.len() - 1;
        }
        self.current = Some(idx);
        self.error = None;
        self.duration = 0.0;
        self.paused = false;
        self.playing = true;
        let path = self.library[idx].path.clone();
        let rate = match &self.sink {
            Some(s) => s.sample_rate(),
            None => match AudioSink::open() {
                Ok(s) => {
                    self.spawn_worker(&s);
                    let rate = s.sample_rate();
                    self.sink = Some(s);
                    rate
                }
                Err(e) => {
                    self.error = Some(format!("音频设备不可用：{e}"));
                    cx.notify();
                    return; // 纯状态播放（无输出；position 恒 0）
                }
            },
        };
        self.generation += 1;
        let gen_id = self.generation;
        if let Some(tx) = &self.worker_tx {
            let _ = tx.send(WorkerCmd::Load { gen_id, path, rate });
        }
        cx.notify();
    }

    /// 启动 worker：首个 sink 建立时启动一次（复用跨曲目）；线程拉起失败则静默
    /// （无 worker 则命令丢失，退纯状态播放）。
    fn spawn_worker(&mut self, sink: &AudioSink) {
        if self.worker_tx.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel::<WorkerCmd>();
        let (evt_tx, evt_rx) = mpsc::channel::<Event>();
        let s = sink.clone();
        let join = std::thread::Builder::new()
            .name("music-player-worker".to_string())
            .spawn(move || worker_main(rx, evt_tx, s))
            .ok();
        // worker 持有新事件通道的写端：交接后主线程读端换新，旧读端丢弃
        self.worker_tx = Some(tx);
        self.evt_rx = evt_rx;
        self.worker_join = join;
    }

    pub fn toggle(&mut self, cx: &mut Context<Self>) {
        if self.paused {
            self.resume(cx);
        } else if self.playing {
            self.pause(cx);
        } else if let Some(cur) = self.current {
            self.play_track(cur, cx);
        } else if !self.library.is_empty() {
            self.play_track(0, cx);
        }
    }

    pub fn pause(&mut self, cx: &mut Context<Self>) {
        if self.paused {
            return;
        }
        self.paused = true;
        self.playing = false;
        if let Some(tx) = &self.worker_tx {
            let _ = tx.send(WorkerCmd::Pause);
        }
        cx.notify();
    }

    pub fn resume(&mut self, cx: &mut Context<Self>) {
        if !self.paused {
            return;
        }
        self.paused = false;
        self.playing = true;
        if let Some(tx) = &self.worker_tx {
            let _ = tx.send(WorkerCmd::Resume);
        }
        cx.notify();
    }

    pub fn next(&mut self, cx: &mut Context<Self>) {
        if self.queue.is_empty() && !self.library.is_empty() {
            return self.play_track(0, cx);
        }
        let Some(n) = next_index(self.queue.len(), self.queue_pos, self.repeat, self.shuffle) else {
            self.pause(cx);
            return;
        };
        self.play_queue_pos(n, cx);
    }

    pub fn prev(&mut self, cx: &mut Context<Self>) {
        if self.queue.is_empty() && !self.library.is_empty() {
            return self.play_track(0, cx);
        }
        let Some(n) = prev_index(self.queue.len(), self.queue_pos, self.repeat) else {
            return;
        };
        self.play_queue_pos(n, cx);
    }

    fn play_queue_pos(&mut self, pos: usize, cx: &mut Context<Self>) {
        let Some(&idx) = self.queue.get(pos) else {
            return;
        };
        self.queue_pos = pos;
        self.play_track(idx, cx);
    }

    /// 播放指定路径的音频文件（shell 集成入口）：先在当前 library 里按路径找下标；
    /// 未找到（如路径在配置目录之外）→ 以该文件**父目录**作该次库重扫，再按路径播放。
    /// 复用 `load_dir`/`play_track`，不重复建队列逻辑。
    pub fn play_path(&mut self, path: impl AsRef<Path>, cx: &mut Context<Self>) {
        let path = path.as_ref().to_path_buf();
        if let Some(idx) = self.library.iter().position(|t| t.path == path) {
            return self.play_track(idx, cx);
        }
        // 路径不在当前库：以文件父目录重扫（父目录相同但文件新加入也会重扫到）。
        if let Some(parent) = path.parent() {
            self.load_dir(parent, cx);
        }
        let Some(idx) = self.library.iter().position(|t| t.path == path) else {
            self.error = Some(format!("音频不在库中：{}", path.display()));
            cx.notify();
            return;
        };
        self.play_track(idx, cx);
    }

    /// seek 到 `seconds`（clamp 到 [0, 探测时长]）；flush 在 worker 内完成。
    pub fn seek(&mut self, seconds: f64, cx: &mut Context<Self>) {
        let max = if self.duration > 0.0 {
            self.duration
        } else {
            f64::INFINITY
        };
        let t = seconds.max(0.0).min(max);
        let gen_id = self.generation;
        if let Some(tx) = &self.worker_tx {
            let _ = tx.send(WorkerCmd::Seek { gen_id, seconds: t });
        }
        cx.notify();
    }

    pub fn set_volume(&mut self, volume: f32, cx: &mut Context<Self>) {
        self.volume = volume.clamp(0.0, 1.0);
        if let Some(s) = &self.sink {
            s.set_volume(self.volume);
        }
        cx.notify();
    }

    pub fn set_repeat(&mut self, repeat: RepeatMode, cx: &mut Context<Self>) {
        self.repeat = repeat;
        cx.notify();
    }

    /// 切换 shuffle：只改开关，下次建队列才生效。
    pub fn toggle_shuffle(&mut self, cx: &mut Context<Self>) {
        self.shuffle = !self.shuffle;
        cx.notify();
    }

    /// 收藏/取消收藏 `library[idx]`（按路径字符串）。
    pub fn toggle_like(&mut self, idx: usize, cx: &mut Context<Self>) {
        let Some(t) = self.library.get(idx) else {
            return;
        };
        let p = t.path.to_string_lossy().to_string();
        if !self.liked.remove(&p) {
            self.liked.insert(p);
        }
        cx.notify();
    }

    pub fn is_liked(&self, idx: usize) -> bool {
        self.library
            .get(idx)
            .map(|t| self.liked.contains(&t.path.to_string_lossy().to_string()))
            .unwrap_or(false)
    }

    /// 停止：关输出、复位播放状态（保留 library/dir/liked）。
    pub fn stop(&mut self, cx: &mut Context<Self>) {
        if let Some(tx) = &self.worker_tx {
            let _ = tx.send(WorkerCmd::Stop);
        }
        if let Some(s) = &self.sink {
            let _ = s.pause();
            s.flush();
        }
        self.playing = false;
        self.paused = false;
        self.queue.clear();
        self.queue_pos = 0;
        self.current = None;
        self.duration = 0.0;
        cx.notify();
    }

    /// 播放位置（秒）：sink 音频时钟（paused 时 worker 已暂停 sink → 天然冻结）；无 sink 0。
    pub fn position(&self) -> f64 {
        self.sink.as_ref().map(|s| s.position()).unwrap_or(0.0)
    }

    // ---- 只读访问器（Phase B UI 用） ----
    pub fn current(&self) -> Option<usize> {
        self.current
    }
    pub fn current_track(&self) -> Option<&Track> {
        self.current.and_then(|i| self.library.get(i))
    }
    pub fn playing(&self) -> bool {
        self.playing
    }
    pub fn paused(&self) -> bool {
        self.paused
    }
    pub fn library(&self) -> &[Track] {
        &self.library
    }
    pub fn mix_groups(&self) -> &[MixGroup] {
        &self.mix_groups
    }
    pub fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
    pub fn duration(&self) -> f64 {
        self.duration
    }
    pub fn volume(&self) -> f32 {
        self.volume
    }
    pub fn repeat(&self) -> RepeatMode {
        self.repeat
    }
    pub fn shuffle(&self) -> bool {
        self.shuffle
    }
    pub fn queue(&self) -> &[usize] {
        &self.queue
    }
    pub fn queue_pos(&self) -> usize {
        self.queue_pos
    }
    pub fn liked_paths(&self) -> &HashSet<String> {
        &self.liked
    }

    /// 播完自动推进：按 repeat 决定下一首；`Off`（含 shuffle）队尾 → 停（不再自动播）。
    fn auto_advance(&mut self, cx: &mut Context<Self>) {
        if self.queue.is_empty() {
            if !self.library.is_empty() {
                return self.play_track(0, cx);
            }
            self.playing = false;
            return;
        }
        match advance_on_finish(self.queue.len(), self.queue_pos, self.repeat) {
            Some(pos) => self.play_queue_pos(pos, cx),
            None => {
                // 队尾 + Off（含 shuffle）：停在尾，不自动往下走
                self.playing = false;
                self.paused = false;
            }
        }
    }
}

impl Drop for MusicPlayer {
    fn drop(&mut self) {
        // 停 worker：发 Shutdown + join（确保 cpal stream 与 dll 句柄干净释放）
        if let Some(tx) = &self.worker_tx {
            let _ = tx.send(WorkerCmd::Shutdown);
        }
        if let Some(join) = self.worker_join.take() {
            let _ = join.join();
        }
    }
}

impl Render for MusicPlayer {
    /// Phase A 不渲染（Phase B 换成真 UI）。
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        gpui::div()
    }
}

// ---------------- 纯逻辑（可单测）：队列导航 ----------------

/// 洗牌（原地 Fisher-Yates + 自带的 xorshift64 确定性伪随机，无外部依赖）。
/// 洗牌序的「随机性」在队列构建时入队，导航阶段不再重掷（可复现、可单测）。
fn shuffle_in_place(q: &mut [usize]) {
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    for i in (1..q.len()).rev() {
        // xorshift64
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let j = (x % (i as u64 + 1)) as usize;
        q.swap(i, j);
    }
}

/// 队列内「下一首」下标（纯函数，可单测）：
/// - 空队列 → `None`；
/// - `One` → 重播当前位（越界钳到末位）；
/// - `Off + 非 shuffle` → 顺序下一首，队尾后停（`None`）；
/// - `All` 或 `Off + shuffle` → 循环回 0（**手动 next 的「洗牌点歌机」语义**：随机序
///   已在队列构建时入队，导航阶段不再重掷）。
/// 注意与 [`advance_on_finish`]（EOF 自动推进，`Off` 一律队尾停）语义刻意不同。
pub fn next_index(
    queue_len: usize,
    pos: usize,
    repeat: RepeatMode,
    shuffle: bool,
) -> Option<usize> {
    if queue_len == 0 {
        return None;
    }
    let pos = pos.min(queue_len - 1);
    match repeat {
        RepeatMode::One => Some(pos),
        RepeatMode::Off if !shuffle => {
            let next = pos + 1;
            (next < queue_len).then_some(next)
        }
        RepeatMode::All | RepeatMode::Off => Some((pos + 1) % queue_len),
    }
}

/// 播完自动推进的下一位（纯函数，可单测）：`Off` **一律队尾停**（不循环，含 shuffle——
/// 与 [`RepeatMode::Off`] 在 model.rs 的文档「不循环（到队尾停）」一致）；`All` 回绕 0；
/// `One` 重播当前。与 [`next_index`]（手动 next 的 Off+shuffle 回绕点歌机语义）不同。
pub fn advance_on_finish(
    queue_len: usize,
    pos: usize,
    repeat: RepeatMode,
) -> Option<usize> {
    if queue_len == 0 {
        return None;
    }
    let pos = pos.min(queue_len - 1);
    match repeat {
        RepeatMode::One => Some(pos),
        RepeatMode::Off => (pos + 1 < queue_len).then_some(pos + 1),
        RepeatMode::All => Some((pos + 1) % queue_len),
    }
}

/// 队列内「上一首」下标（纯函数）：`One` → 重播当前；其余回走（队首回绕到末）。
pub fn prev_index(queue_len: usize, pos: usize, repeat: RepeatMode) -> Option<usize> {
    if queue_len == 0 {
        return None;
    }
    let pos = pos.min(queue_len - 1);
    match repeat {
        RepeatMode::One => Some(pos),
        RepeatMode::Off | RepeatMode::All => Some((pos + queue_len - 1) % queue_len),
    }
}

/// worker 运行上下文（命令分发两侧共用：主轮询 + 回压等待）。
struct WorkerCtx {
    buf: Vec<f32>,
    codec: Option<decode::AudioCodec>,
    active: bool,
    /// 当前 Load 代序号：Opened/Finished/Error 事件都带它，主线程据此过滤陈旧事件。
    cur_gen: u64,
    /// 待落地 Load 目标（命令里攒下，处理完成统一落地，避免在 match 分支重入）。
    load: Option<(u64, PathBuf, u32)>,
}

/// 处理单条命令：更新 ctx。返回 `true` = 退出 worker（Shutdown）。
/// 主轮询与回压等待共用，保证 Seek/Pause/Stop/Load/Shutdown 在任何时机都能被及时处理。
fn dispatch(
    cmd: WorkerCmd,
    ctx: &mut WorkerCtx,
    sink: &AudioSink,
    evt_tx: &mpsc::Sender<Event>,
) -> bool {
    match cmd {
        WorkerCmd::Shutdown => return true,
        WorkerCmd::Stop => {
            ctx.codec = None;
            ctx.active = false;
            ctx.buf.clear();
            let _ = sink.pause();
            sink.flush();
        }
        WorkerCmd::Pause => {
            let _ = sink.pause();
        }
        WorkerCmd::Resume => {
            let _ = sink.play();
        }
        WorkerCmd::Seek { seconds, .. } => {
            if let Some(c) = &mut ctx.codec {
                if c.seek(seconds).is_ok() {
                    sink.flush();
                } else {
                    let _ = evt_tx.send(Event::Error {
                        gen_id: ctx.cur_gen,
                        message: "音频 seek 失败".to_string(),
                    });
                }
            }
        }
        WorkerCmd::Load { gen_id, path, rate } => {
            ctx.load = Some((gen_id, path, rate));
        }
    }
    false
}

/// 落地挂起的 Load（关旧 codec 开新、flush、Opened 事件、起播）。
fn apply_load(ctx: &mut WorkerCtx, sink: &AudioSink, evt_tx: &mpsc::Sender<Event>) {
    let Some((gen_id, path, rate)) = ctx.load.take() else {
        return;
    };
    ctx.cur_gen = gen_id;
    ctx.codec = None;
    ctx.active = false;
    ctx.buf.clear();
    sink.flush();
    match decode::open_audio(&path, rate) {
        Ok(c) => {
            let duration = c.params().duration;
            ctx.codec = Some(c);
            ctx.active = true;
            let _ = sink.play();
            let _ = evt_tx.send(Event::Opened { gen_id, duration });
        }
        Err(e) => {
            let _ = evt_tx.send(Event::Error {
                gen_id,
                message: format!("打开音频失败: {e}"),
            });
        }
    }
}

/// worker 主循环：单线程串行处理命令 + 流式解码推送。
/// - `Load`：关旧 codec 开新（open+params+Opened 事件），进入流式解码；
/// - 流式推进前**阻塞回压**：队列积压 >1s 样本时原地等待（每 5ms 复查 + 顺带抓命令），
///   直到降到阈值下才推下一块。解码比实时快约 9 倍，旧实现「睡 5ms 照推」会把队列打满
///   `QUEUE_MAX_SAMPLES`，`sink.push` 被迫从**队头（即将播放处）丢最旧样本** → 内容前跳
///   （快进）+ 硬切杂音（视频 audio_worker 同款阻塞姿势，见 workers.rs audio_worker）。
/// - EOF → `Finished` 事件（主线程按 repeat/shuffle 决策）；
/// - `Shutdown`/通道断开 → 退出（Drop 时 `join` 保证 dll/cpal 干净释放）。
fn worker_main(rx: mpsc::Receiver<WorkerCmd>, evt_tx: mpsc::Sender<Event>, sink: AudioSink) {
    let mut ctx = WorkerCtx {
        buf: Vec::new(),
        codec: None,
        active: false,
        cur_gen: 0,
        load: None,
    };
    let rate = sink.sample_rate().max(1);
    // 回压阈值：≈1 秒立体声样本。
    let backpressure = rate as usize * 2;

    loop {
        // 取待处理命令：活跃期非阻塞（回压等待段会再抓），空闲期阻塞等
        let cmd = if ctx.active {
            rx.try_recv().ok()
        } else {
            match rx.recv() {
                Ok(c) => Some(c),
                Err(_) => return,
            }
        };
        if let Some(cmd) = cmd
            && dispatch(cmd, &mut ctx, &sink, &evt_tx)
        {
            return;
        }
        // Load 落地（换代：关旧开新 + flush + 起播）
        apply_load(&mut ctx, &sink, &evt_tx);
        if !ctx.active {
            continue;
        }

        // 阻塞回压：队列积压 >1s → 等队列排到阈值下再推下一块（对齐视频 audio_worker）。
        // 等待期间每 5ms 抓一次命令，Seek/Pause/Stop/Load 仍及时；来了就就地处理并回主
        // 循环（不在等待块里深埋逻辑）。这是「音乐正常速度/无杂音」的核心：不守这条，
        // 解码（≈9×实时）会把队列打满上限，push 从队头丢最旧样本 = 快进 + 硬切杂音。
        if sink.queued_len() > backpressure {
            loop {
                if let Ok(cmd) = rx.try_recv()
                    && dispatch(cmd, &mut ctx, &sink, &evt_tx)
                {
                    return;
                }
                apply_load(&mut ctx, &sink, &evt_tx);
                if sink.queued_len() <= backpressure {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            if !ctx.active {
                continue;
            }
        }

        // 解码推进
        match ctx.codec.as_mut().expect("active 必有 codec").next_pcm(&mut ctx.buf) {
            Ok(Some(_pts)) => sink.push(&ctx.buf),
            Ok(None) => {
                // EOF：关 codec，发 Finished（自动下一首由主线程决策）
                ctx.codec = None;
                ctx.active = false;
                ctx.buf.clear();
                let _ = evt_tx.send(Event::Finished { gen_id: ctx.cur_gen });
            }
            Err(e) => {
                ctx.codec = None;
                ctx.active = false;
                ctx.buf.clear();
                let _ = evt_tx.send(Event::Error {
                    gen_id: ctx.cur_gen,
                    message: format!("解码失败: {e}"),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cpal::traits::HostTrait as _;
    use gpui::AppContext as _;
    use gpui::TestAppContext;

    #[test]
    fn next_index_cases() {
        // 空队列
        assert_eq!(next_index(0, 0, RepeatMode::Off, false), None);
        // Off：顺序推进，队尾后停
        assert_eq!(next_index(3, 1, RepeatMode::Off, false), Some(2));
        assert_eq!(next_index(3, 2, RepeatMode::Off, false), None);
        // Off + shuffle：循环（随机性在队列构建时入队）
        assert_eq!(next_index(3, 2, RepeatMode::Off, true), Some(0));
        assert_eq!(next_index(1, 0, RepeatMode::Off, true), Some(0));
        // All：循环
        assert_eq!(next_index(3, 2, RepeatMode::All, false), Some(0));
        assert_eq!(next_index(1, 0, RepeatMode::All, false), Some(0));
        // One：重播当前（越界钳位）
        assert_eq!(next_index(3, 1, RepeatMode::One, false), Some(1));
        assert_eq!(next_index(3, 99, RepeatMode::One, false), Some(2));
    }

    #[test]
    fn prev_index_cases() {
        assert_eq!(prev_index(0, 0, RepeatMode::Off), None);
        assert_eq!(prev_index(3, 0, RepeatMode::Off), Some(2)); // 回绕到末
        assert_eq!(prev_index(3, 1, RepeatMode::Off), Some(0));
        assert_eq!(prev_index(3, 1, RepeatMode::One), Some(1)); // 重播
        assert_eq!(prev_index(3, 0, RepeatMode::All), Some(2));
    }

    /// EOF 自动推进回归（K 语义：`Off` 必须队尾停，含 shuffle——与手动 next 的
    /// 「Off+shuffle 回绕点歌机」语义不同）：此前 `auto_advance` 复用 `next_index`，
    /// 在 `Off+shuffle` 下会回绕 0 无限续播，违背 model.rs「Off = 不循环（到队尾停）」。
    #[test]
    fn advance_on_finish_off_stops_at_tail() {
        // 空队列
        assert_eq!(advance_on_finish(0, 0, RepeatMode::Off), None);
        // Off：顺序推进，队尾一定停（shuffle 与否此处无参——固定停）
        assert_eq!(advance_on_finish(3, 1, RepeatMode::Off), Some(2));
        assert_eq!(advance_on_finish(3, 2, RepeatMode::Off), None);
        assert_eq!(advance_on_finish(1, 0, RepeatMode::Off), None);
        // All：回绕 0
        assert_eq!(advance_on_finish(3, 2, RepeatMode::All), Some(0));
        assert_eq!(advance_on_finish(1, 0, RepeatMode::All), Some(0));
        // One：重播当前（越界钳位）
        assert_eq!(advance_on_finish(3, 1, RepeatMode::One), Some(1));
        assert_eq!(advance_on_finish(3, 99, RepeatMode::One), Some(2));
    }

    #[test]
    fn shuffle_in_place_preserves_multiset() {
        let mut q: Vec<usize> = (0..20).collect();
        let orig = q.clone();
        shuffle_in_place(&mut q);
        let mut s = q.clone();
        s.sort();
        assert_eq!(s, orig, "洗牌只重排、不丢不重元素");
        // 20 元素洗牌结果几乎必变序（确定性伪随机也几乎必变）
        assert_ne!(q, orig);
    }

    /// 手写 1.5s 双声道 16bit 44.1k WAV（低音量正弦）——scan 不读内容，播放走真解码。
    fn write_test_wav(path: &Path, seconds: f64) {
        const RATE: u32 = 44_100;
        const CH: usize = 2;
        let total = (RATE as f64 * seconds * CH as f64).round() as usize;
        let mut data = Vec::with_capacity(total * 2);
        for i in 0..total / CH {
            let t = i as f64 / RATE as f64;
            let v = (t * 440.0 * std::f64::consts::TAU).sin() * 0.12;
            let s = (v * i16::MAX as f64) as i16;
            for _ in 0..CH {
                data.extend_from_slice(&s.to_le_bytes());
            }
        }
        let mut wav = Vec::new();
        let data_len = data.len() as u32;
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data_len).to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&(CH as u16).to_le_bytes());
        wav.extend_from_slice(&RATE.to_le_bytes());
        wav.extend_from_slice(&(RATE * CH as u32 * 2).to_le_bytes());
        wav.extend_from_slice(&(CH as u16 * 2).to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_len.to_le_bytes());
        wav.extend_from_slice(&data);
        std::fs::write(path, wav).unwrap();
    }

    /// 集成（真 dll + 真设备；缺任一 → SKIP）：load_dir → 播第 1 首 → position>0 &&
    /// playing；pause 后 position 冻结；next 切到第 2 首。headless 只证机制（K30），
    /// 真机人工为准（position 不推进则 SKIP 位置相关断言后返回）。
    #[gpui::test]
    async fn integration_plays_pause_freezes_next(cx: &mut TestAppContext) {
        if !crate::decode::bridge_ok() {
            eprintln!("SKIP: media_ffmpeg.dll 不可用（缺视频/音乐运行库）");
            return;
        }
        if cpal::default_host().default_output_device().is_none() {
            eprintln!("SKIP: 无默认音频输出设备");
            return;
        }
        let dir = std::env::temp_dir().join(format!("gpui-music-play-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        write_test_wav(&dir.join("第一首 - 曲目一.wav"), 1.5);
        write_test_wav(&dir.join("第二首 - 曲目二.wav"), 1.5);

        let player: gpui::Entity<MusicPlayer> = cx.update(|app| app.new(|cx| MusicPlayer::new(cx)));
        cx.update(|app| {
            player.update(app, |p, cx| p.load_dir(&dir, cx));
        });
        cx.update(|app| {
            player.update(app, |p, cx| p.play_track(0, cx));
        });

        // 等 Opened（tick 驱动事件入队）：current=0、duration>0、playing
        let mut guard = 0;
        let s = loop {
            cx.update(|app| {
                player.update(app, |p, cx| p.tick(cx));
            });
            let s = cx.update(|app| {
                let p = player.read(app);
                (p.current, p.duration, p.playing)
            });
            if s.0 == Some(0) && s.1 > 0.0 && s.2 {
                break s;
            }
            guard += 1;
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(5));
            assert!(guard < 800, "等待 Opened 超时: {s:?}");
        };
        assert_eq!(s.0, Some(0), "Opened 后 current 应为 0");
        assert!(s.1 > 0.0, "Opened 后应带出真实时长");
        assert!(s.2, "Opened 后应 playing");

        // 等 position 推进（真实 cpal 消费；headless 设备可能不回传——给 ~1.5s 限）
        guard = 0;
        let pos = loop {
            let p = cx.update(|app| player.read(app).position());
            if p > 0.0 {
                break p;
            }
            cx.update(|app| {
                player.update(app, |p, cx| p.tick(cx));
            });
            guard += 1;
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(10));
            if guard > 150 {
                eprintln!("SKIP: position 未推进（headless 设备无消费），位置相关断言跳过；机制以人工真机为准");
                return;
            }
        };
        assert!(pos > 0.0, "position 应 >0");

        // 回压回归（生产者丢样本）：播放若干拍后 push 不得从队头丢最旧样本（否则 = 内容
        // 前跳/快进 + 硬切杂音）。旧 worker「睡 5ms 照推」会把队列打满上限导致 dropped>
        // 0；阻塞式回压后应恒 0。
        let st = cx.update(|app| {
            player.read(app).sink.as_ref().map(|s| s.stats())
        });
        if let Some((_calls, _underrun, _lockfail, _played, dropped)) = st {
            assert_eq!(
                dropped, 0,
                "播放中不得从队头丢样本（回压失效 → 快进+杂音）: stats={st:?}"
            );
        }

        // pause：position 冻结（worker 已暂停 sink → 播放时钟静止）
        cx.update(|app| {
            player.update(app, |p, cx| p.pause(cx));
        });
        std::thread::sleep(Duration::from_millis(250));
        let frozen = cx.update(|app| player.read(app).position());
        std::thread::sleep(Duration::from_millis(250));
        let frozen2 = cx.update(|app| player.read(app).position());
        assert!(
            (frozen2 - frozen).abs() < 0.05,
            "pause 后 position 应冻结: {frozen} -> {frozen2}"
        );

        // next：切到第 2 首（current=1）
        cx.update(|app| {
            player.update(app, |p, cx| p.next(cx));
        });
        guard = 0;
        let s2 = loop {
            cx.update(|app| {
                player.update(app, |p, cx| p.tick(cx));
            });
            let cur = cx.update(|app| player.read(app).current);
            if cur == Some(1) {
                break cur;
            }
            guard += 1;
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(5));
            assert!(guard < 800, "等待 next 切曲超时: current={cur:?}");
        };
        assert_eq!(s2, Some(1), "next 后 current 应切到第 2 首");

        let _ = std::fs::remove_dir_all(&dir);
    }
}

