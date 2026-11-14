//! 输入 / 命中（input/hit）子模块：滚轮缩放与同文件夹切图、放大态滚轮平移、左键拖拽平移、
//! 切图复位（缩放→适应视口 / 平移归零）与「展示像素需要」刷新入口。
//!
//! 几何逻辑收敛于 [`::viewer::pan_zoom`]（image/video 共用）；缩放 / 切图后沿解码流水线
//! （[`super`] 与 [`super::decode`]）触发升档 / 预取 / 切图落盘。私有成员经 `super::` 可见。

use gpui::{
    Context, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ScrollDelta,
    ScrollWheelEvent, Window,
};
use config::image::{FAST_SCROLL_STEP_MS, FAST_SCROLL_THRESHOLD, ZOOM_MAX, ZOOM_MIN, ZOOM_STEP};
use ::viewer::pan_zoom;

/// 放大态滚轮平移每格的视口比例（Lines 滚轮：一下滚轮平移多少视口高；直接切图的
/// 未放大态此值无意义）。
const WHEEL_LINE_PAN_FRACTION: f32 = 0.15;
/// 像素滚轮：每累计一个 [`WHEEL_PIXEL_STEP`] 平移的像素 = 步长 × 本倍率（放大态平移步进）。
const WHEEL_PIXEL_PAN_MULT: f32 = 4.0;

impl super::ImageViewer {

    /// 滚轮处理：Ctrl+滚轮 → 缩放；单滚轮 → 同文件夹切换。挂在内层 wrap
    /// （`#image-scroll` 子树上），bubble 期先于滚动容器原生滚动，`stop_propagation`
    /// 阻止其滚动（对齐 PDF 阅读器的同款姿势）。
    pub(super) fn on_scroll_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let delta_y = pan_zoom::wheel_delta_y(event);
        if delta_y == 0.0 {
            return;
        }
        if event.modifiers.control {
            // 实测本平台：向上滚动 delta.y>0 → 放大；向下 <0 → 缩小（与编辑器字号缩放一致）
            self.adjust_zoom(pan_zoom::zoom_factor(delta_y, ZOOM_STEP), cx);
        } else {
            // 单滚轮：未放大 → 切图（向下滚 → 下一张，向上滚 → 上一张）；放大态 →
            // 先在图片内平移，滚到边缘继续同向外滚才切图（[`Self::wheel_pan_or_switch`]）。
            match event.delta {
                // 行滚轮（鼠标）：1 事件 = 1 格（放大态平移一格 = 视口高的一部分）。
                ScrollDelta::Lines(_) => {
                    let (_, vh) = self.last_viewport;
                    let pan_step = (vh * WHEEL_LINE_PAN_FRACTION).max(32.0);
                    self.wheel_pan_or_switch(if delta_y < 0.0 { 1 } else { -1 }, pan_step, cx);
                }
                // 像素滚轮（触控板）：累计像素，超过一步门限才动作（未放大切图时合并
                // 洪水事件；放大态按步进平移，可连续跟随）。
                ScrollDelta::Pixels(_) => {
                    self.scroll_accum += delta_y;
                    let step = config::image::WHEEL_PIXEL_STEP;
                    let pan_step = step * WHEEL_PIXEL_PAN_MULT;
                    while self.scroll_accum >= step {
                        self.wheel_pan_or_switch(1, pan_step, cx);
                        self.scroll_accum -= step;
                    }
                    while self.scroll_accum <= -step {
                        self.wheel_pan_or_switch(-1, pan_step, cx);
                        self.scroll_accum += step;
                    }
                }
            }
        }
        cx.stop_propagation();
    }

    /// 滚轮分发（未放大 ↔ 放大态切换语义）：
    /// - **未放大**（图两轴都 ≤ 视口）→ 保持原有「滚轮切图」：`dir=1`（向下滚）切下一张、
    ///   `dir=-1` 切上一张；
    /// - **放大态** → 优先**纵向平移**（`dir=1` = 看图片下方，`pan.y -= pan_step`），滚到该轴
    ///   边缘且**继续同向外滚**（越界量）才触发切图——即「滚到底部才切下一张」；纵向不越界
    ///   时才启用横向平移。切图由 [`Self::switch`] 统一复位（缩放→适应视口、平移/拖拽归零）。
    pub(super) fn wheel_pan_or_switch(&mut self, dir: i32, pan_step: f32, cx: &mut Context<Self>) {
        let (vw, vh) = self.last_viewport;
        if vw <= 0.0 || vh <= 0.0 {
            // 视口未知（未布局）：退回切图语义
            self.switch(dir, cx);
            return;
        }
        let (dw, dh) = self
            .intrinsic
            .map(|(iw, ih)| self.display_size(iw, ih, vw, vh))
            .unwrap_or((0.0, 0.0));
        let ay = (dh - vh).max(0.0) * 0.5;
        let ax = (dw - vw).max(0.0) * 0.5;
        if ay <= 0.0 && ax <= 0.0 {
            self.switch(dir, cx);
            return;
        }
        let outward = if ay > 0.0 { &mut self.pan.1 } else { &mut self.pan.0 };
        let limit = if ay > 0.0 { ay } else { ax };
        let inc = if dir > 0 { -pan_step } else { pan_step };
        let next = *outward + inc;
        let clamped = next.clamp(-limit, limit);
        *outward = clamped;
        cx.notify();
        let past_edge = if dir > 0 {
            clamped <= -limit && next < -limit
        } else {
            clamped >= limit && next > limit
        };
        if past_edge {
            self.switch(dir, cx);
        }
    }

    /// 应用缩放因子并约束在 [ZOOM_MIN, ZOOM_MAX]（钳位/无变化短路由 [`pan_zoom::apply_zoom`]）。
    /// 缩放会改变「展示像素需要」（放大需更细档），故刷新 [`Self::display_need`] 并在
    /// 「当前展示比需要更粗」时升档重解。
    pub(super) fn adjust_zoom(&mut self, factor: f32, cx: &mut Context<Self>) {
        if !pan_zoom::apply_zoom(&mut self.zoom, factor, ZOOM_MIN, ZOOM_MAX) {
            return;
        }
        self.update_display_need();
        cx.notify();
        self.escalate_display_denom(cx);
    }

    /// 在同文件夹清单内切换（`dir` = ±1，环绕语义收敛于 [`FolderNav::step`]）。切换本身
    /// O(1)：目标立即写入 `path`（标题/状态栏跟着走），显示由 `request_display` 决定「缓存
    /// 命中秒切 / 未中池解码追最新」。方向变化时重置级联预取锚点；同时做 B4「快速连续
    /// 滚动」判定（连续同向且步间隔 ≤ [`FAST_SCROLL_STEP_MS`] 累计到阈值 → 临时放宽前向窗口）。
    pub(super) fn switch(&mut self, dir: i32, cx: &mut Context<Self>) {
        let Some(next) = self.nav.step(i64::from(dir)) else {
            return;
        };
        if next == self.nav.current() {
            return;
        }
        if dir != self.prefetch_dir {
            self.prefetch_dir = dir;
        }
        // B4：连续同向 + 快速步间隔 → 快滚计数；慢速/换向衰减（迟滞进出，避免抖）
        let now = std::time::Instant::now();
        let rapid = self.last_scroll_dir == dir
            && self
                .last_scroll_at
                .is_some_and(|t| now.duration_since(t).as_millis() as u64 <= FAST_SCROLL_STEP_MS);
        self.last_scroll_dir = dir;
        self.last_scroll_at = Some(now);
        if rapid {
            self.fast_count = self.fast_count.saturating_add(1);
        } else {
            // 慢速/换向：计数折半（自然衰减）；归零才退出快速模式（迟滞）
            self.fast_count /= 2;
        }
        // 进入阈值 FAST_SCROLL_THRESHOLD；进入后只要计数 > 0 保持（迟滞，避免抖进抖出）
        self.scroll_fast =
            self.fast_count >= FAST_SCROLL_THRESHOLD || (self.scroll_fast && self.fast_count > 0);
        let _ = self.nav.set_current(next);
        self.path = self.nav.item(next).expect("switch 目标恒在清单内");
        // 诊断探针（env 门控，默认关）：记录切图时刻 t0（decode 分账 + 滚动期判定起点）
        super::diag::note_switch(&self.path);
        // 切图：缩放复位为适应视口 + 平移/拖拽归零（每张都从初始大小开始）
        self.zoom = 1.0;
        self.pan = (0.0, 0.0);
        self.drag = None;
        self.request_display(cx);
        // 锁定模式：切图 → 底部信息条浮现 1s
        self.show_locked_info(cx);
    }

    /// 显示入口（首图 / 切换共用）：`path` 已指向目标。缓存命中 → 瞬时落盘并启动级联
    /// 预取；未中 → 置 loading 并进入「并行解码池」（去重 / 追最新）。命中后按当前
    /// 展示需要判断是否需升档（缓存的可能是旧档位）。
    ///
    /// R4A 重绘收口：**未就绪目标不置空显示、不整窗重绘**——保持上一张已安装图像，
    /// 仅后台 `queue_decode`；换图只由该目标 `install_current` 落盘后 notify 触发
    /// （缓存命中路径维持现状：同步 install + notify，立即切换）。
    pub(super) fn request_display(&mut self, cx: &mut Context<Self>) {
        let path = self.path.clone();
        if let Some(entry) = self.cache_get(&path) {
            self.install_current(entry, &path, cx);
            self.escalate_display_denom(cx);
            return;
        }
        // 未就绪：不改 `state.data`（显示保持上一张已安装图），也不在此 notify /
        // refresh——重绘只由落盘（install_current）或缓存命中（上文）触发。
        self.state.set_loading();
        // P1b「即时首帧」只在**首开/无上一张**时派 EXIF 缩略图占位（冷打开首帧不空等
        // 熵解码）；已有显示图像的切换不再临时替换——否则快滚到未就绪 JPEG 会闪出模糊
        // 缩略图、且占位落盘还会多点一次整窗重绘（R4A 消除闪烁）。
        if self.state.data.is_none() {
            self.maybe_show_exif_thumb(&path, cx);
        }
        self.queue_decode(path, true, cx);
    }

    // ---- 放大后拖拽平移（鼠标左键抓图滑动浏览其他区域） ----

    /// 按下左键开始拖拽：记录起点鼠标位置 + 当前 `pan`（未放大时 clamp 恒 0，拖动无效果）。
    pub(super) fn on_pan_down(
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

    /// 拖拽平移：按「当前鼠标 - 起点」增量更新 `pan`，clamp 到图始终覆盖视口。
    pub(super) fn on_pan_move(
        &mut self,
        ev: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(drag) = self.drag else {
            return;
        };
        let pos = (ev.position.x.as_f32(), ev.position.y.as_f32());
        let (dx, dy) = pan_zoom::pan_drag_delta(&drag, pos);
        let (vw, vh) = self.last_viewport;
        let (dw, dh) = self
            .intrinsic
            .map(|(iw, ih)| self.display_size(iw, ih, vw, vh))
            .unwrap_or((0.0, 0.0));
        self.pan = Self::clamp_pan((drag.1.0 + dx, drag.1.1 + dy), vw, vh, dw, dh);
        cx.notify();
    }

    /// 拖拽结束（窗口内/外松开统一清态；`on_mouse_up` / `on_mouse_up_out` 共用）。
    pub(super) fn on_pan_up(&mut self, _ev: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if self.drag.is_some() {
            self.drag = None;
            cx.notify();
        }
    }

    /// 平移 clamp：放大后（`dw>vw` 或 `dh>vh`）允许在图内滑动，范围 = ±(图-视口)/2；
    /// 未放大时范围 0（永远居中，拖拽无效果）。以「视口中心 = 图中心」为 0 基准，
    /// 缩放/切图后范围变化由 render 期逐帧重新 clamp 收敛。
    /// 逻辑收敛在 [`pan_zoom::clamp_pan`]（image/video 共用）。
    pub(super) fn clamp_pan(pan: (f32, f32), vw: f32, vh: f32, dw: f32, dh: f32) -> (f32, f32) {
        pan_zoom::clamp_pan(pan, vw, vh, dw, dh)
    }
}
