//! 音乐播放器全体 UI（Phase B）：`MusicView` 视图 + 布局 + 键盘 + 播放器接线。
//!
//! 视觉逐字复刻 `prototype-orbitrum-player-v1.html`（深紫暗色、三栏 + 底部播放栏、
//! 全屏浮层），调色板/渐变/图标都**自含于本 crate**（见 [`theme`]/[`icon`]），
//! 不依赖 `ui` 等其它 crate。播放逻辑全走 Phase A 的 [`crate::player::MusicPlayer`]，
//! 本模块只做 view：读取状态、派发播放器动作、渲染子视图。
//!
//! 布局：根 = flex 列 → 顶部 Windows 风格标题栏 36px（[`titlebar`]）+ `[侧栏 240 |
//! 主区 flex_1 | 右栏 320]`（flex 行）+ 底部播放栏 80px；`fs_open` 时在末尾挂全屏浮层
//! （absolute inset-0，最后子元素 → 绘制/命中最上，覆盖标题栏/三栏/底栏）。
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
mod titlebar;
mod widgets;

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    Anchor, App, AnyElement, Bounds, ClipboardItem, Context, CursorStyle, DefiniteLength, Edges,
    Entity, EntityInputHandler, EventEmitter, Focusable, FocusHandle, InteractiveElement as _,
    IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ObjectFit,
    ParentElement as _, Point, Pixels, Render, RenderImage, ScrollHandle, SharedString, Styled as _,
    StatefulInteractiveElement as _, StyledImage, TextInputConfiguration, UTF16Selection, Window,
    anchored, deferred, div, img, point, px, size,
};
use gpui::AppContext as _;

use crate::model::{RepeatMode, Track};
use crate::player::MusicPlayer;

/// 侧栏主导航分区。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
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

/// 悬停提示显示延迟（ms）：悬停 ≥ 此值才弹名称浮层，防快速划过闪烁。
const TOOLTIP_SHOW_MS: u64 = 500;

/// tooltip 浮层与**按钮盒上缘**竖直间距（px）：`Anchor::BottomCenter` 锚在按钮盒上方
/// 该距离处（浮现在按钮正上方居中——按钮盒锚定，不依赖光标坐标）。
const TOOLTIP_GAP_PX: f32 = 10.0;

/// tooltip 的 deferred 绘制层优先级：高于普通 deferred/内容（对齐 video
/// `HOVER_TIME_LAYER_PRIORITY=120`），保证浮层绘制在 root 最上。
const TOOLTIP_LAYER_PRIORITY: usize = 120;

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
    fs_progress: Rc<Cell<Bounds<Pixels>>>,
    fs_vol: Rc<Cell<Bounds<Pixels>>>,
    /// 全屏进度条**填充渐变**的 canvas 实际绘制盒（`gradient_cover_tracked` 记录）：
    /// 供「填充随播放位置增长」回归断言（canvas 高度曾塌缩 0 → 填充隐形，真机复报五）。
    fs_fill_box: Rc<Cell<Bounds<Pixels>>>,
    // ---- 悬停状态（hover 触发一次重绘即可，无需持续动画） ----
    hover_mix: Option<usize>,
    /// 全屏浮层当前悬停元素 id（按钮高亮/进度条圆点显示用；同一指针只有一个悬停）。
    fs_hover: Option<&'static str>,
    // ---- 悬停提示（共享 tooltip）：单一浮层，根末尾渲染，纯视觉不拦事件 ----
    /// 当前按钮悬停提示：`(文案, 出现时刻)`；渲染按 [`TOOLTIP_SHOW_MS`] 延迟显示。
    /// 离开时**仅当文案与本次钮相同才清空**——同一拍内「旧钮 false + 新钮 true」谁先
    /// 谁后都收敛到新钮（换钮顺序无关）。
    tip: Option<(&'static str, Instant)>,
    /// 当前悬停按钮的窗口坐标盒（由 [`Self::tip_boxes`] 在悬停进入/换钮时拷入）：
    /// tooltip 按它锚定（不再依赖光标坐标——真机 `ev.position`/`cursor_pos` 不可靠，
    /// 曾致提示钉在固定错误位置）。
    tip_box: Option<Bounds<Pixels>>,
    /// 各悬停按钮的窗口坐标盒记录（id → cell）：按钮元素上挂 `theme::bounds_canvas`
    /// prepaint 写入（全屏 seek/音量条同款真机已验证机制）。per-id 隔离——fs 开/关、
    /// tab 切换下不同元素各写各的 cell，互不覆盖。
    tip_boxes: HashMap<&'static str, Rc<Cell<Bounds<Pixels>>>>,
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
    // ---- 主区列表虚拟化（性能优化第二步）：可视窗行 + 显示模型缓存 ----
    /// 主区列表滚动句柄（Library/Liked/Home 单一活跃滚动容器共用；`track_scroll` 绑定、
    /// 偏移驱动可见行窗——见 `main::MainList::visible_range`）。
    main_scroll: ScrollHandle,
    /// 主区「过滤/分组显示模型」缓存（`(分区, 搜索词)` → `(库变更计数, Rc<显示模型>)`）：
    /// 同一搜索词且 `player.mutation()` 不变则复用——消除每帧重过滤 + `Vec<Track>` 深克隆。
    main_list_cache: HashMap<(MainSection, String), (u64, Rc<main::MainList>)>,
    // ---- 高频重绘热区 → 纯展示子视图（性能优化第三步）：动画 notify 只落子视图 ----
    /// 主区「当前播放行」均衡器动画子视图（动画每帧只 notify 它，不再带动父整树）。
    eq_main: Entity<widgets::EqBarsView>,
    /// 右栏队列「当前项」均衡器动画子视图（与主区实例前缀隔离、互不干扰）。
    eq_queue: Entity<widgets::EqBarsView>,
    /// 右栏「正在播放」大封面黑胶三圈子视图（np-art）。
    vinyl_np: Entity<widgets::VinylView>,
    /// 全屏封面黑胶三圈子视图（fs cover；与 np 实例前缀隔离）。
    vinyl_fs: Entity<widgets::VinylView>,
    /// 底栏子视图（B 段）：整块底部播放栏拆出——自带进度心跳 + 交互回父 + tooltip 回父。
    bottom_bar: Entity<bottom::BottomBarView>,
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
        // 子视图各持一份播放器句柄（只读态；父字段移走 player 后不可再借用，先克隆）。
        let player_for_views = player.clone();
        // 底栏回父交互/tooltip 用：父视图弱柄（WeakEntity 防循环引用）。
        let parent_weak = cx.weak_entity();
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
            fs_progress: Rc::new(Cell::new(Bounds::default())),
            fs_vol: Rc::new(Cell::new(Bounds::default())),
            fs_fill_box: Rc::new(Cell::new(Bounds::default())),
            hover_mix: None,
            fs_hover: None,
            tip: None,
            tip_box: None,
            tip_boxes: HashMap::new(),
            covers: HashMap::new(),
            cover_loading: HashSet::new(),
            lyrics_scroll: ScrollHandle::new(),
            lyrics_follow_y: None,
            lyrics_manual_since: None,
            lyrics_manual_last_y: 0.0,
            lyrics_track_path: None,
            main_scroll: ScrollHandle::new(),
            main_list_cache: HashMap::new(),
            eq_main: cx.new(|_| widgets::EqBarsView::new(player_for_views.clone(), "row-current")),
            eq_queue: cx.new(|_| widgets::EqBarsView::new(player_for_views.clone(), "queue-current")),
            vinyl_np: cx
                .new(|_| {
                    widgets::VinylView::new(player_for_views.clone(), right::ART_PX)
                        .ring(0.70, Duration::from_secs(8), false, "np-vinyl-1")
                        .ring(0.50, Duration::from_secs(8), false, "np-vinyl-2")
                        .ring(0.30, Duration::from_secs(8), false, "np-vinyl-3")
                }),
            vinyl_fs: cx
                .new(|_| {
                    widgets::VinylView::new(player_for_views.clone(), fullscreen::FS_COVER_PX)
                        .ring(0.80, Duration::from_secs(12), false, "fsv-a")
                        .ring(0.55, Duration::from_secs(9), true, "fsv-b")
                        .ring(0.30, Duration::from_secs(7), false, "fsv-c")
                }),
            bottom_bar: cx.new(|cx| {
                bottom::BottomBarView::new(player_for_views.clone(), parent_weak.clone(), cx)
            }),
        }
    }

    /// 全屏元素悬停统一处理：既更新 `fs_hover`（高亮/进度圆点）也更新悬停提示
    /// （名称浮层）。`name=None` 的元素（如进度条）只管理高亮、不弹提示；离开动作
    /// 由「前一个钮」的 leave 触发清空，故移到无色区/进度条时不会残留上次的浮层。
    pub(super) fn set_fs_hover_and_tip(
        &mut self,
        id: &'static str,
        name: Option<&'static str>,
        hovered: bool,
        cx: &mut Context<Self>,
    ) {
        self.fs_hover = if hovered { Some(id) } else { None };
        match (hovered, name) {
            (true, Some(n)) => {
                self.tip = Some((n, cx.background_executor().now()));
                self.tip_box = self.tip_boxes.get(id).map(|c| c.get());
                self.schedule_tooltip_flush(cx);
            }
            // 离开只清「本元素自己」的提示（换钮同一拍 false/true 顺序无关）。
            (false, Some(n)) if self.tip.is_some_and(|(l, _)| l == n) => {
                self.tip = None;
                self.tip_box = None;
            }
            _ => {}
        }
        cx.notify();
    }

    /// 进入悬停后安排一次性延迟 notify：render 只在 notify 时重跑，暂停/空闲（无 tick
    /// notify）时仅靠悬停那一帧 notify 无法让 500ms 后的浮层绘出——到点校验仍未离开/
    /// 换钮（`tip` 仍与进入时刻一致）才 `cx.notify()` 渲染浮层；已离开/换钮则
    /// 校验失败 → 零多余重绘。
    fn schedule_tooltip_flush(&self, cx: &mut Context<Self>) {
        let Some(entered) = self.tip else {
            return;
        };
        let weak = cx.weak_entity();
        cx.spawn(async move |_, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(TOOLTIP_SHOW_MS))
                .await;
            let _ = weak.update(cx, |this, cx| {
                if this.tip == Some(entered) {
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// 按钮悬停提示通用监听工厂：进入/换钮 → `tip = Some((名称, 出现时刻))` 并把该按钮
    /// 的窗口盒拷入 `tip_box`（从 [`Self::tip_boxes`] 按 `id` 取——tooltip 锚定按钮盒，
    /// 不再依赖光标）；离开 → 若当前提示正是本钮文案则清空（同一拍「旧钮 false + 新钮
    /// true」顺序无关，收敛到新钮）。返回可直接传给 `.on_hover` 的监听器。时刻用
    /// `background_executor()` 的时钟（测试 `advance_clock` 可精确控制 [`TOOLTIP_SHOW_MS`]
    /// 延迟判定）。
    pub(super) fn hover_tip_listener(
        &mut self,
        cx: &mut Context<Self>,
        id: &'static str,
        label: &'static str,
    ) -> impl Fn(&bool, &mut Window, &mut App) + 'static {
        cx.listener(move |this: &mut MusicView, hovered: &bool, _w, cx| {
            if *hovered {
                this.tip = Some((label, cx.background_executor().now()));
                this.tip_box = this.tip_boxes.get(id).map(|c| c.get());
                this.schedule_tooltip_flush(cx);
            } else if this.tip.is_some_and(|(l, _)| l == label) {
                this.tip = None;
                this.tip_box = None;
            }
            cx.notify();
        })
    }

    /// 取某按钮的窗口盒记录 cell（懒建；per-id 隔离：不同元素各写各 cell，互不覆盖）。
    pub(super) fn tip_box_cell(&mut self, id: &'static str) -> Rc<Cell<Bounds<Pixels>>> {
        self.tip_boxes
            .entry(id)
            .or_insert_with(|| Rc::new(Cell::new(Bounds::default())))
            .clone()
    }

    /// tooltip 是否应显示（悬停已过 [`TOOLTIP_SHOW_MS`] 延迟）。render 与测试共用同一判定。
    pub(super) fn hover_tip_visible(&self, cx: &Context<Self>) -> bool {
        let Some((_, at)) = &self.tip else {
            return false;
        };
        cx.background_executor()
            .now()
            .duration_since(*at)
            >= Duration::from_millis(TOOLTIP_SHOW_MS)
    }

    /// 共享悬停提示浮层（root 末尾子元素 → 绘制在 toast 之后的独立 deferred 层 = 最上）：
    /// `tip` 存在、悬停已过 [`TOOLTIP_SHOW_MS`] 且 `tip_box` 有值 → 在**按钮盒上方**
    /// [`TOOLTIP_GAP_PX`] 处分层浮出小药丸（`Anchor::BottomCenter` 锚点 = 浮层底边中点，
    /// 横贴按钮盒水平中点、竖在按钮盒上缘之上——「浮现在按钮旁边」）。
    ///
    /// 定位用 `deferred(anchored()).position(窗口坐标)`（AGENTS §7 姿势，video
    /// `hover_time_float` 同款）：`tip_box` 是悬停按钮经 `theme::bounds_canvas` 记录的
    /// **窗口坐标盒**（prepaint 写入；与全屏 seek/音量条点拖同一套真机已验证机制），
    /// **彻底不再依赖光标**——真机 `cursor_pos`/`ev.position` 不可靠（音乐视图嵌 shell
    /// 编辑器内、事件坐标漂移 → 提示钉在固定错误位置，用户三连复报）。纯视觉：无鼠标
    /// 监听（不拦点击/滚轮）、不进焦点、不 stop_propagation。
    fn render_tooltip(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        if !self.hover_tip_visible(cx) {
            return div().into_any_element();
        }
        let Some((tip, _)) = self.tip else {
            return div().into_any_element();
        };
        let Some(b) = self.tip_box else {
            return div().into_any_element();
        };
        // 锚点 = 浮层底边中点：横贴按钮盒水平中点、竖在按钮上缘之上 GAP 处。
        let pos = point(
            b.center().x,
            px(b.origin.y.as_f32() - TOOLTIP_GAP_PX),
        );
        let pill = div()
            .debug_selector(|| "music-tooltip".into())
            .px(px(9.))
            .py(px(4.))
            .rounded(px(6.))
            .bg(theme::glass())
            .border_1()
            .border_color(theme::border())
            .shadow(vec![
                gpui::BoxShadow::new(px(0.), px(2.), gpui::black().opacity(0.4))
                    .blur_radius(px(8.)),
            ])
            .text_size(px(11.))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(theme::text())
            .child(tip.to_string());
        deferred(
            anchored()
                .anchor(Anchor::BottomCenter)
                .position(pos)
                .snap_to_window_with_margin(Edges::all(px(6.)))
                .child(pill),
        )
        .with_priority(TOOLTIP_LAYER_PRIORITY)
        .into_any_element()
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

    /// 收藏/取消收藏当前播放曲目（右侧面板「收藏」钮 / 底栏回父入口）。
    pub(super) fn like_current(&mut self, cx: &mut Context<Self>) {
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

    /// 切右侧面板 tab（右栏 header / 底栏回父入口共用）。
    pub(super) fn set_right_tab(&mut self, tab: RightTab, cx: &mut Context<Self>) {
        self.state.right_tab = tab;
        // 面板「歌词」tab 重进：复位歌词跟随/手动态（进入即从自动跟随当前句开始）。
        self.lyrics_manual_since = None;
        self.lyrics_follow_y = None;
        cx.notify();
    }

    // ---- 可交互条（进度/音量 share）：条元素 + 事件 → 播放器 ----

    /// 构建一条可交互条（seek 进度条 / 音量条共用，对齐 video `strip_element`）：
    /// `id` 元素级唯一；`bounds` 由 canvas prepaint 记录窗口坐标盒；`is_seek=true`
    /// 拖拽 → `player.seek(f×duration)`，否则 `player.set_volume(f)`。`tip=Some(名称)`
    /// 时挂悬停提示（音量条用；进度条不弹提示）。
    fn strip_element(
        &mut self,
        id: &'static str,
        bounds: Rc<Cell<Bounds<Pixels>>>,
        fraction: f32,
        is_seek: bool,
        width: Option<f32>,
        tip: Option<&'static str>,
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
        if let Some(_t) = tip {
            // 提示钮的窗口盒记录：复用本条的 bounds cell（同一 Cell，无多余 canvas）。
            self.tip_boxes.entry(id).or_insert_with(|| bounds.clone());
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
        if let Some(t) = tip {
            el = el.on_hover(self.hover_tip_listener(cx, id, t));
        }
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
        } else {
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
        // 拖动/点按跟手不寄生动画/刻度（tick）：音频动画已降频（eq/vinyl `with_max_fps(15.)`）
        // 且暂停态无任何 tick——进度/音量视觉必须由**本视图**自己 notify 驱动重绘。
        // （player 实体 notify 只通知该实体，不等价于重绘本视图；这里补一次幂等通知。）
        cx.notify();
    }

    /// 应用拖拽比例（点按/音量共用入口）：进度 → 立即真实 seek(f×duration) 并进拖拽态
    /// 状态由调用方先置好；音量 → set_volume(f)。拖拽路径统一补视图 notify（见
    /// [`Self::seek_to_fraction`] 注释）——音量分支在此补，seek 分支内已 notify。
    fn apply_strip(&mut self, is_seek: bool, f: f32, cx: &mut Context<Self>) {
        if is_seek {
            self.seek_to_fraction(f, true, cx);
        } else {
            self.player.update(cx, |p, cx| p.set_volume(f, cx));
            // 音量视觉同进度：视图自身 notify，点/拖即时刷新、不寄生动画/tick 帧。
            cx.notify();
        }
    }

    /// 底部播放栏（B 段）：整块已拆成独立 [`bottom::BottomBarView`] 子视图——进度心跳/
    /// 拖拽/按钮/tooltip 全在子视图内，父树只嵌入它（子视图 notify 不带动父整树）。
    pub(super) fn render_bottom(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> AnyElement {
        self.bottom_bar.clone().into_any_element()
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
        // 主界面顶层：Windows 风格标题栏（左 logo+标题、中拖动区、右三钮），真实作用于
        // 应用窗口（`window_control_area` 原生最小化/最大化/关闭 + 拖动/双击切换）。
        root = root
            .child(titlebar::render(window, cx))
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
        // toast 最顶层，其后是共享悬停提示浮层（最上）。
        root = root.child(self.render_toast());
        root = root.child(self.render_tooltip(window, cx));
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
