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

use std::cell::Cell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

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

/// 播放中周期持久化间隔（秒）：进度条不逐帧写盘——`MusicView::on_tick`（250ms 心跳）下
/// 按此粒度合并，禁止逐帧写（K11）。暂停/停止/seek/切歌等离散事件仍即时触发持久化。
const PREFS_PERSIST_INTERVAL: Duration = Duration::from_secs(5);

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
    /// 收藏路径的「新→旧」顺序记录（与 `liked` 镜像；写盘用——配置字段文档即「新→旧」）。
    liked_order: Vec<String>,
    /// 上次播放/选中曲目绝对路径（配置字段 `music_last_track` 的内存镜像；`play_track` 更新）。
    last_track_pref: Option<String>,
    /// 上次播放位置（秒；配置字段 `music_progress_secs` 的内存镜像；pause/stop/seek/周期更新）。
    progress_pref: Option<f64>,
    /// 持久化脏标记：volume / liked / last_track / progress 任一变化即置真；
    /// [`Self::flush_prefs`] 成功后清假（K10：一处脏标记 + 统一 flush，禁散点直调丢状态）。
    prefs_dirty: bool,
    /// 持久化 flush 计数（测试探针：断言「该发生落盘的事件触发了 flush」，对齐 shell 的
    /// `search_scan_count` 先例）。music 自身测试构建（`cargo test -p music`）下写盘被屏蔽
    /// （见 [`Self::flush_prefs`]），此计数仍记录「应写盘」次数。
    prefs_flush_count: u64,
    /// 上次周期持久化检查时刻（[`PREFS_PERSIST_INTERVAL`] 粒度；播放中不逐帧写）。
    last_prefs_attempt: Option<Instant>,
    /// 打开音乐模式待恢复的上次状态 `(last_track 路径, 进度秒)`：由 [`Self::apply_saved_prefs`]
    /// 置入，首个 [`Self::commit_scan`] 库就绪后消费恢复选中（不自动播放）。
    pending_restore: Option<(Option<String>, Option<f64>)>,
    /// 恢复已执行标记：只在打开音乐模式**首次**装配时恢复一次，之后的 `load_dir` 换代
    /// （换目录 / 播放外部文件重扫）**不**重复恢复——防换目录后被旧 prefs 拉回丢显式意图。
    restore_done: bool,
    /// 恢复选中的曲目 `(下标, 上次进度)`：仅当用户对该曲**显式播放**（`play_track` 且
    /// `idx == current`、进度 > 0）时才 `seek` 续播；对其它曲目从头播。库换代即清（下标已漂移）。
    resume_restore: Option<(usize, f64)>,
    /// 待 `Opened` 后异步 seek 的续播落点（秒）：worker 在无 codec 时收到 `Seek` 是空操作，
    /// Load 后 codec 从 0 起播 → 续播必须等 `Opened`（时长就绪）再真 seek（seek 内 clamp）。
    resume_on_open: Option<f64>,
    /// Load 代序号：每次 `play_track` 递增，worker 回送 Opened.gen_id 与之匹配过滤陈旧。
    generation: u64,
    /// 显示基准（秒）：最近一次 seek / 拖拽预览的目标。sink.flush 会归零播放时钟，
    /// 用它 + 时钟增量合成 `position()`，seek 后显示从落点继续走、不再从 0 重走。
    seek_base: f64,
    /// 显示基准锚点：记录 `seek_base` 设定时刻的 sink 时钟（即 seek/预览后、worker
    /// flush 落地前的钟值）。增量合成规则见 [`Self::position`]。
    seek_clock_base: f64,
    /// 拖拽轨道：true 时 `position()` 钉在 `seek_base`（手指位置，对齐视频 scrub
    /// 预览）；松手收尾（`set_scrubbing(false)`）后解锁、恢复由 sink 时钟推进。
    scrubbing: bool,
    /// 已观察到 worker 把 sink 时钟归零（seek flush / 新曲 Load / Stop）——`position()`
    /// 是只读路径，用 `Cell` 在此吸收「时钟回跳」：一旦观察到 `clock < 锚点` 即判定
    /// flush 已落地、此后增量直接取 `clock`（否则 flush 后时钟从 0 重走仍照旧锚点
    /// 起算，会在 seek 落点上方「冻结满 C 秒再突然跃升」）。
    flush_seen: Cell<bool>,
    /// 库/收藏变更计数器：`load_dir`（整体换库、下标漂移）与 `toggle_like`（收藏集变化）
    /// 递增。主区列表渲染按它做「过滤/分组显示模型缓存」失效键——搜索词不变且本计数不变
    /// 时复用缓存，避免每帧重过滤 + 深克隆 `Track`（性能优化第二步）。
    mutation: u64,
    /// 异步扫库装配态：`load_dir` 已把目录遍历/读 .lrc/找封面移出主线程（后台执行），完成前
    /// 此旗置真——UI 显示「正在扫描」空态；完成后 [`Self::commit_scan`] 一次性提交库并置假。
    scanning: bool,
    /// 扫库代次：每次 `load_dir` 递增，异步结果在 `commit_scan` 前比对——换目录/重扫时旧任务
    /// 结果作废（防竞态覆盖新库）。时长探测批量任务也记录它，scan 换代即丢弃（下标已过期）。
    scan_gen: u64,
    /// `play_path` 待播路径：目标不在当前库时以父目录异步重扫，扫完在 `commit_scan` 中按路径
    /// 重定位并播放（保持「播放外部文件」语义不被异步化打断）。
    pending_path: Option<PathBuf>,
    /// 本次会话播放历史（仅内存、不持久化）：会话内「真正开始播放/切换曲目」的曲目路径，
    /// 最近先、同一曲去重后置顶。`load_dir`（首次打开/换库重扫）时清空——不跨库/跨会话保留。
    recent: Vec<String>,
}

impl MusicPlayer {
    /// 新建播放器：事件通道读端就位（worker 在首次开 sink 时才启动并换入新通道），
    /// 事件经 `tick` 消费（见模块文档驱动模型）。启动时读一次偏好：把 `music_liked`
    /// 接线进收藏集、`music_volume` 应用到初始音量（任一打开路径都恢复；上次曲目/进度的
    /// 恢复选中需库就绪，由打开音乐模式时的 `apply_saved_prefs` + 首个 `commit_scan` 完成）。
    /// K8：music 自身测试构建不读真实配置，见 [`Self::prefs`]。
    pub fn new(_cx: &mut Context<Self>) -> Self {
        let (_evt_tx, evt_rx) = mpsc::channel::<Event>();
        let prefs = Self::prefs();
        let liked_prefs = prefs.music_liked;
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
            volume: prefs.music_volume.unwrap_or(1.0).clamp(0.0, 1.0),
            sink: None,
            worker_tx: None,
            worker_join: None,
            evt_rx,
            error: None,
            duration: 0.0,
            liked: liked_prefs.iter().cloned().collect(),
            liked_order: liked_prefs,
            last_track_pref: None,
            progress_pref: None,
            prefs_dirty: false,
            prefs_flush_count: 0,
            last_prefs_attempt: None,
            pending_restore: None,
            restore_done: false,
            resume_restore: None,
            resume_on_open: None,
            generation: 0,
            seek_base: 0.0,
            seek_clock_base: 0.0,
            scrubbing: false,
            flush_seen: Cell::new(false),
            mutation: 0,
            scanning: false,
            scan_gen: 0,
            pending_path: None,
            recent: Vec::new(),
        }
    }

    /// 读用户偏好：music 自身测试构建（`cargo test -p music`）下 `config` 是依赖编译、
    /// 不受其自身 `cfg!(test)` 门控 → 这里按本 crate 测试态屏蔽，防读真实
    /// `%APPDATA%\gpui-editor\config.json` 污染测试（K8）。
    #[cfg(test)]
    fn prefs() -> config::user::UserPrefs {
        config::user::UserPrefs::default()
    }
    #[cfg(not(test))]
    fn prefs() -> config::user::UserPrefs {
        config::user::UserPrefs::load()
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
                // 新曲目从 0 走：显示基准复位（seek 基准由 Opened 前 play_track 已复位，
                // 此处兜底以防任何路径漏过）。
                self.reset_playhead_base();
                self.playing = true;
                self.paused = false;
                // 续播（恢复选中的那首）：worker 此时 codec 就绪，再真 seek(saved) 才生效
                // ——worker 无 codec 时 Seek 是空操作，Load 后 codec 从 0 起播。seek 内 clamp
                // 到真实 duration（恢复时 duration 未探测，落到 Opened 才知其上限）。
                if let Some(saved) = self.resume_on_open.take() {
                    self.seek(saved, cx);
                }
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

    /// 刷新音乐库（异步重扫 + 按艺术家分组 + 后台批量探测时长）。**目录遍历 + 逐曲读 .lrc +
    /// 找封面全部移出主线程**（后台 executor `scan_dir`）：本方法只置装配态并派任务返回，主线程
    /// 不再同步空等（大目录启动「黑 gap」消除）；扫描完成 [`Self::commit_scan`] 一次性提交库 +
    /// **一次 notify** + 触发批量时长探测。当前播放曲目若仍在列保持 `current`/`queue` 按路径
    /// 重定位（重扫后下标可能漂移）。不打断播放。换目录重扫由 `scan_gen` 作废旧结果（防竞态）。
    pub fn load_dir(&mut self, dir: impl AsRef<Path>, cx: &mut Context<Self>) {
        let dir = dir.as_ref().to_path_buf();
        // 换库/首次装载 = 新会话起点：本次会话播放历史清空（最近播放不跨库/跨会话保留），
        // 并递增 mutation 令视图侧「最近播放」显示模型缓存立即失效（不显示旧历史）。
        self.recent.clear();
        self.mutation += 1;
        self.scanning = true;
        self.scan_gen = self.scan_gen.wrapping_add(1);
        let scan_id = self.scan_gen;
        let scanned_dir = dir.clone();
        let weak = cx.weak_entity();
        cx.notify(); // 装配态（「正在扫描」）立即可见
        cx.spawn(async move |_, cx| {
            let scanned = cx
                .background_executor()
                .spawn(async move { scan_dir(&scanned_dir) })
                .await;
            let _ = weak.update(cx, |this, cx| {
                if this.scan_gen != scan_id {
                    return; // 已被更新扫描取代：丢弃旧结果，防竞态覆盖
                }
                this.commit_scan(dir, scanned, cx);
            });
        })
        .detach();
    }

    /// 后台扫描完成的主线程收口：整体置换库 + 重定位当前曲/队列 + 一次 notify + 派批量时长
    /// 探测。只由异步 scan 任务在 `scan_gen` 未过期时调用（主线程串行，无竞态）。
    fn commit_scan(&mut self, dir: PathBuf, scanned: Vec<Track>, cx: &mut Context<Self>) {
        // 先按路径留底，再整体替换（重扫后下标漂移，用路径重定位）。
        let old_paths: Vec<PathBuf> = self.library.iter().map(|t| t.path.clone()).collect();
        self.library = scanned;
        self.mix_groups = build_mix_groups(&self.library);
        self.dir = Some(dir);
        self.scanning = false;
        // 库整体替换 → 视图侧「过滤/分组显示模型」缓存失效。
        self.mutation += 1;
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
        // 恢复上次状态：仅在「打开音乐模式首次装配」执行一次——`pending_restore` 由
        // `load_default_dir`（首次进入音乐模式）置入；此后的 `load_dir` 换代（换目录/
        // 播放外部文件重扫）**不**重复恢复，防换目录后被旧 prefs 拉回丢失显式意图。
        if !self.restore_done {
            self.restore_done = true;
            if let Some((last, prog)) = self.pending_restore.take() {
                self.apply_restore(last, prog, cx);
            }
        } else {
            // 库换代：首次恢复的续播候选下标已失效，清掉防陈旧误命中（用户换到其它曲目
            // 播放不受影响；换回上次恢复曲目也从头播——恢复续播只对首次选中生效）。
            self.resume_restore = None;
            self.resume_on_open = None;
        }
        // 外部文件待播：新库就绪后按路径重定位播放（play_path 的异步延续）。
        if let Some(p) = self.pending_path.take() {
            if let Some(idx) = self.library.iter().position(|t| t.path == *p) {
                cx.notify();
                self.play_track(idx, cx);
                self.probe_durations(cx);
                return;
            }
            self.error = Some(format!("音频不在库中：{}", p.display()));
        }
        cx.notify();
        self.probe_durations(cx);
    }

    /// 后台批量探测曲目时长（open+params+drop）：**整批攒齐后一次 notify**——消除旧实现
    /// 「每首完成即整树 notify×N」的启动尖峰。失败/无 dll 保持 0。采样率沿用 sink
    /// （未开则 44_100，duration 与速率无关）。`scan_gen` 变更（换库重扫）即整批丢弃。
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
        let scan_gen = self.scan_gen;
        let weak = cx.weak_entity();
        cx.spawn(async move |_, cx| {
            let results = cx
                .background_executor()
                .spawn(async move {
                    let mut out = Vec::with_capacity(targets.len());
                    for (i, path) in &targets {
                        out.push((*i, probe_duration(path, rate).unwrap_or(0.0)));
                    }
                    out
                })
                .await;
            let _ = weak.update(cx, |this, cx| {
                if this.scan_gen != scan_gen {
                    return; // 库已换代：本次探测下标已失效，丢弃
                }
                let mut any = false;
                for (i, d) in results {
                    if let Some(t) = this.library.get_mut(i) {
                        t.duration = d;
                        any = true;
                    }
                }
                if any {
                    cx.notify(); // 整批一次
                }
            });
        })
        .detach();
    }

    /// 异步扫库装配态（UI 空态提示用）。
    pub fn scanning(&self) -> bool {
        self.scanning
    }

    // ---- 偏好接线 + 启动恢复 + 统一落盘（K10：一处脏标记 + 唯一 flush） ----

    /// 打开音乐模式时把上次偏好接线进播放器（[`crate::view::MusicView::load_default_dir`]
    /// 入口调用）：
    /// - 音量 → 立即应用（`None` 用播放器默认 1.0；`set_volume` 内 clamp 0..1）；
    /// - 收藏集 → 替换内存镜像（新→旧序保留，与配置字段文档一致）；
    /// - 上次曲目/进度 → 暂存 `pending_restore`，等首个 [`Self::commit_scan`] 库就绪后
    ///   恢复选中定位（**不自动播放**）。
    pub(crate) fn apply_saved_prefs(
        &mut self,
        prefs: &config::user::UserPrefs,
        cx: &mut Context<Self>,
    ) {
        self.set_volume(prefs.music_volume.unwrap_or(1.0), cx);
        self.liked = prefs.music_liked.iter().cloned().collect();
        self.liked_order = prefs.music_liked.clone();
        self.pending_restore = Some((prefs.music_last_track.clone(), prefs.music_progress_secs));
    }

    /// 恢复上次选中（首个 `commit_scan` 调用，库已就绪）：
    /// - 无 `last_track`（从未播放过）→ 不自动选中，保持无 current（空态/首次打开原样）；
    /// - 在库 → `current` 指向该下标（选中高亮）、`playing=false` 不自动播放、进度显示
    ///   `seek_base = last progress`（clamp 到已测 duration；未探测时由 position() 在探测后
    ///   min 钳位）。续播候选 `resume_restore`：仅用户显式播放该曲时 `seek` 续播；
    /// - 不在库（目录变/文件删）→ 落库首选中、进度清 0，并标记脏回写清理旧进度；
    /// - 库空 → 无 current、空态正常。
    fn apply_restore(
        &mut self,
        last_track: Option<String>,
        progress: Option<f64>,
        cx: &mut Context<Self>,
    ) {
        self.playing = false;
        self.paused = false;
        let Some(path) = last_track else {
            return; // 从未播放过：不自动选中
        };
        let Some(idx) = self.library.iter().position(|t| t.path.to_string_lossy() == path) else {
            // 上次曲目不在库（目录变/文件删）→ 落库首选中、进度清 0，回写清理旧 prefs。
            if !self.library.is_empty() {
                self.queue = (0..self.library.len()).collect();
                self.queue_pos = 0;
                self.current = Some(0);
                self.last_track_pref = Some(self.library[0].path.to_string_lossy().to_string());
                self.progress_pref = Some(0.0);
                self.prefs_dirty = true;
            }
            self.error = None;
            self.mutation += 1;
            cx.notify();
            return;
        };
        let dur = self.library[idx].duration;
        let saved = progress.unwrap_or(0.0).max(0.0);
        // 越界钳位到已测 duration；未探测（0）保留原值——显示层 position() 与续播 seek()
        // 在 duration 就绪后二次 min 钳位（「progress > duration → clamp；duration≤0 → 0」）。
        let clamped = if dur > 0.0 { saved.min(dur) } else { saved };
        self.queue = (0..self.library.len()).collect();
        self.queue_pos = idx;
        self.current = Some(idx);
        self.seek_base = clamped;
        self.seek_clock_base = 0.0;
        self.flush_seen.set(false);
        self.scrubbing = false;
        self.last_track_pref = Some(path);
        self.progress_pref = Some(clamped);
        if clamped > 0.0 {
            // 续播候选：仅用户对该曲显式播放时 seek 续播；恢复选中本身绝不启动 sink。
            self.resume_restore = Some((idx, clamped));
        }
        self.error = None;
        self.mutation += 1;
        cx.notify();
    }

    /// 统一落盘入口（K10）：脏才写。先 [`Self::prefs`] 合并既有偏好（防丢 theme/font_size
    /// 等其它字段），仅覆写音乐字段再 `save()`；成功后清脏。每次「应写盘」即自增
    /// `prefs_flush_count`（测试探针）。music 自身测试构建下 `config` 是依赖编译、其自身
    /// `cfg!(test)` 门控不生效 → 这里按本 crate 测试态跳过真实写盘（防污染 `%APPDATA%`，
    /// K8），只数探针；生产/其它构建真写。写失败静默回退（保持内存态，与 config 一致）。
    pub fn flush_prefs(&mut self) {
        if !self.prefs_dirty {
            return;
        }
        self.prefs_flush_count += 1;
        let mut prefs = Self::prefs();
        prefs.music_volume = Some(self.volume);
        prefs.music_last_track = self.last_track_pref.clone();
        prefs.music_progress_secs = self.progress_pref;
        prefs.music_liked = self.liked_order.clone();
        if !cfg!(test) {
            prefs.save();
        }
        self.prefs_dirty = false;
    }

    /// 把当前播放位置写入进度偏好并 flush（暂停/停止/退出音乐模式等事件驱动点）：
    /// 无当前曲目时位置无意义，跳过进度写入（仍 flush 其它待写脏项，如刚收藏）。
    pub fn save_progress_and_flush(&mut self) {
        if self.current.is_some() {
            self.progress_pref = Some(self.position());
            self.prefs_dirty = true;
        }
        self.flush_prefs();
    }

    /// 播放中周期持久化（[`crate::view::MusicView::on_tick`] 每拍调）：每
    /// ≥[`PREFS_PERSIST_INTERVAL`] 一次、**且位置确有变化**才写入进度偏好并 flush
    /// （K11 粒度合并：播放中 5s 落盘一次，禁止逐帧写）。暂停态位置不变 → 不重复写。
    /// 无当前曲目时进度无意义（由暂停/停止/退出等事件已落盘），但仍走统一 flush —— 收藏/
    /// 音量等其它脏项不因「无 current」滞留（K10 一处脏标记 + 统一 flush 收口）。
    pub fn persist_progress_periodic(&mut self, cx: &mut Context<Self>) {
        let now = cx.background_executor().now();
        if self
            .last_prefs_attempt
            .is_some_and(|t| now.duration_since(t) < PREFS_PERSIST_INTERVAL)
        {
            return;
        }
        self.last_prefs_attempt = Some(now);
        if self.current.is_some() {
            let pos = self.position();
            if self.progress_pref.map_or(true, |p| (p - pos).abs() > 1e-6) {
                self.progress_pref = Some(pos);
                self.prefs_dirty = true;
            }
        }
        self.flush_prefs();
    }

    /// 播放 `library[idx]`：queue 空则先建全库队列（shuffle 洗牌）；`idx` 不在队列则
    /// 追加；sink 首次打开（失败 → error 提示，纯状态播放）；发 `Load`、置状态、notify。
    pub fn play_track(&mut self, idx: usize, cx: &mut Context<Self>) {
        if idx >= self.library.len() {
            return;
        }
        // 切歌前落盘旧进度：换到不同曲目且此前有 current 时，把旧位置写入偏好并立即
        // flush（（旧曲, 旧位置）一致对落盘；随后本函数把 last_track/progress 切到新曲
        // 并待下次事件/周期 flush（新曲, 0）——崩溃窗口内磁盘仍是自洽旧对）。
        if self.current.is_some_and(|c| c != idx) {
            self.progress_pref = Some(self.position());
            self.prefs_dirty = true;
            self.flush_prefs();
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
        // 会话播放历史记录（所有播放/切歌路径均收敛于此：行点击/next/prev/play_path/恢复，
        // 记在此处不重不漏；最近先、同曲去重后置顶，toString_lossy 与视图侧路径键一致）。
        self.push_recent(self.library[idx].path.to_string_lossy().to_string());
        // 历史顺序变化 → 视图侧「最近播放」显示模型缓存失效（同 toggle_like 对 Liked 的理由）。
        self.mutation += 1;
        self.error = None;
        self.duration = 0.0;
        self.paused = false;
        self.playing = true;
        self.reset_playhead_base();
        // 续播：用户显式播放「恢复选中的那首」 → 先用 saved 基准显示（seek_base=saved），
        // 真实音频 seek 延到 Opened 后（worker 无 codec 时 Seek 是空操作）；对其它曲目 → 从头播
        // （seek_base 已复位 0）。恢复选中本身只定位、绝不启动 sink——续播只在用户显式播放时。
        let resume = match self.resume_restore {
            Some((ri, p)) if ri == idx && p > 0.0 => {
                self.resume_restore = None; // 一次性：续播用完即止，此后重播从头
                Some(p)
            }
            _ => None,
        };
        if let Some(saved) = resume {
            self.seek_base = saved;
            self.seek_clock_base = self.sink.as_ref().map(|s| s.position()).unwrap_or(0.0);
            self.flush_seen.set(false);
            self.resume_on_open = Some(saved);
        } else {
            // 前一轮「待 Opened 续播」未消费（用户等不到 Opened 就切歌/重播/播放外部文件）→
            // 必须清掉：否则残留落点会被**下一首** Opened 消费，把新曲误 seek 到旧曲位置。
            self.resume_on_open = None;
        }
        // 当前曲目/进度持久化：新曲从 saved（续播）或 0 起，标记脏统一 flush。
        self.last_track_pref = Some(self.library[idx].path.to_string_lossy().to_string());
        self.progress_pref = resume.or(Some(0.0));
        self.prefs_dirty = true;
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
        // 暂停：当前播放位置写入进度偏好并立即 flush（事件驱动，K11 粒度）。
        self.save_progress_and_flush();
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
    /// 未找到（如路径在配置目录之外）→ 以该文件**父目录**作该次库异步重扫，扫完在
    /// [`Self::commit_scan`] 中按路径重定位播放（`pending_path`）；父目录相同但文件新加入
    /// 也会重扫到。复用 `load_dir`/`play_track`，不重复建队列逻辑。
    pub fn play_path(&mut self, path: impl AsRef<Path>, cx: &mut Context<Self>) {
        let path = path.as_ref().to_path_buf();
        if let Some(idx) = self.library.iter().position(|t| t.path == path) {
            return self.play_track(idx, cx);
        }
        // 路径不在当前库：以文件父目录异步重扫，扫完按路径重定位播放。
        if let Some(parent) = path.parent() {
            self.pending_path = Some(path.clone());
            self.load_dir(parent, cx);
            return;
        }
        self.error = Some(format!("音频不在库中：{}", path.display()));
        cx.notify();
    }

    /// seek 到 `seconds`（clamp 到 [0, 探测时长]）；flush 在 worker 内完成。
    /// 同步把显示基准 `seek_base` 置为目标：即使 worker 异步 flush 归零 sink 时钟，
    /// `position()` 也从落点继续走（不再从 0 重走）——进度条点/拖「显示到落点」的关键。
    /// seek 后把落点写入进度偏好并标记脏（不即时 flush——拖拽节流下防刷盘，周期 flush 收）。
    pub fn seek(&mut self, seconds: f64, cx: &mut Context<Self>) {
        let max = if self.duration > 0.0 {
            self.duration
        } else {
            f64::INFINITY
        };
        let t = seconds.max(0.0).min(max);
        self.seek_base = t;
        self.seek_clock_base = self.sink.as_ref().map(|s| s.position()).unwrap_or(0.0);
        self.flush_seen.set(false);
        self.progress_pref = Some(t);
        self.prefs_dirty = true;
        let gen_id = self.generation;
        if let Some(tx) = &self.worker_tx {
            let _ = tx.send(WorkerCmd::Seek { gen_id, seconds: t });
        }
        cx.notify();
    }

    /// 仅更新显示基准（拖拽节流窗口内逐 move 调用：显示即时跟手，不发 worker seek）。
    /// 与 `seek` 同一基准语义：位置 = 新基准 + 自锚点起的时钟增量。
    pub fn set_seek_base(&mut self, seconds: f64, cx: &mut Context<Self>) {
        let max = if self.duration > 0.0 {
            self.duration
        } else {
            0.0
        };
        let t = seconds.max(0.0).min(max);
        self.seek_base = t;
        self.seek_clock_base = self.sink.as_ref().map(|s| s.position()).unwrap_or(0.0);
        cx.notify();
    }

    /// 拖拽轨道开关：进入后 `position()` 钉在 `seek_base`（显示跟手指、不随时钟
    /// 漂移）；退出（松手收尾）解锁。与视图 `state.scrubbing` 对应——`position()`
    /// 需要在拖拽中读到该状态，故在播放器侧留一份源。
    pub fn set_scrubbing(&mut self, scrubbing: bool, cx: &mut Context<Self>) {
        self.scrubbing = scrubbing;
        cx.notify();
    }

    /// 新曲目/停止：播放显示基准复位（正常播放从 0 走）。
    fn reset_playhead_base(&mut self) {
        self.seek_base = 0.0;
        self.seek_clock_base = 0.0;
        self.scrubbing = false;
        self.flush_seen.set(false);
    }

    pub fn set_volume(&mut self, volume: f32, cx: &mut Context<Self>) {
        self.volume = volume.clamp(0.0, 1.0);
        if let Some(s) = &self.sink {
            s.set_volume(self.volume);
        }
        // 音量变化 → 标记持久化脏（统一 flush，K10）。
        self.prefs_dirty = true;
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

    /// 收藏/取消收藏 `library[idx]`（按路径字符串）。收藏变化 → 标记持久化脏并更新
    /// 新→旧顺序镜像 `liked_order`（统一 flush，K10）。
    pub fn toggle_like(&mut self, idx: usize, cx: &mut Context<Self>) {
        let Some(t) = self.library.get(idx) else {
            return;
        };
        let p = t.path.to_string_lossy().to_string();
        if !self.liked.remove(&p) {
            self.liked.insert(p.clone());
            // 新收藏 → 新→旧序头插（对齐配置字段文档）。
            self.liked_order.insert(0, p);
        } else {
            self.liked_order.retain(|x| x != &p);
        }
        // 收藏集变化 → 视图侧「过滤/分组显示模型」缓存（Liked 分区依赖 liked 集）失效。
        self.mutation += 1;
        // 收藏集变化 → 标记持久化脏（K10 一处脏标记 + 统一 flush）。
        self.prefs_dirty = true;
        cx.notify();
    }

    /// 库/收藏变更计数（主区列表渲染缓存失效键，见 [`Self::mutation`] 注释）。
    pub fn mutation(&self) -> u64 {
        self.mutation
    }

    pub fn is_liked(&self, idx: usize) -> bool {
        self.library
            .get(idx)
            .map(|t| self.liked.contains(&t.path.to_string_lossy().to_string()))
            .unwrap_or(false)
    }

    /// 停止：关输出、复位播放状态（保留 library/dir/liked）。停止前把当前播放位置写入
    /// 进度偏好并立即 flush（事件驱动落盘点）。
    pub fn stop(&mut self, cx: &mut Context<Self>) {
        let pos = self.position();
        if let Some(tx) = &self.worker_tx {
            let _ = tx.send(WorkerCmd::Stop);
        }
        if let Some(s) = &self.sink {
            let _ = s.pause();
            s.flush();
        }
        if self.current.is_some() {
            self.progress_pref = Some(pos);
            self.prefs_dirty = true;
        }
        self.playing = false;
        self.paused = false;
        self.queue.clear();
        self.queue_pos = 0;
        self.current = None;
        // 未消费的续播落点/候选随停止清掉：停止后播放任何曲目都从头起，不得残留旧续播
        // 被下一首 Opened 消费（防跨曲目 seek 污染，同 play_track 非续播分支）。
        self.resume_on_open = None;
        self.resume_restore = None;
        self.duration = 0.0;
        self.reset_playhead_base();
        self.flush_prefs();
        cx.notify();
    }

    /// 播放位置（秒）= 显示基准 `seek_base` + 自落点起的时钟增量：
    /// - 正常播放：seek_base=0、锚=0 → 位置 = sink 时钟（与现状一致）；
    /// - `seek(T)` 后：位置立即 = T（显示到落点）；worker flush 归零时钟后进入
    ///   「增量 = clock」直接模式 → 自 T 继续走，不再从 0 重走、也不「冻结再跃升」；
    /// - 拖拽（`scrubbing`）：位置钉在最近预览落点 `seek_base`（跟手指，不随时钟漂移）；
    /// - paused/无 sink：时钟不推进/恒 0 → 位置停在与 seek_base 一致处。
    /// duration<=0（无曲目/未探测）恒 0。
    pub fn position(&self) -> f64 {
        let clock = self.sink.as_ref().map(|s| s.position()).unwrap_or(0.0);
        compose_playhead(
            self.seek_base,
            self.seek_clock_base,
            &self.flush_seen,
            clock,
            self.duration,
            self.scrubbing,
        )
    }

    /// 测试专用注入：设置探测时长（无 worker/设备环境下定位 seek 显示语义用）。
    #[cfg(test)]
    pub(crate) fn set_test_duration(&mut self, seconds: f64) {
        self.duration = seconds;
    }

    /// 测试专用：直接置「播放中 + current=idx」的纯状态播放（**不发 worker/不开设备**，
    /// 头less 视图测试用——真 `play_track` 在有设备/缺 dll 环境会被 worker 的 Error 事件
    /// 翻转 playing=false，破坏确定性）。position 由 seek/seek_base 纯合成驱动。
    #[cfg(test)]
    pub(crate) fn test_start_playing(&mut self, idx: usize) {
        if idx >= self.library.len() {
            return;
        }
        if self.queue.is_empty() {
            self.queue = (0..self.library.len()).collect();
            self.queue_pos = 0;
        }
        self.queue_pos = idx.min(self.queue.len().saturating_sub(1));
        self.current = Some(idx);
        self.playing = true;
        self.paused = false;
        self.error = None;
        self.duration = 0.0;
        self.reset_playhead_base();
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

    /// 本次会话播放历史（最近先；空 = 无记录）。仅内存、不持久化，`load_dir` 时清空。
    pub fn recent(&self) -> &[String] {
        &self.recent
    }

    /// 记录一次播放：同一曲去重后置顶（`retain` 移除旧位置再头插）。
    fn push_recent(&mut self, path: String) {
        self.recent.retain(|p| p != &path);
        self.recent.insert(0, path);
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

/// `position()` 的纯合成（独立成函数供单测直接驱动各种时钟/基准组合）：
/// - duration<=0 → 恒 0（不 panic）；
/// - 观察 `clock < 锚点`（sink 时钟回跳，played_frames 被 flush 归零）即判定 worker
///   flush 已落地 → `flush_seen` 置真，此后增量**直接取 `clock`**（时钟 0 基自落点重走）；
/// - 仍未观察到回跳（flush 在途）：增量 = `(clock − 锚点).max(0)`（异步 gap 内小幅增量，
///   显示不跳端、不闪回溯）；
/// - `scrubbing`：钉在 `seek_base`，不随时钟漂移（拖拽预览跟手指）。
fn compose_playhead(
    seek_base: f64,
    seek_clock_base: f64,
    flush_seen: &Cell<bool>,
    clock: f64,
    duration: f64,
    scrubbing: bool,
) -> f64 {
    if duration <= 0.0 {
        return 0.0;
    }
    if !flush_seen.get() && clock < seek_clock_base {
        flush_seen.set(true);
    }
    if scrubbing {
        return seek_base.min(duration);
    }
    let delta = if flush_seen.get() {
        clock
    } else {
        (clock - seek_clock_base).max(0.0)
    };
    (seek_base + delta).min(duration)
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

    /// 显示基准回归：`seek(T)` 后（有曲目时长、无 worker/sink）`position()≈T`（显示落到
    /// 目标，而不是 sink flush 归零后从 0 重走）；正常播放（base=0）与现状一致；超时长
    /// 钳位；duration=0（无曲目/未探测）显示恒 0、不 panic。
    #[gpui::test]
    async fn seek_updates_position_base_without_sink(cx: &mut TestAppContext) {
        let player: gpui::Entity<MusicPlayer> =
            cx.update(|app| app.new(|cx| MusicPlayer::new(cx)));
        // 无 sink：未 seek 前 position=0（与现状一致）。
        let p0 = cx.read(|app| player.read(app).position());
        assert_eq!(p0, 0.0, "未 seek 前无 sink position 应恒 0");
        // 有曲目时长（无 worker/sink）：seek(7) 后 position≈7（显示基准=seek 目标）。
        cx.update(|app| {
            player.update(app, |p, _| p.set_test_duration(20.0));
        });
        cx.update(|app| {
            player.update(app, |p, cx| p.seek(7.0, cx));
        });
        let p1 = cx.read(|app| player.read(app).position());
        assert!(
            (p1 - 7.0).abs() < 1e-9,
            "seek 后 position 应=seek_base=7.0（显示不归零），实得 {p1}"
        );
        // 正常播放基准：base=0 时 position 由 sink 时钟驱动（无 sink → 0），与现状一致。
        cx.update(|app| {
            player.update(app, |p, cx| p.seek(0.0, cx));
        });
        let p2 = cx.read(|app| player.read(app).position());
        assert_eq!(p2, 0.0, "seek(0) 后无 sink position 应回 0");
        // 超 duration 钳位到 duration。
        cx.update(|app| {
            player.update(app, |p, _| p.set_test_duration(10.0));
        });
        cx.update(|app| {
            player.update(app, |p, cx| p.seek(99.0, cx));
        });
        let p3 = cx.read(|app| player.read(app).position());
        assert!(
            (p3 - 10.0).abs() < 1e-9,
            "seek 超 duration 应钳位到 duration=10，实得 {p3}"
        );
        // duration=0（无曲目/未探测）：点击/拖动不 panic、显示恒 0。
        cx.update(|app| {
            player.update(app, |p, _| p.set_test_duration(0.0));
        });
        cx.update(|app| {
            player.update(app, |p, cx| p.seek(5.0, cx));
        });
        let p4 = cx.read(|app| player.read(app).position());
        assert_eq!(p4, 0.0, "duration=0 无曲目 position 应恒 0");
    }

    /// `compose_playhead` 纯合成回归：seek 提交后（锚点 C、flush 已归零时钟）增量直接取
    /// `clock`，显示自落点继续走——**不得**「冻结到时钟追上锚点再跃升」（原实现的
    /// `(clock−锚点).max(0)` 在 flush 后时钟 0 基重走时会冻结 C 秒）；在途 flush 的异步
    /// gap 内只允许小幅增量；拖拽钉扎不随时钟漂移；超时长钳位；duration=0 恒 0。
    #[test]
    fn compose_playhead_covers_flush_latch_pin_and_clamp() {
        use std::cell::Cell;
        // 在途 flush（尚未观察到时钟回跳）：clock ≥ 锚点 → 增量 = clock − 锚点（≈0）。
        let f = Cell::new(false);
        let p = compose_playhead(120.0, 60.0, &f, 60.05, 200.0, false);
        assert!((p - 120.05).abs() < 1e-9, "flush 在途 gap 内仅小幅增量，实得 {p}");
        assert!(!f.get(), "未观察到回跳前不得 latch");
        // flush 落地：clock=0 < 锚点 → 首次观察到即 latch；随后增量直接取 clock。
        let f = Cell::new(false);
        let p = compose_playhead(120.0, 60.0, &f, 0.0, 200.0, false);
        assert_eq!(p, 120.0, "flush 归零时刻显示恰为落点");
        assert!(f.get(), "观察到时钟回跳应 latch");
        let p2 = compose_playhead(120.0, 60.0, &f, 42.0, 200.0, false);
        assert!((p2 - 162.0).abs() < 1e-9, "latch 后增量=clock，42s 处应 162，实得 {p2}");
        // 拖拽钉扎：时钟再大也不漂移。
        let f = Cell::new(false);
        let p = compose_playhead(40.0, 3.0, &f, 200.0, 120.0, true);
        assert_eq!(p, 40.0, "拖拽中 position 应钉在 seek_base=40");
        // 超时长钳位（latch 后 clock 增长 + 大 seek 基准）。
        let f = Cell::new(false);
        compose_playhead(150.0, 60.0, &f, 0.0, 200.0, false); // latch
        let p = compose_playhead(150.0, 60.0, &f, 80.0, 200.0, false);
        assert_eq!(p, 200.0, "seek+增量超时长应钳位到 duration=200，实得 {p}");
        // duration=0 恒 0（不 panic）。
        let f = Cell::new(false);
        let p = compose_playhead(50.0, 60.0, &f, 10.0, 0.0, false);
        assert_eq!(p, 0.0, "duration=0 恒 0");
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
        // load_dir 已异步扫库：排空后台 scan→commit（真 dll 路径下才到此）。
        cx.background_executor.run_until_parked();
        cx.background_executor.run_until_parked();
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

    // ---- 偏好持久化 + 启动恢复（本轮新功能）----

    /// 驱动异步扫库到主线程装配完成（scan→commit→probe 全落地）。
    fn drain_scan(cx: &mut TestAppContext) {
        for _ in 0..3 {
            cx.background_executor.run_until_parked();
        }
    }

    /// 建含两首 wav 的临时库，返回 `(dir, 首曲路径, 次曲路径, player)`（未 load_dir）。
    fn two_track_library(
        cx: &mut TestAppContext,
        tag: &str,
    ) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf, gpui::Entity<MusicPlayer>) {
        let dir = std::env::temp_dir().join(format!("gpui-music-persist-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p0 = dir.join("甲 - one.wav");
        let p1 = dir.join("乙 - two.wav");
        write_test_wav(&p0, 1.5);
        write_test_wav(&p1, 1.5);
        let player: gpui::Entity<MusicPlayer> = cx.update(|app| app.new(|cx| MusicPlayer::new(cx)));
        (dir, p0, p1, player)
    }

    /// 按路径查库内下标（scan_dir 按 UTF-8 字节序排序——中文文件名「乙/丁」字节序在
    /// 「甲/丙」之前，测试不得假定词序=下标序，统一按路径定位）。
    fn idx_of(
        player: &gpui::Entity<MusicPlayer>,
        cx: &mut TestAppContext,
        path: &std::path::Path,
    ) -> usize {
        let path = path.to_path_buf();
        cx.read(|app| {
            player
                .read(app)
                .library()
                .iter()
                .position(|t| t.path == path)
                .expect("曲目应已在库中")
        })
    }

    /// 恢复语义回归：音量恢复到上次保存值；上次曲目在库 → 选中定位（不自动播放、不启动
    /// sink）；进度条显示上次位置（复用 seek_base 显示基准）。
    #[gpui::test]
    async fn restore_selects_last_track_and_volume_without_playing(cx: &mut TestAppContext) {
        let (dir, _p0, p1, player) = two_track_library(cx, "restore-sel");
        let p1_str = p1.to_string_lossy().to_string();
        let mut prefs = config::user::UserPrefs::default();
        prefs.music_volume = Some(0.33);
        prefs.music_last_track = Some(p1_str.clone());
        prefs.music_progress_secs = Some(12.0);
        cx.update(|app| {
            player.update(app, |p, cx| p.apply_saved_prefs(&prefs, cx));
        });
        cx.update(|app| {
            player.update(app, |p, cx| p.load_dir(&dir, cx));
        });
        drain_scan(cx);

        let (vol, cur, playing, worker_none, last) = cx.read(|app| {
            let p = player.read(app);
            (
                p.volume,
                p.current,
                p.playing,
                p.worker_tx.is_none(),
                p.last_track_pref.clone(),
            )
        });
        let p1_idx = idx_of(&player, cx, &p1);
        assert_eq!(vol, 0.33, "音量应恢复到上次保存值");
        assert_eq!(cur, Some(p1_idx), "上次曲目应选中定位到其下标（按路径）");
        assert!(!playing, "恢复选中不自动播放");
        assert!(worker_none, "恢复选中绝不启动 sink");
        assert_eq!(last.as_deref(), Some(p1_str.as_str()), "last_track 镜像应指向恢复曲目");
        assert_eq!(cx.read(|app| player.read(app).prefs_flush_count), 0, "恢复本身不写盘");

        // 进度显示：恢复时 duration 未探测 → position 0；注入时长（模拟探测完成）→ 显示 12。
        assert_eq!(cx.read(|app| player.read(app).position()), 0.0, "duration≤0 时 position 恒 0");
        cx.update(|app| {
            player.update(app, |p, _| p.set_test_duration(30.0));
        });
        let pos1 = cx.read(|app| player.read(app).position());
        assert!((pos1 - 12.0).abs() < 1e-9, "进度条应显示上次位置 12，实得 {pos1}");
        let rr = cx.read(|app| player.read(app).resume_restore);
        assert_eq!(rr, Some((p1_idx, 12.0)), "续播候选应挂在恢复曲目上");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 恢复语义回归：上次曲目不在库（目录变/文件删）→ 落库首选中、进度清 0、标记脏回写。
    #[gpui::test]
    async fn restore_missing_track_falls_back_to_first(cx: &mut TestAppContext) {
        let (dir, _p0, _p1, player) = two_track_library(cx, "restore-miss");
        let mut prefs = config::user::UserPrefs::default();
        prefs.music_last_track = Some("D:\\gone\\曲目.wav".to_string());
        prefs.music_progress_secs = Some(5.0);
        cx.update(|app| player.update(app, |p, cx| p.apply_saved_prefs(&prefs, cx)));
        cx.update(|app| player.update(app, |p, cx| p.load_dir(&dir, cx)));
        drain_scan(cx);
        let (cur, playing, base, rr, dirty, last) = cx.read(|app| {
            let p = player.read(app);
            (p.current, p.playing, p.seek_base, p.resume_restore, p.prefs_dirty, p.last_track_pref.clone())
        });
        let first = cx.read(|app| {
            player
                .read(app)
                .library()
                .first()
                .map(|t| t.path.to_string_lossy().to_string())
        });
        assert_eq!(cur, Some(0), "上次曲目不在库 → 落库首选中");
        assert!(!playing);
        assert_eq!(base, 0.0, "进度清 0");
        assert_eq!(rr, None, "无续播候选");
        assert!(dirty, "落库首应标记脏回写清理旧进度");
        assert_eq!(last, first, "last_track 镜像应更新为当前库首曲（按扫描序）");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 恢复语义回归：库空 → 无 current、空态正常（不崩）。
    #[gpui::test]
    async fn restore_empty_library_keeps_no_current(cx: &mut TestAppContext) {
        let dir = std::env::temp_dir().join(format!("gpui-music-persist-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let player: gpui::Entity<MusicPlayer> = cx.update(|app| app.new(|cx| MusicPlayer::new(cx)));
        let mut prefs = config::user::UserPrefs::default();
        prefs.music_last_track = Some("D:\\gone\\x.wav".to_string());
        prefs.music_progress_secs = Some(5.0);
        cx.update(|app| player.update(app, |p, cx| p.apply_saved_prefs(&prefs, cx)));
        cx.update(|app| player.update(app, |p, cx| p.load_dir(&dir, cx)));
        drain_scan(cx);
        let (cur, playing) = cx.read(|app| {
            let p = player.read(app);
            (p.current, p.playing)
        });
        assert_eq!(cur, None, "库空 → 无 current");
        assert!(!playing);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 恢复语义回归：从未播放过（无 last_track）→ 不自动选中；音量仍恢复。
    #[gpui::test]
    async fn restore_without_last_track_selects_nothing(cx: &mut TestAppContext) {
        let (dir, _p0, _p1, player) = two_track_library(cx, "restore-none");
        let mut prefs = config::user::UserPrefs::default();
        prefs.music_volume = Some(0.5);
        prefs.music_progress_secs = Some(3.0);
        cx.update(|app| player.update(app, |p, cx| p.apply_saved_prefs(&prefs, cx)));
        cx.update(|app| player.update(app, |p, cx| p.load_dir(&dir, cx)));
        drain_scan(cx);
        let (cur, playing, vol) = cx.read(|app| {
            let p = player.read(app);
            (p.current, p.playing, p.volume)
        });
        assert_eq!(vol, 0.5, "音量仍应恢复");
        assert_eq!(cur, None, "从未播放过 → 不自动选中");
        assert!(!playing);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 恢复语义回归：music_progress_secs 越界（> duration）→ 显示/续播 entry 均 clamp；
    /// duration≤0 → 显示 0（「progress > duration → clamp（duration≤0 → 0）」）。
    #[gpui::test]
    async fn restore_clamps_progress_to_duration(cx: &mut TestAppContext) {
        let (dir, p0, _p1, player) = two_track_library(cx, "restore-clamp");
        let p0_str = p0.to_string_lossy().to_string();
        let mut prefs = config::user::UserPrefs::default();
        prefs.music_last_track = Some(p0_str);
        prefs.music_progress_secs = Some(999.0);
        cx.update(|app| player.update(app, |p, cx| p.apply_saved_prefs(&prefs, cx)));
        cx.update(|app| player.update(app, |p, cx| p.load_dir(&dir, cx)));
        drain_scan(cx);
        // duration 未探测（0）→ position 恒 0。
        assert_eq!(cx.read(|app| player.read(app).position()), 0.0, "duration≤0 → 显示 0");
        // 探测后 duration=30：显示钳位到 30（seek_base=999 → min(duration)）。
        cx.update(|app| player.update(app, |p, _| p.set_test_duration(30.0)));
        let pos1 = cx.read(|app| player.read(app).position());
        assert!((pos1 - 30.0).abs() < 1e-9, "越界进度应钳位到 duration=30，实得 {pos1}");
        // 续播 seek（Opened 后真 seek 路径里的 seek clamp）同样钳位。
        cx.update(|app| player.update(app, |p, cx| p.seek(999.0, cx)));
        let base = cx.read(|app| player.read(app).seek_base);
        assert!((base - 30.0).abs() < 1e-9, "续播 seek 越界应钳位，实得 {base}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 续播语义回归：只有用户「显式播放恢复选中的那首」才从上次进度续播（seek_base=saved +
    /// 待 Opened 真 seek）；对其它曲目从头播；恢复选中本身不启动 sink。
    #[gpui::test]
    async fn resume_restored_track_seeks_saved_only_for_that_track(cx: &mut TestAppContext) {
        let (dir, p0, p1, player) = two_track_library(cx, "resume");
        let p1_str = p1.to_string_lossy().to_string();
        let mut prefs = config::user::UserPrefs::default();
        prefs.music_last_track = Some(p1_str);
        prefs.music_progress_secs = Some(12.0);
        cx.update(|app| player.update(app, |p, cx| p.apply_saved_prefs(&prefs, cx)));
        cx.update(|app| player.update(app, |p, cx| p.load_dir(&dir, cx)));
        drain_scan(cx);

        let (cur, playing) = cx.read(|app| {
            let p = player.read(app);
            (p.current, p.playing)
        });
        let p1_idx = idx_of(&player, cx, &p1);
        assert_eq!((cur, playing), (Some(p1_idx), false), "恢复选中不自动播放（按路径）");

        // 用户显式播放「其它曲目」→ 从头播（seek_base=0），续播候选保留。
        let other = idx_of(&player, cx, &p0);
        cx.update(|app| player.update(app, |p, cx| p.play_track(other, cx)));
        let s0 = cx.read(|app| {
            let p = player.read(app);
            (p.seek_base, p.playing, p.resume_on_open, p.resume_restore)
        });
        assert_eq!(s0.0, 0.0, "其它曲目应从头播（seek_base=0）");
        assert!(s0.1);
        assert_eq!(s0.2, None, "非恢复曲目不挂 resume_on_open");
        assert_eq!(s0.3, Some((p1_idx, 12.0)), "续播候选仍保留给恢复曲目");

        // 用户显式播放「恢复选中的那首」→ 从上次进度续播（seek_base=saved + 待 Opened 真 seek）。
        cx.update(|app| player.update(app, |p, cx| p.play_track(p1_idx, cx)));
        let s1 = cx.read(|app| {
            let p = player.read(app);
            (p.seek_base, p.playing, p.resume_on_open, p.resume_restore, p.progress_pref)
        });
        assert_eq!(s1.0, 12.0, "续播显示基准应为 saved=12");
        assert!(s1.1);
        assert_eq!(s1.2, Some(12.0), "应挂 resume_on_open 待 Opened 真 seek");
        assert_eq!(s1.3, None, "续播候选一次性消费");
        assert_eq!(s1.4, Some(12.0), "进度偏好应记为续播落点");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 续播消费残留回归：恢复曲目已点播放（Opened 未到）→ 立即切到其它曲目，旧续播落点
    /// 必须清掉，不得被下一首 Opened 消费（seek 到错误位置）；stop 同样清掉残留。
    #[gpui::test]
    async fn resume_on_open_cleared_when_track_switches_before_opened(cx: &mut TestAppContext) {
        let (dir, p0, p1, player) = two_track_library(cx, "resume-switch");
        let mut prefs = config::user::UserPrefs::default();
        prefs.music_last_track = Some(p1.to_string_lossy().to_string());
        prefs.music_progress_secs = Some(12.0);
        cx.update(|app| player.update(app, |p, cx| p.apply_saved_prefs(&prefs, cx)));
        cx.update(|app| player.update(app, |p, cx| p.load_dir(&dir, cx)));
        drain_scan(cx);
        let p1_idx = idx_of(&player, cx, &p1);
        let p0_idx = idx_of(&player, cx, &p0);

        // 点恢复曲目 → 挂 resume_on_open 待 Opened 真 seek。
        cx.update(|app| player.update(app, |p, cx| p.play_track(p1_idx, cx)));
        assert_eq!(
            cx.read(|app| player.read(app).resume_on_open),
            Some(12.0),
            "恢复曲目点播应挂 resume_on_open"
        );
        // Opened 到达前切到其它曲目：旧续播落点必须清除（不得由下一首 Opened 消费）。
        cx.update(|app| player.update(app, |p, cx| p.play_track(p0_idx, cx)));
        assert_eq!(
            cx.read(|app| player.read(app).resume_on_open),
            None,
            "切到其它曲目应清掉未消费的续播落点"
        );

        // 再恢复选中并播放（resume_restore 已消费 → 从头播、不挂续播）；随后 stop 也应清
        // 掉任何残留（停止后播放任何曲目都不得续播到旧落点）。
        cx.update(|app| player.update(app, |p, cx| p.play_track(p1_idx, cx)));
        cx.update(|app| player.update(app, |p, cx| p.stop(cx)));
        assert_eq!(
            cx.read(|app| player.read(app).resume_on_open),
            None,
            "stop 应清掉未消费的续播落点"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 切歌前落盘旧进度：换到不同曲目 → 立即 flush（旧曲, 旧位置）一致对；新曲 last_track
    /// 切换、进度清 0（待统一 flush）。
    #[gpui::test]
    async fn switch_track_saves_old_progress_and_new_starts_zero(cx: &mut TestAppContext) {
        let (dir, _p0, p1, player) = two_track_library(cx, "switch");
        cx.update(|app| player.update(app, |p, cx| p.load_dir(&dir, cx)));
        drain_scan(cx);
        let i0 = idx_of(&player, cx, &_p0);
        let i1 = idx_of(&player, cx, &p1);
        cx.update(|app| player.update(app, |p, _| p.test_start_playing(i0)));
        cx.update(|app| player.update(app, |p, _| p.set_test_duration(30.0)));
        cx.update(|app| player.update(app, |p, cx| p.seek(20.0, cx)));
        let (before, dirty0) = cx.read(|app| {
            let p = player.read(app);
            (p.progress_pref, p.prefs_dirty)
        });
        assert_eq!(before, Some(20.0), "seek 后进度偏好应=20");
        assert!(dirty0);

        cx.update(|app| player.update(app, |p, cx| p.play_track(i1, cx)));
        let (count, cur, last, prog, base, playing) = cx.read(|app| {
            let p = player.read(app);
            (p.prefs_flush_count, p.current, p.last_track_pref.clone(), p.progress_pref, p.seek_base, p.playing)
        });
        assert_eq!(count, 1, "切歌前应立即 flush 旧进度（旧曲, 旧位置一致对）");
        assert_eq!(cur, Some(i1));
        assert_eq!(last.as_deref(), Some(p1.to_string_lossy().as_ref()), "last_track 应切到新曲（按路径）");
        assert_eq!(prog, Some(0.0), "新曲从头起、进度清 0");
        assert_eq!(base, 0.0);
        assert!(playing);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// pause / stop / seek 的进度持久化：seek 标记脏但**不即时 flush**；pause/stop 事件驱动
    /// 即时 flush（prefs_flush_count 探针）。
    #[gpui::test]
    async fn pause_stop_seek_flush_progress_probe(cx: &mut TestAppContext) {
        let (dir, _p0, _p1, player) = two_track_library(cx, "pause-stop");
        cx.update(|app| player.update(app, |p, cx| p.load_dir(&dir, cx)));
        drain_scan(cx);
        cx.update(|app| player.update(app, |p, _| p.test_start_playing(0)));
        cx.update(|app| player.update(app, |p, _| p.set_test_duration(30.0)));
        cx.update(|app| player.update(app, |p, cx| p.seek(12.0, cx)));
        let (prog, dirty, count0) = cx.read(|app| {
            let p = player.read(app);
            (p.progress_pref, p.prefs_dirty, p.prefs_flush_count)
        });
        assert_eq!(prog, Some(12.0));
        assert!(dirty, "seek 应标记脏（不即时 flush，防拖拽刷盘）");
        assert_eq!(count0, 0, "seek 不即时 flush");

        // pause：位置写入并即时 flush。
        cx.update(|app| player.update(app, |p, cx| p.pause(cx)));
        let (prog, dirty, count) = cx.read(|app| {
            let p = player.read(app);
            (p.progress_pref, p.prefs_dirty, p.prefs_flush_count)
        });
        assert_eq!(prog, Some(12.0), "暂停应保存当前播放位置");
        assert!(!dirty, "pause flush 后应清脏");
        assert_eq!(count, 1, "pause 应触发一次 flush");

        // resume 后再 stop：停止前位置写入并即时 flush。
        cx.update(|app| player.update(app, |p, cx| p.resume(cx)));
        cx.update(|app| player.update(app, |p, cx| p.stop(cx)));
        let (prog, count, cur) = cx.read(|app| {
            let p = player.read(app);
            (p.progress_pref, p.prefs_flush_count, p.current)
        });
        assert_eq!(prog, Some(12.0), "停止应保存停止前位置");
        assert_eq!(count, 2, "stop 应再触发一次 flush");
        assert_eq!(cur, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 播放中周期持久化：≥5s 一次且位置确有变化才 flush（K11 粒度合并，防逐帧写）；
    /// 5s 窗口内 seek 更新不落盘。
    #[gpui::test]
    async fn periodic_persist_is_5s_gated_and_merges_dirty(cx: &mut TestAppContext) {
        let (dir, _p0, _p1, player) = two_track_library(cx, "periodic");
        cx.update(|app| player.update(app, |p, cx| p.load_dir(&dir, cx)));
        drain_scan(cx);
        cx.update(|app| player.update(app, |p, _| p.test_start_playing(0)));
        cx.update(|app| player.update(app, |p, _| p.set_test_duration(60.0)));
        cx.update(|app| player.update(app, |p, cx| p.seek(9.0, cx)));
        cx.update(|app| player.update(app, |p, cx| p.persist_progress_periodic(cx)));
        let (count, dirty) = cx.read(|app| {
            let p = player.read(app);
            (p.prefs_flush_count, p.prefs_dirty)
        });
        assert_eq!(count, 1, "首次周期检查应 flush 脏进度");
        assert!(!dirty);

        // 5s 窗口内：seek 更新后周期检查不落盘（门控）。
        cx.update(|app| player.update(app, |p, cx| p.seek(10.0, cx)));
        cx.update(|app| player.update(app, |p, cx| p.persist_progress_periodic(cx)));
        let (count, prog) = cx.read(|app| {
            let p = player.read(app);
            (p.prefs_flush_count, p.progress_pref)
        });
        assert_eq!(count, 1, "5s 窗口内不得重复落盘");
        assert_eq!(prog, Some(10.0));

        // 过 5s 且位置变化 → 再次落盘。
        cx.background_executor.advance_clock(Duration::from_secs(6));
        cx.update(|app| player.update(app, |p, cx| p.seek(11.0, cx)));
        cx.update(|app| player.update(app, |p, cx| p.persist_progress_periodic(cx)));
        let (count, dirty) = cx.read(|app| {
            let p = player.read(app);
            (p.prefs_flush_count, p.prefs_dirty)
        });
        assert_eq!(count, 2, "过 5s 且位置变化应再落盘");
        assert!(!dirty);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 空 current 下的脏项收口：无当前曲目（浏览态）收藏后不即时 flush（K10 统一 flush），
    /// 但周期检查仍应把收藏/音量脏项收口——不得因「无 current」让脏数据滞留到退出丢盘。
    #[gpui::test]
    async fn periodic_flush_collects_like_dirty_without_current(cx: &mut TestAppContext) {
        let (dir, p0, p1, player) = two_track_library(cx, "like-idle");
        cx.update(|app| player.update(app, |p, cx| p.load_dir(&dir, cx)));
        drain_scan(cx);
        let i0 = idx_of(&player, cx, &p0);
        let i1 = idx_of(&player, cx, &p1);
        let p0s = p0.to_string_lossy().to_string();
        let p1s = p1.to_string_lossy().to_string();
        // 无 current（浏览态）收藏：只标记脏，不散点直写。
        cx.update(|app| player.update(app, |p, cx| p.toggle_like(i0, cx)));
        let (dirty, count) = cx.read(|app| {
            let p = player.read(app);
            (p.prefs_dirty, p.prefs_flush_count)
        });
        assert!(dirty);
        assert_eq!(count, 0, "收藏不散点直写");

        // 首周期检查：last_prefs_attempt 无基准 → 立即统一 flush 收口（无 current 也 flush）。
        cx.update(|app| player.update(app, |p, cx| p.persist_progress_periodic(cx)));
        let (count, dirty, liked) = cx.read(|app| {
            let p = player.read(app);
            (p.prefs_flush_count, p.prefs_dirty, p.liked_order.clone())
        });
        assert_eq!(count, 1, "无 current 也应 flush 脏收藏");
        assert!(!dirty);
        assert_eq!(liked, vec![p0s.clone()]);

        // 再收藏另一首 → 脏；5s 窗口内周期检查不得落盘。
        cx.update(|app| player.update(app, |p, cx| p.toggle_like(i1, cx)));
        cx.update(|app| player.update(app, |p, cx| p.persist_progress_periodic(cx)));
        let (count, dirty, liked) = cx.read(|app| {
            let p = player.read(app);
            (p.prefs_flush_count, p.prefs_dirty, p.liked_order.clone())
        });
        assert!(dirty, "5s 窗口内收藏更新不落盘（门控）");
        assert_eq!(count, 1);
        assert_eq!(liked, vec![p1s.clone(), p0s.clone()], "新→旧序镜像随后续 flush 一起写");

        // 过 5s → 周期检查再次收口。
        cx.background_executor.advance_clock(Duration::from_secs(6));
        cx.update(|app| player.update(app, |p, cx| p.persist_progress_periodic(cx)));
        let (count, dirty) = cx.read(|app| {
            let p = player.read(app);
            (p.prefs_flush_count, p.prefs_dirty)
        });
        assert_eq!(count, 2, "过 5s 门控应再次 flush");
        assert!(!dirty);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// toggle_like：标记持久化脏 + 更新新→旧序镜像，不散点直写；统一 flush 计数 +1 并清脏。
    #[gpui::test]
    async fn toggle_like_marks_dirty_and_flush_probe(cx: &mut TestAppContext) {
        let (dir, p0, p1, player) = two_track_library(cx, "like");
        cx.update(|app| player.update(app, |p, cx| p.load_dir(&dir, cx)));
        drain_scan(cx);
        let p0s = p0.to_string_lossy().to_string();
        let p1s = p1.to_string_lossy().to_string();
        let i0 = idx_of(&player, cx, &p0);
        let i1 = idx_of(&player, cx, &p1);
        cx.update(|app| player.update(app, |p, cx| p.toggle_like(i0, cx)));
        let (liked0, order, dirty, count) = cx.read(|app| {
            let p = player.read(app);
            (p.is_liked(i0), p.liked_order.clone(), p.prefs_dirty, p.prefs_flush_count)
        });
        assert!(liked0);
        assert_eq!(order, vec![p0s.clone()], "新收藏应为新→旧序头");
        assert!(dirty, "toggle_like 应标记持久化脏");
        assert_eq!(count, 0, "不散点直写");

        cx.update(|app| player.update(app, |p, cx| p.toggle_like(i1, cx)));
        let order = cx.read(|app| player.read(app).liked_order.clone());
        assert_eq!(order, vec![p1s.clone(), p0s.clone()], "新收藏应在头部、旧收藏下移");

        cx.update(|app| player.update(app, |p, _| p.flush_prefs()));
        let (count, dirty) = cx.read(|app| {
            let p = player.read(app);
            (p.prefs_flush_count, p.prefs_dirty)
        });
        assert_eq!(count, 1, "统一 flush 应恰好一次");
        assert!(!dirty);

        cx.update(|app| player.update(app, |p, cx| p.toggle_like(i0, cx)));
        let (liked0, order, dirty) = cx.read(|app| {
            let p = player.read(app);
            (p.is_liked(i0), p.liked_order.clone(), p.prefs_dirty)
        });
        assert!(!liked0);
        assert_eq!(order, vec![p1s], "取消收藏应从序列表移除");
        assert!(dirty);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// music_liked 启动接线：`apply_saved_prefs`（load_default_dir 入口）把 prefs 收藏集
    /// 并入 liked / 序镜像；库内路径即匹配。
    #[gpui::test]
    async fn liked_loads_from_saved_prefs_at_open(cx: &mut TestAppContext) {
        let (dir, p0, _p1, player) = two_track_library(cx, "like-open");
        let p0s = p0.to_string_lossy().to_string();
        let mut prefs = config::user::UserPrefs::default();
        prefs.music_liked = vec![p0s.clone()];
        cx.update(|app| player.update(app, |p, cx| p.apply_saved_prefs(&prefs, cx)));
        cx.update(|app| player.update(app, |p, cx| p.load_dir(&dir, cx)));
        drain_scan(cx);
        let i0 = idx_of(&player, cx, &p0);
        let (is0, order) = cx.read(|app| {
            let p = player.read(app);
            (p.is_liked(i0), p.liked_order.clone())
        });
        assert!(is0, "启动时应把 music_liked 接线进 liked（按路径匹配）");
        assert_eq!(order, vec![p0s], "顺序镜像应与 prefs 一致");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 切库不重复恢复：首次装配恢复选中一次；之后 `load_dir` 换代（换目录）不重复恢复，
    /// 也不保留旧库的续播候选（下标已失效）。
    #[gpui::test]
    async fn dir_switch_does_not_re_restore(cx: &mut TestAppContext) {
        let d1 = std::env::temp_dir().join(format!("gpui-music-persist-d1-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d1);
        std::fs::create_dir_all(&d1).unwrap();
        write_test_wav(&d1.join("甲 - one.wav"), 1.5);
        let d2 = std::env::temp_dir().join(format!("gpui-music-persist-d2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d2);
        std::fs::create_dir_all(&d2).unwrap();
        write_test_wav(&d2.join("丙 - three.wav"), 1.5);
        write_test_wav(&d2.join("丁 - four.wav"), 1.5);

        let player: gpui::Entity<MusicPlayer> = cx.update(|app| app.new(|cx| MusicPlayer::new(cx)));
        let mut prefs = config::user::UserPrefs::default();
        prefs.music_last_track = Some(d1.join("甲 - one.wav").to_string_lossy().to_string());
        prefs.music_progress_secs = Some(12.0);
        cx.update(|app| player.update(app, |p, cx| p.apply_saved_prefs(&prefs, cx)));
        cx.update(|app| player.update(app, |p, cx| p.load_dir(&d1, cx)));
        drain_scan(cx);
        let (cur1, restore_done) = cx.read(|app| {
            let p = player.read(app);
            (p.current, p.restore_done)
        });
        assert_eq!(cur1, Some(0), "首次装配应恢复选中");
        assert!(restore_done);

        // 换目录：不重复恢复；旧库曲目不在新库 → 落库首（新库第一首），续播候选清理。
        cx.update(|app| player.update(app, |p, cx| p.load_dir(&d2, cx)));
        drain_scan(cx);
        let (cur2, rr) = cx.read(|app| {
            let p = player.read(app);
            (p.current, p.resume_restore)
        });
        assert!(restore_done, "换代后仍保持恢复已执行");
        assert_eq!(cur2, Some(0), "换目录不重复恢复：应落库首");
        assert_eq!(rr, None, "旧库续播候选应清理");
        let first = cx.read(|app| {
            let p = player.read(app);
            p.library().first().map(|t| t.path.clone())
        });
        assert!(
            first.as_ref().is_some_and(|p| p.starts_with(&d2)),
            "current 0 应指向新库第一首（非旧库曲目），实际 {first:?}"
        );
        let _ = std::fs::remove_dir_all(&d1);
        let _ = std::fs::remove_dir_all(&d2);
    }
}

