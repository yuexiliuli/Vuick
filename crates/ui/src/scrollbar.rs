//! 自绘滚动条：`Scrollbar`（垂直/水平）+ [`ScrollbarHandle`] 契约，替代
//! `gpui_component::scroll::{Scrollbar, ScrollbarHandle, ScrollbarMode}`。
//!
//! 行为/几何复刻 gpui-component（轨道 16px 于边缘、滑块 6px 常态 / 8px 激活、最小 48px、
//! 点击跳转、拖拽写回 [`ScrollbarHandle::set_offset`]）。App 内全部调用点原为
//! `ScrollbarMode::Always`，此处 Always 全量实现；`Auto` 提供「滚动区滚轮显示 → 空闲
//! [`autohide`](Scrollbar::autohide) 后自隐」：活动区域取句柄滚动容器边界（列表主体滚轮
//! 也算活动）＋组件内空闲定时器到点自行隐藏并触发重绘（mousedown/mouseup 也刷新计时，
//! 拖拽期间 `drag_keep_visible` 保显；计时走 `background_executor().now()`，测试可精确推进）。
//!
//! # 视觉
//! 滑块为胶囊形 + 轴向微渐变（玻璃质感）；悬停/拖拽时滑块由 6px 平滑放大到 8px、
//! 透明度抬升、轨道胶囊淡入，拖拽态切换为 accent 强调色。插值在 `prepaint` 用
//! 指数平滑自驱动收敛（`Window::request_animation_frame` 逐帧推进，收敛即静止，
//! 尊重 `App::reduce_motion`）。轨道/滑块的激活透明度不会盖住正文（顶部叠加层）。
//!
//! # 用法
//! 与 gpui-component 对齐：`Scrollbar::vertical(&handle).mode(ScrollbarMode::Always)
//! .viewport_from_layout()`；容器内作为唯一子元素铺满视口（`request_layout` 填满父级）。
//!
//! # 自定义句柄
//! 编辑器/预览的「确定性量程」句柄（`VerticalScrollHandle`；M5-B5 P4-2 后
//! `PreviewScrollHandle` 随 `MarkdownPreview` 删除，统一 WyEditor 用
//! `gpui::ScrollHandle`（`md-wy-scroll`/`live-scroll`））只需把
//! `impl gpui_component::scroll::ScrollbarHandle` 换成 `impl crate::ui::scrollbar::
//! ScrollbarHandle`——契约方法完全相同。也内置了对 `gpui::ScrollHandle` / `ListState` 的实现。
//!
//! # 交互（对齐 gpui-component 的优化）
//! 拖动提交带双重门控：>1px 差值门控（拖到轨道尽头被 clamp 住时不空转重建）＋**帧级
//! 合并节流**（`MouseMoveEvent` 只把最新目标记进 `pending_target`，`prepaint` 每帧落地
//! 一次——重视图慢帧下也不丢"最后一次在底部"的移动，来回拖不卡滞）。不用
//! `event.dragging()` 门控：平台逐事件上报 `pressed_button` 在部分设备/驱动上偶发
//! `None`，高频拖拽会因此偶尔跳过提交、显得不跟手；以元素自身 `drag_grab`
//! （mousedown 设 / mouseup 清）为「按住中」的权威信号。
//!
//! # 生命周期（gpui 约束）
//! 几何在 `prepaint` 计算（读句柄量程、读写元素级状态），鼠标监听在 `paint` 注册——
//! gpui 的 `window.on_mouse_event` 只允许在 paint 阶段调用（`debug_assert_paint`）。
//!
//! # 状态隔离
//! 跨帧拖拽/悬停状态按元素 id 存放（`use_keyed_state`），每个滚动条实例独立；默认 id 取自
//! 调用点（`ElementId::CodeLocation`），多实例天然分开。

use std::cell::Cell;
use std::panic::Location;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{
    App, Bounds, ContentMask, CursorStyle, Element, ElementId, Entity, EntityId,
    GlobalElementId, Hitbox, HitboxBehavior, Hsla, InspectorElementId, IntoElement, LayoutId,
    ListState, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Point, Pixels, ScrollHandle,
    ScrollWheelEvent, Size, Task, Window, fill, linear_color_stop, linear_gradient, point, px, size,
};

/// 轨道厚（垂直=宽 / 水平=高）。
const TRACK_WIDTH: f32 = 16.;
/// 常态滑块粗细。
const THUMB_WIDTH: f32 = 6.;
/// 激活（悬停/拖拽）滑块粗细。
const THUMB_ACTIVE_WIDTH: f32 = 8.;
/// 滑块距边缘 inset。
const THUMB_INSET: f32 = 4.;
/// 滑块最小长度。
const MIN_THUMB_SIZE: f32 = 48.;
/// `Auto` 模式隐藏前的不滚动时长。
const AUTO_HIDE_IDLE: Duration = Duration::from_secs(2);
/// 滑块常态不透明度。
const THUMB_ALPHA_REST: f32 = 0.35;
/// 滑块悬停不透明度。
const THUMB_ALPHA_HOVER: f32 = 0.62;
/// 拖拽中滑块不透明度（改用 accent 色）。
const THUMB_ALPHA_DRAG: f32 = 0.85;
/// 轨道胶囊不透明度上限（悬停后淡入）。
const TRACK_ALPHA: f32 = 0.20;
/// 轨道胶囊粗细（比激活滑块略宽，呈现包裹效果）。
const TRACK_PILL_WIDTH: f32 = 10.;
/// 滑块纵向微渐变：尾部相对头部的淡化系数（玻璃质感）。
const THUMB_GRADIENT_TAPER: f32 = 0.7;
/// 悬停/拖拽插值的指数平滑时间常数。
const SMOOTH_TIME: Duration = Duration::from_millis(90);

/// 静态轨道宽度（px），与 gpui-component 的 `Scrollbar::width()` 语义一致
/// （编辑器/预览用它预留底部滚动条厚度）。
pub const fn scrollbar_width() -> Pixels {
    px(TRACK_WIDTH)
}

/// 滚动条显示模式。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ScrollbarMode {
    /// 常显。
    #[default]
    Always,
    /// 滚动后可见，停止滚动到 IDLE 后隐藏。
    Auto,
}

/// 滚动条轴向。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ScrollbarAxis {
    Vertical,
    Horizontal,
}

impl ScrollbarAxis {
    fn is_vertical(self) -> bool {
        matches!(self, ScrollbarAxis::Vertical)
    }
}

/// 滚动条句柄契约：几何与滚动状态由句柄提供，滚动条只负责表达与交互。
///
/// 与 gpui-component 的 `ScrollbarHandle` 方法签名一致，便于自定义句柄换 trait 名即切换。
pub trait ScrollbarHandle {
    /// 滚动容器（视口）的像素边界。
    fn viewport_bounds(&self) -> Bounds<Pixels>;
    /// 当前滚动偏移（内容已滚动方向为负，如向下滚 → y 为负）。
    fn offset(&self) -> Point<Pixels>;
    /// 把滚动条表达的新偏移写回滚动状态。
    fn set_offset(&self, offset: Point<Pixels>);
    /// 内容总尺寸（视口 + 可滚距离）。
    fn content_size(&self) -> Size<Pixels>;
    /// 拖拽开始的钩子（如冻结量程）。
    fn start_drag(&self) {}
    /// 拖拽结束的钩子。
    fn end_drag(&self) {}
}

impl ScrollbarHandle for ScrollHandle {
    fn viewport_bounds(&self) -> Bounds<Pixels> {
        self.bounds()
    }

    fn offset(&self) -> Point<Pixels> {
        self.offset()
    }

    fn set_offset(&self, offset: Point<Pixels>) {
        self.set_offset(offset)
    }

    fn content_size(&self) -> Size<Pixels> {
        let bounds = self.bounds();
        let max = self.max_offset();
        size(bounds.size.width + max.x, bounds.size.height + max.y)
    }
}

impl ScrollbarHandle for ListState {
    fn viewport_bounds(&self) -> Bounds<Pixels> {
        self.viewport_bounds()
    }

    fn offset(&self) -> Point<Pixels> {
        self.scroll_px_offset_for_scrollbar()
    }

    fn set_offset(&self, offset: Point<Pixels>) {
        self.set_offset_from_scrollbar(offset)
    }

    fn content_size(&self) -> Size<Pixels> {
        let viewport = self.viewport_bounds();
        size(viewport.size.width, viewport.size.height + self.max_offset_for_scrollbar().y)
    }

    fn start_drag(&self) {
        self.scrollbar_drag_started();
    }

    fn end_drag(&self) {
        self.scrollbar_drag_ended();
    }
}

/// 元素级跨帧状态（按元素 id 隔离，见模块文档）。
#[derive(Default)]
struct UiState {
    /// 拖拽中：鼠标按下点相对滑块命中区原点的偏移。
    drag_grab: Option<Point<Pixels>>,
    /// 拖拽中**最新一次尚未落地的目标偏移 px**（`>1px` 门控通过即记录）。事件可能
    /// 比帧快得多（尤其预览这类 20-30ms/帧的重视图）：旧实现按 `max_fps` 节流**丢弃**
    /// 未到期事件 → 快速来回拖时「最后一次在底部的移动」被丢、thumb 停在旧位、要晃
    /// 几下才跟上。改为事件只记 `pending_target`（永远最新）并预约一帧，`prepaint`
    /// 每帧落地一次——**帧率合并、绝不丢最新位置**。
    pending_target: Option<f32>,
    /// 是否悬停在轨道上（激活更宽）。
    hovering: bool,
    /// 最近一次滚动发生时间（`Auto` 模式隐藏依据）。
    last_scroll: Option<Instant>,
    /// `Auto` 模式是否已隐藏。
    hidden: bool,
    /// 悬停/拖拽插值（0..1）：宽度、轨道、透明度随它平滑过渡。
    present: f32,
    /// 动画推进用的上一帧时间。
    last_frame: Option<Instant>,
    /// Auto：**一次性**空闲自隐定时器句柄（存组件状态 → 新活动到达时 drop 旧句柄 = 立即
    /// 取消，再按 `autohide(idle)` 重排；组件/状态释放时一并取消）。
    hide_timer: Option<Task<()>>,
}

/// 单帧几何（prepaint 计算，paint/事件闭包共用；位置/边界为 Pixel、量程为 f32 标量）。
#[derive(Clone)]
struct Geom {
    /// 轨道条（垂直=右缘 16px，水平=底缘 16px）。
    track: Bounds<Pixels>,
    /// 轨道胶囊（悬停后淡入的细条，宽于滑块并与之同轴居中）。
    track_pill: Bounds<Pixels>,
    /// 滑块命中区（整轨宽，含 inset）。
    thumb_hit: Bounds<Pixels>,
    /// 滑块着色区（滑块宽，随悬停/拖拽插值伸缩）。
    thumb_fill: Bounds<Pixels>,
    /// 滑块命中长度（px 标量）。
    thumb_len: f32,
    /// 内容长（px 标量）。
    scroll_area: f32,
    /// 视口长（px 标量）。
    container: f32,
    /// 偏移最大负值（= -(内容-视口)，px 标量）；偏移合法域 [safe_min, 0]。
    safe_min: f32,
}

/// 自绘滚动条。见模块文档。
///
/// 渐变/隐藏/增宽类参数均为构建期配置（重建即重设，无跨帧持久化需求，
/// 与 `mode`/`axis` 同放组件本身；跨帧的拖拽/悬停/隐藏状态仍在 `UiState`）。
pub struct Scrollbar<H: ScrollbarHandle + Clone> {
    handle: H,
    axis: ScrollbarAxis,
    mode: ScrollbarMode,
    id: ElementId,
    layout_viewport: bool,
    /// `Auto` 空闲隐藏延时（`autohide()`，默认 2s，见 `AUTO_HIDE_IDLE`）。
    autohide_idle: Duration,
    /// 滑块最小长度（`thumb_min()`，默认 48px，见 `MIN_THUMB_SIZE`）。
    min_thumb: f32,
    /// 悬停是否保显（`hide_on_hover()`，默认 true：Auto 悬停不隐藏）。
    hide_on_hover: bool,
    /// 拖拽是否常显（`drag_keep_visible()`，默认 true：Auto 拖拽不隐藏）。
    drag_keep_visible: bool,
    /// 悬停/拖拽是否把滑块 6→8px 增宽（`auto_widen()`，默认 true）。
    auto_widen: bool,
    /// 把「Auto 是否可见」回写的只读快照（`report_visible_to()`，默认 None 零开销）。
    /// prepaint 完成 Auto 判定后写入 `!hidden`；单一事实源仍是组件内部状态，槽只作观测。
    visible_sink: Option<Rc<Cell<bool>>>,
    /// 滑块常态色（`thumb_color()`，默认 None = 全局主题 fg，editor/pdf/tabs 零变化）。
    thumb_color: Option<Hsla>,
    /// 轨道胶囊色（`track_color()`，默认 None = 全局主题 border，零变化）。
    track_color: Option<Hsla>,
    /// 拖拽/激活强调色（`accent_color()`，默认 None = 全局主题 accent，零变化）。
    accent_color: Option<Hsla>,
}

impl<H: ScrollbarHandle + Clone + 'static> Scrollbar<H> {
    /// 以调用点位置作为默认元素 id（跨帧状态隔离的键）。
    #[track_caller]
    pub fn new(handle: &H) -> Self {
        let caller = Location::caller();
        Self {
            handle: handle.clone(),
            axis: ScrollbarAxis::Vertical,
            mode: ScrollbarMode::Always,
            id: ElementId::CodeLocation(*caller),
            layout_viewport: false,
            autohide_idle: AUTO_HIDE_IDLE,
            min_thumb: MIN_THUMB_SIZE,
            hide_on_hover: true,
            drag_keep_visible: true,
            auto_widen: true,
            visible_sink: None,
            thumb_color: None,
            track_color: None,
            accent_color: None,
        }
    }

    /// 垂直滚动条。
    #[track_caller]
    pub fn vertical(handle: &H) -> Self {
        Self::new(handle).axis(ScrollbarAxis::Vertical)
    }

    /// 水平滚动条。
    #[track_caller]
    pub fn horizontal(handle: &H) -> Self {
        Self::new(handle).axis(ScrollbarAxis::Horizontal)
    }

    pub fn mode(mut self, mode: ScrollbarMode) -> Self {
        self.mode = mode;
        self
    }

    pub fn axis(mut self, axis: ScrollbarAxis) -> Self {
        self.axis = axis;
        self
    }

    /// `Auto` 模式下的空闲隐藏延时：停止滚动超过该时长即隐藏。默认 2s（保全现状）。
    pub fn autohide(mut self, idle: Duration) -> Self {
        self.autohide_idle = idle;
        self
    }

    /// 滑块最小长度（px）。默认 48（保全现状）；music 自绘滚动条用 24。
    pub fn thumb_min(mut self, px: f32) -> Self {
        self.min_thumb = px;
        self
    }

    /// 悬停是否保显：`true`（默认）时 Auto 悬停轨道不隐藏、`false` 时悬停只换色不保显。
    pub fn hide_on_hover(mut self, keep: bool) -> Self {
        self.hide_on_hover = keep;
        self
    }

    /// 拖拽是否常显：`true`（默认）时 Auto 拖拽期间不隐藏。
    pub fn drag_keep_visible(mut self, keep: bool) -> Self {
        self.drag_keep_visible = keep;
        self
    }

    /// 悬停/拖拽是否把滑块 6→8px 增宽：`true`（默认）增宽、`false` 恒 6px。
    pub fn auto_widen(mut self, widen: bool) -> Self {
        self.auto_widen = widen;
        self
    }

    /// 把「Auto 是否可见」（`!hidden`）回写到调用方提供的槽，供测试/调试做只读快照。
    /// 省略（默认）零开销；槽写方向只读——调用方不得写回覆盖组件内部判定。
    pub fn report_visible_to(mut self, sink: Rc<Cell<bool>>) -> Self {
        self.visible_sink = Some(sink);
        self
    }

    /// 滑块常态色注入（None=现全局主题 fg，保持现状）。music 等子主题调用点传自己的
    /// theme token；editor/pdf/tabs 等全部默认调用点零变化。
    pub fn thumb_color(mut self, color: Option<Hsla>) -> Self {
        self.thumb_color = color;
        self
    }

    /// 轨道胶囊色注入（None=现全局主题 border，保持现状）。
    pub fn track_color(mut self, color: Option<Hsla>) -> Self {
        self.track_color = color;
        self
    }

    /// 拖拽/激活强调色注入（None=现全局主题 accent，保持现状）。
    pub fn accent_color(mut self, color: Option<Hsla>) -> Self {
        self.accent_color = color;
        self
    }

    /// `Auto` 模式隐藏判定（纯函数，便于测试）：距上次滚动 `since_last_scroll` 超
    /// `autohide_idle` 且（不满足保显条件）→ 隐藏。`None`（从未滚动）→ 隐藏。
    /// 保显条件：`hide_on_hover && hovering`、`drag_keep_visible && dragging` 任一成立即不隐藏。
    fn auto_hidden(&self, since_last_scroll: Option<Duration>, hovering: bool, dragging: bool) -> bool {
        should_auto_hide(
            self.autohide_idle,
            self.hide_on_hover,
            self.drag_keep_visible,
            since_last_scroll,
            hovering,
            dragging,
        )
    }


    /// 用元素自身布局边界作为视口（覆盖句柄的 `viewport_bounds`）。
    pub fn viewport_from_layout(mut self) -> Self {
        self.layout_viewport = true;
        self
    }

    fn resolved_viewport(&self, layout_bounds: Bounds<Pixels>) -> Bounds<Pixels> {
        if self.layout_viewport {
            layout_bounds
        } else {
            self.handle.viewport_bounds()
        }
    }

    /// 几何计算（量程标量 f32；位置/边界 Pixel）。
    ///
    /// `present` 为悬停/拖拽插值（0..1），控制滑块粗细（6→8px）与轨道胶囊显示。
    fn geom(&self, viewport: Bounds<Pixels>, present: f32) -> Option<Geom> {
        let is_v = self.axis.is_vertical();
        let track_w = px(TRACK_WIDTH);
        let track = if is_v {
            Bounds::new(
                point(viewport.right() - track_w, viewport.top()),
                size(track_w, viewport.size.height),
            )
        } else {
            Bounds::new(
                point(viewport.left(), viewport.bottom() - track_w),
                size(viewport.size.width, track_w),
            )
        };

        // 轨道胶囊：贴视口边缘（垂直贴右 / 水平贴底）、宽于激活滑块，悬停后淡入。
        let pill_w = px(TRACK_PILL_WIDTH);
        let track_pill = if is_v {
            // 垂直条：轨道胶囊右缘贴住视口右缘（贴右，对称横向贴底）。此前胶囊以轨道
            // 中线居中悬在右缘内侧 3px，真机观感=竖条与右侧边缘之间留有空隙。
            Bounds::new(
                point(viewport.right() - pill_w, viewport.top()),
                size(pill_w, viewport.size.height),
            )
        } else {
            // 水平条：轨道胶囊底缘贴住视口底（贴底）。此前胶囊以轨道中线居中悬在
            // 底边上方 3px，真机观感=横条与编辑区底边之间留有空隙（容器贴底护栏见
            // `editor/tests/view.rs hscrollbar_overlay_container_flush_to_editor_bottom`；
            // 横条交叉轴贴底以本单测锁死）。
            Bounds::new(
                point(viewport.left(), viewport.bottom() - pill_w),
                size(viewport.size.width, pill_w),
            )
        };

        let content = self.handle.content_size();
        let (scroll_area, container) = if is_v {
            (content.height.as_f32(), viewport.size.height.as_f32())
        } else {
            (content.width.as_f32(), viewport.size.width.as_f32())
        };
        if scroll_area <= container {
            return None;
        }

        let offset = self.handle.offset();
        let pos = if is_v {
            offset.y.as_f32()
        } else {
            offset.x.as_f32()
        }; // 向下滚 → 负

        let thumb_len = (container / scroll_area * container).max(self.min_thumb);
        let travel = (container - thumb_len).max(0.);
        let max_scroll = (scroll_area - container).max(0.);

        // thumb_start：滚动进度 × 滑块可走距离（pos 为负 → 进度为正）
        let frac = if max_scroll > 0. {
            (-pos / max_scroll).clamp(0., 1.)
        } else {
            0.
        };
        let thumb_start = travel * frac;
        let thumb_end = (thumb_start + thumb_len).min(container);
        // 命中/着色长度：两端各缩进 inset（与 gpui-component 一致）
        let hit_len = (thumb_end - thumb_start - THUMB_INSET * 2.).max(0.);

        // 滑块粗细随悬停/拖拽插值伸缩（`auto_widen=false` 时恒 6px，不随插值增宽）。
        let fill_w = if self.auto_widen {
            px(THUMB_WIDTH + (THUMB_ACTIVE_WIDTH - THUMB_WIDTH) * present.clamp(0., 1.))
        } else {
            px(THUMB_WIDTH)
        };
        // 命中/着色区锚定 rail 内缘（含 inset）：
        // - 垂直：右对齐到 track.right()-inset，向**左**延展（滑块在轨道内）。
        // - 水平：下对齐到 track.bottom()-inset，向**上**延展。
        // 用 `Bounds::new`（左上锚）时不能直接拿 `top_right()/bottom_left()+(-inset,...)`
        // 当左上角——那是 TopRight/BottomLeft 锚的算法，会整块滑到轨道外，导致点不到滑块。
        let (thumb_hit, thumb_fill) = if is_v {
            let top = track.top() + px(THUMB_INSET + thumb_start);
            // 垂直条贴右（对称 K144 横向贴底）：滑块/命中区右缘贴住轨道右缘（交叉轴
            // 不再内缩 THUMB_INSET）；`THUMB_INSET` 保留在长轴起始端（顶，行程起点
            // 内缩，两轴对称手感不变）。
            let hit_right = track.right();
            (
                Bounds::new(point(hit_right - track_w, top), size(track_w, px(hit_len))),
                Bounds::new(point(hit_right - fill_w, top), size(fill_w, px(hit_len))),
            )
        } else {
            let left = track.left() + px(THUMB_INSET + thumb_start);
            // 水平条：滑块底缘贴住轨道/视口底（与胶囊一同贴底）。交叉轴不再从底部
            // 内缩 THUMB_INSET（原 4px 悬空 = 用户真机「横条与编辑区底边有空隙」根因）；
            // `THUMB_INSET` 仍保留在长轴左端（行程起点内缩，两轴对称手感不变）。
            (
                Bounds::new(
                    point(left, track.bottom() - track_w),
                    size(px(hit_len), track_w),
                ),
                Bounds::new(
                    point(left, track.bottom() - fill_w),
                    size(px(hit_len), fill_w),
                ),
            )
        };

        Some(Geom {
            track,
            track_pill,
            thumb_hit,
            thumb_fill,
            thumb_len: hit_len,
            scroll_area,
            container,
            safe_min: -max_scroll,
        })
    }
}

/// 滚动条三色解析结果（paint 消费；track/thumb/accent 已按注入或全局主题定案）。
#[derive(Clone, Copy, Debug, PartialEq)]
struct ScrollbarColors {
    track: Hsla,
    thumb: Hsla,
    accent: Hsla,
}

/// 解析滚动条三色：颜色参数有 → 用之；无 → 现全局主题（保持现状，editor/pdf/tabs 等
/// 全部默认调用点零变化）。纯函数便于单测断言「注入生效 / 默认回退全局」。
fn resolve_colors(
    thumb: Option<Hsla>,
    track: Option<Hsla>,
    accent: Option<Hsla>,
    theme: &crate::ui::theme::Theme,
) -> ScrollbarColors {
    ScrollbarColors {
        track: track.unwrap_or(theme.colors.border.into()),
        thumb: thumb.unwrap_or(theme.colors.fg.into()),
        accent: accent.unwrap_or(theme.colors.accent.into()),
    }
}

/// Auto 隐藏判定纯函数（与 `Scrollbar::auto_hidden` 同源；供空闲自动隐藏定时器在后台
/// 复读当前状态，保证判据单一事实源）。`since_last_scroll=None`（从未滚动）→ 隐藏。
fn should_auto_hide(
    idle: Duration,
    hide_on_hover: bool,
    drag_keep_visible: bool,
    since_last_scroll: Option<Duration>,
    hovering: bool,
    dragging: bool,
) -> bool {
    let Some(idle_since) = since_last_scroll else {
        return true;
    };
    let over_idle = idle_since >= idle;
    let holds = (hide_on_hover && hovering) || (drag_keep_visible && dragging);
    over_idle && !holds
}

/// Auto：**精确空闲自隐**——每次滚动/拖拽活动都取消既有 pending 隐藏定时器（drop 旧
/// `Task` = 立即取消，见 scheduler `Task` drop 语义）、按 `autohide(idle)` 重新排**一枚
/// 一次性**定时器，到点即判隐并 `notify`（状态实体被当前视图 observe → 窗口重绘 → thumb
/// 消失）。对比「固定间隔轮询」：活动落在轮询边界时旧实现隐藏延迟可达 ~2×idle（音乐旧
/// 250ms tick ≈3s）；一次性排期最坏 = 上次活动后**精确 idle**。
///
/// 判据同 `should_auto_hide`（单一事实源）：`hide_on_hover=false` 时悬停不排/不刷；
/// `drag_keep_visible=true` 拖拽期间到点不隐（定时器被消费，不重排）——由随后的
/// mouseup / hover-out（hide_on_hover=true 才排）活动重新排期。组件/状态释放（weak
/// 失效或句柄随实体 drop）即取消，不留悬挂任务。
fn arm_auto_hide(
    idle: Duration,
    hide_on_hover: bool,
    drag_keep_visible: bool,
    sink: Option<Rc<Cell<bool>>>,
    state: &Entity<UiState>,
    cx: &mut App,
) {
    let weak = state.downgrade();
    let task = cx.spawn(async move |cx| {
        cx.background_executor().timer(idle).await;
        let Some(ent) = weak.upgrade() else { return };
        cx.update(|app| {
            let s = ent.read(app);
            // 复读用 fake 时钟对齐（与写入侧 `background_executor().now()` 同源）。
            let now = app.background_executor().now();
            let elapsed = s.last_scroll.map(|t| now.saturating_duration_since(t));
            if !should_auto_hide(
                idle,
                hide_on_hover,
                drag_keep_visible,
                elapsed,
                s.hovering,
                s.drag_grab.is_some(),
            ) {
                // 拖拽/悬停按保显条件不隐：等 mouseup / hover-out 活动重新排期。
                return;
            }
            ent.update(app, |s, _| s.hidden = true);
            if let Some(sink) = &sink {
                sink.set(false); // 让测试可在「不重绘」下直接观测到隐藏落定。
            }
            app.notify(ent.entity_id());
        });
    });
    // 存新句柄即 drop 旧句柄（= 取消上一枚定时器）。活动比定时器快时每枚只存活 ≤ idle。
    state.update(cx, |s, _| s.hide_timer = Some(task));
}

/// 从元素状态快照出的只读字段（避免在闭包里二次 borrow）。
struct UiStateSnapshot {
    hovering: bool,
    hidden: bool,
    last_scroll: Option<Instant>,
}

/// `prepaint` 产出、`paint` 消费的状态（公开仅为满足 `Element::PrepaintState` 可见性，
/// 字段私有，别在模块外构造）。
pub struct PrepaintState<H> {
    geom: Option<Geom>,
    hidden: bool,
    /// 本帧悬停/拖拽插值（0..1）。
    present: f32,
    /// 视口命中区（手型光标载体）；仅「有几何且未隐藏」才安装（隐藏/无几何时连命中区
    /// 都不装，避免整块视口挂手型光标或遮挡下层内容）。
    track_hitbox: Option<Hitbox>,
    handle: H,
    axis: ScrollbarAxis,
    mode: ScrollbarMode,
    /// Auto 配置参数快照（paint 监听闭包复制用）。
    autohide_idle: Duration,
    hide_on_hover: bool,
    drag_keep_visible: bool,
    /// 可见性回写槽快照（paint 闭包/自隐定时器用）。
    visible_sink: Option<Rc<Cell<bool>>>,
    /// 颜色参数快照（paint 取色用；None=全局主题，保持现状）。
    thumb_color: Option<Hsla>,
    track_color: Option<Hsla>,
    accent_color: Option<Hsla>,
    state: Entity<UiState>,
    view_id: EntityId,
}

impl<H: ScrollbarHandle + Clone + 'static> PrepaintState<H> {
    /// paint 阶段注册鼠标/滚轮监听（gpui 只允许 paint 里 `on_mouse_event`）。
    fn register_listeners(&mut self, window: &mut Window, _cx: &mut App) {
        let view_id = self.view_id;
        let mode = self.mode;
        let is_v = self.axis.is_vertical();

        // Auto：滚动区滚轮 → 活动 + 排队空闲自动隐藏。活动区域 = 句柄滚动容器边界
        // （`handle.viewport_bounds()`）：音乐主列表滚轮发生在**列表主体**而非 16px 条本体，
        // 必须按滚动区判定（列表滚轮算活动、条内滚轮亦覆盖）；滚动区外（如并排歌词区）不误
        // 触发。Always 无隐藏依据，不注册滚轮监听（对既有 Always 调用点零行为变化）。
        if mode == ScrollbarMode::Auto {
            let region = self.handle.viewport_bounds();
            let state = self.state.clone();
            let autohide_idle = self.autohide_idle;
            let hide_on_hover = self.hide_on_hover;
            let drag_keep_visible = self.drag_keep_visible;
            let sink = self.visible_sink.clone();
            window.on_mouse_event(move |event: &ScrollWheelEvent, phase, _window, cx| {
                if !phase.bubble() || !region.contains(&event.position) {
                    return;
                }
                // 时间戳一律走 `background_executor().now()`——测试用 fake 时钟可精确推进
                // 超时（K29/`Instant::now()` 真机在测试里推进不了）。
                let now = cx.background_executor().now();
                state.update(cx, |s, _| s.last_scroll = Some(now));
                cx.notify(view_id);
                arm_auto_hide(
                    autohide_idle,
                    hide_on_hover,
                    drag_keep_visible,
                    sink.clone(),
                    &state,
                    cx,
                );
            });
        }

        let Some(geom) = &self.geom else { return };
        let geom = geom.clone();
        let track = geom.track;
        let thumb_hit = geom.thumb_hit;
        let thumb_len = geom.thumb_len;
        let scroll_area = geom.scroll_area;
        let container = geom.container;
        let safe_min = geom.safe_min;
        let handle = self.handle.clone();
        let state = self.state.clone();

        // 按下：命中的点 → 开始拖拽；轨道空白 → 跳转。
        window.on_mouse_event({
            let handle = handle.clone();
            let state = state.clone();
            let autohide_idle = self.autohide_idle;
            let hide_on_hover = self.hide_on_hover;
            let drag_keep_visible = self.drag_keep_visible;
            let mode = self.mode;
            let sink = self.visible_sink.clone();
            move |event: &MouseDownEvent, phase, _window, cx| {
                if !phase.bubble() || !track.contains(&event.position) {
                    return;
                }
                cx.stop_propagation();
                let offset = handle.offset();
                if thumb_hit.contains(&event.position) {
                    let grab = event.position - thumb_hit.origin;
                    handle.start_drag();
                    state.update(cx, |s, _| {
                        s.drag_grab = Some(grab);
                        // 新拖拽从干净状态开始（清掉上一条手势残留的未落地目标）。
                        s.pending_target = None;
                    });
                } else {
                    // 点击跳转：滑块中心对准点击位。
                    let (pos, denom) = if is_v {
                        (event.position.y.as_f32(), track.size.height.as_f32() - thumb_len)
                    } else {
                        (event.position.x.as_f32(), track.size.width.as_f32() - thumb_len)
                    };
                    let pct = ((pos
                        - thumb_len / 2.
                        - if is_v {
                            track.origin.y.as_f32()
                        } else {
                            track.origin.x.as_f32()
                        })
                        / denom)
                        .clamp(0., 1.);
                    let target = (-((scroll_area - container) * pct)).max(safe_min);
                    if is_v {
                        handle.set_offset(point(offset.x, px(target)));
                    } else {
                        handle.set_offset(point(px(target), offset.y));
                    }
                }
                // 按下 = 滚动活动：刷新 Auto 空闲计时（从按下载 3s）；拖拽期间 drag_keep
                // 保显、松手后从松手时刻再计时。每次活动取消旧定时器、重排一次性新定时器。
                if mode == ScrollbarMode::Auto {
                    let now = cx.background_executor().now();
                    state.update(cx, |s, _| s.last_scroll = Some(now));
                    arm_auto_hide(
                        autohide_idle,
                        hide_on_hover,
                        drag_keep_visible,
                        sink.clone(),
                        &state,
                        cx,
                    );
                }
                cx.notify(view_id);
            }
        });

        // 移动：更新悬停 + 拖拽写回 offset（>1px 差值门控 + 每帧 prepaint 落地最新目标，
        // 替代旧的 max_fps 丢弃式节流——避免快速来回拖把最后位置丢掉、thumb 停在旧位）。
        // 注意：**不**用 `event.dragging()` 门控——drag_grab 只在本滚动条 mousedown 设、
        // mouseup 清，已是我们自己的「按住中」权威信号；平台逐事件上报的 pressed_button
        // 在部分设备/驱动上会间歇性为 None，用它门控会把高频拖拽偶尔跳过提交、显得不跟手。
        window.on_mouse_event({
            let handle = handle.clone();
            let state = state.clone();
            let mode = self.mode;
            let autohide_idle = self.autohide_idle;
            let hide_on_hover = self.hide_on_hover;
            let drag_keep_visible = self.drag_keep_visible;
            let sink = self.visible_sink.clone();
            move |event: &MouseMoveEvent, _phase, _window, cx| {
                let hovered = track.contains(&event.position);
                let mut notify = false;

                // 悬停状态切换：hover 进入不排/不刷（`hide_on_hover=false` 时不参与显隐）；
                // hover 离开且为「保显型悬停」（hide_on_hover=true）→ 把被悬停保显“跳过”
                // 的那枚一次性定时器补排一枚（判据仍锚定既有 `last_scroll`，不刷新计时）。
                let prev_hovering = state.read(cx).hovering;
                state.update(cx, |s, _| s.hovering = hovered);
                if mode == ScrollbarMode::Auto && hide_on_hover && prev_hovering && !hovered {
                    arm_auto_hide(
                        autohide_idle,
                        hide_on_hover,
                        drag_keep_visible,
                        sink.clone(),
                        &state,
                        cx,
                    );
                }

                if state.read(cx).drag_grab.is_some() {
                    cx.stop_propagation();
                    let grab = state.read(cx).drag_grab.unwrap();
                    let (pos, grab_axis, denom) = if is_v {
                        (
                            event.position.y.as_f32(),
                            grab.y.as_f32(),
                            track.size.height.as_f32() - thumb_len,
                        )
                    } else {
                        (
                            event.position.x.as_f32(),
                            grab.x.as_f32(),
                            track.size.width.as_f32() - thumb_len,
                        )
                    };
                    let origin = if is_v {
                        track.origin.y.as_f32()
                    } else {
                        track.origin.x.as_f32()
                    };
                    let pct = ((pos - grab_axis - origin) / denom).clamp(0., 1.);
                    let target = (-((scroll_area - container) * pct)).max(safe_min);
                    let offset = handle.offset();
                    let cur = if is_v {
                        offset.y.as_f32()
                    } else {
                        offset.x.as_f32()
                    };
                    // >1px 门控：未真正移动（如拖到轨道尽头被 clamp 住）就跳过。
                    // 通过后**只记最新目标**（不直接提交）：事件可能比帧快得多，若按
                    // max_fps 扔未到期事件，快速来回拖时最后位置会被丢弃、thumb 停在
                    // 旧位。prepaint 每帧把 pending_target 落地一次（帧率合并、不丢）。
                    if (target - cur).abs() > 1.0 {
                        state.update(cx, |s, _| s.pending_target = Some(target));
                        notify = true;
                    }
                } else if hovered {
                    notify = true;
                }
                if notify {
                    cx.notify(view_id);
                }
            }
        });

        // 抬起：结束拖拽。
        window.on_mouse_event({
            let handle = handle.clone();
            let state = state.clone();
            let mode = self.mode;
            let autohide_idle = self.autohide_idle;
            let hide_on_hover = self.hide_on_hover;
            let drag_keep_visible = self.drag_keep_visible;
            let sink = self.visible_sink.clone();
            move |_event: &MouseUpEvent, phase, _window, cx| {
                if phase.bubble() && state.read(cx).drag_grab.is_some() {
                    handle.end_drag();
                    // 松手 = 活动收尾：从此刻（fake 时钟）起 3s 无新活动才隐藏（对齐旧行为——
                    // 拖拽整段由 drag_keep 保显，结束后从这里重新计空闲并重排一次性定时器）。
                    if mode == ScrollbarMode::Auto {
                        let up_now = cx.background_executor().now();
                        state.update(cx, |s, _| {
                            s.drag_grab = None;
                            s.last_scroll = Some(up_now);
                        });
                        arm_auto_hide(
                            autohide_idle,
                            hide_on_hover,
                            drag_keep_visible,
                            sink.clone(),
                            &state,
                            cx,
                        );
                    } else {
                        state.update(cx, |s, _| s.drag_grab = None);
                    }
                    cx.notify(view_id);
                }
            }
        });
    }
}

impl<H: ScrollbarHandle + Clone + 'static> IntoElement for Scrollbar<H> {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl<H: ScrollbarHandle + Clone + 'static> Element for Scrollbar<H> {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState<H>;

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        base::util::source_location()
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        // 铺满父级（调用点通常把它放进 `.absolute().inset_0()` 覆盖层）。
        let mut style = gpui::Style {
            flex_grow: 1.0,
            flex_shrink: 1.0,
            ..Default::default()
        };
        style.size.width = gpui::relative(1.).into();
        style.size.height = gpui::relative(1.).into();
        (window.request_layout(style, None, cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let viewport = self.resolved_viewport(bounds);

        // 元素级状态：按本滚动条 id 隔离，跨帧保持拖拽/悬停。
        let state = window.use_keyed_state(self.id.clone(), cx, |_, _| UiState::default());
        let view_id = window.current_view();

        let st = {
            let s = state.read(cx);
            UiStateSnapshot {
                hovering: s.hovering,
                hidden: s.hidden,
                last_scroll: s.last_scroll,
            }
        };
        // Auto 隐藏判定收敛到 `auto_hidden`（autohide/hide_on_hover/drag_keep_visible 参数
        // 生效，默认值 = 现状）；Always 仍走持久化的 `st.hidden`，逐位不变。
        let dragging = state.read(cx).drag_grab.is_some();
        let hidden = if self.mode == ScrollbarMode::Auto {
            // 计时走 fake 时钟（`background_executor().now()`）：测试可精确推进 3s 超时。
            let now = cx.background_executor().now();
            self.auto_hidden(
                st.last_scroll.map(|ts| now.saturating_duration_since(ts)),
                st.hovering,
                dragging,
            )
        } else {
            st.hidden
        };
        // 回写可见快照（调用方经 `report_visible_to` 提供 Rc<Cell<bool>> 时；prepaint 已
        // 完成 Auto 判定，此值即当前帧真实可见性）。
        if let Some(sink) = self.visible_sink.as_ref() {
            sink.set(!hidden);
        }

        // 悬停/拖拽插值：指数平滑朝目标收敛（自驱动逐帧推进，收敛后静止）。
        let now = Instant::now();
        let (present, animating) = {
            let s = state.read(cx);
            let target = if s.hovering || s.drag_grab.is_some() { 1. } else { 0. };
            if cx.reduce_motion() || (s.present - target).abs() < 0.001 {
                (target, false)
            } else {
                let dt = s
                    .last_frame
                    .map(|t| now.saturating_duration_since(t))
                    .unwrap_or(SMOOTH_TIME)
                    .as_secs_f32()
                    .max(1. / 60.); // 极小 dt 防数值异常
                let k = 1. - (-dt / SMOOTH_TIME.as_secs_f32()).exp();
                let p = s.present + (target - s.present) * k;
                (p, (p - target).abs() >= 0.001)
            }
        };
        state.update(cx, |s, _| {
            s.present = present;
            s.last_frame = Some(now);
        });

        // 拖拽目标**帧级落地**：mousemove 只把最新目标记进 `pending_target`（事件可能比
        // 帧快得多，尤其预览这类重视图），prepaint 这里每帧落地一次——帧率合并、绝不丢
        // 最后位置（旧 max_fps 节流会在快速来回拖时丢掉"在底部"的最后一单 → thumb 停在
        // 旧位、要晃几下才跟上）。落地后约下一帧，让内容在新滚动位置重绘。
        let mut flushed = false;
        if let Some(target) = state.read(cx).pending_target {
            let cur_off = self.handle.offset();
            let is_v = self.axis.is_vertical();
            let cur = if is_v {
                cur_off.y.as_f32()
            } else {
                cur_off.x.as_f32()
            };
            if (target - cur).abs() > 1.0 {
                if is_v {
                    self.handle.set_offset(point(cur_off.x, px(target)));
                } else {
                    self.handle.set_offset(point(px(target), cur_off.y));
                }
                flushed = true;
            }
            state.update(cx, |s, _| s.pending_target = None);
        }

        let geom = self.geom(viewport, present);
        // 命中区/手型光标仅在「有几何且未隐藏」（`show`）时安装：隐藏或无几何时连视口
        // 命中区都不装——不遮挡下层内容、也不给整块视口挂手型光标。光标经
        // `window.set_cursor_style` 内部路径，headless 不可观测，真机才可验证（K49/K52）。
        let show = geom.is_some() && !hidden;
        let track_hitbox = if show {
            Some(window.with_content_mask(
                Some(ContentMask { bounds: viewport }),
                |window| window.insert_hitbox(viewport, HitboxBehavior::Normal),
            ))
        } else {
            None
        };
        if flushed {
            cx.notify(view_id);
        }
        // 未收敛且可见 → 预约下一帧继续动画（收敛后自动静止，无空转帧）。
        if animating && !hidden && geom.is_some() {
            window.request_animation_frame();
        }

        PrepaintState {
            geom,
            hidden,
            present,
            track_hitbox,
            handle: self.handle.clone(),
            axis: self.axis,
            mode: self.mode,
            autohide_idle: self.autohide_idle,
            hide_on_hover: self.hide_on_hover,
            drag_keep_visible: self.drag_keep_visible,
            visible_sink: self.visible_sink.clone(),
            thumb_color: self.thumb_color,
            track_color: self.track_color,
            accent_color: self.accent_color,
            state,
            view_id,
        }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let _ = bounds;
        prepaint.register_listeners(window, cx);

        let theme = crate::ui::theme::Theme::global(cx);
        // 三色一次解析：参数有 → 用之；无 → 全局主题（status quo）。paint 只消费解析结果。
        let colors =
            resolve_colors(prepaint.thumb_color, prepaint.track_color, prepaint.accent_color, theme);
        let is_v = prepaint.axis.is_vertical();
        let present = prepaint.present;

        // 轨道胶囊：仅悬停/拖拽时随插值淡入（常态不干扰阅读）。
        if let Some(geom) = &prepaint.geom {
            let track_alpha = (TRACK_ALPHA * present).clamp(0., 1.);
            if track_alpha > 0. {
                let track_bg = colors.track;
                let radius = geom.track_pill.size.width.min(geom.track_pill.size.height) / 2.;
                window.paint_quad(
                    fill(geom.track_pill, track_bg.opacity(track_alpha))
                        .corner_radii(radius.max(px(0.))),
                );
            }
        }

        if !prepaint.hidden
            && let Some(geom) = &prepaint.geom
        {
            // 拖拽 → accent 强调色；否则 thumb 常态色（透明度随插值过渡）。
            let dragging = prepaint.state.read(cx).drag_grab.is_some();
            let alpha = if dragging {
                THUMB_ALPHA_DRAG
            } else {
                THUMB_ALPHA_REST + (THUMB_ALPHA_HOVER - THUMB_ALPHA_REST) * present
            };
            let base = if dragging { colors.accent } else { colors.thumb };
            // 胶囊滑块 + 轴向微渐变（玻璃质感：头部实、尾部淡）。
            let head = base.opacity(alpha);
            let tail = base.opacity(alpha * THUMB_GRADIENT_TAPER);
            let bg = if is_v {
                linear_gradient(
                    180.,
                    linear_color_stop(head, 0.),
                    linear_color_stop(tail, 1.),
                )
            } else {
                linear_gradient(90., linear_color_stop(head, 0.), linear_color_stop(tail, 1.))
            };
            let radius = geom.thumb_fill.size.width.min(geom.thumb_fill.size.height) / 2.;
            window.paint_quad(fill(geom.thumb_fill, bg).corner_radii(radius.max(px(0.))));
        }
        if let Some(hitbox) = &prepaint.track_hitbox {
            window.set_cursor_style(CursorStyle::PointingHand, hitbox);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[derive(Clone)]
    struct FakeHandle {
        viewport: Bounds<Pixels>,
        offset: Point<Pixels>,
        content: Size<Pixels>,
    }

    impl ScrollbarHandle for FakeHandle {
        fn viewport_bounds(&self) -> Bounds<Pixels> {
            self.viewport
        }
        fn offset(&self) -> Point<Pixels> {
            self.offset
        }
        fn set_offset(&self, _offset: Point<Pixels>) {}
        fn content_size(&self) -> Size<Pixels> {
            self.content
        }
    }

    fn vp() -> Bounds<Pixels> {
        Bounds::new(point(px(0.), px(0.)), size(px(200.), px(400.)))
    }

    #[test]
    fn hides_when_content_fits() {
        let h = FakeHandle {
            viewport: vp(),
            offset: point(px(0.), px(0.)),
            content: size(px(200.), px(400.)),
        };
        assert!(Scrollbar::vertical(&h).geom(vp(), 0.).is_none());
    }

    #[test]
    fn thumb_geometry_scales_with_scroll_and_stays_in_viewport() {
        let content = size(px(200.), px(1600.));
        let make = |offset_y: f32| {
            FakeHandle {
                viewport: vp(),
                offset: point(px(0.), px(offset_y)),
                content,
            }
        };

        let g0 = Scrollbar::vertical(&make(0.)).geom(vp(), 0.).expect("超长内容应显示");
        // 命中长 = (container/scroll_area*container).max(min) 再两端各缩进 inset
        let raw = (400.0_f32 / 1600.0 * 400.0).max(MIN_THUMB_SIZE);
        let expected_hit = (raw - 2.0 * THUMB_INSET).max(0.);
        assert!(
            (g0.thumb_len - expected_hit).abs() < 0.001,
            "拇指长度应按比例（含 inset）"
        );
        assert!(g0.thumb_fill.size.height > px(0.));
        // 未滚动：拇指贴轨道顶部（含 inset）
        assert!(g0.thumb_fill.origin.y.as_f32() >= THUMB_INSET - 0.001);

        // 滚动 25% → 拇指下移，且不越出视口
        let g1 = Scrollbar::vertical(&make(-300.)).geom(vp(), 0.).expect("仍超长");
        assert!(
            g1.thumb_fill.origin.y > g0.thumb_fill.origin.y,
            "下滚后拇指应下移"
        );
        assert!(
            g1.thumb_fill.bottom() <= vp().bottom() + px(0.5),
            "拇指不得越出视口底部"
        );

        // 激活（悬停/拖拽，present=1）比常态更粗
        let g_hot = Scrollbar::vertical(&make(0.)).geom(vp(), 1.).expect("仍超长");
        assert_eq!(g_hot.thumb_fill.size.width, px(THUMB_ACTIVE_WIDTH));
        assert_eq!(g0.thumb_fill.size.width, px(THUMB_WIDTH));
        // 轨道胶囊宽于激活滑块、长度轴铺满轨道高、交叉轴右缘贴住视口右（贴右，对称
        // 于横向贴底；旧几何胶囊以轨道中线居中悬在右缘内侧 3px）。
        assert!(g_hot.track_pill.size.width > g_hot.thumb_fill.size.width);
        assert!(
            (g_hot.track_pill.right() - vp().right()).as_f32().abs() < 0.5,
            "垂直轨道胶囊右缘应贴住视口右（贴右，不悬浮；对称 K144 横向贴底）"
        );
        assert_eq!(g_hot.track_pill.size.height, g_hot.track.size.height);
    }

    #[test]
    fn thumb_width_interpolates_with_present() {
        let h = FakeHandle {
            viewport: vp(),
            offset: point(px(0.), px(0.)),
            content: size(px(200.), px(1600.)),
        };
        let mid = THUMB_WIDTH + (THUMB_ACTIVE_WIDTH - THUMB_WIDTH) * 0.5;
        let g = Scrollbar::vertical(&h).geom(vp(), 0.5).expect("仍超长");
        assert!(
            (g.thumb_fill.size.width.as_f32() - mid).abs() < 0.001,
            "滑块宽度应随插值 0..1 线性过渡"
        );
    }

    #[test]
    fn thumb_width_interpolates_with_present_horizontal() {
        // P2-11 补齐判据：水平滚动条的「轨道悬停扩展」与垂直同律——悬停/拖拽插值
        // （`present` 0→1）驱动滑块由 6px 加粗到 8px、轨道胶囊淡入且与轨道同轴居中。
        let h = FakeHandle {
            viewport: vp(),
            offset: point(px(0.), px(0.)),
            content: size(px(1600.), px(200.)),
        };
        let rest = Scrollbar::horizontal(&h).geom(vp(), 0.).expect("仍超宽");
        let hot = Scrollbar::horizontal(&h).geom(vp(), 1.).expect("仍超宽");
        assert_eq!(rest.thumb_fill.size.height, px(THUMB_WIDTH), "常态滑块 6px");
        assert_eq!(
            hot.thumb_fill.size.height,
            px(THUMB_ACTIVE_WIDTH),
            "悬停/拖拽滑块加粗到 8px"
        );
        assert!(hot.track_pill.size.width > hot.thumb_fill.size.width, "轨道胶囊宽于滑块");
        assert!(
            (hot.track_pill.center().x - hot.track.center().x).as_f32().abs() < 0.5,
            "水平轨道胶囊长度轴应居中于轨道"
        );
        assert!(
            (hot.track_pill.bottom() - vp().bottom()).as_f32().abs() < 0.5,
            "水平轨道胶囊底应贴住视口底（贴底，不悬浮；见 K144 横条空隙复现）"
        );
        assert_eq!(
            hot.track_pill.size.width,
            hot.track.size.width,
            "水平轨道胶囊铺满轨道长"
        );
    }

    #[test]
    fn thumb_hit_and_fill_stay_inside_track() {
        // 回归：旧实现拿 `Bounds::new(top_right()+(-inset,..))` 当左上锚——那是
        // TopRight/BottomLeft 锚算法，整块命中区被外推到轨道外（垂直右越 4px、
        // 水平下越 4px），导致点不到滑块、拖拽永不触发（内容只有松开才跳）。
        let h = FakeHandle {
            viewport: vp(),
            offset: point(px(0.), px(0.)),
            content: size(px(200.), px(1600.)),
        };
        let gv = Scrollbar::vertical(&h).geom(vp(), 0.).expect("仍超长");
        // 垂直条贴右精确钉死（对称 K144 横向贴底）：滑块/命中区右缘 == 轨道右缘
        // （交叉轴不再内缩 THUMB_INSET）；着色区左缘 == 轨道右缘 − 滑块粗细（钉死厚度）。
        // 同时保留左边界守卫：着色区不越轨道左界——旧 TopRight 锚会把命中区整块外推
        // 到轨道右侧导致点不到滑块，此处左界守卫防「贴右后却溢出左缘」的形态。
        let v_track_right = gv.track.right();
        let v_fill_w = gv.thumb_fill.size.width.as_f32();
        assert!(
            (gv.thumb_hit.right() - v_track_right).as_f32().abs() < 0.5,
            "垂直命中区右缘应贴住轨道右缘（{:.1} vs {:.1}）",
            gv.thumb_hit.right().as_f32(),
            v_track_right.as_f32()
        );
        assert!(
            (gv.thumb_fill.right() - v_track_right).as_f32().abs() < 0.5,
            "垂直滑块右缘应贴住轨道右缘（{:.1} vs {:.1}）",
            gv.thumb_fill.right().as_f32(),
            v_track_right.as_f32()
        );
        assert!(
            (gv.thumb_fill.left().as_f32() - (v_track_right.as_f32() - v_fill_w)).abs() < 0.5,
            "垂直滑块着色区左缘应精确=轨道右缘−滑块粗细（{:.1} vs {:.1}）",
            gv.thumb_fill.left().as_f32(),
            v_track_right.as_f32() - v_fill_w
        );
        assert!(
            gv.thumb_fill.left() >= gv.track.left() - px(0.5),
            "垂直滑块着色区不越轨道左界（保留左边界守卫）"
        );

        let hh = FakeHandle {
            viewport: vp(),
            offset: point(px(0.), px(0.)),
            content: size(px(1600.), px(400.)),
        };
        let gh = Scrollbar::horizontal(&hh).geom(vp(), 0.).expect("仍超宽");
        // 水平条贴底精确钉死（K144 修复的新不变式）：滑块/命中区底缘 == 轨道底
        // （交叉轴不再内缩 THUMB_INSET）；着色区顶缘 == 轨道底 − 滑块粗细（钉死厚度）。
        // 同时**恢复旧守卫**：着色区不越轨道上界（fill.top >= track.top−0.5）——
        // 新几何下该式恒真（fill.top=底−6 ≫ track.top=底−16），但作为「贴底后仍不越界」
        // 的边界守卫保留，防止未来把滑块改成超出轨道上沿的形态。
        let h_track_bottom = gh.track.bottom();
        let h_fill_w = gh.thumb_fill.size.height.as_f32();
        assert!(
            (gh.thumb_hit.bottom() - h_track_bottom).as_f32().abs() < 0.5,
            "水平命中区下缘应贴住轨道底（{:.1} vs {:.1}）",
            gh.thumb_hit.bottom().as_f32(),
            h_track_bottom.as_f32()
        );
        assert!(
            (gh.thumb_fill.bottom() - h_track_bottom).as_f32().abs() < 0.5,
            "水平滑块下缘应贴住轨道底（{:.1} vs {:.1}）",
            gh.thumb_fill.bottom().as_f32(),
            h_track_bottom.as_f32()
        );
        assert!(
            (gh.thumb_fill.top().as_f32() - (h_track_bottom.as_f32() - h_fill_w)).abs() < 0.5,
            "水平滑块着色区顶缘应精确=轨道底−滑块粗细（{:.1} vs {:.1}）",
            gh.thumb_fill.top().as_f32(),
            h_track_bottom.as_f32() - h_fill_w
        );
        assert!(
            gh.thumb_fill.top() >= gh.track.top() - px(0.5),
            "水平滑块着色区不越轨道上界（恢复旧守卫）"
        );
    }

    // —— Auto 可配置参数：先红后绿（K144），默认值必须保全现状 ——

    /// 构造一个内容超长、可滚动的竖直 Auto 滚动条（几何相关测试共用）。
    fn auto_scrollbar(h: &FakeHandle) -> Scrollbar<FakeHandle> {
        Scrollbar::vertical(h).mode(ScrollbarMode::Auto)
    }

    #[test]
    fn autohide_delay_parameter_takes_effect() {
        // 默认 2s：滚动后 0.5s 不隐藏、3s 隐藏；从未滚动 → 隐藏。
        let h = FakeHandle {
            viewport: vp(),
            offset: point(px(0.), px(0.)),
            content: size(px(200.), px(1600.)),
        };
        let sb = auto_scrollbar(&h);
        assert!(!sb.auto_hidden(Some(Duration::from_millis(500)), false, false), "滚动刚发生不隐藏");
        assert!(sb.auto_hidden(Some(Duration::from_secs(3)), false, false), "超过默认 2s 隐藏");
        assert!(sb.auto_hidden(None, false, false), "从未滚动即隐藏");
        // autohide(3s)：2s 时仍不隐藏、3s 隐藏 → 参数生效。
        let sb3 = sb.autohide(Duration::from_secs(3));
        assert!(!sb3.auto_hidden(Some(Duration::from_secs(2)), false, false), "3s 延时下 2s 仍显示");
        assert!(sb3.auto_hidden(Some(Duration::from_secs(3)), false, false), "3s 延时下 3s 隐藏");
    }

    #[test]
    fn drag_keeps_visible_during_drag() {
        let h = FakeHandle {
            viewport: vp(),
            offset: point(px(0.), px(0.)),
            content: size(px(200.), px(1600.)),
        };
        let sb = auto_scrollbar(&h);
        // 默认 drag_keep_visible=true：已过 idle 但正拖拽 → 不隐藏。
        assert!(!sb.auto_hidden(Some(Duration::from_secs(10)), false, true), "拖拽期间不隐藏");
        // drag_keep_visible(false)：拖拽不保显 → 仍隐藏。
        let sb_off = sb.drag_keep_visible(false);
        assert!(sb_off.auto_hidden(Some(Duration::from_secs(10)), false, true), "关闭拖拽保显后照常隐藏");
    }

    #[test]
    fn hide_on_hover_false_does_not_keep_visible_on_hover() {
        let h = FakeHandle {
            viewport: vp(),
            offset: point(px(0.), px(0.)),
            content: size(px(200.), px(1600.)),
        };
        let sb = auto_scrollbar(&h);
        // 默认 hide_on_hover=true：已过 idle 但正悬停 → 不隐藏（hover 保显）。
        assert!(!sb.auto_hidden(Some(Duration::from_secs(10)), true, false), "默认 hover 保显");
        // hide_on_hover(false)：悬停不再保显 → 隐藏。
        let sb_off = sb.hide_on_hover(false);
        assert!(sb_off.auto_hidden(Some(Duration::from_secs(10)), true, false), "关闭保显后悬停也隐藏");
        // 未过 idle 不隐藏（对照）。
        assert!(!sb_off.auto_hidden(Some(Duration::from_millis(10)), true, false), "未过 idle 不隐藏");
    }

    #[test]
    fn thumb_min_parameter_takes_effect() {
        // 小视口 + 短内容：raw 命中长 = 100/300*100 ≈ 33.3 < 默认 min 48 → 被钳到 48。
        let vp_small = Bounds::new(point(px(0.), px(0.)), size(px(100.), px(100.)));
        let h = FakeHandle {
            viewport: vp_small,
            offset: point(px(0.), px(0.)),
            content: size(px(100.), px(300.)),
        };
        // 默认 48：geom.thumb_len 是两端缩进 inset 后的命中长 → + 2*inset = 原始滑块长 = 48。
        let g = Scrollbar::vertical(&h).geom(vp_small, 0.).expect("仍超长");
        assert!(
            (g.thumb_len + 2.0 * THUMB_INSET - 48.).abs() < 0.001,
            "默认最小滑块 48 生效"
        );
        // thumb_min(24)：33.3 > 24 → 不再钳，滑块明显更短。
        let g2 = Scrollbar::vertical(&h).thumb_min(24.).geom(vp_small, 0.).expect("仍超长");
        assert!(
            g2.thumb_len + 2.0 * THUMB_INSET < 40.,
            "thumb_min=24 允许更短滑块"
        );
        // 断言几何仍不越视口。
        assert!(g2.thumb_fill.bottom() <= vp_small.bottom() + px(0.5));
    }

    #[test]
    fn auto_widen_false_keeps_rest_width_on_active() {
        let h = FakeHandle {
            viewport: vp(),
            offset: point(px(0.), px(0.)),
            content: size(px(200.), px(1600.)),
        };
        let sb = Scrollbar::vertical(&h).auto_widen(false);
        let rest = sb.geom(vp(), 0.).expect("仍超长");
        let hot = sb.geom(vp(), 1.).expect("仍超长");
        assert_eq!(rest.thumb_fill.size.width, px(THUMB_WIDTH), "常态 6px");
        assert_eq!(
            hot.thumb_fill.size.width,
            px(THUMB_WIDTH),
            "auto_widen=false 激活不增宽（保持 6px）"
        );
        // 对照：默认激活加宽到 8px（现状保全）。
        let dflt = Scrollbar::vertical(&h).geom(vp(), 1.).expect("仍超长");
        assert_eq!(dflt.thumb_fill.size.width, px(THUMB_ACTIVE_WIDTH), "默认激活加宽 8px");
    }

    /// `auto_hidden` 在 `Since::Before(None)` 等形态下对 Always 模式不定义——此测试只核对
    /// 默认构造的 Auto 参数值（默认保全现状的看门狗）。
    #[test]
    fn auto_params_default_preserve_current_behavior() {
        let h = FakeHandle {
            viewport: vp(),
            offset: point(px(0.), px(0.)),
            content: size(px(200.), px(1600.)),
        };
        let sb = auto_scrollbar(&h);
        assert_eq!(sb.autohide_idle, AUTO_HIDE_IDLE, "默认 2s");
        assert_eq!(sb.min_thumb, MIN_THUMB_SIZE, "默认 48");
        assert!(sb.hide_on_hover, "默认 hover 保显");
        assert!(sb.drag_keep_visible, "默认拖拽常显");
        assert!(sb.auto_widen, "默认激活增宽");
        assert!(
            sb.thumb_color.is_none() && sb.track_color.is_none() && sb.accent_color.is_none(),
            "颜色参数默认 None（走全局主题，status quo 保全）"
        );
    }

    // —— 颜色参数（P4 深紫扩展点第一块）：注入生效 + 默认回退全局，二者都须守护 ——

    #[gpui::test]
    async fn color_params_override_global_and_default_to_global(cx: &mut TestAppContext) {
        let theme = cx.update(|cx| crate::ui::theme::Theme::global(cx).clone());
        // 默认 None → 全局主题（现状）：editor/pdf/tabs 等全部调用点零变化。
        let dflt = resolve_colors(None, None, None, &theme);
        assert_eq!(dflt.thumb, theme.colors.fg.into(), "默认 thumb=全局 fg");
        assert_eq!(dflt.track, theme.colors.border.into(), "默认 track=全局 border");
        assert_eq!(dflt.accent, theme.colors.accent.into(), "默认 accent=全局 accent");
        // 三参全注入 → paint 用量色而非全局（assert 注入 Hsla 生效）。
        let injected: Hsla = gpui::rgb(0x7c5cfc).into();
        let got = resolve_colors(Some(injected), Some(injected), Some(injected), &theme);
        assert_eq!(got.thumb, injected, "注入 thumb 生效");
        assert_eq!(got.track, injected, "注入 track 生效");
        assert_eq!(got.accent, injected, "注入 accent 生效");
        assert_ne!(got.thumb, theme.colors.fg.into(), "注入后不再取全局 fg");
        // 单参注入、其余默认：只覆盖对应槽（调用点部分注入安全）。
        let half = resolve_colors(Some(injected), None, None, &theme);
        assert_eq!(half.thumb, injected, "仅注入 thumb");
        assert_eq!(half.track, theme.colors.border.into(), "track 仍默认全局");
        assert_eq!(half.accent, theme.colors.accent.into(), "accent 仍默认全局");
    }

    // —— N1：精确空闲自隐时序（真实事件 + fake 时钟；验证「上次活动 + 精确 idle」，非轮询边界）——

    /// 承载 Auto 滚动条的宿主视图：render 直接返回 `ui::Scrollbar`，让窗口真实装配 + 每帧
    /// 注册窗口级事件监听（`report_visible_to` 槽回写可见性快照）。
    struct AutoSbHost {
        handle: FakeHandle,
        shown: Rc<Cell<bool>>,
        idle: Duration,
    }

    impl gpui::Render for AutoSbHost {
        fn render(&mut self, _: &mut gpui::Window, _: &mut gpui::Context<Self>) -> impl gpui::IntoElement {
            Scrollbar::vertical(&self.handle)
                .mode(ScrollbarMode::Auto)
                .autohide(self.idle)
                .hide_on_hover(false)
                .drag_keep_visible(true)
                .report_visible_to(self.shown.clone())
        }
    }

    /// N1 回归：自隐必须「上次活动 + **精确 idle**」落定，而非固定轮询边界（活动落在轮询
    /// 边界时旧实现在 ~2×idle 才隐藏）。autohide(3s) 下：
    /// t=0 滚轮 → 显示；t=2.9s 再滚轮 → 仍显示（此时 t=0 那枚一次性定时器已被取消）；
    /// t=2.9+3.0=5.9s 无活动 → **已隐藏**（若退化为轮询边界 ≈6s 则此刻仍可见）。
    #[gpui::test]
    async fn auto_hide_fires_exactly_idle_after_last_activity(cx: &mut TestAppContext) {
        use gpui::{Modifiers, ScrollDelta, ScrollWheelEvent, TouchPhase};
        let shown = Rc::new(Cell::new(false));
        let (_, wcx) = cx.add_window_view({
            let shown = shown.clone();
            move |_, _| AutoSbHost {
                handle: FakeHandle {
                    viewport: vp(),
                    offset: point(px(0.), px(0.)),
                    content: size(px(200.), px(1600.)),
                },
                shown,
                idle: Duration::from_secs(3),
            }
        });
        let mut wcx = wcx;
        let mut wheel = |wcx: &mut gpui::VisualTestContext| {
            let pos = point(px(100.), px(100.)); // 在句柄滚动区（0,0,200,400）内
            wcx.simulate_event(ScrollWheelEvent {
                position: pos,
                delta: ScrollDelta::Pixels(point(px(0.), px(-60.))),
                modifiers: Modifiers::default(),
                touch_phase: TouchPhase::Moved,
            });
            wcx.run_until_parked();
        };
        // 首帧：从未滚动 → 隐藏。
        wcx.update(|window, cx| window.draw(cx).clear(cx));
        wcx.run_until_parked();
        assert!(!shown.get(), "初始应隐藏");
        // t=0：滚轮 → 显示。
        wheel(&mut wcx);
        wcx.update(|window, cx| window.draw(cx).clear(cx));
        wcx.run_until_parked();
        assert!(shown.get(), "t=0 滚轮后应显示");
        // t=2.9s：再滚轮（取消 t=0 定时器、重排）→ 仍显示。
        wcx.executor().advance_clock(Duration::from_millis(2900));
        wcx.run_until_parked();
        wheel(&mut wcx);
        wcx.update(|window, cx| window.draw(cx).clear(cx));
        wcx.run_until_parked();
        assert!(shown.get(), "t=2.9s 再滚轮后仍显示");
        // t=2.9+3.0=5.9s：无活动 → 一次性定时器到点即隐（精确 3s，非轮询边界 ≈6s）。
        wcx.executor().advance_clock(Duration::from_millis(3000));
        wcx.run_until_parked();
        assert!(!shown.get(), "距上次活动精确 3.0s 应已隐藏（证明非轮询边界延迟）");
    }

    // —— 复现：点击轨道跳转（`scrollbar.rs:707-716`）目标错乘 `scroll_area` 而非
    // `(scroll_area - container)`（= max_scroll），thumb 落点系统性偏（K144 复现先行）——
    //
    // 视觉坐标链（vp=200x400、content=200x1600）：
    //   scroll_area=1600, container=400 → max_scroll=1200, safe_min=-1200（geom :442/:501）
    //   raw thumb_len=100, hit_len=100-2·4=92（geom.thumb_len，含两端 THUMB_INSET 内缩）
    //   denom=400-92=308, track 竖直 x∈[184,200]
    //   点击 y → pct=(y - hit_len/2 - 0)/308=(y-46)/308（滑块中心对准点击位）
    //   期望 target = -(max_scroll × pct)；当前实现 = -(scroll_area × pct)（错，:716）
    //   ① y=200 → pct=(200-46)/308=0.5 → 期望 -600；当前 -(1600×0.5)=-800（偏大 200）
    //   ② y=320 → pct=274/308≈0.8896 → 期望 -1200×274/308≈-1067.53；
    //     当前 -(1600×0.8896)≈-1423 被夹到 safe_min=-1200（整体到顶）

    /// 记录 `set_offset` 落点的句柄：几何只读，把点击跳转写回的目标 offset 存进 Rc<Cell>。
    #[derive(Clone)]
    struct ClickProbeHandle {
        viewport: Bounds<Pixels>,
        offset: Point<Pixels>,
        content: Size<Pixels>,
        recorded: Rc<Cell<Option<Point<Pixels>>>>,
    }

    impl ScrollbarHandle for ClickProbeHandle {
        fn viewport_bounds(&self) -> Bounds<Pixels> {
            self.viewport
        }
        fn offset(&self) -> Point<Pixels> {
            self.offset
        }
        fn set_offset(&self, offset: Point<Pixels>) {
            self.recorded.set(Some(offset));
        }
        fn content_size(&self) -> Size<Pixels> {
            self.content
        }
    }

    /// 承载 Scrollbar 的宿主：Always 模式（不掺 Auto 自隐定时器），渲染供 paint 注册点击监听。
    struct ClickProbeHost {
        handle: ClickProbeHandle,
    }

    impl gpui::Render for ClickProbeHost {
        fn render(&mut self, _: &mut gpui::Window, _: &mut gpui::Context<Self>) -> impl gpui::IntoElement {
            Scrollbar::vertical(&self.handle).mode(ScrollbarMode::Always)
        }
    }

    #[gpui::test]
    async fn click_track_jump_uses_max_scroll_not_scroll_area(cx: &mut TestAppContext) {
        use gpui::{Modifiers, MouseButton};

        let recorded = Rc::new(Cell::new(None));
        let (_, wcx) = cx.add_window_view({
            let recorded = recorded.clone();
            let handle = ClickProbeHandle {
                viewport: vp(),
                offset: point(px(0.), px(0.)),
                content: size(px(200.), px(1600.)), // scroll_area=1600 > container=400
                recorded,
            };
            move |_, _| ClickProbeHost { handle }
        });
        let mut wcx = wcx;
        wcx.update(|window, cx| window.draw(cx).clear(cx)); // 首帧：prepaint 几何 + paint 注册点击监听
        wcx.run_until_parked();

        // 轨道中线 x=192（轨道 x∈[184,200]），y 定 pct。
        let track_x = px(192.0);
        let click = |wcx: &mut gpui::VisualTestContext, y: f32| {
            recorded.set(None);
            wcx.simulate_mouse_move(point(track_x, px(y)), MouseButton::Left, Modifiers::default());
            wcx.simulate_mouse_down(point(track_x, px(y)), MouseButton::Left, Modifiers::default());
            wcx.run_until_parked();
            recorded.get().map(|p| p.y.as_f32()).expect("点击轨道应触发 set_offset")
        };

        // ① 中点（pct=0.5）：期望 -(1200×0.5)=-600；当前 -(1600×0.5)=-800。
        let y = click(&mut wcx, 200.0);
        assert!(
            (y + 600.0).abs() < 1.0,
            "点击轨道中点（pct=0.5）落点应=-(max_scroll×0.5)=-600，实际 {y}（当前实现错乘 scroll_area=-800）"
        );

        // ② 近底（y=320）：pct=(320−thumb_hit/2)/denom=(320−46)/(400−92)=274/308≈0.8896
        //   （geom.thumb_len=hit_len=92，含两端 THUMB_INSET 内缩；非裸 thumb_len=100）。
        //   期望 -(1200×274/308)≈-1067.53；旧实现错乘 scroll_area=-(1600×0.8896)≈-1423
        //   被夹到 safe_min=-1200（整体到顶）。
        let y = click(&mut wcx, 320.0);
        let expect = -(1200.0 * 274.0 / 308.0);
        assert!(
            (y - expect).abs() < 1.0,
            "点击轨道近底（pct=274/308≈0.8896）落点应=-(max_scroll×pct)≈-1067.53，实际 {y}（当前实现越界被夹到 -1200=整体到顶）"
        );
    }
}
