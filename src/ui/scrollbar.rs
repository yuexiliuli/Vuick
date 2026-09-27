//! 自绘滚动条：`Scrollbar`（垂直/水平）+ [`ScrollbarHandle`] 契约，替代
//! `gpui_component::scroll::{Scrollbar, ScrollbarHandle, ScrollbarMode}`。
//!
//! 行为/几何复刻 gpui-component（轨道 16px 于边缘、滑块 6px 常态 / 8px 激活、最小 48px、
//! 点击跳转、拖拽写回 [`ScrollbarHandle::set_offset`]）。App 内全部调用点原为
//! `ScrollbarMode::Always`，此处 Always 全量实现；`Auto` 提供「滚动后 IDLE 隐藏」的基础版。
//!
//! # 用法
//! 与 gpui-component 对齐：`Scrollbar::vertical(&handle).mode(ScrollbarMode::Always)
//! .viewport_from_layout()`；容器内作为唯一子元素铺满视口（`request_layout` 填满父级）。
//!
//! # 自定义句柄
//! 编辑器/预览的「确定性量程」句柄（`VerticalScrollHandle`/`PreviewVerticalScrollHandle`）
//! 只需把 `impl gpui_component::scroll::ScrollbarHandle` 换成 `impl crate::ui::scrollbar::
//! ScrollbarHandle`——契约方法完全相同。也内置了对 `gpui::ScrollHandle` / `ListState` 的实现。
//!
//! # 生命周期（gpui 约束）
//! 几何在 `prepaint` 计算（读句柄量程、读写元素级状态），鼠标监听在 `paint` 注册——
//! gpui 的 `window.on_mouse_event` 只允许在 paint 阶段调用（`debug_assert_paint`）。
//!
//! # 状态隔离
//! 跨帧拖拽/悬停状态按元素 id 存放（`use_keyed_state`），每个滚动条实例独立；默认 id 取自
//! 调用点（`ElementId::CodeLocation`），多实例天然分开。

use std::panic::Location;
use std::time::{Duration, Instant};

use gpui::{
    App, Bounds, ContentMask, CursorStyle, Element, ElementId, Entity, EntityId,
    GlobalElementId, Hitbox, HitboxBehavior, Hsla, InspectorElementId, IntoElement, LayoutId,
    ListState, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Point, Pixels, ScrollHandle,
    ScrollWheelEvent, Size, Window, fill, point, px, size,
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
    /// 是否悬停在轨道上（激活更宽）。
    hovering: bool,
    /// 最近一次滚动发生时间（`Auto` 模式隐藏依据）。
    last_scroll: Option<Instant>,
    /// `Auto` 模式是否已隐藏。
    hidden: bool,
}

/// 单帧几何（prepaint 计算，paint/事件闭包共用；位置/边界为 Pixel、量程为 f32 标量）。
#[derive(Clone)]
struct Geom {
    /// 轨道条（垂直=右缘 16px，水平=底缘 16px）。
    track: Bounds<Pixels>,
    /// 滑块命中区（整轨宽，含 inset）。
    thumb_hit: Bounds<Pixels>,
    /// 滑块着色区（滑块宽）。
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
pub struct Scrollbar<H: ScrollbarHandle + Clone> {
    handle: H,
    axis: ScrollbarAxis,
    mode: ScrollbarMode,
    id: ElementId,
    layout_viewport: bool,
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
    fn geom(&self, viewport: Bounds<Pixels>, hovering: bool) -> Option<Geom> {
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

        let thumb_len = (container / scroll_area * container).max(MIN_THUMB_SIZE);
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

        let fill_w = if hovering { px(THUMB_ACTIVE_WIDTH) } else { px(THUMB_WIDTH) };
        let (thumb_hit, thumb_fill) = if is_v {
            let origin = track.top_right() + point(px(-THUMB_INSET), px(THUMB_INSET + thumb_start));
            (
                Bounds::new(origin, size(track_w, px(hit_len))),
                Bounds::new(origin, size(fill_w, px(hit_len))),
            )
        } else {
            let origin =
                track.bottom_left() + point(px(THUMB_INSET + thumb_start), px(-THUMB_INSET));
            (
                Bounds::new(origin, size(px(hit_len), track_w)),
                Bounds::new(origin, size(px(hit_len), fill_w)),
            )
        };

        Some(Geom {
            track,
            thumb_hit,
            thumb_fill,
            thumb_len: hit_len,
            scroll_area,
            container,
            safe_min: -max_scroll,
        })
    }
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
    track_hitbox: Hitbox,
    handle: H,
    axis: ScrollbarAxis,
    mode: ScrollbarMode,
    state: Entity<UiState>,
    view_id: EntityId,
}

impl<H: ScrollbarHandle + Clone + 'static> PrepaintState<H> {
    /// paint 阶段注册鼠标/滚轮监听（gpui 只允许 paint 里 `on_mouse_event`）。
    fn register_listeners(&mut self, window: &mut Window, _cx: &mut App) {
        let view_id = self.view_id;
        let mode = self.mode;
        let is_v = self.axis.is_vertical();

        // 记录最近一次滚动（Auto 隐藏依据；Always 无害）。
        window.on_mouse_event({
            let track = self.geom.as_ref().map(|g| g.track);
            let state = self.state.clone();
            move |event: &ScrollWheelEvent, phase, _window, cx| {
                if phase.bubble()
                    && let Some(track) = track
                    && track.contains(&event.position)
                {
                    state.update(cx, |s, _| {
                        s.last_scroll = Some(Instant::now());
                        if mode == ScrollbarMode::Auto {
                            s.hidden = false;
                        }
                    });
                    cx.notify(view_id);
                }
            }
        });

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
            move |event: &MouseDownEvent, phase, _window, cx| {
                if !phase.bubble() || !track.contains(&event.position) {
                    return;
                }
                cx.stop_propagation();
                let offset = handle.offset();
                if thumb_hit.contains(&event.position) {
                    let grab = event.position - thumb_hit.origin;
                    handle.start_drag();
                    state.update(cx, |s, _| s.drag_grab = Some(grab));
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
                    let target = (-(scroll_area * pct)).max(safe_min);
                    if is_v {
                        handle.set_offset(point(offset.x, px(target)));
                    } else {
                        handle.set_offset(point(px(target), offset.y));
                    }
                }
                cx.notify(view_id);
            }
        });

        // 移动：更新悬停 + 拖拽写回 offset。
        window.on_mouse_event({
            let handle = handle.clone();
            let state = state.clone();
            move |event: &MouseMoveEvent, _phase, _window, cx| {
                let hovered = track.contains(&event.position);
                let mut notify = false;

                state.update(cx, |s, _| s.hovering = hovered);

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
                    if is_v {
                        handle.set_offset(point(offset.x, px(target)));
                    } else {
                        handle.set_offset(point(px(target), offset.y));
                    }
                    notify = true;
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
            move |_event: &MouseUpEvent, phase, _window, cx| {
                if phase.bubble() && state.read(cx).drag_grab.is_some() {
                    handle.end_drag();
                    state.update(cx, |s, _| s.drag_grab = None);
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
        None
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
        let track_hitbox = window.with_content_mask(
            Some(ContentMask { bounds: viewport }),
            |window| window.insert_hitbox(viewport, HitboxBehavior::Normal),
        );

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
        let hidden = if self.mode == ScrollbarMode::Auto {
            match st.last_scroll {
                Some(ts) => ts.elapsed() >= AUTO_HIDE_IDLE && !st.hovering,
                None => true,
            }
        } else {
            st.hidden
        };

        let geom = self.geom(viewport, st.hovering);
        PrepaintState {
            geom,
            hidden,
            track_hitbox,
            handle: self.handle.clone(),
            axis: self.axis,
            mode: self.mode,
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
        let thumb_bg: Hsla = theme.colors.fg.into();
        let thumb_bg = if prepaint.state.read(cx).hovering {
            thumb_bg.opacity(0.55)
        } else {
            thumb_bg.opacity(0.35)
        };

        if !prepaint.hidden
            && let Some(geom) = &prepaint.geom
        {
            let radius =
                (geom.thumb_fill.size.width.min(geom.thumb_fill.size.height) / 2.0).max(px(0.));
            // 滑块
            window.paint_quad(fill(geom.thumb_fill, thumb_bg).corner_radii(radius));
        }
        window.set_cursor_style(CursorStyle::PointingHand, &prepaint.track_hitbox);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(Scrollbar::vertical(&h).geom(vp(), false).is_none());
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

        let g0 = Scrollbar::vertical(&make(0.)).geom(vp(), false).expect("超长内容应显示");
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
        let g1 = Scrollbar::vertical(&make(-300.)).geom(vp(), false).expect("仍超长");
        assert!(
            g1.thumb_fill.origin.y > g0.thumb_fill.origin.y,
            "下滚后拇指应下移"
        );
        assert!(
            g1.thumb_fill.bottom() <= vp().bottom() + px(0.5),
            "拇指不得越出视口底部"
        );

        // 激活（悬停）比常态更粗
        let g_hot = Scrollbar::vertical(&make(0.)).geom(vp(), true).expect("仍超长");
        assert_eq!(g_hot.thumb_fill.size.width, px(THUMB_ACTIVE_WIDTH));
        assert_eq!(g0.thumb_fill.size.width, px(THUMB_WIDTH));
    }
}
