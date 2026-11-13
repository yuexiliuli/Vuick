use super::*;
impl super::VideoViewer {


    /// 暂停标志是否该显示（右上角玻璃徽标）：**有画面且未在播放**（暂停/自然结尾都算停）；
    /// 加载中（无帧）与打开出错（占位）时不显示。纯函数，测试可注入三态确定性断言。
    pub(super) fn paused_badge_shown(playing: bool, has_frame: bool, load_error: bool) -> bool {
        !playing && has_frame && !load_error
    }


    /// 居中占位玻璃卡（加载中 / 加载失败共用）：铺满画布再 flex 居中，信息收敛在
    /// 半透明圆角卡内（与 toast / 控制条同一套玻璃样式）。
    pub(super) fn placeholder(text: &str, color: Hsla, cx: &App) -> gpui::AnyElement {
        let theme = ui::theme::Theme::global(cx);
        let dark = theme.is_dark();
        let surface: Hsla = theme.colors.surface.into();
        let border: Hsla = theme.colors.border.into();
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .px_4()
                    .py_2()
                    .rounded_lg()
                    .bg(surface.opacity(if dark {
                        GLASS_PANEL_ALPHA_DARK
                    } else {
                        GLASS_PANEL_ALPHA_LIGHT
                    }))
                    .border_1()
                    .border_color(border)
                    // 玻璃卡投影 = 黑 hsla 散值（明暗同值、按高程定 alpha/blur；ui::theme 无阴影 token，表单专属留注释）
                    .shadow(vec![
                        gpui::BoxShadow::new(px(0.), px(4.), gpui::hsla(0., 0., 0., 0.16))
                            .blur_radius(px(12.)),
                        gpui::BoxShadow::new(px(0.), px(1.), gpui::hsla(0., 0., 0., 0.12))
                            .blur_radius(px(3.)),
                    ])
                    .text_size(px(14.))
                    .text_color(color)
                    .child(text.to_string()),
            )
            .into_any_element()
    }


    /// 阅读画布底色（同 ImageViewer：亮 #F5F5F5 / 暗 #202020，中性灰阶、与外壳 `window_bg` 同源）。
    pub(super) fn default_canvas(cx: &App) -> Hsla {
        let dark = ui::theme::Theme::global(cx).is_dark();
        ui::theme::ChromeColors::for_mode(dark).window_bg.into()
    }


    /// 玻璃面板底色：theme surface × 模式半透明 alpha（白主题=亮玻璃、暗主题=深玻璃，
    /// 与 toast/placeholder/hover-time 同一套取色）。纯函数，测试注入明/暗各一面断言
    /// （回归：曾硬编码 #191a1b 致白主题下控制条为黑条）。
    pub(super) fn glass_panel_bg(surface: Hsla, dark: bool) -> Hsla {
        surface.opacity(if dark {
            GLASS_PANEL_ALPHA_DARK
        } else {
            GLASS_PANEL_ALPHA_LIGHT
        })
    }


    /// F1：进度条悬停时间浮层（纯视觉，**无任何事件监听** → 不拦 seek 条点击/拖拽/dblclick，
    /// 命中穿透；也不占布局——deferred 独立绘制层，随 `.position(窗口坐标)` 悬浮）。
    /// 纵向固定浮在进度条上方 [`HOVER_TIME_GAP_PX`]，横向跟随光标；`deferred(anchored(Window))`
    /// 定位用 `.position(窗口坐标)`（坑速查：不用 `.offset`），`snap_to_window_with_margin`
    /// 自动把左右边缘 Clamp 在窗口内（0 秒/片尾悬停被截断的风险由它消掉）。配色全走
    /// `ui::theme`（同 toast/暂停徽标的 surface/border/fg 取色姿势，明暗主题都可读），
    /// 出现/离开瞬间短弹簧淡入淡出，收敛即静止（无常驻重绘）。测试探针：`debug_selector`
    /// 静态 `HOVER_TIME` → `debug_bounds("HOVER_TIME")` 断言「元素存在 + 定位」；文本由
    /// `fmt_time(hovered_target)` 纯函数链断言（render 唯一文本来源）。
    pub(super) fn hover_time_float(
        secs: f64,
        cursor_x: f32,
        seek_bounds: &Rc<Cell<Bounds<Pixels>>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let b = seek_bounds.get();
        if b.size.width.as_f32() <= 0.0 {
            return div().into_any_element();
        }
        let theme = ui::theme::Theme::global(cx);
        let surface: Hsla = theme.colors.surface.into();
        let border: Hsla = theme.colors.border.into();
        let fg: Hsla = theme.colors.fg.into();
        let is_dark = theme.is_dark();
        let label = Self::fmt_time(secs);
        // 纵向固定：贴进度条上缘上移 `HOVER_TIME_GAP_PX`（BottomCenter 锚点 = 卡片底边中点）。
        let top = (b.origin.y - px(HOVER_TIME_GAP_PX)).as_f32().max(0.0);
        let card = div()
            .id("video-hover-time")
            .debug_selector(move || "HOVER_TIME".to_string())
            .whitespace_nowrap()
            .px_2()
            .py_1()
            .rounded_md()
            .border_1()
            .border_color(border)
            .bg(surface.opacity(if is_dark {
                GLASS_PANEL_ALPHA_DARK
            } else {
                GLASS_PANEL_ALPHA_LIGHT
            }))
            // 投影同 placeholder 玻璃卡：黑 hsla 0.16@6，表单专属（theme 无阴影 token）
            .shadow(vec![
                gpui::BoxShadow::new(px(0.), px(2.), gpui::hsla(0., 0., 0., 0.16))
                    .blur_radius(px(6.)),
            ])
            .text_size(px(11.))
            .text_color(fg)
            .child(label)
            .with_spring(
                "video-hover-time-fade",
                hover_spring(1.0).from(0.0),
                |this, v| this.opacity(v),
            );
        deferred(
            anchored()
                .anchor(Anchor::BottomCenter)
                .position(point(px(cursor_x), px(top)))
                .snap_to_window_with_margin(Edges::all(px(6.)))
                .child(card),
        )
        .with_priority(HOVER_TIME_LAYER_PRIORITY)
        .into_any_element()
    }


    /// 底部全屏控制条：上一支 / 播放·暂停 / 下一支 / 时间 / seek 进度条 /
    /// 静音 + 音量条 / 解码偏好切换 / 播放模式 / 倍速循环 / 清单位置。
    ///
    /// 普通模式**常驻**显示。纯视频（锁定）模式为浮层：恒挂一根底部热区（`#video-controls`），
    /// `controls_auto.visible()` 时渲染整条，隐藏时是透明热区——hover **只挂在这根条带**上（进入即
    /// 浮现且不隐藏，离开 1s 后自隐；鼠标在条带之外的画面区域不触发，锁定的初始自隐不会被
    /// 画面内停留的鼠标打断）。
    pub(super) fn control_bar(&mut self, window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let strip = div()
            .id("video-controls")
            .absolute()
            .bottom_0()
            .left_0()
            .right_0()
            .h(px(CONTROL_BAR_H))
            .on_hover(cx.listener(Self::on_controls_hover));
        if self.locked && !self.controls_auto.visible() {
            // 纯视频模式隐藏：透明热区（继续占底部、可接收 hover 触发浮现）
            return strip.into_any_element();
        }
        // 可见控制条：双击不让其漏到画面区（与 `#video-main` 双击全屏互斥）；须挂在
        // `if 隐藏早退` 之后——隐藏态透明热区下方就是视频画面，双击仍需命中画面元素。
        // on_mouse_move：F1 悬停浮层「移出 seek 条即隐藏」的清理点（条内任意移首都冒泡到这）。
        let strip = strip
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_controls_mouse_down))
            .on_mouse_move(cx.listener(Self::on_controls_move));
        let colors = ui::theme::Theme::global(cx).colors;
        let dark = ui::theme::Theme::global(cx).is_dark();
        let fg: Hsla = colors.fg.into();
        let muted: Hsla = colors.fg_muted.into();
        let border: Hsla = colors.border.into();
        let accent: Hsla = colors.accent.into();
        let entity = cx.entity();

        let seek_strip = Self::strip_element(
            "video-seek",
            self.seek_bounds.clone(),
            self.ratio(),
            muted,
            accent,
            true,
            window,
            cx,
        );
        let vol_strip = Self::strip_element(
            "video-vol",
            self.vol_bounds.clone(),
            self.effective_volume(),
            muted,
            accent,
            false,
            window,
            cx,
        );

        let time = format!(
            "{} / {}",
            Self::fmt_time(self.playhead),
            Self::fmt_time(self.duration)
        );
        let playlist = format!(
            "{}/{} · {:.0}%",
            self.nav.current() + 1,
            self.nav.len(),
            self.zoom * 100.0
        );

        let play_icon = if self.playing {
            IconName::Pause
        } else {
            IconName::Play
        };
        let vol_icon = if self.muted || self.effective_volume() <= 0.001 {
            IconName::VolumeX
        } else {
            IconName::Volume
        };

        // 玻璃拟态：半透明面板（透出视频）+ 顶部直角 + 细边框 +
        // 上抛柔和投影（矩阵浮层风格）。面板底色取 theme surface（明暗各自取色，与
        // toast/placeholder/hover-time 同一套玻璃样式），再乘模式半透明 alpha——白主题
        // 是亮玻璃、不再是黑条（回归 `control_bar_panel_follows_theme`）。纯视觉。
        let panel = Self::glass_panel_bg(colors.surface.into(), dark);
        strip
            .bg(panel)
            .border_1()
            .border_color(border)
            // 控制条上抛投影 = 黑 hsla 双散值（0.18@16 + 0.14@4），面板专属，theme 无阴影 token
            .shadow(vec![
                gpui::BoxShadow::new(px(0.), px(-6.), gpui::hsla(0., 0., 0., 0.18))
                    .blur_radius(px(16.)),
                gpui::BoxShadow::new(px(0.), px(-2.), gpui::hsla(0., 0., 0., 0.14))
                    .spread_radius(px(-1.))
                    .blur_radius(px(4.)),
            ])
            .flex()
            .items_center()
            .gap_x(px(8.))
            .px_3()
            .child(control_icon_button(
                "video-prev",
                IconName::SkipBack,
                fg,
                entity.clone(),
                |v, cx| v.switch_file(-1, cx),
            ))
            .child(control_icon_button(
                "video-play",
                play_icon,
                accent,
                entity.clone(),
                |v, cx| v.toggle_play(cx),
            ))
            .child(control_icon_button(
                "video-next",
                IconName::SkipForward,
                fg,
                entity.clone(),
                |v, cx| v.switch_file(1, cx),
            ))
            .child(
                div()
                    .whitespace_nowrap()
                    .text_size(px(11.))
                    .text_color(muted)
                    .child(time),
            )
            .child(seek_strip)
            .child(control_icon_button(
                "video-mute",
                vol_icon,
                fg,
                entity.clone(),
                |v, cx| v.toggle_mute(cx),
            ))
            .child(vol_strip)
            .child({
                // 解码偏好（自动硬解优先 ↔ 强制软解）：图标随状态换（芯片=自动/硬解优先、
                // 滑杆=强制软解）+ 高亮区分（自动=accent、软解=fg）+ 悬停浮层说明当前状态。
                // 点击循环并重开当前 reader @当前播放位置（见 `cycle_decode_pref`）。
                // 悬停/tooltip 挂在包一层相对定位 div 上——Button 自带 hover 处理器（再用
                // `.on_hover` 会撞 gpui「同元素重复挂 on_hover」断言）。
                let entity = entity.clone();
                let tip = ui::tooltip::tooltip_state("video-decode", window, cx);
                let tip_on = ui::tooltip::tooltip_visible(&tip, cx);
                let view_id = window.current_view();
                let (icon, tint, title) = match self.decode_pref {
                    1 => (
                        IconName::DecodeSoft,
                        fg,
                        "强制软解：始终用 CPU 软件解码，时序恒定",
                    ),
                    _ => (
                        IconName::DecodeAuto,
                        accent,
                        "自动（硬解优先）：GPU 硬解，失败自动回软解",
                    ),
                };
                let btn = Button::new("video-decode")
                    .ghost()
                    .rounded_full()
                    .w(px(28.))
                    .h(px(28.))
                    .child(Icon::new(icon).size(px(16.)).text_color(tint))
                    .on_click(move |_, _w, app| {
                        entity.update(app, |v, cx| v.cycle_decode_pref(cx));
                    });
                div()
                    .id("video-decode-host")
                    .relative()
                    .w(px(28.))
                    .h(px(28.))
                    .on_hover(move |hovered, _w, cx| {
                        ui::tooltip::tooltip_hover(&tip, *hovered, cx);
                        cx.notify(view_id);
                    })
                    .child(btn)
                    .child(ui::tooltip::tooltip_layer(
                        "video-decode",
                        title,
                        tip_on,
                        28.0,
                        28.0,
                        cx,
                    ))
            })
            .child({
                // 模式按钮在 speed/clone 之外单独 clone：后文 camera 按钮仍需用 entity
                let entity = entity.clone();
                Button::new("video-mode")
                    .ghost()
                    .rounded_full()
                    .label(self.playback_mode.label())
                    .w(px(44.))
                    .h(px(28.))
                    .text_size(px(11.))
                    .on_click(move |_, _w, app| {
                        entity.update(app, |v, cx| v.cycle_playback_mode(cx));
                    })
            })
            .child({
                let entity = entity.clone();
                Button::new("video-speed")
                    .ghost()
                    .rounded_full()
                    .label(format!("{:.2}×", self.speed))
                    .w(px(40.))
                    .h(px(28.))
                    .text_size(px(11.))
                    .on_click(move |_, _w, app| {
                        entity.update(app, |v, cx| v.speed_step(1, cx));
                    })
            })
            .child(
                div()
                    .whitespace_nowrap()
                    .text_size(px(11.))
                    .text_color(muted)
                    .child(playlist),
            )
            .child(control_icon_button(
                "video-shot",
                IconName::Camera,
                accent,
                entity.clone(),
                |v, cx| v.screenshot_png(cx),
            ))
            .into_any_element()
    }


    /// 可交互横向条（seek 进度条 / 音量条共用）：玻璃细轨 + 主色填充 + hover 放大 thumb。
    /// 轨道视觉厚度从 16px 收到 4px（交互热区仍是 16px，`bounds` 盒不变），canvas 记录
    /// **窗口坐标**盒到 `bounds`（渲染期写入；事件回调读）。`seek=true` 时 `flex_1`
    /// 拉伸并走 seek 处理器，否则固定宽走音量处理器。
    pub(super) fn strip_element(
        id: &'static str,
        bounds: Rc<Cell<Bounds<Pixels>>>,
        fraction: f32,
        track: Hsla,
        fill: Hsla,
        seek: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let f = fraction.clamp(0.0, 1.0);
        // 元素级 hover 态：只驱动 thumb 放大/淡入（native 按钮同款姿势），不碰
        // seek/vol 已有的 on_mouse_down/on_mouse_move 交互路径。
        let hover_flag = window.use_keyed_state(format!("{id}-hover"), cx, |_, _| false);
        let hovered = *hover_flag.read(cx);
        let view_id = window.current_view();
        let mut el = div()
            .id(id)
            .h(px(16.))
            .relative()
            .cursor(CursorStyle::PointingHand)
            .on_hover({
                let flag = hover_flag.clone();
                move |hovered, _w, cx| {
                    flag.update(cx, |v, _| *v = *hovered);
                    cx.notify(view_id);
                }
            })
            .child(
                // 玻璃细轨（4px 半透明）+ 主色填充 + hover thumb：thumb 圆心始终对准
                // 填充末端（= 播放位置）；大小/偏移/透明度共用同一 0..1 弹簧值，增大的
                // 同时圆心不漂移（top/right 由尺寸反推）。
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
                            .rounded_full()
                            .bg(track.opacity(0.30))
                            .child(
                                div()
                                    .absolute()
                                    .left_0()
                                    .top_0()
                                    .bottom_0()
                                    .w(DefiniteLength::Fraction(f))
                                    .rounded_full()
                                    .bg(fill)
                                    .child(
                                        div()
                                            .absolute()
                                            .with_spring(
                                                format!("{id}-thumb"),
                                                hover_spring(if hovered { 1.0 } else { 0.0 }),
                                                {
                                                    let fill = fill;
                                                    move |this, v| {
                                                        let s = 4.0 + 6.0 * v;
                                                        this.w(px(s))
                                                            .h(px(s))
                                                            // 垂直居中：轨道半高 2px − 半径 s/2
                                                            .top(px(2.0 - s / 2.0))
                                                            // 圆心对准填充末端：右缘外扩 s/2
                                                            .right(px(-s / 2.0))
                                                            .opacity(0.15 + 0.85 * v)
                                                            .bg(fill)
                                                    }
                                                },
                                            ),
                                    ),
                            ),
                    ),
            )
            .child(canvas(
                move |b, _w, _app| {
                    bounds.set(b);
                },
                |_, _, _, _| {},
            ))
            .when(seek, |this| this.flex_1())
            .when(!seek, |this| this.w(px(56.)));
        if seek {
            el = el
                .on_mouse_down(MouseButton::Left, cx.listener(Self::on_seek_down))
                .on_mouse_move(cx.listener(Self::on_seek_move))
                .on_mouse_up(MouseButton::Left, cx.listener(Self::on_seek_up))
                .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_seek_up));
        } else {
            el = el
                .on_mouse_down(MouseButton::Left, cx.listener(Self::on_vol_down))
                .on_mouse_move(cx.listener(Self::on_vol_move));
        }
        el.into_any_element()
    }
}

impl Render for VideoViewer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let canvas = self.bg.unwrap_or_else(|| Self::default_canvas(cx));
        let theme = ui::theme::Theme::global(cx);
        let is_dark = theme.is_dark();
        let muted: Hsla = theme.colors.fg_muted.into();
        // 状态浮层（右上角）配色：Copy 抽出，避免在 `control_bar(&mut cx)` 之后借用 theme
        let toast_surface: Hsla = theme.colors.surface.into();
        let toast_border: Hsla = theme.colors.border.into();
        let toast_fg: Hsla = theme.colors.fg.into();

        let (vw, vh) = self.viewport_size(window);
        // 普通模式预留底部控制条高度（控制条常驻，画面 contain 到其上）；
        // 纯视频（锁定）模式控制条是浮层 → 画面充满整个画布区。
        let area_h = if self.locked {
            vh.max(1.0)
        } else {
            (vh - CONTROL_BAR_H).max(1.0)
        };
        // 按显示尺寸自适应解码封顶：画面已上屏且窗口尺寸落进新的 256px「桶」时，只
        // 重开视频解码器（restart_audio=false 不打断音频/进度），把 sws 输出与逐帧
        // 上传切成显示所需像素数（省 decode/swscale/纹理上传，播放 CPU 大头之一）。
        if self.has_frame()
            && !self.loading
            && self.pending_seek.is_none()
            && vw > 0.0
            && area_h > 0.0
        {
            let wanted = self.desired_decode_cap(vw, area_h);
            if wanted != self.decode_cap {
                self.decode_cap = wanted;
                let t = self.playhead;
                self.start_decoder_at(t, None, false);
            }
        }
        // 视频实际显示矩形（contain 等比+缩放+平移后的像素框，单位 f32）。暂停徽标以此为基准
        // 挂右上角内边距，不随窗口/画布走——防止 contain 后有黑边时徽标飘出视频范围。
        let mut vrect = (0.0, 0.0, vw, area_h);
        let body: gpui::AnyElement = if let Some(err) = &self.load_error {
            Self::placeholder(&format!("无法打开视频：{err}"), muted, cx)
        } else {
            match (self.gpu_frame.as_ref(), self.frame.clone(), self.intrinsic) {
                // 双设备 GPU 直通帧：canvas paint 里调 `paint_external_yuv`（外部共享 NV12
                // 纹理直接走 vendored YUV→BGRA 管线，CPU 零回读/零 sws）。bounds 取与软图
                // 同款 contain×缩放×平移后的像素框。
                (Some(g), _, Some((iw, ih))) => {
                    let (dw, dh) = Self::contain_fit(iw, ih, vw, area_h);
                    let (dw, dh) = (dw * self.zoom, dh * self.zoom);
                    self.pan = Self::clamp_pan(self.pan, vw, area_h, dw, dh);
                    let (ox, oy) = ((vw - dw) * 0.5 + self.pan.0, (area_h - dh) * 0.5 + self.pan.1);
                    vrect = (ox, oy, dw, dh);
                    let handle = g.shared_handle;
                    let tex_size = gpui::Size {
                        width: px(iw.max(1.0)),
                        height: px(ih.max(1.0)),
                    };
                    gpui::canvas(
                        move |_, _, _| (),
                        move |bounds, (), window, _cx| {
                            // 测试探针：paint_external_yuv 真的收到本帧共享句柄
                            //（production 为 no-op）
                            crate::hw::record_hw_paint(handle);
                            window.paint_external_yuv(
                                bounds,
                                gpui::Corners::default(),
                                handle,
                                tex_size,
                            );
                        },
                    )
                    .absolute()
                    .left(px(ox))
                    .top(px(oy))
                    .w(px(dw))
                    .h(px(dh))
                    .into_any_element()
                }
                // 软 BGRA 帧（`RenderImage`）：硬解 reader 的帧在解码线程内核已回读+sws
                // 转 BGRA，与软解逐字同构。
                (_, Some(render), Some((iw, ih))) => {
                    // 手动 contain 适配 × 缩放（绝对定位 + 显式像素框，**不用 100% 尺寸**）：
                    // 竖屏/横屏都等比缩放居中，不会因 flex 子级百分比高度失效而按固有
                    // 尺寸放大只露局部（对齐 ImageViewer 的 display_size 姿势）。放大超出
                    // 视口时由 `#video-viewer overflow_hidden` 裁剪在画布内。
                    let (dw, dh) = Self::contain_fit(iw, ih, vw, area_h);
                    let (dw, dh) = (dw * self.zoom, dh * self.zoom);
                    // 放大后拖拽平移：clamp 收敛到图覆盖画布区（未放大时 pan 恒 0 =
                    // 居中；中心锚定缩放），再把偏移并到像素框坐标。
                    self.pan = Self::clamp_pan(self.pan, vw, area_h, dw, dh);
                    let (ox, oy) = ((vw - dw) * 0.5 + self.pan.0, (area_h - dh) * 0.5 + self.pan.1);
                    vrect = (ox, oy, dw, dh);
                    img(gpui::ImageSource::Render(render))
                        .object_fit(ObjectFit::Contain)
                        .absolute()
                        .left(px(ox))
                        .top(px(oy))
                        .w(px(dw))
                        .h(px(dh))
                        .into_any_element()
                }
                (_, _, _) if self.playing || self.loading => {
                    Self::placeholder("正在打开视频…", muted, cx)
                }
                (_, _, _) => Self::placeholder("暂停", muted, cx),
            }
        };

        let mut root = div()
            .id("video-viewer")
            .size_full()
            .relative()
            .overflow_hidden()
            .bg(canvas)
            .child(
                div()
                    .id("video-main")
                    .size_full()
                    .relative()
                    .track_scroll(&self.viewport)
                    .track_focus(&self.focus_handle)
                    .cursor(if self.drag.is_some() {
                        CursorStyle::ClosedHand
                    } else {
                        CursorStyle::OpenHand
                    })
                    .on_key_down(cx.listener(Self::on_key_down))
                    .on_scroll_wheel(cx.listener(Self::on_scroll_wheel))
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::on_pan_down))
                    // 双击画面 → 全屏 + 锁定（单击/拖拽仍是 on_pan_down）。互斥不靠「上层覆盖
                    // 元素优先消费」：gpui 鼠标事件在命中栈内冒泡，控制条/状态浮层/进度条命中
                    // 的双击由各自 `on_controls_mouse_down` `stop_propagation` 拦截，到不了这里；
                    // 隐藏态控制条透明热区不挂该拦截 → 双击仍是画面全屏。
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::on_video_surface_mouse_down))
                    .on_mouse_move(cx.listener(Self::on_pan_move))
                    .on_mouse_up(MouseButton::Left, cx.listener(Self::on_pan_up))
                    .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_pan_up))
                    .child(body),
            );
        // 控制条：普通模式常驻（画面预留底部高度）；纯视频（锁定）模式为浮层按需浮现，
        // 画面充满整个画布区。
        root = root.child(self.control_bar(window, cx));
        // F1 悬停时间提示：未拖拽且正悬停进度条（hover_x 在条内被记录）时挂出，纯视觉无交互；
        // 拖拽/移出/换文件时 hover_x 已被清 None 或 scrubbing 为真 → 不渲染浮层。
        if !self.scrubbing
            && let Some(secs) = self.hovered_target
            && let Some(x) = self.hover_x
        {
            root = root.child(Self::hover_time_float(secs, x, &self.seek_bounds, window, cx));
        }
        // 右上角瞬时状态浮层（截图结果提示等）：在期内才挂，过期由定时器清除兜底。
        if let Some((text, at)) = &self.status
            && at.elapsed() < std::time::Duration::from_millis(STATUS_MS)
        {
            root = root.child(
                div()
                    .absolute()
                    .top_3()
                    .right_3()
                    .px_3()
                    .py_1()
                    .rounded_lg()
                    .border_1()
                    .border_color(toast_border)
                    // 状态浮层同样吞双击（归属 `on_controls_mouse_down` 互斥族：不向画面漏）。
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::on_controls_mouse_down))
                    // 玻璃卡片：半透明表面 + 圆角 + 高光内阴影；出现时短弹簧淡入（收敛即静止，
                    // 无持续重绘）。消失仍由 `STATUS_MS` 到期卸载，不做离场动画。
                    .bg(toast_surface.opacity(if is_dark {
                        GLASS_PANEL_ALPHA_DARK
                    } else {
                        GLASS_PANEL_ALPHA_LIGHT
                    }))
                    // 状态浮层投影 = 黑 hsla 双散值（0.18@10 + 0.14@2），玻璃卡专属，theme 无阴影 token
                    .shadow(vec![
                        gpui::BoxShadow::new(px(0.), px(4.), gpui::hsla(0., 0., 0., 0.18))
                            .blur_radius(px(10.)),
                        gpui::BoxShadow::new(px(0.), px(1.), gpui::hsla(0., 0., 0., 0.14))
                            .blur_radius(px(2.)),
                    ])
                    .text_size(px(12.))
                    .text_color(toast_fg)
                    .child(text.clone())
                    .with_spring(
                        "video-toast-fade",
                        hover_spring(1.0).from(0.0),
                        |this, v| this.opacity(v),
                    ),
            );
        }
        // 暂停标志（右上角）：仅放大 ❚❚ 双竖条图标，无圆底/边框/投影；暂停/停在结尾时
        // 显示。以**视频实际矩形** `vrect` 为基准（右上角内 32px），不随窗口/画布走；纯视觉
        // 无交互（不遮双击全屏）；仅在有画面且非播放、未出错时挂出。
        let badge_shown = Self::paused_badge_shown(self.playing, self.has_frame(), self.load_error.is_some());
        if badge_shown {
            let badge_left = (vrect.0 + vrect.2 - 80.0).max(4.0);
            let badge_top = (vrect.1 + 30.0).max(4.0);
            root = root.child(
                div()
                    .id("video-paused-badge")
                    .absolute()
                    .left(px(badge_left))
                    .top(px(badge_top))
                    .child(Icon::new(IconName::Pause).size(px(60.)).text_color(toast_fg))
                    .with_spring(
                        "video-paused-fade",
                        hover_spring(1.0).from(0.0),
                        |this, v| this.opacity(v),
                    ),
            );
        }
        // 字幕渲染层：有字幕、字幕可见、且当前时间有活跃字幕时叠加。
        // 定位以**视频实际矩形** `vrect`（contain 适配后含缩放/平移的画面像素框，
        // 本函数前段逐帧就绪时更新）为基准：画面底部居中、留安全边距，字幕不越出
        // 画面、不遮控制条；无画面（占位/黑边）时退回画布视频区底部。gpui 没有文字
        // 描边能力（styled/text_system 无 text_stroke），故退化为**半透明深色圆角
        // 底条**保证可读性（颜色走 subtitle_style.outline_color，默认黑，即
        // 「白字黑描边」的落底实现，零新依赖）。
        if self.subtitle_visible && !self.subtitles.is_empty() {
            if let Some(subtitle) = find_active_subtitle(&self.subtitles, self.playhead) {
                // 文本已在加载时清洗（见 start());当前在字幕窗口内 → 直接显示
                let text = subtitle.text.clone();
                if !text.trim().is_empty() {
                    let (vx, vy, vdw, vdh) = vrect;
                    // 随画面高度适度放大（参考高度 480，钳位 0.8x..=1.8x）
                    let scale = (vdh / 480.0).clamp(0.8, 1.8);
                    let font_size = self.subtitle_style.font_size * scale;
                    let strip_bg = self.subtitle_style.outline_color.opacity(0.55);
                    root = root.child(
                        div()
                            .absolute()
                            .left(px(vx))
                            .top(px(vy))
                            .w(px(vdw))
                            .h(px(vdh))
                            .flex()
                            .flex_col()
                            .items_center()
                            .justify_end()
                            .pb_4()
                            .child(
                                div()
                                    .max_w_full()
                                    .px_4()
                                    .py_2()
                                    .rounded_lg()
                                    .bg(strip_bg)
                                    .text_size(px(font_size))
                                    .text_color(self.subtitle_style.primary_color)
                                    .child(text),
                            ),
                    );
                }
            }
        }
        root
    }
}

fn control_icon_button(
    id: &'static str,
    icon: IconName,
    tint: Hsla,
    entity: gpui::Entity<VideoViewer>,
    action: impl Fn(&mut VideoViewer, &mut Context<VideoViewer>) + 'static,
) -> gpui::AnyElement {
    Button::new(id)
        .ghost()
        // 圆形 hover 底：靠 `Button` 自带的选定/按压 backplate 弹簧打在圆内高亮
        .rounded_full()
        .w(px(28.))
        .h(px(28.))
        .child(Icon::new(icon).size(px(16.)).text_color(tint))
        .on_click(move |_, _w, app| {
            entity.update(app, |v, cx| action(v, cx));
        })
        .into_any_element()
}

pub(super) fn audio_read_rate(device_rate: u32, speed: f32) -> u32 {
    let speed = speed.clamp(0.25, 4.0);
    (f64::from(device_rate) / f64::from(speed)).round().max(1.0) as u32
}
