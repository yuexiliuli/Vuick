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
//! 刷新进度/均衡器（path 限制：暂停/空闲不持续重绘，K12）；③ toast 过期清理。
//! 焦点：根 `track_focus` + `on_key_down` 处理全局快捷键（Space/←/→/Esc/F/1/2/3），
//! 搜索框持焦时让路（不抢输入键）。

pub mod theme;
mod bottom;
mod fullscreen;
mod icon;
mod main;
mod right;
mod sidebar;
mod widgets;

use std::cell::Cell;
use std::ops::Range;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{
    App, AnyElement, Bounds, ClipboardItem, Context, CursorStyle, DefiniteLength, Entity,
    EntityInputHandler, EventEmitter, Focusable, FocusHandle, InteractiveElement as _, IntoElement,
    KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, ParentElement as _, Point, Pixels,
    Render, SharedString, Styled as _, TextInputConfiguration, UTF16Selection, Window, div, point,
    px, size,
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

/// 全屏浮层右侧 tab。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum FsTab {
    /// 控制（进度/控制/音量）。
    #[default]
    Controls,
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
    /// 全屏浮层右侧 tab。
    pub fs_tab: FsTab,
    /// 进度条悬停 x（窗口坐标；预留扩展）。
    #[allow(dead_code)]
    pub hover_x: Option<f32>,
    /// 导航「返回」占位（预留）。
    #[allow(dead_code)]
    pub back_off: bool,
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
    // ---- 可交互条（进度/音量）的窗口坐标盒（canvas prepaint 记录） ----
    np_progress: Rc<Cell<Bounds<Pixels>>>,
    bb_progress: Rc<Cell<Bounds<Pixels>>>,
    fs_progress: Rc<Cell<Bounds<Pixels>>>,
    bb_vol: Rc<Cell<Bounds<Pixels>>>,
    fs_vol: Rc<Cell<Bounds<Pixels>>>,
    // ---- 悬停状态（hover 触发一次重绘即可，无需持续动画） ----
    hover_mix: Option<usize>,
}

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
        }
    }

    /// 常驻 tick（250ms）：消费 worker 事件（内部已 notify）+ 播放中刷新进度 +
    /// toast 过期清理。空闲（非播放）不 notify → 不持续重绘（K12）。
    fn on_tick(&mut self, cx: &mut Context<Self>) {
        self.player.update(cx, |p, cx| p.tick(cx));
        // 播放错误浮现（如缺 video_ffmpeg.dll / 打开失败）：仅在错误值**变化**时弹一次
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
        // 播放中才高频刷新进度条/均衡器位置；toast 在期也刷新（保证按时出现/消失）。
        if self.player.read(cx).playing() || self.toast.is_some() {
            cx.notify();
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
        cx.notify();
    }

    fn set_section(&mut self, section: MainSection, cx: &mut Context<Self>) {
        self.state.section = section;
        cx.notify();
    }

    fn set_right_tab(&mut self, tab: RightTab, cx: &mut Context<Self>) {
        self.state.right_tab = tab;
        cx.notify();
    }

    fn set_fs_tab(&mut self, tab: FsTab, cx: &mut Context<Self>) {
        self.state.fs_tab = tab;
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
            return;
        }
        if let Some(f) = fraction_at(&bounds, ev.position) {
            self.apply_strip(is_seek, f, cx);
        }
    }

    /// 应用拖拽比例：进度 → seek(f×duration)；音量 → set_volume(f)。
    fn apply_strip(&mut self, is_seek: bool, f: f32, cx: &mut Context<Self>) {
        if is_seek {
            let dur = self.player.read(cx).duration();
            self.player.update(cx, |p, cx| p.seek(dur * f as f64, cx));
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
