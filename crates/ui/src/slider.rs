//! 自绘横向滑块 `ui::Slider`：可交互拖条 / 只读进度条。
//!
//! # 能力
//! - 整条 16px 命中高即命中区（whole-bar 命中，**无 thumb 抓取**）；点击/拖拽按 x 映射 0..1。
//! - 交互语义沿用音乐/视频进度条：x 落在命中盒内才出分数（**music 语义**，`fraction_at`
//!   越界返回 `None`），坐标按条宽钳 [0,1]；**拖拽出条**（x<0 / x>W）值持续钳 0/1 跟随，
//!   松手按末值提交。
//! - 三类回调：[`Slider::on_change`]（点按即发）、[`Slider::on_change_during_drag`]
//!   （拖拽 move）、[`Slider::on_commit`]（`CommitMode::OnRelease` 松手精提交；
//!   `Immediate` 模式值在拖拽途中已持续下发，松手不再补发）。
//! - 可选 thumb（[`SliderThumb`]：base/hover 半径、颜色、可选辉光），hover 弹簧放大
//!   （参数同 `motion::hover_spring`，临界阻尼），圆心始终贴填充末端、不随弹簧漂移。
//! - `interactive(false)` = 只读进度条：不插入命中盒（无 cursor）、不注册任何鼠标监听
//!   （点/拖不触发任何回调）。
//!
//! # 几何与状态
//! 复用 `scrollbar` 姿势：`prepaint` 计算几何 + 元素级状态（[`Window::use_keyed_state`]
//! 按元素 id 隔离），`paint` 注册鼠标监听（gpui `on_mouse_event` 仅允许 paint 阶段）与
//! 自绘。命中盒（`cell`）由组件**自持**（本地布局盒，prepaint 的 `bounds`），消费方不再
//! 传 `Rc<Cell<Bounds>>`。
//!
//! # 显示
//! 命中区 16px 高 + 居中薄轨（`track_height` + 圆角）+ 左对齐填充（Solid 实色 /
//! Gradient 横向渐变），填充末端 = 当前 fraction × 条宽。

use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{
    App, Bounds, CursorStyle, Element, ElementId, Entity, EntityId, GlobalElementId, Hitbox,
    HitboxBehavior, Hsla, InspectorElementId, IntoElement, LayoutId, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Point, Pixels, SpringConfig, SpringState, Window, fill,
    linear_color_stop, linear_gradient, point, px, size,
};

/// 命中区高度（px）：整条即命中区，不依赖 thumb 位置。
const HIT_HEIGHT: f32 = 16.0;
/// 默认薄轨高（px）。
const DEFAULT_TRACK_HEIGHT: f32 = 3.0;
/// thumb 辉光透明度（叠加在原色之上的弱化层）。
const THUMB_GLOW_ALPHA: f32 = 0.35;
/// thumb 辉光半径相对 thumb 半径的倍率。
const THUMB_GLOW_SCALE: f32 = 2.4;

/// 拖动值钳位（0..1；NaN 归一 0）。
fn clamp_value(v: f32) -> f32 {
    if v.is_nan() {
        0.0
    } else {
        v.clamp(0.0, 1.0)
    }
}

/// 回调别名：值 + 窗口/应用上下文。
type SliderHandler = Rc<dyn Fn(f32, &mut Window, &mut App)>;

/// 填充样式：左对齐的进度填充。
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum SliderFill {
    /// 单调实色。
    Solid(Hsla),
    /// 左 → 右横向渐变（`[起点, 终点]`）。
    Gradient([Hsla; 2]),
}

/// thumb 规格：常态 / hover 半径（弹簧插值）、颜色、可选辉光。
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct SliderThumb {
    /// 常态半径（px，弹簧 0）。
    pub base: f32,
    /// hover 半径（px，弹簧 1）。
    pub hover: f32,
    /// thumb 填充色。
    pub color: Hsla,
    /// 可选辉光色（低于 alpha 的大半径光环）。
    pub glow: Option<Hsla>,
}

/// 提交时机：值变更的"精提交"发生在何时。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CommitMode {
    /// 即时：点按/拖拽途中即持续生效，松手不补发 on_commit。
    Immediate,
    /// 松手精提交：松开鼠标时发 on_commit(末值)。
    OnRelease,
}

/// 自绘横向滑块。见模块文档。
pub struct Slider {
    id: ElementId,
    /// 显示源值（0..1，builder 时**内存 clamp**；拖拽中由 `scrollbar` 式拖拽 fraction 覆盖）。
    /// 公开只读字段：「value 读回 = clamp 后的值」。
    pub value: f32,
    interactive: bool,
    commit_mode: CommitMode,
    track_height: Pixels,
    /// 轨/填充圆角半径；None → 轨道高一半（胶囊）。
    track_radius: Option<f32>,
    /// 轨道底色；None → 主题 border。
    track_color: Option<Hsla>,
    /// 进度填充；None → 主题 accent 实色。
    fill: Option<SliderFill>,
    /// 可选 thumb；None = 不画。
    thumb: Option<SliderThumb>,
    on_change: Option<SliderHandler>,
    on_change_during_drag: Option<SliderHandler>,
    on_commit: Option<SliderHandler>,
}

/// 元素级跨帧状态（按元素 id + `"hover"` 键隔离，见模块文档）。
#[derive(Default)]
struct SliderState {
    /// 拖拽中（down 设 / up 清）；move 只在拖拽中跟随。
    dragging: bool,
    /// 指针是否在命中盒内（悬停，驱动 thumb 弹簧 target）。
    hovered: bool,
    /// 拖拽中的当前 fraction（down/move 更新；up 清除后显示回落到消费方喂入的 value）。
    drag_fraction: Option<f32>,
    /// thumb 弹簧当前位置（0..1，半径随它插值）。
    present: f32,
    /// 弹簧当前速度（跨帧保持，供解析积分续推）。
    velocity: f32,
    /// 上一帧时间（计算 dt）。
    last_frame: Option<Instant>,
}

/// 单帧几何：全部由 `cell`（命中/布局盒）+ display fraction 推导。
#[derive(Clone)]
struct Geom {
    /// 命中盒（整个条，16px 高，width = 布局宽）。
    cell: Bounds<Pixels>,
    /// 居中薄轨（背景）。
    track: Bounds<Pixels>,
    /// 左对齐填充（末端 = fraction × 条宽）。
    fill: Bounds<Pixels>,
    /// 填充末端（thumb 圆心）。
    fill_end: Point<Pixels>,
    /// 轨/填充圆角半径（px 标量）。
    radius: f32,
}

/// x 坐标 → 0..1 进度（**music 语义**）：x 在命中盒横向范围内才 `Some`（含右缘），
/// 越界返回 `None`；坐标按条宽钳 [0,1]。未布局（宽<=0）返回 `None`。
fn fraction_at(cell: &Bounds<Pixels>, x: Pixels) -> Option<f32> {
    let width = cell.size.width.as_f32();
    if width <= 0.0 {
        return None;
    }
    let left = cell.origin.x.as_f32();
    let right = (cell.origin.x + cell.size.width).as_f32();
    let x = x.as_f32();
    if x < left || x > right {
        return None;
    }
    Some(((x - left) / width).clamp(0.0, 1.0))
}

/// 拖拽期间的分母映射：无论指针是否出盒，一律按 x 映射并钳 [0,1]（出条取端点），
/// 只在拖拽中（`dragging`）使用——点击门控仍走 `fraction_at`（含盒语义）。
fn drag_fraction(cell: &Bounds<Pixels>, x: Pixels) -> f32 {
    let width = cell.size.width.as_f32().max(0.001);
    let left = cell.origin.x.as_f32();
    ((x.as_f32() - left) / width).clamp(0.0, 1.0)
}

/// 2D 命中判断（含右/下缘）：gpui `Bounds::contains` 是右/下半开区间，而滑条语义——
/// music `fraction_at`——右缘 x=W 属盒内，故手动闭区间判断。
fn hit_contains(cell: &Bounds<Pixels>, pos: Point<Pixels>) -> bool {
    let left = cell.origin.x.as_f32();
    let right = (cell.origin.x + cell.size.width).as_f32();
    let top = cell.origin.y.as_f32();
    let bottom = (cell.origin.y + cell.size.height).as_f32();
    let x = pos.x.as_f32();
    let y = pos.y.as_f32();
    x >= left && x <= right && y >= top && y <= bottom
}

/// thumb 半径：base → hover 随弹簧 `present` 插值（hover 不大于 base 时恒为 base）。
fn thumb_radius(thumb: &SliderThumb, present: f32) -> f32 {
    if thumb.hover > thumb.base {
        thumb.base + (thumb.hover - thumb.base) * present.clamp(0.0, 1.0)
    } else {
        thumb.base
    }
}

/// thumb 弹簧单步推进：与 `motion::hover_spring` 同参数（临界阻尼，解析积分），
/// 返回 `(新位置, 新速度, 是否仍在动画)`。提取成纯函数便于直接单测 0↔1 收敛。
fn spring_step(present: f32, velocity: f32, target: f32, dt: f32) -> (f32, f32, bool) {
    let config = SpringConfig::new(
        crate::motion::HOVER_STIFFNESS,
        2.0 * crate::motion::HOVER_STIFFNESS.sqrt(),
        1.0,
    );
    let next = config.step(
        SpringState {
            position: present,
            velocity,
        },
        target,
        dt,
    );
    let animating = !config.is_settled(next, target, crate::motion::HOVER_EPSILON);
    (next.position, next.velocity, animating)
}

impl Slider {
    /// 以调用方指定的 id 构建（跨帧状态按 id 隔离；多实例用不同 id）。
    pub fn new(id: impl Into<ElementId>) -> Self {
        Self {
            id: id.into(),
            value: 0.0,
            interactive: true,
            commit_mode: CommitMode::Immediate,
            track_height: px(DEFAULT_TRACK_HEIGHT),
            track_radius: None,
            track_color: None,
            fill: None,
            thumb: None,
            on_change: None,
            on_change_during_drag: None,
            on_commit: None,
        }
    }

    /// 显示源值（0..1，**内存 clamp**：越界取端点、NaN 归 0）。读回经公开字段
    /// [`Slider::value`]（== clamp 后的值）。
    pub fn value(mut self, value: f32) -> Self {
        self.value = clamp_value(value);
        self
    }

    /// `false` = 只读进度条：无 cursor、点/拖不触发任何回调。
    pub fn interactive(mut self, interactive: bool) -> Self {
        self.interactive = interactive;
        self
    }

    /// 提交时机（默认 `Immediate`）。
    pub fn commit_mode(mut self, commit_mode: CommitMode) -> Self {
        self.commit_mode = commit_mode;
        self
    }

    pub fn track_height(mut self, track_height: Pixels) -> Self {
        self.track_height = track_height;
        self
    }

    /// 轨/填充圆角半径（px）；None → 轨道高一半（胶囊）。
    pub fn track_radius(mut self, radius: f32) -> Self {
        self.track_radius = Some(radius);
        self
    }

    /// 轨道底色（None → 主题 border）。
    pub fn track_color(mut self, color: Hsla) -> Self {
        self.track_color = Some(color);
        self
    }

    /// 进度填充（None → 主题 accent 实色）。
    pub fn fill(mut self, fill: SliderFill) -> Self {
        self.fill = Some(fill);
        self
    }

    /// `Some(SliderThumb)` 画 thumb；`None` 不画。
    pub fn thumb(mut self, thumb: Option<SliderThumb>) -> Self {
        self.thumb = thumb;
        self
    }

    /// 点按/click 即发（命中盒内按下，值为该点 fraction）。
    #[allow(dead_code)]
    pub fn on_change(mut self, handler: impl Fn(f32, &mut Window, &mut App) + 'static) -> Self {
        self.on_change = Some(Rc::new(handler));
        self
    }

    /// 拖拽 move 持续下发（值为当前拖拽 fraction，拖出条时钳 0/1）。
    #[allow(dead_code)]
    pub fn on_change_during_drag(
        mut self,
        handler: impl Fn(f32, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_change_during_drag = Some(Rc::new(handler));
        self
    }

    /// `CommitMode::OnRelease` 松手精提交（末值：松手点精确分数，窗外沿用最后锁定落点）。
    #[allow(dead_code)]
    pub fn on_commit(mut self, handler: impl Fn(f32, &mut Window, &mut App) + 'static) -> Self {
        self.on_commit = Some(Rc::new(handler));
        self
    }

    /// 几何推演（纯函数，供 paint/测试共用）：命中盒 = 布局盒，轨/填充纵向居中，
    /// 填充末端 = fraction × 条宽。
    fn geometry(&self, cell: Bounds<Pixels>, fraction: f32) -> Geom {
        let fraction = fraction.clamp(0.0, 1.0);
        let width = cell.size.width.as_f32();
        let track_h = self.track_height.as_f32().min(cell.size.height.as_f32());
        let cy = cell.center().y.as_f32();
        let top = cy - track_h / 2.0;
        let fill_w = width * fraction;
        let radius = self.track_radius.unwrap_or(track_h / 2.0).max(0.0);
        Geom {
            cell,
            track: Bounds::new(
                point(cell.origin.x, px(top)),
                size(px(width), px(track_h)),
            ),
            fill: Bounds::new(
                point(cell.origin.x, px(top)),
                size(px(fill_w), px(track_h)),
            ),
            fill_end: point(cell.origin.x + px(fill_w), cell.center().y),
            radius,
        }
    }
}

impl IntoElement for Slider {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for Slider {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState;

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
        // 铺满可用宽、固定 16px 命中高（整条即命中区）。
        let mut style = gpui::Style {
            flex_grow: 1.0,
            flex_shrink: 1.0,
            ..Default::default()
        };
        style.size.width = gpui::relative(1.).into();
        style.size.height = px(HIT_HEIGHT).into();
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
        // 元素级状态：按本滑条 id 隔离跨帧悬停/拖拽/弹簧。
        let state = window.use_keyed_state((self.id.clone(), "hover"), cx, |_, _| {
            SliderState::default()
        });
        let view_id = window.current_view();

        // 显示 fraction：拖拽中跟随手指，否则用消费方喂入的 value（内存 clamp 过）。
        let fraction = state.read(cx).drag_fraction.unwrap_or(self.value);
        let geom = self.geometry(bounds, fraction);

        // 悬停弹簧：与 motion::hover_spring 同参数（临界阻尼），自驱动逐帧收敛。
        // dt 下限 1/60：headless 下帧间隔墙钟≈0，不加下限弹簧几乎不动、收敛要无数帧。
        let now = Instant::now();
        let st = state.read(cx);
        let target = if st.hovered || st.dragging { 1.0 } else { 0.0 };
        let (present, velocity, animating) = if cx.reduce_motion() {
            (target, 0.0, false)
        } else {
            let dt = st
                .last_frame
                .map(|t| now.saturating_duration_since(t))
                .unwrap_or(Duration::from_millis(8))
                .as_secs_f32()
                .max(1.0 / 60.0);
            if st.present == target && st.velocity == 0.0 {
                // 已静止在目标：不重复推弹簧，避免无谓动画帧。
                (target, 0.0, false)
            } else {
                let (p, v, anim) = spring_step(st.present, st.velocity, target, dt);
                (p, v, anim)
            }
        };
        state.update(cx, |s, _| {
            s.present = present;
            s.velocity = velocity;
            s.last_frame = Some(now);
        });
        if animating {
            window.request_animation_frame();
        }

        // 命中盒：仅 interactive 插入（只读模式无事件、无 cursor）。
        let hitbox = self
            .interactive
            .then(|| window.insert_hitbox(geom.cell, HitboxBehavior::Normal));

        // 主题默认色在此解析（prepaint 有 App 上下文，Theme 全局可取）。
        let theme = crate::ui::theme::Theme::global(cx);
        let track_color = self.track_color.unwrap_or_else(|| theme.colors.border.into());
        let fill = self
            .fill
            .unwrap_or(SliderFill::Solid(theme.colors.accent.into()));

        PrepaintState {
            geom,
            present,
            interactive: self.interactive,
            commit_mode: self.commit_mode,
            track_color,
            fill,
            thumb: self.thumb,
            on_change: self.on_change.clone(),
            on_change_during_drag: self.on_change_during_drag.clone(),
            on_commit: self.on_commit.clone(),
            state,
            hitbox,
            view_id,
        }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        prepaint.register_listeners(window, cx);

        let geom = prepaint.geom.clone();
        let radius = px(geom.radius);

        // 薄轨（背景）。
        window.paint_quad(fill(geom.track, prepaint.track_color).corner_radii(radius));
        // 左对齐填充（末端 = fraction × 条宽；fraction=0 时宽度 0，跳过绘制）。
        if geom.fill.size.width.as_f32() > 0.0 {
            let fill_color = match &prepaint.fill {
                SliderFill::Solid(c) => (*c).into(),
                SliderFill::Gradient([a, b]) => linear_gradient(
                    90.,
                    linear_color_stop(*a, 0.),
                    linear_color_stop(*b, 1.),
                ),
            };
            window.paint_quad(fill(geom.fill, fill_color).corner_radii(radius));
        }

        // 可选 thumb：圆心贴填充末端（随 fraction 移动，不随弹簧漂移），半径弹簧插值。
        if let Some(thumb) = prepaint.thumb {
            let r = thumb_radius(&thumb, prepaint.present);
            let c = geom.fill_end;
            if let Some(glow) = thumb.glow {
                let gr = r * THUMB_GLOW_SCALE;
                let gb = Bounds::new(
                    point(c.x - px(gr), c.y - px(gr)),
                    size(px(gr * 2.0), px(gr * 2.0)),
                );
                window.paint_quad(fill(gb, glow.opacity(THUMB_GLOW_ALPHA)).corner_radii(px(gr)));
            }
            let tb = Bounds::new(
                point(c.x - px(r), c.y - px(r)),
                size(px(r * 2.0), px(r * 2.0)),
            );
            window.paint_quad(fill(tb, thumb.color).corner_radii(px(r)));
        }

        // 仅 interactive：手型光标（gpui 只在该命中盒悬停时应用；只读模式无 cursor）。
        if let Some(hitbox) = &prepaint.hitbox {
            window.set_cursor_style(CursorStyle::PointingHand, hitbox);
        }
    }
}

/// `prepaint` 产出、`paint` 消费的状态（公开仅为满足 `Element::PrepaintState` 可见性，
/// 字段私有，别在模块外构造）。
pub struct PrepaintState {
    geom: Geom,
    /// 本帧 thumb 弹簧插值（0..1）。
    present: f32,
    interactive: bool,
    commit_mode: CommitMode,
    track_color: Hsla,
    fill: SliderFill,
    thumb: Option<SliderThumb>,
    on_change: Option<SliderHandler>,
    on_change_during_drag: Option<SliderHandler>,
    on_commit: Option<SliderHandler>,
    state: Entity<SliderState>,
    hitbox: Option<Hitbox>,
    view_id: EntityId,
}

impl PrepaintState {
    /// paint 阶段注册鼠标监听（gpui `on_mouse_event` 仅允许 paint；只读模式不注册）。
    fn register_listeners(&mut self, window: &mut Window, _cx: &mut App) {
        if !self.interactive {
            return;
        }
        let cell = self.geom.cell;
        let state = self.state.clone();
        let view_id = self.view_id;
        let on_change = self.on_change.clone();
        let on_change_during_drag = self.on_change_during_drag.clone();
        let on_commit = self.on_commit.clone();
        let commit_mode = self.commit_mode;

        // 按下：命中盒内才响应（整条 16px 命中高即命中区，无 thumb 抓取）。
        let state_down = state.clone();
        window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
            if !phase.bubble() || !hit_contains(&cell, event.position) {
                return;
            }
            cx.stop_propagation();
            let Some(f) = fraction_at(&cell, event.position.x) else {
                return;
            };
            state_down.update(cx, |s, _| {
                s.dragging = true;
                s.hovered = true;
                s.drag_fraction = Some(f);
            });
            if let Some(cb) = &on_change {
                cb(f, window, cx);
            }
            cx.notify(view_id);
        });

        // 移动：拖拽中跟随手指（分母钳位，出条取端点 0/1）；未拖拽时仅维护悬停态。
        let state_move = state.clone();
        window.on_mouse_event(move |event: &MouseMoveEvent, _phase, window, cx| {
            if state_move.read(cx).dragging {
                cx.stop_propagation();
                let f = drag_fraction(&cell, event.position.x);
                state_move.update(cx, |s, _| s.drag_fraction = Some(f));
                if let Some(cb) = &on_change_during_drag {
                    cb(f, window, cx);
                }
                cx.notify(view_id);
            } else {
                let hovered = hit_contains(&cell, event.position);
                let changed = state_move.update(cx, |s, _| {
                    let old = s.hovered;
                    s.hovered = hovered;
                    old != hovered
                });
                if changed {
                    cx.notify(view_id);
                }
            }
        });

        // 抬起：结束拖拽；OnRelease 用松手精确分数（窗外沿用最后锁定落点）精提交。
        let state_up = state.clone();
        window.on_mouse_event(move |event: &MouseUpEvent, phase, window, cx| {
            if !phase.bubble() || !state_up.read(cx).dragging {
                return;
            }
            cx.stop_propagation();
            // 末值：松手点在盒内取该点精确分数，否则沿用拖拽最后锁定落点（拖出窗外松手）。
            let f = fraction_at(&cell, event.position.x)
                .or(state_up.read(cx).drag_fraction)
                .unwrap_or(0.0);
            if commit_mode == CommitMode::OnRelease
                && let Some(cb) = &on_commit
            {
                cb(f, window, cx);
            }
            state_up.update(cx, |s, _| {
                s.dragging = false;
                s.drag_fraction = None;
            });
            cx.notify(view_id);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{
        Context, InteractiveElement, Modifiers, MouseButton, ParentElement, Render, Styled,
        TestAppContext, VisualTestContext, div, point, px,
    };
    use std::cell::RefCell;
    use std::rc::Rc;

    struct Harness {
        value: f32,
        interactive: bool,
        commit_mode: CommitMode,
        with_thumb: bool,
        changes: Rc<RefCell<Vec<f32>>>,
        drags: Rc<RefCell<Vec<f32>>>,
        commits: Rc<RefCell<Vec<f32>>>,
        render_count: Rc<std::cell::Cell<usize>>,
    }

    impl Render for Harness {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            self.render_count.set(self.render_count.get() + 1);
            let changes = self.changes.clone();
            let drags = self.drags.clone();
            let commits = self.commits.clone();
            let mut slider = Slider::new("sld")
                .value(self.value)
                .interactive(self.interactive)
                .commit_mode(self.commit_mode)
                .track_height(px(4.))
                .track_radius(2.)
                .track_color(gpui::hsla(0.0, 0.0, 0.0, 0.25))
                .fill(SliderFill::Solid(gpui::hsla(0.60, 0.60, 0.90, 1.0)))
                .on_change(move |f, _, _| changes.borrow_mut().push(f))
                .on_change_during_drag(move |f, _, _| drags.borrow_mut().push(f))
                .on_commit(move |f, _, _| commits.borrow_mut().push(f));
            if self.with_thumb {
                slider = slider.thumb(Some(SliderThumb {
                    base: 5.,
                    hover: 8.,
                    color: gpui::hsla(0.60, 0.60, 0.90, 1.0),
                    glow: Some(gpui::hsla(0.60, 0.60, 0.90, 1.0)),
                }));
            }
            div()
                .debug_selector(|| "slider-wrap".to_string())
                .w(px(200.))
                .h(px(16.))
                .child(slider)
        }
    }

    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::type_complexity)]
    fn harness(
        cx: &mut TestAppContext,
        value: f32,
        interactive: bool,
        commit_mode: CommitMode,
        with_thumb: bool,
    ) -> (
        &mut VisualTestContext,
        Rc<RefCell<Vec<f32>>>,
        Rc<RefCell<Vec<f32>>>,
        Rc<RefCell<Vec<f32>>>,
        Rc<std::cell::Cell<usize>>,
    ) {
        let changes = Rc::new(RefCell::new(Vec::new()));
        let drags = Rc::new(RefCell::new(Vec::new()));
        let commits = Rc::new(RefCell::new(Vec::new()));
        let render_count = Rc::new(std::cell::Cell::new(0));
        let (_, cx) = cx.add_window_view({
            let changes = changes.clone();
            let drags = drags.clone();
            let commits = commits.clone();
            let render_count = render_count.clone();
            move |_, _| Harness {
                value,
                interactive,
                commit_mode,
                with_thumb,
                changes,
                drags,
                commits,
                render_count,
            }
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        (cx, changes, drags, commits, render_count)
    }

    /// 命中盒（= wrapper div 布局盒，Slider 铺满其中）。
    fn cell(cx: &mut VisualTestContext) -> Bounds<Pixels> {
        cx.debug_bounds("slider-wrap").expect("slider 应在场")
    }

    // ---- 纯函数：盒语义 / 几何 / 值 clamp ----

    #[test]
    fn fraction_at_is_box_contained_and_clamped() {
        // 演算：盒 (10,4,200,16)；x=60 → (60-10)/200=0.25；右缘 210 → 1.0。
        let cell = Bounds::new(point(px(10.), px(4.)), size(px(200.), px(16.)));
        assert_eq!(fraction_at(&cell, px(10.)), Some(0.0), "左缘=0");
        assert_eq!(fraction_at(&cell, px(60.)), Some(0.25));
        assert_eq!(fraction_at(&cell, px(210.)), Some(1.0), "右缘=1（含右缘）");
        assert_eq!(fraction_at(&cell, px(5.)), None, "越左 → None（music 语义）");
        assert_eq!(fraction_at(&cell, px(300.)), None, "越右 → None");
        let zero = Bounds::new(point(px(0.), px(0.)), size(px(0.), px(16.)));
        assert_eq!(fraction_at(&zero, px(0.)), None, "未布局宽 0 → None");
    }

    #[test]
    fn value_is_clamped_in_memory() {
        assert_eq!(Slider::new("s").value, 0.0, "默认值 0.0");
        assert_eq!(Slider::new("s").value(-0.5).value, 0.0, "负值钳 0");
        assert_eq!(Slider::new("s").value(1.7).value, 1.0, "超上限钳 1");
        assert_eq!(Slider::new("s").value(f32::NAN).value, 0.0, "NaN 归一 0");
    }

    #[test]
    fn geometry_fills_left_aligned_to_fraction() {
        // 演算：盒 (10,4,200,16)；fraction 0.25 → 填充宽 50、轨宽 200、居中线 y=12。
        let cell = Bounds::new(point(px(10.), px(4.)), size(px(200.), px(16.)));
        let s = Slider::new("s").track_height(px(4.));
        let g = s.geometry(cell, 0.25);
        assert_eq!(g.fill.size.width, px(50.));
        assert_eq!(g.track.size.width, px(200.));
        assert!(
            (g.track.center().y - cell.center().y).as_f32().abs() < 0.001,
            "轨道纵向居中于 16px 命中区"
        );
        assert_eq!(
            g.fill_end,
            point(cell.origin.x + px(50.), cell.center().y),
            "填充末端 = fraction × 条宽"
        );
        assert_eq!(g.fill.origin.y, g.track.origin.y, "填充与轨道同高同顶");
        // fraction 越界 → geometry 内部 clamp。
        assert_eq!(s.geometry(cell, -0.3).fill.size.width, px(0.));
        assert_eq!(s.geometry(cell, 1.5).fill.size.width, px(200.));
    }

    #[test]
    fn thumb_center_tracks_fill_end_without_drift() {
        // 圆心=填充末端，由 fraction 决定、与弹簧 present 无关——任意 frac 恒成立。
        let cell = Bounds::new(point(px(10.), px(4.)), size(px(200.), px(16.)));
        let s = Slider::new("s");
        for frac in [0.0, 0.25, 0.5, 1.0] {
            let g = s.geometry(cell, frac);
            assert_eq!(
                g.fill_end,
                point(g.cell.origin.x + px(200.0 * frac), g.cell.center().y),
                "圆心贴填充末端（不漂移）frac={frac}"
            );
        }
        // 半径随弹簧 0..1 线性插值 base→hover。
        let thumb = SliderThumb {
            base: 5.,
            hover: 8.,
            color: gpui::hsla(0.5, 0.5, 0.9, 1.0),
            glow: None,
        };
        assert_eq!(thumb_radius(&thumb, 0.0), 5.0, "弹簧 0 → base");
        assert_eq!(thumb_radius(&thumb, 1.0), 8.0, "弹簧 1 → hover");
        assert_eq!(thumb_radius(&thumb, 0.5), 6.5, "弹簧 0.5 → 线性中值");
    }

    #[test]
    fn spring_converges_0_to_1_and_back() {
        // 与 motion::hover_spring 同参数的临界阻尼弹簧：0→1 与 1→0 都应收敛。
        let (mut p, mut v) = (0.0f32, 0.0f32);
        let mut steps = 0;
        for _ in 0..600 {
            let (np, nv, animating) = spring_step(p, v, 1.0, 1.0 / 60.0);
            p = np;
            v = nv;
            steps += 1;
            if !animating {
                break;
            }
        }
        assert!(
            (p - 1.0).abs() < crate::motion::HOVER_EPSILON * 2.0,
            "0→1 应收敛于 1（{}步, p={p}）",
            steps
        );
        // 复位 target=0：从已收敛的 1 回落收敛回 0。
        for _ in 0..600 {
            let (np, nv, animating) = spring_step(p, v, 0.0, 1.0 / 60.0);
            p = np;
            v = nv;
            if !animating {
                break;
            }
        }
        assert!(
            p.abs() < crate::motion::HOVER_EPSILON * 2.0,
            "1→0 应收敛于 0（p={p}）"
        );
    }

    // ---- 交互（真实元素 + 事件模拟） ----

    #[gpui::test]
    fn click_edges_map_to_zero_and_one(cx: &mut TestAppContext) {
        let (cx, changes, _, commits, _) =
            harness(cx, 0.0, true, CommitMode::Immediate, false);
        let b = cell(cx);
        // 左缘 x=left → fraction 0.0。
        let p0 = point(b.left(), b.center().y);
        cx.simulate_mouse_move(p0, None, Modifiers::default());
        cx.simulate_mouse_down(p0, MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_up(p0, MouseButton::Left, Modifiers::default());
        assert_eq!(&*changes.borrow(), &[0.0], "点击 x=0 → on_change(0.0)");
        assert!(commits.borrow().is_empty(), "Immediate 单击不下发 on_commit");

        // 右缘 x=right（含右缘，闭区间）→ fraction 1.0。
        let (cx, changes, _, commits, _) = harness(cx, 0.0, true, CommitMode::Immediate, false);
        let b = cell(cx);
        let p1 = point(b.right(), b.center().y);
        cx.simulate_mouse_move(p1, None, Modifiers::default());
        cx.simulate_mouse_down(p1, MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_up(p1, MouseButton::Left, Modifiers::default());
        assert_eq!(&*changes.borrow(), &[1.0], "点击 x=W → on_change(1.0)");
        assert!(commits.borrow().is_empty());
    }

    #[gpui::test]
    fn click_outside_box_is_ignored(cx: &mut TestAppContext) {
        let (cx, changes, drags, commits, _) =
            harness(cx, 0.0, true, CommitMode::OnRelease, false);
        let b = cell(cx);
        let cy = b.center().y;
        // 横向出盒（x 越左 / 越右）。
        let x_left = point(b.left() - px(10.), cy);
        let x_right = point(b.right() + px(10.), cy);
        // 纵向出盒（y 越上 / 越下，命中区高 16）。
        let y_top = point(b.center().x, b.top() - px(10.));
        let y_bottom = point(b.center().x, b.bottom() + px(10.));
        // 双维出盒。
        let xy_out = point(b.left() - px(20.), b.top() - px(20.));
        for p in [x_left, x_right, y_top, y_bottom, xy_out] {
            cx.simulate_mouse_move(p, None, Modifiers::default());
            cx.simulate_mouse_down(p, MouseButton::Left, Modifiers::default());
            cx.simulate_mouse_up(p, MouseButton::Left, Modifiers::default());
        }
        assert!(
            changes.borrow().is_empty(),
            "盒外横/纵/双维点击都不触发 on_change，实际={:?}",
            *changes.borrow()
        );
        assert!(drags.borrow().is_empty());
        assert!(commits.borrow().is_empty());
    }

    #[gpui::test]
    fn drag_follows_then_commits_on_release(cx: &mut TestAppContext) {
        let (cx, changes, drags, commits, _) =
            harness(cx, 0.0, true, CommitMode::OnRelease, false);
        let b = cell(cx);
        let w = b.size.width.as_f32();
        let cy = b.center().y;
        let p25 = point(b.left() + px(0.25 * w), cy);
        let p50 = point(b.left() + px(0.5 * w), cy);
        let p75 = point(b.left() + px(0.75 * w), cy);
        cx.simulate_mouse_move(p25, None, Modifiers::default());
        cx.simulate_mouse_down(p25, MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_move(p50, Some(MouseButton::Left), Modifiers::default());
        cx.simulate_mouse_move(p75, Some(MouseButton::Left), Modifiers::default());
        cx.simulate_mouse_up(p75, MouseButton::Left, Modifiers::default());
        assert_eq!(&*changes.borrow(), &[0.25], "down → on_change(0.25)");
        assert_eq!(
            &*drags.borrow(),
            &[0.5, 0.75],
            "move → on_change_during_drag×2"
        );
        assert_eq!(
            &*commits.borrow(),
            &[0.75],
            "OnRelease up → on_commit(末值=0.75)"
        );
    }

    #[gpui::test]
    fn drag_immediate_mode_never_commits_on_up(cx: &mut TestAppContext) {
        let (cx, changes, drags, commits, _) =
            harness(cx, 0.0, true, CommitMode::Immediate, false);
        let b = cell(cx);
        let w = b.size.width.as_f32();
        let cy = b.center().y;
        let p50 = point(b.left() + px(0.5 * w), cy);
        let p70 = point(b.left() + px(0.7 * w), cy);
        cx.simulate_mouse_move(p50, None, Modifiers::default());
        cx.simulate_mouse_down(p50, MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_move(p70, Some(MouseButton::Left), Modifiers::default());
        cx.simulate_mouse_up(p70, MouseButton::Left, Modifiers::default());
        assert_eq!(&*changes.borrow(), &[0.5]);
        assert_eq!(&*drags.borrow(), &[0.7]);
        assert!(
            commits.borrow().is_empty(),
            "Immediate 模式 up 不发 on_commit（值已在拖拽途中持续下发）"
        );
    }

    #[gpui::test]
    fn drag_beyond_edges_clamps_fraction(cx: &mut TestAppContext) {
        let (cx, changes, drags, commits, _) =
            harness(cx, 0.0, true, CommitMode::OnRelease, false);
        let b = cell(cx);
        let w = b.size.width.as_f32();
        let cy = b.center().y;
        let center = point(b.left() + px(0.5 * w), cy);
        let far_left = point(b.left() - px(100.), cy);
        let far_right = point(b.right() + px(100.), cy);
        cx.simulate_mouse_move(center, None, Modifiers::default());
        cx.simulate_mouse_down(center, MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_move(far_left, Some(MouseButton::Left), Modifiers::default());
        cx.simulate_mouse_move(far_right, Some(MouseButton::Left), Modifiers::default());
        cx.simulate_mouse_up(far_right, MouseButton::Left, Modifiers::default());
        assert_eq!(&*changes.borrow(), &[0.5]);
        assert_eq!(
            &*drags.borrow(),
            &[0.0, 1.0],
            "拖出条外 x<0/x>W → 值钳 0/1"
        );
        assert_eq!(
            &*commits.borrow(),
            &[1.0],
            "窗外松手 → 沿用最后锁定落点 1.0"
        );
    }

    #[gpui::test]
    fn read_only_slider_is_inert(cx: &mut TestAppContext) {
        let (cx, changes, drags, commits, _) =
            harness(cx, 0.4, false, CommitMode::OnRelease, false);
        let b = cell(cx);
        let p = point(b.center().x, b.center().y);
        cx.simulate_mouse_move(p, None, Modifiers::default());
        cx.simulate_mouse_down(p, MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_move(
            point(b.left() + px(20.), b.center().y),
            Some(MouseButton::Left),
            Modifiers::default(),
        );
        cx.simulate_mouse_up(b.center(), MouseButton::Left, Modifiers::default());
        cx.run_until_parked();
        assert!(
            changes.borrow().is_empty(),
            "interactive(false)：点击不触发 on_change"
        );
        assert!(
            drags.borrow().is_empty(),
            "interactive(false)：拖动不触发 on_change_during_drag"
        );
        assert!(
            commits.borrow().is_empty(),
            "interactive(false)：松手不触发 on_commit"
        );
    }

    #[gpui::test]
    fn hover_wires_spring_and_settles_both_ways(cx: &mut TestAppContext) {
        // 端到端：移入命中盒驱动 hover 态 + 弹簧动画帧，run_until_parked 应收敛静止
        //（弹簧 0→1 不跑飞）；移出收敛回 0。弹簧数学的确定收敛由 `spring_converges_*`
        // 纯函数测试兜底，这里证明「hover 事件确实接到元素上」。
        let (cx, _, _, _, render_count) =
            harness(cx, 0.5, true, CommitMode::OnRelease, true);
        let before = render_count.get();
        let b = cell(cx);
        let inside = point(b.center().x, b.center().y);
        let outside = point(b.right() + px(80.), b.center().y);

        cx.simulate_mouse_move(inside, None, Modifiers::default());
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.run_until_parked();
        let after_hover = render_count.get();
        assert!(
            after_hover > before,
            "移入命中盒应触发重绘（hover 接线生效），before={before} after={after_hover}"
        );

        cx.simulate_mouse_move(outside, None, Modifiers::default());
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.run_until_parked();
        assert!(
            render_count.get() > after_hover,
            "移出后应再触发重绘（弹簧回落），after_hover={after_hover} now={}",
            render_count.get()
        );
    }
}
