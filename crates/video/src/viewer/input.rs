use super::*;
impl super::VideoViewer {


    pub(super) fn on_key_down(&mut self, e: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        match e.keystroke.key.as_str() {
            "space" => self.toggle_play(cx),
            "left" => self.seek(-SEEK_STEP_SEC, cx),
            "right" => self.seek(SEEK_STEP_SEC, cx),
            "pagedown" => self.switch_file(1, cx),
            "pageup" => self.switch_file(-1, cx),
            "m" => self.toggle_mute(cx),
            "-" => self.set_volume(self.volume - VOLUME_STEP, cx),
            "=" => self.set_volume(self.volume + VOLUME_STEP, cx),
            "[" => self.speed_step(-1, cx),
            "]" => self.speed_step(1, cx),
            "s" => self.screenshot_png(cx),
            "v" => self.toggle_subtitle(cx),
            _ => return, // 其余键不拦截
        }
        cx.stop_propagation();
    }


    /// 滚轮处理（视频画布自己吃掉，不参与进度）：`Ctrl+滚轮` → 缩放；`Shift+滚轮` →
    /// 同文件夹切换；普通滚轮**不做任何事**（不再控制进度）。`stop_propagation` 保持
    /// 「视频占满整区、滚轮不滚动任何东西」的既有语义。
    pub(super) fn on_scroll_wheel(
        &mut self,
        ev: &ScrollWheelEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let delta_y = pan_zoom::wheel_delta_y(ev);
        self.handle_wheel(delta_y, ev.modifiers.control, ev.modifiers.shift, cx);
        cx.stop_propagation();
    }


    /// 滚轮分发（可单测）：`ctrl` → 缩放；`shift` → 同文件夹切换；否则空操作。
    pub(super) fn handle_wheel(
        &mut self,
        delta_y: f32,
        ctrl: bool,
        shift: bool,
        cx: &mut Context<Self>,
    ) {
        if delta_y == 0.0 {
            return;
        }
        if ctrl {
            // Ctrl+滚轮：放大/缩小（向上滚 = 放大，向下 = 缩小；与图片/PDF 阅读器一致）
            self.adjust_zoom(pan_zoom::zoom_factor(delta_y, ZOOM_STEP), cx);
        } else if shift {
            // Shift+滚轮：同文件夹切换（向下滚 = 下一支）
            self.switch_file(if delta_y < 0.0 { 1 } else { -1 }, cx);
        }
    }


    /// 左键按下开始拖拽：记录起点鼠标位置 + 当前 `pan`（未放大时 clamp 恒 0，拖动无效果）。
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


    /// 拖拽平移：按「当前鼠标 − 起点」增量更新 `pan`，clamp 到图始终覆盖画布区。
    pub(super) fn on_pan_move(
        &mut self,
        ev: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(drag) = self.drag else {
            return;
        };
        let (vw, vh) = self.viewport_size(window);
        let area_h = if self.locked {
            vh.max(1.0)
        } else {
            (vh - CONTROL_BAR_H).max(1.0)
        };
        let (dw, dh) = self.display_size(vw, vh);
        let pos = (ev.position.x.as_f32(), ev.position.y.as_f32());
        let (dx, dy) = pan_zoom::pan_drag_delta(&drag, pos);
        self.pan = Self::clamp_pan((drag.1.0 + dx, drag.1.1 + dy), vw, area_h, dw, dh);
        cx.notify();
    }


    /// 拖拽结束（窗口内/外松开统一清态；`on_mouse_up` / `on_mouse_up_out` 共用）。
    pub(super) fn on_pan_up(&mut self, _ev: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if self.drag.is_some() {
            self.drag = None;
            cx.notify();
        }
    }


    // ---- 纯视频模式（参考纯图片锁定）：隐藏外壳、视频充满窗口、控制条为浮层 ----

    /// 是否「纯视频」干净播放模式（宿主据此隐藏菜单栏/选项卡栏/侧边栏/状态栏，视频充满窗口）。
    pub fn is_locked(&self) -> bool {
        self.locked
    }


    /// 切换纯视频模式：锁定 = 隐藏非视频 UI、视频充满窗口（宿主壳体重排后自动缩放填满）、
    /// 控制条浮层按需浮现；解锁回归。切换时平移/拖拽归零。
    pub fn toggle_lock(&mut self, cx: &mut Context<Self>) {
        let locked = !self.locked;
        self.set_locked(locked, cx);
    }


    /// 直接设置纯视频（锁定）模式（`toggle_lock` 翻转与双击全屏共用同一副作用，避免平行
    /// 重写）：锁定 = 隐藏非视频 UI、视频充满窗口；解锁回归。切换时平移/拖拽归零。
    pub(super) fn set_locked(&mut self, locked: bool, cx: &mut Context<Self>) {
        self.locked = locked;
        self.pan = (0.0, 0.0);
        self.drag = None;
        // 进/出锁定都让控制条浮现一次（锁定态随即 1s 自隐）
        self.show_controls_temporarily(cx);
        cx.notify();
    }


    /// 双击画面区是否应**退出**全屏（纯函数，不读窗口，测试可注入 `is_fs` 确定性断言退出
    /// 路径）：当前已全屏 → 退出；未全屏 → 进入（进入后自动进纯视频锁定，再双击退出并恢复
    /// 进入前的模式）。
    pub(super) fn should_exit_fullscreen(is_fs: bool) -> bool {
        is_fs
    }


    /// 视频画面区（`#video-main`）双击：进入 OS 窗口全屏 + 纯视频锁定；已全屏时再双击退出
    /// 并恢复进入前的锁定模式（进入前已锁定则保持锁定）。控制条/进度条/状态浮层是画面上层
    /// 覆盖子元素、优先消费其命中的双击，不会到达这里；纯事件驱动，不引入定时器/每帧工作。
    pub(super) fn on_video_surface_mouse_down(
        &mut self,
        ev: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 双击语义与欢迎页/编辑区同套（click_count >= 2；左键由 on_mouse_down(Left) 保证，
        // 单击 = click_count 1 直接 return，不抢单击/拖拽语义）
        if ev.click_count < 2 {
            return;
        }
        if Self::should_exit_fullscreen(window.is_fullscreen()) {
            // 退出全屏：恢复进入全屏前的模式（全屏期间手动切锁定会按进入前快照恢复——可接受微偏）
            window.toggle_fullscreen();
            let restore = self.pre_fullscreen_locked;
            self.set_locked(restore, cx);
        } else {
            // 进入全屏：先记录进入前锁态，再进全屏并锁定（set_locked 内已归零平移/浮现控制条）
            self.pre_fullscreen_locked = self.locked;
            window.toggle_fullscreen();
            self.set_locked(true, cx);
        }
    }


    // ---- 控制条浮现 / 自隐（锁定模式浮层；普通模式常驻） ----

    /// 控制条浮现并排队「离开 1s 后自隐」（切视频 / hover 离开时调用；悬停中不隐藏）。
    /// 时序逻辑收敛于 [`auto_hide::AutoHide::show_temporarily`]。
    pub(super) fn show_controls_temporarily(&mut self, cx: &mut Context<Self>) {
        self.controls_auto
            .show_temporarily(|_| true, |v| &mut v.controls_auto, cx);
    }


    /// 查看器悬停态：`#video-viewer` 根 hover。进入 → 浮现且不隐藏；离开 → 1s 后自隐。
    pub(super) fn on_controls_hover(&mut self, hovered: &bool, _window: &mut Window, cx: &mut Context<Self>) {
        self.set_controls_hover(*hovered, cx);
    }


    /// 悬停态设置（独立于窗口，便于测试）：悬停期间不受自动隐藏影响。
    pub(super) fn set_controls_hover(&mut self, hovered: bool, cx: &mut Context<Self>) {
        if hovered {
            self.controls_auto.hover_in();
            cx.notify();
        } else {
            // F1：鼠标离开控制条整条 → 悬停时间浮层一并隐藏（条内移开由 on_controls_move 清）
            self.hover_x = None;
            self.controls_auto.hover_out();
            self.show_controls_temporarily(cx);
        }
    }


    /// 控制条 / 状态浮层命中**双击** → 终止向 `#video-main` 冒泡（与画面双击全屏互斥）：
    /// seek/音量条等子元素先消费各自的 mousedown，轮到本处理器时只吞掉 `click_count>=2`，
    /// 使双击进度条/按钮/空隙不再误触进入/退出全屏。单击透传不拦截——验收口径「单击/
    /// 拖拽行为全不变」（拖拽起点仍按既有方式落到画面元素）。
    pub(super) fn on_controls_mouse_down(
        &mut self,
        ev: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if ev.click_count >= 2 {
            cx.stop_propagation();
        }
    }


    /// seek 点按：进入拖拽预览（begin_scrub）——先落点预览、静音；松手提交。
    pub(super) fn on_seek_down(
        &mut self,
        e: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(f) = fraction_at(&self.seek_bounds, e.position) {
            self.begin_scrub(f, cx);
        }
    }


    /// seek 拖拽（左键按住才推进；移出条到窗外松开由 `on_seek_up`/`on_mouse_up_out` 兜底）。
    pub(super) fn on_seek_move(
        &mut self,
        e: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !e.dragging() {
            // 已在拖拽但这一拍 dragging 变假（松开）而 mouse_up 未落在条上 → 按当前落点收尾
            if self.scrubbing {
                self.end_scrub_at_current(cx);
                return;
            }
            // M0：未拖拽的悬停 → 记录悬停位置并（空闲时）后台预解码该落点帧，点击即现。
            // F1：同时记下光标的窗口坐标 x，悬停时间浮层紧随光标横向移动。
            if let Some(f) = fraction_at(&self.seek_bounds, e.position) {
                self.hover_x = Some(e.position.x.as_f32());
                self.update_hover_target(self.clamped_scrub_target(f), cx);
                cx.notify(); // 浮层文本/横坐标随光标实时刷新
            }
            return;
        }
        if let Some(f) = fraction_at(&self.seek_bounds, e.position) {
            if self.scrubbing {
                self.update_scrub(f, cx);
            } else {
                self.begin_scrub(f, cx);
            }
        }
        self.hover_x = None; // 开始拖拽 → 悬停浮层隐藏（拖拽时当前时间已跟手指）
        cx.notify();
    }


    /// F1：光标是否落在 seek 进度条**盒内**（窗口坐标，含纵向）。`fraction_at` 只做 x→进度
    /// 映射、对条外点也返回钳位值，不能当「在条上」判据。盒由 canvas prepaint 记录：canvas
    /// 自身布局高度为 0（条高由外层 `.h(px(16.))` 撑起、同 `strip_element`），故纵向用
    /// 「盒顶 + 条高」重建。在 → 返回该点对应秒（钳位 [0, duration]）；不在/未布局 → None。
    pub(super) fn seek_hover_target_at(&self, point: Point<Pixels>) -> Option<f64> {
        let b = self.seek_bounds.get();
        if b.size.width.as_f32() <= 0.0 {
            return None;
        }
        let x = point.x.as_f32();
        let y = point.y.as_f32();
        let left = b.origin.x.as_f32();
        let right = (b.origin.x + b.size.width).as_f32();
        let top = b.origin.y.as_f32();
        let bar_h = if b.size.height.as_f32() > 0.0 {
            b.size.height.as_f32()
        } else {
            16.0
        };
        if x < left || x > right || y < top || y > top + bar_h {
            return None;
        }
        let f = ((x - left) / (right - left)).clamp(0.0, 1.0) as f32;
        Some(self.clamped_scrub_target(f))
    }


    /// 控制条内任意指针移动：光标离开 seek 进度条（移到按钮/空隙/音量条等）→ 清除悬停
    /// 浮层（x 不在盒内即隐藏；回到条上由 seek 元素自己的 `on_seek_move` 重新记录 → 复现）。
    pub(super) fn on_controls_move(
        &mut self,
        e: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.seek_hover_target_at(e.position).is_none() && self.hover_x.is_some() {
            self.hover_x = None;
            cx.notify();
        }
    }


    /// seek 拖拽结束（窗口内松开）：按松手位置最终提交。
    pub(super) fn on_seek_up(
        &mut self,
        e: &MouseUpEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.scrubbing {
            return;
        }
        if let Some(f) = fraction_at(&self.seek_bounds, e.position) {
            self.end_scrub(f, cx);
            // F1：松手仍在进度条盒内（含纵向，`fraction_at` 只比 x）→ 恢复正常 hover：
            // 同步把 hovered_target 刷新到松手落点（否则浮层文本残留拖拽前悬停的旧秒数，
            // 出现「文本与横坐标错位」）；拖到条外（纵向出去）松开 → 保持隐藏不残影。
            if let Some(target) = self.seek_hover_target_at(e.position) {
                self.hovered_target = Some(target);
                self.hover_x = Some(e.position.x.as_f32());
            } else {
                self.hover_x = None;
            }
            cx.notify();
        } else {
            self.end_scrub_at_current(cx);
        }
    }


    /// 音量条点按：屏 x → 分数 → 音量。
    pub(super) fn on_vol_down(
        &mut self,
        e: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(f) = fraction_at(&self.vol_bounds, e.position) {
            self.set_volume(f, cx);
        }
    }


    /// 音量条拖拽。
    pub(super) fn on_vol_move(
        &mut self,
        e: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !e.dragging() {
            return;
        }
        if let Some(f) = fraction_at(&self.vol_bounds, e.position) {
            self.set_volume(f, cx);
        }
    }
}
