//! 音乐播放器全体 UI（Phase B）：`MusicView` 视图 + 布局 + 键盘 + 播放器接线。
//!
//! 视觉逐字复刻 `prototype-orbitrum-player-v1.html`（深紫暗色、三栏 + 底部播放栏、
//! 全屏浮层），调色板/渐变/图标都**自含于本 crate**（见 [`theme`]/[`icon`]），
//! 不依赖 `ui` 等其它 crate。播放逻辑全走 Phase A 的 [`crate::player::MusicPlayer`]，
//! 本模块只做 view：读取状态、派发播放器动作、渲染子视图。
//!
//! 布局：根 = flex 列 → `[侧栏 240 | 主区 flex_1 | 右栏 320]`（flex 行）+ 底部播放栏
//! 80px；`fs_open` 时在末尾挂全屏浮层（absolute inset-0，最后子元素 → 绘制/命中最上）。
//!
//! 驱动：`new` 起一个常驻 `cx.spawn` tick 循环（250ms）——① `player.tick` 消费 worker
//! 事件（Opened/Finished/Error，事件变化内部已 notify）；② 仅**播放中**才 `cx.notify()`
//! 刷新进度/均衡器/歌词/封面（path 限制：暂停/空闲不持续重绘，K12）；③ toast 过期清理。
//! 焦点：根 `track_focus` + `on_key_down` 处理全局快捷键（Space/←/→/Esc/F/1/2/3），
//! 搜索框持焦时让路（不抢输入键）。
//! 封面：`cover.rs` 解码同名 `.jpg/.png/.webp` 成 `RenderImage`（BGRA），本文件用
//! `covers` 缓存 + 懒加载（后台线程解码 → 主线程入缓存 + notify）；渲染位（全屏封面/
//! 正在播放/底栏缩略图/列表缩略图）一律先问 [`MusicView::cover_for`]，None 回退渐变。

pub mod theme;
mod bottom;
mod fullscreen;
mod icon;
mod main;
mod right;
mod sidebar;
mod widgets;

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    App, AnyElement, Bounds, ClipboardItem, Context, CursorStyle, DefiniteLength, Entity,
    EntityInputHandler, EventEmitter, Focusable, FocusHandle, InteractiveElement as _, IntoElement,
    KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ObjectFit,
    ParentElement as _, Point, Pixels, Render, RenderImage, ScrollHandle, SharedString, Styled as _,
    StyledImage, TextInputConfiguration, UTF16Selection, Window, div, img, point, px, size,
};

use crate::model::{RepeatMode, Track};
use crate::player::MusicPlayer;

/// 侧栏主导航分区。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum MainSection {
    /// 首页（推荐 Mix + 热门歌曲表）。
    #[default]
    Home,
    /// 音乐库（按艺术家分组）。
    Library,
    /// 收藏（只列已收藏）。
    Liked,
}

/// 右侧「正在播放」面板 tab。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum RightTab {
    /// 歌曲（大封面 + 控制）。
    #[default]
    Song,
    Lyrics,
    Queue,
}

/// 视图本地状态（播放器状态在 [`MusicPlayer`] 里读）。
#[derive(Default)]
pub struct ViewState {
    /// 主导航分区（首页/音乐库/收藏）。
    pub section: MainSection,
    /// 右侧面板 tab（歌曲/歌词/队列）。
    pub right_tab: RightTab,
    /// 主区搜索词（输入即过滤）。
    pub search: String,
    /// 全屏浮层是否打开。
    pub fs_open: bool,
    /// 进度条悬停 x（窗口坐标；预留扩展）。
    #[allow(dead_code)]
    pub hover_x: Option<f32>,
    /// 导航「返回」占位（预留）。
    #[allow(dead_code)]
    pub back_off: bool,
    // ---- 进度条拖拽交互态（对齐视频 scrub 状态机，音频-only 适配） ----
    /// 进度条是否正在拖拽（左键按下未松开）。
    pub scrubbing: bool,
    /// 拖拽当前锁定的落点（秒）：逐 move 更新，松手按此收尾提交（「松手停在当前位置」）。
    pub scrub_target: f64,
    /// 本次拖拽已向 worker 真实 seek 过的落点：松手同落点不重复 seek（无杂音）。
    pub scrub_seeked: Option<f64>,
    /// 拖拽节流：上次真实 seek 的时刻（密集 move 防刷爆 worker/杂音，对齐视频
    /// `SCRUB_RETARGET_MS`）。节流窗口内每 move 只碰显示基准，不影响跟手。
    pub last_scrub_retarget: Option<Instant>,
}

/// 音乐视图事件（shell 订阅退出音乐模式）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MusicEvent {
    /// 请求退出音乐模式（非全屏按 Esc 时发出）。
    ExitRequested,
}

/// toast 常显时长（ms），原型 `.toast` 1.8s。
const TOAST_MS: Duration = Duration::from_millis(1800);

/// 常驻 tick 周期（ms）：进度刷新节拍（约束 ≤250ms 才 notify，防持续逐帧重绘）。
const TICK_INTERVAL_MS: u64 = 250;

/// 音乐播放器全 UI 视图。
pub struct MusicView {
    /// 播放器实体（唯一状态源）。
    pub player: Entity<MusicPlayer>,
    /// 视图本地状态。
    pub state: ViewState,
    focus_handle: FocusHandle,
    /// 主区搜索框焦点（输入键让路/IME 注册用）。
    search_focus: FocusHandle,
    /// 主区搜索 IME 组合中（marked）区间（utf16）；None = 无组合。
    search_marked: Option<Range<usize>>,
    /// 中央 toast：`(文案, 出现时刻)`，超时由 tick/render 清理。
    toast: Option<(SharedString, Instant)>,
    /// 上次已向用户浮现的播放错误（防每次 tick 重复弹 toast）。
    last_error: Option<String>,
    /// 常驻 tick 循环句柄（view 销毁即取消）。
    _tick: Option<gpui::Task<()>>,
    // ---- 封面图像缓存（懒加载 + 库预取；后台解码 → 主线程入缓存） ----
    /// 已解码成功的封面（path → BGRA `RenderImage`，供渲染复用）。
    covers: HashMap<PathBuf, Arc<RenderImage>>,
    /// 正在解码的封面 path（防重复派发任务）。
    cover_loading: HashSet<PathBuf>,
    // ---- 可交互条（进度/音量）的窗口坐标盒（canvas prepaint 记录） ----
    np_progress: Rc<Cell<Bounds<Pixels>>>,
    bb_progress: Rc<Cell<Bounds<Pixels>>>,
    fs_progress: Rc<Cell<Bounds<Pixels>>>,
    bb_vol: Rc<Cell<Bounds<Pixels>>>,
    fs_vol: Rc<Cell<Bounds<Pixels>>>,
    // ---- 悬停状态（hover 触发一次重绘即可，无需持续动画） ----
    hover_mix: Option<usize>,
    /// 全屏浮层当前悬停元素 id（按钮高亮/进度条圆点显示用；同一指针只有一个悬停）。
    fs_hover: Option<&'static str>,
    // ---- 歌词面板滚动（可滚浏览全部 + 自动跟随当前句） ----
    /// 歌词滚动容器句柄（`overflow_y_scroll` + `track_scroll`；原生滚轮与自动跟随
    /// 共用这一个 offset——跟随只写它、放手动时保持用户位置不动它）。
    lyrics_scroll: ScrollHandle,
    /// 上次经自动跟随写入的目标偏移（句首坐标 y，offset 取其负值；None=当前非跟随态，
    /// 如已手动滚动/暂停/未布局）。用于检测「用户手动滚动」：实测 offset 偏离它即
    /// 记 `lyrics_manual_since` 暂停跟随。
    lyrics_follow_y: Option<f32>,
    /// 手动浏览开始时刻（来自 `cx.background_executor().now()`，测试的虚拟时钟可控）。
    /// 停止滚动约 3 秒（[`super::fullscreen::LYRIC_FOLLOW_RESUME_MS`]）后自动恢复跟随。
    lyrics_manual_since: Option<Instant>,
    /// 手动浏览期间上次观察到的滚动偏移（仍在滚动 → 刷新 3 秒计时）。
    lyrics_manual_last_y: f32,
    /// 当前参与歌词跟随的曲目路径（切歌/换曲时复位跟随态，新曲从自动跟随开始）。
    lyrics_track_path: Option<PathBuf>,
}

/// 进度条拖拽真实 seek 节流（ms）：与视频查看器同量级；节流窗口内只更新显示基准
/// （`set_seek_base` 即时跟手），真实 codec.seek + flush 降到节流粒度，防拖拽刷爆 worker/杂音。
const SCRUB_RETARGET_MS: u64 = 60;

impl MusicView {
    /// 新建音乐视图：建焦点句柄 + 起常驻 tick 循环（不在此 bind keys——根部
    /// `on_key_down` 统一处理全局快捷键）。
    pub fn new(player: Entity<MusicPlayer>, cx: &mut Context<Self>) -> Self {
        let weak = cx.weak_entity();
        let task = cx.spawn(async move |_, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(TICK_INTERVAL_MS))
                    .await;
                let _ = weak.update(cx, |this, cx| this.on_tick(cx));
            }
        });
        Self {
            player,
            state: ViewState::default(),
            focus_handle: cx.focus_handle(),
            search_focus: cx.focus_handle(),
            search_marked: None,
            toast: None,
            last_error: None,
            _tick: Some(task),
            np_progress: Rc::new(Cell::new(Bounds::default())),
            bb_progress: Rc::new(Cell::new(Bounds::default())),
            fs_progress: Rc::new(Cell::new(Bounds::default())),
            bb_vol: Rc::new(Cell::new(Bounds::default())),
            fs_vol: Rc::new(Cell::new(Bounds::default())),
            hover_mix: None,
            fs_hover: None,
            covers: HashMap::new(),
            cover_loading: HashSet::new(),
            lyrics_scroll: ScrollHandle::new(),
            lyrics_follow_y: None,
            lyrics_manual_since: None,
            lyrics_manual_last_y: 0.0,
            lyrics_track_path: None,
        }
    }

    /// 全屏元素悬停状态：hover 进入置 id、离开清空（同一指针同时只悬停一个元素）。
    /// 触发一次 notify 让高亮/进度圆点立即更新。
    pub(super) fn set_fs_hover(
        &mut self,
        id: &'static str,
        hovered: bool,
        cx: &mut Context<Self>,
    ) {
        self.fs_hover = if hovered { Some(id) } else { None };
        cx.notify();
    }

    /// 常驻 tick（250ms）：消费 worker 事件（内部已 notify）+ 播放中刷新进度 +
    /// toast 过期清理。空闲（非播放）不 notify → 不持续重绘（K12）。
    fn on_tick(&mut self, cx: &mut Context<Self>) {
        self.player.update(cx, |p, cx| p.tick(cx));
        // 封面懒加载 + 库预取：每拍扫描库缺失封面的曲目派解码任务（幂等；58 首每 250ms
        // 查 HashSet 开销可忽略）。不用一次性 latch——「库晚于首次 tick 才 load / 事后放图」
        // 时旧实现预取空转 + latch 卡死，封面永不呈现（K29 类）。
        self.ensure_library_covers(cx);
        // 播放错误浮现（如缺 media_ffmpeg.dll / 打开失败）：仅在错误值**变化**时弹一次
        // toast，避免每拍重复；错误清除后记录复位（下次再错能再弹）。
        let err = self.player.read(cx).error().map(ToString::to_string);
        if err.as_deref() != self.last_error.as_deref() {
            self.last_error = err.clone();
            if let Some(e) = err {
                self.toast = Some((format!("播放失败：{e}").into(), Instant::now()));
            }
        }
        // toast 过期：清掉（下次渲染可能不会发生，主动 notify 一次让其消失）
        let toast_expired =
            self.toast.as_ref().is_some_and(|(_, at)| at.elapsed() >= TOAST_MS);
        if toast_expired {
            self.toast = None;
        }
        // 歌词自动跟随：播放中把滚动偏移钉到「active 行居中」；检测到用户手动滚动则
        // 暂停跟随、停止滚动约 3 秒后恢复（指挥权在这是为了避开 render 的无副作用纪律）。
        self.update_lyrics_follow(cx);
        // 播放中才高频刷新进度条/均衡器位置；toast 在期也刷新（保证按时出现/消失）。
        if self.player.read(cx).playing() || self.toast.is_some() {
            cx.notify();
        }
    }

    // ---- 封面图像懒加载（解码在后台线程；渲染只读缓存，None 回退渐变） ----

    /// 懒加载某曲封面：未缓存/未在途 → 派后台解码任务（幂等，`cover_loading` 防重）。
    /// 任务必须 `.detach()`：gpui `Task` 句柄 drop 即取消 future——旧实现 `let _ =
    /// cx.spawn(...)` 把任务当场取消（`cover_loading` 卡死、封面永不入缓存，真机三处
    /// 全渐变占位的根因）。解码放 `background_executor`（240ms 级大图不卡主线程），
    /// 完成后回主线程入缓存 + notify。
    fn spawn_cover_decode(&mut self, path: &Path, cx: &mut Context<Self>) {
        if self.covers.contains_key(path) || self.cover_loading.contains(path) {
            return;
        }
        self.cover_loading.insert(path.to_path_buf());
        let weak = cx.weak_entity();
        let p = path.to_path_buf();
        let p2 = p.clone();
        cx.spawn(async move |_, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { crate::cover::cover_from_file(&p) })
                .await;
            let _ = weak.update(cx, |this, cx| {
                this.cover_loading.remove(&p2);
                if let Ok(render) = result {
                    this.covers.insert(p2.clone(), render);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 纯读：某曲封面位图（None = 无封面/尚未就绪，渲染回退渐变）。
    pub(super) fn cover_for(&self, path: Option<&Path>) -> Option<Arc<RenderImage>> {
        let path = path?;
        self.covers.get(path).cloned()
    }

    /// 当前播放曲目的封面（None = 无封面/尚未就绪）。
    pub(super) fn cover_for_current(&mut self, cx: &mut Context<Self>) -> Option<Arc<RenderImage>> {
        let path = self
            .player
            .read(cx)
            .current_track()
            .and_then(|t| t.cover_path.clone());
        self.cover_for(path.as_deref())
    }

    /// 每 tick 扫描全库「缺失封面」的曲目并派解码任务（预取 + 当前曲兜底合一）：
    /// 已在缓存 / 已在途的跳过；新曲、换目录（`load_dir` 重扫）、事后放图都能在下一拍补上。
    /// 58 首 × HashSet contains_key 每 250ms 开销可忽略，不做一次性 latch。
    fn ensure_library_covers(&mut self, cx: &mut Context<Self>) {
        let paths: Vec<PathBuf> = self
            .player
            .read(cx)
            .library()
            .iter()
            .filter_map(|t| t.cover_path.clone())
            .filter(|p| !self.covers.contains_key(p) && !self.cover_loading.contains(p))
            .collect();
        for p in paths {
            self.spawn_cover_decode(&p, cx);
        }
    }

    /// 通用封面渲染：有图 → 铺满的 `img`（`ObjectFit::Cover`）；无图 → `fallback`。
    pub(super) fn render_cover_or(
        &self,
        cover: Option<Arc<RenderImage>>,
        fallback: AnyElement,
    ) -> AnyElement {
        match cover {
            Some(render) => div()
                .size_full()
                .child(
                    img(gpui::ImageSource::Render(render))
                        .object_fit(ObjectFit::Cover)
                        .w_full()
                        .h_full(),
                )
                .into_any_element(),
            None => fallback,
        }
    }

    /// 先聚焦根（任意在 view 内点按都回焦点，除搜索框本身——其 `stop_propagation`）。
    fn on_root_mouse_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if ev.button == MouseButton::Left {
            self.focus_handle.focus(window, cx);
        }
    }

    /// 全局快捷键（root `on_key_down`；搜索框持焦时让路）。`stop_propagation` 防漏给
    /// 下层（编辑器等外部）。
    fn on_key(&mut self, e: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.search_focus.is_focused(window) {
            return; // 输入框持焦：全局快捷键放行（输入键由搜索框自处理）
        }
        match e.keystroke.key.as_str() {
            "space" => self.toggle_play(cx),
            "left" => self.player.update(cx, |p, cx| p.prev(cx)),
            "right" => self.player.update(cx, |p, cx| p.next(cx)),
            "escape" => {
                if self.state.fs_open {
                    self.state.fs_open = false;
                    cx.notify();
                } else {
                    cx.emit(MusicEvent::ExitRequested);
                }
            }
            "f" => self.toggle_fs(cx),
            "1" => self.set_section(MainSection::Home, cx),
            "2" => self.set_section(MainSection::Library, cx),
            "3" => self.set_section(MainSection::Liked, cx),
            _ => return,
        }
        cx.stop_propagation();
    }

    // ---- 对外访问器 ----
    pub fn focus_handle(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    /// 中央 toast（1.8s 自动消失）。
    pub fn toast_msg(&mut self, s: impl Into<String>) {
        self.toast = Some((s.into().into(), Instant::now()));
    }

    /// 载入默认音乐目录（shell 集成入口）：先读 `UserPrefs::music_dir`（未设置/空 →
    /// [`config::limits::DEFAULT_MUSIC_DIR`] 兜底），随后 `MusicPlayer::load_dir`。
    /// 目录不存在/为空 → 库为空（UI 显示空态），不报错。
    pub fn load_default_dir(&mut self, cx: &mut Context<Self>) {
        let dir = config::user::UserPrefs::load()
            .music_dir
            .filter(|d| !d.is_empty())
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(config::limits::DEFAULT_MUSIC_DIR));
        self.player.update(cx, |p, cx| p.load_dir(dir, cx));
    }

    // ---- 播放器接线（view → player，均极薄转发） ----
    pub fn toggle_play(&mut self, cx: &mut Context<Self>) {
        self.player.update(cx, |p, cx| p.toggle(cx));
    }

    pub fn handle_play_idx(&mut self, idx: usize, cx: &mut Context<Self>) {
        if idx >= self.player.read(cx).library().len() {
            return;
        }
        self.player.update(cx, |p, cx| p.play_track(idx, cx));
        if let Some(t) = self.player.read(cx).current_track() {
            self.toast_msg(format!("正在播放：{} - {}", t.title, t.artist));
        }
    }

    pub fn toggle_like(&mut self, idx: usize, cx: &mut Context<Self>) {
        let was = self.player.read(cx).is_liked(idx);
        self.player.update(cx, |p, cx| p.toggle_like(idx, cx));
        self.toast_msg(if was { "取消收藏" } else { "已收藏" });
    }

    /// 收藏/取消收藏当前播放曲目（右侧面板「收藏」钮）。
    fn like_current(&mut self, cx: &mut Context<Self>) {
        if let Some(i) = self.player.read(cx).current() {
            self.toggle_like(i, cx);
        }
    }

    pub fn cycle_repeat(&mut self, cx: &mut Context<Self>) {
        let next = match self.player.read(cx).repeat() {
            RepeatMode::Off => RepeatMode::All,
            RepeatMode::All => RepeatMode::One,
            RepeatMode::One => RepeatMode::Off,
        };
        self.player.update(cx, |p, cx| p.set_repeat(next, cx));
    }

    pub fn toggle_shuffle(&mut self, cx: &mut Context<Self>) {
        self.player.update(cx, |p, cx| p.toggle_shuffle(cx));
    }

    pub fn toggle_fs(&mut self, cx: &mut Context<Self>) {
        self.state.fs_open = !self.state.fs_open;
        // 全屏切换：复位歌词跟随/手动态（换渲染上下文从自动跟随重新开始）。
        self.lyrics_manual_since = None;
        self.lyrics_follow_y = None;
        cx.notify();
    }

    fn set_section(&mut self, section: MainSection, cx: &mut Context<Self>) {
        self.state.section = section;
        cx.notify();
    }

    fn set_right_tab(&mut self, tab: RightTab, cx: &mut Context<Self>) {
        self.state.right_tab = tab;
        // 面板「歌词」tab 重进：复位歌词跟随/手动态（进入即从自动跟随当前句开始）。
        self.lyrics_manual_since = None;
        self.lyrics_follow_y = None;
        cx.notify();
    }

    // ---- 可交互条（进度/音量 share）：条元素 + 事件 → 播放器 ----

    /// 构建一条可交互条（seek 进度条 / 音量条共用，对齐 video `strip_element`）：
    /// `id` 元素级唯一；`bounds` 由 canvas prepaint 记录窗口坐标盒；`is_seek=true`
    /// 拖拽 → `player.seek(f×duration)`，否则 `player.set_volume(f)`。
    fn strip_element(
        &mut self,
        id: &'static str,
        bounds: Rc<Cell<Bounds<Pixels>>>,
        fraction: f32,
        is_seek: bool,
        width: Option<f32>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let f = fraction.clamp(0.0, 1.0);
        let fill_col = theme::accent();
        let track_col = theme::bg_hover();
        let mut el = div()
            .id(id)
            .h(px(16.))
            .relative()
            .cursor(CursorStyle::PointingHand)
            .child(
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
                            .rounded(px(2.))
                            .bg(track_col.opacity(0.6))
                            .child(
                                div()
                                    .absolute()
                                    .left_0()
                                    .top_0()
                                    .bottom_0()
                                    .w(DefiniteLength::Fraction(f))
                                    .rounded(px(2.))
                                    .bg(fill_col),
                            ),
                    ),
            )
            .child(theme::bounds_canvas(bounds.clone()));
        if let Some(w) = width {
            el = el.w(px(w));
        } else {
            el = el.flex_1();
        }
        let b1 = bounds.clone();
        el = el.on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, ev, _w, cx| this.on_strip_down(is_seek, b1.clone(), ev, cx)),
        );
        let b2 = bounds.clone();
        el = el.on_mouse_move(
            cx.listener(move |this, ev, _w, cx| this.on_strip_drag(is_seek, b2.clone(), ev, cx)),
        );
        // 松手收尾（进度条）：窗口内松开 / 拖出窗外松开都按最后锁定落点提交，保证
        // 「松手停在当前位置」。音量条无需收尾（on_strip_up 对 !is_seek 直接返回）。
        let b3 = bounds.clone();
        el = el.on_mouse_up(
            MouseButton::Left,
            cx.listener(move |this, ev, _w, cx| this.on_strip_up(is_seek, b3.clone(), ev, cx)),
        );
        el = el.on_mouse_up_out(
            MouseButton::Left,
            cx.listener(move |this, ev, _w, cx| this.on_strip_up(is_seek, bounds.clone(), ev, cx)),
        );
        el.into_any_element()
    }

    fn on_strip_down(
        &mut self,
        is_seek: bool,
        bounds: Rc<Cell<Bounds<Pixels>>>,
        ev: &MouseDownEvent,
        cx: &mut Context<Self>,
    ) {
        if let Some(f) = fraction_at(&bounds, ev.position) {
            if is_seek {
                // 点按即跳：进拖拽态并以该点为基准立即真实 seek（显示由 seek_base 即时到落点）。
                self.state.scrubbing = true;
                self.state.last_scrub_retarget = Some(Instant::now());
                // 同步钉住播放器显示基准：拖拽中 position = 手指落点，不随时钟漂移。
                self.player.update(cx, |p, cx| p.set_scrubbing(true, cx));
            }
            self.apply_strip(is_seek, f, cx);
        }
    }

    fn on_strip_drag(
        &mut self,
        is_seek: bool,
        bounds: Rc<Cell<Bounds<Pixels>>>,
        ev: &MouseMoveEvent,
        cx: &mut Context<Self>,
    ) {
        if !ev.dragging() {
            // 已松开但 mouse_up 未送达本条（如拖出窗外释放）→ 按最后锁定落点收尾
            // （对齐视频 `on_seek_move` 的 `!dragging && scrubbing` 兜底）。
            if self.state.scrubbing {
                self.end_strip_scrub(cx);
            }
            return;
        }
        if let Some(f) = fraction_at(&bounds, ev.position) {
            if is_seek && self.state.scrubbing {
                self.seek_to_fraction(f, false, cx);
            } else if !is_seek {
                self.apply_strip(false, f, cx);
            }
        }
    }

    /// 进度条松手收尾（窗口内 `on_mouse_up` / 拖出窗外 `on_mouse_up_out` 共用）：
    /// 窗口内优先用松手位置的精确分数刷新落点；窗外（fraction_at None）沿用最后锁定
    /// 落点。随后按最终落点提交，「松手停在当前位置」。
    fn on_strip_up(
        &mut self,
        is_seek: bool,
        bounds: Rc<Cell<Bounds<Pixels>>>,
        ev: &MouseUpEvent,
        cx: &mut Context<Self>,
    ) {
        if !is_seek || !self.state.scrubbing {
            return;
        }
        if let Some(f) = fraction_at(&bounds, ev.position) {
            let dur = self.player.read(cx).duration();
            self.state.scrub_target = dur * f as f64;
        }
        self.end_strip_scrub(cx);
    }

    /// 拖拽收尾：若最终落点尚未真实 seek（节流窗口内直接松手）→ 补发最后一次精确
    /// 提交；否则（已 seek 过同落点）只退出拖拽态，不重复 seek（无杂音）。
    fn end_strip_scrub(&mut self, cx: &mut Context<Self>) {
        if !self.state.scrubbing {
            return;
        }
        let target = self.state.scrub_target;
        self.state.scrubbing = false;
        // 先解锁播放器显示基准（拖拽钉扎解除），再按最终落点提交真实 seek。
        self.player.update(cx, |p, cx| p.set_scrubbing(false, cx));
        if self.state.scrub_seeked != Some(target) {
            self.state.scrub_seeked = Some(target);
            self.player.update(cx, |p, cx| p.seek(target, cx));
        }
        cx.notify();
    }

    /// 把进度条分数应用为 seek 落点：`force=true`（点按/松手收尾）无条件真实 seek；
    /// `false`（拖动）走 [`SCRUB_RETARGET_MS`] 节流——节流窗口内只更新显示基准
    /// （`set_seek_base`，显示由 seek_base 即时跟手，不依赖 worker）。
    fn seek_to_fraction(&mut self, f: f32, force: bool, cx: &mut Context<Self>) {
        let dur = self.player.read(cx).duration();
        let target = dur * f as f64;
        self.state.scrub_target = target;
        if force {
            self.state.last_scrub_retarget = Some(Instant::now());
            self.state.scrub_seeked = Some(target);
            self.player.update(cx, |p, cx| p.seek(target, cx));
            return;
        }
        let due = match self.state.last_scrub_retarget {
            Some(t) => t.elapsed() >= Duration::from_millis(SCRUB_RETARGET_MS),
            None => true,
        };
        if due {
            self.state.last_scrub_retarget = Some(Instant::now());
            self.state.scrub_seeked = Some(target);
            self.player.update(cx, |p, cx| p.seek(target, cx));
        } else {
            // 节流窗口内：显示仍逐 move 实时跟手（仅更新显示基准，不发 worker seek）。
            self.player.update(cx, |p, cx| p.set_seek_base(target, cx));
        }
    }

    /// 应用拖拽比例（点按/音量共用入口）：进度 → 立即真实 seek(f×duration) 并进拖拽态
    /// 状态由调用方先置好；音量 → set_volume(f)。
    fn apply_strip(&mut self, is_seek: bool, f: f32, cx: &mut Context<Self>) {
        if is_seek {
            self.seek_to_fraction(f, true, cx);
        } else {
            self.player.update(cx, |p, cx| p.set_volume(f, cx));
        }
    }

    // ---- toast 浮层 ----
    fn render_toast(&mut self) -> AnyElement {
        let Some((msg, _)) = &self.toast else {
            return div().into_any_element();
        };
        div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .px_7()
                    .py_3()
                    .rounded(px(12.))
                    .bg(theme::glass())
                    .border_1()
                    .border_color(theme::accent())
                    .text_size(px(14.))
                    .text_color(theme::text())
                    .child(msg.clone()),
            )
            .into_any_element()
    }
}

impl Focusable for MusicView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<MusicEvent> for MusicView {}

impl Render for MusicView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // toast 到期：render 侧兜底清理（tick 正常时已清，这里防万一）。
        if self.toast.as_ref().is_some_and(|(_, at)| at.elapsed() >= TOAST_MS) {
            self.toast = None;
        }
        let mut root = div()
            .id("music-view")
            .debug_selector(|| "music-view".into())
            .size_full()
            .relative()
            .overflow_hidden()
            .flex()
            .flex_col()
            .bg(theme::bg_deep())
            .text_color(theme::text())
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::on_key))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_root_mouse_down));
        // 上三栏（侧栏 240 / 主区 flex_1 / 右栏 320）+ 底部播放栏 80。
        root = root
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_h(px(0.))
                    .child(self.render_sidebar(window, cx))
                    .child(self.render_main(window, cx))
                    .child(self.render_right(window, cx)),
            )
            .child(self.render_bottom(window, cx));
        // 全屏浮层：最后子元素 → 绘制/命中最上层（覆盖三栏 + 底栏）。
        if self.state.fs_open {
            root = root.child(self.render_fullscreen(window, cx));
        }
        // toast 最顶层。
        root = root.child(self.render_toast());
        root
    }
}

/// 搜索框 IME（中文）输入落点：替换 `range`（None = 替换 marked/光标处）文本。
/// `mark=true` 记下组合区间（Windows 组合更新以 None 区间替换它，防「拼音历史拼接」）。
impl MusicView {
    fn ime_replace(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        mark: bool,
        cx: &mut Context<Self>,
    ) {
        let s = &self.state.search;
        let total = s.encode_utf16().count();
        let (start, end) = match &range {
            Some(r) => (r.start.min(total), r.end.min(total)),
            None => match &self.search_marked {
                Some(m) => (m.start.min(total), m.end.min(total)),
                None => (total, total),
            },
        };
        let bs = utf16_to_byte(s, start);
        let be = utf16_to_byte(s, end).max(bs);
        let mut new = String::with_capacity(s.len() + text.len());
        new.push_str(&s[..bs]);
        new.push_str(text);
        new.push_str(&s[be..]);
        self.state.search = new;
        if mark {
            let bs_u16 = byte_to_utf16(&self.state.search, bs);
            let len = text.encode_utf16().count();
            self.search_marked = Some(bs_u16..bs_u16 + len);
        } else {
            self.search_marked = None;
        }
        cx.notify();
    }
}

/// utf16 下标 → 字节下标（char 边界）。
fn utf16_to_byte(s: &str, u16_idx: usize) -> usize {
    let mut n = 0usize;
    for (bi, ch) in s.char_indices() {
        if n >= u16_idx {
            return bi;
        }
        n += ch.len_utf16();
    }
    s.len()
}

/// 字节下标 → utf16 下标（`byte_idx` 必须是字符边界）。
fn byte_to_utf16(s: &str, byte_idx: usize) -> usize {
    s.get(..byte_idx.min(s.len()))
        .map(|x| x.encode_utf16().count())
        .unwrap_or(0)
}

/// 搜索框 IME/文本输入：`main.rs` 的 canvas 在**搜索持焦**时经 `ElementInputHandler`
/// 注册本实现（组合文本直接落 `state.search`，与 ASCII 直插同一数据源）。
impl EntityInputHandler for MusicView {
    fn accepts_text_input(&self, _w: &mut Window, _cx: &mut Context<Self>) -> bool {
        true
    }

    fn text_length_utf16(&mut self, _w: &mut Window, _cx: &mut Context<Self>) -> Option<usize> {
        Some(self.state.search.encode_utf16().count())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _w: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let len = self.state.search.encode_utf16().count();
        Some(UTF16Selection {
            range: len..len,
            reversed: false,
        })
    }

    fn marked_text_range(&self, _w: &mut Window, _cx: &mut Context<Self>) -> Option<Range<usize>> {
        self.search_marked.clone()
    }

    fn text_for_range(
        &mut self,
        range: Range<usize>,
        _adjusted_range: &mut Option<Range<usize>>,
        _w: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let s = &self.state.search;
        let b0 = utf16_to_byte(s, range.start);
        let b1 = utf16_to_byte(s, range.end);
        if b0 <= b1 && b1 <= s.len() {
            Some(s[b0..b1].to_string())
        } else {
            None
        }
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.ime_replace(range, text, false, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        new_text: &str,
        _new_selected_range: Option<Range<usize>>,
        _w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.ime_replace(range, new_text, true, cx);
    }

    fn unmark_text(&mut self, _w: &mut Window, _cx: &mut Context<Self>) {
        self.search_marked = None;
    }

    fn bounds_for_range(
        &mut self,
        _range_utf16: Range<usize>,
        element_bounds: Bounds<Pixels>,
        _w: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        // 简化：光标列 = 元素左缘 + 3px（中文 IME 候选窗跟随近似）。
        Some(Bounds::new(
            point(element_bounds.origin.x + px(3.), element_bounds.origin.y + px(2.)),
            size(px(2.), element_bounds.size.height - px(4.)),
        ))
    }

    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _w: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        // 简化：单行框光标恒在末尾（不做点击摆光标）。
        Some(self.state.search.encode_utf16().count())
    }

    fn set_selected_text_range(
        &mut self,
        _range_utf16: Range<usize>,
        _w: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn paste(&mut self, item: ClipboardItem, _w: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = item.text() {
            self.ime_replace(None, &text, false, cx);
        }
    }

    fn text_input_configuration(
        &mut self,
        _w: &mut Window,
        _cx: &mut Context<Self>,
    ) -> TextInputConfiguration {
        TextInputConfiguration::default()
    }
}

/// 坐标盒 → 横向比例（x 出条返回 None；未布局宽<=0 返回 None）。
fn fraction_at(bounds: &Rc<Cell<Bounds<Pixels>>>, pos: Point<Pixels>) -> Option<f32> {
    let b = bounds.get();
    if b.size.width.as_f32() <= 0.0 {
        return None;
    }
    let left = b.origin.x.as_f32();
    let right = (b.origin.x + b.size.width).as_f32();
    let x = pos.x.as_f32();
    if x < left || x > right {
        return None;
    }
    Some(((x - left) / (right - left)).clamp(0.0, 1.0))
}

/// 主区搜索过滤纯函数：标题/艺术家大小写不敏感任一命中；空查询返回全部。
/// 拆出便于单测（`main.rs::tests`）。
pub fn filter_tracks_by_query(tracks: &[Track], q: &str) -> Vec<usize> {
    let ql = q.trim().to_lowercase();
    if ql.is_empty() {
        return (0..tracks.len()).collect();
    }
    tracks
        .iter()
        .enumerate()
        .filter(|(_, t)| {
            t.title.to_lowercase().contains(&ql) || t.artist.to_lowercase().contains(&ql)
        })
        .map(|(i, _)| i)
        .collect()
}

/// 列表空态：标题 + 副文案居中（主区 Library/Liked 空库提示）。
pub(super) fn empty_state(title: &str, sub: &str) -> AnyElement {
    div()
        .w_full()
        .py_10()
        .flex()
        .flex_col()
        .items_center()
        .text_color(theme::text2())
        .child(
            div()
                .text_size(px(15.))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(theme::text2())
                .child(title.to_string()),
        )
        .child(
            div()
                .pt_1()
                .text_size(px(12.))
                .text_color(theme::text3())
                .child(sub.to_string()),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests;
